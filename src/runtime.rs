//! SELECTABLE SCRIPT RUNTIME — the plumbing that lets the shell pick which
//! interpreter executes a page's scripts.
//!
//! Two runtimes implement the per-page `ScriptRuntime` contract:
//! - `script.rs` (always compiled): Option A — our pure-Rust built-in
//!   interpreter (`js.rs`), the default.
//! - this module's `engine_b` (only under the `engine-b` cargo feature):
//!   Option B — a full JS engine core from crates.io, opt-in for
//!   compatibility (~ +30 MB/page RAM). The choice is per-app (not per-tab —
//!   two engine heaps would double the cost), persisted in `mazda.conf`,
//!   and applies on the NEXT navigation (a runtime is created per page; no
//!   hot-swap of a live page's engine).
//!
//! UI wording (fixed spec):
//! ```text
//! JavaScript engine:
//!   (•) A — Pure Rust built-in (light)
//!   ( ) B — Full JS engine core (compatibility, +30 MB/page)
//! ```
//!
//! # Trait shape
//! Deliberately SMALL (5 methods): exactly what the loader worker needs to
//! drive a page (`run_scripts`, `dispatch`, `run_due_timers`,
//! `run_due_frames`, `next_wake_deadline`). A `RuntimeCaps` struct was
//! considered instead and rejected: every native maps 1:1 onto both
//! engines and timers/rAF are shell-side in both, so there is nothing a
//! caps struct could honestly declare.
//!
//! # DomHost
//! Natives on both engines call INTO [`DomHost`]: arena reads/writes plus
//! the page's side channels (navigation request, dirty flag,
//! preventDefault, page RAM storage), plain Rust in and out — adapters
//! translate their own values at the rim only. Host re-points at the page
//! DOM at the start of every script turn and is dereferenced only inside
//! that turn on the loader worker thread (the worker-thread-only
//! discipline script.rs has documented since the DOM wave).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use crate::html::{self, Dom, NodeKind};

// ---------------------------------------------------------------------------
// Turn result + runtime contract
// ---------------------------------------------------------------------------

/// Result of one script turn (script run / event dispatch / timer or rAF
/// fire) — the same contract for both engines.
#[derive(Debug, Default, Clone)]
pub struct Turn {
    /// DOM mutated during the turn → the caller must re-layout
    pub dirty: bool,
    /// `location.assign(...)` / `location.href = ...` target
    pub nav: Option<String>,
    /// a listener called `event.preventDefault()` (link navigation veto)
    pub prevent_default: bool,
}

impl Turn {
    pub fn none() -> Turn {
        Turn::default()
    }
}

/// What the loader worker needs from whichever engine runs the page.
pub trait ScriptRuntime {
    /// Run every `<script>` (inline + `src`, document order) and settle
    /// synchronously-scheduled work (0 ms timers).
    fn run_scripts(&mut self, dom: &mut Dom) -> Turn;
    /// Dispatch event `kinds` (in order) at `node` with the event fields.
    fn dispatch(
        &mut self,
        dom: &mut Dom,
        node: u32,
        kinds: &[&str],
        key: Option<&str>,
        page_y: Option<f32>,
        value: Option<&str>,
    ) -> Turn;
    /// Fire timers whose deadline passed.
    fn run_due_timers(&mut self, dom: &mut Dom) -> Turn;
    /// One coalesced `requestAnimationFrame` frame pass.
    fn run_due_frames(&mut self, dom: &mut Dom) -> Turn;
    /// Earlier of (next timer deadline, next rAF epoch) — drives the
    /// worker's `recv_timeout`. None = block until an input job arrives.
    fn next_wake_deadline(&self) -> Option<Instant>;
}

/// Build the runtime for `choice` against a fresh page (per-navigation
/// constructor — the engine choice applies from the next load on).
pub fn make_runtime(choice: EngineChoice, dom: &mut Dom, base: &str) -> Box<dyn ScriptRuntime> {
    match choice {
        EngineChoice::A => Box::new(crate::script::PageScripts::new(dom, base)),
        EngineChoice::B => {
            #[cfg(feature = "engine-b")]
            if ENGINE_B_OK {
                return Box::new(engine_b::RuntimeB::new(dom, base));
            }
            // --engine=b / conf b on a build without the feature: warn
            // (once) and run A rather than crashing.
            warn_engine_b_unavailable();
            Box::new(crate::script::PageScripts::new(dom, base))
        }
    }
}

/// Is the engine-core runtime compiled into this build? (a const so the
/// make/dispatch paths can branch without cfg noise at each site).
#[cfg(feature = "engine-b")]
const ENGINE_B_OK: bool = true;
#[cfg(not(feature = "engine-b"))]
const ENGINE_B_OK: bool = false;

/// One-per-process fallback warning (a settings switch re-loads pages, so
/// a per-load message would flood the stderr log).
fn warn_engine_b_unavailable() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_ok() {
        eprintln!("[settings] engine B unavailable (built without feature); using A instead");
    }
}

// ---------------------------------------------------------------------------
// Shared scheduler budgets (single source for both engines)
// ---------------------------------------------------------------------------

/// Pending `setTimeout` jobs the worker keeps per page — a runaway
/// `setTimeout(loop)` storm would otherwise grow the queue without bound.
pub(crate) const MAX_PENDING_TIMERS: usize = 512;
/// Same guard for `requestAnimationFrame` callbacks queued but not yet
/// run in a frame pass (a page rAF-looping inside layout-heavy pages).
pub(crate) const MAX_PENDING_FRAMES: usize = 128;
/// Frame epoch ≈ one vsync — rAF callbacks coalesce to the next pass.
pub(crate) const FRAME_EPOCH: u64 = 16;

// ---------------------------------------------------------------------------
// Engine choice + mazda.conf persistence
// ---------------------------------------------------------------------------

/// Which interpreter runs the pages. `A` = pure-Rust built-in (default),
/// `B` = full JS engine core (behind the `engine-b` build feature).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineChoice {
    A,
    B,
}

impl EngineChoice {
    /// Config-file spelling (lowercase single letter).
    pub fn as_conf(self) -> &'static str {
        match self {
            EngineChoice::A => "a",
            EngineChoice::B => "b",
        }
    }

    /// Is this choice compiled into THIS build? B needs `--features engine-b`.
    pub fn available(self) -> bool {
        match self {
            EngineChoice::A => true,
            EngineChoice::B => ENGINE_B_OK,
        }
    }
}

/// Persistent app settings (`mazda.conf`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Conf {
    pub engine: EngineChoice,
}

impl Default for Conf {
    fn default() -> Self {
        Conf { engine: EngineChoice::A }
    }
}

/// Parse the conf file body: `key=value` lines, whitespace tolerated,
/// unknown keys ignored (forward-compat), a malformed engine value leaves
/// the DEFAULT (A). A line without `=` corrupts the file sense and resets
/// everything. Never panics — corruption may cost settings, never crash.
pub fn parse_conf(text: &str) -> Conf {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once('=') {
            Some((k, v)) if k.trim() == "engine" => {
                let conf = match v.trim() {
                    "a" => Conf { engine: EngineChoice::A },
                    "b" => Conf { engine: EngineChoice::B },
                    _ => return Conf::default(),
                };
                return conf;
            }
            Some(_) => {} // unknown key: ignore
            None => return Conf::default(), // corrupt line → default
        }
    }
    Conf::default()
}

/// Serialize one setting per line.
pub fn serialize_conf(conf: &Conf) -> String {
    format!("engine={}\n", conf.engine.as_conf())
}

impl Conf {
    /// Startup resolution: `--engine=a|b` flag > `MAZDA_ENGINE` env var >
    /// `mazda.conf` on disk > built-in default A. The CLI/env knob is
    /// legitimate testing plumbing for the selectable-runtime wave — the
    /// same choice the settings page turns, reachable without UI input
    /// (which this shell may never synthesize).
    pub fn init(args: &[String]) -> Conf {
        let flag = cli_engine_flag(args);
        let env = std::env::var("MAZDA_ENGINE").ok();
        resolve_engine(flag, env.as_deref(), load_conf())
    }

    /// Write-through-on-change: the settings UI calls this whenever the
    /// user flips the radio. File lives next to the disk-cache root the
    /// shell already manages (`%LOCALAPPDATA%\mazda-cache`, conf one level
    /// up — same resolution pattern as main.rs `cache_dir`).
    pub fn save(&self) {
        if !self.engine.available() && self.engine == EngineChoice::B {
            // trying to persist a choice this build cannot run: keep A on
            // disk so a plain default build can't come up in B.
            eprintln!("[settings] not persisting engine B (built without feature)");
            return;
        }
        let path = conf_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, serialize_conf(self)) {
            eprintln!("[settings] conf write failed {}: {e}", path.display());
        }
    }
}

/// Run of the argv engine key: parses `--engine=a` / `--engine=B`
/// (trim + case-insensitive). None when unrelated flags are passed.
pub fn cli_engine_flag(args: &[String]) -> Option<EngineChoice> {
    let mut out: Option<EngineChoice> = None;
    for a in args {
        if let Some(v) = a.strip_prefix("--engine=") {
            out = match v.trim().to_ascii_lowercase().as_str() {
                "a" => Some(EngineChoice::A),
                "b" => Some(EngineChoice::B),
                _ => out,
            };
        }
    }
    out
}

/// `MAZDA_ENGINE=a|B` env spelling (trim + lowercase; garbage = unset).
fn env_engine(v: &str) -> Option<EngineChoice> {
    match v.trim().to_ascii_lowercase().as_str() {
        "a" => Some(EngineChoice::A),
        "b" => Some(EngineChoice::B),
        _ => None,
    }
}

/// Precedence chain (pure, tested): flag > env > conf > default A. A value
/// this build cannot honor (B compiled out) collapses to the default A
/// rather than being remembered.
pub fn resolve_engine(flag: Option<EngineChoice>, env: Option<&str>, conf: Conf) -> Conf {
    let honor = |c: Option<EngineChoice>| -> Option<Conf> {
        c.filter(|e| e.available()).map(|e| Conf { engine: e })
    };
    honor(flag)
        .or_else(|| honor(env.and_then(env_engine)))
        .unwrap_or(conf)
}

/// `%LOCALAPPDATA%\mazda.conf` — next to the mazda-cache root the shell
/// already owns (see main.rs `cache_dir`, `...\mazda-cache\..\mazda.conf`).
/// Fallback: temp dir, mirroring the cache-dir fallback.
pub fn conf_path() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("mazda.conf")
}

/// Read the conf file; missing / unreadable / corrupt → default (A).
pub fn load_conf() -> Conf {
    match std::fs::read_to_string(conf_path()) {
        Ok(text) => parse_conf(&text),
        Err(_) => Conf::default(),
    }
}

// ---------------------------------------------------------------------------
// DomHost — the shared surface both engines' natives call into
// ---------------------------------------------------------------------------

/// Page-scoped RAM storage (`localStorage` == `sessionStorage`, merged;
/// worker-local, dropped on navigation — the honest subset documented in
/// script.rs).
pub type Storage = Rc<RefCell<HashMap<String, String>>>;

/// Window-level listener-table key (`window.addEventListener`; also used
/// for `document.addEventListener` — both are page-wide targets with no
/// own arena node). u32::MAX cannot collide with a real node id.
pub const WIN_NODE: u32 = u32::MAX;

impl Default for DomHost {
    fn default() -> Self {
        Self::new_stub()
    }
}

/// The host the natives call into for the CURRENT script turn. Established
/// fresh per turn by whichever runtime runs (`host_install`), torn down at
/// turn end (`host_take`) and folded into the runtime's own scheduling
/// state. Raw pointer discipline (worker thread only; identical to the
/// pre-wave script.rs Ctx): `dom` points at the loader worker's kept-page
/// DOM, is re-pointed at the start of every turn, and is dereferenced only
/// inside that one turn; arena indices stay valid for the page's lifetime.
pub struct DomHost {
    dom: *mut Dom,
    base: String,
    /// page RAM storage handle (shared with the owning runtime)
    storage: Storage,
    // ---- side channels, folded by the owning runtime at turn end ----
    /// `location.assign(...)` / reload target (absolute, resolved vs base)
    nav: Option<String>,
    /// a script mutated the DOM during this turn → re-layout
    dirty: bool,
    /// a listener called `event.preventDefault()` (navigation veto)
    prevent_default: bool,
    /// honest-log guard: the storage note prints once per page
    storage_noted: bool,
}

impl DomHost {
    pub fn new(dom: &mut Dom, base: &str, storage: Storage) -> Self {
        DomHost {
            dom: dom as *mut Dom,
            base: base.to_string(),
            storage,
            nav: None,
            dirty: false,
            prevent_default: false,
            storage_noted: false,
        }
    }

    /// Turn-less instantiation (no DOM behind it; used only in tests).
    fn new_stub() -> Self {
        DomHost {
            dom: std::ptr::null_mut(),
            base: String::new(),
            storage: Rc::default(),
            nav: None,
            dirty: false,
            prevent_default: false,
            storage_noted: true,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn dom_ref(&self) -> &Dom {
        unsafe { &*self.dom }
    }

    // ---- reads over the arena (nested-safe, owned results) ----

    /// Attribute read (case-insensitive name), owned copy.
    pub fn attr(&self, node: u32, name: &str) -> Option<String> {
        self.dom_ref().attr(node, name).map(|s| s.to_string())
    }

    /// `body` descendant of the page (falls back to the document root).
    /// Only the engine-core adapter needs it (A reads this at page setup).
    #[cfg(feature = "engine-b")]
    pub fn body_root(&self) -> u32 {
        self.dom_ref().body_root()
    }

    /// Concatenated text of every descendant text node (textContent /
    /// innerHTML source — markup stored as literal text).
    pub fn child_text(&self, node: u32) -> String {
        crate::script::child_text(self.dom_ref(), node)
    }

    /// Whole-document simple-selector matches (document order).
    pub fn query_all(&self, sel: &str) -> Vec<u32> {
        crate::script::query_all_dom(self.dom_ref(), sel)
    }

    /// First match in the subtree of `root` (root excluded; document order).
    pub fn first_subtree(&self, root: u32, sel: &str) -> Option<u32> {
        crate::script::subtree_query_dom(self.dom_ref(), root, sel)
            .into_iter()
            .next()
    }

    /// All subtree matches for `sel`.
    pub fn subtree_all(&self, root: u32, sel: &str) -> Vec<u32> {
        crate::script::subtree_query_dom(self.dom_ref(), root, sel)
    }

    /// First `<html>` element (documentElement wrapper source). Only the
    /// engine-core adapter needs it (A reads this at page setup).
    #[cfg(feature = "engine-b")]
    pub fn html_element(&self) -> u32 {
        let dom = self.dom_ref();
        (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "html")
            .unwrap_or(0)
    }

    /// Readable props snapshotted at wrap time — the exact set the built-in
    /// engine exposes on element wrappers (script.rs `wrap_element`), shared
    /// verbatim so engine-core scripts see the same element objects.
    pub fn snapshot(&self, node: u32) -> Option<ElemProps> {
        let dom = self.dom_ref();
        if node as usize >= dom.nodes.len() {
            return None;
        }
        let tag = dom.tag(node).to_string();
        let lower = tag.to_ascii_lowercase();
        Some(ElemProps {
            tag: tag.to_ascii_uppercase(),
            id: dom.attr(node, "id").map(|s| s.to_string()),
            class: dom.attr(node, "class").map(|s| s.to_string()),
            value: matches!(lower.as_str(), "input" | "textarea" | "select")
                .then(|| {
                    dom.attr(node, "value")
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                }),
            ty: (lower == "input")
                .then(|| dom.attr(node, "type").unwrap_or("text").to_string()),
            checked: lower == "input" && dom.attr(node, "checked").is_some(),
            href: (lower == "a").then(|| {
                dom.attr(node, "href").map(|s| s.to_string()).unwrap_or_default()
            }),
            text: self.child_text(node),
        })
    }

    // ---- writes over the arena (set the dirty flag on change) ----

    /// `parent.appendChild(child)` re-links the arena. False when either id
    /// is out of range (native then performs no side effects).
    pub fn append_child(&mut self, parent: u32, child: u32) -> bool {
        let dom = unsafe { &mut *self.dom };
        let ok = (child as usize) < dom.nodes.len() && (parent as usize) < dom.nodes.len();
        if ok {
            dom.nodes[child as usize].parent = parent;
            dom.nodes[parent as usize].children.push(child);
            self.dirty = true;
        }
        ok
    }

    /// Set (or add) an attribute; returns whether anything changed.
    pub fn set_attribute(&mut self, node: u32, name: &str, val: &str) -> bool {
        let dom = unsafe { &mut *self.dom };
        let Some(NodeKind::Element(_, attrs)) =
            dom.nodes.get_mut(node as usize).map(|n| &mut n.kind)
        else {
            return false;
        };
        match attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            Some((_, v)) => {
                if v == val {
                    return false;
                }
                *v = val.to_string();
                self.dirty = true;
                true
            }
            None => {
                let mut v: Vec<(String, String)> = attrs.iter().cloned().collect();
                v.push((name.to_string(), val.to_string()));
                *attrs = v.into_boxed_slice();
                self.dirty = true;
                true
            }
        }
    }

    /// Remove an attribute; true (and dirty) only when it existed.
    pub fn remove_attribute(&mut self, node: u32, name: &str) -> bool {
        let dom = unsafe { &mut *self.dom };
        let Some(NodeKind::Element(_, attrs)) =
            dom.nodes.get_mut(node as usize).map(|n| &mut n.kind)
        else {
            return false;
        };
        if let Some(i) = attrs.iter().position(|(k, _)| k.eq_ignore_ascii_case(name)) {
            let mut v: Vec<(String, String)> = attrs.iter().cloned().collect();
            v.remove(i);
            *attrs = v.into_boxed_slice();
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Replace all children with one text node (`textContent`, text-only
    /// `innerHTML`).
    pub fn set_text_content(&mut self, node: u32, text: &str) {
        let dom = unsafe { &mut *self.dom };
        if (node as usize) >= dom.nodes.len() {
            return;
        }
        dom.nodes[node as usize].children.clear();
        if !text.is_empty() {
            let id = dom.nodes.len() as u32;
            dom.nodes.push(html::Node {
                parent: node,
                children: Vec::new(),
                kind: NodeKind::Text(text.to_string()),
            });
            dom.nodes[node as usize].children.push(id);
        }
        self.dirty = true;
    }

    /// `createElement(tag)`: clones the TagId from any existing same-tag
    /// element (the tag interner is private to html.rs); unknown tags
    /// return None (the caller logs `[js]` honestly). The arena push is
    /// split from wrapper construction so both engines wrap after —
    /// unwrapped orphan nodes can never leak into script-visible state.
    pub fn create_element(&mut self, tag: &str) -> Option<u32> {
        let dom = unsafe { &mut *self.dom };
        let tid = all_elements_dom(dom)
            .into_iter()
            .find(|&i| dom.tag(i) == tag)
            .and_then(|i| match &dom.nodes[i as usize].kind {
                NodeKind::Element(t, _) => Some(*t),
                _ => None,
            })?;
        let id = dom.nodes.len() as u32;
        dom.nodes.push(html::Node {
            parent: 0,
            children: Vec::new(),
            kind: NodeKind::Element(tid, Box::from([])),
        });
        self.dirty = true;
        Some(id)
    }

    // ---- side channels (scripts → runtime, folded at turn end) ----

    /// `location.assign(...)` / `location.href = ...`: resolve against the
    /// page base immediately; only the FIRST navigation request of a turn
    /// is kept (a job results in at most one navigation).
    pub fn request_nav(&mut self, target: &str) {
        let resolved = crate::resolve(&self.base, target).unwrap_or_else(|| target.to_string());
        if self.nav.is_none() {
            self.nav = Some(resolved);
        }
    }

    /// The navigation request accumulated this turn (moved out, so folding
    /// twice cannot double-fire).
    pub fn take_nav(&mut self) -> Option<String> {
        self.nav.take()
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn prevent_default(&self) -> bool {
        self.prevent_default
    }

    /// `event.preventDefault()` — flags the shell's one native default
    /// action (link navigation); the UI thread cancels a not-yet-started
    /// navigation with the click's decision result.
    pub fn note_prevent_default(&mut self, node: u32) {
        eprintln!("[js] event.preventDefault (node {node}) — navigation may be canceled");
        self.prevent_default = true;
    }

    // ---- storage (page RAM; exactly the semantics script.rs documents) ----

    pub fn storage_get(&self, key: &str) -> Option<String> {
        self.storage.borrow().get(key).cloned()
    }

    pub fn storage_set(&mut self, key: &str, val: &str) {
        if !self.storage_noted {
            self.storage_noted = true;
            eprintln!(
                "[js] localStorage: in-memory only, dropped on navigation (sessionStorage merged; no disk backing)"
            );
        }
        self.storage.borrow_mut().insert(key.to_string(), val.to_string());
    }

    pub fn storage_remove(&mut self, key: &str) {
        self.storage.borrow_mut().remove(key);
    }

    pub fn storage_len(&self) -> usize {
        self.storage.borrow().len()
    }

    pub fn storage_clear(&mut self) {
        self.storage.borrow_mut().clear();
    }

    // ---- page services (one implementation, both engines) ----

    /// Resolve `href` against the page base.
    pub fn resolve_url(&self, href: &str) -> Option<String> {
        crate::resolve(&self.base, href)
    }

    /// Synchronous `fetch` body + content-type, via the shell's bounded,
    /// disk-cached fetch.
    pub fn fetch_text(&self, url: &str) -> Result<(String, Option<String>), String> {
        let abs = crate::resolve(&self.base, url)
            .ok_or_else(|| format!("unresolvable url {url:?}"))?;
        let (bytes, ct) = crate::fetch(&abs)?;
        Ok((String::from_utf8_lossy(&bytes).into_owned(), ct))
    }

    /// `matchMedia("(prefers-color-scheme: …)")` — mirrors the single OS
    /// preference constant css.rs resolves elements with; other queries →
    /// no match. Keep this mirror in sync with css.rs.
    pub fn match_media_dark(&self, q: &str) -> bool {
        // this mirror must stay in sync with css.rs PREFERS_DARK (= true)
        const PREFERS_DARK_MIRROR: bool = true;
        if q.contains("prefers-color-scheme") {
            if q.contains("dark") {
                PREFERS_DARK_MIRROR
            } else if q.contains("light") {
                !PREFERS_DARK_MIRROR
            } else {
                false
            }
        } else {
            false
        }
    }
}

/// Wrapper-prop snapshot (see `DomHost::snapshot`): exactly the readable
/// surface the built-in engine exposes on element wrappers.
pub struct ElemProps {
    /// upper-cased tag (tagName)
    pub tag: String,
    pub id: Option<String>,
    pub class: Option<String>,
    /// current value — input/textarea/select only
    pub value: Option<String>,
    /// input `type` (default "text")
    pub ty: Option<String>,
    /// input `checked`
    pub checked: bool,
    /// anchor `href`
    pub href: Option<String>,
    /// concatenated descendant text
    pub text: String,
}

// ---------------------------------------------------------------------------
// Pure DOM helpers live in script.rs (pub(crate)): child_text,
// query_all_dom, subtree_query_dom, all_elements_dom. The host delegates to
// them — one expression of each algorithm, tests keep working.
// ---------------------------------------------------------------------------

/// All element nodes in document order (skips the `#document` sentinel at
/// index 0, which is an Element kind with tag "#document") — DomHost-side
/// copy; the canonical implementation is script.rs `all_elements_dom`.
fn all_elements_dom(dom: &Dom) -> Vec<u32> {
    crate::script::all_elements_dom(dom)
}

// ---------------------------------------------------------------------------
// Thread-local turn context: install → run natives → take → fold
// ---------------------------------------------------------------------------

thread_local! {
    static HOST: RefCell<Option<DomHost>> = const { RefCell::new(None) };
}

/// Re-point the host at `dom`/`base`/`storage` for the duration of a turn.
pub fn host_install(dom: &mut Dom, base: &str, storage: &Storage) {
    HOST.with(|h| {
        *h.borrow_mut() = Some(DomHost::new(dom, base, storage.clone()));
    });
}

/// Take the host back at turn end. Panics are impossible by contract: only
/// turn drivers call this, paired with `host_install`.
pub fn host_take() -> DomHost {
    HOST.with(|h| {
        h.borrow_mut()
            .take()
            .expect("[js] script context vanished mid-turn")
    })
}

/// Read a host handle inside the current turn. Immutable on purpose: read
/// natives nest (getElementById reads the arena while wrapping the found
/// element). Panics are impossible by contract: only natives call this,
/// and natives only run inside a turn.
pub fn host<R>(f: impl FnOnce(&DomHost) -> R) -> R {
    HOST.with(|h| {
        let b = h.borrow();
        let cx = b.as_ref().expect("[js] native ran outside a script turn");
        f(cx)
    })
}

/// Mutate the host inside the current turn (arena writes, side-channel
/// notes). Two writes never nest — a native finishes its write before any
/// nested call happens.
pub fn host_mut<R>(f: impl FnOnce(&mut DomHost) -> R) -> R {
    HOST.with(|h| {
        let mut b = h.borrow_mut();
        let cx = b.as_mut().expect("[js] native ran outside a script turn");
        f(cx)
    })
}

// ---------------------------------------------------------------------------
// Option B — engine-core adapter (compiled only with `--features engine-b`)
// ---------------------------------------------------------------------------
// Honest boundaries: engine-core scripts get EXACTLY the same native
// surface as Option A (no more) — DOM operations stay ours, via DomHost.
// Mapped: the engine core's own promise/job queue IS used for microtasks
// (`run_jobs` after every turn) and its internal event model's promises
// therefore behave. Unmapped pieces: the engine core has no timers of its
// own to reconcile (setTimeout lives on our side in both engines), and
// `cancelAnimationFrame` does not exist yet in either engine (callback ids
// are returned but unused — same shortcut in script.rs).
#[cfg(feature = "engine-b")]
pub mod engine_b {
    use super::*;

    use std::time::Duration;

    use boa_engine::{
        js_string, Context, JsError, JsNativeError, JsObject, JsResult, JsString, JsValue,
        NativeFunction, Source,
        object::{builtins::JsArray, FunctionObjectBuilder},
    };

    /// Native fn-pointer shape the engine core accepts (0.22).
    type NativeFn = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;

    // ---- value helpers (plain Rust ⇆ engine core, at the rim only) ----

    fn s_val(s: &str) -> JsValue {
        JsValue::from(JsString::from(s))
    }

    fn n_val(n: f64) -> JsValue {
        JsValue::from(n)
    }

    /// `ToString` coercion (JS abstract operation); error → None.
    fn v_str(v: &JsValue, ctx: &mut Context) -> Option<String> {
        v.to_string(ctx).ok().map(|s| s.to_std_string_escaped())
    }

    fn arg_str(args: &[JsValue], i: usize, ctx: &mut Context) -> Option<String> {
        args.get(i).and_then(|v| v_str(v, ctx))
    }

    fn arg_num(args: &[JsValue], i: usize, ctx: &mut Context) -> Option<f64> {
        args.get(i).and_then(|v| v.to_number(ctx).ok())
    }

    fn typ(msg: &str) -> JsError {
        JsNativeError::typ().with_message(msg.to_string()).into()
    }

    /// Fresh plain object (Object.prototype chain).
    fn obj_val(ctx: &Context) -> JsObject {
        JsObject::with_object_proto(ctx.intrinsics())
    }

    /// Own-data-property install (fresh objects; failures impossible by
    /// construction — swallowed honestly with `let _`).
    fn set_own(o: &JsObject, ctx: &mut Context, key: &str, v: impl Into<JsValue>) {
        let _ = o.create_data_property_or_throw(JsString::from(key), v, ctx);
    }

    /// A named native function object (fn-pointer natives are stateless —
    /// per-page state flows through the thread-locals instead).
    fn fn_obj(ctx: &Context, name: &str, length: usize, f: NativeFn) -> JsValue {
        FunctionObjectBuilder::new(ctx.realm(), NativeFunction::from_fn_ptr(f))
            .name(JsString::from(name))
            .length(length)
            .build()
            .into()
    }

    // ---- per-turn adapter side (thread-local, like DomHost) ----

    /// What natives queue DURING a turn; the owning runtime folds it at
    /// turn end (listeners into the table, timers/rAF onto the wake
    /// queues with absolute deadlines). Single loader thread ⇒ the
    /// thread-local is page-ownerless between turns.
    struct BSide {
        /// element-method prototype installed per page
        el_proto: Option<JsObject>,
        /// ((node, kind), callback) registrations made this turn
        listeners: Vec<((u32, String), JsValue)>,
        /// (delay_ms, callback, args)
        timers: Vec<(f64, JsValue, Vec<JsValue>)>,
        /// requestAnimationFrame callbacks queued this turn
        raf: Vec<JsValue>,
        /// monotonic timer/rAF ids handed to scripts (both unused — same
        /// as Option A: no cancel* natives yet)
        timer_id: f64,
        raf_id: f64,
    }

    impl Default for BSide {
        fn default() -> Self {
            BSide {
                el_proto: None,
                listeners: Vec::new(),
                timers: Vec::new(),
                raf: Vec::new(),
                timer_id: 0.0,
                raf_id: 0.0,
            }
        }
    }

    thread_local! {
        static BSIDE: RefCell<BSide> = const { RefCell::new(BSide {
            el_proto: None, listeners: Vec::new(), timers: Vec::new(),
            raf: Vec::new(), timer_id: 0.0, raf_id: 0.0,
        }) };
    }

    fn bside<R>(f: impl FnOnce(&mut BSide) -> R) -> R {
        BSIDE.with(|b| f(&mut b.borrow_mut()))
    }

    /// Fresh page: drop any leftover adapter state (per-page contexts are
    /// dropped with the page; the thread-local outlives them).
    fn bside_reset() {
        BSIDE.with(|b| *b.borrow_mut() = BSide::default());
    }

    /// The page's element-method prototype (installed by `RuntimeB::new`).
    fn b_proto() -> JsObject {
        bside(|b| {
            b.el_proto
                .clone()
                .expect("[js] engine-core element prototype not installed")
        })
    }

    // ---- element wrappers (snapshot props + prototype methods) ----

    /// Snapshot `node` into an engine-core object: `__node` (id) data
    /// property + readable props taken live at wrap time (identical to
    /// script.rs `wrap_element`), methods via the shared `el_proto`.
    fn wrap(ctx: &mut Context, node: u32) -> JsValue {
        let obj = JsObject::from_proto_and_data(Some(b_proto()), ());
        let _ = obj.create_data_property_or_throw(js_string!("__node"), n_val(node as f64), ctx);
        if let Some(p) = host(|h| h.snapshot(node)) {
            set_own(&obj, ctx, "tagName", s_val(&p.tag));
            set_own(&obj, ctx, "id", s_val(p.id.as_deref().unwrap_or("")));
            set_own(&obj, ctx, "className", s_val(p.class.as_deref().unwrap_or("")));
            if let Some(v) = p.value {
                set_own(&obj, ctx, "value", s_val(&v));
            }
            if let Some(t) = p.ty {
                set_own(&obj, ctx, "type", s_val(&t));
                set_own(&obj, ctx, "checked", JsValue::from(p.checked));
            }
            if let Some(href) = p.href {
                set_own(&obj, ctx, "href", s_val(&href));
            }
            set_own(&obj, ctx, "textContent", s_val(&p.text));
        }
        obj.into()
    }

    /// The full event object handed to listeners (mirror of script.rs
    /// `wrap_event`): type/target/bubbles + per-kind fields + preventDefault.
    fn wrap_event(
        ctx: &mut Context,
        node: u32,
        kind: &str,
        key: Option<&str>,
        page_y: Option<f32>,
        value: Option<&str>,
    ) -> JsValue {
        let o = obj_val(ctx);
        set_own(&o, ctx, "type", s_val(kind));
        // compute the wrapped target FIRST (one &mut borrow at a time)
        let target = wrap(ctx, node);
        set_own(&o, ctx, "target", target);
        set_own(&o, ctx, "bubbles", JsValue::from(true));
        if let Some(k) = key {
            set_own(&o, ctx, "key", s_val(k));
        }
        if let Some(y) = page_y {
            set_own(&o, ctx, "pageY", JsValue::from(f64::from(y)));
        }
        if let Some(v) = value {
            set_own(&o, ctx, "value", s_val(v));
        }
        set_own(&o, ctx, "preventDefault", fn_obj(ctx, "preventDefault", 0, b_ev_prevent_default));
        o.into()
    }

    /// `__node` off a wrapper; TypeError when missing/not an object
    /// (mirror of script.rs `this_node`).
    fn this_node(this: &JsValue, ctx: &mut Context) -> JsResult<u32> {
        let Some(obj) = this.as_object() else {
            return Err(typ("TypeError: not an element"));
        };
        let v = obj.get(js_string!("__node"), ctx)?;
        let n = v.as_number().ok_or_else(|| typ("TypeError: not an element"))?;
        if n.is_finite() && n >= 0.0 && n.fract() == 0.0 {
            Ok(n as u32)
        } else {
            Err(typ("TypeError: not an element"))
        }
    }

    // ---- natives: document group ----

    fn b_d_get_element_by_id(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let Some(sel) = arg_str(args, 0, ctx) else { return Ok(JsValue::null()) };
        let found = host(|h| h.query_all(&format!("#{sel}")).first().copied());
        Ok(match found {
            Some(n) => wrap(ctx, n),
            None => JsValue::null(),
        })
    }

    fn b_d_query(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let Some(sel) = arg_str(args, 0, ctx) else { return Ok(JsValue::null()) };
        let found = host(|h| h.query_all(&sel).first().copied());
        Ok(match found {
            Some(n) => wrap(ctx, n),
            None => JsValue::null(),
        })
    }

    fn b_d_query_all(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let Some(sel) = arg_str(args, 0, ctx) else {
            return Ok(JsArray::from_iter([], ctx).into());
        };
        let nodes = host(|h| h.query_all(&sel));
        // collect first: one &mut borrow of ctx at a time (wrap then build)
        let items: Vec<JsValue> = nodes.into_iter().map(|n| wrap(ctx, n)).collect();
        Ok(JsArray::from_iter(items, ctx).into())
    }

    fn b_d_create_element(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let Some(tag) = arg_str(args, 0, ctx) else { return Ok(JsValue::null()) };
        let tag = tag.to_ascii_lowercase();
        let made = host_mut(|h| h.create_element(&tag));
        let Some(n) = made else {
            eprintln!("[js] createElement: unknown tag {tag:?} (not in document)");
            return Ok(JsValue::null());
        };
        Ok(wrap(ctx, n))
    }

    // ---- natives: element group (on the shared el_proto; `this` = wrapper) ----

    fn b_el_get_element_by_id(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let Some(sel) = arg_str(args, 0, ctx) else { return Ok(JsValue::null()) };
        let found = host(|h| h.first_subtree(node, &format!("#{sel}")));
        Ok(match found {
            Some(n) => wrap(ctx, n),
            None => JsValue::null(),
        })
    }

    fn b_el_query(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let Some(sel) = arg_str(args, 0, ctx) else { return Ok(JsValue::null()) };
        let found = host(|h| h.first_subtree(node, &sel));
        Ok(match found {
            Some(n) => wrap(ctx, n),
            None => JsValue::null(),
        })
    }

    fn b_el_query_all(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let Some(sel) = arg_str(args, 0, ctx) else {
            return Ok(JsArray::from_iter([], ctx).into());
        };
        let nodes = host(|h| h.subtree_all(node, &sel));
        let items: Vec<JsValue> = nodes.into_iter().map(|n| wrap(ctx, n)).collect();
        Ok(JsArray::from_iter(items, ctx).into())
    }

    fn b_el_append(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let child_val = args.first().cloned().unwrap_or_else(JsValue::undefined);
        let child = this_node(&child_val, ctx)?;
        // dirty flag set inside the host write itself
        host_mut(|h| h.append_child(node, child));
        Ok(child_val)
    }

    fn b_el_set_attr(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let name = arg_str(args, 0, ctx).unwrap_or_default();
        let val = arg_str(args, 1, ctx).unwrap_or_default();
        host_mut(|h| h.set_attribute(node, &name, &val));
        Ok(JsValue::undefined())
    }

    fn b_el_get_attr(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let name = arg_str(args, 0, ctx).unwrap_or_default();
        let v = host(|h| h.attr(node, &name));
        Ok(v.map(|s| s_val(&s)).unwrap_or_else(JsValue::null))
    }

    fn b_el_rm_attr(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let name = arg_str(args, 0, ctx).unwrap_or_default();
        host_mut(|h| h.remove_attribute(node, &name));
        Ok(JsValue::undefined())
    }

    fn b_el_add_listener(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let Some(kind) = arg_str(args, 0, ctx) else { return Ok(JsValue::undefined()) };
        let cb = args.get(1).cloned().unwrap_or_else(JsValue::undefined);
        if !cb.is_callable() {
            eprintln!("[js] addEventListener: callback is not a function");
            return Ok(JsValue::undefined());
        }
        bside(|b| b.listeners.push(((node, kind), cb)));
        Ok(JsValue::undefined())
    }

    fn b_el_set_text(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let t = arg_str(args, 0, ctx).unwrap_or_default();
        host_mut(|h| h.set_text_content(node, &t));
        Ok(JsValue::undefined())
    }

    fn b_el_set_inner(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let s = arg_str(args, 0, ctx).unwrap_or_default();
        // text-only innerHTML: markup fragments are stored literally
        if s.contains('<') {
            eprintln!("[js] innerHTML: markup fragments stored as literal text (no parser)");
        }
        host_mut(|h| h.set_text_content(node, &s));
        Ok(JsValue::undefined())
    }

    fn b_el_set_value(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let t = arg_str(args, 0, ctx).unwrap_or_default();
        host_mut(|h| h.set_attribute(node, "value", &t));
        Ok(JsValue::undefined())
    }

    fn b_el_set_class(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx)?;
        let t = arg_str(args, 0, ctx).unwrap_or_default();
        host_mut(|h| h.set_attribute(node, "class", &t));
        Ok(JsValue::undefined())
    }

    fn b_el_focus(this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, _ctx).unwrap_or(0);
        // the shell owns real focus; log intent (same as Option A)
        eprintln!("[js] element.focus() requested (node {node})");
        Ok(JsValue::undefined())
    }

    // ---- natives: location / window / event object ----

    fn b_loc_assign(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let target = arg_str(args, 0, ctx).unwrap_or_default();
        host_mut(|h| h.request_nav(&target));
        Ok(JsValue::undefined())
    }

    fn b_loc_reload(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
        // a reload is a navigation to the current page (refetch through the
        // full pipeline; the disk cache may serve it up to TTL)
        let base = host(|h| h.base().to_string());
        host_mut(|h| h.request_nav(&base));
        Ok(JsValue::undefined())
    }

    fn b_win_add_listener(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let Some(kind) = arg_str(args, 0, ctx) else { return Ok(JsValue::undefined()) };
        let cb = args.get(1).cloned().unwrap_or_else(JsValue::undefined);
        if !cb.is_callable() {
            eprintln!("[js] addEventListener: callback is not a function");
            return Ok(JsValue::undefined());
        }
        // window + document registrations share the global bucket (WIN_NODE)
        bside(|b| b.listeners.push(((WIN_NODE, kind), cb)));
        Ok(JsValue::undefined())
    }

    fn b_ev_prevent_default(this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let node = this_node(this, ctx).unwrap_or(0);
        host_mut(|h| h.note_prevent_default(node));
        Ok(JsValue::undefined())
    }

    // ---- natives: timers / rAF (shell-side queues, same as Option A) ----

    fn b_set_timeout(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let cb = args.first().cloned().unwrap_or_else(JsValue::undefined);
        if !cb.is_callable() {
            return Err(typ("TypeError: setTimeout callback is not a function"));
        }
        // NaN / negative → 0 (same clamp as Option A's timer native)
        let delay = arg_num(args, 1, ctx)
            .map(|n| if n.is_nan() || n < 0.0 { 0.0 } else { n })
            .unwrap_or(0.0);
        let rest: Vec<JsValue> = args.iter().skip(2).cloned().collect();
        let id = bside(|b| {
            b.timer_id += 1.0;
            b.timers.push((delay, cb, rest));
            b.timer_id
        });
        Ok(n_val(id))
    }

    fn b_raf(_this: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
        let cb = args.first().cloned().unwrap_or_else(JsValue::undefined);
        if !cb.is_callable() {
            eprintln!("[js] requestAnimationFrame: callback is not a function");
            return Ok(n_val(0.0));
        }
        let id = bside(|b| {
            b.raf_id += 1.0;
            b.raf.push(cb);
            b.raf_id
        });
        Ok(n_val(id))
    }

    // ---- natives: storage / fetch / dialogs / console ----

    fn b_storage_get(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let key = arg_str(args, 0, ctx).unwrap_or_default();
        Ok(host(|h| h.storage_get(&key))
            .map(|v| s_val(&v))
            .unwrap_or_else(JsValue::null))
    }

    fn b_storage_set(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let key = arg_str(args, 0, ctx).unwrap_or_default();
        let val = arg_str(args, 1, ctx).unwrap_or_default();
        host_mut(|h| h.storage_set(&key, &val));
        Ok(JsValue::undefined())
    }

    fn b_storage_remove(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let key = arg_str(args, 0, ctx).unwrap_or_default();
        host_mut(|h| h.storage_remove(&key));
        Ok(JsValue::undefined())
    }

    fn b_storage_length(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(n_val(host(|h| h.storage_len()) as f64))
    }

    fn b_storage_clear(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
        host_mut(|h| h.storage_clear());
        Ok(JsValue::undefined())
    }

    fn b_fetch(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let url = arg_str(args, 0, ctx).unwrap_or_default();
        let Some(abs) = host(|h| h.resolve_url(&url)) else {
            eprintln!("[js] fetch: unresolvable url {url:?}");
            return Ok(JsValue::null());
        };
        match host(|h| h.fetch_text(&url)) {
            Ok((body, ct)) => {
                // shortcut: cap honored by fetch's own 20MB bound; response
                // exposes text() only (no headers/blob) — same as Option A
                let o = obj_val(ctx);
                set_own(&o, ctx, "ok", JsValue::from(true));
                set_own(&o, ctx, "status", n_val(200.0));
                set_own(&o, ctx, "url", s_val(&abs));
                set_own(&o, ctx, "__body", s_val(&body));
                set_own(&o, ctx, "text", fn_obj(ctx, "text", 0, b_response_text));
                if let Some(ct) = ct {
                    set_own(&o, ctx, "contentType", s_val(&ct));
                }
                Ok(o.into())
            }
            Err(err) => {
                eprintln!("[js] fetch failed {abs}: {err}");
                let o = obj_val(ctx);
                set_own(&o, ctx, "ok", JsValue::from(false));
                set_own(&o, ctx, "status", n_val(0.0));
                set_own(&o, ctx, "url", s_val(&abs));
                set_own(&o, ctx, "__body", s_val(""));
                set_own(&o, ctx, "text", fn_obj(ctx, "text", 0, b_response_text));
                Ok(o.into())
            }
        }
    }

    /// `Response.prototype.text()` stand-in: returns the cached `__body`.
    fn b_response_text(this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        match this.as_object() {
            Some(o) => Ok(o.get(js_string!("__body"), ctx)?),
            None => Ok(s_val("")),
        }
    }

    fn b_match_media(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let q = arg_str(args, 0, ctx).unwrap_or_default();
        let dark = host(|h| h.match_media_dark(&q));
        let o = obj_val(ctx);
        set_own(&o, ctx, "matches", JsValue::from(dark));
        set_own(&o, ctx, "media", s_val(&q));
        Ok(o.into())
    }

    // dialog stand-ins: no dialog UI this round, stderr only ([js]) — same
    // wording as Option A.

    fn b_alert(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let msg = arg_str(args, 0, ctx).unwrap_or_default();
        eprintln!("[js] alert({msg:?})");
        Ok(JsValue::undefined())
    }

    fn b_confirm(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let msg = arg_str(args, 0, ctx).unwrap_or_default();
        eprintln!("[js] confirm({msg:?}) → true (no dialog UI; auto-accepts)");
        Ok(JsValue::from(true))
    }

    fn b_prompt(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let msg = arg_str(args, 0, ctx).unwrap_or_default();
        eprintln!("[js] prompt({msg:?}) → null (no input UI)");
        Ok(JsValue::null())
    }

    fn b_console(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        // match engine logging style: script console output goes to stderr
        // (no [js] tag — identical to Option A's console native)
        let mut parts: Vec<String> = Vec::new();
        for a in args {
            if let Some(s) = v_str(a, ctx) {
                parts.push(s);
            }
        }
        eprintln!("{}", parts.join(" "));
        Ok(JsValue::undefined())
    }

    // ---- global surface (mirror of script.rs PageScripts::new) ----

    fn create_globals(ctx: &mut Context, base: &str) {
        let global = ctx.global_object();

        // -- document --------------------------------------------------
        let doc = obj_val(ctx);
        set_own(&doc, ctx, "getElementById", fn_obj(ctx, "document.getElementById", 1, b_d_get_element_by_id));
        set_own(&doc, ctx, "querySelector", fn_obj(ctx, "document.querySelector", 1, b_d_query));
        set_own(&doc, ctx, "querySelectorAll", fn_obj(ctx, "document.querySelectorAll", 1, b_d_query_all));
        set_own(&doc, ctx, "createElement", fn_obj(ctx, "document.createElement", 1, b_d_create_element));
        // no incremental loader: by the time scripts run, parsing is finished
        set_own(&doc, ctx, "readyState", s_val("complete"));
        // document-level listeners share the global (window) bucket (same
        // logged design shortcut as Option A)
        set_own(&doc, ctx, "addEventListener", fn_obj(ctx, "document.addEventListener", 2, b_win_add_listener));
        set_own(&global, ctx, "document", doc);

        // -- storage (merged localStorage == sessionStorage, honest) ----
        let st = obj_val(ctx);
        set_own(&st, ctx, "getItem", fn_obj(ctx, "localStorage.getItem", 1, b_storage_get));
        set_own(&st, ctx, "setItem", fn_obj(ctx, "localStorage.setItem", 2, b_storage_set));
        set_own(&st, ctx, "removeItem", fn_obj(ctx, "localStorage.removeItem", 1, b_storage_remove));
        set_own(&st, ctx, "length", fn_obj(ctx, "localStorage.length", 0, b_storage_length));
        set_own(&st, ctx, "clear", fn_obj(ctx, "localStorage.clear", 0, b_storage_clear));
        set_own(&global, ctx, "localStorage", st.clone());
        set_own(&global, ctx, "sessionStorage", st); // honest: same store

        // -- navigator: fixed honest UA string (no engine names) --------
        let nav = obj_val(ctx);
        set_own(&nav, ctx, "userAgent", s_val("Mazda/1.0"));
        set_own(&global, ctx, "navigator", nav);

        // -- window aliases the global object ---------------------------
        set_own(&global, ctx, "window", global.clone());
        set_own(&global, ctx, "addEventListener", fn_obj(ctx, "addEventListener", 2, b_win_add_listener));
        set_own(&global, ctx, "requestAnimationFrame", fn_obj(ctx, "requestAnimationFrame", 1, b_raf));
        set_own(&global, ctx, "setTimeout", fn_obj(ctx, "setTimeout", 1, b_set_timeout));

        // -- location ---------------------------------------------------
        let loc = obj_val(ctx);
        set_own(&loc, ctx, "href", s_val(base));
        set_own(&loc, ctx, "assign", fn_obj(ctx, "location.assign", 1, b_loc_assign));
        set_own(&loc, ctx, "replace", fn_obj(ctx, "location.replace", 1, b_loc_assign)); // shortcut: same as assign
        set_own(&loc, ctx, "reload", fn_obj(ctx, "location.reload", 0, b_loc_reload));
        set_own(&global, ctx, "location", loc);

        // -- fetch + dialogs + matchMedia -------------------------------
        set_own(&global, ctx, "fetch", fn_obj(ctx, "fetch", 1, b_fetch));
        set_own(&global, ctx, "matchMedia", fn_obj(ctx, "matchMedia", 1, b_match_media));
        set_own(&global, ctx, "alert", fn_obj(ctx, "alert", 1, b_alert));
        set_own(&global, ctx, "confirm", fn_obj(ctx, "confirm", 1, b_confirm));
        set_own(&global, ctx, "prompt", fn_obj(ctx, "prompt", 1, b_prompt));

        // -- console shim: variants all log identically (same as A) -----
        let console = obj_val(ctx);
        for name in ["log", "info", "debug", "warn", "error"] {
            set_own(
                &console,
                ctx,
                name,
                fn_obj(ctx, &format!("console.{name}"), 0, b_console),
            );
        }
        set_own(&global, ctx, "console", console);
    }

    /// The shared element-method prototype: 13 methods bound ONCE per page
    /// (wrappers only carry the `__node` id + prop snapshot). Mirrors the
    /// full surface script.rs wraps per element.
    fn build_element_proto(ctx: &mut Context) -> JsObject {
        let proto = obj_val(ctx);
        let methods: &[(&str, usize, NativeFn)] = &[
            ("getElementById", 1, b_el_get_element_by_id),
            ("querySelector", 1, b_el_query),
            ("querySelectorAll", 1, b_el_query_all),
            ("appendChild", 1, b_el_append),
            ("setAttribute", 2, b_el_set_attr),
            ("getAttribute", 1, b_el_get_attr),
            ("removeAttribute", 1, b_el_rm_attr),
            ("addEventListener", 2, b_el_add_listener),
            ("setTextContent", 1, b_el_set_text),
            ("setInnerHTML", 1, b_el_set_inner),
            ("setValue", 1, b_el_set_value),
            ("setClassName", 1, b_el_set_class),
            ("focus", 0, b_el_focus),
        ];
        for (name, len, f) in methods {
            set_own(&proto, ctx, name, fn_obj(ctx, &format!("Element.{name}"), *len, *f));
        }
        proto
    }

    /// Merge one turn into an accumulator (same fold as Option A).
    fn merge_turn(out: &mut Turn, t: Turn) {
        out.dirty = out.dirty || t.dirty;
        if out.nav.is_none() {
            out.nav = t.nav;
        }
        out.prevent_default = out.prevent_default || t.prevent_default;
    }

    /// Per-page engine-core state. Lives on the loader worker thread (the
    /// engine core's `Context` is not `Send`, exactly like `JsEngine`).
    pub struct RuntimeB {
        ctx: Context,
        /// (node, kind) → callbacks — element + window-level listeners
        listeners: HashMap<(u32, String), Vec<JsValue>>,
        /// absolute deadline → (callback, args) — same shape as Option A
        timers: Vec<(Instant, JsValue, Vec<JsValue>)>,
        /// rAF (deadline, callback) — coalesced frame pass per epoch
        raf: Vec<(Instant, JsValue)>,
        /// in-memory page storage (the same Rc the natives see)
        storage: Storage,
        base: String,
    }

    impl RuntimeB {
        pub fn new(dom: &mut Dom, base: &str) -> RuntimeB {
            // one per-page context (the same "fresh engine per page" design
            // as Option A); dropped with the page on navigation
            let mut ctx = Context::default();
            let storage: Storage = Rc::default();

            bside_reset(); // fresh page: no leaked adapter state
            create_globals(&mut ctx, base);
            let el_proto = build_element_proto(&mut ctx);
            bside(|b| b.el_proto = Some(el_proto));

            // body/documentElement wrappers snapshot DOM props (one
            // init-only keep of the host)
            host_install(dom, base, &storage);
            let body = host(|h| h.body_root());
            let html_el = host(|h| h.html_element());
            let docv = ctx
                .global_object()
                .get(js_string!("document"), &mut ctx)
                .expect("[js] document global vanished during engine-core init");
            if let Some(doc) = docv.as_object() {
                let _ = doc.create_data_property_or_throw(js_string!("body"), wrap(&mut ctx, body), &mut ctx);
                let _ =
                    doc.create_data_property_or_throw(js_string!("documentElement"), wrap(&mut ctx, html_el), &mut ctx);
            }
            let _ = host_take(); // init reads only — nothing to fold

            RuntimeB {
                ctx,
                listeners: HashMap::new(),
                timers: Vec::new(),
                raf: Vec::new(),
                storage,
                base: base.to_string(),
            }
        }

        /// Settle the engine core's own promise/job queue after each call
        /// (the embedder contract the engine's microtasks live in).
        fn settle_jobs(&mut self) {
            if let Err(e) = self.ctx.run_jobs() {
                eprintln!("[js] microtask error: {e}");
            }
        }

        /// Fold the adapter-side queue a turn filled (new listeners into
        /// the table; timers → absolute deadlines; rAF → next frame
        /// epoch), with the same backlog caps as Option A.
        fn take_bside(&mut self) {
            let now = Instant::now();
            for ((node, kind), cb) in bside(|b| std::mem::take(&mut b.listeners)) {
                self.listeners.entry((node, kind)).or_default().push(cb);
            }
            for (delay, cb, args) in bside(|b| std::mem::take(&mut b.timers)) {
                let d = if delay.is_finite() && delay > 0.0 { delay as u64 } else { 0 };
                self.timers.push((now + Duration::from_millis(d), cb, args));
            }
            if self.timers.len() > MAX_PENDING_TIMERS {
                let drop = self.timers.len() - MAX_PENDING_TIMERS;
                let dropped = self.timers.drain(..drop).count();
                eprintln!("[js] timer backlog: dropped {dropped} oldest (cap {MAX_PENDING_TIMERS})");
            }
            let deadline = Instant::now() + Duration::from_millis(FRAME_EPOCH);
            for cb in bside(|b| std::mem::take(&mut b.raf)) {
                self.raf.push((deadline, cb));
            }
            if self.raf.len() > MAX_PENDING_FRAMES {
                let drop = self.raf.len() - MAX_PENDING_FRAMES;
                let dropped = self.raf.drain(..drop).count();
                eprintln!("[js] rAF backlog: dropped {dropped} queued frames (cap {MAX_PENDING_FRAMES})");
            }
        }

        /// Invoke one callback as a turn: fresh host install → call →
        /// settle jobs → fold (side effects + adapter queue). Never
        /// panics: errors come back to be logged by the caller.
        fn call_turn<F>(&mut self, dom: &mut Dom, f: F) -> (JsResult<JsValue>, Turn)
        where
            F: FnOnce(&mut Context) -> JsResult<JsValue>,
        {
            host_install(dom, &self.base, &self.storage);
            let r = f(&mut self.ctx);
            self.settle_jobs();
            self.take_bside();
            let mut host = host_take();
            (
                r,
                Turn {
                    dirty: host.dirty(),
                    nav: host.take_nav(),
                    prevent_default: host.prevent_default(),
                },
            )
        }
    }

    impl ScriptRuntime for RuntimeB {
        /// Run every `<script>` in the document (inline + `src`, document
        /// order), then settle 0 ms timers. Never panics: script errors
        /// are logged `[js]` and skipped. Mirrors script.rs run_scripts.
        fn run_scripts(&mut self, dom: &mut Dom) -> Turn {
            let scripts = crate::script::collect_scripts(dom);
            let mut out = Turn::none();
            for s in &scripts {
                let src = match &s.src {
                    Some(u) => {
                        let abs = match crate::resolve(&self.base, u) {
                            Some(a) => a,
                            None => {
                                eprintln!("[js] script src unresolvable: {u:?}");
                                continue;
                            }
                        };
                        match crate::fetch(&abs) {
                            Ok((bytes, _)) => String::from_utf8_lossy(&bytes).into_owned(),
                            Err(e) => {
                                eprintln!("[js] script fetch failed {abs}: {e}");
                                continue;
                            }
                        }
                    }
                    None => s.text.clone(),
                };
                if src.trim().is_empty() {
                    continue;
                }
                eprintln!(
                    "[js] run {} ({} bytes)",
                    s.src.as_deref().unwrap_or("inline script"),
                    src.len()
                );
                let (r, t) = self.call_turn(dom, |ctx| ctx.eval(Source::from_bytes(&src)));
                merge_turn(&mut out, t);
                if let Err(err) = r {
                    eprintln!("[js] script error: {err}");
                }
            }
            // fire any timers already due (delay 0 from these scripts)
            let t = self.run_due_timers(dom);
            let dirty = out.dirty || t.dirty;
            let nav = if out.nav.is_none() { t.nav } else { out.nav };
            Turn { dirty, nav, prevent_default: false }
        }

        /// Dispatch `kinds` (in order, each its own turn) to `node`'s
        /// listeners, bubbling up the parent chain (no capture phase);
        /// window-level listeners fire last. Same structure as
        /// `PageScripts::dispatch_ext`.
        fn dispatch(
            &mut self,
            dom: &mut Dom,
            node: u32,
            kinds: &[&str],
            key: Option<&str>,
            page_y: Option<f32>,
            value: Option<&str>,
        ) -> Turn {
            // bubbling order: target → ancestors (no capture — shortcut)
            let mut chain = Vec::new();
            let mut cur = Some(node);
            while let Some(n) = cur {
                chain.push(n);
                cur = dom
                    .nodes
                    .get(n as usize)
                    .and_then(|nd| if nd.parent == n { None } else { Some(nd.parent) });
            }
            let mut out = Turn::none();
            for kind in kinds {
                // element chain first, then the window-level stop
                let mut stops: Vec<(u32, bool)> = chain.iter().map(|&n| (n, false)).collect();
                stops.push((WIN_NODE, true));
                for (n, is_win) in stops {
                    let kind_s = kind.to_string();
                    let cbs: Vec<JsValue> = self
                        .listeners
                        .get(&(n, kind_s.clone()))
                        .cloned()
                        .unwrap_or_default();
                    for cb in cbs {
                        let label = format!("[js] listener error ({kind_s} on node {n})");
                        let (r, t) = self.call_turn(dom, |ctx| {
                            let evt = wrap_event(ctx, node, kind, key, page_y, value);
                            // `this` = the element the listener is attached
                            // to; window-level listeners get the event
                            // object (closest honest stand-in — Option A)
                            let this = if is_win { evt.clone() } else { wrap(ctx, n) };
                            match cb.as_object() {
                                Some(func) => func.call(&this, &[evt], ctx),
                                None => Ok(JsValue::undefined()),
                            }
                        });
                        merge_turn(&mut out, t);
                        if let Err(err) = r {
                            eprintln!("{label}: {err}");
                        }
                    }
                }
            }
            let t = self.run_due_timers(dom);
            out.dirty = out.dirty || t.dirty;
            if out.nav.is_none() {
                out.nav = t.nav;
            }
            out
        }

        /// Fire every timer whose deadline passed (loop: a timer may queue
        /// another; chained ones land on later passes — Option A's exact
        /// scheduler).
        fn run_due_timers(&mut self, dom: &mut Dom) -> Turn {
            let now = Instant::now();
            let mut out = Turn::none();
            loop {
                let due = self
                    .timers
                    .iter()
                    .position(|(t, _, _)| *t <= now)
                    .map(|i| self.timers.swap_remove(i));
                let Some((_, cb, args)) = due else { break };
                let (r, t) = self.call_turn(dom, move |ctx| match cb.as_object() {
                    Some(func) => func.call(&JsValue::undefined(), &args, ctx),
                    None => Ok(JsValue::undefined()),
                });
                merge_turn(&mut out, t);
                if let Err(err) = r {
                    eprintln!("[js] timer error: {err}");
                }
            }
            out
        }

        /// One coalesced frame pass: every queued `requestAnimationFrame`
        /// callback whose epoch elapsed fires this pass, then one
        /// re-layout if any mutated the DOM. Same as Option A (no
        /// timestamp argument yet).
        fn run_due_frames(&mut self, dom: &mut Dom) -> Turn {
            let now = Instant::now();
            // due callbacks, oldest first (enqueue order preserved)
            let mut due: Vec<JsValue> = Vec::new();
            let mut alive: Vec<(Instant, JsValue)> = Vec::new();
            for (deadline, cb) in self.raf.drain(..) {
                if deadline <= now {
                    due.push(cb);
                } else {
                    alive.push((deadline, cb));
                }
            }
            self.raf = alive;
            let mut out = Turn::none();
            for cb in due {
                let (r, t) = self.call_turn(dom, move |ctx| match cb.as_object() {
                    Some(func) => func.call(&JsValue::undefined(), &[], ctx),
                    None => Ok(JsValue::undefined()),
                });
                merge_turn(&mut out, t);
                if let Err(err) = r {
                    eprintln!("[js] rAF error: {err}");
                }
            }
            out
        }

        /// The worker's single wake budget: earlier of (next timer
        /// deadline, next rAF epoch). None = block until an input job
        /// arrives.
        fn next_wake_deadline(&self) -> Option<Instant> {
            let timer = self.timers.iter().map(|(t, _, _)| *t).min();
            let frame = self.raf.iter().map(|(t, _)| *t).min();
            match (timer, frame) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
    }
}

#[cfg(all(test, feature = "engine-b"))]
mod engine_b_tests {
    use super::*;

    /// The full per-page contract, driven through `make_runtime` so the
    /// whole selectable path is exercised: script execution mutates the
    /// DOM, listeners fire (and mutate), a 0 ms timer settles within the
    /// run and its navigation reaches the Turn, and storage round-trips.
    #[test]
    fn engine_b_contract_script_dispatch_timer_nav_storage() {
        let mut dom = crate::html::parse(
            r#"<html><body><p id="t">old</p><button id="b">go</button><p id="out"></p><script>
                document.getElementById("t").setTextContent("rusty");
                document.getElementById("b").addEventListener("click", function () {
                    document.getElementById("out").setInnerHTML("clicked");
                });
                localStorage.setItem("k", "val");
                document.getElementById("t").addEventListener("click", function () {
                    document.getElementById("t").setInnerHTML(localStorage.getItem("k"));
                });
                setTimeout(function () { location.assign("/later"); }, 0);
            </script></body></html>"#,
        );
        assert!(EngineChoice::B.available(), "feature build must offer B");
        let mut rt = make_runtime(EngineChoice::B, &mut dom, "https://example.com/");

        let t1 = rt.run_scripts(&mut dom);
        assert!(t1.dirty, "setTextContent during run_scripts must dirty");
        let p = crate::script::query_all_dom(&dom, "#t")[0];
        assert_eq!(crate::script::child_text(&dom, p), "rusty");
        assert_eq!(
            t1.nav.as_deref(),
            Some("https://example.com/later"),
            "the 0 ms timer fires within run_scripts; nav lands in the Turn"
        );

        // dispatch: the button listener runs and mutates (dirty + text)
        let b = crate::script::query_all_dom(&dom, "#b")[0];
        let t2 = rt.dispatch(&mut dom, b, &["click"], None, None, None);
        assert!(t2.dirty, "listener mutation must dirty");
        let out = crate::script::query_all_dom(&dom, "#out")[0];
        assert_eq!(crate::script::child_text(&dom, out), "clicked");
        assert_eq!(t2.nav, None);
        assert!(!t2.prevent_default);

        // storage flows back into a listener-driven mutation
        let t = crate::script::query_all_dom(&dom, "#t")[0];
        rt.dispatch(&mut dom, t, &["click"], None, None, None);
        let t = crate::script::query_all_dom(&dom, "#t")[0];
        assert_eq!(crate::script::child_text(&dom, t), "val", "localStorage round trip");

        // timers drained; idle wake = None (block on input)
        assert!(rt.next_wake_deadline().is_none(), "queue exhausted after the pass");
    }

    /// A script error is logged, never fatal, and the page survives.
    #[test]
    fn engine_b_error_is_logged_not_fatal() {
        let mut dom = crate::html::parse(
            r#"<html><body><script>throw new Error("boom");</script>
                <p id="after">x</p></body></html>"#,
        );
        let mut rt = make_runtime(EngineChoice::B, &mut dom, "https://example.com/");
        let _ = rt.run_scripts(&mut dom);
        assert_eq!(
            crate::script::query_all_dom(&dom, "#after").len(),
            1,
            "page survives a script throw"
        );
    }
}

#[cfg(test)]
mod conf_tests {
    use super::*;

    /// Round trip: serialize → parse → same engine (the whole conf format).
    #[test]
    fn conf_round_trip() {
        for choice in [EngineChoice::A, EngineChoice::B] {
            let c = Conf { engine: choice };
            let text = serialize_conf(&c);
            assert_eq!(text, format!("engine={}\n", choice.as_conf()));
            let back = parse_conf(&text);
            assert_eq!(back, c, "round trip for engine={}", choice.as_conf());
        }
    }

    /// A corrupt file (garbage line) resets to the built-in default A —
    /// settings may be lost, never a panic.
    #[test]
    fn corrupt_conf_falls_back_to_default_a() {
        assert_eq!(parse_conf("garbage line"), Conf::default());
        assert_eq!(parse_conf("engine=q"), Conf::default());
        assert_eq!(parse_conf("engine="), Conf::default());
        assert_eq!(parse_conf("engine=a\nbroken-no-equals"), Conf::default());
    }

    /// Forward-compat: unknown keys are ignored, trailing whitespace and
    /// comments tolerated, empty file → default A.
    #[test]
    fn conf_tolerates_noise_and_defaults_clean() {
        assert_eq!(parse_conf("# comment\n\n  engine = b  \nx_next=1"), Conf { engine: EngineChoice::B });
        // default A when a known-but-A value or nothing is present
        assert_eq!(parse_conf("other=yes"), Conf::default());
        assert_eq!(parse_conf(""), Conf::default());
    }

    /// PRECEDENCE: `--engine` flag > MAZDA_ENGINE env > conf file value.
    #[test]
    fn engine_precedence_flag_env_conf() {
        let conf_on_disk = Conf { engine: EngineChoice::B };
        // flag wins over env and disk
        assert_eq!(
            resolve_engine(Some(EngineChoice::A), Some("b"), conf_on_disk).engine,
            EngineChoice::A
        );
        // env wins over disk
        assert_eq!(
            resolve_engine(None, Some("b"), conf_on_disk).engine,
            EngineChoice::B
        );
        // disk value when neither is set
        assert_eq!(
            resolve_engine(None, None, conf_on_disk).engine,
            EngineChoice::B
        );
        // honest subset: an errant env value falls back to the disk value
        // (env parsing accepts only a/b; anything else is treated as unset
        // by Conf::init's ok/check), disk still decides
    }

    /// The CLI knob's accepted spellings (and rejections).
    #[test]
    fn cli_flag_spellings() {
        let args = |s: &str| vec![s.to_string()];
        assert_eq!(cli_engine_flag(&args("--engine=a")), Some(EngineChoice::A));
        assert_eq!(cli_engine_flag(&args("--engine=b")), Some(EngineChoice::B));
        assert_eq!(cli_engine_flag(&args("--engine=x")), None);
        assert_eq!(cli_engine_flag(&args("https://x.com")), None);
        assert_eq!(cli_engine_flag(&args("--engine")), None);
    }
}
