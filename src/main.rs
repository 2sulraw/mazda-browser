//! Mazda — Phase 1: fetch → tokenize → arena DOM → word-wrap layout → render.
//! Phase 2: link hit-testing, click navigation, back history.
mod css;
mod html;
mod image;
mod layout;
mod svg;

use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont};
use layout::{Fam, Layout, Style};
use minifb::{Key, KeyRepeat, MouseButton, Window, WindowOptions};

const PADDING: usize = 12;
const BG: u32 = 0x00_1a_1a_1a;
/// Display pixels of decoded images kept in RAM per page (~4B/px RGBA).
/// Charged at decode time against laid-out size (what's retained), not
/// intrinsic size — over-budget images keep their probed dims (space
/// reserved, nothing drawn).
/// shortcut: first-come document-order cap, not LRU — pages rarely exceed it.
const IMG_PIXEL_BUDGET: usize = 8_000_000;

/// A rendered page: enough to draw it and hit-test its links.
struct Page {
    url: String,
    laid: Layout,
    /// page background from body/html background-color (fallback: BG const)
    bg: u32,
}

fn main() {
    let start_url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://example.com".to_string());

    let fonts = Fonts::load();
    let width = 1000usize;
    let height = 700usize;
    let max_w = (width - 2 * PADDING) as f32;
    let page_h = height.saturating_sub(2 * PADDING) as f32;

    let mut page = load_page(&start_url, &fonts, max_w, page_h);
    let mut back_stack: Vec<String> = Vec::new();

    let mut window = Window::new(
        &format!("Mazda — {}", page.url),
        width,
        height,
        WindowOptions::default(),
    )
    .expect("unable to open window");

    let mut scroll = 0.0f32;
    let mut buffer = vec![BG; width * height];
    let mut was_down = false;

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let max_scroll = (page.laid.height - page_h).max(0.0);
        if window.is_key_pressed(Key::Down, KeyRepeat::Yes) {
            scroll = (scroll + 40.0).min(max_scroll);
        }
        if window.is_key_pressed(Key::Up, KeyRepeat::Yes) {
            scroll = (scroll - 40.0).max(0.0);
        }
        if let Some((_, wy)) = window.get_scroll_wheel() {
            scroll = (scroll - wy * 60.0).clamp(0.0, max_scroll);
        }
        // back navigation (browser-style backspace)
        if window.is_key_pressed(Key::Backspace, KeyRepeat::No) {
            eprintln!("[key] backspace (no-repeat)");
            if let Some(prev) = back_stack.pop() {
                page = load_page(&prev, &fonts, max_w, page_h);
                scroll = 0.0;
                window.set_title(&format!("Mazda — {}", page.url));
            }
        }

        // click: hit-test runs for an href, resolve relative to current page
        let down = window.get_mouse_down(MouseButton::Left);
        if down && !was_down {
            if let Some((mx, my)) = window.get_mouse_pos(minifb::MouseMode::Pass) {
                let rx = mx - PADDING as f32;
                let my_doc = my - PADDING as f32; // viewport y in page space
                let hit = page.laid.runs.iter().find(|r| {
                    // fixed runs don't scroll: compare in viewport coords
                    let ry = if r.fixed { my_doc } else { my_doc + scroll };
                    r.link.is_some()
                        && rx >= r.x
                        && rx <= r.x + r.width
                        && ry >= r.y
                        && ry <= r.y + r.style.size * 1.35
                });
                eprintln!("[click] mouse=({mx:.0},{my:.0}) scroll={scroll:.0} hit={:?}", hit.map(|h| &h.text));
                if let Some(hit) = hit {
                    let href = hit.link.as_ref().unwrap().to_string();
                    if let Some(next) = resolve(&page.url, &href) {
                        eprintln!("[nav] {} → {next}", page.url);
                        back_stack.push(page.url.clone());
                        page = load_page(&next, &fonts, max_w, page_h);
                        scroll = 0.0;
                        window.set_title(&format!("Mazda — {}", page.url));
                    }
                }
            }
        }
        was_down = down;

        buffer.fill(page.bg);
        render(&mut buffer, width, height, &page.laid, &fonts, scroll, page_h);

        let mem = format!("RSS {:.1} MB", working_set_mb());
        draw_text(
            &mut buffer,
            width,
            (width - PADDING - mem.len() * 8) as f32,
            (height - PADDING - 14) as f32,
            &mem,
            &fonts,
            &Style {
                color: 0x00_88_cc_88,
                ..Style::new()
            },
        );

        window
            .update_with_buffer(&buffer, width, height)
            .expect("render failed");
    }
    println!("final RSS: {:.1} MB", working_set_mb());
}

/// Fetch + parse + layout one page. `vh` = viewport height (fixed boxes).
fn load_page(url: &str, fonts: &Fonts, max_w: f32, vh: f32) -> Page {
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
            format!("<p>Failed to load: {e}</p>")
        }
    };
    let dom = html::parse(&source);
    eprintln!("[mem] after parse: {:.1} MB ({} nodes)", working_set_mb(), dom.nodes.len());
    let root = dom.body_root();

    // Phase 1b: collect every stylesheet in document order — <style> blocks
    // (usually in <head>, which body_root skips) and external <link>s — then
    // parse ONCE into precompiled selectors.
    let mut sources = Vec::new();
    collect_sheets(&dom, 0, &mut sources);
    let mut css_text = String::new();
    let mut ext_count = 0usize;
    for src in &sources {
        match src {
            SheetSrc::Inline(text) => {
                css_text.push_str(text);
                css_text.push('\n');
            }
            SheetSrc::Link(href) => {
                let Some(abs) = resolve(url, href) else { continue };
                match fetch(&abs) {
                    Ok((bytes, _)) => {
                        // CSS is UTF-8 in practice (spec @charset unsupported — shortcut)
                        css_text.push_str(&String::from_utf8_lossy(&bytes));
                        css_text.push('\n');
                        ext_count += 1;
                    }
                    Err(e) => eprintln!("[css] stylesheet fetch failed {abs}: {e}"),
                }
            }
        }
    }
    // media queries evaluate against the window's CSS pixel width
    let viewport = max_w + 2.0 * PADDING as f32;
    let sheet = css::parse_for(&css_text, viewport);
    eprintln!(
        "[css] {} bytes of style, {} rules ({} external sheets)",
        css_text.len(),
        sheet.rule_count(),
        ext_count
    );

    // cascade over the body element itself: page bg + initial inherited style
    let body = sheet.resolve(&dom, root, Style::new(), dom.attr(root, "style"));
    let bg = body.bg.unwrap_or(BG);

    let mut root_box = layout::build(&dom, root, body.style, body.bx, &sheet);
    eprintln!(
        "[mem] after box tree: {:.1} MB ({} boxes)",
        working_set_mb(),
        layout::count_boxes(&root_box)
    );

    // fetch + probe every <img> header (disk-cached; no pixel decode yet —
    // layout only needs the intrinsic size to size the box)
    let mut img_budget = IMG_PIXEL_BUDGET;
    let (n_img, skipped) = probe_images(&mut root_box, url);
    if n_img > 0 || skipped > 0 {
        eprintln!("[img] {n_img} probed, {skipped} skipped (format)");
    }

    let mut laid = layout::layout_in(&mut root_box, max_w, vh, &|s: &str, st: &Style| fonts.measure(s, st));
    // decode pixels only now, one image at a time, resampled straight to the
    // laid-out display size — full-resolution buffers never accumulate
    let n_px = decode_images(&mut root_box, url, &mut laid, &mut img_budget, false);
    if n_px > 0 {
        eprintln!("[img] {n_px} decoded at display size");
    }
    eprintln!(
        "[mem] after layout: {:.1} MB ({} runs)",
        working_set_mb(),
        laid.runs.len()
    );
    if let Ok(v) = std::env::var("MAZDA_DUMP_RUNS") {
        eprintln!("[layout] total height={:.0}", laid.height);
        let all = v == "2"; // MAZDA_DUMP_RUNS=2 dumps every run, not just 6
        for (i, r) in laid.runs.iter().enumerate() {
            if !all && i >= 6 {
                break;
            }
            eprintln!("[run {i}] y={:.0} x={:.0} text={:?}", r.y, r.x, r.text);
        }
        for (i, r) in laid.runs.iter().filter(|r| r.link.is_some()).enumerate() {
            eprintln!(
                "[link {i}] y={:.0} x={:.0} w={:.0} href={:?} text={:?}",
                r.y, r.x, r.width, r.link, r.text
            );
        }
    }
    Page {
        url: url.to_string(),
        laid,
        bg,
    }
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
        } else if let Some(abs) = resolve(base, &src) {
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
                        eprintln!("[img] no size from {abs} ({} bytes, starts {:?})",
                            bytes.len(), &bytes[..bytes.len().min(12)]);
                        skipped += 1; // WebP/AVIF/corrupt
                    }
                },
                Err(e) => {
                    eprintln!("[img] fetch failed {abs}: {e}");
                    skipped += 1;
                }
            }
        } else {
            skipped += 1; // non-navigable scheme (data:, javascript: …)
        }
    }
    for c in &mut n.children {
        let (p, s) = probe_images(c, base);
        probed += p;
        skipped += s;
    }
    (probed, skipped)
}

/// Post-layout: decode each probed image (disk cache hit, no refetch),
/// resample to its laid-out display size, and emit draw entries. One image's
/// full-size buffer is alive at a time. `budget` caps the display pixels that
/// stay in RAM — over-budget images keep their `Probed` dims (space reserved,
/// nothing drawn).
fn decode_images(
    n: &mut layout::BoxNode,
    base: &str,
    laid: &mut Layout,
    budget: &mut usize,
    fixed: bool,
) -> usize {
    let fixed = fixed || n.bx.pos == css::Pos::Fixed;
    let mut count = 0usize;
    if let Some(layout::ImgSlot::Probed { src, .. }) = &n.img {
        let src = src.to_string();
        let inline = src.starts_with('<'); // inline <svg> markup — no URL
        let dw = n.w.round().max(1.0) as u32;
        let dh = n.h.round().max(1.0) as u32;
        let px = (dw as usize) * (dh as usize);
        if px <= *budget {
            let bytes = if inline {
                Some(src.clone().into_bytes())
            } else {
                resolve(base, &src)
                    .and_then(|abs| fetch_image(&abs).ok().map(|(b, _)| b))
            };
            if let Some(bytes) = bytes {
                *budget -= px;
                // raster: decode then resample; svg: rasterize straight at
                // display size (vector, no full-size buffer ever exists)
                let img = image::decode(&bytes)
                    .map(|full| {
                        if full.w > dw || full.h > dh {
                            image::downscale(&full, dw, dh)
                        } else {
                            full
                        }
                    })
                    .or_else(|| svg::render(&bytes, dw, dh));
                if let Some(img) = img {
                    laid.images
                        .push((n.x, n.y, n.w, n.h, std::rc::Rc::new(img), fixed));
                    count += 1;
                }
            }
        }
    }
    for c in &mut n.children {
        count += decode_images(c, base, laid, budget, fixed);
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

/// Resolve `href` against `base`; None for non-navigable schemes (javascript:)
/// or same-document links (anchors — we can't scroll-to-fragment yet).
fn resolve(base: &str, href: &str) -> Option<String> {
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

// ---------- HTTP disk cache (Phase 2) ----------
// shortcut: our own TTL instead of HTTP revalidation — no ETag/If-Modified-Since
// or 304 handling; a cached response is served as-is until TTL expires, then
// refetched. Eviction = oldest mtime at a file-count cap (not frequency-LRU).

const CACHE_TTL_SECS: u64 = 300;
const CACHE_MAX_FILES: usize = 300;
const CACHE_MAX_BODY: usize = 5 * 1024 * 1024;

fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("mazda-cache")
}

/// FNV-1a 64 — collision chance at 300 files is ~1e-13; not worth a hash dep.
fn cache_key(url: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in url.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Entry format: `<unix-secs>\n<content-type>\n<body>`. None = miss/expired.
fn cache_read_in(dir: &std::path::Path, url: &str, now: u64) -> Option<(Vec<u8>, Option<String>)> {
    let data = std::fs::read(dir.join(cache_key(url))).ok()?;
    let nl1 = data.iter().position(|&b| b == b'\n')?;
    let ts: u64 = std::str::from_utf8(&data[..nl1]).ok()?.parse().ok()?;
    if now.saturating_sub(ts) >= CACHE_TTL_SECS {
        return None; // expired (clock skew treated as stale, safe direction)
    }
    let rest = &data[nl1 + 1..];
    let nl2 = rest.iter().position(|&b| b == b'\n')?;
    let ct = std::str::from_utf8(&rest[..nl2]).ok()?.to_string();
    Some((
        rest[nl2 + 1..].to_vec(),
        (!ct.is_empty()).then_some(ct),
    ))
}

fn cache_write_in(
    dir: &std::path::Path,
    url: &str,
    now: u64,
    bytes: &[u8],
    ct: Option<&str>,
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
    let mut out = format!("{now}\n{}\n", ct.unwrap_or("")).into_bytes();
    out.extend_from_slice(bytes);
    let _ = std::fs::write(dir.join(cache_key(url)), out);
}

fn fetch(url: &str) -> Result<(Vec<u8>, Option<String>), String> {
    fetch_accept(url, None)
}

/// Fetch an image: content-negotiate for the formats we can decode.
/// Image proxies (next/image et al.) serve WebP/AVIF when the client
/// advertises support — omitting them gets the original PNG/JPEG back.
fn fetch_image(url: &str) -> Result<(Vec<u8>, Option<String>), String> {
    fetch_accept(url, Some("image/png, image/jpeg;q=0.9"))
}

fn fetch_accept(
    url: &str,
    accept: Option<&str>,
) -> Result<(Vec<u8>, Option<String>), String> {
    let dir = cache_dir();
    if let Some(hit) = cache_read_in(&dir, url, now_secs()) {
        eprintln!("[cache] hit {url}");
        return Ok(hit);
    }
    let mut req = ureq::get(url).set("User-Agent", "Mazda/0.1 (minimal Rust browser)");
    if let Some(a) = accept {
        req = req.set("Accept", a);
    }
    let resp = req.call().map_err(|e| e.to_string())?;
    let content_type = resp.header("content-type").map(|s| s.to_string());
    // into_reader transparently gunzips (ureq gzip feature); cap at 20MB like a browser
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
    cache_write_in(&dir, url, now_secs(), &buf, content_type.as_deref());
    Ok((buf, content_type))
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
}

/// Smallest full-coverage options first — raw bytes stay resident once loaded.
const CJK_FONTS: &[&str] = &[
    r"C:\Windows\Fonts\msyhl.ttc", // YaHei Light 11.6MB
    r"C:\Windows\Fonts\msyh.ttc",   // YaHei 18.8MB
    r"C:\Windows\Fonts\simsun.ttc",
    r"C:\Windows\Fonts\msjh.ttc",
    r"C:\Windows\Fonts\YuGothR.ttc",
    r"C:\Windows\Fonts\simhei.ttf",
];
const HANGUL_FONTS: &[&str] = &[
    r"C:\Windows\Fonts\malgunsl.ttf", // Malgun Light 4.8MB
    r"C:\Windows\Fonts\malgun.ttf",
];

impl Fonts {
    fn load() -> Self {
        Fonts {
            regular: load_font(&[
                r"C:\Windows\Fonts\segoeui.ttf",
                r"C:\Windows\Fonts\arial.ttf",
            ]),
            bold: std::sync::OnceLock::new(),
            italic: std::sync::OnceLock::new(),
            mono: std::sync::OnceLock::new(),
            serif: std::sync::OnceLock::new(),
            cjk: std::sync::OnceLock::new(),
            hangul: std::sync::OnceLock::new(),
        }
    }

    fn pick(&self, s: &Style) -> &FontVec {
        if s.fam == Fam::Mono {
            self.mono.get_or_init(|| {
                load_font(&[r"C:\Windows\Fonts\consola.ttf", r"C:\Windows\Fonts\cour.ttf"])
            })
        } else if s.fam == Fam::Serif {
            // Georgia ships regular/bold/italic/bold-italic on every Windows
            self.serif.get_or_init(|| {
                load_font(&[r"C:\Windows\Fonts\georgia.ttf", r"C:\Windows\Fonts\times.ttf"])
            })
        } else if s.bold && s.italic {
            &self.regular // no segoui-bi fallback; shortcut: faux-skip
        } else if s.bold {
            self.bold.get_or_init(|| {
                load_font(&[
                    r"C:\Windows\Fonts\segoeuib.ttf",
                    r"C:\Windows\Fonts\arialbd.ttf",
                ])
            })
        } else if s.italic {
            self.italic.get_or_init(|| {
                load_font(&[
                    r"C:\Windows\Fonts\segoeuii.ttf",
                    r"C:\Windows\Fonts\ariali.ttf",
                ])
            })
        } else {
            &self.regular
        }
    }

    /// Font that actually has a glyph for `ch`: primary style font, else the
    /// lazily-loaded CJK/Hangul fallback, else primary (renders .notdef).
    /// Range-gated so Thai/Arabic/etc. (no fallback font for them) never pay
    /// the font-load cost just to learn there's no glyph.
    /// shortcut: fallback is always the *regular* weight — no bold/italic CJK.
    fn pick_font_for(&self, ch: char, style: &Style) -> &FontVec {
        let primary = self.pick(style);
        if primary.glyph_id(ch).0 != 0 {
            return primary;
        }
        if is_cjk(ch) {
            if let Some(f) = self.cjk.get_or_init(|| try_load_font(CJK_FONTS)) {
                if f.glyph_id(ch).0 != 0 {
                    return f;
                }
            }
        }
        if is_hangul(ch) {
            if let Some(f) = self.hangul.get_or_init(|| try_load_font(HANGUL_FONTS)) {
                if f.glyph_id(ch).0 != 0 {
                    return f;
                }
            }
        }
        primary
    }

    fn measure(&self, text: &str, style: &Style) -> f32 {
        let scale = PxScale::from(style.size);
        text.chars()
            .map(|c| {
                let f = self.pick_font_for(c, style);
                f.as_scaled(scale).h_advance(f.glyph_id(c))
            })
            .sum()
    }
}

fn load_font(candidates: &[&str]) -> FontVec {
    try_load_font(candidates).unwrap_or_else(|| panic!("no system font found"))
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
) {
    // two passes: normal content (scrolled) first, then fixed overlays on
    // top in viewport coordinates (no scroll offset)
    for want_fixed in [false, true] {
        // box-shadows under everything else in this pass — blurred rounded
        // rects, per-pixel distance falloff over blur/2
        for s in &laid.shadows {
            if s.fixed != want_fixed {
                continue;
            }
            let y0 = if s.fixed { s.y } else { s.y - scroll };
            // grow the paint area by the blur so the falloff isn't clipped
            let pad = s.blur;
            if y0 + s.h + pad < 0.0 || y0 - pad > page {
                continue;
            }
            let (sx, sy) = (PADDING as f32 + s.x - pad, PADDING as f32 + y0 - pad);
            let (bw, bh) = (s.w + pad * 2.0, s.h + pad * 2.0);
            let a = (s.color >> 24) as u8;
            let half = (s.blur * 0.5).max(0.01);
            let (cx, cy) = (s.w * 0.5 + pad, s.h * 0.5 + pad);
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
        for &(x, y, w, h, color, rad, fixed) in &laid.rects {
            if fixed != want_fixed {
                continue;
            }
            let y = if fixed { y } else { y - scroll };
            if y + h < 0.0 || y > page {
                continue;
            }
            let (sx, sy) = (PADDING as f32 + x, (PADDING as f32 + y).max(0.0));
            let r = rad.min(w * 0.5).min(h * 0.5);
            for dy in 0..h.max(0.0) as usize {
                let py = (sy as usize) + dy;
                if py >= height {
                    break;
                }
                let ly = dy as f32; // y relative to rect top
                for dx in 0..w.max(0.0) as usize {
                    let px = sx as usize + dx;
                    if px >= width {
                        break;
                    }
                    // rounded corners: cull pixels outside the corner circles
                    if r > 0.5 {
                        let lx = dx as f32;
                        let cx = if lx < r { r } else if lx > w - r { w - r } else { lx };
                        let cy = if ly < r { r } else if ly > h - r { h - r } else { ly };
                        let (ex, ey) = (lx - cx, ly - cy);
                        if ex * ex + ey * ey > r * r {
                            continue;
                        }
                    }
                    buf[py * width + px] = color;
                }
            }
        }
        // decoded <img> boxes next (nearest-neighbour scale, alpha-blended)
        for (x, y, w, h, img, fixed) in &laid.images {
            if *fixed != want_fixed {
                continue;
            }
            let y0 = if *fixed { *y } else { y - scroll };
            if y0 + h < 0.0 || y0 > page {
                continue;
            }
            let (sx, sy) = (PADDING as f32 + x, PADDING as f32 + y0);
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
                    let c = img.px[(v * img.w + u) as usize];
                    let a = (c >> 24) as u8;
                    let idx = py as usize * width + px as usize;
                    if a == 255 {
                        buf[idx] = c & 0x00ff_ffff;
                    } else if a > 0 {
                        blend(&mut buf[idx], c, a);
                    }
                }
            }
        }
        for run in &laid.runs {
            if run.fixed != want_fixed {
                continue;
            }
            let y = if run.fixed { run.y } else { run.y - scroll };
            if y < -run.style.size * 2.0 || y > page + run.style.size {
                continue;
            }
            draw_text(
                buf,
                width,
                PADDING as f32 + run.x,
                PADDING as f32 + y,
                &run.text,
                fonts,
                &run.style,
            );
        }
        for &(x, y, w, h, color, fixed) in &laid.underline_rects {
            if fixed != want_fixed {
                continue;
            }
            let y = if fixed { y } else { y - scroll };
            if y < 0.0 || y > page {
                continue;
            }
            let (sx, sy) = (PADDING as f32 + x, PADDING as f32 + y);
            for dy in 0..h.max(1.0) as usize {
                for dx in 0..w as usize {
                    let px = sx as usize + dx;
                    let py = (sy as usize) + dy;
                    if px < width && py * width + px < buf.len() {
                        buf[py * width + px] = color;
                    }
                }
            }
        }
    }
    let _ = height;
}

fn draw_text(
    buf: &mut [u32],
    width: usize,
    x0: f32,
    y0: f32,
    s: &str,
    fonts: &Fonts,
    style: &Style,
) {
    let scale = PxScale::from(style.size);
    // baseline sits at y0 + ascent (~0.8em for these fonts)
    let baseline = y0 + style.size * 0.8;
    let mut x = x0;
    for ch in s.chars() {
        // same font choice as measure() so widths match what's drawn
        let font = fonts.pick_font_for(ch, style);
        let scaled = font.as_scaled(scale);
        let id = font.glyph_id(ch);
        let glyph = ab_glyph::Glyph {
            id,
            scale,
            position: ab_glyph::point(x, baseline),
        };
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                if coverage <= 0.0 {
                    return;
                }
                let dx = bounds.min.x as isize + gx as isize;
                let dy = bounds.min.y as isize + gy as isize;
                if dx >= 0 && (dx as usize) < width && dy >= 0 {
                    let idx = dy as usize * width + dx as usize;
                    if idx < buf.len() {
                        let a = (coverage * 255.0).clamp(0.0, 255.0) as u8;
                        blend(&mut buf[idx], style.color, a);
                    }
                }
            });
        }
        x += scaled.h_advance(id);
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

/// Working-set size in MB via GetProcessMemoryInfo.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mazda-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn cache_roundtrip_and_ttl() {
        let dir = test_dir("cache-rt");
        cache_write_in(&dir, "https://x.com/", 1000, b"hello", Some("text/html"));
        // fresh read returns body + content-type
        assert_eq!(
            cache_read_in(&dir, "https://x.com/", 1000 + CACHE_TTL_SECS - 1),
            Some((b"hello".to_vec(), Some("text/html".to_string())))
        );
        // expired
        assert_eq!(
            cache_read_in(&dir, "https://x.com/", 1000 + CACHE_TTL_SECS),
            None
        );
        // different URL = miss (FNV key differs)
        assert_eq!(cache_read_in(&dir, "https://y.com/", 1000), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_oversized_body_not_stored() {
        let dir = test_dir("cache-big");
        let big = vec![0u8; CACHE_MAX_BODY + 1];
        cache_write_in(&dir, "https://x.com/", 1000, &big, None);
        assert_eq!(cache_read_in(&dir, "https://x.com/", 1000), None);
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
    fn latin_stays_on_primary_font() {
        let fonts = Fonts::load();
        let primary = fonts.pick(&Style::new());
        for ch in "Hello, world! 123".chars() {
            assert!(
                std::ptr::eq(fonts.pick_font_for(ch, &Style::new()), primary),
                "char {ch:?} should not trigger fallback"
            );
        }
        // Thai has no fallback font — must not even consult one (costs nothing,
        // stays .notdef as before)
        assert!(
            std::ptr::eq(fonts.pick_font_for('ก', &Style::new()), primary),
            "Thai should stay on primary"
        );
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
}

