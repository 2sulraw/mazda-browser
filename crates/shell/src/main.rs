//! P1 browser shell: the trusted side of the process split.
//!
//! It owns the window, the tabs and the input; it supervises exactly one
//! `renderer` process, forwards commands over stdin and paints the
//! double-buffered frame the renderer publishes in shared memory.
//!
//! Tabs are shell state: a tab is a URL, a label and a scroll offset. Only the
//! active tab has a page in the renderer, so the other nine are hibernated by
//! construction — there is no engine heap to unload. Switching tabs replays
//! the tab's DOM snapshot in the same renderer process.
//!
//! If the renderer dies, it is restarted and the active tab is replayed from
//! its snapshot. A small memory governor samples the renderer and reclaims it
//! (restart + snapshot restore) when it goes over budget.
//!
//! For measurement the shell also accepts line commands on stdin:
//! `tab <n>`, `open <url>`, `close <n>`, `reload`, `status`, `quit`. It answers
//! on stdout with `[tabs]`, `[page]`, `[gov]` and `[status]` lines.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, ViewportCommand,
};
use mazda_ipc::{Command, Event};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How many times a renderer may be restarted for crashes before the shell
/// gives up. Reset by every successful page (`title` event), so a crash loop
/// on one broken page cannot spin forever. Governor reclaims do not count.
const MAX_RESTARTS: u32 = 5;
const GOVERNOR_PERIOD: Duration = Duration::from_secs(3);
const RECLAIM_COOLDOWN: Duration = Duration::from_secs(60);

// Custom chrome. The window has no OS decorations: the titlebar, tabs and
// window buttons are ours, so the tab strip can live where the title goes.
const TITLE_H: f32 = 40.0;
const TOOLBAR_H: f32 = 44.0;
const STATUS_H: f32 = 22.0;
const WIN_BTN_W: f32 = 46.0;
const LOGO_W: f32 = 44.0;

const TITLE_BG: Color32 = Color32::from_rgb(0xDE, 0xE1, 0xE6);
const TOOLBAR_BG: Color32 = Color32::from_rgb(0xF1, 0xF3, 0xF4);
const STATUS_BG: Color32 = Color32::from_rgb(0xF1, 0xF3, 0xF4);
const TAB_ACTIVE: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
const TAB_HOVER: Color32 = Color32::from_rgb(0xD0, 0xD4, 0xD9);
const TAB_CLOSE_HOVER: Color32 = Color32::from_rgb(0xC2, 0xC6, 0xCB);
const WIN_HOVER: Color32 = Color32::from_rgb(0xCB, 0xD0, 0xD6);
const CLOSE_RED: Color32 = Color32::from_rgb(0xE8, 0x11, 0x23);
const PILL_BG: Color32 = Color32::from_rgb(0xE6, 0xE9, 0xED);
const PILL_HOVER: Color32 = Color32::from_rgb(0xD8, 0xDC, 0xE1);
const BORDER: Color32 = Color32::from_rgb(0xCF, 0xD3, 0xD8);
const TEXT: Color32 = Color32::from_rgb(0x20, 0x21, 0x24);
const MUTED: Color32 = Color32::from_rgb(0x5F, 0x63, 0x68);
const ACCENT: Color32 = Color32::from_rgb(0x1A, 0x73, 0xE8);

// ---------------------------------------------------------------------------
// Renderer supervisor

struct Renderer {
    proc: mazda_sandbox::Process,
    stdin: std::fs::File,
    rx: Receiver<Event>,
    exe: PathBuf,
    /// directory granted to the renderer (`tmp` = its `%TEMP%`); uploads are
    /// copied here so the renderer can read them
    sandbox: PathBuf,
    /// page the supervisor replays after a crash
    url: String,
    restarts: u32,
}

impl Renderer {
    fn spawn(exe: PathBuf, initial: Option<&str>) -> Option<Self> {
        // the renderer never sees the real profile: its frame file and cache
        // live in this directory, which the sandbox grants it access to
        let dir = std::env::temp_dir().join(format!("mazda-sandbox-{}", std::process::id()));
        if mazda_sandbox::prepare_dir(&dir).is_err() {
            eprintln!("[sandbox] cannot prepare {}", dir.display());
            return None;
        }
        let tmp = dir.join("tmp");
        let local = dir.join("localappdata");
        std::fs::create_dir_all(&tmp).ok()?;
        std::fs::create_dir_all(&local).ok()?;

        let mut args: Vec<OsString> = Vec::new();
        if let Some(url) = initial {
            args.push(url.into());
        }
        let env = [
            ("TMP", tmp.as_os_str().to_owned()),
            ("TEMP", tmp.as_os_str().to_owned()),
            ("LOCALAPPDATA", local.as_os_str().to_owned()),
        ];
        let memory_mb = std::env::var("MAZDA_RENDERER_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(512);
        let limits = mazda_sandbox::Limits {
            memory_mb,
            allow_network: true,
        };
        let sandboxed = std::env::var("MAZDA_SANDBOX").map(|v| v != "off").unwrap_or(true);
        let child = match if sandboxed {
            mazda_sandbox::spawn(&exe, &args, &env, &limits)
        } else {
            mazda_sandbox::spawn_plain(&exe, &args, &env)
        } {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[sandbox] spawn failed: {e}");
                return None;
            }
        };
        let (stdin, stdout, stderr, proc) = child.into_parts();
        eprintln!(
            "[sandbox] renderer pid {} ({})",
            proc.id(),
            if !sandboxed {
                "MAZDA_SANDBOX=off"
            } else if mazda_sandbox::available() {
                "app container + job object"
            } else {
                "unavailable on this platform"
            }
        );

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Some(ev) = Event::decode(&line) else {
                    continue;
                };
                if tx.send(ev).is_err() {
                    break;
                }
            }
            // EOF means the renderer is gone; the supervisor decides what next
            let _ = tx.send(Event::Exited);
        });
        // the renderer's stderr is its diagnostic stream; forward it verbatim
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                eprintln!("{line}");
            }
        });

        Some(Self {
            proc,
            stdin,
            rx,
            exe,
            sandbox: dir,
            url: initial.unwrap_or("").to_string(),
            restarts: 0,
        })
    }

    fn pid(&self) -> u32 {
        self.proc.id()
    }

    fn send(&mut self, cmd: Command) {
        let _ = writeln!(self.stdin, "{}", cmd.encode());
        let _ = self.stdin.flush();
    }

    /// Restart the renderer without an initial load; the caller sends the
    /// resize + restore commands once it knows the current viewport.
    fn restart(&mut self) -> bool {
        if self.restarts >= MAX_RESTARTS {
            return false;
        }
        self.proc.kill();
        self.proc.wait();
        let Some(mut fresh) = Self::spawn(self.exe.clone(), None) else {
            return false;
        };
        fresh.url = self.url.clone();
        fresh.restarts = self.restarts + 1;
        *self = fresh;
        true
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let _ = self.send(Command::Quit);
        self.proc.kill();
        self.proc.wait();
    }
}

// ---------------------------------------------------------------------------
// Tabs

struct Tab {
    url: String,
    label: String,
    scroll: f32,
}

impl Tab {
    fn new(url: &str) -> Self {
        let url = url.trim().to_string();
        Self {
            label: short_title(&url),
            url,
            scroll: 0.0,
        }
    }
}

/// One download: the renderer streams it into its sandbox temp dir; the shell
/// moves the finished file into the user's Downloads folder.
struct Download {
    name: String,
    done: u64,
    total: u64,
    state: DlState,
}

enum DlState {
    Active,
    Done(std::path::PathBuf),
    Failed(String),
}

/// `%USERPROFILE%\Downloads` when it exists, else the system temp dir.
fn downloads_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .map(|p| p.join("Downloads"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// `dir/name`, `dir/name (1)`, ... so an existing file is never overwritten.
fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let mut path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 1..10_000 {
        path = dir.join(format!("{stem} ({n}){ext}"));
        if !path.exists() {
            return path;
        }
    }
    path
}

/// Move the renderer's temp file out of the sandbox. Rename is atomic on the
/// same volume; copy+remove covers a temp dir on another drive.
fn adopt_download(src: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let dir = downloads_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = src.file_name().and_then(|s| s.to_str()).unwrap_or("download");
    let dst = unique_path(&dir, name);
    match std::fs::rename(src, &dst) {
        Ok(()) => Ok(dst),
        Err(_) => {
            std::fs::copy(src, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
            let _ = std::fs::remove_file(src);
            Ok(dst)
        }
    }
}

// ---------------------------------------------------------------------------
// File upload
//
// `<input type=file>` is clicked inside the renderer, but the OS picker runs
// here in the trusted shell. The chosen file is copied into the renderer's
// sandbox temp dir (the only place it may read) and its path travels back over
// the command channel.

/// Open the native Open dialog and return what the user chose. Runs on the UI
/// thread; `comdlg32` blocks here until the dialog closes, which is normal for
/// a picker.
#[cfg(windows)]
fn pick_file() -> Option<PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    // filter pairs are NUL separated and the list ends with a second NUL
    let filter: Vec<u16> = "All Files\0*.*\0\0".encode_utf16().collect();
    let mut buf = [0u16; 1024];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    // the click came from the shell window, so it is the foreground window
    ofn.hwndOwner = unsafe { GetForegroundWindow() };
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

#[cfg(not(windows))]
fn pick_file() -> Option<PathBuf> {
    eprintln!("[form] the OS file picker is Windows-only for now");
    None
}

/// Copy a picked file into the renderer's sandbox. The renderer cannot open
/// the user profile, so the bytes have to cross the boundary as a copy.
fn stage_upload(src: &Path, uploads: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(uploads).map_err(|e| format!("{}: {e}", uploads.display()))?;
    let name = src.file_name().and_then(|s| s.to_str()).unwrap_or("upload");
    let dst = unique_path(uploads, name);
    std::fs::copy(src, &dst).map_err(|e| format!("{}: {e}", src.display()))?;
    Ok(dst)
}

/// Short tab-label from a URL: host minus `www.`, or `New tab`.
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

/// Accept bare hosts and `host/path` the way an address bar should.
fn normalize(input: &str) -> String {
    let s = input.trim();
    if s.is_empty() || s.contains("://") || s.starts_with("about:") || s.starts_with("data:") {
        return s.to_string();
    }
    if s.starts_with("localhost") || s.parse::<std::net::IpAddr>().is_ok() {
        return format!("http://{s}");
    }
    if s.contains('.') && !s.contains(' ') {
        return format!("https://{s}");
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// Control channel (stdin) — used by the measurement tools

enum Ctl {
    Tab(usize),
    Open(String),
    Close(usize),
    Reload,
    Status,
    Quit,
}

impl Ctl {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        match head {
            "tab" => rest.parse().ok().map(Ctl::Tab),
            "open" if !rest.is_empty() => Some(Ctl::Open(rest.to_string())),
            "close" => rest.parse().ok().map(Ctl::Close),
            "reload" => Some(Ctl::Reload),
            "status" => Some(Ctl::Status),
            "quit" => Some(Ctl::Quit),
            _ => None,
        }
    }
}

fn spawn_control() -> Receiver<Ctl> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let Some(c) = Ctl::parse(&line) else { continue };
            if tx.send(c).is_err() {
                break;
            }
        }
    });
    rx
}

// ---------------------------------------------------------------------------
// Shell

struct Shell {
    r: Renderer,
    tabs: Vec<Tab>,
    active: usize,
    map: Option<mazda_ipc::FrameMap>,
    map_path: String,
    tex: Option<egui::TextureHandle>,
    /// reusable RGBA scratch buffer for texture uploads
    rgba: Vec<u8>,
    seq: u32,
    scroll: f32,
    /// page zoom factor (Ctrl+= / Ctrl+- / Ctrl+0)
    zoom: f32,
    /// time the last scroll command was written; a frame arriving after it
    /// measures the round trip
    sent: Option<Instant>,
    lat: Vec<f32>,
    /// downloads this session (active, saved, or failed)
    downloads: Vec<Download>,
    downloads_open: bool,
    /// find-in-page bar (Ctrl+F) and the last result the renderer reported
    find_open: bool,
    find_query: String,
    find_active: u32,
    find_total: u32,
    /// focus the find box on the next frame (opened or after Enter)
    find_focus: bool,
    title: String,
    err: Option<String>,
    /// content size last sent to the renderer
    size: (usize, usize),
    /// set after a restart: replay this page once the new renderer is sized
    pending_restore: Option<String>,
    /// scroll to apply when the next `title` lands (tab switch / restore)
    pending_scroll: Option<f32>,
    /// a page load is in flight
    loading: bool,
    url_edit: String,
    url_focused: bool,
    focus_url: bool,
    ctl: Receiver<Ctl>,
    /// Requests written to the renderer, oldest first. The renderer answers
    /// one `title`/`nav` per request in order, so an event belongs to the
    /// front entry even when the user switched tabs while it was in flight.
    /// Without this, a slow load's title landed on whichever tab was active
    /// when it finished (the one-tab lag measured by `measure-tabs.ps1`).
    inflight: VecDeque<(usize, String)>,
    // governor
    budget_mb: f64,
    last_sample: Instant,
    last_mb: f64,
    over: u32,
    last_reclaim: Instant,
}

impl Shell {
    fn new(tabs: Vec<Tab>, active: usize) -> Option<Self> {
        let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        let exe = dir.join(if cfg!(windows) { "renderer.exe" } else { "renderer" });
        let mut tabs = tabs;
        if tabs.is_empty() {
            tabs.push(Tab::new(""));
        }
        let active = active.min(tabs.len() - 1);
        let start = tabs[active].url.clone();
        let r = Renderer::spawn(exe, (!start.is_empty()).then_some(start.as_str()))?;
        let budget_mb = std::env::var("MAZDA_BUDGET_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(280.0);
        let url_edit = tabs[active].url.clone();
        let mut inflight = VecDeque::new();
        if !start.is_empty() {
            // `Renderer::spawn` already asked for this page
            inflight.push_back((active, start.clone()));
        }
        Some(Self {
            r,
            tabs,
            active,
            inflight,
            map: None,
            map_path: String::new(),
            tex: None,
            rgba: Vec::new(),
            seq: 0,
            scroll: 0.0,
            zoom: 1.0,
            sent: None,
            lat: Vec::new(),
            downloads: Vec::new(),
            downloads_open: false,
            find_open: false,
            find_query: String::new(),
            find_active: 0,
            find_total: 0,
            find_focus: false,
            title: String::new(),
            err: None,
            size: (0, 0),
            pending_restore: None,
            pending_scroll: None,
            loading: !url_edit.is_empty(),
            url_edit,
            url_focused: false,
            focus_url: false,
            ctl: spawn_control(),
            budget_mb,
            last_sample: Instant::now(),
            last_mb: 0.0,
            over: 0,
            // allow the first reclaim immediately (tests set a tiny budget)
            last_reclaim: Instant::now() - RECLAIM_COOLDOWN,
        })
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn save_scroll(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.scroll = self.scroll;
        }
    }

    /// Close the find bar and drop every highlight in the renderer.
    fn close_find(&mut self) {
        if self.find_open {
            self.find_open = false;
            self.find_query.clear();
            self.find_active = 0;
            self.find_total = 0;
            self.r.send(Command::FindClose);
        }
    }

    /// Load `url` in the active tab (fresh fetch).
    fn navigate(&mut self, url: String) {
        self.navigate_tab(self.active, url);
    }

    /// Load `url` in tab `tab`. Events come back in request order, so the
    /// request is queued for [`Shell::drain`] to attribute correctly even if
    /// the user switches away before the page finishes.
    fn navigate_tab(&mut self, tab: usize, url: String) {
        let url = normalize(&url);
        if url.is_empty() {
            return;
        }
        let active = tab == self.active;
        if active {
            self.save_scroll();
        }
        if let Some(t) = self.tabs.get_mut(tab) {
            t.url = url.clone();
            t.label = short_title(&url);
            t.scroll = 0.0;
        }
        if active {
            self.url_edit = url.clone();
            self.scroll = 0.0;
            self.pending_scroll = None;
            self.loading = true;
            self.err = None;
            self.tex = None;
            self.r.url = url.clone();
            self.title.clear();
            println!("[nav] {url}");
        }
        self.inflight.push_back((tab, url.clone()));
        self.r.send(Command::Load(url));
    }

    /// Switch to tab `i`, replaying its snapshot in the renderer.
    fn select_tab(&mut self, i: usize) {
        if i >= self.tabs.len() || i == self.active {
            return;
        }
        self.save_scroll();
        self.active = i;
        let (url, scroll) = {
            let t = &self.tabs[i];
            (t.url.clone(), t.scroll)
        };
        self.url_edit = url.clone();
        self.scroll = scroll;
        self.pending_scroll = None;
        self.loading = false;
        self.err = None;
        self.tex = None;
        self.title.clear();
        self.r.url = url.clone();
        if url.is_empty() {
            self.pending_scroll = None;
        } else {
            self.loading = true;
            self.inflight.push_back((i, url.clone()));
            self.r.send(Command::Restore(url.clone()));
            self.pending_scroll = Some(scroll);
        }
        println!(
            "[tabs] active={} n={} url={}",
            self.active,
            self.tabs.len(),
            url
        );
    }

    fn new_tab(&mut self) {
        self.save_scroll();
        self.tabs.push(Tab::new(""));
        let i = self.tabs.len() - 1;
        self.active = i;
        self.scroll = 0.0;
        self.pending_scroll = None;
        self.loading = false;
        self.tex = None;
        self.url_edit.clear();
        self.focus_url = true;
        println!("[tabs] active={} n={} url=", self.active, self.tabs.len());
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::new(""));
        }
        let was_active = i == self.active;
        if i < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len() - 1);
        if was_active {
            let (url, scroll) = {
                let t = &self.tabs[self.active];
                (t.url.clone(), t.scroll)
            };
            self.url_edit = url.clone();
            self.scroll = scroll;
            self.tex = None;
            self.r.url = url.clone();
            if url.is_empty() {
                self.loading = false;
                self.pending_scroll = None;
            } else {
                self.loading = true;
                self.title.clear();
                self.inflight.push_back((self.active, url.clone()));
                self.r.send(Command::Restore(url));
                self.pending_scroll = Some(scroll);
            }
        }
        println!(
            "[tabs] active={} n={} url={}",
            self.active,
            self.tabs.len(),
            self.active_tab().url
        );
    }

    fn reload(&mut self) {
        let url = self.active_tab().url.clone();
        if !url.is_empty() {
            self.navigate(url);
        }
    }

    fn apply_pending_scroll(&mut self) {
        if let Some(s) = self.pending_scroll.take() {
            if s > 0.0 {
                self.scroll = s;
                self.r.send(Command::Scroll(s));
            }
        }
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.r.rx.try_recv() {
            match ev {
                Event::Frame {
                    path, width, height, seq, ..
                } => {
                    if seq == self.seq {
                        continue;
                    }
                    let path = path.display().to_string();
                    if path != self.map_path {
                        self.map = mazda_ipc::FrameMap::open(std::path::Path::new(&path)).ok();
                        self.map_path = path;
                        self.tex = None;
                    }
                    if let Some(m) = self.map.as_ref() {
                        if m.dims() == (width, height) {
                            self.seq = seq;
                            // the pixels changed: re-upload on the next frame
                            self.tex = None;
                            if let Some(t0) = self.sent.take() {
                                self.lat.push(t0.elapsed().as_secs_f32() * 1000.0);
                                // keep the p50 display bounded in a long session
                                if self.lat.len() > 512 {
                                    self.lat.remove(0);
                                }
                            }
                        }
                    }
                }
                Event::Title(t) => {
                    self.r.restarts = 0;
                    // the renderer answers requests in order: this title
                    // belongs to the oldest request, not necessarily to the
                    // tab the user is looking at now
                    let owner = self
                        .inflight
                        .pop_front()
                        .map(|(i, _)| i)
                        .unwrap_or(self.active);
                    if let Some(tab) = self.tabs.get_mut(owner) {
                        tab.url = t.clone();
                        tab.label = short_title(&t);
                    }
                    if owner == self.active {
                        self.loading = false;
                        self.err = None;
                        self.title = t.clone();
                        self.r.url = t.clone();
                        if !self.url_focused {
                            self.url_edit = t.clone();
                        }
                        self.apply_pending_scroll();
                    }
                    println!("[page] tab={owner} {t}");
                }
                Event::Nav(url) => {
                    // a script navigation belongs to the page that caused it;
                    // the request it replaces will not emit a title
                    let owner = self
                        .inflight
                        .pop_front()
                        .map(|(i, _)| i)
                        .unwrap_or(self.active);
                    self.navigate_tab(owner, url);
                }
                Event::Error(e) => {
                    let owner = self
                        .inflight
                        .pop_front()
                        .map(|(i, _)| i)
                        .unwrap_or(self.active);
                    if owner == self.active {
                        self.loading = false;
                        self.err = Some(e.clone());
                    }
                    println!("[error] tab={owner} {e}");
                }
                Event::Download { done, total, name } => {
                    match self
                        .downloads
                        .iter_mut()
                        .find(|d| d.name == name && matches!(d.state, DlState::Active))
                    {
                        Some(d) => {
                            d.done = done;
                            d.total = total;
                        }
                        None => self.downloads.push(Download {
                            name: name.clone(),
                            done,
                            total,
                            state: DlState::Active,
                        }),
                    }
                    println!("[download] {name} {done}/{total}");
                }
                Event::DownloadDone { path } => {
                    let name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("download")
                        .to_string();
                    let state = match adopt_download(&path) {
                        Ok(p) => {
                            println!("[download] saved {}", p.display());
                            DlState::Done(p)
                        }
                        Err(e) => {
                            println!("[download] move failed: {e}");
                            DlState::Failed(e)
                        }
                    };
                    match self.downloads.iter_mut().find(|d| d.name == name) {
                        Some(d) => d.state = state,
                        None => self.downloads.push(Download {
                            name,
                            done: 0,
                            total: 0,
                            state,
                        }),
                    }
                }
                Event::DownloadFailed { name, err } => {
                    println!("[download] {name}: {err}");
                    match self.downloads.iter_mut().find(|d| d.name == name) {
                        Some(d) => d.state = DlState::Failed(err),
                        None => self.downloads.push(Download {
                            name,
                            done: 0,
                            total: 0,
                            state: DlState::Failed(err),
                        }),
                    }
                }
                Event::PickFile { node } => {
                    let uploads = self.r.sandbox.join("tmp").join("mazda-uploads");
                    match pick_file() {
                        Some(src) => match stage_upload(&src, &uploads) {
                            Ok(dst) => {
                                let name = src
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                println!(
                                    "[form] picked {} -> {} for node {node}",
                                    src.display(),
                                    dst.display()
                                );
                                self.r.send(Command::FileChosen {
                                    node,
                                    name,
                                    path: dst.to_string_lossy().into_owned(),
                                });
                            }
                            Err(e) => println!("[form] upload copy failed: {e}"),
                        },
                        None => println!("[form] file pick cancelled"),
                    }
                }
                Event::FindResult { active, total } => {
                    self.find_active = active;
                    self.find_total = total;
                    println!("[find] {active}/{total}");
                }
                Event::ScrollTo(y) => {
                    // the renderer moved the page to a find hit; adopt the
                    // offset so tab switching remembers it
                    self.scroll = y.max(0.0);
                    self.save_scroll();
                }
                Event::Exited => {
                    if self.r.restart() {
                        self.err = Some(format!(
                            "renderer crashed — restoring from snapshot ({}/{})",
                            self.r.restarts, MAX_RESTARTS
                        ));
                        println!("[supervisor] restart {}", self.r.restarts);
                        self.map = None;
                        self.map_path.clear();
                        self.tex = None;
                        self.seq = 0;
                        // the restarted renderer is back at 100% zoom
                        self.r.send(Command::Zoom(self.zoom));
                        // the restarted renderer never saw the old commands
                        self.inflight.clear();
                        // force a resize (and then the restore) next frame
                        self.size = (0, 0);
                        let url = self.active_tab().url.clone();
                        self.pending_restore = (!url.is_empty()).then_some(url);
                        self.pending_scroll = Some(self.scroll);
                        self.loading = !self.pending_restore.is_none();
                    } else {
                        self.err = Some("renderer exited; restart limit reached".into());
                        println!("[supervisor] gave up after {} restarts", MAX_RESTARTS);
                        break;
                    }
                }
            }
        }
    }

    /// Restart the renderer to give its heap back, then replay the active tab
    /// from its snapshot. This is the governor's only real lever while one
    /// renderer serves every tab.
    fn reclaim(&mut self) {
        if !self.r.restart() {
            self.err = Some("memory governor could not restart the renderer".into());
            return;
        }
        // a reclaim is not a crash: it must not eat the crash budget
        self.r.restarts = 0;
        self.map = None;
        self.map_path.clear();
        self.tex = None;
        self.seq = 0;
        self.size = (0, 0);
        self.inflight.clear();
        self.r.send(Command::Zoom(self.zoom));
        let url = self.active_tab().url.clone();
        self.pending_restore = (!url.is_empty()).then_some(url);
        self.pending_scroll = Some(self.scroll);
        self.loading = true;
        println!("[gov] reclaimed renderer");
    }

    fn govern(&mut self) {
        if self.last_sample.elapsed() < GOVERNOR_PERIOD {
            return;
        }
        self.last_sample = Instant::now();
        let Some(mb) = process_private_mb(self.r.pid()) else {
            return;
        };
        self.last_mb = mb;
        if mb > self.budget_mb {
            self.over += 1;
            if self.over >= 2 && !self.loading && self.last_reclaim.elapsed() > RECLAIM_COOLDOWN {
                self.over = 0;
                self.last_reclaim = Instant::now();
                println!("[gov] renderer {mb:.1} MB > budget {:.0} MB", self.budget_mb);
                self.reclaim();
            }
        } else {
            self.over = 0;
        }
    }

    fn control(&mut self, ctx: &egui::Context) {
        while let Ok(c) = self.ctl.try_recv() {
            match c {
                Ctl::Tab(i) => self.select_tab(i),
                Ctl::Open(url) => {
                    self.save_scroll();
                    self.tabs.push(Tab::new(&url));
                    self.active = self.tabs.len() - 1;
                    self.tex = None;
                    self.scroll = 0.0;
                    self.pending_scroll = None;
                    self.loading = true;
                    self.title.clear();
                    self.r.url = self.tabs[self.active].url.clone();
                    self.inflight
                        .push_back((self.active, self.r.url.clone()));
                    self.r.send(Command::Load(self.tabs[self.active].url.clone()));
                    println!(
                        "[tabs] active={} n={} url={}",
                        self.active,
                        self.tabs.len(),
                        self.active_tab().url
                    );
                }
                Ctl::Close(i) => self.close_tab(i),
                Ctl::Reload => self.reload(),
                Ctl::Status => {
                    let shell = process_private_mb(std::process::id()).unwrap_or(0.0);
                    println!(
                        "[status] tabs={} active={} url={} shell_private={:.1} renderer_private={:.1}",
                        self.tabs.len(),
                        self.active,
                        self.active_tab().url,
                        shell,
                        self.last_mb
                    );
                }
                Ctl::Quit => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            ctx.request_repaint();
        }
    }

    fn upload(&mut self, ctx: &egui::Context) {
        let Some(m) = self.map.as_ref() else { return };
        let (w, h) = m.dims();
        let Some(hdr) = m.copy_rgba(&mut self.rgba) else {
            return;
        };
        let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &self.rgba);
        self.tex = Some(ctx.load_texture("page", img, egui::TextureOptions::LINEAR));
        self.seq = hdr.seq;
    }
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain();
        self.govern();
        self.control(ui.ctx());

        // actions collected from the chrome, applied after painting so the
        // tab list can be borrowed while the widgets are built
        let mut select = None;
        let mut close = None;
        let mut open_new = false;
        let mut go = None;
        let mut reload = false;

        let full = ui.max_rect();
        let title_rect = Rect::from_min_size(full.min, egui::vec2(full.width(), TITLE_H));
        let toolbar_rect = Rect::from_min_size(
            egui::pos2(full.left(), title_rect.bottom()),
            egui::vec2(full.width(), TOOLBAR_H),
        );
        let status_rect = Rect::from_min_size(
            egui::pos2(full.left(), full.bottom() - STATUS_H),
            egui::vec2(full.width(), STATUS_H),
        );
        let page_rect = Rect::from_min_max(
            egui::pos2(full.left(), toolbar_rect.bottom()),
            egui::pos2(full.right(), status_rect.top()),
        );

        let painter = ui.painter().clone();
        painter.rect_filled(title_rect, 0.0, TITLE_BG);
        painter.rect_filled(toolbar_rect, 0.0, TOOLBAR_BG);
        painter.rect_filled(status_rect, 0.0, STATUS_BG);
        painter.line_segment(
            [
                egui::pos2(full.left(), toolbar_rect.bottom() - 0.5),
                egui::pos2(full.right(), toolbar_rect.bottom() - 0.5),
            ],
            Stroke::new(1.0, BORDER),
        );

        // ---- window controls (rightmost, like a native titlebar) -------
        let win_left = title_rect.right() - WIN_BTN_W * 3.0;
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        let win_min = win_button(ui, title_rect, win_left, "win-min");
        let win_max = win_button(ui, title_rect, win_left + WIN_BTN_W, "win-max");
        let win_close = win_button(ui, title_rect, win_left + WIN_BTN_W * 2.0, "win-close");

        // ---- draggable strip: logo area and the gap right of the tabs ---
        let logo_rect = Rect::from_min_size(
            title_rect.min,
            egui::vec2(LOGO_W, title_rect.height()),
        );
        let drag_logo = ui.interact(logo_rect, ui.id().with("drag-logo"), Sense::click_and_drag());
        if drag_logo.drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }

        // logo mark
        let mark = Rect::from_center_size(
            egui::pos2(logo_rect.center().x, title_rect.center().y),
            egui::vec2(22.0, 22.0),
        );
        painter.rect_filled(mark, 6.0, ACCENT);
        painter.text(
            mark.center(),
            Align2::CENTER_CENTER,
            "M",
            FontId::proportional(13.0),
            Color32::WHITE,
        );

        // ---- tabs -------------------------------------------------------
        let tabs_left = title_rect.left() + LOGO_W;
        let tabs_right = win_left - 4.0;
        let plus_w = 30.0;
        let avail = (tabs_right - tabs_left - plus_w).max(1.0);
        let n = self.tabs.len().max(1) as f32;
        let tab_w = (avail / n).clamp(58.0, 240.0);
        let mut x = tabs_left;
        for (i, t) in self.tabs.iter().enumerate() {
            if x >= tabs_right - 8.0 {
                break;
            }
            let r = Rect::from_min_max(
                egui::pos2(x, title_rect.top() + 6.0),
                egui::pos2((x + tab_w).min(tabs_right), title_rect.bottom() + 1.0),
            );
            let resp = ui.interact(r, ui.id().with(("tab", i)), Sense::click_and_drag());
            let active = i == self.active;
            let hovered = resp.hovered();
            let rounding = CornerRadius {
                nw: 8,
                ne: 8,
                sw: 0,
                se: 0,
            };
            if active {
                painter.rect_filled(r, rounding, TAB_ACTIVE);
            } else if hovered {
                painter.rect_filled(r, rounding, TAB_HOVER);
            }

            // close button: always on the active tab, on hover otherwise
            let close_center = egui::pos2(r.right() - 13.0, r.center().y);
            let close_rect = Rect::from_center_size(close_center, egui::vec2(19.0, 19.0));
            let close_resp =
                ui.interact(close_rect, ui.id().with(("tab-close", i)), Sense::click());
            // narrow tabs spend too much of their label on a permanent close
            // button: reserve it for the hovered tab instead
            let narrow = r.width() < 86.0;
            let show_close = (active && !narrow) || hovered || close_resp.hovered();
            if show_close {
                if close_resp.hovered() {
                    painter.circle_filled(close_center, 9.0, TAB_CLOSE_HOVER);
                }
                draw_x(
                    &painter,
                    close_center,
                    3.6,
                    if close_resp.hovered() { TEXT } else { MUTED },
                );
            }

            let label = if t.url.is_empty() {
                "New tab".to_string()
            } else {
                t.label.clone()
            };
            let font = FontId::proportional(12.5);
            let max_text = (r.width() - if show_close { 36.0 } else { 18.0 }).max(6.0);
            let shown = elide(ui, &label, &font, max_text);
            painter.text(
                egui::pos2(r.left() + 11.0, r.center().y),
                Align2::LEFT_CENTER,
                shown,
                font,
                if active { TEXT } else { MUTED },
            );

            if close_resp.clicked() || resp.middle_clicked() {
                close = Some(i);
            } else if resp.clicked() {
                select = Some(i);
            }
            if resp.drag_started() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }
            x += tab_w;
        }

        // new-tab button
        let plus_x = x.min(tabs_right - plus_w);
        let plus_rect = Rect::from_min_size(
            egui::pos2(plus_x + 2.0, title_rect.top() + 6.0),
            egui::vec2(26.0, title_rect.height() - 9.0),
        );
        let plus_resp = ui.interact(plus_rect, ui.id().with("new-tab"), Sense::click());
        if plus_resp.hovered() {
            painter.rect_filled(plus_rect, 7.0, TAB_HOVER);
        }
        let pc = plus_rect.center();
        painter.line_segment(
            [egui::pos2(pc.x - 6.0, pc.y), egui::pos2(pc.x + 6.0, pc.y)],
            Stroke::new(1.5, TEXT),
        );
        painter.line_segment(
            [egui::pos2(pc.x, pc.y - 6.0), egui::pos2(pc.x, pc.y + 6.0)],
            Stroke::new(1.5, TEXT),
        );
        if plus_resp.clicked() {
            open_new = true;
        }

        // gap between the plus button and the window controls is a drag area
        let gap = Rect::from_min_max(
            egui::pos2((plus_x + plus_w).min(tabs_right), title_rect.top()),
            egui::pos2(win_left, title_rect.bottom()),
        );
        if gap.width() > 4.0 {
            let drag_gap = ui.interact(gap, ui.id().with("drag-gap"), Sense::click_and_drag());
            if drag_gap.drag_started() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }
            if drag_gap.double_clicked() || drag_logo.double_clicked() {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
        }

        // window buttons: paint hover state, then the glyph
        if win_min.hovered() {
            painter.rect_filled(win_min.rect, 0.0, WIN_HOVER);
        }
        if win_max.hovered() {
            painter.rect_filled(win_max.rect, 0.0, WIN_HOVER);
        }
        if win_close.hovered() {
            painter.rect_filled(win_close.rect, 0.0, CLOSE_RED);
        }
        let glyph = if win_close.hovered() {
            Color32::WHITE
        } else {
            TEXT
        };
        draw_min(&painter, win_min.rect.center(), glyph);
        draw_max(
            &painter,
            win_max.rect.center(),
            glyph,
            maximized,
            if win_max.hovered() { WIN_HOVER } else { TITLE_BG },
        );
        draw_x(&painter, win_close.rect.center(), 5.0, glyph);
        if win_min.clicked() {
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::Minimized(true));
        }
        if win_max.clicked() {
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
        }
        if win_close.clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }

        // ---- toolbar: reload + address pill ----------------------------
        let reload_rect = Rect::from_center_size(
            egui::pos2(toolbar_rect.left() + 28.0, toolbar_rect.center().y),
            egui::vec2(34.0, 34.0),
        );
        let reload_resp = ui.interact(reload_rect, ui.id().with("reload"), Sense::click());
        if reload_resp.hovered() {
            painter.circle_filled(reload_rect.center(), 16.0, PILL_HOVER);
        }
        draw_reload(
            &painter,
            reload_rect.center(),
            6.5,
            if reload_resp.hovered() { TEXT } else { MUTED },
        );
        if reload_resp.clicked() {
            reload = true;
        }

        // downloads button sits at the toolbar's right edge; the address pill
        // ends before it
        let dl_rect = Rect::from_center_size(
            egui::pos2(toolbar_rect.right() - 27.0, toolbar_rect.center().y),
            egui::vec2(34.0, 34.0),
        );
        let pill = Rect::from_min_max(
            egui::pos2(reload_rect.right() + 10.0, toolbar_rect.top() + 7.0),
            egui::pos2(dl_rect.left() - 8.0, toolbar_rect.bottom() - 7.0),
        );
        painter.rect_filled(pill, 16.0, PILL_BG);
        draw_globe(
            &painter,
            egui::pos2(pill.left() + 17.0, pill.center().y),
            6.0,
            if self.loading { ACCENT } else { MUTED },
        );

        let text_rect = Rect::from_min_max(
            egui::pos2(pill.left() + 31.0, pill.top() + 1.0),
            egui::pos2(pill.right() - 36.0, pill.bottom() - 1.0),
        );
        let url_resp = ui.put(
            text_rect,
            egui::TextEdit::singleline(&mut self.url_edit)
                .frame(egui::Frame::NONE)
                .hint_text("Search or enter address"),
        );
        self.url_focused = url_resp.has_focus();
        if self.focus_url {
            url_resp.request_focus();
            self.focus_url = false;
        }
        if url_resp.has_focus() {
            painter.rect_stroke(
                pill,
                16.0,
                Stroke::new(1.5, ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        let enter = url_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

        let go_rect = Rect::from_center_size(
            egui::pos2(pill.right() - 19.0, pill.center().y),
            egui::vec2(28.0, 28.0),
        );
        let go_resp = ui.interact(go_rect, ui.id().with("go"), Sense::click());
        if go_resp.hovered() {
            painter.circle_filled(go_rect.center(), 13.0, PILL_HOVER);
        }
        painter.text(
            go_rect.center(),
            Align2::CENTER_CENTER,
            "→",
            FontId::proportional(16.0),
            if go_resp.hovered() { ACCENT } else { MUTED },
        );
        if go_resp.clicked() || enter {
            go = Some(self.url_edit.clone());
        }

        // ---- downloads button + active badge ---------------------------
        let dl_resp = ui.interact(dl_rect, ui.id().with("downloads"), Sense::click());
        if dl_resp.hovered() {
            painter.circle_filled(dl_rect.center(), 16.0, PILL_HOVER);
        }
        let dl_active = self
            .downloads
            .iter()
            .any(|d| matches!(d.state, DlState::Active));
        let dl_col = if dl_resp.hovered() || self.downloads_open || dl_active {
            ACCENT
        } else {
            MUTED
        };
        let dc = dl_rect.center();
        // arrow into a tray
        painter.line_segment(
            [egui::pos2(dc.x, dc.y - 7.0), egui::pos2(dc.x, dc.y + 3.0)],
            Stroke::new(1.6, dl_col),
        );
        painter.line_segment(
            [egui::pos2(dc.x - 4.5, dc.y - 1.5), egui::pos2(dc.x, dc.y + 3.0)],
            Stroke::new(1.6, dl_col),
        );
        painter.line_segment(
            [egui::pos2(dc.x + 4.5, dc.y - 1.5), egui::pos2(dc.x, dc.y + 3.0)],
            Stroke::new(1.6, dl_col),
        );
        painter.line_segment(
            [egui::pos2(dc.x - 6.5, dc.y + 7.0), egui::pos2(dc.x + 6.5, dc.y + 7.0)],
            Stroke::new(1.6, dl_col),
        );
        if dl_active {
            painter.circle_filled(egui::pos2(dc.x + 8.0, dc.y - 8.0), 3.0, ACCENT);
        }
        if dl_resp.clicked() {
            self.downloads_open = !self.downloads_open;
        }

        // ---- keyboard: the usual browser chords ------------------------
        let mut k_new = false;
        let mut k_close = false;
        let mut k_reload = false;
        let mut k_focus = false;
        let mut k_find = false;
        let mut k_zoom = 0i32;
        let mut k_zoom_reset = false;
        let mut k_next = 0i32;
        let mut k_select = None;
        ui.input(|i| {
            if !i.modifiers.command {
                return;
            }
            if i.key_pressed(egui::Key::T) {
                k_new = true;
            }
            if i.key_pressed(egui::Key::W) {
                k_close = true;
            }
            if i.key_pressed(egui::Key::R) {
                k_reload = true;
            }
            if i.key_pressed(egui::Key::L) {
                k_focus = true;
            }
            if i.key_pressed(egui::Key::F) {
                k_find = true;
            }
            if i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals) {
                k_zoom = 1;
            }
            if i.key_pressed(egui::Key::Minus) {
                k_zoom = -1;
            }
            if i.key_pressed(egui::Key::Num0) {
                k_zoom_reset = true;
            }
            if i.key_pressed(egui::Key::Tab) {
                k_next = if i.modifiers.shift { -1 } else { 1 };
            }
            let digits = [
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
                egui::Key::Num6,
                egui::Key::Num7,
                egui::Key::Num8,
                egui::Key::Num9,
            ];
            for (n, key) in digits.iter().enumerate() {
                if i.key_pressed(*key) {
                    k_select = Some(n);
                }
            }
        });

        // apply chrome actions before touching the page
        if let Some(i) = close {
            self.close_tab(i);
        }
        if let Some(i) = select {
            self.select_tab(i);
        }
        if open_new {
            self.new_tab();
        }
        if reload || k_reload {
            self.reload();
        }
        if let Some(url) = go {
            self.navigate(url);
        }
        if k_new {
            self.new_tab();
        }
        if k_close {
            self.close_tab(self.active);
        }
        if k_focus {
            self.focus_url = true;
        }
        if k_find {
            self.find_open = true;
            self.find_focus = true;
        }
        if self.find_open && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_find();
        }
        if k_zoom != 0 || k_zoom_reset {
            let z = if k_zoom_reset {
                1.0
            } else {
                zoom_step(self.zoom, k_zoom)
            };
            if (z - self.zoom).abs() > 0.001 {
                self.zoom = z;
                self.r.send(Command::Zoom(z));
            }
        }
        if k_next != 0 {
            let n = self.tabs.len() as i32;
            let next = (self.active as i32 + k_next).rem_euclid(n) as usize;
            self.select_tab(next);
        }
        if let Some(n) = k_select {
            if n < self.tabs.len() {
                self.select_tab(n);
            }
        }

        // ---- find bar (Ctrl+F) ------------------------------------------
        let mut find_rect: Option<Rect> = None;
        if self.find_open {
            let mut step = 0i32;
            let mut changed = false;
            let mut close_now = false;
            let bar_w = 318.0;
            let pos = egui::pos2(
                page_rect.right() - bar_w - 14.0,
                page_rect.top() + 10.0,
            );
            let shown = egui::Area::new(egui::Id::new("find-bar"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(bar_w);
                        ui.horizontal(|ui| {
                            let resp = ui.add_sized(
                                [180.0, 22.0],
                                egui::TextEdit::singleline(&mut self.find_query)
                                    .hint_text("Find in page"),
                            );
                            if self.find_focus {
                                resp.request_focus();
                                self.find_focus = false;
                            }
                            let count = if self.find_total == 0 {
                                "0/0".to_string()
                            } else {
                                format!("{}/{}", self.find_active, self.find_total)
                            };
                            ui.label(
                                egui::RichText::new(count).color(MUTED).size(11.5),
                            );
                            if ui.button("✕").clicked() {
                                close_now = true;
                            }
                            if resp.changed() {
                                changed = true;
                            }
                            // Enter steps forward, Shift+Enter back; the
                            // TextEdit gives up focus on Enter, so take it
                            // back for the next press
                            if ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && (resp.has_focus() || resp.lost_focus())
                            {
                                step =
                                    if ui.input(|i| i.modifiers.shift) { -1 } else { 1 };
                                self.find_focus = true;
                            }
                        });
                    });
                });
            find_rect = Some(shown.response.rect);
            if changed {
                self.r.send(Command::Find(self.find_query.clone()));
            }
            if step != 0 {
                self.r.send(Command::FindStep(step));
            }
            if close_now {
                self.close_find();
            }
        }

        // ---- page -------------------------------------------------------
        let page = !self.active_tab().url.is_empty();
        let size = (
            page_rect.width().max(1.0).round() as usize,
            page_rect.height().max(1.0).round() as usize,
        );
        if size != self.size {
            self.size = size;
            self.r.send(Command::Resize {
                width: size.0 as u32,
                height: size.1 as u32,
            });
        }
        // after a crash: the new renderer is sized, now replay the tab
        if let Some(url) = self.pending_restore.take() {
            self.inflight.push_back((self.active, url.clone()));
            self.r.send(Command::Restore(url));
        }

        if page && self.seq != 0 && self.tex.is_none() {
            self.upload(ui.ctx());
        }
        if page {
            if let Some(t) = &self.tex {
                painter.image(
                    t.id(),
                    page_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        } else {
            painter.rect_filled(page_rect, 0.0, Color32::from_gray(248));
            painter.text(
                page_rect.center(),
                Align2::CENTER_CENTER,
                "New tab — type an address",
                FontId::proportional(18.0),
                MUTED,
            );
        }

        // loading hint: a thin accent line under the toolbar
        if self.loading {
            let clip = Rect::from_min_size(
                page_rect.left_top(),
                egui::vec2(page_rect.width(), 2.0),
            );
            painter.rect_filled(clip, 0.0, ACCENT);
        }

        // input -> renderer (only over the page, never over the chrome)
        let hover = ui.input(|i| i.pointer.hover_pos());
        let over_page = hover.map(|p| page_rect.contains(p)).unwrap_or(false);
        let wheel = if over_page {
            ui.input(|i| i.smooth_scroll_delta().y)
        } else {
            0.0
        };
        let click = ui.input(|i| {
            i.pointer
                .primary_clicked()
                .then(|| i.pointer.interact_pos())
                .flatten()
                .filter(|p| page_rect.contains(*p))
        });
        if wheel != 0.0 && page {
            // the renderer scrolls in CSS pixels; the wheel arrives in
            // physical pixels
            self.scroll = (self.scroll - wheel / self.zoom).max(0.0);
            self.r.send(Command::Scroll(self.scroll));
            self.sent = Some(Instant::now());
        }
        if let Some(p) = click.filter(|_| page).filter(|p| {
            !find_rect.map(|r| r.contains(*p)).unwrap_or(false)
        }) {
            self.r.send(Command::Click {
                x: p.x - page_rect.min.x,
                y: p.y - page_rect.min.y,
            });
        }

        // ---- status bar -------------------------------------------------
        let active_dl = self
            .downloads
            .iter()
            .find(|d| matches!(d.state, DlState::Active));
        let left = if let Some(d) = active_dl {
            let pct = if d.total > 0 {
                format!("{}%", d.done * 100 / d.total.max(1))
            } else {
                format!("{} KB", d.done / 1024)
            };
            format!("Downloading {} — {pct}", d.name)
        } else if let Some(e) = &self.err {
            format!("⚠ {e}")
        } else if self.loading {
            format!("Loading…  {}", self.active_tab().url)
        } else if !self.title.is_empty() {
            self.title.clone()
        } else {
            self.active_tab().url.clone()
        };
        let status_font = FontId::proportional(11.5);
        let left = elide(ui, &left, &status_font, status_rect.width() * 0.62);
        painter.text(
            egui::pos2(status_rect.left() + 12.0, status_rect.center().y),
            Align2::LEFT_CENTER,
            left,
            status_font.clone(),
            if self.err.is_some() { CLOSE_RED } else { MUTED },
        );
        let mut right = format!("{}/{} tabs", self.active + 1, self.tabs.len());
        if (self.zoom - 1.0).abs() > 0.001 {
            right.push_str(&format!("   {:.0}%", self.zoom * 100.0));
        }
        if self.last_mb > 0.0 {
            right.push_str(&format!("   {:.0} MB", self.last_mb));
        }
        if !self.lat.is_empty() {
            let mut v = self.lat.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p50 = v[v.len() / 2];
            right.push_str(&format!("   p50 {p50:.1} ms"));
        }
        painter.text(
            egui::pos2(status_rect.right() - 12.0, status_rect.center().y),
            Align2::RIGHT_CENTER,
            right,
            status_font,
            MUTED,
        );

        // ---- downloads panel --------------------------------------------
        if self.downloads_open {
            let mut open = true;
            egui::Window::new("Downloads")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(380.0)
                .anchor(
                    Align2::RIGHT_TOP,
                    [-14.0, title_rect.height() + toolbar_rect.height() + 8.0],
                )
                .show(ui.ctx(), |ui| {
                    if self.downloads.is_empty() {
                        ui.label("Nothing downloaded yet.");
                    }
                    for d in self.downloads.iter().rev() {
                        ui.horizontal(|ui| match &d.state {
                            DlState::Active => {
                                let text = if d.total > 0 {
                                    format!("{}%", d.done * 100 / d.total.max(1))
                                } else {
                                    format!("{} KB", d.done / 1024)
                                };
                                let frac = if d.total > 0 {
                                    (d.done as f32 / d.total as f32).clamp(0.0, 1.0)
                                } else {
                                    0.0
                                };
                                ui.add(
                                    egui::ProgressBar::new(frac)
                                        .desired_width(150.0)
                                        .text(text),
                                );
                                ui.label(elide(ui, &d.name, &FontId::proportional(12.5), 190.0));
                            }
                            DlState::Done(p) => {
                                let shown = elide(ui, &d.name, &FontId::proportional(12.5), 240.0);
                                if ui
                                    .link(shown)
                                    .on_hover_text(p.display().to_string())
                                    .clicked()
                                {
                                    open_path(p);
                                }
                                ui.label(egui::RichText::new("saved").color(MUTED).size(11.0));
                            }
                            DlState::Failed(e) => {
                                ui.colored_label(CLOSE_RED, "⚠");
                                ui.label(elide(ui, &d.name, &FontId::proportional(12.5), 220.0));
                                ui.label(egui::RichText::new(e).color(MUTED).size(11.0));
                            }
                        });
                    }
                });
            self.downloads_open = open;
        }

        ui.ctx().request_repaint_after(Duration::from_millis(8));
    }
}

/// Next/previous browser-zoom step (Ctrl+= / Ctrl+-).
fn zoom_step(cur: f32, dir: i32) -> f32 {
    const STEPS: [f32; 17] = [
        0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0,
        4.0, 5.0,
    ];
    if dir > 0 {
        STEPS
            .iter()
            .copied()
            .find(|&s| s > cur + 0.001)
            .unwrap_or(5.0)
    } else {
        STEPS
            .iter()
            .rev()
            .copied()
            .find(|&s| s < cur - 0.001)
            .unwrap_or(0.25)
    }
}

/// Open a saved download with the platform's file handler (the trusted shell
/// process; the renderer never touches the profile).
fn open_path(path: &std::path::Path) {
    #[cfg(windows)]
    let cmd = "explorer.exe";
    #[cfg(not(windows))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

/// Truncate `text` with an ellipsis so it fits in `max_w`.
fn elide(ui: &egui::Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: &str| {
        ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE)
                .size()
                .x
        })
    };
    if width(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut n = chars.len();
    while n > 0 {
        let s: String = chars[..n].iter().collect::<String>() + "…";
        if width(&s) <= max_w {
            return s;
        }
        n -= 1;
    }
    "…".to_string()
}

fn win_button(ui: &mut egui::Ui, title: Rect, x: f32, id: &str) -> egui::Response {
    let r = Rect::from_min_size(
        egui::pos2(x, title.top()),
        egui::vec2(WIN_BTN_W, title.height()),
    );
    ui.interact(r, ui.id().with(id), Sense::click())
}

fn draw_x(p: &egui::Painter, c: egui::Pos2, s: f32, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    p.line_segment(
        [egui::pos2(c.x - s, c.y - s), egui::pos2(c.x + s, c.y + s)],
        stroke,
    );
    p.line_segment(
        [egui::pos2(c.x + s, c.y - s), egui::pos2(c.x - s, c.y + s)],
        stroke,
    );
}

fn draw_min(p: &egui::Painter, c: egui::Pos2, color: Color32) {
    p.line_segment(
        [egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x + 5.0, c.y)],
        Stroke::new(1.3, color),
    );
}

fn draw_max(p: &egui::Painter, c: egui::Pos2, color: Color32, restored: bool, bg: Color32) {
    let stroke = Stroke::new(1.3, color);
    if restored {
        let back = Rect::from_center_size(
            egui::pos2(c.x + 1.5, c.y - 1.5),
            egui::vec2(8.0, 8.0),
        );
        p.rect_stroke(back, 1.0, stroke, egui::StrokeKind::Inside);
        let front = Rect::from_center_size(
            egui::pos2(c.x - 1.5, c.y + 1.5),
            egui::vec2(8.0, 8.0),
        );
        p.rect_filled(front, 1.0, bg);
        p.rect_stroke(front, 1.0, stroke, egui::StrokeKind::Inside);
    } else {
        let r = Rect::from_center_size(c, egui::vec2(9.0, 9.0));
        p.rect_stroke(r, 1.0, stroke, egui::StrokeKind::Inside);
    }
}

fn draw_reload(p: &egui::Painter, c: egui::Pos2, r: f32, color: Color32) {
    use std::f32::consts::TAU;
    let start = -0.28 * TAU;
    let end = 0.72 * TAU;
    let n = 20;
    let pts: Vec<egui::Pos2> = (0..=n)
        .map(|i| {
            let a = start + (end - start) * i as f32 / n as f32;
            egui::pos2(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.6, color)));
    // arrow head at the open end
    let a = end;
    let tip = egui::pos2(c.x + (r + 2.0) * a.cos(), c.y + (r + 2.0) * a.sin());
    let back = egui::pos2(c.x + (r - 3.5) * a.cos(), c.y + (r - 3.5) * a.sin());
    let nx = -a.sin();
    let ny = a.cos();
    p.line_segment(
        [tip, egui::pos2(back.x + 2.5 * nx, back.y + 2.5 * ny)],
        Stroke::new(1.6, color),
    );
    p.line_segment(
        [tip, egui::pos2(back.x - 2.5 * nx, back.y - 2.5 * ny)],
        Stroke::new(1.6, color),
    );
}

fn draw_globe(p: &egui::Painter, c: egui::Pos2, r: f32, color: Color32) {
    let stroke = Stroke::new(1.2, color);
    p.circle_stroke(c, r, stroke);
    p.line_segment(
        [
            egui::pos2(c.x - r, c.y),
            egui::pos2(c.x + r, c.y),
        ],
        stroke,
    );
    p.line_segment(
        [
            egui::pos2(c.x, c.y - r),
            egui::pos2(c.x, c.y + r),
        ],
        Stroke::new(1.0, color),
    );
}

impl Drop for Shell {
    fn drop(&mut self) {
        if !self.lat.is_empty() {
            let mut v = self.lat.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: usize| v[(q * v.len() / 100).min(v.len() - 1)];
            println!(
                "[spike] scroll latency p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms ({} samples)",
                p(50),
                p(95),
                v[v.len() - 1],
                v.len()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Private bytes of another process (governor)

#[cfg(windows)]
fn process_private_mb(pid: u32) -> Option<f64> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(h, &mut c, c.cb) != 0;
        CloseHandle(h);
        ok.then(|| c.PagefileUsage as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(not(windows))]
fn process_private_mb(pid: u32) -> Option<f64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb: f64 = s.lines().find_map(|l| {
        let rest = l.strip_prefix("VmRSS:")?;
        rest.split_whitespace().next()?.parse().ok()
    })?;
    Some(kb / 1024.0)
}

// ---------------------------------------------------------------------------

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut urls: Vec<String> = Vec::new();
    let mut active = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tabs" => {
                i += 1;
                if let Some(list) = args.get(i) {
                    urls = list.split(',').map(|s| s.trim().to_string()).collect();
                }
            }
            "--active" => {
                i += 1;
                active = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            a if !a.starts_with("--") => urls.push(a.to_string()),
            _ => {}
        }
        i += 1;
    }
    let tabs: Vec<Tab> = if urls.is_empty() {
        vec![Tab::new("")]
    } else {
        urls.iter().map(|u| Tab::new(u)).collect()
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Mazda")
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([900.0, 560.0])
            // the titlebar, tabs and window buttons are drawn by the shell
            .with_decorations(false)
            .with_resizable(true),
        ..Default::default()
    };
    eframe::run_native(
        "Mazda",
        options,
        Box::new(move |cc| {
            let mut visuals = egui::Visuals::light();
            visuals.override_text_color = Some(TEXT);
            visuals.selection.bg_fill = ACCENT;
            cc.egui_ctx.set_visuals(visuals);
            Ok(Box::new(
                Shell::new(tabs, active).expect("failed to spawn renderer"),
            ))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The renderer can only read inside its granted dir, so a picked file is
    /// copied there. A second pick of the same name must not overwrite the
    /// first (the renderer may still be uploading it).
    #[test]
    fn stage_upload_copies_and_dedupes() {
        let dir = std::env::temp_dir().join(format!("mazda-stage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("notes.txt");
        std::fs::write(&src, b"hello").unwrap();
        let uploads = dir.join("uploads");
        let a = stage_upload(&src, &uploads).unwrap();
        let b = stage_upload(&src, &uploads).unwrap();
        assert_ne!(a, b, "second copy gets its own name");
        assert_eq!(std::fs::read(&a).unwrap(), b"hello");
        assert_eq!(a.file_name().unwrap(), "notes.txt");
        assert_eq!(b.file_name().unwrap(), "notes (1).txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zoom_steps_climb_and_stop_at_the_ends() {
        assert_eq!(zoom_step(1.0, 1), 1.1);
        assert_eq!(zoom_step(1.1, -1), 1.0);
        assert_eq!(zoom_step(0.25, -1), 0.25, "bottom is sticky");
        assert_eq!(zoom_step(5.0, 1), 5.0, "top is sticky");
        assert_eq!(zoom_step(0.9, 1), 1.0);
        assert_eq!(zoom_step(3.0, -1), 2.5);
    }
}
