//! Mazda — Phase 1: fetch → tokenize → arena DOM → word-wrap layout → render.
//! Phase 2: link hit-testing, click navigation, back history.
mod css;
mod html;
mod image;
mod js;
mod layout;
mod script;
mod svg;
mod ui_chrome;
mod woff;

use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont};
use eframe::egui;
use layout::{Fam, Layout, Style};
use std::collections::HashMap;
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

/// A rendered page: enough to draw it and hit-test its links.
struct Page {
    url: String,
    laid: Layout,
    /// page background from body/html background-color (fallback: BG const)
    bg: u32,
}

/// One browser tab. Pages are NOT kept in RAM (that would blow the budget) —
/// a tab stores just its URL + scroll offset, and switching tabs reloads.
/// shortcut: reload-on-switch instead of caching every tab's DOM.
struct Tab {
    url: String,
    scroll: f32,
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

fn main() -> eframe::Result {
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
}

/// Address-bar heuristic (unchanged from the minifb shell): a dot and no
/// space means URL (https:// prefixed), anything else is a DuckDuckGo search.
fn address_target(q: &str) -> String {
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

/// 0x00RRGGBB framebuffer → egui ColorImage (RGBA, opaque).
fn pixels_to_image(pixels: &[u32], w: usize, h: usize) -> egui::ColorImage {
    debug_assert_eq!(pixels.len(), w * h);
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for &p in pixels {
        rgba.push((p >> 16) as u8); // R
        rgba.push((p >> 8) as u8); // G
        rgba.push(p as u8); // B
        rgba.push(255);
    }
    egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba)
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // dark widget theme so TextEdit's cursor/selection read on the dark
        // chrome; the page rasterizes its own fonts regardless
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let fonts = Fonts::load();
        let start = std::env::args()
            .nth(1)
            .unwrap_or_else(|| "https://example.com".to_string());
        // logical viewport = NativeOptions initial size; one texture pixel per
        // logical point (a HiDPI framebuffer upscales the texture — fine at
        // pixels_per_point 1.0; shortcut: no per-monitor DPI raster scale).
        let (pw, ph) = (1000usize, (700.0 - TAB_H - ADDR_H) as usize);
        let url = address_target(start.trim());
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
                images: Vec::new(),
                shadows: Vec::new(),
                open_line: false,
                forms: Vec::new(),
                focused: None,
                form_values: Default::default(),
            },
            bg: BG,
        };
        let pixels = vec![page.bg; pw * ph];
        // one long-lived worker thread; unbounded channels so neither side
        // can ever block the other on send
        let (job_tx, job_rx) = mpsc::channel::<LoadReq>();
        let (done_tx, done_rx) = mpsc::channel::<LoadDone>();
        std::thread::spawn(move || loader_thread(job_rx, done_tx));
        let mut app = App {
            fonts,
            page,
            tabs: vec![Tab {
                url: url.clone(),
                scroll: 0.0,
            }],
            active: 0,
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
        };
        app.dispatch(&url, pw, ph, 0.0);
        app
    }

    /// Queue a load of `url` on the worker thread (never blocks the UI).
    fn dispatch(&mut self, url: &str, pw: usize, ph: usize, scroll: f32) {
        let id = self.nav.begin();
        eprintln!("[nav] queued load {url} (#{id}, viewport {pw}x{ph})");
        let req = LoadReq::Load {
            id,
            url: url.to_string(),
            max_w: page_w(pw),
            vh: page_h(ph),
        };
        // receiver only drops when the worker thread is gone
        if self.jobs.send(req).is_err() {
            eprintln!("[nav] loader thread unavailable, load of {url} dropped");
        }
        self.pending = Some(PendingNav { scroll });
        self.expect_nav = true;
        // a fresh navigation clears form focus/values (worker starts clean)
        self.forms = layout::FormState::default();
    }

    /// Queue a re-layout of the current page (resize / form typing). Keeps
    /// scroll + address bar; carries the live form state forward.
    fn dispatch_relayout(&mut self, pw: usize, ph: usize) {
        let id = self.nav.begin();
        let url = self.page.url.clone();
        let req = LoadReq::Relayout {
            id,
            url: url.clone(),
            max_w: page_w(pw),
            vh: page_h(ph),
            forms: self.forms.clone(),
        };
        if self.jobs.send(req).is_err() {
            eprintln!("[nav] loader thread unavailable, relayout of {url} dropped");
        }
        self.expect_nav = false;
    }

    /// Queue a form-control event (click/submit/change). The worker mutates
    /// its DOM, fires listeners, re-lays out and re-emits.
    fn dispatch_event(&mut self, node: u32, kind: &str, value: Option<String>, pw: usize, ph: usize) {
        let id = self.nav.begin();
        let req = LoadReq::Event {
            id,
            url: self.page.url.clone(),
            max_w: page_w(pw),
            vh: page_h(ph),
            node,
            kind: kind.to_string(),
            value,
            forms: self.forms.clone(),
        };
        if self.jobs.send(req).is_err() {
            eprintln!("[nav] loader thread unavailable, {kind} event dropped");
        }
        self.expect_nav = false;
    }

    /// Drain finished loads; apply the newest (stale generations are dropped).
    fn drain_results(&mut self, pw: usize, ph: usize) {
        while let Ok(done) = self.results.try_recv() {
            if !self.nav.accept(done.id) {
                eprintln!("[nav] dropped stale result #{} (want #{})", done.id, self.nav.next);
                continue;
            }
            if done.panicked {
                eprintln!("[nav] load #{} failed (pipeline panic)", done.id);
                continue;
            }
            // a script may have asked to navigate (location.assign etc.)
            if let Some(nav_url) = done.nav {
                eprintln!("[js] script navigated to {nav_url}");
                self.dispatch(&nav_url, pw, ph, 0.0);
                continue;
            }
            let Some(mut wire) = done.page else {
                eprintln!("[nav] load #{} returned no page", done.id);
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

    /// Re-rasterize the active page into `pixels` (marks the texture dirty).
    fn repaint_page(&mut self, pw: usize, ph: usize) {
        self.pixels = vec![self.page.bg; pw * ph];
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
        self.dirty = true;
    }

    /// Load `url` into the active tab: push history, reset scroll.
    fn navigate(&mut self, url: &str, pw: usize, ph: usize) {
        self.back_stack.push(self.page.url.clone());
        self.dispatch(url, pw, ph, 0.0);
        self.tabs[self.active].url = url.to_string();
        self.address = url.to_string();
    }

    /// Load `url` into the active tab WITHOUT touching history (tab switch /
    /// close / back), restoring `scroll`.
    fn reload_active(&mut self, url: &str, pw: usize, ph: usize, scroll: f32) {
        self.dispatch(url, pw, ph, scroll);
        self.tabs[self.active].url = url.to_string();
        self.address = url.to_string();
    }

    /// A form control was clicked. Text fields take focus (no worker job
    /// needed until a key arrives); checkboxes/radios toggle + fire
    /// listeners on the worker; submit buttons log `[form] submit` and fire
    /// click listeners. The worker owns the DOM, so toggles/events go there.
    fn on_form_click(&mut self, fb: layout::FormBox, pw: usize, ph: usize) {
        match fb.kind {
            layout::FormKind::TextInput { .. } => {
                // focus this field, blur any other (values ride the wire)
                if self.forms.focused != Some(fb.node) {
                    self.forms.focused = Some(fb.node);
                    // seed the control's current display value so typing
                    // starts from what's shown
                    let cur = self.page.laid.form_value(fb.node).to_string();
                    self.forms.values.insert(fb.node, cur);
                    self.dispatch_relayout(pw, ph);
                }
            }
            layout::FormKind::Checkbox | layout::FormKind::Radio => {
                // worker toggles `checked`, dispatches click+change, relays
                self.forms.focused = None;
                self.dispatch_event(fb.node, "click", None, pw, ph);
            }
            layout::FormKind::Submit => {
                // shortcut: no form POST — the shell logs name=value (the
                // same observable contract the layout module documents)
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
                self.forms.focused = None;
                self.dispatch_event(fb.node, "click", None, pw, ph);
            }
            layout::FormKind::Select | layout::FormKind::Generic => {
                // no popup / non-interactive yet — just blur
                self.forms.focused = None;
                self.dispatch_relayout(pw, ph);
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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

        // Actions are collected during the panel closures and applied after
        // them (the page size is only known once the panels claimed their
        // space, and it keeps the borrows simple).
        let mut activate: Option<usize> = None;
        let mut close: Option<usize> = None;
        let mut new_tab = false;
        let mut go_back = false;
        let mut go: Option<String> = None; // address submit or link click

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
                for ev in ui_chrome::toolbar(ui, &mut self.address, can_back) {
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

                // scrolling: wheel + Up/Down, clamped, suppressed while typing
                let max_scroll = (self.page.laid.height - page_h(ph)).max(0.0);
                // form typing: route keys to the focused field (the address
                // bar owns the keyboard while it's focused). Enter submits
                // (shell logs), Escape blurs.
                if self.forms.focused.is_some() && !self.address_focused {
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
                    let mut dirty = false;
                    for a in actions {
                        match a {
                            KeyAction::Insert(c) => {
                                self.forms.values.entry(field).or_default().push(c);
                                dirty = true;
                            }
                            KeyAction::Backspace => {
                                if let Some(v) = self.forms.values.get_mut(&field) {
                                    v.pop();
                                }
                                dirty = true;
                            }
                            KeyAction::Submit => {
                                let name = self
                                    .form_names
                                    .get(&field)
                                    .cloned()
                                    .unwrap_or_default();
                                let val = self.forms.values.get(&field).cloned().unwrap_or_default();
                                eprintln!("[form] submit {name}={val}");
                            }
                            KeyAction::Blur => {
                                self.forms.focused = None;
                                dirty = true;
                            }
                            KeyAction::None => {}
                        }
                    }
                    if dirty {
                        // push the new value to the worker so it re-lays out
                        // with the updated text (and fires change listeners)
                        self.dispatch_event(field, "change", self.forms.values.get(&field).cloned(), pw, ph);
                    }
                }
                if !ui.ctx().egui_wants_keyboard_input() && !self.address_focused {
                    let before = self.scroll;
                    let wheel = ui.input(|i| i.smooth_scroll_delta());
                    if wheel.y != 0.0 {
                        self.scroll = (self.scroll - wheel.y).clamp(0.0, max_scroll);
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                        self.scroll = (self.scroll + 40.0).min(max_scroll);
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                        self.scroll = (self.scroll - 40.0).max(0.0);
                    }
                    if self.scroll != before {
                        self.repaint_page(pw, ph);
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
                    let resp = ui.add(
                        egui::Image::new(&*tex)
                            .fit_to_exact_size(egui::vec2(iw as f32, ih as f32))
                            .texture_options(egui::TextureOptions::NEAREST)
                            .sense(egui::Sense::click()),
                    );
                    // link hit-test — identical math to the old minifb shell
                    if resp.clicked() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let rx = p.x - resp.rect.min.x - PADDING as f32;
                            let my_doc = p.y - resp.rect.min.y - PADDING as f32;
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
                                        eprintln!("[nav] {} → {next}", self.page.url);
                                        go = Some(next);
                                    }
                                } else {
                                    // click on empty space blurs any focused field
                                    if self.forms.focused.take().is_some() {
                                        self.dispatch_relayout(pw, ph);
                                    }
                                }
                            }
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
            });

        let (pw, ph) = vp;

        // ---- apply the deferred actions with the real page size ----
        if let Some(i) = activate {
            if i != self.active && i < self.tabs.len() {
                self.tabs[self.active].scroll = self.scroll;
                self.tabs[self.active].url = self.page.url.clone();
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
            self.tabs[self.active].scroll = self.scroll;
            self.tabs[self.active].url = self.page.url.clone();
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
        // while a load is in flight, poll fast so the result lands promptly;
        // otherwise the RSS overlay keeps ticking at 1 Hz
        ui.ctx().request_repaint_after(if self.pending.is_some() {
            Duration::from_millis(100)
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
    },
    /// re-cascade + re-layout the kept page (resize / DOM change), carrying
    /// the UI's live form state forward
    Relayout {
        id: u64,
        url: String,
        max_w: f32,
        vh: f32,
        forms: layout::FormState,
    },
    /// a form control was activated (click/submit) or a text field changed:
    /// the worker mutates its DOM, fires listeners, re-lays out, re-emits
    Event {
        id: u64,
        url: String,
        max_w: f32,
        vh: f32,
        /// DOM node id of the control
        node: u32,
        /// "click" | "change" | "submit"
        kind: String,
        /// text value to push into the control (text inputs), else None
        value: Option<String>,
        forms: layout::FormState,
    },
}

impl LoadReq {
    fn id(&self) -> u64 {
        match self {
            LoadReq::Load { id, .. }
            | LoadReq::Relayout { id, .. }
            | LoadReq::Event { id, .. } => *id,
        }
    }
    fn url(&self) -> &str {
        match self {
            LoadReq::Load { url, .. }
            | LoadReq::Relayout { url, .. }
            | LoadReq::Event { url, .. } => url,
        }
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
    /// x, y, w, h, pixels (owned), fixed
    images: Vec<(f32, f32, f32, f32, image::ImageData, bool)>,
    shadows: Vec<layout::ShadowRect>,
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
}

/// Result sent back by the worker: the wire page (None = the pipeline panicked),
/// plus a script-requested navigation and the forms state the worker ended on.
struct LoadDone {
    id: u64,
    page: Option<PageWire>,
    /// script called `location.assign`/`location = ...` — UI should navigate
    nav: Option<String>,
    /// the worker's pipeline panicked (caught; process survives)
    panicked: bool,
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
    PageWire {
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
            })
            .collect(),
        underline_rects: p.laid.underline_rects,
        rects: p.laid.rects,
        images: p
            .laid
            .images
            .into_iter()
            .map(|(x, y, w, h, img, fixed)| {
                // freshly decoded per page: refcount is 1, but be safe
                let owned = std::rc::Rc::try_unwrap(img).unwrap_or_else(|rc| image::ImageData {
                    w: rc.w,
                    h: rc.h,
                    px: rc.px.clone(),
                });
                (x, y, w, h, owned, fixed)
            })
            .collect(),
        shadows: p.laid.shadows,
        stacks,
        faces,
        forms: p.laid.forms,
        focused: p.laid.focused,
        form_values: p.laid.form_values,
        form_names,
    }
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
                })
                .collect(),
            height: w.height,
            underline_rects: w.underline_rects,
            rects: w.rects,
            images: w
                .images
                .into_iter()
                .map(|(x, y, w, h, img, fixed)| (x, y, w, h, std::rc::Rc::new(img), fixed))
                .collect(),
            shadows: w.shadows,
            open_line: false, // paint-only consumer; flow state not needed
            // forms state rides the wire: rects for hit-testing, focus +
            // values for painting the accent border / caret / typed text
            forms: w.forms,
            focused: w.focused,
            form_values: w.form_values,
        },
    }
}

/// The parsed page the worker keeps between jobs so a re-layout or a script
/// event doesn't refetch/reparse. One live page per worker (the tabs reload
/// on switch). Holds the non-`Send` script engine, so it never leaves this
/// thread.
struct KeptPage {
    url: String,
    dom: html::Dom,
    /// concatenated stylesheet text (re-parsed per layout: media queries
    /// depend on the viewport width)
    css_text: String,
    /// `@font-face` defs (re-registered per layout)
    faces: Vec<css::FontFace>,
    /// script engine + listeners for this page; None until scripts ran
    scripts: Option<script::PageScripts>,
}

/// The single long-lived loader thread. Owns its own `Fonts` (font types
/// aren't `Send`, so each thread loads its own — system fonts come from disk
/// anyway) and the one kept page. Superseded loads/relayouts still queued are
/// skipped, but a form Event is never dropped (it carries a user action).
fn loader_thread(jobs: mpsc::Receiver<LoadReq>, results: mpsc::Sender<LoadDone>) {
    let mut fonts = Fonts::load();
    let mut kept: Option<KeptPage> = None;
    // geometry + form state of the last layout (timers re-layout with them)
    let mut last_geom = (0.0f32, 0.0f32);
    let mut last_forms = layout::FormState::default();
    // id of the job that produced the kept page (timer pushes reuse it so
    // the UI's generation gate accepts them)
    let mut current_id = 0u64;
    loop {
        // wake for the next script timer instead of blocking forever
        let timeout = kept
            .as_ref()
            .and_then(|k| k.scripts.as_ref().and_then(|s| s.next_timer_deadline()))
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));
        let first = match jobs.recv_timeout(timeout) {
            Ok(j) => j,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // fire due timers; a mutation re-lays out and pushes a result
                let mut push = None;
                if let Some(kp) = kept.as_mut() {
                    let KeptPage { dom, scripts, css_text, faces, url, .. } = kp;
                    if let Some(scripts) = scripts.as_mut() {
                        let (mw, vh) = last_geom;
                        if scripts.run_due_timers(dom) {
                            let (page, form_names) = layout_page(
                                dom, css_text, faces, url, &mut fonts, mw, vh, &last_forms,
                            );
                            let stacks = fonts.stacks.borrow().clone();
                            let out_faces = collect_out_faces(&fonts);
                            let wire = page_to_wire(page, stacks, out_faces, form_names);
                            push = Some(wire);
                        }
                    }
                }
                if let Some(wire) = push {
                    eprintln!("[js] timer mutated the page; pushed relayout");
                    if results
                        .send(LoadDone { id: current_id, page: Some(wire), nav: None, panicked: false })
                        .is_err()
                    {
                        break;
                    }
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // ---- coalesce: drop superseded loads/relayouts, keep every event ----
        let mut batch = vec![first];
        while let Ok(n) = jobs.try_recv() {
            batch.push(n);
        }
        // a Load supersedes everything queued before it (we'd never fetch it)
        if let Some(p) = batch.iter().rposition(|r| matches!(r, LoadReq::Load { .. })) {
            if p > 0 {
                eprintln!("[nav] skipped {p} superseded job(s)");
            }
            batch.drain(..p);
        }
        // collapse a consecutive run of relayouts to the newest (events break
        // the run, so an event between two relayouts survives)
        let mut out: Vec<LoadReq> = Vec::new();
        let mut collapsed = 0u32;
        for j in batch {
            if matches!(j, LoadReq::Relayout { .. })
                && matches!(out.last(), Some(LoadReq::Relayout { .. }))
            {
                out.pop();
                collapsed += 1;
            }
            out.push(j);
        }
        if collapsed > 0 {
            eprintln!("[nav] skipped {collapsed} superseded relayout(s)");
        }

        for job in out {
            let t0 = Instant::now();
            let id = job.id();
            let url = job.url().to_string();
            // worker panics must not take the process down
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process_job(&mut kept, &mut fonts, &job, &mut last_geom, &mut last_forms)
            }));
            let (wire, nav, panicked) = match outcome {
                Ok(r) => r,
                Err(_) => {
                    eprintln!("[nav] loader pipeline panicked for {url}");
                    kept = None; // don't trust a half-built page
                    (None, None, true)
                }
            };
            if wire.is_some() {
                current_id = id;
            }
            eprintln!(
                "[nav] worker finished {url} in {:.0} ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            // unbounded channel: never blocks; UI drains every frame
            if results
                .send(LoadDone { id, page: wire, nav, panicked })
                .is_err()
            {
                break; // UI side gone
            }
        }
    }
}

/// Handle one job against the kept page. Returns (wire page, script-requested
/// navigation, panicked-flag). Falls back to a full load when there's no kept
/// page for the job's URL.
fn process_job(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    job: &LoadReq,
    last_geom: &mut (f32, f32),
    last_forms: &mut layout::FormState,
) -> (Option<PageWire>, Option<String>, bool) {
    match job {
        LoadReq::Load { url, max_w, vh, .. } => {
            *last_geom = (*max_w, *vh);
            *last_forms = layout::FormState::default();
            do_load(kept, fonts, url, *max_w, *vh)
        }
        LoadReq::Relayout { url, max_w, vh, forms, .. } => {
            *last_geom = (*max_w, *vh);
            *last_forms = forms.clone();
            if !kept_matches(kept, url) {
                return do_load(kept, fonts, url, *max_w, *vh);
            }
            let kp = kept.as_mut().unwrap();
            let (page, form_names) = layout_page(
                &kp.dom, &kp.css_text, &kp.faces, url, fonts, *max_w, *vh, forms,
            );
            let stacks = fonts.stacks.borrow().clone();
            let out_faces = collect_out_faces(fonts);
            (Some(page_to_wire(page, stacks, out_faces, form_names)), None, false)
        }
        LoadReq::Event { url, max_w, vh, node, kind, value, forms, .. } => {
            *last_geom = (*max_w, *vh);
            *last_forms = forms.clone();
            if !kept_matches(kept, url) {
                return do_load(kept, fonts, url, *max_w, *vh);
            }
            let kp = kept.as_mut().unwrap();
            // push the control's state into the worker DOM (value / checked)
            apply_event(kp, *node, kind, value.as_deref());
            // fire the node's listeners for this kind (bubbles up the tree)
            let mut nav = None;
            {
                let KeptPage { dom, scripts, .. } = kp;
                if let Some(scripts) = scripts.as_mut() {
                    let t = scripts.dispatch(dom, *node, kind);
                    nav = t.nav;
                }
            }
            // always re-layout: the event likely changed the DOM
            let (page, form_names) = layout_page(
                &kp.dom, &kp.css_text, &kp.faces, url, fonts, *max_w, *vh, forms,
            );
            let stacks = fonts.stacks.borrow().clone();
            let out_faces = collect_out_faces(fonts);
            (Some(page_to_wire(page, stacks, out_faces, form_names)), nav, false)
        }
    }
}

fn kept_matches(kept: &Option<KeptPage>, url: &str) -> bool {
    kept.as_ref().map(|k| k.url == url).unwrap_or(false)
}

/// Full load: fetch + parse + run `<script>` + layout. Seeds/ replaces the
/// kept page.
fn do_load(
    kept: &mut Option<KeptPage>,
    fonts: &mut Fonts,
    url: &str,
    max_w: f32,
    vh: f32,
) -> (Option<PageWire>, Option<String>, bool) {
    let (dom, css_text, faces) = fetch_and_parse(url);
    let mut kp = KeptPage { url: url.to_string(), dom, css_text, faces, scripts: None };
    // run every <script> (inline + src) BEFORE layout so the DOM is settled
    let mut scripts = script::PageScripts::new(&mut kp.dom, url);
    let turn = scripts.run_scripts(&mut kp.dom);
    if turn.dirty {
        // inline scripts mutated the DOM before the first layout — the
        // layout below already sees the mutated tree
        eprintln!("[js] scripts mutated the DOM before layout");
    }
    kp.scripts = Some(scripts);
    let (page, form_names) = layout_page(
        &kp.dom, &kp.css_text, &kp.faces, url, fonts, max_w, vh,
        &layout::FormState::default(),
    );
    let stacks = fonts.stacks.borrow().clone();
    let out_faces = collect_out_faces(fonts);
    let wire = page_to_wire(page, stacks, out_faces, form_names);
    *kept = Some(kp);
    (Some(wire), turn.nav, false)
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

/// Apply a form event's state to the worker DOM before listeners fire.
fn apply_event(kp: &mut KeptPage, node: u32, kind: &str, value: Option<&str>) {
    // text input: push the typed value into the `value` attribute so both
    // layout and scripts reading getAttribute("value") see it
    if let Some(v) = value {
        set_dom_attr(&mut kp.dom, node, "value", v);
    }
    if kind == "click" {
        let tag = kp.dom.tag(node).to_string();
        if tag == "input" {
            let ty = kp.dom.attr(node, "type").unwrap_or("text").to_string();
            if ty == "checkbox" || ty == "radio" {
                if kp.dom.attr(node, "checked").is_some() {
                    remove_dom_attr(&mut kp.dom, node, "checked");
                } else {
                    set_dom_attr(&mut kp.dom, node, "checked", "");
                }
            }
        }
    }
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
fn fetch_and_parse(url: &str) -> (html::Dom, String, Vec<css::FontFace>) {
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

    // Phase 1b: collect every stylesheet in document order — <style> blocks
    // (usually in <head>, which body_root skips) and external <link>s — then
    // parse ONCE into precompiled selectors.
    let mut sources = Vec::new();
    collect_sheets(&dom, 0, &mut sources);
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
    (dom, css_text, faces)
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
) -> (Page, HashMap<u32, String>) {
    let root = dom.body_root();
    // media queries evaluate against the window's CSS pixel width
    let viewport = max_w + 2.0 * PADDING as f32;
    let mut sheet = css::parse_for(css_text, viewport);
    // viewport units (vh/vw/vmin/vmax) resolve against the real window size
    sheet.set_viewport(viewport, vh);
    eprintln!("[css] {} rules", sheet.rule_count());
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
    fonts.begin_page(owned_faces);

    // cascade over the body element itself: page bg + initial inherited style
    // (spec: the canvas background comes from <body> if it has one, else
    // <html>'s — body wins per the HTML spec)
    let body = sheet.resolve(dom, root, Style::new(), dom.attr(root, "style"));
    let bg = body
        .bg
        .or_else(|| sheet.resolve(dom, html_el, Style::new(), dom.attr(html_el, "style")).bg)
        .unwrap_or(BG);

    let mut root_box = layout::build(dom, root, body.style, body.bx, &sheet);
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

    // layout_in_forms applies `state` (focus + typed values) so the caret,
    // accent border and live text paint correctly
    let mut laid = layout::layout_in_forms(&mut root_box, max_w, vh, &|s: &str, st: &Style| fonts.measure(s, st), state);
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
    let form_names = collect_form_names(dom);
    let page = Page {
        url: url.to_string(),
        laid,
        bg,
    };
    (page, form_names)
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
            .build()
    })
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
    let mut req = http_agent()
        .get(url)
        .set("User-Agent", "Mazda/0.1 (minimal Rust browser)");
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
    /// interned font-family stacks (shared with the cascade via Sheet)
    stacks: css::StackTable,
    /// `@font-face` faces declared by the CURRENT page (cleared on navigation)
    faces: Vec<PageFace>,
    /// one lazy slot per SYSTEM_FAMILIES entry
    sys: Vec<std::sync::OnceLock<Option<FontVec>>>,
}

/// One page's `@font-face` definition with its lazily-fetched font.
struct PageFace {
    family: String, // lowercased, as parsed
    wmin: u16,
    wmax: u16,
    italic: bool,
    srcs: Vec<String>, // absolute URLs in declared order
    font: std::sync::OnceLock<Option<FontVec>>,
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
        }
    }

    /// Page-scoped font state: drop the previous page's webfonts and stack
    /// ids (RAM freed on navigation), register the new page's `@font-face`
    /// defs — each font itself still loads lazily on first use.
    fn begin_page(&mut self, faces: Vec<css::FontFace>) {
        self.faces = faces
            .into_iter()
            .map(|f| PageFace {
                family: f.family,
                wmin: f.wmin,
                wmax: f.wmax,
                italic: f.italic,
                srcs: f.srcs,
                font: std::sync::OnceLock::new(),
            })
            .collect();
        let mut t = self.stacks.borrow_mut();
        t.clear();
        t.push(Vec::new()); // id 0 = "no stack"
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

    /// Font that actually has a glyph for `ch`: the element's `font-family`
    /// stack in CSS order (webfonts + installed aliases), else the primary
    /// style font, else the lazily-loaded CJK/Hangul fallback, else primary
    /// (renders .notdef).
    /// Range-gated so Thai/Arabic/etc. (no fallback font for them) never pay
    /// the font-load cost just to learn there's no glyph.
    /// shortcut: fallback is always the *regular* weight — no bold/italic CJK.
    fn pick_font_for(&self, ch: char, style: &Style) -> &FontVec {
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

    /// One family name from a stack: the page's `@font-face` webfont for it
    /// (fetched+decoded once, disk-cached), else a known installed alias.
    /// None → the caller continues to the next name in the stack.
    fn stack_font(&self, name: &str, style: &Style, ch: char) -> Option<&FontVec> {
        // best face for this weight/italic, else the family's regular face
        let want: u16 = if style.bold { 700 } else { 400 };
        let face = self
            .faces
            .iter()
            .find(|f| {
                f.family == name && f.italic == style.italic && f.wmin <= want && want <= f.wmax
            })
            .or_else(|| self.faces.iter().find(|f| f.family == name && !f.italic));
        if let Some(def) = face {
            if let Some(f) = def.font.get_or_init(|| load_webfont(&def.srcs)) {
                if f.glyph_id(ch).0 != 0 {
                    return Some(f);
                }
            }
        }
        let idx = SYSTEM_FAMILIES.iter().position(|(n, _)| *n == name)?;
        if let Some(f) = self.sys[idx].get_or_init(|| try_load_font(SYSTEM_FAMILIES[idx].1)) {
            if f.glyph_id(ch).0 != 0 {
                return Some(f);
            }
        }
        None
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

/// Fetch + decode one webfont (`@font-face` src list, declared order).
/// Disk-cached like any resource; containers: raw sfnt + WOFF1 via
/// `woff::decode_font` (WOFF2 → None → try the next src / fall back).
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
            let (sx, sy) = (PADDING as f32 + s.x - pad, PADDING as f32 + top + y0 - pad);
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
            let (sx, sy) = (PADDING as f32 + x, (PADDING as f32 + top + y).max(0.0));
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
                PADDING as f32 + top + y,
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
            let (sx, sy) = (PADDING as f32 + x, PADDING as f32 + top + y);
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
    fn pick_font_for_honors_stack_order() {
        let mut fonts = Fonts::load();
        // unknown family + generic → walk continues to the generic fallback
        fonts.stacks.borrow_mut().push(vec![
            "definitely-not-a-font-x".to_string(),
            "serif".to_string(),
        ]);
        let mut st = Style::new();
        st.fam_stack = 1;
        assert!(std::ptr::eq(fonts.pick_font_for('R', &st), fonts.pick(&st)));
        // an installed alias in the stack beats the generic pick (HN: Verdana)
        fonts.stacks.borrow_mut().push(vec!["verdana".to_string()]);
        st.fam_stack = 2;
        let f = fonts.pick_font_for('R', &st);
        assert!(
            !std::ptr::eq(f, fonts.pick(&st)),
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
            font: std::sync::OnceLock::from(Some(pre)),
        });
        fonts.stacks.borrow_mut().push(vec!["testface".to_string()]);
        let mut st = Style::new();
        st.fam = Fam::Serif; // generic pick would be Georgia
        st.fam_stack = 1;
        let f = fonts.pick_font_for('R', &st);
        assert!(
            std::ptr::eq(f, fonts.faces[0].font.get().unwrap().as_ref().unwrap()),
            "the page's webfont wins over the generic"
        );
    }

    #[test]
    fn webfont_fetch_failure_falls_through_stack() {
        let mut fonts = Fonts::load();
        fonts.faces.push(PageFace {
            family: "ghost".to_string(),
            wmin: 400,
            wmax: 400,
            italic: false,
            srcs: vec!["https://invalid.invalid/f.woff".to_string()],
            font: std::sync::OnceLock::new(),
        });
        fonts.stacks.borrow_mut().push(vec!["ghost".to_string()]);
        let st = Style::new();
        // unreachable src → old behaviour: the primary generic pick
        assert!(std::ptr::eq(fonts.pick_font_for('R', &st), fonts.pick(&st)));
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
            }],
            height: 123.0,
            underline_rects: vec![(1.0, 2.0, 3.0, 4.0, 0xff0000, false)],
            rects: vec![(1.0, 2.0, 3.0, 4.0, 0x00ff00, 2.0, true)],
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
            }],
            open_line: true,
            forms: Vec::new(),
            focused: None,
            form_values: Default::default(),
        };
        let page = Page {
            url: "https://example.com/".into(),
            laid,
            bg: 0xabcdef,
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
            kind: "click".into(),
            value: None,
            forms: layout::FormState::default(),
        };
        let re = LoadReq::Relayout {
            id: 4,
            url: "https://example.com/".into(),
            max_w: 100.0,
            vh: 200.0,
            forms: layout::FormState::default(),
        };
        assert_eq!(ev.id(), 3);
        assert_eq!(re.id(), 4);
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
}

