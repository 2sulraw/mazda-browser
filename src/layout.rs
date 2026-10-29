//! DOM → styled atoms → line layout using real glyph metrics.
//! shortcut: only inline/block split + word wrap (no float/flex/absolute);
//! upgrade when a real CSS engine lands (Phase 1b+).

use crate::css::{Align, BoxStyle, Dir, Display, Justify, Len, Pos, Sheet, Track};
use crate::html::{Dom, NodeKind};
use crate::image::ImageData;

/// Resolved generic font family (CSS `font-family` stack → first known
/// generic; unknown names are skipped, default = Sans).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fam {
    Sans,
    Serif,
    Mono,
}

#[derive(Clone, Copy, PartialEq)]
pub struct Style {
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: u32,
    pub underline: bool,
    pub fam: Fam,
    /// 0 = left, 1 = center, 2 = right (inherited, text-align)
    pub text_align: u8,
    /// line-height in px; 0 = default (size * 1.35) — inherited
    pub lh: f32,
    /// `white-space: nowrap` — never wrap to a new line (truncate/ellipsis
    /// buttons, single-line labels)
    pub nowrap: bool,
}

impl Style {
    pub const fn new() -> Self {
        Style {
            size: 15.0,
            bold: false,
            italic: false,
            color: 0x00_00_00_00, // UA default = black (matches every browser)
            underline: false,
            fam: Fam::Sans,
            text_align: 0,
            lh: 0.0,
            nowrap: false,
        }
    }

    /// resolved line box height for this style
    pub fn line_height(&self) -> f32 {
        if self.lh > 0.0 {
            self.lh
        } else {
            self.size * 1.35
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Atom {
    pub text: String,
    pub style: Style,
    /// forced line break (block element boundary or <br>)
    pub hard_break: bool,
    /// raw href of the enclosing <a>, resolved against base URL only on click
    pub link: Option<std::rc::Rc<str>>,
}

/// Apply presentational defaults for an element tag.
fn style_for(tag: &str, base: Style) -> Style {
    let mut s = base;
    match tag {
        "h1" => {
            s.size = (base.size * 2.0).max(28.0);
            s.bold = true;
        }
        "h2" => {
            s.size = (base.size * 1.6).max(24.0);
            s.bold = true;
        }
        "h3" => {
            s.size = (base.size * 1.35).max(20.0);
            s.bold = true;
        }
        "h4" | "h5" | "h6" => {
            s.size = (base.size * 1.15).max(17.0);
            s.bold = true;
        }
        "b" | "strong" | "th" => s.bold = true,
        "i" | "em" | "cite" | "var" => s.italic = true,
        "u" | "ins" => s.underline = true,
        "code" | "kbd" | "samp" | "pre" | "tt" => {
            s.fam = Fam::Mono;
            s.size = base.size - 1.0;
        }
        "a" => {
            s.color = 0x00_6a_b3_ff;
            s.underline = true;
        }
        "small" => s.size = base.size - 2.0,
        "del" => s.color = 0x00_cc_77_77,
        _ => {}
    }
    s
}

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div" | "section" | "article" | "header" | "footer" | "nav" | "aside"
            | "main" | "blockquote" | "figure" | "figcaption" | "ul" | "ol" | "li" | "dl"
            | "dt" | "dd" | "table" | "tr" | "td" | "th" | "caption" | "form" | "fieldset"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "hr" | "address" | "center"
    )
}

/// Replaced-element payload for `<img>`: src before fetch, intrinsic size
/// after the header probe, pixels only after layout (downscaled to display
/// size — RAM rule). Failed/unsupported stay dim-only → box reserves space,
/// nothing draws.
pub enum ImgSlot {
    Pending(std::rc::Rc<str>),
    /// fetched, header parsed: intrinsic size known, pixels not decoded
    Probed { src: std::rc::Rc<str>, w: u32, h: u32 },
    Ready(std::rc::Rc<ImageData>),
}

impl ImgSlot {
    /// intrinsic (w, h) if known
    pub fn dims(&self) -> Option<(u32, u32)> {
        match self {
            ImgSlot::Pending(_) => None,
            ImgSlot::Probed { w, h, .. } => Some((*w, *h)),
            ImgSlot::Ready(d) => Some((d.w, d.h)),
        }
    }
}

/// Box tree node: a text run (leaf) or an element box (block/flex/grid).
pub struct BoxNode {
    pub is_text: bool,
    pub style: Style,
    pub bx: BoxStyle,
    pub text: String,
    /// forced line break (<br>/<hr>)
    pub hard_break: bool,
    /// raw href of the enclosing <a>, resolved against base URL only on click
    pub link: Option<std::rc::Rc<str>>,
    /// `<img>` replacement data (None for every other box)
    pub img: Option<ImgSlot>,
    pub children: Vec<BoxNode>,
    // layout output (absolute CSS px)
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// last line still has content (block flow paragraph-gap bookkeeping)
    pub open: bool,
    /// range of `runs` this subtree emitted during place() — phase-2
    /// repositioning shifts exactly it (nested ranges compose)
    pub run_range: (u32, u32),
    /// same for `underline_rects`
    pub ul_range: (u32, u32),
}

impl BoxNode {
    fn group(style: Style, bx: BoxStyle) -> Self {
        BoxNode {
            is_text: false,
            style,
            bx,
            text: String::new(),
            hard_break: false,
            link: None,
            img: None,
            children: Vec::new(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            open: false,
            run_range: (0, 0),
            ul_range: (0, 0),
        }
    }

    fn leaf(style: Style, text: String, hard_break: bool, link: Option<std::rc::Rc<str>>) -> Self {
        BoxNode {
            is_text: true,
            style,
            bx: BoxStyle::default(),
            text,
            hard_break,
            link,
            img: None,
            children: Vec::new(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            open: false,
            run_range: (0, 0),
            ul_range: (0, 0),
        }
    }
}

/// Elements whose content is never rendered (also covers `<style>`/`<script>`
/// that appear inside body — head is already excluded by `body_root`).
const NON_RENDERED: &[&str] = &[
    "script", "style", "noscript", "template", "head", "title", "meta", "link",
];

/// Build the box tree for `node`'s subtree. `style`/`bx` are the cascade
/// result for `node` itself (main resolves body before calling).
pub fn build(dom: &Dom, node: u32, style: Style, bx: BoxStyle, sheet: &Sheet) -> BoxNode {
    let mut root = BoxNode::group(style, bx);
    build_into(dom, node, style, sheet, None, &mut root.children);
    root
}

/// Total node count (for the [mem] log).
pub fn count_boxes(n: &BoxNode) -> usize {
    1 + n.children.iter().map(count_boxes).sum::<usize>()
}

fn build_into(
    dom: &Dom,
    node: u32,
    style: Style,
    sheet: &Sheet,
    link: Option<std::rc::Rc<str>>,
    out: &mut Vec<BoxNode>,
) {
    for &child in &dom.nodes[node as usize].children {
        match &dom.nodes[child as usize].kind {
            NodeKind::Text(t) => {
                let t = t.trim_matches('\u{a0}');
                if !t.is_empty() {
                    out.push(BoxNode::leaf(style, t.to_string(), false, link.clone()));
                }
            }
            NodeKind::Element(..) => {
                let tag = dom.tag(child);
                if NON_RENDERED.contains(&tag) {
                    continue;
                }
                if tag == "br" || tag == "hr" {
                    out.push(BoxNode::leaf(style, String::new(), true, None));
                    continue;
                }
                let child_style = if tag == "body" { style } else { style_for(tag, style) };
                // author CSS (rules + inline style) cascades over UA defaults;
                // relative font-sizes compute against the TRUE parent (style),
                // not the UA-bumped child_style (headings would compound 2x)
                let outcome = sheet.resolve_with_parent(dom, child, child_style, dom.attr(child, "style"), style.size);
                if outcome.display_none {
                    continue;
                }
                if tag == "img" {
                    // replaced element: box now, pixels when main fetches the src.
                    // width/height attributes are presentational hints (CSS wins:
                    // outcome.bx already holds declared width/height).
                    let mut g = BoxNode::group(outcome.style, outcome.bx);
                    if g.bx.width.is_none() {
                        g.bx.width = dom.attr(child, "width").and_then(|v| v.parse::<f32>().ok()).map(Len::Px);
                    }
                    if g.bx.height.is_none() {
                        g.bx.height = dom.attr(child, "height").and_then(|v| v.parse::<f32>().ok()).map(Len::Px);
                    }
                    if let Some(src) = dom.attr(child, "src").filter(|s| !s.is_empty()) {
                        g.img = Some(ImgSlot::Pending(std::rc::Rc::from(src)));
                    }
                    out.push(g);
                    continue;
                }
                if tag == "svg" {
                    // inline <svg>: the parser kept the raw inner markup as a
                    // text child (svg is RAW_TEXT). Reassemble the full
                    // `<svg attrs>…</svg>` and hand it to the shared image
                    // pipeline (ImgSlot::Pending holding markup, not a URL).
                    let mut markup = String::from("<svg");
                    if let NodeKind::Element(_, attrs) = &dom.nodes[child as usize].kind {
                        for (k, v) in attrs.iter() {
                            markup.push_str(&format!(" {k}=\"{}\"", v.replace('"', "&quot;")));
                        }
                    }
                    markup.push('>');
                    for &gc in &dom.nodes[child as usize].children {
                        if let NodeKind::Text(t) = &dom.nodes[gc as usize].kind {
                            markup.push_str(t);
                        }
                    }
                    markup.push_str("</svg>");
                    // currentColor = the element's cascaded color (class
                    // rules like .text-next-blue already resolved via var())
                    let hex = format!("#{:06x}", outcome.style.color & 0x00ff_ffff);
                    let markup = markup
                        .replace("currentColor", &hex)
                        .replace("currentcolor", &hex);
                    let mut g = BoxNode::group(outcome.style, outcome.bx);
                    if g.bx.width.is_none() {
                        g.bx.width = dom.attr(child, "width").and_then(|v| v.parse::<f32>().ok()).map(Len::Px);
                    }
                    if g.bx.height.is_none() {
                        g.bx.height = dom.attr(child, "height").and_then(|v| v.parse::<f32>().ok()).map(Len::Px);
                    }
                    g.img = Some(ImgSlot::Pending(std::rc::Rc::from(markup)));
                    out.push(g);
                    continue;
                }
                // <a href> opens a link scope; nested <a> is invalid HTML so inherit-or-replace is enough
                let child_link = if tag == "a" {
                    dom.attr(child, "href")
                        .map(|h| std::rc::Rc::from(h))
                        .or_else(|| link.clone())
                } else {
                    link.clone()
                };
                // which elements get a box: flex/grid/block always; inline
                // tags only when CSS says so; Auto follows the tag
                let boxed = match outcome.bx.display {
                    Display::Auto => is_block(tag),
                    Display::Inline => false,
                    _ => true,
                };
                if boxed {
                    let mut g = BoxNode::group(outcome.style, outcome.bx);
                    build_into(dom, child, outcome.style, sheet, child_link, &mut g.children);
                    out.push(g);
                } else {
                    // inline element: no box, children flatten into parent
                    // flow. shortcut: horizontal margins (e.g. HN's
                    // `.hnname{margin-right:5px}`) can't shift inline text, so
                    // they emit one separator space — visually equivalent
                    let has_margin = |l: Len| !matches!(l, Len::Px(v) if v == 0.0);
                    if has_margin(outcome.bx.margin[3]) {
                        out.push(BoxNode::leaf(
                            outcome.style,
                            " ".into(),
                            false,
                            child_link.clone(),
                        ));
                    }
                    build_into(dom, child, outcome.style, sheet, child_link, out);
                    if has_margin(outcome.bx.margin[1]) {
                        out.push(BoxNode::leaf(
                            outcome.style,
                            " ".into(),
                            false,
                            None,
                        ));
                    }
                }
            }
        }
    }
}

/// One painted `box-shadow` — resolved to px at layout. `color` already
/// carries alpha (0xAARRGGBB); drawn as a blurred rounded rect.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ShadowRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
    /// blur radius (0 = hard edge)
    pub blur: f32,
    /// packed alpha × color
    pub color: u32,
    pub fixed: bool,
}

pub struct PositionedRun {
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub style: Style,
    pub width: f32,
    pub link: Option<std::rc::Rc<str>>,
    /// inside a `position: fixed` subtree — drawn in viewport coordinates
    /// (no scroll offset), on top of scrolled content
    pub fixed: bool,
}

pub struct Layout {
    pub runs: Vec<PositionedRun>,
    pub height: f32,
    /// x, y, w, h, color (matches the run's text color), fixed
    pub underline_rects: Vec<(f32, f32, f32, f32, u32, bool)>,
    /// filled box backgrounds (x, y, w, h, color, radius, fixed) — drawn under text
    pub rects: Vec<(f32, f32, f32, f32, u32, f32, bool)>,
    /// decoded `<img>` boxes (x, y, w, h, pixels, fixed) — drawn under text
    pub images: Vec<(f32, f32, f32, f32, std::rc::Rc<ImageData>, bool)>,
    /// box-shadows (under backgrounds), paint order = list order
    pub shadows: Vec<ShadowRect>,
    /// true if the last line still has content on it (no trailing <br>) —
    /// block flow uses this to decide whether to add the paragraph gap
    pub open_line: bool,
}

/// Default viewport height for [`layout_tree`] — matches the fixed window in
/// main(); production passes the real one through [`layout_in`] (tests don't
/// depend on it).
#[cfg(test)]
const VIEWPORT_H: f32 = 676.0;

/// Lay out the box tree at (0, 0) within `max_width`, viewport height
/// [`VIEWPORT_H`] (see [`layout_in`] for the real one).
#[cfg(test)]
pub fn layout_tree(
    root: &mut BoxNode,
    max_width: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
) -> Layout {
    layout_in(root, max_width, VIEWPORT_H, measure)
}

/// Like [`layout_tree`] but with the real viewport height — `position: fixed`
/// boxes resolve `bottom`/`right` insets against (0, 0, max_width, vh).
pub fn layout_in(
    root: &mut BoxNode,
    max_width: f32,
    viewport_h: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
) -> Layout {
    let mut runs = Vec::new();
    let mut uls = Vec::new();
    // initial containing block = viewport (0, 0, max_width, vh)
    let cb = (0.0f32, 0.0f32, max_width, viewport_h);
    place(root, 0.0, 0.0, max_width, measure, &mut runs, &mut uls);
    // phase 2: resolve relative/absolute/fixed against their containing
    // blocks now that every in-flow position is final (root flow box = cb)
    position_all(root, cb, cb, cb, false, measure, &mut runs, &mut uls);
    let mut rects = Vec::new();
    let mut images = Vec::new();
    let mut shadows = Vec::new();
    collect_paint(root, false, &mut rects, &mut images, &mut shadows);
    Layout {
        runs,
        height: root.h,
        underline_rects: uls,
        rects,
        images,
        shadows,
        open_line: root.open,
    }
}

/// Post-layout walk: every box with a background-color emits a filled rect;
/// every decoded `<img>` emits its pixels (parent-first, so children paint
/// over their parent's fill). `fixed` inherits down — a `position: fixed`
/// subtree paints in viewport coordinates.
fn collect_paint(
    n: &BoxNode,
    fixed: bool,
    out: &mut Vec<(f32, f32, f32, f32, u32, f32, bool)>,
    imgs: &mut Vec<(f32, f32, f32, f32, std::rc::Rc<ImageData>, bool)>,
    shadows: &mut Vec<ShadowRect>,
) {
    if !n.is_text {
        let fixed = fixed || n.bx.pos == Pos::Fixed;
        let bt = resolve_len(n.bx.border[0], n.w);
        let br = resolve_len(n.bx.border[1], n.w);
        let bb = resolve_len(n.bx.border[2], n.w);
        let bl = resolve_len(n.bx.border[3], n.w);
        let has_b = bt > 0.0 || br > 0.0 || bb > 0.0 || bl > 0.0;
        // currentcolor: unspecified border color = the element's text color
        let bc = n.bx.bcolor.unwrap_or(n.style.color);
        let r = resolve_len(n.bx.radius, n.w).min(n.w * 0.5).min(n.h * 0.5);
        let ring = has_b && r > 0.0 && n.bx.bg.is_some();
        // box-shadows: spread grows the rect, blur softens it; color
        // resolves to currentcolor when unspecified, alpha from the color
        for s in &n.bx.shadows {
            let sp = s.spread;
            let c = s.color.unwrap_or(n.style.color);
            let a = s.alpha as u32;
            shadows.push(ShadowRect {
                x: n.x + s.dx - sp,
                y: n.y + s.dy - sp,
                w: (n.w + sp * 2.0).max(0.0),
                h: (n.h + sp * 2.0).max(0.0),
                radius: r,
                blur: s.blur,
                color: (a << 24) | c,
                fixed,
            });
        }
        if let Some(c) = n.bx.bg {
            if ring {
                // rounded ring: fill the border-box with the border color,
                // then the background inset over it (inner radius ≈ r − border)
                out.push((n.x, n.y, n.w, n.h, bc, r, fixed));
                let inner = (r - bt.max(br).max(bb).max(bl)).max(0.0);
                out.push((
                    n.x + bl,
                    n.y + bt,
                    (n.w - bl - br).max(0.0),
                    (n.h - bt - bb).max(0.0),
                    c,
                    inner,
                    fixed,
                ));
            } else {
                out.push((n.x, n.y, n.w, n.h, c, r, fixed));
            }
        }
        if has_b && !ring {
            // four side strips over the background (square corners on a
            // rounded backgroundless box — shortcut)
            if bt > 0.0 {
                out.push((n.x, n.y, n.w, bt, bc, 0.0, fixed));
            }
            if bb > 0.0 {
                out.push((n.x, n.y + n.h - bb, n.w, bb, bc, 0.0, fixed));
            }
            if bl > 0.0 {
                out.push((n.x, n.y + bt, bl, (n.h - bt - bb).max(0.0), bc, 0.0, fixed));
            }
            if br > 0.0 {
                out.push((n.x + n.w - br, n.y + bt, br, (n.h - bt - bb).max(0.0), bc, 0.0, fixed));
            }
        }
        if let Some(ImgSlot::Ready(d)) = &n.img {
            if n.w > 0.0 && n.h > 0.0 {
                imgs.push((n.x, n.y, n.w, n.h, d.clone(), fixed));
            }
        }
        for ch in &n.children {
            collect_paint(ch, fixed, out, imgs, shadows);
        }
    }
}

/// Symbolic length → px against the container width.
fn resolve_len(l: Len, avail: f32) -> f32 {
    match l {
        Len::Px(p) => p,
        Len::Pct(p) => p / 100.0 * avail,
    }
}

fn leaf_atom(n: &BoxNode) -> Atom {
    Atom {
        text: n.text.clone(),
        style: n.style,
        hard_break: n.hard_break,
        link: n.link.clone(),
    }
}

/// Shift an already-laid subtree by (dx, dy) — used by flex cross-axis align
/// and by the paragraph-gap insertion after the child was placed.
fn shift_tree(n: &mut BoxNode, dx: f32, dy: f32) {
    n.x += dx;
    n.y += dy;
    for c in &mut n.children {
        shift_tree(c, dx, dy);
    }
}

/// Containing block (x, y, w, h) — the nearest positioned ancestor's padding
/// box, or the viewport for `position: fixed`.
type CB = (f32, f32, f32, f32);

/// Place one node. `avail` = containing block's content width.
/// Returns the outer vertical advance (margin-top + border-box height +
/// margin-bottom) so block flow can stack siblings without overlap.
fn place(
    n: &mut BoxNode,
    x: f32,
    y: f32,
    avail: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
    runs: &mut Vec<PositionedRun>,
    uls: &mut Vec<UR>,
) -> f32 {
    // every run/underline this subtree emits lands in [run0..]/[ul0..] —
    // stored on the node so phase-2 repositioning can shift exactly that
    // range (nested shifts compose; appends never move existing entries)
    let (run0, ul0) = (runs.len(), uls.len());
    n.x = x;
    n.y = y;
    debug_assert!(!n.is_text, "text leaves are batched by their container");
    // margins (symbolic % resolves against the container width)
    let ml = resolve_len(n.bx.margin[3], avail);
    let mr = resolve_len(n.bx.margin[1], avail);
    let mt = resolve_len(n.bx.margin[0], avail);
    let mb = resolve_len(n.bx.margin[2], avail);
    n.x += ml;
    n.y += mt;
    // resolve border-box width
    let mut w = (avail - ml - mr).max(0.0);
    if let Some(l) = n.bx.width {
        w = resolve_len(l, avail);
    }
    if let Some(m) = n.bx.max_width {
        w = w.min(resolve_len(m, avail));
    }
    // replaced element with known intrinsic size: width unless overridden
    if n.img.is_some() && n.bx.width.is_none() {
        if let Some((iw, _)) = n.img.as_ref().and_then(ImgSlot::dims) {
            w = (iw as f32).min(avail);
        }
    }
    if n.bx.margin_x_auto {
        n.x = x + (avail - w).max(0.0) * 0.5;
    }
    // content box = border box inset by borders + padding (shortcut:
    // border-box model — an explicit `width` includes both, matching most
    // modern sites)
    let pl = resolve_len(n.bx.pad[3], w);
    let pr = resolve_len(n.bx.pad[1], w);
    let pt = resolve_len(n.bx.pad[0], w);
    let pb = resolve_len(n.bx.pad[2], w);
    let bl = resolve_len(n.bx.border[3], w);
    let br = resolve_len(n.bx.border[1], w);
    let bt = resolve_len(n.bx.border[0], w);
    let bb = resolve_len(n.bx.border[2], w);
    let (bx0, by0) = (n.x, n.y);
    n.x += pl + bl;
    n.y += pt + bt;
    n.w = (w - pl - pr - bl - br).max(0.0);
    if n.img.is_some() {
        // replaced element: no children — height from declared height,
        // else aspect ratio of the content width (no size known: 0 → invisible)
        n.h = match (n.bx.height, n.img.as_ref().and_then(ImgSlot::dims)) {
            (Some(l), _) => resolve_len(l, avail),
            (None, Some((iw, ih))) => n.w * ih as f32 / (iw as f32).max(1.0),
            (None, None) => 0.0,
        };
    } else {
        match (n.bx.display, n.bx.dir) {
            (Display::Flex, Dir::Row) => flex_row(n, measure, runs, uls),
            (Display::Flex, Dir::Column) => flex_col(n, measure, runs, uls),
            (Display::Grid, _) => grid(n, measure, runs, uls),
            _ => block(n, measure, runs, uls), // Block / Auto(block tag)
        }
    }
    // min-height: floor the content height. A flex column with vertical
    // `justify` pushes its stack into the freed space (hero uses
    // `min-h-[560px] justify-end` so the NEXT outline floats above the pill).
    // shortcut: `%` min-height resolves against width, not the container
    // height (real pages use px here).
    if let Some(mh) = n.bx.min_height {
        let min_px = resolve_len(mh, avail);
        if n.h < min_px {
            let dy = match (n.bx.display, n.bx.dir, n.bx.justify) {
                (Display::Flex, Dir::Column, Justify::Center) => (min_px - n.h) * 0.5,
                (Display::Flex, Dir::Column, Justify::End) => min_px - n.h,
                _ => 0.0,
            };
            if dy != 0.0 {
                for ch in &mut n.children {
                    shift_tree(ch, 0.0, dy);
                }
                for r in &mut runs[run0..] {
                    r.y += dy;
                }
                for u in &mut uls[ul0..] {
                    u.1 += dy;
                }
            }
            n.h = min_px;
        }
    }
    // restore border-box coords; height includes vertical padding + borders
    n.h += pt + pb + bt + bb;
    n.x = bx0;
    n.y = by0;
    n.w = w;
    n.run_range = (run0 as u32, runs.len() as u32);
    n.ul_range = (ul0 as u32, uls.len() as u32);
    mt + n.h + mb
}

/// Phase-2 walk — resolve non-static boxes once every in-flow position is
/// final (parent-first so nested relative/absolute shifts compose).
/// `cb` = nearest positioned ancestor's padding box (abs), `flow` = parent's
/// content box (relative % insets), `vcb` = viewport (fixed), `fixed` =
/// inside a `position: fixed` subtree (paints without scroll offset).
fn position_all(
    n: &mut BoxNode,
    cb: CB,
    flow: CB,
    vcb: CB,
    fixed: bool,
    measure: &dyn Fn(&str, &Style) -> f32,
    runs: &mut Vec<PositionedRun>,
    uls: &mut Vec<UR>,
) {
    let pos = n.bx.pos;
    let fixed = fixed || pos == Pos::Fixed;
    let inset = n.bx.inset; // [Option<Len>; 4] is Copy — avoid borrow conflicts
    match pos {
        // in flow already (also the root, laid by layout_in)
        Pos::Static => {}
        // stays in flow, just shifts — dx = left (wins) else −right
        Pos::Relative => {
            let dx = match (inset[3], inset[1]) {
                (Some(l), _) => resolve_len(l, flow.2),
                (_, Some(r)) => -resolve_len(r, flow.2),
                _ => 0.0,
            };
            let dy = match (inset[0], inset[2]) {
                (Some(t), _) => resolve_len(t, flow.3),
                (_, Some(b)) => -resolve_len(b, flow.3),
                _ => 0.0,
            };
            if dx != 0.0 || dy != 0.0 {
                shift_tree(n, dx, dy);
                let (a, b) = n.run_range;
                for r in &mut runs[a as usize..b as usize] {
                    r.x += dx;
                    r.y += dy;
                }
                let (a, b) = n.ul_range;
                for u in &mut uls[a as usize..b as usize] {
                    u.0 += dx;
                    u.1 += dy;
                }
            }
        }
        // left flow during place() — lay it out now against its containing
        // block, then shift to satisfy the insets
        Pos::Absolute | Pos::Fixed => {
            let c = if pos == Pos::Fixed { vcb } else { cb };
            // pre-size: both horizontal insets → stretch; else shrink-to-fit
            // (max-content clamped to the CB — shortcut: no min-content floor)
            // (img keeps its intrinsic width from place())
            if n.bx.width.is_none() && n.img.is_none() {
                n.bx.width = match (inset[3], inset[1]) {
                    (Some(l), Some(r)) => Some(Len::Px(
                        (c.2 - resolve_len(l, c.2) - resolve_len(r, c.2)).max(0.0),
                    )),
                    _ => Some(Len::Px(natural_w(n, c.2, measure).min(c.2))),
                };
            }
            // provisional = CB origin (shortcut: real static position was
            // skipped by the flow); insets resolve % against the CB axis
            place(n, c.0, c.1, c.2, measure, runs, uls);
            let ml = resolve_len(n.bx.margin[3], c.2);
            let mr = resolve_len(n.bx.margin[1], c.2);
            let mt = resolve_len(n.bx.margin[0], c.2);
            let mb = resolve_len(n.bx.margin[2], c.2);
            // over-constrained → top/left wins (ltr)
            let dx = match (inset[3], inset[1]) {
                (Some(l), _) => c.0 + resolve_len(l, c.2) + ml - n.x,
                (_, Some(r)) => c.0 + c.2 - resolve_len(r, c.2) - mr - n.w - n.x,
                _ => 0.0,
            };
            let dy = match (inset[0], inset[2]) {
                (Some(t), _) => c.1 + resolve_len(t, c.3) + mt - n.y,
                (_, Some(b)) => c.1 + c.3 - resolve_len(b, c.3) - mb - n.h - n.y,
                _ => 0.0,
            };
            if dx != 0.0 || dy != 0.0 {
                shift_tree(n, dx, dy);
                let (a, b) = n.run_range;
                for r in &mut runs[a as usize..b as usize] {
                    r.x += dx;
                    r.y += dy;
                }
                let (a, b) = n.ul_range;
                for u in &mut uls[a as usize..b as usize] {
                    u.0 += dx;
                    u.1 += dy;
                }
            }
            if fixed {
                let (a, b) = n.run_range;
                for r in &mut runs[a as usize..b as usize] {
                    r.fixed = true;
                }
                let (a, b) = n.ul_range;
                for u in &mut uls[a as usize..b as usize] {
                    u.5 = true;
                }
            }
        }
    }
    // CB for descendants: positioned boxes expose their padding box; everyone
    // else forwards theirs. The flow box (relative %) = own content box.
    let kid_cb = if pos == Pos::Static {
        cb
    } else {
        let bl = resolve_len(n.bx.border[3], n.w);
        let br = resolve_len(n.bx.border[1], n.w);
        let bt = resolve_len(n.bx.border[0], n.w);
        let bb = resolve_len(n.bx.border[2], n.w);
        (n.x + bl, n.y + bt, (n.w - bl - br).max(0.0), (n.h - bt - bb).max(0.0))
    };
    let pl = resolve_len(n.bx.pad[3], n.w);
    let pr = resolve_len(n.bx.pad[1], n.w);
    let pt = resolve_len(n.bx.pad[0], n.w);
    let pb = resolve_len(n.bx.pad[2], n.w);
    let kid_flow = (
        kid_cb.0 + pl,
        kid_cb.1 + pt,
        (kid_cb.2 - pl - pr).max(0.0),
        (kid_cb.3 - pt - pb).max(0.0),
    );
    for ch in &mut n.children {
        position_all(ch, kid_cb, kid_flow, vcb, fixed, measure, runs, uls);
    }
}

type UR = (f32, f32, f32, f32, u32, bool); // x, y, w, h, color, fixed

fn push_runs(mut src: Vec<PositionedRun>, dx: f32, dy: f32, dst: &mut Vec<PositionedRun>) {
    for r in &mut src {
        r.x += dx;
        r.y += dy;
    }
    dst.append(&mut src);
}

fn push_uls(mut src: Vec<UR>, dx: f32, dy: f32, dst: &mut Vec<UR>) {
    for u in &mut src {
        u.0 += dx;
        u.1 += dy;
    }
    dst.append(&mut src);
}

/// The 1–2 items of a flex/grid row: consecutive text leaves merge into one
/// anonymous item (matching the spec's anonymous flex item).
#[derive(Clone, Copy)]
enum Item {
    Text(usize, usize), // range into children
    Grp(usize),
}

fn items_of(children: &[BoxNode]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < children.len() {
        if children[i].is_text {
            let start = i;
            while i < children.len() && children[i].is_text {
                i += 1;
            }
            // whitespace-only runs are not items (spec: anonymous item
            // creation skips whitespace) — they used to eat a grid track
            if children[start..i].iter().all(|c| c.text.trim().is_empty()) {
                continue;
            }
            out.push(Item::Text(start, i));
        } else if matches!(children[i].bx.pos, Pos::Absolute | Pos::Fixed) {
            i += 1; // out of flow: placed in phase 2 (position_all)
        } else {
            out.push(Item::Grp(i));
            i += 1;
        }
    }
    out
}

/// max-content (unwrapped) width of a node, including horizontal padding.
fn natural_w(n: &BoxNode, avail: f32, measure: &dyn Fn(&str, &Style) -> f32) -> f32 {
    let content = natural_w_inner(n, avail, measure);
    if n.is_text {
        return content;
    }
    content
        + resolve_len(n.bx.pad[3], avail)
        + resolve_len(n.bx.pad[1], avail)
        + resolve_len(n.bx.border[3], avail)
        + resolve_len(n.bx.border[1], avail)
}

fn natural_w_inner(n: &BoxNode, avail: f32, measure: &dyn Fn(&str, &Style) -> f32) -> f32 {
    if n.is_text {
        return if n.hard_break {
            0.0
        } else {
            measure(&n.text, &n.style)
        };
    }
    if let Some(l) = n.bx.width {
        return resolve_len(l, avail);
    }
    if let Some((iw, _)) = n.img.as_ref().and_then(ImgSlot::dims) {
        return iw as f32; // intrinsic (padding added by the wrapper)
    }
    match (n.bx.display, n.bx.dir) {
        (Display::Flex, Dir::Row) => {
            let items = items_of(&n.children);
            let k = items.len();
            let mut sum = 0.0;
            for it in &items {
                sum += item_natural(n, it, avail, measure);
            }
            let w = sum + n.bx.gap_col * (k as f32 - 1.0).max(0.0);
            // a wrapping row fits the available width (fit-content) — sizing
            // it to the unwrapped sum made rust-lang.org's nav overflow and
            // clip its last items, wrap branch never engaging
            if n.bx.wrap && avail > 0.0 {
                w.min(avail)
            } else {
                w
            }
        }
        (Display::Flex, Dir::Column) => n
            .children
            .iter()
            .filter(|c| !matches!(c.bx.pos, Pos::Absolute | Pos::Fixed))
            .map(|c| natural_w(c, avail, measure))
            .fold(0.0f32, f32::max),
        (Display::Grid, _) => {
            if n.bx.cols.is_empty() {
                avail
            } else {
                tracks_w(&n.bx.cols, avail, n.bx.gap_col)
            }
        }
        // block: widest inline run or child box
        _ => {
            let mut m = 0.0f32;
            let mut run = 0.0f32;
            for c in &n.children {
                if matches!(c.bx.pos, Pos::Absolute | Pos::Fixed) {
                    continue; // out of flow: no contribution to shrink-to-fit
                }
                if c.is_text {
                    run += natural_w(c, avail, measure);
                } else {
                    m = m.max(run).max(natural_w(c, avail, measure));
                    run = 0.0;
                }
            }
            m.max(run)
        }
    }
}

fn item_natural(
    parent: &BoxNode,
    it: &Item,
    avail: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
) -> f32 {
    match it {
        Item::Text(a, b) => parent.children[*a..*b]
            .iter()
            .map(|c| natural_w(c, avail, measure))
            .sum(),
        Item::Grp(i) => natural_w(&parent.children[*i], avail, measure),
    }
}

/// Resolved track widths + gaps: returns total width of all tracks+gaps.
fn tracks_w(cols: &[Track], avail: f32, gap: f32) -> f32 {
    let mut fixed = 0.0;
    let mut fr = 0.0;
    for t in cols {
        match t {
            Track::Px(p) => fixed += p,
            Track::Pct(p) => fixed += p / 100.0 * avail,
            Track::Fr(f) => fr += f,
        }
    }
    let gaps = gap * (cols.len() as f32 - 1.0).max(0.0);
    let free = (avail - fixed - gaps).max(0.0);
    fixed + if fr > 0.0 { free } else { 0.0 } + gaps
}

fn resolve_tracks(cols: &[Track], avail: f32, gap: f32) -> Vec<f32> {
    let mut fixed = 0.0;
    let mut fr = 0.0;
    for t in cols {
        match t {
            Track::Px(p) => fixed += p,
            Track::Pct(p) => fixed += p / 100.0 * avail,
            Track::Fr(f) => fr += f,
        }
    }
    let gaps = gap * (cols.len() as f32 - 1.0).max(0.0);
    let free = (avail - fixed - gaps).max(0.0);
    cols.iter()
        .map(|t| match t {
            Track::Px(p) => *p,
            Track::Pct(p) => p / 100.0 * avail,
            Track::Fr(f) => {
                if fr > 0.0 {
                    free * f / fr
                } else {
                    0.0
                }
            }
        })
        .collect()
}

/// Batch consecutive text leaves starting at `i`, lay them as one wrapped
/// segment at (n.x, y). Returns (height, open_line), advances `i`.
fn lay_text_run(
    n: &mut BoxNode,
    i: &mut usize,
    y: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
    runs: &mut Vec<PositionedRun>,
    uls: &mut Vec<UR>,
) -> (f32, bool) {
    let mut atoms = Vec::new();
    while *i < n.children.len() && n.children[*i].is_text {
        atoms.push(leaf_atom(&n.children[*i]));
        *i += 1;
    }
    let l = layout(&atoms, n.w, measure);
    push_runs(l.runs, n.x, y, runs);
    push_uls(l.underline_rects, n.x, y, uls);
    (l.height, l.open_line)
}

/// Block flow: consecutive text leaves become wrapped line segments; child
/// boxes are placed at the running y. The 0.6*size paragraph gap is added at
/// block boundaries (matches the legacy hard_break semantics: gap only after
/// content, never between empty breaks).
fn block(n: &mut BoxNode, measure: &dyn Fn(&str, &Style) -> f32, runs: &mut Vec<PositionedRun>, uls: &mut Vec<UR>) {
    let mut y = n.y; // absolute — matches place()/push_runs
    let mut open = false;
    let mut i = 0;
    while i < n.children.len() {
        if n.children[i].is_text {
            // text run after an open line: leading break of an anonymous
            // block, style = this block's style
            if open {
                y += n.style.size * 0.6;
            }
            let (h, o) = lay_text_run(n, &mut i, y, measure, runs, uls);
            y += h;
            open = o;
        } else {
            if matches!(n.children[i].bx.pos, Pos::Absolute | Pos::Fixed) {
                i += 1; // out of flow: sized/placed by position_all (phase 2)
                continue;
            }
            if open {
                y += n.style.size * 0.6; // leading break before child box
                open = false;
            }
            let adv = place(&mut n.children[i], n.x, y, n.w, measure, runs, uls);
            y += adv;
            if n.children[i].open {
                // child's trailing break: gap styled by the child itself
                y += n.children[i].style.size * 0.6;
            }
            i += 1;
        }
    }
    n.h = y - n.y;
    n.open = open;
}

/// Flex row: max-content sizing with proportional shrink on overflow,
/// justify distributes free space, cross-axis align shifts items into the
/// row height. shortcut: no wrap, no grow/basis, stretch = start.
fn flex_row(n: &mut BoxNode, measure: &dyn Fn(&str, &Style) -> f32, runs: &mut Vec<PositionedRun>, uls: &mut Vec<UR>) {
    let items = items_of(&n.children);
    let k = items.len();
    if k == 0 {
        n.h = 0.0;
        n.open = false;
        return;
    }
    let gap = n.bx.gap_col;
    let mut widths: Vec<f32> = items.iter().map(|it| item_natural(n, it, n.w, measure)).collect();
    let natural_sum: f32 = widths.iter().sum();
    let gaps = gap * (k as f32 - 1.0).max(0.0);
    // nowrap shrinks to fit; wrap keeps natural widths and breaks lines
    if !n.bx.wrap && natural_sum + gaps > n.w && natural_sum > 0.0 {
        let scale = ((n.w - gaps).max(0.0)) / natural_sum;
        for w in &mut widths {
            *w *= scale;
        }
    }
    // wrap: greedily split items into lines that fit the content width
    let lines: Vec<std::ops::Range<usize>> = if n.bx.wrap {
        let mut out = Vec::new();
        let (mut start, mut used) = (0usize, 0.0f32);
        for i in 0..k {
            let need = widths[i] + if i > start { gap } else { 0.0 };
            if i > start && used + need > n.w {
                out.push(start..i);
                start = i;
                used = 0.0;
            }
            used += need;
        }
        out.push(start..k);
        out
    } else {
        vec![0..k]
    };
    let mut y = n.y;
    let mut total_h = 0.0f32;
    let mut last_open = false;
    for (li, line) in lines.iter().enumerate() {
        if li > 0 {
            y += n.bx.gap_row; // row-gap between wrapped lines
        }
        let lh = flex_line(n, &items, &widths, line.clone(), y, gap, measure, runs, uls, &mut last_open);
        y += lh;
        total_h += lh + if li > 0 { n.bx.gap_row } else { 0.0 };
    }
    n.h = total_h;
    n.open = last_open;
}

/// Place one flex line (justify + cross-axis align); returns the line height.
#[allow(clippy::too_many_arguments)]
fn flex_line(
    n: &mut BoxNode,
    items: &[Item],
    widths: &[f32],
    line: std::ops::Range<usize>,
    y: f32,
    gap: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
    runs: &mut Vec<PositionedRun>,
    uls: &mut Vec<UR>,
    last_open: &mut bool,
) -> f32 {
    let k = line.len();
    let used: f32 = line.clone().map(|i| widths[i]).sum::<f32>() + gap * (k as f32 - 1.0).max(0.0);
    let free = (n.w - used).max(0.0);
    let (x_off, extra) = match n.bx.justify {
        Justify::Center => (free * 0.5, 0.0),
        Justify::End => (free, 0.0),
        Justify::SpaceBetween if k > 1 => (0.0, free / (k as f32 - 1.0)),
        _ => (0.0, 0.0), // Start
    };
    let mut row_h = 0.0f32;
    let mut x = n.x + x_off;
    let mut marks: Vec<(usize, usize, f32)> = Vec::with_capacity(k); // run_start, ul_start, h
    for idx in line.clone() {
        let w = widths[idx];
        let rs = runs.len();
        let us = uls.len();
        let (ih, io) = match items[idx] {
            Item::Text(a, b) => {
                let mut atoms = Vec::new();
                for c in &n.children[a..b] {
                    atoms.push(leaf_atom(c));
                }
                let l = layout(&atoms, w, measure);
                push_runs(l.runs, x, y, runs);
                push_uls(l.underline_rects, x, y, uls);
                (l.height, l.open_line)
            }
            Item::Grp(i) => {
                let adv = place(&mut n.children[i], x, y, w, measure, runs, uls);
                (adv, n.children[i].open) // cross-size = margin box
            }
        };
        marks.push((rs, us, ih));
        row_h = row_h.max(ih);
        *last_open = io;
        x += w + gap + extra;
    }
    // cross-axis align: shift each item (runs slice + box coords) into row_h
    for (idx, &(rs, us, h)) in marks.iter().enumerate() {
        let dy = match n.bx.align {
            Align::Center => (row_h - h) * 0.5,
            Align::End => row_h - h,
            _ => 0.0, // Start / Stretch (stretch: shortcut, no reflow)
        };
        if dy == 0.0 {
            continue;
        }
        let end = marks.get(idx + 1).map(|m| m.0).unwrap_or(runs.len());
        let uend = marks.get(idx + 1).map(|m| m.1).unwrap_or(uls.len());
        for r in &mut runs[rs..end] {
            r.y += dy;
        }
        for u in &mut uls[us..uend] {
            u.1 += dy;
        }
        if let Item::Grp(i) = items[line.start + idx] {
            shift_tree(&mut n.children[i], 0.0, dy);
        }
    }
    row_h
}

/// Flex column: stack items with row-gap. shortcut: justify is a no-op (no
/// height prop yet); text items lay full-width (text-align centers them),
/// box items honor align-items on the x axis.
fn flex_col(n: &mut BoxNode, measure: &dyn Fn(&str, &Style) -> f32, runs: &mut Vec<PositionedRun>, uls: &mut Vec<UR>) {
    let items = items_of(&n.children);
    if items.is_empty() {
        n.h = 0.0;
        n.open = false;
        return;
    }
    let mut y = n.y;
    let mut last_open = false;
    for (idx, it) in items.iter().enumerate() {
        if idx > 0 {
            y += n.bx.gap_row;
        }
        let rs = runs.len();
        let us = uls.len();
        let (ih, io) = match it {
            Item::Text(a, b) => {
                let mut atoms = Vec::new();
                for c in &n.children[*a..*b] {
                    atoms.push(leaf_atom(c));
                }
                let l = layout(&atoms, n.w, measure);
                push_runs(l.runs, n.x, y, runs);
                push_uls(l.underline_rects, n.x, y, uls);
                (l.height, l.open_line)
            }
            Item::Grp(i) => {
                let adv = place(&mut n.children[*i], n.x, y, n.w, measure, runs, uls);
                let dx = match n.bx.align {
                    Align::Center => (n.w - n.children[*i].w) * 0.5,
                    Align::End => n.w - n.children[*i].w,
                    _ => 0.0,
                };
                if dx != 0.0 {
                    shift_tree(&mut n.children[*i], dx, 0.0);
                    for r in &mut runs[rs..] {
                        r.x += dx;
                    }
                    for u in &mut uls[us..] {
                        u.0 += dx;
                    }
                }
                (adv, n.children[*i].open)
            }
        };
        y += ih;
        last_open = io;
    }
    n.h = y - n.y;
    n.open = last_open;
}

/// Grid: cells over resolved tracks; row height = tallest cell, row-gap
/// between rows. Items with `grid-column: <name>` place into the
/// [name-start]…[name-end] column span (stacking in successive rows);
/// auto items flow row-major. shortcut: named and auto items can share a
/// row index (overlap) — real grids resolve full placement sparsely.
fn grid(n: &mut BoxNode, measure: &dyn Fn(&str, &Style) -> f32, runs: &mut Vec<PositionedRun>, uls: &mut Vec<UR>) {
    let items = items_of(&n.children);
    if items.is_empty() {
        n.h = 0.0;
        n.open = false;
        return;
    }
    let cols: Vec<f32> = if n.bx.cols.is_empty() {
        vec![n.w]
    } else {
        resolve_tracks(&n.bx.cols, n.w, n.bx.gap_col)
    };
    let nc = cols.len();
    // x offset + width of columns [c0, c1)
    let span_xw = |c0: usize, c1: usize| {
        let x: f32 = cols[..c0].iter().sum::<f32>() + n.bx.gap_col * c0 as f32;
        let w: f32 = cols[c0..c1].iter().sum::<f32>() + n.bx.gap_col * (c1 - c0 - 1) as f32;
        (x, w)
    };
    // resolve each item's (row, c0, c1)
    let resolve_col = |name: &str| -> Option<(usize, usize)> {
        let line = |suffix: &str| {
            n.bx
                .col_lines
                .iter()
                .find(|(l, _)| l == name || l == &format!("{name}{suffix}"))
                .map(|(_, i)| *i)
        };
        let s = line("-start")?;
        let e = line("-end").filter(|&e| e > s).unwrap_or(s + 1);
        Some((s, e.min(nc)))
    };
    let mut col_row = vec![0usize; nc]; // next free row per column (named)
    let mut ar = 0usize; // auto cursor row
    let mut ac = 0usize; // auto cursor col
    let mut spots: Vec<(usize, usize, usize)> = Vec::with_capacity(items.len());
    for it in &items {
        let named = match it {
            Item::Grp(i) => n.children[*i]
                .bx
                .grid_col
                .as_deref()
                .and_then(resolve_col),
            Item::Text(..) => None,
        };
        spots.push(match named {
            Some((s, e)) => {
                let r = (s..e).map(|c| col_row[c]).max().unwrap_or(0);
                for c in s..e {
                    col_row[c] = r + 1;
                }
                (r, s, e)
            }
            None => {
                let sp = (ar, ac, (ac + 1).min(nc));
                ac += 1;
                if ac >= nc {
                    ac = 0;
                    ar += 1;
                }
                sp
            }
        });
    }
    let nrows = spots.iter().map(|s| s.0).max().unwrap_or(0) + 1;
    let mut y = n.y;
    let mut last_open = false;
    for r in 0..nrows {
        let mut row_h = 0.0f32;
        let mut marks: Vec<(usize, usize, f32)> = Vec::with_capacity(nc); // run_start, ul_start, h
        let mut row_items: Vec<usize> = (0..items.len()).filter(|&i| spots[i].0 == r).collect();
        row_items.sort_by_key(|&i| spots[i].1);
        let mut cell_ios: Vec<bool> = Vec::with_capacity(nc);
        for &idx in &row_items {
            let (_, c0, c1) = spots[idx];
            let (dx, w) = span_xw(c0, c1);
            let x = n.x + dx;
            let rs = runs.len();
            let us = uls.len();
            let (ih, io) = match items[idx] {
                Item::Text(a, b) => {
                    let mut atoms = Vec::new();
                    for ch in &n.children[a..b] {
                        atoms.push(leaf_atom(ch));
                    }
                    let l = layout(&atoms, w, measure);
                    push_runs(l.runs, x, y, runs);
                    push_uls(l.underline_rects, x, y, uls);
                    (l.height, l.open_line)
                }
                Item::Grp(i) => {
                    let adv = place(&mut n.children[i], x, y, w, measure, runs, uls);
                    (adv, n.children[i].open)
                }
            };
            marks.push((rs, us, ih));
            cell_ios.push(io);
            row_h = row_h.max(ih);
        }
        // cross-axis align within the row (ranges are safe: the next row
        // hasn't been laid yet)
        for (ci, &(rs, us, h)) in marks.iter().enumerate() {
            let dy = match n.bx.align {
                Align::Center => (row_h - h) * 0.5,
                Align::End => row_h - h,
                _ => 0.0,
            };
            if dy == 0.0 {
                continue;
            }
            let end = marks.get(ci + 1).map(|m| m.0).unwrap_or(runs.len());
            let uend = marks.get(ci + 1).map(|m| m.1).unwrap_or(uls.len());
            for rr in &mut runs[rs..end] {
                rr.y += dy;
            }
            for u in &mut uls[us..uend] {
                u.1 += dy;
            }
            if let Item::Grp(i) = items[row_items[ci]] {
                shift_tree(&mut n.children[i], 0.0, dy);
            }
        }
        last_open = cell_ios.last().copied().unwrap_or(false);
        y += row_h;
        if r + 1 < nrows {
            y += n.bx.gap_row;
        }
    }
    n.h = y - n.y;
    n.open = last_open;
}
pub fn layout(atoms: &[Atom], max_width: f32, measure: &dyn Fn(&str, &Style) -> f32) -> Layout {
    let mut runs = Vec::new();
    let mut underline_rects = Vec::new();
    let mut y = 0.0f32;
    let mut x = 0.0f32;
    let mut line_height = 0.0f32;

    // split text into word atoms first so wrapping is per-word
    let mut words: Vec<Atom> = Vec::new();
    for a in atoms {
        if a.hard_break {
            words.push(Atom {
                text: String::new(),
                style: a.style,
                hard_break: true,
                link: None,
            });
            continue;
        }
        // split on any whitespace, remembering which had a trailing space
        // (empty split pieces at the boundaries ARE meaningful: they mean
        // this atom starts/ends with whitespace that must separate it from
        // the neighboring inline element's text)
        let mut first = true;
        let mut pending_ws = false;
        for w in a.text.split(|c: char| c.is_whitespace()) {
            if w.is_empty() {
                pending_ws = true;
                continue;
            }
            if !first || pending_ws {
                words.push(Atom {
                    text: " ".into(),
                    style: a.style,
                    hard_break: false,
                    link: a.link.clone(),
                });
            }
            first = false;
            pending_ws = false;
            words.push(Atom {
                text: w.to_string(),
                style: a.style,
                hard_break: false,
                link: a.link.clone(),
            });
        }
        // atom ends with whitespace → separator before the next atom's text
        // (a space atom at line start is a no-op: the draw loop only
        // advances x when x > 0)
        if pending_ws {
            words.push(Atom {
                text: " ".into(),
                style: a.style,
                hard_break: false,
                link: a.link.clone(),
            });
        }
    }

    let space_w = |s: &Style| measure(" ", s);

    for w in &words {
        if w.hard_break {
            if x > 0.0 {
                y += line_height;
                x = 0.0;
                line_height = 0.0;
                y += w.style.size * 0.6; // paragraph gap only after content, not between empty breaks
            }
            continue;
        }
        if w.text == " " {
            if x > 0.0 {
                x += space_w(&w.style);
            }
            continue;
        }
        let ww = measure(&w.text, &w.style);
        if x > 0.0 && x + ww > max_width && !w.style.nowrap {
            y += line_height;
            x = 0.0;
            line_height = 0.0;
        }
        let lh = w.style.line_height();
        runs.push(PositionedRun {
            x,
            y,
            text: w.text.clone(),
            style: w.style,
            width: ww,
            link: w.link.clone(),
            fixed: false,
        });
        if w.style.underline {
            underline_rects.push((x, y + lh - 2.0, ww, 1.0, w.style.color, false));
        }
        x += ww;
        line_height = line_height.max(lh);
    }
    y += line_height;

    // text-align: shift each completed line inside max_width
    let mut i = 0;
    while i < runs.len() {
        let y0 = runs[i].y;
        let align = runs[i].style.text_align;
        let sz = runs[i].style.size;
        let mut j = i;
        let mut lw = 0.0f32;
        while j < runs.len() && runs[j].y == y0 {
            lw = lw.max(runs[j].x + runs[j].width);
            j += 1;
        }
        let dx = match align {
            1 => (max_width - lw) * 0.5,
            2 => max_width - lw,
            _ => 0.0,
        };
        if dx != 0.0 {
            for r in &mut runs[i..j] {
                r.x += dx;
            }
            let band = y0 + sz * 1.4;
            for u in underline_rects.iter_mut() {
                if u.1 >= y0 && u.1 < band {
                    u.0 += dx;
                }
            }
        }
        i = j;
    }

    Layout {
        runs,
        height: y,
        underline_rects,
        rects: Vec::new(),
        images: Vec::new(),
        shadows: Vec::new(),
        open_line: x > 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_atom(text: &str, hard: bool) -> Atom {
        Atom {
            text: text.into(),
            style: Style::new(),
            hard_break: hard,
            link: None,
        }
    }

    #[test]
    fn wraps_at_width() {
        let atoms = vec![mk_atom("word word word word", false)];
        // 4px per char + space => 9px words, wraps at 20px
        let l = layout(&atoms, 20.0, &|s, _| s.len() as f32 * 4.0);
        let ys: std::collections::HashSet<i32> =
            l.runs.iter().map(|r| r.y as i32).collect();
        assert!(ys.len() >= 2, "expected multiple lines, got {}", ys.len());
        for r in &l.runs {
            assert!(r.x + r.width <= 20.0 + 0.001, "run overflows: {:?}", r.text);
        }
    }

    #[test]
    fn inline_boundary_spaces_survive() {
        // "create " + <strong>"high-quality"</strong> + " with ..." — trailing/
        // leading whitespace at atom edges must become space atoms (regression:
        // rendered as "createhigh-quality ... applicationswith")
        let atoms = vec![
            mk_atom("to create ", false),
            mk_atom("high-quality", false),
            mk_atom(" with power", false),
        ];
        let l = layout(&atoms, 500.0, &|s, _| s.len() as f32 * 4.0);
        let texts: Vec<&str> = l.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["to", "create", "high-quality", "with", "power"]);
        // "create" and "high-quality" are separated by actual x space
        let create = l.runs.iter().find(|r| r.text == "create").unwrap();
        let hq = l.runs.iter().find(|r| r.text == "high-quality").unwrap();
        assert!(hq.x >= create.x + create.width, "no gap before strong");
        // whitespace-only atom (between two inline elements) also separates
        let l2 = layout(&[mk_atom("a", false), mk_atom(" ", false), mk_atom("b", false)], 500.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l2.runs.len(), 2);
        assert!(l2.runs[1].x > l2.runs[0].x + l2.runs[0].width, "ws-only atom separates");
    }

    #[test]
    fn hard_break_starts_new_line() {
        let atoms = vec![mk_atom("a", false), mk_atom("", true), mk_atom("b", false)];
        let l = layout(&atoms, 100.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l.runs.len(), 2);
        assert!(l.runs[1].y > l.runs[0].y);
    }

    #[test]
    fn tabs_and_newlines_never_reach_draw() {
        // regression: "\n\t" chunks used to become .notdef box glyphs
        let atoms = vec![mk_atom("main\tmenu\n\theader", false)];
        let l = layout(&atoms, 500.0, &|s, _| s.len() as f32 * 4.0);
        let texts: Vec<&str> = l.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["main", "menu", "header"]);
        for r in &l.runs {
            assert!(r.text.chars().all(|c| !c.is_whitespace()));
        }
    }

    #[test]
    fn long_word_overflows_rather_than_loop() {
        let atoms = vec![mk_atom("aaaaaaaaaa", false)]; // wider than max
        let l = layout(&atoms, 10.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l.runs.len(), 1);
    }

    #[test]
    fn link_survives_word_split() {
        let mut a = mk_atom("click here now", false);
        a.link = Some(std::rc::Rc::from("/page"));
        let l = layout(&[a], 500.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l.runs.len(), 3);
        for r in &l.runs {
            assert_eq!(r.link.as_deref(), Some("/page"));
        }
    }

    #[test]
    fn non_link_runs_have_no_href() {
        let l = layout(&[mk_atom("plain text", false)], 500.0, &|s, _| s.len() as f32 * 4.0);
        assert!(l.runs.iter().all(|r| r.link.is_none()));
    }

    // ---- box tree (Track A) ----

    /// 4px/char measure shared by the box tests.
    fn m4(s: &str, _st: &Style) -> f32 {
        s.len() as f32 * 4.0
    }

    fn leaf(text: &str) -> BoxNode {
        BoxNode::leaf(Style::new(), text.into(), false, None)
    }

    fn flex_row(children: Vec<BoxNode>, bx: BoxStyle) -> BoxNode {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Flex,
                dir: Dir::Row,
                ..bx
            },
        );
        n.children = children;
        n
    }

    fn flex_col(children: Vec<BoxNode>, bx: BoxStyle) -> BoxNode {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Flex,
                dir: Dir::Column,
                ..bx
            },
        );
        n.children = children;
        n
    }

    #[test]
    fn min_height_flex_col_justify_end_pushes_content_down() {
        // hero: `min-h-[560px] flex-col justify-end` — a short stack is pushed
        // to the bottom of the 560px box (the NEXT outline floats above it)
        let mut root = flex_col(
            vec![leaf("pill")],
            BoxStyle {
                min_height: Some(Len::Px(560.0)),
                justify: Justify::End,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        assert_eq!(root.h, 560.0, "box floors to min-height");
        assert_eq!(l.runs.len(), 1);
        // text is pushed to the bottom of the 560px box (not left at the top)
        assert!(l.runs[0].y > 500.0, "y={} should be near the bottom", l.runs[0].y);
        assert!(l.runs[0].y < 560.0, "y={} within the box", l.runs[0].y);
    }

    #[test]
    fn nowrap_text_stays_on_one_line() {
        // `truncate` class (white-space:nowrap) must not wrap — the Ask AI
        // button stacked vertically without it
        let a = Atom {
            text: "Ask AI".into(),
            style: Style {
                nowrap: true,
                ..Style::new()
            },
            hard_break: false,
            link: None,
        };
        // width fits only ~2 chars → would wrap without nowrap
        let l = layout(&[a], 10.0, &|s, st| s.len() as f32 * st.size * 0.5);
        // words are separate runs; nowrap = they share one line (same y)
        assert!(l.runs.len() >= 1);
        let y0 = l.runs[0].y;
        assert!(
            l.runs.iter().all(|r| (r.y - y0).abs() < 0.01),
            "all words on one line"
        );
        assert!(l.height <= l.runs[0].style.line_height() + 0.01, "single line");
    }

    #[test]
    fn min_height_shorter_content_unchanged() {        // content already taller than min-height → no shift, height stays
        // (boxed children: adjacent text leaves merge into one anonymous item)
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut root = flex_col(
            vec![boxed("a"), boxed("b"), boxed("c")],
            BoxStyle {
                min_height: Some(Len::Px(20.0)),
                justify: Justify::End,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        // 3 lines at default 15px × 1.35 line-height
        let content_h = 3.0 * 15.0 * 1.35;
        assert!((root.h - content_h).abs() < 0.01, "h={} content={}", root.h, content_h);
        assert_eq!(l.runs[0].y, 0.0, "first line stays at top");
    }

    #[test]
    fn flex_row_places_items_with_gap() {
        // second item is a box: adjacent text leaves would merge into one
        // anonymous item (that's spec behavior, tested below)
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut root = flex_row(
            vec![boxed("ab"), boxed("cd")],
            BoxStyle {
                gap_col: 10.0,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        assert_eq!(l.runs.len(), 2);
        assert_eq!(l.runs[0].x, 0.0);
        assert_eq!(l.runs[1].x, 8.0 + 10.0, "gap between items");
        assert_eq!(l.runs[0].y, l.runs[1].y, "same row line");
    }

    #[test]
    fn adjacent_text_leaves_merge_into_one_item() {
        let mut root = flex_row(
            vec![leaf("ab"), leaf("cd")],
            BoxStyle {
                gap_col: 10.0,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        // one anonymous item: "ab cd" laid as a single text segment
        // (the space only advances x, it's not a run)
        assert_eq!(l.runs.len(), 2);
        assert_eq!(l.runs[0].x, 0.0);
        assert_eq!(l.runs[1].text, "cd");
        assert!(l.runs[1].x > l.runs[0].x, "space advanced x");
    }

    #[test]
    fn flex_row_shrinks_to_fit() {
        // two 40px items in a 60px box: shrink proportionally, no overflow
        let mut root = flex_row(
            vec![leaf("aaaaaaaaaa"), leaf("bbbbbbbbbb")],
            BoxStyle::default(),
        );
        let l = layout_tree(&mut root, 60.0, &m4);
        assert!(l.runs.iter().all(|r| r.x + r.width <= 60.0 + 0.001), "overflowed");
        assert_eq!(l.runs.len(), 2);
    }

    #[test]
    fn flex_row_wrap_breaks_lines() {
        // two 40px boxes in a 60px box with wrap: no shrink, second item
        // starts a new line (natural widths preserved); boxes, not leaves —
        // adjacent text leaves merge into one anonymous item
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut root = flex_row(
            vec![boxed("aaaaaaaaaa"), boxed("bbbbbbbbbb")],
            BoxStyle {
                wrap: true,
                gap_row: 5.0,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 60.0, &m4);
        assert_eq!(l.runs.len(), 2);
        assert_eq!(l.runs[0].width, 40.0, "natural width kept");
        assert_eq!(l.runs[0].y, l.runs[1].y - 15.0 * 1.35 - 5.0, "second line + gap");
        assert_eq!(l.runs[1].x, 0.0, "wrapped item starts at line start");
    }

    #[test]
    fn flex_row_wrap_keeps_fitting_items_together() {
        // three 20px items, 60px box: all fit one line (no wrap taken)
        let mut root = flex_row(
            vec![leaf("aaaaa"), leaf("bbbbb"), leaf("ccccc")],
            BoxStyle {
                wrap: true,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 60.0, &m4);
        let ys: std::collections::HashSet<i32> = l.runs.iter().map(|r| r.y as i32).collect();
        assert_eq!(ys.len(), 1, "single line when everything fits");
        assert!(l.runs.iter().all(|r| r.width == 20.0), "no shrink with wrap");
    }

    #[test]
    fn flex_row_justify_center() {
        let mut root = flex_row(
            vec![leaf("ab")],
            BoxStyle {
                justify: Justify::Center,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 100.0, &m4);
        // item is 8px wide, centered in 100 → x = 46
        assert!((l.runs[0].x - 46.0).abs() < 0.01, "x={}", l.runs[0].x);
    }

    #[test]
    fn flex_row_align_end_shifts_short_item_down() {
        // item 1: one text line; item 2: block box with 2 lines (via <br>)
        let tall = {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![
                leaf("x"),
                BoxNode::leaf(Style::new(), String::new(), true, None), // <br>
                leaf("y"),
            ];
            g
        };
        let mut root = flex_row(
            vec![leaf("aa"), tall],
            BoxStyle {
                align: Align::End,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        let line = 15.0 * 1.35;
        let gap = 15.0 * 0.6; // <br> paragraph gap inside the tall block
        // short item ("aa") shifted to the bottom of the tall item's height
        let short = l.runs.iter().find(|r| r.text == "aa").unwrap();
        let tall_first = l.runs.iter().find(|r| r.text == "x").unwrap();
        assert!((short.y - (tall_first.y + line + gap)).abs() < 0.01, "short.y={}", short.y);
        assert!((root.h - (line * 2.0 + gap)).abs() < 0.01, "h={}", root.h);
    }

    #[test]
    fn flex_col_stacks_with_row_gap() {
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Flex,
                dir: Dir::Column,
                gap_row: 10.0,
                ..BoxStyle::default()
            },
        );
        n.children = vec![boxed("a"), boxed("b")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert_eq!(l.runs.len(), 2);
        let line = 15.0 * 1.35;
        assert!((l.runs[1].y - (line + 10.0)).abs() < 0.01, "y={}", l.runs[1].y);
    }

    #[test]
    fn grid_three_equal_columns() {
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Grid,
                cols: vec![Track::Fr(1.0), Track::Fr(1.0), Track::Fr(1.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![boxed("a"), boxed("b"), boxed("c"), boxed("d")];
        let l = layout_tree(&mut n, 90.0, &m4);
        assert_eq!(l.runs.len(), 4);
        // row 1: x = 0, 30, 60; row 2 wraps to x = 0 on a new line
        assert_eq!(l.runs[0].x, 0.0);
        assert_eq!(l.runs[1].x, 30.0);
        assert_eq!(l.runs[2].x, 60.0);
        assert_eq!(l.runs[3].x, 0.0);
        let line = 15.0 * 1.35;
        assert!(l.runs[3].y >= line, "second row below first: {}", l.runs[3].y);
    }

    #[test]
    fn width_px_and_margin_auto_center() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                width: Some(Len::Px(50.0)),
                margin_x_auto: true,
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("hello world wrap")];
        let l = layout_tree(&mut n, 150.0, &m4);
        // box x = (150 - 50) / 2 = 50; first run starts at box x
        assert!((n.x - 50.0).abs() < 0.01, "box x={}", n.x);
        assert!((l.runs[0].x - 50.0).abs() < 0.01);
        assert!(n.w <= 50.0 + 0.001);
    }

    #[test]
    fn block_flow_paragraph_gap() {
        // two block children: second starts after first's height + 0.6*size gap
        let mut n = BoxNode::group(Style::new(), BoxStyle::default());
        let mk_block = |t: &str| {
            let mut b = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            b.children = vec![leaf(t)];
            b
        };
        n.children = vec![mk_block("one"), mk_block("two")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert_eq!(l.runs.len(), 2);
        let line = 15.0 * 1.35;
        let expect = line + 15.0 * 0.6;
        assert!((l.runs[1].y - expect).abs() < 0.01, "y={} expect={}", l.runs[1].y, expect);
    }

    #[test]
    fn text_align_center_shifts_line() {
        let mut n = BoxNode::group(Style::new(), BoxStyle::default());
        // text-align is inherited, so the leaf carries it (build_into does this)
        n.children = vec![BoxNode::leaf(
            Style {
                text_align: 1,
                ..Style::new()
            },
            "ab".into(),
            false,
            None,
        )];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert!((l.runs[0].x - 46.0).abs() < 0.01, "x={}", l.runs[0].x);
    }

    // ---- box chrome (padding/margin/line-height/background) ----

    #[test]
    fn padding_insets_content_and_grows_box() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                pad: [Len::Px(10.0), Len::Px(20.0), Len::Px(10.0), Len::Px(20.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("ab")];
        let l = layout_tree(&mut n, 200.0, &m4);
        // content starts at left padding; box height = line + top + bottom pad
        assert!((l.runs[0].x - 20.0).abs() < 0.01, "x={}", l.runs[0].x);
        assert!((l.runs[0].y - 10.0).abs() < 0.01, "y={}", l.runs[0].y);
        assert!((n.h - (15.0 * 1.35 + 20.0)).abs() < 0.01, "h={}", n.h);
        // border-box width unchanged
        assert!((n.w - 200.0).abs() < 0.01);
    }

    #[test]
    fn padding_shrinks_wrap_width() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                pad: [Len::Px(0.0), Len::Px(25.0), Len::Px(0.0), Len::Px(25.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("aaaa bbbb cccc")];
        let l = layout_tree(&mut n, 100.0, &m4);
        // content width = 50px → "aaaa"(16)+"bbbb"(16)+"cccc"(16) wraps to 2+ lines
        let ys: std::collections::HashSet<i32> = l.runs.iter().map(|r| r.y as i32).collect();
        assert!(ys.len() >= 2, "expected wrap in 50px content, got {} lines", ys.len());
        for r in &l.runs {
            assert!(r.x <= 75.0 + 0.001, "run escapes right padding: {:?}", r.text);
        }
    }

    #[test]
    fn border_insets_content_and_grows_box() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                border: [Len::Px(2.0), Len::Px(4.0), Len::Px(2.0), Len::Px(4.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("ab")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert!((l.runs[0].x - 4.0).abs() < 0.01, "x={}", l.runs[0].x);
        assert!((l.runs[0].y - 2.0).abs() < 0.01, "y={}", l.runs[0].y);
        assert!((n.h - (15.0 * 1.35 + 4.0)).abs() < 0.01, "h={}", n.h);
        assert!((n.w - 100.0).abs() < 0.01, "border-box width unchanged");
    }

    #[test]
    fn border_paints_strips_or_ring() {
        // square box with bg: background + 4 side strips
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                border: [Len::Px(1.0); 4],
                bg: Some(0x123456),
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("a")];
        let l = layout_tree(&mut n, 50.0, &m4);
        assert_eq!(l.rects.len(), 5, "bg + 4 strips: {:?}", l.rects);
        assert_eq!(l.rects[1].4, 0x000000, "unspecified border = currentcolor");
        // rounded box with bg: ring = border-box fill + background inset over it
        let mut m2 = BoxNode::group(
            Style::new(),
            BoxStyle {
                border: [Len::Px(2.0); 4],
                radius: Len::Px(6.0),
                bg: Some(0x123456),
                ..BoxStyle::default()
            },
        );
        m2.children = vec![leaf("a")];
        let l2 = layout_tree(&mut m2, 50.0, &m4);
        assert_eq!(l2.rects.len(), 2, "ring = outer + inner: {:?}", l2.rects);
        assert_eq!(l2.rects[0].4, 0x000000, "outer ring = currentcolor (text color)");
        assert!((l2.rects[1].2 - 46.0).abs() < 0.01, "inner w={}", l2.rects[1].2);
        assert!((l2.rects[1].5 - 4.0).abs() < 0.01, "inner radius={}", l2.rects[1].5);
    }

    #[test]
    fn shadow_emitted_offset_spread_alpha_fixed() {
        use crate::css::Shadow;
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                bg: Some(0xffffff),
                width: Some(Len::Px(100.0)),
                shadows: vec![Shadow {
                    dx: 0.0,
                    dy: 4.0,
                    blur: 10.0,
                    spread: 2.0,
                    color: Some(0x000000),
                    alpha: 128,
                }],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("a")];
        let l = layout_tree(&mut n, 200.0, &m4);
        assert_eq!(l.shadows.len(), 1, "one shadow rect");
        let s = &l.shadows[0];
        // spread grows the rect by 2px each side, offset down 4px
        assert!((s.x - -2.0).abs() < 0.01, "x={}", s.x);
        assert!((s.y - 2.0).abs() < 0.01, "y={}", s.y);
        assert!((s.w - 104.0).abs() < 0.01, "w={}", s.w);
        // plain blocks size to content (declared height only applies to
        // replaced elements) — compare against the emitted bg rect
        assert!((s.h - (l.rects[0].3 + 4.0)).abs() < 0.01, "h={}", s.h);
        assert_eq!(s.blur, 10.0);
        assert_eq!(s.color, (128u32 << 24) | 0x000000, "alpha packed in");
        assert!(!s.fixed, "in-flow box");
        // currentcolor: no shadow color → the element's text color
        let mut m2 = BoxNode::group(
            Style::new(),
            BoxStyle {
                shadows: vec![Shadow {
                    dx: 1.0,
                    dy: 1.0,
                    blur: 0.0,
                    spread: 0.0,
                    color: None,
                    alpha: 255,
                }],
                pos: Pos::Fixed,
                ..BoxStyle::default()
            },
        );
        m2.children = vec![leaf("a")];
        let l2 = layout_tree(&mut m2, 200.0, &m4);
        assert_eq!(l2.shadows[0].color, 0xff000000, "currentcolor (alpha-premultiplied)");
        assert!(l2.shadows[0].fixed, "fixed subtree");
    }

    #[test]
    fn margin_top_advances_sibling() {
        let mk = |mt: Len| {
            let mut b = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    margin: [mt, Len::Px(0.0), Len::Px(0.0), Len::Px(0.0)],
                    ..BoxStyle::default()
                },
            );
            b.children = vec![leaf("x")];
            b
        };
        let mut n = BoxNode::group(Style::new(), BoxStyle::default());
        n.children = vec![mk(Len::Px(0.0)), mk(Len::Px(15.0))];
        let l = layout_tree(&mut n, 100.0, &m4);
        let line = 15.0 * 1.35;
        let gap = 15.0 * 0.6; // block-boundary paragraph gap
        // second block: first line + gap + 15px margin-top
        assert!(
            (l.runs[1].y - (line + gap + 15.0)).abs() < 0.01,
            "y={}",
            l.runs[1].y
        );
    }

    #[test]
    fn line_height_overrides_default() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle::default(),
        );
        n.children = vec![BoxNode::leaf(
            Style {
                lh: 30.0,
                ..Style::new()
            },
            "a b".into(),
            false,
            None,
        )];
        let l = layout_tree(&mut n, 8.0, &m4); // 4px words, wraps to 2 lines
        let ys: Vec<f32> = {
            let mut v: Vec<f32> = l.runs.iter().map(|r| r.y).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v.dedup();
            v
        };
        assert_eq!(ys.len(), 2, "expected 2 lines, runs={}", l.runs.len());
        assert!((ys[1] - ys[0] - 30.0).abs() < 0.01, "line delta={}", ys[1] - ys[0]);
    }

    #[test]
    fn background_emits_rect_under_content() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                bg: Some(0x112233),
                radius: Len::Px(8.0),
                pad: [Len::Px(5.0), Len::Px(5.0), Len::Px(5.0), Len::Px(5.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("hi")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert_eq!(l.rects.len(), 1);
        let &(x, y, w, h, color, rad, fixed) = &l.rects[0];
        assert!(!fixed, "static box is not fixed");
        assert_eq!(color, 0x112233);
        assert!((x - 0.0).abs() < 0.01 && (y - 0.0).abs() < 0.01);
        assert!((w - 100.0).abs() < 0.01, "w={}", w);
        assert!((h - (15.0 * 1.35 + 10.0)).abs() < 0.01, "h={}", h);
        assert!((rad - 8.0).abs() < 0.01, "radius={}", rad);
    }

    // ---- replaced elements (<img>) ----

    /// group box carrying a decoded 100×50 image.
    fn img_box(bx: BoxStyle) -> BoxNode {
        let mut n = BoxNode::group(Style::new(), bx);
        n.img = Some(ImgSlot::Ready(std::rc::Rc::new(crate::image::ImageData {
            w: 100,
            h: 50,
            px: vec![0; 100 * 50],
        })));
        n
    }

    #[test]
    fn img_uses_intrinsic_size_clamped_to_avail() {
        let mut n = img_box(BoxStyle::default());
        let l = layout_tree(&mut n, 300.0, &m4);
        assert!((n.w - 100.0).abs() < 0.01, "w={}", n.w);
        assert!((n.h - 50.0).abs() < 0.01, "h={}", n.h);
        assert_eq!(l.images.len(), 1);
        assert!((l.images[0].3 - 50.0).abs() < 0.01);

        let mut narrow = img_box(BoxStyle::default());
        layout_tree(&mut narrow, 60.0, &m4);
        assert!((narrow.w - 60.0).abs() < 0.01, "clamped w={}", narrow.w);
        assert!((narrow.h - 30.0).abs() < 0.01, "aspect h={}", narrow.h);
    }

    #[test]
    fn img_width_declared_height_keeps_aspect() {
        let mut n = img_box(BoxStyle {
            width: Some(Len::Px(50.0)),
            ..BoxStyle::default()
        });
        layout_tree(&mut n, 300.0, &m4);
        assert!((n.w - 50.0).abs() < 0.01, "w={}", n.w);
        assert!((n.h - 25.0).abs() < 0.01, "h={}", n.h);
    }

    #[test]
    fn img_declared_height_wins_over_aspect() {
        let mut n = img_box(BoxStyle {
            height: Some(Len::Px(80.0)),
            ..BoxStyle::default()
        });
        layout_tree(&mut n, 300.0, &m4);
        assert!((n.h - 80.0).abs() < 0.01, "h={}", n.h);
    }

    #[test]
    fn inline_svg_becomes_img_box_with_raw_markup() {
        // <svg> is RAW_TEXT in the parser; build() must reassemble the full
        // markup into an ImgSlot so the image pipeline rasterizes it
        let dom = crate::html::parse(
            r#"<body><svg viewBox="0 0 10 20" width="10" height="20"><path d="M0 0h10"/></svg></body>"#,
        );
        let root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &crate::css::parse(""));
        fn find_svg(n: &BoxNode) -> Option<&ImgSlot> {
            if n.img.is_some() {
                return n.img.as_ref();
            }
            n.children.iter().find_map(find_svg)
        }
        let Some(ImgSlot::Pending(src)) = find_svg(&root) else {
            panic!("svg should produce a pending img slot");
        };
        assert!(src.starts_with("<svg"), "markup starts with root tag: {src:?}");
        // the HTML parser lowercases attr keys; svg.rs get_attr is
        // case-insensitive to match
        assert!(src.contains("viewbox=\"0 0 10 20\""), "attrs kept: {src:?}");
        assert!(src.contains("<path d=\"M0 0h10\"/>"), "inner markup kept: {src:?}");
        assert!(src.ends_with("</svg>"), "closed: {src:?}");
    }

    #[test]
    fn undecoded_img_with_declared_size_reserves_space() {
        // fetch/decode failed but width/height attributes were parsed → box still there
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                width: Some(Len::Px(40.0)),
                height: Some(Len::Px(20.0)),
                ..BoxStyle::default()
            },
        );
        n.img = Some(ImgSlot::Pending(std::rc::Rc::from("x.png")));
        let l = layout_tree(&mut n, 300.0, &m4);
        assert!((n.w - 40.0).abs() < 0.01);
        assert!((n.h - 20.0).abs() < 0.01, "declared h={}", n.h);
        assert!(l.images.is_empty(), "no pixels → nothing drawn");
    }

    #[test]
    fn img_advances_block_flow() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut first = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                ..BoxStyle::default()
            },
        );
        first.children = vec![leaf("ab")];
        let second = img_box(BoxStyle::default());
        root.children = vec![first, second];
        let l = layout_tree(&mut root, 300.0, &m4);
        let line = 15.0 * 1.35;
        let gap = 15.0 * 0.6;
        assert!((l.images[0].1 - (line + gap)).abs() < 0.01, "img y={}", l.images[0].1);
    }

    // ---- position (relative / absolute / fixed) ----

    /// group with `position` + insets, one text child.
    fn pos_box(pos: Pos, inset: [Option<Len>; 4], children: Vec<BoxNode>) -> BoxNode {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                pos,
                inset,
                ..BoxStyle::default()
            },
        );
        n.children = children;
        n
    }

    #[test]
    fn absolute_leaves_flow_and_honors_insets() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let flow = pos_box(Pos::Static, [None; 4], vec![leaf("flowtxt")]);
        // top: 10, left: 20 against the root's padding box (= viewport here)
        let abs = pos_box(Pos::Absolute, [Some(Len::Px(10.0)), None, None, Some(Len::Px(20.0))], vec![leaf("abs")]);
        root.children = vec![flow, abs];
        let l = layout_tree(&mut root, 300.0, &m4);
        // out-of-flow: contributes nothing to the parent's height
        // (flow line + trailing paragraph gap after the child's open line)
        let flow_h = 15.0 * 1.35 + 15.0 * 0.6;
        assert!((root.h - flow_h).abs() < 0.01, "root.h={} (flow only)", root.h);
        let abs_run = l.runs.iter().find(|r| r.text == "abs").expect("abs run");
        let flow_run = l.runs.iter().find(|r| r.text == "flowtxt").expect("flow run");
        assert!((abs_run.x - 20.0).abs() < 0.01, "x={}", abs_run.x);
        assert!((abs_run.y - 10.0).abs() < 0.01, "y={}", abs_run.y);
        assert!(!abs_run.fixed);
        // flow run untouched by the absolute sibling
        assert!((flow_run.y - flow_h).abs() < 0.01 || flow_run.y == 0.0, "y={}", flow_run.y);
    }

    #[test]
    fn absolute_shrink_to_fit_and_right_bottom_insets() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        // right: 10, bottom: 10, no width → shrink-to-fit (4 chars × 4px)
        let abs = pos_box(
            Pos::Absolute,
            [None, Some(Len::Px(10.0)), Some(Len::Px(10.0)), None],
            vec![leaf("abcd")],
        );
        root.children = vec![abs];
        // layout_in with a 200×100 viewport
        let l = layout_in(&mut root, 300.0, 100.0, &m4);
        let run = l.runs.iter().find(|r| r.text == "abcd").expect("run");
        let bw = 16.0; // 4 chars × 4px
        let bh = 15.0 * 1.35;
        assert!((run.x - (300.0 - 10.0 - bw)).abs() < 0.01, "x={}", run.x);
        assert!((run.y - (100.0 - 10.0 - bh)).abs() < 0.01, "y={}", run.y);
        let abs_n = &root.children[0];
        assert!((abs_n.w - bw).abs() < 0.01, "shrink w={}", abs_n.w);
    }

    #[test]
    fn relative_shifts_subtree_and_runs() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut inner = pos_box(Pos::Static, [None; 4], vec![leaf("deep")]);
        inner.children = vec![leaf("deep")];
        let rel = pos_box(Pos::Relative, [Some(Len::Px(5.0)), None, None, Some(Len::Px(7.0))], vec![leaf("moved")]);
        root.children = vec![rel, inner];
        let l = layout_tree(&mut root, 300.0, &m4);
        // relative stays in flow: the sibling still starts at flow position
        // (two lines + a trailing paragraph gap after each open child line)
        let line = 15.0 * 1.35;
        let h = line * 2.0 + 15.0 * 0.6 * 2.0;
        assert!((root.h - h).abs() < 0.01, "h={} = two lines + gaps", root.h);
        let moved = l.runs.iter().find(|r| r.text == "moved").expect("run");
        assert!((moved.x - 7.0).abs() < 0.01, "x={}", moved.x);
        assert!((moved.y - 5.0).abs() < 0.01, "y={}", moved.y);
    }

    #[test]
    fn fixed_marks_paint_and_uses_viewport() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        // bottom bar: bottom: 0 against the viewport (layout_in vh = 100)
        let bar = pos_box(Pos::Fixed, [None, None, Some(Len::Px(0.0)), None], vec![leaf("bar")]);
        root.children = vec![bar];
        let l = layout_in(&mut root, 300.0, 100.0, &m4);
        let run = l.runs.iter().find(|r| r.text == "bar").expect("run");
        assert!(run.fixed, "run inside fixed subtree is marked fixed");
        let bh = 15.0 * 1.35;
        assert!((run.y - (100.0 - bh)).abs() < 0.01, "y={} = vh - h", run.y);
    }

    #[test]
    fn fixed_stretches_to_viewport_and_rect_marked_fixed() {
        // fixed with left+right insets stretches to the viewport width,
        // not the parent's — parent is a narrow positioned box
        let mut root = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                pos: Pos::Absolute,
                width: Some(Len::Px(150.0)),
                inset: [Some(Len::Px(50.0)), None, None, Some(Len::Px(50.0))],
                ..BoxStyle::default()
            },
        );
        root.children = vec![pos_box(
            Pos::Fixed,
            [Some(Len::Px(0.0)), Some(Len::Px(0.0)), None, Some(Len::Px(0.0))],
            vec![],
        )];
        root.children[0].bx.bg = Some(0x334455);
        let l = layout_in(&mut root, 300.0, 100.0, &m4);
        assert_eq!(l.rects.len(), 1, "bg → one rect: {:?}", l.rects);
        assert!(l.rects[0].6, "fixed box's rect carries the fixed flag");
        let fixed_box = &root.children[0];
        // parent: width 150 shifted to x=50; fixed ignores it — viewport CB
        assert!((root.w - 150.0).abs() < 0.01, "root w={}", root.w);
        assert!((root.x - 50.0).abs() < 0.01, "root x={}", root.x);
        assert!((fixed_box.w - 300.0).abs() < 0.01, "fixed w={} = viewport", fixed_box.w);
        assert!((fixed_box.x - 0.0).abs() < 0.01, "fixed x={} = viewport", fixed_box.x);
    }
}
