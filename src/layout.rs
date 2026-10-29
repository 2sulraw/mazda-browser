//! DOM → styled atoms → line layout using real glyph metrics.
//! shortcut: only inline/block split + word wrap (no float/flex/absolute);
//! upgrade when a real CSS engine lands (Phase 1b+).

use crate::css::{Align, BoxStyle, Dir, Display, Justify, Len, Sheet, Track};
use crate::html::{Dom, NodeKind};
use crate::image::ImageData;

#[derive(Clone, Copy, PartialEq)]
pub struct Style {
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: u32,
    pub underline: bool,
    pub mono: bool,
    /// 0 = left, 1 = center, 2 = right (inherited, text-align)
    pub text_align: u8,
    /// line-height in px; 0 = default (size * 1.35) — inherited
    pub lh: f32,
}

impl Style {
    pub const fn new() -> Self {
        Style {
            size: 15.0,
            bold: false,
            italic: false,
            color: 0x00_e8_e8_e8,
            underline: false,
            mono: false,
            text_align: 0,
            lh: 0.0,
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
            s.mono = true;
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
        }
    }
}

/// Elements whose content is never rendered (also covers `<style>`/`<script>`
/// that appear inside body — head is already excluded by `body_root`).
const NON_RENDERED: &[&str] = &[
    "script", "style", "noscript", "template", "head", "title", "meta", "link", "svg",
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
                // author CSS (rules + inline style) cascades over UA defaults
                let outcome = sheet.resolve(dom, child, child_style, dom.attr(child, "style"));
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
                    // inline element: no box, children flatten into parent flow
                    build_into(dom, child, outcome.style, sheet, child_link, out);
                }
            }
        }
    }
}

pub struct PositionedRun {
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub style: Style,
    pub width: f32,
    pub link: Option<std::rc::Rc<str>>,
}

pub struct Layout {
    pub runs: Vec<PositionedRun>,
    pub height: f32,
    /// x, y, w, h, color (matches the run's text color)
    pub underline_rects: Vec<(f32, f32, f32, f32, u32)>,
    /// filled box backgrounds (x, y, w, h, color, radius) — drawn under text
    pub rects: Vec<(f32, f32, f32, f32, u32, f32)>,
    /// decoded `<img>` boxes (x, y, w, h, pixels) — drawn under text
    pub images: Vec<(f32, f32, f32, f32, std::rc::Rc<ImageData>)>,
    /// true if the last line still has content on it (no trailing <br>) —
    /// block flow uses this to decide whether to add the paragraph gap
    pub open_line: bool,
}

/// Lay out the box tree at (0, 0) within `max_width`. Groups get x/y/w/h;
/// text leaves are laid as line segments by their container (block) or as
/// individual items (flex/grid).
pub fn layout_tree(
    root: &mut BoxNode,
    max_width: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
) -> Layout {
    let mut runs = Vec::new();
    let mut uls = Vec::new();
    place(root, 0.0, 0.0, max_width, measure, &mut runs, &mut uls);
    let mut rects = Vec::new();
    let mut images = Vec::new();
    collect_paint(root, &mut rects, &mut images);
    Layout {
        runs,
        height: root.h,
        underline_rects: uls,
        rects,
        images,
        open_line: root.open,
    }
}

/// Post-layout walk: every box with a background-color emits a filled rect;
/// every decoded `<img>` emits its pixels (parent-first, so children paint
/// over their parent's fill).
fn collect_paint(
    n: &BoxNode,
    out: &mut Vec<(f32, f32, f32, f32, u32, f32)>,
    imgs: &mut Vec<(f32, f32, f32, f32, std::rc::Rc<ImageData>)>,
) {
    if !n.is_text {
        let bt = resolve_len(n.bx.border[0], n.w);
        let br = resolve_len(n.bx.border[1], n.w);
        let bb = resolve_len(n.bx.border[2], n.w);
        let bl = resolve_len(n.bx.border[3], n.w);
        let has_b = bt > 0.0 || br > 0.0 || bb > 0.0 || bl > 0.0;
        // currentcolor: unspecified border color = the element's text color
        let bc = n.bx.bcolor.unwrap_or(n.style.color);
        let r = resolve_len(n.bx.radius, n.w).min(n.w * 0.5).min(n.h * 0.5);
        let ring = has_b && r > 0.0 && n.bx.bg.is_some();
        if let Some(c) = n.bx.bg {
            if ring {
                // rounded ring: fill the border-box with the border color,
                // then the background inset over it (inner radius ≈ r − border)
                out.push((n.x, n.y, n.w, n.h, bc, r));
                let inner = (r - bt.max(br).max(bb).max(bl)).max(0.0);
                out.push((
                    n.x + bl,
                    n.y + bt,
                    (n.w - bl - br).max(0.0),
                    (n.h - bt - bb).max(0.0),
                    c,
                    inner,
                ));
            } else {
                out.push((n.x, n.y, n.w, n.h, c, r));
            }
        }
        if has_b && !ring {
            // four side strips over the background (square corners on a
            // rounded backgroundless box — shortcut)
            if bt > 0.0 {
                out.push((n.x, n.y, n.w, bt, bc, 0.0));
            }
            if bb > 0.0 {
                out.push((n.x, n.y + n.h - bb, n.w, bb, bc, 0.0));
            }
            if bl > 0.0 {
                out.push((n.x, n.y + bt, bl, (n.h - bt - bb).max(0.0), bc, 0.0));
            }
            if br > 0.0 {
                out.push((n.x + n.w - br, n.y + bt, br, (n.h - bt - bb).max(0.0), bc, 0.0));
            }
        }
        if let Some(ImgSlot::Ready(d)) = &n.img {
            if n.w > 0.0 && n.h > 0.0 {
                imgs.push((n.x, n.y, n.w, n.h, d.clone()));
            }
        }
        for ch in &n.children {
            collect_paint(ch, out, imgs);
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
    // restore border-box coords; height includes vertical padding + borders
    n.h += pt + pb + bt + bb;
    n.x = bx0;
    n.y = by0;
    n.w = w;
    mt + n.h + mb
}

type UR = (f32, f32, f32, f32, u32);

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
            out.push(Item::Text(start, i));
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
            sum + n.bx.gap_col * (k as f32 - 1.0).max(0.0)
        }
        (Display::Flex, Dir::Column) => n
            .children
            .iter()
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
    if natural_sum + gaps > n.w && natural_sum > 0.0 {
        let scale = ((n.w - gaps).max(0.0)) / natural_sum;
        for w in &mut widths {
            *w *= scale;
        }
    }
    let used: f32 = widths.iter().sum::<f32>() + gaps;
    let free = (n.w - used).max(0.0);
    let (x_off, extra) = match n.bx.justify {
        Justify::Center => (free * 0.5, 0.0),
        Justify::End => (free, 0.0),
        Justify::SpaceBetween if k > 1 => (0.0, free / (k as f32 - 1.0)),
        _ => (0.0, 0.0), // Start
    };
    let mut row_h = 0.0f32;
    let mut last_open = false;
    let mut x = n.x + x_off;
    let mut marks: Vec<(usize, usize, f32, bool)> = Vec::with_capacity(k); // run_start, ul_start, h, open
    for (idx, it) in items.iter().enumerate() {
        let w = widths[idx];
        let rs = runs.len();
        let us = uls.len();
        let (ih, io) = match it {
            Item::Text(a, b) => {
                let mut atoms = Vec::new();
                for c in &n.children[*a..*b] {
                    atoms.push(leaf_atom(c));
                }
                let l = layout(&atoms, w, measure);
                push_runs(l.runs, x, n.y, runs);
                push_uls(l.underline_rects, x, n.y, uls);
                (l.height, l.open_line)
            }
            Item::Grp(i) => {
                let adv = place(&mut n.children[*i], x, n.y, w, measure, runs, uls);
                (adv, n.children[*i].open) // cross-size = margin box
            }
        };
        marks.push((rs, us, ih, io));
        row_h = row_h.max(ih);
        last_open = io;
        x += w + gap + extra;
    }
    // cross-axis align: shift each item (runs slice + box coords) into row_h
    for (idx, &(rs, us, h, _)) in marks.iter().enumerate() {
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
        if let Item::Grp(i) = items[idx] {
            shift_tree(&mut n.children[i], 0.0, dy);
        }
    }
    n.h = row_h;
    n.open = last_open;
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

/// Grid: row-major cells over resolved tracks; row height = tallest cell,
/// row-gap between rows. shortcut: empty cols = single implicit column,
/// no spanning, no implicit rows beyond item count.
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
    let nrows = (items.len() + nc - 1) / nc;
    let mut y = n.y;
    let mut last_open = false;
    for r in 0..nrows {
        let mut row_h = 0.0f32;
        let mut marks: Vec<(usize, usize, f32)> = Vec::with_capacity(nc); // run_start, ul_start, h
        let mut cell_ios: Vec<bool> = Vec::with_capacity(nc);
        let mut x = n.x;
        for c in 0..nc {
            let idx = r * nc + c;
            if idx >= items.len() {
                break;
            }
            let rs = runs.len();
            let us = uls.len();
            let w = cols[c];
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
            x += w + n.bx.gap_col;
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
            if let Item::Grp(i) = items[r * nc + ci] {
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
        let mut first = true;
        for w in a.text.split(|c: char| c.is_whitespace()) {
            if w.is_empty() {
                continue;
            }
            if !first {
                words.push(Atom {
                    text: " ".into(),
                    style: a.style,
                    hard_break: false,
                    link: a.link.clone(),
                });
            }
            first = false;
            words.push(Atom {
                text: w.to_string(),
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
        if x > 0.0 && x + ww > max_width {
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
        });
        if w.style.underline {
            underline_rects.push((x, y + lh - 2.0, ww, 1.0, w.style.color));
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
        assert_eq!(l.rects[1].4, 0xe8e8e8, "unspecified border = currentcolor");
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
        assert_eq!(l2.rects[0].4, 0xe8e8e8, "outer ring = currentcolor (text color)");
        assert!((l2.rects[1].2 - 46.0).abs() < 0.01, "inner w={}", l2.rects[1].2);
        assert!((l2.rects[1].5 - 4.0).abs() < 0.01, "inner radius={}", l2.rects[1].5);
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
        let &(x, y, w, h, color, rad) = &l.rects[0];
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
}
