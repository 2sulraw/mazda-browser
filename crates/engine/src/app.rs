//! Mazda — fetch → tokenize → arena DOM → word-wrap layout → render.
//! Phase S: this module still holds the shell + the paint pipeline; the
//! headless renderer reuses it through [`run_headless`].
use crate::{cookie, css, html, image, js, layout, net, runtime, svg, ui_chrome, woff};

use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont};
use eframe::egui;
use layout::{Fam, Layout, Style};
use runtime::{Conf, EngineChoice, FontsChoice, ScriptRuntime};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const PADDING: usize = 12;
const BG: u32 = 0x00_ff_ff_ff; // UA default canvas = white (matches every browser)
/// Display pixels of decoded images kept in RAM per page (~4B/px RGBA).
/// Charged at decode time against laid-out size (what's retained), not
/// intrinsic size — over-budget images keep their probed dims (space
/// reserved, nothing drawn).
/// shortcut: first-come document-order cap, not LRU — pages rarely exceed it.
const IMG_PIXEL_BUDGET: usize = 8_000_000;

/// Decoded-pixel memo for RELAYOUTS. Every relayout (a timer mutating the
/// DOM, a hover, an event) rebuilds the page from scratch, so without a memo
/// the same 300-odd `<img>`s are fetched, decoded, downscaled and cropped
/// again — that was both the runaway relayout time and the 104 → 118 MB
/// working-set climb on a single Wikipedia article. Keyed by the exact decode
/// inputs, so a changed size or crop misses and decodes normally.
/// Lives inside `KeptPage`, so a navigation drops the whole memo.
/// Cap stays under `IMG_PIXEL_BUDGET`: the memo can hold one page's images,
/// no more. Cycles (not recency) eviction would need a real LRU; the oldest
/// insert is dropped instead and re-decoded if it is needed again.
#[derive(Default)]
struct ImgCache {
    map: std::collections::HashMap<ImgKey, (Cached, u64)>,
    /// cached display pixels (4 B each) — the memo's own RAM accounting
    px: usize,
    tick: u64,
    /// images actually decoded (a cache miss) — the relayout log reports it
    decodes: usize,
}
const IMG_CACHE_PIXELS: usize = 6_000_000;

/// Everything a decoded slot depends on. `src` is a hash: a `data:` URL can be
/// megabytes and the real key would be copied per lookup.
/// shortcut: a 64-bit FNV hash of the source; two DIFFERENT sources colliding
/// would paint the wrong picture, which needs ~1e-19 odds between two images
/// on one page — the same bet the HTTP disk cache already makes.
#[derive(PartialEq, Eq, Hash, Clone)]
struct ImgKey {
    src: u64,
    dw: u32,
    dh: u32,
    /// mask tint: the enclosing box's background color, None = untinted
    tint: Option<u32>,
    /// object-fit content rect, as raw f32 bits (exact compare, no rounding)
    fit: [u32; 4],
    /// object-fit visible window, if the image overflows its box
    clip: Option<[u32; 4]>,
}

/// A cached slot: the pixels to draw (None = wholly clipped away) and where.
type Cached = Option<(Rc<image::ImageData>, (f32, f32, f32, f32))>;

fn px_of(c: &Cached) -> usize {
    c.as_ref().map_or(0, |(img, _)| img.px.len())
}

impl ImgCache {
    fn get(&mut self, k: &ImgKey) -> Option<Cached> {
        self.tick += 1;
        let tick = self.tick;
        let e = self.map.get_mut(k)?;
        e.1 = tick; // recency, so a hot slot survives eviction
        Some(e.0.clone())
    }

    fn put(&mut self, k: ImgKey, v: Cached) {
        self.tick += 1;
        self.decodes += 1;
        if let Some((old, _)) = self.map.insert(k, (v.clone(), self.tick)) {
            self.px -= px_of(&old);
        }
        self.px += px_of(&v);
        // evict oldest inserts until the memo fits its cap
        while self.px > IMG_CACHE_PIXELS {
            let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
            else {
                return;
            };
            if let Some((old, _)) = self.map.remove(&oldest) {
                self.px -= px_of(&old);
            }
        }
    }
}

/// A rendered page: enough to draw it and hit-test its links.
struct Page {
    url: String,
    laid: Layout,
    /// playable `<video>` elements: draw rect, local file, and which
    /// `laid.images` entry holds the frame the renderer keeps replacing
    media: Vec<layout::MediaEntry>,
    /// `<a download>` targets: the renderer saves these instead of navigating
    downloads: Vec<layout::DownloadLink>,
    /// page background from body/html background-color (fallback: BG const)
    bg: u32,
    /// inspector metadata by DOM node (built worker-side — the UI thread
    /// has no DOM). Only elements that are hit-test targets get an entry.
    elements: HashMap<u32, ElementInfo>,
}

/// What the element inspector shows about one element. The DOM lives on the
/// worker thread, so its identity travels with the page: tag/id/classes, an
/// ancestor path, the border box, a few attributes and a direct-text sample.
#[derive(Clone, Debug, Default, PartialEq)]
struct ElementInfo {
    tag: String,
    id: String,
    classes: Vec<String>,
    /// `html > body > div#app > p.lead`, root first
    path: String,
    /// border box in document coordinates (viewport ones when `fixed`)
    rect: (f32, f32, f32, f32),
    fixed: bool,
    /// a short attribute sample (`href`, `src`, …) — id/class already shown
    attrs: Vec<(String, String)>,
    /// collapsed direct text content of the element (empty for containers)
    text: String,
    /// when this element sits inside a `<label>` that names a form control,
    /// that control's DOM node: a click on the label's text activates the
    /// control exactly like a click on the control itself. None elsewhere.
    label_for: Option<u32>,
}

impl ElementInfo {
    /// Compact `tag#id.class` form used by the highlight chip.
    fn selector(&self) -> String {
        let mut s = self.tag.clone();
        if !self.id.is_empty() {
            s.push('#');
            s.push_str(&self.id);
        }
        for c in &self.classes {
            s.push('.');
            s.push_str(c);
        }
        s
    }
}

/// The inspector is limited to this many characters per metadata string so a
/// pathological page can't balloon the worker→UI payload.
const ELEMENT_TEXT_MAX: usize = 100;
const ELEMENT_ATTR_MAX: usize = 5;
const ELEMENT_VALUE_MAX: usize = 80;
const ELEMENT_PATH_DEPTH: usize = 6;

/// DOM-derived metadata for the inspector, one entry per hit-test node.
/// `targets` is in tree order, so an element's own box (pushed before its
/// text runs) supplies its rect.
fn collect_elements(
    dom: &html::Dom,
    targets: &[layout::HoverTarget],
) -> HashMap<u32, ElementInfo> {
    let timing = std::env::var_os("MAZDA_TIME").is_some();
    let mut t_info = std::time::Duration::ZERO;
    let mut t_label = std::time::Duration::ZERO;
    let mut out: HashMap<u32, ElementInfo> = HashMap::new();
    // label node -> control it activates, resolved once per page wire
    let mut labelled: HashMap<u32, Option<u32>> = HashMap::new();
    for t in targets {
        if out.contains_key(&t.node) || dom.tag(t.node) == "#text" {
            continue;
        }
        let t0 = Instant::now();
        let mut info = element_info(dom, t.node, t.rect, t.fixed);
        t_info += t0.elapsed();
        let t0 = Instant::now();
        if let Some(label) = label_ancestor(dom, t.node) {
            info.label_for = *labelled
                .entry(label)
                .or_insert_with(|| label_control(dom, label));
        }
        t_label += t0.elapsed();
        out.insert(t.node, info);
    }
    if timing {
        eprintln!(
            "[time] elements detail: {} targets, {} unique, info {:.0} ms, label {:.0} ms",
            targets.len(),
            out.len(),
            t_info.as_secs_f64() * 1000.0,
            t_label.as_secs_f64() * 1000.0
        );
    }
    out
}

/// Nearest `<label>` ancestor of `node` (the node itself counts).
fn label_ancestor(dom: &html::Dom, node: u32) -> Option<u32> {
    let mut cur = node;
    loop {
        if dom.tag(cur) == "label" {
            return Some(cur);
        }
        if cur == 0 {
            return None;
        }
        cur = dom.nodes[cur as usize].parent;
    }
}

/// The control a `<label>` activates: its `for=` target when that id names a
/// labelable element, else the first labelable descendant in tree order.
fn label_control(dom: &html::Dom, label: u32) -> Option<u32> {
    fn labelable(dom: &html::Dom, n: u32) -> bool {
        matches!(dom.tag(n), "input" | "select" | "textarea" | "button")
    }
    if let Some(id) = dom.attr(label, "for").map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(n) = (1..dom.nodes.len() as u32).find(|&n| {
            labelable(dom, n) && dom.attr(n, "id") == Some(id)
        }) {
            return Some(n);
        }
    }
    let mut stack: Vec<u32> = dom.nodes[label as usize].children.iter().rev().copied().collect();
    while let Some(n) = stack.pop() {
        if labelable(dom, n) {
            return Some(n);
        }
        for &c in dom.nodes[n as usize].children.iter().rev() {
            stack.push(c);
        }
    }
    None
}

fn element_info(
    dom: &html::Dom,
    node: u32,
    rect: (f32, f32, f32, f32),
    fixed: bool,
) -> ElementInfo {
    let id = dom.attr(node, "id").unwrap_or("").trim().to_string();
    let classes: Vec<String> = dom
        .attr(node, "class")
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut attrs = Vec::new();
    if let html::NodeKind::Element(_, list) = &dom.nodes[node as usize].kind {
        for (k, v) in list.iter() {
            if attrs.len() >= ELEMENT_ATTR_MAX {
                break;
            }
            if k.eq_ignore_ascii_case("id") || k.eq_ignore_ascii_case("class") {
                continue;
            }
            attrs.push((k.clone(), truncate_chars(v, ELEMENT_VALUE_MAX)));
        }
    }
    // direct text children only: cheap even on huge container elements
    let mut text = String::new();
    for &c in &dom.nodes[node as usize].children {
        if let html::NodeKind::Text(t) = &dom.nodes[c as usize].kind {
            for w in t.split_whitespace() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(w);
                if text.chars().count() >= ELEMENT_TEXT_MAX {
                    break;
                }
            }
        }
        if text.chars().count() >= ELEMENT_TEXT_MAX {
            break;
        }
    }
    ElementInfo {
        tag: dom.tag(node).to_string(),
        id,
        classes,
        path: element_path(dom, node),
        rect,
        fixed,
        attrs,
        text: truncate_chars(&text, ELEMENT_TEXT_MAX),
        label_for: None,
    }
}

/// `html > body > div#app > p.lead`, capped at [`ELEMENT_PATH_DEPTH`]
/// segments (the head is elided, the element itself always kept).
fn element_path(dom: &html::Dom, node: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = node;
    while cur != 0 && parts.len() < ELEMENT_PATH_DEPTH {
        let tag = dom.tag(cur);
        if tag == "#document" || tag == "#text" {
            break;
        }
        let mut seg = tag.to_string();
        if let Some(id) = dom.attr(cur, "id").map(str::trim).filter(|s| !s.is_empty()) {
            seg.push('#');
            seg.push_str(id);
        }
        if let Some(cls) = dom.attr(cur, "class") {
            for c in cls.split_whitespace().take(3) {
                seg.push('.');
                seg.push_str(c);
            }
        }
        parts.push(seg);
        cur = dom.nodes[cur as usize].parent;
    }
    if cur != 0 {
        parts.push("\u{2026}".into()); // "…" — the path continues upward
    }
    parts.reverse();
    parts.join(" > ")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// One browser tab. Pages are NOT kept in RAM (that would blow the budget) —
/// a tab stores just its URL + scroll offset, and switching tabs reloads.
/// shortcut: reload-on-switch instead of caching every tab's DOM.
struct Tab {
    url: String,
    scroll: f32,
}

// ---------------------------------------------------------------------------
// Tab store (Phase 3): the tab list lives on disk, so a restart reopens the
// same tabs instead of one blank page. Each tab still loads on its first
// activation and takes the DOM snapshot path when one exists, so a restored
// session paints without a re-fetch.
// ---------------------------------------------------------------------------

/// Format: `active=<index>` then one URL per line, verbatim (no escaping, so
/// a corrupt or over-long line is dropped instead of costing RAM).
const SESSION_MAX_TABS: usize = 64;
const SESSION_MAX_URL: usize = 16 * 1024;

fn session_path() -> std::path::PathBuf {
    cache_dir().join("session")
}

fn encode_session(tabs: &[Tab], active: usize) -> String {
    let mut out = format!("active={active}\n");
    for t in tabs.iter().take(SESSION_MAX_TABS) {
        out.push_str(&t.url);
        out.push('\n');
    }
    out
}

/// `None` = nothing usable on disk (missing, empty, or every line rejected),
/// in which case the caller starts a fresh one-tab session.
fn decode_session(text: &str) -> Option<(Vec<Tab>, usize)> {
    let mut active = 0usize;
    let mut tabs = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(n) = line.strip_prefix("active=") {
            active = n.trim().parse().unwrap_or(0);
            continue;
        }
        let url = line.trim();
        if url.is_empty() || url.len() > SESSION_MAX_URL || tabs.len() == SESSION_MAX_TABS {
            continue;
        }
        tabs.push(Tab {
            url: url.to_string(),
            scroll: 0.0,
        });
    }
    (!tabs.is_empty()).then_some((tabs, active))
}

/// FNV-1a over the URL list + active index: the file is written only when
/// this changes, so idle frames never touch the disk.
fn session_digest(tabs: &[Tab], active: usize) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325 ^ active as u64;
    for t in tabs {
        for &b in t.url.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Window shell: eframe/egui with the glow backend. The page itself is still
// rasterized by render() below into a CPU 0x00RRGGBB buffer and shown as a
// single NEAREST texture; egui owns the window, tab strip and address bar.
// ---------------------------------------------------------------------------

/// Tab-strip (= titlebar) height, logical points.
const TAB_H: f32 = ui_chrome::TAB_H;
/// Address-row height, logical points.
const ADDR_H: f32 = ui_chrome::TOOLBAR_H;

/// Run the full desktop shell (eframe/egui). This is the pre-split `main`.
pub fn run_shell() -> eframe::Result {
    // Borderless shell: the titlebar (tab strip, window controls, drag
    // region) and the toolbar are drawn by ui_chrome; no native frame.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Mazda")
            .with_decorations(false)
            .with_inner_size([1000.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Mazda",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

/// One message in the headless loop: a stdin command or a finished load.
enum HeadlessMsg {
    Cmd(String),
    Done(LoadDone),
}

/// Paint one page into a mapped BGRA frame and announce it on stdout.
/// One find-in-page hit: a slice of one laid-out text run.
#[derive(Clone, Copy)]
struct FindHit {
    /// document coords (viewport coords inside a `position: fixed` subtree)
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fixed: bool,
}

/// Live find-in-page state. Matches are per text run, so a phrase split
/// across runs (or a styled `b` in the middle) is not found — a deliberate
/// shortcut until runs carry a paragraph index.
struct FindState {
    query: String,
    hits: Vec<FindHit>,
    active: usize,
}

impl FindState {
    fn new(query: String, laid: &layout::Layout) -> Self {
        let mut s = Self {
            query,
            hits: Vec::new(),
            active: 0,
        };
        s.rescan(laid);
        s
    }

    /// Rebuild the hit list against the current layout (new page, relayout).
    fn rescan(&mut self, laid: &layout::Layout) {
        self.hits.clear();
        if self.query.is_empty() {
            self.active = 0;
            return;
        }
        let q = self.query.to_lowercase();
        // own a copy so the scan loop does not hold a borrow of `self.query`
        // (the fallback needle below) while `push_run_hit` mutates `self`
        let q_orig = self.query.clone();
        for r in &laid.runs {
            let lower = r.text.to_lowercase();
            // case folding can change byte length (`İ`, `ß`): when it did,
            // scan the original text so offsets stay on char boundaries
            let (hay, needle) = if lower.len() == r.text.len() {
                (&lower, q.as_str())
            } else {
                (&r.text, q_orig.as_str())
            };
            let mut start = 0;
            while let Some(i) = hay[start..].find(needle) {
                let at = start + i;
                self.push_run_hit(r, at, needle.len());
                start = at + needle.len().max(1);
            }
        }
        self.active = if self.hits.is_empty() {
            0
        } else {
            self.active.min(self.hits.len() - 1)
        };
    }

    /// Turn a byte range of run `r` into a rect, sizing x by character share
    /// (per-glyph advances are not kept after layout).
    fn push_run_hit(&mut self, r: &layout::PositionedRun, byte: usize, len: usize) {
        let Some(before) = r.text.get(..byte) else { return };
        let Some(matched) = r.text.get(byte..byte + len.max(1)) else { return };
        let total = r.text.chars().count().max(1);
        let b = before.chars().count();
        let span = matched.chars().count().max(1);
        let f0 = b as f32 / total as f32;
        let f1 = (b + span) as f32 / total as f32;
        self.hits.push(FindHit {
            x: r.x + r.width * f0,
            y: r.y,
            w: (r.width * (f1 - f0)).max(2.0),
            h: r.style.size * 1.25,
            fixed: r.fixed,
        });
    }

    /// 1-based active match (0 = none) for the shell's counter.
    fn active_no(&self) -> u32 {
        if self.hits.is_empty() {
            0
        } else {
            self.active as u32 + 1
        }
    }
}

/// Scroll offset that brings `hit` into view with a margin; never scrolls a
/// `fixed` hit (already on screen).
fn find_scroll(hit: &FindHit, scroll: f32, vh: f32, doc_h: f32) -> f32 {
    let mut s = scroll;
    if !hit.fixed {
        let margin = 40.0;
        if hit.y < s + margin {
            s = (hit.y - margin).max(0.0);
        } else if hit.y + hit.h > s + vh - margin {
            s = (hit.y + hit.h - vh + margin).max(0.0);
        }
    }
    s.clamp(0.0, (doc_h - vh).max(0.0))
}

/// Blend one highlight rect over already-rendered pixels (0x00RRGGBB).
fn blend_rect(
    buf: &mut [u32],
    width: usize,
    height: usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    rgb: (u8, u8, u8),
    alpha: f32,
) {
    let x0 = x.floor().max(0.0) as usize;
    let y0 = y.floor().max(0.0) as usize;
    let x1 = ((x + w).ceil()).max(0.0) as usize;
    let y1 = ((y + h).ceil()).max(0.0) as usize;
    let x1 = x1.min(width);
    let y1 = y1.min(height);
    let mix = |c: u32, n: u8| -> u32 { (c as f32 * (1.0 - alpha) + n as f32 * alpha) as u32 };
    for py in y0..y1 {
        let row = py * width;
        for px in x0..x1 {
            let i = row + px;
            let p = buf[i];
            let r = mix((p >> 16) & 0xff, rgb.0);
            let g = mix((p >> 8) & 0xff, rgb.1);
            let b = mix(p & 0xff, rgb.2);
            buf[i] = (r << 16) | (g << 8) | b;
        }
    }
}

/// Draw every hit (active one orange, the rest yellow) over the painted page.
fn paint_find(
    buf: &mut [u32],
    width: usize,
    height: usize,
    find: &FindState,
    scroll: f32,
) {
    for (i, h) in find.hits.iter().enumerate() {
        let (rgb, alpha) = if i == find.active {
            ((0xff, 0x8a, 0x00), 0.45)
        } else {
            ((0xff, 0xeb, 0x3b), 0.35)
        };
        let y = if h.fixed { h.y } else { h.y - scroll };
        blend_rect(
            buf,
            width,
            height,
            h.x + PADDING as f32,
            y + PADDING as f32,
            h.w,
            h.h,
            rgb,
            alpha,
        );
    }
}

/// Bilinear scale of a 0x00RRGGBB buffer (zoom in/out of the painted page).
fn scale_pixels(src: &[u32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u32> {
    let mut out = vec![0u32; dw * dh];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    let xr = sw as f32 / dw as f32;
    let yr = sh as f32 / dh as f32;
    for y in 0..dh {
        let sy = ((y as f32 + 0.5) * yr - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let fy = sy - y0 as f32;
        for x in 0..dw {
            let sx = ((x as f32 + 0.5) * xr - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let fx = sx - x0 as f32;
            let p00 = src[y0 * sw + x0];
            let p10 = src[y0 * sw + x1];
            let p01 = src[y1 * sw + x0];
            let p11 = src[y1 * sw + x1];
            let mix = |shift: u32| -> u32 {
                let a = ((p00 >> shift) & 0xff) as f32;
                let b = ((p10 >> shift) & 0xff) as f32;
                let c = ((p01 >> shift) & 0xff) as f32;
                let d = ((p11 >> shift) & 0xff) as f32;
                let top = a * (1.0 - fx) + b * fx;
                let bot = c * (1.0 - fx) + d * fx;
                (top * (1.0 - fy) + bot * fy).round() as u32
            };
            out[y * dw + x] = (mix(16) << 16) | (mix(8) << 8) | mix(0);
        }
    }
    out
}

/// A4 at 96 dpi with a half-inch margin (the bitmap pages printed by
/// [`write_print_pdf`]).
const PRINT_PAGE_W: usize = 794;
const PRINT_PAGE_H: usize = 1123;
const PRINT_MARGIN: usize = 48;

/// zlib stream for a PDF `/FlateDecode` image.
fn zlib_bytes(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let mut e =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data)?;
    e.finish()
}

/// Assemble a PDF from one zlib-compressed RGB image per page. The images are
/// drawn to fill the whole A4 MediaBox (the margins are in the bitmap), so the
/// file has no text layer yet - a deliberate shortcut.
fn build_pdf(images: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    // offsets[n] is the byte offset of object n; index 0 is the free head
    let mut offsets: Vec<usize> = vec![0];
    let mut obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]| {
        offsets.push(out.len());
        let n = offsets.len() - 1;
        out.extend_from_slice(format!("{n} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };
    obj(
        &mut out,
        &mut offsets,
        b"<< /Type /Catalog /Pages 2 0 R >>",
    );
    let kids = (0..images.len())
        .map(|i| format!("{} 0 R", 3 + i * 3))
        .collect::<Vec<_>>()
        .join(" ");
    obj(
        &mut out,
        &mut offsets,
        format!(
            "<< /Type /Pages /Kids [{kids}] /Count {} >>",
            images.len()
        )
        .as_bytes(),
    );
    for (i, img) in images.iter().enumerate() {
        let page_no = 3 + i * 3;
        let img_no = page_no + 2;
        obj(
            &mut out,
            &mut offsets,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] \
                 /Resources << /XObject << /Im0 {img_no} 0 R >> >> \
                 /Contents {} 0 R >>",
                page_no + 1
            )
            .as_bytes(),
        );
        let content = b"q 595 0 0 842 0 0 cm /Im0 Do Q";
        let mut body =
            format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        body.extend_from_slice(content);
        body.extend_from_slice(b"\nendstream");
        obj(&mut out, &mut offsets, &body);
        // the image object carries binary data: push it by hand
        offsets.push(out.len());
        let n = offsets.len() - 1;
        let hdr = format!(
            "{n} 0 obj\n<< /Type /XObject /Subtype /Image /Width {PRINT_PAGE_W} \
             /Height {PRINT_PAGE_H} /ColorSpace /DeviceRGB /BitsPerComponent 8 \
             /Filter /FlateDecode /Length {} >>\nstream\n",
            img.len()
        );
        out.extend_from_slice(hdr.as_bytes());
        out.extend_from_slice(img);
        out.extend_from_slice(b"\nendstream\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", offsets.len()).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for &o in &offsets[1..] {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    out
}

/// Render the whole kept document into an A4 PDF in the temp dir.
///
/// Each page is a slice of the document at CSS resolution, scaled to the
/// printable width, composited over white and zlib-compressed. This keeps the
/// print path independent of pagination in layout; the cost is bitmap (not
/// selectable) text and pages split at slice boundaries.
fn write_print_pdf(
    page: &Page,
    fonts: &Fonts,
    pw: usize,
) -> std::io::Result<std::path::PathBuf> {
    let printable_w = PRINT_PAGE_W - 2 * PRINT_MARGIN;
    let printable_h = PRINT_PAGE_H - 2 * PRINT_MARGIN;
    let s = printable_w as f32 / pw.max(1) as f32;
    let chunk = (printable_h as f32 / s).ceil() as usize;
    let total = page.laid.height.ceil().max(1.0) as usize + 2 * PADDING;
    let pages = total.div_ceil(chunk.max(1));
    let mut images = Vec::with_capacity(pages);
    for i in 0..pages {
        let y0 = i * chunk;
        let src_h = chunk.min(total.saturating_sub(y0)).max(1);
        let mut src = vec![page.bg; pw * src_h];
        // fixed elements repeat at each page's top (no compositor here)
        render(
            &mut src,
            pw,
            src_h,
            &page.laid,
            fonts,
            y0 as f32,
            src_h as f32,
            0.0,
        );
        let dst_h = ((src_h as f32 * s).round() as usize)
            .clamp(1, printable_h);
        let scaled = scale_pixels(&src, pw, src_h, printable_w, dst_h);
        let mut pg = vec![0x00_ff_ff_ffu32; PRINT_PAGE_W * PRINT_PAGE_H];
        for y in 0..dst_h {
            let row = &scaled[y * printable_w..(y + 1) * printable_w];
            let off = (PRINT_MARGIN + y) * PRINT_PAGE_W + PRINT_MARGIN;
            pg[off..off + printable_w].copy_from_slice(row);
        }
        let mut rgb = Vec::with_capacity(pg.len() * 3);
        for p in &pg {
            rgb.push((p >> 16) as u8);
            rgb.push((p >> 8) as u8);
            rgb.push(*p as u8);
        }
        images.push(zlib_bytes(&rgb)?);
    }
    let pdf = build_pdf(&images);
    let dir = std::env::temp_dir().join("mazda-print");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("page-{}.pdf", std::process::id()));
    std::fs::write(&path, pdf)?;
    eprintln!(
        "[print] {} pages, {} bytes -> {}",
        images.len(),
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
        path.display()
    );
    Ok(path)
}

fn headless_paint(
    page: &Page,
    fonts: &Fonts,
    pw: usize,
    ph: usize,
    zoom: f32,
    scroll: f32,
    find: Option<&FindState>,
    frame: &mut Option<mazda_ipc::FrameMap>,
    seq: &mut u32,
) {
    // CSS-space buffer, then a bilinear scale to the physical frame size
    let lw = ((pw as f32 / zoom.max(0.01)).round() as usize).max(1);
    let lh = ((ph as f32 / zoom.max(0.01)).round() as usize).max(1);
    let mut pixels = vec![page.bg; lw * lh];
    render(&mut pixels, lw, lh, &page.laid, fonts, scroll, page_h(lh), 0.0);
    if let Some(f) = find {
        paint_find(&mut pixels, lw, lh, f, scroll);
    }
    draw_scrollbar(&mut pixels, lw, lh, page.laid.height, page_h(lh), scroll);
    let pixels = if (lw, lh) == (pw, ph) {
        pixels
    } else {
        scale_pixels(&pixels, lw, lh, pw, ph)
    };
    let path = mazda_ipc::frame_path(&std::env::temp_dir(), std::process::id(), pw, ph);
    if frame.as_ref().map(|f| f.dims() != (pw, ph)).unwrap_or(true) {
        *frame = mazda_ipc::FrameMap::create(&path, pw, ph).ok();
    }
    let Some(f) = frame.as_mut() else { return };
    *seq = seq.wrapping_add(1);
    f.write(*seq, &pixels);
    println!(
        "{}",
        mazda_ipc::Event::Frame {
            path,
            width: pw,
            height: ph,
            doc_height: page.laid.height,
            seq: *seq,
            // the page this frame belongs to: a load that finishes after the
            // user switched tabs still publishes, and the shell must not paint
            // it on the tab that is now active
            url: page.url.clone(),
        }
        .encode()
    );
}

/// Link hit test for the headless renderer (same rule as the shell's
/// `App::link_at`, which needs the whole App).
fn headless_link_at(page: &Page, x: f32, y: f32, scroll: f32) -> Option<String> {
    let run = page.laid.runs.iter().find(|r| {
        let ry = if r.fixed { y } else { y + scroll };
        r.link.is_some()
            && x >= r.x
            && x <= r.x + r.width
            && ry >= r.y
            && ry <= r.y + r.style.size * 1.35
    })?;
    resolve(&page.url, run.link.as_deref()?)
}

/// One live `<video>` on the renderer's current page. The player owns the
/// decoders; the page keeps the draw rect and the `laid.images` index whose
/// pixels are replaced as frames arrive.
struct Playing {
    /// index into `Page::media`
    media: usize,
    player: mazda_media::Player,
    /// display pixel target (the first-frame entry's decoded size)
    w: u32,
    h: u32,
}

/// Open one player (renderer side: decoding happens here, never on the
/// worker). `None` when the container has no image entry or decoding fails.
fn start_player(page: &Page, i: usize) -> Option<Playing> {
    let m = page.media.get(i)?;
    let (w, h) = page
        .laid
        .images
        .get(m.index)
        .map(|e| (e.4.w.max(1), e.4.h.max(1)))?;
    let path = match (&m.path, &m.url) {
        (Some(p), _) => std::path::PathBuf::from(p),
        // `preload="metadata"`: the probe left the body on the network. The
        // click that starts playback is the gesture that fetches it.
        // shortcut: this runs on the UI thread, so the first click on a
        // deferred video waits for the body (GitHub's 1.3 MB clip ~1 s).
        (None, Some(u)) => {
            let t0 = Instant::now();
            let bytes = match fetch(u) {
                Ok((bytes, _)) => bytes,
                Err(e) => {
                    eprintln!("[media] body fetch {}: {e}", log_url(u));
                    return None;
                }
            };
            match media_file(u, &bytes) {
                Ok((p, _)) => {
                    eprintln!(
                        "[media] body {} KB in {:.0} ms on first play",
                        bytes.len() / 1024,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                    p
                }
                Err(e) => {
                    eprintln!("[media] body {}: {e}", log_url(u));
                    return None;
                }
            }
        }
        (None, None) => return None,
    };
    match mazda_media::Player::open(std::path::Path::new(&path)) {
        Ok(mut player) => {
            player.set_looping(m.looping);
            player.set_muted(m.muted);
            Some(Playing {
                media: i,
                player,
                w,
                h,
            })
        }
        Err(e) => {
            eprintln!("[media] renderer open {}: {e}", path.display());
            None
        }
    }
}

/// Players a freshly adopted page starts with: only silent autoplay runs
/// without a gesture, the same rule the big browsers apply. Every other
/// video opens (and plays) on the first click, so a page full of videos
/// costs no decoder memory until it is actually watched.
fn start_players(page: &Page) -> Vec<Playing> {
    let mut out = Vec::new();
    for (i, m) in page.media.iter().enumerate() {
        if !(m.autoplay && m.muted) {
            continue;
        }
        if let Some(mut pl) = start_player(page, i) {
            pl.player.play();
            out.push(pl);
        }
    }
    out
}

/// Find or create the player for media entry `i`, then toggle it.
fn toggle_player(players: &mut Vec<Playing>, page: &Page, i: usize) {
    if let Some(k) = players.iter().position(|pl| pl.media == i) {
        players[k].player.toggle();
        eprintln!(
            "[media] {}",
            if players[k].player.is_playing() {
                "play"
            } else {
                "pause"
            }
        );
        return;
    }
    if let Some(mut pl) = start_player(page, i) {
        pl.player.play();
        eprintln!("[media] play (opened)");
        players.push(pl);
    }
}

/// Messages a download thread sends back to the renderer's main loop.
enum DlMsg {
    Progress { done: u64, total: u64, name: String },
    Done { path: std::path::PathBuf },
    Failed { name: String, err: String },
}

/// Suggested save name: the `download` attribute value when usable, else the
/// URL's last segment. Path separators and control characters become `_` so a
/// hostile page cannot write outside the download dir.
fn download_name(url: &str, suggested: &str) -> String {
    let raw = if suggested.is_empty() {
        url.split(['?', '#']).next().unwrap_or(url).rsplit('/').next().unwrap_or("")
    } else {
        suggested
    };
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    name = name.trim().trim_matches('.').to_string();
    if name.is_empty() {
        name = "download".to_string();
    }
    if name.len() > 120 {
        name.truncate(120);
    }
    name
}

/// Stream `url` into the renderer's temp dir, reporting progress. Runs on its
/// own thread: a 20 MB file must not freeze the page. The shell moves the
/// finished file out of the sandbox (the renderer cannot write the profile).
fn start_download(url: String, name: String, tx: mpsc::Sender<DlMsg>) {
    std::thread::spawn(move || {
        let dir = std::env::temp_dir().join("mazda-downloads");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            let _ = tx.send(DlMsg::Failed { name, err: format!("temp dir: {e}") });
            return;
        }
        let path = dir.join(&name);
        let mut req = http_agent().get(&url);
        for (k, v) in net::request_headers(None) {
            req = req.set(k, &v);
        }
        if let Some(c) = cookie::request_header(&url) {
            req = req.set("Cookie", &c);
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(DlMsg::Failed { name, err: e.to_string() });
                return;
            }
        };
        cookie::store_response(&url, &set_cookie_values(&resp));
        let total = resp
            .header("content-length")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let mut reader = resp.into_reader();
        let mut file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(DlMsg::Failed { name, err: format!("create: {e}") });
                return;
            }
        };
        let _ = tx.send(DlMsg::Progress { done: 0, total, name: name.clone() });
        let mut done = 0u64;
        let mut last = std::time::Instant::now();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = match std::io::Read::read(&mut reader, &mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.send(DlMsg::Failed { name, err: e.to_string() });
                    return;
                }
            };
            if let Err(e) = std::io::Write::write_all(&mut file, &chunk[..n]) {
                let _ = tx.send(DlMsg::Failed { name, err: format!("write: {e}") });
                return;
            }
            done += n as u64;
            if last.elapsed() >= std::time::Duration::from_millis(150) {
                last = std::time::Instant::now();
                let _ = tx.send(DlMsg::Progress {
                    done,
                    total,
                    name: name.clone(),
                });
            }
        }
        if let Err(e) = file.sync_all() {
            let _ = tx.send(DlMsg::Failed { name, err: format!("sync: {e}") });
            return;
        }
        // a final tick so the shell reads 100% even for tiny files
        let _ = tx.send(DlMsg::Progress {
            done,
            total: if total == 0 { done } else { total },
            name: name.clone(),
        });
        let _ = tx.send(DlMsg::Done { path });
    });
}

/// Forward queued download status to the shell; `active` clears on a terminal
/// message so the loop can go back to its idle grace period.
fn drain_downloads(rx: &mpsc::Receiver<DlMsg>, active: &mut bool) {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            DlMsg::Progress { done, total, name } => println!(
                "{}",
                mazda_ipc::Event::Download { done, total, name }.encode()
            ),
            DlMsg::Done { path } => {
                *active = false;
                println!("{}", mazda_ipc::Event::DownloadDone { path }.encode());
            }
            DlMsg::Failed { name, err } => {
                *active = false;
                eprintln!("[download] {name}: {err}");
                println!("{}", mazda_ipc::Event::DownloadFailed { name, err }.encode());
            }
        }
    }
}

fn media_image(frame: mazda_media::Frame, w: u32, h: u32) -> image::ImageData {
    image::downscale(
        &image::ImageData {
            w: frame.w,
            h: frame.h,
            px: frame.px,
        },
        w,
        h,
    )
}

/// Advance every player and repaint when a new frame landed. Returns true
/// when the frame map changed (the caller already painted).
fn tick_players(
    players: &mut [Playing],
    page: &mut Option<Page>,
    fonts: &Fonts,
    pw: usize,
    ph: usize,
    zoom: f32,
    scroll: f32,
    find: Option<&FindState>,
    frame: &mut Option<mazda_ipc::FrameMap>,
    seq: &mut u32,
) -> bool {
    if players.is_empty() {
        return false;
    }
    let Some(p) = page.as_mut() else { return false };
    let mut dirty = false;
    for pl in players.iter_mut() {
        let Some(f) = pl.player.next_frame() else { continue };
        let Some(idx) = p.media.get(pl.media).map(|m| m.index) else { continue };
        if let Some(slot) = p.laid.images.get_mut(idx) {
            slot.4 = Rc::new(media_image(f, pl.w, pl.h));
            dirty = true;
        }
    }
    if dirty {
        headless_paint(p, fonts, pw, ph, zoom, scroll, find, frame, seq);
    }
    dirty
}

/// Phase S headless renderer.
///
/// Loads one page, paints it into a memory-mapped BGRA frame, and answers
/// line commands on stdin. It never initializes eframe: the process exists
/// only to be the sandboxed side of the split.
///
/// Commands: `load <url>`, `resize <w> <h>`, `scroll <y>`, `click <x> <y>`,
/// `find <query>`, `find-step <±1>`, `find-close`, `zoom <factor>`, `print`,
/// `quit`. Events: `frame <path> <w> <h> <doc_h> <seq>`, `title <url>`,
/// `nav <url>`, `find-result <active> <total>`, `scroll-to <y>`,
/// `print-done <path>`, `error <msg>`.
pub fn run_headless() {
    use std::io::BufRead;

    // cold-start accounting: `[time] first paint` / `[time] painted at` are
    // the two numbers a user feels (content on screen vs. load finished)
    let t_start = Instant::now();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut url: Option<String> = None;
    let once = args.iter().any(|a| a == "--once");
    let mut pw = 1000usize;
    let mut ph = 670usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--once" => {}
            "--size" => {
                if let Some((w, h)) = args.get(i + 1).and_then(|s| s.split_once('x')) {
                    pw = w.parse().unwrap_or(pw);
                    ph = h.parse().unwrap_or(ph);
                }
                // skip the value: `1000x670` is not a URL
                i += 1;
            }
            a if a.starts_with("--") => {}
            a => url = Some(a.to_string()),
        }
        i += 1;
    }

    let mut frame: Option<mazda_ipc::FrameMap> = None;
    let mut seq = 0u32;
    let mut fonts = Fonts::load();
    // the settings page can turn site webfonts off; the headless harness uses
    // the same switch (`MAZDA_FONTS=b`) for the reload-soak bisection.
    let fonts_choice = if std::env::var("MAZDA_FONTS").ok().as_deref() == Some("b") {
        FontsChoice::B
    } else {
        FontsChoice::A
    };

    // the same loader thread the shell uses: fetch → DOM → script → layout
    let (jobs_tx, jobs_rx) = mpsc::channel();
    let (res_tx, res_rx) = mpsc::channel();
    std::thread::spawn(move || loader_thread(jobs_rx, res_tx));

    // multiplex stdin commands and loader results onto one channel
    let (tx, rx) = mpsc::channel::<HeadlessMsg>();
    let done_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(done) = res_rx.recv() {
            if done_tx.send(HeadlessMsg::Done(done)).is_err() {
                break;
            }
        }
    });
    // `--once` is the harness mode: no stdin, one frame, then exit
    if !once {
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if tx.send(HeadlessMsg::Cmd(line)).is_err() {
                    return;
                }
            }
            // the shell closed stdin: shut the renderer down with it
            let _ = tx.send(HeadlessMsg::Cmd("quit".to_string()));
        });
    }

    let mut page: Option<Page> = None;
    let mut scroll = 0.0f32;
    let mut nav_id = 0u64;
    let mut prev_counters = crate::js::heap_counters();
    // players for the current page (this thread owns COM/MF; navigation drops
    // them all — files stay in the disk cache)
    let mut players: Vec<Playing> = Vec::new();
    // downloads stream on their own threads; the main loop forwards progress
    let (dl_tx, dl_rx) = mpsc::channel::<DlMsg>();
    let mut dl_active = false;
    // live form state (typed text + picked files) rides every worker job so a
    // relayout never loses it; a navigation resets it
    let mut forms = layout::FormState::default();
    // find-in-page: the query survives navigation, the hit list is rebuilt
    // against each new layout
    let mut find: Option<FindState> = None;
    // page zoom: layout runs at CSS size `physical / zoom`, then the frame is
    // scaled back up; scroll offsets stay in CSS pixels
    let mut zoom = 1.0f32;

    if let Some(url) = url {
        nav_id += 1;
        let _ = jobs_tx.send(LoadReq::Load {
            id: nav_id,
            url,
            max_w: zoomed_w(pw, zoom),
            vh: zoomed_h(ph, zoom),
            engine: EngineChoice::A,
            fonts: fonts_choice,
            restore: false,
        });
    }

    loop {
        // videos need a ~60 Hz tick; downloads report as they arrive; an idle
        // page only wakes to answer stdin
        let wait = if !players.is_empty() {
            std::time::Duration::from_millis(16)
        } else if dl_active {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(500)
        };
        let msg = match rx.recv_timeout(wait) {
            Ok(msg) => msg,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drain_downloads(&dl_rx, &mut dl_active);
                tick_players(
                    &mut players, &mut page, &fonts, pw, ph, zoom, scroll, find.as_ref(),
                    &mut frame, &mut seq,
                );
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            HeadlessMsg::Done(done) => {
                if done.panicked {
                    println!("{}", mazda_ipc::Event::Error("pipeline panic".into()).encode());
                    continue;
                }
                if done.partial {
                    // first paint: markup + stylesheets are laid out, the
                    // page's scripts are still running. Show it now; the
                    // job's final page replaces it when the scripts settle.
                    let Some(mut wire) = done.page else { continue };
                    let stacks = std::mem::take(&mut wire.stacks);
                    let faces = std::mem::take(&mut wire.faces);
                    fonts.begin_page(faces);
                    *fonts.stacks.borrow_mut() = stacks;
                    let p = wire_to_page(wire);
                    scroll = scroll.min((p.laid.height - zoomed_h(ph, zoom)).max(0.0));
                    page = Some(p);
                    if let Some(p) = page.as_ref() {
                        headless_paint(
                            p,
                            &fonts,
                            pw,
                            ph,
                            zoom,
                            scroll,
                            find.as_ref(),
                            &mut frame,
                            &mut seq,
                        );
                    }
                    if std::env::var_os("MAZDA_TIME").is_some() {
                        eprintln!("[time] first paint: {} ms", t_start.elapsed().as_millis());
                    }
                    continue;
                }
                let nav_done = done.nav.is_some() && done.nav_done;
                if let Some(nav) = done.nav {
                    if !done.nav_done {
                        // script navigation: the shell decides whether to load
                        println!("{}", mazda_ipc::Event::Nav(nav).encode());
                        continue;
                    }
                }
                let Some(mut wire) = done.page else { continue };
                // a form submission (or any navigation) starts a fresh
                // document: typed values and picks belong to the old one
                if nav_done {
                    forms = layout::FormState::default();
                }
                // adopt the worker's font state exactly like the shell does
                let stacks = std::mem::take(&mut wire.stacks);
                let faces = std::mem::take(&mut wire.faces);
                fonts.begin_page(faces);
                *fonts.stacks.borrow_mut() = stacks;
                let p = wire_to_page(wire);
                // navigation drops every decoder; the new page's videos open
                // their files (already in the disk cache) on this thread
                players = start_players(&p);
                let max = (p.laid.height - zoomed_h(ph, zoom)).max(0.0);
                scroll = scroll.min(max);
                let title = p.url.clone();
                page = Some(p);
                // a fresh layout invalidates every old rect: rebuild the hits
                // (the query stays, like a browser's find bar across pages)
                if let Some(f) = find.as_mut() {
                    let laid = &page.as_ref().unwrap().laid;
                    f.rescan(laid);
                    println!(
                        "{}",
                        mazda_ipc::Event::FindResult {
                            active: f.active_no(),
                            total: f.hits.len() as u32,
                        }
                        .encode()
                    );
                }
                if let Some(p) = page.as_ref() {
                    headless_paint(p, &fonts, pw, ph, zoom, scroll, find.as_ref(), &mut frame, &mut seq);
                }
                if std::env::var_os("MAZDA_TIME").is_some() {
                    eprintln!("[time] painted at {} ms", t_start.elapsed().as_millis());
                }
                println!("{}", mazda_ipc::Event::Title(title).encode());
                if cfg!(feature = "count-alloc") {
                    // live Rust heap with the page just painted: a slope over
                    // reloads is retained memory, a flat line is allocator
                    // slack catching up (or nothing at all)
                    let c = crate::js::heap_counters();
                    eprintln!(
                        "[mem] live heap: {:.1} MB (peak {:.1} MB) · freed +{} obj, +{} fn, +{} env, {} env live",
                        crate::live_alloc_bytes() as f64 / (1024.0 * 1024.0),
                        crate::peak_alloc_bytes() as f64 / (1024.0 * 1024.0),
                        c.objs_freed.wrapping_sub(prev_counters.objs_freed),
                        c.closures_freed.wrapping_sub(prev_counters.closures_freed),
                        c.envs_freed.wrapping_sub(prev_counters.envs_freed),
                        c.envs_live,
                    );
                    prev_counters = c;
                    let buckets: Vec<String> = crate::alloc_buckets()
                        .into_iter()
                        .filter(|(_, b)| *b > 0)
                        .map(|(n, b)| format!("{n}:{:.2}", b as f64 / (1024.0 * 1024.0)))
                        .collect();
                    eprintln!("[mem] buckets MB: {}", buckets.join(" "));
                }
                if once {
                    break;
                }
            }
            HeadlessMsg::Cmd(line) => {
                let Some(cmd) = mazda_ipc::Command::decode(&line) else {
                    continue;
                };
                match cmd {
                    mazda_ipc::Command::Quit => break,
                    mazda_ipc::Command::Load(url) => {
                        nav_id += 1;
                        scroll = 0.0;
                        players.clear();
                        forms = layout::FormState::default();
                        let _ = jobs_tx.send(LoadReq::Load {
                            id: nav_id,
                            url,
                            max_w: zoomed_w(pw, zoom),
                            vh: zoomed_h(ph, zoom),
                            engine: EngineChoice::A,
                            fonts: fonts_choice,
                            restore: false,
                        });
                    }
                    mazda_ipc::Command::Restore(url) => {
                        nav_id += 1;
                        scroll = 0.0;
                        players.clear();
                        forms = layout::FormState::default();
                        let _ = jobs_tx.send(LoadReq::Load {
                            id: nav_id,
                            url,
                            max_w: zoomed_w(pw, zoom),
                            vh: zoomed_h(ph, zoom),
                            engine: EngineChoice::A,
                            fonts: fonts_choice,
                            restore: true,
                        });
                    }
                    mazda_ipc::Command::Resize { width, height } => {
                        pw = width as usize;
                        ph = height as usize;
                        frame = None;
                        if let Some(p) = page.as_ref() {
                            nav_id += 1;
                            let _ = jobs_tx.send(LoadReq::Relayout {
                                id: nav_id,
                                url: p.url.clone(),
                                max_w: zoomed_w(pw, zoom),
                                vh: zoomed_h(ph, zoom),
                                forms: forms.clone(),
                                hover: None,
                                engine: EngineChoice::A,
                            });
                            scroll = scroll.min((p.laid.height - zoomed_h(ph, zoom)).max(0.0));
                            headless_paint(
                                p, &fonts, pw, ph, zoom, scroll, find.as_ref(), &mut frame, &mut seq,
                            );
                        }
                    }
                    mazda_ipc::Command::FileChosen { node, name, path } => {
                        eprintln!("[form] picked {name} ({path}) for node {node}");
                        forms.focused = None;
                        forms.values.insert(node, name.clone());
                        forms.files.insert(node, layout::FilePick { name, path });
                        if let Some(p) = page.as_ref() {
                            // the control shows the filename: re-lay out the
                            // kept page with the new state (no repaint here —
                            // the worker's page arrives and paints)
                            nav_id += 1;
                            let _ = jobs_tx.send(LoadReq::Relayout {
                                id: nav_id,
                                url: p.url.clone(),
                                max_w: zoomed_w(pw, zoom),
                                vh: zoomed_h(ph, zoom),
                                forms: forms.clone(),
                                hover: None,
                                engine: EngineChoice::A,
                            });
                        }
                    }
                    mazda_ipc::Command::Scroll(y) => {
                        if let Some(p) = page.as_ref() {
                            let max = (p.laid.height - zoomed_h(ph, zoom)).max(0.0);
                            scroll = y.clamp(0.0, max);
                            headless_paint(
                                p, &fonts, pw, ph, zoom, scroll, find.as_ref(), &mut frame, &mut seq,
                            );
                            let _ = jobs_tx.send(LoadReq::Scroll {
                                url: p.url.clone(),
                                y: scroll,
                            });
                        }
                    }
                    mazda_ipc::Command::Find(q) => {
                        if q.trim().is_empty() {
                            // an empty query is a clear, not a search
                            find = None;
                            println!(
                                "{}",
                                mazda_ipc::Event::FindResult { active: 0, total: 0 }.encode()
                            );
                            if let Some(p) = page.as_ref() {
                                headless_paint(
                                    p, &fonts, pw, ph, zoom, scroll, None, &mut frame, &mut seq,
                                );
                            }
                        } else if let Some(p) = page.as_ref() {
                            // a new query starts at its first hit and brings
                            // that hit into view
                            let f = FindState::new(q, &p.laid);
                            if let Some(h) = f.hits.first() {
                                scroll = find_scroll(h, scroll, zoomed_h(ph, zoom), p.laid.height);
                                println!("{}", mazda_ipc::Event::ScrollTo(scroll).encode());
                            }
                            println!(
                                "{}",
                                mazda_ipc::Event::FindResult {
                                    active: f.active_no(),
                                    total: f.hits.len() as u32,
                                }
                                .encode()
                            );
                            find = Some(f);
                            if let Some(f) = find.as_ref() {
                                headless_paint(
                                    p, &fonts, pw, ph, zoom, scroll, Some(f), &mut frame, &mut seq,
                                );
                            }
                        }
                    }
                    mazda_ipc::Command::FindStep(d) => {
                        if let (Some(f), Some(p)) = (find.as_mut(), page.as_ref()) {
                            if !f.hits.is_empty() {
                                let n = f.hits.len() as i32;
                                f.active =
                                    (((f.active as i32 + d) % n + n) % n) as usize;
                                scroll = find_scroll(
                                    &f.hits[f.active],
                                    scroll,
                                    zoomed_h(ph, zoom),
                                    p.laid.height,
                                );
                                println!("{}", mazda_ipc::Event::ScrollTo(scroll).encode());
                                println!(
                                    "{}",
                                    mazda_ipc::Event::FindResult {
                                        active: f.active_no(),
                                        total: n as u32,
                                    }
                                    .encode()
                                );
                                headless_paint(
                                    p, &fonts, pw, ph, zoom, scroll, Some(f), &mut frame, &mut seq,
                                );
                            }
                        }
                    }
                    mazda_ipc::Command::FindClose => {
                        if find.take().is_some() {
                            println!(
                                "{}",
                                mazda_ipc::Event::FindResult { active: 0, total: 0 }.encode()
                            );
                            if let Some(p) = page.as_ref() {
                                headless_paint(
                                    p, &fonts, pw, ph, zoom, scroll, None, &mut frame, &mut seq,
                                );
                            }
                        }
                    }
                    mazda_ipc::Command::Zoom(z) => {
                        if z.is_finite() && (0.25..=5.0).contains(&z) {
                            zoom = z;
                            if let Some(p) = page.as_ref() {
                                // a new CSS viewport width means a fresh layout
                                nav_id += 1;
                                let _ = jobs_tx.send(LoadReq::Relayout {
                                    id: nav_id,
                                    url: p.url.clone(),
                                    max_w: zoomed_w(pw, zoom),
                                    vh: zoomed_h(ph, zoom),
                                    forms: forms.clone(),
                                    hover: None,
                                    engine: EngineChoice::A,
                                });
                                scroll = scroll
                                    .min((p.laid.height - zoomed_h(ph, zoom)).max(0.0));
                                headless_paint(
                                    p, &fonts, pw, ph, zoom, scroll, find.as_ref(),
                                    &mut frame, &mut seq,
                                );
                            }
                        }
                    }
                    mazda_ipc::Command::Print => match page.as_ref() {
                        Some(p) => match write_print_pdf(p, &fonts, pw) {
                            Ok(path) => println!(
                                "{}",
                                mazda_ipc::Event::PrintDone { path }.encode()
                            ),
                            Err(e) => println!(
                                "{}",
                                mazda_ipc::Event::PrintFailed(e.to_string()).encode()
                            ),
                        },
                        None => println!(
                            "{}",
                            mazda_ipc::Event::PrintFailed("no page to print".into())
                                .encode()
                        ),
                    },
                    mazda_ipc::Command::Click { x, y } => {
                        // the shell clicks in physical pixels; the page is in
                        // CSS pixels at this zoom
                        let (x, y) = (x / zoom, y / zoom);
                        if let Some(p) = page.as_mut() {
                            // a click inside a video toggles playback (no
                            // native controls yet); everywhere else keeps the
                            // link hit test
                            let hit = p.media.iter().position(|m| {
                                let py = if m.fixed { y } else { y + scroll };
                                x >= m.x && x <= m.x + m.w && py >= m.y && py <= m.y + m.h
                            });
                            if let Some(i) = hit {
                                toggle_player(&mut players, p, i);
                            } else if let Some(fb) = p.laid.hit_test_form(x, y, scroll).map(|i| p.laid.forms[i]) {
                                // form controls: a file input asks the shell
                                // for the OS picker, a submit button runs the
                                // full submission (listeners, then GET/POST)
                                match fb.kind {
                                    layout::FormKind::File => {
                                        println!(
                                            "{}",
                                            mazda_ipc::Event::PickFile { node: fb.node }.encode()
                                        );
                                    }
                                    layout::FormKind::Submit => {
                                        eprintln!("[form] submit click on node {}", fb.node);
                                        nav_id += 1;
                                        let _ = jobs_tx.send(LoadReq::Event {
                                            id: nav_id,
                                            url: p.url.clone(),
                                            max_w: zoomed_w(pw, zoom),
                                            vh: zoomed_h(ph, zoom),
                                            node: fb.node,
                                            kinds: vec!["click".into()],
                                            value: None,
                                            key: None,
                                            href: None,
                                            submit: true,
                                            forms: forms.clone(),
                                            engine: EngineChoice::A,
                                        });
                                    }
                                    _ => {}
                                }
                            } else if let Some(href) = headless_link_at(p, x, y, scroll) {
                                // `<a download>` saves the target; every other
                                // link keeps normal navigation
                                match p.downloads.iter().find(|d| d.href == href) {
                                    Some(d) => {
                                        let name = download_name(&href, &d.name);
                                        eprintln!("[download] {href} -> {name}");
                                        start_download(href, name, dl_tx.clone());
                                        dl_active = true;
                                    }
                                    None => {
                                        println!("{}", mazda_ipc::Event::Nav(href).encode())
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // playback advances even while commands stream in
        tick_players(
            &mut players, &mut page, &fonts, pw, ph, zoom, scroll, find.as_ref(), &mut frame, &mut seq,
        );
        drain_downloads(&dl_rx, &mut dl_active);
    }
}

/// One point in the laid-out text stream: a text run index (into
/// `Layout::runs`) plus a byte offset into that run's string.
#[derive(Clone, Copy, PartialEq, Debug)]
struct TextPos {
    run: usize,
    ch: usize,
}

/// A drag-highlighted text range, anchor first in stream order (normalized
/// when built, so `anchor.run/ch <= head.run/ch`).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Selection {
    anchor: TextPos,
    head: TextPos,
}

/// Right-click menu state for the page: where the click landed (screen
/// points), what was under it, and the rect it painted last frame (the
/// outside-click test needs it before this frame's layout exists).
#[derive(Clone)]
struct ContextMenu {
    /// screen-space anchor — the menu opens down-right of it
    pos: egui::Pos2,
    /// resolved absolute target when the click was on a link
    link: Option<String>,
    /// DOM node under the pointer (drives "Inspect element")
    node: Option<u32>,
    /// painted bounds from the previous frame; None on the opening frame
    rect: Option<egui::Rect>,
}

struct App {
    fonts: Fonts,
    page: Page,
    tabs: Vec<Tab>,
    active: usize,
    back_stack: Vec<String>,
    address: String,
    address_focused: bool,
    scroll: f32,
    /// UI-owned form interaction state: focused control + typed values.
    /// Rides along on every worker job so it survives re-layouts.
    forms: layout::FormState,
    /// true when the in-flight job is a real navigation (reset scroll +
    /// address bar); false for relayout/event (preserve both)
    expect_nav: bool,
    /// node id → `name` attr for the current page (worker-built), used to
    /// log `[form] submit name=value` from the shell
    form_names: HashMap<u32, String>,
    pixels: Vec<u32>,
    /// GPU texture holding the last upload of `pixels`
    tex: Option<egui::TextureHandle>,
    /// pixel size of `pixels`/`tex` (viewport minus the two egui panels)
    last_size: (usize, usize),
    /// `pixels` changed since the last texture upload
    dirty: bool,
    /// mirror of the viewport fullscreen state for the F11 toggle
    fullscreen: bool,
    /// last title pushed to the viewport (avoid re-sending every frame)
    title: String,
    /// ---- async loader (page loads run on a worker thread) ----
    /// generation counter: the newest request wins, older results are dropped
    nav: NavGate,
    /// job queue into the loader thread (unbounded: send never blocks the UI)
    jobs: mpsc::Sender<LoadReq>,
    /// results from the loader thread, drained once per frame
    results: mpsc::Receiver<LoadDone>,
    /// the in-flight request we still accept a result for
    pending: Option<PendingNav>,
    /// latest viewport size seen (drives the resize→relayout debounce);
    /// `last_size` stays the size of the `pixels` buffer / texture
    vp_size: (usize, usize),
    /// set when the viewport changed; fires a relayout once the size settled
    pending_resize: Option<Instant>,
    /// link click awaiting the worker's click-listener decision: (job id,
    /// resolved target). The worker dispatches the page's click listeners;
    /// if one calls preventDefault, the navigation never starts (decision
    /// rides `LoadDone.prevent_default` back on the same result).
    pending_link: Option<(u64, String)>,
    /// persistent app settings (mazda.conf loaded/resolved at startup;
    /// written through whenever the settings page changes one)
    conf: Conf,
    /// digest of the tab list last written to the session file; None until
    /// the first frame writes it
    session_saved: Option<u64>,
    /// the ⚙ settings overlay is open (drawn + interactive)
    settings_open: bool,
    /// hovered leaf DOM node from the last frame's hit test — the sheet's
    /// hover cell travels with each relayout (`LoadReq::Relayout::hover`);
    /// cleared on navigation. None = nothing hovered.
    hovered: Option<u32>,
    /// a hover relayout is in flight — keep polling fast until its result
    /// lands (a relayout doesn't set `pending`, so the 1 Hz tick alone
    /// would delay the hover repaint by up to a second)
    hover_wait: bool,
    /// scrollbar drag in progress: distance from the pointer to the thumb's
    /// top edge, in viewport units. None = not dragging.
    bar_grab: Option<f32>,
    /// pointer position at the previous frame. `:hover` is re-evaluated only
    /// when the pointer moved (or a button went down): re-evaluating it after
    /// our OWN relayout fed a loop on real pages — the hovered box shifts its
    /// children under a still pointer, the next hit test lands on a sibling,
    /// which shifts it too, and the page re-lays out forever.
    last_ptr: Option<egui::Pos2>,
    /// element inspector mode: the pointer highlights the element under it,
    /// a click pins one, Esc (or the toolbar toggle) leaves
    inspect: bool,
    /// pinned element node id from the inspector (survives relayouts,
    /// cleared on navigation); the details panel follows it
    pinned: Option<u32>,
    /// text selection highlighted in the page (set by drag)
    selection: Option<Selection>,
    /// drag origin while the primary button is down; None = not selecting
    select_anchor: Option<TextPos>,
    /// open page context menu (right-click); None = closed
    menu: Option<ContextMenu>,
}

/// Address-bar heuristic (unchanged from the minifb shell): a dot and no
/// space means URL (https:// prefixed), anything else is a DuckDuckGo search.
/// data: URLs are passed through untouched (inline documents are never
/// search queries — they contain commas/spaces that would corrupt it).
fn address_target(q: &str) -> String {
    if q.starts_with("data:") {
        return q.to_string();
    }
    if q.contains('.') && !q.contains(' ') {
        if q.starts_with("http") {
            q.to_string()
        } else {
            format!("https://{q}")
        }
    } else {
        format!("https://duckduckgo.com/?q={}", q.replace(' ', "+"))
    }
}

/// Text-wrap width for a page viewport (matches the old shell).
fn page_w(pw: usize) -> f32 {
    (pw as f32 - 2.0 * PADDING as f32).max(40.0)
}

/// Viewport height for layout / scroll clamping (matches the old shell).
fn page_h(ph: usize) -> f32 {
    (ph as f32 - 2.0 * PADDING as f32).max(40.0)
}

/// Layout width for a zoom factor: the renderer lays out (and paints) in CSS
/// pixels at `pw / zoom`, then scales the frame up to the physical size.
fn zoomed_w(pw: usize, zoom: f32) -> f32 {
    page_w(((pw as f32 / zoom.max(0.01)).round() as usize).max(1))
}

/// Layout/viewport height for a zoom factor (see [`zoomed_w`]).
fn zoomed_h(ph: usize, zoom: f32) -> f32 {
    page_h(((ph as f32 / zoom.max(0.01)).round() as usize).max(1))
}

/// Page scrollbar: width of the strip in the right gutter, in pixels.
const BAR_W: usize = 8;
/// Shortest thumb the bar will draw, so a barely-overflowing document
/// still leaves something grabbable.
const BAR_MIN: f32 = 24.0;
/// Page-load progress bar: height in logical points, pinned to the top edge
/// of the page viewport (same accent as the address pill's focus ring).
const LOAD_BAR_H: f32 = 3.0;
const LOAD_BAR_RGB: (u8, u8, u8) = (0x6a, 0x9c, 0xff);

/// Horizontal band of the scrollbar in page-buffer pixels `(x0, x1)`.
/// One source of truth for both painting and hit-testing.
fn scrollbar_strip(pw: usize) -> (f32, f32) {
    let x0 = (pw.saturating_sub(PADDING) + 2) as f32;
    (x0, (x0 + BAR_W as f32).min(pw as f32 - 2.0))
}

/// Thumb `(y, h)` in viewport units (0 = top of the page area), or None
/// when the document fits (no bar). The length is proportional to
/// viewport/document and floored at [`BAR_MIN`].
fn scrollbar_thumb(doc_h: f32, page: f32, scroll: f32) -> Option<(f32, f32)> {
    if !(doc_h > page) || page <= 0.0 {
        return None;
    }
    let max_scroll = doc_h - page;
    let th = (page * page / doc_h).max(BAR_MIN).min(page);
    let travel = page - th;
    let y = (scroll.clamp(0.0, max_scroll) / max_scroll) * travel;
    Some((y, th))
}

/// Scroll offset for a thumb top at `y` — the inverse of
/// [`scrollbar_thumb`], used while dragging.
fn scroll_from_thumb(doc_h: f32, page: f32, y: f32) -> f32 {
    let max_scroll = (doc_h - page).max(0.0);
    if max_scroll == 0.0 || page <= 0.0 {
        return 0.0;
    }
    let th = (page * page / doc_h).max(BAR_MIN).min(page);
    let travel = (page - th).max(1.0);
    (y / travel * max_scroll).clamp(0.0, max_scroll)
}

/// 0x00RRGGBB framebuffer → egui ColorImage (RGBA, opaque).
fn pixels_to_image(pixels: &[u32], w: usize, h: usize) -> egui::ColorImage {
    debug_assert_eq!(pixels.len(), w * h);
    // one pass into the ColorImage's own buffer: the old byte-vec version
    // allocated twice and pushed 4 bytes per pixel (half a scroll frame)
    let rgba: Vec<egui::Color32> = pixels
        .iter()
        .map(|&p| egui::Color32::from_rgb((p >> 16) as u8, (p >> 8) as u8, p as u8))
        .collect();
    egui::ColorImage::new([w, h], rgba)
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // dark widget theme so TextEdit's cursor/selection read on the dark
        // chrome; the page rasterizes its own fonts regardless
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let fonts = Fonts::load();
        // start URL = first arg that is not a `--knob` (the selectable
        // runtime/fonts CLI overrides may come in any position; every
        // `--xxx[=value]` knob is excluded, never mistaken for a URL)
        let cli = std::env::args().skip(1).find(|a| !a.starts_with("--"));
        // logical viewport = NativeOptions initial size; one texture pixel per
        // logical point (a HiDPI framebuffer upscales the texture — fine at
        // pixels_per_point 1.0; shortcut: no per-monitor DPI raster scale).
        let (pw, ph) = (1000usize, (700.0 - TAB_H - ADDR_H) as usize);
        // A page named on the command line starts a fresh one-tab session;
        // otherwise the previous session comes back (the active tab loads
        // below through the snapshot path, the rest wait for activation).
        let restored = cli.is_none().then(|| {
            std::fs::read_to_string(session_path())
                .ok()
                .and_then(|t| decode_session(&t))
        });
        let (tabs, active) = match restored.flatten() {
            Some((tabs, active)) => {
                let active = active.min(tabs.len() - 1);
                (tabs, active)
            }
            None => {
                let url =
                    address_target(cli.as_deref().unwrap_or("https://example.com").trim());
                (vec![Tab { url, scroll: 0.0 }], 0)
            }
        };
        let url = tabs[active].url.clone();
        // The window opens immediately with a blank page; the first load runs
        // on the worker thread (it used to block window creation until the
        // network answered — the other half of the "stuck after clicking"
        // complaint).
        let page = Page {
            url: url.clone(),
            laid: Layout {
                runs: Vec::new(),
                height: 0.0,
                underline_rects: Vec::new(),
                rects: Vec::new(),
                rect_grad: Vec::new(),
                rect_ring: Vec::new(),
                images: Vec::new(),
                shadows: Vec::new(),
                open_line: false,
                forms: Vec::new(),
                focused: None,
                form_values: Default::default(),
                sticky: Vec::new(),
                backdrops: Vec::new(),
                hover_targets: Vec::new(),
            },
            bg: BG,
            media: Vec::new(),
            downloads: Vec::new(),
            elements: HashMap::new(),
        };
        let pixels = vec![page.bg; pw * ph];
        // one long-lived worker thread; unbounded channels so neither side
        // can ever block the other on send
        let (job_tx, job_rx) = mpsc::channel::<LoadReq>();
        let (done_tx, done_rx) = mpsc::channel::<LoadDone>();
        std::thread::spawn(move || loader_thread(job_rx, done_tx));
        // resolve the script engine once: `--engine=a|b` flag > MAZDA_ENGINE
        // env > mazda.conf on disk > default A (the selectable runtime;
        // the settings page writes through on every change from here)
        let args: Vec<String> = std::env::args().collect();
        let conf = Conf::init(&args);
        let mut app = App {
            fonts,
            page,
            tabs,
            active,
            back_stack: Vec::new(),
            address: url.clone(),
            address_focused: false,
            scroll: 0.0,
            forms: layout::FormState::default(),
            expect_nav: true,
            form_names: HashMap::new(),
            pixels,
            tex: None,
            last_size: (pw, ph),
            dirty: true,
            fullscreen: false,
            title: "Mazda".to_string(),
            nav: NavGate::new(),
            jobs: job_tx,
            results: done_rx,
            pending: None,
            vp_size: (pw, ph),
            pending_resize: None,
            pending_link: None,
            conf,
            settings_open: false,
            session_saved: None,
            hovered: None,
            hover_wait: false,
            bar_grab: None,
            last_ptr: None,
            inspect: false,
            pinned: None,
            selection: None,
            select_anchor: None,
            menu: None,
        };
        // A URL named on the command line is a fresh visit: fetch it, never
        // restore a snapshot (a snapshot carries the DOM the previous visit
        // ended with — scripts still run, but server-side variants such as
        // locale or theme would be frozen). Session restore keeps the
        // snapshot path: it is the same page the user left, and skipping the
        // network is the whole point of the snapshot.
        app.dispatch(&url, pw, ph, 0.0, cli.is_none());
        app
    }

    /// Write the tab list when it changed. Called once per frame, so a page
    /// load and a tab action both land here; an idle frame writes nothing.
    fn sync_session(&mut self) {
        let d = session_digest(&self.tabs, self.active);
        if self.session_saved == Some(d) {
            return;
        }
        let path = session_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, encode_session(&self.tabs, self.active)) {
            eprintln!("[session] write failed {}: {e}", path.display());
        }
        self.session_saved = Some(d);
    }

    /// Queue a load of `url` on the worker thread (never blocks the UI).
    /// `restore` = try the DOM snapshot before fetching (tab switch / back).
    fn dispatch(&mut self, url: &str, pw: usize, ph: usize, scroll: f32, restore: bool) {
        let id = self.nav.begin();
        eprintln!("[nav] queued load {url} (#{id}, viewport {pw}x{ph})");
        let req = LoadReq::Load {
            id,
            url: url.to_string(),
            max_w: page_w(pw),
            vh: page_h(ph),
            engine: self.conf.engine,
            fonts: self.conf.fonts,
            restore,
        };
        // receiver only drops when the worker thread is gone
        if self.jobs.send(req).is_err() {
            eprintln!("[nav] loader thread unavailable, load of {url} dropped");
        }
        self.pending = Some(PendingNav { scroll });
        self.expect_nav = true;
        // a fresh navigation clears form focus/values (worker starts clean)
        self.forms = layout::FormState::default();
        // …and the hover cell (the new page's worker side starts at None)
        self.hovered = None;
        // …and anything the inspector/selection pointed into the old page
        self.pinned = None;
        self.selection = None;
        self.select_anchor = None;
        self.menu = None;
    }

    /// Queue a re-layout of the current page (resize / form typing / hover
    /// change). Keeps scroll + address bar; carries the live form state
    /// AND the current hover cell forward.
    fn dispatch_relayout(&mut self, pw: usize, ph: usize) {
        let id = self.nav.begin();
        let url = self.page.url.clone();
        let req = LoadReq::Relayout {
            id,
            url: url.clone(),
            max_w: page_w(pw),
            vh: page_h(ph),
            forms: self.forms.clone(),
            hover: self.hovered,
            engine: self.conf.engine,
        };
        if self.jobs.send(req).is_err() {
            eprintln!("[nav] loader thread unavailable, relayout of {url} dropped");
        }
        self.expect_nav = false;
    }

    /// Queue a form/page event job (click/submit/change/input/scroll-adjacent).
    /// The worker mutates its DOM, fires the registered listeners for each
    /// kind (in order), re-lays out and re-emits. Returns the job id so
    /// click-decision callers can match the worker's verdict.
    fn dispatch_event(
        &mut self,
        node: u32,
        kinds: &[&str],
        value: Option<String>,
        key: Option<&str>,
        pw: usize,
        ph: usize,
    ) -> u64 {
        self.dispatch_event_ext(node, kinds, value, key, false, pw, ph)
    }

    /// `dispatch_event` with a submission trigger: the worker runs the
    /// listed listeners, then the form's `submit` listeners may veto
    /// (preventDefault) before the real GET/POST load executes — all under
    /// this one job's NavGate id (the submission IS the async load).
    fn dispatch_event_ext(
        &mut self,
        node: u32,
        kinds: &[&str],
        value: Option<String>,
        key: Option<&str>,
        submit: bool,
        pw: usize,
        ph: usize,
    ) -> u64 {
        let id = self.nav.begin();
        let req = LoadReq::Event {
            id,
            url: self.page.url.clone(),
            max_w: page_w(pw),
            vh: page_h(ph),
            node,
            kinds: kinds.iter().map(|s| s.to_string()).collect(),
            value,
            key: key.map(|s| s.to_string()),
            href: None,
            submit,
            forms: self.forms.clone(),
            engine: self.conf.engine,
        };
        if self.jobs.send(req).is_err() {
            eprintln!("[nav] loader thread unavailable, {} event dropped", kinds.join("+"));
        }
        self.expect_nav = false;
        id
    }

    /// Drain finished loads; apply the newest (stale generations are dropped).
    fn drain_results(&mut self, pw: usize, ph: usize) {
        while let Ok(done) = self.results.try_recv() {
            if !self.nav.accept(done.id) {
                eprintln!("[nav] dropped stale result #{} (want #{})", done.id, self.nav.next);
                continue;
            }
            // accepted ⇒ dispatched at/after our newest hover job, so its
            // page carries the current hover cell — the wait is over
            self.hover_wait = false;
            if done.partial {
                // first paint of a load in progress: adopt the markup+CSS
                // layout and paint it. Navigation state (pending marker,
                // history, address) belongs to the final page of this job.
                if let Some(wire) = done.page {
                    self.show_partial(wire, pw, ph);
                }
                continue;
            }
            if done.panicked {
                eprintln!("[nav] load #{} failed (pipeline panic)", done.id);
                // a failed job must not leave the shell thinking it is still
                // loading (that pinned the progress bar on and made the next
                // tab click look ignored) — drop the in-flight marker
                self.pending = None;
                self.expect_nav = false;
                continue;
            }
            // a script may have asked to navigate (location.assign etc.)
            if let Some(nav_url) = done.nav {
                if done.nav_done {
                    // form submission loaded worker-side (nav + rendered
                    // target page arrive together): adopt the page directly
                    // with full navigation semantics (history + address); a
                    // UI-side re-dispatch here would re-fetch the target as
                    // a plain GET and LOSE a POST body.
                    eprintln!(
                        "[nav] form submission accept at #{} → {nav_url}",
                        done.id
                    );
                    self.back_stack.push(self.page.url.clone());
                    self.expect_nav = true;
                    self.pending = None;
                    // fall through to the shared page-apply below (the
                    // wire's url IS nav_url and the expect_nav branch
                    // updates tabs/address from it)
                } else {
                    // plain script nav on a concurrently-fetched page: the
                    // target was never fetched — re-dispatch it as a load
                    eprintln!("[js] script navigated to {nav_url}");
                    self.dispatch(&nav_url, pw, ph, 0.0, false);
                    continue;
                }
            }
            // link click awaiting its listener decision: veto (preventDefault)
            // or proceed (navigate). See `click_decision` (pure, tested).
            if let Some((pid, url)) = self.pending_link.clone() {
                if pid == done.id {
                    match click_decision(&self.pending_link, done.prevent_default) {
                        LinkDecision::Navigate(target) => {
                            self.pending_link = None;
                            eprintln!("[js] click listeners done → navigate {target}");
                            self.navigate(&target, pw, ph);
                            continue;
                        }
                        LinkDecision::Cancel => {
                            self.pending_link = None;
                            eprintln!("[js] preventDefault — link navigation to {url} canceled");
                            // fall through: the page result still applies
                            // (a listener may have mutated the DOM)
                        }
                        LinkDecision::Ignore => {}
                    }
                } else {
                    // an unrelated result was accepted first (resize,
                    // another job): the click is stale, drop the decision
                    eprintln!("[nav] link click superseded by job #{}; not navigating to {url}", done.id);
                    self.pending_link = None;
                }
            }
            let Some(mut wire) = done.page else {
                eprintln!("[nav] load #{} returned no page", done.id);
                // same rule as the panic branch: never leave a load hanging
                self.pending = None;
                self.expect_nav = false;
                continue;
            };
            // adopt the worker's font state: stack ids baked into run styles
            // index this table, and the renderer must resolve webfonts too
            let stacks = std::mem::take(&mut wire.stacks);
            let faces = std::mem::take(&mut wire.faces);
            self.fonts.begin_page(faces); // resets the stack table to [id 0]
            *self.fonts.stacks.borrow_mut() = stacks;
            // the worker built node→name for [form] submit logging
            let form_names = std::mem::take(&mut wire.form_names);
            self.page = wire_to_page(wire);
            self.last_size = (pw, ph);
            if self.expect_nav {
                let scroll = self.pending.take().map(|p| p.scroll).unwrap_or(0.0);
                self.scroll = scroll.min((self.page.laid.height - page_h(ph)).max(0.0));
                self.tabs[self.active].url = self.page.url.clone();
                self.tabs[self.active].scroll = self.scroll;
                self.address = self.page.url.clone();
                // fresh page: its worker side starts with hover = None
                self.hovered = None;
            } else {
                // relayout/event: keep scroll (clamped to new height) and
                // don't clobber whatever the user is typing in the address
                // bar. Adopt the worker's focused field + values so paint
                // matches what the worker laid out.
                self.scroll = self.scroll.min((self.page.laid.height - page_h(ph)).max(0.0));
                self.tabs[self.active].scroll = self.scroll;
                self.forms.focused = self.page.laid.focused;
            }
            // stash names for submit logging (kept with the page)
            self.form_names = form_names;
            self.repaint_page(pw, ph);
            eprintln!("[nav] applied #{} → {}", self.nav.next, self.page.url);
        }
    }

    /// Adopt a first-paint snapshot: markup + stylesheets are laid out while
    /// the load's scripts are still running. Paints now, keeps every
    /// navigation marker untouched, and is replaced by the job's final page.
    fn show_partial(&mut self, mut wire: PageWire, pw: usize, ph: usize) {
        let stacks = std::mem::take(&mut wire.stacks);
        let faces = std::mem::take(&mut wire.faces);
        self.fonts.begin_page(faces);
        *self.fonts.stacks.borrow_mut() = stacks;
        self.form_names = std::mem::take(&mut wire.form_names);
        self.page = wire_to_page(wire);
        self.scroll = self
            .scroll
            .min((self.page.laid.height - page_h(ph)).max(0.0));
        self.repaint_page(pw, ph);
        eprintln!("[nav] first paint of #{} in progress", self.nav.next);
    }

    /// Re-rasterize the active page into `pixels` (marks the texture dirty).
    fn repaint_page(&mut self, pw: usize, ph: usize) {
        // the buffer and `last_size` must never disagree: a width change
        // (window resize) followed by a scroll repaint used to leave the
        // texture dims stale and trip the color-image length assertion
        self.last_size = (pw, ph);
        // reuse the buffer: a scroll repaint used to allocate pw*ph again
        if self.pixels.len() != pw * ph {
            self.pixels = vec![self.page.bg; pw * ph];
        } else {
            self.pixels.fill(self.page.bg);
        }
        render(
            &mut self.pixels,
            pw,
            ph,
            &self.page.laid,
            &self.fonts,
            self.scroll,
            page_h(ph),
            0.0,
        );
        // viewport chrome painted on top of the content (never scrolls)
        draw_scrollbar(
            &mut self.pixels,
            pw,
            ph,
            self.page.laid.height,
            page_h(ph),
            self.scroll,
        );
        self.dirty = true;
    }

    /// Apply a scroll offset: clamp, repaint, and tell the worker. Scroll
    /// jobs coalesce worker-side (a drag storm stays cheap) and only
    /// re-lay-out when a listener actually mutated the DOM.
    fn set_scroll(&mut self, y: f32, pw: usize, ph: usize) {
        let max = (self.page.laid.height - page_h(ph)).max(0.0);
        let y = y.clamp(0.0, max);
        if y == self.scroll {
            return;
        }
        self.scroll = y;
        self.tabs[self.active].scroll = y;
        self.repaint_page(pw, ph);
        let req = LoadReq::Scroll {
            url: self.page.url.clone(),
            y,
        };
        if self.jobs.send(req).is_err() {
            eprintln!("[nav] loader thread unavailable, scroll event dropped");
        }
    }

    /// Load `url` into the active tab: push history, reset scroll.
    fn navigate(&mut self, url: &str, pw: usize, ph: usize) {
        self.back_stack.push(self.page.url.clone());
        self.dispatch(url, pw, ph, 0.0, false);
        self.tabs[self.active].url = url.to_string();
        self.address = url.to_string();
    }

    /// Load `url` into the active tab WITHOUT touching history (tab switch /
    /// close / back), restoring `scroll`.
    fn reload_active(&mut self, url: &str, pw: usize, ph: usize, scroll: f32) {
        // tab switch / close / back: the snapshot avoids re-fetching the page
        self.dispatch(url, pw, ph, scroll, true);
        self.tabs[self.active].url = url.to_string();
        self.address = url.to_string();
    }

    /// Toggle the element inspector. Leaving the mode drops the pinned
    /// element; a text selection stays visible.
    fn set_inspect(&mut self, on: bool) {
        if self.inspect == on {
            return;
        }
        self.inspect = on;
        self.pinned = None;
        self.select_anchor = None;
        eprintln!("[inspect] {}", if on { "on" } else { "off" });
    }

    /// Nearest character boundary to `x` inside one laid-out run. Advances
    /// come from the same font picker the renderer measures with; the sum is
    /// rescaled to the run's laid width so kerning can't drift the estimate.
    fn char_at_x(&self, run: &layout::PositionedRun, x: f32) -> usize {
        if run.text.is_empty() {
            return 0;
        }
        let advances: Vec<f32> = run
            .text
            .chars()
            .map(|c| {
                let mut buf = [0u8; 4];
                self.fonts.measure(c.encode_utf8(&mut buf), &run.style)
            })
            .collect();
        let total: f32 = advances.iter().sum();
        let rel = (x - run.x).max(0.0);
        let target = if run.width > 0.0 && total > 0.0 {
            rel * total / run.width
        } else {
            rel
        };
        let mut acc = 0.0f32;
        let mut best = 0usize;
        let mut best_d = target;
        let mut idx = 0usize;
        for (c, adv) in run.text.chars().zip(&advances) {
            acc += *adv;
            idx += c.len_utf8();
            let d = (acc - target).abs();
            if d < best_d {
                best_d = d;
                best = idx;
            }
        }
        best
    }

    /// Link target under a content point (`x`, `y` before the scroll offset),
    /// resolved against the page URL. Same run hit-test as a left click, so a
    /// right click on a link reports exactly what a click would open.
    fn link_at(&self, x: f32, y: f32, scroll: f32) -> Option<String> {
        let run = self.page.laid.runs.iter().find(|r| {
            let ry = if r.fixed { y } else { y + scroll };
            r.link.is_some()
                && x >= r.x
                && x <= r.x + r.width
                && ry >= r.y
                && ry <= r.y + r.style.size * 1.35
        })?;
        let href = run.link.as_deref()?;
        resolve(&self.page.url, href)
    }

    /// The control a click at (x, y) reaches through a `<label>`: the pointer
    /// sits on label content and the label names a laid-out control (the
    /// worker resolves label -> control per page wire, see
    /// `ElementInfo.label_for`). None when no label covers the point.
    fn label_form_at(&self, x: f32, y: f32) -> Option<layout::FormBox> {
        let (node, _) = self.page.laid.hit_hover(x, y, self.scroll)?;
        let target = self.page.elements.get(&node)?.label_for?;
        self.page.laid.forms.iter().find(|f| f.node == target).copied()
    }

    /// Select every laid-out text run (the "Select all" menu action). Empty
    /// runs are skipped so the first/last positions always point at real
    /// characters.
    fn select_all(&mut self) {
        let mut first = None;
        let mut last = None;
        for (i, r) in self.page.laid.runs.iter().enumerate() {
            if r.text.is_empty() {
                continue;
            }
            first.get_or_insert(TextPos { run: i, ch: 0 });
            last = Some(TextPos {
                run: i,
                ch: r.text.len(),
            });
        }
        self.selection = match (first, last) {
            (Some(anchor), Some(head)) => Some(Selection { anchor, head }),
            _ => None,
        };
    }

    /// Text position under a content point (`x`, `y` are page-buffer units
    /// before the scroll offset). Prefers a run the point is inside;
    /// otherwise the closest run (vertically) so a drag past the last line
    /// still extends the selection instead of stopping.
    fn text_pos_at(&self, x: f32, y: f32, scroll: f32) -> Option<TextPos> {
        let mut exact: Option<usize> = None;
        let mut nearest: Option<(f32, usize)> = None;
        for (i, r) in self.page.laid.runs.iter().enumerate() {
            if r.text.is_empty() {
                continue;
            }
            let vy = if r.fixed { y } else { y + scroll };
            let (top, bot) = (r.y, r.y + r.style.line_height());
            if vy >= top && vy <= bot && x >= r.x - 1.0 && x <= r.x + r.width + 1.0 {
                exact = Some(i); // later runs paint on top
                continue;
            }
            let dy = if vy < top {
                top - vy
            } else if vy > bot {
                vy - bot
            } else {
                0.0
            };
            let score = dy * 1000.0 + (x - (r.x + r.width * 0.5)).abs() * 0.01;
            if nearest.map_or(true, |(s, _)| score < s) {
                nearest = Some((score, i));
            }
        }
        let i = exact.or(nearest.map(|(_, i)| i))?;
        let r = &self.page.laid.runs[i];
        Some(TextPos {
            run: i,
            ch: self.char_at_x(r, x),
        })
    }

    /// Plain text of the current selection (newline between visual lines,
    /// space across a word gap), used by Ctrl+C.
    fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let mut out = String::new();
        // (right edge, y) of the previous included run — layout splits at
        // spaces, so a horizontal gap between two runs IS a space
        let mut prev_end: Option<(f32, f32)> = None;
        for i in sel.anchor.run..=sel.head.run {
            let r = self.page.laid.runs.get(i)?;
            let from = if i == sel.anchor.run { sel.anchor.ch } else { 0 };
            let to = if i == sel.head.run {
                sel.head.ch
            } else {
                r.text.len()
            };
            let from = from.min(r.text.len());
            let to = to.min(r.text.len()).max(from);
            if let Some((x_end, py)) = prev_end {
                if (py - r.y).abs() > 0.5 {
                    out.push('\n');
                } else if r.x > x_end + 0.5
                    && !out.is_empty()
                    && !out.ends_with(char::is_whitespace)
                {
                    out.push(' ');
                }
            }
            out.push_str(&r.text[from..to]);
            prev_end = Some((r.x + r.width, r.y));
        }
        Some(out)
    }

    /// A form control was clicked. Text fields take focus (no worker job
    /// needed until a key arrives); the field that loses focus fires its
    /// `change` listeners one round trip before the new focus settles.
    /// checkboxes/radios toggle + fire listeners on the worker; submit
    /// buttons log `[form] submit` and fire click listeners. The worker
    /// owns the DOM, so toggles/events go there.
    fn on_form_click(&mut self, fb: layout::FormBox, pw: usize, ph: usize) {
        match fb.kind {
            layout::FormKind::TextInput { .. } => {
                // blur → change (with the final value) for the field giving
                // up focus, THEN focus the clicked one (two round trips,
                // processed in order by the worker)
                if self.forms.focused != Some(fb.node) {
                    if let Some(prev) = self.forms.focused.take() {
                        let val = self.forms.values.get(&prev).cloned();
                        self.dispatch_event(prev, &["change"], val, None, pw, ph);
                    }
                    // focus this field and seed the control's current
                    // display value so typing starts from what's shown
                    self.forms.focused = Some(fb.node);
                    let cur = self.page.laid.form_value(fb.node).to_string();
                    self.forms.values.insert(fb.node, cur);
                    self.dispatch_relayout(pw, ph);
                }
            }
            layout::FormKind::Checkbox | layout::FormKind::Radio => {
                // worker toggles `checked`, dispatches click+change, relays
                self.forms.focused = None;
                self.dispatch_event(fb.node, &["click"], None, None, pw, ph);
            }
            layout::FormKind::Submit => {
                // real submission: the shell logs the visible pair (same
                // observable contract as before) and the event job now
                // carries the submission trigger — the worker runs the
                // button's click listeners, then the form's submit
                // listeners (preventDefault may cancel), then executes the
                // GET/POST load on the worker thread under this job's gate.
                let name = self
                    .form_names
                    .get(&fb.node)
                    .cloned()
                    .unwrap_or_default();
                let val = self
                    .forms
                    .values
                    .get(&fb.node)
                    .cloned()
                    .unwrap_or_else(|| self.page.laid.form_value(fb.node).to_string());
                eprintln!("[form] submit {name}={val}");
                // leaving a text field on Enter fires its change listeners
                if let Some(prev) = self.forms.focused.take() {
                    if prev != fb.node {
                        let val = self.forms.values.get(&prev).cloned();
                        self.dispatch_event(prev, &["change"], val, None, pw, ph);
                    }
                }
                self.forms.focused = None;
                self.dispatch_event_ext(fb.node, &["click"], None, None, true, pw, ph);
            }
            layout::FormKind::Select | layout::FormKind::Generic | layout::FormKind::File => {
                // no popup / non-interactive yet — just blur
                if let Some(prev) = self.forms.focused.take() {
                    let val = self.forms.values.get(&prev).cloned();
                    self.dispatch_event(prev, &["change"], val, None, pw, ph);
                } else {
                    self.dispatch_relayout(pw, ph);
                }
            }
        }
    }
}

/// What a keystroke does to the focused form field. Pure so it's testable
/// without synthesizing input (a hard constraint of this shell).
#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    /// append a char to the focused field's value
    Insert(char),
    /// delete the char before the caret
    Backspace,
    /// Enter in a text field → submit (shell logs it)
    Submit,
    /// Escape → blur the field
    Blur,
    /// key doesn't apply (no field focused / not an editing key)
    None,
}

/// Map one key event to a form action. `has_field_focus` = a form field is
/// focused; `address_focused` suppresses page typing (the address bar owns
/// the keyboard). Printable chars arrive as `egui::Event::Text`, not here —
/// this handles the editing/navigation keys only.
fn route_key(key: egui::Key, has_field_focus: bool, address_focused: bool) -> KeyAction {
    if address_focused {
        return KeyAction::None;
    }
    match key {
        egui::Key::Escape if has_field_focus => KeyAction::Blur,
        egui::Key::Enter if has_field_focus => KeyAction::Submit,
        egui::Key::Backspace if has_field_focus => KeyAction::Backspace,
        _ => KeyAction::None,
    }
}

/// The in-flight load we still accept a result for.
struct PendingNav {
    scroll: f32,
}

/// Map a cascaded `cursor` keyword to the egui pointer icon.
fn cursor_icon_of(c: css::Cursor) -> egui::CursorIcon {
    match c {
        css::Cursor::Default => egui::CursorIcon::Default,
        css::Cursor::Pointer => egui::CursorIcon::PointingHand,
        css::Cursor::Text => egui::CursorIcon::Text,
        css::Cursor::Move => egui::CursorIcon::Move,
        css::Cursor::Crosshair => egui::CursorIcon::Crosshair,
        css::Cursor::NotAllowed => egui::CursorIcon::NotAllowed,
        css::Cursor::Help => egui::CursorIcon::Help,
        css::Cursor::Grab => egui::CursorIcon::Grab,
        css::Cursor::ColResize => egui::CursorIcon::ResizeHorizontal,
        css::Cursor::RowResize => egui::CursorIcon::ResizeVertical,
    }
}

/// Should a change of the hit-tested node cost a relayout?
///
/// - `pointer_active`: `:hover` is re-evaluated only when the pointer moved
///   (or a button went down). A still pointer must NOT re-hover: after our own
///   relayout the hovered box can shift its children out from under it, the
///   next hit test lands on a sibling, and the page re-lays out forever (a
///   real article reproduced this at ~4 s per pass).
/// - `busy`: another job is in flight — retry next frame (self-healing).
/// - `scrolling`: content slides under a still pointer, so every frame would
///   "change hover" and each change costs a full re-layout.
fn hover_should_relayout(
    new_hovered: Option<u32>,
    hovered: Option<u32>,
    pointer_active: bool,
    busy: bool,
    scrolling: bool,
) -> bool {
    pointer_active && new_hovered != hovered && !busy && !scrolling
}

/// Translucent wash + outline over one element's border box, with a small
/// `tag#id.class` chip. `info.rect` is in document coordinates (viewport
/// coordinates for `position: fixed`), so the scroll offset is applied here.
fn paint_inspect_box(
    painter: &egui::Painter,
    page: egui::Rect,
    scroll: f32,
    info: &ElementInfo,
    pinned: bool,
) {
    let (x, y, w, h) = info.rect;
    let sy = if info.fixed { y } else { y - scroll };
    let rect = egui::Rect::from_min_size(
        page.min + egui::vec2(PADDING as f32 + x, PADDING as f32 + sy),
        egui::vec2(w.max(2.0), h.max(2.0)),
    );
    let visible = rect.intersect(page);
    if !visible.is_positive() {
        return;
    }
    painter.rect_filled(
        visible,
        0.0,
        egui::Color32::from_rgba_unmultiplied(0x5b, 0x9d, 0xff, 44),
    );
    painter.rect_stroke(
        visible,
        0.0,
        egui::Stroke::new(
            if pinned { 2.0 } else { 1.0 },
            egui::Color32::from_rgba_unmultiplied(0x5b, 0x9d, 0xff, 230),
        ),
        egui::StrokeKind::Inside,
    );

    // chip above the box; pushed inside when the box starts at the top edge
    let font = egui::FontId::monospace(11.0);
    let galley = painter.layout_no_wrap(info.selector(), font, egui::Color32::WHITE);
    let pad = egui::vec2(5.0, 2.0);
    let mut at = egui::pos2(rect.left(), rect.top() - galley.size().y - 2.0 * pad.y - 2.0);
    if at.y < page.top() {
        at.y = rect.top() + 2.0;
    }
    at.x = at.x.clamp(page.left(), (page.right() - galley.size().x - 2.0 * pad.x).max(page.left()));
    let chip = egui::Rect::from_min_size(at, galley.size() + 2.0 * pad);
    painter.rect_filled(chip, 3.0, egui::Color32::from_rgb(0x1f, 0x6f, 0xd1));
    painter.galley(at + pad, galley, egui::Color32::WHITE);
}

/// Floating details card for the inspected element (bottom-left of the page,
/// clear of the scrollbar and the RSS meter).
fn paint_inspect_panel(
    ui: &egui::Ui,
    page: egui::Rect,
    info: &ElementInfo,
    pinned: bool,
) {
    let bg: egui::Color32 = egui::Color32::from_rgba_unmultiplied(0x1b, 0x1c, 0x1f, 235);
    let border: egui::Color32 = egui::Color32::from_rgb(0x3a, 0x3d, 0x44);
    let dim: egui::Color32 = egui::Color32::from_rgb(0x8b, 0x90, 0x96);
    let pad = 10.0;
    let w = 340.0_f32.min((page.width() - 24.0).max(200.0));
    let (x, y, bw, bh) = info.rect;

    let mut lines: Vec<(String, egui::Color32, f32)> = vec![
        (info.selector(), egui::Color32::from_rgb(0x8f, 0xbb, 0xff), 13.0),
        (
            info.path.clone(),
            dim,
            10.5,
        ),
        (
            format!("{bw:.0} \u{00d7} {bh:.0}  \u{00b7}  at {x:.0}, {y:.0}"),
            egui::Color32::from_rgb(0xc9, 0xcd, 0xd2),
            11.5,
        ),
    ];
    for (k, v) in info.attrs.iter().take(4) {
        lines.push((
            format!("{k}=\"{v}\""),
            egui::Color32::from_rgb(0xb6, 0xbd, 0xc6),
            11.0,
        ));
    }
    if !info.text.is_empty() {
        lines.push((
            format!("\u{201c}{}\u{201d}", info.text),
            egui::Color32::from_rgb(0x9a, 0xa0, 0xa6),
            11.0,
        ));
    }
    lines.push((
        if pinned {
            "click an element to pin \u{00b7} Esc exits"
        } else {
            "click to pin \u{00b7} Esc exits"
        }
        .into(),
        dim,
        10.0,
    ));

    let painter = ui.painter().with_clip_rect(page);
    let font_of = |size: f32| egui::FontId::proportional(size);
    let galleys: Vec<_> = lines
        .iter()
        .map(|(text, color, size)| {
            painter.layout(text.clone(), font_of(*size), *color, w - 2.0 * pad)
        })
        .collect();
    let h = galleys.iter().map(|g| g.size().y + 3.0).sum::<f32>() + 2.0 * pad - 3.0;
    let panel = egui::Rect::from_min_size(
        egui::pos2(page.left() + 10.0, (page.bottom() - 10.0 - h).max(page.top())),
        egui::vec2(w, h),
    );
    painter.rect_filled(panel, 7.0, bg);
    painter.rect_stroke(
        panel,
        7.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    let mut at = panel.min + egui::vec2(pad, pad);
    for (galley, (_, color, _)) in galleys.into_iter().zip(&lines) {
        let size = galley.size();
        painter.galley(at, galley, *color);
        at.y += size.y + 3.0;
    }
}

/// What a page context-menu row does when clicked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuAction {
    Back,
    Reload,
    Copy,
    SelectAll,
    OpenLink,
    OpenInNewTab,
    CopyLink,
    Inspect,
}

/// One row of the page context menu: an action (label + enabled) or a rule.
enum MenuRow {
    Action(MenuAction, String, bool),
    Sep,
}

/// Row metrics for [`paint_context_menu`].
const CTX_ROW_H: f32 = 24.0;
const CTX_SEP_H: f32 = 7.0;
const CTX_PAD_X: f32 = 13.0;
const CTX_PAD_Y: f32 = 5.0;

/// Paint the page context menu at `anchor` and report the clicked row. The
/// rect is returned too: the outside-click test runs at the start of the
/// next frame, before this frame's layout exists.
fn paint_context_menu(
    ui: &egui::Ui,
    page: egui::Rect,
    anchor: egui::Pos2,
    rows: &[MenuRow],
) -> (Option<MenuAction>, egui::Rect) {
    let bg: egui::Color32 = egui::Color32::from_rgba_unmultiplied(0x1b, 0x1c, 0x1f, 245);
    let border: egui::Color32 = egui::Color32::from_rgb(0x3a, 0x3d, 0x44);
    let hover: egui::Color32 = egui::Color32::from_rgb(0x2f, 0x3a, 0x4d);
    let fg: egui::Color32 = egui::Color32::from_rgb(0xd7, 0xdb, 0xe0);
    let dim: egui::Color32 = egui::Color32::from_rgb(0x6c, 0x71, 0x78);
    let rule: egui::Color32 = egui::Color32::from_rgb(0x33, 0x36, 0x3c);
    let font = egui::FontId::proportional(12.5);

    let painter = ui.painter().with_clip_rect(page);
    // width fits the widest label (floor 150 so short menus still look right)
    let mut w = 150.0f32;
    for row in rows {
        if let MenuRow::Action(_, label, _) = row {
            let galley = painter.layout_no_wrap(label.clone(), font.clone(), fg);
            w = w.max(galley.size().x + 2.0 * CTX_PAD_X);
        }
    }
    let h = rows
        .iter()
        .map(|r| match r {
            MenuRow::Action(..) => CTX_ROW_H,
            MenuRow::Sep => CTX_SEP_H,
        })
        .sum::<f32>()
        + 2.0 * CTX_PAD_Y;
    // open down-right of the click, flipped/shifted to stay inside the page
    let rect = egui::Rect::from_min_size(
        egui::pos2(
            anchor.x.min(page.right() - w - 4.0).max(page.left() + 4.0),
            anchor.y.min(page.bottom() - h - 4.0).max(page.top() + 4.0),
        ),
        egui::vec2(w, h),
    );
    painter.rect_filled(rect, 6.0, bg);
    painter.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );

    let mut out = None;
    let mut y = rect.top() + CTX_PAD_Y;
    for (i, row) in rows.iter().enumerate() {
        match row {
            MenuRow::Sep => {
                let ly = y + CTX_SEP_H * 0.5;
                painter.line_segment(
                    [
                        egui::pos2(rect.left() + 8.0, ly),
                        egui::pos2(rect.right() - 8.0, ly),
                    ],
                    egui::Stroke::new(1.0, rule),
                );
                y += CTX_SEP_H;
            }
            MenuRow::Action(action, label, enabled) => {
                let r = egui::Rect::from_min_size(
                    egui::pos2(rect.left() + 4.0, y),
                    egui::vec2(w - 8.0, CTX_ROW_H),
                );
                let resp = ui.interact(r, ui.scope_id().with(("ctx_row", i)), egui::Sense::click());
                if *enabled && resp.hovered() {
                    painter.rect_filled(r, 4.0, hover);
                }
                painter.text(
                    egui::pos2(r.left() + CTX_PAD_X - 4.0, r.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    font.clone(),
                    if *enabled { fg } else { dim },
                );
                if *enabled && resp.clicked() {
                    out = Some(*action);
                }
                y += CTX_ROW_H;
            }
        }
    }
    (out, rect)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // deferred cookie-jar write (worker mutations inside the same second
        // as the last write wait here; see cookie::flush)
        cookie::flush();
        // tab store: persists the URL list whenever a load or tab action
        // changed it (the digest gate keeps idle frames off the disk)
        self.sync_session();
        // apply any finished page load (stale generations are dropped inside)
        {
            let (pw, ph) = self.vp_size;
            self.drain_results(pw, ph);
        }
        // F11 toggles real (viewport) fullscreen; non-repeat so a held key
        // doesn't strobe.
        if ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::F11,
                        pressed: true,
                        repeat: false,
                        ..
                    }
                )
            })
        }) {
            self.fullscreen = !self.fullscreen;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }

        // ---- inspector shortcuts: Ctrl+Shift+C (or F12) toggles the mode,
        // Esc leaves it, Ctrl+C copies a drag selection ----
        let (toggle_inspect, inspect_f12, escape) = ui.input(|i| {
            (
                i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::C),
                i.key_pressed(egui::Key::F12),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if toggle_inspect || inspect_f12 {
            self.menu = None;
            self.set_inspect(!self.inspect);
        } else if escape {
            // Esc closes the context menu before it leaves inspector mode
            if self.menu.take().is_none() && self.inspect {
                self.set_inspect(false);
            }
        }
        if self.selection.is_some()
            && !self.address_focused
            && ui.input(|i| {
                i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::C)
            })
        {
            if let Some(text) = self.selected_text() {
                if !text.is_empty() {
                    eprintln!("[select] copied {} chars", text.chars().count());
                    ui.ctx().copy_text(text);
                }
            }
        }

        // Actions are collected during the panel closures and applied after
        // them (the page size is only known once the panels claimed their
        // space, and it keeps the borrows simple).
        let mut activate: Option<usize> = None;
        let mut close: Option<usize> = None;
        let mut new_tab = false;
        let mut go_back = false;
        let mut go: Option<String> = None; // address submit or link click

        // A press outside the open context menu dismisses it. The rect comes
        // from the last frame's paint (this frame's menu may not be laid out
        // yet). `menu_dismissed` also suppresses the page press that did it,
        // so closing the menu never starts a text selection.
        let mut menu_dismissed = false;
        if let Some(rect) = self.menu.as_ref().and_then(|m| m.rect) {
            let pressed_outside = ui.input(|i| {
                i.pointer.any_pressed()
                    && i.pointer
                        .interact_pos()
                        .map_or(true, |p| !rect.contains(p))
            });
            if pressed_outside {
                self.menu = None;
                menu_dismissed = true;
            }
        }

        // ---- tab strip (the custom titlebar) ----
        let titles: Vec<String> = self.tabs.iter().map(|t| short_title(&t.url)).collect();
        egui::Panel::top("tab_strip")
            .exact_size(TAB_H)
            .frame(egui::Frame::NONE.fill(ui_chrome::CHROME_BG))
            .show_separator_line(false)
            .show(ui, |ui| {
                for ev in ui_chrome::tab_strip(ui, &titles, self.active) {
                    match ev {
                        ui_chrome::TabEvent::Activate(i) => activate = Some(i),
                        ui_chrome::TabEvent::Close(i) => close = Some(i),
                        ui_chrome::TabEvent::NewTab => new_tab = true,
                    }
                }
            });

        // ---- toolbar: back button + address pill ----
        egui::Panel::top("toolbar")
            .exact_size(ADDR_H)
            .frame(egui::Frame::NONE.fill(ui_chrome::CHROME_BG))
            .show_separator_line(false)
            .show(ui, |ui| {
                let can_back = !self.back_stack.is_empty();
                for ev in ui_chrome::toolbar(ui, &mut self.address, can_back, self.inspect) {
                    match ev {
                        ui_chrome::ToolbarEvent::Back => go_back = true,
                        ui_chrome::ToolbarEvent::Submit(q) => {
                            self.address = q;
                            go = Some(address_target(&self.address));
                        }
                        ui_chrome::ToolbarEvent::Focused(f) => {
                            if self.address_focused && !f {
                                // blur without submit (e.g. Escape) → restore URL
                                self.address = self.page.url.clone();
                            }
                            self.address_focused = f;
                        }
                        ui_chrome::ToolbarEvent::Settings => {
                            self.settings_open = true;
                        }
                        ui_chrome::ToolbarEvent::Inspect => {
                            self.set_inspect(!self.inspect);
                        }
                    }
                }
            });

        // ---- page viewport (must come last: panels claim space first) ----
        let mut vp = self.last_size;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let avail = ui.available_size();
                // one texture pixel per logical point (see App::new)
                let pw = avail.x.max(1.0) as usize;
                let ph = avail.y.max(1.0) as usize;
                vp = (pw, ph);

                if (pw, ph) != self.vp_size {
                    // live resize: the old code re-ran the WHOLE pipeline
                    // (fetch+parse+layout) synchronously here — the main
                    // freeze. It now goes to the worker; debounce ~150ms so a
                    // drag storm coalesces into one relayout (worker-side
                    // coalescing drops the queue in between anyway).
                    // shortcut: fixed debounce, no measurement-based decision —
                    // even cache-hit re-layouts of real pages are well over
                    // 100ms (see "[nav] worker finished" ms in the logs).
                    self.vp_size = (pw, ph);
                    self.pending_resize = Some(Instant::now());
                }
                if let Some(t) = self.pending_resize {
                    if t.elapsed() >= Duration::from_millis(150) {
                        self.pending_resize = None;
                        eprintln!("[nav] resize → relayout at {pw}x{ph}");
                        // re-layout only: no refetch/reparse/script re-run
                        self.dispatch_relayout(pw, ph);
                    }
                }

                // form typing: route keys to the focused field (the address
                // bar owns the keyboard while it's focused). Enter submits
                // (shell logs + fires change with the final value), Escape
                // blurs (change fired).
                if self.forms.focused.is_some() && !self.address_focused && !self.inspect {
                    let field = self.forms.focused.unwrap();
                    let mut actions: Vec<KeyAction> = Vec::new();
                    ui.input(|i| {
                        for ev in &i.events {
                            match ev {
                                // printable text (the actual typed chars)
                                egui::Event::Text(t) => {
                                    for c in t.chars() {
                                        if !c.is_control() {
                                            actions.push(KeyAction::Insert(c));
                                        }
                                    }
                                }
                                egui::Event::Key {
                                    key,
                                    pressed: true,
                                    repeat: false,
                                    ..
                                } => {
                                    let a = route_key(*key, true, false);
                                    if a != KeyAction::None {
                                        actions.push(a);
                                    }
                                }
                                _ => {}
                            }
                        }
                    });
                    // keyboard events dispatched to the worker alongside the
                    // value application (keydown sees the post-insertion
                    // value — honest shortcut, a pre-image pass would need a
                    // second round trip per keystroke)
                    let mut keydown: Option<String> = None;
                    let mut dirty = false;
                    let mut typed = false;
                    let mut change_now: Option<String> = None;
                    let mut got_submit = false;
                    for a in actions {
                        match a {
                            KeyAction::Insert(c) => {
                                self.forms.values.entry(field).or_default().push(c);
                                dirty = true;
                                typed = true;
                            }
                            KeyAction::Backspace => {
                                if let Some(v) = self.forms.values.get_mut(&field) {
                                    v.pop();
                                }
                                dirty = true;
                                typed = true;
                                keydown = Some("Backspace".into());
                            }
                            KeyAction::Submit => {
                                let name = self
                                    .form_names
                                    .get(&field)
                                    .cloned()
                                    .unwrap_or_default();
                                let val = self.forms.values.get(&field).cloned().unwrap_or_default();
                                eprintln!("[form] submit {name}={val}");
                                // Enter → real submission: change with the
                                // final value AND the submission trigger (the
                                // worker sends the form's submit event after
                                // the field's keydown/input/change listeners)
                                keydown = Some("Enter".into());
                                change_now = Some(val);
                                got_submit = true;
                            }
                            KeyAction::Blur => {
                                // Escape → change + blur (the value stays
                                // what it was; the field gives up focus)
                                keydown = Some("Escape".into());
                                change_now = self.forms.values.get(&field).cloned();
                                self.forms.focused = None;
                                dirty = true;
                            }
                            KeyAction::None => {}
                        }
                    }
                    if dirty || got_submit {
                        // push the new value to the worker so it re-lays out
                        // with the updated text, firing listeners: `input`
                        // per keystroke (real-input-event semantics; the
                        // per-keystroke job is also what syncs the value),
                        // `change` on Enter/Escape with the final value. The
                        // Enter case rides the submission trigger.
                        let mut kinds: Vec<&str> = Vec::new();
                        if keydown.is_some() {
                            kinds.push("keydown");
                        }
                        if typed {
                            kinds.push("input");
                        } else if change_now.is_some() {
                            kinds.push("change");
                        }
                        if kinds.is_empty() {
                            kinds.push("change");
                        }
                        self.dispatch_event_ext(field, &kinds, change_now.or_else(|| self.forms.values.get(&field).cloned()), keydown.as_deref(), got_submit, pw, ph);
                    }
                }
                if !ui.ctx().egui_wants_keyboard_input() && !self.address_focused {
                    let wheel = ui.input(|i| i.smooth_scroll_delta());
                    if wheel.y != 0.0 {
                        self.set_scroll(self.scroll - wheel.y, pw, ph);
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                        self.set_scroll(self.scroll + 40.0, pw, ph);
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                        self.set_scroll(self.scroll - 40.0, pw, ph);
                    }
                }

                // CPU pixels → GPU texture, only when something changed
                if self.dirty {
                    let img = pixels_to_image(&self.pixels, self.last_size.0, self.last_size.1);
                    match &mut self.tex {
                        Some(t) => t.set(img, egui::TextureOptions::NEAREST),
                        None => {
                            self.tex = Some(ui.ctx().load_texture(
                                "page",
                                img,
                                egui::TextureOptions::NEAREST,
                            ))
                        }
                    }
                    self.dirty = false;
                }

                if let Some(tex) = &self.tex {
                    let (iw, ih) = self.last_size;
                    // A resize relayout is in flight until the worker's wire
                    // lands: the buffer still holds the old viewport. Draw it
                    // stretched over the new one — drawing it at its old size
                    // left the rest of a fullscreen window blank ("everything
                    // dismantled") for as long as the reflow took. Pointer
                    // coordinates map back into buffer space.
                    let kx = iw as f32 / pw as f32;
                    let ky = ih as f32 / ph as f32;
                    let stale = iw != pw || ih != ph;
                    let resp = ui.add(
                        egui::Image::new(&*tex)
                            .fit_to_exact_size(egui::vec2(pw as f32, ph as f32))
                            .texture_options(if stale {
                                egui::TextureOptions::LINEAR
                            } else {
                                egui::TextureOptions::NEAREST
                            })
                            .sense(egui::Sense::click_and_drag()),
                    );
                    let to_buf = |p: egui::Pos2| {
                        egui::pos2((p.x - resp.rect.min.x) * kx, (p.y - resp.rect.min.y) * ky)
                    };
                    // scrollbar geometry, buffer-local (one buffer pixel per
                    // logical point while the buffer matches the viewport)
                    let (bar_x0, bar_x1) = scrollbar_strip(iw);
                    let page = page_h(ih);
                    let doc_h = self.page.laid.height;
                    let bar_y0 = PADDING as f32;
                    let bar_y1 = ih as f32 - PADDING as f32;
                    let in_bar = |p: egui::Pos2| {
                        let lx = to_buf(p).x;
                        let ly = to_buf(p).y;
                        lx >= bar_x0 && lx <= bar_x1 && ly >= bar_y0 && ly <= bar_y1
                    };
                    // scrollbar press: grab the thumb, or jump the track so
                    // the thumb centres on the pointer (then keep dragging —
                    // the grab offset is half a thumb)
                    let press = ui
                        .input(|i| i.pointer.primary_pressed().then(|| i.pointer.interact_pos()))
                        .flatten()
                        .filter(|p| resp.rect.contains(*p) && in_bar(*p));
                    if let Some(p) = press {
                        if let Some((ty, th)) = scrollbar_thumb(doc_h, page, self.scroll) {
                            let ly = to_buf(p).y - bar_y0;
                            if ly >= ty && ly <= ty + th {
                                self.bar_grab = Some(ly - ty);
                            } else {
                                self.bar_grab = Some(th * 0.5);
                                self.set_scroll(scroll_from_thumb(doc_h, page, ly - th * 0.5), pw, ph);
                            }
                        }
                    }
                    if let Some(grab) = self.bar_grab {
                        if resp.dragged() || resp.drag_started() {
                            if let Some(p) = resp.interact_pointer_pos() {
                                let ly = to_buf(p).y - bar_y0;
                                self.set_scroll(scroll_from_thumb(doc_h, page, ly - grab), pw, ph);
                            }
                        }
                        if resp.drag_stopped() || !ui.input(|i| i.pointer.any_down()) {
                            self.bar_grab = None;
                        }
                    }
                    // link hit-test — identical math to the old minifb shell
                    // (a press inside the scrollbar strip belongs to the bar).
                    // In inspector mode a click pins the element under the
                    // pointer and never navigates.
                    if resp.clicked() && !resp.interact_pointer_pos().map_or(false, in_bar) {
                        if self.inspect {
                            if let Some(p) = resp.interact_pointer_pos() {
                                let rx = to_buf(p).x - PADDING as f32;
                                let my_doc = to_buf(p).y - PADDING as f32;
                                self.pinned = self
                                    .page
                                    .laid
                                    .hit_hover(rx, my_doc, self.scroll)
                                    .map(|(n, _)| n);
                                eprintln!("[inspect] pin {:?}", self.pinned);
                            }
                        } else if let Some(p) = resp.interact_pointer_pos() {
                            let rx = to_buf(p).x - PADDING as f32;
                            let my_doc = to_buf(p).y - PADDING as f32;
                            // form controls win over links underneath them
                            let fh = self.page.laid.hit_test_form(rx, my_doc, self.scroll);
                            if let Some(fi) = fh {
                                let fb = self.page.laid.forms[fi];
                                self.on_form_click(fb, pw, ph);
                            } else {
                                let hit = self.page.laid.runs.iter().find(|r| {
                                    let ry = if r.fixed { my_doc } else { my_doc + self.scroll };
                                    r.link.is_some()
                                        && rx >= r.x
                                        && rx <= r.x + r.width
                                        && ry >= r.y
                                        && ry <= r.y + r.style.size * 1.35
                                });
                                if let Some(hit) = hit {
                                    let href = hit.link.clone().unwrap();
                                    if let Some(next) = resolve(&self.page.url, &href) {
                                        // async preventDefault round trip: the
                                        // worker dispatches click listeners on
                                        // the anchor (resolved by href — layout
                                        // runs carry the href, not a node id)
                                        // and reports whether one called
                                        // preventDefault; we navigate only when
                                        // it comes back clean (`drain_results`)
                                        let id = self.nav.begin();
                                        eprintln!("[nav] {} → {next} (click listeners first)", self.page.url);
                                        let req = LoadReq::Event {
                                            id,
                                            url: self.page.url.clone(),
                                            max_w: page_w(pw),
                                            vh: page_h(ph),
                                            node: 0,
                                            kinds: vec!["click".into()],
                                            value: None,
                                            key: None,
                                            href: Some(href.to_string()),
                                            submit: false,
                                            forms: self.forms.clone(),
                                            engine: self.conf.engine,
                                        };
                                        if self.jobs.send(req).is_err() {
                                            eprintln!("[nav] loader thread unavailable, link click dropped");
                                        }
                                        self.expect_nav = false;
                                        self.pending_link = Some((id, next));
                                    }
                                } else if let Some(fb) = self.label_form_at(rx, my_doc) {
                                    // a click on `<label>` content activates
                                    // the control the label names - exactly the
                                    // click path a control takes (checkbox/
                                    // radio toggle, text focus, submit)
                                    self.on_form_click(fb, pw, ph);
                                } else {
                                    // click on empty space blurs any focused
                                    // field — firing its change listeners
                                    // (final value) instead of a bare relayout
                                    if let Some(prev) = self.forms.focused.take() {
                                        let val = self.forms.values.get(&prev).cloned();
                                        self.dispatch_event(prev, &["change"], val, None, pw, ph);
                                    }
                                }
                            }
                        }
                    }

                    // ---- right-click menu ----
                    // Opens at the pointer and remembers what was under it:
                    // a link target adds its own rows, a hit-test node enables
                    // "Inspect element". Right-clicking the scrollbar is left
                    // to the shell (it would only offer a page menu).
                    if resp.secondary_clicked() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            if !in_bar(p) {
                                let rx = to_buf(p).x - PADDING as f32;
                                let my_doc = to_buf(p).y - PADDING as f32;
                                let node = self
                                    .page
                                    .laid
                                    .hit_hover(rx, my_doc, self.scroll)
                                    .map(|(n, _)| n);
                                let link = self.link_at(rx, my_doc, self.scroll);
                                eprintln!("[menu] open at ({:.0}, {:.0}) link={link:?}", p.x, p.y);
                                self.menu = Some(ContextMenu {
                                    pos: p,
                                    link,
                                    node,
                                    rect: None,
                                });
                            }
                        }
                    }

                    // ---- drag to highlight text ----
                    // A press on page text starts (or clears) the selection;
                    // moving with the button held extends it toward the
                    // pointer; release keeps it. Form controls own their
                    // press, and inspector mode selects elements instead.
                    let sel_press = ui
                        .input(|i| i.pointer.primary_pressed().then(|| i.pointer.interact_pos()))
                        .flatten()
                        .filter(|p| {
                            resp.rect.contains(*p)
                                && !in_bar(*p)
                                && self.menu.is_none()
                                && !menu_dismissed
                        });
                    if let Some(p) = sel_press {
                        let rx = to_buf(p).x - PADDING as f32;
                        let my_doc = to_buf(p).y - PADDING as f32;
                        self.select_anchor = if self.inspect
                            || self.page.laid.hit_test_form(rx, my_doc, self.scroll).is_some()
                        {
                            None
                        } else {
                            self.text_pos_at(rx, my_doc, self.scroll)
                        };
                        self.selection = self
                            .select_anchor
                            .map(|a| Selection { anchor: a, head: a });
                    }
                    if let Some(anchor) = self.select_anchor {
                        if ui.input(|i| i.pointer.primary_down()) {
                            if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                                let rx = to_buf(p).x - PADDING as f32;
                                // clamp so dragging past the viewport edges
                                // still lands on the first/last line
                                let my_doc = (to_buf(p).y - PADDING as f32)
                                    .clamp(0.0, page_h(ih));
                                if let Some(head) = self.text_pos_at(rx, my_doc, self.scroll) {
                                    let ordered = if (anchor.run, anchor.ch) <= (head.run, head.ch)
                                    {
                                        Selection { anchor, head }
                                    } else {
                                        Selection { anchor: head, head: anchor }
                                    };
                                    self.selection = Some(ordered);
                                }
                            }
                        } else {
                            self.select_anchor = None;
                            // a plain click leaves no highlight behind
                            if self.selection.map_or(false, |s| s.anchor == s.head) {
                                self.selection = None;
                            }
                        }
                    }

                    // hover: read the pointer (input is read-only here —
                    // never synthesized), hit-test with the SAME math as the
                    // link hit-test above, and rebuild when the effective
                    // hovered set changed
                    let hover_pos = ui.input(|i| i.pointer.hover_pos());
                    // Browsers update :hover on pointer motion, not on layout
                    // — see `last_ptr`. A press also counts (same spot, new
                    // target under the pointer, e.g. after an overlay closed).
                    let ptr_active =
                        hover_pos != self.last_ptr || ui.input(|i| i.pointer.any_pressed());
                    self.last_ptr = hover_pos;
                    let inside = hover_pos.filter(|p| resp.rect.contains(*p));
                    let over_bar = inside.map_or(false, in_bar);
                    let hit = inside.filter(|_| !over_bar).and_then(|p| {
                        let rx = to_buf(p).x - PADDING as f32;
                        let my_doc = to_buf(p).y - PADDING as f32;
                        self.page.laid.hit_hover(rx, my_doc, self.scroll)
                    });
                    if over_bar {
                        // the bar is chrome, not page content: no link hover,
                        // no `cursor: pointer` leaking through it
                        ui.ctx().set_cursor_icon(if self.bar_grab.is_some() {
                            egui::CursorIcon::Grabbing
                        } else {
                            egui::CursorIcon::Default
                        });
                    } else if inside.is_some() {
                        // cursor follows the hit; no hit (page background)
                        // = Default. Only written inside the page image —
                        // over the chrome egui's own per-frame output wins
                        let icon = if self.inspect {
                            egui::CursorIcon::Crosshair
                        } else {
                            hit.and_then(|(_, c)| c)
                                .map(cursor_icon_of)
                                .unwrap_or(egui::CursorIcon::Default)
                        };
                        ui.ctx().set_cursor_icon(icon);
                    }
                    let new_hovered = if self.inspect || self.bar_grab.is_some() {
                        // the inspector highlights without restyling the page
                        None
                    } else {
                        hit.map(|(n, _)| n)
                    };
                    if hover_should_relayout(new_hovered, self.hovered, ptr_active, self.pending.is_some() || self.pending_link.is_some(), ui.input(|i| i.is_scrolling())) {
                        self.hovered = new_hovered;
                        self.dispatch_relayout(pw, ph);
                        self.hover_wait = true;
                        ui.ctx().request_repaint();
                    }

                    // ---- text selection highlight (drag) ----
                    if let Some(sel) = self.selection {
                        let painter = ui.painter().with_clip_rect(resp.rect);
                        let fill =
                            egui::Color32::from_rgba_unmultiplied(0x4a, 0x8f, 0xf0, 90);
                        for i in sel.anchor.run..=sel.head.run {
                            let Some(r) = self.page.laid.runs.get(i) else {
                                continue;
                            };
                            let from = if i == sel.anchor.run {
                                sel.anchor.ch
                            } else {
                                0
                            }
                            .min(r.text.len());
                            let to = if i == sel.head.run {
                                sel.head.ch
                            } else {
                                r.text.len()
                            }
                            .min(r.text.len())
                            .max(from);
                            if from >= to {
                                continue;
                            }
                            let x0 = r.x + self.fonts.measure(&r.text[..from], &r.style);
                            let x1 = r.x + self.fonts.measure(&r.text[..to], &r.style);
                            let sy = if r.fixed { r.y } else { r.y - self.scroll };
                            let rect = egui::Rect::from_min_size(
                                resp.rect.min
                                    + egui::vec2(
                                        (PADDING as f32 + x0) * kx,
                                        (PADDING as f32 + sy) * ky,
                                    ),
                                egui::vec2(
                                    ((x1 - x0).max(1.0)) * kx,
                                    r.style.line_height() * ky,
                                ),
                            );
                            if rect.intersects(resp.rect) {
                                painter.rect_filled(rect, 1.0, fill);
                            }
                        }
                    }

                    // ---- element inspector overlay ----
                    if self.inspect {
                        let hover_node = hit.map(|(n, _)| n);
                        let painter = ui.painter().with_clip_rect(resp.rect);
                        // the pinned box stays visible while the pointer is
                        // elsewhere; the hovered box paints over it
                        if let Some(n) = self.pinned.filter(|n| Some(*n) != hover_node) {
                            if let Some(info) = self.page.elements.get(&n) {
                                paint_inspect_box(&painter, resp.rect, self.scroll, info, true);
                            }
                        }
                        if let Some(info) = hover_node.and_then(|n| self.page.elements.get(&n)) {
                            paint_inspect_box(
                                &painter,
                                resp.rect,
                                self.scroll,
                                info,
                                self.pinned == hover_node,
                            );
                        }
                        if let Some(info) = self
                            .pinned
                            .or(hover_node)
                            .and_then(|n| self.page.elements.get(&n))
                        {
                            paint_inspect_panel(ui, resp.rect, info, self.pinned.is_some());
                        }
                    }

                    // ---- page-load progress bar ----
                    // `pending` is set by dispatch() (address bar, link click,
                    // tab switch, reload) and cleared when the worker's page
                    // lands, so the bar is honest: it runs exactly while a new
                    // page is on its way, and the old page stays readable
                    // underneath it.
                    if self.pending.is_some() {
                        let painter = ui.painter().with_clip_rect(resp.rect);
                        let track = egui::Rect::from_min_max(
                            resp.rect.left_top(),
                            egui::pos2(resp.rect.right(), resp.rect.top() + LOAD_BAR_H),
                        );
                        let (r, g, b) = LOAD_BAR_RGB;
                        // faint full-width track (also the "no estimate" cue)
                        painter.rect_filled(
                            track,
                            0.0,
                            egui::Color32::from_rgba_unmultiplied(r, g, b, 48),
                        );
                        // indeterminate sweep: one highlight crosses the
                        // track, wrapping around (no byte-level progress is
                        // exposed by the loader, so no fake percentage)
                        let t = ui.input(|i| i.time) as f32;
                        let span = 160.0f32;
                        let sweep = track.width() + span;
                        let x0 = track.left() - span + (t * 420.0) % sweep.max(1.0);
                        let seg = egui::Rect::from_min_max(
                            egui::pos2(x0, track.top()),
                            egui::pos2(x0 + span, track.bottom()),
                        )
                        .intersect(track);
                        if seg.is_positive() {
                            painter.rect_filled(
                                seg,
                                0.0,
                                egui::Color32::from_rgb(r, g, b),
                            );
                        }
                        // the sweep moves every frame, not just on input
                        ui.ctx().request_repaint();
                    }

                    // ---- page context menu (last, so it paints over the
                    // page and its overlays) ----
                    if let Some(menu) = self.menu.clone() {
                        let mut rows: Vec<MenuRow> = vec![
                            MenuRow::Action(
                                MenuAction::Back,
                                "Back".into(),
                                !self.back_stack.is_empty(),
                            ),
                            MenuRow::Action(MenuAction::Reload, "Reload".into(), true),
                            MenuRow::Sep,
                            MenuRow::Action(
                                MenuAction::Copy,
                                "Copy".into(),
                                self.selected_text().is_some_and(|t| !t.is_empty()),
                            ),
                            MenuRow::Action(
                                MenuAction::SelectAll,
                                "Select all".into(),
                                !self.page.laid.runs.is_empty(),
                            ),
                        ];
                        if menu.link.is_some() {
                            rows.push(MenuRow::Sep);
                            rows.push(MenuRow::Action(
                                MenuAction::OpenLink,
                                "Open link".into(),
                                true,
                            ));
                            rows.push(MenuRow::Action(
                                MenuAction::OpenInNewTab,
                                "Open link in new tab".into(),
                                true,
                            ));
                            rows.push(MenuRow::Action(
                                MenuAction::CopyLink,
                                "Copy link address".into(),
                                true,
                            ));
                        }
                        if menu
                            .node
                            .is_some_and(|n| self.page.elements.contains_key(&n))
                        {
                            rows.push(MenuRow::Sep);
                            rows.push(MenuRow::Action(
                                MenuAction::Inspect,
                                "Inspect element".into(),
                                true,
                            ));
                        }
                        let (action, rect) = paint_context_menu(ui, resp.rect, menu.pos, &rows);
                        if let Some(m) = self.menu.as_mut() {
                            m.rect = Some(rect);
                        }
                        match action {
                            Some(MenuAction::Back) => {
                                self.menu = None;
                                go_back = true;
                            }
                            Some(MenuAction::Reload) => {
                                self.menu = None;
                                eprintln!("[menu] reload");
                                let url = self.page.url.clone();
                                self.dispatch(&url, pw, ph, self.scroll, false);
                            }
                            Some(MenuAction::Copy) => {
                                self.menu = None;
                                if let Some(text) = self.selected_text() {
                                    if !text.is_empty() {
                                        eprintln!("[menu] copy {} chars", text.chars().count());
                                        ui.ctx().copy_text(text);
                                    }
                                }
                            }
                            Some(MenuAction::SelectAll) => {
                                self.menu = None;
                                self.select_all();
                                eprintln!("[menu] select all");
                            }
                            Some(MenuAction::OpenLink) => {
                                self.menu = None;
                                if let Some(url) = menu.link.clone() {
                                    eprintln!("[menu] open link {url}");
                                    self.navigate(&url, pw, ph);
                                }
                            }
                            Some(MenuAction::OpenInNewTab) => {
                                self.menu = None;
                                if let Some(url) = menu.link.clone() {
                                    // the tab loads on its first activation,
                                    // exactly like a restored session tab
                                    eprintln!("[menu] open link in new tab: {url}");
                                    self.tabs.push(Tab { url, scroll: 0.0 });
                                }
                            }
                            Some(MenuAction::CopyLink) => {
                                self.menu = None;
                                if let Some(url) = &menu.link {
                                    eprintln!("[menu] copy link address {url}");
                                    ui.ctx().copy_text(url.clone());
                                }
                            }
                            Some(MenuAction::Inspect) => {
                                self.menu = None;
                                self.set_inspect(true);
                                if let Some(n) = menu.node {
                                    if self.page.elements.contains_key(&n) {
                                        self.pinned = Some(n);
                                    }
                                }
                            }
                            None => {}
                        }
                    }
                }

                // RSS overlay, bottom-right (small and dim)
                let rss = format!("RSS {:.1} MB", working_set_mb());
                ui.painter().text(
                    ui.max_rect().right_bottom() - egui::vec2(10.0, 10.0),
                    egui::Align2::RIGHT_BOTTOM,
                    rss,
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(0x5f, 0x63, 0x68),
                );

                // ---- ⚙ settings overlay — drawn LAST so it sits over the
                // page; the choice persists immediately and applies from the
                // next navigation (here: re-load the current page through the
                // new engine, the honest "no hot-swap" flow) ----
                if self.settings_open {
                    for ev in ui_chrome::settings_overlay(
                        ui,
                        self.conf.engine,
                        crate::runtime::EngineChoice::B.available(),
                        self.conf.fonts,
                    ) {
                        match ev {
                            ui_chrome::SettingsEvent::SetEngine(choice) => {
                                self.conf.engine = choice;
                                self.conf.save();
                                eprintln!("[settings] engine set to {}", choice.as_conf());
                                // note (not yet shipped): a per-engine reload
                                // of OTHER tabs happens on their next load;
                                // this reload covers the current one
                                let target = self.page.url.clone();
                                self.dispatch(&target, pw, ph, 0.0, false);
                            }
                            ui_chrome::SettingsEvent::SetFonts(choice) => {
                                self.conf.fonts = choice;
                                self.conf.save();
                                eprintln!("[settings] fonts set to {}", choice.as_conf());
                                // shortcut: applies on the NEXT navigation —
                                // no reload here (the engine row reloads
                                // because it swaps the running interpreter;
                                // fonts only change what the next load
                                // fetches, so the current page is untouched).
                                // A form-submit load fired by a worker Event
                                // also keeps the loaded page's policy; the
                                // next dispatch() (link / address bar / reload)
                                // is what adopts the new one.
                            }
                            ui_chrome::SettingsEvent::Close => {
                                self.settings_open = false;
                            }
                        }
                    }
                }
            });

        let (pw, ph) = vp;

        // ---- apply the deferred actions with the real page size ----
        if let Some(i) = activate {
            if i != self.active && i < self.tabs.len() {
                // Only the scroll offset moves tabs now: a tab's `url` is the
                // page it is loading OR showing, set at dispatch/land time.
                // Copying `self.page.url` here used to overwrite the target
                // with the previous tab's page while a switch was still in
                // flight — the labels converged and clicking them reloaded the
                // same page, which read as "switching doesn't switch".
                self.tabs[self.active].scroll = self.scroll;
                self.active = i;
                let (url, saved) = (self.tabs[i].url.clone(), self.tabs[i].scroll);
                self.reload_active(&url, pw, ph, saved);
            }
        }
        if let Some(i) = close {
            if self.tabs.len() > 1 && i < self.tabs.len() {
                self.tabs.remove(i);
                if self.active >= self.tabs.len() {
                    self.active = self.tabs.len() - 1;
                } else if self.active > i {
                    self.active -= 1;
                }
                let url = self.tabs[self.active].url.clone();
                let saved = self.tabs[self.active].scroll;
                self.reload_active(&url, pw, ph, saved);
            }
        }
        if new_tab {
            // same rule as activate: keep the finished tab's URL (it is
            // already in `tabs`), carry only its scroll
            self.tabs[self.active].scroll = self.scroll;
            self.tabs.push(Tab {
                url: "https://example.com".into(),
                scroll: 0.0,
            });
            self.active = self.tabs.len() - 1;
            self.reload_active("https://example.com", pw, ph, 0.0);
        }
        if go_back {
            if let Some(prev) = self.back_stack.pop() {
                self.reload_active(&prev, pw, ph, 0.0);
            }
        }
        if let Some(target) = go {
            self.navigate(&target, pw, ph);
        }

        // window title follows the page (capture script matches "Mazda*");
        // "…" marks a load in flight
        let title = format!(
            "Mazda — {}{}",
            self.page.url,
            if self.pending.is_some() { " …" } else { "" }
        );
        if title != self.title {
            self.title = title.clone();
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
        // wheel momentum lives in egui's smoothed delta, which only advances
        // when a frame runs: without this, one wheel notch applied a slice of
        // its delta and then sat still until the next input event
        if ui.input(|i| i.smooth_scroll_delta().y != 0.0) {
            ui.ctx().request_repaint();
        }
        // while a load, a link-click decision or a hover relayout is in
        // flight, poll fast so the result lands promptly; otherwise the RSS
        // overlay ticks at 1 Hz
        ui.ctx().request_repaint_after(if self.pending.is_some()
            || self.pending_link.is_some()
            || self.hover_wait
        {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(1)
        });
    }
}

/// Short tab-label from a URL: host (minus www.) or the last path segment.
fn short_title(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = rest.split('/').next().unwrap_or(rest);
    let h = host.strip_prefix("www.").unwrap_or(host);
    if h.is_empty() {
        "New tab".into()
    } else {
        h.to_string()
    }
}

// ---------------------------------------------------------------------------
// Async page loading. The full pipeline (fetch → parse → cascade → layout →
// image decode) used to run on the egui update thread, so clicking a link,
// opening a tab or even resizing froze the window for seconds. It now runs on
// one long-lived worker thread; the UI thread only sends jobs and drains
// results, guarded by a generation counter so a slow old load can never
// overwrite a newer navigation.
// ---------------------------------------------------------------------------

/// Monotonic request id: only the newest navigation's result is accepted.
struct NavGate {
    next: u64,
}

impl NavGate {
    fn new() -> Self {
        NavGate { next: 0 }
    }
    /// Take a fresh id for a new request (navigation / reload / resize).
    fn begin(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    /// Should the result of request `id` be applied? Only the latest wins.
    fn accept(&self, id: u64) -> bool {
        id == self.next
    }
}

/// A page-load job for the worker thread. Plain data (`Send`): URL + the
/// viewport metrics layout needs, plus the form interaction state the UI
/// thread owns (so a re-layout preserves what the user typed/focused).
enum LoadReq {
    /// full pipeline: fetch → parse → script → cascade → layout
    Load {
        id: u64,
        url: String,
        max_w: f32,
        vh: f32,
        /// which script runtime builds the page's scripts (selectable
        /// runtime). A relayout/event never switches engines: the kept
        /// page keeps its runtime; this only matters for loads.
        engine: EngineChoice,
        /// webfont policy for this load (`A` = site fonts, `B` = system
        /// fonts only). Like `engine`, a relayout/event never switches it -
        /// the settings choice applies on the NEXT navigation.
        fonts: FontsChoice,
        /// Phase 3: restore the DOM snapshot instead of re-fetching when one
        /// exists (tab switch / back). A fresh navigation passes false.
        restore: bool,
    },
    /// re-cascade + re-layout the kept page (resize / DOM change / hover
    /// change), carrying the UI's live form state + hover cell forward
    Relayout {
        id: u64,
        url: String,
        max_w: f32,
        vh: f32,
        forms: layout::FormState,
        /// hovered leaf DOM node (the sheet's `:hover` cell) — the shell
        /// re-dispatches when its hit test changes the effective set
        hover: Option<u32>,
        engine: EngineChoice,
    },
    /// a form control was activated (click/submit) or a text field changed:
    /// the worker mutates its DOM, fires each kind's listeners in order,
    /// re-lays out, re-emits. `href` targets a LINK click (layout runs
    /// carry the href, not a node id — the worker resolves the first
    /// matching anchor and bubbles from there).
    /// `submit` = this activation is a submission trigger: after the listed
    /// listeners run (clean), the worker sends the form's own `submit`
    /// listeners the submit event and executes the real GET/POST load.
    Event {
        id: u64,
        url: String,
        max_w: f32,
        vh: f32,
        /// DOM node id of the control (0 when `href` targeting is used)
        node: u32,
        /// event kinds to dispatch in order, e.g. ["keydown","input"]
        kinds: Vec<String>,
        /// text value to push into the control (text inputs), else None
        value: Option<String>,
        /// key name for keyboard events ("Backspace"/"Enter"/"Escape")
        key: Option<String>,
        /// raw href of the clicked link (click-decision flow)
        href: Option<String>,
        /// submission trigger (Enter in a field / submit-button click)
        submit: bool,
        forms: layout::FormState,
        engine: EngineChoice,
    },
    /// the viewport scrolled: dispatches scroll listeners only (no
    /// generation id — scroll events never gate other results, and the
    /// worker coalesces a run of them to the newest)
    Scroll { url: String, y: f32 },
}

impl LoadReq {
    /// The engine to build a (re)load with when this job falls back to
    /// `do_load` (kept page missing). Scroll never loads.
    fn engine(&self) -> EngineChoice {
        match self {
            LoadReq::Load { engine, .. } | LoadReq::Relayout { engine, .. } | LoadReq::Event { engine, .. } => *engine,
            LoadReq::Scroll { .. } => EngineChoice::A,
        }
    }

    /// The webfont policy a fresh navigation carries; relayout/event/scroll
    /// keep the current page's policy (settings apply on the next load).
    fn fonts_choice(&self) -> Option<FontsChoice> {
        match self {
            LoadReq::Load { fonts, .. } => Some(*fonts),
            _ => None,
        }
    }
}

impl LoadReq {
    fn id(&self) -> u64 {
        match self {
            LoadReq::Load { id, .. }
            | LoadReq::Relayout { id, .. }
            | LoadReq::Event { id, .. } => *id,
            // scroll never gates a result: it doesn't bump the UI's
            // generation counter and no result is sent for it
            LoadReq::Scroll { .. } => 0,
        }
    }
    fn url(&self) -> &str {
        match self {
            LoadReq::Load { url, .. }
            | LoadReq::Relayout { url, .. }
            | LoadReq::Event { url, .. }
            | LoadReq::Scroll { url, .. } => url,
        }
    }
}

/// What the UI does with a link-click listener decision.
#[derive(Debug, PartialEq, Eq)]
enum LinkDecision {
    /// listeners ran clean → start the navigation
    Navigate(String),
    /// a listener called preventDefault → cancel (never start)
    Cancel,
    /// no decision outstanding (defensive; can't happen while gated)
    Ignore,
}

/// Pure verdict for an arriving click decision (testable — no synthetic
/// input the UI could act on by itself). `pending` = the awaiting decision.
fn click_decision(pending: &Option<(u64, String)>, prevent_default: bool) -> LinkDecision {
    match pending {
        Some((_, url)) => {
            if prevent_default {
                LinkDecision::Cancel
            } else {
                LinkDecision::Navigate(url.clone())
            }
        }
        None => LinkDecision::Ignore,
    }
}

/// Pure submit-vs-cancel verdict for one submission stage (mirrors
/// `click_decision` in shape; worker-side the decision has two stages —
/// activation + form): the activating click's listeners and the form's
/// `submit` listeners each write one `preventDefault` flag — either being
/// set cancels the not-yet-built request (the request is only built clean).
#[derive(Debug, PartialEq, Eq)]
enum SubmitDecision {
    Submit,
    Cancel,
}

fn submit_decision(prevent_default: bool) -> SubmitDecision {
    if prevent_default {
        SubmitDecision::Cancel
    } else {
        SubmitDecision::Submit
    }
}

/// Wire form of a laid-out page: everything `Page`/`Layout` hold, minus the
/// `Rc`s that aren't `Send`. The UI thread re-wraps into `Rc` on receipt.
struct PageWire {
    url: String,
    bg: u32,
    height: f32,
    runs: Vec<RunWire>,
    /// x, y, w, h, color, fixed
    underline_rects: Vec<(f32, f32, f32, f32, u32, bool)>,
    /// x, y, w, h, color, radius, fixed
    rects: Vec<(f32, f32, f32, f32, u32, f32, bool)>,
    /// parallel to `rects`: Some = paint that rect as a gradient
    rect_grad: Vec<Option<std::sync::Arc<css::Gradient>>>,
    /// parallel to `rects`: Some(t) = paint that rect as a border ring of
    /// thickness t instead of a solid fill
    rect_ring: Vec<Option<f32>>,
    /// x, y, w, h, pixels (owned), fixed, effective opacity, filter
    images: Vec<(
        f32,
        f32,
        f32,
        f32,
        image::ImageData,
        bool,
        u8,
        Option<std::sync::Arc<css::Filter>>,
    )>,
    shadows: Vec<layout::ShadowRect>,
    /// playable `<video>` elements: draw rect + file + `images` index
    media: Vec<layout::MediaEntry>,
    /// `<a download>` targets (href resolved against the page)
    downloads: Vec<layout::DownloadLink>,
    /// sticky pinning records — the UI thread applies them per frame in
    /// `render` (indexes into the channels above, so they ride along)
    sticky: Vec<layout::StickyGroup>,
    /// `backdrop-filter` region records — same story as `sticky` (the UI
    /// thread's `render` withholds the subtree spans and blurs the region)
    backdrops: Vec<layout::BackdropGroup>,
    /// the cascade's font-family stack table (ids are baked into run styles)
    stacks: Vec<Vec<String>>,
    /// the page's `@font-face` defs so the renderer resolves webfonts too
    faces: Vec<css::FontFace>,
    /// laid-out form-control rects (hit-testing) for the UI thread
    forms: Vec<layout::FormBox>,
    /// focused control node id (paints the accent border + caret)
    focused: Option<u32>,
    /// live values by node id (what each control displays)
    form_values: HashMap<u32, String>,
    /// node id → `name` attribute, built worker-side (the UI thread has no
    /// DOM access) so `[form] submit name=value` logging works in the shell
    form_names: HashMap<u32, String>,
    /// hover hit-test targets (rect + node + cursor + fixed), tree order
    hover_targets: Vec<layout::HoverTarget>,
    /// inspector metadata by node (worker-built; see [`ElementInfo`])
    elements: HashMap<u32, ElementInfo>,
}

/// One laid-out text run without `Rc` (link href as a plain `String`).
struct RunWire {
    x: f32,
    y: f32,
    text: String,
    style: Style,
    width: f32,
    link: Option<String>,
    fixed: bool,
    /// clip window (x0, y0, x1, y1, doc coords) from ancestor overflow/clip
    /// boxes — None = unclipped; degenerate (x0 ≥ x1) = draw nothing
    clip: Option<(f32, f32, f32, f32)>,
    /// effective opacity byte (255 = opaque) for the run's glyphs
    alpha: u8,
}

/// Result sent back by the worker: the wire page (None = the pipeline panicked),
/// plus a script-requested navigation, the click-decision flag and the
/// forms state the worker ended on.
struct LoadDone {
    id: u64,
    page: Option<PageWire>,
    /// script called `location.assign`/`location = ...` — UI should navigate
    nav: Option<String>,
    /// the worker ALREADY fetched + rendered this `nav` target (a form
    /// submission, or a nested POST `form.submit()` during the load): the
    /// UI adopts the page instead of re-dispatching (a re-dispatch would
    /// refetch a POST as plain GET and lose the body). false for plain
    /// script navs (`location.assign`) — those still re-dispatch.
    nav_done: bool,
    /// a click listener called preventDefault → the UI drops the pending
    /// link navigation this job's id belongs to (never started)
    prevent_default: bool,
    /// the worker's pipeline panicked (caught; process survives)
    panicked: bool,
    /// a first-paint snapshot (markup + stylesheets, no scripts yet): the UI
    /// shows it and keeps waiting for the final page of the same job
    partial: bool,
}

/// Convert a `Page` (worker-side, holds `Rcs`) into the `Send` wire form.
/// `form_names` is built worker-side (node → `name` attr) — the UI thread
/// can't read the DOM, and `[form] submit name=value` logging needs it.
fn page_to_wire(
    p: Page,
    stacks: Vec<Vec<String>>,
    faces: Vec<css::FontFace>,
    form_names: HashMap<u32, String>,
) -> PageWire {
    // a reflow's remaining cost lands here, not in the layout stages
    let t0 = Instant::now();
    let wire = PageWire {
        url: p.url,
        bg: p.bg,
        height: p.laid.height,
        runs: p
            .laid
            .runs
            .into_iter()
            .map(|r| RunWire {
                x: r.x,
                y: r.y,
                text: r.text,
                style: r.style,
                width: r.width,
                link: r.link.as_ref().map(|l| l.to_string()),
                fixed: r.fixed,
                clip: r.clip,
                alpha: r.alpha,
            })
            .collect(),
        underline_rects: p.laid.underline_rects,
        rects: p.laid.rects,
        rect_grad: p.laid.rect_grad,
        rect_ring: p.laid.rect_ring,
        images: p
            .laid
            .images
            .into_iter()
            .map(|(x, y, w, h, img, fixed, alpha, filter)| {
                // freshly decoded per page: refcount is 1, but be safe
                let owned = std::rc::Rc::try_unwrap(img).unwrap_or_else(|rc| image::ImageData {
                    w: rc.w,
                    h: rc.h,
                    px: rc.px.clone(),
                });
                (x, y, w, h, owned, fixed, alpha, filter)
            })
            .collect(),
        shadows: p.laid.shadows,
        sticky: p.laid.sticky,
        backdrops: p.laid.backdrops,
        media: p.media,
        downloads: p.downloads,
        stacks,
        faces,
        forms: p.laid.forms,
        focused: p.laid.focused,
        form_values: p.laid.form_values,
        form_names,
        hover_targets: p.laid.hover_targets,
        elements: p.elements,
    };
    if std::env::var_os("MAZDA_TIME").is_some() {
        eprintln!(
            "[time] page to wire: {:.0} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
    wire
}

/// Rebuild a `Page` from the wire form on the UI thread (re-wraps in `Rc`).
fn wire_to_page(w: PageWire) -> Page {
    Page {
        url: w.url,
        bg: w.bg,
        laid: Layout {
            runs: w
                .runs
                .into_iter()
                .map(|r| layout::PositionedRun {
                    x: r.x,
                    y: r.y,
                    text: r.text,
                    style: r.style,
                    width: r.width,
                    link: r.link.map(std::rc::Rc::from),
                    fixed: r.fixed,
                    clip: r.clip,
                    // hover identity rides `hover_targets`, not the runs
                    node: None,
                    cursor: None,
                    alpha: r.alpha,
                })
                .collect(),
            height: w.height,
            underline_rects: w.underline_rects,
            rects: w.rects,
            rect_grad: w.rect_grad,
            rect_ring: w.rect_ring,
            images: w
                .images
                .into_iter()
                .map(|(x, y, w, h, img, fixed, alpha, filter)| {
                    (x, y, w, h, std::rc::Rc::new(img), fixed, alpha, filter)
                })
                .collect(),
            shadows: w.shadows,
            sticky: w.sticky,
            backdrops: w.backdrops,
            open_line: false, // paint-only consumer; flow state not needed
            // forms state rides the wire: rects for hit-testing, focus +
            // values for painting the accent border / caret / typed text
            forms: w.forms,
            focused: w.focused,
            form_values: w.form_values,
            hover_targets: w.hover_targets,
        },
        media: w.media,
        downloads: w.downloads,
        elements: w.elements,
    }
}

/// The parsed page the worker keeps between jobs so a re-layout or a script
/// event doesn't refetch/reparse. One page is active; the pages of the other
/// tabs stay parked (see [`bind_parked`]) and come back on a switch. Holds the
/// non-`Send` script engine, so it never leaves this thread.
struct KeptPage {
    url: String,
    dom: html::Dom,
    /// concatenated stylesheet text (re-parsed per layout: media queries
    /// depend on the viewport width)
    css_text: String,
    /// `@font-face` defs (re-registered per layout)
    faces: Vec<css::FontFace>,
    /// script engine + listeners for this page; None until scripts ran.
    /// Trait object: the engine choice (settings page / CLI / conf) picks
    /// the concrete runtime at load time and the rest of the shell never
    /// knows which one runs.
    scripts: Option<Box<dyn ScriptRuntime>>,
    /// engine this page was BUILT with — form-submission loads fired by
    /// script turns (`form.submit()` POST) re-run the full pipeline from a
    /// context that has no `LoadReq` around, so the page's own engine
    /// choice is the one that must apply (same answer as `job.engine()`
    /// for jobs).
    engine: EngineChoice,
    /// last hover cell a Relayout job set (None on a fresh page): events,
    /// scrolls and timer re-layouts re-cascade with it. Shortcut: a
    /// mismatched full load resets this to None while the shell may still
    /// hold a node - the next pointer move re-syncs (self-heals).
    hover: Option<u32>,
    /// decoded-image memo shared by this page's relayouts (see `ImgCache`)
    imgs: ImgCache,
    /// The parsed stylesheet, keyed by the viewport width it was parsed for
    /// (media queries are decided at parse time). See `layout_page`.
    sheet: Option<(f32, css::Sheet)>,
    /// This page is `fetch_and_parse`'s "Failed to load" error page. It is
    /// kept like any page (a resize re-lays it out without refetching), but a
    /// Restore must never REUSE it: the site may be back, and reusing would
    /// pin the error forever. The snapshot path has the same rule (an error
    /// page is never snapshotted).
    failed: bool,
}

/// The single long-lived loader thread. Owns its own `Fonts` (font types
/// aren't `Send`, so each thread loads its own — system fonts come from disk
/// anyway) and the one kept page. Superseded loads/relayouts still queued are
/// skipped, but a form Event is never dropped (it carries a user action).
///
/// Unified scheduler loop (one explicit wake discipline, no ad-hoc timer
/// checks): wake = earlier of {next timer deadline, next rAF epoch} ∪
/// {block until an input job arrives}; on wake: fire due timers → fire due
/// rAF frames (one coalesced pass) → drain microtasks (done inside every
/// script turn) → re-layout + push a result if any handler mutated the DOM
/// → check the job channel again. Jobs are processed one at a time with a
/// due-JS-work interleave between them, so a flood of input events can
/// never starve timers, and the channel timeout keeps the thread from
/// busy-spinning.
fn loader_thread(jobs: mpsc::Receiver<LoadReq>, results: mpsc::Sender<LoadDone>) {
    let mut fonts = Fonts::load();
    let mut kept: Option<KeptPage> = None;
    // pages of the background tabs, frozen: nothing advances their timers
    // until one is bound again (see `bind_parked`). Bounded by count.
    let mut parked: Vec<KeptPage> = Vec::new();
    // geometry + form state of the last layout (timers re-layout with them)
    let mut last_geom = (0.0f32, 0.0f32);
    let mut last_forms = layout::FormState::default();
    // id of the job that produced the kept page (timer pushes reuse it so
    // the UI's generation gate accepts them)
    let mut current_id = 0u64;
    loop {
        let wake = kept
            .as_ref()
            .and_then(|k| k.scripts.as_ref().and_then(|s| s.next_wake_deadline()));
        let timeout = recv_budget(wake, Instant::now());
        let first = match jobs.recv_timeout(timeout) {
            Ok(j) => j,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // JS work came due: timers → rAF frames → microtasks (each
                // turn drains its own); a mutation pushes the re-layout
                fire_due(&mut kept, &mut fonts, &mut last_geom, &mut last_forms, current_id, &results);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // ---- coalesce: drop superseded loads/relayouts/scrolls, keep every event ----
        let mut batch = vec![first];
        while let Ok(n) = jobs.try_recv() {
            batch.push(n);
        }
        for job in coalesce_batch(batch) {
            let t0 = Instant::now();
            let id = job.id();
            let url = job.url().to_string();
            let scroll_like = matches!(job, LoadReq::Scroll { .. });
            // The page this job is about. A restore (tab switch / back) binds
            // the live page the worker parked for that URL, so the job runs
            // against a live DOM instead of a rebuild.
            let keep_live = match &job {
                LoadReq::Load { restore, .. } => *restore,
                _ => true,
            };
            if !(keep_live && bind_parked(&mut kept, &mut parked, &url)) {
                // the job rebuilds its target (`do_load`): keep the page we
                // are leaving, so a switch back to it is a bind, not a rebuild
                if let Some(old) = kept.take() {
                    if old.url == url {
                        kept = Some(old); // a reload of the same page: nothing to park
                    } else {
                        park_page(&mut parked, old);
                    }
                }
            }
            // first paint goes out mid-job: markup + stylesheets, laid out,
            // before the scripts run. A load can take 10+ s on this link and
            // a blank window for all of it is the worst part of it.
            let early = |wire: PageWire| {
                let _ = results.send(LoadDone {
                    id,
                    page: Some(wire),
                    nav: None,
                    nav_done: false,
                    prevent_default: false,
                    panicked: false,
                    partial: true,
                });
            };
            // worker panics must not take the process down (employed for
            // loads, relayouts AND events alike)
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process_job(
                    &mut kept,
                    &mut fonts,
                    &job,
                    &mut last_geom,
                    &mut last_forms,
                    Some(&early),
                )
            }));
            let (wire, nav, pd, panicked, nav_done) = match outcome {
                Ok(r) => r,
                Err(_) => {
                    eprintln!("[nav] loader pipeline panicked for {url}");
                    kept = None; // don't trust a half-built page
                    (None, None, false, true, false)
                }
            };
            // a fresh load replaces the page for its URL: a parked page from
            // an older visit of the same URL is stale now
            if matches!(job, LoadReq::Load { restore: false, .. }) && kept_matches(&kept, &url) {
                parked.retain(|p| p.url != url);
            }
            css::dump_misses(); // no-op unless MAZDA_MISS=1
            js::dump_misses(); // no-op unless MAZDA_MISS=1
            eprintln!(
                "[nav] worker finished {url} in {:.0} ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            // scroll events that didn't mutate anything get no result —
            // the UI has already scrolled itself; those that did re-layout
            // push with the current id so the generation gate accepts them
            if wire.is_some() || panicked || nav.is_some() || !scroll_like {
                if wire.is_some() {
                    current_id = id;
                }
                if results
                    .send(LoadDone {
                    id,
                    page: wire,
                    nav,
                    nav_done,
                    prevent_default: pd,
                    panicked,
                    partial: false,
                })
                    .is_err()
                {
                    break; // UI side gone
                }
            }
            // starvation-free interleave: a handler may have made timers or
            // rAF frames due — run the JS turn between user jobs, never let
            // an event flood push them out
            fire_due(&mut kept, &mut fonts, &mut last_geom, &mut last_forms, current_id, &results);
        }
    }
}

/// The unified wake budget for the worker's `recv_timeout`: the wait until
/// the earliest due piece of JS work (timer or rAF epoch), or the long
/// idle block when nothing is scheduled. Never below ~1ms so a deadline
/// that is already momentarily past cannot cause a busy-spin wake loop.
fn recv_budget(wake: Option<Instant>, now: Instant) -> Duration {
    match wake {
        Some(d) => d.saturating_duration_since(now).max(Duration::from_millis(1)),
        // no pending JS work: block for input jobs (an hour = the practical
        // forever; the channel still wakes us for every job)
        None => Duration::from_secs(3600),
    }
}

/// Coalesce a drained job batch worker-side (pure, tested): a `Load`
/// supersedes everything queued before it (we'd never fetch the older
/// page), a run of consecutive relayouts collapses to the newest (events
/// break the run, so an event between two relayouts survives), and a run
/// of consecutive scroll events collapses to the newest scroll.
fn coalesce_batch(batch: Vec<LoadReq>) -> Vec<LoadReq> {
    // a Load supersedes everything queued before it
    let mut batch = batch;
    if let Some(p) = batch.iter().rposition(|r| matches!(r, LoadReq::Load { .. })) {
        if p > 0 {
            eprintln!("[nav] skipped {p} superseded job(s)");
        }
        batch.drain(..p);
    }
    // collapse consecutive relayouts / scrolls to the newest of the run
    let mut out: Vec<LoadReq> = Vec::new();
    let mut collapsed = 0u32;
    let mut scrolls = 0u32;
    for j in batch {
        let same_run = match (&j, out.last()) {
            (LoadReq::Relayout { .. }, Some(LoadReq::Relayout { .. })) => true,
            (LoadReq::Scroll { .. }, Some(LoadReq::Scroll { .. })) => true,
            _ => false,
        };
        if same_run {
            out.pop();
            // count which kind collapsed (events are never dropped)
            if matches!(j, LoadReq::Scroll { .. }) {
                scrolls += 1;
            } else {
                collapsed += 1;
            }
        }
        out.push(j);
    }
    if collapsed > 0 {
        eprintln!("[nav] skipped {collapsed} superseded relayout(s)");
    }
    if scrolls > 0 {
        eprintln!("[nav] coalesced {scrolls} scroll event(s)");
    }
    out
}

/// Fire the currently-due share of the page's JS work (timers, then the
/// rAF frame pass; microtasks drain inside each turn). Any DOM mutation
/// re-lays out and pushes a wire result; a script navigation from a timer
/// or rAF callback now reaches the UI too (it used to be dropped).
/// Panics in event/timer dispatch are caught: a half-mutated page is
/// dropped rather than trusted.
#[allow(clippy::too_many_arguments)]
fn fire_due(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    last_geom: &mut (f32, f32),
    last_forms: &mut layout::FormState,
    current_id: u64,
    results: &mpsc::Sender<LoadDone>,
) {
    if !matches!(
        kept.as_ref().map(|k| k.scripts.is_some()).unwrap_or(false),
        true
    ) {
        return;
    }
    // cheap pre-check: nothing due → no turn at all
    let due_now = {
        let KeptPage { scripts, .. } = kept.as_ref().unwrap();
        scripts
            .as_ref()
            .map(|s| s.next_wake_deadline().map(|d| d <= Instant::now()).unwrap_or(false))
            .unwrap_or(false)
    };
    if !due_now {
        return;
    }
    // engine of the kept page (form-submission loads fired from timer/rAF
    // turns re-run the full pipeline and must use the same runtime choice)
    let engine = kept.as_ref().map(|k| k.engine).unwrap_or(EngineChoice::A);
    let KeptPage { dom, scripts, css_text, faces, url, hover, imgs, sheet, .. } =
        kept.as_mut().unwrap();
    let (mw, vh) = *last_geom;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scripts
            .as_mut()
            .map(|s| {
                let mut dirty = false;
                let mut nav = None;
                let mut form_nav = None;
                let t = s.run_due_timers(dom);
                dirty |= t.dirty;
                if nav.is_none() {
                    nav = t.nav;
                }
                if form_nav.is_none() {
                    form_nav = t.form_nav;
                }
                let f = s.run_due_frames(dom);
                dirty |= f.dirty;
                if nav.is_none() {
                    nav = f.nav;
                }
                if form_nav.is_none() {
                    form_nav = f.form_nav;
                }
                (dirty, nav, form_nav)
            })
            .unwrap_or((false, None, None))
    }));
    let (dirty, nav, form_nav) = match outcome {
        Ok(r) => r,
        Err(_) => {
            eprintln!("[nav] JS turn panicked (timers/rAF); page dropped");
            *kept = None;
            return;
        }
    };
    if let Some(f) = form_nav {
        // a timer/rAF callback form.submit()ed a POST: the worker executes
        // the load and hands up the rendered page (same gate id)
        eprintln!("[js] timer form.submit() (POST) executes a load → {}", f.url);
        let (wire, nav2) = do_form_nav(kept, fonts, &f, mw, vh, engine);
        let done = nav2.is_none();
        let _ = results.send(LoadDone {
            id: current_id,
            page: wire,
            nav: nav2.or(Some(f.url)),
            // fetched by the worker (unless the loaded page's own scripts
            // asked for a plain nav — that one re-dispatches)
            nav_done: done,
            prevent_default: false,
            panicked: false,
            partial: false,
        });
        return;
    }
    if let Some(nav_url) = nav {
        // a timer/rAF callback asked to navigate: hand it up untouched
        eprintln!("[js] timer navigation → {nav_url}");
        if results
            .send(LoadDone {
            id: current_id,
            page: None,
            nav: Some(nav_url),
            nav_done: false,
            prevent_default: false,
            panicked: false,
            partial: false,
        })
            .is_err()
        {
            return; // UI side gone
        }
        if dirty {
            eprintln!("[js] timer also mutated the page (nav wins)");
        }
        return;
    }
    if dirty {
        let (page, form_names) = layout_page(
            &*dom, css_text, faces, url, fonts, mw, vh, last_forms, *hover, imgs, sheet,
            false,
        );
        let stacks = fonts.stacks.borrow().clone();
        let out_faces = collect_out_faces(fonts);
        let wire = page_to_wire(page, stacks, out_faces, form_names);
        eprintln!("[js] timer/frame mutated the page; pushed relayout");
        if results
            .send(LoadDone {
            id: current_id,
            page: Some(wire),
            nav: None,
            nav_done: false,
            prevent_default: false,
            panicked: false,
            partial: false,
        })
            .is_err()
        {
            // UI side gone
        }
    }
}

/// Handle one job against the kept page. Returns (wire page, script-requested
/// navigation, preventDefault flag, panicked flag, nav-done flag — true when
/// the worker already fetched+rendered the `nav` target). Falls back to a
/// full load when there's no kept page for the job's URL.
fn process_job(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    job: &LoadReq,
    last_geom: &mut (f32, f32),
    last_forms: &mut layout::FormState,
    early: Option<&dyn Fn(PageWire)>,
) -> (Option<PageWire>, Option<String>, bool, bool, bool) {
    // a fresh navigation adopts the settings-page webfont policy; layout
    // runs (relayout/event/scroll) keep the loaded page's policy
    if let Some(choice) = job.fonts_choice() {
        fonts.set_choice(choice);
    }
    match job {
        LoadReq::Load { url, max_w, vh, restore, .. } => {
            *last_geom = (*max_w, *vh);
            *last_forms = layout::FormState::default();
            // The live page for this URL was bound by the caller (the page
            // already showing, or a parked tab): re-lay it out instead of
            // rebuilding it from the snapshot. Scripts do NOT run again - the
            // DOM is the one the user left, which is also more correct than
            // the pre-script snapshot a rebuild starts from.
            if *restore
                && kept_matches(kept, url)
                && !kept.as_ref().map(|k| k.failed).unwrap_or(false)
            {
                eprintln!("[tabs] reusing the live page for {url} (no fetch, no scripts)");
                let kp = kept.as_mut().unwrap();
                // the hover cell is a node id from the OLD page: a stale one
                // would style whatever node happens to carry that id now
                kp.hover = None;
                let (page, form_names) = layout_page(
                    &kp.dom, &kp.css_text, &kp.faces, url, fonts, *max_w, *vh,
                    last_forms, None, &mut kp.imgs, &mut kp.sheet, false,
                );
                let stacks = fonts.stacks.borrow().clone();
                let out_faces = collect_out_faces(fonts);
                return (Some(page_to_wire(page, stacks, out_faces, form_names)), None, false, false, false);
            }
            let r = do_load(kept, fonts, url, *max_w, *vh, job.engine(), *restore, early);
            (r.0, r.1, false, r.2, r.3)
        }
        LoadReq::Relayout { url, max_w, vh, forms, hover, .. } => {
            *last_geom = (*max_w, *vh);
            *last_forms = forms.clone();
            if !kept_matches(kept, url) {
                let r = do_load(kept, fonts, url, *max_w, *vh, job.engine(), false, None);
                return (r.0, r.1, false, r.2, r.3);
            }
            let kp = kept.as_mut().unwrap();
            // remember the cell: every later job (event/scroll/timer)
            // re-cascades with it until the next Relayout updates it
            kp.hover = *hover;
            let (page, form_names) = layout_page(
                &kp.dom, &kp.css_text, &kp.faces, url, fonts, *max_w, *vh, forms, *hover,
                &mut kp.imgs, &mut kp.sheet, false,
            );
            let stacks = fonts.stacks.borrow().clone();
            let out_faces = collect_out_faces(fonts);
            (Some(page_to_wire(page, stacks, out_faces, form_names)), None, false, false, false)
        }
        LoadReq::Event { url, max_w, vh, node, kinds, value, key, href, submit, forms, .. } => {
            *last_geom = (*max_w, *vh);
            *last_forms = forms.clone();
            if !kept_matches(kept, url) {
                let r = do_load(kept, fonts, url, *max_w, *vh, job.engine(), false, None);
                return (r.0, r.1, false, r.2, r.3);
            }
            let kp = kept.as_mut().unwrap();
            // link clicks arrive by href (layout runs carry hrefs, not node
            // ids): the first anchor whose resolved href matches is the
            // target and the click bubbles from there. shortcut: several
            // identical links share one listener target (first in DOM order).
            let target = match href {
                Some(h) => find_node_by_href(&kp.dom, url, h).unwrap_or(0),
                None => *node,
            };
            // push the control's state into the worker DOM BEFORE the
            // value-bearing kinds dispatch (input/change see the final value)
            let value_kinds = kinds.iter().any(|k| k == "input" || k == "change");
            if value_kinds {
                if let Some(v) = value {
                    apply_event_value(kp, target, v);
                }
            }
            // restore the click semantics that used to live in apply_event:
            // a click on a checkbox/radio toggles `checked` before listeners
            // fire (toggles/checked boxes re-layout with the worker DOM)
            if kinds.iter().any(|k| k == "click") {
                apply_click_toggle(kp, target);
            }
            // (href clicks may resolve no anchor: nothing to listen on, the
            // re-layout below still runs)
            // fire the target's listeners for each kind, in order
            let mut nav = None;
            let mut pd = false;
            let mut script_form_nav: Option<runtime::FormNav> = None;
            {
                let KeptPage { dom, scripts, .. } = kp;
                if let Some(scripts) = scripts.as_mut() {
                    let kind_refs: Vec<&str> = kinds.iter().map(|s| s.as_str()).collect();
                    let t = ScriptRuntime::dispatch(
                        scripts.as_mut(),
                        dom, target, &kind_refs, key.as_deref(), None, None,
                    );
                    nav = t.nav;
                    pd = t.prevent_default;
                    script_form_nav = t.form_nav;
                }
            }
            // a listener form.submit()ing a POST during the activation
            // outranks every later stage of this job (a real navigation has
            // already been queued worker-side — execute it now)
            if nav.is_none() {
                if let Some(f) = script_form_nav {
                    eprintln!("[nav] listener form.submit() (POST) executes this job's load");
                    let (wire, nav2) = do_form_nav(kept, fonts, &f, *max_w, *vh, job.engine());
                    let done = nav2.is_none();
                    return (wire, nav2.or(Some(f.url.clone())), false, false, done);
                }
            }
            // ---- real submission stage ----
            // Order (see the module contract): the activating listeners above
            // had their veto — a click listener's preventDefault stops
            // activation entirely (no submit event, `submit_decision`);
            // otherwise the form's own submit listeners receive a `submit`
            // event and a preventDefault there cancels the navigation (the
            // same pure verdict, second stage). Still clean → the worker
            // collects the successful pairs and executes the GET/POST load
            // under this job's NavGate id (the result comes back as nav +
            // rendered page in one shot).
            // NOTE: if the listened-to node navigated itself (nav/form_nav
            // above), that wins over the form data.
            if *submit && nav.is_none() {
                if submit_decision(pd) == SubmitDecision::Cancel {
                    eprintln!("[js] preventDefault — submission canceled at activation (no submit event)");
                } else {
                    let submission = build_submission(&kp.dom, target, &forms.values, &forms.files, url);
                    match submission {
                        None => {
                            eprintln!(
                                "[form] activation on node {target} has no enclosing <form> (or unresolvable action) — no submission"
                            );
                        }
                        Some(sub) => {
                            eprintln!(
                                "[form] submit method={} url={} body={} bytes ({})",
                                sub.method.as_str(),
                                sub.target,
                                sub.body.len(),
                                sub.content_type
                            );
                            // the form's submit listeners get the submit event
                            // (bubbles like the click dispatch). A preventDefault
                            // there vetoes the build (`submit_decision` again); a
                            // listener's own navigation outranks the form data.
                            let mut vetoed = false;
                            {
                                let KeptPage { dom, scripts, .. } = kp;
                                if let (Some(scripts), Some(form)) =
                                    (scripts.as_mut(), form_ancestor(dom, target))
                                {
                                    let t = ScriptRuntime::dispatch(
                                        scripts.as_mut(),
                                        dom, form, &["submit"], None, None, None,
                                    );
                                    if let Some(f) = t.form_nav {
                                        // a submit listener form.submit()ed a
                                        // POST itself: execute that instead
                                        eprintln!("[nav] submit listener form.submit() (POST) executes this job's load");
                                        let (wire, nav2) =
                                            do_form_nav(kept, fonts, &f, *max_w, *vh, job.engine());
                                        let done = nav2.is_none();
                                        return (wire, nav2.or(Some(f.url)), false, false, done);
                                    }
                                    if let Some(nav_target) = t.nav {
                                        eprintln!("[js] submit listener navigated to {nav_target}");
                                        return (None, Some(nav_target), false, false, false);
                                    }
                                    vetoed = submit_decision(t.prevent_default) == SubmitDecision::Cancel;
                                }
                            }
                            if vetoed {
                                eprintln!("[js] preventDefault — form submission canceled by the submit event");
                            } else {
                                match sub.method {
                                    SubmitMethod::Get => {
                                        eprintln!("[nav] queued load (form GET {})", sub.target);
                                        let (wire, script_nav, panicked, nav_done) =
                                            do_load(kept, fonts, &sub.target, *max_w, *vh, job.engine(), false, None);
                                        if let Some(s) = &script_nav {
                                            // the fetched page's scripts asked for
                                            // a plain nav: it wins (the ordinary
                                            // load contract) — the UI re-dispatches
                                            // it, the submission still went out
                                            eprintln!("[js] post-submission script nav → {s}");
                                        }
                                        // script nav present → it is UI-side work
                                        // (nav_done false); clean GET → the wire IS
                                        // the submission's target page (adopt)
                                        let done = nav_done || script_nav.is_none();
                                        return (wire, script_nav.or(Some(sub.target)), pd, panicked, done);
                                    }
                                    SubmitMethod::Post => {
                                        let (wire, nav2) = do_form_nav_raw(
                                            kept,
                                            fonts,
                                            &sub.target,
                                            &sub.body,
                                            &sub.content_type,
                                            *max_w,
                                            *vh,
                                            job.engine(),
                                        );
                                        let done = nav2.is_none();
                                        return (wire, nav2.or(Some(sub.target)), false, false, done);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // always re-layout: the event likely changed the DOM
            let (page, form_names) = layout_page(
                &kp.dom, &kp.css_text, &kp.faces, url, fonts, *max_w, *vh, forms, kp.hover,
                &mut kp.imgs, &mut kp.sheet, false,
            );
            let stacks = fonts.stacks.borrow().clone();
            let out_faces = collect_out_faces(fonts);
            (Some(page_to_wire(page, stacks, out_faces, form_names)), nav, pd, false, false)
        }
        LoadReq::Scroll { url, y } => {
            // scroll listeners only; no re-layout unless a listener mutated
            // the DOM (then the wire result carries it). No generation id:
            // scrolling must never preempt other results' gating.
            if !kept_matches(kept, url) {
                eprintln!("[nav] scroll event for unloaded page dropped");
                return (None, None, false, false, false);
            }
            let kp = kept.as_mut().unwrap();
            let body = kp.dom.body_root();
            let (page, form_names) = {
                    let mut nav = None;
                    let mut dirty = false;
                    let mut form_nav: Option<runtime::FormNav> = None;
                    {
                        let KeptPage { dom, scripts, .. } = kp;
                        if let Some(scripts) = scripts.as_mut() {
                            let t = ScriptRuntime::dispatch(
                                scripts.as_mut(),
                                dom, body, &["scroll"], None, Some(*y), None,
                            );
                            nav = t.nav;
                            dirty = t.dirty;
                            form_nav = t.form_nav;
                        }
                        if let Some(f) = form_nav {
                            // a scroll listener form.submit()ed a POST:
                            // execute worker-side, exactly like a script nav
                            eprintln!("[js] scroll listener form.submit() (POST) executes a load");
                            let engine = kept.as_ref().map(|k| k.engine).unwrap_or(EngineChoice::A);
                            let (wire, nav2) =
                                do_form_nav(kept, fonts, &f, last_geom.0, last_geom.1, engine);
                            let done = nav2.is_none();
                            return (wire, nav2.or(Some(f.url.clone())), false, false, done);
                        }
                        if let Some(nav_url) = nav {
                            // scroll listener navigated: hand it up untouched
                            eprintln!("[js] scroll listener navigates to {nav_url}");
                            return (None, Some(nav_url), false, false, false);
                        }
                        if !dirty {
                            // nothing mutated: the UI paints its own scroll; no
                            // result (keeps the generation gate untouched)
                            return (None, None, false, false, false);
                        }
                    }
                let (page, fnames) = layout_page(
                    &kp.dom, &kp.css_text, &kp.faces, url, fonts, last_geom.0, last_geom.1, last_forms,
                    kp.hover, &mut kp.imgs, &mut kp.sheet, false,
                );
                (page, fnames)
            };
            let stacks = fonts.stacks.borrow().clone();
            let out_faces = collect_out_faces(fonts);
            (Some(page_to_wire(page, stacks, out_faces, form_names)), None, false, false, false)
        }
    }
}

/// First `<a>` whose resolved href equals `href` (worker has the DOM; the
/// UI's link runs carry only the href).
fn find_node_by_href(dom: &html::Dom, base: &str, href: &str) -> Option<u32> {
    (1..dom.nodes.len() as u32)
        .filter(|&i| dom.tag(i) == "a")
        .find(|&i| {
            let raw = dom.attr(i, "href").map(|s| s.to_string()).unwrap_or_default();
            crate::resolve(base, &raw).as_deref() == Some(href)
                || raw == href
        })
}

/// Push a typed value into the target control's `value` attribute so both
/// layout and scripts reading getAttribute("value") see it. (Only text
/// fields get values today; checkbox/radio state flips are click-only.)
fn apply_event_value(kp: &mut KeptPage, node: u32, value: &str) {
    set_dom_attr(&mut kp.dom, node, "value", value);
}

/// A click on a checkbox/radio toggles the `checked` attribute state
/// worker-side before listeners fire (the shell owns the click, the worker
/// owns the DOM).
fn apply_click_toggle(kp: &mut KeptPage, node: u32) {
    toggle_control(&mut kp.dom, node);
}

/// The checked-state half of a click, DOM-only so it is testable without a
/// page. A checkbox flips; a radio never unchecks (browsers keep a clicked
/// radio selected) and picking one clears every other radio of the same
/// group - same `name`, same form owner, or the whole document for a
/// form-less group. Listeners/relayout are the caller's job.
fn toggle_control(dom: &mut html::Dom, node: u32) {
    if dom.tag(node) != "input" {
        return;
    }
    let ty = dom.attr(node, "type").unwrap_or("text").to_ascii_lowercase();
    match ty.as_str() {
        "checkbox" => {
            if dom.attr(node, "checked").is_some() {
                remove_dom_attr(dom, node, "checked");
            } else {
                set_dom_attr(dom, node, "checked", "");
            }
        }
        "radio" => {
            // `name` is copied first: clearing the group mutates attributes
            let name = dom.attr(node, "name").unwrap_or("").to_string();
            let scope = form_ancestor(dom, node).unwrap_or(0);
            if !name.is_empty() {
                let group: Vec<u32> = (1..dom.nodes.len() as u32)
                    .filter(|&i| i != node && same_radio_group(dom, i, &name, scope))
                    .collect();
                for i in group {
                    remove_dom_attr(dom, i, "checked");
                }
            }
            set_dom_attr(dom, node, "checked", "");
        }
        _ => {}
    }
}

/// True when `other` is a radio in the group `name` under `scope` (the form
/// owner node, or 0 for form-less radios, which group per document).
fn same_radio_group(dom: &html::Dom, other: u32, name: &str, scope: u32) -> bool {
    dom.tag(other) == "input"
        && dom
            .attr(other, "type")
            .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
        && dom.attr(other, "name") == Some(name)
        && form_ancestor(dom, other).unwrap_or(0) == scope
}

fn kept_matches(kept: &Option<KeptPage>, url: &str) -> bool {
    kept.as_ref().map(|k| k.url == url).unwrap_or(false)
}

/// Background pages the worker keeps alive. Bounded by COUNT, never by memory
/// (the engine has no memory budget): the page parked longest ago is dropped
/// and its tab falls back to the snapshot restore every switch used to take.
const PARKED_MAX: usize = 8;

/// Bind the parked page for `url` as the active one, parking the page it
/// replaces. Returns true when the active page already is `url` or a parked
/// page was bound for it.
///
/// This is what makes a tab switch cheap: a parked page keeps its live DOM,
/// its script heap and its decoded images, so switching back costs one
/// re-layout instead of a snapshot read + script replay + two cascades. A
/// parked page is frozen: `fire_due` only advances the ACTIVE page's timers.
fn bind_parked(kept: &mut Option<KeptPage>, parked: &mut Vec<KeptPage>, url: &str) -> bool {
    if kept_matches(kept, url) {
        return true;
    }
    let Some(i) = parked.iter().position(|p| p.url == url) else {
        return false;
    };
    let page = parked.remove(i);
    eprintln!("[tabs] bound the live page for {url}");
    if let Some(old) = kept.replace(page) {
        park_page(parked, old);
    }
    true
}

/// Park a page for a later switch back. A page parked earlier for the same URL
/// (an older visit) is dropped, so a switch back always binds the newest one.
fn park_page(parked: &mut Vec<KeptPage>, page: KeptPage) {
    // an error page is not worth a slot: one fetch rebuilds it
    if page.failed {
        return;
    }
    parked.retain(|p| p.url != page.url);
    parked.push(page);
    if parked.len() > PARKED_MAX {
        let dropped = parked.remove(0);
        eprintln!(
            "[tabs] dropped the parked page for {} ({} kept)",
            dropped.url,
            parked.len()
        );
    }
}

/// Full load: fetch + parse + run `<script>` + layout. Seeds/ replaces the
/// kept page. `settle_load` is the shared back half — submission-driven
/// loads arrive with bytes already fetched (POST) and skip `fetch_and_parse`.
fn do_load(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    url: &str,
    max_w: f32,
    vh: f32,
    engine: EngineChoice,
    restore: bool,
    early: Option<&dyn Fn(PageWire)>,
) -> (Option<PageWire>, Option<String>, bool, bool) {
    // a snapshot restore (tab switch / back) replays scripts from the disk
    // cache and is already fast: a pre-script frame there only pays a cascade
    let early = if restore { None } else { early };
    let (dom, css_text, faces, failed) = fetch_and_parse(url, restore);
    let (wire, nav, form_nav) =
        settle_load(kept, fonts, url, max_w, vh, engine, dom, css_text, faces, early);
    // mark the error page so a Restore refetches instead of reusing it (the
    // next successful load clears the mark with the page it replaces)
    if failed {
        if let Some(k) = kept.as_mut() {
            k.failed = true;
        }
    }
    if let Some(f) = form_nav {
        // an inline script form.submit()ed a POST during the load — the
        // worker executes it now (the first page's wire is replaced). Same
        // declaration as the real-submission POST stage: the clean case
        // pairs nav (= the POST target) + the fetched wire under nav_done
        // so the UI adopts it (accept log + history); the POST response
        // page's own script nav outranks (re-dispatched, plain contract).
        eprintln!("[js] inline form.submit() (POST) during load → {}", f.url);
        let (wire, nav2) = do_form_nav(kept, fonts, &f, max_w, vh, engine);
        let done = nav2.is_none();
        return (wire, nav2.or(Some(f.url)), false, done);
    }
    // plain script nav (location.assign etc.): handed up untouched — the UI
    // re-dispatches it as a fresh load (the POST form.submit() case was the
    // branch above, already executed worker-side)
    (wire, nav, false, false)
}

/// POST-submission load: `fetch_post` on the worker thread, then the exact
/// same parse → script → layout pipeline as a plain load; failure becomes
/// the existing error page. Nav: Some(target url) for the UI (a post-load
/// script nav wins — matching the plain-load contract).
fn do_form_nav(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    f: &runtime::FormNav,
    max_w: f32,
    vh: f32,
    engine: EngineChoice,
) -> (Option<PageWire>, Option<String>) {
    do_form_nav_raw(
        kept,
        fonts,
        &f.url,
        f.body.as_bytes(),
        URLENCODED,
        max_w,
        vh,
        engine,
    )
}

/// `do_form_nav` for a body that is not necessarily urlencoded text
/// (multipart uploads are bytes).
fn do_form_nav_raw(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    url: &str,
    body: &[u8],
    content_type: &str,
    max_w: f32,
    vh: f32,
    engine: EngineChoice,
) -> (Option<PageWire>, Option<String>) {
    eprintln!(
        "[nav] queued load (form POST {url}, {content_type} body {} bytes)",
        body.len()
    );
    let fetched = fetch_post(url, body, content_type);
    let (wire, nav, form_nav2) = do_load_full(kept, fonts, url, fetched, max_w, vh, engine);
    if let Some(f2) = form_nav2 {
        eprintln!("[js] form.submit() chain after POST load (dropped, one step)");
        eprintln!("[nav] form submission chain → {}", f2.url);
    }
    (wire, nav)
}

/// Load from already-fetched bytes (POST responses): parse + styles + run +
/// layout, same pipeline discipline as `do_load`. Error → the existing
/// "Failed to load" error page.
fn do_load_full(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    url: &str,
    fetched: Result<(Vec<u8>, Option<String>), String>,
    max_w: f32,
    vh: f32,
    engine: EngineChoice,
) -> (Option<PageWire>, Option<String>, Option<runtime::FormNav>) {
    let (dom, css_text, faces) = match fetched {
        Ok((bytes, ct)) => {
            let s = html::decode_html(&bytes, ct.as_deref());
            eprintln!(
                "[mem] after fetch: {:.1} MB (html {} KB, {})",
                working_set_mb(),
                s.len() / 1024,
                ct.as_deref().unwrap_or("no content-type")
            );
            parse_source_with_styles(url, s)
        }
        Err(e) => {
            eprintln!("[nav] form POST failed for {url}: {e}");
            parse_source_with_styles(url, format!("<p>Failed to load: {e}</p>"))
        }
    };
    settle_load(kept, fonts, url, max_w, vh, engine, dom, css_text, faces, None)
}

/// Parse an HTML source + collect its stylesheets (`fetch_and_parse`'s back
/// half, reused for POST responses so the stylesheet handling exists once).
/// The parsed tree is snapshotted BEFORE scripts run: a restore replays the
/// scripts, so listeners/state are rebuilt exactly as on a fresh load.
fn parse_source_with_styles(url: &str, source: String) -> (html::Dom, String, Vec<css::FontFace>) {
    let dom = html::parse(&source);
    eprintln!("[mem] after parse: {:.1} MB ({} nodes)", working_set_mb(), dom.nodes.len());
    let (css_text, faces) = styles_for(&dom, url);
    (dom, css_text, faces)
}

/// Collect every stylesheet in document order — `<style>` blocks (usually in
/// `<head>`, which body_root skips) and external `<link>`s — then parse ONCE
/// into precompiled selectors. Split from parsing so a DOM restored from a
/// snapshot re-collects styles without re-parsing HTML.
fn styles_for(dom: &html::Dom, url: &str) -> (String, Vec<css::FontFace>) {
    let mut sources = Vec::new();
    collect_sheets(dom, 0, &mut sources);
    let mut css_text = String::new();
    let mut ext_count = 0usize;
    // `@font-face` defs collected per-sheet: the cascade concatenates sheets,
    // which would lose each sheet's base URL for resolving relative src paths
    let mut faces: Vec<css::FontFace> = Vec::new();
    for src in &sources {
        match src {
            SheetSrc::Inline(text) => {
                faces.extend(css::collect_font_faces(text, url));
                css_text.push_str(text);
                css_text.push('\n');
            }
            SheetSrc::Link(href) => {
                let Some(abs) = resolve(url, href) else { continue };
                match fetch(&abs) {
                    Ok((bytes, _)) => {
                        // CSS is UTF-8 in practice (spec @charset unsupported — shortcut)
                        let text = String::from_utf8_lossy(&bytes);
                        faces.extend(css::collect_font_faces(&text, &abs));
                        css_text.push_str(&text);
                        css_text.push('\n');
                        ext_count += 1;
                    }
                    Err(e) => eprintln!("[css] stylesheet fetch failed {abs}: {e}"),
                }
            }
        }
    }
    eprintln!(
        "[css] {} bytes of style ({} external sheets)",
        css_text.len(),
        ext_count
    );
    (css_text, faces)
}

/// The shared back half of every load (plain fetch or a POST submission's
/// bytes): run `<script>`, layout, replace the kept page, wire out.
/// Also hands back a `form_nav` if one of the page's own inline scripts
/// form.submit()ed a POST (the caller decides whether to execute it).
fn settle_load(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    url: &str,
    max_w: f32,
    vh: f32,
    engine: EngineChoice,
    dom: html::Dom,
    css_text: String,
    faces: Vec<css::FontFace>,
    early: Option<&dyn Fn(PageWire)>,
) -> (Option<PageWire>, Option<String>, Option<runtime::FormNav>) {
    let mut kp = KeptPage {
        url: url.to_string(),
        dom,
        css_text,
        faces,
        scripts: None,
        engine,
        hover: None,
        imgs: ImgCache::default(),
        sheet: None,
        failed: false,
    };
    #[cfg(feature = "count-alloc")]
    let heap_mb = || crate::live_alloc_bytes() as f64 / (1024.0 * 1024.0);
    #[cfg(feature = "count-alloc")]
    let h0 = heap_mb();
    // Progressive first paint: the markup and its stylesheets, laid out,
    // before a single script runs. A cold github.com load spends ~5-7 s in
    // script execution and ~2 s more in the image phase; a window that stays
    // blank through all of it reads as broken no matter what the total is.
    // This pass skips webfonts (system fonts until the final paint) and every
    // image/media round trip, so it costs one cascade and puts text on screen.
    if let (Some(sink), true) = (early, first_paint_on()) {
        let saved = fonts.webfonts;
        fonts.set_choice(FontsChoice::B);
        let mut scratch = ImgCache::default();
        let (page, form_names) = layout_page(
            &kp.dom, &kp.css_text, &kp.faces, url, fonts, max_w, vh,
            &layout::FormState::default(), None, &mut scratch, &mut kp.sheet, true,
        );
        fonts.set_choice(saved);
        let stacks = fonts.stacks.borrow().clone();
        sink(page_to_wire(page, stacks, collect_out_faces(fonts), form_names));
    }
    // the script bodies are about to be asked for one by one: overlap their
    // round trips now that the first frame is out (see `preload`)
    preload(&script_urls(&kp.dom, url));
    // the markup's own `<img>` bytes: fetch them on a side thread so they
    // overlap the script phase instead of blocking it, and so the probes
    // after layout are cache hits rather than ~2-4 s of round trips
    // serialized behind script execution (CSS background images still start
    // at box-tree time, when their URLs are known).
    let early_imgs = image_urls(&kp.dom, url);
    std::thread::spawn(move || {
        preload_accept(&early_imgs, Some(IMAGE_ACCEPT), "preload (images, early)");
    });
    // same story for `<video>`: the probe only needs the container header
    // (64 KB), and the layout-time probe reads it back off disk
    let early_media = media_urls(&kp.dom, url);
    if !early_media.is_empty() {
        std::thread::spawn(move || {
            for u in early_media.iter().take(4) {
                let _ = media_prefix_probe(u);
            }
        });
    }
    // run every <script> (inline + src) BEFORE layout so the DOM is settled
    let mut scripts = runtime::make_runtime(engine, &mut kp.dom, url);
    let turn = if no_js() {
        // diagnostic switch (reload soak): skip script execution entirely
        runtime::Turn {
            dirty: false,
            nav: None,
            prevent_default: false,
            form_nav: None,
        }
    } else {
        scripts.run_scripts(&mut kp.dom)
    };
    if turn.dirty {
        // inline scripts mutated the DOM before the first layout — the
        // layout below already sees the mutated tree
        eprintln!("[js] scripts mutated the DOM before layout");
    }
    // scripts that render markup add `<img>` elements the parse-time preload
    // could not know about; those are the ones the final probe still pays for
    // (~0.4 s). Start them now, on a side thread, so the round trips overlap
    // the cascade instead of following it.
    let post_imgs = image_urls(&kp.dom, url);
    if !post_imgs.is_empty() {
        std::thread::spawn(move || {
            preload_accept(&post_imgs, Some(IMAGE_ACCEPT), "preload (images, post-script)");
        });
    }    #[cfg(feature = "count-alloc")]
    let h1 = heap_mb();
    kp.scripts = Some(scripts);
    let (page, form_names) = layout_page(
        &kp.dom, &kp.css_text, &kp.faces, url, fonts, max_w, vh,
        &layout::FormState::default(),
        // fresh page: the shell clears its hover cell on navigation too
        None,
        &mut kp.imgs,
        &mut kp.sheet,
        false,
    );
    #[cfg(feature = "count-alloc")]
    let h2 = heap_mb();
    let stacks = fonts.stacks.borrow().clone();
    let out_faces = collect_out_faces(fonts);
    let wire = page_to_wire(page, stacks, out_faces, form_names);
    #[cfg(feature = "count-alloc")]
    let h3 = heap_mb();
    *kept = Some(kp);
    #[cfg(feature = "count-alloc")]
    eprintln!(
        "[mem] phase dom={h0:.1} js={h1:.1} layout={h2:.1} wire={h3:.1} keep={:.1} MB",
        heap_mb()
    );
    (Some(wire), turn.nav, turn.form_nav)
}

/// Snapshot the worker font picker's faces into `css::FontFace` for the wire.
fn collect_out_faces(fonts: &Fonts) -> Vec<css::FontFace> {
    fonts
        .faces
        .iter()
        .map(|f| css::FontFace {
            family: f.family.clone(),
            srcs: f.srcs.clone(),
            wmin: f.wmin,
            wmax: f.wmax,
            italic: f.italic,
        })
        .collect()
}

/// Set an attribute on a DOM element node (case-insensitive name).
fn set_dom_attr(dom: &mut html::Dom, node: u32, name: &str, val: &str) {
    let Some(nr) = dom.nodes.get_mut(node as usize) else { return };
    if let html::NodeKind::Element(_, attrs) = &mut nr.kind {
        if let Some(slot) = attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            slot.1 = val.to_string();
        } else {
            let mut v: Vec<(String, String)> = attrs.iter().cloned().collect();
            v.push((name.to_string(), val.to_string()));
            *attrs = v.into_boxed_slice();
        }
    }
}

/// Remove an attribute from a DOM element node.
fn remove_dom_attr(dom: &mut html::Dom, node: u32, name: &str) {
    let Some(nr) = dom.nodes.get_mut(node as usize) else { return };
    if let html::NodeKind::Element(_, attrs) = &mut nr.kind {
        if let Some(i) = attrs.iter().position(|(k, _)| k.eq_ignore_ascii_case(name)) {
            let mut v: Vec<(String, String)> = attrs.iter().cloned().collect();
            v.remove(i);
            *attrs = v.into_boxed_slice();
        }
    }
}

// ---------------------------------------------------------------------------
// Real form submission: collect successful form-control pairs from the
// worker DOM, resolve the `<form>`'s method/action, then the loader worker
// executes GET (a normal load) or an `application/x-www-form-urlencoded`
// POST through the same fetch path — the response page is parsed/rendered
// exactly like any navigation.
// ---------------------------------------------------------------------------

/// A successful form-control pair ready for URL encoding (`name=value` order
/// preserved; document order — the spec-relevant order for urlencoded data).
pub(crate) fn collect_form_pairs(
    dom: &html::Dom,
    form: u32,
    live: &HashMap<u32, String>,
    submitter: Option<u32>,
) -> Vec<(String, String)> {
    fn walk(
        dom: &html::Dom,
        node: u32,
        live: &HashMap<u32, String>,
        submitter: Option<u32>,
        out: &mut Vec<(String, String)>,
    ) {
        for &c in &dom.nodes[node as usize].children {
            let tag = dom.tag(c).to_ascii_lowercase();
            // disabled controls never submit - neither by their own
            // attribute nor through a disabled <fieldset> ancestor (the
            // spec's first-<legend> escape hatch is not modelled)
            if control_disabled(dom, c) {
                continue;
            }
            if tag == "select" {
                // the select submits its selected option's value; the
                // options are not controls, so no recursion
                if let Some(name) = dom.attr(c, "name") {
                    if let Some(v) = selected_option_value(dom, c) {
                        out.push((name.to_string(), v));
                    }
                }
                continue;
            }
            if tag == "button" {
                // a submit button is a successful control only when it is
                // the one that submitted this form
                if Some(c) == submitter {
                    let ty = dom.attr(c, "type").unwrap_or("submit");
                    if ty.eq_ignore_ascii_case("submit") {
                        if let Some(name) = dom.attr(c, "name") {
                            out.push((
                                name.to_string(),
                                dom.attr(c, "value").unwrap_or("").to_string(),
                            ));
                        }
                    }
                }
                continue;
            }
            // honest submission set: inputs + textarea. `type=hidden` still
            // submits: it must never be editable, and the DOM (not the
            // layout) is the source of truth here. `type=file` stays out
            // (no picker UI exists).
            if matches!(tag.as_str(), "input" | "textarea") {
                if let Some(name) = dom.attr(c, "name") {
                    let name = name.to_string();
                    if tag == "textarea" {
                        // live typed text first (what the user sees), then a
                        // scripted `value` attribute, then the markup default
                        // (the textarea's own text content)
                        let value = live
                            .get(&c)
                            .cloned()
                            .or_else(|| dom.attr(c, "value").map(|s| s.to_string()))
                            .unwrap_or_else(|| crate::script::child_text(dom, c));
                        out.push((name, value));
                    } else {
                        let ty = dom
                            .attr(c, "type")
                            .unwrap_or("text")
                            .trim()
                            .to_ascii_lowercase();
                        match ty.as_str() {
                            // the activated submit button contributes its
                            // own pair; reset/button/image never submit
                            "submit" => {
                                if Some(c) == submitter {
                                    out.push((
                                        name,
                                        dom.attr(c, "value").unwrap_or("").to_string(),
                                    ));
                                }
                            }
                            "reset" | "button" | "image" | "file" => {}
                            // checked state lives engine-side in the worker
                            // DOM (the shell's click toggles carry through
                            // `toggle_control`): only checked boxes submit
                            "checkbox" | "radio" => {
                                if dom.attr(c, "checked").is_some() {
                                    // HTML default when no value attr: "on"
                                    let value = dom
                                        .attr(c, "value")
                                        .unwrap_or("on")
                                        .to_string();
                                    out.push((name, value));
                                }
                            }
                            // text-ish inputs + the inert generic types
                            // (range/color/date - static value attr, no
                            // interaction to change it: submitted as-is)
                            _ => {
                                let value = live
                                    .get(&c)
                                    .cloned()
                                    .or_else(|| dom.attr(c, "value").map(|s| s.to_string()))
                                    .unwrap_or_default();
                                out.push((name, value));
                            }
                        }
                    }
                }
            }
            walk(dom, c, live, submitter, out);
        }
    }
    let mut out = Vec::new();
    walk(dom, form, live, submitter, &mut out);
    out
}

/// A control is disabled by its own `disabled` attribute or by sitting in a
/// disabled `<fieldset>`.
fn control_disabled(dom: &html::Dom, node: u32) -> bool {
    if dom.attr(node, "disabled").is_some() {
        return true;
    }
    let mut cur = node;
    while cur != 0 {
        cur = match dom.nodes.get(cur as usize) {
            Some(n) => n.parent,
            None => return false,
        };
        if cur == 0 {
            break;
        }
        if dom.tag(cur) == "fieldset" && dom.attr(cur, "disabled").is_some() {
            return true;
        }
    }
    false
}

/// Submitted value of a `<select>`: the selected option (browsers select the
/// first option when none is marked), `value` attribute or its label text.
/// shortcut: a `multiple` select submits only that one value - there is no
/// popup to pick several options with.
fn selected_option_value(dom: &html::Dom, select: u32) -> Option<String> {
    fn collect(dom: &html::Dom, node: u32, out: &mut Vec<u32>) {
        for &c in &dom.nodes[node as usize].children {
            match dom.tag(c) {
                "option" => out.push(c),
                "optgroup" | "select" => collect(dom, c, out),
                _ => {}
            }
        }
    }
    fn disabled_option(dom: &html::Dom, opt: u32) -> bool {
        if dom.attr(opt, "disabled").is_some() {
            return true;
        }
        let p = dom.nodes[opt as usize].parent;
        p != 0 && dom.tag(p) == "optgroup" && dom.attr(p, "disabled").is_some()
    }
    let mut opts = Vec::new();
    collect(dom, select, &mut opts);
    // remembered selection wins even when disabled (the browser still has
    // it selected); the implicit first selection skips disabled options
    let pick = opts
        .iter()
        .copied()
        .find(|&o| dom.attr(o, "selected").is_some())
        .or_else(|| opts.iter().copied().find(|&o| !disabled_option(dom, o)))?;
    Some(match dom.attr(pick, "value") {
        Some(v) => v.to_string(),
        None => crate::script::child_text(dom, pick)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    })
}

/// `application/x-www-form-urlencoded` serial (the `url` crate's
/// form_urlencoded serializer: space → `+`, `&`/`=`/non-ASCII escaped).
pub(crate) fn urlencode_pairs(pairs: &[(String, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (n, v) in pairs {
        ser.append_pair(n, v);
    }
    ser.finish()
}

/// Nearest enclosing `<form>` (the activated node itself if it is one —
/// `form.submit()` targets the form directly). None = nothing to submit.
pub(crate) fn form_ancestor(dom: &html::Dom, node: u32) -> Option<u32> {
    if dom.tag(node) == "form" {
        return Some(node);
    }
    let mut cur = node;
    while cur != 0 {
        cur = dom.nodes.get(cur as usize)?.parent;
        if cur != 0 && dom.tag(cur) == "form" {
            return Some(cur);
        }
    }
    None
}

/// Resolve the form's `action` (missing action = the current document URL).
/// Unlike link `resolve`, same-document targets are legal here (a POST/GET
/// to the page's own URL — the anchor rule would veto those) and fragments
/// are dropped (form data replaces the query either way).
pub(crate) fn resolve_form_target(base: &str, action: Option<&str>) -> Option<String> {
    let mut target = match action.map(|a| a.trim()) {
        None | Some("") => url::Url::parse(base).ok()?,
        Some(a) => url::Url::parse(base).ok()?.join(a).ok()?,
    };
    if !matches!(target.scheme(), "http" | "https" | "data") {
        // javascript:/mailto:/… form actions are not worker-loadable
        return None;
    }
    target.set_fragment(None);
    Some(target.to_string())
}

/// GET submission URL: the form data serial REPLACES the target's query
/// (spec: mutate the parsed action with the urlencoded data set; real
/// browsers drop the original query — `action="/s?q=1"` + field q=2 →
/// `?q=2`). Fragments dropped (they'd survive a navigation but a query edit
/// is the whole point here).
pub(crate) fn form_get_url(target: &str, data: &str) -> Option<String> {
    let mut u = url::Url::parse(target).ok()?;
    u.set_fragment(None);
    u.set_query(if data.is_empty() { None } else { Some(data) });
    Some(u.to_string())
}

/// GET or POST.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmitMethod {
    Get,
    Post,
}

impl SubmitMethod {
    fn as_str(self) -> &'static str {
        match self {
            SubmitMethod::Get => "GET",
            SubmitMethod::Post => "POST",
        }
    }
    /// `<form method>` spelling (default GET per the HTML spec; anything
    /// that is not POST is treated as GET).
    fn from_form(m: Option<&str>) -> SubmitMethod {
        match m.map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "post" => SubmitMethod::Post,
            _ => SubmitMethod::Get,
        }
    }
}

/// One resolved submission ready for the worker to execute (or for a
/// script's `form.submit()`: GET rides the ordinary nav channel, POST the
/// body-carrying `FormNav` channel).
pub(crate) struct SubmissionReq {
    pub method: SubmitMethod,
    /// absolute target (GET: the built `?name=value&…` URL)
    pub target: String,
    /// urlencoded data (GET: the query serial; POST: the request body)
    pub body: Vec<u8>,
    /// `Content-Type` for a POST body (urlencoded or multipart)
    pub content_type: String,
}

/// True when `node` is a submit button (`<button>` without a `type`, or with
/// `type=submit`; `<input type=submit>`).
fn is_submit_control(dom: &html::Dom, node: u32) -> bool {
    match dom.tag(node) {
        "button" => dom
            .attr(node, "type")
            .map(|t| t.eq_ignore_ascii_case("submit"))
            .unwrap_or(true),
        "input" => dom
            .attr(node, "type")
            .is_some_and(|t| t.eq_ignore_ascii_case("submit")),
        _ => false,
    }
}

/// True when `node` takes typed text - the controls whose Enter key performs
/// the implicit submission (hidden/button/checkable kinds do not).
fn is_text_control(dom: &html::Dom, node: u32) -> bool {
    if dom.tag(node) == "textarea" {
        return true;
    }
    if dom.tag(node) != "input" {
        return false;
    }
    let ty = dom.attr(node, "type").unwrap_or("text").to_ascii_lowercase();
    !matches!(
        ty.as_str(),
        "checkbox" | "radio" | "submit" | "reset" | "button" | "image" | "file" | "hidden"
    )
}

/// Which control counts as the submitter of this submission. The activated
/// submit button names itself; Enter in a text field is an implicit
/// submission, which the browser performs by "clicking" the form's default
/// (first enabled) submit button, so that button names itself; anything else
/// (notably `form.submit()`, which activates the form itself) names nobody.
fn implicit_submitter(dom: &html::Dom, form: u32, activated: u32) -> Option<u32> {
    if is_submit_control(dom, activated) {
        return Some(activated);
    }
    if !is_text_control(dom, activated) {
        return None;
    }
    (1..dom.nodes.len() as u32).find(|&n| {
        is_submit_control(dom, n)
            && !control_disabled(dom, n)
            && form_ancestor(dom, n) == Some(form)
    })
}

/// Build the submission for the `<form>` that encloses `activated` (walk up
/// from the submit button / the Enter-activated field). `live` holds the
/// UI thread's typed values by node id (the value store rides every event
/// job); the worker DOM's `value` attributes are the scripted fallback.
pub(crate) fn build_submission(
    dom: &html::Dom,
    activated: u32,
    live: &HashMap<u32, String>,
    files: &HashMap<u32, layout::FilePick>,
    base: &str,
) -> Option<SubmissionReq> {
    let form = form_ancestor(dom, activated)?;
    let pairs = collect_form_pairs(dom, form, live, implicit_submitter(dom, form, activated));
    let action = resolve_form_target(base, dom.attr(form, "action"))
        .or_else(|| resolve_form_target(base, None))
        .or_else(|| {
            eprintln!("[form] submission action unresolvable against {base}");
            None
        })?;
    let method = SubmitMethod::from_form(dom.attr(form, "method"));
    // multipart when the form asks for it or a file was picked; otherwise the
    // body stays the plain urlencoded serial
    let picked = picked_files(dom, form, files);
    let multipart = method == SubmitMethod::Post
        && (!picked.is_empty()
            || dom
                .attr(form, "enctype")
                .is_some_and(|e| e.eq_ignore_ascii_case("multipart/form-data")));
    let (body, content_type) = if multipart {
        multipart_body(&pairs, &picked)
    } else {
        (urlencode_pairs(&pairs).into_bytes(), URLENCODED.to_string())
    };
    let target = match method {
        SubmitMethod::Get => form_get_url(&action, &String::from_utf8_lossy(&body)).unwrap_or(action),
        SubmitMethod::Post => action,
    };
    Some(SubmissionReq {
        method,
        target,
        body,
        content_type,
    })
}

const URLENCODED: &str = "application/x-www-form-urlencoded";

/// Successful `<input type=file>` controls of `form` that have a pick, in
/// document order: (name attribute, pick). Disabled controls and disabled
/// fieldsets are skipped exactly like `collect_form_pairs`.
fn picked_files(
    dom: &html::Dom,
    form: u32,
    files: &HashMap<u32, layout::FilePick>,
) -> Vec<(String, layout::FilePick)> {
    fn walk(
        dom: &html::Dom,
        node: u32,
        files: &HashMap<u32, layout::FilePick>,
        out: &mut Vec<(String, layout::FilePick)>,
    ) {
        for &c in &dom.nodes[node as usize].children {
            if dom.tag(c) == "input"
                && dom.attr(c, "type").is_some_and(|t| t.eq_ignore_ascii_case("file"))
                && !control_disabled(dom, c)
            {
                if let (Some(name), Some(pick)) = (dom.attr(c, "name"), files.get(&c)) {
                    out.push((name.to_string(), pick.clone()));
                }
            }
            walk(dom, c, files, out);
        }
    }
    let mut out = Vec::new();
    walk(dom, form, files, &mut out);
    out
}

/// `multipart/form-data` body: text pairs first, then one part per file.
/// Returns the bytes and the matching `Content-Type` header value. A file
/// that cannot be read is skipped (logged): the rest of the submission
/// still goes out, the way a browser drops an inaccessible part.
fn multipart_body(
    pairs: &[(String, String)],
    files: &[(String, layout::FilePick)],
) -> (Vec<u8>, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let boundary = format!("----MazdaFormBoundary{:x}{nanos:x}", std::process::id());
    let mut body: Vec<u8> = Vec::new();
    for (name, value) in pairs {
        push_str(
            &mut body,
            &format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n",
                header_escape(name)
            ),
        );
        body.extend_from_slice(value.as_bytes());
        push_str(&mut body, "\r\n");
    }
    for (name, file) in files {
        let bytes = match std::fs::read(&file.path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[form] upload {} unreadable: {e}", file.path);
                continue;
            }
        };
        push_str(
            &mut body,
            &format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
                header_escape(name),
                header_escape(&file.name)
            ),
        );
        body.extend_from_slice(&bytes);
        push_str(&mut body, "\r\n");
    }
    push_str(&mut body, &format!("--{boundary}--\r\n"));
    (body, format!("multipart/form-data; boundary={boundary}"))
}

fn push_str(body: &mut Vec<u8>, s: &str) {
    body.extend_from_slice(s.as_bytes());
}

/// Keep a form field name/filename from breaking the part header: quotes,
/// backslashes and line breaks become `_`.
fn header_escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '"' | '\\' | '\r' | '\n') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// is a form POST allowed for a `data:` target? no — fetch_post cannot read
/// a response from an inline document; the caller turns it into the error
/// page (honest, same wording as a failed fetch).
/// Every `Set-Cookie` value of a response. `header()` returns only the
/// first one, and a login response often sets several at once.
fn set_cookie_values(resp: &ureq::Response) -> Vec<String> {
    resp.all("set-cookie").iter().map(|s| s.to_string()).collect()
}

pub(crate) fn fetch_post(
    url: &str,
    body: &[u8],
    content_type: &str,
) -> Result<(Vec<u8>, Option<String>), String> {
    let mut req = http_agent().post(url).set("Content-Type", content_type);
    // a form POST is a document navigation: the same identity every GET uses
    for (name, value) in net::request_headers(None) {
        req = req.set(name, &value);
    }
    if let Some(c) = cookie::request_header(url) {
        req = req.set("Cookie", &c);
    }
    // `send` treats a slice as a reader (chunked); `send_bytes` sends a
    // Content-Length body, which every form endpoint accepts
    let resp = req.send_bytes(body).map_err(|e| e.to_string())?;
    cookie::store_response(url, &set_cookie_values(&resp));
    let content_type = resp.header("content-type").map(|s| s.to_string());
    // same 20MB cap as every GET fetch (no disk cache: a POST response is
    // per-submission state, caching it would be dishonest)
    const CAP: usize = 20 * 1024 * 1024;
    let mut reader = resp.into_reader();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    while buf.len() < CAP {
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok((buf, content_type))
}

/// Every form control's node id → `name` attribute (for `[form] submit`
/// logging in the shell, which can't read the DOM).
fn collect_form_names(dom: &html::Dom) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    for i in 1..dom.nodes.len() as u32 {
        let tag = dom.tag(i);
        if matches!(tag, "input" | "textarea" | "select" | "button") {
            if let Some(name) = dom.attr(i, "name") {
                out.insert(i, name.to_string());
            }
        }
    }
    out
}

/// Fetch + parse one page's HTML and every stylesheet (network + parse —
/// the expensive half of the pipeline, done once per navigation).
fn fetch_and_parse(url: &str, restore: bool) -> (html::Dom, String, Vec<css::FontFace>, bool) {
    // Phase 3: a tab switch / back restores the snapshot instead of
    // re-fetching + re-tokenizing. The tree is the PRE-SCRIPT one, so
    // `settle_load` still replays the page's scripts (listeners intact).
    if restore {
        if let Some(dom) = snapshot_read(url) {
            eprintln!(
                "[dom] restored snapshot for {url} ({} nodes, no fetch)",
                dom.nodes.len()
            );
            let (css_text, faces) = styles_for(&dom, url);
            return (dom, css_text, faces, false);
        }
        eprintln!("[dom] no snapshot for {url} - fetching");
    }
    let source = match fetch(url) {
        Ok((bytes, ct)) => {
            let s = html::decode_html(&bytes, ct.as_deref());
            eprintln!(
                "[mem] after fetch: {:.1} MB (html {} KB, {})",
                working_set_mb(),
                s.len() / 1024,
                ct.as_deref().unwrap_or("no content-type")
            );
            s
        }
        Err(e) => {
            eprintln!("[nav] fetch failed for {url}: {e}");
            // never snapshot an error page: a later restore would show it
            // forever even after the site came back (the live page is marked
            // `failed` for the same reason)
            let (dom, css_text, faces) =
                parse_source_with_styles(url, format!("<p>Failed to load: {e}</p>"));
            return (dom, css_text, faces, true);
        }
    };
    let dom = html::parse(&source);
    eprintln!("[mem] after parse: {:.1} MB ({} nodes)", working_set_mb(), dom.nodes.len());
    snapshot_write(url, &dom);
    // overlap the page's subresource round trips before the sequential
    // phases start asking for them (see `preload`). Stylesheets first: the
    // first paint only needs those, and `settle_load` fetches the script
    // bodies once that frame is out.
    preload(&stylesheet_urls(&dom, url));
    let (css_text, faces) = styles_for(&dom, url);
    (dom, css_text, faces, false)
}

/// The markup's own subresources: external stylesheets and `<script src>`
/// (module entries included, a `nomodule` fallback skipped when the page has
/// a module build — it never runs, so it is never worth a request).
/// Stylesheet URLs of the document, in order: the first paint needs these and
/// nothing else.
fn stylesheet_urls(dom: &html::Dom, url: &str) -> Vec<String> {
    let mut sheets = Vec::new();
    collect_sheets(dom, 0, &mut sheets);
    let mut out = Vec::new();
    for s in &sheets {
        if let SheetSrc::Link(href) = s {
            if let Some(abs) = resolve(url, href) {
                out.push(abs);
            }
        }
    }
    out
}

/// `<script src>` URLs (module/nomodule selection applied): the bodies the
/// script phase is about to ask for.
fn script_urls(dom: &html::Dom, url: &str) -> Vec<String> {
    let scripts = crate::script::collect_scripts(dom);
    let has_modules = scripts.iter().any(|s| s.is_module);
    let mut out = Vec::new();
    for s in &scripts {
        if s.is_nomodule && has_modules {
            continue;
        }
        if let Some(src) = &s.src {
            if let Some(abs) = resolve(url, src) {
                out.push(abs);
            }
        }
    }
    out
}

/// Every subresource the markup names (stylesheets, then script bodies).
fn subresource_urls(dom: &html::Dom, url: &str) -> Vec<String> {
    let mut out = stylesheet_urls(dom, url);
    out.extend(script_urls(dom, url));
    out
}

/// `<img src>` URLs the markup names, resolved against the page. The box-tree
/// pass asks for the same URLs (`pending_image_urls`); fetching them up front
/// overlaps their round trips with script execution instead of paying them
/// after it.
fn image_urls(dom: &html::Dom, url: &str) -> Vec<String> {
    let mut out = Vec::new();
    for i in 1..dom.nodes.len() as u32 {
        if !dom.tag(i).eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = dom.attr(i, "src") else { continue };
        if src.starts_with('<') || src.starts_with("data:") {
            continue; // inline SVG / data: URIs need no round trip
        }
        if let Some(abs) = resolve_img(url, src) {
            out.push(abs);
        }
    }
    out
}

/// `<video>` sources the markup names — the element's own `src` plus its
/// `<source>` children — resolved against the page. The media probe later
/// asks for exactly these; warming their container header early means it
/// reads them off disk instead of paying a round trip after script execution.
fn media_urls(dom: &html::Dom, url: &str) -> Vec<String> {
    let mut out = Vec::new();
    for i in 1..dom.nodes.len() as u32 {
        if !dom.tag(i).eq_ignore_ascii_case("video") {
            continue;
        }
        if let Some(src) = dom.attr(i, "src") {
            if let Some(abs) = resolve_img(url, src) {
                out.push(abs);
            }
        }
        for &c in &dom.nodes[i as usize].children {
            if !dom.tag(c).eq_ignore_ascii_case("source") {
                continue;
            }
            if let Some(src) = dom.attr(c, "src") {
                if let Some(abs) = resolve_img(url, src) {
                    out.push(abs);
                }
            }
        }
    }
    out
}

/// Cascade + box tree + layout + image decode — the re-runnable half.
/// Called for fresh loads, resizes, form events and script timers alike;
/// `state` seeds focus/values so the controls paint what the user typed.
/// Returns the page plus the node→name table for `[form]` logging.
fn layout_page(
    dom: &html::Dom,
    css_text: &str,
    faces: &[css::FontFace],
    url: &str,
    fonts: &mut Fonts,
    max_w: f32,
    vh: f32,
    state: &layout::FormState,
    hover: Option<u32>,
    imgs: &mut ImgCache,
    // The page's parsed stylesheet, kept between passes: a page is laid out
    // again after its scripts run, on a resize, on a form keystroke, on a tab
    // switch - and parsing github's 6.5 MB of CSS costs 144 ms each time.
    // Only the viewport width changes what parsing decides (media queries),
    // so that is the key. `Sheet::begin_pass` drops the per-node memos that
    // described the DOM of the previous pass. None = nothing parsed yet.
    sheet_cache: &mut Option<(f32, css::Sheet)>,
    // first_paint: the caller is going to lay this page out again once the
    // scripts have run, so skip everything that only the final pass needs
    // (image + media probing, i.e. network round trips). The early frame
    // shows markup + stylesheets and nothing that costs a download.
    first_paint: bool,
) -> (Page, HashMap<u32, String>) {
    let root = dom.body_root();
    // MAZDA_TIME=1 prints per-stage layout timings: the reflow cost is what
    // a window resize pays, so knowing which stage dominates is the only way
    // to make fullscreen feel instant.
    let timing = std::env::var_os("MAZDA_TIME").is_some();
    let mut stage_at = Instant::now();
    macro_rules! stage {
        ($name:expr) => {
            if timing {
                eprintln!(
                    "[time] {}: {:.0} ms",
                    $name,
                    stage_at.elapsed().as_secs_f64() * 1000.0
                );
                stage_at = Instant::now();
            }
        };
    }
    // media queries evaluate against the window's CSS pixel width
    let viewport = max_w + 2.0 * PADDING as f32;
    let (mut sheet, reused) = match sheet_cache.take() {
        Some((w, s)) if w == viewport => (s, true),
        _ => (css::parse_for(css_text, viewport), false),
    };
    sheet.begin_pass();
    stage!("css parse");
    // viewport units (vh/vw/vmin/vmax) resolve against the real window size
    sheet.set_viewport(viewport, vh);
    // the hover cell (shell hit test) — `:hover` rules match against it
    sheet.set_hovered(hover);
    if reused {
        eprintln!("[css] sheet reused ({} rules)", sheet.rule_count());
    } else {
        eprintln!("[css] {} rules", sheet.rule_count());
    }
    // cascade writes font-family stack ids into the font picker's own table
    sheet.set_stacks(fonts.stacks.clone());
    // FontFace isn't Clone (all fields are) — copy for begin_page, which
    // takes ownership and the KeptPage keeps its own copy for re-layouts
    let owned_faces: Vec<css::FontFace> = faces
        .iter()
        .map(|f| css::FontFace {
            family: f.family.clone(),
            srcs: f.srcs.clone(),
            wmin: f.wmin,
            wmax: f.wmax,
            italic: f.italic,
        })
        .collect();
    // the real <html> element scopes custom-property themes (below) and is the
    // canvas-background fallback
    let html_el = dom.nodes[0]
        .children
        .iter()
        .copied()
        .find(|&c| dom.tag(c) == "html")
        .unwrap_or(0);
    sheet.scope_vars(dom, html_el);
    stage!("scope vars");
    fonts.begin_page(owned_faces);

    // cascade <html> FIRST: its computed font-size is the root of `rem` (see
    // css::ROOT_PX), so it must be known before the body's subtree resolves
    // lengths. Reset to the 16px initial value first so a previous page's
    // root can't leak into this one (the cell is per-thread).
    css::set_root_px(16.0);
    let html_st = sheet.resolve(dom, html_el, Style::new(), dom.attr(html_el, "style"));
    css::set_root_px(html_st.style.size);

    // cascade over the body element itself: page bg + initial inherited style
    // (spec: the canvas background comes from <body> if it has one, else
    // <html>'s — body wins per the HTML spec)
    let mut body = sheet.resolve(dom, root, Style::new(), dom.attr(root, "style"));
    let bg = body.bg.or(html_st.bg).unwrap_or(BG);
    // `cursor` inherits down the DOM at box-build time (layout.rs threads
    // the effective value from box to box) — <html> is body's only
    // ancestor, so seed body's undeclared cursor from its cascade
    if body.bx.cursor.is_none() {
        body.bx.cursor = html_st.bx.cursor;
    }

    let mut root_box = layout::build_scoped(dom, root, body.style, body.bx, &sheet, body.vars.as_ref());
    stage!("cascade + box tree");
    if timing {
        let (tested, matched, decls) = css::cascade_stats_take();
        let (probes, hops) = sheet.scheme_stats_take();
        eprintln!(
            "[css] {tested} candidates tested, {matched} matched, {decls} declarations applied"
        );
        eprintln!(
            "[css] selector: {} ancestor hops",
            css::sel_hops_take()
        );
        eprintln!("[css] color-scheme: {probes} selector tests, {hops} ancestor hops");
        eprintln!(
            "[time] cascade rules: {:.0} ms of that",
            layout::cascade_ns_take() as f64 / 1e6
        );
    }
    eprintln!(
        "[mem] after box tree: {:.1} MB ({} boxes)",
        working_set_mb(),
        layout::count_boxes(&root_box)
    );
    // nothing below touches the sheet: park it for this page's next pass (the
    // post-script layout of a cold load, a resize, a keystroke, a switch)
    *sheet_cache = Some((viewport, sheet));

    // fetch + probe every <img> header (disk-cached; no pixel decode yet —
    // layout only needs the intrinsic size to size the box)
    let mut img_budget = IMG_PIXEL_BUDGET;
    // the probe fetches every image in document order, one at a time; overlap
    // the round trips first (same reason as the markup preload)
    let mut img_urls = Vec::new();
    if !first_paint {
        pending_image_urls(&root_box, url, &mut img_urls);
        preload_accept(&img_urls, Some(IMAGE_ACCEPT), "preload (images)");
    }
    let (n_img, skipped) = if first_paint {
        // inline <svg> costs no request, so it already has pixels to paint
        probe_images_local(&mut root_box)
    } else {
        probe_images(&mut root_box, url)
    };
    stage!("image probe");
    if n_img > 0 || skipped > 0 {
        eprintln!("[img] {n_img} probed, {skipped} skipped (format)");
    }
    // `<video>`: fetch + container probe (no decoding yet — the renderer owns
    // the player; layout only needs the intrinsic size and the DRM answer)
    let (n_media, bad_media) = if first_paint {
        (0, 0)
    } else {
        probe_media(&mut root_box, url)
    };
    stage!("media probe");
    if n_media > 0 || bad_media > 0 {
        eprintln!("[media] {n_media} playable, {bad_media} refused");
    }

    // layout_in_forms applies `state` (focus + typed values) so the caret,
    // accent border and live text paint correctly
    let mut laid = layout::layout_in_forms(&mut root_box, max_w, vh, &|s: &str, st: &Style| fonts.measure(s, st), state);
    stage!("layout + text");
    // decode pixels only now, one image at a time, resampled straight to the
    // laid-out display size — full-resolution buffers never accumulate
    let mut sticky_next = 0usize; // sticky-group index counter, see decode_images
    let mut backdrop_next = 0usize; // backdrop-group index counter, same story
    let decodes_before = imgs.decodes;
    let n_px = decode_images(
        &mut root_box,
        url,
        &mut laid,
        &mut img_budget,
        false,
        None,
        false,
        &mut sticky_next,
        &mut backdrop_next,
        imgs,
        &fonts,
    );
    stage!("image decode");
    if n_px > 0 {
        eprintln!(
            "[img] {n_px} drawn ({} decoded, {} from the relayout memo)",
            imgs.decodes - decodes_before,
            n_px.saturating_sub(imgs.decodes - decodes_before)
        );
    }
    // videos whose first frame landed in `laid.images` become renderer players
    let mut media = Vec::new();
    collect_media_entries(&root_box, false, &mut media);
    if !media.is_empty() {
        eprintln!("[media] {} player(s) handed to the renderer", media.len());
    }
    eprintln!(
        "[mem] after layout: {:.1} MB ({} runs)",
        working_set_mb(),
        laid.runs.len()
    );
    // MAZDA_DUMP_BOXES=2 walks the laid-out box tree: tag.class, rect, display,
    // position, tracks. The fastest way to answer "who is this box and why is it
    // that wide" without guessing from a screenshot.
    if std::env::var("MAZDA_DUMP_BOXES").as_deref() == Ok("2") {
        fn walk(b: &layout::BoxNode, dom: &html::Dom, depth: usize) {
            let what = match b.node {
                Some(n) => {
                    let cls = dom.attr(n, "class").unwrap_or("");
                    if cls.is_empty() {
                        dom.tag(n).to_string()
                    } else {
                        format!("{}.{}", dom.tag(n), cls)
                    }
                }
                None if b.is_text => format!("\"{}\"", b.text),
                None => "(anon)".to_string(),
            };
            let cols = b
                .bx
                .cols
                .iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(",");
            eprintln!(
                "[box] {}{} x={:.0} y={:.0} w={:.0} h={:.0} disp={:?} pos={:?}{}{}{}{}{}{}",
                "  ".repeat(depth),
                what,
                b.x,
                b.y,
                b.w,
                b.h,
                b.bx.display,
                b.bx.pos,
                if cols.is_empty() { String::new() } else { format!(" cols=[{cols}]") },
                if b.bx.grid_area.is_some() { format!(" area={:?}", b.bx.grid_area) } else { String::new() },
                if b.bx.width.is_some() || b.bx.max_width.is_some() {
                    format!(
                        " width={:?} max={:?} cb={} pad={:?} border={:?}",
                        b.bx.width, b.bx.max_width, b.bx.content_box, b.bx.pad, b.bx.border
                    )
                } else {
                    String::new()
                },
                if b.bx.grow > 0.0 { format!(" grow={}", b.bx.grow) } else { String::new() },
                if b.bx.wrap { " wrap".to_string() } else { String::new() },
                if b.bx.margin_x_auto || b.bx.margin_l_auto || b.bx.margin_r_auto {
                    format!(
                        " auto=l{}r{}",
                        b.bx.margin_l_auto as u8, b.bx.margin_r_auto as u8
                    )
                } else {
                    String::new()
                },
            );
            for c in &b.children {
                walk(c, dom, depth + 1);
            }
        }
        walk(&root_box, dom, 0);
    }
    let dump_runs = std::env::var("MAZDA_DUMP_RUNS").unwrap_or_default();
    // "0" = the `total height` line only (what the visual harness reads).
    // Dumping every link/underline is NOT free when stderr is a redirected
    // pipe: each `eprintln!` is a separate write, which added seconds to a
    // reflow and hid the real layout cost inside the timing lines.
    if !dump_runs.is_empty() {
        eprintln!("[layout] total height={:.0}", laid.height);
        if dump_runs != "0" {
            let all = dump_runs == "2"; // =2 dumps every run, not just 6
            for (i, r) in laid.runs.iter().enumerate() {
                if !all && i >= 6 {
                    break;
                }
                eprintln!(
                    "[run {i}] y={:.0} x={:.0} w={:.0} ul={} c={:06x} text={:?}",
                    r.y,
                    r.x,
                    r.width,
                    r.style.underline as u8,
                    r.style.color,
                    r.text
                );
            }
            for (i, r) in laid.runs.iter().filter(|r| r.link.is_some()).enumerate() {
                eprintln!(
                    "[link {i}] y={:.0} x={:.0} w={:.0} href={:?} text={:?}",
                    r.y, r.x, r.width, r.link, r.text
                );
            }
            // underline channel: text-decoration bars (a rect channel entry here is
            // a decoration, not a border/background)
            for (i, u) in laid.underline_rects.iter().enumerate() {
                eprintln!("[ul {i}] x={:.0} y={:.0} w={:.0} h={:.0} c={:06x}", u.0, u.1, u.2, u.3, u.4);
            }
            if std::env::var("MAZDA_DUMP_BOXES").is_ok() {
                // background/border rects in paint order (z is the stacking key)
                for (i, r) in laid.rects.iter().enumerate() {
                    eprintln!(
                        "[rect {i}] x={:.0} y={:.0} w={:.0} h={:.0} c={:06x} z={:.0}",
                        r.0, r.1, r.2, r.3, r.4, r.5
                    );
                }
            }
        }
    }
    stage!("dump");
    let form_names = collect_form_names(dom);
    // `<a download>` targets the renderer saves instead of navigating
    let downloads = collect_downloads(dom, url);
    // inspector identity for every hit-test node (the UI thread has no DOM)
    let elements = collect_elements(dom, &laid.hover_targets);
    stage!("elements");
    let page = Page {
        url: url.to_string(),
        laid,
        media,
        downloads,
        bg,
        elements,
    };
    (page, form_names)
}

/// Every `<a href download>` in document order, href resolved against the
/// page. A present-but-empty `download` value means "derive the name from the
/// URL", which the renderer does.
fn collect_downloads(dom: &html::Dom, base: &str) -> Vec<layout::DownloadLink> {
    let mut out = Vec::new();
    for n in 0..dom.nodes.len() as u32 {
        if dom.tag(n) != "a" {
            continue;
        }
        let Some(download) = dom.attr(n, "download") else {
            continue;
        };
        let Some(href) = dom.attr(n, "href").map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(href) = resolve(base, href) else { continue };
        out.push(layout::DownloadLink {
            href,
            name: download.trim().to_string(),
        });
    }
    out
}

/// One stylesheet source found in document order.
enum SheetSrc {
    Inline(String),
    /// href of a `<link rel=stylesheet>`, unresolved
    Link(String),
}

/// Walk the box tree fetching every `<img src>` and parsing only its header
/// (disk-cached). Returns (probed, skipped); skipped = bad URL/format. Boxes
/// keep `Probed` (or `Pending` on failure) so declared sizes still reserve
/// space even when the pixels will never decode. No budget here: headers cost
/// no pixels — the budget is charged at decode time against display size.
fn probe_images(n: &mut layout::BoxNode, base: &str) -> (usize, usize) {
    probe_images_in(n, base, true)
}

/// The first frame's probe: inline `<svg>` only. Its markup needs no
/// download, so the logo/nav icons can be rasterized with the early frame
/// instead of waiting for the network images the second pass fetches.
fn probe_images_local(n: &mut layout::BoxNode) -> (usize, usize) {
    probe_images_in(n, "", false)
}

fn probe_images_in(n: &mut layout::BoxNode, base: &str, network: bool) -> (usize, usize) {
    if no_images() {
        // diagnostic switch (reload soak): no image fetch/decode at all
        n.img = None;
        return (0, 0);
    }
    let mut probed = 0usize;
    let mut skipped = 0usize;
    if let Some(layout::ImgSlot::Pending(src)) = &n.img {
        let src = src.to_string();
        if src.starts_with('<') {
            // inline <svg> markup (no fetch) — probe intrinsic size directly
            match svg::probe(src.as_bytes()) {
                Some((w, h)) => {
                    probed += 1;
                    n.img = Some(layout::ImgSlot::Probed {
                        src: std::rc::Rc::from(src.as_str()),
                        w,
                        h,
                    });
                }
                None => skipped += 1,
            }
        } else if !network {
            // leave Pending: the final pass fetches it
        } else if let Some(abs) = resolve_img(base, &src) {
            match fetch_image(&abs) {
                Ok((bytes, _)) => match image::probe(&bytes).or_else(|| svg::probe(&bytes)) {
                    Some((w, h)) => {
                        probed += 1;
                        n.img = Some(layout::ImgSlot::Probed {
                            src: std::rc::Rc::from(src.as_str()),
                            w,
                            h,
                        });
                    }
                    None => {
                        eprintln!("[img] no size from {} ({} bytes, starts {:?})",
                            log_url(&abs), bytes.len(), &bytes[..bytes.len().min(12)]);
                        skipped += 1; // AVIF/corrupt
                    }
                },
                Err(e) => {
                    eprintln!("[img] fetch failed {}: {e}", log_url(&abs));
                    skipped += 1;
                }
            }
        } else {
            skipped += 1; // non-navigable scheme (data:, javascript: …)
        }
    }
    for c in &mut n.children {
        let (p, s) = probe_images_in(c, base, network);
        probed += p;
        skipped += s;
    }
    (probed, skipped)
}

/// Every `<img>` the probe below will fetch, as absolute URLs (inline `<svg>`
/// markup is parsed in place and needs no request).
fn pending_image_urls(n: &layout::BoxNode, base: &str, out: &mut Vec<String>) {
    if let Some(layout::ImgSlot::Pending(src)) = &n.img {
        if !src.starts_with('<') {
            if let Some(abs) = resolve_img(base, src) {
                out.push(abs);
            }
        }
    }
    for c in &n.children {
        pending_image_urls(c, base, out);
    }
}

fn no_media() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MAZDA_NO_MEDIA").is_some())
}

/// Walk every `<video>`, fetch its bytes once, parse the MP4 header, and
/// write a plain `.mp4` next to the other cache files for the renderer to
/// open. Only the container is inspected here — decoding happens in the
/// renderer process, on its main thread.
///
/// The intrinsic size is what layout needs (ratio/sizing); DRM and
/// unsupported containers mark the box `failed`, which keeps its space and
/// draws nothing, matching a browser that refused to play.
fn probe_media(n: &mut layout::BoxNode, base: &str) -> (usize, usize) {
    let mut probed = 0usize;
    let mut failed = 0usize;
    if no_media() {
        n.media = None;
        return (0, 0);
    }
    let pending = n
        .media
        .as_ref()
        .is_some_and(|m| m.path.is_none() && !m.failed);
    if pending {
        let srcs: Vec<String> = n
            .media
            .as_ref()
            .map(|m| m.srcs.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        // `preload="metadata"` (and no autoplay): a browser only reads enough
        // of the container to size the box, then waits for the first play to
        // fetch the body. GitHub's hero clip is 1.3 MB of pure probe waste.
        let defer = n
            .media
            .as_ref()
            .is_some_and(|m| m.preload_metadata && !m.autoplay);
        let mut last_err = "no media URL".to_string();
        let mut ok = false;
        for src in &srcs {
            let Some(abs) = resolve_img(base, src) else {
                last_err = format!("non-navigable media URL: {src}");
                continue;
            };
            if defer {
                if let Some((meta, path)) = media_prefix_probe(&abs) {
                    let lazy = path.is_none();
                    if let Some(m) = n.media.as_mut() {
                        m.path = path.map(|p| std::rc::Rc::from(p.to_string_lossy().as_ref()));
                        m.src = Some(std::rc::Rc::from(abs.as_str()));
                        m.lazy = lazy;
                        m.w = meta.width;
                        m.h = meta.height;
                        m.duration = meta.duration;
                        if m.w == 0 || m.h == 0 {
                            m.w = 300; // UA replaced size when the header is silent
                            m.h = 150;
                        }
                    }
                    if !lazy {
                        // the whole file is on disk: the first frame replaces
                        // the poster in `decode_images`
                        n.img = None;
                    }
                    ok = true;
                    break;
                }
                // header not in the prefix: fall through to the whole body
            }
            let bytes = match fetch(&abs) {
                Ok((bytes, _)) => bytes,
                Err(e) => {
                    last_err = format!("fetch {src}: {e}");
                    continue;
                }
            };
            match media_file(&abs, &bytes) {
                Ok((path, meta)) => {
                    if let Some(m) = n.media.as_mut() {
                        m.path = Some(std::rc::Rc::from(path.to_string_lossy().as_ref()));
                        m.src = Some(std::rc::Rc::from(abs.as_str()));
                        m.w = meta.width;
                        m.h = meta.height;
                        m.duration = meta.duration;
                        if m.w == 0 || m.h == 0 {
                            m.w = 300; // UA replaced size when the header is silent
                            m.h = 150;
                        }
                    }
                    // the container was accepted, so the poster's job is done:
                    // the first frame replaces it in `decode_images`
                    n.img = None;
                    ok = true;
                    break;
                }
                Err(e) => last_err = format!("{e} ({})", log_url(src)),
            }
        }
        if ok {
            probed += 1;
        } else {
            eprintln!("[media] {last_err}");
            if let Some(m) = n.media.as_mut() {
                m.failed = true;
                m.auto_h = false;
            }
            if n.bx.height.is_none() {
                n.bx.min_height = Some(css::Len::Px(150.0));
            }
            failed += 1;
        }
    }
    for c in &mut n.children {
        let (p, f) = probe_media(c, base);
        probed += p;
        failed += f;
    }
    (probed, failed)
}

/// Parse and validate a media container header: DRM and trackless containers
/// are refused here, before any decoder exists. Shared by the whole-body probe
/// and the `preload="metadata"` header probe.
fn media_meta(bytes: &[u8]) -> Result<mazda_media::mp4::Mp4Meta, String> {
    let meta =
        mazda_media::mp4::parse(bytes).ok_or_else(|| "not an MP4 (ISO-BMFF) file".to_string())?;
    if meta.drm {
        return Err("DRM-protected media is not supported".to_string());
    }
    if !meta.has_video && !meta.has_audio {
        return Err("no audio or video track".to_string());
    }
    Ok(meta)
}

/// Cache one fetched media file. The file name is the URL hash, so a relayout
/// reuses the same bytes without touching the network.
fn media_file(
    src: &str,
    bytes: &[u8],
) -> Result<(std::path::PathBuf, mazda_media::mp4::Mp4Meta), String> {
    let meta = media_meta(bytes)?;
    let dir = cache_dir().join("media");
    std::fs::create_dir_all(&dir).map_err(|e| format!("media cache: {e}"))?;
    let path = dir.join(format!("{}.mp4", hash_str(src)));
    let same = std::fs::metadata(&path).map(|m| m.len() as usize == bytes.len());
    if !same.unwrap_or(false) {
        std::fs::write(&path, bytes).map_err(|e| format!("media write: {e}"))?;
    }
    Ok((path, meta))
}

/// Bytes requested for a `preload="metadata"` header probe. An MP4 that puts
/// `moov` before `mdat` (faststart, what CDNs ship) parses from this prefix;
/// a file that buries the header at the end falls back to the whole-body
/// fetch, exactly what the engine did before.
const MEDIA_PREFIX_BYTES: usize = 64 * 1024;

/// Fetch at most `limit` bytes of `url`. `complete` means the server ignored
/// `Range` and `bytes` is the whole body (which is then cached as usual); a
/// partial body is never written to the HTTP cache as if it were whole.
fn fetch_prefix(url: &str, limit: usize) -> Result<(Vec<u8>, bool), String> {
    let now = now_secs();
    if let Some(hit) = cache_read_in(&cache_dir(), url) {
        if hit.fresh(now) {
            eprintln!("[cache] hit {url}");
            return Ok((hit.body, true));
        }
    }
    let t_net = Instant::now();
    let mut req = http_agent()
        .get(url)
        .set("Range", &format!("bytes=0-{}", limit - 1));
    for (name, value) in net::request_headers(Some("video/mp4,video/*;q=0.9,*/*;q=0.8")) {
        req = req.set(name, &value);
    }
    // `Range` counts bytes of the *encoded* body, so a compressed answer would
    // be an undecodable fragment. Media is already compressed; ask for it as
    // stored.
    req = req.set("Accept-Encoding", "identity");
    if let Some(c) = cookie::request_header(url) {
        req = req.set("Cookie", &c);
    }
    let resp = req.call().map_err(|e| e.to_string())?;
    cookie::store_response(url, &set_cookie_values(&resp));
    let complete = resp.status() == 200;
    let content_type = resp.header("content-type").map(|s| s.to_string());
    let encoding = resp.header("content-encoding").map(|s| s.to_ascii_lowercase());
    let ttl = cache_ttl(resp.header("cache-control"));
    let etag = resp.header("etag").map(|s| s.to_string());
    // a `200` means the Range was ignored: read the whole body like `fetch`
    const CAP: usize = 20 * 1024 * 1024;
    let want = if complete { CAP } else { limit };
    let mut reader = resp.into_reader();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    while buf.len() < want {
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let take = n.min(want - buf.len());
        buf.extend_from_slice(&chunk[..take]);
        if take < n {
            break;
        }
    }
    // a server that ignored `identity` still gets a decodable body; a partial
    // one cannot be decoded and is left to the caller's fallback
    let buf = if complete && encoding.as_deref() == Some("br") {
        decode_br(&buf)?
    } else {
        buf
    };
    if complete {
        cache_write_in(
            &cache_dir(),
            url,
            now_secs(),
            ttl,
            &buf,
            content_type.as_deref(),
            etag.as_deref(),
        );
    }
    if std::env::var_os("MAZDA_TIME").is_some() {
        eprintln!(
            "[net] {:6.1} ms {:8} B {}{}",
            t_net.elapsed().as_secs_f64() * 1000.0,
            buf.len(),
            log_url(url),
            if complete { "" } else { " (prefix)" }
        );
    }
    Ok((buf, complete))
}


/// Header-only media probe: fetch the container prefix and report its
/// metadata. `Some((meta, None))` = header known, body still on the network;
/// `Some((meta, Some(path)))` = the server sent the whole file anyway; `None`
/// = the header is not in the prefix (or the fetch failed), so the caller has
/// to fetch the whole body.
///
/// The prefix itself is kept on disk (`<hash>.head`): a relayout rebuilds the
/// box tree from the DOM, and re-probing from the network would pay a round
/// trip per resize for bytes we already read.
fn media_prefix_probe(
    abs: &str,
) -> Option<(mazda_media::mp4::Mp4Meta, Option<std::path::PathBuf>)> {
    let dir = cache_dir().join("media");
    let head = dir.join(format!("{}.head", hash_str(abs)));
    if let Ok(bytes) = std::fs::read(&head) {
        if let Ok(meta) = media_meta(&bytes) {
            return Some((meta, None));
        }
    }
    let (bytes, complete) = fetch_prefix(abs, MEDIA_PREFIX_BYTES).ok()?;
    if complete {
        let (path, meta) = media_file(abs, &bytes).ok()?;
        return Some((meta, Some(path)));
    }
    let meta = media_meta(&bytes).ok()?;
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&head, &bytes);
    Some((meta, None))
}

/// Worker-side: every probed `<video>` whose first frame decoded becomes a
/// renderer player. The rect is the object-fit content rect layout computed.
fn collect_media_entries(
    n: &layout::BoxNode,
    fixed: bool,
    out: &mut Vec<layout::MediaEntry>,
) {
    let fixed = fixed || n.bx.pos == css::Pos::Fixed;
    if let Some(m) = &n.media {
        let rect = n.img_fit.unwrap_or((n.x, n.y, n.w, n.h));
        if let Some(e) = m.entry(rect, fixed) {
            out.push(e);
        }
    }
    for c in &n.children {
        collect_media_entries(c, fixed, out);
    }
}

/// Post-layout: decode each probed image (disk cache hit, no refetch),
/// resample to its laid-out display size, and emit draw entries. One image's
/// full-size buffer is alive at a time. `budget` caps the display pixels that
/// stay in RAM — over-budget images keep their `Probed` dims (space reserved,
/// nothing drawn).
///
/// `parent_bg` / `parent_masked` describe the ENCLOSING box: its bg color
/// tints a mask placeholder's pixels (RGB replaced, alpha kept — layout's
/// `tint_masks` covers slots that were already decoded, main decodes after
/// layout so the tint happens right here), and a live mask placeholder on
/// that box means `collect_paint` already suppressed its background image —
/// decoding it would draw what paint deliberately dropped (mask wins).
///
/// `next` / `next_bd` count sticky / backdrop boxes met in tree order: the
/// walk visits the same boxes with the same predicates as `collect_paint`'s
/// group recording, so each index lands on this box's [`layout::StickyGroup`]
/// / [`layout::BackdropGroup`]; each group's `imgs_dec` span then covers the
/// images its subtree appended here (they sit past every z-sorted entry,
/// outside the layout-time `imgs` span).
fn decode_slot(
    src: &str,
    abs: Option<&str>,
    dw: u32,
    dh: u32,
    tint: Option<u32>,
    clip: Option<(f32, f32, f32, f32)>,
    fit: (f32, f32, f32, f32),
    fonts: &Fonts,
) -> Option<Cached> {
    let (fx, fy, fw, fh) = fit;
    // inline <svg> markup arrives as its own bytes; everything else is fetched
    // (disk-cached) by absolute URL
    let bytes = if src.starts_with('<') {
        Some(src.as_bytes().to_vec())
    } else {
        abs.and_then(|a| fetch_image(a).ok().map(|(b, _)| b))
    }?;
    // raster: decode then resample; svg: rasterize straight at display size
    // (vector, no full-size buffer ever exists)
    let img = image::decode(&bytes)
        .map(|full| {
            if full.w > dw || full.h > dh {
                image::downscale(&full, dw, dh)
            } else {
                full
            }
        })
        .or_else(|| {
            let mut img = svg::render(&bytes, dw, dh)?;
            // `<text>` is not in the vector rasterizer (it has no font
            // metrics): paint the runs the parser handed us into the same
            // buffer, on top of the shapes
            paint_svg_text(&mut img, &svg::text_runs(&bytes, dw, dh), fonts);
            Some(img)
        })?;
    // a mask draws tinted with its box's background color (RGB replaced,
    // alpha kept) — no bg color, no tint
    let img = match tint {
        Some(c) => layout::tint_image(&img, c),
        None => img,
    };
    // cover/none: keep only the part inside the element's box; the draw rect
    // shrinks to that window (nearest sampling maps the crop across it exactly)
    let (img, draw) = match clip {
        Some((cx0, cy0, cx1, cy1)) if cx1 > cx0 && cy1 > cy0 => {
            // content-space coordinate → crop pixel index
            let map = |p: f32, origin: f32, span: f32, n: u32, up: bool| {
                let f = (p - origin) / span.max(1.0) * n as f32;
                let f = if up { f.ceil() } else { f.floor() };
                f.clamp(0.0, n as f32) as u32
            };
            (
                image::crop(
                    &img,
                    map(cx0, fx, fw, img.w, false),
                    map(cy0, fy, fh, img.h, false),
                    map(cx1, fx, fw, img.w, true),
                    map(cy1, fy, fh, img.h, true),
                ),
                Some((cx0, cy0, cx1 - cx0, cy1 - cy0)),
            )
        }
        Some(_) => (img, None), // wholly outside the box
        None => (img, Some((fx, fy, fw, fh))),
    };
    Some(draw.map(|d| (Rc::new(img), d)))
}

fn paint_svg_text(img: &mut image::ImageData, runs: &[svg::TextRun], fonts: &Fonts) {
    if runs.is_empty() || img.w == 0 || img.h == 0 {
        return;
    }
    let width = img.w as usize;
    for run in runs {
        if run.text.is_empty() || run.size <= 0.0 || run.alpha <= 0.0 {
            continue;
        }
        let mut style = Style::new();
        style.size = run.size as f32;
        style.color = run.color;
        // advances come from the SAME picker the glyph blit uses, so the
        // positions stay in step with the rasterized ink
        let adv: Vec<f32> = run
            .text
            .chars()
            .map(|ch| {
                let font = fonts.pick_font_for(ch, &style);
                font.as_scaled(em_scale(&font, style.size))
                    .h_advance(font.glyph_id(ch))
            })
            .collect();
        let total: f32 = adv.iter().sum();
        match &run.path {
            // plain text: one pen, anchor shifts the origin
            None => {
                let mut x = run.x as f32
                    - match run.anchor {
                        1 => total * 0.5,
                        2 => total,
                        _ => 0.0,
                    };
                for (ch, a) in run.text.chars().zip(adv.iter()) {
                    blit_glyph(
                        &mut img.px,
                        width,
                        fonts,
                        &style,
                        ch,
                        x,
                        run.y as f32,
                        run.alpha as f32,
                    );
                    x += *a;
                }
            }
            // <textPath>: glyphs ride the polyline; `textLength` stretches the
            // advances so the run fills the path (MDN's mandala: slashes
            // spread around a circle). shortcut: the stretch is a uniform
            // scale of every advance instead of extra spacing between glyphs,
            // and glyphs stay upright (the spec's rotate="0" default).
            Some(pts) if pts.len() >= 2 => {
                let mut lens: Vec<f64> = Vec::with_capacity(pts.len());
                let mut acc = 0.0f64;
                for w in pts.windows(2) {
                    acc += ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt();
                    lens.push(acc);
                }
                let plen = acc.max(1e-6);
                let stretch = run
                    .text_len
                    .map(|t| (t / total.max(1e-6) as f64).max(1e-6))
                    .unwrap_or(1.0);
                if run.anchor != 0 && run.text_len.is_none() {
                    // start marker along the path for middle/end anchoring
                    let mut s = match run.anchor {
                        1 => (plen - total as f64) * 0.5,
                        _ => plen - total as f64,
                    }
                    .max(0.0);
                    for (ch, a) in run.text.chars().zip(adv.iter()) {
                        if let Some((p, n)) = path_point(pts, &lens, s) {
                            blit_glyph(
                                &mut img.px,
                                width,
                                fonts,
                                &style,
                                ch,
                                (p.0 + n.0 * run.dy) as f32,
                                (p.1 + n.1 * run.dy) as f32,
                                run.alpha as f32,
                            );
                        }
                        s += *a as f64;
                    }
                    continue;
                }
                let mut s = 0.0f64;
                for (ch, a) in run.text.chars().zip(adv.iter()) {
                    if let Some((p, n)) = path_point(pts, &lens, s) {
                        blit_glyph(
                            &mut img.px,
                            width,
                            fonts,
                            &style,
                            ch,
                            (p.0 + n.0 * run.dy) as f32,
                            (p.1 + n.1 * run.dy) as f32,
                            run.alpha as f32,
                        );
                    }
                    s += *a as f64 * stretch;
                }
            }
            Some(_) => {} // degenerate path: nothing to lay on
        }
    }
}

/// Point at arc length `s` on a polyline plus its unit normal `(-ty, tx)`
/// (the "inward" side for a clockwise path, which is how `dy` on a textPath
/// is applied).
fn path_point(
    pts: &[(f64, f64)],
    lens: &[f64],
    s: f64,
) -> Option<((f64, f64), (f64, f64))> {
    if pts.len() < 2 || lens.is_empty() {
        return None;
    }
    let s = s.clamp(0.0, *lens.last()?);
    let i = lens.partition_point(|&l| l < s).min(lens.len() - 1);
    let (a, b) = (pts[i], pts[i + 1]);
    let seg = lens[i] - if i == 0 { 0.0 } else { lens[i - 1] };
    let t = if seg > 1e-9 { (s - if i == 0 { 0.0 } else { lens[i - 1] }) / seg } else { 0.0 };
    let p = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    Some((p, (-dy / len, dx / len)))
}

/// One glyph into an ARGB buffer (the SVG image's pixels), source-over.
#[allow(clippy::too_many_arguments)]
fn blit_glyph(
    buf: &mut [u32],
    width: usize,
    fonts: &Fonts,
    style: &Style,
    ch: char,
    x: f32,
    baseline: f32,
    alpha: f32,
) {
    let font = fonts.pick_font_for(ch, style);
    let id = font.glyph_id(ch);
    let scale = em_scale(&font, style.size);
    let Some(m) = fonts.glyph_mask(&font, id, scale, x, baseline) else { return };
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    if a == 0 {
        return;
    }
    let gx0 = x.floor() as i32 + m.ox;
    let gy0 = baseline.floor() as i32 + m.oy;
    for gy in 0..m.h {
        let dy = gy0 + gy as i32;
        if dy < 0 {
            continue;
        }
        let row = dy as usize * width;
        if row >= buf.len() {
            continue;
        }
        for gx in 0..m.w {
            let cov = m.cov[gy * m.w + gx] as u32 * a / 255;
            if cov == 0 {
                continue;
            }
            let dx = gx0 + gx as i32;
            if dx < 0 || dx as usize >= width {
                continue;
            }
            let idx = row + dx as usize;
            if idx < buf.len() {
                blend_argb(&mut buf[idx], style.color, cov);
            }
        }
    }
}

/// Source-over onto a PREMULTIPLIED-ALPHA-free ARGB pixel; unlike the page
/// buffer (`blend`) the destination alpha must survive so the SVG image
/// composites correctly.
fn blend_argb(dst: &mut u32, src: u32, a: u32) {
    let inv = 255 - a;
    let (dr, dg, db, da) = ((*dst >> 16) & 0xff, (*dst >> 8) & 0xff, *dst & 0xff, *dst >> 24);
    let r = ((src >> 16) & 0xff) * a / 255 + dr * inv / 255;
    let g = ((src >> 8) & 0xff) * a / 255 + dg * inv / 255;
    let b = (src & 0xff) * a / 255 + db * inv / 255;
    let oa = (a + da * inv / 255).min(255);
    *dst = oa << 24 | r << 16 | g << 8 | b;
}

fn decode_images(
    n: &mut layout::BoxNode,
    base: &str,
    laid: &mut Layout,
    budget: &mut usize,
    fixed: bool,
    parent_bg: Option<u32>,
    parent_masked: bool,
    next: &mut usize,
    next_bd: &mut usize,
    cache: &mut ImgCache,
    fonts: &Fonts,
) -> usize {
    let fixed = fixed || n.bx.pos == css::Pos::Fixed;
    // same predicate as collect_paint's sticky recording (DFS order aligns
    // the indices); img0 opens this subtree's append window before its
    // own push
    let grp = (!n.is_text && n.bx.pos == css::Pos::Sticky && n.bx.inset[0].is_some())
        .then(|| {
            let i = *next;
            *next += 1;
            i
        });
    // same again for the backdrop-filter recording
    let bdg = (!n.is_text && n.bx.backdrop > 0.0).then(|| {
        let i = *next_bd;
        *next_bd += 1;
        i
    });
    let img0 = laid.images.len();
    let mut count = 0usize;
    // the box's own background image loses to a live mask on the same box:
    // skip the whole decode (no pixels fetched, no budget charged)
    let bg_lost = n.is_bg && parent_masked;
    let own = if bg_lost { None } else { n.img.as_ref() };
    if let Some(layout::ImgSlot::Probed { src, .. }) = own {
        let src = src.to_string();
        let inline = src.starts_with('<'); // inline <svg> markup — no URL
        // object-fit: decode at the CONTENT rect (the box itself under the
        // initial fill); a cover/none overflow is cropped to its visible
        // window below. shortcut: the budget is charged at content size, so
        // a hugely oversized cover crop is skipped like any big image.
        let (fx, fy, fw, fh) = n.img_fit.unwrap_or((n.x, n.y, n.w, n.h));
        let dw = fw.round().max(1.0) as u32;
        let dh = fh.round().max(1.0) as u32;
        let px = (dw as usize) * (dh as usize);
        let abs = if inline { None } else { resolve_img(base, &src) };
        let tint = match (n.is_mask, parent_bg) {
            (true, Some(c)) => Some(c),
            _ => None,
        };
        let clip = n.img_clip;
        // exact decode inputs — see ImgKey; a relayout of an unchanged slot
        // hits and reuses the same pixels (shared Rc), a resized one misses
        let key = ImgKey {
            src: hash_str(abs.as_deref().unwrap_or(&src)),
            dw,
            dh,
            tint,
            fit: [fx.to_bits(), fy.to_bits(), fw.to_bits(), fh.to_bits()],
            clip: clip.map(|(a, b, c, d)| [a.to_bits(), b.to_bits(), c.to_bits(), d.to_bits()]),
        };
        if px <= *budget {
            let slot = cache.get(&key).or_else(|| {
                let v = decode_slot(&src, abs.as_deref(), dw, dh, tint, clip, (fx, fy, fw, fh), fonts)?;
                cache.put(key.clone(), v.clone());
                Some(v)
            });
            // Some(None) = decoded but wholly clipped away: nothing to draw
            // and nothing retained, so no budget is charged
            if let Some(Some((img, draw))) = slot {
                *budget -= px;
                laid.images.push((
                    draw.0,
                    draw.1,
                    draw.2,
                    draw.3,
                    img,
                    fixed,
                    layout::eff_alpha(n.eff),
                    n.bx.filter.clone(),
                ));
                count += 1;
            }
        }
    }
    // `<video>`: decode the first frame here so the page ships a real frame
    // (the renderer re-opens the same file and swaps these pixels as
    // playback advances). The poster was already dropped when the container
    // was accepted, so a decode failure leaves the box empty, like a refused
    // video.
    let media_src = n
        .media
        .as_ref()
        .and_then(|m| m.path.clone().map(|p| (p, m.looping, m.muted)));
    let media_lazy = n.media.as_ref().is_some_and(|m| m.lazy);
    if let Some((path, looping, muted)) = media_src {
        let (fx, fy, fw, fh) = n.img_fit.unwrap_or((n.x, n.y, n.w, n.h));
        let dw = fw.round().max(1.0) as u32;
        let dh = fh.round().max(1.0) as u32;
        let px = (dw as usize) * (dh as usize);
        if px <= *budget {
            match mazda_media::Player::open(std::path::Path::new(&*path)) {
                Ok(mut player) => {
                    player.set_looping(looping);
                    player.set_muted(muted);
                    if let Some(f) = player.next_frame() {
                        let frame = image::downscale(
                            &image::ImageData { w: f.w, h: f.h, px: f.px },
                            dw,
                            dh,
                        );
                        *budget -= px;
                        if let Some(m) = n.media.as_mut() {
                            m.img = Some(laid.images.len());
                        }
                        laid.images.push((
                            fx,
                            fy,
                            fw,
                            fh,
                            Rc::new(frame),
                            fixed,
                            layout::eff_alpha(n.eff),
                            n.bx.filter.clone(),
                        ));
                        count += 1;
                    }
                }
                Err(e) => eprintln!("[media] {}: {e}", log_url(&path)),
            }
        }
    } else if media_lazy && n.img.is_none() {
        // `preload="metadata"` with no poster: a browser paints the box black
        // until playback starts. This frame is also the slot the renderer
        // swaps for real frames, so the video stays clickable.
        let (fx, fy, fw, fh) = n.img_fit.unwrap_or((n.x, n.y, n.w, n.h));
        let dw = fw.round().max(1.0) as u32;
        let dh = fh.round().max(1.0) as u32;
        let px = (dw as usize) * (dh as usize);
        if px <= *budget {
            *budget -= px;
            let img = Rc::new(image::ImageData {
                w: dw,
                h: dh,
                px: vec![0xff00_0000; px],
            });
            if let Some(m) = n.media.as_mut() {
                m.img = Some(laid.images.len());
            }
            laid.images.push((
                fx,
                fy,
                fw,
                fh,
                img,
                fixed,
                layout::eff_alpha(n.eff),
                n.bx.filter.clone(),
            ));
            count += 1;
        }
    }
    // context for the children: this box's bg color (mask tint) and whether
    // a live mask lives on it (suppresses its bg-image child) — computed
    // before the mutable walk over the same field
    let masked = n.children.iter().any(|c| c.is_mask && c.img.is_some());
    let bg = n.bx.bg;
    for c in &mut n.children {
        count += decode_images(c, base, laid, budget, fixed, bg, masked, next, next_bd, cache, fonts);
    }
    if let Some(i) = grp {
        if let Some(g) = laid.sticky.get_mut(i) {
            g.imgs_dec = (img0, laid.images.len());
        }
    }
    if let Some(i) = bdg {
        if let Some(g) = laid.backdrops.get_mut(i) {
            g.imgs_dec = (img0, laid.images.len());
        }
    }
    count
}

/// Collect every stylesheet in the subtree, in document order (external
/// sheets and <style> blocks interleave as the markup declares them).
fn collect_sheets(dom: &html::Dom, node: u32, out: &mut Vec<SheetSrc>) {
    match dom.tag(node) {
        "style" => {
            let mut text = String::new();
            for &c in &dom.nodes[node as usize].children {
                if let html::NodeKind::Text(t) = &dom.nodes[c as usize].kind {
                    text.push_str(t);
                    text.push('\n');
                }
            }
            out.push(SheetSrc::Inline(text));
        }
        "link" => {
            let rel = dom.attr(node, "rel").unwrap_or("");
            if rel.split_whitespace().any(|r| r.eq_ignore_ascii_case("stylesheet")) {
                if let Some(href) = dom.attr(node, "href") {
                    out.push(SheetSrc::Link(href.to_string()));
                }
            }
        }
        _ => {}
    }
    for &c in &dom.nodes[node as usize].children {
        collect_sheets(dom, c, out);
    }
}

/// Resolve an IMAGE url against `base`. `data:` URLs pass through untouched:
/// the fetch layer already turns them into bytes (`fetch_accept` ->
/// `data_url_body`), and a normal document-relative resolution would reject
/// the non-http scheme. Everything else resolves like any other URL.
fn resolve_img(base: &str, src: &str) -> Option<String> {
    let s = src.trim();
    if s.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("data:")) {
        return Some(s.to_string());
    }
    resolve(base, s)
}

/// URL for a log line -- a `data:` payload would otherwise dump kilobytes.
fn log_url(u: &str) -> String {
    if u.len() > 80 {
        format!("{}...", u.chars().take(72).collect::<String>())
    } else {
        u.to_string()
    }
}

/// Resolve `href` against `base`; None for non-navigable schemes (javascript:)
/// or same-document links (anchors — we can't scroll-to-fragment yet).
pub(crate) fn resolve(base: &str, href: &str) -> Option<String> {
    let base = url::Url::parse(base).ok()?;
    let joined = base.join(href).ok()?;
    if !matches!(joined.scheme(), "http" | "https") {
        return None;
    }
    let mut base_no_frag = base.clone();
    base_no_frag.set_fragment(None);
    let mut joined_no_frag = joined.clone();
    joined_no_frag.set_fragment(None);
    if joined_no_frag == base_no_frag {
        return None; // same page: would just re-fetch
    }
    Some(joined.to_string())
}

// ---------- data: inline documents (test/verification pages) ----------
// shortcut: text payloads only — base64 and percent-decoded forms; other
// mediatypes pass through as body bytes with their declared type.

/// Decode `data:<mediatype>[;base64],<data>` into (bytes, content-type).
/// Returns None on malformed URLs. No charset parameter modeling —
/// callers decode UTF-8 themselves (matches the html module's sniffing).
fn data_url_body(url: &str) -> Option<(Vec<u8>, Option<String>)> {
    let rest = url.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let header = &rest[..comma];
    let payload = &rest[comma + 1..];
    let (media, b64) = match header.find(';') {
        Some(p) => {
            let (m, rest) = header.split_at(p);
            let is_b64 = rest == ";base64";
            if !is_b64 && !rest.is_empty() {
                // an unmodeled parameter (;charset etc.) — treat as plain text
                (m, false)
            } else {
                (m, is_b64)
            }
        }
        None => (header, false),
    };
    let media = media.split(';').next().unwrap_or("");
    let ct = (!media.is_empty()).then(|| {
        // `data:text/html` shorthand has no subtype → default text/plain
        media.to_string()
    });
    let bytes = if b64 {
        b64_decode(payload)?
    } else {
        percent_decode(payload)
    };
    Some((bytes, ct))
}

/// Percent-decoding (%XX pairs become bytes; anything else echoes).
fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// RFC 4648 base64 (std/-alpha pad tolerant). None on a non-b64 char.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = s.bytes().filter(|&b| b != b'=' && !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in &clean {
        acc = (acc << 6) | val(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

// ---------- HTTP disk cache (Phase 2) ----------
// shortcut: our own TTL instead of HTTP revalidation — no ETag/If-Modified-Since
// or 304 handling; a cached response is served as-is until its max-age expires.
// Eviction = oldest mtime at a file-count cap (not frequency-LRU).

const CACHE_TTL_SECS: u64 = 300;
/// A `max-age` larger than a year is treated as a year: without revalidation a
/// bogus header would otherwise pin a URL forever.
const CACHE_MAX_AGE_CAP: u64 = 365 * 24 * 3600;
const CACHE_MAX_FILES: usize = 300;
const CACHE_MAX_BODY: usize = 5 * 1024 * 1024;

pub(crate) fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("mazda-cache")
}

/// FNV-1a 64 — collision chance at 300 files is ~1e-13; not worth a hash dep.
fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn cache_key(url: &str) -> String {
    format!("{:016x}", hash_str(url))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One disk-cache entry. Format:
/// `<unix-secs> <ttl> <etag>\n<content-type>\n<body>`.
///
/// `-` stands for "no validator". An entry written by an earlier format (no
/// ttl field, or no etag field) still parses; the missing field takes the
/// default TTL / no validator. A stale entry is returned rather than dropped,
/// because the response body is what a 304 has to be answered with.
#[derive(Debug, PartialEq, Eq)]
struct CacheEntry {
    body: Vec<u8>,
    ct: Option<String>,
    /// `ETag` from the response that produced `body`; `If-None-Match` for the
    /// next request. ETag syntax has no spaces or newlines, so the first line
    /// stays whitespace-splittable.
    etag: Option<String>,
    ts: u64,
    ttl: u64,
}

impl CacheEntry {
    /// `ttl == 0` (`no-store`/`no-cache`/`max-age=0`) never serves from disk:
    /// it is a body kept only to answer a revalidation.
    fn fresh(&self, now: u64) -> bool {
        // clock skew is treated as stale: the safe direction
        self.ttl > 0 && now.saturating_sub(self.ts) < self.ttl
    }
}

fn cache_read_in(dir: &std::path::Path, url: &str) -> Option<CacheEntry> {
    let data = std::fs::read(dir.join(cache_key(url))).ok()?;
    let nl1 = data.iter().position(|&b| b == b'\n')?;
    let head = std::str::from_utf8(&data[..nl1]).ok()?;
    let mut fields = head.split_ascii_whitespace();
    let ts: u64 = fields.next()?.parse().ok()?;
    let ttl: u64 = fields
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(CACHE_TTL_SECS);
    let etag = fields
        .next()
        .filter(|s| *s != "-")
        .map(|s| s.to_string());
    let rest = &data[nl1 + 1..];
    let nl2 = rest.iter().position(|&b| b == b'\n')?;
    let ct = std::str::from_utf8(&rest[..nl2]).ok()?.to_string();
    Some(CacheEntry {
        body: rest[nl2 + 1..].to_vec(),
        ct: (!ct.is_empty()).then_some(ct),
        etag,
        ts,
        ttl,
    })
}

/// Cache lifetime from `Cache-Control`: `max-age=N`, capped at a year.
/// `no-store` / `no-cache` answer 0 ("always revalidate"); a response without
/// the header keeps the 5-minute default. shortcut: `Expires` is not parsed —
/// every site this engine loads sends `max-age`.
fn cache_ttl(cc: Option<&str>) -> u64 {
    let Some(cc) = cc else {
        return CACHE_TTL_SECS;
    };
    let cc = cc.to_ascii_lowercase();
    if cc.contains("no-store") || cc.contains("no-cache") {
        return 0;
    }
    if let Some(i) = cc.find("max-age=") {
        let digits: String = cc[i + 8..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse::<u64>() {
            return n.min(CACHE_MAX_AGE_CAP);
        }
    }
    CACHE_TTL_SECS
}

fn cache_write_in(
    dir: &std::path::Path,
    url: &str,
    now: u64,
    ttl: u64,
    bytes: &[u8],
    ct: Option<&str>,
    etag: Option<&str>,
) {
    if bytes.len() > CACHE_MAX_BODY {
        return; // huge bodies aren't worth nav latency or disk
    }
    let _ = std::fs::create_dir_all(dir);
    // keep the file count (thus disk) bounded: evict oldest by mtime
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut files: Vec<_> = rd
            .flatten()
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
            .collect();
        if files.len() >= CACHE_MAX_FILES {
            files.sort_by_key(|(_, t)| *t);
            for (p, _) in files.iter().take(files.len() + 1 - CACHE_MAX_FILES) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    // a validator containing whitespace would break the first line's split
    let tag = match etag {
        Some(e) if !e.contains(char::is_whitespace) => e,
        _ => "-",
    };
    let mut out = format!("{now} {ttl} {tag}\n{}\n", ct.unwrap_or("")).into_bytes();
    out.extend_from_slice(bytes);
    // Atomically: the image prefetch (P65) writes while the script phase reads,
    // and a reader must never see half an entry. The temp name is unique per
    // writer so two threads fetching the same URL cannot interleave.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{}.{seq}.tmp", cache_key(url)));
    if std::fs::write(&tmp, &out).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(cache_key(url)));
    }
    let _ = std::fs::remove_file(&tmp);
}

// ---------- DOM snapshots (Phase 3) ----------
// A tab switch used to re-fetch + re-tokenize the page. The parsed tree is
// now serialized to `mazda-cache\dom\<url hash>` and restored instead;
// stylesheets are re-collected from the restored tree (their bytes are in the
// HTTP disk cache) so media queries still see the live viewport.
// shortcut: keyed by URL only (no validators/expiry beyond the HTTP cache's
// TTL for the sheets), one file per page, oldest-mtime eviction at the cap.

const SNAP_MAX_FILES: usize = 48;
/// Per-file cap: a page whose DOM serializes larger than this isn't worth
/// the disk write; it just loads normally next time.
const SNAP_MAX_BYTES: usize = 16 * 1024 * 1024;

fn snapshot_dir() -> std::path::PathBuf {
    cache_dir().join("dom")
}

/// None on any miss/corruption — callers fall back to a normal fetch.
fn snapshot_read(url: &str) -> Option<html::Dom> {
    // inline documents are local + tiny: re-parsing is cheaper than disk I/O
    if url.starts_with("data:") {
        return None;
    }
    let data = std::fs::read(snapshot_dir().join(cache_key(url))).ok()?;
    if data.len() > SNAP_MAX_BYTES {
        return None;
    }
    html::Dom::from_bytes(&data)
}

fn snapshot_write(url: &str, dom: &html::Dom) {
    if url.starts_with("data:") {
        return; // probes must not evict real pages from the store
    }
    let bytes = dom.to_bytes();
    if bytes.len() > SNAP_MAX_BYTES {
        eprintln!("[dom] snapshot for {url} too large ({} KB), not saved", bytes.len() / 1024);
        return;
    }
    let dir = snapshot_dir();
    let _ = std::fs::create_dir_all(&dir);
    // same oldest-mtime eviction as the HTTP cache
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = rd
            .flatten()
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
            .collect();
        if files.len() >= SNAP_MAX_FILES {
            files.sort_by_key(|(_, t)| *t);
            for (p, _) in files.iter().take(files.len() + 1 - SNAP_MAX_FILES) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    match std::fs::write(dir.join(cache_key(url)), &bytes) {
        Ok(()) => eprintln!("[dom] snapshot saved for {url} ({} KB)", bytes.len() / 1024),
        Err(e) => eprintln!("[dom] snapshot write failed for {url}: {e}"),
    }
}

thread_local! {
    /// Nanoseconds this worker thread spent inside [`fetch`]. The script
    /// runtime subtracts it from its turn timing so `MAZDA_TIME` reports
    /// execution rather than network wait; page loads fetch synchronously, so
    /// a reference site's script turn would otherwise measure its downloads.
    static FETCH_NANOS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn fetch(url: &str) -> Result<(Vec<u8>, Option<String>), String> {
    let t0 = Instant::now();
    let r = fetch_accept(url, None);
    FETCH_NANOS.with(|c| c.set(c.get() + t0.elapsed().as_nanos() as u64));
    r
}

/// Take (and reset) the fetch time accumulated on this thread.
pub(crate) fn fetch_nanos_take() -> u64 {
    FETCH_NANOS.with(|c| c.replace(0))
}

/// Image `Accept` header: formats `image::decode`/`svg::render` handle. WebP
/// leads (servers that honour `Accept` then skip the JPEG/PNG round trip);
/// AVIF is deliberately absent -- advertising a format with no decoder would
/// only buy unrenderable bytes.
const IMAGE_ACCEPT: &str = "image/webp, image/png, image/jpeg;q=0.9";

/// Fetch an image: content-negotiate for the formats we can decode.
/// Image proxies (next/image et al.) serve WebP when the client advertises
/// support -- omitting it gets the original PNG/JPEG back. WebP goes first
/// and AVIF stays out: we decode one and not the other.
fn fetch_image(url: &str) -> Result<(Vec<u8>, Option<String>), String> {
    fetch_accept(url, Some(IMAGE_ACCEPT))
}

/// Network timeouts: ureq's defaults leave reads unbounded ("requests may
/// block forever on reads by default"), which turned one dead server into a
/// permanently stuck tab. The worker thread absorbs the wait either way, but
/// bounded requests keep the queue moving.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .timeout_write(WRITE_TIMEOUT)
            // Wave 28: rustls ordered like Chrome (see net.rs for the
            // wreq-vs-tune-rustls decision and the honest JA3/JA4 limits)
            .tls_config(net::tls_config())
            .build()
    })
}

/// Page loads fetch subresources one at a time, and a round trip to a CDN is
/// ~300 ms from here: a 40-request page spent 12 s waiting on RTTs a browser
/// overlaps. Chrome multiplexes everything over one HTTP/2 connection; we
/// speak HTTP/1.1, so overlap means several connections. Preload the markup's
/// subresources on a small pool right after parse -- the sequential phases
/// that follow (stylesheets, `<script src>`, image probes) then hit the cache.
///
/// The pool is deliberately tiny: browsers use 6 per origin for HTTP/1.1, and
/// a bigger burst only queues in the server's accept backlog.
const PRELOAD_THREADS: usize = 6;

/// HTTP/2: streams, not sockets. Cold github.com issues 69 requests; with
/// enough in flight they share one connection's round trip instead of
/// queueing twelve behind it. The ceiling is memory for the bodies (each
/// fetch is capped by [`HTTP_BODY_CAP`]).
const PRELOAD_THREADS_H2: usize = 24;

/// Fetch `urls` concurrently (deduped, order irrelevant). Errors are logged
/// and swallowed: a failed preload just means the later sequential fetch
/// retries and reports the failure in its own phase.
pub(crate) fn preload(urls: &[String]) {
    preload_accept(urls, None, "preload");
}

/// Same pool, different `[time]` label: the module-graph preloader runs while
/// the script phase is already in progress, so its round trips must be
/// visible apart from the initial subresource preload.
pub(crate) fn preload_labeled(urls: &[String], label: &str) {
    preload_accept(urls, None, label);
}

/// [`preload`] with a content-negotiation `Accept` header. The cache is keyed
/// by URL only, so the preload must ask for the same representation the
/// sequential phase would: images lead with WebP, and a preload that asked for
/// HTML would poison that entry with the original JPEG.
fn preload_accept(urls: &[String], accept: Option<&str>, label: &str) {
    let mut seen = std::collections::HashSet::new();
    let todo: Vec<&str> = urls
        .iter()
        .map(|u| u.as_str())
        .filter(|u| seen.insert(*u))
        .collect();
    if todo.len() < 2 {
        return; // one request: the sequential path is already one round trip
    }
    let t0 = Instant::now();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let lanes = preload_threads().min(todo.len());
    std::thread::scope(|s| {
        for _ in 0..lanes {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(url) = todo.get(i) else { break };
                if let Err(e) = fetch_accept(url, accept) {
                    eprintln!("[preload] {} failed: {e}", log_url(url));
                }
            });
        }
    });
    if std::env::var_os("MAZDA_TIME").is_some() {
        eprintln!(
            "[time] {}: {:.0} ms ({} requests, {} lanes)",
            label,
            t0.elapsed().as_secs_f64() * 1000.0,
            todo.len(),
            lanes
        );
    }
}

/// `Content-Encoding` limit *after* decompression. The compressed read is
/// capped at 20 MB, but a compressed body can expand far past that; no page
/// needs a single response bigger than this.
const DECODE_CAP: usize = 64 * 1024 * 1024;

/// Largest response body either transport will keep.
const HTTP_BODY_CAP: usize = 20 * 1024 * 1024;

/// Sink that refuses to grow past [`DECODE_CAP`], so a hostile body cannot
/// expand without bound.
struct Capped(Vec<u8>);

impl std::io::Write for Capped {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + b.len() > DECODE_CAP {
            return Err(std::io::Error::other("decoded body over cap"));
        }
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Decode a brotli body. ureq gunzips for us; brotli is decoded here because
/// ureq's own `brotli` feature pins `brotli-decompressor` 4.0.0, which this
/// project's offline registry does not carry (see Cargo.toml).
fn decode_br(buf: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Capped(Vec::with_capacity(buf.len().saturating_mul(3)));
    brotli_decompressor::BrotliDecompress(&mut std::io::Cursor::new(buf), &mut out)
        .map_err(|e| format!("brotli decode: {e}"))?;
    Ok(out.0)
}

/// Decode a gzip body. Only the HTTP/2 path needs this: ureq strips the coding
/// and hands back decoded bytes already.
fn decode_gzip(buf: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Capped(Vec::with_capacity(buf.len().saturating_mul(3)));
    std::io::copy(&mut flate2::read::GzDecoder::new(buf), &mut out)
        .map_err(|e| format!("gzip decode: {e}"))?;
    Ok(out.0)
}

fn fetch_accept(
    url: &str,
    accept: Option<&str>,
) -> Result<(Vec<u8>, Option<String>), String> {
    // data: URLs never touch the network or the disk cache
    if url.starts_with("data:") {
        return data_url_body(url).ok_or_else(|| format!("malformed data: url {url:?}"));
    }
    coalesce_fetch(url, || fetch_accept_raw(url, accept))
}

/// One answer per URL per page load: the preload bursts and the sequential
/// phases that follow ask for the same URLs, and a URL already downloading on
/// another thread is joined, not fetched a second time. The earlier a preload
/// starts, the more of its work the sequential phase would otherwise repeat.
fn coalesce_fetch(
    url: &str,
    work: impl FnOnce() -> Result<(Vec<u8>, Option<String>), String>,
) -> Result<(Vec<u8>, Option<String>), String> {
    type Slot = std::sync::Arc<(
        std::sync::Mutex<Option<Result<(Vec<u8>, Option<String>), String>>>,
        std::sync::Condvar,
    )>;
    fn slots() -> &'static std::sync::Mutex<std::collections::HashMap<String, Slot>> {
        static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Slot>>> =
            std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }
    fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }
    let follower = {
        let mut map = lock(slots());
        match map.get(url) {
            Some(s) => Some(s.clone()),
            None => {
                map.insert(
                    url.to_string(),
                    std::sync::Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new())),
                );
                None
            }
        }
    };
    let Some(slot) = follower else {
        // this thread owns the URL: run the request, publish it, then drop the
        // slot. A panic is published as an error so a waiter cannot hang on a
        // leader that died.
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
            .unwrap_or_else(|_| Err(format!("fetch panicked: {url}")));
        if let Some(slot) = lock(slots()).remove(url) {
            let (cell, cv) = &*slot;
            *lock(cell) = Some(out.clone());
            cv.notify_all();
        }
        return out;
    };
    let (cell, cv) = &*slot;
    let mut g = lock(cell);
    while g.is_none() {
        g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
    }
    g.as_ref().expect("slot filled").clone()
}

/// [`fetch_accept`] without the in-flight table: disk cache, network, cache.
fn fetch_accept_raw(
    url: &str,
    accept: Option<&str>,
) -> Result<(Vec<u8>, Option<String>), String> {
    let dir = cache_dir();
    let now = now_secs();
    // A stale entry is kept, not discarded: its body answers a 304 and its
    // ETag is what turns a re-download into one round trip.
    let mut stored = None;
    if let Some(hit) = cache_read_in(&dir, url) {
        if hit.fresh(now) {
            eprintln!("[cache] hit {url}");
            return Ok((hit.body, hit.ct));
        }
        stored = Some(hit);
    }
    let mut headers = net::request_headers(accept);
    if let Some(c) = cookie::request_header(url) {
        headers.push(("Cookie", c));
    }
    if let Some(tag) = stored.as_ref().and_then(|c| c.etag.as_deref()) {
        headers.push(("If-None-Match", tag.to_string()));
    }
    let t_net = Instant::now();
    // HTTP/2 first when asked for; any failure falls through to ureq below, so
    // a server that only speaks HTTP/1.1 still loads.
    if http2_on() {
        match crate::h2net::get(url, &headers, HTTP_BODY_CAP) {
            Ok(r) => {
                // the response belongs to the last hop, not to the URL that was
                // asked for: cookies are host-scoped, and the page resolves its
                // subresources against it
                cookie::store_response(&r.final_url, &set_cookie_values_of(&r.headers));
                return finish_fetch(
                    &dir,
                    url,
                    now,
                    r.status,
                    r.header("content-type").map(|s| s.to_string()),
                    r.header("content-encoding").map(|s| s.to_ascii_lowercase()),
                    r.header("cache-control").map(|cc| cache_ttl(Some(cc))),
                    r.header("etag").map(|s| s.to_string()),
                    r.body,
                    stored,
                    t_net,
                );
            }
            Err(e) => eprintln!("[h2] {} failed: {e}", log_url(url)),
        }
    }
    let mut req = http_agent().get(url);
    for (name, value) in &headers {
        req = req.set(name, value);
    }
    let resp = req.call().map_err(|e| e.to_string())?;
    // ureq already followed the chain: this is where the body came from
    let final_url = resp.get_url().to_string();
    cookie::store_response(&final_url, &set_cookie_values(&resp));
    let status = resp.status();
    let ct = resp.header("content-type").map(|s| s.to_string());
    let encoding = resp.header("content-encoding").map(|s| s.to_ascii_lowercase());
    let ttl = resp.header("cache-control").map(|cc| cache_ttl(Some(cc)));
    let etag = resp.header("etag").map(|s| s.to_string());
    // A 304 has no body, but servers still send the selected representation's
    // `Content-Encoding`, so feeding `into_reader` an empty gzip stream fails
    // with "unexpected end of file". Nothing to read: answer from the cache.
    let mut buf = Vec::new();
    if status != 304 {
        // into_reader transparently gunzips (ureq gzip feature); cap at 20MB
        let mut reader = resp.into_reader();
        let mut chunk = [0u8; 8192];
        while buf.len() < HTTP_BODY_CAP {
            let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }
    finish_fetch(
        &dir, url, now, status, ct, encoding, ttl, etag, buf, stored, t_net,
    )
}

/// Shared tail of both transports: answer a 304 from the stored body, decode
/// the content coding, write the cache entry, log the request.
#[allow(clippy::too_many_arguments)]
fn finish_fetch(
    dir: &std::path::Path,
    url: &str,
    now: u64,
    status: u16,
    ct: Option<String>,
    encoding: Option<String>,
    ttl: Option<u64>,
    etag: Option<String>,
    buf: Vec<u8>,
    stored: Option<CacheEntry>,
    t_net: Instant,
) -> Result<(Vec<u8>, Option<String>), String> {
    // Not Modified: the stored body is still current. Refresh its timestamp
    // (and its lifetime, if the 304 carries one) so the rest of this page load
    // does not ask again.
    if status == 304 {
        let c = stored.ok_or_else(|| format!("304 without a cached body for {url}"))?;
        let ttl = ttl.unwrap_or(c.ttl);
        cache_write_in(dir, url, now, ttl, &c.body, c.ct.as_deref(), c.etag.as_deref());
        if std::env::var_os("MAZDA_TIME").is_some() {
            eprintln!(
                "[net] {:6.1} ms {:8} B {} (304)",
                t_net.elapsed().as_secs_f64() * 1000.0,
                0,
                log_url(url)
            );
        }
        return Ok((c.body, c.ct));
    }
    // ureq gunzips for us; the HTTP/2 path receives the bytes as sent, so both
    // codings are decoded here whenever one is present.
    let buf = match encoding.as_deref() {
        Some("br") => decode_br(&buf)?,
        Some("gzip") => decode_gzip(&buf)?,
        _ => buf,
    };
    // `Cache-Control` decides how long the disk entry may be reused: GitHub's
    // hashed bundles are `max-age=31536000, immutable`, its HTML is
    // `max-age=0, private, must-revalidate`. Ignoring this used to re-download
    // the whole page after 5 minutes.
    cache_write_in(
        dir,
        url,
        now_secs(),
        ttl.unwrap_or(CACHE_TTL_SECS),
        &buf,
        ct.as_deref(),
        etag.as_deref(),
    );
    if std::env::var_os("MAZDA_TIME").is_some() {
        eprintln!(
            "[net] {:6.1} ms {:8} B {}{}",
            t_net.elapsed().as_secs_f64() * 1000.0,
            buf.len(),
            log_url(url),
            // the byte count is the decoded size; this says how it arrived
            match encoding.as_deref() {
                Some("br") => " (br)",
                Some("gzip") => " (gzip)",
                _ => "",
            }
        );
    }
    Ok((buf, ct))
}

/// Progressive first paint (markup + stylesheets laid out before the scripts
/// run) is on by default: it is what makes a slow load show content instead of
/// a blank window. It costs one extra cascade; `MAZDA_FIRSTPAINT=0` turns it
/// off for A/B measurements.
fn first_paint_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("MAZDA_FIRSTPAINT").ok().as_deref(), Some("0") | Some("off")))
}

/// HTTP/2 is the default: one connection per origin, every page request a
/// stream on it. Cold github.com is 69 requests; over HTTP/1.1 with six lanes
/// that is ~12 sequential round trips (P71 measured ~1 s per request on this
/// link, ~15 s cold). `MAZDA_HTTP2=0` (or `MAZDA_HTTP1=1`) keeps the old
/// HTTP/1.1 path for A/B measurements.
fn http2_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if std::env::var_os("MAZDA_HTTP1").is_some() {
            return false;
        }
        !matches!(
            std::env::var("MAZDA_HTTP2").ok().as_deref(),
            Some("0") | Some("false") | Some("off")
        )
    })
}

/// How many requests to have in flight at once. HTTP/2 multiplexes them as
/// streams on one connection, so the limit is memory, not sockets; HTTP/1.1
/// keeps the traditional six per host.
fn preload_threads() -> usize {
    if http2_on() {
        PRELOAD_THREADS_H2
    } else {
        PRELOAD_THREADS
    }
}

fn set_cookie_values_of(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.clone())
        .collect()
}

/// Decoded webfont bytes kept in RAM per page (per `Fonts` instance: the
/// loader thread and the UI painter each own one, so a page is bounded at
/// 2 × 32 MB across both). Site fonts often ship 3-8 faces at 1-4 MB each;
/// 32 MB keeps normal pages fully covered while a hostile page (dozens of
/// huge faces) cannot grow without bound.
/// Rule: on every successful decode the oldest-USED face is evicted (LRU by
/// `last_use` stamp) until the new font fits; a single font larger than the
/// cap is skipped entirely (system fallback) rather than breaking the cap.
/// shortcut: this is a per-page decoded-font byte budget, NOT the spec's
/// font-set loading semantics (unicode-range/font-display untouched).
const WEBFONT_BUDGET: usize = 32 << 20;

/// One stderr note per process when site webfonts are off (option B, or a
/// build without the decoder). `begin_page` runs on every relayout, so a
/// plain eprintln would spam the log on hover/typing.
fn note_webfont_skip(system_only: bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static NOTED: AtomicBool = AtomicBool::new(false);
    if !NOTED.swap(true, Ordering::Relaxed) {
        if system_only {
            eprintln!("[font] webfonts disabled for this page (system fonts only)");
        } else {
            eprintln!("[font] webfonts unavailable (built without the woff2 feature)");
        }
    }
}


/// Font variants load on first use. ab_glyph parses lazily (~1MB raw bytes per
/// font, glyph outlines parsed on demand) — the whole set costs less than one
/// eagerly-parsed fontdue font did.
struct Fonts {
    regular: FontVec,
    bold: std::sync::OnceLock<FontVec>,
    italic: std::sync::OnceLock<FontVec>,
    mono: std::sync::OnceLock<FontVec>,
    /// serif family (Georgia etc.); lazy — only when CSS asks for it
    serif: std::sync::OnceLock<FontVec>,
    /// CJK (Chinese + kana) fallback; only loaded when a glyph is missing
    cjk: std::sync::OnceLock<Option<FontVec>>,
    /// Hangul fallback, same laziness
    hangul: std::sync::OnceLock<Option<FontVec>>,
    /// interned font-family stacks (shared with the cascade via Sheet)
    stacks: css::StackTable,
    /// `@font-face` faces declared by the CURRENT page (cleared on navigation)
    faces: Vec<PageFace>,
    /// one lazy slot per SYSTEM_FAMILIES entry
    sys: Vec<std::sync::OnceLock<Option<FontVec>>>,
    /// webfont policy the CURRENT page was loaded with (`A` site fonts,
    /// `B` system only). Set from the navigation job, never mid-page.
    webfonts: FontsChoice,
    /// per-page cap (const in production; tests lower it to exercise LRU)
    webfont_cap: usize,
    /// decoded webfont bytes currently retained for this page
    webfont_bytes: std::cell::Cell<usize>,
    /// monotonically increasing access stamp (see [`PageFace::last_use`])
    webfont_clock: std::cell::Cell<u64>,
    /// Rasterized glyph coverage for the CURRENT page, keyed by (font
    /// identity, glyph, size, 1/4-px pen fraction). Scroll repaints used to
    /// re-outline every visible glyph each frame; this makes them a blit.
    glyphs: std::cell::RefCell<HashMap<GlyphKey, Rc<GlyphMask>>>,
}

/// (font address, glyph id, size bits, pen x fraction, pen y fraction) —
/// each in 1/4 px steps, which is all ab_glyph's rasterizer can tell apart.
type GlyphKey = (usize, u16, u32, u8, u8);

/// One rasterized glyph at a fixed subpixel offset: coverage bytes plus the
/// integer pixel origin relative to the pen's floor.
struct GlyphMask {
    w: usize,
    h: usize,
    ox: i32,
    oy: i32,
    cov: Vec<u8>,
}

/// One page's `@font-face` definition with its lazily-fetched font.
/// `Rc` (not a plain Vec) so the byte-budget LRU can drop a cached face
/// while a glyph already picked for painting stays alive for that frame.
struct PageFace {
    family: String, // lowercased, as parsed
    wmin: u16,
    wmax: u16,
    italic: bool,
    srcs: Vec<String>, // absolute URLs in declared order
    font: std::cell::RefCell<Option<Rc<FontVec>>>,
    /// decoded size this face contributed to the page budget (0 = uncached)
    bytes: std::cell::Cell<usize>,
    /// last-picked stamp for LRU eviction (higher = more recent)
    last_use: std::cell::Cell<u64>,
    /// negative cache: every `src` failed, or the face exceeds the whole
    /// budget. Without it a broken/giant face would refetch on EVERY glyph
    /// (the old `OnceLock` cached failures; the LRU slot must too).
    failed: std::cell::Cell<bool>,
}

/// A font chosen for one character: system faces are borrowed from the
/// picker's long-lived slots, webfonts are shared so eviction cannot free a
/// font that the current paint pass is still using.
enum Picked<'a> {
    Borrowed(&'a FontVec),
    Shared(Rc<FontVec>),
}

impl std::ops::Deref for Picked<'_> {
    type Target = FontVec;
    fn deref(&self) -> &FontVec {
        match self {
            Picked::Borrowed(f) => f,
            Picked::Shared(f) => f,
        }
    }
}

#[cfg(test)]
impl Picked<'_> {
    fn ptr(&self) -> *const FontVec {
        &**self as *const FontVec
    }
}

/// Installed families sites commonly ask for by name → font files. One
/// regular-weight file per family: bold/italic requests fall back to the
/// generic pick's real bold/italic faces (shortcut: no per-family variants).
/// Not exhaustive on purpose — lazily loaded only when a page actually asks.
#[cfg(windows)]
const SYSTEM_FAMILIES: &[(&str, &[&str])] = &[
    ("verdana", &[r"C:\Windows\Fonts\verdana.ttf"]),
    ("tahoma", &[r"C:\Windows\Fonts\tahoma.ttf"]),
    ("trebuchet ms", &[r"C:\Windows\Fonts\trebuc.ttf"]),
    ("times new roman", &[r"C:\Windows\Fonts\times.ttf"]),
    ("georgia", &[r"C:\Windows\Fonts\georgia.ttf"]),
    ("courier new", &[r"C:\Windows\Fonts\cour.ttf"]),
    ("helvetica", &[r"C:\Windows\Fonts\arial.ttf"]),
    ("arial", &[r"C:\Windows\Fonts\arial.ttf"]),
    ("calibri", &[r"C:\Windows\Fonts\calibri.ttf"]),
    ("cambria", &[r"C:\Windows\Fonts\cambria.ttc"]),
    ("segoe ui", &[r"C:\Windows\Fonts\segoeui.ttf"]),
    ("impact", &[r"C:\Windows\Fonts\impact.ttf"]),
    ("comic sans ms", &[r"C:\Windows\Fonts\comic.ttf"]),
    ("arial black", &[r"C:\Windows\Fonts\ariblk.ttf"]),
];

/// shortcut: guessed Linux font paths (Debian/Ubuntu/Fedora locations) —
/// enough to build and usually land, never verified on a real Linux box.
#[cfg(not(windows))]
const SYSTEM_FAMILIES: &[(&str, &[&str])] = &[
    (
        "arial",
        &[
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ],
    ),
    (
        "helvetica",
        &[
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ],
    ),
    (
        "verdana",
        &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"],
    ),
    (
        "times new roman",
        &[
            "/usr/share/fonts/truetype/liberation/LiberationSerif-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf",
        ],
    ),
    (
        "georgia",
        &["/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf"],
    ),
    (
        "courier new",
        &[
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        ],
    ),
    (
        "segoe ui",
        &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"],
    ),
];

/// Generic keywords in a stack — the walk skips them; the derived `Fam`
/// already picks the right system default for them.
fn is_generic_family(n: &str) -> bool {
    matches!(
        n,
        "serif"
            | "sans-serif"
            | "monospace"
            | "cursive"
            | "fantasy"
            | "system-ui"
            | "ui-serif"
            | "ui-sans-serif"
            | "ui-monospace"
            | "ui-rounded"
            | "math"
            | "emoji"
    )
}

/// Smallest full-coverage options first — raw bytes stay resident once loaded.
#[cfg(windows)]
const CJK_FONTS: &[&str] = &[
    r"C:\Windows\Fonts\msyhl.ttc", // YaHei Light 11.6MB
    r"C:\Windows\Fonts\msyh.ttc",   // YaHei 18.8MB
    r"C:\Windows\Fonts\simsun.ttc",
    r"C:\Windows\Fonts\msjh.ttc",
    r"C:\Windows\Fonts\YuGothR.ttc",
    r"C:\Windows\Fonts\simhei.ttf",
];
#[cfg(not(windows))]
const CJK_FONTS: &[&str] = &[
    // shortcut: guessed Linux paths, unverified (see SYSTEM_FAMILIES above)
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
];
#[cfg(windows)]
const HANGUL_FONTS: &[&str] = &[
    r"C:\Windows\Fonts\malgunsl.ttf", // Malgun Light 4.8MB
    r"C:\Windows\Fonts\malgun.ttf",
];
#[cfg(not(windows))]
const HANGUL_FONTS: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
];

/// Primary face candidates per weight/style (used by [`Fonts::load`] and
/// [`Fonts::pick`]).
#[cfg(windows)]
const FONT_REGULAR: &[&str] = &[r"C:\Windows\Fonts\segoeui.ttf", r"C:\Windows\Fonts\arial.ttf"];
#[cfg(not(windows))]
const FONT_REGULAR: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
];
#[cfg(windows)]
const FONT_BOLD: &[&str] = &[r"C:\Windows\Fonts\segoeuib.ttf", r"C:\Windows\Fonts\arialbd.ttf"];
#[cfg(not(windows))]
const FONT_BOLD: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
];
#[cfg(windows)]
const FONT_ITALIC: &[&str] = &[r"C:\Windows\Fonts\segoeuii.ttf", r"C:\Windows\Fonts\ariali.ttf"];
#[cfg(not(windows))]
const FONT_ITALIC: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Oblique.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Italic.ttf",
];
#[cfg(windows)]
const FONT_MONO: &[&str] = &[r"C:\Windows\Fonts\consola.ttf", r"C:\Windows\Fonts\cour.ttf"];
#[cfg(not(windows))]
const FONT_MONO: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
];
#[cfg(windows)]
const FONT_SERIF: &[&str] = &[r"C:\Windows\Fonts\georgia.ttf", r"C:\Windows\Fonts\times.ttf"];
#[cfg(not(windows))]
const FONT_SERIF: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSerif-Regular.ttf",
];

impl Fonts {
    fn load() -> Self {
        Fonts {
            regular: load_font(FONT_REGULAR),
            bold: std::sync::OnceLock::new(),
            italic: std::sync::OnceLock::new(),
            mono: std::sync::OnceLock::new(),
            serif: std::sync::OnceLock::new(),
            cjk: std::sync::OnceLock::new(),
            hangul: std::sync::OnceLock::new(),
            stacks: std::rc::Rc::new(std::cell::RefCell::new(vec![Vec::new()])),
            faces: Vec::new(),
            sys: (0..SYSTEM_FAMILIES.len())
                .map(|_| std::sync::OnceLock::new())
                .collect(),
            webfonts: FontsChoice::A,
            webfont_cap: WEBFONT_BUDGET,
            webfont_bytes: std::cell::Cell::new(0),
            webfont_clock: std::cell::Cell::new(0),
            glyphs: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Adopt the policy of the page being loaded (set from the navigation
    /// job, so the settings change applies on NEXT navigation).
    fn set_choice(&mut self, choice: FontsChoice) {
        self.webfonts = choice;
    }

    /// Page-scoped font state: drop the previous page's webfonts and stack
    /// ids (RAM freed on navigation), register the new page's `@font-face`
    /// defs — each font itself still loads lazily on first use.
    fn begin_page(&mut self, faces: Vec<css::FontFace>) {
        self.faces.clear();
        self.webfont_bytes.set(0);
        self.webfont_clock.set(0);
        // glyph masks are keyed by font address + size: a new page may reuse
        // an address, so never carry them across a navigation
        self.glyphs.borrow_mut().clear();
        // reset the shared stack table BEFORE any early return: the cascade
        // (running later in `layout_page`) pushes this page's stacks into it,
        // so stale names/id slots must never survive a navigation
        {
            let mut t = self.stacks.borrow_mut();
            t.clear();
            t.push(Vec::new()); // id 0 = "no stack"
        }
        if self.webfonts == FontsChoice::B || !cfg!(feature = "woff2") {
            // system-fonts-only mode (or a dev build with no WOFF2 decoder):
            // keep NO face definitions, so the lazy loader is never
            // reachable — no webfont request is made and nothing is
            // downloaded, decoded or cached (the UI RAM/network claim).
            // release builds are made WITH the `woff2` feature.
            note_webfont_skip(self.webfonts == FontsChoice::B);
            return;
        }
        self.faces = faces
            .into_iter()
            .map(|f| PageFace {
                family: f.family,
                wmin: f.wmin,
                wmax: f.wmax,
                italic: f.italic,
                srcs: f.srcs,
                font: std::cell::RefCell::new(None),
                bytes: std::cell::Cell::new(0),
                last_use: std::cell::Cell::new(0),
                failed: std::cell::Cell::new(false),
            })
            .collect();
        if !self.faces.is_empty() {
            eprintln!("[font] {} @font-face defs for this page", self.faces.len());
        }
    }

    fn pick(&self, s: &Style) -> &FontVec {
        if s.fam == Fam::Mono {
            self.mono.get_or_init(|| load_font(FONT_MONO))
        } else if s.fam == Fam::Serif {
            // Georgia ships regular/bold/italic/bold-italic on every Windows
            self.serif.get_or_init(|| load_font(FONT_SERIF))
        } else if s.bold && s.italic {
            &self.regular // no segoui-bi fallback; shortcut: faux-skip
        } else if s.bold {
            self.bold.get_or_init(|| load_font(FONT_BOLD))
        } else if s.italic {
            self.italic.get_or_init(|| load_font(FONT_ITALIC))
        } else {
            &self.regular
        }
    }

    /// Cached raster of one glyph at a pen position. The pen is snapped to a
    /// 1/4-px grid so a whole page's text reuses a handful of masks per glyph
    /// (ab_glyph only resolves positions that differ in that grid anyway).
    /// shortcut: flat cap, clear-all at 8192 masks (per-page text is smaller).
    fn glyph_mask(
        &self,
        font: &FontVec,
        id: ab_glyph::GlyphId,
        scale: PxScale,
        x: f32,
        baseline: f32,
    ) -> Option<Rc<GlyphMask>> {
        let frac = |v: f32| -> u8 {
            let f = ((v - v.floor()) * 4.0).round() as u8;
            if f == 4 {
                0
            } else {
                f
            }
        };
        let (fx, fy) = (frac(x), frac(baseline));
        let key = (font as *const FontVec as usize, id.0, scale.x.to_bits(), fx, fy);
        if let Some(m) = self.glyphs.borrow().get(&key) {
            return Some(m.clone());
        }
        let position = ab_glyph::point(fx as f32 * 0.25, fy as f32 * 0.25);
        let glyph = ab_glyph::Glyph { id, scale, position };
        let outlined = font.outline_glyph(glyph)?;
        let b = outlined.px_bounds();
        let (w, h) = (b.width() as usize, b.height() as usize);
        if w == 0 || h == 0 {
            return None;
        }
        let mut cov = vec![0u8; w * h];
        outlined.draw(|gx, gy, c| {
            cov[gy as usize * w + gx as usize] = (c * 255.0).clamp(0.0, 255.0) as u8;
        });
        let m = Rc::new(GlyphMask {
            w,
            h,
            ox: b.min.x as i32,
            oy: b.min.y as i32,
            cov,
        });
        let mut cache = self.glyphs.borrow_mut();
        if cache.len() >= 8192 {
            cache.clear();
        }
        cache.insert(key, m.clone());
        Some(m)
    }

    /// Font that actually has a glyph for `ch`: the element's `font-family`
    /// stack in CSS order (webfonts + installed aliases), else the primary
    /// style font, else the lazily-loaded CJK/Hangul fallback, else primary
    /// (renders .notdef).
    /// Range-gated so Thai/Arabic/etc. (no fallback font for them) never pay
    /// the font-load cost just to learn there's no glyph.
    /// shortcut: fallback is always the *regular* weight — no bold/italic CJK.
    fn pick_font_for(&self, ch: char, style: &Style) -> Picked<'_> {
        if style.fam_stack != 0 {
            let table = self.stacks.borrow();
            if let Some(names) = table.get(style.fam_stack as usize) {
                for name in names {
                    if is_generic_family(name) {
                        continue; // the Fam-derived generic pick covers these
                    }
                    if let Some(f) = self.stack_font(name, style, ch) {
                        return f;
                    }
                }
            }
        }
        let primary = self.pick(style);
        if primary.glyph_id(ch).0 != 0 {
            return Picked::Borrowed(primary);
        }
        if is_cjk(ch) {
            if let Some(f) = self.cjk.get_or_init(|| try_load_font(CJK_FONTS)) {
                if f.glyph_id(ch).0 != 0 {
                    return Picked::Borrowed(f);
                }
            }
        }
        if is_hangul(ch) {
            if let Some(f) = self.hangul.get_or_init(|| try_load_font(HANGUL_FONTS)) {
                if f.glyph_id(ch).0 != 0 {
                    return Picked::Borrowed(f);
                }
            }
        }
        Picked::Borrowed(primary)
    }

    /// One family name from a stack: the page's `@font-face` webfont for it
    /// (fetched+decoded once, charged against the per-page LRU budget), else
    /// a known installed alias. None → the caller continues to the stack.
    fn stack_font(&self, name: &str, style: &Style, ch: char) -> Option<Picked<'_>> {
        // best face for this weight/italic, else the family's regular face
        let want: u16 = if style.bold { 700 } else { 400 };
        let idx = self
            .faces
            .iter()
            .position(|f| {
                f.family == name && f.italic == style.italic && f.wmin <= want && want <= f.wmax
            })
            .or_else(|| self.faces.iter().position(|f| f.family == name && !f.italic));
        if let Some(i) = idx {
            // `B` never gets here: begin_page leaves `faces` empty, so no
            // webfont URL is even fetched (the UI's RAM promise)
            let empty = self.faces[i].font.borrow().is_none() && !self.faces[i].failed.get();
            if empty {
                match load_webfont(&self.faces[i].srcs) {
                    Some(font) => self.cache_webfont(i, font),
                    // negative cache: a dead src list is not retried per glyph
                    None => self.faces[i].failed.set(true),
                }
            }
            let cached = self.faces[i].font.borrow().clone();
            if let Some(f) = cached {
                if f.glyph_id(ch).0 != 0 {
                    self.touch_webfont(i);
                    return Some(Picked::Shared(f));
                }
            }
        }
        let idx = SYSTEM_FAMILIES.iter().position(|(n, _)| *n == name)?;
        if let Some(f) = self.sys[idx].get_or_init(|| try_load_font(SYSTEM_FAMILIES[idx].1)) {
            if f.glyph_id(ch).0 != 0 {
                return Some(Picked::Borrowed(f));
            }
        }
        None
    }

    /// Cache one decoded webfont against the page budget: LRU-evict the
    /// least-recently-picked cached faces until it fits (see WEBFONT_BUDGET).
    /// A font bigger than the whole cap is skipped (system fallback).
    fn cache_webfont(&self, face: usize, font: FontVec) {
        let size = font.as_slice().len();
        if size > self.webfont_cap {
            eprintln!(
                "[font] webfont {} KB exceeds the {}-KB page budget — system fallback",
                size / 1024,
                self.webfont_cap / 1024
            );
            // remember the verdict: re-decoding a too-large face per glyph
            // would burn CPU forever
            self.faces[face].failed.set(true);
            return;
        }
        let mut evicted = false;
        while self.webfont_bytes.get() + size > self.webfont_cap {
            let victim = self
                .faces
                .iter()
                .enumerate()
                .filter(|(_, f)| f.font.borrow().is_some())
                .min_by_key(|(_, f)| f.last_use.get())
                .map(|(i, _)| i);
            let Some(v) = victim else { break };
            *self.faces[v].font.borrow_mut() = None;
            let freed = self.faces[v].bytes.replace(0);
            self.webfont_bytes
                .set(self.webfont_bytes.get().saturating_sub(freed));
            eprintln!("[font] page budget: evicted {freed} KB webfont");
            evicted = true;
        }
        if evicted {
            // glyph masks key on the font's address, which the freed Rc just
            // made reusable — drop them before a new face can land there
            self.glyphs.borrow_mut().clear();
        }
        let clock = self.webfont_clock.get() + 1;
        self.webfont_clock.set(clock);
        *self.faces[face].font.borrow_mut() = Some(Rc::new(font));
        self.faces[face].bytes.set(size);
        self.faces[face].last_use.set(clock);
        self.webfont_bytes.set(self.webfont_bytes.get() + size);
    }

    /// Mark a cached face as most-recently used (O(1); eviction scans stamps).
    fn touch_webfont(&self, face: usize) {
        let clock = self.webfont_clock.get() + 1;
        self.webfont_clock.set(clock);
        self.faces[face].last_use.set(clock);
    }

    fn measure(&self, text: &str, style: &Style) -> f32 {
        text.chars()
            .map(|c| {
                let f = self.pick_font_for(c, style);
                let scale = em_scale(&f, style.size);
                f.as_scaled(scale).h_advance(f.glyph_id(c))
            })
            .sum()
    }
}

/// ab_glyph's `PxScale` is "make the font's HEIGHT (ascent − descent) this
/// many pixels", but CSS `font-size` is the EM size, so handing `style.size`
/// straight in laid text out at `em / height` of its real width — Fira Sans
/// (height 1200 units, em 1000) came out 5/6 wide, Alfa Slab One 0.73. Both
/// advances and glyph rasters go through this conversion so they stay in step.
fn em_scale(font: &FontVec, size: f32) -> PxScale {
    let upem = font.units_per_em().unwrap_or(0.0);
    if upem > 0.0 {
        PxScale::from(size * font.height_unscaled() / upem)
    } else {
        PxScale::from(size)
    }
}

fn load_font(candidates: &[&str]) -> FontVec {
    try_load_font(candidates).unwrap_or_else(|| panic!("no system font found"))
}

/// Fetch + decode one webfont (`@font-face` src list, declared order).
/// Disk-cached like any resource; containers: raw sfnt + WOFF1, plus WOFF2
/// when this build has the `woff2` feature (`woff::decode_font` returns None
/// otherwise → try the next src / fall back to system fonts).
fn load_webfont(srcs: &[String]) -> Option<FontVec> {
    for u in srcs {
        let Ok((bytes, _)) = fetch(u) else { continue };
        let Some(sfnt) = woff::decode_font(&bytes) else {
            continue;
        };
        if let Ok(f) = FontVec::try_from_vec(sfnt.into_owned()) {
            eprintln!("[font] loaded webfont {u} ({} KB)", bytes.len() / 1024);
            return Some(f);
        }
    }
    None
}

/// First candidate that parses; None if none do (used for optional fallbacks).
fn try_load_font(candidates: &[&str]) -> Option<FontVec> {
    for p in candidates {
        if let Ok(bytes) = std::fs::read(p) {
            if let Ok(f) = FontVec::try_from_vec(bytes) {
                return Some(f);
            }
        }
    }
    None
}

/// Han, kana, CJK punctuation/symbols, compat ideographs, halfwidth forms.
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF | 0xFF60..=0xFF9F)
}

/// Hangul jamo, compat jamo, syllables.
fn is_hangul(ch: char) -> bool {
    matches!(ch as u32, 0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF)
}

fn render(
    buf: &mut [u32],
    width: usize,
    height: usize,
    laid: &Layout,
    fonts: &Fonts,
    scroll: f32,
    page: f32,
    // y offset of the page content inside the buffer (the old shell drew
    // the page below its chrome; the egui shell passes 0.0 and lets the
    // panels own that space instead)
    top: f32,
) {
    // sticky pinning: this frame's per-entry y offsets (empty on pages
    // without sticky boxes → every lookup is 0.0, paint unchanged); the
    // shift never re-sorts — zsort already ran at layout, so a moved
    // entry keeps its stacking level (sticky-vs-z is approximate)
    let so = layout::sticky_frame(laid, scroll);
    // backdrop-filter: a backdrop box's subtree entries are WITHHELD from
    // the normal channel paint (phase 1), its border-box region is
    // snapshotted and blurred over what phase 1 painted, then the subtree
    // paints on top (phase 2). Pages without the property record no groups:
    // `has_bd` stays false, phase 1 paints every entry and phase 2 never
    // runs — their output is byte-identical to a build without the feature.
    let has_bd = !laid.backdrops.is_empty();
    // one channel pass (shadows → rects → images → runs → underlines);
    // `one` false = phase 1 paints everything EXCEPT the deferred spans,
    // `one` true = phase 2 repaints exactly those entries; `bd` holds this
    // pass's merged deferred spans per channel (all empty without the
    // property, which makes phase 1 the plain paint and phase 2 a no-op)
    let paint = |buf: &mut [u32], want_fixed: bool, one: bool, bd: &[Vec<(usize, usize)>; 5]| {
        // box-shadows under everything else in this pass — blurred rounded
        // rects, per-pixel distance falloff over blur/2
        for (i, s) in laid.shadows.iter().enumerate() {
            if layout::in_spans(&bd[0], i) != one {
                continue;
            }
            if s.fixed != want_fixed {
                continue;
            }
            let y = s.y + so.sh.get(i).copied().unwrap_or(0.0);
            let y0 = if s.fixed { y } else { y - scroll };
            // grow the paint area by the blur so the falloff isn't clipped
            let pad = s.blur;
            if y0 + s.h + pad < 0.0 || y0 - pad > page {
                continue;
            }
            let (sx, sy) = (PADDING as f32 + s.x - pad, PADDING as f32 + top + y0 - pad);
            let (bw, bh) = (s.w + pad * 2.0, s.h + pad * 2.0);
            let a = (s.color >> 24) as u8;
            let half = (s.blur * 0.5).max(0.01);
            let (cx, cy) = (s.w * 0.5 + pad, s.h * 0.5 + pad);
            // the element's own border box in buffer coords: an outer shadow
            // is clipped to the outside of it (CSS), so a transparent box
            // never shows the shadow through itself
            let hy = if s.fixed { s.hole.1 } else { s.hole.1 - scroll };
            let (hx0, hy0) = (PADDING as f32 + s.hole.0, PADDING as f32 + top + hy);
            let (hw, hh) = (s.hole.2, s.hole.3);
            let hr = s.radius.min(hw * 0.5).min(hh * 0.5);
            for dy in 0..bh.max(0.0) as usize {
                let py = sy as i32 + dy as i32;
                if py < 0 || py >= height as i32 {
                    continue;
                }
                for dx in 0..bw.max(0.0) as usize {
                    let px = sx as i32 + dx as i32;
                    if px < 0 || px >= width as i32 {
                        continue;
                    }
                    // hole: inside the rounded border box → not shadow
                    let ox = px as f32 + 0.5 - hx0;
                    let oy = py as f32 + 0.5 - hy0;
                    if ox >= 0.0 && ox < hw && oy >= 0.0 && oy < hh {
                        let ex = ox.min(hw - ox);
                        let ey = oy.min(hh - oy);
                        if ex >= hr || ey >= hr || (hr - ex).powi(2) + (hr - ey).powi(2) <= hr * hr {
                            continue;
                        }
                    }
                    // signed distance to the (rounded) rect, then smoothstep
                    let lx = dx as f32 + pad;
                    let ly = dy as f32 + pad;
                    let qx = (lx - cx).abs().max(0.0) - s.w * 0.5 + s.radius;
                    let qy = (ly - cy).abs().max(0.0) - s.h * 0.5 + s.radius;
                    let dist = {
                        let (ax, ay) = (qx.max(0.0), qy.max(0.0));
                        (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - s.radius
                    };
                    if dist >= half {
                        continue;
                    }
                    let t = ((half - dist) / (2.0 * half)).clamp(0.0, 1.0);
                    let f = t * t * (3.0 - 2.0 * t); // smoothstep
                    let idx = py as usize * width + px as usize;
                    blend(&mut buf[idx], s.color, (a as f32 * f) as u8);
                }
            }
        }
        // box backgrounds first (parent-before-child), then text on top
        for (i, &(x, y, w, h, color, rad, fixed)) in laid.rects.iter().enumerate() {
            if layout::in_spans(&bd[1], i) != one {
                continue;
            }
            if fixed != want_fixed {
                continue;
            }
            let y = y + so.rect.get(i).copied().unwrap_or(0.0);
            let y = if fixed { y } else { y - scroll };
            if y + h < 0.0 || y > page {
                continue;
            }
            let (sx, sy) = (PADDING as f32 + x, PADDING as f32 + top + y);
            let r = rad.min(w * 0.5).min(h * 0.5);
            // high byte = effective opacity (0 = un-packed colour, treat as
            // opaque so every pre-opacity caller keeps painting solid)
            let alpha = match (color >> 24) as u8 {
                0 => 255,
                a => a,
            };
            let rgb = color & 0x00ff_ffff;
            // a gradient slot paints the rect per-pixel (same geometry,
            // radius, sticky shift and transform as the solid fill)
            if let Some(g) = laid.rect_grad.get(i).and_then(|g| g.as_deref()) {
                paint_gradient(buf, width, height, sx, sy, w, h, r, g, alpha);
                continue;
            }
            // a border RING paints only the band inside the outer radius, so a
            // translucent background behind it stays visible (P85)
            if let Some(t) = laid.rect_ring.get(i).copied().flatten() {
                paint_ring_rect(buf, width, height, sx, sy, w, h, r, t, rgb, alpha);
                continue;
            }
            paint_solid_rect(buf, width, height, sx, sy, w, h, r, rgb, alpha);
        }
        // decoded <img> boxes next (nearest-neighbour scale, alpha-blended)
        for (i, (x, y, w, h, img, fixed, ialpha, filt)) in laid.images.iter().enumerate() {
            if layout::in_spans(&bd[2], i) != one {
                continue;
            }
            if *fixed != want_fixed {
                continue;
            }
            let y = *y + so.img.get(i).copied().unwrap_or(0.0);
            let y0 = if *fixed { y } else { y - scroll };
            if y0 + h < 0.0 || y0 > page {
                continue;
            }
            let (sx, sy) = (PADDING as f32 + x, PADDING as f32 + top + y0);
            for dy in 0..h.max(0.0) as usize {
                let py = sy as i32 + dy as i32;
                if py < 0 || py >= height as i32 {
                    continue;
                }
                let v = (dy as f32 * img.h as f32 / h) as u32;
                for dx in 0..w.max(0.0) as usize {
                    let px = sx as i32 + dx as i32;
                    if px < 0 || px >= width as i32 {
                        continue;
                    }
                    let u = (dx as f32 * img.w as f32 / w) as u32;
                    let mut c = img.px[(v * img.w + u) as usize];
                    if let Some(f) = filt.as_deref() {
                        c = f.apply_rgb(c);
                    }
                    let a = ((c >> 24) as u16 * *ialpha as u16 / 255) as u8;
                    let idx = py as usize * width + px as usize;
                    if a == 255 {
                        buf[idx] = c & 0x00ff_ffff;
                    } else if a > 0 {
                        blend(&mut buf[idx], c, a);
                    }
                }
            }
        }
        for (i, run) in laid.runs.iter().enumerate() {
            if layout::in_spans(&bd[3], i) != one {
                continue;
            }
            if run.fixed != want_fixed {
                continue;
            }
            // sticky pin: the run and its clip window move together
            // (glued like a transform — see StickyGroup's shortcuts)
            let dy = so.run.get(i).copied().unwrap_or(0.0);
            let y = if run.fixed { run.y + dy } else { run.y + dy - scroll };
            if y < -run.style.size * 2.0 || y > page + run.style.size {
                continue;
            }
            // clip window converts like y (fixed = viewport coords, else
            // doc coords − scroll). Fully-outside / degenerate window →
            // draw NOTHING (the skip-link case: text outside a 1px box).
            let clip = run.clip.map(|(x0, y0, x1, y1)| {
                let (y0, y1) = (y0 + dy, y1 + dy);
                let (y0, y1) = if run.fixed { (y0, y1) } else { (y0 - scroll, y1 - scroll) };
                (x0, y0, x1, y1)
            });
            if let Some((x0, y0, x1, y1)) = clip {
                if x1 <= x0
                    || y1 <= y0
                    || run.x >= x1
                    || run.x + run.width <= x0
                    || y >= y1
                    || y + run.style.size <= y0
                {
                    continue;
                }
            }
            // rects/images/underlines were already intersected in layout
            // (collect_paint / apply_run_clips) — only runs clip here.
            draw_text(
                buf,
                width,
                PADDING as f32 + run.x,
                PADDING as f32 + top + y,
                &run.text,
                fonts,
                &run.style,
                run.alpha,
                clip.map(|(x0, y0, x1, y1)| {
                    (
                        PADDING as f32 + x0,
                        PADDING as f32 + top + y0,
                        PADDING as f32 + x1,
                        PADDING as f32 + top + y1,
                    )
                }),
            );
        }
        for (i, &(x, y, w, h, color, fixed)) in laid.underline_rects.iter().enumerate() {
            if layout::in_spans(&bd[4], i) != one {
                continue;
            }
            if fixed != want_fixed {
                continue;
            }
            let y = y + so.ul.get(i).copied().unwrap_or(0.0);
            let y = if fixed { y } else { y - scroll };
            if y < 0.0 || y > page {
                continue;
            }
            let (sx, sy) = (PADDING as f32 + x, PADDING as f32 + top + y);
            let alpha = match (color >> 24) as u8 {
                0 => 255,
                a => a,
            };
            let rgb = color & 0x00ff_ffff;
            paint_solid_rect(buf, width, height, sx, sy, w, h.max(1.0), 0.0, rgb, alpha);
        }
    };
    // two passes: normal content (scrolled) first, then fixed overlays on
    // top in viewport coordinates (no scroll offset); each pass runs the
    // phase split above — blur only exists between them when a backdrop
    // group of this pass's fixedness was recorded
    for want_fixed in [false, true] {
        let bd: [Vec<(usize, usize)>; 5] = if has_bd {
            layout::backdrop_spans(&laid.backdrops, want_fixed)
        } else {
            Default::default()
        };
        paint(buf, want_fixed, false, &bd);
        if has_bd {
            blur_backdrops(buf, width, height, laid, &so, scroll, top, want_fixed);
            paint(buf, want_fixed, true, &bd);
        }
    }
}

fn draw_text(
    buf: &mut [u32],
    width: usize,
    x0: f32,
    y0: f32,
    s: &str,
    fonts: &Fonts,
    style: &Style,
    // effective opacity byte for this run (255 = opaque)
    alpha: u8,
    // clip window in BUFFER coords (x0, y0, x1, y1); glyphs fully outside
    // are skipped — partial overlap still blends (the buffer has no clip
    // stack; intersection was pre-applied to the run's window in layout)
    clip: Option<(f32, f32, f32, f32)>,
) {
    // `text-shadow` layers paint UNDER this run's own glyphs (the shadow is
    // part of the text's painting), so they go first. shortcut: the
    // underline/strike/overline decorations are NOT shadowed — they ride a
    // separate rect channel.
    if style.text_shadow.iter().any(Option::is_some) {
        draw_text_shadows(buf, width, x0, y0, s, fonts, style, alpha, clip);
    }
    for_each_glyph(x0, y0, s, fonts, style, |m, gx0, gy0| {
        if let Some((cx0, cy0, cx1, cy1)) = clip {
            if (gx0 + m.w as i32) as f32 <= cx0
                || gx0 as f32 >= cx1
                || (gy0 + m.h as i32) as f32 <= cy0
                || gy0 as f32 >= cy1
            {
                return; // glyph entirely outside the clip window
            }
        }
        for gy in 0..m.h {
            let dy = gy0 + gy as i32;
            if dy < 0 {
                continue;
            }
            let row = dy as usize * width;
            if row >= buf.len() {
                continue;
            }
            for gx in 0..m.w {
                let cov = m.cov[gy * m.w + gx];
                if cov == 0 {
                    continue;
                }
                let dx = gx0 + gx as i32;
                if dx < 0 || dx as usize >= width {
                    continue;
                }
                let idx = row + dx as usize;
                if idx < buf.len() {
                    let cov = ((cov as u16 * alpha as u16) / 255) as u8;
                    if cov > 0 {
                        blend(&mut buf[idx], style.color, cov);
                    }
                }
            }
        }
    });
}

/// Walk one run's glyphs in pen order and hand each rasterized mask plus its
/// integer top-left to `f`. `draw_text` (the ink) and `draw_text_shadows`
/// (the shadow plane) share it, so the letter/word-spacing arithmetic —
/// which must stay bit-identical to the width layout measured, or painted
/// ink drifts off the run (wrap decisions, clip windows, text-align,
/// underlines) — lives in exactly one place.
fn for_each_glyph<F: FnMut(&Rc<GlyphMask>, i32, i32)>(
    x0: f32,
    y0: f32,
    s: &str,
    fonts: &Fonts,
    style: &Style,
    mut f: F,
) {
    // baseline = the font's real ascent below the line box top, after the
    // half-leading CSS centres the em box with: the old flat `0.8em` guess sat
    // text several px high on every line whose `line-height` exceeded 1em.
    let first = s.chars().next().unwrap_or(' ');
    let baseline = {
        let m = fonts.pick_font_for(first, style);
        let upem = m.units_per_em().unwrap_or(0.0);
        let em = if upem > 0.0 { style.size / upem } else { 1.0 };
        let asc = m.ascent_unscaled() * em;
        let desc = -m.descent_unscaled() * em;
        y0 + (style.line_height() - asc - desc) * 0.5 + asc
    };
    let mut x = x0;
    for ch in s.chars() {
        let gap = if ch.is_whitespace() { style.word_spacing } else { 0.0 } + style.letter_spacing;
        // same font choice as measure() so widths match what's drawn
        let font = fonts.pick_font_for(ch, style);
        let scale = em_scale(&font, style.size);
        let scaled = font.as_scaled(scale);
        let id = font.glyph_id(ch);
        let advance = scaled.h_advance(id);
        // cached coverage (rasterized once per glyph per subpixel offset)
        if let Some(m) = fonts.glyph_mask(&font, id, scale, x, baseline) {
            f(&m, x.floor() as i32 + m.ox, baseline.floor() as i32 + m.oy);
        }
        x += advance + gap;
    }
}

/// Paint this run's `text-shadow` layers: stamp the glyph coverage into a
/// small plane, box-blur it once per layer (three passes ≈ a Gaussian with
/// σ = blur/2, the CSS definition of the blur radius) and composite at the
/// layer's offset — clipped like the glyphs, UNDER the run's own ink.
/// Layers are emitted in reverse so layer 0 (topmost in CSS) paints last.
/// shortcut: offsets are rounded to whole pixels (no sub-pixel shadow
/// position) and the plane is clamped to the buffer, so blur that runs off
/// the edge is cut with the viewport.
fn draw_text_shadows(
    buf: &mut [u32],
    width: usize,
    x0: f32,
    y0: f32,
    s: &str,
    fonts: &Fonts,
    style: &Style,
    alpha: u8,
    clip: Option<(f32, f32, f32, f32)>,
) {
    let mut ink: Vec<(std::rc::Rc<GlyphMask>, i32, i32)> = Vec::new();
    for_each_glyph(x0, y0, s, fonts, style, |m, gx, gy| {
        ink.push((m.clone(), gx, gy))
    });
    if ink.is_empty() {
        return;
    }
    // ink bounds, then the blur halo around them
    let (mut bx0, mut by0, mut bx1, mut by1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for (m, gx, gy) in &ink {
        bx0 = bx0.min(*gx);
        by0 = by0.min(*gy);
        bx1 = bx1.max(*gx + m.w as i32);
        by1 = by1.max(*gy + m.h as i32);
    }
    let blur_max = style
        .text_shadow
        .iter()
        .flatten()
        .map(|l| l.blur)
        .fold(0.0f32, f32::max);
    // 3 box passes of radius blur/2 spread the ink by ~1.5 × blur
    let pad = (blur_max * 1.5).ceil() as i32 + 1;
    // nothing outside the buffer can be drawn — clamp the plane to it
    let height = buf.len() / width.max(1);
    let (ox, oy) = ((bx0 - pad).max(0), (by0 - pad).max(0));
    let (ex, ey) = (
        (bx1 + pad).min(width as i32),
        (by1 + pad).min(height as i32),
    );
    let (pw, ph) = ((ex - ox).max(0) as usize, (ey - oy).max(0) as usize);
    if pw == 0 || ph == 0 {
        return;
    }
    let mut plane = vec![0u8; pw * ph];
    for (m, gx, gy) in &ink {
        let (sx, sy) = (gx - ox, gy - oy);
        for my in 0..m.h {
            let py = sy + my as i32;
            if py < 0 || py as usize >= ph {
                continue;
            }
            for mx in 0..m.w {
                let px = sx + mx as i32;
                if px < 0 || px as usize >= pw {
                    continue;
                }
                let i = py as usize * pw + px as usize;
                plane[i] = plane[i].saturating_add(m.cov[my * m.w + mx]);
            }
        }
    }
    let mut work = plane.clone();
    let mut tmp = vec![0u8; plane.len()];
    for layer in style.text_shadow.iter().flatten().rev() {
        if layer.blur > 0.0 {
            work.copy_from_slice(&plane);
            let r = (layer.blur * 0.5).round().max(1.0) as usize;
            for _ in 0..3 {
                box_blur(&mut work, &mut tmp, pw, ph, r);
            }
        }
        let color = if layer.current { style.color } else { layer.color };
        let la = ((layer.alpha as u16 * alpha as u16) / 255) as u8;
        if la == 0 {
            continue;
        }
        for y in 0..ph {
            let dy = oy as f32 + y as f32 + layer.dy;
            let dy = dy.round() as i32;
            if dy < 0 || dy as usize >= height {
                continue;
            }
            for x in 0..pw {
                let cov = work[y * pw + x];
                if cov == 0 {
                    continue;
                }
                let dx = ox as f32 + x as f32 + layer.dx;
                let dx = dx.round() as i32;
                if dx < 0 || dx as usize >= width {
                    continue;
                }
                if let Some((cx0, cy0, cx1, cy1)) = clip {
                    if (dx as f32) < cx0
                        || (dx as f32) >= cx1
                        || (dy as f32) < cy0
                        || (dy as f32) >= cy1
                    {
                        continue;
                    }
                }
                let a = ((cov as u16 * la as u16) / 255) as u8;
                if a > 0 {
                    blend(&mut buf[dy as usize * width + dx as usize], color, a);
                }
            }
        }
    }
}

/// `backdrop-filter`: snapshot the already-painted pixels under each
/// backdrop group's border box (its subtree's entries are still withheld —
/// see `render`'s phase split), box-blur the three colour planes by the
/// group's radius, and write the result back REPLACING the backdrop,
/// clipped to the rounded border box. Only this pass's `fixed`ness runs
/// here; a disabled group (`blur <= 0`) is skipped like a missing one.
fn blur_backdrops(
    buf: &mut [u32],
    width: usize,
    height: usize,
    laid: &Layout,
    so: &layout::StickyFrame,
    scroll: f32,
    top: f32,
    want_fixed: bool,
) {
    if laid.backdrops.is_empty() || width == 0 || height == 0 {
        return;
    }
    let mut plane: Vec<u8> = Vec::new();
    let mut tmp: Vec<u8> = Vec::new();
    for g in &laid.backdrops {
        if g.fixed != want_fixed || g.blur <= 0.0 {
            continue;
        }
        // the region moves with its sticky box like every entry in its
        // spans (the subtree's first rect — or run — carries the summed
        // pin offsets; pages without sticky record 0.0 everywhere)
        let dy = so
            .rect
            .get(g.rects.0)
            .or_else(|| so.run.get(g.runs.0))
            .copied()
            .unwrap_or(0.0);
        let (x, y, w, h) = g.rect;
        let y = if g.fixed { y } else { y - scroll } + dy;
        let (sx, sy) = (PADDING as f32 + x, PADDING as f32 + top + y);
        let (rw, rh) = (w.max(0.0) as usize, h.max(0.0) as usize);
        if rw == 0 || rh == 0 {
            continue;
        }
        // 3 box passes of radius ≈ blur track the spec's Gaussian σ = blur
        // (the text-shadow recipe; a single pass comes out ~2x too sharp)
        let r = (g.blur.round() as usize).max(1);
        plane.resize(rw * rh, 0);
        tmp.resize(rw * rh, 0);
        for &shift in &[16u32, 8, 0] {
            // snapshot this plane; samples outside the buffer clamp to its
            // edge (the same edge treatment box_blur gives its own windows)
            for dy in 0..rh {
                let py = (sy as i32 + dy as i32).clamp(0, height as i32 - 1);
                for dx in 0..rw {
                    let px = (sx as i32 + dx as i32).clamp(0, width as i32 - 1);
                    plane[dy * rw + dx] =
                        ((buf[py as usize * width + px as usize] >> shift) & 0xff) as u8;
                }
            }
            for _ in 0..3 {
                box_blur(&mut plane, &mut tmp, rw, rh, r);
            }
            // write back inside the rounded border box (corner cull like
            // paint_solid_rect); pixels off-buffer are simply not written
            for dy in 0..rh {
                let ly = dy as f32;
                for dx in 0..rw {
                    let lx = dx as f32;
                    if g.radius > 0.5 {
                        let cx = if lx < g.radius {
                            g.radius
                        } else if lx > w - g.radius {
                            w - g.radius
                        } else {
                            lx
                        };
                        let cy = if ly < g.radius {
                            g.radius
                        } else if ly > h - g.radius {
                            h - g.radius
                        } else {
                            ly
                        };
                        if (lx - cx) * (lx - cx) + (ly - cy) * (ly - cy) > g.radius * g.radius {
                            continue;
                        }
                    }
                    let px = sx as i32 + dx as i32;
                    let py = sy as i32 + dy as i32;
                    if px < 0 || px >= width as i32 || py < 0 || py >= height as i32 {
                        continue;
                    }
                    let idx = py as usize * width + px as usize;
                    buf[idx] = (buf[idx] & !(0xffu32 << shift))
                        | ((plane[dy * rw + dx] as u32) << shift);
                }
            }
        }
    }
}

/// One horizontal + one vertical box-blur pass over a coverage plane
/// (clamped at the plane's edges), in place through `tmp`.
fn box_blur(a: &mut [u8], tmp: &mut [u8], w: usize, h: usize, r: usize) {
    for y in 0..h {
        let row = &a[y * w..y * w + w];
        let out = &mut tmp[y * w..y * w + w];
        for x in 0..w {
            let lo = x.saturating_sub(r);
            let hi = (x + r + 1).min(w);
            let (mut sum, mut n) = (0u32, 0u32);
            for &v in &row[lo..hi] {
                sum += v as u32;
                n += 1;
            }
            out[x] = (sum / n.max(1)) as u8;
        }
    }
    for x in 0..w {
        for y in 0..h {
            let lo = y.saturating_sub(r);
            let hi = (y + r + 1).min(h);
            let (mut sum, mut n) = (0u32, 0u32);
            for yy in lo..hi {
                sum += tmp[yy * w + x] as u32;
                n += 1;
            }
            a[y * w + x] = (sum / n.max(1)) as u8;
        }
    }
}

fn blend(dst: &mut u32, src: u32, alpha: u8) {
    let a = alpha as u32;
    let sr = (src >> 16) & 0xff;
    let sg = (src >> 8) & 0xff;
    let sb = src & 0xff;
    let dr = (*dst >> 16) & 0xff;
    let dg = (*dst >> 8) & 0xff;
    let db = *dst & 0xff;
    let r = (sr * a + dr * (255 - a)) / 255;
    let g = (sg * a + dg * (255 - a)) / 255;
    let b = (sb * a + db * (255 - a)) / 255;
    *dst = (r << 16) | (g << 8) | b;
}

/// Interpolate a gradient's color at `t` (0..1) and return `(rgb, alpha)`.
fn grad_at(g: &css::Gradient, t: f32) -> (u32, u8) {
    let s = &g.stops;
    if s.is_empty() {
        return (0, 0);
    }
    if t <= s[0].pos {
        return (s[0].color, s[0].alpha);
    }
    let last = &s[s.len() - 1];
    if t >= last.pos {
        return (last.color, last.alpha);
    }
    for w in s.windows(2) {
        if t <= w[1].pos {
            let d = w[1].pos - w[0].pos;
            let f = if d > 1e-6 { (t - w[0].pos) / d } else { 1.0 };
            // premultiplied interpolation (CSS Color 4): a fade to
            // `transparent` keeps its hue instead of darkening toward black
            let a0 = w[0].alpha as f32 / 255.0;
            let a1 = w[1].alpha as f32 / 255.0;
            let a = a0 + (a1 - a0) * f;
            let pm = |c0: u32, c1: u32| -> u32 {
                if a <= 1e-6 {
                    0
                } else {
                    (((c0 as f32 * a0) * (1.0 - f) + (c1 as f32 * a1) * f) / a) as u32
                }
            };
            let r = pm((w[0].color >> 16) & 0xff, (w[1].color >> 16) & 0xff);
            let gch = pm((w[0].color >> 8) & 0xff, (w[1].color >> 8) & 0xff);
            let b = pm(w[0].color & 0xff, w[1].color & 0xff);
            return (r << 16 | gch << 8 | b, (a * 255.0).round() as u8);
        }
    }
    (last.color, last.alpha)
}

/// Resolve one radial center axis against the painted box extent.
fn rad_axis(p: css::BgPos, extent: f32) -> f32 {
    match p {
        css::BgPos::Pct(v) => v / 100.0 * extent,
        css::BgPos::Px(v) => v,
    }
}

/// Resolve a radial gradient's center and radii against the painted box.
/// `t = sqrt(((x-cx)/rx)^2 + ((y-cy)/ry)^2)` is 0 at the center and 1 on the
/// ending shape. shortcut: corner keywords use the chosen corner's |dx|/|dy|
/// as the ellipse radii (so `t` reaches 1 exactly at that corner) rather
/// than the spec's aspect-ratio scaling; a circle's percentage radius
/// resolves against `hypot(w, h) / sqrt(2)`.
fn radial_radii(g: &css::Radial, w: f32, h: f32) -> (f32, f32, f32, f32) {
    let cx = rad_axis(g.center[0], w);
    let cy = rad_axis(g.center[1], h);
    let near_x = cx.min((w - cx).abs());
    let far_x = cx.max((w - cx).abs());
    let near_y = cy.min((h - cy).abs());
    let far_y = cy.max((h - cy).abs());
    let corner = |x: f32, y: f32| (x * x + y * y).sqrt();
    let circ_pct = (w * w + h * h).sqrt() / std::f32::consts::SQRT_2;
    let (mut rx, mut ry) = match g.size {
        css::RadSize::Keyword(e) => match (g.circle, e) {
            (true, css::RadExtent::ClosestSide) => {
                let v = near_x.min(near_y);
                (v, v)
            }
            (true, css::RadExtent::FarthestSide) => {
                let v = far_x.max(far_y);
                (v, v)
            }
            (true, css::RadExtent::ClosestCorner) => {
                let v = corner(near_x, near_y);
                (v, v)
            }
            (true, css::RadExtent::FarthestCorner) => {
                let v = corner(far_x, far_y);
                (v, v)
            }
            (false, css::RadExtent::ClosestSide) => (near_x, near_y),
            (false, css::RadExtent::FarthestSide) => (far_x, far_y),
            (false, css::RadExtent::ClosestCorner) => (near_x, near_y),
            (false, css::RadExtent::FarthestCorner) => (far_x, far_y),
        },
        css::RadSize::Lengths(lx, ly) => {
            let one = |l: css::Len, base: f32| match l {
                css::Len::Px(v) => v,
                css::Len::Pct(p) => p / 100.0 * base,
                css::Len::Calc { px, pct } => px + pct / 100.0 * base,
            };
            let x = lx
                .map(|l| one(l, if g.circle { circ_pct } else { w }))
                .unwrap_or(far_x);
            let y = ly
                .map(|l| one(l, if g.circle { circ_pct } else { h }))
                .unwrap_or(far_y);
            if g.circle {
                let v = x.max(0.0);
                (v, v)
            } else {
                (x, y)
            }
        }
    };
    // degenerate axes: keep the metric finite (the last stop fills the box)
    if !(rx > 1e-4) {
        rx = 1.0;
    }
    if !(ry > 1e-4) {
        ry = 1.0;
    }
    (cx, cy, rx, ry)
}

/// Paint one `linear-gradient` / `radial-gradient` into a rect of the page
/// buffer. For a linear gradient the gradient line crosses the rect centre
/// along `g.dir`; its length is the rect's extent along that direction (the
/// CSS rule for a linear gradient), so `t` is 0 at the entry edge and 1 at
/// the exit edge. A radial gradient maps each pixel through
/// [`radial_radii`](fn.radial_radii.html)'s normalized distance instead.
/// Fill one axis-aligned rect into the buffer, clipped to the buffer edges.
/// `sx`/`sy` are buffer coords and may be negative (a painted transform can
/// put a box left of / above the page): the span is intersected with the
/// buffer instead of only clamping the origin, which would smear the box's
/// full width/height from pixel 0.
fn paint_solid_rect(
    buf: &mut [u32],
    width: usize,
    height: usize,
    sx: f32,
    sy: f32,
    w: f32,
    h: f32,
    r: f32,
    rgb: u32,
    alpha: u8,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let (ox, oy) = (sx as i32, sy as i32);
    let (xa, xb) = (
        ox.max(0) as usize,
        (ox + w as i32).clamp(0, width as i32) as usize,
    );
    if xa >= xb {
        return;
    }
    // square corners: one row fill per line instead of per-pixel math
    if r <= 0.5 {
        for dy in 0..h as i32 {
            let py = oy + dy;
            if py < 0 {
                continue;
            }
            let py = py as usize;
            if py >= height {
                break;
            }
            if alpha == 255 {
                buf[py * width + xa..py * width + xb].fill(rgb);
            } else {
                for idx in &mut buf[py * width + xa..py * width + xb] {
                    blend(idx, rgb, alpha);
                }
            }
        }
        return;
    }
    for dy in 0..h as i32 {
        let py = oy + dy;
        if py < 0 {
            continue;
        }
        let py = py as usize;
        if py >= height {
            break;
        }
        let ly = dy as f32; // y relative to rect top
        for dx in 0..w as i32 {
            let px = ox + dx;
            if px < 0 {
                continue;
            }
            if px >= width as i32 {
                break;
            }
            let px = px as usize;
            // rounded corners: cull pixels outside the corner circles
            let lx = dx as f32;
            let cx = if lx < r { r } else if lx > w - r { w - r } else { lx };
            let cy = if ly < r { r } else if ly > h - r { h - r } else { ly };
            let (ex, ey) = (lx - cx, ly - cy);
            if ex * ex + ey * ey > r * r {
                continue;
            }
            if alpha == 255 {
                buf[py * width + px] = rgb;
            } else {
                blend(&mut buf[py * width + px], rgb, alpha);
            }
        }
    }
}

/// Horizontal extent `(x0, x1)` of a rounded rect at row `ly`, in rect-local
/// coordinates (`0..w` by `ly` within `0..h`).
fn round_span(ly: f32, w: f32, h: f32, r: f32) -> (f32, f32) {
    let r = r.min(w * 0.5).min(h * 0.5);
    if r <= 0.0 {
        return (0.0, w);
    }
    let d = if ly < r {
        let dy = r - ly;
        r - (r * r - dy * dy).max(0.0).sqrt()
    } else if ly > h - r {
        let dy = ly - (h - r);
        r - (r * r - dy * dy).max(0.0).sqrt()
    } else {
        0.0
    };
    (d, w - d)
}

/// Paint a rounded-rect BORDER RING: the band between the outer rounded rect
/// and the same rect inset by `t` (its radius shrunk by `t`). A border must be
/// a ring, not a fill of the whole border box, or the border colour shows
/// through a translucent background painted on top of it (see SPIKE P85).
/// Two spans per row: the ring's left and right parts, never the interior.
#[allow(clippy::too_many_arguments)]
fn paint_ring_rect(
    buf: &mut [u32],
    width: usize,
    height: usize,
    sx: f32,
    sy: f32,
    w: f32,
    h: f32,
    r: f32,
    t: f32,
    rgb: u32,
    alpha: u8,
) {
    if w <= 0.0 || h <= 0.0 || t <= 0.0 {
        return;
    }
    let t = t.min(w * 0.5).min(h * 0.5);
    let (iw, ih) = (w - t * 2.0, h - t * 2.0);
    let ri = (r - t).max(0.0);
    let (ox, oy) = (sx as i32, sy as i32);
    let fill = |buf: &mut [u32], py: i32, x0: f32, x1: f32| {
        if py < 0 || py as usize >= height {
            return;
        }
        let xa = (ox as f32 + x0).round().max(0.0).min(width as f32) as usize;
        let xb = (ox as f32 + x1).round().max(0.0).min(width as f32) as usize;
        if xa >= xb {
            return;
        }
        let row = &mut buf[py as usize * width + xa..py as usize * width + xb];
        if alpha == 255 {
            row.fill(rgb);
        } else {
            for idx in row {
                blend(idx, rgb, alpha);
            }
        }
    };
    for dy in 0..h as i32 {
        let py = oy + dy;
        if py < 0 {
            continue;
        }
        if py as usize >= height {
            break;
        }
        let ly = dy as f32 + 0.5;
        let (o0, o1) = round_span(ly, w, h, r);
        // rows above/below the inner rect are ring from edge to edge
        if ly < t || ly > t + ih {
            fill(buf, py, o0, o1);
            continue;
        }
        let (i0, i1) = round_span(ly - t, iw, ih, ri);
        fill(buf, py, o0, (t + i0).min(o1));
        fill(buf, py, (t + i1).max(o0), o1);
    }
}

fn paint_gradient(
    buf: &mut [u32],
    width: usize,
    height: usize,
    sx: f32,
    sy: f32,
    w: f32,
    h: f32,
    r: f32,
    g: &css::Gradient,
    // effective opacity byte of the box that owns this gradient
    opacity: u8,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let len = (w * g.dir.0).abs() + (h * g.dir.1).abs();
    let len = if len > 1e-6 { len } else { 1.0 };
    let (hw, hh) = (w * 0.5, h * 0.5);
    let radial = g.radial.as_ref().map(|r| radial_radii(r, w, h));
    let (ox, oy) = (sx as i32, sy as i32);
    for dy in 0..h.max(0.0) as usize {
        let py = oy + dy as i32;
        if py < 0 {
            continue;
        }
        if py >= height as i32 {
            break;
        }
        let ly = dy as f32;
        for dx in 0..w.max(0.0) as usize {
            let px = ox + dx as i32;
            if px < 0 {
                continue;
            }
            if px >= width as i32 {
                break;
            }
            if r > 0.5 {
                let lx = dx as f32;
                let cx = if lx < r { r } else if lx > w - r { w - r } else { lx };
                let cy = if ly < r { r } else if ly > h - r { h - r } else { ly };
                let (ex, ey) = (lx - cx, ly - cy);
                if ex * ex + ey * ey > r * r {
                    continue;
                }
            }
            let lx = dx as f32;
            let t = match radial {
                Some((cx, cy, rx, ry)) => {
                    let nx = (lx - cx) / rx;
                    let ny = (ly - cy) / ry;
                    (nx * nx + ny * ny).sqrt()
                }
                None => 0.5 + ((lx - hw) * g.dir.0 + (ly - hh) * g.dir.1) / len,
            };
            let (c, a) = grad_at(g, t.clamp(0.0, 1.0));
            let a = ((a as u16 * opacity as u16) / 255) as u8;
            let idx = py as usize * width + px as usize;
            if a == 255 {
                buf[idx] = c;
            } else if a > 0 {
                blend(&mut buf[idx], c, a);
            }
        }
    }
}

/// Fill a vertical pill (rounded top and bottom, square sides) into the
/// page buffer, alpha-blended. Used for the scrollbar track and thumb.
fn fill_pill(
    buf: &mut [u32],
    width: usize,
    height: usize,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    color: u32,
    alpha: u8,
) {
    let r = ((x1 - x0) * 0.5).max(0.5);
    let y0 = y0.max(0.0);
    let y1 = y1.min(height as f32);
    let xa = x0.max(0.0) as usize;
    let xb = x1.min(width as f32) as usize;
    let mut py = y0 as usize;
    while (py as f32) < y1 && py < height {
        // inset from the pill's rounded end at this row (0 in the middle)
        let cy = py as f32 + 0.5;
        let d = (cy - y0).min(y1 - cy).max(0.0);
        let inset = if d < r {
            r - (r * r - (r - d) * (r - d)).max(0.0).sqrt()
        } else {
            0.0
        };
        let lo = (xa as f32 + inset) as usize;
        let hi = (xb as f32 - inset).max(0.0) as usize;
        for px in lo..hi {
            blend(&mut buf[py * width + px], color, alpha);
        }
        py += 1;
    }
}

/// Page scrollbar drawn after every render in viewport coordinates: a faint
/// track in the right gutter and a proportional thumb. Shortcut: no
/// hover highlight, no arrow buttons, no CSS scrollbar styling, and no
/// scrollbar gutter reservation — the strip paints over the page padding,
/// which is outside the laid-out content, so nothing is occluded.
fn draw_scrollbar(buf: &mut [u32], width: usize, height: usize, doc_h: f32, page: f32, scroll: f32) {
    let Some((ty, th)) = scrollbar_thumb(doc_h, page, scroll) else {
        return;
    };
    let (x0, x1) = scrollbar_strip(width);
    let top = PADDING as f32;
    let bottom = (height as f32 - PADDING as f32).max(top);
    fill_pill(buf, width, height, x0, x1, top, bottom, 0x00_00_00, 18);
    fill_pill(buf, width, height, x0, x1, top + ty, top + ty + th, 0x00_00_00, 96);
}

/// Working-set size in MB via GetProcessMemoryInfo.
#[cfg(windows)]
fn working_set_mb() -> f64 {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ) != 0
        {
            counters.WorkingSetSize as f64 / (1024.0 * 1024.0)
        } else {
            0.0
        }
    }
}

/// Working-set size in MB via /proc/self/status VmRSS.
#[cfg(not(windows))]
fn working_set_mb() -> f64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0.0;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0.0);
            return kb / 1024.0;
        }
    }
    0.0
}

/// Diagnostic switches for the reload soak. `MAZDA_NO_JS` skips script
/// execution and `MAZDA_NO_IMG` skips image fetch/decode, so a memory slope
/// can be attributed to the page's scripts, its images, or the core pipeline.
/// Both default off and cost one cached environment lookup.
fn no_js() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MAZDA_NO_JS").is_some())
}

fn no_images() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MAZDA_NO_IMG").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_scroll_brings_a_hit_into_view_and_clamps() {
        let hit = |y: f32| FindHit {
            x: 0.0,
            y,
            w: 10.0,
            h: 16.0,
            fixed: false,
        };
        // below the viewport: scroll just enough plus the margin
        assert_eq!(find_scroll(&hit(640.0), 0.0, 600.0, 2000.0), 96.0);
        // above the viewport: pin the top with the margin
        assert_eq!(find_scroll(&hit(100.0), 300.0, 600.0, 2000.0), 60.0);
        // clamp at the document end
        assert_eq!(find_scroll(&hit(1950.0), 0.0, 600.0, 2000.0), 1400.0);
        // a fixed hit is already on screen: never scroll
        let fixed = FindHit { fixed: true, ..hit(1999.0) };
        assert_eq!(find_scroll(&fixed, 42.0, 600.0, 2000.0), 42.0);
    }

    #[test]
    fn print_pdf_has_a_page_per_image_and_a_valid_xref() {
        let pdf = build_pdf(&[vec![0u8; 10], vec![1u8; 20]]);
        let s = String::from_utf8_lossy(&pdf);
        assert!(s.starts_with("%PDF-1.4\n"));
        assert!(s.ends_with("%%EOF\n"));
        assert!(s.contains("/Count 2"));
        assert_eq!(s.matches("/Type /Page ").count(), 2);
        assert_eq!(s.matches("/Subtype /Image").count(), 2);
        // startxref must point at the xref table itself (not "startxref").
        // Search the raw bytes: the binary comment on line 2 is not UTF-8, so
        // offsets in the lossy string would not be file offsets.
        let kw = b"startxref\n";
        let start = pdf.windows(kw.len()).rposition(|w| w == kw).unwrap() + kw.len();
        let len = pdf[start..].iter().position(|&b| b == b'\n').unwrap();
        let startxref: usize = std::str::from_utf8(&pdf[start..start + len])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let xref = pdf.windows(6).rposition(|w| w == b"\nxref\n").unwrap() + 1;
        assert_eq!(startxref, xref);
    }

    /// A minimal live page for the parking tests: no scripts were run, an
    /// empty stylesheet, no font faces.
    fn fake_page(url: &str) -> KeptPage {
        KeptPage {
            url: url.to_string(),
            dom: html::parse("<!doctype html><p>x</p>"),
            css_text: String::new(),
            faces: Vec::new(),
            scripts: None,
            engine: EngineChoice::A,
            hover: None,
            imgs: ImgCache::default(),
            sheet: None,
            failed: false,
        }
    }

    #[test]
    fn a_parked_page_is_bound_by_url() {
        let mut kept = Some(fake_page("https://a/"));
        let mut parked = Vec::new();
        assert!(!bind_parked(&mut kept, &mut parked, "https://b/"), "not parked");
        let old = kept.take().unwrap();
        park_page(&mut parked, old);
        assert!(bind_parked(&mut kept, &mut parked, "https://a/"));
        assert_eq!(kept.as_ref().unwrap().url, "https://a/");
        assert!(parked.is_empty(), "the bound page left the lot");
    }

    #[test]
    fn switching_between_two_live_pages_only_moves_them() {
        let mut kept = Some(fake_page("https://a/"));
        let mut parked = vec![fake_page("https://b/")];
        assert!(bind_parked(&mut kept, &mut parked, "https://b/"));
        assert_eq!(kept.as_ref().unwrap().url, "https://b/");
        assert_eq!(parked.len(), 1, "the page we left took its place");
        assert_eq!(parked[0].url, "https://a/");
        assert!(bind_parked(&mut kept, &mut parked, "https://a/"));
        assert_eq!(kept.as_ref().unwrap().url, "https://a/");
        assert_eq!(parked[0].url, "https://b/");
    }

    #[test]
    fn parking_the_same_url_twice_keeps_the_newest_page() {
        let mut parked = Vec::new();
        let mut old = fake_page("https://a/");
        old.hover = Some(7); // a marker for "this is the old one"
        park_page(&mut parked, old);
        park_page(&mut parked, fake_page("https://a/"));
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].hover, None, "the newest visit wins");
    }

    #[test]
    fn the_parking_lot_drops_the_oldest_page_past_its_bound() {
        let mut parked = Vec::new();
        for i in 0..=PARKED_MAX {
            park_page(&mut parked, fake_page(&format!("https://{i}/")));
        }
        assert_eq!(parked.len(), PARKED_MAX);
        assert_eq!(parked[0].url, "https://1/");
        assert_eq!(parked[PARKED_MAX - 1].url, format!("https://{}/", PARKED_MAX));
    }

    #[test]
    fn blend_rect_mixes_only_inside_the_rect() {
        let mut buf = vec![0x00_ff_ff_ffu32; 4 * 4];
        blend_rect(&mut buf, 4, 4, 1.0, 1.0, 2.0, 2.0, (0, 0, 0), 0.5);
        assert_eq!(buf[0], 0x00_ff_ff_ff, "outside the rect stays white");
        assert_eq!(buf[5], 0x00_7f_7f_7f, "inside is a 50% mix");
        assert_eq!(buf[15], 0x00_ff_ff_ff);
    }

    #[test]
    fn em_scale_treats_the_css_size_as_the_em_square() {
        // ab_glyph divides `PxScale` by the font's HEIGHT (ascent − descent),
        // not its em, so `PxScale::from(18.0)` laid Fira Sans out at 5/6 of the
        // requested size. `em_scale` must reproduce the em-square advance that
        // a CSS `font-size` means.
        let fonts = Fonts::load();
        let style = Style::new();
        let font = fonts.pick_font_for('A', &style);
        let upem = font.units_per_em().expect("units per em");
        let (size, id) = (18.0f32, font.glyph_id('A'));
        let want = font.h_advance_unscaled(id) * size / upem;
        let got = font.as_scaled(em_scale(&font, size)).h_advance(id);
        assert!((got - want).abs() < 0.01, "got {got} want {want}");
        // the unadjusted scale is measurably different for a font whose
        // ascent−descent ≠ em (every desktop face here)
        let naive = font.as_scaled(PxScale::from(size)).h_advance(id);
        assert!(
            (naive - want).abs() > 0.05,
            "fixture font has height == em; pick another: naive={naive} want={want}"
        );
    }

    #[test]
    fn session_round_trips_caps_and_rejects_garbage() {
        let tabs = vec![
            Tab {
                url: "https://a.example/".into(),
                scroll: 0.0,
            },
            Tab {
                url: "https://b.example/x?y=1&z=2".into(),
                scroll: 0.0,
            },
        ];
        let text = encode_session(&tabs, 1);
        let (back, active) = decode_session(&text).expect("session");
        assert_eq!(active, 1);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].url, "https://a.example/");
        // `=` and `&` in a URL survive verbatim (no escaping in the format)
        assert_eq!(back[1].url, "https://b.example/x?y=1&z=2");
        // an out-of-range index is clamped by the caller, kept as written here
        assert_eq!(
            decode_session("active=9\nhttps://x.example/\n").unwrap().1,
            9
        );
        // nothing usable: missing, blank, or only an active line
        assert!(decode_session("").is_none());
        assert!(decode_session("active=0\n\n\n").is_none());
        // wrong line endings are tolerated
        assert!(decode_session("active=0\r\nhttps://x.example/\r\n").is_some());
        // caps: tab count and per-URL length
        let many: String = (0..200).map(|i| format!("https://e.example/{i}\n")).collect();
        assert_eq!(
            decode_session(&format!("active=0\n{many}")).unwrap().0.len(),
            SESSION_MAX_TABS
        );
        let long = format!("active=0\nhttps://e.example/{}\n", "x".repeat(SESSION_MAX_URL));
        assert!(decode_session(&long).is_none());
        // the write gate changes with the active tab and with any URL
        assert_eq!(session_digest(&tabs, 0), session_digest(&tabs, 0));
        assert_ne!(session_digest(&tabs, 0), session_digest(&tabs, 1));
        let mut moved = vec![
            Tab {
                url: "https://a.example/z".into(),
                scroll: 0.0,
            },
            Tab {
                url: "https://b.example/x?y=1&z=2".into(),
                scroll: 0.0,
            },
        ];
        assert_ne!(session_digest(&tabs, 0), session_digest(&moved, 0));
        moved[0].url.pop();
        assert_eq!(session_digest(&tabs, 0), session_digest(&moved, 0));
    }

    #[test]
    fn glyph_masks_cache_by_subpixel_pen() {
        let fonts = Fonts::load();
        let style = Style::new();
        let scale = PxScale::from(style.size);
        let font = fonts.pick_font_for('A', &style);
        let id = font.glyph_id('A');
        let a = fonts.glyph_mask(&font, id, scale, 12.0, 100.0).unwrap();
        // same subpixel position, different whole-pixel offset → same raster
        let b = fonts
            .glyph_mask(&font, id, scale, 15.0, 107.0)
            .unwrap();
        assert!(Rc::ptr_eq(&a, &b));
        assert!(a.w > 0 && a.h > 0 && a.cov.iter().any(|&c| c > 0));
        // a different 1/4-px pen fraction is a different raster
        let c = fonts.glyph_mask(&font, id, scale, 12.25, 100.0).unwrap();
        assert!(!Rc::ptr_eq(&a, &c));
    }

    #[test]
    fn glyph_cache_blit_matches_cold_render() {
        let fonts = Fonts::load();
        let style = Style::new();
        let (w, h) = (240, 40);
        let mut warm = vec![0x00ff_ffffu32; w * h];
        draw_text(&mut warm, w, 2.0, 4.0, "Hamburgefonstiv 123", &fonts, &style, 255, None);
        fonts.glyphs.borrow_mut().clear(); // cold
        let mut cold = vec![0x00ff_ffffu32; w * h];
        draw_text(&mut cold, w, 2.0, 4.0, "Hamburgefonstiv 123", &fonts, &style, 255, None);
        assert_eq!(warm, cold);
        assert!(warm.iter().any(|&p| p != 0x00ff_ffff), "text drew no ink");
    }

    /// Every non-white pixel of a run painted on a white `w`-wide buffer.
    fn lit(buf: &[u32], w: usize) -> std::collections::HashSet<(i32, i32)> {
        buf.iter()
            .enumerate()
            .filter(|&(_, &p)| p != 0x00ff_ffff)
            .map(|(i, _)| ((i % w) as i32, (i / w) as i32))
            .collect()
    }

    #[test]
    fn text_shadow_paints_the_glyphs_shifted_under_the_ink() {
        let fonts = Fonts::load();
        let (w, h) = (240, 60);
        let base = Style::new();
        let run = |style: &Style| {
            let mut buf = vec![0x00ff_ffffu32; w * h];
            draw_text(&mut buf, w, 2.0, 4.0, "Ag1", &fonts, style, 255, None);
            buf
        };
        let plain = lit(&run(&base), w);
        assert!(!plain.is_empty(), "control drew ink");
        // the shadow's silhouette must be exactly the ink moved by (dx, dy)
        let mut want = plain.clone();
        for (x, y) in &plain {
            want.insert((x + 5, y + 5));
        }
        let layer = |color: u32, current: bool| css::TextShadow {
            dx: 5.0,
            dy: 5.0,
            blur: 0.0,
            color,
            alpha: 255,
            current,
        };
        // a hard shadow is the ink shifted by the offsets, and the glyph ink
        // still paints on top of it
        let mut style = base;
        style.text_shadow = [Some(layer(0xff0000, false)), None];
        let hard = run(&style);
        assert_eq!(lit(&hard, w), want, "shadow = ink shifted by the offsets");
        // the glyph ink paints on top: some pixel ends up dark (a red shadow
        // pixel can never be dark on white, so only the ink can make one)
        assert!(
            hard.iter().any(|&p| {
                p & 0xff < 100 && (p >> 8) & 0xff < 100 && (p >> 16) & 0xff < 100
            }),
            "glyph ink still painted over the shadow"
        );
        // `currentcolor` (no colour given) paints in the run's own colour:
        // the same shifted silhouette, and never the layer's stored colour
        let mut style = base;
        style.color = 0x0000ff;
        style.text_shadow = [Some(layer(0xff0000, true)), None];
        let cur = run(&style);
        assert_eq!(lit(&cur, w), want, "currentcolor shadow painted too");
        assert!(
            !cur.iter().any(|&p| p == 0x00ff_0000),
            "the stored colour is ignored while `current` is set"
        );
        // blur spreads the shadow past the offset silhouette
        let mut style = base;
        style.text_shadow = [Some(css::TextShadow { blur: 8.0, ..layer(0xff0000, false) }), None];
        let blurred = lit(&run(&style), w);
        assert!(
            blurred.len() > want.len(),
            "blur spreads ink: {} vs {}",
            blurred.len(),
            want.len()
        );
    }

    /// Paint-only layout: a red left half behind, then one deferred subtree
    /// rect (the backdrop box's own background) — everything else empty.
    fn bd_page(backdrops: Vec<layout::BackdropGroup>) -> Layout {
        Layout {
            runs: Vec::new(),
            height: 64.0,
            underline_rects: Vec::new(),
            rects: vec![
                // red left half: layout x 0..20 → buffer 12..32 (PADDING)
                (0.0, 0.0, 20.0, 64.0, 0x00ff_0000, 0.0, false),
                // the backdrop box's own opaque rect (buffer 18..22 × 16..20)
                (6.0, 4.0, 4.0, 4.0, 0x0000_ff00, 0.0, false),
            ],
            rect_grad: vec![None, None],
            rect_ring: vec![None, None],
            images: Vec::new(),
            shadows: Vec::new(),
            open_line: false,
            forms: Vec::new(),
            focused: None,
            form_values: Default::default(),
            sticky: Vec::new(),
            backdrops,
            hover_targets: Vec::new(),
        }
    }

    #[test]
    fn backdrop_filter_blurs_the_region_and_paints_the_subtree_last() {
        let (w, h) = (64usize, 64usize);
        let group = layout::BackdropGroup {
            shadows: (0, 0),
            rects: (1, 2), // defers only the box's own rect
            imgs: (0, 0),
            imgs_dec: (0, 0),
            runs: (0, 0),
            uls: (0, 0),
            // region (4, 2, 32, 40) → buffer (16, 14)..(48, 54); it straddles
            // the red/white edge at buffer x = 32
            rect: (4.0, 2.0, 32.0, 40.0),
            radius: 0.0,
            blur: 4.0,
            fixed: false,
        };
        let fonts = Fonts::load();
        let mut buf = vec![0x00ff_ffffu32; w * h];
        render(
            &mut buf,
            w,
            h,
            &bd_page(vec![group]),
            &fonts,
            0.0,
            64.0,
            0.0,
        );
        let at = |x: usize, y: usize| buf[y * w + x];
        // outside the region: the blur never writes there
        assert_eq!(at(14, 40), 0x00ff_0000, "red left of the region stays sharp");
        assert_eq!(at(50, 40), 0x00ff_ffff, "white right of the region stays sharp");
        // inside: the red/white edge at buffer x = 32 got mixed by the blur
        let edge = at(32, 40);
        let gch = (edge >> 8) & 0xff;
        assert!(gch > 0 && gch < 255, "region edge blurred, got {edge:#08x}");
        // region interior far from the edge: a constant survives the blur
        assert_eq!(at(17, 30), 0x00ff_0000, "constant interior unchanged");
        // the deferred subtree rect painted AFTER the blur: exactly its own
        // colour, with no green bled into the pre-blur snapshot
        assert_eq!(at(19, 17), 0x0000_ff00, "own rect sharp on top");
        assert_eq!(at(16, 30), 0x00ff_0000, "own rect absent from the snapshot");
        // gating: the same channels without a group paint the plain picture
        // (sharp edge, phase 2 never runs) — the feature is record-gated
        let mut plain = vec![0x00ff_ffffu32; w * h];
        render(&mut plain, w, h, &bd_page(Vec::new()), &fonts, 0.0, 64.0, 0.0);
        assert_eq!(plain[40 * w + 32], 0x00ff_ffff, "no group → no blur");
        assert_eq!(plain[17 * w + 19], 0x0000_ff00, "own rect still paints");
        assert_eq!(plain[30 * w + 16], 0x00ff_0000, "no snapshot either");
    }


    /// Paint-only layout: one box whose background is 6 % black and whose
    /// border is a 2px ring, both rounded — the translucent-pill case.
    fn ring_page() -> Layout {
        Layout {
            runs: Vec::new(),
            height: 64.0,
            underline_rects: Vec::new(),
            rects: vec![
                // the background: 6 % black over the white page
                (0.0, 0.0, 40.0, 40.0, 0x0f00_0000, 6.0, false),
                // its border ring, 2px of #d2d9d4
                (0.0, 0.0, 40.0, 40.0, 0x00d2_d9d4, 6.0, false),
            ],
            rect_grad: vec![None, None],
            rect_ring: vec![None, Some(2.0)],
            images: Vec::new(),
            shadows: Vec::new(),
            open_line: false,
            forms: Vec::new(),
            focused: None,
            form_values: Default::default(),
            sticky: Vec::new(),
            backdrops: Vec::new(),
            hover_targets: Vec::new(),
        }
    }

    #[test]
    fn a_border_ring_leaves_the_translucent_fill_visible() {
        let (w, h) = (64usize, 64usize);
        let fonts = Fonts::load();
        let mut buf = vec![0x00ff_ffffu32; w * h];
        render(&mut buf, w, h, &ring_page(), &fonts, 0.0, 64.0, 0.0);
        let at = |x: usize, y: usize| buf[y * w + x];
        // interior: 6 % black over white, NOT over the border colour
        assert_eq!(at(32, 32), 0x00f0_f0f0, "interior keeps the page behind");
        // the ring itself paints the border colour
        assert_eq!(at(12, 32), 0x00d2_d9d4, "left ring edge");
        assert_eq!(at(51, 32), 0x00d2_d9d4, "right ring edge");
        assert_eq!(at(32, 12), 0x00d2_d9d4, "top ring edge");
        // a rounded corner: outside the outer radius the page shows, just
        // inside it the ring paints
        assert_eq!(at(12, 12), 0x00ff_ffff, "corner stays page-coloured");
        assert_eq!(at(14, 14), 0x00d2_d9d4, "ring follows the corner arc");
    }

    #[test]
    fn svg_text_paints_ink_over_the_vector_pass() {
        let fonts = Fonts::load();
        let svg = br##"<svg width="120" height="40" font-size="28"><text x="4" y="32" fill="#ff0000">AW</text></svg>"##;
        let mut img = svg::render(svg, 120, 40).expect("vector pass");
        assert!(img.px.iter().all(|&p| p == 0), "the vector pass has no text");
        paint_svg_text(&mut img, &svg::text_runs(svg, 120, 40), &fonts);
        let ink = img.px.iter().filter(|&&p| p != 0).count();
        assert!(ink > 20, "glyphs blitted: {ink} px of ink");
        // red fill survives the ARGB blend, alpha included
        assert!(
            img.px
                .iter()
                .any(|&p| p >> 24 > 0 && (p >> 16) & 0xff > 128 && (p >> 8) & 0xff < 0x80),
            "red glyph pixels"
        );
    }

    #[test]
    fn pixels_to_image_keeps_rgb() {
        let img = pixels_to_image(&[0x0012_3456, 0x00ff_ffff], 2, 1);
        assert_eq!(img.size, [2, 1]);
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(0x12, 0x34, 0x56));
        assert_eq!(img.pixels[1], egui::Color32::WHITE);
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mazda-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn cache_roundtrip_and_ttl() {
        let dir = test_dir("cache-rt");
        cache_write_in(
            &dir,
            "https://x.com/",
            1000,
            CACHE_TTL_SECS,
            b"hello",
            Some("text/html"),
            None,
        );
        // fresh read returns body + content-type
        let hit = cache_read_in(&dir, "https://x.com/").unwrap();
        assert_eq!(
            hit,
            CacheEntry {
                body: b"hello".to_vec(),
                ct: Some("text/html".to_string()),
                etag: None,
                ts: 1000,
                ttl: CACHE_TTL_SECS,
            }
        );
        // still fresh one second before the TTL, stale at it
        assert!(hit.fresh(1000 + CACHE_TTL_SECS - 1));
        assert!(!hit.fresh(1000 + CACHE_TTL_SECS));
        // different URL = miss (FNV key differs)
        assert_eq!(cache_read_in(&dir, "https://y.com/"), None);
        // a response with a year-long max-age outlives the old 5-minute
        // boundary, and its own TTL still expires it
        cache_write_in(&dir, "https://a.com/", 1000, 86_400, b"a", None, None);
        let a = cache_read_in(&dir, "https://a.com/").unwrap();
        assert!(a.fresh(1000 + 20_000));
        assert!(!a.fresh(1000 + 86_400));
        // ttl 0 means "never serve from disk" (no-store / max-age=0)
        cache_write_in(&dir, "https://b.com/", 1000, 0, b"b", None, None);
        assert!(!cache_read_in(&dir, "https://b.com/").unwrap().fresh(1000));
        // an entry from the old single-number format still parses
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(cache_key("https://old.com/")),
            b"1000\ntext/plain\nold",
        )
        .unwrap();
        let old = cache_read_in(&dir, "https://old.com/").unwrap();
        assert_eq!(old.body, b"old");
        assert_eq!(old.ttl, CACHE_TTL_SECS);
        assert!(old.fresh(1000 + 60));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_keeps_the_validator_for_revalidation() {
        let dir = test_dir("cache-etag");
        let tag = "W/\"e45b698b3519d765491286052229fbe2\"";
        // `max-age=0`: never fresh, but the body and the ETag survive so a
        // later 304 can be answered without downloading it again
        cache_write_in(
            &dir,
            "https://x.com/",
            1000,
            0,
            b"doc",
            Some("text/html"),
            Some(tag),
        );
        let hit = cache_read_in(&dir, "https://x.com/").unwrap();
        assert!(!hit.fresh(1000), "max-age=0 is never served from disk");
        assert_eq!(hit.etag.as_deref(), Some(tag));
        assert_eq!(hit.body, b"doc");
        // answering that 304 rewrites the entry with the new timestamp
        cache_write_in(
            &dir,
            "https://x.com/",
            2000,
            0,
            &hit.body,
            hit.ct.as_deref(),
            hit.etag.as_deref(),
        );
        let again = cache_read_in(&dir, "https://x.com/").unwrap();
        assert_eq!((again.ts, again.ttl), (2000, 0));
        assert_eq!(again.etag.as_deref(), Some(tag));
        // no validator stores `-` and reads back as None
        cache_write_in(&dir, "https://n.com/", 1000, 60, b"n", None, None);
        assert_eq!(cache_read_in(&dir, "https://n.com/").unwrap().etag, None);
        // a validator that would break the header line is dropped, not written
        cache_write_in(&dir, "https://w.com/", 1000, 60, b"w", None, Some("bad tag"));
        let w = cache_read_in(&dir, "https://w.com/").unwrap();
        assert_eq!(w.etag, None);
        assert_eq!(w.body, b"w");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_ttl_follows_cache_control() {
        assert_eq!(cache_ttl(None), CACHE_TTL_SECS);
        assert_eq!(
            cache_ttl(Some("public, max-age=31536000, immutable")),
            31_536_000
        );
        assert_eq!(cache_ttl(Some("max-age=60")), 60);
        assert_eq!(cache_ttl(Some("max-age=0, private, must-revalidate")), 0);
        assert_eq!(cache_ttl(Some("no-store")), 0);
        assert_eq!(cache_ttl(Some("no-cache")), 0);
        assert_eq!(cache_ttl(Some("public")), CACHE_TTL_SECS);
        // an absurd max-age is capped instead of pinning a URL forever
        assert_eq!(
            cache_ttl(Some("max-age=99999999999")),
            CACHE_MAX_AGE_CAP
        );
    }

    /// The rejected image-prefetch experiment (SPIKE P65) still exposed a real
    /// hazard: a reader can touch an entry while another lane writes it. A
    /// write goes to a unique temp file and is renamed into place, so a reader
    /// only ever sees a whole entry.
    #[test]
    fn cache_reader_never_sees_a_partial_entry() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let dir = test_dir("cache-race");
        let body = vec![b'x'; 100_000];
        let done = AtomicBool::new(false);
        let reads = AtomicUsize::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..120 {
                    cache_write_in(
                        &dir,
                        "https://race.com/",
                        1000 + i,
                        CACHE_TTL_SECS,
                        &body,
                        Some("text/plain"),
                        None,
                    );
                }
                done.store(true, Ordering::Relaxed);
            });
            while !done.load(Ordering::Relaxed) {
                if let Some(e) = cache_read_in(&dir, "https://race.com/") {
                    assert_eq!(e.body.len(), body.len(), "a read saw a partial body");
                    assert!(e.body.iter().all(|&b| b == b'x'), "torn body");
                    reads.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::yield_now();
            }
        });
        assert!(reads.load(Ordering::Relaxed) > 0, "the reader raced the writer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn brotli_body_decodes_to_the_identity_bytes() {
        // a real githubassets response: 3819 B identity, 1148 B gzip, 1068 B br
        let raw = include_bytes!("../tests/fixtures/rxr.js.br");
        assert_eq!(raw.len(), 1068);
        let out = decode_br(raw).expect("decode a real brotli body");
        assert_eq!(out.len(), 3819, "identity length");
        assert!(
            out.starts_with(b"performance.mark(\"js-parse-end:"),
            "decoded bytes are the bundle"
        );
        // junk is an error, never a panic or a silently empty body
        assert!(decode_br(b"not brotli").is_err());
    }

    #[test]
    fn gzip_body_decodes_to_the_identity_bytes() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"performance.mark(\"x\");").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(decode_gzip(&gz).unwrap(), b"performance.mark(\"x\");");
        // junk is an error, never a panic or a silently empty body
        assert!(decode_gzip(b"not gzip").is_err());
    }

    /// A real 304 from github.com carries `Content-Encoding: gzip` and no
    /// body. Reading that stream as gzip failed with "unexpected end of file";
    /// the body must come from the cache instead.
    #[test]
    fn a_304_answers_from_the_cache_and_ignores_the_content_coding() {
        let dir = test_dir("finish-304");
        cache_write_in(
            &dir,
            "https://x.com/",
            1000,
            0,
            b"doc",
            Some("text/html"),
            Some("\"v1\""),
        );
        let stored = cache_read_in(&dir, "https://x.com/").unwrap();
        let (body, ct) = finish_fetch(
            &dir,
            "https://x.com/",
            2000,
            304,
            Some("text/html".to_string()),
            Some("gzip".to_string()),
            Some(0),
            Some("\"v1\"".to_string()),
            Vec::new(),
            Some(stored),
            Instant::now(),
        )
        .unwrap();
        assert_eq!(body, b"doc", "the stored body answers the 304");
        assert_eq!(ct.as_deref(), Some("text/html"));
        // the entry's clock restarted, so the rest of this load does not ask
        let again = cache_read_in(&dir, "https://x.com/").unwrap();
        assert_eq!((again.ts, again.ttl), (2000, 0));
        assert_eq!(again.etag.as_deref(), Some("\"v1\""));
        // a 304 with nothing stored is an error, not an empty page
        assert!(finish_fetch(
            &dir,
            "https://y.com/",
            2000,
            304,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            Instant::now(),
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_oversized_body_not_stored() {
        let dir = test_dir("cache-big");
        let big = vec![0u8; CACHE_MAX_BODY + 1];
        cache_write_in(&dir, "https://x.com/", 1000, CACHE_TTL_SECS, &big, None, None);
        assert_eq!(cache_read_in(&dir, "https://x.com/"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_stylesheets_in_document_order() {
        let dom = html::parse(
            r#"<head>
                 <link rel="stylesheet" href="/a.css">
                 <link rel="preload stylesheet" href="/b.css">
                 <link rel="icon" href="/f.ico">
                 <style>p { color: red }</style>
               </head>
               <body><link rel="stylesheet" href="/c.css"></body>"#,
        );
        let mut out = Vec::new();
        collect_sheets(&dom, 0, &mut out);
        let hrefs: Vec<&str> = out
            .iter()
            .filter_map(|s| match s {
                SheetSrc::Link(h) => Some(h.as_str()),
                _ => None,
            })
            .collect();
        // rel=icon must NOT count; multi-value rel must count; order preserved
        assert_eq!(hrefs, ["/a.css", "/b.css", "/c.css"]);
        assert!(matches!(out[2], SheetSrc::Inline(_)), "style block in order");
    }

    #[test]
    fn resolves_relative_links() {
        assert_eq!(
            resolve("https://example.com/a/b", "c").as_deref(),
            Some("https://example.com/a/c")
        );
        assert_eq!(
            resolve("https://example.com/a/b", "/root").as_deref(),
            Some("https://example.com/root")
        );
        assert_eq!(
            resolve("https://example.com/a/b", "https://other.x/").as_deref(),
            Some("https://other.x/")
        );
    }

    #[test]
    fn rejects_non_navigable_schemes() {
        assert_eq!(resolve("https://x.com/", "javascript:alert(1)"), None);
        assert_eq!(resolve("https://x.com/", "#section"), None);
        assert_eq!(resolve("https://x.com/", "mailto:a@b.c"), None);
        assert_eq!(resolve("not a url", "relative"), None);
    }

    #[test]
    fn collects_only_download_links() {
        let dom = html::parse(
            r#"<body>
                 <a href="/a.zip" download>one</a>
                 <a href="/b.zip" download="renamed.zip">two</a>
                 <a href="/c.html">plain</a>
                 <a download>no href</a>
                 <a href="javascript:void(0)" download>bad scheme</a>
               </body>"#,
        );
        let d = collect_downloads(&dom, "https://example.com/dir/page.html");
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].href, "https://example.com/a.zip");
        assert_eq!(d[0].name, "", "empty download attr -> derive from URL");
        assert_eq!(d[1].href, "https://example.com/b.zip");
        assert_eq!(d[1].name, "renamed.zip");
    }

    #[test]
    fn download_names_cannot_escape_the_temp_dir() {
        assert_eq!(download_name("https://x/a/b.mp4?x=1", ""), "b.mp4");
        assert_eq!(download_name("https://x/", ""), "download");
        // a hostile `download` value loses every path separator and dot edge
        let n = download_name("https://x/f", "../../evil\\name.zip");
        assert!(!n.contains('/') && !n.contains('\\'), "{n}");
        assert!(!n.starts_with('.'), "{n}");
        // empty/blank suggestions fall back
        assert_eq!(download_name("https://x/", "   "), "download");
        assert_eq!(download_name("https://x/", ".."), "download");
    }

    #[test]
    fn latin_stays_on_primary_font() {
        let fonts = Fonts::load();
        let primary = fonts.pick(&Style::new());
        for ch in "Hello, world! 123".chars() {
            assert!(
                std::ptr::eq(fonts.pick_font_for(ch, &Style::new()).ptr(), primary),
                "char {ch:?} should not trigger fallback"
            );
        }
        // Thai has no fallback font — must not even consult one (costs nothing,
        // stays .notdef as before)
        assert!(
            std::ptr::eq(fonts.pick_font_for('ก', &Style::new()).ptr(), primary),
            "Thai should stay on primary"
        );
    }

    #[test]
    fn pick_font_for_honors_stack_order() {
        let mut fonts = Fonts::load();
        // unknown family + generic → walk continues to the generic fallback
        fonts.stacks.borrow_mut().push(vec![
            "definitely-not-a-font-x".to_string(),
            "serif".to_string(),
        ]);
        let mut st = Style::new();
        st.fam_stack = 1;
        assert!(std::ptr::eq(
            fonts.pick_font_for('R', &st).ptr(),
            fonts.pick(&st)
        ));
        // an installed alias in the stack beats the generic pick (HN: Verdana)
        fonts.stacks.borrow_mut().push(vec!["verdana".to_string()]);
        st.fam_stack = 2;
        let f = fonts.pick_font_for('R', &st);
        assert!(
            !std::ptr::eq(f.ptr(), fonts.pick(&st)),
            "verdana should win over the generic primary"
        );
        assert!(f.glyph_id('R').0 != 0);
    }

    #[test]
    fn webfont_face_wins_over_generic() {
        let mut fonts = Fonts::load();
        // simulate an already-declared face: the slot is pre-filled because
        // loading goes through the network (decode path covered in woff tests)
        let pre = try_load_font(FONT_REGULAR).unwrap();
        fonts.faces.push(PageFace {
            family: "testface".to_string(),
            wmin: 400,
            wmax: 400,
            italic: false,
            srcs: vec![],
            font: std::cell::RefCell::new(Some(Rc::new(pre))),
            bytes: std::cell::Cell::new(0),
            last_use: std::cell::Cell::new(0),
            failed: std::cell::Cell::new(false),
        });
        fonts.stacks.borrow_mut().push(vec!["testface".to_string()]);
        let mut st = Style::new();
        st.fam = Fam::Serif; // generic pick would be Georgia
        st.fam_stack = 1;
        let f = fonts.pick_font_for('R', &st);
        let cached = fonts.faces[0].font.borrow();
        assert!(
            std::ptr::eq(f.ptr(), cached.as_ref().unwrap().as_ref()),
            "the page's webfont wins over the generic"
        );
    }

    #[test]
    fn webfont_fetch_failure_falls_through_stack() {
        let mut fonts = Fonts::load();
        // no usable src: offline-deterministic stand-in for "every fetch/decode
        // failed" (the container fallbacks themselves are covered in woff.rs)
        fonts.faces.push(PageFace {
            family: "ghost".to_string(),
            wmin: 400,
            wmax: 400,
            italic: false,
            srcs: vec![],
            font: std::cell::RefCell::new(None),
            bytes: std::cell::Cell::new(0),
            last_use: std::cell::Cell::new(0),
            failed: std::cell::Cell::new(false),
        });
        fonts.stacks.borrow_mut().push(vec!["ghost".to_string()]);
        let mut st = Style::new();
        st.fam_stack = 1;
        // nowhere to load from → old behaviour: the primary generic pick
        assert!(std::ptr::eq(
            fonts.pick_font_for('R', &st).ptr(),
            fonts.pick(&st)
        ));
        // and the dead face is negative-cached (no per-glyph retry)
        assert!(fonts.faces[0].failed.get());
    }

    /// The per-page budget: oldest-picked face is evicted first once a new
    /// decode would exceed the cap; a face larger than the whole cap is
    /// skipped and negative-cached (system fallback).
    #[test]
    fn webfont_budget_evicts_least_recently_used() {
        let mut fonts = Fonts::load();
        let base = try_load_font(FONT_REGULAR).unwrap();
        let size = base.as_slice().len();
        assert!(size > 0);
        // room for exactly two faces (same size, so the arithmetic is tight)
        fonts.webfont_cap = size * 2 + 1;
        for family in ["one", "two", "three"] {
            fonts.faces.push(PageFace {
                family: family.to_string(),
                wmin: 400,
                wmax: 400,
                italic: false,
                srcs: vec![],
                font: std::cell::RefCell::new(None),
                bytes: std::cell::Cell::new(0),
                last_use: std::cell::Cell::new(0),
                failed: std::cell::Cell::new(false),
            });
        }
        let dup = || FontVec::try_from_vec(base.as_slice().to_vec()).unwrap();
        fonts.cache_webfont(0, dup());
        fonts.cache_webfont(1, dup());
        assert_eq!(fonts.webfont_bytes.get(), size * 2);
        // touch 0 so face 1 becomes the oldest, then a third face must evict it
        fonts.touch_webfont(0);
        fonts.cache_webfont(2, dup());
        assert!(fonts.faces[0].font.borrow().is_some(), "recent face kept");
        assert!(fonts.faces[1].font.borrow().is_none(), "oldest evicted");
        assert!(fonts.faces[2].font.borrow().is_some(), "new face cached");
        assert_eq!(fonts.webfont_bytes.get(), size * 2, "accounting stays exact");

        // a face bigger than the entire cap is skipped (never breaks the cap)
        let mut small = Fonts::load();
        small.webfont_cap = size - 1;
        small.faces.push(PageFace {
            family: "huge".to_string(),
            wmin: 400,
            wmax: 400,
            italic: false,
            srcs: vec![],
            font: std::cell::RefCell::new(None),
            bytes: std::cell::Cell::new(0),
            last_use: std::cell::Cell::new(0),
            failed: std::cell::Cell::new(false),
        });
        small.cache_webfont(0, dup());
        assert!(small.faces[0].font.borrow().is_none());
        assert!(small.faces[0].failed.get(), "oversize verdict is remembered");
        assert_eq!(small.webfont_bytes.get(), 0);
    }

    /// B (system fonts only) must register NO face defs, so the lazy loader
    /// is unreachable: no webfont fetch, decode or cache at all.
    #[test]
    fn fonts_b_skips_webfont_loading_entirely() {
        let mut fonts = Fonts::load();
        fonts.stacks.borrow_mut().push(vec!["stale-from-previous-page".into()]);
        fonts.set_choice(FontsChoice::B);
        fonts.begin_page(vec![css::FontFace {
            family: "siteface".into(),
            srcs: vec!["https://invalid.invalid/font.woff2".into()],
            wmin: 400,
            wmax: 700,
            italic: false,
        }]);
        assert!(fonts.faces.is_empty(), "B keeps no @font-face defs");
        // stack table reset before the early return (no stale leak)
        assert_eq!(fonts.stacks.borrow().len(), 1, "only the id-0 slot");
        // and the picker falls straight through to system fonts
        assert!(std::ptr::eq(
            fonts.pick_font_for('R', &Style::new()).ptr(),
            fonts.pick(&Style::new())
        ));
        // A registers the same def (lazily; no network call until a glyph) —
        // except in a build compiled without the WOFF2 decoder, where the
        // whole webfont path is skipped (release builds enable the feature)
        fonts.set_choice(FontsChoice::A);
        fonts.begin_page(vec![css::FontFace {
            family: "siteface".into(),
            srcs: vec![],
            wmin: 400,
            wmax: 700,
            italic: false,
        }]);
        #[cfg(feature = "woff2")]
        assert_eq!(fonts.faces.len(), 1);
        #[cfg(not(feature = "woff2"))]
        assert!(fonts.faces.is_empty(), "feature-off build fetches no webfonts");
    }

    /// The policy is carried by a fresh navigation only: relayout/event/scroll
    /// jobs return None, so a settings change never mutates the live page.
    #[test]
    fn fonts_policy_applies_on_next_navigation_only() {
        let load = LoadReq::Load {
            id: 1,
            url: "https://x/".into(),
            max_w: 100.0,
            vh: 100.0,
            engine: EngineChoice::A,
            fonts: FontsChoice::B,
            restore: false,
        };
        assert_eq!(load.fonts_choice(), Some(FontsChoice::B));
        let re = LoadReq::Relayout {
            id: 2,
            url: "https://x/".into(),
            max_w: 100.0,
            vh: 100.0,
            forms: layout::FormState::default(),
            hover: None,
            engine: EngineChoice::A,
        };
        assert_eq!(re.fonts_choice(), None);
        assert_eq!(LoadReq::Scroll { url: "https://x/".into(), y: 0.0 }.fonts_choice(), None);
    }

    #[test]
    fn cjk_fallback_finds_real_glyphs() {
        let fonts = Fonts::load();
        // Chinese + kana via CJK slot, Hangul via hangul slot (or primary if it has it)
        for ch in ['中', 'あ', '한'] {
            let f = fonts.pick_font_for(ch, &Style::new());
            assert_ne!(f.glyph_id(ch).0, 0, "no glyph found anywhere for {ch:?}");
        }
        // measure must agree with draw: real advance width, not notdef
        let w = fonts.measure("中文テスト", &Style::new());
        assert!(w > 30.0, "expected ~5 full-width glyphs, got {w}");
        assert_eq!(fonts.measure("abc", &Style::new()) > 10.0, true);
    }

    #[test]
    fn list_marker_glyphs_are_present_and_measure_non_zero() {
        // the synthesized markers (layout.rs marker_text) draw disc/circle/
        // square from these code points; a font without one would silently
        // render .notdef at zero advance, so pin the coverage we rely on
        let fonts = Fonts::load();
        for ch in ['\u{2022}', '\u{25e6}', '\u{25aa}'] {
            let f = fonts.pick_font_for(ch, &Style::new());
            assert_ne!(f.glyph_id(ch).0, 0, "no glyph for {ch:?}");
            assert!(
                fonts.measure(&ch.to_string(), &Style::new()) > 0.0,
                "{ch:?} measures zero"
            );
        }
    }

    #[test]
    fn nav_gate_only_latest_wins() {
        let mut g = NavGate::new();
        let a = g.begin();
        assert!(g.accept(a), "first request is the latest");
        let b = g.begin();
        assert!(!g.accept(a), "older request is stale after a new one");
        assert!(g.accept(b), "newest request wins");
        let c = g.begin();
        assert!(!g.accept(b) && !g.accept(a) && g.accept(c));
    }

    /// The worker→UI channel crosses threads: the wire form must be `Send`
    /// (compile-time check — this is the whole point of `PageWire`).
    #[test]
    fn page_wire_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<LoadReq>();
        assert_send::<LoadDone>();
        assert_send::<PageWire>();
        assert_send::<RunWire>();
    }

    fn sample_wire() -> PageWire {
        let laid = Layout {
            runs: vec![layout::PositionedRun {
                x: 1.0,
                y: 2.0,
                text: "hello".into(),
                style: Style::new(),
                width: 30.0,
                link: Some(std::rc::Rc::from("https://example.com/x")),
                fixed: false,
                clip: None,
                node: None,
                cursor: None,
                alpha: 255,
            }],
            height: 123.0,
            underline_rects: vec![(1.0, 2.0, 3.0, 4.0, 0xff0000, false)],
            rects: vec![(1.0, 2.0, 3.0, 4.0, 0x00ff00, 2.0, true)],
            rect_grad: vec![None],
            rect_ring: vec![None],
            images: vec![(
                5.0,
                6.0,
                7.0,
                8.0,
                std::rc::Rc::new(image::ImageData {
                    w: 1,
                    h: 1,
                    px: vec![0x80445566],
                }),
                false,
                255,
                None,
            )],
            shadows: vec![layout::ShadowRect {
                x: 9.0,
                y: 10.0,
                w: 11.0,
                h: 12.0,
                radius: 1.0,
                blur: 2.0,
                color: 0x33000000,
                fixed: false,
                hole: (0.0, 0.0, 0.0, 0.0),
            }],
            open_line: true,
            forms: Vec::new(),
            focused: None,
            form_values: Default::default(),
            sticky: vec![layout::StickyGroup {
                shadows: (0, 0),
                rects: (0, 1),
                imgs: (0, 0),
                imgs_dec: (0, 0),
                runs: (0, 1),
                uls: (0, 1),
                doc_y: 50.0,
                doc_h: 20.0,
                top: 40.0,
                container_y1: 500.0,
                fixed: false,
            }],
            backdrops: vec![layout::BackdropGroup {
                shadows: (0, 0),
                rects: (0, 1),
                imgs: (0, 0),
                imgs_dec: (0, 0),
                runs: (0, 1),
                uls: (0, 1),
                rect: (1.0, 2.0, 30.0, 14.0),
                radius: 4.0,
                blur: 8.0,
                fixed: false,
            }],
            hover_targets: vec![layout::HoverTarget {
                rect: (1.0, 2.0, 30.0, 14.0),
                node: 7,
                cursor: Some(css::Cursor::Pointer),
                fixed: false,
            }],
        };
        let page = Page {
            url: "https://example.com/".into(),
            laid,
            bg: 0xabcdef,
            media: Vec::new(),
            downloads: Vec::new(),
            elements: HashMap::from([(
                7,
                ElementInfo {
                    tag: "a".into(),
                    id: "x".into(),
                    classes: vec!["y".into()],
                    path: "body > a#x.y".into(),
                    rect: (1.0, 2.0, 30.0, 14.0),
                    fixed: false,
                    attrs: vec![("href".into(), "/".into())],
                    text: "hello".into(),
                    label_for: None,
                },
            )]),
        };
        page_to_wire(
            page,
            vec![Vec::new(), vec!["verdana".into()]],
            vec![css::FontFace {
                family: "test".into(),
                srcs: vec!["https://example.com/f.woff".into()],
                wmin: 400,
                wmax: 700,
                italic: false,
            }],
            HashMap::new(),
        )
    }

    #[test]
    fn wire_roundtrip_preserves_page() {
        let wire = sample_wire();
        let stacks = wire.stacks.clone();
        assert_eq!(stacks.len(), 2, "stack table travels with the page");
        assert_eq!(wire.faces.len(), 1, "@font-face defs travel too");
        let page = wire_to_page(wire);
        assert_eq!(page.url, "https://example.com/");
        assert_eq!(page.bg, 0xabcdef);
        assert_eq!(page.laid.height, 123.0);
        assert_eq!(page.laid.runs.len(), 1);
        let r = &page.laid.runs[0];
        assert_eq!(r.text, "hello");
        assert_eq!(r.link.as_deref(), Some("https://example.com/x"));
        assert_eq!(page.laid.images.len(), 1);
        assert_eq!(page.laid.images[0].4.px, vec![0x80445566]);
        assert_eq!(page.laid.shadows.len(), 1);
        assert_eq!(page.laid.shadows[0].blur, 2.0);
        assert_eq!(page.laid.rects.len(), 1);
        assert_eq!(page.laid.underline_rects.len(), 1);
        // sticky pinning records ride the wire too (render needs them)
        assert_eq!(page.laid.sticky.len(), 1);
        assert_eq!(page.laid.sticky[0].top, 40.0);
        assert_eq!(page.laid.sticky[0].rects, (0, 1));
        // so do the backdrop-filter region records
        assert_eq!(page.laid.backdrops.len(), 1);
        assert_eq!(page.laid.backdrops[0].blur, 8.0);
        assert_eq!(page.laid.backdrops[0].rects, (0, 1));
        // hover hit-test targets ride the wire (the shell has no box tree)
        assert_eq!(page.laid.hover_targets.len(), 1);
        assert_eq!(page.laid.hover_targets[0].node, 7);
        assert_eq!(
            page.laid.hover_targets[0].cursor,
            Some(css::Cursor::Pointer)
        );
        // inspector metadata rides the wire too (the UI thread has no DOM)
        let info = page.elements.get(&7).expect("element metadata travels");
        assert_eq!(info.selector(), "a#x.y");
        assert_eq!(info.attrs[0], ("href".to_string(), "/".to_string()));
    }

    /// The inspector's element identity: selector, ancestor path, box and a
    /// direct-text sample, collected once per hit-test node.
    #[test]
    fn inspector_metadata_describes_hit_test_nodes() {
        let dom = html::parse(
            "<html><body><div id='app' class='wrap main'><p class='lead'>Hello world</p></div></body></html>",
        );
        let mut p = 0u32;
        let mut text = 0u32;
        for i in 0..dom.nodes.len() as u32 {
            match dom.tag(i) {
                "p" => p = i,
                "#text" if text == 0 => text = i,
                _ => {}
            }
        }
        assert!(p != 0 && text != 0);
        let targets = vec![
            layout::HoverTarget {
                rect: (10.0, 20.0, 30.0, 14.0),
                node: p,
                cursor: None,
                fixed: false,
            },
            // a text leaf is not an inspectable element
            layout::HoverTarget {
                rect: (10.0, 20.0, 30.0, 14.0),
                node: text,
                cursor: None,
                fixed: false,
            },
        ];
        let els = collect_elements(&dom, &targets);
        assert_eq!(els.len(), 1, "one entry per element, text leaves skipped");
        let info = els.get(&p).unwrap();
        assert_eq!(info.selector(), "p.lead");
        assert_eq!(info.rect, (10.0, 20.0, 30.0, 14.0));
        assert!(info.path.ends_with("div#app.wrap.main > p.lead"));
        assert_eq!(info.text, "Hello world");
    }

    /// Form interaction state must survive the worker→UI hop: focus, typed
    /// values, laid-out control rects and the node→name table.
    #[test]
    fn wire_roundtrip_preserves_forms() {
        let mut wire = sample_wire();
        wire.forms.push(layout::FormBox {
            node: 7,
            rect: (1.0, 2.0, 30.0, 18.0),
            kind: layout::FormKind::TextInput { password: false },
            focused: true,
            fixed: false,
        });
        wire.focused = Some(7);
        wire.form_values.insert(7, "typed!".into());
        wire.form_names.insert(7, "q".into());
        let focused = wire.focused;
        let names = wire.form_names.clone();
        let page = wire_to_page(wire);
        assert_eq!(page.laid.focused, focused, "focus travels");
        assert_eq!(page.laid.form_value(7), "typed!", "typed value travels");
        assert_eq!(page.laid.forms.len(), 1, "hit-test rects travel");
        assert_eq!(page.laid.forms[0].node, 7);
        assert_eq!(names.get(&7).map(|s| s.as_str()), Some("q"));
    }

    /// Key routing is a pure fn (no synthesized input — verified here):
    /// editing keys map only when a field is focused, and the address bar
    /// always wins when it has focus.
    #[test]
    fn key_routing_pure_fn() {
        use egui::Key;
        // field focused → editing keys route
        assert_eq!(route_key(Key::Backspace, true, false), KeyAction::Backspace);
        assert_eq!(route_key(Key::Enter, true, false), KeyAction::Submit);
        assert_eq!(route_key(Key::Escape, true, false), KeyAction::Blur);
        // no field focus → nothing routes (arrow keys keep scrolling)
        assert_eq!(route_key(Key::Backspace, false, false), KeyAction::None);
        assert_eq!(route_key(Key::Enter, false, false), KeyAction::None);
        // address bar focused → page keys suppressed entirely
        assert_eq!(route_key(Key::Backspace, true, true), KeyAction::None);
        assert_eq!(route_key(Key::Escape, true, true), KeyAction::None);
        // unrelated keys never route
        assert_eq!(route_key(Key::ArrowDown, true, false), KeyAction::None);
        assert_eq!(route_key(Key::F11, true, false), KeyAction::None);
    }

    /// `collect_form_names` reads name attributes off form controls only —
    /// that table is how the shell logs `[form] submit name=value` without
    /// DOM access.
    #[test]
    fn form_names_cover_controls_only() {
        let dom = html::parse(
            r#"<html><body><form>
                <input name="user" type="text">
                <input type="password" no-name-x>
                <textarea name="msg"></textarea>
                <button name="go" type="submit">Go</button>
                <div name="not-a-control"></div>
            </form></body></html>"#,
        );
        let names = collect_form_names(&dom);
        let by_name: std::collections::HashMap<&str, u32> = names
            .iter()
            .map(|(&n, s)| (s.as_str(), n))
            .collect();
        assert!(by_name.contains_key("user"));
        assert!(by_name.contains_key("msg"));
        assert!(by_name.contains_key("go"));
        assert_eq!(names.len(), 3, "non-controls and unnamed inputs excluded");
    }

    /// Successful form-control collection (the worker-side collector):
    /// text/password/textarea values, hidden inputs submit, checked state
    /// comes from the worker DOM's `checked` attribute (the shell's click
    /// toggles carry through), checked-only radios/checkboxes, and
    /// unnamed/file controls never submit. A submit button is a pair only
    /// when it is the activated one. The urlencoded serial then turns
    /// spaces into `+` and escapes `&`/`=` (url crate).
    #[test]
    fn form_pairs_collect_and_encode() {
        let dom = html::parse(
            r#"<html><body><form>
                <input name="user" type="text" value="Ann">
                <input type="hidden" name="csrf" value="t0">
                <textarea name="msg">plain</textarea>
                <input name="cb" type="checkbox" value="yes" checked>
                <input name="cb2" type="checkbox">
                <input name="r1" type="radio" value="a" checked>
                <input name="r2" type="radio" value="b">
                <input name="go" type="submit" value="Go">
                <input name="doc" type="file">
                <input value="no name no submit">
                <input name="sp" value="a b">
                <input name="esc" value="x&y=z">
            </form></body></html>"#,
        );
        let form = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "form")
            .unwrap();
        let pairs = collect_form_pairs(&dom, form, &Default::default(), None);
        let map: HashMap<&str, &str> =
            pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(map.get("user"), Some(&"Ann"));
        assert_eq!(map.get("csrf"), Some(&"t0"), "hidden inputs submit");
        assert_eq!(
            map.get("msg"),
            Some(&"plain"),
            "textarea default = its own text content"
        );
        assert_eq!(map.get("cb"), Some(&"yes"), "checked box submits its value");
        assert_eq!(map.get("r1"), Some(&"a"), "only the checked radio submits");
        assert_eq!(map.get("sp"), Some(&"a b"));
        assert_eq!(map.get("esc"), Some(&"x&y=z"));
        assert!(!map.contains_key("cb2"), "unchecked box never submits");
        assert!(!map.contains_key("r2"), "unchecked radio never submits");
        assert!(
            !map.contains_key("go"),
            "a submit button is a pair only when it is the activated one"
        );
        assert!(!map.contains_key("doc"), "file controls excluded (no picker)");
        assert!(!map.contains_key("anon"), "nameless controls excluded");
        // activating the submit button adds its pair, in document order
        let go = (1..dom.nodes.len() as u32)
            .find(|&i| dom.attr(i, "name") == Some("go"))
            .unwrap();
        let with_go = collect_form_pairs(&dom, form, &Default::default(), Some(go));
        let gmap: HashMap<&str, &str> = with_go
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            gmap.get("go"),
            Some(&"Go"),
            "the activated submitter submits name=value"
        );
        assert_eq!(
            gmap.len(),
            map.len() + 1,
            "activating a submit button adds exactly one pair"
        );
        let ser = urlencode_pairs(&pairs);
        assert!(ser.contains("sp=a+b"), "space urlencodes to +");
        assert!(ser.contains("esc=x%26y%3Dz"), "& and = must escape");

        // a value-less checked checkbox defaults to "on"
        let dom2 = html::parse(
            r#"<html><body><form>
                <input name="cb" type="checkbox" checked>
            </form></body></html>"#,
        );
        let form2 = (1..dom2.nodes.len() as u32)
            .find(|&i| dom2.tag(i) == "form")
            .unwrap();
        assert_eq!(
            collect_form_pairs(&dom2, form2, &Default::default(), None),
            vec![("cb".to_string(), "on".to_string())],
            "checkbox without a value attr submits \"on\""
        );
    }

    /// The UI's live (typed) value store wins over the DOM's `value`
    /// attribute — what the user sees is what submits.
    #[test]
    fn form_pairs_live_value_overrides_dom_attr() {
        let dom = html::parse(
            r#"<html><body><form>
                <input name="q" type="text" value="dom-value">
            </form></body></html>"#,
        );
        let form = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "form")
            .unwrap();
        let node = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "input")
            .unwrap();
        let mut live: HashMap<u32, String> = HashMap::new();
        live.insert(node, "typed".to_string());
        assert_eq!(
            collect_form_pairs(&dom, form, &live, None),
            vec![("q".to_string(), "typed".to_string())],
            "the live typed value replaces the markup default"
        );
    }

    /// Radio/checkbox click semantics: a radio click never unchecks it,
    /// moves the `checked` flag across its whole group (same `name` + same
    /// form owner), and leaves same-name radios in another form alone.
    /// Checkboxes keep the plain toggle.
    #[test]
    fn radio_clicks_move_the_group_and_never_uncheck() {
        let mut dom = html::parse(
            r#"<html><body>
                <form id="a">
                    <input type="radio" name="size" value="s">
                    <input type="radio" name="size" value="m">
                    <input type="radio" name="other" value="x" checked>
                    <input type="checkbox" name="cb" checked>
                </form>
                <form id="b">
                    <input type="radio" name="size" value="s" checked>
                    <input type="radio" name="size" value="l">
                </form>
                <input type="radio" name="loose" value="1">
                <input type="radio" name="loose" value="2">
            </body></html>"#,
        );
        let find = |dom: &html::Dom, pred: &dyn Fn(&html::Dom, u32) -> bool| -> u32 {
            (1..dom.nodes.len() as u32)
                .find(|&n| pred(dom, n))
                .expect("node")
        };
        let size_m = find(&dom, &|d, n| {
            d.attr(n, "name") == Some("size") && d.attr(n, "value") == Some("m")
        });
        let size_s_b = find(&dom, &|d, n| {
            d.attr(n, "name") == Some("size")
                && d.attr(n, "value") == Some("s")
                && d.attr(n, "checked").is_some()
        });
        let other_x = find(&dom, &|d, n| d.attr(n, "name") == Some("other"));
        let cb = find(&dom, &|d, n| d.tag(n) == "input" && d.attr(n, "type") == Some("checkbox"));
        toggle_control(&mut dom, size_m);
        assert!(
            dom.attr(size_m, "checked").is_some(),
            "clicking a radio checks it"
        );
        assert!(
            dom.attr(size_s_b, "checked").is_some(),
            "a same-name radio in ANOTHER form keeps its state"
        );
        toggle_control(&mut dom, size_m);
        assert!(
            dom.attr(size_m, "checked").is_some(),
            "clicking a checked radio never unchecks it"
        );
        // the loose group is scoped to the document, so those two interact
        let loose1 = find(&dom, &|d, n| {
            d.attr(n, "name") == Some("loose") && d.attr(n, "value") == Some("1")
        });
        let loose2 = find(&dom, &|d, n| {
            d.attr(n, "name") == Some("loose") && d.attr(n, "value") == Some("2")
        });
        toggle_control(&mut dom, loose2);
        assert!(dom.attr(loose2, "checked").is_some());
        toggle_control(&mut dom, loose1);
        assert!(dom.attr(loose1, "checked").is_some());
        assert!(
            dom.attr(loose2, "checked").is_none(),
            "form-less radios group per document"
        );
        // unrelated controls are untouched
        assert!(dom.attr(other_x, "checked").is_some());
        toggle_control(&mut dom, cb);
        assert!(
            dom.attr(cb, "checked").is_none(),
            "a checkbox still unchecks on click"
        );
    }

    /// Disabled controls never submit (own attribute or a disabled
    /// `<fieldset>`), and a `<select>` submits its selected option's
    /// `value` - or its label text when the option has no `value`.
    #[test]
    fn form_pairs_skip_disabled_and_submit_the_selected_option() {
        let dom = html::parse(
            r#"<html><body><form>
                <input name="q" value="ok">
                <input name="gone" value="x" disabled>
                <fieldset disabled>
                    <input name="inner" value="y">
                    <textarea name="inner-area">z</textarea>
                </fieldset>
                <select name="lang">
                    <option value="en">English</option>
                    <option value="fr" selected>Francais</option>
                </select>
                <select name="region">
                    <option selected>   Europe
                        wide  </option>
                    <option value="us">US</option>
                </select>
                <select name="off" disabled>
                    <option value="zz" selected>off</option>
                </select>
                <select name="empty"></select>
            </form></body></html>"#,
        );
        let form = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "form")
            .unwrap();
        let pairs = collect_form_pairs(&dom, form, &Default::default(), None);
        let map: HashMap<&str, &str> =
            pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(map.get("q"), Some(&"ok"));
        assert!(!map.contains_key("gone"), "disabled control excluded");
        assert!(
            !map.contains_key("inner") && !map.contains_key("inner-area"),
            "controls inside a disabled fieldset are excluded"
        );
        assert_eq!(
            map.get("lang"),
            Some(&"fr"),
            "the selected option's value attribute submits"
        );
        assert_eq!(
            map.get("region"),
            Some(&"Europe wide"),
            "no value attribute: the option label, whitespace collapsed"
        );
        assert!(!map.contains_key("off"), "a disabled select never submits");
        assert!(
            !map.contains_key("empty"),
            "a select with no options contributes nothing"
        );
        // a checked radio in the same group as a re-clicked one keeps only
        // one successful pair (the click side clears the rest)
        let dom2 = html::parse(
            r#"<html><body><form>
                <select name="s"><option value="a">A</option><option value="b">B</option></select>
            </form></body></html>"#,
        );
        let form2 = (1..dom2.nodes.len() as u32)
            .find(|&i| dom2.tag(i) == "form")
            .unwrap();
        assert_eq!(
            collect_form_pairs(&dom2, form2, &Default::default(), None),
            vec![("s".to_string(), "a".to_string())],
            "no selected option: the first enabled one is implicit"
        );
    }

    /// The submitter of a submission: the clicked submit button, the form's
    /// default button on an implicit (Enter) submission, and nobody for
    /// `form.submit()` - which activates the form itself.
    #[test]
    fn activated_submit_button_adds_its_pair() {
        let dom = html::parse(
            r#"<html><body><form action="/go">
                <input name="q" value="x">
                <button type="submit" name="act" value="save">Save</button>
                <button type="submit" name="other" value="del">Delete</button>
            </form></body></html>"#,
        );
        let find = |name: &str| -> u32 {
            (1..dom.nodes.len() as u32)
                .find(|&i| dom.attr(i, "name") == Some(name))
                .unwrap()
        };
        let sub = build_submission(
            &dom,
            find("act"),
            &Default::default(),
            &Default::default(),
            "https://e.com/",
        )
        .expect("submission");
        assert_eq!(
            std::str::from_utf8(&sub.body).unwrap(),
            "q=x&act=save",
            "the clicked button is included"
        );
        let enter = build_submission(
            &dom,
            find("q"),
            &Default::default(),
            &Default::default(),
            "https://e.com/",
        )
        .expect("submission");
        assert_eq!(
            std::str::from_utf8(&enter.body).unwrap(),
            "q=x&act=save",
            "implicit submission names the form's default (first) submit button"
        );
        let form = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "form")
            .unwrap();
        assert_eq!(
            std::str::from_utf8(
                &build_submission(
                    &dom,
                    form,
                    &Default::default(),
                    &Default::default(),
                    "https://e.com/"
                )
                .unwrap()
                .body
            )
            .unwrap(),
            "q=x",
            "form.submit() activates the form: no submitter pair"
        );
        // a form with no submit button names nobody on Enter either
        let dom3 = html::parse(
            r#"<html><body><form action="/go"><input name="q" value="x"></form></body></html>"#,
        );
        let q3 = (1..dom3.nodes.len() as u32)
            .find(|&i| dom3.attr(i, "name") == Some("q"))
            .unwrap();
        assert_eq!(
            std::str::from_utf8(
                &build_submission(
                    &dom3,
                    q3,
                    &Default::default(),
                    &Default::default(),
                    "https://e.com/"
                )
                .unwrap()
                .body
            )
            .unwrap(),
            "q=x"
        );
        // `<input type=submit>` behaves the same, but a reset button never
        // becomes a pair
        let dom2 = html::parse(
            r#"<html><body><form action="/go">
                <input type="submit" name="go" value="Go">
                <input type="reset" name="wipe" value="Reset">
            </form></body></html>"#,
        );
        let go = (1..dom2.nodes.len() as u32)
            .find(|&i| dom2.attr(i, "name") == Some("go"))
            .unwrap();
        assert_eq!(
            std::str::from_utf8(
                &build_submission(
                    &dom2,
                    go,
                    &Default::default(),
                    &Default::default(),
                    "https://e.com/"
                )
                .unwrap()
                .body
            )
            .unwrap(),
            "go=Go"
        );
        let wipe = (1..dom2.nodes.len() as u32)
            .find(|&i| dom2.attr(i, "name") == Some("wipe"))
            .unwrap();
        assert_eq!(
            std::str::from_utf8(
                &build_submission(
                    &dom2,
                    wipe,
                    &Default::default(),
                    &Default::default(),
                    "https://e.com/"
                )
                .unwrap()
                .body
            )
            .unwrap(),
            "",
            "a reset button never submits"
        );
    }

    /// A `<label>` names its control by `for=` or as its first labelable
    /// descendant, and every hit-test element inside it carries that
    /// control in `ElementInfo.label_for` - which is what the shell's click
    /// path uses to turn a click on label text into a click on the control.
    #[test]
    fn labels_resolve_their_control_for_clicks() {
        let dom = html::parse(
            r#"<html><body><form>
                <label for="cb1">Agree</label>
                <input id="cb1" type="checkbox">
                <label>Pick <span>one</span><input name="w" type="radio"></label>
                <label for="missing">Broken</label>
                <label>Just text</label>
            </form></body></html>"#,
        );
        let find = |pred: &dyn Fn(&html::Dom, u32) -> bool| -> u32 {
            (1..dom.nodes.len() as u32)
                .find(|&i| pred(&dom, i))
                .expect("node")
        };
        let agree = find(&|d, n| d.tag(n) == "label" && d.attr(n, "for") == Some("cb1"));
        let cb1 = find(&|d, n| d.attr(n, "id") == Some("cb1"));
        assert_eq!(label_control(&dom, agree), Some(cb1), "for= names the control");
        let wrapped = find(&|d, n| {
            d.tag(n) == "label" && d.attr(n, "for").is_none() && d.attr(n, "name").is_none()
                && d.nodes[n as usize]
                    .children
                    .iter()
                    .any(|&c| d.tag(c) == "span")
        });
        let radio = find(&|d, n| d.attr(n, "name") == Some("w"));
        assert_eq!(
            label_control(&dom, wrapped),
            Some(radio),
            "no for=: the first labelable descendant"
        );
        let span = find(&|d, n| d.tag(n) == "span");
        assert_eq!(label_ancestor(&dom, span), Some(wrapped));
        let broken = find(&|d, n| d.tag(n) == "label" && d.attr(n, "for") == Some("missing"));
        assert_eq!(
            label_control(&dom, broken),
            None,
            "a dangling for= with no control names nobody"
        );
        // the shell-facing form: every element inside the label maps to the
        // control, including nested inline children
        let targets = vec![
            layout::HoverTarget {
                rect: (0.0, 0.0, 10.0, 10.0),
                node: span,
                cursor: None,
                fixed: false,
            },
            layout::HoverTarget {
                rect: (0.0, 0.0, 40.0, 14.0),
                node: agree,
                cursor: None,
                fixed: false,
            },
        ];
        let els = collect_elements(&dom, &targets);
        assert_eq!(
            els.get(&span).unwrap().label_for,
            Some(radio),
            "nested label content still activates the control"
        );
        assert_eq!(
            els.get(&agree).unwrap().label_for,
            Some(cb1),
            "the label element itself carries its control"
        );
    }

    /// GET URL build: the urlencoded data set REPLACES the target's query
    /// and the fragment is dropped (spec mutation of the parsed action).
    #[test]
    fn form_get_url_replaces_the_whole_query() {
        assert_eq!(
            form_get_url("https://e.com/s?old=1&older=2#frag", "q=2").unwrap(),
            "https://e.com/s?q=2",
            "old query and fragment do not survive a GET submission"
        );
        assert_eq!(
            form_get_url("https://e.com/s?q=1", "").unwrap(),
            "https://e.com/s",
            "empty data set clears the query entirely"
        );
    }

    /// Action resolution: no action = the current document URL (fragment
    /// dropped), relative actions join the base, same-document targets are
    /// legal (unlike link resolution!), and non-loadable schemes are refused.
    #[test]
    fn resolve_form_target_rules() {
        assert_eq!(
            resolve_form_target("https://e.com/pg#top", None).as_deref(),
            Some("https://e.com/pg"),
        );
        assert_eq!(
            resolve_form_target("https://e.com/pg", Some("sub/x?f=1#z")).as_deref(),
            Some("https://e.com/sub/x?f=1"),
        );
        assert_eq!(
            resolve_form_target("https://e.com/pg", Some("/pg#self")).as_deref(),
            Some("https://e.com/pg"),
            "same-document actions are legal for forms",
        );
        assert_eq!(
            resolve_form_target("https://e.com/pg", Some("javascript:alert(1)")).is_some(),
            false,
            "non-http(s)/data schemes cannot route through the worker"
        );
    }

    /// build_submission (pure): finds the enclosing `<form>`, picks the
    /// method, and builds the target — GET mutates the action with the
    /// data query, POST keeps the action untouched; either stage's veto
    /// (the pure `submit_decision`) cancels before anything is built.
    #[test]
    fn build_submission_get_post_and_decision() {
        let dom = html::parse(
            r#"<html><body>
            <form action="https://e.com/find"><input name="q" type="text" value="big cat"></form>
            <form action="https://e.com/save" method="post"><input name="t" type="text" value="a&b"></form>
            </body></html>"#,
        );
        let node_with = |name: &str| {
            (1..dom.nodes.len() as u32)
                .find(|&i| dom.attr(i, "name") == Some(name))
                .unwrap()
        };
        let sub = build_submission(
            &dom,
            node_with("q"),
            &Default::default(),
            &Default::default(),
            "https://src.com/",
        )
        .unwrap();
        assert_eq!(sub.method, SubmitMethod::Get);
        assert_eq!(sub.target, "https://e.com/find?q=big+cat");
        assert_eq!(std::str::from_utf8(&sub.body).unwrap(), "q=big+cat");
        // the second form: POST target untouched, body urlencoded; also
        // proves form_ancestor picked the RIGHT enclosing form
        let sub = build_submission(
            &dom,
            node_with("t"),
            &Default::default(),
            &Default::default(),
            "https://src.com/",
        )
        .unwrap();
        assert_eq!(sub.method, SubmitMethod::Post);
        assert_eq!(sub.target, "https://e.com/save");
        assert_eq!(std::str::from_utf8(&sub.body).unwrap(), "t=a%26b");
        // no enclosing form / no activation → no submission at all
        assert!(build_submission(
            &dom,
            0,
            &Default::default(),
            &Default::default(),
            "https://src.com/"
        )
        .is_none());

        // the decision helper both stages share: a preventDefault anywhere
        // cancels the not-yet-built request
        assert_eq!(submit_decision(false), SubmitDecision::Submit);
        assert_eq!(submit_decision(true), SubmitDecision::Cancel);
    }

    /// A picked file turns a POST into `multipart/form-data`: text pairs keep
    /// their names, the file part carries the user-visible filename (escaped),
    /// and a file input with no pick contributes nothing.
    #[test]
    fn file_pick_makes_a_multipart_post() {
        let dom = html::parse(
            r#"<html><body><form action="/up" method="post">
                <input name="note" value="hi">
                <input type="file" name="doc">
                <input type="file" name="none">
            </form></body></html>"#,
        );
        let node_with = |name: &str| {
            (1..dom.nodes.len() as u32)
                .find(|&i| dom.attr(i, "name") == Some(name))
                .unwrap()
        };
        let path = std::env::temp_dir().join(format!(
            "mazda-upload-test-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"file bytes").unwrap();
        let mut files = HashMap::new();
        files.insert(
            node_with("doc"),
            layout::FilePick {
                name: "a\"b.txt".into(),
                path: path.to_string_lossy().into_owned(),
            },
        );
        let sub = build_submission(
            &dom,
            node_with("note"),
            &Default::default(),
            &files,
            "https://e.com/",
        )
        .unwrap();
        assert_eq!(sub.method, SubmitMethod::Post);
        assert_eq!(sub.target, "https://e.com/up");
        assert!(
            sub.content_type.starts_with("multipart/form-data; boundary="),
            "content type names the boundary: {}",
            sub.content_type
        );
        let text = String::from_utf8_lossy(&sub.body);
        assert!(text.contains("name=\"note\"\r\n\r\nhi\r\n"), "{text}");
        assert!(text.contains("filename=\"a_b.txt\""), "quotes escaped: {text}");
        assert!(text.contains("file bytes"));
        assert!(text.ends_with("--\r\n"), "closing boundary last");
        assert!(!text.contains("name=\"none\""), "no pick, no part");
        let _ = std::fs::remove_file(&path);
    }

    /// `enctype=multipart/form-data` alone is enough: a text-only form still
    /// posts multipart, the way a browser does.
    #[test]
    fn enctype_multipart_without_a_file() {
        let dom = html::parse(
            r#"<html><body><form action="/u" method="post" enctype="multipart/form-data">
                <input name="a" value="1">
            </form></body></html>"#,
        );
        let n = (1..dom.nodes.len() as u32)
            .find(|&i| dom.attr(i, "name") == Some("a"))
            .unwrap();
        let sub = build_submission(
            &dom,
            n,
            &Default::default(),
            &Default::default(),
            "https://e.com/",
        )
        .unwrap();
        assert!(sub.content_type.starts_with("multipart/form-data; boundary="));
        let text = String::from_utf8_lossy(&sub.body);
        assert!(text.contains("name=\"a\"\r\n\r\n1\r\n"), "{text}");
    }

    /// The worker's real submission stage through `process_job` (no network
    /// here): a `submit` listener's `preventDefault` vetoes the build — no
    /// navigation, no request (the veto fires before any pair is sent) —
    /// and an activation with no enclosing `<form>` submits nothing either.
    /// Both fall through to the plain post-event relayout.
    #[test]
    fn submit_event_veto_and_no_form_paths() {
        let mut dom = html::parse(
            r#"<html><body><form id="f" action="http://127.0.0.1:1/unreachable" method="get">
                <input name="q" type="text" value="v">
                <button id="b" type="submit">Go</button>
            </form>
            <script>
                var f = document.getElementById("f");
                f.addEventListener("submit", function (e) { e.preventDefault(); });
            </script></body></html>"#,
        );
        let mut scripts = runtime::make_runtime(EngineChoice::A, &mut dom, "http://src/");
        scripts.run_scripts(&mut dom);
        let mut kept = Some(KeptPage {
            url: "http://src/".into(),
            dom,
            css_text: String::new(),
            faces: Vec::new(),
            scripts: Some(scripts),
            engine: EngineChoice::A,
            hover: None,
            imgs: ImgCache::default(),
            sheet: None,
            failed: false,
        });
        let mut fonts = Fonts::load();
        let mut last_geom = (0.0_f32, 0.0_f32);
        let mut last_forms = layout::FormState::default();
        // the kept page's DOM is built again after run_scripts; find the
        // button node from the live tree
        let btn = {
            let d = &kept.as_ref().unwrap().dom;
            (1..d.nodes.len() as u32).find(|&i| d.tag(i) == "button").unwrap()
        };
        let (wire, nav, _pd, panicked, nav_done) =
            process_job(&mut kept, &mut fonts, &LoadReq::Event {
                id: 1,
                url: "http://src/".into(),
                max_w: 100.0,
                vh: 100.0,
                node: btn,
                kinds: vec!["click".into()],
                value: None,
                key: None,
                href: None,
                submit: true,
                forms: layout::FormState::default(),
                engine: EngineChoice::A,
            }, &mut last_geom, &mut last_forms, None);
        assert!(nav.is_none(), "vetoed submission must not navigate");
        assert!(!nav_done, "no worker-executed navigation on veto");
        assert!(!panicked);
        assert!(wire.is_some(), "veto falls through to the plain relayout");

        // activation with no enclosing <form>: nothing to submit, and the
        // field's change/input listeners still run (the pre-submission code
        // paths are untouched by the submission stage)
        let mut dom2 = html::parse(
            r#"<html><body><p>
                <input name="loose" type="text" value="x">
            </p></body></html>"#,
        );
        let mut kept2 = {
            let mut scripts = runtime::make_runtime(EngineChoice::A, &mut dom2, "http://src/");
            scripts.run_scripts(&mut dom2);
            Some(KeptPage {
                url: "http://src/".into(),
                dom: dom2,
                css_text: String::new(),
                faces: Vec::new(),
                scripts: Some(scripts),
                engine: EngineChoice::A,
                hover: None,
                imgs: ImgCache::default(),
                sheet: None,
                failed: false,
            })
        };
        let mut fonts2 = Fonts::load();
        let input = {
            let d = &kept2.as_ref().unwrap().dom;
            (1..d.nodes.len() as u32).find(|&i| d.tag(i) == "input").unwrap()
        };
        let (wire2, nav2, _pd2, panicked2, nav_done2) =
            process_job(&mut kept2, &mut fonts2, &LoadReq::Event {
                id: 2,
                url: "http://src/".into(),
                max_w: 100.0,
                vh: 100.0,
                node: input,
                kinds: vec!["click".into()],
                value: None,
                key: None,
                href: None,
                submit: true,
                forms: layout::FormState::default(),
                engine: EngineChoice::A,
            }, &mut last_geom, &mut last_forms, None);
        assert!(nav2.is_none() && !nav_done2 && !panicked2 && wire2.is_some(),
            "formless activation = plain relayout");
    }

    /// A preload burst and the sequential phase that follows ask for the same
    /// URLs: the second caller must join the download in flight, not start a
    /// second one. Eight threads, one request.
    #[test]
    fn coalesce_fetch_runs_one_request_per_url() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let out: Vec<Vec<u8>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        coalesce_fetch("https://e.com/one", || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            Ok((vec![7u8], None))
                        })
                        .map(|(b, _)| b)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap().unwrap()).collect()
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one download, not eight");
        assert!(out.iter().all(|b| b == &vec![7u8]), "every caller got the bytes");
        // the slot is gone once answered: the next caller runs its own request
        coalesce_fetch("https://e.com/one", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((vec![8u8], None))
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The markup's own `<img>` sources are fetched on a side thread while the
    /// scripts run, so they must be the URLs layout will ask for: resolved
    /// against the page, and no round trip for inline `data:`/SVG sources.
    #[test]
    fn image_urls_resolve_markup_sources_and_skip_inline_ones() {
        let dom = html::parse(
            r#"<html><body>
            <img src="a.png"><img src="/b.png">
            <img src="data:image/png;base64,AA"><img>
            <svg><image href="c.png"/></svg>
            </body></html>"#,
        );
        assert_eq!(
            image_urls(&dom, "https://e.com/x/y"),
            vec![
                "https://e.com/x/a.png".to_string(),
                "https://e.com/b.png".to_string()
            ]
        );
    }

    /// `<video>` and its `<source>` children name the URLs the media probe
    /// will ask for; the early warm must collect the same ones.
    #[test]
    fn media_urls_collect_video_and_source_children() {
        let dom = html::parse(
            r#"<html><body>
            <video src="v.mp4"></video>
            <video><source src="a.webm"><source src="/b.mp4"></video>
            <img src="not-media.png">
            </body></html>"#,
        );
        assert_eq!(
            media_urls(&dom, "https://e.com/x/y"),
            vec![
                "https://e.com/x/v.mp4".to_string(),
                "https://e.com/x/a.webm".to_string(),
                "https://e.com/b.mp4".to_string()
            ]
        );
    }

    /// Progressive first paint: the frame the worker pushes before the scripts
    /// run must show the markup as written. A cold load spends seconds in
    /// script execution, and a blank window for all of it reads as broken.
    #[test]
    fn first_paint_shows_the_markup_before_scripts_add_to_it() {
        if !first_paint_on() {
            return; // A/B run with MAZDA_FIRSTPAINT=0 in the environment
        }
        let dom = html::parse(
            r#"<html><body><h1 id="t">markup</h1>
            <script>document.getElementById("t").setTextContent("scripted");</script>
            </body></html>"#,
        );
        let text = |w: &PageWire| {
            w.runs
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let sink = |wire: PageWire| seen.borrow_mut().push(text(&wire));
        let mut kept = None;
        let mut fonts = Fonts::load();
        let (final_wire, _, _) = settle_load(
            &mut kept,
            &mut fonts,
            "http://src/",
            800.0,
            600.0,
            EngineChoice::A,
            dom,
            String::new(),
            Vec::new(),
            Some(&sink),
        );
        let seen = seen.into_inner();
        assert_eq!(seen.len(), 1, "one early frame per load");
        assert!(
            seen[0].contains("markup") && !seen[0].contains("scripted"),
            "early frame carries the pre-script DOM: {:?}",
            seen[0]
        );
        let final_wire = final_wire.expect("the final page is still pushed");
        assert!(
            text(&final_wire).contains("scripted"),
            "the final page carries the script's mutation: {:?}",
            text(&final_wire)
        );
    }

    /// Worker coalescing collapses superseded relayouts but must never drop
    /// a form Event (it carries a user action).
    #[test]
    fn loadreq_accessors_and_event_identity() {
        let ev = LoadReq::Event {
            id: 3,
            url: "https://example.com/".into(),
            max_w: 100.0,
            vh: 200.0,
            node: 9,
            kinds: vec!["keydown".into(), "input".into()],
            value: None,
            key: None,
            href: None,
            submit: false,
            forms: layout::FormState::default(),
            engine: EngineChoice::A,
        };
        let re = LoadReq::Relayout {
            id: 4,
            url: "https://example.com/".into(),
            max_w: 100.0,
            vh: 200.0,
            forms: layout::FormState::default(),
            hover: None,
            engine: EngineChoice::A,
        };
        let sc = LoadReq::Scroll { url: "https://example.com/".into(), y: 12.0 };
        assert_eq!(ev.id(), 3);
        assert_eq!(re.id(), 4);
        assert_eq!(sc.id(), 0, "scroll never gates a result");
        assert_eq!(ev.url(), "https://example.com/");
        assert!(matches!(ev, LoadReq::Event { .. }));
        assert!(!matches!(re, LoadReq::Event { .. }));
    }

    /// Script errors and form events stay `Send` across the channel after
    /// the enum refactor (compile-time, like `page_wire_is_send`).
    #[test]
    fn loadreq_is_send_variants() {
        fn assert_send<T: Send>() {}
        assert_send::<LoadReq>();
        let _ = LoadReq::Load {
            id: 1,
            url: "https://example.com/".into(),
            max_w: 1.0,
            vh: 1.0,
            engine: EngineChoice::A,
            fonts: FontsChoice::A,
            restore: false,
        };
    }

    #[test]
    fn fetch_timeouts_are_bounded() {
        assert!(CONNECT_TIMEOUT.as_secs() > 0);
        assert!(READ_TIMEOUT.as_secs() > 0);
        assert!(WRITE_TIMEOUT.as_secs() > 0);
        // an agent builds with them (guards against API drift)
        let _ = http_agent();
    }

    #[test]
    fn fallback_coverage_report() {
        let msyh = try_load_font(CJK_FONTS).unwrap();
        let mal = try_load_font(HANGUL_FONTS);
        // U+4E2D han, U+3042 hiragana, U+D55C hangul syllable, U+0E01 thai (control)
        for (label, ch) in [("han", '\u{4E2D}'), ("kana", '\u{3042}'), ("hangul", '\u{D55C}'), ("thai", '\u{0E01}')] {
            eprintln!("msyhl {label}: gid={}", msyh.glyph_id(ch).0);
        }
        if let Some(m) = mal {
            eprintln!("malgun hangul: gid={}", m.glyph_id('\u{D55C}').0);
        }
        // the control: neither fallback may claim Thai
        assert_eq!(msyh.glyph_id('\u{0E01}').0, 0, "yahei should not have Thai");
    }

    // ---- js-event-loop wave: worker scheduler + event coverage ----

    /// preventDefault cancel-navigation decision (pure): the UI navigates
    /// only when listeners came back clean, and only while a decision is
    /// actually pending.
    #[test]
    fn click_decision_cancels_or_navigates() {
        let pending = Some((7u64, "https://example.com/x".to_string()));
        assert!(
            matches!(
                click_decision(&pending, true),
                LinkDecision::Cancel
            ),
            "preventDefault must cancel the not-yet-started navigation"
        );
        assert_eq!(
            click_decision(&pending, false),
            LinkDecision::Navigate("https://example.com/x".to_string()),
            "clean listeners → start the navigation"
        );
        assert!(
            matches!(click_decision(&None, false), LinkDecision::Ignore),
            "no pending decision → nothing to act on"
        );
    }

    /// Worker-side coalescing (pure): Load supersedes, relayout runs
    /// collapse, scroll runs collapse to the newest — but a user Event is
    /// never dropped and breaks both collapsing runs.
    #[test]
    fn coalesce_batch_superseded_runs() {
        let load = |id| LoadReq::Load { id, url: "https://x.com/".into(), max_w: 1.0, vh: 1.0, engine: EngineChoice::A, fonts: FontsChoice::A, restore: false };
        let relo = |id| LoadReq::Relayout { id, url: "https://x.com/".into(), max_w: 1.0, vh: 1.0, forms: layout::FormState::default(), hover: None, engine: EngineChoice::A };
        let ev = |id, kind: &str| LoadReq::Event {
            id,
            url: "https://x.com/".into(),
            max_w: 1.0,
            vh: 1.0,
            node: 1,
            kinds: vec![kind.into()],
            value: None,
            key: None,
            href: None,
            submit: false,
            forms: layout::FormState::default(),
            engine: EngineChoice::A,
        };
        let sc = |y| LoadReq::Scroll { url: "https://x.com/".into(), y };
        // newest Load wins whole queue
        let out = coalesce_batch(vec![load(1), ev(2, "click"), load(3)]);
        assert_eq!(out.len(), 1, "only the newest Load survives");
        assert!(matches!(out[0], LoadReq::Load { id: 3, .. }));
        // consecutive relayouts collapse to the newest
        let out = coalesce_batch(vec![relo(5), relo(6), relo(7)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id(), 7);
        // event breaks a relayout run: both survive
        let out = coalesce_batch(vec![relo(5), ev(6, "change"), relo(7)]);
        assert_eq!(out.len(), 3);
        // consecutive scrolls collapse to the newest y
        let out = coalesce_batch(vec![sc(10.0), sc(20.0), sc(30.0)]);
        assert_eq!(out.len(), 1, "scroll run collapses to latest");
        if let LoadReq::Scroll { y, .. } = &out[0] {
            assert_eq!(*y, 30.0);
        }
        // scroll event is never collapsed into a relayout
        let out = coalesce_batch(vec![relo(1), sc(9.0), relo(2)]);
        assert_eq!(out.len(), 3, "mixed kinds never merge");
    }

    /// Hover must not re-dispatch when the pointer is still: the hit test
    /// after our own relayout can land on a different node, which used to
    /// feed an endless relayout loop on a real article (one pass per layout,
    /// ~4 s each). Only pointer motion (or a press) may change `:hover`;
    /// busy jobs retry next frame; scrolling never re-hovers.
    #[test]
    fn hover_relayout_needs_pointer_motion() {
        // still pointer, hit changed by our own relayout → no new job
        assert!(!hover_should_relayout(Some(2), Some(1), false, false, false));
        // pointer moved onto another node → relayout
        assert!(hover_should_relayout(Some(2), Some(1), true, false, false));
        // unchanged hit → nothing to do even when moving
        assert!(!hover_should_relayout(Some(1), Some(1), true, false, false));
        // a job is already in flight → self-heal next frame
        assert!(!hover_should_relayout(Some(2), Some(1), true, true, false));
        // mid-scroll: content slides under a still pointer
        assert!(!hover_should_relayout(Some(2), Some(1), true, false, true));
        // leaving the page (Some → None) still needs a motion
        assert!(hover_should_relayout(None, Some(1), true, false, false));
        assert!(!hover_should_relayout(None, Some(1), false, false, false));
    }

    /// The unified wake budget: wait until the earliest due JS work; never
    /// less than ~1ms even for a momentarily-passed deadline (busy-spin
    /// guard); long idle block when nothing is scheduled.
    #[test]
    fn recv_budget_never_spins_and_blocks_idle() {        let now = Instant::now();
        assert_eq!(recv_budget(None, now), Duration::from_secs(3600));
        // past deadline → clamps to the 1ms floor (no zero/zero wake loop)
        assert_eq!(recv_budget(Some(now - Duration::from_secs(5)), now), Duration::from_millis(1));
        // future deadline → exact wait (saturating)
        assert_eq!(recv_budget(Some(now + Duration::from_secs(2)), now), Duration::from_secs(2));
    }

    /// data: URL decoding (the inline-document pages this shell can open
    /// with no network): plain, percent-encoded, base64 and malformed.
    #[test]
    fn data_urls_decode_inline_documents() {
        let (b, ct) = data_url_body("data:text/html,<h1>hi</h1>").unwrap();
        assert_eq!(ct.as_deref(), Some("text/html"));
        assert_eq!(b, b"<h1>hi</h1>");
        // percent-encoded %3C %3E and a charset parameter (unmodeled, still text)
        let (b, _) = data_url_body("data:text/html,%3Chtml%3EOK%3C/html%3E").unwrap();
        assert_eq!(b, b"<html>OK</html>");
        // base64 payload ("hello" → aGVsbG8=)
        let (b, _) = data_url_body("data:text/plain;base64,aGVsbG8=").unwrap();
        assert_eq!(b, b"hello");
        // mediatype omitted → no content-type, plain payload
        let (b, ct) = data_url_body("data:,plain").unwrap();
        assert_eq!(ct, None);
        assert_eq!(b, b"plain");
        // malformed: no comma
        assert!(data_url_body("data:text/html").is_none());
        assert!(data_url_body("https://example.com/x").is_none());
    }

    /// The address bar passes data: URLs through (they are never searches).
    #[test]
    fn address_target_passes_data_urls() {
        let u = "data:text/html,<p>x</p>".to_string();
        assert_eq!(address_target(&u), u, "data: is not a search query");
        // unchanged legacy behavior still holds
        assert_eq!(address_target("example.com"), "https://example.com");
        assert!(address_target("rust lang").starts_with("https://duckduckgo.com/"));
    }

    /// Link targets resolve against the page URL; an absolute `http(s)` href
    /// must survive a non-http base (a `data:` test page), which is what the
    /// context menu's link rows rely on.
    #[test]
    fn link_targets_resolve_from_a_data_page() {
        assert_eq!(
            resolve("data:text/html,<p>x</p>", "https://www.rust-lang.org/tools/install")
                .as_deref(),
            Some("https://www.rust-lang.org/tools/install")
        );
        assert_eq!(
            resolve("https://x.com/a/b", "../c").as_deref(),
            Some("https://x.com/c")
        );
    }

    /// Link clicks land on a DOM node even though layout runs carry only
    /// the href: the worker resolves the first anchor with a matching
    /// resolved href and bubbles the click from there.
    #[test]
    fn link_clicks_resolve_to_anchor_nodes() {
        let dom = html::parse(
            r#"<html><body><a href="/a">A</a><a href="/b">B</a>
                <a href="https://other.example/c">C</a></body></html>"#,
        );
        let base = "https://x.com/";
        assert_eq!(find_node_by_href(&dom, base, "https://x.com/a").unwrap(), dom_child_link(&dom, "/a"));
        assert_eq!(find_node_by_href(&dom, base, "https://x.com/b").unwrap(), dom_child_link(&dom, "/b"));
        assert_eq!(find_node_by_href(&dom, base, "https://other.example/c").unwrap(), dom_child_link(&dom, "https://other.example/c"));
        assert_eq!(find_node_by_href(&dom, base, "https://x.com/missing"), None);
    }

    /// helper: node id of the first <a> with the given raw href.
    fn dom_child_link(dom: &html::Dom, href: &str) -> u32 {
        (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "a" && dom.attr(i, "href") == Some(href))
            .unwrap()
    }

    // ---------- image sources: data: URLs, srcset / <picture> ----------

    /// Fixture PNG (2x3 RGBA) through the same encoder image.rs tests use.
    fn fixture_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 2, 3);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            let data: Vec<u8> = (0..6)
                .flat_map(|i| [((i + 1) * 40) as u8, 0x20, 0x10, 0xff])
                .collect();
            w.write_image_data(&data).unwrap();
        }
        bytes
    }

    /// Fixture WebP: 3x2 lossless RGBA (same bytes as image.rs's fixture).
    const FIXTURE_WEBP: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x3c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x4c,
        0x2f, 0x00, 0x00, 0x00, 0x2f, 0x02, 0x40, 0x00, 0x10, 0x2f, 0x20, 0x10, 0x48, 0xda, 0x1f, 0x7a,
        0x0d, 0x01, 0x41, 0x91, 0xff, 0xa3, 0x09, 0x08, 0x8a, 0xfc, 0x1f, 0x8d, 0x20, 0xdb, 0xa6, 0x68,
        0xbb, 0x3f, 0xd4, 0x03, 0xe6, 0x86, 0x95, 0xb9, 0x40, 0x20, 0x40, 0x99, 0x2a, 0x5c, 0x10, 0xd1,
        0xff, 0xf8, 0x52, 0x00,
    ];

    /// RFC 4648 base64 (test-only encoder for the `data:` fixtures).
    fn b64(bytes: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let n = (c[0] as u32) << 16
                | (c.get(1).copied().unwrap_or(0) as u32) << 8
                | c.get(2).copied().unwrap_or(0) as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
        }
        out
    }

    fn find_img(n: &layout::BoxNode) -> Option<&layout::ImgSlot> {
        if n.img.is_some() {
            return n.img.as_ref();
        }
        n.children.iter().find_map(find_img)
    }

    /// `<img>` pipeline end to end: box build -> probe -> layout -> decode.
    /// Returns (probed, skipped, probed dims, drawn (w, h, pixels)).
    #[allow(clippy::type_complexity)]
    fn img_pipeline(
        markup: &str,
        base: &str,
        vw: f32,
    ) -> (
        usize,
        usize,
        Option<(u32, u32)>,
        Option<(u32, u32, Vec<u32>)>,
    ) {
        let (a, b, c, d, _) = img_pipeline_cached(markup, base, vw, &mut ImgCache::default());
        (a, b, c, d)
    }

    /// `img_pipeline` with a caller-supplied memo, so a test can run two
    /// layouts back to back and check the second reused the pixels.
    fn img_pipeline_cached(
        markup: &str,
        base: &str,
        vw: f32,
        cache: &mut ImgCache,
    ) -> (
        usize,
        usize,
        Option<(u32, u32)>,
        Option<(u32, u32, Vec<u32>)>,
        Vec<Rc<image::ImageData>>,
    ) {
        let dom = html::parse(markup);
        let mut root = layout::build(
            &dom,
            dom.body_root(),
            Style::new(),
            css::BoxStyle::default(),
            &css::parse_for("", vw),
        );
        let (probed, skipped) = probe_images(&mut root, base);
        let dims = match find_img(&root) {
            Some(layout::ImgSlot::Probed { w, h, .. }) => Some((*w, *h)),
            _ => None,
        };
        let mut laid =
            layout::layout_tree(&mut root, vw, &|s: &str, _: &Style| s.chars().count() as f32 * 8.0);
        let mut budget = IMG_PIXEL_BUDGET;
        let mut next = 0usize;
        let mut next_bd = 0usize;
        let fonts = Fonts::load();
        decode_images(
            &mut root,
            base,
            &mut laid,
            &mut budget,
            false,
            None,
            false,
            &mut next,
            &mut next_bd,
            cache,
            &fonts,
        );
        let drawn = laid
            .images
            .first()
            .map(|(_, _, _, _, img, _, _, _)| (img.w, img.h, img.px.clone()));
        let rcs = laid.images.iter().map(|e| e.4.clone()).collect();
        (probed, skipped, dims, drawn, rcs)
    }

    #[test]
    fn first_frame_probes_inline_svg_but_not_network_images() {
        // the early frame skips network images (a download would delay the
        // paint) but an inline <svg> is markup, so its logo/icon pixels are
        // available immediately — they must not be blank until the second
        // pass the way a fetched <img> is
        let dom = html::parse(
            r#"<div><svg width="16" height="16" viewBox="0 0 16 16"><path d="M0 0h16v16H0z"/></svg><img src="a.png"></div>"#,
        );
        let mut root = layout::build(
            &dom,
            dom.body_root(),
            Style::new(),
            css::BoxStyle::default(),
            &css::parse_for("", 1000.0),
        );
        let (probed, skipped) = probe_images_local(&mut root);
        assert_eq!((probed, skipped), (1, 0), "the svg was probed, the img left alone");
        fn slots(n: &layout::BoxNode, out: &mut Vec<String>) {
            match &n.img {
                Some(layout::ImgSlot::Probed { w, h, .. }) => out.push(format!("probed {w}x{h}")),
                Some(layout::ImgSlot::Pending(s)) => out.push(format!("pending {s}")),
                _ => {}
            }
            for c in &n.children {
                slots(c, out);
            }
        }
        let mut out = Vec::new();
        slots(&root, &mut out);
        assert_eq!(out, ["probed 16x16", "pending a.png"]);
    }

    #[test]
    fn relayout_reuses_decoded_pixels_from_the_cache() {
        // A relayout (timer mutation, hover, event) rebuilds the page; the
        // memo must hand back the SAME buffer instead of decoding again —
        // that reuse is what keeps a relayout from re-paying the page's
        // whole image budget and what flattened the RAM climb.
        let url = format!("data:image/png;base64,{}", b64(&fixture_png()));
        let markup = format!(r#"<body><img src="{url}"></body>"#);
        let mut cache = ImgCache::default();
        let (_, _, _, first, rc1) = img_pipeline_cached(&markup, "https://example.com/x", 800.0, &mut cache);
        assert_eq!(rc1.len(), 1);
        let (_, _, _, second, rc2) = img_pipeline_cached(&markup, "https://example.com/x", 800.0, &mut cache);
        assert_eq!(second, first, "same picture on the relayout");
        assert!(Rc::ptr_eq(&rc1[0], &rc2[0]), "the pixel buffer is shared, not re-decoded");
        assert_eq!(cache.map.len(), 1, "the second layout added no slot");
        // a different display size is a different slot: fresh decode
        let scaled = format!(r#"<body><img src="{url}" width="1" height="1"></body>"#);
        let (_, _, _, _, rc3) = img_pipeline_cached(&scaled, "https://example.com/x", 800.0, &mut cache);
        assert_eq!(cache.map.len(), 2, "a resized image is its own slot");
        assert!(!Rc::ptr_eq(&rc1[0], &rc3[0]), "and decodes again");
    }

    #[test]
    fn data_url_images_probe_and_decode_in_every_supported_format() {
        let png = format!("data:image/png;base64,{}", b64(&fixture_png()));
        let webp = format!("data:image/webp;base64,{}", b64(FIXTURE_WEBP));
        // percent-encoded SVG payload (the `#`-free, quote-light form sites use)
        let svg = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' \
                   width='10' height='20'%3E%3Crect width='10' height='20' fill='red'/%3E%3C/svg%3E"
            .to_string();
        for (url, want) in [(&png, (2u32, 3u32)), (&webp, (3, 2)), (&svg, (10, 20))] {
            let markup = format!(r#"<body><img src="{url}"></body>"#);
            let (probed, skipped, dims, drawn) = img_pipeline(&markup, "https://example.com/x", 800.0);
            assert_eq!((probed, skipped), (1, 0), "probed one, skipped none: {url}");
            assert_eq!(dims, Some(want), "intrinsic size from {url}");
            let (w, h, px) = drawn.expect("pixels decoded");
            assert_eq!((w, h), want, "decoded size from {url}");
            assert!(px.iter().any(|p| p & 0x00ff_ffff != 0), "opaque pixels: {url}");
        }
    }

    #[test]
    fn data_url_reaches_the_box_untouched_and_round_trips_through_resolve() {
        let url = format!("data:image/png;base64,{}", b64(&fixture_png()));
        // resolve_img passes `data:` through verbatim (any case)
        assert_eq!(resolve_img("https://example.com/p", &url).as_deref(), Some(url.as_str()));
        let upper = url.replacen("data:", "DATA:", 1);
        assert_eq!(resolve_img("https://example.com/p", &upper).as_deref(), Some(upper.as_str()));
        // ordinary image URLs still resolve against the page
        assert_eq!(
            resolve_img("https://example.com/a/b", "img/x.png").as_deref(),
            Some("https://example.com/a/img/x.png")
        );
        // the box carries the exact attribute string
        let dom = html::parse(&format!(r#"<body><img src="{url}"></body>"#));
        let root = layout::build(
            &dom,
            dom.body_root(),
            Style::new(),
            css::BoxStyle::default(),
            &css::parse(""),
        );
        let Some(layout::ImgSlot::Pending(src)) = find_img(&root) else {
            panic!("img box should carry the source")
        };
        assert_eq!(src.as_ref(), url.as_str());
    }

    #[test]
    fn data_url_background_image_decodes_too() {
        let url = format!("data:image/png;base64,{}", b64(&fixture_png()));
        let markup = format!(
            r#"<body><div style="background-image: url({url})">x</div></body>"#
        );
        let (probed, skipped, dims, drawn) = img_pipeline(&markup, "https://example.com/", 800.0);
        assert_eq!((probed, skipped), (1, 0), "bg data: URL probed");
        assert_eq!(dims, Some((2, 3)));
        assert!(drawn.is_some(), "bg data: URL decoded");
    }

    #[test]
    fn gradient_stop_interpolation_is_linear_and_clamped() {
        let g = css::Gradient {
            dir: (1.0, 0.0),
            radial: None,
            stops: vec![
                css::GradStop { pos: 0.2, color: 0xff0000, alpha: 255 },
                css::GradStop { pos: 0.6, color: 0x0000ff, alpha: 0 },
            ],
        };
        assert_eq!(grad_at(&g, 0.1), (0xff0000, 255), "before first stop clamps");
        let (c, a) = grad_at(&g, 0.4);
        assert_eq!(c, 0xff0000, "premultiplied midpoint keeps the opaque hue");
        assert!((a as i32 - 127).abs() <= 1, "midpoint alpha: {a}");
        assert_eq!(grad_at(&g, 0.9), (0x0000ff, 0), "after last stop clamps");
        // two opaque stops still interpolate both RGB channels linearly
        let solid = css::Gradient {
            dir: (1.0, 0.0),
            radial: None,
            stops: vec![
                css::GradStop { pos: 0.0, color: 0xff0000, alpha: 255 },
                css::GradStop { pos: 1.0, color: 0x0000ff, alpha: 255 },
            ],
        };
        assert_eq!(grad_at(&solid, 0.5), (0x7f007f, 255));
    }

    #[test]
    fn solid_rect_clips_instead_of_smearing_from_the_edge() {
        // a painted transform can put a box left of / above the page; the
        // visible span must be the INTERSECTION, not the full width drawn
        // from pixel 0 (the old clamp smeared negative origins)
        let mut buf = vec![0u32; 20 * 10];
        paint_solid_rect(&mut buf, 20, 10, -5.0, -3.0, 10.0, 8.0, 0.0, 0xff0000, 255);
        assert_eq!(buf[0], 0xff0000);
        assert_eq!(buf[4], 0xff0000, "last visible column of the 10px rect");
        assert_eq!(buf[5], 0, "rect ends at x=5");
        assert_eq!(buf[4 * 20], 0xff0000, "last visible row");
        assert_eq!(buf[5 * 20], 0, "rect ends at y=5");

        // fully off-buffer: nothing is painted (no panic, no smear)
        let mut buf = vec![0u32; 20 * 10];
        paint_solid_rect(&mut buf, 20, 10, -30.0, 0.0, 10.0, 4.0, 0.0, 0xff0000, 255);
        assert!(buf.iter().all(|&p| p == 0));

        // the rounded path clips the same way
        let mut buf = vec![0u32; 20 * 10];
        paint_solid_rect(&mut buf, 20, 10, -5.0, -3.0, 10.0, 8.0, 3.0, 0x0000ff, 255);
        assert_eq!(buf[3 * 20 + 5], 0, "outside the corner radius");
        assert_eq!(buf[3 * 20 + 3], 0x0000ff, "inside the rounded rect");
    }

    #[test]
    fn linear_gradient_paints_along_its_direction() {
        let g = css::Gradient {
            dir: (1.0, 0.0), // to right
            radial: None,
            stops: vec![
                css::GradStop { pos: 0.0, color: 0xff0000, alpha: 255 },
                css::GradStop { pos: 1.0, color: 0x0000ff, alpha: 255 },
            ],
        };
        let mut buf = vec![0u32; 100];
        paint_gradient(&mut buf, 10, 10, 0.0, 0.0, 10.0, 1.0, 0.0, &g, 255);
        assert_eq!(buf[0], 0xff0000, "left edge = first stop");
        let (r0, _, b0) = ((buf[0] >> 16) & 0xff, (buf[0] >> 8) & 0xff, buf[0] & 0xff);
        let (r9, _, b9) = ((buf[9] >> 16) & 0xff, (buf[9] >> 8) & 0xff, buf[9] & 0xff);
        assert_eq!((r0, b0), (255, 0));
        assert!(b9 > 200 && r9 < 55, "right edge near the last stop: {r9}/{b9}");

        // a vertical direction changes only the y axis: every row differs
        let v = css::Gradient { dir: (0.0, 1.0), ..g };
        let mut buf = vec![0u32; 100];
        paint_gradient(&mut buf, 10, 10, 0.0, 0.0, 1.0, 10.0, 0.0, &v, 255);
        assert_eq!(buf[0], 0xff0000);
        assert!(buf[9 * 10] & 0xff > 200, "bottom row near blue");
    }

    #[test]
    fn radial_gradient_paints_from_its_center_outward() {
        // 10x10 box, circle radius 5 at the center: the middle pixel is the
        // first stop and a corner (distance ~7.07) is past the last stop
        let g = css::Gradient {
            dir: (0.0, 1.0),
            radial: Some(css::Radial {
                center: [css::BgPos::Pct(50.0); 2],
                circle: true,
                size: css::RadSize::Lengths(
                    Some(css::Len::Px(5.0)),
                    Some(css::Len::Px(5.0)),
                ),
            }),
            stops: vec![
                css::GradStop { pos: 0.0, color: 0xff0000, alpha: 255 },
                css::GradStop { pos: 1.0, color: 0x0000ff, alpha: 255 },
            ],
        };
        let mut buf = vec![0u32; 100];
        paint_gradient(&mut buf, 10, 10, 0.0, 0.0, 10.0, 10.0, 0.0, &g, 255);
        assert_eq!(buf[5 * 10 + 5], 0xff0000, "center = first stop");
        assert_eq!(buf[0], 0x0000ff, "corner past the radius = last stop");
        assert!(buf[6 * 10 + 6] != 0xff0000, "off-center pixel blended");
    }

    #[test]
    fn image_accept_advertises_webp_only_among_modern_formats() {
        assert!(IMAGE_ACCEPT.contains("image/webp"), "{IMAGE_ACCEPT}");
        assert!(!IMAGE_ACCEPT.contains("avif"), "no decoder, no advert");
        // formats we can decode stay advertised
        assert!(IMAGE_ACCEPT.contains("image/png"));
        assert!(IMAGE_ACCEPT.contains("image/jpeg"));
    }

    #[test]
    fn plain_img_src_is_the_same_url_as_before() {
        // byte-identical paint guard: no responsive markup -> the box holds
        // the raw `src` attribute exactly as it always did
        let src = "img/hero.png?w=1&h=2#frag";
        let dom = html::parse(&format!(r#"<body><img src="{src}" width="20"></body>"#));
        let root = layout::build(
            &dom,
            dom.body_root(),
            Style::new(),
            css::BoxStyle::default(),
            &css::parse(""),
        );
        let Some(layout::ImgSlot::Pending(chosen)) = find_img(&root) else {
            panic!("img box should carry the source")
        };
        assert_eq!(chosen.as_ref(), src);
    }

    #[test]
    fn srcset_and_picture_reach_the_box_as_the_chosen_url() {
        // the URL that layout carries is the one probe/decode will fetch
        let markup = r#"<body><picture>
            <source type="image/webp" srcset="h-400.webp 400w, h-900.webp 900w">
            <img src="h.jpg" srcset="h-400.jpg 400w, h-900.jpg 900w">
        </picture></body>"#;
        let dom = html::parse(markup);
        let root = layout::build(
            &dom,
            dom.body_root(),
            Style::new(),
            css::BoxStyle::default(),
            &css::parse_for("", 800.0),
        );
        let Some(layout::ImgSlot::Pending(chosen)) = find_img(&root) else {
            panic!("img box should carry the source")
        };
        assert_eq!(chosen.as_ref(), "h-900.webp");
    }

    #[test]
    fn scrollbar_thumb_is_proportional_and_floored() {
        // no overflow -> no bar at all
        assert_eq!(scrollbar_thumb(600.0, 600.0, 0.0), None);
        assert_eq!(scrollbar_thumb(400.0, 600.0, 0.0), None);
        // 1200 of 600: half the viewport, travel = 300, ends pinned
        let (y0, h) = scrollbar_thumb(1200.0, 600.0, 0.0).unwrap();
        assert_eq!((y0, h), (0.0, 300.0));
        let (ym, _) = scrollbar_thumb(1200.0, 600.0, 300.0).unwrap();
        assert_eq!(ym, 150.0);
        let (ye, _) = scrollbar_thumb(1200.0, 600.0, 600.0).unwrap();
        assert_eq!(ye, 300.0);
        // far past the end clamps like a real scroll offset
        let (ybig, _) = scrollbar_thumb(1200.0, 600.0, 9999.0).unwrap();
        assert_eq!(ybig, 300.0);
        // a huge document: thumb floored at BAR_MIN
        let (_, min_h) = scrollbar_thumb(100_000.0, 600.0, 0.0).unwrap();
        assert_eq!(min_h, BAR_MIN);
        // a tiny viewport still yields a sane thumb
        let (_, tiny) = scrollbar_thumb(1e6, 20.0, 0.0).unwrap();
        assert!(tiny > 0.0 && tiny <= 20.0);
    }

    #[test]
    fn scrollbar_thumb_inverse_round_trips() {
        let (doc, page) = (2400.0, 600.0);
        for scroll in [0.0, 37.0, 900.0, 1800.0] {
            let (y, _) = scrollbar_thumb(doc, page, scroll).unwrap();
            let back = scroll_from_thumb(doc, page, y);
            assert!((back - scroll).abs() < 1e-3, "{scroll} -> {y} -> {back}");
        }
        // top/bottom of the strip map to the scroll extremes
        assert_eq!(scroll_from_thumb(doc, 600.0, 0.0), 0.0);
        assert_eq!(scroll_from_thumb(doc, 600.0, 600.0), 1800.0);
        // negative travel clamps instead of going negative
        assert_eq!(scroll_from_thumb(doc, 600.0, -50.0), 0.0);
        // a page that fits has nothing to scroll
        assert_eq!(scroll_from_thumb(500.0, 600.0, 10.0), 0.0);
    }

    #[test]
    fn scrollbar_strip_is_inside_the_right_gutter() {
        let (x0, x1) = scrollbar_strip(1000);
        assert!(x1 > x0);
        // right of the content area, left of the buffer edge
        assert!(x0 >= (1000 - PADDING) as f32);
        assert!(x1 <= 998.0);
        assert_eq!(x1 - x0, BAR_W as f32);
        // a pathological narrow viewport can't produce an inverted strip
        let (a, b) = scrollbar_strip(10);
        assert!(b >= a);
    }
}

