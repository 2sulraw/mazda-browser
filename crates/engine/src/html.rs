//! HTML tokenizer + arena DOM tree builder.
//! shortcut: forgiving tag soup parsing (no spec-compliant tree construction, no
//! foster parenting for tables); upgrade when real-world pages break.

pub enum Token {
    Text(String),
    /// `<!-- ... -->` body kept verbatim: React's streaming bootstrap parks
    /// its boundary markers in comments and rewrites their `data`.
    Comment(String),
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

/// Elements whose content is raw text (no tags inside). `noscript` belongs
/// here because scripting is always enabled in this engine: per HTML, a
/// scripting-enabled parser keeps `<noscript>` content as text, so a
/// `<style>` inside it is never collected and a `<meta http-equiv=refresh>`
/// inside it never navigates. (Pages use that to hide their non-JS layout -
/// google.com ships `table,div,span,p{display:none}` in one.)
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "svg", "noscript"];

/// Interned tag name: `u16` index into `Dom::tags` instead of a per-node String.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct TagId(u16);

pub enum NodeKind {
    Element(TagId, Box<[(String, String)]>),
    Text(String),
    /// Comment body without the `<!--` / `-->` delimiters. Layout skips
    /// these (no box, no text), but scripts see them as `nodeType === 8`
    /// with a writable `data`, and sibling navigation counts them.
    Comment(String),
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

    /// Intern (or look up) a tag name. Script `createElement` grows the
    /// table on demand — any name is legal, like the real API.
    pub fn tag_id(&mut self, name: &str) -> TagId {
        self.intern(name)
    }

    pub fn tag(&self, idx: u32) -> &str {
        match &self.nodes[idx as usize].kind {
            NodeKind::Element(t, _) => &self.tags[t.0 as usize],
            NodeKind::Text(_) => "#text",
            NodeKind::Comment(_) => "#comment",
        }
    }

    /// True for real elements: skips text, comments and the `#document`
    /// sentinel at index 0.
    pub fn is_element(&self, idx: u32) -> bool {
        match &self.nodes[idx as usize].kind {
            NodeKind::Element(t, _) => self.tags[t.0 as usize].as_ref() != "#document",
            _ => false,
        }
    }

    /// Attribute value for an element node (case-insensitive name).
    pub fn attr(&self, idx: u32, name: &str) -> Option<&str> {
        match &self.nodes[idx as usize].kind {
            NodeKind::Element(_, attrs) => attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str()),
            _ => None,
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
            Token::Comment(t) => {
                let parent = *stack.last().unwrap();
                dom.add(parent, NodeKind::Comment(t));
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
                    // keep the body: scripts read and rewrite marker comments
                    out.push(Token::Comment(s[i + 4..i + 4 + e].to_string()));
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
                let end = raw_text_end(s, i, &name);
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

/// End offset of a RAW_TEXT element's content. `<svg>` nests (SVG-in-SVG is
/// legal and common: icon sprites, `<symbol>` wrappers), so its close tag is
/// matched with a depth counter instead of the first `</svg>`. Every other
/// raw-text element (`script`, `style`, `textarea`, `title`) cannot nest.
fn raw_text_end(s: &str, from: usize, name: &str) -> usize {
    if name != "svg" {
        let lower = s[from..].to_ascii_lowercase();
        return match lower.find(&format!("</{name}")) {
            Some(c) => from + c,
            None => s.len(),
        };
    }
    let bytes = s.as_bytes();
    let mut depth = 1usize;
    let mut i = from;
    while i < bytes.len() {
        let Some(rel) = s[i..].find('<') else { return s.len() };
        let lt = i + rel;
        let Some(gt_rel) = s[lt..].find('>') else { return s.len() };
        let gt = lt + gt_rel;
        let tag = s[lt + 1..gt].trim_start();
        let closing = tag.starts_with('/');
        let tag = tag.trim_start_matches('/').trim_start();
        let name_end = tag.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(tag.len());
        let tname = &tag[..name_end];
        if tname.eq_ignore_ascii_case("svg") {
            if closing {
                depth -= 1;
                if depth == 0 {
                    return lt;
                }
            } else if !tag.trim_end().ends_with('/') {
                depth += 1;
            }
        }
        i = gt + 1;
    }
    s.len()
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

// ---------- snapshot serialization (Phase 3) ----------
// A parsed DOM is the expensive half of a navigation (fetch + tokenize +
// tree build). Serializing it lets a tab switch restore the tree instead of
// re-fetching. Layout is cheap and viewport-dependent, so it is NOT stored.

/// On-disk snapshot version. Bump whenever the byte layout changes: a stale
/// file then fails the version check and is treated as a miss, never as a
/// mis-parse.
pub const SNAP_VERSION: u16 = 2;
const SNAP_MAGIC: &[u8; 4] = b"MDOM";
/// Refuse absurd node counts before allocating (corrupt/hostile file).
const SNAP_MAX_NODES: usize = 4_000_000;
const SNAP_MAX_ATTRS: usize = 1_000_000;

fn snap_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Length-prefixed UTF-8 (u32 length: attribute values can be data: URLs).
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// Bounds-checked reader: every accessor returns None past the end.
struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n)?;
        let s = self.b.get(self.p..end)?;
        self.p = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        Some(std::str::from_utf8(self.take(n)?).ok()?.to_string())
    }
}

impl Dom {
    /// Serialize the whole arena: magic + version + tag table + nodes, with a
    /// trailing FNV-1a hash so a truncated/bit-rotted file is a clean miss.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.nodes.len() * 12 + 64);
        out.extend_from_slice(SNAP_MAGIC);
        put_u16(&mut out, SNAP_VERSION);
        put_u32(&mut out, self.tags.len() as u32);
        for t in &self.tags {
            put_str(&mut out, t);
        }
        put_u32(&mut out, self.nodes.len() as u32);
        for n in &self.nodes {
            put_u32(&mut out, n.parent);
            put_u32(&mut out, n.children.len() as u32);
            for &c in &n.children {
                put_u32(&mut out, c);
            }
            match &n.kind {
                NodeKind::Element(t, attrs) => {
                    out.push(0);
                    put_u16(&mut out, t.0);
                    put_u32(&mut out, attrs.len() as u32);
                    for (k, v) in attrs.iter() {
                        put_str(&mut out, k);
                        put_str(&mut out, v);
                    }
                }
                NodeKind::Text(s) => {
                    out.push(1);
                    put_str(&mut out, s);
                }
                NodeKind::Comment(s) => {
                    out.push(2);
                    put_str(&mut out, s);
                }
            }
        }
        let h = snap_hash(&out);
        out.extend_from_slice(&h.to_le_bytes());
        out
    }

    /// Inverse of `to_bytes`. None on any structural problem (bad magic,
    /// wrong version, hash mismatch, out-of-range index, invalid UTF-8,
    /// trailing bytes) — the caller treats None as a cache miss.
    pub fn from_bytes(data: &[u8]) -> Option<Dom> {
        // magic + version + hash at minimum
        if data.len() < SNAP_MAGIC.len() + 2 + 8 {
            return None;
        }
        let (body, tail) = data.split_at(data.len() - 8);
        let want = u64::from_le_bytes(tail.try_into().ok()?);
        if snap_hash(body) != want {
            return None;
        }
        let mut r = Reader { b: body, p: 0 };
        if r.take(SNAP_MAGIC.len())? != SNAP_MAGIC {
            return None;
        }
        if r.u16()? != SNAP_VERSION {
            return None;
        }
        let ntags = r.u32()? as usize;
        if ntags > u16::MAX as usize + 1 {
            return None;
        }
        let mut tags: Vec<Box<str>> = Vec::with_capacity(ntags.min(4096));
        let mut tag_index = std::collections::HashMap::new();
        for i in 0..ntags {
            let s: Box<str> = r.str()?.into();
            // a duplicate would make tag ids ambiguous
            if tag_index.insert(s.clone(), i as u16).is_some() {
                return None;
            }
            tags.push(s);
        }
        let nnodes = r.u32()? as usize;
        if nnodes == 0 || nnodes > SNAP_MAX_NODES {
            return None;
        }
        let mut nodes: Vec<Node> = Vec::with_capacity(nnodes.min(65_536));
        for _ in 0..nnodes {
            let parent = r.u32()?;
            let nc = r.u32()? as usize;
            if nc > nnodes {
                return None;
            }
            let mut children = Vec::with_capacity(nc.min(4096));
            for _ in 0..nc {
                children.push(r.u32()?);
            }
            let kind = match r.u8()? {
                0 => {
                    let t = r.u16()?;
                    if t as usize >= tags.len() {
                        return None;
                    }
                    let na = r.u32()? as usize;
                    if na > SNAP_MAX_ATTRS {
                        return None;
                    }
                    let mut attrs = Vec::with_capacity(na.min(1024));
                    for _ in 0..na {
                        attrs.push((r.str()?, r.str()?));
                    }
                    NodeKind::Element(TagId(t), attrs.into_boxed_slice())
                }
                1 => NodeKind::Text(r.str()?),
                2 => NodeKind::Comment(r.str()?),
                _ => return None,
            };
            nodes.push(Node { parent, children, kind });
        }
        if r.p != body.len() {
            return None; // trailing junk
        }
        // links must be consistent in both directions; otherwise layout's
        // parent walks and the hit test would see a tree that never existed
        for (i, n) in nodes.iter().enumerate() {
            if n.parent as usize >= nnodes || (i != 0 && n.parent == i as u32) {
                return None;
            }
            for &c in &n.children {
                let c = c as usize;
                if c >= nnodes || nodes[c].parent != i as u32 {
                    return None;
                }
            }
        }
        if nodes[0].parent != 0 {
            return None;
        }
        Some(Dom { nodes, tags, tag_index })
    }
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
    fn nested_svg_raw_text_keeps_inner_markup() {
        // A `<svg>` inside `<svg>` is legal; its close tag must not end the
        // outer element's RAW_TEXT payload at the first `</svg>`.
        let html = r#"<body><svg viewBox="0 0 24 24"><svg width="12" height="12"><rect width="12" height="12"/></svg><circle cx="6" cy="6" r="3"/></svg><p>after</p></body>"#;
        let toks = tokenize(html);
        let payload = toks
            .iter()
            .find_map(|t| match t {
                Token::Text(s) if s.contains("<rect") => Some(s),
                _ => None,
            })
            .expect("svg raw-text payload");
        assert!(payload.contains("</svg>"), "inner close kept: {payload:?}");
        assert!(payload.contains("<circle"), "markup after nested svg kept: {payload:?}");
        assert!(
            toks.iter().any(|t| matches!(t, Token::StartTag { name, .. } if name == "p")),
            "document continues after the outer </svg>"
        );
    }

    #[test]
    fn comments_are_nodes_and_survive_snapshot_round_trip() {
        let dom = parse("<body><!--$?--><template id='B:0'></template><!--/$--><p>x</p></body>");
        let comments: Vec<&str> = dom
            .nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Comment(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(comments, vec!["$?", "/$"]);
        // the boundary template's previous sibling in the parent's child list
        // is the `$?` marker, and it must not be reported as an element
        let tmpl = (0..dom.nodes.len() as u32)
            .find(|&i| dom.attr(i, "id") == Some("B:0"))
            .expect("template");
        let siblings = &dom.nodes[dom.nodes[tmpl as usize].parent as usize].children;
        let pos = siblings.iter().position(|&c| c == tmpl).unwrap();
        assert_eq!(
            dom.tag(siblings[pos - 1]),
            "#comment",
            "marker precedes the template"
        );
        assert!(!dom.is_element(siblings[pos - 1]));
        // text/comment bytes survive the DOM snapshot cache
        let back = Dom::from_bytes(&dom.to_bytes()).expect("snapshot round trip");
        let back_comments: Vec<&str> = back
            .nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Comment(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(back_comments, vec!["$?", "/$"]);
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
    fn noscript_content_is_inert_because_scripting_is_on() {
        // google.com ships its non-JS fallback as a <noscript> holding a
        // <style> that hides the page and a meta refresh to the basic
        // version. With scripting enabled both must stay text.
        let dom = parse(
            "<body><noscript><style>div{display:none}</style><p>fallback</p>\
             </noscript><p>live</p></body>",
        );
        let noscript = (0..dom.nodes.len())
            .find(|&i| dom.tag(i as u32) == "noscript")
            .expect("noscript element");
        let kids = &dom.nodes[noscript].children;
        assert_eq!(kids.len(), 1, "noscript should own exactly its text");
        match &dom.nodes[kids[0] as usize].kind {
            NodeKind::Text(t) => assert!(t.contains("<style>"), "raw text: {t:?}"),
            _ => panic!("noscript content must be a text node"),
        }
        // nothing inside it became an element
        assert_eq!(
            (0..dom.nodes.len())
                .filter(|&i| dom.tag(i as u32) == "style")
                .count(),
            0,
            "the noscript <style> must not be a stylesheet"
        );
        assert_eq!(
            (0..dom.nodes.len())
                .filter(|&i| dom.tag(i as u32) == "p")
                .count(),
            1,
            "the noscript <p> must not exist"
        );
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

    /// Every `is_text` leaf in a laid box tree (what paint emits as runs).
    fn text_leaves(n: &crate::layout::BoxNode, out: &mut Vec<String>) {
        if n.is_text && !n.text.is_empty() {
            out.push(n.text.clone());
        }
        for c in &n.children {
            text_leaves(c, out);
        }
    }

    /// Sheet from every live `<style>` block in document order (what
    /// main.rs `collect_sheets` does over the post-hoist tree).
    fn collect_style_text(dom: &Dom) -> String {
        let mut css = String::new();
        for i in 0..dom.nodes.len() as u32 {
            if dom.tag(i) == "style" {
                for &c in &dom.nodes[i as usize].children {
                    if let NodeKind::Text(t) = &dom.nodes[c as usize].kind {
                        css.push_str(t);
                        css.push('\n');
                    }
                }
            }
        }
        css
    }

    #[test]
    fn dsd_hoist_binds_shadow_styles_and_keeps_svg_payload() {
        // MDN header shape: a custom element's declarative shadow root
        // hoists a <style> whose rules target the hoisted buttons, plus a
        // <button> wrapping an icon <svg>. Two invariants the hoist must
        // guarantee:
        //   1. the hoisted <style> still cascades onto the hoisted
        //      elements (the style element becomes a live sibling; flat
        //      expansion must not orphan or re-order it);
        //   2. the <svg> payload (one RAW_TEXT child of <svg>) survives
        //      hoisting untouched, exactly where layout's inline-svg image
        //      pipeline reassembles it.
        let html = r#"<body><mdn-search-button><template shadowroot="open" shadowrootmode="open"><style>.mdn-search-button{display:flex;justify-content:space-between}</style><button class="mdn-search-button" type="button"><svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/></svg></button></template></mdn-search-button></body>"#;
        let dom = parse(html);
        // (1) hoisted style cascades onto the hoisted button
        let sheet = crate::css::parse_for(&collect_style_text(&dom), 1000.0);
        let btn = find_tag(&dom, "button");
        let out = sheet.resolve(&dom, btn, crate::layout::Style::new(), dom.attr(btn, "style"));
        let flex = out.bx.display;
        assert!(
            matches!(flex, crate::css::Display::Flex),
            "hoisted shadow style must cascade onto the hoisted button, got {flex:?}"
        );
        // the flat expansion hoisted the whole shadow payload into the host
        let host = find_tag(&dom, "mdn-search-button");
        assert_eq!(child_tags(&dom, host), ["style", "button"]);
        // (2) svg payload intact under <svg> after hoisting
        let svg = find_tag(&dom, "svg");
        let payload: String = dom.nodes[svg as usize]
            .children
            .iter()
            .filter_map(|&c| match &dom.nodes[c as usize].kind {
                NodeKind::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            payload.contains("<path d=\"m21 21-4.34-4.34\"/><circle cx=\"11\" cy=\"11\" r=\"8\"/>"),
            "svg shape markup must remain the svg element's RAW_TEXT child: {payload:?}"
        );
    }

    #[test]
    fn hoisted_shadow_svg_markup_never_becomes_text() {
        // Reproduce (%TEMP%/mazda-mdn4.png): the shadow-root template of
        // <mdn-search-button> hoists a <button> wrapping an icon <svg>.
        // `<svg>` is RAW_TEXT, so the shape markup lives as one text child
        // of <svg>; it must never surface as a paintable text run. Fixed in
        // layout.rs `contained_text`: the element arm skips `svg` (and for
        // the same class `script`/`style`) subtrees - markup content of svg
        // elements is never text. The svg image pipeline is unaffected: it
        // reads the svg element's own text children directly, not through
        // contained_text.
        let html = r#"<body><header><mdn-search-button><template shadowroot="open" shadowrootmode="open"><style>.mdn-search-button{width:5rem;border-radius:100px}</style><button class="mdn-search-button" title="Search the site"><svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" viewBox="0 0 24 24"><path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/></svg></button></template></mdn-search-button></header></body>"#;
        let dom = parse(html);
        let sheet = crate::css::parse_for("", 1000.0);
        let root = dom.body_root();
        let tree = crate::layout::build(
            &dom,
            root,
            crate::layout::Style::new(),
            crate::css::BoxStyle::default(),
            &sheet,
        );
        let mut runs = Vec::new();
        text_leaves(&tree, &mut runs);
        for r in &runs {
            assert!(
                !(r.contains("<path") || r.contains("<circle") || r.contains("m21 21")),
                "svg markup leaked into a text run: {r:?} (all: {runs:?})"
            );
        }
    }

    #[test]
    fn plain_svg_never_leaks_into_a_button_label() {
        // Same bug class with no shadow root at all: <button><svg/></button>
        // is everywhere in real markup (icon buttons) - proof the DSD hoist
        // is NOT the mechanism. Fixed in layout.rs `contained_text`; see the
        // previous test.
        let html = r#"<body><button><svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/></svg>ok</button></body>"#;
        let dom = parse(html);
        let sheet = crate::css::parse_for("", 1000.0);
        let root = dom.body_root();
        let tree = crate::layout::build(
            &dom,
            root,
            crate::layout::Style::new(),
            crate::css::BoxStyle::default(),
            &sheet,
        );
        let mut runs = Vec::new();
        text_leaves(&tree, &mut runs);
        assert!(
            runs.iter().all(|r| !(r.contains("<path") || r.contains("<circle"))),
            "svg markup leaked into text runs: {runs:?}"
        );
        assert!(runs.iter().any(|r| r.contains("ok")), "button label text must survive: {runs:?}");
    }

    #[test]
    fn snapshot_round_trip_preserves_the_tree() {
        let html = r#"<body class="a b"><p id=x data-v="1 2">hi &amp; bye</p><img src="data:image/png;base64,AAA"><div><span>deep</span></div></body>"#;
        let dom = parse(html);
        let bytes = dom.to_bytes();
        let back = Dom::from_bytes(&bytes).expect("snapshot must load");

        assert_eq!(back.nodes.len(), dom.nodes.len());
        assert_eq!(back.tags.len(), dom.tags.len());
        for i in 0..dom.nodes.len() {
            let (a, b) = (&dom.nodes[i], &back.nodes[i]);
            assert_eq!(a.parent, b.parent, "parent of node {i}");
            assert_eq!(a.children, b.children, "children of node {i}");
            assert_eq!(dom.tag(i as u32), back.tag(i as u32), "tag of node {i}");
            match (&a.kind, &b.kind) {
                (NodeKind::Text(x), NodeKind::Text(y)) => assert_eq!(x, y),
                (NodeKind::Element(ta, aa), NodeKind::Element(tb, ab)) => {
                    assert_eq!(ta.0, tb.0);
                    assert_eq!(aa, ab, "attrs of node {i}");
                }
                _ => panic!("kind changed at node {i}"),
            }
        }
        // the interned tag table is rebuilt: attribute lookup still works
        assert_eq!(back.attr(1, "CLASS"), Some("a b"));
        assert_eq!(back.attr(2, "data-v"), Some("1 2"));
        assert_eq!(back.body_root(), dom.body_root());
    }

    #[test]
    fn snapshot_rejects_corrupt_or_truncated_files() {
        let dom = parse("<body><p>ok</p></body>");
        let bytes = dom.to_bytes();

        // truncated at every interesting boundary: always None, never a panic
        for cut in [0, 3, 4, 6, 9, bytes.len() / 2, bytes.len() - 1] {
            assert!(Dom::from_bytes(&bytes[..cut]).is_none(), "truncation at {cut}");
        }
        // a flipped payload byte breaks the trailing hash
        let mut flipped = bytes.clone();
        flipped[10] ^= 0xFF;
        assert!(Dom::from_bytes(&flipped).is_none());
        // a flipped hash byte breaks it too
        let mut bad_hash = bytes.clone();
        let last = bad_hash.len() - 1;
        bad_hash[last] ^= 0x01;
        assert!(Dom::from_bytes(&bad_hash).is_none());
        // trailing junk is not silently accepted
        let mut junk = bytes.clone();
        junk.push(0);
        assert!(Dom::from_bytes(&junk).is_none());
        // wrong magic
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert!(Dom::from_bytes(&magic).is_none());
    }

    #[test]
    fn snapshot_rejects_inconsistent_links() {
        // Build a valid snapshot, then re-hash a hand-edited tree: a child
        // whose parent points elsewhere must be refused, not laid out.
        let dom = parse("<body><p>x</p></body>");
        let mut bytes = dom.to_bytes();
        let body_len = bytes.len() - 8;
        let body = &mut bytes[..body_len];
        // node 1 = <body>, node 2 = <p>: rewrite node 2's parent to 0
        // (layout + hit test assume parent/child agreement)
        let pos = node_start(body, 2);
        body[pos..pos + 4].copy_from_slice(&0u32.to_le_bytes());
        let h = snap_hash(body);
        let n = bytes.len();
        bytes[n - 8..].copy_from_slice(&h.to_le_bytes());
        assert!(Dom::from_bytes(&bytes).is_none());
    }

    /// Byte offset of node `idx` in a snapshot body (test-only helper: walks
    /// the variable-length records instead of guessing).
    fn node_start(body: &[u8], idx: usize) -> usize {
        let mut r = Reader { b: body, p: 0 };
        r.take(4).unwrap();
        r.u16().unwrap();
        let ntags = r.u32().unwrap();
        for _ in 0..ntags {
            r.str().unwrap();
        }
        r.u32().unwrap(); // node count
        for _ in 0..idx {
            skip_node(&mut r);
        }
        r.p
    }

    fn skip_node(r: &mut Reader) {
        r.u32().unwrap(); // parent
        for _ in 0..r.u32().unwrap() {
            r.u32().unwrap();
        }
        match r.u8().unwrap() {
            0 => {
                r.u16().unwrap();
                for _ in 0..r.u32().unwrap() {
                    r.str().unwrap();
                    r.str().unwrap();
                }
            }
            _ => {
                r.str().unwrap();
            }
        }
    }
}

