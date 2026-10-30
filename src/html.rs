//! HTML tokenizer + arena DOM tree builder.
//! shortcut: forgiving tag soup parsing (no spec-compliant tree construction, no
//! foster parenting for tables); upgrade when real-world pages break.

pub enum Token {
    Text(String),
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
}

/// Void elements never have children, so they must not be pushed on the stack.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
    "source", "track", "wbr",
];

/// Elements whose content is raw text (no tags inside).
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "svg"];

/// Interned tag name: `u16` index into `Dom::tags` instead of a per-node String.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct TagId(u16);

pub enum NodeKind {
    Element(TagId, Box<[(String, String)]>),
    Text(String),
}

pub struct Node {
    pub parent: u32,
    pub children: Vec<u32>,
    pub kind: NodeKind,
}

/// Arena DOM: nodes indexed by u32, index 0 is the document root.
pub struct Dom {
    pub nodes: Vec<Node>,
    /// interned tag-name table; TagId.0 indexes here
    tags: Vec<Box<str>>,
    tag_index: std::collections::HashMap<Box<str>, u16>,
}

impl Dom {
    fn new() -> Self {
        let mut dom = Dom {
            nodes: Vec::new(),
            tags: Vec::new(),
            tag_index: std::collections::HashMap::new(),
        };
        let root = dom.intern("#document");
        dom.nodes.push(Node {
            parent: 0,
            children: Vec::new(),
            kind: NodeKind::Element(root, Box::from([])),
        });
        dom
    }

    fn intern(&mut self, name: &str) -> TagId {
        if let Some(&id) = self.tag_index.get(name) {
            return TagId(id);
        }
        let id = self.tags.len() as u16;
        self.tags.push(name.into());
        self.tag_index.insert(name.into(), id);
        TagId(id)
    }

    fn add(&mut self, parent: u32, kind: NodeKind) -> u32 {
        let idx = self.nodes.len() as u32;
        self.nodes[parent as usize].children.push(idx);
        self.nodes.push(Node {
            parent,
            children: Vec::new(),
            kind,
        });
        idx
    }

    pub fn tag(&self, idx: u32) -> &str {
        match &self.nodes[idx as usize].kind {
            NodeKind::Element(t, _) => &self.tags[t.0 as usize],
            NodeKind::Text(_) => "#text",
        }
    }

    /// Attribute value for an element node (case-insensitive name).
    pub fn attr(&self, idx: u32, name: &str) -> Option<&str> {
        match &self.nodes[idx as usize].kind {
            NodeKind::Element(_, attrs) => attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str()),
            NodeKind::Text(_) => None,
        }
    }

    /// First `body` descendant, else the document root.
    pub fn body_root(&self) -> u32 {
        let mut queue = vec![0u32];
        while let Some(i) = queue.pop() {
            if self.tag(i) == "body" {
                return i;
            }
            for &c in &self.nodes[i as usize].children {
                queue.push(c);
            }
        }
        0
    }
}

pub fn parse(html: &str) -> Dom {
    let mut dom = Dom::new();
    let mut stack: Vec<u32> = vec![0];
    for token in tokenize(html) {
        match token {
            Token::Text(t) => {
                if !t.is_empty() {
                    let parent = *stack.last().unwrap();
                    dom.add(parent, NodeKind::Text(decode_entities(&t)));
                }
            }
            Token::StartTag {
                name,
                attrs,
                self_closing,
            } => {
                close_implied(&name, &mut stack, &dom);
                let parent = *stack.last().unwrap();
                let tid = dom.intern(&name);
                let attrs: Box<[(String, String)]> = attrs.into_boxed_slice();
                let idx = dom.add(parent, NodeKind::Element(tid, attrs));
                if !self_closing && !VOID.contains(&name.as_str()) {
                    stack.push(idx);
                }
            }
            Token::EndTag { name } => {
                // pop to matching open tag; ignore end tags with no match
                if let Some(pos) = stack
                    .iter()
                    .rposition(|&i| dom.tag(i) == name.as_str())
                {
                    stack.truncate(pos);
                }
            }
        }
    }
    expand_declarative_shadow_roots(&mut dom);
    dom
}

/// `<template shadowroot=...>` / `<template shadowrootmode=...>` with value
/// `open` or `closed` (ASCII case-insensitive) declares a shadow root.
fn is_declarative_shadow_template(dom: &Dom, idx: u32) -> bool {
    ["shadowroot", "shadowrootmode"].iter().any(|&a| {
        dom.attr(idx, a)
            .is_some_and(|v| v.eq_ignore_ascii_case("open") || v.eq_ignore_ascii_case("closed"))
    })
}

/// Post-parse pass: expand declarative shadow DOM templates.
/// shortcut: FLAT EXPANSION only — the template's children are spliced into
/// the parent's child list at the template's position and the template is
/// unlinked; no real shadow-root abstraction, no `<slot>` distribution, no
/// `::slotted`/scoped-style isolation (backlog). `shadowroot="closed"` is
/// treated identically to `"open"` since rendering ignores the mode.
/// Plain `<template>` (no shadowroot attr) is untouched: its children stay
/// under the template node, which layout's NON_RENDERED list skips — i.e.
/// inert per spec.
fn expand_declarative_shadow_roots(dom: &mut Dom) {
    // Each node is queued at most once (the tree is acyclic), so shadow
    // templates nested inside shadow templates terminate without looping.
    let mut queue = vec![0u32];
    while let Some(i) = queue.pop() {
        let children = std::mem::take(&mut dom.nodes[i as usize].children);
        let mut kept = Vec::with_capacity(children.len());
        for c in children {
            if dom.tag(c) == "template" && is_declarative_shadow_template(dom, c) {
                hoist_shadow_children(dom, i, c, &mut kept, &mut queue);
                // the unlinked template node remains in the arena but is
                // unreachable from the root, so layout never sees it
            } else {
                kept.push(c);
                queue.push(c);
            }
        }
        dom.nodes[i as usize].children = kept;
    }
}

/// Recursively splice `tmpl`'s children into `parent`'s child list (`out`).
/// shortcut: a shadow template nested directly inside another one (e.g. DSD
/// emitted by nested web components) is expanded inline into the same parent,
/// recursing — the parent was already walked, so a queue pass can't catch it.
fn hoist_shadow_children(dom: &mut Dom, parent: u32, tmpl: u32, out: &mut Vec<u32>, queue: &mut Vec<u32>) {
    let grandkids = std::mem::take(&mut dom.nodes[tmpl as usize].children);
    for g in grandkids {
        if dom.tag(g) == "template" && is_declarative_shadow_template(dom, g) {
            hoist_shadow_children(dom, parent, g, out, queue);
        } else {
            dom.nodes[g as usize].parent = parent;
            out.push(g);
            queue.push(g); // descendants may hold more DSD templates
        }
    }
}

/// Implicit end tags for `<p>a<p>b`, `<li>x<li>y`, headings, cells and rows.
fn close_implied(incoming: &str, stack: &mut Vec<u32>, dom: &Dom) {
    let top = dom.tag(*stack.last().unwrap());
    let close_top = match incoming {
        "li" => top == "li",
        "dt" | "dd" => top == "dt" || top == "dd",
        "td" | "th" => top == "td" || top == "th",
        "tr" => top == "td" || top == "th" || top == "tr",
        t if matches!(t, "p" | "div" | "ul" | "ol" | "table" | "blockquote" | "section"
            | "article" | "header" | "footer" | "nav" | "aside" | "form" | "dl" | "figure") =>
            top == "p",
        t if matches!(t, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => {
            top == "p" || matches!(top, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
        }
        _ => false,
    };
    if close_top {
        stack.pop();
    }
}

/// Parse ` href="/x" id='y' data-z=raw ` (after the tag name) into pairs.
fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let key_start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' {
            i += 1;
        }
        let key = s[key_start..i].to_ascii_lowercase();
        if key.is_empty() {
            i += 1;
            continue;
        }
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let quote = b[i];
                i += 1;
                let vstart = i;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                value = s[vstart..i].to_string();
                if i < b.len() {
                    i += 1; // closing quote
                }
            } else {
                let vstart = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = s[vstart..i].to_string();
            }
        }
        out.push((key, decode_entities(&value)));
    }
    out
}

pub fn tokenize(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut text_start = 0usize;

    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        // `<` not starting a tag is literal text (e.g. "a < b")
        let next = b.get(i + 1).copied().unwrap_or(0);
        if !next.is_ascii_alphabetic() && next != b'/' && next != b'!' && next != b'?' {
            i += 1;
            continue;
        }
        if text_start < i {
            out.push(Token::Text(s[text_start..i].to_string()));
        }

        if s[i..].starts_with("<!--") {
            match s[i + 4..].find("-->") {
                Some(e) => {
                    i = i + 4 + e + 3;
                }
                None => break,
            }
            text_start = i;
            continue;
        }
        if next == b'!' || next == b'?' {
            // doctype / processing instruction: skip
            match s[i..].find('>') {
                Some(e) => {
                    i += e + 1;
                }
                None => break,
            }
            text_start = i;
            continue;
        }

        // find '>' respecting quotes (href="a>b" must not end the tag)
        let mut tag_end = None;
        let mut in_quote: Option<u8> = None;
        for (k, c) in s[i..].bytes().enumerate() {
            match in_quote {
                Some(q) if c == q => in_quote = None,
                Some(_) => {}
                None if c == b'"' || c == b'\'' => in_quote = Some(c),
                None if c == b'>' => {
                    tag_end = Some(i + k);
                    break;
                }
                None => {}
            }
        }
        let Some(tag_end) = tag_end else { break };
        let raw_tag = &s[i + 1..tag_end];
        let end_tag = raw_tag.starts_with('/');
        let name: String = raw_tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let name = name.to_ascii_lowercase();

        if name.is_empty() {
            // not a tag after all, keep as text
            i += 1;
            text_start = i;
            continue;
        }

        if end_tag {
            out.push(Token::EndTag { name });
            i = tag_end + 1;
        } else {
            let self_closing = raw_tag.trim_end().ends_with('/');
            let attrs = parse_attrs(&raw_tag[name.len()..]);
            out.push(Token::StartTag {
                name: name.clone(),
                attrs,
                self_closing,
            });
            i = tag_end + 1;
            if RAW_TEXT.contains(&name.as_str()) && !self_closing {
                // swallow raw text content up to the matching close tag, but
                // keep it as a text child — <style> content is our CSS source
                let lower = s[i..].to_ascii_lowercase();
                let end = match lower.find(&format!("</{name}")) {
                    Some(c) => i + c,
                    None => s.len(),
                };
                if end > i {
                    out.push(Token::Text(s[i..end].to_string()));
                }
                // close the element: raw-text handling swallows the literal
                // `</name>` so synthesize its end tag (EOF also closes it)
                out.push(Token::EndTag { name });
                // jump past the close tag (or to EOF if it's missing)
                i = if end < s.len() {
                    s[end..].find('>').map(|g| end + g + 1).unwrap_or(s.len())
                } else {
                    s.len()
                };
                // no close tag: rest of document belongs to the element
                text_start = i;
                continue;
            }
        }
        text_start = i;
    }
    if text_start < s.len() {
        out.push(Token::Text(s[text_start..].to_string()));
    }
    out
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find('&') {
        out.push_str(&rest[..a]);
        rest = &rest[a..];
        let Some(semi_rel) = rest.find(';').filter(|&p| p <= 10) else {
            out.push('&');
            rest = &rest[1..];
            break;
        };
        let ent = &rest[1..semi_rel];
        let replacement = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match replacement {
            Some(c) => {
                out.push(c);
                rest = &rest[semi_rel + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decode response bytes to a String: transport-layer charset first (`content-type`
/// header), then `<meta charset>` in the first 1024 bytes, then UTF-8. BOM wins
/// over all of them (handled by encoding_rs `decode`).
pub fn decode_html(bytes: &[u8], content_type: Option<&str>) -> String {
    let enc = content_type
        .and_then(parse_charset_label)
        .or_else(|| sniff_meta_charset(bytes))
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    enc.decode(bytes).0.into_owned()
}

/// `text/html; charset=Shift_JIS` → `shift_jis`
fn parse_charset_label(content_type: &str) -> Option<String> {
    let idx = content_type.to_ascii_lowercase().find("charset=")?;
    let rest = &content_type[idx + 8..];
    let end = rest
        .find(|c: char| c == ';' || c.is_whitespace())
        .unwrap_or(rest.len());
    let label = rest[..end].trim().trim_matches('"').trim_matches('\'');
    if label.is_empty() {
        None
    } else {
        Some(label.to_ascii_lowercase())
    }
}

/// HTML spec prescan, simplified: scan `<meta ...>` tags in the first 1024 bytes
/// for `charset=` (covers `<meta charset=x>` and the http-equiv content-type form).
fn sniff_meta_charset(bytes: &[u8]) -> Option<String> {
    let window = &bytes[..bytes.len().min(1024)];
    let mut search = 0usize;
    while let Some(rel) = find_ascii_ci(&window[search..], b"<meta") {
        let start = search + rel;
        let tag_end = find_ascii(&window[start..], b">").map(|e| start + e)?;
        let tag = &window[start..tag_end];
        if let Some(pos) = find_ascii_ci(tag, b"charset=") {
            let rest = &tag[pos + 8..];
            let end = rest
                .iter()
                .position(|&c| c == b';' || c == b'"' || c == b'\'' || c.is_ascii_whitespace())
                .unwrap_or(rest.len());
            let label = String::from_utf8_lossy(&rest[..end]).trim().to_string();
            if !label.is_empty() {
                return Some(label);
            }
        }
        search = tag_end + 1;
    }
    None
}

/// Case-sensitive ASCII substring search.
fn find_ascii(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Case-insensitive ASCII substring search (ASCII only — tags are ASCII).
fn find_ascii_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_without_hints() {
        assert_eq!(decode_html("héllo".as_bytes(), None), "héllo");
    }

    #[test]
    fn meta_charset_gbk() {
        // "中" in GBK is D6 D0
        let mut bytes = b"<html><head><meta charset=gbk></head><body>".to_vec();
        bytes.extend_from_slice(&[0xD6, 0xD0]);
        assert_eq!(decode_html(&bytes, None), "<html><head><meta charset=gbk></head><body>中");
    }

    #[test]
    fn header_charset_wins_over_meta() {
        // header says windows-1251 ("я" = 0xFF), meta lies about utf-8
        let bytes = b"<meta charset=utf-8> \xFF".to_vec();
        assert_eq!(
            decode_html(&bytes, Some("text/html; charset=windows-1251")),
            "<meta charset=utf-8> я"
        );
    }

    #[test]
    fn bom_wins_and_is_stripped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("hi".as_bytes());
        assert_eq!(decode_html(&bytes, Some("text/html; charset=gbk")), "hi");
    }

    #[test]
    fn http_equiv_meta_form() {
        let bytes = b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=Shift_JIS\">".to_vec();
        // label recognized (for_label succeeds); body: "あ" in Shift_JIS = 0x82 0xA0
        let mut full = bytes.clone();
        full.extend_from_slice(&[0x82, 0xA0]);
        let out = decode_html(&full, None);
        assert!(out.ends_with('あ'), "got {out:?}");
    }

    #[test]
    fn parse_charset_label_tests() {
        assert_eq!(parse_charset_label("text/html; charset=UTF-8").as_deref(), Some("utf-8"));
        assert_eq!(parse_charset_label("text/html").is_none(), true);
        assert_eq!(parse_charset_label("text/html; charset=\"gbk\"").as_deref(), Some("gbk"));
    }

    fn text_of(dom: &Dom) -> String {
        dom.nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn nested_elements() {
        let dom = parse("<div><p>Hello <b>bold</b></p></div>");
        let p = dom.nodes.iter().position(|n| dom.tag(n.parent as usize as u32) == "p" && matches!(n.kind, NodeKind::Element(..)));
        assert!(p.is_some(), "expected a <p> child");
        assert_eq!(text_of(&dom), "Hello |bold");
    }

    #[test]
    fn implied_li_close() {
        let dom = parse("<ul><li>one<li>two</ul>");
        let li_indices: Vec<u32> = dom
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| matches!(&n.kind, NodeKind::Element(..)) && dom.tag(*i as u32) == "li")
            .map(|(i, _)| i as u32)
            .collect();
        assert_eq!(li_indices.len(), 2, "expected implied close to yield two <li>");
        // "two" must be inside the second li, not nested in the first
        let second_li = li_indices[1];
        let txt: String = dom.nodes[second_li as usize]
            .children
            .iter()
            .filter_map(|&c| match &dom.nodes[c as usize].kind {
                NodeKind::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(txt.contains("two"), "second li text: {txt:?}");
    }

    #[test]
    fn script_content_kept_but_isolated() {
        let dom = parse("<body><p>keep</p><script>var x = '<b>';</script><p>this</p></body>");
        // content stays as a child of <script> (same mechanism <style> uses to
        // preserve CSS source); layout's NON_RENDERED list keeps it off-screen
        let script = (0..dom.nodes.len())
            .find(|&i| dom.tag(i as u32) == "script")
            .expect("script element");
        let kids = &dom.nodes[script].children;
        assert_eq!(kids.len(), 1, "script should own exactly its text");
        match &dom.nodes[kids[0] as usize].kind {
            NodeKind::Text(t) => assert_eq!(t, "var x = '<b>';"),
            _ => panic!("script content must be a text node"),
        }
        // body-level visible text is unaffected
        assert_eq!(text_of(&dom), "keep|var x = '<b>';|this");
    }

    #[test]
    fn void_elements_dont_capture_siblings() {
        let dom = parse("<p><img src=x>after</p>");
        assert_eq!(text_of(&dom), "after");
    }

    #[test]
    fn entities() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &#65;&#x42;"), "a & b <c> AB");
        assert_eq!(decode_entities("5 < 6 & unknown&foo;"), "5 < 6 & unknown&foo;");
    }

    #[test]
    fn raw_lt_in_text() {
        let dom = parse("<p>5 < 6 and 7 > 2</p>");
        assert_eq!(text_of(&dom), "5 < 6 and 7 > 2");
    }

    #[test]
    fn tags_are_interned_not_duplicated() {
        // 3 <p> nodes, but only one TagId may exist for "p"
        let dom = parse("<p>a<p>b<p>c");
        let p_count = dom
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| matches!(&n.kind, NodeKind::Element(..)) && dom.tag(*i as u32) == "p")
            .count();
        assert_eq!(p_count, 3);
        // distinct tag names get distinct ids; equal names share one table slot
        let ids: std::collections::HashSet<u16> = dom
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(_i, n)| match &n.kind {
                NodeKind::Element(t, _) => Some(t.0),
                _ => None,
            })
            .collect();
        // #document, p — exactly 2 interned strings for this document
        assert_eq!(ids.len(), 2, "expected 2 unique tags, got {ids:?}");
    }

    #[test]
    fn body_root_finds_body() {
        let dom = parse("<html><head><title>x</title></head><body><p>hi</p></body></html>");
        let root = dom.body_root();
        assert_eq!(dom.tag(root), "body");
    }

    /// children (by tag or text) of `parent` in the live tree, in order
    fn child_tags(dom: &Dom, parent: u32) -> Vec<String> {
        dom.nodes[parent as usize]
            .children
            .iter()
            .map(|&c| dom.tag(c).to_string())
            .collect()
    }

    fn find_tag(dom: &Dom, name: &str) -> u32 {
        (0..dom.nodes.len())
            .find(|&i| dom.tag(i as u32) == name)
            .expect("element not found") as u32
    }

    #[test]
    fn declarative_shadow_template_hoists_children() {
        // (a) <template shadowroot="open"> children become div's children
        let dom = parse(r#"<div><template shadowroot="open"><p>hi</p></template></div>"#);
        let div = find_tag(&dom, "div");
        assert_eq!(child_tags(&dom, div), ["p"], "p should be a live child of div");
        // template must be unreachable from the root (it lingers in the arena)
        let mut seen = vec![false; dom.nodes.len()];
        let mut stack = vec![0u32];
        while let Some(i) = stack.pop() {
            seen[i as usize] = true;
            stack.extend(dom.nodes[i as usize].children.iter().copied());
        }
        assert!(
            (0..dom.nodes.len()).all(|i| dom.tag(i as u32) != "template" || !seen[i]),
            "template must not be reachable from the document root"
        );
        let p = find_tag(&dom, "p");
        assert_eq!(dom.nodes[p as usize].parent, div, "hoisted p reparents to div");
        assert_eq!(text_of(&dom), "hi");
    }

    #[test]
    fn declarative_shadow_attr_order_and_case() {
        // (b) either attribute spelling works, in any order, value case-insensitive
        let dom = parse(
            r#"<div><template shadowrootmode="open" shadowroot="closed"><p>one</p></template><template shadowroot=OPEN><p>two</p></template></div>"#,
        );
        let div = find_tag(&dom, "div");
        assert_eq!(child_tags(&dom, div), ["p", "p"], "both templates hoist");
        assert_eq!(text_of(&dom), "one|two");
    }

    #[test]
    fn plain_template_stays_inert() {
        // (c) <template> without a shadowroot attr keeps its contents off the live tree
        let dom = parse("<div><template><p>x</p></template></div>");
        let div = find_tag(&dom, "div");
        assert_eq!(child_tags(&dom, div), ["template"], "template stays, p is not hoisted");
        let tmpl = find_tag(&dom, "template");
        assert_eq!(child_tags(&dom, tmpl), ["p"], "p remains inert under the template");
        // and a bogus shadowroot value is not declarative shadow DOM either
        let dom = parse(r#"<div><template shadowroot="garbage"><p>y</p></template></div>"#);
        let div = find_tag(&dom, "div");
        assert_eq!(child_tags(&dom, div), ["template"]);
    }

    #[test]
    fn nested_declarative_shadow_templates_terminate() {
        // (d) DSD inside DSD: everything hoists outward, no infinite loop
        let dom = parse(
            r#"<div><template shadowroot="open"><p>a</p><template shadowrootmode="closed"><span>b</span></template></template><p>after</p></div>"#,
        );
        let div = find_tag(&dom, "div");
        assert_eq!(child_tags(&dom, div), ["p", "span", "p"]);
        assert_eq!(text_of(&dom), "a|b|after");
        let span = find_tag(&dom, "span");
        assert_eq!(dom.nodes[span as usize].parent, div, "inner hoist reaches div");
    }
}

