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
    let (_, root_attrs) = root_tag(txt)?;
    let vb = get_attr(root_attrs, "viewBox")
        .and_then(nums4)
        .unwrap_or([0.0, 0.0, w as f64, h as f64]);
    let (vw, vh) = (vb[2].max(1e-6), vb[3].max(1e-6));
    let scale = (w as f64 / vw).min(h as f64 / vh); // xMidYMid meet
    let k = SS as f64; // everything below works in supersampled space
    let base = M {
        a: scale * k,
        b: 0.0,
        c: 0.0,
        d: scale * k,
        e: ((w as f64 - vw * scale) / 2.0 - vb[0] * scale) * k,
        f: ((h as f64 - vh * scale) / 2.0 - vb[1] * scale) * k,
    };

    let grads = harvest_gradients(txt);
    let (w2, h2) = (w as usize * SS, h as usize * SS);
    let mut img = vec![0u32; (w as usize) * (h as usize)];
    let mut mask = vec![0u8; w2 * h2];

    // per-open-tag stacks (matrix, style); balanced by the matching close
    let mut mats = vec![base];
    let mut styles = vec![Style::default()];
    let mut depth = 0usize;
    let mut skip_at: Option<usize> = None; // inside defs/clip/text/…: don't draw
    let mut i = 0usize;

    while let Some(tag) = next_tag(txt, &mut i) {
        if tag.closing {
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
            continue; // already inside a skip container (stack still pushed)
        }
        if SKIP.contains(&tag.name) {
            if !tag.selfclose {
                skip_at = Some(depth);
            }
            continue;
        }

        // paint the shape (shapes have no drawn children either way)
        let st = st.effective();
        let mut fill_polys: Vec<Vec<(f64, f64)>> = Vec::new();
        let mut stroke_subs: Option<Vec<Sub>> = None;
        match tag.name {
            "path" => {
                let Some(d) = get_attr(tag.attrs, "d") else { continue };
                let subs = parse_path(d);
                stroke_subs = Some(subs.clone());
                fill_polys = subs.iter().filter(|s| s.pts.len() >= 3).map(|s| s.pts.clone()).collect();
            }
            "rect" => {
                let x = attrf(tag.attrs, "x");
                let y = attrf(tag.attrs, "y");
                let rw = attrf(tag.attrs, "width");
                let rh = attrf(tag.attrs, "height");
                if rw <= 0.0 || rh <= 0.0 {
                    continue;
                }
                let rx = get_attr(tag.attrs, "rx").and_then(parse_px).unwrap_or(0.0).min(rw / 2.0);
                let ry = get_attr(tag.attrs, "ry").and_then(parse_px).unwrap_or(rx).min(rh / 2.0);
                fill_polys = vec![rect_poly(x, y, rw, rh, rx, ry)];
                stroke_subs = Some(vec![Sub { pts: fill_polys[0].clone(), closed: true }]);
            }
            "circle" | "ellipse" => {
                let cx = attrf(tag.attrs, "cx");
                let cy = attrf(tag.attrs, "cy");
                let rx = if tag.name == "circle" { attrf(tag.attrs, "r") } else { attrf(tag.attrs, "rx") };
                let ry = if tag.name == "circle" { attrf(tag.attrs, "r") } else { attrf(tag.attrs, "ry") };
                if rx <= 0.0 || ry <= 0.0 {
                    continue;
                }
                fill_polys = vec![ellipse_poly(cx, cy, rx, ry)];
                stroke_subs = Some(vec![Sub { pts: fill_polys[0].clone(), closed: true }]);
            }
            "polygon" | "polyline" => {
                let nums = get_attr(tag.attrs, "points").map(nums).unwrap_or_default();
                let pts: Vec<(f64, f64)> = nums.chunks_exact(2).map(|c| (c[0], c[1])).collect();
                if pts.len() < 2 {
                    continue;
                }
                if tag.name == "polygon" && pts.len() >= 3 {
                    fill_polys = vec![pts.clone()];
                }
                stroke_subs = Some(vec![Sub { pts, closed: tag.name == "polygon" }]);
            }
            "line" => {
                let p = vec![
                    (attrf(tag.attrs, "x1"), attrf(tag.attrs, "y1")),
                    (attrf(tag.attrs, "x2"), attrf(tag.attrs, "y2")),
                ];
                stroke_subs = Some(vec![Sub { pts: p, closed: false }]);
            }
            _ => continue, // <g>/<svg>/<a>/unknown: container only
        }

        if let Some(col) = st.fill.resolve(&grads) {
            let col = scale_alpha(col, (st.fill_opacity * 255.0).round() as u32);
            let polys: Vec<Vec<(f64, f64)>> = fill_polys
                .iter()
                .map(|p| p.iter().map(|&(x, y)| m.apply(x, y)).collect())
                .collect();
            if !polys.is_empty() {
                mask.fill(0);
                fill_scan(&mut mask, w2, h2, &polys, st.evenodd);
                comp(&mut img, &mask, w, h, col);
            }
        }
        if let (Some(col), Some(subs)) = (st.stroke.resolve(&grads), stroke_subs) {
            // matrix already maps to supersampled space → hypot gives super px
            let sw = st.sw * (m.a.hypot(m.b) + m.c.hypot(m.d)) / 2.0;
            if sw > 0.0 {
                let polys = stroke_polys(&subs, m, sw);
                if !polys.is_empty() {
                    let col = scale_alpha(col, (st.stroke_opacity * 255.0).round() as u32);
                    mask.fill(0);
                    fill_scan(&mut mask, w2, h2, &polys, false);
                    comp(&mut img, &mask, w, h, col);
                }
            }
        }
    }

    Some(ImageData { w, h, px: img })
}

const SKIP: &[&str] = &[
    "defs", "clipPath", "mask", "filter", "pattern", "marker", "symbol", "text", "tspan",
    "style", "title", "desc", "metadata", "linearGradient", "radialGradient", "script",
    "image", "use",
];

// ---------- xml scanning (shortcut: not namespace-aware) ----------

struct Tag<'a> {
    name: &'a str,
    attrs: &'a str,
    selfclose: bool,
    closing: bool,
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
        return Some(Tag { name, attrs: attrs.trim(), selfclose, closing });
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
        }
    }
}

impl Style {
    /// Inherit + apply presentation attributes (inline style wins over them).
    fn with(&self, raw: &str) -> Style {
        let mut s = self.clone();
        for key in ["fill", "stroke", "stroke-width", "fill-rule", "fill-opacity", "stroke-opacity", "opacity"] {
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
    fn rect_fills_center_leaves_corners() {
        let img = render(SVG.as_bytes(), 10, 10).unwrap();
        let at = |x: usize, y: usize| img.px[y * 10 + x];
        assert_eq!(at(5, 5), 0xffff_0000); // red center
        assert_eq!(at(0, 0), 0); // untouched corner
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
}
