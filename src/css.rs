//! CSS subset (Phase 1b, PLAN Recommendations §2): stylesheet text is parsed
//! ONCE into a rule list with precompiled selectors — never re-parsed per element.
//! shortcut: no pseudo-classes, no @media/@import, no !important (stripped and
//! ignored), shorthand `font:` unsupported; background-color is only honored
//! for the page background (body/html), not per-box. Box layout properties
//! (display:flex/grid, width, gap, …) cascade into `BoxStyle` — layout.rs
//! consumes it. Upgrade when the CSS engine grows (Phase 1c+).

use crate::html::Dom;
use crate::layout::{Fam, Style};

pub type Decl = (String, String);

/// A resolved length. `%` stays symbolic — the container width isn't known
/// until layout walks the tree.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Len {
    Px(f32),
    Pct(f32),
}

/// One `grid-template-columns` track.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Track {
    Fr(f32),
    Px(f32),
    Pct(f32),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Display {
    /// no `display:` declaration — follow the tag (block tag → block box,
    /// inline tag → flatten into the parent's text flow)
    Auto,
    Inline,
    Block,
    Flex,
    Grid,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Dir {
    Row,
    Column,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Justify {
    Start,
    Center,
    End,
    SpaceBetween,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    Start,
    Center,
    End,
    Stretch,
}

/// CSS `position`. `sticky` maps to `Relative` (shortcut: no scroll
/// threshold — it just offsets in flow).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Pos {
    Static,
    Relative,
    Absolute,
    Fixed,
}

/// Per-element box layout properties — the non-inherited half of CSS.
/// Default = plain block flow (matches pre-flex rendering).
#[derive(Clone, PartialEq, Debug)]
pub struct BoxStyle {
    pub display: Display,
    pub dir: Dir,
    pub justify: Justify,
    pub align: Align,
    pub gap_row: f32,
    pub gap_col: f32,
    pub cols: Vec<Track>,
    pub width: Option<Len>,
    pub max_width: Option<Len>,
    /// explicit height — shortcut: only consulted for replaced (`<img>`) boxes
    pub height: Option<Len>,
    pub margin_x_auto: bool,
    /// top, right, bottom, left (CSS shorthand order)
    pub pad: [Len; 4],
    /// top, right, bottom, left — `margin_x_auto` overrides left/right
    pub margin: [Len; 4],
    /// per-box background-color
    pub bg: Option<u32>,
    /// border-radius (all corners; % resolves against box width at layout)
    pub radius: Len,
    /// border widths, top/right/bottom/left (% resolves against box width)
    pub border: [Len; 4],
    /// border color; None = currentcolor (the element's own text color,
    /// resolved at paint time — never a hardcoded default)
    pub bcolor: Option<u32>,
    /// CSS position (non-static boxes leave normal flow)
    pub pos: Pos,
    /// top, right, bottom, left — None = unspecified (absolute keeps its
    /// provisional position, relative doesn't shift on that axis)
    pub inset: [Option<Len>; 4],
    /// `box-shadow` list, first = topmost (paint order reversed)
    pub shadows: Vec<Shadow>,
}

/// One outer `box-shadow`: offset, blur, spread, color. `color: None` =
/// currentcolor (the element's text color, resolved at paint time).
/// shortcut: `inset` shadows are dropped (no inner-shadow rendering).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Shadow {
    pub dx: f32,
    pub dy: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Option<u32>,
    /// shadow opacity 0-255 (CSS shadows carry their own alpha)
    pub alpha: u8,
}

impl Default for BoxStyle {
    fn default() -> Self {
        BoxStyle {
            display: Display::Auto,
            dir: Dir::Row,
            justify: Justify::Start,
            align: Align::Stretch,
            gap_row: 0.0,
            gap_col: 0.0,
            cols: Vec::new(),
            width: None,
            max_width: None,
            height: None,
            margin_x_auto: false,
            pad: [Len::Px(0.0); 4],
            margin: [Len::Px(0.0); 4],
            bg: None,
            radius: Len::Px(0.0),
            border: [Len::Px(0.0); 4],
            bcolor: None, // currentcolor
            pos: Pos::Static,
            inset: [None; 4],
            shadows: Vec::new(),
        }
    }
}

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
    /// non-inherited box layout properties (flex/grid/width/gap/…)
    pub bx: BoxStyle,
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
    "text-align",
    "flex-direction",
    "justify-content",
    "align-items",
    "gap",
    "row-gap",
    "column-gap",
    "grid-template-columns",
    "width",
    "max-width",
    "height",
    "padding",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "margin-top",
    "margin-bottom",
    "line-height",
    "margin",
    "margin-left",
    "margin-right",
    "margin-inline",
    "margin-inline-start",
    "margin-inline-end",
    "border-radius",
    "border",
    "border-width",
    "border-color",
    "border-top",
    "border-right",
    "border-bottom",
    "border-left",
    "position",
    "top",
    "right",
    "bottom",
    "left",
    "inset",
    "box-shadow",
];

/// Parse a stylesheet (concatenated `<style>` + fetched sheets) into a
/// cascade-sorted rule list, assuming a 1000px desktop viewport.
#[cfg(test)] // production uses parse_for (media queries need the viewport)
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
        let mut bx = BoxStyle::default();

        for rule in &self.rules {
            if !matches(dom, node, &rule.segs) {
                continue;
            }
            for d in &rule.decls {
                if apply_decl(d, &mut style, &mut bg, &mut bx) {
                    display_none = true;
                }
            }
        }
        if let Some(inline) = inline {
            // inline style = specificity (1,0,0,0): applied last, wins
            for d in &parse_decls(inline) {
                if apply_decl(d, &mut style, &mut bg, &mut bx) {
                    display_none = true;
                }
            }
        }
        StyleOutcome {
            style,
            bg,
            display_none,
            bx,
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

fn apply_decl(d: &Decl, style: &mut Style, bg: &mut Option<u32>, bx: &mut BoxStyle) -> bool {
    match d.0.as_str() {
        "color" => {
            if let Some(c) = parse_color(&d.1) {
                style.color = c;
            }
        }
        "background" | "background-color" => {
            if let Some(c) = parse_color(&d.1) {
                *bg = Some(c);
                bx.bg = Some(c); // per-box fill (cards, nav bars)
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
        "font-family" => style.fam = parse_fam(&d.1),
        "text-decoration" => style.underline = d.1.contains("underline"),
        "line-height" => {
            // unitless = multiplier of font-size; px/rem/% absolute (of size)
            style.lh = if let Some(n) = d.1.strip_suffix('%') {
                n.trim().parse::<f32>()
                    .map(|f| f / 100.0 * style.size)
                    .unwrap_or(0.0)
            } else if d.1.trim().ends_with("px") {
                d.1.trim().trim_end_matches("px").trim().parse::<f32>().unwrap_or(0.0)
            } else if let Ok(n) = d.1.trim().parse::<f32>() {
                if n > 0.0 && n < 10.0 {
                    n * style.size // unitless multiplier (1.5, 1.75 …)
                } else {
                    0.0 // nonsense → default
                }
            } else {
                0.0
            };
        }
        "display" => match d.1.as_str() {
            "none" => return true,
            "flex" => bx.display = Display::Flex,
            "grid" => bx.display = Display::Grid,
            "block" => bx.display = Display::Block,
            "inline" | "inline-block" | "inline-flex" | "inline-grid" => bx.display = Display::Inline,
            _ => {}
        },
        // shadows carry their own alpha (rgba() is the common case) and may
        // stack — parse the whole list, first shadow paints on top
        "box-shadow" => bx.shadows = parse_box_shadows(&d.1),
        // fully transparent = invisible; shortcut: partial opacity ignored
        // (rasterizer has no alpha compositing), so only 0 hides the subtree
        "opacity" => return d.1.parse::<f32>().map(|v| v <= 0.0).unwrap_or(false),
        "text-align" => {
            style.text_align = match d.1.as_str() {
                "center" => 1,
                "right" | "end" => 2,
                _ => 0, // left/start; shortcut: justify renders as left
            }
        }
        "flex-direction" => {
            bx.dir = if d.1.contains("column") { Dir::Column } else { Dir::Row }
        }
        "justify-content" => {
            bx.justify = match d.1.as_str() {
                "center" => Justify::Center,
                "flex-end" | "end" | "right" => Justify::End,
                "space-between" | "space-around" | "space-evenly" => Justify::SpaceBetween,
                _ => Justify::Start, // flex-start/left/default
            }
        }
        "align-items" => {
            bx.align = match d.1.as_str() {
                "center" => Align::Center,
                "flex-end" | "end" | "bottom" => Align::End,
                "stretch" => Align::Stretch,
                _ => Align::Start, // flex-start/baseline/default
            }
        }
        "gap" => {
            let mut it = d.1.split_whitespace();
            if let (Some(a), b) = (it.next(), it.next()) {
                if let Some(v) = parse_len(a) {
                    bx.gap_row = len_px(v, 0.0);
                    bx.gap_col = len_px(v, 0.0);
                }
                if let Some(v) = b.and_then(parse_len) {
                    bx.gap_col = len_px(v, 0.0); // `gap: row col`
                }
            }
        }
        "row-gap" => {
            if let Some(v) = parse_len(&d.1) {
                bx.gap_row = len_px(v, 0.0);
            }
        }
        "column-gap" => {
            if let Some(v) = parse_len(&d.1) {
                bx.gap_col = len_px(v, 0.0);
            }
        }
        "grid-template-columns" => bx.cols = parse_tracks(&d.1),
        "width" => bx.width = parse_len(&d.1).filter(|_| d.1 != "auto"),
        "height" => bx.height = parse_len(&d.1).filter(|_| d.1 != "auto"),
        "max-width" => {
            bx.max_width = if d.1 == "none" {
                None
            } else {
                parse_len(&d.1)
            }
        }
        // shortcut: only the auto (centering) half of margin is honored —
        // fixed margins/padding per side don't exist yet
        "margin" => {
            let v = parse_sides(&d.1);
            bx.margin = v;
            if d.1.contains("auto") {
                // auto in a horizontal slot → centering shortcut
                bx.margin_x_auto = true;
            }
        }
        "padding" => bx.pad = parse_sides(&d.1),
        "margin-top" => bx.margin[0] = parse_len(&d.1).unwrap_or(Len::Px(0.0)),
        "margin-bottom" => bx.margin[2] = parse_len(&d.1).unwrap_or(Len::Px(0.0)),
        "margin-left" | "margin-inline" | "margin-inline-start" => {
            if d.1.contains("auto") {
                bx.margin_x_auto = true;
            } else {
                bx.margin[3] = parse_len(&d.1).unwrap_or(Len::Px(0.0));
            }
        }
        "margin-right" | "margin-inline-end" => {
            if d.1.contains("auto") {
                bx.margin_x_auto = true;
            } else {
                bx.margin[1] = parse_len(&d.1).unwrap_or(Len::Px(0.0));
            }
        }
        "padding-top" => bx.pad[0] = parse_len(&d.1).unwrap_or(Len::Px(0.0)),
        "padding-right" => bx.pad[1] = parse_len(&d.1).unwrap_or(Len::Px(0.0)),
        "padding-bottom" => bx.pad[2] = parse_len(&d.1).unwrap_or(Len::Px(0.0)),
        "padding-left" => bx.pad[3] = parse_len(&d.1).unwrap_or(Len::Px(0.0)),
        // first value only (per-corner radii: shortcut)
        "border-radius" | "border-top-left-radius" => {
            if let Some(v) = d.1.split_whitespace().next().and_then(parse_len) {
                bx.radius = v;
            }
        }
        // style words (solid/dashed/dotted) ignored — everything renders solid;
        // the full shorthand resets color to currentcolor when unspecified
        // (spec cascade), per-side shorthands only touch color when given
        "border" => {
            let (w, c) = border_parts(&d.1);
            if let Some(w) = w {
                bx.border = [w; 4];
            }
            bx.bcolor = c;
        }
        "border-width" => bx.border = parse_sides(&d.1),
        "border-color" => {
            if let Some(c) = parse_color(&d.1) {
                bx.bcolor = Some(c);
            }
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let side = match d.0.as_str() {
                "border-top" => 0,
                "border-right" => 1,
                "border-bottom" => 2,
                _ => 3,
            };
            let (w, c) = border_parts(&d.1);
            if let Some(w) = w {
                bx.border[side] = w;
            }
            if c.is_some() {
                bx.bcolor = c;
            }
        }
        "position" => {
            bx.pos = match d.1.trim() {
                "relative" | "sticky" => Pos::Relative, // sticky: shortcut = relative
                "absolute" => Pos::Absolute,
                "fixed" => Pos::Fixed,
                _ => Pos::Static, // static / bogus
            }
        }
        // insets — `auto`/bogus parse to None (absolute keeps its provisional
        // position on that edge, relative doesn't shift along it)
        "top" => bx.inset[0] = parse_len(&d.1),
        "right" => bx.inset[1] = parse_len(&d.1),
        "bottom" => bx.inset[2] = parse_len(&d.1),
        "left" => bx.inset[3] = parse_len(&d.1),
        // inset shorthand: 1–4 values, `auto` slots stay None (unlike
        // margin/padding where 0 is the fallback — None means "unset edge")
        "inset" => {
            let vals: Vec<Option<Len>> = d.1.split_whitespace().map(parse_len).collect();
            bx.inset = match vals.len() {
                1 => [vals[0]; 4],
                2 => [vals[0], vals[1], vals[0], vals[1]],
                3 => [vals[0], vals[1], vals[2], vals[1]],
                4 => [vals[0], vals[1], vals[2], vals[3]],
                _ => [None; 4],
            };
        }
        _ => {}
    }
    false
}

/// `border` shorthand value → (width, color); `None` in either slot means
/// "not specified" (color falls back to currentcolor at paint time).
/// Style words (`solid`, `dashed`, …) are ignored — everything renders solid
/// (shortcut); `none` resets the width to 0.
fn border_parts(v: &str) -> (Option<Len>, Option<u32>) {
    let mut w = None;
    let mut c = None;
    for t in v.split_whitespace() {
        if t.eq_ignore_ascii_case("none") {
            w = Some(Len::Px(0.0));
        } else if let Some(l) = parse_len(t) {
            w = Some(l);
        } else if let Some(col) = parse_color(t) {
            c = Some(col);
        }
    }
    (w, c)
}

/// CSS side shorthand (`margin`/`padding`: 1–4 whitespace-separated values)
/// → [top, right, bottom, left].
fn parse_sides(v: &str) -> [Len; 4] {
    let z = Len::Px(0.0);
    let vals: Vec<Len> = v.split_whitespace().filter_map(parse_len).collect();
    match vals.len() {
        1 => [vals[0], vals[0], vals[0], vals[0]],
        2 => [vals[0], vals[1], vals[0], vals[1]],
        3 => [vals[0], vals[1], vals[2], vals[1]],
        4 => [vals[0], vals[1], vals[2], vals[3]],
        _ => [z, z, z, z], // includes `auto` slots (handled separately)
    }
}

/// `200px` / `50%` / `1.5rem` → [`Len`]. Bare numbers are lenient px
/// (matches `parse_size`); `em`≈rem shortcut (needs element font size).
fn parse_len(v: &str) -> Option<Len> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix('%') {
        return n.trim().parse::<f32>().ok().map(Len::Pct);
    }
    if let Some(n) = v.strip_suffix("rem") {
        return n.trim().parse::<f32>().ok().map(|f| Len::Px(f * 16.0));
    }
    if let Some(n) = v.strip_suffix("em") {
        return n.trim().parse::<f32>().ok().map(|f| Len::Px(f * 16.0));
    }
    if let Some(n) = v.strip_suffix("px") {
        return n.trim().parse::<f32>().ok().map(Len::Px);
    }
    v.parse::<f32>().ok().map(Len::Px)
}

/// Symbolic length → px against a known container width (Pct resolves here).
fn len_px(l: Len, container: f32) -> f32 {
    match l {
        Len::Px(p) => p,
        Len::Pct(p) => p / 100.0 * container,
    }
}

/// `grid-template-columns`: `1fr 1fr 1fr`, `repeat(3, 1fr)`,
/// `minmax(0, 1fr)`, `200px 50%`. Empty = single implicit column.
/// shortcut: `repeat(auto-fill, …)` and named lines are dropped.
fn parse_tracks(v: &str) -> Vec<Track> {
    let mut out = Vec::new();
    let v = v.trim();
    if v.is_empty() || v == "none" {
        return out;
    }
    // expand repeat(n, …) and minmax(a, b) first so tokens are space-free
    let mut s = String::new();
    let mut i = 0;
    let b = v.as_bytes();
    while i < b.len() {
        if v[i..].starts_with("repeat(") {
            let open = i + 6;
            let Some(close) = match_paren(b, open) else { break };
            let inner = &v[open + 1..close];
            let Some((n, list)) = inner.split_once(',') else { break };
            if let Ok(n) = n.trim().parse::<usize>() {
                for _ in 0..n.min(64) {
                    s.push_str(list);
                    s.push(' ');
                }
            }
            i = close + 1;
        } else if v[i..].starts_with("minmax(") {
            // minmax(a, b) → take the max side (b); handles inner spaces
            let open = i + 6; // index of '('
            let Some(close) = match_paren(b, open) else { break };
            let inner = &v[open + 1..close];
            if let Some((_, max)) = inner.rsplit_once(',') {
                s.push_str(max.trim());
                s.push(' ');
            }
            i = close + 1;
        } else {
            s.push(b[i] as char);
            i += 1;
        }
    }
    for tok in s.split_whitespace() {
        let tok = tok.trim_end_matches(',');
        if let Some(n) = tok.strip_suffix("fr") {
            if let Ok(f) = n.trim().parse::<f32>() {
                out.push(Track::Fr(f.max(0.0)));
            }
        } else if let Some(l) = parse_len(tok) {
            out.push(match l {
                Len::Px(p) => Track::Px(p),
                Len::Pct(p) => Track::Pct(p),
            });
        }
        // unknown tokens (auto, named lines) are skipped
    }
    out
}

/// Index of the `)` matching the `(` at `open`.
fn match_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0u32;
    for i in open..b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `16px`, `1.2em` (relative to current size), `120%`, bare number (lenient px).
fn parse_size(v: &str, current: f32) -> Option<f32> {
    if let Some(n) = v.strip_suffix("px") {
        return n.trim().parse().ok();
    }
    if let Some(n) = v.strip_suffix("rem") {
        return n.trim().parse::<f32>().ok().map(|f| f * 16.0); // root = 16px
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
    // rgb()/rgba() — alpha 0 = transparent (dropped), alpha > 0 renders
    // opaque (the rasterizer has no compositing; documented shortcut)
    let inner = v
        .strip_prefix("rgba(")
        .or_else(|| v.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'));
    if let Some(inner) = inner {
        let mut parts = inner.split(',');
        let r = parts.next()?.trim().parse::<u32>().ok()?;
        let g = parts.next()?.trim().parse::<u32>().ok()?;
        let b = parts.next()?.trim().parse::<u32>().ok()?;
        if r > 255 || g > 255 || b > 255 {
            return None;
        }
        if v.starts_with("rgba(") {
            if let Some(a) = parts.next().and_then(|s| s.trim().parse::<f32>().ok()) {
                if a <= 0.0 {
                    return None; // transparent
                }
            }
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

/// `font-family` stack → first recognized generic family. Walks the list in
/// order (CSS picks the first available font); unquoted multi-word names are
/// matched by keyword. Unknown webfonts/sans stacks → Sans.
fn parse_fam(v: &str) -> Fam {
    for name in v.split(',') {
        let n = name.trim().trim_matches('"').trim_matches('\'').to_ascii_lowercase();
        // mono before serif: "SFMono-Regular" etc. contain neither, but be safe
        if n.contains("mono")
            || n.contains("courier")
            || n.contains("consol")
            || n.contains("menlo")
            || n.contains("monaco")
            || n.contains("code")
        {
            return Fam::Mono;
        }
        // `sans-serif` also contains "serif" — check the negative first
        if n.contains("sans") || n.contains("arial") || n.contains("helvetica") {
            return Fam::Sans;
        }
        if n.contains("serif")
            || n.contains("georgia")
            || n.contains("times")
            || n.contains("garamond")
            || n.contains("palatino")
            || n.contains("cambria")
            || n.contains("constantia")
        {
            return Fam::Serif;
        }
    }
    Fam::Sans
}

/// Like [`parse_color`] but keeps alpha — shadows need real opacity
/// (an `rgba(0,0,0,0.4)` shadow at full opacity looks like a sticker).
/// Returns (rgb, alpha 0-255); None = unparseable.
fn parse_color_alpha(v: &str) -> Option<(u32, u8)> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix('#') {
        let hex = hex.trim();
        let digits = u32::from_str_radix(hex, 16).ok()?;
        return match hex.len() {
            3 => parse_color(v).map(|c| (c, 255)),
            4 => {
                let (r, g, b, a) = (
                    (digits >> 8) & 0xf,
                    (digits >> 4) & 0xf,
                    digits & 0xf,
                    (digits >> 12) & 0xf,
                );
                let f = |n: u32| (n << 4 | n) as u8;
                Some(((f(r) as u32) << 16 | (f(g) as u32) << 8 | f(b) as u32, f(a)))
            }
            6 => Some((digits, 255)),
            8 => Some((digits >> 8, (digits & 0xff) as u8)),
            _ => None,
        };
    }
    let is_rgba = v.starts_with("rgba(");
    let inner = v
        .strip_prefix("rgba(")
        .or_else(|| v.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'));
    if let Some(inner) = inner {
        let mut parts = inner.split(',');
        let r = parts.next()?.trim().parse::<u32>().ok()?;
        let g = parts.next()?.trim().parse::<u32>().ok()?;
        let b = parts.next()?.trim().parse::<u32>().ok()?;
        if r > 255 || g > 255 || b > 255 {
            return None;
        }
        let a = if is_rgba {
            (parts.next().and_then(|s| s.trim().parse::<f32>().ok()).unwrap_or(1.0).clamp(0.0, 1.0)
                * 255.0)
                .round() as u8
        } else {
            255
        };
        return Some((r << 16 | g << 8 | b, a));
    }
    parse_color(v).map(|c| (c, 255))
}

/// `box-shadow` value → list, first = topmost (paint order). `none`, empty
/// and unparseable → empty list. `inset` shadows are dropped (shortcut: no
/// inner-shadow rendering).
fn parse_box_shadows(v: &str) -> Vec<Shadow> {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    // split on top-level commas (rgba() has its own, inside parens)
    let mut parts: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in v.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    parts.iter().filter_map(|p| parse_shadow(p)).collect()
}

/// One comma-separated shadow: `dx dy [blur] [spread] [color]` in any
/// token order (color first or last, as sites write it).
fn parse_shadow(v: &str) -> Option<Shadow> {
    let mut lens = [0.0f32; 4];
    let mut n_lens = 0usize;
    let mut color = None;
    // re-join whitespace-split tokens that sit inside `rgba(...)` parens
    let mut toks: Vec<String> = Vec::new();
    let mut depth = 0usize;
    for t in v.split_whitespace() {
        if !toks.is_empty() && depth > 0 {
            toks.last_mut().unwrap().push_str(t);
        } else {
            toks.push(t.to_string());
        }
        depth += t.matches('(').count();
        depth = depth.saturating_sub(t.matches(')').count());
    }
    for t in toks {
        if t.eq_ignore_ascii_case("inset") {
            return None; // inner shadow: dropped (shortcut)
        }
        if let Some(l) = parse_len(&t) {
            if n_lens < 4 {
                lens[n_lens] = match l {
                    Len::Px(p) => p,
                    Len::Pct(_) => 0.0, // % of what? no resolved width here — ignore
                };
                n_lens += 1;
            }
        } else if color.is_none() {
            color = parse_color_alpha(&t);
        }
    }
    if n_lens < 2 {
        return None; // need at least the two offsets
    }
    Some(Shadow {
        dx: lens[0],
        dy: lens[1],
        blur: lens[2].max(0.0),
        spread: lens[3],
        color: color.map(|c| c.0),
        alpha: color.map(|c| c.1).unwrap_or(255),
    })
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
                    if i < b.len() && Some(b[i]) == quote {
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

fn media_len(v: &str) -> Option<f32> {
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
            "min-width" => media_len(v).map(|px| vw >= px).unwrap_or(false),
            "max-width" => media_len(v).map(|px| vw <= px).unwrap_or(false),
            _ => false,
        };
    }
    // range syntax (Media Queries L4 / Tailwind v4): (width >= 48rem),
    // (640px > width), (48rem <= width)
    for op in [">=", "<=", ">", "<"] {
        if let Some((l, r)) = inner.split_once(op) {
            let num = |s: &str| {
                let s = s.trim();
                if s == "width" { Some(vw) } else { media_len(s) }
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
        assert!(s.bold && s.italic && s.fam == Fam::Mono && s.underline);
    }

    #[test]
    fn font_family_stack_resolves_to_generic() {
        let sheet = parse(
            ".a { font-family: Georgia, 'Times New Roman', serif } \
             .b { font-family: 'Helvetica Neue', Arial, sans-serif } \
             .c { font-family: 'Fira Code', Menlo, monospace } \
             .d { font-family: Inter, system-ui, sans-serif }",
        );
        let dom = dom_of(
            r#"<div class=a></div><div class=b></div><div class=c></div><div class=d></div>"#,
        );
        let get = |c: &str| sheet.resolve(&dom, by_attr(&dom, "class", c), Style::new(), None).style;
        assert_eq!(get("a").fam, Fam::Serif, "georgia keyword");
        assert_eq!(get("b").fam, Fam::Sans, "helvetica keyword");
        assert_eq!(get("c").fam, Fam::Mono, "menlo keyword before generic");
        assert_eq!(get("d").fam, Fam::Sans, "unknown webfonts fall through to sans");
        // sans-serif must not be misread as serif
        assert_eq!(parse_fam("sans-serif"), Fam::Sans);
        assert_eq!(parse_fam("serif"), Fam::Serif);
        assert_eq!(parse_fam("ui-monospace"), Fam::Mono);
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

    // ---- box properties (Track A) ----

    #[test]
    fn tracks_parse_fr_px_pct_and_repeat() {
        assert_eq!(
            parse_tracks("repeat(3, 1fr)"),
            vec![Track::Fr(1.0), Track::Fr(1.0), Track::Fr(1.0)]
        );
        assert_eq!(
            parse_tracks("200px 50% 1fr"),
            vec![Track::Px(200.0), Track::Pct(50.0), Track::Fr(1.0)]
        );
        // minmax(0, 1fr) → take the max side
        assert_eq!(parse_tracks("minmax(0, 1fr)"), vec![Track::Fr(1.0)]);
        assert!(parse_tracks("none").is_empty());
        // garbage tokens are skipped, valid ones survive
        assert_eq!(parse_tracks("[line1] auto 2fr"), vec![Track::Fr(2.0)]);
    }

    #[test]
    fn len_parses_px_pct_and_bare() {
        assert_eq!(parse_len("12px"), Some(Len::Px(12.0)));
        assert_eq!(parse_len("50%"), Some(Len::Pct(50.0)));
        assert_eq!(parse_len("24"), Some(Len::Px(24.0)));
        assert_eq!(parse_len("auto"), None);
        assert_eq!(parse_len("1.5rem"), Some(Len::Px(24.0))); // rem = 16px root
    }

    #[test]
    fn box_props_cascade_into_boxstyle() {
        let sheet = parse(
            ".row { display: flex; flex-direction: column; gap: 8px 12px; justify-content: center; align-items: end }\
             .grid { display: grid; grid-template-columns: repeat(2, 1fr); column-gap: 5px }\
             .box { width: 300px; max-width: 50%; margin: 0 auto }\
             .inl { display: inline }",
        );
        let dom = dom_of(
            r#"<div class=row></div><div class=grid></div><div class=box></div><div class=inl></div><div class=nocls></div>"#,
        );
        let bx = |cls: &str| sheet.resolve(&dom, by_attr(&dom, "class", cls), Style::new(), None).bx;

        let row = bx("row");
        assert_eq!(row.display, Display::Flex);
        assert_eq!(row.dir, Dir::Column);
        assert_eq!(row.gap_row, 8.0);
        assert_eq!(row.gap_col, 12.0, "`gap: row col` splits");
        assert_eq!(row.justify, Justify::Center);
        assert_eq!(row.align, Align::End);

        let grid = bx("grid");
        assert_eq!(grid.display, Display::Grid);
        assert_eq!(grid.cols, vec![Track::Fr(1.0), Track::Fr(1.0)]);
        assert_eq!(grid.gap_col, 5.0);
        assert_eq!(grid.gap_row, 0.0);

        let k = bx("box");
        assert_eq!(k.width, Some(Len::Px(300.0)));
        assert_eq!(k.max_width, Some(Len::Pct(50.0)));
        assert!(k.margin_x_auto, "`margin: 0 auto` → auto centering");

        assert_eq!(bx("inl").display, Display::Inline);
        // no declarations → Auto (follow the tag)
        let plain = sheet.resolve(&dom, by_attr(&dom, "class", "nocls"), Style::new(), None);
        assert_eq!(plain.bx.display, Display::Auto);
    }

    #[test]
    fn padding_margin_lineheight_background_cascade() {
        let sheet = parse(
            ".card { padding: 16px 24px; margin: 8px auto; background-color: #222222; line-height: 1.6 }\
             .hd { font-size: 2rem; line-height: 40px; margin-top: 1.5rem; padding-bottom: 0.5rem }",
        );
        let dom = dom_of(r#"<div class=card></div><div class=hd></div>"#);

        let card = sheet.resolve(&dom, by_attr(&dom, "class", "card"), Style::new(), None);
        assert_eq!(card.bx.pad, [Len::Px(16.0), Len::Px(24.0), Len::Px(16.0), Len::Px(24.0)]);
        assert!(card.bx.margin_x_auto, "`margin: 8px auto` → auto");
        assert_eq!(card.bx.margin[0], Len::Px(8.0));
        assert_eq!(card.bx.bg, Some(0x222222));
        assert!((card.style.lh - 1.6 * 15.0).abs() < 0.01, "lh={}", card.style.lh);

        // rem = 16px root
        let hd_style = Style::new();
        let hd = sheet.resolve(&dom, by_attr(&dom, "class", "hd"), hd_style, None);
        assert!((hd.style.size - 32.0).abs() < 0.01, "size={}", hd.style.size);
        assert!((hd.style.lh - 40.0).abs() < 0.01, "lh={}", hd.style.lh);
        assert_eq!(hd.bx.margin[0], Len::Px(24.0)); // 1.5rem
        assert_eq!(hd.bx.pad[2], Len::Px(8.0)); // 0.5rem

        // per-side shorthands
        let sheet2 = parse(".s { padding: 1px 2px 3px 4px; margin-bottom: 9px }");
        let dom2 = dom_of(r#"<div class=s></div>"#);
        let s = sheet2.resolve(&dom2, by_attr(&dom2, "class", "s"), Style::new(), None);
        assert_eq!(
            s.bx.pad,
            [Len::Px(1.0), Len::Px(2.0), Len::Px(3.0), Len::Px(4.0)]
        );
        assert_eq!(s.bx.margin[2], Len::Px(9.0));
    }

    #[test]
    fn border_radius_and_rgba() {
        let sheet = parse(".c { border-radius: 12px; background: rgba(255, 0, 0, 0.5) }\
                           .t { background: rgba(0, 0, 0, 0) }");
        let dom = dom_of(r#"<div class=c></div><div class=t></div>"#);
        let c = sheet.resolve(&dom, by_attr(&dom, "class", "c"), Style::new(), None);
        assert_eq!(c.bx.radius, Len::Px(12.0));
        assert_eq!(c.bx.bg, Some(0xff0000), "rgba alpha>0 renders opaque");
        let t = sheet.resolve(&dom, by_attr(&dom, "class", "t"), Style::new(), None);
        assert_eq!(t.bx.bg, None, "rgba(…,0) = transparent = no fill");
    }

    #[test]
    fn border_shorthand_and_per_side() {
        let sheet = parse(
            ".b { border: 1px solid #eaeaea }\
             .t { border: 2px solid red; border-width: 0 0 3px }\
             .n { border: none }\
             .k { color: #123456; border: 1px solid }",
        );
        let dom = dom_of(r#"<div class=b></div><div class=t></div><div class=n></div><div class=k></div>"#);
        let b = sheet.resolve(&dom, by_attr(&dom, "class", "b"), Style::new(), None);
        assert_eq!(b.bx.border, [Len::Px(1.0); 4]);
        assert_eq!(b.bx.bcolor, Some(0xeaeaea), "hex color");
        // later border-width overrides the shorthand's side list
        let t = sheet.resolve(&dom, by_attr(&dom, "class", "t"), Style::new(), None);
        assert_eq!(
            t.bx.border,
            [Len::Px(0.0), Len::Px(0.0), Len::Px(3.0), Len::Px(0.0)]
        );
        assert_eq!(t.bx.bcolor, Some(0xff0000), "named color kept from shorthand");
        let n = sheet.resolve(&dom, by_attr(&dom, "class", "n"), Style::new(), None);
        assert_eq!(n.bx.border, [Len::Px(0.0); 4], "border: none resets widths");
        assert_eq!(n.bx.bcolor, None, "border: none resets color to currentcolor");
        let k = sheet.resolve(&dom, by_attr(&dom, "class", "k"), Style::new(), None);
        assert_eq!(k.bx.bcolor, None, "unspecified color stays currentcolor");
        assert_eq!(k.style.color, 0x123456, "…which is the element's text color");
    }

    #[test]
    fn position_and_insets_parse() {
        let sheet = parse(
            ".a { position: absolute; top: 10px; left: 20% }\
             .r { position: relative; bottom: 5px }\
             .f { position: fixed; right: 1em }\
             .s { position: sticky; top: 0 }\
             .d { position: static }\
             .u { position: bogus; top: auto }",
        );
        let dom = dom_of(
            r#"<div class=a></div><div class=r></div><div class=f></div><div class=s></div><div class=d></div><div class=u></div>"#,
        );
        let get = |c: &str| sheet.resolve(&dom, by_attr(&dom, "class", c), Style::new(), None);
        let a = get("a");
        assert_eq!(a.bx.pos, Pos::Absolute);
        assert_eq!(a.bx.inset[0], Some(Len::Px(10.0)), "top");
        assert_eq!(a.bx.inset[3], Some(Len::Pct(20.0)), "left %");
        assert_eq!(a.bx.inset[1], None, "right unspecified");
        let r = get("r");
        assert_eq!(r.bx.pos, Pos::Relative);
        assert_eq!(r.bx.inset[2], Some(Len::Px(5.0)), "bottom");
        let f = get("f");
        assert_eq!(f.bx.pos, Pos::Fixed);
        assert_eq!(f.bx.inset[1], Some(Len::Px(16.0)), "right 1em");
        // sticky → relative (shortcut: no scroll threshold)
        assert_eq!(get("s").bx.pos, Pos::Relative);
        assert_eq!(get("d").bx.pos, Pos::Static);
        // unknown keyword and unparseable `auto` → static/None
        let u = get("u");
        assert_eq!(u.bx.pos, Pos::Static, "bogus keyword ignored");
        assert_eq!(u.bx.inset[0], None, "auto → None");
    }

    #[test]
    fn inset_shorthand() {
        let sheet = parse(".a { inset: 0 } .b { inset: 1px 2px } .c { inset: 1px auto }");
        let dom = dom_of(r#"<div class=a></div><div class=b></div><div class=c></div>"#);
        let get = |c: &str| sheet.resolve(&dom, by_attr(&dom, "class", c), Style::new(), None);
        assert_eq!(get("a").bx.inset, [Some(Len::Px(0.0)); 4], "one value → all sides");
        assert_eq!(
            get("b").bx.inset,
            [Some(Len::Px(1.0)), Some(Len::Px(2.0)), Some(Len::Px(1.0)), Some(Len::Px(2.0))],
            "two values → tb lr"
        );
        assert_eq!(get("c").bx.inset[1], None, "auto slot stays None");
        assert_eq!(get("c").bx.inset[0], Some(Len::Px(1.0)));
    }

    #[test]
    fn box_shadow_parse() {
        let sheet = parse(".a { box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25) }");
        let dom = dom_of("<div class=a></div>");
        let s = sheet.resolve(&dom, by_attr(&dom, "class", "a"), Style::new(), None);
        let sh = &s.bx.shadows;
        assert_eq!(sh.len(), 1);
        assert_eq!((sh[0].dx, sh[0].dy, sh[0].blur, sh[0].spread), (0.0, 4.0, 12.0, 0.0));
        assert_eq!(sh[0].color, Some(0x000000), "rgba color");
        assert_eq!(sh[0].alpha, 64, "0.25 × 255 ≈ 64");
    }

    #[test]
    fn box_shadow_color_anywhere_and_defaults() {
        // color before the lengths (sites write both orders); no color →
        // currentcolor (None), default alpha 255
        let sheet = parse(".a { box-shadow: #ff0000 1px 2px 3px 4px } .b { box-shadow: 1px 2px }");
        let dom = dom_of("<div class=a></div><div class=b></div>");
        let get = |c: &str| sheet.resolve(&dom, by_attr(&dom, "class", c), Style::new(), None);
        let a = &get("a").bx.shadows[0];
        assert_eq!(a.color, Some(0xff0000));
        assert_eq!((a.dx, a.dy, a.blur, a.spread), (1.0, 2.0, 3.0, 4.0));
        assert_eq!(a.alpha, 255);
        let b = &get("b").bx.shadows[0];
        assert_eq!(b.color, None, "no color → currentcolor");
        assert_eq!((b.dx, b.dy, b.blur, b.spread), (1.0, 2.0, 0.0, 0.0));
    }

    #[test]
    fn box_shadow_multi_inset_none() {
        // first shadow paints on top; `inset` shadows are dropped (shortcut);
        // `none` and empty → no shadows
        let sheet = parse(
            ".a { box-shadow: 0 1px #111111, 0 2px 4px #222222, inset 0 3px #333333 } \
             .b { box-shadow: none } .c { }",
        );
        let dom = dom_of("<div class=a></div><div class=b></div><div class=c></div>");
        let get = |c: &str| sheet.resolve(&dom, by_attr(&dom, "class", c), Style::new(), None);
        let a = &get("a").bx.shadows;
        assert_eq!(a.len(), 2, "inset dropped");
        assert_eq!(a[0].color, Some(0x111111), "first = topmost");
        assert_eq!(a[1].color, Some(0x222222));
        assert!(get("b").bx.shadows.is_empty(), "none");
        assert!(get("c").bx.shadows.is_empty(), "absent");
    }
}


