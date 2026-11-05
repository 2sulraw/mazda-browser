//! Minimal SVG rasterizer — enough for site logos and icons (Phase 1f).
//!
//! Supports: `path` (M/L/H/V/C/S/Q/T/A/Z, abs+rel), rect (incl. rx/ry),
//! circle, ellipse, polygon, polyline, line; `g`/svg `transform`
//! (matrix/translate/scale/rotate/skewX/skewY); fill (nonzero + evenodd),
//! stroke (width, round joins), fill-opacity/opacity; named/hex/rgb()/rgba()
//! colors; linearGradient/radialGradient collapsed to the flat midpoint of
//! their stop colors.
//! shortcut: no text, clip/mask/filter/pattern, no `use`/`image`, no CSS
//! <style> blocks, no dash/miter/linecap variants, gradients render flat.

use crate::image::ImageData;

/// 2x2 supersampling — cheap antialiasing for thin logo strokes.
const SS: usize = 2;

/// Intrinsic size from the root `<svg>` width/height (fallback: viewBox,
/// then the spec default 300x150). `None` = not an SVG.
pub fn probe(bytes: &[u8]) -> Option<(u32, u32)> {
    let txt = std::str::from_utf8(bytes).ok()?;
    let (_, raw) = root_tag(txt)?;
    let w = get_attr(raw, "width").and_then(parse_px);
    let h = get_attr(raw, "height").and_then(parse_px);
    if let (Some(w), Some(h)) = (w, h) {
        return Some((w.max(1.0) as u32, h.max(1.0) as u32));
    }
    if let Some(v) = get_attr(raw, "viewBox").and_then(nums4) {
        return Some((v[2].max(1.0) as u32, v[3].max(1.0) as u32));
    }
    Some((w.unwrap_or(300.0) as u32, h.unwrap_or(150.0) as u32)) // spec default
}

/// Rasterize at the requested size (vector → pixel-exact at any scale).
pub fn render(bytes: &[u8], w: u32, h: u32) -> Option<ImageData> {
    let txt = std::str::from_utf8(bytes).ok()?;
    let base = root_space(txt, w, h)?;
    let (root_attrs, cs, ce) = root_content(txt)?;
    let grads = harvest_gradients(txt);
    let defs = harvest_defs(txt);
    let ctx = Ctx {
        txt,
        defs: &defs,
        grads: &grads,
        w: w as usize,
        h: h as usize,
        w2: w as usize * SS,
        h2: h as usize * SS,
    };
    let vb = root_vb(root_attrs, w, h);
    let m = match get_attr(root_attrs, "transform") {
        Some(t) => base * parse_transform(t),
        None => base,
    };
    let state = State {
        m,
        st: Style::default().with(root_attrs),
        clips: Vec::new(),
        masks: Vec::new(),
        filters: Vec::new(),
        vp: (vb[2], vb[3]),
    };
    let mut img = vec![0u32; ctx.w * ctx.h];
    paint_range(&ctx, cs, ce, &state, &mut img, 0);
    Some(ImageData { w, h, px: img })
}

const SKIP: &[&str] = &[
    "defs", "clipPath", "mask", "filter", "pattern", "marker", "symbol", "text", "tspan",
    "style", "title", "desc", "metadata", "linearGradient", "radialGradient", "script",
    "image", "foreignObject", "use",
];

// ---------- text ----------

/// One run of SVG text, resolved to FINAL-IMAGE pixels (viewBox scale and
/// supersampling already divided out) and handed to the shell, which owns the
/// font metrics. The vector parser does not measure glyphs, so `<text>` is
/// not painted here: `main.rs` positions and blits the glyphs.
pub struct TextRun {
    /// whitespace-collapsed, entity-decoded content (tspans flattened)
    pub text: String,
    /// baseline origin in image pixels (x, y)
    pub x: f64,
    pub y: f64,
    /// font size in image pixels
    pub size: f64,
    /// 0x00RRGGBB
    pub color: u32,
    /// fill-opacity × opacity, 0..1
    pub alpha: f64,
    /// text-anchor: 0 start, 1 middle, 2 end
    pub anchor: u8,
    /// `<textPath href="#id">` polyline in image pixels (None = plain text)
    pub path: Option<Vec<(f64, f64)>>,
    /// `textLength` in image pixels: glyph advances are stretched to fill it
    pub text_len: Option<f64>,
    /// `dy` in image pixels — a perpendicular offset when on a path
    pub dy: f64,
}

/// `<path id=…>` polylines, for `<textPath href="#id">`.
fn harvest_paths(txt: &str) -> Vec<(String, Vec<(f64, f64)>)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(tag) = next_tag(txt, &mut i) {
        if tag.closing || tag.name != "path" {
            continue;
        }
        let (Some(id), Some(d)) = (get_attr(tag.attrs, "id"), get_attr(tag.attrs, "d")) else {
            continue;
        };
        let subs = parse_path(d);
        if let Some(sub) = subs.into_iter().max_by_key(|s| s.pts.len()) {
            if sub.pts.len() >= 2 {
                out.push((id.to_string(), sub.pts));
            }
        }
    }
    out
}

/// Text between tags: drop nested markup (tspan/textPath) and decode the
/// five XML entities plus numeric references.
fn text_of(inner: &str) -> String {
    let mut raw = String::new();
    let mut rest = inner;
    while let Some(lt) = rest.find('<') {
        raw.push_str(&rest[..lt]);
        match rest[lt..].find('>') {
            Some(gt) => rest = &rest[lt + gt + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    raw.push_str(rest);
    // entities
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let rest = &raw[i..];
        let Some(semi) = rest.find(';').filter(|&e| e <= 12) else {
            out.push('&');
            continue;
        };
        let ent = &rest[1..semi];
        let rep = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => {
                let num = ent.strip_prefix('#').map(|n| {
                    let (digits, radix) = match n.strip_prefix(['x', 'X']) {
                        Some(h) => (h, 16),
                        None => (n, 10),
                    };
                    u32::from_str_radix(digits, radix).ok().and_then(char::from_u32)
                });
                num.flatten()
            }
        };
        match rep {
            Some(r) => {
                out.push(r);
                // skip the reference's chars (they are ASCII, 1 byte each)
                for _ in 0..semi {
                    it.next();
                }
            }
            None => out.push('&'),
        }
    }
    out
}

/// `xml:space` default: runs of ASCII whitespace collapse to one space and
/// the ends are trimmed; NBSP is a real glyph and must survive.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() && c != '\u{a0}' {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

struct OpenText {
    /// depth AFTER the `<text>` open tag — the matching close tag reports it
    depth: usize,
    m: M,
    style: Style,
    /// byte offset just past the `<text …>` tag
    start: usize,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    text_len: Option<f64>,
    path: Option<Vec<(f64, f64)>>,
}

/// All `<text>` runs in the document, in paint order, in image pixels.
pub fn text_runs(bytes: &[u8], w: u32, h: u32) -> Vec<TextRun> {
    let mut out = Vec::new();
    let Ok(txt) = std::str::from_utf8(bytes) else { return out };
    let Some(base) = root_space(txt, w, h) else { return out };
    let grads = harvest_gradients(txt);
    let paths = harvest_paths(txt);
    let k = SS as f64;

    let mut mats = vec![base];
    let mut styles = vec![Style::default()];
    let mut depth = 0usize;
    let mut skip_at: Option<usize> = None;
    let mut open: Option<OpenText> = None;
    let mut i = 0usize;

    while let Some(tag) = next_tag(txt, &mut i) {
        if tag.closing {
            if let Some(o) = open.take_if(|o| o.depth == depth) {
                let text = collapse_ws(&text_of(&txt[o.start..tag.start]));
                if !text.is_empty() {
                    let sc = (o.m.a.hypot(o.m.b) + o.m.c.hypot(o.m.d)) / 2.0; // super px per user unit
                    let px = |v: f64| v * sc / k;
                    let on_path = o.path.is_some();
                    let (ox, oy) = if on_path {
                        o.m.apply(o.x, o.y)
                    } else {
                        o.m.apply(o.x + o.dx, o.y + o.dy)
                    };
                    let path = o.path.as_ref().map(|pts| {
                        pts.iter()
                            .map(|&(px_, py_)| {
                                let q = o.m.apply(px_, py_);
                                (q.0 / k, q.1 / k)
                            })
                            .collect()
                    });
                    if let Some(col) = o.style.fill.resolve(&grads) {
                        out.push(TextRun {
                            text,
                            x: ox / k,
                            y: oy / k,
                            size: px(o.style.fs),
                            color: col & 0x00ff_ffff,
                            alpha: o.style.fill_opacity.clamp(0.0, 1.0),
                            anchor: o.style.anchor,
                            path,
                            text_len: o.text_len.map(px),
                            dy: if on_path { px(o.dy) } else { 0.0 },
                        });
                    }
                }
                open = None;
            }
            if skip_at == Some(depth) {
                skip_at = None;
            }
            depth = depth.saturating_sub(1);
            mats.pop();
            styles.pop();
            continue;
        }

        let m = mats.last().copied().unwrap_or(base);
        let m = match get_attr(tag.attrs, "transform") {
            Some(t) => m * parse_transform(t),
            None => m,
        };
        let st = styles.last().cloned().unwrap_or_default().with(tag.attrs);
        if !tag.selfclose {
            depth += 1;
            mats.push(m);
            styles.push(st.clone());
        }
        if skip_at.is_some() {
            continue;
        }
        if let Some(o) = &mut open {
            if tag.name == "textPath" {
                // the textPath's own presentation attributes style the glyphs
                // it carries (MDN sizes each ring separately)
                for key in ["font-size", "fill"] {
                    if let Some(v) = get_style(tag.attrs, key).or_else(|| get_attr(tag.attrs, key)) {
                        o.style.apply(key, v);
                    }
                }
            }
            // a textPath inside the run: its href only says WHERE the text
            // goes, the glyphs still come from the enclosing <text>
            if tag.name == "textPath" && o.path.is_none() {
                if let Some(href) = get_attr(tag.attrs, "href")
                    .or_else(|| get_attr(tag.attrs, "xlink:href"))
                {
                    o.path = paths
                        .iter()
                        .find(|(id, _)| id == href.trim_start_matches('#'))
                        .map(|(_, pts)| pts.clone());
                }
                if o.text_len.is_none() {
                    o.text_len = get_attr(tag.attrs, "textLength").and_then(parse_px);
                }
            }
            continue;
        }
        if tag.name == "text" {
            if !tag.selfclose {
                open = Some(OpenText {
                    depth,
                    m,
                    style: st.effective(),
                    start: i,
                    x: attrf(tag.attrs, "x"),
                    y: attrf(tag.attrs, "y"),
                    dx: attrf(tag.attrs, "dx"),
                    dy: attrf(tag.attrs, "dy"),
                    text_len: get_attr(tag.attrs, "textLength").and_then(parse_px),
                    path: None,
                });
            }
            continue;
        }
        if SKIP.contains(&tag.name) && !tag.selfclose {
            skip_at = Some(depth);
        }
    }
    out
}

// ---------- element tree walk ----------
//
// Definitions (`clipPath`, `mask`, `filter`, `pattern`, `symbol`, any
// `id`-carrying element) are harvested once into byte ranges, then painted on
// demand. The paint walk recurses per container instead of keeping parallel
// matrix/style stacks: `use` and clip/mask content need to re-enter arbitrary
// subtrees, which a streaming walk cannot express.

struct Def<'a> {
    tag: &'a str,
    attrs: &'a str,
    content_start: usize,
    content_end: usize,
}

struct Defs<'a> {
    items: Vec<(String, Def<'a>)>,
}

impl<'a> Defs<'a> {
    fn get(&self, id: &str) -> Option<&Def<'a>> {
        self.items.iter().find(|(k, _)| k == id).map(|(_, d)| d)
    }
}

fn harvest_defs<'a>(txt: &'a str) -> Defs<'a> {
    let mut items = Vec::new();
    let mut i = 0usize;
    while let Some(t) = next_tag(txt, &mut i) {
        if t.closing {
            continue;
        }
        let Some(id) = get_attr(t.attrs, "id") else { continue };
        let (cs, ce) = if t.selfclose {
            (i, i)
        } else {
            let (ce, _) = matching_close(txt, i, t.name);
            (i, ce)
        };
        items.push((
            id.to_string(),
            Def { tag: t.name, attrs: t.attrs, content_start: cs, content_end: ce },
        ));
    }
    Defs { items }
}

/// Content end + byte offset just past the matching close tag. `from` is the
/// first byte after the open tag. A missing close runs to EOF.
fn matching_close(txt: &str, from: usize, name: &str) -> (usize, usize) {
    let mut i = from;
    let mut depth = 0usize;
    while let Some(t) = next_tag(txt, &mut i) {
        if t.name != name {
            continue;
        }
        if t.closing {
            if depth == 0 {
                return (t.start, i);
            }
            depth -= 1;
        } else if !t.selfclose {
            depth += 1;
        }
    }
    (txt.len(), txt.len())
}

fn root_content(txt: &str) -> Option<(&str, usize, usize)> {
    let mut i = 0usize;
    let t = next_tag(txt, &mut i)?;
    if t.closing || t.name != "svg" {
        return None;
    }
    if t.selfclose {
        return Some((t.attrs, i, i));
    }
    let (ce, _) = matching_close(txt, i, "svg");
    Some((t.attrs, i, ce))
}

/// Root user-space viewport: the `viewBox` when present, else the declared
/// intrinsic `width`/`height` (an SVG without a viewBox still has a fixed
/// 1:1 user coordinate system — its intrinsic size), else the requested
/// raster size. Using the requested size as the fallback would map only the
/// top-left `w×h` user units and clip the rest when rendering below the
/// intrinsic size.
fn root_vb(root_attrs: &str, w: u32, h: u32) -> [f64; 4] {
    if let Some(v) = get_attr(root_attrs, "viewBox").and_then(nums4) {
        return v;
    }
    let iw = get_attr(root_attrs, "width")
        .and_then(parse_px)
        .unwrap_or(w as f64);
    let ih = get_attr(root_attrs, "height")
        .and_then(parse_px)
        .unwrap_or(h as f64);
    [0.0, 0.0, iw.max(1e-6), ih.max(1e-6)]
}

/// Root user space → supersampled image pixels (`viewBox` fit, xMidYMid meet).
fn root_space(txt: &str, w: u32, h: u32) -> Option<M> {
    let (_, root_attrs) = root_tag(txt)?;
    let vb = root_vb(root_attrs, w, h);
    let (vw, vh) = (vb[2].max(1e-6), vb[3].max(1e-6));
    let scale = (w as f64 / vw).min(h as f64 / vh);
    let k = SS as f64;
    Some(M {
        a: scale * k,
        b: 0.0,
        c: 0.0,
        d: scale * k,
        e: ((w as f64 - vw * scale) / 2.0 - vb[0] * scale) * k,
        f: ((h as f64 - vh * scale) / 2.0 - vb[1] * scale) * k,
    })
}

fn is_shape(name: &str) -> bool {
    matches!(
        name,
        "path" | "rect" | "circle" | "ellipse" | "polygon" | "polyline" | "line"
    )
}

#[derive(Clone)]
enum ClipRef {
    /// `clipPath` id, matrix of the user space it was declared in
    Path(String, M),
    /// viewport rectangle (nested `<svg>`, `<use><symbol>`), matrix + x/y/w/h
    Rect(M, f64, f64, f64, f64),
}

#[derive(Clone)]
struct State {
    m: M,
    st: Style,
    clips: Vec<ClipRef>,
    masks: Vec<(String, M)>,
    filters: Vec<(String, M)>,
    /// current viewport in user units (percentage width/height resolve here)
    vp: (f64, f64),
}

struct Ctx<'a> {
    txt: &'a str,
    defs: &'a Defs<'a>,
    grads: &'a [(String, u32)],
    w: usize,
    h: usize,
    w2: usize,
    h2: usize,
}

fn paint_range(ctx: &Ctx, start: usize, end: usize, state: &State, img: &mut [u32], depth: usize) {
    if depth > 24 {
        return;
    }
    let mut i = start;
    while let Some(tag) = next_tag(ctx.txt, &mut i) {
        if tag.start >= end {
            break;
        }
        if tag.closing {
            continue;
        }
        if is_shape(tag.name) || tag.selfclose {
            paint_element(ctx, &tag, None, state, img, depth);
        } else {
            let (ce, after) = matching_close(ctx.txt, i, tag.name);
            paint_element(ctx, &tag, Some((i, ce)), state, img, depth);
            i = after;
        }
    }
}

fn paint_element(
    ctx: &Ctx,
    tag: &Tag,
    content: Option<(usize, usize)>,
    state: &State,
    img: &mut [u32],
    depth: usize,
) {
    let m = match get_attr(tag.attrs, "transform") {
        Some(t) => state.m * parse_transform(t),
        None => state.m,
    };
    let mut child = state.clone();
    child.m = m;
    child.st = state.st.with(tag.attrs);
    if let Some(id) = ref_id(tag.attrs, "clip-path") {
        child.clips.push(ClipRef::Path(id, m));
    }
    if let Some(id) = ref_id(tag.attrs, "mask") {
        child.masks.push((id, m));
    }
    if let Some(id) = ref_id(tag.attrs, "filter") {
        child.filters.push((id, m));
    }

    match tag.name {
        "svg" => {
            // nested viewport: x/y/width/height + optional viewBox, clipped
            // to the viewport rectangle in the parent's user space
            if let Some((cs, ce)) = content {
                let (nm, vw, vh) = nested_viewport(m, tag.attrs, state.vp);
                let x = attrf(tag.attrs, "x");
                let y = attrf(tag.attrs, "y");
                let w = len_attr(tag.attrs, "width", state.vp.0).unwrap_or(state.vp.0);
                let h = len_attr(tag.attrs, "height", state.vp.1).unwrap_or(state.vp.1);
                let mut s = child;
                s.m = nm;
                s.vp = (vw, vh);
                s.clips.push(ClipRef::Rect(m, x, y, w, h));
                paint_range(ctx, cs, ce, &s, img, depth + 1);
            }
        }
        "use" => paint_use(ctx, tag, &child, img, depth),
        n if is_shape(n) => paint_shape(ctx, n, tag.attrs, &child, img, depth),
        n if SKIP.contains(&n) => {}
        _ => {
            if let Some((cs, ce)) = content {
                paint_range(ctx, cs, ce, &child, img, depth + 1);
            }
        }
    }
}

fn len_attr(attrs: &str, key: &str, base: f64) -> Option<f64> {
    let v = get_attr(attrs, key)?.trim();
    if let Some(p) = v.strip_suffix('%') {
        return p.parse::<f64>().ok().map(|p| p / 100.0 * base);
    }
    parse_px(v)
}

fn translate(x: f64, y: f64) -> M {
    M { e: x, f: y, ..M::id() }
}

/// Nested `<svg>` viewport → (content matrix, viewBox width, viewBox height).
fn nested_viewport(m: M, attrs: &str, parent_vp: (f64, f64)) -> (M, f64, f64) {
    let x = attrf(attrs, "x");
    let y = attrf(attrs, "y");
    let w = len_attr(attrs, "width", parent_vp.0).unwrap_or(parent_vp.0).max(1e-6);
    let h = len_attr(attrs, "height", parent_vp.1).unwrap_or(parent_vp.1).max(1e-6);
    match get_attr(attrs, "viewBox").and_then(nums4) {
        Some(vb) => {
            let s = (w / vb[2]).min(h / vb[3]);
            let tx = x + (w - vb[2] * s) / 2.0 - vb[0] * s;
            let ty = y + (h - vb[3] * s) / 2.0 - vb[1] * s;
            (m * M { a: s, b: 0.0, c: 0.0, d: s, e: tx, f: ty }, vb[2], vb[3])
        }
        None => (m * translate(x, y), w, h),
    }
}

fn paint_use(ctx: &Ctx, tag: &Tag, state: &State, img: &mut [u32], depth: usize) {
    if depth > 16 {
        return;
    }
    let Some(href) = get_attr(tag.attrs, "href").or_else(|| get_attr(tag.attrs, "xlink:href")) else {
        return;
    };
    let Some(def) = ctx.defs.get(href.trim().trim_start_matches('#')) else { return };
    let x = attrf(tag.attrs, "x");
    let y = attrf(tag.attrs, "y");
    let m = state.m * translate(x, y);

    if def.tag == "symbol" || def.tag == "svg" {
        // symbol/svg is a viewport: use width/height (+ symbol viewBox)
        let w = len_attr(tag.attrs, "width", state.vp.0)
            .or_else(|| len_attr(def.attrs, "width", state.vp.0))
            .unwrap_or(state.vp.0)
            .max(1e-6);
        let h = len_attr(tag.attrs, "height", state.vp.1)
            .or_else(|| len_attr(def.attrs, "height", state.vp.1))
            .unwrap_or(state.vp.1)
            .max(1e-6);
        let mut s = state.clone();
        s.st = state.st.with(def.attrs);
        match get_attr(def.attrs, "viewBox").and_then(nums4) {
            Some(vb) => {
                let sc = (w / vb[2]).min(h / vb[3]);
                let tx = (w - vb[2] * sc) / 2.0 - vb[0] * sc;
                let ty = (h - vb[3] * sc) / 2.0 - vb[1] * sc;
                s.m = m * M { a: sc, b: 0.0, c: 0.0, d: sc, e: tx, f: ty };
                s.vp = (vb[2], vb[3]);
            }
            None => {
                s.m = m;
                s.vp = (w, h);
            }
        }
        s.clips.push(ClipRef::Rect(state.m, x, y, w, h));
        paint_range(ctx, def.content_start, def.content_end, &s, img, depth + 1);
        return;
    }

    if is_shape(def.tag) {
        let mut s = state.clone();
        s.m = m * match get_attr(def.attrs, "transform") {
            Some(t) => parse_transform(t),
            None => M::id(),
        };
        s.st = state.st.with(def.attrs);
        paint_shape(ctx, def.tag, def.attrs, &s, img, depth + 1);
        return;
    }

    // group / unknown container: its content re-enters under the use's style
    let mut s = state.clone();
    s.m = m * match get_attr(def.attrs, "transform") {
        Some(t) => parse_transform(t),
        None => M::id(),
    };
    s.st = state.st.with(def.attrs);
    paint_range(ctx, def.content_start, def.content_end, &s, img, depth + 1);
}


// ---------- shapes and paint effects ----------

enum FillSrc {
    Color(u32),
    Pattern(String),
}

fn resolve_fill(p: &Paint, ctx: &Ctx) -> Option<FillSrc> {
    match p {
        Paint::None => None,
        Paint::Color(c) => Some(FillSrc::Color(*c)),
        Paint::Url(id) => {
            // unknown gradient id → per spec the element is not rendered
            if let Some((_, c)) = ctx.grads.iter().find(|(k, _)| k == id) {
                return Some(FillSrc::Color(*c));
            }
            match ctx.defs.get(id) {
                Some(d) if d.tag == "pattern" => Some(FillSrc::Pattern(id.clone())),
                _ => None,
            }
        }
    }
}

/// `url(#id)` in a presentation attribute or inline style; `none` → None.
fn ref_id(raw: &str, key: &str) -> Option<String> {
    let v = get_style(raw, key).or_else(|| get_attr(raw, key))?.trim();
    if v.eq_ignore_ascii_case("none") || v.is_empty() {
        return None;
    }
    let inner = v
        .strip_prefix("url(")
        .and_then(|r| r.split(')').next())
        .unwrap_or(v)
        .trim()
        .trim_matches(['"', '\'']);
    let id = inner.trim().trim_start_matches('#');
    (!id.is_empty()).then(|| id.to_string())
}

fn shape_geometry(name: &str, attrs: &str) -> (Vec<Vec<(f64, f64)>>, Option<Vec<Sub>>) {
    let mut fill_polys: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut stroke_subs: Option<Vec<Sub>> = None;
    match name {
        "path" => {
            let Some(d) = get_attr(attrs, "d") else {
                return (fill_polys, stroke_subs);
            };
            let subs = parse_path(d);
            stroke_subs = Some(subs.clone());
            fill_polys = subs.iter().filter(|s| s.pts.len() >= 3).map(|s| s.pts.clone()).collect();
        }
        "rect" => {
            let x = attrf(attrs, "x");
            let y = attrf(attrs, "y");
            let rw = attrf(attrs, "width");
            let rh = attrf(attrs, "height");
            if rw <= 0.0 || rh <= 0.0 {
                return (fill_polys, stroke_subs);
            }
            let rx = get_attr(attrs, "rx").and_then(parse_px).unwrap_or(0.0).min(rw / 2.0);
            let ry = get_attr(attrs, "ry").and_then(parse_px).unwrap_or(rx).min(rh / 2.0);
            fill_polys = vec![rect_poly(x, y, rw, rh, rx, ry)];
            stroke_subs = Some(vec![Sub { pts: fill_polys[0].clone(), closed: true }]);
        }
        "circle" | "ellipse" => {
            let cx = attrf(attrs, "cx");
            let cy = attrf(attrs, "cy");
            let rx = if name == "circle" { attrf(attrs, "r") } else { attrf(attrs, "rx") };
            let ry = if name == "circle" { attrf(attrs, "r") } else { attrf(attrs, "ry") };
            if rx <= 0.0 || ry <= 0.0 {
                return (fill_polys, stroke_subs);
            }
            fill_polys = vec![ellipse_poly(cx, cy, rx, ry)];
            stroke_subs = Some(vec![Sub { pts: fill_polys[0].clone(), closed: true }]);
        }
        "polygon" | "polyline" => {
            let nums = get_attr(attrs, "points").map(nums).unwrap_or_default();
            let pts: Vec<(f64, f64)> = nums.chunks_exact(2).map(|c| (c[0], c[1])).collect();
            if pts.len() < 2 {
                return (fill_polys, stroke_subs);
            }
            if name == "polygon" && pts.len() >= 3 {
                fill_polys = vec![pts.clone()];
            }
            stroke_subs = Some(vec![Sub { pts, closed: name == "polygon" }]);
        }
        "line" => {
            let p = vec![
                (attrf(attrs, "x1"), attrf(attrs, "y1")),
                (attrf(attrs, "x2"), attrf(attrs, "y2")),
            ];
            stroke_subs = Some(vec![Sub { pts: p, closed: false }]);
        }
        _ => {}
    }
    (fill_polys, stroke_subs)
}

fn polys_bbox(polys: &[Vec<(f64, f64)>]) -> (f64, f64, f64, f64) {
    let mut b = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in polys {
        for &(x, y) in p {
            b.0 = b.0.min(x);
            b.1 = b.1.min(y);
            b.2 = b.2.max(x);
            b.3 = b.3.max(y);
        }
    }
    if b.0 > b.2 {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        (b.0, b.1, b.2 - b.0, b.3 - b.1)
    }
}

fn paint_shape(
    ctx: &Ctx,
    name: &str,
    attrs: &str,
    state: &State,
    img: &mut [u32],
    _depth: usize,
) {
    let (fill_polys, stroke_subs) = shape_geometry(name, attrs);
    if fill_polys.is_empty() && stroke_subs.is_none() {
        return;
    }
    let st = state.st.effective();
    let m = state.m;
    let bbox = polys_bbox(&fill_polys);
    let cov = effect_coverage(ctx, &state.clips, &state.masks, bbox);

    if !state.filters.is_empty() {
        // draw the element into an isolated layer, run the filter chain, then
        // composite (clip/mask apply to the filter result, per spec)
        let mut layer = vec![0u32; ctx.w * ctx.h];
        paint_geometry(ctx, &mut layer, &fill_polys, &stroke_subs, &st, m, bbox, None);
        let out = filter_chain(ctx, &state.filters, layer, m);
        composite_layer(ctx, img, &out, cov.as_deref());
    } else {
        paint_geometry(ctx, img, &fill_polys, &stroke_subs, &st, m, bbox, cov.as_deref());
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_geometry(
    ctx: &Ctx,
    dst: &mut [u32],
    fill_polys: &[Vec<(f64, f64)>],
    stroke_subs: &Option<Vec<Sub>>,
    st: &Style,
    m: M,
    bbox: (f64, f64, f64, f64),
    cov: Option<&[u8]>,
) {
    let mut mask = vec![0u8; ctx.w2 * ctx.h2];

    if let Some(fill) = resolve_fill(&st.fill, ctx) {
        let polys: Vec<Vec<(f64, f64)>> = fill_polys
            .iter()
            .map(|p| p.iter().map(|&(x, y)| m.apply(x, y)).collect())
            .collect();
        if !polys.is_empty() {
            mask.fill(0);
            fill_scan(&mut mask, ctx.w2, ctx.h2, &polys, st.evenodd);
            if let Some(c) = cov {
                mul_mask(&mut mask, c);
            }
            match fill {
                FillSrc::Color(col) => {
                    let col = scale_alpha(col, (st.fill_opacity * 255.0).round() as u32);
                    comp(dst, &mask, ctx.w as u32, ctx.h as u32, col);
                }
                FillSrc::Pattern(id) => {
                    blend_pattern(ctx, dst, &mask, &id, m, bbox, st.fill_opacity);
                }
            }
        }
    }

    if let (Some(col), Some(subs)) = (st.stroke.resolve(ctx.grads), stroke_subs) {
        // matrix already maps to supersampled space → hypot gives super px
        let sw = st.sw * (m.a.hypot(m.b) + m.c.hypot(m.d)) / 2.0;
        if sw > 0.0 {
            let polys = stroke_polys(subs, m, sw);
            if !polys.is_empty() {
                mask.fill(0);
                fill_scan(&mut mask, ctx.w2, ctx.h2, &polys, false);
                if let Some(c) = cov {
                    mul_mask(&mut mask, c);
                }
                let col = scale_alpha(col, (st.stroke_opacity * 255.0).round() as u32);
                comp(dst, &mask, ctx.w as u32, ctx.h as u32, col);
            }
        }
    }
}

fn mul_mask(mask: &mut [u8], cov: &[u8]) {
    for (m, c) in mask.iter_mut().zip(cov) {
        *m = (*m as u32 * *c as u32 / 255) as u8;
    }
}

fn and_into(acc: &mut Option<Vec<u8>>, buf: Vec<u8>) {
    match acc {
        Some(a) => {
            for (x, y) in a.iter_mut().zip(&buf) {
                *x = (*x as u32 * *y as u32 / 255) as u8;
            }
        }
        None => *acc = Some(buf),
    }
}

fn bbox_matrix(b: (f64, f64, f64, f64)) -> M {
    if b.2 <= 0.0 || b.3 <= 0.0 {
        M::id()
    } else {
        M { a: b.2, b: 0.0, c: 0.0, d: b.3, e: b.0, f: b.1 }
    }
}

fn effect_coverage(
    ctx: &Ctx,
    clips: &[ClipRef],
    masks: &[(String, M)],
    bbox: (f64, f64, f64, f64),
) -> Option<Vec<u8>> {
    if clips.is_empty() && masks.is_empty() {
        return None;
    }
    let mut out: Option<Vec<u8>> = None;
    for c in clips {
        let buf = match c {
            ClipRef::Path(id, m) => clip_coverage(ctx, id, *m, bbox),
            ClipRef::Rect(m, x, y, w, h) => rect_coverage(ctx, *m, *x, *y, *w, *h),
        };
        // an invalid reference is treated as `none` (no clipping)
        if let Some(b) = buf {
            and_into(&mut out, b);
        }
    }
    for (id, m) in masks {
        if let Some(b) = mask_coverage(ctx, id, *m, bbox) {
            and_into(&mut out, b);
        }
    }
    out
}

fn rect_coverage(ctx: &Ctx, m: M, x: f64, y: f64, w: f64, h: f64) -> Option<Vec<u8>> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let poly = rect_poly(x, y, w, h, 0.0, 0.0);
    let pts: Vec<(f64, f64)> = poly.iter().map(|&(px, py)| m.apply(px, py)).collect();
    let mut buf = vec![0u8; ctx.w2 * ctx.h2];
    fill_scan(&mut buf, ctx.w2, ctx.h2, &[pts], false);
    Some(buf)
}

fn clip_coverage(
    ctx: &Ctx,
    id: &str,
    m: M,
    bbox: (f64, f64, f64, f64),
) -> Option<Vec<u8>> {
    let def = ctx.defs.get(id)?;
    if def.tag != "clipPath" {
        return None;
    }
    let mut cm = m;
    if let Some(t) = get_attr(def.attrs, "transform") {
        cm = cm * parse_transform(t);
    }
    if get_attr(def.attrs, "clipPathUnits")
        .map(|u| u.eq_ignore_ascii_case("objectBoundingBox"))
        .unwrap_or(false)
    {
        cm = cm * bbox_matrix(bbox);
    }
    let mut polys = Vec::new();
    collect_clip_polys(ctx, def.content_start, def.content_end, cm, &mut polys, 0);
    if polys.is_empty() {
        return None;
    }
    let mut buf = vec![0u8; ctx.w2 * ctx.h2];
    fill_scan(&mut buf, ctx.w2, ctx.h2, &polys, false);
    Some(buf)
}

fn collect_clip_polys(
    ctx: &Ctx,
    start: usize,
    end: usize,
    m: M,
    out: &mut Vec<Vec<(f64, f64)>>,
    depth: usize,
) {
    if depth > 12 {
        return;
    }
    let mut i = start;
    while let Some(t) = next_tag(ctx.txt, &mut i) {
        if t.start >= end {
            break;
        }
        if t.closing {
            continue;
        }
        let tm = match get_attr(t.attrs, "transform") {
            Some(v) => m * parse_transform(v),
            None => m,
        };
        if is_shape(t.name) {
            let (polys, _) = shape_geometry(t.name, t.attrs);
            for p in polys {
                out.push(p.iter().map(|&(x, y)| tm.apply(x, y)).collect());
            }
        } else if t.name == "use" {
            if let Some(href) = get_attr(t.attrs, "href").or_else(|| get_attr(t.attrs, "xlink:href")) {
                if let Some(d) = ctx.defs.get(href.trim().trim_start_matches('#')) {
                    if is_shape(d.tag) {
                        let dm = tm
                            * translate(attrf(t.attrs, "x"), attrf(t.attrs, "y"))
                            * match get_attr(d.attrs, "transform") {
                                Some(v) => parse_transform(v),
                                None => M::id(),
                            };
                        let (polys, _) = shape_geometry(d.tag, d.attrs);
                        for p in polys {
                            out.push(p.iter().map(|&(x, y)| dm.apply(x, y)).collect());
                        }
                    }
                }
            }
        } else if !t.selfclose && !SKIP.contains(&t.name) {
            let (ce, after) = matching_close(ctx.txt, i, t.name);
            collect_clip_polys(ctx, i, ce, tm, out, depth + 1);
            i = after;
        }
    }
}

fn mask_coverage(
    ctx: &Ctx,
    id: &str,
    m: M,
    bbox: (f64, f64, f64, f64),
) -> Option<Vec<u8>> {
    let def = ctx.defs.get(id)?;
    if def.tag != "mask" {
        return None;
    }
    let mut mm = m;
    if let Some(t) = get_attr(def.attrs, "transform") {
        mm = mm * parse_transform(t);
    }
    if get_attr(def.attrs, "maskContentUnits")
        .map(|u| u.eq_ignore_ascii_case("objectBoundingBox"))
        .unwrap_or(false)
    {
        mm = mm * bbox_matrix(bbox);
    }
    let mut cov = vec![0u8; ctx.w2 * ctx.h2];
    // mask content inherits the mask element's own paint properties
    let inherit = Style::default().with(def.attrs);
    paint_mask_shapes(ctx, def.content_start, def.content_end, mm, &mut cov, 0, &inherit);
    // The mask region (default -10%..120% of the bounding box) clips the
    // content, so a stray shape cannot paint outside the referencing element.
    if let Some((rx, ry, rw, rh)) = mask_region(def.attrs, bbox) {
        if let Some(rc) = rect_coverage(ctx, m, rx, ry, rw, rh) {
            mul_mask(&mut cov, &rc);
        }
    }
    Some(cov)
}

fn mask_region(attrs: &str, bbox: (f64, f64, f64, f64)) -> Option<(f64, f64, f64, f64)> {
    let (bx, by, bw, bh) = bbox;
    let obb = !get_attr(attrs, "maskUnits")
        .map(|u| u.eq_ignore_ascii_case("userSpaceOnUse"))
        .unwrap_or(false);
    if bw <= 0.0 || bh <= 0.0 {
        return None;
    }
    let frac = |k: &str, def: f64, base: f64| -> f64 {
        match get_attr(attrs, k) {
            Some(v) => {
                let v = v.trim();
                if let Some(p) = v.strip_suffix('%') {
                    p.parse::<f64>().unwrap_or(def * 100.0) / 100.0 * base
                } else {
                    parse_px(v).unwrap_or(def * base)
                }
            }
            None => def * base,
        }
    };
    let origin = if obb { (bx, by) } else { (0.0, 0.0) };
    Some((
        origin.0 + frac("x", -0.1, bw),
        origin.1 + frac("y", -0.1, bh),
        frac("width", 1.2, bw),
        frac("height", 1.2, bh),
    ))
}

#[allow(clippy::too_many_arguments)]
fn paint_mask_shapes(
    ctx: &Ctx,
    start: usize,
    end: usize,
    m: M,
    cov: &mut [u8],
    depth: usize,
    inherit: &Style,
) {
    if depth > 12 {
        return;
    }
    let mut i = start;
    let mut tmp = vec![0u8; ctx.w2 * ctx.h2];
    while let Some(t) = next_tag(ctx.txt, &mut i) {
        if t.start >= end {
            break;
        }
        if t.closing {
            continue;
        }
        let tm = match get_attr(t.attrs, "transform") {
            Some(v) => m * parse_transform(v),
            None => m,
        };
        if is_shape(t.name) {
            let st = inherit.with(t.attrs).effective();
            let (polys, _) = shape_geometry(t.name, t.attrs);
            if polys.is_empty() {
                continue;
            }
            // luminance (default mask-type) × fill alpha × opacities
            let Some(col) = st.fill.resolve(ctx.grads) else { continue };
            let r = ((col >> 16) & 255) as f64;
            let g = ((col >> 8) & 255) as f64;
            let b = (col & 255) as f64;
            let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let a = ((col >> 24) & 255) as f64 / 255.0;
            let w = (lum * a * st.fill_opacity * 255.0).clamp(0.0, 255.0);
            if w <= 0.0 {
                continue;
            }
            let polys: Vec<Vec<(f64, f64)>> = polys
                .iter()
                .map(|p| p.iter().map(|&(x, y)| tm.apply(x, y)).collect())
                .collect();
            tmp.fill(0);
            fill_scan(&mut tmp, ctx.w2, ctx.h2, &polys, st.evenodd);
            for (c, t) in cov.iter_mut().zip(&tmp) {
                let add = *t as u32 * w as u32 / 255;
                *c = (*c as u32 + add - (*c as u32 * add / 255)).min(255) as u8;
            }
        } else if !t.selfclose && !SKIP.contains(&t.name) {
            let (ce, after) = matching_close(ctx.txt, i, t.name);
            let next = inherit.with(t.attrs);
            paint_mask_shapes(ctx, i, ce, tm, cov, depth + 1, &next);
            i = after;
        }
    }
}

// ---------- patterns ----------

fn blend_pattern(
    ctx: &Ctx,
    dst: &mut [u32],
    mask: &[u8],
    id: &str,
    m: M,
    bbox: (f64, f64, f64, f64),
    opacity: f64,
) {
    let Some(def) = ctx.defs.get(id) else { return };
    if def.tag != "pattern" {
        return;
    }
    let obb = !get_attr(def.attrs, "patternUnits")
        .map(|u| u.eq_ignore_ascii_case("userSpaceOnUse"))
        .unwrap_or(false);
    let (px, py, pw, ph) = pattern_rect(def.attrs, bbox, obb);
    if pw <= 0.0 || ph <= 0.0 {
        return;
    }
    let sc = m_scale(m) / SS as f64;
    let tw = (pw * sc).abs().max(1.0);
    let th = (ph * sc).abs().max(1.0);
    let (ox, oy) = m.apply(px, py);

    // Paint the pattern content once, in absolute device coordinates. Sampling
    // below folds every pixel back into the first tile, so the content only
    // has to exist there (the normal case: pattern kids live inside the tile).
    let mut pat = vec![0u32; ctx.w * ctx.h];
    let mut pstate = State {
        m,
        st: Style::default().with(def.attrs),
        clips: Vec::new(),
        masks: Vec::new(),
        filters: Vec::new(),
        vp: (pw, ph),
    };
    if let Some(t) = get_attr(def.attrs, "patternTransform") {
        // patternTransform maps the tile's local space around its origin
        pstate.m = m * translate(px, py) * parse_transform(t) * translate(-px, -py);
    }
    paint_range(ctx, def.content_start, def.content_end, &pstate, &mut pat, 0);

    let w = ctx.w;
    let h = ctx.h;
    for y in 0..h {
        for x in 0..w {
            let m4 = mask[(2 * y) * ctx.w2 + 2 * x] as u32
                + mask[(2 * y) * ctx.w2 + 2 * x + 1] as u32
                + mask[(2 * y + 1) * ctx.w2 + 2 * x] as u32
                + mask[(2 * y + 1) * ctx.w2 + 2 * x + 1] as u32;
            if m4 == 0 {
                continue;
            }
            let sx = (ox + (x as f64 - ox).rem_euclid(tw)).clamp(0.0, (w - 1) as f64) as usize;
            let sy = (oy + (y as f64 - oy).rem_euclid(th)).clamp(0.0, (h - 1) as f64) as usize;
            let src = pat[sy * w + sx];
            if src >> 24 == 0 {
                continue;
            }
            let a = ((src >> 24) * m4 / 1020) as f64 / 255.0 * opacity;
            blend_px(&mut dst[y * w + x], src, a);
        }
    }
}

fn pattern_rect(
    attrs: &str,
    bbox: (f64, f64, f64, f64),
    obb: bool,
) -> (f64, f64, f64, f64) {
    let (bx, by, bw, bh) = bbox;
    let val = |k: &str, def: f64, base: f64| -> f64 {
        match get_attr(attrs, k) {
            Some(v) => {
                let v = v.trim();
                if let Some(p) = v.strip_suffix('%') {
                    p.parse::<f64>().unwrap_or(def * 100.0) / 100.0 * base
                } else {
                    parse_px(v).unwrap_or(def * base)
                }
            }
            None => def * base,
        }
    };
    if obb {
        if bw <= 0.0 || bh <= 0.0 {
            return (0.0, 0.0, 0.0, 0.0);
        }
        (
            bx + val("x", 0.0, bw),
            by + val("y", 0.0, bh),
            val("width", 0.0, bw),
            val("height", 0.0, bh),
        )
    } else {
        (val("x", 0.0, bw), val("y", 0.0, bh), val("width", 0.0, bw), val("height", 0.0, bh))
    }
}

// ---------- filters ----------

fn m_scale(m: M) -> f64 {
    (m.a.hypot(m.b) + m.c.hypot(m.d)) / 2.0
}

fn blend_px(dst: &mut u32, src: u32, a: f64) {
    let a = a.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    let d = *dst;
    let (sr, sg, sb) = (((src >> 16) & 255) as f64, ((src >> 8) & 255) as f64, (src & 255) as f64);
    let (dr, dg, db, da) = (
        ((d >> 16) & 255) as f64,
        ((d >> 8) & 255) as f64,
        (d & 255) as f64,
        (d >> 24) as f64,
    );
    let sa = a;
    let inv = 1.0 - sa;
    let r = (sr * sa + dr * inv) as u32;
    let g = (sg * sa + dg * inv) as u32;
    let b = (sb * sa + db * inv) as u32;
    let oa = (sa * 255.0 + da * inv) as u32;
    *dst = (oa.min(255) << 24) | (r.min(255) << 16) | (g.min(255) << 8) | b.min(255);
}

fn composite_layer(ctx: &Ctx, img: &mut [u32], layer: &[u32], cov: Option<&[u8]>) {
    let w = ctx.w;
    for y in 0..ctx.h {
        for x in 0..w {
            let mut a = (layer[y * w + x] >> 24) as u32;
            if a == 0 {
                continue;
            }
            if let Some(c) = cov {
                let c4 = (c[(2 * y) * ctx.w2 + 2 * x] as u32
                    + c[(2 * y) * ctx.w2 + 2 * x + 1] as u32
                    + c[(2 * y + 1) * ctx.w2 + 2 * x] as u32
                    + c[(2 * y + 1) * ctx.w2 + 2 * x + 1] as u32)
                    / 4;
                a = a * c4 / 255;
            }
            if a == 0 {
                continue;
            }
            let src = layer[y * w + x];
            blend_px(&mut img[y * w + x], src, a as f64 / 255.0);
        }
    }
}

// ---------- filters ----------

enum Prim {
    Blur { input: Option<String>, sx: f64, sy: f64, result: Option<String> },
    Offset { input: Option<String>, dx: f64, dy: f64, result: Option<String> },
    Flood { color: u32, opacity: f64, result: Option<String> },
    ColorMatrix { input: Option<String>, m: [f64; 20], result: Option<String> },
    Merge { inputs: Vec<String>, result: Option<String> },
    Blend { input: Option<String>, in2: Option<String>, mode: String, result: Option<String> },
    Composite { input: Option<String>, in2: Option<String>, op: String, k: [f64; 4], result: Option<String> },
    DropShadow {
        input: Option<String>,
        dx: f64,
        dy: f64,
        sx: f64,
        sy: f64,
        color: u32,
        opacity: f64,
        result: Option<String>,
    },
}

fn parse_filter(ctx: &Ctx, def: &Def) -> Vec<Prim> {
    let mut out = Vec::new();
    let mut i = def.content_start;
    while let Some(t) = next_tag(ctx.txt, &mut i) {
        if t.start >= def.content_end {
            break;
        }
        if t.closing {
            continue;
        }
        let input = get_attr(t.attrs, "in").map(|s| s.to_string());
        let result = get_attr(t.attrs, "result").map(|s| s.to_string());
        match t.name {
            "feGaussianBlur" => {
                let n = get_attr(t.attrs, "stdDeviation").map(nums).unwrap_or_default();
                let sx = n.first().copied().unwrap_or(0.0).abs();
                let sy = n.get(1).copied().unwrap_or(sx).abs();
                out.push(Prim::Blur { input, sx, sy, result });
            }
            "feOffset" => out.push(Prim::Offset {
                input,
                dx: attrf(t.attrs, "dx"),
                dy: attrf(t.attrs, "dy"),
                result,
            }),
            "feFlood" => {
                let color = get_style(t.attrs, "flood-color")
                    .or_else(|| get_attr(t.attrs, "flood-color"))
                    .and_then(parse_color)
                    .unwrap_or(0xff00_0000);
                let opacity = get_style(t.attrs, "flood-opacity")
                    .or_else(|| get_attr(t.attrs, "flood-opacity"))
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0);
                out.push(Prim::Flood { color, opacity, result });
            }
            "feColorMatrix" => {
                let ty = get_attr(t.attrs, "type").unwrap_or("matrix");
                let mtx = match ty {
                    "saturate" => {
                        let s = attrf(t.attrs, "values");
                        saturate_matrix(s)
                    }
                    "hueRotate" => hue_matrix(attrf(t.attrs, "values")),
                    "luminanceToAlpha" => {
                        let mut m = [0.0; 20];
                        m[15] = 0.2126;
                        m[16] = 0.7152;
                        m[17] = 0.0722;
                        m
                    }
                    _ => {
                        let v = get_attr(t.attrs, "values").map(nums).unwrap_or_default();
                        let mut m = [0.0; 20];
                        for (k, x) in v.iter().take(20).enumerate() {
                            m[k] = *x;
                        }
                        m
                    }
                };
                out.push(Prim::ColorMatrix { input, m: mtx, result });
            }
            "feMerge" => {
                let mut inputs = Vec::new();
                let (ce, after) = matching_close(ctx.txt, i, "feMerge");
                let mut j = i;
                while let Some(n) = next_tag(ctx.txt, &mut j) {
                    if n.start >= ce {
                        break;
                    }
                    if n.closing || n.name != "feMergeNode" {
                        continue;
                    }
                    inputs.push(get_attr(n.attrs, "in").unwrap_or("SourceGraphic").to_string());
                }
                i = after;
                out.push(Prim::Merge { inputs, result });
            }
            "feBlend" => out.push(Prim::Blend {
                input,
                in2: get_attr(t.attrs, "in2").map(|s| s.to_string()),
                mode: get_attr(t.attrs, "mode").unwrap_or("normal").to_ascii_lowercase(),
                result,
            }),
            "feComposite" => {
                let k = [
                    attrf(t.attrs, "k1"),
                    attrf(t.attrs, "k2"),
                    attrf(t.attrs, "k3"),
                    attrf(t.attrs, "k4"),
                ];
                out.push(Prim::Composite {
                    input,
                    in2: get_attr(t.attrs, "in2").map(|s| s.to_string()),
                    op: get_attr(t.attrs, "operator").unwrap_or("over").to_ascii_lowercase(),
                    k,
                    result,
                });
            }
            "feDropShadow" => {
                let n = get_attr(t.attrs, "stdDeviation").map(nums).unwrap_or_default();
                let sx = n.first().copied().unwrap_or(0.0).abs();
                let sy = n.get(1).copied().unwrap_or(sx).abs();
                let color = get_style(t.attrs, "flood-color")
                    .or_else(|| get_attr(t.attrs, "flood-color"))
                    .and_then(parse_color)
                    .unwrap_or(0xff00_0000);
                let opacity = get_style(t.attrs, "flood-opacity")
                    .or_else(|| get_attr(t.attrs, "flood-opacity"))
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0);
                out.push(Prim::DropShadow {
                    input,
                    dx: attrf(t.attrs, "dx"),
                    dy: attrf(t.attrs, "dy"),
                    sx,
                    sy,
                    color,
                    opacity,
                    result,
                });
            }
            _ => {}
        }
    }
    out
}

fn saturate_matrix(s: f64) -> [f64; 20] {
    let s = s.clamp(0.0, 10.0);
    [
        0.213 + 0.787 * s, 0.715 - 0.715 * s, 0.072 - 0.072 * s, 0.0, 0.0,
        0.213 - 0.213 * s, 0.715 + 0.285 * s, 0.072 - 0.072 * s, 0.0, 0.0,
        0.213 - 0.213 * s, 0.715 - 0.715 * s, 0.072 + 0.928 * s, 0.0, 0.0,
        0.0, 0.0, 0.0, 1.0, 0.0,
    ]
}

fn hue_matrix(deg: f64) -> [f64; 20] {
    let a = deg.to_radians();
    let (s, c) = a.sin_cos();
    [
        0.213 + c * 0.787 - s * 0.213, 0.715 - c * 0.715 - s * 0.715, 0.072 - c * 0.072 + s * 0.928, 0.0, 0.0,
        0.213 - c * 0.213 + s * 0.143, 0.715 + c * 0.285 + s * 0.140, 0.072 - c * 0.072 - s * 0.283, 0.0, 0.0,
        0.213 - c * 0.213 - s * 0.787, 0.715 - c * 0.715 + s * 0.715, 0.072 + c * 0.928 + s * 0.072, 0.0, 0.0,
        0.0, 0.0, 0.0, 1.0, 0.0,
    ]
}

fn filter_chain(ctx: &Ctx, refs: &[(String, M)], layer: Vec<u32>, m: M) -> Vec<u32> {
    let mut cur = layer;
    for (id, _) in refs {
        if let Some(def) = ctx.defs.get(id) {
            if def.tag == "filter" {
                cur = apply_filter(ctx, def, cur, m);
            }
        }
    }
    cur
}

fn apply_filter(ctx: &Ctx, def: &Def, source: Vec<u32>, m: M) -> Vec<u32> {
    let prims = parse_filter(ctx, def);
    if prims.is_empty() {
        return source;
    }
    let scale = m_scale(m) / SS as f64;
    let region = expand_bbox(alpha_bbox(&source, ctx.w, ctx.h), ctx.w, ctx.h, 0.1);
    let mut results: Vec<(String, Vec<u32>)> = vec![
        ("SourceGraphic".to_string(), source.clone()),
        ("SourceAlpha".to_string(), alpha_layer(&source)),
    ];
    let mut prev = source;
    for p in &prims {
        let res = eval_prim(ctx, p, &results, &prev, &region, scale);
        let name = match p {
            Prim::Blur { result, .. }
            | Prim::Offset { result, .. }
            | Prim::Flood { result, .. }
            | Prim::ColorMatrix { result, .. }
            | Prim::Merge { result, .. }
            | Prim::Blend { result, .. }
            | Prim::Composite { result, .. }
            | Prim::DropShadow { result, .. } => result.clone(),
        };
        if let Some(n) = name {
            results.push((n, res.clone()));
        }
        prev = res;
    }
    prev
}

fn eval_prim(
    ctx: &Ctx,
    p: &Prim,
    results: &[(String, Vec<u32>)],
    prev: &[u32],
    region: &Option<(usize, usize, usize, usize)>,
    scale: f64,
) -> Vec<u32> {
    let pick = |name: &Option<String>| -> Vec<u32> {
        match name {
            Some(n) => results
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| prev.to_vec()),
            None => prev.to_vec(),
        }
    };
    match p {
        Prim::Blur { input, sx, sy, .. } => {
            gaussian_blur(&pick(input), ctx.w, ctx.h, sx * scale, sy * scale)
        }
        Prim::Offset { input, dx, dy, .. } => {
            offset(&pick(input), ctx.w, ctx.h, (dx * scale).round() as isize, (dy * scale).round() as isize)
        }
        Prim::Flood { color, opacity, .. } => flood(*color, *opacity, *region, ctx.w, ctx.h),
        Prim::ColorMatrix { input, m, .. } => color_matrix(&pick(input), m),
        Prim::Merge { inputs, .. } => {
            let mut out = vec![0u32; ctx.w * ctx.h];
            for name in inputs {
                let src = results
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.as_slice())
                    .unwrap_or(prev);
                for (d, s) in out.iter_mut().zip(src) {
                    let a = (*s >> 24) as f64 / 255.0;
                    blend_px(d, *s, a);
                }
            }
            out
        }
        Prim::Blend { input, in2, mode, .. } => {
            let a = pick(input);
            let b = pick(in2);
            blend_layers(&a, &b, mode)
        }
        Prim::Composite { input, in2, op, k, .. } => {
            let a = pick(input);
            let b = pick(in2);
            composite_layers(&a, &b, op, k)
        }
        Prim::DropShadow { input, dx, dy, sx, sy, color, opacity, .. } => {
            let src = pick(input);
            let alpha = alpha_layer(&src);
            let shifted = offset(&alpha, ctx.w, ctx.h, (dx * scale).round() as isize, (dy * scale).round() as isize);
            let blurred = gaussian_blur(&shifted, ctx.w, ctx.h, sx * scale, sy * scale);
            let mut shadow = blurred;
            let (cr, cg, cb) = ((color >> 16) & 255, (color >> 8) & 255, color & 255);
            for px in shadow.iter_mut() {
                let a = ((*px >> 24) as f64 * opacity) as u32;
                *px = a.min(255) << 24 | cr << 16 | cg << 8 | cb;
            }
            let mut out = shadow;
            for (d, s) in out.iter_mut().zip(&src) {
                let a = (*s >> 24) as f64 / 255.0;
                blend_px(d, *s, a);
            }
            out
        }
    }
}

fn alpha_layer(src: &[u32]) -> Vec<u32> {
    src.iter().map(|p| (p & 0xff00_0000) | 0x00ff_ffff).collect()
}

fn alpha_bbox(src: &[u32], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            if src[y * w + x] >> 24 != 0 {
                any = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    any.then_some((x0, y0, x1, y1))
}

fn expand_bbox(
    b: Option<(usize, usize, usize, usize)>,
    w: usize,
    h: usize,
    frac: f64,
) -> Option<(usize, usize, usize, usize)> {
    let (x0, y0, x1, y1) = b?;
    let padx = ((x1 - x0) as f64 * frac).ceil() as usize;
    let pady = ((y1 - y0) as f64 * frac).ceil() as usize;
    Some((
        x0.saturating_sub(padx),
        y0.saturating_sub(pady),
        (x1 + padx).min(w),
        (y1 + pady).min(h),
    ))
}

fn flood(
    color: u32,
    opacity: f64,
    region: Option<(usize, usize, usize, usize)>,
    w: usize,
    h: usize,
) -> Vec<u32> {
    let mut out = vec![0u32; w * h];
    let Some((x0, y0, x1, y1)) = region else { return out };
    let a = (((color >> 24) & 255) as f64 * opacity) as u32;
    let c = a.min(255) << 24 | color & 0x00ff_ffff;
    for y in y0..y1 {
        for x in x0..x1 {
            out[y * w + x] = c;
        }
    }
    out
}

fn offset(src: &[u32], w: usize, h: usize, dx: isize, dy: isize) -> Vec<u32> {
    let mut out = vec![0u32; w * h];
    if dx == 0 && dy == 0 {
        return src.to_vec();
    }
    for y in 0..h {
        let sy = y as isize - dy;
        if sy < 0 || sy >= h as isize {
            continue;
        }
        for x in 0..w {
            let sx = x as isize - dx;
            if sx < 0 || sx >= w as isize {
                continue;
            }
            out[y * w + x] = src[sy as usize * w + sx as usize];
        }
    }
    out
}

fn box_radius(s: f64) -> usize {
    if s <= 0.0 {
        0
    } else {
        (s * 3.0 * (2.0 * std::f64::consts::PI).sqrt() / 4.0 + 0.5).floor() as usize
    }
}

fn box_h(buf: &mut [f32], w: usize, h: usize, r: usize) {
    if r == 0 || w == 0 {
        return;
    }
    let ri = r as isize;
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut row = vec![0f32; w];
    for y in 0..h {
        let base = y * w;
        let mut sum = 0.0f32;
        for k in -ri..=ri {
            sum += buf[base + (k.clamp(0, w as isize - 1) as usize)];
        }
        for x in 0..w {
            row[x] = sum * norm;
            let add = (x as isize + ri + 1).clamp(0, w as isize - 1) as usize;
            let sub = (x as isize - ri).clamp(0, w as isize - 1) as usize;
            sum += buf[base + add] - buf[base + sub];
        }
        buf[base..base + w].copy_from_slice(&row);
    }
}

fn box_v(buf: &mut [f32], w: usize, h: usize, r: usize) {
    if r == 0 || h == 0 {
        return;
    }
    let ri = r as isize;
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut col = vec![0f32; h];
    for x in 0..w {
        let mut sum = 0.0f32;
        for k in -ri..=ri {
            sum += buf[(k.clamp(0, h as isize - 1) as usize) * w + x];
        }
        for y in 0..h {
            col[y] = sum * norm;
            let add = (y as isize + ri + 1).clamp(0, h as isize - 1) as usize;
            let sub = (y as isize - ri).clamp(0, h as isize - 1) as usize;
            sum += buf[add * w + x] - buf[sub * w + x];
        }
        for y in 0..h {
            buf[y * w + x] = col[y];
        }
    }
}

fn gaussian_blur(src: &[u32], w: usize, h: usize, sx: f64, sy: f64) -> Vec<u32> {
    if sx <= 0.0 && sy <= 0.0 {
        return src.to_vec();
    }
    let n = w * h;
    let mut r = vec![0f32; n];
    let mut g = vec![0f32; n];
    let mut b = vec![0f32; n];
    let mut a = vec![0f32; n];
    for i in 0..n {
        let p = src[i];
        let al = ((p >> 24) & 255) as f32;
        let k = al / 255.0;
        r[i] = ((p >> 16) & 255) as f32 * k;
        g[i] = ((p >> 8) & 255) as f32 * k;
        b[i] = (p & 255) as f32 * k;
        a[i] = al;
    }
    let dx = box_radius(sx);
    let dy = box_radius(sy);
    for _ in 0..3 {
        if dx > 0 {
            box_h(&mut r, w, h, dx);
            box_h(&mut g, w, h, dx);
            box_h(&mut b, w, h, dx);
            box_h(&mut a, w, h, dx);
        }
        if dy > 0 {
            box_v(&mut r, w, h, dy);
            box_v(&mut g, w, h, dy);
            box_v(&mut b, w, h, dy);
            box_v(&mut a, w, h, dy);
        }
    }
    let mut out = vec![0u32; n];
    for i in 0..n {
        let al = a[i].clamp(0.0, 255.0);
        if al < 0.5 {
            continue;
        }
        let k = 255.0 / al;
        let rr = (r[i] * k).clamp(0.0, 255.0) as u32;
        let gg = (g[i] * k).clamp(0.0, 255.0) as u32;
        let bb = (b[i] * k).clamp(0.0, 255.0) as u32;
        out[i] = (al as u32) << 24 | rr << 16 | gg << 8 | bb;
    }
    out
}

fn color_matrix(src: &[u32], m: &[f64; 20]) -> Vec<u32> {
    src.iter()
        .map(|p| {
            let r = ((p >> 16) & 255) as f64 / 255.0;
            let g = ((p >> 8) & 255) as f64 / 255.0;
            let b = (p & 255) as f64 / 255.0;
            let a = ((p >> 24) & 255) as f64 / 255.0;
            let ch = |row: usize| {
                (m[row * 5] * r
                    + m[row * 5 + 1] * g
                    + m[row * 5 + 2] * b
                    + m[row * 5 + 3] * a
                    + m[row * 5 + 4])
                    .clamp(0.0, 1.0)
            };
            let (nr, ng, nb, na) = (ch(0), ch(1), ch(2), ch(3));
            ((na * 255.0) as u32) << 24
                | ((nr * 255.0) as u32) << 16
                | ((ng * 255.0) as u32) << 8
                | (nb * 255.0) as u32
        })
        .collect()
}

fn blend_layers(a: &[u32], b: &[u32], mode: &str) -> Vec<u32> {
    a.iter()
        .zip(b)
        .map(|(&sa, &sb)| {
            let (sar, sag, sab, saa) = (
                ((sa >> 16) & 255) as f64,
                ((sa >> 8) & 255) as f64,
                (sa & 255) as f64,
                (sa >> 24) as f64 / 255.0,
            );
            let (sbr, sbg, sbb, sba) = (
                ((sb >> 16) & 255) as f64,
                ((sb >> 8) & 255) as f64,
                (sb & 255) as f64,
                (sb >> 24) as f64 / 255.0,
            );
            let op = |x: f64, y: f64| match mode {
                "multiply" => x * y / 255.0,
                "screen" => 255.0 - (255.0 - x) * (255.0 - y) / 255.0,
                "darken" => x.min(y),
                "lighten" => x.max(y),
                _ => x,
            };
            let cr = op(sar, sbr);
            let cg = op(sag, sbg);
            let cb = op(sab, sbb);
            // source-over where the blended color is the "source" channel
            let inv = 1.0 - saa;
            let r = cr * saa + sbr * inv;
            let g = cg * saa + sbg * inv;
            let b = cb * saa + sbb * inv;
            let al = saa + sba * inv;
            ((al * 255.0) as u32) << 24
                | (r.clamp(0.0, 255.0) as u32) << 16
                | (g.clamp(0.0, 255.0) as u32) << 8
                | b.clamp(0.0, 255.0) as u32
        })
        .collect()
}

fn composite_layers(a: &[u32], b: &[u32], op: &str, k: &[f64; 4]) -> Vec<u32> {
    a.iter()
        .zip(b)
        .map(|(&sa, &sb)| {
            let (sr, sg, sbb, saa) = (
                ((sa >> 16) & 255) as f64,
                ((sa >> 8) & 255) as f64,
                (sa & 255) as f64,
                (sa >> 24) as f64 / 255.0,
            );
            let (dr, dg, db, da) = (
                ((sb >> 16) & 255) as f64,
                ((sb >> 8) & 255) as f64,
                (sb & 255) as f64,
                (sb >> 24) as f64 / 255.0,
            );
            let (r, g, b, al) = match op {
                "in" => (sr, sg, sbb, saa * da),
                "out" => (sr, sg, sbb, saa * (1.0 - da)),
                "atop" => (sr * da + dr * (1.0 - saa), sg * da + dg * (1.0 - saa), sbb * da + db * (1.0 - saa), da),
                "xor" => (sr * (1.0 - da) + dr * (1.0 - saa), sg * (1.0 - da) + dg * (1.0 - saa), sbb * (1.0 - da) + db * (1.0 - saa), saa * (1.0 - da) + da * (1.0 - saa)),
                "arithmetic" => {
                    let f = |x: f64, y: f64| (k[0] * x * y + k[1] * x + k[2] * y + k[3]).clamp(0.0, 255.0);
                    (f(sr, dr), f(sg, dg), f(sbb, db), saa + da - saa * da)
                }
                _ => {
                    // over
                    let inv = 1.0 - saa;
                    (sr * saa + dr * inv, sg * saa + dg * inv, sbb * saa + db * inv, saa + da * inv)
                }
            };
            ((al.clamp(0.0, 1.0) * 255.0) as u32) << 24
                | (r.clamp(0.0, 255.0) as u32) << 16
                | (g.clamp(0.0, 255.0) as u32) << 8
                | b.clamp(0.0, 255.0) as u32
        })
        .collect()
}



// ---------- xml scanning (shortcut: not namespace-aware) ----------

struct Tag<'a> {
    name: &'a str,
    attrs: &'a str,
    selfclose: bool,
    closing: bool,
    /// byte offset of this tag's `<` in the source (text content between
    /// tags is sliced with it)
    start: usize,
}

/// Root `<svg ...>`: returns (tag name, raw attrs string).
fn root_tag(txt: &str) -> Option<(&str, &str)> {
    let mut i = 0usize;
    let t = next_tag(txt, &mut i)?;
    (!t.closing && t.name == "svg").then_some((t.name, t.attrs))
}

fn next_tag<'a>(txt: &'a str, i: &mut usize) -> Option<Tag<'a>> {
    loop {
        let lt = txt[*i..].find('<')? + *i;
        let rest = &txt[lt..];
        if rest.starts_with("<!--") {
            *i = lt + txt[lt..].find("-->")? + 3;
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            *i = lt + txt[lt..].find('>')? + 1;
            continue;
        }
        let gt = lt + txt[lt..].find('>')?;
        let inner = rest[1..gt - lt].trim();
        *i = gt + 1;
        let (closing, inner) = match inner.strip_prefix('/') {
            Some(r) => (true, r.trim_start()),
            None => (false, inner),
        };
        let name_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
        let (name, mut attrs) = inner.split_at(name_end);
        let selfclose = attrs.trim_end().ends_with('/');
        if selfclose {
            attrs = attrs.trim_end().trim_end_matches('/');
        }
        if name.is_empty() {
            continue;
        }
        return Some(Tag { name, attrs: attrs.trim(), selfclose, closing, start: lt });
    }
}

/// Attribute value by exact key (`key="v"`, `key='v'`, `key=v`).
fn get_attr<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = raw;
    while !rest.is_empty() {
        let ws = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
        rest = &rest[ws..];
        let eq = rest.find('=')?;
        let name = rest[..eq].trim_end();
        let after = rest[eq + 1..].trim_start();
        let (val, tail) = match after.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let end = after[1..].find(q)? + 1;
                (&after[1..end], &after[end + 1..])
            }
            _ => {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                (&after[..end], &after[end..])
            }
        };
        if name.eq_ignore_ascii_case(key) {
            return Some(val);
        }
        rest = tail;
    }
    None
}

/// Inline `style="a:b;c:d"` property.
fn get_style<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    let style = get_attr(raw, "style")?;
    for decl in style.split(';') {
        let Some((k, v)) = decl.split_once(':') else { continue };
        if k.trim() == key {
            return Some(v.trim());
        }
    }
    None
}

fn attrf(raw: &str, key: &str) -> f64 {
    get_attr(raw, key).and_then(parse_px).unwrap_or(0.0)
}

/// "12" / "12px" / "12.5" → number; %, em, url(…), missing → None.
fn parse_px(s: &str) -> Option<f64> {
    let s = s.trim();
    let n: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+' || *c == 'e' || *c == 'E')
        .collect();
    let v: f64 = n.parse().ok()?;
    let unit = s[n.len()..].trim();
    (unit.is_empty() || unit == "px").then_some(v)
}

fn nums(s: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() {
            let start = i;
            i += 1;
            let mut dot = false;
            while i < b.len()
                && (b[i].is_ascii_digit()
                    || (b[i] == b'.' && !dot)
                    || b[i] == b'e'
                    || b[i] == b'E'
                    || ((b[i] == b'-' || b[i] == b'+') && (b[i - 1] == b'e' || b[i - 1] == b'E')))
            {
                if b[i] == b'.' {
                    dot = true;
                }
                i += 1;
            }
            if let Ok(v) = s[start..i].parse::<f64>() {
                out.push(v);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn nums4(s: &str) -> Option<[f64; 4]> {
    let n = nums(s);
    (n.len() == 4 && n[2] > 0.0 && n[3] > 0.0).then_some([n[0], n[1], n[2], n[3]])
}

// ---------- color ----------

/// Packed `0xAARRGGBB`.
fn parse_color(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix('#') {
        let d: Vec<u32> = h.chars().filter_map(|c| c.to_digit(16)).collect();
        return match d.len() {
            3 => Some(0xff00_0000 | d[0] * 0x11 << 16 | d[1] * 0x11 << 8 | d[2] * 0x11),
            6 => Some(0xff00_0000 | (d[0] * 16 + d[1]) << 16 | (d[2] * 16 + d[3]) << 8 | (d[4] * 16 + d[5])),
            8 => {
                Some((d[0] * 16 + d[1]) << 24 | (d[2] * 16 + d[3]) << 16 | (d[4] * 16 + d[5]) << 8 | (d[6] * 16 + d[7]))
            }
            _ => None,
        };
    }
    let ls = s.to_ascii_lowercase();
    if let Some(inner) = ls
        .strip_prefix("rgb")
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
    {
        let v = nums(inner);
        if v.len() < 3 {
            return None;
        }
        let a = if v.len() >= 4 { (v[3].clamp(0.0, 1.0) * 255.0) as u32 } else { 255 };
        let pct = inner.contains('%');
        let ch = |x: f64| {
            let x = if pct { x / 100.0 * 255.0 } else { x };
            (x.clamp(0.0, 255.0)) as u32
        };
        let (r, g, b) = (ch(v[0]), ch(v[1]), ch(v[2]));
        return Some(a << 24 | r << 16 | g << 8 | b);
    }
    let n = match ls.as_str() {
        "black" => 0xff00_0000,
        "white" => 0xffff_ffff,
        "red" => 0xffff_0000,
        "lime" | "green" => 0xff00_ff00,
        "blue" => 0xff00_00ff,
        "yellow" => 0xff_ffff00,
        "cyan" | "aqua" => 0xff00_ffff,
        "magenta" | "fuchsia" => 0xffff_00ff,
        "gray" | "grey" | "silver" => 0xff80_8080,
        "dimgray" | "dimgrey" => 0xff69_6969,
        "lightgray" | "lightgrey" => 0xffd3_d3d3,
        "darkgray" | "darkgrey" => 0xffa9_a9a9,
        "maroon" => 0xff80_0000,
        "navy" => 0xff00_0080,
        "olive" => 0xff80_8000,
        "purple" => 0xff80_0080,
        "teal" => 0xff00_8080,
        "orange" => 0xff_ffa500,
        "coral" => 0xff7f_5050,
        "tomato" => 0xff63_4747,
        "gold" => 0xffda_a520,
        "khaki" => 0xfff0_e68c,
        "salmon" => 0xfffa_8072,
        "brown" => 0xffa5_2a2a,
        "violet" => 0xffee_82ee,
        "indigo" => 0xff4b_0082,
        _ => return None,
    };
    Some(n)
}

/// `Paint`: none / plain color / gradient url (resolved against the table).
#[derive(Clone)]
enum Paint {
    None,
    Color(u32),
    Url(String),
}

impl Paint {
    fn resolve(&self, grads: &[(String, u32)]) -> Option<u32> {
        match self {
            Paint::None => None,
            Paint::Color(c) => Some(*c),
            // unknown gradient id → per spec the element is not rendered
            Paint::Url(id) => grads.iter().find(|(k, _)| k == id).map(|(_, c)| *c),
        }
    }
}

// ---------- style ----------

#[derive(Clone)]
struct Style {
    fill: Paint,
    stroke: Paint,
    sw: f64,
    opacity: f64,
    fill_opacity: f64,
    stroke_opacity: f64,
    evenodd: bool,
    /// `font-size` in user units (SVG's initial value is 16)
    fs: f64,
    /// `text-anchor`: 0 start, 1 middle, 2 end
    anchor: u8,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            fill: Paint::Color(0xff00_0000), // spec default: black
            stroke: Paint::None,
            sw: 1.0,
            opacity: 1.0,
            fill_opacity: 1.0,
            stroke_opacity: 1.0,
            evenodd: false,
            fs: 16.0,
            anchor: 0,
        }
    }
}

impl Style {
    /// Inherit + apply presentation attributes (inline style wins over them).
    fn with(&self, raw: &str) -> Style {
        let mut s = self.clone();
        for key in [
            "fill",
            "stroke",
            "stroke-width",
            "fill-rule",
            "fill-opacity",
            "stroke-opacity",
            "opacity",
            "font-size",
            "text-anchor",
        ] {
            if let Some(v) = get_style(raw, key).or_else(|| get_attr(raw, key)) {
                s.apply(key, v);
            }
        }
        s
    }

    fn apply(&mut self, key: &str, v: &str) {
        let v = v.trim();
        match key {
            "fill" => self.fill = self.paint(v),
            "stroke" => self.stroke = self.paint(v),
            "stroke-width" => self.sw = parse_px(v).unwrap_or(self.sw).max(0.0),
            "fill-rule" => self.evenodd = v.eq_ignore_ascii_case("evenodd"),
            "fill-opacity" => self.fill_opacity = v.parse::<f64>().unwrap_or(self.fill_opacity).clamp(0.0, 1.0),
            "stroke-opacity" => self.stroke_opacity = v.parse::<f64>().unwrap_or(self.stroke_opacity).clamp(0.0, 1.0),
            "opacity" => self.opacity = v.parse::<f64>().unwrap_or(self.opacity).clamp(0.0, 1.0),
            "font-size" => {
                if let Some(n) = parse_px(v) {
                    self.fs = n.max(0.0)
                } else if let Some(n) = v.strip_suffix("rem").and_then(|n| n.trim().parse::<f64>().ok()) {
                    // CSS px root (16): the SVG viewBox scale is applied later
                    self.fs = (n * 16.0).max(0.0)
                } else if let Some(n) = v.strip_suffix("em").and_then(|n| n.trim().parse::<f64>().ok()) {
                    self.fs = (n * self.fs).max(0.0)
                }
            }
            "text-anchor" => {
                self.anchor = match v {
                    "middle" => 1,
                    "end" => 2,
                    _ => 0,
                }
            }
            _ => {}
        }
    }

    /// `none` / `url(#id)` (fallback color after `)` if given) / plain color.
    fn paint(&self, v: &str) -> Paint {
        let v = v.trim();
        if v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("transparent") {
            return Paint::None;
        }
        if let Some(rest) = v.strip_prefix("url(") {
            let (id, after) = match rest.find(')') {
                Some(e) => (&rest[..e], rest[e + 1..].trim()),
                None => (rest, ""),
            };
            if let Some(c) = parse_color(after) {
                return Paint::Color(c);
            }
            return Paint::Url(id.trim_start_matches('#').to_string());
        }
        Paint::Color(parse_color(v).unwrap_or(0xff00_0000))
    }

    /// Fold element opacity into the two opacities (group opacity ≈ inherited
    /// — shortcut: overlapping children inside an opacity group blend twice).
    fn effective(&self) -> Style {
        let mut s = self.clone();
        s.fill_opacity *= s.opacity;
        s.stroke_opacity *= s.opacity;
        s
    }
}

// ---------- geometry ----------

#[derive(Clone, Copy)]
struct M {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl M {
    fn id() -> Self {
        M { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 }
    }
    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f)
    }
}

impl std::ops::Mul for M {
    type Output = M;
    fn mul(self, o: M) -> M {
        M {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }
}

fn parse_transform(s: &str) -> M {
    let mut m = M::id();
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim();
        let Some(close) = rest[open..].find(')') else { break };
        let v = nums(&rest[open + 1..open + close]);
        rest = &rest[open + close + 1..];
        let t = match (name, v.len()) {
            ("matrix", 6) => M { a: v[0], b: v[1], c: v[2], d: v[3], e: v[4], f: v[5] },
            ("translate", 1) => M { e: v[0], ..M::id() },
            ("translate", _) => M { e: v[0], f: v[1], ..M::id() },
            ("scale", 1) => M { a: v[0], d: v[0], ..M::id() },
            ("scale", _) => M { a: v[0], d: v[1], ..M::id() },
            ("rotate", 1..=2) => {
                let (s, c) = v[0].to_radians().sin_cos();
                let mut r = M { a: c, b: s, c: -s, d: c, ..M::id() };
                if v.len() == 2 {
                    r = M { e: v[1], f: v[1], ..M::id() } * r * M { e: -v[1], f: -v[1], ..M::id() };
                }
                r
            }
            ("skewX", 1) => M { c: v[0].to_radians().tan(), ..M::id() },
            ("skewY", 1) => M { b: v[0].to_radians().tan(), ..M::id() },
            _ => M::id(),
        };
        m = m * t;
    }
    m
}

#[derive(Clone)]
struct Sub {
    pts: Vec<(f64, f64)>,
    closed: bool,
}

/// Full path-data parser → flattened subpaths (curves become polylines).
fn parse_path(d: &str) -> Vec<Sub> {
    let b = d.as_bytes();
    let mut i = 0usize;
    let mut subs: Vec<Sub> = Vec::new();
    let mut cur = Sub { pts: Vec::new(), closed: false };
    let mut pt = (0.0, 0.0);
    let mut sub_start = (0.0, 0.0);
    let mut cmd = 0u8;
    let mut prev_ctrl: Option<(f64, f64)> = None; // for S/T smoothness

    macro_rules! ws {
        () => {
            while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
                i += 1;
            }
        };
    }
    macro_rules! num {
        () => {{
            ws!();
            let start = i;
            if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
                i += 1;
            }
            let mut dot = false; // "105.08.02" = two numbers: 105.08, .02
            while i < b.len()
                && (b[i].is_ascii_digit()
                    || (b[i] == b'.' && !dot)
                    || b[i] == b'e'
                    || b[i] == b'E'
                    || ((b[i] == b'-' || b[i] == b'+') && i > start && (b[i - 1] == b'e' || b[i - 1] == b'E')))
            {
                if b[i] == b'.' {
                    dot = true;
                }
                i += 1;
            }
            // from_utf8 on bytes (never panics on non-ASCII lands)
            std::str::from_utf8(&b[start..i]).ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0)
        }};
    }
    macro_rules! pair {
        () => {{
            let a = num!();
            let c = num!();
            (a, c)
        }};
    }
    macro_rules! flush {
        () => {
            if cur.pts.len() >= 2 {
                subs.push(std::mem::replace(&mut cur, Sub { pts: Vec::new(), closed: false }));
            } else {
                cur.pts.clear();
                cur.closed = false;
            }
        };
    }

    loop {
        ws!();
        if i >= b.len() {
            break;
        }
        let before = i;
        let c = b[i];
        if c.is_ascii_alphabetic() {
            cmd = c;
            i += 1;
        } else if cmd == 0 {
            i += 1;
            continue;
        }
        let rel = cmd.is_ascii_lowercase();
        let u = cmd.to_ascii_uppercase();
        let (px, py) = pt;
        match u {
            b'M' => {
                flush!();
                let (x, y) = pair!();
                pt = if rel { (px + x, py + y) } else { (x, y) };
                cur.pts.push(pt);
                sub_start = pt;
                prev_ctrl = None;
                cmd = if rel { b'l' } else { b'L' }; // implicit lineto
            }
            b'L' => {
                let (x, y) = pair!();
                pt = if rel { (px + x, py + y) } else { (x, y) };
                cur.pts.push(pt);
                prev_ctrl = None;
            }
            b'H' => {
                let x = num!();
                pt = (if rel { px + x } else { x }, py);
                cur.pts.push(pt);
                prev_ctrl = None;
            }
            b'V' => {
                let y = num!();
                pt = (px, if rel { py + y } else { y });
                cur.pts.push(pt);
                prev_ctrl = None;
            }
            b'C' => {
                let (x1, y1) = pair!();
                let (x2, y2) = pair!();
                let (x, y) = pair!();
                let p1 = if rel { (px + x1, py + y1) } else { (x1, y1) };
                let p2 = if rel { (px + x2, py + y2) } else { (x2, y2) };
                let p3 = if rel { (px + x, py + y) } else { (x, y) };
                flatten_cubic(&mut cur.pts, pt, p1, p2, p3);
                prev_ctrl = Some(p2);
                pt = p3;
            }
            b'S' => {
                let (x2, y2) = pair!();
                let (x, y) = pair!();
                let p1 = match prev_ctrl {
                    Some((cx, cy)) => (2.0 * px - cx, 2.0 * py - cy),
                    None => pt,
                };
                let p2 = if rel { (px + x2, py + y2) } else { (x2, y2) };
                let p3 = if rel { (px + x, py + y) } else { (x, y) };
                flatten_cubic(&mut cur.pts, pt, p1, p2, p3);
                prev_ctrl = Some(p2);
                pt = p3;
            }
            b'Q' => {
                let (x1, y1) = pair!();
                let (x, y) = pair!();
                let p1 = if rel { (px + x1, py + y1) } else { (x1, y1) };
                let p3 = if rel { (px + x, py + y) } else { (x, y) };
                flatten_quad(&mut cur.pts, pt, p1, p3);
                prev_ctrl = Some(p1);
                pt = p3;
            }
            b'T' => {
                let (x, y) = pair!();
                let p1 = match prev_ctrl {
                    Some((cx, cy)) => (2.0 * px - cx, 2.0 * py - cy),
                    None => pt,
                };
                let p3 = if rel { (px + x, py + y) } else { (x, y) };
                flatten_quad(&mut cur.pts, pt, p1, p3);
                prev_ctrl = Some(p1);
                pt = p3;
            }
            b'A' => {
                let rx = num!();
                let ry = num!();
                let _rot = num!();
                let laf = num!() != 0.0;
                let sf = num!() != 0.0;
                let (x, y) = pair!();
                let end = if rel { (px + x, py + y) } else { (x, y) };
                flatten_arc(&mut cur.pts, pt, rx.abs(), ry.abs(), laf, sf, end);
                pt = end;
                prev_ctrl = None;
            }
            b'Z' => {
                cur.closed = true;
                flush!();
                pt = sub_start;
            }
            _ => {
                // unknown command: fall through; the no-progress guard skips a char
            }
        }
        if i == before {
            i += 1; // safety: never spin on an unconsumed char
        }
    }
    flush!();
    subs
}

fn flatten_cubic(out: &mut Vec<(f64, f64)>, p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64)) {
    let len = (p1.0 - p0.0).hypot(p1.1 - p0.1) + (p2.0 - p1.0).hypot(p2.1 - p1.1) + (p3.0 - p2.0).hypot(p3.1 - p2.1);
    let n = (len / 4.0).ceil().clamp(4.0, 48.0) as usize;
    for k in 1..=n {
        let t = k as f64 / n as f64;
        let u = 1.0 - t;
        out.push((
            u * u * u * p0.0 + 3.0 * u * u * t * p1.0 + 3.0 * u * t * t * p2.0 + t * t * t * p3.0,
            u * u * u * p0.1 + 3.0 * u * u * t * p1.1 + 3.0 * u * t * t * p2.1 + t * t * t * p3.1,
        ));
    }
}

fn flatten_quad(out: &mut Vec<(f64, f64)>, p0: (f64, f64), p1: (f64, f64), p2: (f64, f64)) {
    // exact cubic equivalent
    let c1 = (p0.0 + 2.0 / 3.0 * (p1.0 - p0.0), p0.1 + 2.0 / 3.0 * (p1.1 - p0.1));
    let c2 = (p2.0 + 2.0 / 3.0 * (p1.0 - p2.0), p2.1 + 2.0 / 3.0 * (p1.1 - p2.1));
    flatten_cubic(out, p0, c1, c2, p2);
}

/// Arc → cubic segments (endpoint-to-center per SVG spec F.6).
fn flatten_arc(out: &mut Vec<(f64, f64)>, p0: (f64, f64), rx: f64, ry: f64, laf: bool, sf: bool, p1: (f64, f64)) {
    if rx <= 0.0 || ry <= 0.0 || (p0.0 == p1.0 && p0.1 == p1.1) {
        out.push(p1); // degenerate: line to end (spec: identical ends draw nothing)
        return;
    }
    let (x1, y1) = p0;
    let (x2, y2) = p1;
    let dx = (x1 - x2) / 2.0;
    let dy = (y1 - y2) / 2.0;
    let mut rx = rx;
    let mut ry = ry;
    let lambda = dx * dx / (rx * rx) + dy * dy / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }
    let den = rx * rx * dy * dy + ry * ry * dx * dx;
    let num = rx * rx * ry * ry - rx * rx * dy * dy - ry * ry * dx * dx;
    let coef = if den == 0.0 { 0.0 } else { (num / den).max(0.0).sqrt() };
    let coef = if laf == sf { -coef } else { coef }; // spec F.6.5 step 2
    let (cxp, cyp) = (coef * rx * dy / ry, -coef * ry * dx / rx);
    let (cx, cy) = (cxp + (x1 + x2) / 2.0, cyp + (y1 + y2) / 2.0);
    let ang = |ux: f64, uy: f64, vx: f64, vy: f64| {
        let (d, l) = ((ux * vx + uy * vy).clamp(-1.0, 1.0), ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt().max(1e-12));
        let a = (d / l).acos();
        if ux * vy - uy * vx < 0.0 { -a } else { a }
    };
    let th1 = ang(1.0, 0.0, (dx - cxp) / rx, (dy - cyp) / ry);
    let mut dth = ang((dx - cxp) / rx, (dy - cyp) / ry, (-dx - cxp) / rx, (-dy - cyp) / ry);
    if !sf && dth > 0.0 {
        dth -= std::f64::consts::TAU;
    } else if sf && dth < 0.0 {
        dth += std::f64::consts::TAU;
    }
    let n = ((dth.abs() / std::f64::consts::FRAC_PI_2).ceil() as usize).max(1);
    let step = dth / n as f64;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let mut th = th1;
    let mut cur = p0;
    for _ in 0..n {
        let th2 = th + step;
        let (sn, cs) = th.sin_cos(); // sin, cos
        let (sn2, cs2) = th2.sin_cos();
        let e1 = (cx + rx * cs2, cy + ry * sn2);
        let ctrl1 = (cur.0 - k * rx * sn, cur.1 + k * ry * cs);
        let ctrl2 = (e1.0 + k * rx * sn2, e1.1 - k * ry * cs2);
        flatten_cubic(out, cur, ctrl1, ctrl2, e1);
        cur = e1;
        th = th2;
    }
    out.push(p1); // snap rounding drift to the exact endpoint
}

fn rect_poly(x: f64, y: f64, w: f64, h: f64, rx: f64, ry: f64) -> Vec<(f64, f64)> {
    if rx <= 0.0 && ry <= 0.0 {
        return vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)];
    }
    let mut p = Vec::new();
    let mut arc = |p: &mut Vec<(f64, f64)>, ccx: f64, ccy: f64, a0: f64, a1: f64| {
        for k in 0..=4 {
            let a = a0 + (a1 - a0) * k as f64 / 4.0;
            p.push((ccx + rx * a.cos(), ccy + ry * a.sin()));
        }
    };
    let q = std::f64::consts::FRAC_PI_2;
    arc(&mut p, x + w - rx, y + ry, -q, 0.0);
    arc(&mut p, x + w - rx, y + h - ry, 0.0, q);
    arc(&mut p, x + rx, y + h - ry, q, 2.0 * q);
    arc(&mut p, x + rx, y + ry, 2.0 * q, 3.0 * q);
    p
}

fn ellipse_poly(cx: f64, cy: f64, rx: f64, ry: f64) -> Vec<(f64, f64)> {
    (0..48)
        .map(|k| {
            let a = k as f64 / 48.0 * std::f64::consts::TAU;
            (cx + rx * a.cos(), cy + ry * a.sin())
        })
        .collect()
}

/// Stroke outline polygons: one quad per segment + round joins (octagons).
fn stroke_polys(subs: &[Sub], m: M, sw: f64) -> Vec<Vec<(f64, f64)>> {
    let r = sw / 2.0;
    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    for sub in subs {
        let pts: Vec<(f64, f64)> = sub.pts.iter().map(|&(x, y)| m.apply(x, y)).collect();
        if pts.len() < 2 {
            continue;
        }
        let nseg = if sub.closed { pts.len() } else { pts.len() - 1 };
        for s in 0..nseg {
            let (x0, y0) = pts[s];
            let (x1, y1) = pts[(s + 1) % pts.len()];
            let (dx, dy) = (x1 - x0, y1 - y0);
            let len = dx.hypot(dy);
            if len < 1e-9 {
                continue;
            }
            let (nx, ny) = (-dy / len * r, dx / len * r);
            out.push(vec![
                (x0 + nx, y0 + ny),
                (x1 + nx, y1 + ny),
                (x1 - nx, y1 - ny),
                (x0 - nx, y0 - ny),
            ]);
        }
        // round joins: every vertex (closed) or interior vertices (open)
        if r >= 0.75 {
            let jrange: Vec<usize> = if sub.closed {
                (0..pts.len()).collect()
            } else {
                (1..pts.len().saturating_sub(1)).collect()
            };
            for j in jrange {
                let (cx, cy) = pts[j];
                out.push(ellipse_poly(cx, cy, r, r));
            }
        }
    }
    out
}

// ---------- raster core ----------

/// Scanline fill (nonzero winding or even-odd) of closed polygons.
fn fill_scan(mask: &mut [u8], w2: usize, h2: usize, polys: &[Vec<(f64, f64)>], evenodd: bool) {
    let mut xs: Vec<(f64, i32)> = Vec::new();
    for y in 0..h2 {
        let yy = y as f64 + 0.5;
        xs.clear();
        for poly in polys {
            let n = poly.len();
            for k in 0..n {
                let (x0, y0) = poly[k];
                let (x1, y1) = poly[(k + 1) % n];
                if (y0 <= yy && y1 > yy) || (y1 <= yy && y0 > yy) {
                    let t = (yy - y0) / (y1 - y0);
                    xs.push((x0 + t * (x1 - x0), if y1 > y0 { 1 } else { -1 }));
                }
            }
        }
        if xs.is_empty() {
            continue;
        }
        xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let row = &mut mask[y * w2..(y + 1) * w2];
        let mut wind = 0i32;
        for pair in xs.windows(2) {
            wind += pair[0].1;
            let inside = if evenodd { wind & 1 != 0 } else { wind != 0 };
            if inside {
                let x0 = (pair[0].0.max(0.0) as usize).min(w2);
                let x1 = (pair[1].0.ceil().max(0.0) as usize).min(w2);
                if x0 < x1 {
                    row[x0..x1].fill(255);
                }
            }
        }
    }
}

/// Source-over composite the 2x2 supersampled mask onto the image.
fn comp(img: &mut [u32], mask: &[u8], w: u32, h: u32, col: u32) {
    let (w, h) = (w as usize, h as usize);
    let w2 = w * SS;
    let (sr, sg, sb) = ((col >> 16) & 255, (col >> 8) & 255, col & 255);
    let sa = col >> 24;
    for y in 0..h {
        for x in 0..w {
            let m = mask[(2 * y) * w2 + 2 * x] as u32
                + mask[(2 * y) * w2 + 2 * x + 1] as u32
                + mask[(2 * y + 1) * w2 + 2 * x] as u32
                + mask[(2 * y + 1) * w2 + 2 * x + 1] as u32; // 0..1020
            if m == 0 {
                continue;
            }
            let a = sa * m / 1020; // effective alpha 0..255
            let i = y * w + x;
            let d = img[i];
            let (dr, dg, db, da) = ((d >> 16) & 255, (d >> 8) & 255, d & 255, d >> 24);
            let inv = 255 - a;
            let r = (sr * a + dr * inv) / 255;
            let g = (sg * a + dg * inv) / 255;
            let b = (sb * a + db * inv) / 255;
            let oa = (a + da * inv / 255).min(255);
            img[i] = oa << 24 | r << 16 | g << 8 | b;
        }
    }
}

fn scale_alpha(col: u32, k: u32) -> u32 {
    let a = ((col >> 24) & 255) * k.min(255) / 255;
    a << 24 | col & 0x00ff_ffff
}

// ---------- gradients (flat midpoint — shortcut) ----------

/// `id → average of first/last stop color` for linear/radial gradients.
fn harvest_gradients(txt: &str) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    let mut i = 0usize;
    while let Some(tag) = next_tag(txt, &mut i) {
        if tag.closing || !(tag.name == "linearGradient" || tag.name == "radialGradient") {
            continue;
        }
        let Some(id) = get_attr(tag.attrs, "id").map(|s| s.to_string()) else { continue };
        let mut stops: Vec<u32> = Vec::new();
        if !tag.selfclose {
            let mut inner = i;
            while let Some(t2) = next_tag(txt, &mut inner) {
                if t2.closing {
                    break;
                }
                if t2.name == "stop" {
                    let raw = t2.attrs;
                    let col = get_style(raw, "stop-color")
                        .or_else(|| get_attr(raw, "stop-color"))
                        .and_then(parse_color);
                    if let Some(c) = col {
                        stops.push(c);
                    }
                }
            }
            i = inner;
        }
        if let (Some(&f), Some(&l)) = (stops.first(), stops.last()) {
            let ch = |sh: u32| (((f >> sh) & 255) + ((l >> sh) & 255)) / 2;
            out.push((id, ch(24) << 24 | ch(16) << 16 | ch(8) << 8 | ch(0)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect x="2" y="2" width="6" height="6" fill="red"/></svg>"#;

    #[test]
    fn probe_width_height_and_viewbox() {
        assert_eq!(probe(SVG.as_bytes()), Some((10, 10)));
        let vb = br#"<svg viewBox="0 0 40 20"><path d="M0 0h40v20H0z"/></svg>"#;
        assert_eq!(probe(vb), Some((40, 20)));
        assert_eq!(probe(b"<html><body>nope"), None);
        assert_eq!(probe(b""), None);
    }

    #[test]
    fn unparseable_payload_renders_none_never_text() {
        // Pins the svg pipeline's contract: garbage / non-svg payload (e.g.
        // an html RAW_TEXT blob misdelivered to the image path) must render
        // None (blank box in layout) - never a fallback that draws the raw
        // markup source as pixels.
        assert!(render(b"garbage without any root tag", 24, 24).is_none());
        assert!(render(b"<html><body>not svg</body></html>", 24, 24).is_none());
        assert!(render(b"<svg unterminated", 24, 24).is_none());
    }

    #[test]
    fn rect_fills_center_leaves_corners() {
        let img = render(SVG.as_bytes(), 10, 10).unwrap();
        let at = |x: usize, y: usize| img.px[y * 10 + x];
        assert_eq!(at(5, 5), 0xffff_0000); // red center
        assert_eq!(at(0, 0), 0); // untouched corner
    }

    #[test]
    fn intrinsic_size_scales_when_no_viewbox() {
        // An SVG with width/height but no viewBox has a fixed user coordinate
        // system of that intrinsic size. Rendering at a smaller or larger
        // raster size must scale the whole drawing, not map only the top-left
        // w×h user units (which clipped the logo at 80px).
        let s = br#"<svg width="100" height="100"><rect width="100" height="100" fill="black"/></svg>"#;
        for &n in &[50u32, 100, 200] {
            let img = render(s, n, n).expect("renders");
            assert_eq!((img.w, img.h), (n, n));
            assert!(
                img.px.iter().all(|&p| p == 0xff00_0000),
                "full-bleed rect must cover {n}x{n}"
            );
        }
    }

    #[test]
    fn evenodd_hole_and_none_fill() {
        // two stacked squares, even-odd → middle is a hole
        let s = br#"<svg width="10" height="10" fill-rule="evenodd"><path d="M0 0h10v10H0z M2 2h6v6H2z" fill="black"/></svg>"#;
        let img = render(s, 10, 10).unwrap();
        assert_eq!(img.px[5 * 10 + 5], 0); // hole
        assert_eq!(img.px[1 * 10 + 1], 0xff00_0000); // ring
        let n = br#"<svg width="10" height="10"><rect width="10" height="10" fill="none"/></svg>"#;
        assert!(render(n, 10, 10).unwrap().px.iter().all(|&p| p == 0));
    }

    #[test]
    fn transform_translate_moves_shape() {
        let s = br#"<svg width="10" height="10"><g transform="translate(5,5)"><rect width="4" height="4" fill="black"/></g></svg>"#;
        let img = render(s, 10, 10).unwrap();
        assert_eq!(img.px[0], 0);
        assert_eq!(img.px[7 * 10 + 7], 0xff00_0000);
    }

    #[test]
    fn circle_and_stroke_draw() {
        let s = br#"<svg width="10" height="10"><circle cx="5" cy="5" r="3" fill="none" stroke="black" stroke-width="2"/></svg>"#;
        let img = render(s, 10, 10).unwrap();
        assert_eq!(img.px[5 * 10 + 5], 0); // hollow center
        assert_ne!(img.px[2 * 10 + 5], 0); // ring top
        assert_eq!(img.px[0], 0); // outside
    }

    #[test]
    fn gradient_collapses_to_mid_stop() {
        let s = br#"<svg width="4" height="4"><defs><linearGradient id="g"><stop stop-color="black"/><stop stop-color="white"/></linearGradient></defs><rect width="4" height="4" fill="url(#g)"/></svg>"#;
        let img = render(s, 4, 4).unwrap();
        let p = img.px[0];
        let (r, g, b) = ((p >> 16) & 255, (p >> 8) & 255, p & 255);
        assert_eq!((r, g, b), (127, 127, 127)); // midpoint gray
    }

    #[test]
    fn path_relative_and_arc_commands() {
        // arc-heavy: near-full-circle with A commands must not panic and must fill
        let s = br#"<svg width="20" height="20"><path d="M10 2 a8 8 0 1 0 0.01 0 z" fill="black"/></svg>"#;
        let img = render(s, 20, 20).unwrap();
        assert_eq!(img.px[10 * 20 + 10], 0xff00_0000);
        assert_eq!(img.px[1], 0);
    }

    #[test]
    fn style_attr_and_inherited_fill() {
        // root fill=none inherits to the rect (must stay invisible), inline style wins
        let s = br#"<svg width="6" height="6" fill="none"><rect width="6" height="6" style="fill:#00ff00"/></svg>"#;
        let img = render(s, 6, 6).unwrap();
        assert_eq!(img.px[0], 0xff00_ff00);
        let s2 = br#"<svg width="6" height="6" fill="none"><rect width="6" height="6"/></svg>"#;
        assert!(render(s2, 6, 6).unwrap().px.iter().all(|&p| p == 0));
    }

    #[test]
    fn clip_path_limits_the_painted_area() {
        let s = br##"<svg width="10" height="10"><defs><clipPath id="c"><rect width="5" height="5"/></clipPath></defs><rect width="10" height="10" fill="black" clip-path="url(#c)"/></svg>"##;
        let img = render(s, 10, 10).unwrap();
        assert_eq!(img.px[2 * 10 + 2], 0xff00_0000);
        assert_eq!(img.px[8 * 10 + 8], 0, "outside the clip stays transparent");
    }

    #[test]
    fn mask_white_region_limits_the_painted_area() {
        let s = br##"<svg width="10" height="10"><defs><mask id="m"><rect width="5" height="10" fill="white"/></mask></defs><rect width="10" height="10" fill="black" mask="url(#m)"/></svg>"##;
        let img = render(s, 10, 10).unwrap();
        assert_eq!(img.px[2 * 10 + 2], 0xff00_0000);
        assert_eq!(img.px[2 * 10 + 8], 0, "mask hole stays transparent");
    }

    #[test]
    fn mask_inherits_fill_from_the_mask_element() {
        let s = br##"<svg width="10" height="10"><defs><mask id="m" fill="white"><rect width="5" height="10"/></mask></defs><rect width="10" height="10" fill="black" mask="url(#m)"/></svg>"##;
        let img = render(s, 10, 10).unwrap();
        assert_eq!(img.px[2 * 10 + 2], 0xff00_0000);
        assert_eq!(img.px[2 * 10 + 8], 0);
    }

    #[test]
    fn pattern_tiles_the_fill() {
        let s = br##"<svg width="4" height="4"><defs><pattern id="p" width="2" height="2" patternUnits="userSpaceOnUse"><rect width="1" height="1" fill="red"/></pattern></defs><rect width="4" height="4" fill="url(#p)"/></svg>"##;
        let img = render(s, 4, 4).unwrap();
        assert_eq!(img.px[0], 0xffff_0000);
        assert_eq!(img.px[1], 0, "gap inside the tile stays transparent");
        assert_eq!(img.px[2], 0xffff_0000, "pattern repeats horizontally");
        assert_eq!(img.px[2 * 4 + 2], 0xffff_0000, "pattern repeats vertically");
    }

    #[test]
    fn use_offsets_a_referenced_shape() {
        let s = br##"<svg width="10" height="10"><defs><rect id="r" width="3" height="3" fill="blue"/></defs><use href="#r" x="5"/></svg>"##;
        let img = render(s, 10, 10).unwrap();
        assert_eq!(img.px[1 * 10 + 6], 0xff00_00ff);
        assert_eq!(img.px[1 * 10 + 1], 0, "the definition itself is not drawn");
    }

    #[test]
    fn use_symbol_scales_its_viewbox() {
        let s = br##"<svg width="10" height="10"><defs><symbol id="s" viewBox="0 0 10 10"><rect width="10" height="10" fill="green"/></symbol></defs><use href="#s" width="5" height="5"/></svg>"##;
        let img = render(s, 10, 10).unwrap();
        assert_eq!(img.px[2 * 10 + 2], 0xff00_ff00);
        assert_eq!(img.px[7 * 10 + 7], 0, "symbol paints only its viewport");
    }

    #[test]
    fn fe_gaussian_blur_spreads_ink_outside_the_shape() {
        let s = br##"<svg width="10" height="10"><defs><filter id="f"><feGaussianBlur stdDeviation="1"/></filter></defs><rect x="4" y="4" width="2" height="2" fill="black" filter="url(#f)"/></svg>"##;
        let img = render(s, 10, 10).unwrap();
        assert_ne!(img.px[5 * 10 + 5] >> 24, 0, "center keeps alpha");
        assert_ne!(img.px[5 * 10 + 3], 0, "blur reaches outside the source rect");
    }

    #[test]
    fn nested_svg_gets_its_own_viewport() {
        let s = br##"<svg width="20" height="20"><svg x="10" y="10" width="10" height="10"><rect width="10" height="10" fill="black"/></svg></svg>"##;
        let img = render(s, 20, 20).unwrap();
        assert_eq!(img.px[15 * 20 + 15], 0xff00_0000);
        assert_eq!(img.px[2 * 20 + 2], 0, "nested content is confined to its viewport");
    }

#[test]
    fn text_run_position_size_colour_and_anchor() {
        let s = br##"<svg width="100" height="40" font-size="20"><text x="10" y="30" fill="#ff0000" text-anchor="middle">Hi &amp; bye</text></svg>"##;
        let runs = text_runs(s, 100, 40);
        assert_eq!(runs.len(), 1, "one run");
        assert_eq!(runs[0].text, "Hi & bye");
        assert!((runs[0].x - 10.0).abs() < 0.01, "x={}", runs[0].x);
        assert!((runs[0].y - 30.0).abs() < 0.01, "y={}", runs[0].y);
        assert!((runs[0].size - 20.0).abs() < 0.01, "size={}", runs[0].size);
        assert_eq!(runs[0].color, 0x00ff_0000);
        assert_eq!(runs[0].anchor, 1);
        assert!(runs[0].path.is_none());
        // no <text> ink in the vector pass — the shell paints it
        let img = render(s, 100, 40).unwrap();
        assert!(img.px.iter().all(|&p| p == 0));
    }

    #[test]
    fn text_path_run_carries_polyline_text_length_and_dy() {
        let s = br##"<svg width="20" height="20"><defs><path id="c" d="M2 10h16"/></defs><text dy="3" textLength="16"><textPath href="#c">abc</textPath></text></svg>"##;
        let runs = text_runs(s, 20, 20);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "abc");
        let p = runs[0].path.as_ref().expect("path");
        assert_eq!(p.len(), 2);
        assert!((p[0].0 - 2.0).abs() < 0.01 && (p[0].1 - 10.0).abs() < 0.01);
        assert!((runs[0].text_len.unwrap() - 16.0).abs() < 0.01);
        assert!((runs[0].dy - 3.0).abs() < 0.01);
    }

    #[test]
    fn text_tspans_flatten_and_whitespace_collapses_nbsp_survives() {
        let s = "<svg width=\"10\" height=\"10\"><text>  a<tspan>/</tspan>\n\n b&nbsp;c  </text></svg>".as_bytes();
        let runs = text_runs(s, 10, 10);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "a/ b\u{a0}c");
    }

    #[test]
    fn text_inside_defs_is_not_a_run() {
        let s = br#"<svg width="10" height="10"><defs><text x="1" y="1">hidden</text></defs></svg>"#;
        assert!(text_runs(s, 10, 10).is_empty());
    }

}
