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
    /// `flex-wrap: wrap` — items that don't fit start a new line (nowrap =
    /// shrink-to-fit, the old behavior)
    pub wrap: bool,
    pub gap_row: f32,
    pub gap_col: f32,
    pub cols: Vec<Track>,
    /// `[line-name]` boundaries from `grid-template-columns`
    /// (name → track index it precedes)
    pub col_lines: Vec<(String, usize)>,
    /// this grid item's `grid-column: <name>` — resolved against the
    /// container's `col_lines` (`name-start`…`name-end`) at layout
    pub grid_col: Option<String>,
    pub width: Option<Len>,
    pub max_width: Option<Len>,
    /// explicit height — shortcut: only consulted for replaced (`<img>`) boxes
    pub height: Option<Len>,
    /// `min-height` — floor on the box's content height (hero sections use
    /// `min-h-[560px]` + `justify-end` to push content to the bottom)
    pub min_height: Option<Len>,
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
            wrap: false,
            gap_row: 0.0,
            gap_col: 0.0,
            cols: Vec::new(),
            col_lines: Vec::new(),
            grid_col: None,
            width: None,
            max_width: None,
            height: None,
            min_height: None,
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
#[derive(Clone)]
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
    /// cascade-ordered `(selector, value)` pairs for `color-scheme` only.
    /// The property is consulted for every element but declared on very few
    /// rules, so `resolve` walks this tiny slice instead of the full rule list.
    schemes: Vec<(Vec<Seg>, String)>,
    /// effective custom properties (--name → value) consumed by `resolve`.
    /// Starts as the flat last-definition-wins map (parse time); `scope_vars`
    /// rewrites it once the real <html> element is known.
    vars: std::collections::HashMap<String, String>,
    /// every custom-property definition as (selector, value) in document
    /// order — what `scope_vars` filters by against the <html> root's theme.
    var_defs: std::collections::HashMap<String, Vec<(String, String)>>,
    /// the `<html>` class-list theme polarity (Some(true)=dark) captured by
    /// `scope_vars` — fallback for elements with no declared `color-scheme`.
    /// None until `scope_vars` runs (then the system preference decides).
    root_pol: Option<bool>,
    /// shared font-family stack interner (the loader injects its table via
    /// `set_stacks` so the font picker reads the same Vec). None in tests →
    /// `Style::fam_stack` stays 0 = generic pick, the pre-wiring behaviour.
    stacks: Option<StackTable>,
}

/// Interned `font-family` comma stacks: the cascade assigns each distinct
/// value a `Style::fam_stack` id as it resolves; the font picker reads the
/// names back at measure/draw time. Rc: layout is single-threaded.
pub type StackTable = std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>;

/// One `@font-face` block. Family is lowercased (matching is case-insensitive);
/// src URLs keep their original case (paths may be case-sensitive).
pub struct FontFace {
    pub family: String,
    /// absolute URLs in declared order — first one we can fetch+decode wins
    pub srcs: Vec<String>,
    /// weight range (`400 700` variable faces); both 400 for a single weight
    pub wmin: u16,
    pub wmax: u16,
    pub italic: bool,
}

/// Every `@font-face` definition in one stylesheet, with `src` URLs resolved
/// against that sheet's own URL (`base`). The loader calls this per-sheet
/// BEFORE concatenating text for `parse_for`, which would lose each base URL.
/// The blocks themselves are still skipped by the main parse (no rules inside).
pub fn collect_font_faces(text: &str, base: &str) -> Vec<FontFace> {
    let s = strip_comments(text);
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(at) = s[i..].find("@font-face") {
        let start = i + at;
        let Some(ob) = s[start..].find('{') else { break };
        let Some(cb) = match_brace(b, start + ob) else { break };
        if let Some(f) = parse_font_face(&s[start + ob + 1..cb], base) {
            out.push(f);
        }
        i = cb + 1;
    }
    out
}

/// Scan one `@font-face` block body (raw — NOT `parse_decls`, which lowercases
/// values and src paths can be case-sensitive).
fn parse_font_face(block: &str, base: &str) -> Option<FontFace> {
    let mut family = String::new();
    let mut srcs = Vec::new();
    let (mut wmin, mut wmax) = (400u16, 400u16);
    let mut italic = false;
    for raw in block.split(';') {
        let Some(colon) = raw.find(':') else { continue };
        let prop = raw[..colon].trim().to_ascii_lowercase();
        let val = raw[colon + 1..].trim();
        match prop.as_str() {
            "font-family" => {
                family = val.trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase()
            }
            "font-style" => italic = val.starts_with("italic") || val.starts_with("oblique"),
            // single weight (`400`) or variable range (`100 900`)
            "font-weight" => {
                let mut it = val.split_whitespace().filter_map(|t| t.parse::<u16>().ok());
                wmin = it.next()?;
                wmax = it.next().unwrap_or(wmin);
            }
            // url(...) sources in declared order; local(...) skipped (we can't
            // query installed-font names) — the URL fallbacks follow anyway
            "src" => {
                let mut rest = val;
                while let Some(u) = rest.find("url(") {
                    let after = &rest[u + 4..];
                    let Some(end) = after.find(')') else { break };
                    let target = after[..end].trim().trim_matches(|c| c == '"' || c == '\'');
                    // join (not the page resolver): font assets may be
                    // same-document/fragment-free absolute URLs already
                    let abs = url::Url::parse(base)
                        .and_then(|b| b.join(target))
                        .map(|j| j.to_string())
                        .unwrap_or_else(|_| target.to_string());
                    srcs.push(abs);
                    rest = &after[end + 1..];
                }
            }
            _ => {}
        }
    }
    if family.is_empty() || srcs.is_empty() {
        return None;
    }
    Some(FontFace {
        family,
        srcs,
        wmin,
        wmax,
        italic,
    })
}

impl Sheet {
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Wire the loader's shared stack table so `Style::fam_stack` ids resolved
    /// during layout index the same Vec the font picker reads back.
    pub fn set_stacks(&mut self, t: StackTable) {
        self.stacks = Some(t);
    }

    /// Id of this font-family value's comma stack in the shared table
    /// (0 = "no stack" → generic pick). Repeated values share one id.
    fn intern_stack(&self, v: &str) -> u16 {
        let Some(t) = &self.stacks else { return 0 };
        let names: Vec<String> = v
            .split(',')
            .map(|n| n.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase())
            .filter(|n| !n.is_empty())
            .collect();
        if names.is_empty() {
            return 0;
        }
        let mut tab = t.borrow_mut();
        if let Some(id) = tab.iter().position(|s| s == &names) {
            return id as u16;
        }
        tab.push(names);
        (tab.len() - 1) as u16
    }

    /// Interned stack id for one (var-substituted) declaration — only
    /// `font-family` has one; everything else costs just the string compare.
    fn fam_stack_of(&self, d: &Decl) -> u16 {
        if d.0 == "font-family" {
            self.intern_stack(&d.1)
        } else {
            0
        }
    }

    /// Drop custom-property definitions whose selector asserts a theme state the
    /// real `<html>` element contradicts (Wikipedia's root declares
    /// `clientpref-day`, so its `clientpref-night` dark vars must not override the
    /// `:root` light ones). Runs once after the DOM is built; before it, `vars`
    /// is the flat last-definition-wins map.
    /// Definitions from rules that really apply to the real `<html>` outrank
    /// every other scope: `html.skin-theme-clientpref-os` vars (entered via
    /// `@media (prefers-color-scheme:dark)`) never match a clientpref-day
    /// root, so they must not beat the root's own `:root` palette.
    /// shortcut: only a theme declared on the <html> root is modeled. A JS-
    /// activated theme (nextjs `.dark`, absent from server HTML) is NOT a
    /// contradiction, so its dark values are kept — matching a system-dark render.
    pub fn scope_vars(&mut self, dom: &Dom, html: u32) {
        let root_pol = dom.attr(html, "class").and_then(theme_polarity);
        self.root_pol = root_pol; // fallback theme for light-dark() below
        self.vars.clear();
        for (name, defs) in &self.var_defs {
            // winner among definitions whose selector applies to the real
            // <html> element — those ARE the root's custom properties
            let mut on_root: Option<&str> = None;
            // last non-contradicting definition otherwise (a JS-activated
            // theme, absent from server HTML, can't contradict anything)
            let mut chosen: Option<&str> = None;
            for (sel, val) in defs {
                let sel_pol = selector_polarity(sel);
                // both known and opposite => the definition contradicts the root
                let contradicts = matches!(
                    (root_pol, sel_pol),
                    (Some(false), Some(true)) | (Some(true), Some(false))
                );
                // a selector the DOM says matches <html> can't contradict it —
                // the polarity tokens are a guess, the DOM is the truth (a list
                // like `:root,.skin-invert,.notheme` reads dark by token but is
                // the page's own light palette)
                let applies = root_pol.is_some() && sel_applies_to_root(dom, html, sel);
                if contradicts && !applies {
                    continue;
                }
                if applies {
                    on_root = Some(val); // last on-root definition wins
                } else {
                    chosen = Some(val); // fallback when no on-root def exists
                }
            }
            if let Some(v) = on_root.or(chosen) {
                self.vars.insert(name.clone(), v.to_string());
            }
        }
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
    "color-scheme",
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
    "flex-wrap",
    "gap",
    "row-gap",
    "column-gap",
    "grid-template-columns",
    "grid-column",
    "width",
    "max-width",
    "height",
    "min-height",
    "white-space",
    "padding",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "margin-top",
    "margin-bottom",
    "line-height",
    "visibility",
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

/// The OS color-scheme preference this build renders for (`true` = the system
/// prefers dark, as on this machine). shortcut: no live OS listener — one
/// honest global read at parse/resolve time, so `@media
/// (prefers-color-scheme:…)` and `light-dark()` always agree; a light-OS user
/// sets it to `false` and the `light dark` branch of `light-dark()` becomes
/// the light one, with `prefers-color-scheme: light` blocks applying instead.
const PREFERS_DARK: bool = true;

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
    let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut var_defs: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
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
            // custom properties are collected for EVERY rule regardless of
            // selector — they live in one page-wide map. Each definition is
            // also kept with its selector text so `scope_vars` can later drop
            // the ones whose selector contradicts the real <html> root's theme
            // (Wikipedia's html.skin-theme-clientpref-night dark vars vs its
            // clientpref-day root). shortcut: only a theme declared on the
            // root element is modeled — a JS-activated theme (nextjs .dark,
            // absent from server HTML) can't contradict anything.
            let real: Vec<Decl> = decls
                .iter()
                .filter_map(|d| {
                    if d.0.starts_with("--") {
                        vars.insert(d.0.clone(), d.1.clone());
                        var_defs
                            .entry(d.0.clone())
                            .or_default()
                            .push((sel_text.trim().to_string(), d.1.clone()));
                        None
                    } else {
                        Some(d.clone())
                    }
                })
                .collect();
            if real.is_empty() {
                i = cb + 1;
                continue;
            }
            for sel in split_selectors(sel_text) {
                if let Some(segs) = parse_selector(sel) {
                    let spec = spec_of(&segs);
                    rules.push(Rule {
                        segs,
                        decls: real.clone(),
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
    // color-scheme decides light-dark() for every element but is declared on
    // very few rules — keep the (already cascade-ordered) slice so resolve can
    // find a winning value without re-walking the full rule list. Last
    // declaration within a rule wins (source order), like every property.
    let schemes: Vec<(Vec<Seg>, String)> = rules
        .iter()
        .filter_map(|r| {
            r.decls
                .iter()
                .rev()
                .find(|d| d.0 == "color-scheme")
                .map(|d| (r.segs.clone(), d.1.clone()))
        })
        .collect();
    Sheet {
        rules,
        schemes,
        vars,
        var_defs,
        root_pol: None,
        stacks: None,
    }
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
        // feature queries: enter a branch only if we actually support what it
        // tests for. Modern sites ship a native form and an `@supports not`
        // polyfill of the same custom property; evaluating the query for real
        // keeps the polyfill (whose logic we can't run) from shadowing the
        // native values (MDN's `light-dark()` colours).
        "supports" => supports_matches(&s[ne..pe]),
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

/// Split a selector list on top-level commas only: commas inside `(...)`
/// (`:is(h1,h2)`) or preceded by a backslash (`\,` in escaped class names)
/// are literal. shortcut: nesting depth beyond one level isn't tracked —
/// real stylesheets don't nest deeper than `:is(:is(...))`.
fn split_selectors(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0u32;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1, // escaped char — next byte is literal
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
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
    /// The parent font-size defaults to `base.size` (correct wherever `base` is
    /// the true parent style); callers that pass a UA-adjusted base (headings)
    /// use `resolve_with_parent` so relative font-sizes compute against the
    /// real parent.
    pub fn resolve(&self, dom: &Dom, node: u32, base: Style, inline: Option<&str>) -> StyleOutcome {
        self.resolve_with_parent(dom, node, base, inline, base.size)
    }

    /// `resolve`, but `%`/`em`/`inherit` in `font-size` resolve against
    /// `parent_size` (the parent's true font-size) instead of `base.size`.
    pub fn resolve_with_parent(
        &self,
        dom: &Dom,
        node: u32,
        base: Style,
        inline: Option<&str>,
        parent_size: f32,
    ) -> StyleOutcome {
        let mut style = base;
        let mut bg = None;
        let mut display_none = false;
        let mut bx = BoxStyle::default();
        // used color scheme for THIS element (own declaration, else the
        // nearest ancestor's — the property inherits) — decides light-dark()
        let dark = self.used_dark(dom, node, inline);

        for rule in &self.rules {
            if !matches(dom, node, &rule.segs) {
                continue;
            }
            for d in &rule.decls {
                let sd = self.subst(d);
                let fs = self.fam_stack_of(&sd);
                if apply_decl(&sd, &mut style, &mut bg, &mut bx, parent_size, dark, fs) {
                    display_none = true;
                }
            }
        }
        if let Some(inline) = inline {
            // inline style = specificity (1,0,0,0): applied last, wins
            for d in &parse_decls(inline) {
                let sd = self.subst(d);
                let fs = self.fam_stack_of(&sd);
                if apply_decl(&sd, &mut style, &mut bg, &mut bx, parent_size, dark, fs) {
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

    /// Substitute `var(--name[, fallback])` references in a decl value.
    /// No-op (no allocation path beyond the check) when there's no `var(`.
    fn subst(&self, d: &Decl) -> Decl {
        if !d.1.contains("var(") {
            return d.clone();
        }
        (d.0.clone(), subst_vars(&d.1, &self.vars, 0))
    }

    /// The cascaded `color-scheme` value for `node`: matched rules in cascade
    /// order (last wins), then the inline `style` (applied last → wins).
    fn own_scheme(&self, dom: &Dom, node: u32, inline: Option<&str>) -> Option<String> {
        let mut val: Option<String> = None;
        for (segs, v) in &self.schemes {
            if matches(dom, node, segs) {
                // same var() substitution resolve applies to every declaration
                val = Some(if v.contains("var(") {
                    subst_vars(v, &self.vars, 0)
                } else {
                    v.clone()
                });
            }
        }
        if let Some(inline) = inline {
            for d in &parse_decls(inline) {
                if d.0 == "color-scheme" {
                    val = Some(self.subst(d).1);
                }
            }
        }
        val
    }

    /// Used color scheme for `node` (true = dark): the nearest declared
    /// `color-scheme` up the DOM parent chain (the property inherits, so a
    /// root `color-scheme: light` reaches descendants that don't override
    /// it), else the root's theme polarity, else the system preference.
    fn used_dark(&self, dom: &Dom, node: u32, inline: Option<&str>) -> bool {
        let mut n = node;
        loop {
            let here = if n == node { inline } else { dom.attr(n, "style") };
            if let Some(v) = self.own_scheme(dom, n, here) {
                return used_scheme(Some(&v), self.root_pol, PREFERS_DARK);
            }
            let parent = dom.nodes[n as usize].parent;
            if parent == n {
                break; // document root (its parent is itself)
            }
            n = parent;
        }
        used_scheme(None, self.root_pol, PREFERS_DARK)
    }
}

/// Replace every `var(--name[, fallback])` in `v` from the custom-property
/// map; unknown names fall back, unresolvable → empty string. Depth-capped
/// recursion handles nested vars (`--a: var(--b)`).
fn subst_vars(v: &str, vars: &std::collections::HashMap<String, String>, depth: u8) -> String {
    if depth > 5 {
        return String::new(); // cycle guard
    }
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find("var(") {
        out.push_str(&rest[..i]);
        // find the matching close paren (values may nest: var(--a, var(--b)))
        let body_start = i + 4;
        let mut dep = 1u32;
        let mut j = body_start;
        let bytes = rest.as_bytes();
        while j < bytes.len() && dep > 0 {
            match bytes[j] {
                b'(' => dep += 1,
                b')' => dep -= 1,
                _ => {}
            }
            j += 1;
        }
        if dep != 0 {
            // unterminated var( — emit the rest literally
            out.push_str(&rest[body_start..]);
            return out;
        }
        let body = &rest[body_start..j - 1];
        let (name, fallback) = match body.find(',') {
            Some(c) => (body[..c].trim(), body[c + 1..].trim()),
            None => (body.trim(), ""),
        };
        let val = match vars.get(name) {
            Some(v) => subst_vars(v, vars, depth + 1),
            None => subst_vars(fallback, vars, depth + 1),
        };
        out.push_str(&val);
        rest = &rest[j..];
    }
    out.push_str(rest);
    out
}

/// Theme polarity of ONE token, split into alphanumeric segments so
/// `highlight`/`today` don't match `light`/`day`. Some(true)=dark, Some(false)=light.
fn token_polarity(tok: &str) -> Option<bool> {
    for seg in tok.split(|c: char| !c.is_ascii_alphanumeric()) {
        match seg {
            "dark" | "night" | "invert" => return Some(true),
            "light" | "day" => return Some(false),
            _ => {}
        }
    }
    None
}

/// First class token on the root with a clean theme polarity (root's declared theme).
fn theme_polarity(class_attr: &str) -> Option<bool> {
    class_attr.split_whitespace().find_map(token_polarity)
}

/// Polarity a selector asserts (dark-scoped `.night`/`.dark` vs light-scoped). Neutral selectors (`:root`, `html`, `body`) → None.
fn selector_polarity(sel: &str) -> Option<bool> {
    token_polarity(sel)
}

/// Does this definition's selector apply to the real `<html>` element?
/// [`Sheet::scope_vars`] uses it to prefer the root's own custom properties
/// over ones scoped to a different theme state (`html.skin-theme-clientpref-os`
/// vars on a clientpref-day page never apply in a real browser). `:root`
/// counts even though pseudo-classes don't parse — it IS the root by definition.
fn sel_applies_to_root(dom: &Dom, html: u32, sel: &str) -> bool {
    if split_selectors(sel).iter().any(|p| p.trim() == ":root") {
        return true;
    }
    parse_selector(sel)
        .map(|segs| matches(dom, html, &segs))
        .unwrap_or(false)
}

/// Used color scheme for one element (true = dark) from its declared
/// `color-scheme` value: an explicit `dark`/`light` (with or without `only`)
/// pins it, `light dark` = "either is fine" follows the system preference
/// (`pref_dark`, a `PREFERS_DARK` bool at the call sites); no usable
/// declaration falls back to the root's theme polarity, else the preference.
fn used_scheme(declared: Option<&str>, root_polarity: Option<bool>, pref_dark: bool) -> bool {
    let Some(v) = declared else {
        return root_polarity.unwrap_or(pref_dark);
    };
    let has = |w: &str| v.split_whitespace().any(|t| t == w);
    match (has("light"), has("dark")) {
        (true, true) => pref_dark, // `light dark` / `dark light` → preference
        (_, true) => true,         // `dark`, `only dark`
        (true, false) => false,    // `light`, `only light`
        // `normal`, unknown keywords, empty → root theme, else preference
        (false, false) => root_polarity.unwrap_or(pref_dark),
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
        // custom properties (--x) always survive: var() resolution needs them
        if !p.starts_with("--") && !KNOWN.contains(&p.as_str()) {
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

/// `dark` = the element's used color scheme — picks the `light-dark()` branch
/// on color-bearing values before they reach `parse_color`.
fn apply_decl(
    d: &Decl,
    style: &mut Style,
    bg: &mut Option<u32>,
    bx: &mut BoxStyle,
    parent: f32,
    dark: bool,
    // interned font-family stack id for this decl (from the shared table)
    fam_stack: u16,
) -> bool {
    match d.0.as_str() {
        "color" => {
            if let Some(c) = parse_color(pick_light_dark(&d.1, dark)) {
                style.color = c;
            }
        }
        "background" | "background-color" => {
            if let Some(c) = parse_color(pick_light_dark(&d.1, dark)) {
                *bg = Some(c);
                bx.bg = Some(c); // per-box fill (cards, nav bars)
            }
        }
        // consumed before the cascade in `resolve_with_parent` (the used
        // value decides `light-dark()`) — nothing to do per declaration here
        "color-scheme" => {}
        "font-size" => {
            // `%`/`em` resolve against the PARENT's font-size, and each declaration
            // REPLACES the cascade (spec) — computing against the running style.size
            // used to compound `188% × 1.8em × 1.8em` and freeze it under `inherit`.
            match d.1.trim() {
                "inherit" | "unset" => style.size = parent, // restore the inherited size
                "initial" => style.size = 16.0,
                _ => {
                    if let Some(px) = parse_size(&d.1, parent) {
                        style.size = px.clamp(6.0, 400.0);
                    }
                }
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
        "font-family" => {
            style.fam = parse_fam(&d.1);
            style.fam_stack = fam_stack;
        }
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
            // inline-block/inline-flex/inline-grid get a real box so their
            // background/border/paint (buttons, badges) actually renders —
            // shortcut: they stack as blocks instead of sitting mid-line
            "inline-block" | "inline-flex" | "inline-grid" => bx.display = Display::Block,
            "inline" => bx.display = Display::Inline,
            _ => {}
        },
        // shadows carry their own alpha (rgba() is the common case) and may
        // stack — parse the whole list, first shadow paints on top
        "box-shadow" => bx.shadows = parse_box_shadows(&d.1, dark),
        // fully transparent = invisible; shortcut: partial opacity ignored
        // (rasterizer has no alpha compositing), so only 0 hides the subtree
        "opacity" => return d.1.parse::<f32>().map(|v| v <= 0.0).unwrap_or(false),
        // shortcut: hidden/collapse treated as display:none (real CSS keeps
        // the box in flow — usually fine, sites use it on off-screen
        // measurement clones)
        "visibility" => return matches!(d.1.as_str(), "hidden" | "collapse"),
        "text-align" => {
            style.text_align = match d.1.as_str() {
                "center" => 1,
                "right" | "end" => 2,
                _ => 0, // left/start; shortcut: justify renders as left
            }
        }
        "white-space" => {
            style.nowrap = matches!(d.1.as_str(), "nowrap" | "pre" | "pre-line")
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
        // wrap | wrap-reverse both wrap (reverse axis ignored for now)
        "flex-wrap" => bx.wrap = d.1.split(|c: char| c.is_whitespace() || c == '-').any(|t| t == "wrap"),
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
        "grid-template-columns" => {
            let (cols, lines) = parse_tracks(&d.1);
            bx.cols = cols;
            bx.col_lines = lines;
        }
        // item placement into named lines: `grid-column: content` spans the
        // [content-start]…[content-end] pair from the container's template
        "grid-column" => {
            let first = d.1.split('/').next().unwrap_or("").trim();
            if !first.is_empty() && first != "auto" && !first.parse::<u32>().is_ok() {
                bx.grid_col = Some(first.to_string());
            }
        }
        "width" => bx.width = parse_len(&d.1).filter(|_| d.1 != "auto"),
        "height" => bx.height = parse_len(&d.1).filter(|_| d.1 != "auto"),
        "min-height" => bx.min_height = parse_len(&d.1).filter(|_| d.1 != "auto"),
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
            let (w, c) = border_parts(&d.1, dark);
            if let Some(w) = w {
                bx.border = [w; 4];
            }
            bx.bcolor = c;
        }
        "border-width" => bx.border = parse_sides(&d.1),
        "border-color" => {
            if let Some(c) = parse_color(pick_light_dark(&d.1, dark)) {
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
            let (w, c) = border_parts(&d.1, dark);
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
/// (shortcut); `none` resets the width to 0. Color tokens may be
/// `light-dark(a, b)` — picked against the used scheme (`dark`).
fn border_parts(v: &str, dark: bool) -> (Option<Len>, Option<u32>) {
    let mut w = None;
    let mut c = None;
    for t in joined_tokens(v) {
        if t.eq_ignore_ascii_case("none") {
            w = Some(Len::Px(0.0));
        } else if let Some(l) = parse_len(&t) {
            w = Some(l);
        } else if let Some(col) = parse_color(pick_light_dark(&t, dark)) {
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
/// Returns the tracks plus line names (`[content-start]` → index of the
/// track boundary it precedes) so items can place via `grid-column: name`.
/// shortcut: `repeat(auto-fill, …)` and multi-token line names are dropped.
fn parse_tracks(v: &str) -> (Vec<Track>, Vec<(String, usize)>) {
    let mut out = Vec::new();
    let mut lines: Vec<(String, usize)> = Vec::new();
    let v = v.trim();
    if v.is_empty() || v == "none" {
        return (out, lines);
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
        } else if v[i..].starts_with("calc(") || v[i..].starts_with("max(") || v[i..].starts_with("min(") {
            // unresolvable math (needs the viewport for vw) — skip the WHOLE
            // paren group. Cutting inside it used to leak inner literals
            // (e.g. `calc(50vw - 720px)` minted a phantom 720px track)
            let open = v[i..].find('(').unwrap();
            let Some(close) = match_paren(b, i + open) else { break };
            s.push(' '); // keep neighbors separated
            i = close + 1;
        } else {
            s.push(b[i] as char);
            i += 1;
        }
    }
    for tok in s.split_whitespace() {
        // [line-name] → record the boundary index it precedes
        if let Some(name) = tok.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            if !name.contains(' ') {
                lines.push((name.to_string(), out.len()));
            }
            continue;
        }
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
        // unknown tokens (auto) are skipped
    }
    (out, lines)
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
    let v = v.trim();
    // clamp(min, val, max) — common for responsive font sizes; resolve the
    // inner values (px/rem/% only; vw needs the viewport, see parse_for note)
    if let Some(inner) = v
        .strip_prefix("clamp(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if parts.len() == 3 {
            let (Some(lo), Some(hi)) = (parse_size(parts[0], current), parse_size(parts[2], current))
            else {
                return None;
            };
            let val = parse_size(parts[1], current).unwrap_or(lo);
            return Some(val.clamp(lo, hi));
        }
        return None;
    }
    // vw/vh units: no viewport here (static parse) — treat vw as % of a
    // 1000px window (our fixed width) so headings stay proportional
    if let Some(n) = v.strip_suffix("vw") {
        return n.trim().parse::<f32>().ok().map(|f| f / 100.0 * 1000.0);
    }
    if let Some(n) = v.strip_suffix("vh") {
        return n.trim().parse::<f32>().ok().map(|f| f / 100.0 * 700.0);
    }
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

/// `light-dark(light, dark)` → the branch for the used scheme (`dark`),
/// splitting the top-level comma (arguments nest: `light-dark(var(--a),
/// rgb(1, 2, 3))`). Returns `v` untouched unless the WHOLE value is one
/// `light-dark(…)` call, so plain colors — and every site that never uses
/// the function — flow through unchanged. Runs AFTER var() substitution, so
/// `var(--x, light-dark(a, b))` and `light-dark(var(--l), var(--d))` are
/// already plain values by the time we split.
fn pick_light_dark(v: &str, dark: bool) -> &str {
    let t = v.trim();
    let Some(inner) = t.strip_prefix("light-dark(").and_then(|s| s.strip_suffix(')')) else {
        return v;
    };
    let mut depth = 0i32;
    let mut comma = None;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return v; // suffix wasn't our closing paren — leave it alone
                }
            }
            ',' if depth == 0 => {
                comma = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(comma) = comma else {
        return v; // not two arguments — unparseable, same as before
    };
    let branch = if dark { &inner[comma + 1..] } else { &inner[..comma] };
    branch.trim()
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
    if v.starts_with("hsl(") || v.starts_with("hsla(") {
        return parse_hsl(v).map(|(c, _)| c);
    }
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

/// `hsl(h, s%, l%[, a])` → (rgb, alpha 0-255). Accepts `%`-less s/l too
/// (the site emits `hsla(var(--ds-x-value), 1)` with values like `0, 0%, 10%`).
fn parse_hsl(v: &str) -> Option<(u32, u8)> {
    let is_hsla = v.starts_with("hsla(");
    let inner = v
        .strip_prefix("hsla(")
        .or_else(|| v.strip_prefix("hsl("))
        .and_then(|s| s.strip_suffix(')'))?;
    let mut parts = inner.split(',').map(|p| p.trim());
    let h = parts.next()?.trim_end_matches("deg").parse::<f32>().ok()?;
    let s = parts.next()?.trim_end_matches('%').parse::<f32>().ok()? / 100.0;
    let l = parts.next()?.trim_end_matches('%').parse::<f32>().ok()? / 100.0;
    let a = if is_hsla {
        parts.next().and_then(|p| p.parse::<f32>().ok()).unwrap_or(1.0)
    } else {
        1.0
    };
    if !(0.0..=1.0).contains(&s) || !(0.0..=1.0).contains(&l) {
        return None;
    }
    // hsl → rgb (standard piecewise conversion)
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let q = |t: f32| ((t + m).clamp(0.0, 1.0) * 255.0).round() as u32;
    let rgb = q(r1) << 16 | q(g1) << 8 | q(b1);
    Some((rgb, (a.clamp(0.0, 1.0) * 255.0).round() as u8))
}

/// Like [`parse_color`] but keeps alpha — shadows need real opacity
/// (an `rgba(0,0,0,0.4)` shadow at full opacity looks like a sticker).
/// Returns (rgb, alpha 0-255); None = unparseable.
fn parse_color_alpha(v: &str) -> Option<(u32, u8)> {
    let v = v.trim();
    if v.starts_with("hsl(") || v.starts_with("hsla(") {
        return parse_hsl(v);
    }
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
/// inner-shadow rendering). Color tokens may be `light-dark(a, b)` — picked
/// against the used scheme (`dark`).
fn parse_box_shadows(v: &str, dark: bool) -> Vec<Shadow> {
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
    parts.iter().filter_map(|p| parse_shadow(p, dark)).collect()
}

/// Whitespace-split value tokens with `(...)` groups re-joined: function
/// arguments (`rgba(0, 0, 0, 0.4)`, `light-dark(#111, #222)`) span several
/// whitespace chunks and must stay whole for per-token parsing.
fn joined_tokens(v: &str) -> Vec<String> {
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
    toks
}

/// One comma-separated shadow: `dx dy [blur] [spread] [color]` in any
/// token order (color first or last, as sites write it).
fn parse_shadow(v: &str, dark: bool) -> Option<Shadow> {
    let mut lens = [0.0f32; 4];
    let mut n_lens = 0usize;
    let mut color = None;
    for t in joined_tokens(v) {
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
            color = parse_color_alpha(pick_light_dark(&t, dark));
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

// ---------- @supports feature queries (evaluated once, at parse time) ----------

/// One `@supports` condition. We answer honestly from the feature set Mazda
/// actually implements, so a site's native branch is entered and its
/// `@supports not (…)` polyfill (whose runtime logic we can't execute) is
/// skipped rather than shadowing working CSS.
/// shortcut: only `property: value` plain tests (plus `not`/`and`/`or`) are
/// parsed; an unparseable condition is treated as unsupported (`false`), the
/// safe direction — we never enter a branch whose syntax we don't understand.
fn supports_matches(prelude: &str) -> bool {
    supports_condition(prelude.trim())
}

fn supports_condition(q: &str) -> bool {
    let q = q.trim();
    // top-level `or` (lowest precedence) — any side true => true
    if let Some((l, r)) = split_top_level(q, " or ") {
        return supports_condition(l) || supports_condition(r);
    }
    // top-level `and` — every conjunct must hold
    if let Some(parts) = split_all_top_level(q, " and ") {
        return parts.iter().all(|p| supports_condition(p));
    }
    if let Some(inner) = q.strip_prefix("not ").map(str::trim) {
        return !supports_condition(inner);
    }
    // plain `property: value` test (parens already stripped by caller path)
    let inner = q
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(q)
        .trim();
    supports_feature(inner)
}

/// Do we support this `property: value` pair? Answered from `KNOWN` — the set
/// of properties the cascade actually implements — which makes
/// `(color: light-dark(…))` true (color values incl. light-dark parse) and
/// unknown-feature tests false.
/// shortcut: property-level known-ness only; a value we can't parse
/// (`color: oklch(…)`) is still claimed supported. The failure mode is
/// entering a branch whose decls then no-op — safer than skipping a branch
/// whose property we do handle.
fn supports_feature(test: &str) -> bool {
    let Some((prop, _value)) = test.split_once(':') else {
        return false; // bare `(color)` form — needs a value to test
    };
    KNOWN.contains(&prop.trim())
}

/// Split `s` on the FIRST top-level occurrence of `sep`, returning both halves
/// (paren/bracket depth ignored inside). None if `sep` isn't present.
fn split_top_level<'a>(s: &'a str, sep: &str) -> Option<(&'a str, &'a str)> {
    let b = s.as_bytes();
    let sb = sep.as_bytes();
    let mut depth = 0u32;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && b[i..].starts_with(sb) {
            return Some((s[..i].trim(), s[i + sb.len()..].trim()));
        }
        i += 1;
    }
    None
}

/// Split `s` on EVERY top-level occurrence of `sep`; returns None if fewer
/// than two parts (so the caller knows there was no separator).
fn split_all_top_level<'a>(s: &'a str, sep: &str) -> Option<Vec<&'a str>> {
    let b = s.as_bytes();
    let sb = sep.as_bytes();
    let mut depth = 0u32;
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && b[i..].starts_with(sb) {
            parts.push(s[start..i].trim());
            i += sb.len();
            start = i;
            continue;
        }
        i += 1;
    }
    if parts.is_empty() {
        return None; // no separator found
    }
    parts.push(s[start..].trim());
    Some(parts)
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
            // the OS preference — one honest global (see PREFERS_DARK)
            "prefers-color-scheme" => match v {
                "dark" => PREFERS_DARK,
                "light" => !PREFERS_DARK,
                // no-preference/bogus: we always model a definite preference
                _ => false,
            },
            _ => false,
        };
    }
    // boolean form `(prefers-color-scheme)` = "does the system prefer dark"
    if inner == "prefers-color-scheme" {
        return PREFERS_DARK;
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
    fn opacity_zero_drawer_is_hidden() {
        // real nextjs.org mobile-menu drawer: fixed + opacity-0 via Tailwind
        let sheet = parse(
            ".fixed{position:fixed} .opacity-0{opacity:0} .inset-x-0{left:0;right:0} \
             .top-16{top:4rem} .bottom-0{bottom:0} .z-40{z-index:40}",
        );
        let dom = dom_of(r#"<div class="fixed inset-x-0 top-16 bottom-0 z-40 opacity-0"></div>"#);
        let out = sheet.resolve(&dom, by_attr(&dom, "class", "fixed inset-x-0 top-16 bottom-0 z-40 opacity-0"), Style::new(), None);
        assert!(out.display_none, "opacity-0 drawer must be hidden");
    }

    #[test]
    fn visibility_hidden_hides_subtree() {
        // sites hide measurement clones with `visibility: invisible/hidden`
        // (Tailwind `.invisible`); shortcut: treated as display:none
        let sheet = parse(".a { visibility: hidden } .b { visibility: visible }");
        let dom = dom_of("<div class=a></div><div class=b></div>");
        assert!(sheet.resolve(&dom, by_attr(&dom, "class", "a"), Style::new(), None).display_none);
        assert!(!sheet.resolve(&dom, by_attr(&dom, "class", "b"), Style::new(), None).display_none);
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
    fn flex_wrap_parses() {
        let sheet = parse(".a { flex-wrap: wrap } .b { flex-wrap: wrap-reverse } .c { flex-wrap: nowrap } .d { }");
        let dom = dom_of("<div class=a></div><div class=b></div><div class=c></div><div class=d></div>");
        let get = |c: &str| sheet.resolve(&dom, by_attr(&dom, "class", c), Style::new(), None).bx;
        assert!(get("a").wrap);
        assert!(get("b").wrap, "wrap-reverse wraps (reverse ignored)");
        assert!(!get("c").wrap, "nowrap stays single-line");
        assert!(!get("d").wrap, "default nowrap");
    }

    #[test]
    fn color_parsing() {
        assert_eq!(parse_color("#abc"), Some(0xaabbcc));
        assert_eq!(parse_color("#ff0000"), Some(0xff0000));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(0x010203));
        assert_eq!(parse_color("white"), Some(0xffffff));
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_color("var(--x)"), None);
        assert_eq!(parse_color("hsl(0, 100%, 50%)"), Some(0xff0000));
        assert_eq!(parse_color("hsl(120, 100%, 25%)"), Some(0x008000));
    }

    #[test]
    fn hsl_alpha_parses() {
        assert_eq!(parse_color_alpha("hsla(0, 0%, 10%, 1)"), Some((0x1a1a1a, 255)));
        assert_eq!(parse_color_alpha("hsla(210, 50%, 40%, 0.5)"), Some((0x336699, 128)));
    }

    #[test]
    fn custom_properties_resolve_var() {
        // :root-style rules we can't match still populate the var map
        let sheet = parse(
            ":root, :host { --ds-bg: #0a0a0a; --ds-mono: ui-monospace, Menlo, monospace; --b: #00ff00; --a: var(--b) } \
             .pill { background: var(--ds-bg); font-family: var(--ds-mono) } \
             .fb { color: var(--missing, #ff0000) } \
             .nest { color: var(--a) }",
        );
        let dom = dom_of(
            "<div class=pill></div><div class=fb></div><div class=nest></div>",
        );
        let pill = sheet.resolve(&dom, by_attr(&dom, "class", "pill"), Style::new(), None);
        assert_eq!(pill.bg, Some(0x0a0a0a), "var in background-color");
        assert_eq!(pill.style.fam, Fam::Mono, "var in font-family");
        let fb = sheet.resolve(&dom, by_attr(&dom, "class", "fb"), Style::new(), None);
        assert_eq!(fb.style.color, 0xff0000, "fallback used when var unknown");
        let nest = sheet.resolve(&dom, by_attr(&dom, "class", "nest"), Style::new(), None);
        assert_eq!(nest.style.color, 0x00ff00, "nested var chain resolves");
    }

    #[test]
    fn vars_collected_from_any_selector() {
        // Documented shortcut: the flat page-wide var map takes definitions
        // from EVERY selector. It resolves nextjs (.dark block) and Wikipedia
        // (:root light) alike — theme scoping is not modeled. See css.rs
        // collect comment for the trade-off.
        let sheet = parse(
            ":root { --bg: #f8f9fa } \
             html.skin-theme-clientpref-night { --bg: #202122 } \
             body { background-color: var(--bg, #ffffff) }",
        );
        let dom = dom_of("<body class=skin-vector><p>x</p></body>");
        let body = by_tag(&dom, "body");
        // later rule wins in the flat map (source order)
        assert_eq!(
            sheet.resolve(&dom, body, Style::new(), None).bg,
            Some(0x202122),
            "flat var map: last definition wins regardless of selector"
        );
    }

    #[test]
    fn scope_vars_drops_root_contradicting_theme_defs() {
        let mut sheet = parse(
            ":root { --bg: #f8f9fa } \
             html.skin-theme-clientpref-night { --bg: #202122 } \
             body { background-color: var(--bg, #ffffff) }",
        );
        // the real root declares the DAY (light) theme
        let dom = dom_of("<html class=\"skin-theme-clientpref-day\"><body><p>x</p></body></html>");
        let html = by_tag(&dom, "html");
        sheet.scope_vars(&dom, html);
        let body = by_tag(&dom, "body");
        assert_eq!(
            sheet.resolve(&dom, body, Style::new(), None).bg,
            Some(0xf8f9fa),
            "night dark vars dropped when root declares day"
        );
    }

    #[test]
    fn scope_vars_keeps_dark_when_root_declares_no_theme() {
        let mut sheet = parse(
            ":root { --bg: #f8f9fa } .dark { --bg: #202122 } \
             body { background-color: var(--bg, #ffffff) }",
        );
        // root has no theme class (nextjs server HTML) — dark def must survive
        let dom = dom_of("<html><body><p>x</p></body></html>");
        let html = by_tag(&dom, "html");
        sheet.scope_vars(&dom, html);
        let body = by_tag(&dom, "body");
        assert_eq!(
            sheet.resolve(&dom, body, Style::new(), None).bg,
            Some(0x202122),
            "no root theme => no contradiction => dark kept"
        );
    }

    #[test]
    fn scope_vars_prefers_on_root_defs_over_other_theme_scopes() {
        // Wikipedia shape: `@media (prefers-color-scheme:dark)` matches here,
        // so the OS-theme block parses — but its html.skin-theme-clientpref-os
        // selector never applies to the clientpref-day root, and its later
        // dark defs must not outrank the page's own `:root` palette in the
        // flat last-wins map
        let mut sheet = parse(
            ":root { --bg: #ffffff } \
             @media screen and (prefers-color-scheme:dark) { \
               html.skin-theme-clientpref-os { --bg: #101418 } \
             } \
             body { background-color: var(--bg, #f0f0f0) }",
        );
        let dom = dom_of("<html class=\"skin-theme-clientpref-day\"><body><p>x</p></body></html>");
        let html = by_tag(&dom, "html");
        sheet.scope_vars(&dom, html);
        let body = by_tag(&dom, "body");
        assert_eq!(
            sheet.resolve(&dom, body, Style::new(), None).bg,
            Some(0xffffff),
            "os-scoped vars lose to the root's own defs on a day-declaring root"
        );
    }

    #[test]
    fn scope_vars_keeps_root_matching_selector_lists_despite_polarity_tokens() {
        // the light palette ships as `:root,.skin-invert,.notheme` — the
        // `invert` token reads dark, but the list really applies to the root
        // (ground truth beats the token guess), so it must not be dropped
        let mut sheet = parse(
            ":root,.skin-invert,.notheme { --bg: #f8f9fa } \
             html.skin-theme-clientpref-night { --bg: #101418 } \
             body { background-color: var(--bg, #ffffff) }",
        );
        let dom = dom_of("<html class=\"skin-theme-clientpref-day\"><body><p>x</p></body></html>");
        let html = by_tag(&dom, "html");
        sheet.scope_vars(&dom, html);
        let body = by_tag(&dom, "body");
        assert_eq!(
            sheet.resolve(&dom, body, Style::new(), None).bg,
            Some(0xf8f9fa),
            "a selector list containing :root applies to the root regardless of tokens"
        );
    }

    #[test]
    fn scope_vars_uses_other_theme_scopes_only_as_fallback() {
        // a name defined nowhere else still resolves from the os block — the
        // flat map has no per-element scoping; the value only loses when the
        // root itself defines the name
        let mut sheet = parse(
            "html.skin-theme-clientpref-os { --only: #202122 } \
             body { background-color: var(--only, #ffffff) }",
        );
        let dom = dom_of("<html class=\"skin-theme-clientpref-day\"><body><p>x</p></body></html>");
        let html = by_tag(&dom, "html");
        sheet.scope_vars(&dom, html);
        let body = by_tag(&dom, "body");
        assert_eq!(
            sheet.resolve(&dom, body, Style::new(), None).bg,
            Some(0x202122),
            "non-root theme scopes still supply names the root doesn't define"
        );
    }

    #[test]
    fn class_matching_is_whitespace_split() {
        let sheet = parse(".btn.primary { color: #111111 }");
        let dom = dom_of(r#"<div class="btn big primary"></div>"#);
        assert_eq!(style_for_tag(&sheet, &dom, "div").color, 0x111111);
    }

    #[test]
    fn selector_list_commas_inside_functions_stay_unsplit() {
        // Tailwind prose: splitting on every ',' minted a bare `h4` rule that
        // matched every <h4> and fought over its `position`
        let sheet = parse(
            ".prose :is(h1,h2,h3,h4)[id]{position:relative} \
             .\\[\\&_h4\\]\\:relative h4{position:absolute} \
             .ok, .fine{color:red}",
        );
        let dom = dom_of(r#"<h4 class=other>x</h4><h4 id=q>x</h4><div class="ok fine">y</div><div class=ok>z</div><div class=fine>w</div>"#);
        let plain = by_attr(&dom, "class", "other");
        assert!(
            sheet.resolve(&dom, plain, Style::new(), None).bx.pos == crate::css::Pos::Static,
            "bare h4 must not inherit position from :is(h1,h2,h3,h4)"
        );
        // top-level comma split still works
        assert_eq!(
            sheet.resolve(&dom, by_attr(&dom, "class", "ok"), Style::new(), None).style.color,
            0xff0000
        );
        assert_eq!(
            sheet.resolve(&dom, by_attr(&dom, "class", "fine"), Style::new(), None).style.color,
            0xff0000
        );
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
    fn font_size_inherit_keeps_parent_size() {
        // Wikipedia: #mp-welcome{font-size:162%} #mp-welcome h1{font-size:inherit}
        // — without inherit support the h1 doubled to the UA 2x default
        let sheet = parse("#w { font-size: 162% } #w h1 { font-size: inherit }");
        let dom = dom_of("<div id=w><h1>x</h1></div>");
        let parent = sheet.resolve(&dom, by_attr(&dom, "id", "w"), Style::new(), None);
        let h1 = sheet.resolve(&dom, by_tag(&dom, "h1"), parent.style, None);
        // parent: 15 * 1.62 = 24.3; h1 inherit must keep exactly that
        assert!((parent.style.size - 24.3).abs() < 0.01, "parent {}", parent.style.size);
        assert_eq!(h1.style.size, parent.style.size, "inherit keeps parent size");
    }

    #[test]
    fn font_size_cascades_against_parent_not_compounding() {
        // Several matching font-size rules must each compute against the PARENT
        // and replace (not compound); a top-specificity `inherit` restores it.
        let sheet = parse(
            "#w { font-size: 162% } \
             h1 { font-size: 188% } \
             .wrap h1 { font-size: 1.8em } \
             .wrap h1 { font-size: 1.8em } \
             #w h1 { font-size: inherit }",
        );
        let dom = dom_of("<div id=w class=wrap><h1>hi</h1></div>");
        let parent = sheet.resolve(&dom, by_attr(&dom, "id", "w"), Style::new(), None);
        // #w = 15 * 1.62 = 24.3
        let h1 = sheet.resolve(&dom, by_tag(&dom, "h1"), parent.style, None);
        assert_eq!(
            h1.style.size, parent.style.size,
            "inherit caps the h1 at the parent size (no compounding)"
        );
    }

    #[test]
    fn heading_author_fontsize_ignores_ua_default() {
        // build_into passes a UA-bumped base (h1 = 2x parent) but the true parent
        // size; an author `em`/`%` must compute against the PARENT, replacing the UA default.
        let sheet = parse("h1 { font-size: 1.5em }");
        let dom = dom_of("<h1>hi</h1>");
        let mut parent = Style::new();
        parent.size = 20.0;
        let mut ua_bumped = parent; // what style_for produces for <h1>
        ua_bumped.size = 40.0;
        let h1 = sheet.resolve_with_parent(&dom, by_tag(&dom, "h1"), ua_bumped, None, 20.0);
        assert_eq!(h1.style.size, 30.0, "1.5em of the parent (20), not of the UA base (40)");
    }

    #[test]
    fn font_size_clamp_and_vw_parse() {
        let sheet = parse(".a { font-size: clamp(16px, 2.5vw, 32px) } .b { font-size: 5vw }");
        let dom = dom_of("<div class=a></div><div class=b></div>");
        let a = sheet.resolve(&dom, by_attr(&dom, "class", "a"), Style::new(), None);
        // 2.5vw = 25px, inside [16, 32]
        assert!((a.style.size - 25.0).abs() < 0.01, "clamp {}", a.style.size);
        let b = sheet.resolve(&dom, by_attr(&dom, "class", "b"), Style::new(), None);
        // 5vw = 50px, clamped to the 400px cap only
        assert!((b.style.size - 50.0).abs() < 0.01, "vw {}", b.style.size);
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
    fn supports_light_dark_enters_native_branch_skips_polyfill() {
        // A site ships a native branch and an `@supports not(...)` polyfill of
        // the SAME custom property. We support light-dark(), so the native
        // branch must win and the polyfill must be skipped — otherwise the
        // polyfill's fallback logic shadows the dark values.
        let sheet = parse(
            "@supports (color: light-dark(#fff, #fff)) { \
               :root { --bg: #202122 } } \
             @supports not (color: light-dark(#fff, #fff)) { \
               :root { --bg: #ffffff } } \
             p { background-color: var(--bg, #ffffff) }",
        );
        let dom = dom_of("<html><body><p>x</p></body></html>");
        assert_eq!(
            sheet.resolve(&dom, by_tag(&dom, "p"), Style::new(), None).bg,
            Some(0x202122),
            "native branch entered, polyfill skipped"
        );
        // sanity: the polyfill branch alone (native absent) would give white
        let poly = parse(
            "@supports not (color: light-dark(#fff, #fff)) { \
               :root { --bg: #ffffff } } \
             p { background-color: var(--bg, #abcdef) }",
        );
        assert_eq!(
            poly.resolve(&dom, by_tag(&dom, "p"), Style::new(), None).bg,
            Some(0xabcdef),
            "polyfill skipped => fallback colour used"
        );
    }

    #[test]
    fn supports_and_or_not_and_unknown_conditions() {
        // `and` conjuncts: both hold => enter
        let both = "@supports (color: light-dark(#fff, #fff)) and (display: block) { p { color: #111111 } }";
        // second conjunct is a property we don't implement => skip
        let drop = "@supports (color: light-dark(#fff, #fff)) and (accent-color: red) { p { color: #111111 } }";
        let dom = dom_of("<p></p>");
        assert_eq!(style_for_tag(&parse(both), &dom, "p").color, 0x111111);
        assert_eq!(parse(drop).rule_count(), 0, "unsupported conjunct skips");
        // `or`: one side true => enter even if the other is unknown
        let orq = "@supports (accent-color: red) or (color: light-dark(#fff, #fff)) { p { color: #222222 } }";
        assert_eq!(style_for_tag(&parse(orq), &dom, "p").color, 0x222222);
        // neither side supported => skip
        let none = "@supports (accent-color: red) or (tab-size: 4) { p { color: #444444 } }";
        assert_eq!(parse(none).rule_count(), 0);
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
        let tracks = |v: &str| parse_tracks(v).0;
        assert_eq!(
            tracks("repeat(3, 1fr)"),
            vec![Track::Fr(1.0), Track::Fr(1.0), Track::Fr(1.0)]
        );
        assert_eq!(
            tracks("200px 50% 1fr"),
            vec![Track::Px(200.0), Track::Pct(50.0), Track::Fr(1.0)]
        );
        // minmax(0, 1fr) → take the max side
        assert_eq!(tracks("minmax(0, 1fr)"), vec![Track::Fr(1.0)]);
        assert!(tracks("none").is_empty());
        // garbage tokens are skipped, valid ones survive
        assert_eq!(tracks("[line1] auto 2fr"), vec![Track::Fr(2.0)]);
        // line names are recorded with the boundary index they precede
        assert_eq!(
            parse_tracks("[a] 1fr [b-start] 200px [b-end]").1,
            vec![("a".to_string(), 0), ("b-start".to_string(), 1), ("b-end".to_string(), 2)]
        );
    }

    #[test]
    fn calc_in_tracks_does_not_leak_phantom_lengths() {
        // MDN: `max(1rem,calc(50vw - 720px + 1rem))` used to be cut inside
        // the parens, minting a bogus 720px track that zeroed the 1fr tracks
        let (cols, _) = parse_tracks(
            "[s] max(1rem,calc(50vw - 720px + 1rem)) [c0] 1fr [c1] minmax(0,48rem) [c2] 1fr [e]",
        );
        assert_eq!(cols, vec![Track::Fr(1.0), Track::Px(768.0), Track::Fr(1.0)]);
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

    #[test]
    fn light_dark_picks_dark_branch_when_scheme_dark() {
        // the used color-scheme (here: each element's own declaration) picks
        // the branch before the value reaches parse_color
        let sheet = parse(
            "div { color-scheme: dark; color: light-dark(#111111, #eeeeee) } \
             span { color-scheme: light; color: light-dark(#111111, #eeeeee) }",
        );
        let dom = dom_of("<div></div><span></span>");
        assert_eq!(style_for_tag(&sheet, &dom, "div").color, 0xeeeeee, "dark → 2nd arg");
        assert_eq!(style_for_tag(&sheet, &dom, "span").color, 0x111111, "light → 1st arg");
    }

    #[test]
    fn color_scheme_light_dark_follows_pref() {
        // `light dark` = "either is fine" → the system preference (dark)
        let sheet = parse(
            "html { color-scheme: light dark } p { color: light-dark(#000000, #ffffff) }",
        );
        let dom = dom_of("<html><p></p></html>");
        assert_eq!(style_for_tag(&sheet, &dom, "p").color, 0xffffff, "pref dark → 2nd arg");
        // a fixed `light` on the root pins descendants that don't override it
        let sheet2 = parse(
            "html { color-scheme: light } p { color: light-dark(#000000, #ffffff) }",
        );
        assert_eq!(style_for_tag(&sheet2, &dom, "p").color, 0x000000, "declared light wins");
        // the root's light THEME CLASS must not beat `light dark` — the
        // preference decides between the two branches
        let mut sheet3 = parse(
            "html { color-scheme: light dark } p { color: light-dark(#000000, #ffffff) }",
        );
        let dom3 = dom_of("<html class=light><p></p></html>");
        sheet3.scope_vars(&dom3, by_tag(&dom3, "html"));
        assert_eq!(
            style_for_tag(&sheet3, &dom3, "p").color,
            0xffffff,
            "light dark → pref, not root class"
        );
    }

    #[test]
    fn prefers_color_scheme_media_applies() {
        // PREFERS_DARK = true: the dark block applies, the light one doesn't
        let sheet = parse(
            "@media (prefers-color-scheme: dark) { p { color: #ff0000 } } \
             @media (prefers-color-scheme: light) { q { color: #00ff00 } }",
        );
        let dom = dom_of("<p></p><q></q>");
        assert_eq!(style_for_tag(&sheet, &dom, "p").color, 0xff0000, "dark block applies");
        assert_eq!(style_for_tag(&sheet, &dom, "q").color, 0x000000, "light block skipped");
    }

    #[test]
    fn light_dark_inside_var_fallback() {
        // var() resolves first, then the light-dark() branch picks (pref dark)
        let sheet = parse("p { color: var(--missing, light-dark(#222222, #333333)) }");
        let dom = dom_of("<p></p>");
        assert_eq!(style_for_tag(&sheet, &dom, "p").color, 0x333333);
    }

    #[test]
    fn font_face_rules_are_collected() {
        // shape of rust-lang's fonts.css: woff2 first, WOFF1 fallback after
        let faces = collect_font_faces(
            "@font-face { font-family: \"Alfa Slab One\"; \
             src: url(/static/woff2/A.woff2) format(\"woff2\"), \
             url(/static/woff/A.woff) format(\"woff\"); \
             unicode-range: U+0000-00FF; font-weight: 400 }",
            "https://x.org/static/styles/fonts.css",
        );
        assert_eq!(faces.len(), 1);
        let f = &faces[0];
        assert_eq!(f.family, "alfa slab one"); // lowercased for matching
        assert_eq!(
            f.srcs,
            [
                "https://x.org/static/woff2/A.woff2".to_string(),
                "https://x.org/static/woff/A.woff".to_string()
            ],
            "src order preserved, resolved against the SHEET's URL"
        );
        assert_eq!((f.wmin, f.wmax, f.italic), (400, 400, false));
    }

    #[test]
    fn font_face_src_urls_keep_case() {
        // paths may be case-sensitive — never lowercased (unlike parse_decls)
        let faces = collect_font_faces(
            "@font-face{font-family:X;src:url(/MIXedCase/Font.WoFF)}",
            "https://x.org/",
        );
        assert_eq!(faces[0].srcs, ["https://x.org/MIXedCase/Font.WoFF"]);
    }

    #[test]
    fn font_face_variable_weight_range_and_italic() {
        let faces = collect_font_faces(
            "@font-face{font-family:Inter;src:url(a.woff2);font-weight:100 900;font-style:italic}",
            "https://x.org/",
        );
        assert_eq!((faces[0].wmin, faces[0].wmax, faces[0].italic), (100, 900, true));
    }

    #[test]
    fn font_face_needs_family_and_src() {
        // a local()-only face (nextjs "Fallback") yields no URL to fetch → skipped
        let faces = collect_font_faces(
            "@font-face{font-family:X;src:local(Y)}",
            "https://x.org/",
        );
        assert!(faces.is_empty());
    }

    #[test]
    fn font_family_stack_is_interned() {
        let mut sheet = parse(
            "h1 { font-family: \"Alfa Slab One\", serif }\
             p { font-family: \"ALFA SLAB ONE\", SERIF }",
        );
        let table: StackTable = std::rc::Rc::new(std::cell::RefCell::new(vec![Vec::new()]));
        sheet.set_stacks(table.clone());
        let dom = dom_of("<h1></h1><p></p>");
        let h1 = style_for_tag(&sheet, &dom, "h1");
        let p = style_for_tag(&sheet, &dom, "p");
        assert_eq!(h1.fam, Fam::Serif, "generic fallback unchanged");
        assert_eq!(h1.fam_stack, 1, "interned at first use");
        assert_eq!(p.fam_stack, 1, "case-insensitive match shares one id");
        assert_eq!(
            table.borrow()[1],
            ["alfa slab one".to_string(), "serif".to_string()]
        );
        // no stack table wired (tests using plain parse()) → id 0, old behaviour
        let plain = parse("h1 { font-family: serif }");
        assert_eq!(style_for_tag(&plain, &dom, "h1").fam_stack, 0);
    }
}


