//! CSS subset (Phase 1b, PLAN Recommendations §2): stylesheet text is parsed
//! ONCE into a rule list with precompiled selectors — never re-parsed per element.
//! shortcut: no pseudo-classes, no @media/@import, no !important (stripped and
//! ignored), no external <link> stylesheets, shorthand `font:` unsupported;
//! background-color is only honored for the page background (body/html), not
//! per-box (no box tree yet). Upgrade when the CSS engine grows (Phase 1b+).

use crate::html::Dom;
use crate::layout::Style;

pub type Decl = (String, String);

/// One compound step of a selector, e.g. `nav > a.current[href]`.
struct Seg {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<(String, Option<String>)>,
    /// joined to the left segment by `>` (else descendant)
    child: bool,
}

struct Rule {
    segs: Vec<Seg>,
    decls: Vec<Decl>,
    /// (ids, classes+attrs, tags) — cascade order key
    spec: (u16, u16, u16),
    order: u32,
}

pub struct Sheet {
    /// sorted by (spec, order) at parse time; cascade = apply matches in order
    rules: Vec<Rule>,
}

impl Sheet {
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// Result of cascading all rules + inline style onto one element's base style.
pub struct StyleOutcome {
    pub style: Style,
    /// background-color (only meaningful for body/html — page fill)
    pub bg: Option<u32>,
    pub display_none: bool,
}

/// Properties we actually use; everything else (margin, padding, …) is dropped
/// at parse time to keep the rule list small.
const KNOWN: &[&str] = &[
    "color",
    "background",
    "background-color",
    "font-size",
    "font-weight",
    "font-style",
    "font-family",
    "text-decoration",
    "display",
    "opacity",
];

/// Parse a stylesheet (concatenated `<style>` + fetched sheets) into a
/// cascade-sorted rule list, assuming a 1000px desktop viewport.
pub fn parse(text: &str) -> Sheet {
    parse_for(text, 1000.0)
}

/// Like [`parse`], but media queries are evaluated against `vw` (CSS pixels)
/// at build time — blocks that don't match are never parsed, so they cost
/// nothing per element. shortcut: no re-evaluation on resize (sheets are
/// parsed once per page load; Mazda's window is fixed-size).
pub fn parse_for(text: &str, vw: f32) -> Sheet {
    let s = strip_comments(text);
    let b = s.as_bytes();
    let mut rules: Vec<Rule> = Vec::new();
    let mut order = 0u32;
    let mut i = 0usize;

    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'}' {
            // stray closer (end of a media/layer block we parsed into) —
            // never glue it onto the next selector
            i += 1;
            continue;
        }
        if c == b'@' {
            i = handle_at_rule(&s, b, i, vw);
            continue;
        }
        let Some(ob) = s[i..].find('{') else { break };
        let Some(cb) = match_brace(b, i + ob) else { break };
        let sel_text = &s[i..i + ob];
        let block = &s[i + ob + 1..cb];
        let decls = parse_decls(block);
        if !decls.is_empty() {
            for sel in sel_text.split(',') {
                if let Some(segs) = parse_selector(sel) {
                    let spec = spec_of(&segs);
                    rules.push(Rule {
                        segs,
                        decls: decls.clone(),
                        spec,
                        order,
                    });
                    order += 1;
                }
                // selectors we can't parse (pseudo-classes etc.) are dropped,
                // but the rule still applies via its other comma-separated parts
            }
        }
        i = cb + 1;
    }

    rules.sort_by_key(|r| (r.spec, r.order));
    Sheet { rules }
}

/// Position just past an at-rule. `@media`/`@layer` blocks whose condition
/// matches are entered (their inner rules parse normally); everything else
/// (`@keyframes`, `@supports`, `@font-face`, unmatched `@media`, statements
/// like `@import`) is skipped whole.
fn handle_at_rule(s: &str, b: &[u8], at: usize, vw: f32) -> usize {
    let mut ne = at + 1;
    while ne < b.len() && (b[ne].is_ascii_alphabetic() || b[ne] == b'-') {
        ne += 1;
    }
    let name = &s[at + 1..ne];
    // prelude ends at the first `{` (block) or `;` (statement)
    let mut pe = at;
    while pe < b.len() && b[pe] != b'{' && b[pe] != b';' {
        pe += 1;
    }
    if pe >= b.len() {
        return b.len();
    }
    if b[pe] == b';' {
        return pe + 1; // @import …;  /  @layer a, b;
    }
    let transparent = match name {
        "media" => media_matches(&s[ne..pe], vw),
        // shortcut: layer order ≈ source order (cross-layer specificity off)
        "layer" => true,
        _ => false,
    };
    if transparent {
        pe + 1 // parse inside; the closing '}' is consumed as a stray
    } else {
        skip_at_rule(s, at)
    }
}

/// Index of the `}` matching the `{` at `open` (quote-aware); None if open.
fn match_brace(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0u32;
    let mut i = open;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' => quote = Some(c),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

impl Sheet {
    /// Cascade every matching rule + the inline `style` attribute over `base`.
    pub fn resolve(&self, dom: &Dom, node: u32, base: Style, inline: Option<&str>) -> StyleOutcome {
        let mut style = base;
        let mut bg = None;
        let mut display_none = false;

        for rule in &self.rules {
            if !matches(dom, node, &rule.segs) {
                continue;
            }
            for d in &rule.decls {
                if apply_decl(d, &mut style, &mut bg) {
                    display_none = true;
                }
            }
        }
        if let Some(inline) = inline {
            // inline style = specificity (1,0,0,0): applied last, wins
            for d in &parse_decls(inline) {
                if apply_decl(d, &mut style, &mut bg) {
                    display_none = true;
                }
            }
        }
        StyleOutcome {
            style,
            bg,
            display_none,
        }
    }
}

/// Parse a declaration block: `color: #fff; font-size: 16px` (no braces).
pub fn parse_decls(block: &str) -> Vec<Decl> {
    let mut out = Vec::new();
    for part in block.split(';') {
        let Some((p, v)) = part.split_once(':') else {
            continue;
        };
        let p = p.trim().to_ascii_lowercase();
        if !KNOWN.contains(&p.as_str()) {
            continue;
        }
        let mut v = v.trim().to_ascii_lowercase();
        if let Some(pos) = v.find("!important") {
            v.truncate(pos); // shortcut: !important ignored entirely
        }
        let v = v.trim().to_string();
        if !v.is_empty() {
            out.push((p, v));
        }
    }
    out
}

fn apply_decl(d: &Decl, style: &mut Style, bg: &mut Option<u32>) -> bool {
    match d.0.as_str() {
        "color" => {
            if let Some(c) = parse_color(&d.1) {
                style.color = c;
            }
        }
        "background" | "background-color" => {
            if let Some(c) = parse_color(&d.1) {
                *bg = Some(c);
            }
        }
        "font-size" => {
            if let Some(px) = parse_size(&d.1, style.size) {
                style.size = px.clamp(6.0, 400.0);
            }
        }
        "font-weight" => {
            if let Ok(n) = d.1.parse::<u16>() {
                style.bold = n >= 600;
            } else {
                match d.1.as_str() {
                    "bold" | "bolder" => style.bold = true,
                    "normal" | "lighter" => style.bold = false,
                    _ => {}
                }
            }
        }
        "font-style" => style.italic = matches!(d.1.as_str(), "italic" | "oblique"),
        "font-family" => style.mono = d.1.contains("mono") || d.1.contains("courier"),
        "text-decoration" => style.underline = d.1.contains("underline"),
        "display" => return d.1 == "none",
        // fully transparent = invisible; shortcut: partial opacity ignored
        // (rasterizer has no alpha compositing), so only 0 hides the subtree
        "opacity" => return d.1.parse::<f32>().map(|v| v <= 0.0).unwrap_or(false),
        _ => {}
    }
    false
}

/// `16px`, `1.2em` (relative to current size), `120%`, bare number (lenient px).
fn parse_size(v: &str, current: f32) -> Option<f32> {
    if let Some(n) = v.strip_suffix("px") {
        return n.trim().parse().ok();
    }
    if let Some(n) = v.strip_suffix("em") {
        return n.trim().parse::<f32>().ok().map(|f| f * current);
    }
    if let Some(n) = v.strip_suffix('%') {
        return n.trim().parse::<f32>().ok().map(|f| f / 100.0 * current);
    }
    v.trim().parse().ok()
}

/// `#rgb`, `#rrggbb`, `rgb(r,g,b)`, a few names. None = unparseable/transparent.
fn parse_color(v: &str) -> Option<u32> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix('#') {
        let hex = hex.trim();
        let digits = u32::from_str_radix(hex, 16).ok()?;
        return match hex.len() {
            3 => {
                let (r, g, b) = ((digits >> 8) & 0xf, (digits >> 4) & 0xf, digits & 0xf);
                Some((r << 4 | r) << 16 | (g << 4 | g) << 8 | (b << 4 | b))
            }
            6 => Some(digits),
            8 => Some(digits >> 8), // drop alpha, lenient
            _ => None,
        };
    }
    if let Some(inner) = v.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let mut parts = inner.split(',');
        let r = parts.next()?.trim().parse::<u32>().ok()?;
        let g = parts.next()?.trim().parse::<u32>().ok()?;
        let b = parts.next()?.trim().parse::<u32>().ok()?;
        if r > 255 || g > 255 || b > 255 {
            return None;
        }
        return Some(r << 16 | g << 8 | b);
    }
    match v {
        "black" => Some(0x000000),
        "white" => Some(0xffffff),
        "red" => Some(0xff0000),
        "green" => Some(0x008000),
        "lime" => Some(0x00ff00),
        "blue" => Some(0x0000ff),
        "yellow" => Some(0xffff00),
        "orange" => Some(0xffa500),
        "purple" => Some(0x800080),
        "gray" | "grey" => Some(0x808080),
        "cyan" | "aqua" => Some(0x00ffff),
        "magenta" | "fuchsia" => Some(0xff00ff),
        "brown" => Some(0xa52a2a),
        "pink" => Some(0xffc0cb),
        _ => None, // transparent, currentColor, var(), url() …
    }
}

// ---------- selector parsing (once per sheet) ----------

fn flush<'a>(tokens: &mut Vec<&'a str>, s: &'a str, start: usize, end: usize) {
    if end > start {
        tokens.push(&s[start..end]);
    }
}

/// True if the selector contains a `:` that starts a pseudo-class/element:
/// not a CSS escape (`\:`) and not inside `[...]` (attribute values may
/// legally contain colons, e.g. `[href^="http:"]`). Escaped colons belong to
/// class names like Tailwind's `.data-\[state\=closed\]\:hidden`.
fn has_pseudo(s: &str) -> bool {
    let mut esc = false;
    let mut depth = 0u32;
    for c in s.chars() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' => esc = true,
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

fn parse_selector(s: &str) -> Option<Vec<Seg>> {
    let s = s.trim();
    if s.is_empty() || has_pseudo(s) {
        return None; // pseudo-classes/elements unsupported (comment in module docs)
    }
    // tokenize on whitespace and '>'
    let mut tokens: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() || bytes[i] == b'>' {
            flush(&mut tokens, s, start, i);
            if bytes[i] == b'>' {
                tokens.push(">");
            }
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    flush(&mut tokens, s, start, i);

    let mut segs: Vec<Seg> = Vec::new();
    let mut pending_child = false;
    for t in tokens {
        if t == ">" {
            if segs.is_empty() {
                return None; // stray combinator
            }
            pending_child = true;
            continue;
        }
        let mut seg = parse_compound(t)?;
        seg.child = pending_child;
        pending_child = false;
        segs.push(seg);
    }
    if segs.is_empty() {
        None
    } else {
        Some(segs)
    }
}

fn parse_compound(s: &str) -> Option<Seg> {
    let mut seg = Seg {
        tag: None,
        id: None,
        classes: Vec::new(),
        attrs: Vec::new(),
        child: false,
    };
    let b: Vec<char> = s.chars().collect();
    let mut i = 0usize;

    if b.first() == Some(&'*') {
        i = 1;
    } else if b.first().map(|c| c.is_ascii_alphabetic() || *c == '_').unwrap_or(false) {
        let st = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '-' || b[i] == '_') {
            i += 1;
        }
        seg.tag = Some(b[st..i].iter().collect::<String>().to_ascii_lowercase());
    }

    while i < b.len() {
        match b[i] {
            '#' | '.' => {
                let is_id = b[i] == '#';
                i += 1;
                // scan with CSS escapes: `\[`, `\=`, `\:` are literal chars in
                // the name (Tailwind utility classes like `data-[x=y]:hidden`)
                let mut name = String::new();
                while i < b.len() {
                    match b[i] {
                        '\\' if i + 1 < b.len() => {
                            name.push(b[i + 1]);
                            i += 2;
                        }
                        '.' | '#' | '[' => break,
                        c => {
                            name.push(c);
                            i += 1;
                        }
                    }
                }
                if name.is_empty() {
                    return None;
                }
                if is_id {
                    seg.id = Some(name);
                } else {
                    seg.classes.push(name);
                }
            }
            '[' => {
                i += 1;
                let st = i;
                while i < b.len() && b[i] != ']' && b[i] != '=' {
                    i += 1;
                }
                let name: String = b[st..i].iter().collect::<String>().to_ascii_lowercase();
                let mut val = None;
                if i < b.len() && b[i] == '=' {
                    i += 1;
                    let quote = if i < b.len() && (b[i] == '"' || b[i] == '\'') {
                        let q = b[i];
                        i += 1;
                        Some(q)
                    } else {
                        None
                    };
                    let st = i;
                    while i < b.len() && b[i] != ']' && Some(b[i]) != quote {
                        i += 1;
                    }
                    val = Some(b[st..i].iter().collect::<String>().to_ascii_lowercase());
                    if Some(b[i]) == quote {
                        i += 1;
                    }
                }
                if i < b.len() && b[i] == ']' {
                    i += 1;
                }
                if name.is_empty() {
                    return None;
                }
                seg.attrs.push((name, val));
            }
            _ => return None, // unexpected char (e.g. pseudo sneaking past the filter)
        }
    }
    Some(seg)
}

fn spec_of(segs: &[Seg]) -> (u16, u16, u16) {
    let mut ids = 0u16;
    let mut cls = 0u16;
    let mut tags = 0u16;
    for s in segs {
        if s.id.is_some() {
            ids += 1;
        }
        cls += s.classes.len() as u16 + s.attrs.len() as u16;
        if s.tag.is_some() {
            tags += 1;
        }
    }
    (ids, cls, tags)
}

/// Right-to-left selector match against the ancestor chain.
fn matches(dom: &Dom, node: u32, segs: &[Seg]) -> bool {
    let mut idx = segs.len() - 1;
    let mut cur = node;
    if !seg_matches(dom, cur, &segs[idx]) {
        return false;
    }
    while idx > 0 {
        let need_child = segs[idx].child;
        idx -= 1;
        loop {
            let parent = dom.nodes[cur as usize].parent;
            if parent == 0 {
                return false; // reached document root
            }
            cur = parent;
            if seg_matches(dom, cur, &segs[idx]) {
                break;
            }
            if need_child {
                return false; // direct parent didn't match
            }
        }
    }
    true
}

fn seg_matches(dom: &Dom, node: u32, seg: &Seg) -> bool {
    if let Some(t) = &seg.tag {
        if dom.tag(node) != t {
            return false;
        }
    }
    if let Some(id) = &seg.id {
        if dom.attr(node, "id") != Some(id.as_str()) {
            return false;
        }
    }
    for c in &seg.classes {
        let has = dom
            .attr(node, "class")
            .map(|v| v.split_whitespace().any(|w| w == c))
            .unwrap_or(false);
        if !has {
            return false;
        }
    }
    for (name, want) in &seg.attrs {
        match dom.attr(node, name) {
            Some(v) => {
                if let Some(w) = want {
                    if !v.eq_ignore_ascii_case(w) {
                        return false;
                    }
                }
            }
            None => return false,
        }
    }
    true
}

// ---------- helpers ----------

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        match rest.find("*/") {
            Some(end) => rest = &rest[end + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------- media queries (evaluated once, at parse time) ----------

/// Comma-separated query list = OR. Used for `@media` preludes.
fn media_matches(list: &str, vw: f32) -> bool {
    list.split(',').any(|q| media_single(q.trim(), vw))
}

fn media_single(q: &str, vw: f32) -> bool {
    let q = q.trim();
    let q = q.strip_prefix("only ").unwrap_or(q).trim();
    if q.is_empty() || q == "all" || q == "screen" {
        return true;
    }
    if q == "print" {
        return false; // we render to a screen
    }
    if q.starts_with("not ") || q.contains(" or ") {
        return false; // unsupported → treat as non-match (block skipped)
    }
    q.split(" and ")
        .all(|part| media_feature(part.trim(), vw))
}

/// Root font size for rem/em lengths in media queries (CSS spec: root font).
const ROOT_PX: f32 = 16.0;

fn parse_len(v: &str) -> Option<f32> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix("px") {
        return n.trim().parse().ok();
    }
    if let Some(n) = v.strip_suffix("rem") {
        return n.trim().parse::<f32>().ok().map(|f| f * ROOT_PX);
    }
    if let Some(n) = v.strip_suffix("em") {
        return n.trim().parse::<f32>().ok().map(|f| f * ROOT_PX);
    }
    v.parse().ok() // bare number = px
}

/// One `(…)` feature. Unknown features return false (block skipped), which
/// is the safe direction: we never apply a stylesheet we don't understand.
fn media_feature(part: &str, vw: f32) -> bool {
    let p = part.trim();
    if p.is_empty() || p == "all" || p == "screen" {
        return true;
    }
    if p == "print" || p == "speech" {
        return false;
    }
    let Some(inner) = p.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
        return false;
    };
    let inner = inner.trim();
    // classic form: (hover: hover), (min-width: 48rem)
    if let Some((k, v)) = inner.split_once(':') {
        let (k, v) = (k.trim(), v.trim());
        return match k {
            // desktop assumptions: pointer device with hover
            "hover" => v == "hover",
            "pointer" => v == "fine",
            "orientation" => v == "landscape", // 1000x700 window
            "min-width" => parse_len(v).map(|px| vw >= px).unwrap_or(false),
            "max-width" => parse_len(v).map(|px| vw <= px).unwrap_or(false),
            _ => false,
        };
    }
    // range syntax (Media Queries L4 / Tailwind v4): (width >= 48rem),
    // (640px > width), (48rem <= width)
    for op in [">=", "<=", ">", "<"] {
        if let Some((l, r)) = inner.split_once(op) {
            let num = |s: &str| {
                let s = s.trim();
                if s == "width" { Some(vw) } else { parse_len(s) }
            };
            let (Some(a), Some(b)) = (num(l), num(r)) else {
                return false;
            };
            return match op {
                ">=" => a >= b,
                "<=" => a <= b,
                ">" => a > b,
                _ => a < b,
            };
        }
    }
    false
}

/// Position just past an at-rule (`@media {…}` or `@import …;`).
fn skip_at_rule(s: &str, at: usize) -> usize {
    let b = s.as_bytes();
    let mut i = at;
    while i < b.len() && b[i] != b'{' && b[i] != b';' {
        i += 1;
    }
    if i >= b.len() {
        return i;
    }
    if b[i] == b';' {
        return i + 1;
    }
    // block: brace-match
    let mut depth = 0u32;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn dom_of(markup: &str) -> html::Dom {
        html::parse(markup)
    }

    /// Index of the first node matching `pred` (as u32 arena index).
    fn find(dom: &html::Dom, pred: impl Fn(&html::Dom, u32) -> bool) -> u32 {
        (0..dom.nodes.len())
            .find(|&i| pred(dom, i as u32))
            .map(|i| i as u32)
            .expect("node not found")
    }

    fn by_tag(dom: &html::Dom, tag: &str) -> u32 {
        find(dom, |d, i| d.tag(i) == tag)
    }

    fn by_attr(dom: &html::Dom, name: &str, val: &str) -> u32 {
        find(dom, |d, i| d.attr(i, name) == Some(val))
    }

    /// Cascade and return the final style for the first element with this tag.
    fn style_for_tag(sheet: &Sheet, dom: &html::Dom, tag: &str) -> Style {
        let node = by_tag(dom, tag);
        sheet.resolve(dom, node, Style::new(), dom.attr(node, "style")).style
    }

    #[test]
    fn specificity_wins() {
        let sheet = parse("#a { color: #111111 } .b { color: #222222 } div { color: #333333 }");
        let dom = dom_of("<div id=a class=b></div>");
        assert_eq!(style_for_tag(&sheet, &dom, "div").color, 0x111111);
    }

    #[test]
    fn equal_specificity_uses_source_order() {
        let sheet = parse("p { color: #111111 } p { color: #222222 }");
        let dom = dom_of("<p></p>");
        assert_eq!(style_for_tag(&sheet, &dom, "p").color, 0x222222);
    }

    #[test]
    fn descendant_and_child_combinators() {
        let sheet = parse("nav a { color: #111111 } p > b { font-weight: bold } a { color: #999999 }");
        // a under nav: descendant rule (spec 0,1,2) vs plain a (0,0,1) → descendant wins
        let dom = dom_of("<nav><a id=x></a></nav><p><b id=y></b><span><b id=z></b></span></p>");
        let a = by_attr(&dom, "id", "x");
        assert_eq!(sheet.resolve(&dom, a, Style::new(), None).style.color, 0x111111);
        // direct child b matches p > b; nested b does not (but b UA default is bold anyway,
        // so assert via color instead with a dedicated rule)
        let sheet2 = parse("p > b { color: #111111 } b { color: #999999 }");
        let y = by_attr(&dom, "id", "y");
        let z = by_attr(&dom, "id", "z");
        assert_eq!(sheet2.resolve(&dom, y, Style::new(), None).style.color, 0x111111);
        assert_eq!(sheet2.resolve(&dom, z, Style::new(), None).style.color, 0x999999);
    }

    #[test]
    fn pseudo_selectors_dropped_others_kept() {
        let sheet = parse("a:hover { color: #111111 } p { color: #222222 }");
        let dom = dom_of("<p></p>");
        assert_eq!(style_for_tag(&sheet, &dom, "p").color, 0x222222);
    }

    #[test]
    fn inline_style_wins() {
        let sheet = parse("p { color: #111111; font-size: 10px }");
        let dom = dom_of(r#"<p style="color: #222222"></p>"#);
        let node = by_tag(&dom, "p");
        let out = sheet.resolve(&dom, node, Style::new(), dom.attr(node, "style"));
        assert_eq!(out.style.color, 0x222222);
        assert_eq!(out.style.size, 10.0); // non-overridden rule still applies
    }

    #[test]
    fn display_none_flag() {
        let sheet = parse("p { display: none }");
        let dom = dom_of("<p></p>");
        let node = by_tag(&dom, "p");
        assert!(sheet.resolve(&dom, node, Style::new(), None).display_none);
        let sheet2 = parse("p { display: block }");
        assert!(!sheet2.resolve(&dom, node, Style::new(), None).display_none);
    }

    #[test]
    fn comments_and_atrules_are_skipped() {
        let sheet = parse(
            "/* comment { bogus */ @media screen { p { color: #ff0000 } } p { color: #222222 }",
        );
        let dom = dom_of("<p></p>");
        assert_eq!(style_for_tag(&sheet, &dom, "p").color, 0x222222);
    }

    #[test]
    fn font_declarations() {
        let sheet = parse(
            "p { font-size: 2em; font-weight: 700; font-style: italic; font-family: Consolas, monospace; text-decoration: underline }",
        );
        let dom = dom_of("<p></p>");
        let s = style_for_tag(&sheet, &dom, "p");
        assert_eq!(s.size, 30.0); // 2em of 15px default
        assert!(s.bold && s.italic && s.mono && s.underline);
    }

    #[test]
    fn color_parsing() {
        assert_eq!(parse_color("#abc"), Some(0xaabbcc));
        assert_eq!(parse_color("#ff0000"), Some(0xff0000));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(0x010203));
        assert_eq!(parse_color("white"), Some(0xffffff));
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_color("var(--x)"), None);
    }

    #[test]
    fn class_matching_is_whitespace_split() {
        let sheet = parse(".btn.primary { color: #111111 }");
        let dom = dom_of(r#"<div class="btn big primary"></div>"#);
        assert_eq!(style_for_tag(&sheet, &dom, "div").color, 0x111111);
    }

    #[test]
    fn escaped_colon_class_matches_like_a_browser() {
        // real rule from nextjs.org's stylesheet that was hiding its dropdowns
        let sheet = parse(
            r".data-\[state\=closed\]\:hidden[data-state=closed]{display: none} p { color: #222222 }",
        );
        // element with the literal class token + attribute: hidden
        let dom = dom_of(
            r#"<div id=d1 data-state="closed" class="fixed data-[state=closed]:hidden"></div>"#,
        );
        assert!(sheet.resolve(&dom, by_attr(&dom, "id", "d1"), Style::new(), None).display_none);
        // same class but open state: visible
        let dom2 = dom_of(r#"<div id=d2 data-state="open" class="data-[state=closed]:hidden"></div>"#);
        assert!(!sheet.resolve(&dom2, by_attr(&dom2, "id", "d2"), Style::new(), None).display_none);
        // bare pseudo-class still dropped (and its sibling rule still applies)
        let dom3 = dom_of("<p></p>");
        assert_eq!(style_for_tag(&sheet, &dom3, "p").color, 0x222222);
    }

    #[test]
    fn colon_inside_attribute_value_is_not_a_pseudo() {
        assert!(!has_pseudo(r#"a[href^="http:"]"#));
        assert!(!has_pseudo(r#".data-\:x"#));
        assert!(has_pseudo("a:hover"));
        assert!(has_pseudo("li::before"));
        assert!(!has_pseudo(".plain .other"));
    }

    #[test]
    fn media_queries_evaluated_against_viewport() {
        // max-width block must NOT apply at 1000px…
        let sheet = parse("@media (max-width: 640px) { p { display: none } }");
        let dom = dom_of("<p></p>");
        assert!(!sheet.resolve(&dom, by_tag(&dom, "p"), Style::new(), None).display_none);
        // …but does at 500px
        let sheet500 = parse_for("@media (max-width: 640px) { p { display: none } }", 500.0);
        assert!(sheet500.resolve(&dom, by_tag(&dom, "p"), Style::new(), None).display_none);
        // Tailwind v4 range syntax: (width >= 48rem) = 768px
        let md = "@media (width >= 48rem) { p { color: #111111 } }";
        assert_eq!(style_for_tag(&parse(md), &dom, "p").color, 0x111111);
        let md700 = parse_for(md, 700.0);
        assert_eq!(md700.rule_count(), 0, "700px < 768px: block skipped");
        // hover capability (desktop assumption) + and-chains + comma OR
        let h = "@media (hover: hover) and (pointer: fine) { p { color: #111111 } }";
        assert_eq!(style_for_tag(&parse(h), &dom, "p").color, 0x111111);
        let or = "@media (scripting: none), (hover: hover) { p { color: #111111 } }";
        assert_eq!(style_for_tag(&parse(or), &dom, "p").color, 0x111111);
        // unsupported constructs skip the block (never applied blindly)
        let not = "@media not screen { p { color: #111111 } }";
        assert_eq!(parse(not).rule_count(), 0);
        // rules after the media block still parse (stray '}' consumed)
        let after = "@media (max-width: 640px) { p { display: none } } q { color: #222222 }";
        let q = dom_of("<q></q>");
        assert_eq!(style_for_tag(&parse(after), &q, "q").color, 0x222222);
    }

    #[test]
    fn nested_blocks_do_not_desync_the_parser() {
        // inner nested rule must not swallow what comes after it
        let sheet = parse(
            ".a { color: #111111 } .b { color: #222222; .c { color: #333333 } } .d { color: #444444 }",
        );
        let dom = dom_of("<div class=a></div><div class=b></div><div class=d></div>");
        assert_eq!(style_for_tag(&sheet, &dom, "div").color, 0x111111); // first .a
        let d = find(&dom, |dd, i| dd.attr(i, "class") == Some("d"));
        assert_eq!(sheet.resolve(&dom, d, Style::new(), None).style.color, 0x444444);
        // nested decls are dropped (documented shortcut), outer ones survive
        let b = find(&dom, |dd, i| dd.attr(i, "class") == Some("b"));
        assert_eq!(sheet.resolve(&dom, b, Style::new(), None).style.color, 0x222222);
    }

    #[test]
    fn layer_blocks_are_transparent() {
        // @layer contents parse as regular rules (order ≈ source order)
        let sheet = parse("@layer base { p { color: #111111 } }");
        let dom = dom_of("<p></p>");
        assert_eq!(style_for_tag(&sheet, &dom, "p").color, 0x111111);
        // statement form (@layer a, b;) is a no-op, following rules parse
        let sheet2 = parse("@layer theme, base; q { color: #222222 }");
        let q = dom_of("<q></q>");
        assert_eq!(style_for_tag(&sheet2, &q, "q").color, 0x222222);
    }

    #[test]
    fn opacity_zero_hides_subtree() {
        // real nextjs.org mobile drawer: hidden only by `opacity-0`
        let sheet = parse(".drawer { opacity: 0 } .faded { opacity: .5 } .shown { opacity: 1 }");
        let dom = dom_of("<div class=drawer></div><div class=faded></div><div class=shown></div>");
        assert!(sheet.resolve(&dom, by_attr(&dom, "class", "drawer"), Style::new(), None).display_none);
        // partial/full opacity keeps rendering (rasterizer has no alpha — documented shortcut)
        assert!(!sheet.resolve(&dom, by_attr(&dom, "class", "faded"), Style::new(), None).display_none);
        assert!(!sheet.resolve(&dom, by_attr(&dom, "class", "shown"), Style::new(), None).display_none);
        // inline style too
        let dom2 = dom_of(r#"<div id=x style="opacity:0"></div>"#);
        assert!(sheet.resolve(&dom2, by_attr(&dom2, "id", "x"), Style::new(), Some("opacity:0")).display_none);
    }

    #[test]
    fn attr_selectors() {
        let sheet = parse(r#"input[type=text] { color: #111111 } input[type=pass] { color: #222222 } input[disabled] { font-style: italic }"#);
        let dom = dom_of(r#"<input type=text><input type=pass><input disabled><input>"#);
        let inputs: Vec<u32> = dom
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| dom.tag(*i as u32) == "input")
            .map(|(i, _)| i as u32)
            .collect();
        assert_eq!(sheet.resolve(&dom, inputs[0], Style::new(), None).style.color, 0x111111);
        assert_eq!(sheet.resolve(&dom, inputs[1], Style::new(), None).style.color, 0x222222);
        assert!(sheet.resolve(&dom, inputs[2], Style::new(), None).style.italic);
        assert!(!sheet.resolve(&dom, inputs[3], Style::new(), None).style.italic);
    }
}


