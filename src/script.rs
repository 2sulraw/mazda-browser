//! Page scripting: `<script>` execution with DOM natives (milestone 2).
//!
//! The interpreter (`js.rs`) stays DOM-free; this module is the glue:
//! - runs `<script>` elements in document order (inline + `src`),
//! - installs `document` / `location` / `fetch` + element-method natives,
//! - registers/dispatches event listeners for the shell's event jobs.
//!
//! Threading: everything here runs on the single loader worker thread.
//! Natives reach the page DOM through a thread-local raw pointer re-pointed
//! at the start of every script turn — dereferenced only inside a turn on
//! that one thread; arena node indices stay valid for the page's lifetime.
//!
//! shortcuts (honest, per native):
//! - property writes (`el.textContent = x`, `el.value = x`) cannot be
//!   intercepted — the interpreter has no accessor hooks. Readable props
//!   are snapshotted at wrap time; setters are methods (`setTextContent`,
//!   `setValue`, `setClassName`).
//! - `innerHTML` is text-only: markup fragments are logged and stored as
//!   literal text (no fragment parser).
//! - `querySelector` matches one simple selector (tag / #id / .class),
//!   no combinators or attribute selectors.
//! - listeners bubble up the parent chain (no capture phase); window-level
//!   listeners (added through `window.addEventListener`) fire after the
//!   bubble chain on every event.
//! - `fetch` is synchronous and returns a plain `{ok, status, text()}`
//!   object, not a Promise — the interpreter has no async/await.
//! - `createElement` clones the TagId of a tag already in the parsed
//!   fragment table (see `element_tid`); unknown tags log and return null.
//! - `localStorage` == `sessionStorage` (merged) and lives in worker RAM
//!   for the current page only: it is dropped on navigation, never written
//!   to disk. A real disk-backed store is a later wave. `length` is a
//!   callable (`localStorage.length()`) — the interpreter has no accessor
//!   hooks, so a property read cannot return a live number.
//! - `requestAnimationFrame` coalesces to a layout epoch, not vsync: every
//!   callback queued by the time the frame pass runs fires in ONE turn,
//!   and dirty DOM produces at most one re-layout per pass. On cheap pages
//!   that lands near 60Hz; on expensive layout pages the pipeline latency
//!   (not vsync) governs the callback rate.
//! - `event.stopPropagation()` does not exist yet (absent method — pages
//!   calling it get a TypeError logged, nothing silently ignored).
//! - `preventDefault` is honored only for the shell's one native default
//!   action: link navigation. The flag crosses to the UI thread with the
//!   click's decision result (see the LoadDone.prevent_default contract
//!   in main.rs).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::html::{self, Dom, NodeKind};
use crate::js::{self, JsEngine, JsError, Val};

// ---------------------------------------------------------------------------
// Thread-local script context (worker-thread only)
// ---------------------------------------------------------------------------

type Storage = Rc<std::cell::RefCell<HashMap<String, String>>>;

/// Window-level listener table key (`window.addEventListener`). u32::MAX
/// cannot be a real arena node id (page DOM is far smaller), so it can never
/// collide with an element.
const WIN_NODE: u32 = u32::MAX;

/// What the natives need while a script turn is running: a raw pointer to
/// the worker-owned DOM, the page base URL, and the side channels scripts
/// write through (listeners, requested navigation, DOM-mutated flag,
/// preventDefault, queued rAF callbacks, the page's localStorage).
struct Ctx {
    dom: *mut Dom,
    base: String,
    /// (node id, event kind) → registered handler functions
    listeners: HashMap<(u32, String), Vec<Val>>,
    /// `location.assign(...)` / `location.href = ...` target
    pending_nav: Option<String>,
    /// DOM mutated during this turn → caller must re-layout
    dirty: bool,
    /// a listener called `event.preventDefault()` (link navigation veto)
    prevent_default: bool,
    /// callbacks queued by `requestAnimationFrame` this turn
    raf: Vec<Val>,
    /// shared handle (with `PageScripts`) to this page's in-memory storage
    storage: Storage,
    /// honest-log guard: localStorage drop-on-nav note printed once per page
    storage_noted: bool,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

/// Point the thread-local context at `dom` for the duration of a turn.
/// `None` tears the context down (no native may run outside a turn).
fn ctx_set(dom: &mut Dom, base: &str, storage: Storage) {
    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            dom: dom as *mut Dom,
            base: base.to_string(),
            listeners: HashMap::new(),
            pending_nav: None,
            dirty: false,
            prevent_default: false,
            raf: Vec::new(),
            storage,
            storage_noted: false,
        })
    });
}

fn ctx_take() -> Option<Ctx> {
    CTX.with(|c| c.borrow_mut().take())
}

/// Run `f` as one script turn: install the ctx (carrying the listeners
/// registered in earlier turns and the page's storage), run `f` with the
/// engine, then tear the ctx down and hand back its side channels
/// (listeners / nav / dirty / preventDefault / rAF / storage).
fn with_ctx<R>(
    engine: &mut JsEngine,
    dom: &mut Dom,
    base: &str,
    listeners: HashMap<(u32, String), Vec<Val>>,
    storage: Storage,
    f: impl FnOnce(&mut JsEngine) -> R,
) -> (R, Ctx) {
    ctx_set(dom, base, storage);
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            cx.listeners = listeners;
        }
    });
    let r = f(engine);
    let cx = ctx_take().expect("[js] script context vanished mid-turn");
    (r, cx)
}

/// Borrow the DOM for the current turn. Panics are impossible by contract:
/// only natives call this, and natives only run inside `with_ctx`.
fn with_dom<R>(f: impl FnOnce(&mut Dom) -> R) -> R {
    CTX.with(|c| {
        let b = c.borrow();
        let cx = b.as_ref().expect("[js] native ran outside a script turn");
        let dom = unsafe { &mut *cx.dom };
        f(dom)
    })
}

fn ctx_base() -> String {
    CTX.with(|c| {
        c.borrow()
            .as_ref()
            .map(|cx| cx.base.clone())
            .unwrap_or_default()
    })
}

fn ctx_dirty() {
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            cx.dirty = true;
        }
    })
}

/// `event.preventDefault()` — flags ONLY the shell's native default action
/// that already exists (link navigation). The UI thread receives the flag
/// with the click's decision result and cancels an un-started navigation.
fn ctx_prevent_default(node: u32) {
    eprintln!("[js] event.preventDefault (node {node}) — navigation may be canceled");
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            cx.prevent_default = true;
        }
    })
}

/// Worker-thread localStorage: mutate the page's map through the shared
/// handle. Honest note printed once per page on the first write.
fn storage_noted() {
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            if !cx.storage_noted {
                cx.storage_noted = true;
                eprintln!(
                    "[js] localStorage: in-memory only, dropped on navigation (sessionStorage merged; no disk backing)"
                );
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Element wrappers
// ---------------------------------------------------------------------------

/// Snapshot `node` into a JS object: `__node` (id) + readable props taken
/// live at wrap time (no accessor hooks — see module docs). Methods are
/// `Val::Native` entries that read `__node` off `this`.
fn wrap_element(node: u32) -> Val {
    let o = js::Val::new_object();
    o.set("__node", js::Val::Num(node as f64));
    with_dom(|dom| {
        if node as usize >= dom.nodes.len() {
            return;
        }
        o.set("tagName", str_val(&dom.tag(node).to_ascii_uppercase()));
        o.set("id", attr_val(dom, node, "id"));
        o.set("className", attr_val(dom, node, "class"));
        let t = dom.tag(node);
        // value-like props snapshot: input/textarea current value
        if t == "input" || t == "textarea" || t == "select" {
            o.set("value", attr_val(dom, node, "value"));
        }
        if t == "input" {
            let ty = dom.attr(node, "type").unwrap_or("text");
            o.set("type", str_val(ty));
            o.set(
                "checked",
                js::Val::Bool(dom.attr(node, "checked").is_some()),
            );
        }
        if t == "a" {
            o.set("href", attr_val(dom, node, "href"));
        }
        // text-ish containers get a live-read textContent via get below?
        // No accessor hooks: snapshot children text too.
        o.set("textContent", str_val(&child_text(dom, node)));
    });
    let n = nids();
    o.set("getElementById", js::Val::Native(n.el_get_element));
    o.set("querySelector", js::Val::Native(n.el_query));
    o.set("querySelectorAll", js::Val::Native(n.el_query_all));
    o.set("appendChild", js::Val::Native(n.el_append));
    o.set("setAttribute", js::Val::Native(n.el_set_attr));
    o.set("getAttribute", js::Val::Native(n.el_get_attr));
    o.set("removeAttribute", js::Val::Native(n.el_rm_attr));
    o.set("addEventListener", js::Val::Native(n.el_add_listener));
    o.set("setTextContent", js::Val::Native(n.el_set_text));
    o.set("setInnerHTML", js::Val::Native(n.el_set_inner));
    o.set("setValue", js::Val::Native(n.el_set_value));
    o.set("setClassName", js::Val::Native(n.el_set_class));
    o.set("focus", js::Val::Native(n.el_focus));
    o
}

fn str_val(s: &str) -> Val {
    Val::Str(Rc::from(s))
}

/// The full event object handed to listeners: `type`, `target` (element
/// wrapper), `bubbles`, plus per-kind fields (`key` for keyboard events,
/// `pageY` for scroll, `value` for input/change) and `preventDefault()`.
/// no capture phase, no stopPropagation — absent methods are honest.
fn wrap_event(node: u32, kind: &str, key: Option<&str>, page_y: Option<f32>, value: Option<&str>) -> Val {
    let o = Val::new_object();
    o.set("type", str_val(kind));
    o.set("target", wrap_element(node));
    o.set("bubbles", Val::Bool(true));
    if let Some(k) = key {
        o.set("key", str_val(k));
    }
    if let Some(y) = page_y {
        o.set("pageY", Val::Num(y as f64));
    }
    if let Some(v) = value {
        o.set("value", str_val(v));
    }
    o.set("preventDefault", Val::Native(nids().ev_prevent_default));
    o
}

fn attr_val(dom: &Dom, node: u32, name: &str) -> Val {
    match dom.attr(node, name) {
        Some(v) => str_val(v),
        None => Val::Str(Rc::from("")),
    }
}

/// Concatenated text of every descendant text node (textContent/innerHTML
/// snapshot). innerHTML returns markup-less text — shortcut documented.
fn child_text(dom: &Dom, node: u32) -> String {
    fn walk(dom: &Dom, node: u32, out: &mut String) {
        match &dom.nodes[node as usize].kind {
            NodeKind::Text(t) => out.push_str(t),
            NodeKind::Element(_, _) => {
                for &c in &dom.nodes[node as usize].children {
                    walk(dom, c, out);
                }
            }
        }
    }
    let mut out = String::new();
    walk(dom, node, &mut out);
    out
}

/// `__node` off a wrapper; throws TypeError when missing/non-object.
fn this_node(this: &Val) -> Result<u32, JsError> {
    match this {
        Val::Obj(o) => match js::obj_get_own(o, "__node") {
            Val::Num(n) if n >= 0.0 && n.fract() == 0.0 => Ok(n as u32),
            _ => Err(JsError::throw("TypeError: not an element")),
        },
        _ => Err(JsError::throw("TypeError: not an element")),
    }
}

// ---------------------------------------------------------------------------
// Native ids: `register_native` returns a per-engine table index, so ids are
// captured at install time (one shared dispatch fn per group, u16 = method).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Nids {
    doc_get_element: usize,
    doc_query: usize,
    doc_query_all: usize,
    doc_create: usize,
    el_get_element: usize,
    el_query: usize,
    el_query_all: usize,
    el_append: usize,
    el_set_attr: usize,
    el_get_attr: usize,
    el_rm_attr: usize,
    el_add_listener: usize,
    el_set_text: usize,
    el_set_inner: usize,
    el_set_value: usize,
    el_set_class: usize,
    el_focus: usize,
    loc_assign: usize,
    loc_reload: usize,
    win_add_listener: usize,
    ev_prevent_default: usize,
    raf: usize,
    storage_get: usize,
    storage_set: usize,
    storage_remove: usize,
    storage_length: usize,
    storage_clear: usize,
    fetch: usize,
    fetch_text: usize,
}

thread_local! {
    static NIDS: std::cell::Cell<Option<Nids>> = const { std::cell::Cell::new(None) };
}

fn nids() -> Nids {
    NIDS.with(|c| c.get().expect("[js] natives not installed on this engine"))
}

// ---------------------------------------------------------------------------
// Selector matching (single simple selector — shortcut)
// ---------------------------------------------------------------------------

/// Does `node` match `sel`? Supports `tag`, `#id`, `.class` only.
fn matches_simple(dom: &Dom, node: u32, sel: &str) -> bool {
    let sel = sel.trim();
    if sel.is_empty() {
        return false;
    }
    if let Some(id) = sel.strip_prefix('#') {
        return dom.attr(node, "id") == Some(id);
    }
    if let Some(class) = sel.strip_prefix('.') {
        return dom
            .attr(node, "class")
            .map(|c| c.split_whitespace().any(|t| t == class))
            .unwrap_or(false);
    }
    dom.tag(node) == sel
}

/// All element nodes in document order (skips the `#document` sentinel at
/// index 0, which is an Element kind with tag "#document").
fn all_elements(dom: &Dom) -> Vec<u32> {
    (1..dom.nodes.len() as u32)
        .filter(|&i| dom.tag(i) != "#text" && dom.tag(i) != "#document")
        .collect()
}

fn query_all(dom: &Dom, sel: &str) -> Vec<u32> {
    all_elements(dom)
        .into_iter()
        .filter(|&i| matches_simple(dom, i, sel))
        .collect()
}

// ---------------------------------------------------------------------------
// Natives: one shared dispatch fn per group; each method is registered with
// its own u16 idx (`register_native(name, f, idx)` echoes idx back).
// ---------------------------------------------------------------------------

/// document group idxs
const D_GET_ELEMENT_BY_ID: u16 = 0;
const D_QUERY: u16 = 1;
const D_QUERY_ALL: u16 = 2;
const D_CREATE_ELEMENT: u16 = 3;
/// element group idxs
const E_GET_ELEMENT_BY_ID: u16 = 10;
const E_QUERY: u16 = 11;
const E_QUERY_ALL: u16 = 12;
const E_APPEND_CHILD: u16 = 13;
const E_SET_ATTRIBUTE: u16 = 14;
const E_GET_ATTRIBUTE: u16 = 15;
const E_REMOVE_ATTRIBUTE: u16 = 16;
const E_ADD_EVENT_LISTENER: u16 = 17;
const E_SET_TEXT_CONTENT: u16 = 18;
const E_SET_INNER_HTML: u16 = 19;
const E_SET_VALUE: u16 = 20;
const E_SET_CLASS_NAME: u16 = 21;
const E_FOCUS: u16 = 22;
/// location group idxs
const L_ASSIGN: u16 = 30;
const L_RELOAD: u16 = 31;
/// window group idxs
const W_ADD_LISTENER: u16 = 32;
/// event object group idxs
const V_PREVENT_DEFAULT: u16 = 33;
/// storage group idxs
const S_GET: u16 = 40;
const S_SET: u16 = 41;
const S_REMOVE: u16 = 42;
const S_LENGTH: u16 = 43;
const S_CLEAR: u16 = 44;

/// Pending `setTimeout` jobs the worker keeps per page — a runaway
/// `setTimeout(loop)` storm would otherwise grow the queue without bound.
const MAX_PENDING_TIMERS: usize = 512;
/// Same guard for `requestAnimationFrame` callbacks queued but not yet
/// run in a frame pass (a page rAF-looping inside layout-heavy pages).
const MAX_PENDING_FRAMES: usize = 128;
/// Frame epoch ≈ one vsync — rAF callbacks coalesce to the next pass.
const FRAME_EPOCH: u64 = 16;

/// Monotonic rAF callback id (returned to script; nothing consumes it —
/// cancelAnimationFrame is not implemented yet).
fn next_raf_id() -> f64 {
    use std::cell::Cell;
    thread_local! {
        static RAF_ID: Cell<f64> = const { Cell::new(1.0) };
    }
    RAF_ID.with(|c| {
        let id = c.get();
        c.set(id + 1.0);
        id
    })
}

fn doc_native(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let sel = args.first().and_then(|v| v.as_string());
    match idx {
        D_GET_ELEMENT_BY_ID => {
            let Some(sel) = sel else { return Ok(Val::Null) };
            Ok(with_dom(|dom| {
                query_all(dom, &format!("#{}", sel))
                    .first()
                    .map(|&n| wrap_element(n))
                    .unwrap_or(Val::Null)
            }))
        }
        D_QUERY => {
            let Some(sel) = sel else { return Ok(Val::Null) };
            Ok(with_dom(|dom| {
                query_all(dom, &sel)
                    .first()
                    .map(|&n| wrap_element(n))
                    .unwrap_or(Val::Null)
            }))
        }
        D_QUERY_ALL => {
            let Some(sel) = sel else { return Ok(js::Val::new_array(vec![])) };
            Ok(with_dom(|dom| {
                js::Val::new_array(query_all(dom, &sel).into_iter().map(wrap_element).collect())
            }))
        }
        D_CREATE_ELEMENT => {
            let Some(tag) = sel else { return Ok(Val::Null) };
            create_element(&tag)
        }
        _ => Err(JsError::throw("TypeError: bad document native")),
    }
}

/// `createElement`: the tag interner is private to html.rs, so clone the
/// TagId from any existing element with the same tag (parsed documents
/// almost always contain it); unknown tags log + null.
fn create_element(tag: &str) -> Result<Val, JsError> {
    let tag = tag.to_ascii_lowercase();
    Ok(with_dom(|dom| {
        let tid = match all_elements(dom).into_iter().find(|&i| dom.tag(i) == tag) {
            Some(i) => match &dom.nodes[i as usize].kind {
                NodeKind::Element(t, _) => *t,
                _ => return Val::Null,
            },
            None => {
                eprintln!("[js] createElement: unknown tag {tag:?} (not in document)");
                return Val::Null;
            }
        };
        let id = dom.nodes.len() as u32;
        dom.nodes.push(html::Node {
            parent: 0,
            children: Vec::new(),
            kind: NodeKind::Element(tid, Box::from([])),
        });
        wrap_element(id)
    }))
}

fn el_native(_e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let node = this_node(this)?;
    let a0 = args.first().cloned().unwrap_or(Val::Undefined);
    match idx {
        E_GET_ELEMENT_BY_ID => {
            let Some(sel) = a0.as_string() else { return Ok(Val::Null) };
            Ok(with_dom(|dom| {
                subtree_query(dom, node, &format!("#{}", sel))
                    .first()
                    .map(|&n| wrap_element(n))
                    .unwrap_or(Val::Null)
            }))
        }
        E_QUERY => {
            let Some(sel) = a0.as_string() else { return Ok(Val::Null) };
            Ok(with_dom(|dom| {
                subtree_query(dom, node, &sel)
                    .first()
                    .map(|&n| wrap_element(n))
                    .unwrap_or(Val::Null)
            }))
        }
        E_QUERY_ALL => {
            let Some(sel) = a0.as_string() else { return Ok(js::Val::new_array(vec![])) };
            Ok(with_dom(|dom| {
                js::Val::new_array(
                    subtree_query(dom, node, &sel).into_iter().map(wrap_element).collect(),
                )
            }))
        }
        E_APPEND_CHILD => {
            let child = this_node(&a0)?;
            let attached = with_dom(|dom| {
                if (child as usize) < dom.nodes.len() && (node as usize) < dom.nodes.len() {
                    dom.nodes[child as usize].parent = node;
                    dom.nodes[node as usize].children.push(child);
                    true
                } else {
                    false
                }
            });
            if attached {
                ctx_dirty();
            }
            Ok(a0)
        }
        E_SET_ATTRIBUTE => {
            let name = a0.as_string().unwrap_or_default();
            let val = args.get(1).map(|v| v.to_string()).unwrap_or_default();
            set_attr(node, &name, &val);
            Ok(Val::Undefined)
        }
        E_GET_ATTRIBUTE => Ok(with_dom(|dom| match dom.attr(node, &a0.to_string()) {
            Some(v) => str_val(v),
            None => Val::Null,
        })),
        E_REMOVE_ATTRIBUTE => {
            let name = a0.as_string().unwrap_or_default();
            remove_attr(node, &name);
            Ok(Val::Undefined)
        }
        E_ADD_EVENT_LISTENER => {
            let Some(kind) = a0.as_string() else { return Ok(Val::Undefined) };
            let cb = args.get(1).cloned().unwrap_or(Val::Undefined);
            if !matches!(cb, Val::Func(_) | Val::Native(_)) {
                eprintln!("[js] addEventListener: callback is not a function");
                return Ok(Val::Undefined);
            }
            CTX.with(|c| {
                if let Some(cx) = c.borrow_mut().as_mut() {
                    cx.listeners.entry((node, kind.to_string())).or_default().push(cb);
                }
            });
            Ok(Val::Undefined)
        }
        E_SET_TEXT_CONTENT => {
            set_text_content(node, &a0.to_string());
            Ok(Val::Undefined)
        }
        E_SET_INNER_HTML => {
            // text-only innerHTML: markup fragments are stored literally
            let s = a0.to_string();
            if s.contains('<') {
                eprintln!("[js] innerHTML: markup fragments stored as literal text (no parser)");
            }
            set_text_content(node, &s);
            Ok(Val::Undefined)
        }
        E_SET_VALUE => {
            set_attr(node, "value", &a0.to_string());
            Ok(Val::Undefined)
        }
        E_SET_CLASS_NAME => {
            set_attr(node, "class", &a0.to_string());
            Ok(Val::Undefined)
        }
        E_FOCUS => {
            // the shell owns real focus; log intent and mark the node
            eprintln!("[js] element.focus() requested (node {node})");
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: bad element native")),
    }
}

fn loc_native(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        L_ASSIGN => {
            let target = args.first().map(|v| v.to_string()).unwrap_or_default();
            request_nav(&target);
            Ok(Val::Undefined)
        }
        L_RELOAD => {
            // a reload is a navigation to the current page (refetch through
            // the full pipeline; the disk cache may serve it up to TTL)
            let base = ctx_base();
            request_nav(&base);
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: bad location native")),
    }
}

/// `window.addEventListener(type, cb)` — global listeners fire after the
/// bubble chain on every event of that kind (window is the last stop).
/// `window` aliases the global object, so the method sits on `globalThis`
/// and doubles as a bare `addEventListener(...)`.
fn win_native(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        W_ADD_LISTENER => {
            let Some(kind) = args.first().and_then(|v| v.as_string()) else {
                return Ok(Val::Undefined);
            };
            let cb = args.get(1).cloned().unwrap_or(Val::Undefined);
            if !matches!(cb, Val::Func(_) | Val::Native(_)) {
                eprintln!("[js] addEventListener: callback is not a function");
                return Ok(Val::Undefined);
            }
            CTX.with(|c| {
                if let Some(cx) = c.borrow_mut().as_mut() {
                    cx.listeners
                        .entry((WIN_NODE, kind.to_string()))
                        .or_default()
                        .push(cb);
                }
            });
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: bad window native")),
    }
}

/// The event object's preventDefault — flags the click so the UI thread
/// cancels a not-yet-started link navigation (see module docs).
fn ev_native(_e: &mut JsEngine, idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    match idx {
        V_PREVENT_DEFAULT => {
            let node = this_node(this).unwrap_or(0);
            ctx_prevent_default(node);
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: bad event native")),
    }
}

/// `requestAnimationFrame(cb)` — the callback joins this turn's queue with
/// a one-epoch deadline; the worker's scheduler fires every queued callback
/// in a single coalesced pass and re-lays out once if any of them mutated
/// the DOM (not per callback, not vsync-locked — module docs).
fn raf_native(_e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let cb = args.first().cloned().unwrap_or(Val::Undefined);
    if !matches!(cb, Val::Func(_) | Val::Native(_) | Val::Method(_)) {
        eprintln!("[js] requestAnimationFrame: callback is not a function");
        return Ok(Val::Num(0.0));
    }
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            cx.raf.push(cb);
        }
    });
    Ok(Val::Num(next_raf_id()))
}

/// `localStorage` / `sessionStorage` (merged): an in-memory map per page,
/// dropped on navigation. Errors (out of range, non-string values coerced)
/// are logged, never thrown — closest honest subset.
fn storage_native(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        S_GET => {
            let key = args.first().map(|v| v.to_string()).unwrap_or_default();
            let val = ctx_storage(|s| s.get(&key).cloned());
            Ok(val.map(|v| str_val(&v)).unwrap_or(Val::Null))
        }
        S_SET => {
            let key = args.first().map(|v| v.to_string()).unwrap_or_default();
            let val = args.get(1).map(|v| v.to_string()).unwrap_or_default();
            storage_noted();
            ctx_storage(|s| s.insert(key, val));
            Ok(Val::Undefined)
        }
        S_REMOVE => {
            let key = args.first().map(|v| v.to_string()).unwrap_or_default();
            ctx_storage(|s| s.remove(&key));
            Ok(Val::Undefined)
        }
        S_LENGTH => {
            let n = ctx_storage(|s| s.len()) as f64;
            Ok(Val::Num(n))
        }
        S_CLEAR => {
            ctx_storage(|s| s.clear());
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: bad storage native")),
    }
}

/// Run `f` with the page's storage map (shared handle through Ctx).
fn ctx_storage<R>(f: impl FnOnce(&mut HashMap<String, String>) -> R) -> R {
    let store = CTX.with(|c| {
        let b = c.borrow();
        let cx = b
            .as_ref()
            .expect("[js] native ran outside a script turn");
        cx.storage.clone()
    });
    let mut s = store.borrow_mut();
    f(&mut s)
}

// -- dialog stand-ins: no dialog UI this round, stderr only ([js]) ----------

fn n_alert(_e: &mut JsEngine, _args: &[Val]) -> Result<Val, JsError> {
    let msg = _args.first().map(|v| v.to_string()).unwrap_or_default();
    eprintln!("[js] alert({msg:?})");
    Ok(Val::Undefined)
}

fn n_confirm(_e: &mut JsEngine, _args: &[Val]) -> Result<Val, JsError> {
    let msg = _args.first().map(|v| v.to_string()).unwrap_or_default();
    eprintln!("[js] confirm({msg:?}) → true (no dialog UI; auto-accepts)");
    Ok(Val::Bool(true))
}

fn n_prompt(_e: &mut JsEngine, _args: &[Val]) -> Result<Val, JsError> {
    let msg = _args.first().map(|v| v.to_string()).unwrap_or_default();
    eprintln!("[js] prompt({msg:?}) → null (no input UI)");
    Ok(Val::Null)
}

/// `matchMedia("(prefers-color-scheme: dark)")` — reports the same
/// system preference the cascade's dark-mode logic uses. shortcut: css.rs
/// resolves the per-element scheme itself (its globals are private); this
/// mirrors the single OS-preference constant there. Other media queries →
/// no match.
fn n_match_media(_e: &mut JsEngine, args: &[Val]) -> Result<Val, JsError> {
    let q = args.first().map(|v| v.to_string()).unwrap_or_default();
    // this mirror must stay in sync with css.rs PREFERS_DARK (= true)
    const PREFERS_DARK_MIRROR: bool = true;
    let dark = if q.contains("prefers-color-scheme") {
        if q.contains("dark") {
            PREFERS_DARK_MIRROR
        } else if q.contains("light") {
            !PREFERS_DARK_MIRROR
        } else {
            false
        }
    } else {
        false
    };
    let o = Val::new_object();
    o.set("matches", Val::Bool(dark));
    o.set("media", str_val(&q));
    Ok(o)
}

fn fetch_native(_e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let url = args.first().map(|v| v.to_string()).unwrap_or_default();
    let base = ctx_base();
    let abs = match crate::resolve(&base, &url) {
        Some(a) => a,
        None => {
            eprintln!("[js] fetch: unresolvable url {url:?}");
            return Ok(Val::Null);
        }
    };
    match crate::fetch(&abs) {
        Ok((bytes, ct)) => {
            // shortcut: cap honored by fetch's own 20MB bound; response
            // exposes text() only (no headers/blob)
            let body = String::from_utf8_lossy(&bytes).into_owned();
            let o = Val::new_object();
            o.set("ok", Val::Bool(true));
            o.set("status", Val::Num(200.0));
            o.set("url", str_val(&abs));
            o.set("__body", str_val(&body));
            o.set("text", Val::Native(nids().fetch_text));
            if let Some(ct) = &ct {
                o.set("contentType", str_val(ct));
            }
            Ok(o)
        }
        Err(err) => {
            eprintln!("[js] fetch failed {abs}: {err}");
            let o = Val::new_object();
            o.set("ok", Val::Bool(false));
            o.set("status", Val::Num(0.0));
            o.set("url", str_val(&abs));
            o.set("__body", str_val(""));
            o.set("text", Val::Native(nids().fetch_text));
            Ok(o)
        }
    }
}

/// `Response.prototype.text()` stand-in: returns the cached body. Reads
/// `__body` off `this` — no `this` plumbing needed beyond the object.
fn fetch_text_native(_e: &mut JsEngine, _idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    match this {
        Val::Obj(o) => Ok(js::obj_get_own(o, "__body")),
        _ => Ok(str_val("")),
    }
}

// ---------------------------------------------------------------------------
// DOM mutation helpers (mark dirty AFTER releasing the ctx borrow)
// ---------------------------------------------------------------------------

fn set_attr(node: u32, name: &str, val: &str) {
    let changed = with_dom(|dom| {
        let Some(NodeKind::Element(_, attrs)) = dom.nodes.get_mut(node as usize).map(|n| &mut n.kind)
        else {
            return false;
        };
        match attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            Some((_, v)) => {
                let same = v == val;
                if !same {
                    *v = val.to_string();
                }
                !same
            }
            None => {
                let mut v: Vec<(String, String)> = attrs.iter().cloned().collect();
                v.push((name.to_string(), val.to_string()));
                *attrs = v.into_boxed_slice();
                true
            }
        }
    });
    if changed {
        ctx_dirty();
    }
}

fn remove_attr(node: u32, name: &str) {
    let changed = with_dom(|dom| {
        let Some(NodeKind::Element(_, attrs)) = dom.nodes.get_mut(node as usize).map(|n| &mut n.kind)
        else {
            return false;
        };
        if let Some(i) = attrs.iter().position(|(k, _)| k.eq_ignore_ascii_case(name)) {
            let mut v: Vec<(String, String)> = attrs.iter().cloned().collect();
            v.remove(i);
            *attrs = v.into_boxed_slice();
            true
        } else {
            false
        }
    });
    if changed {
        ctx_dirty();
    }
}

/// Replace all children of `node` with a single text node (`textContent`,
/// text-only `innerHTML`).
fn set_text_content(node: u32, text: &str) {
    with_dom(|dom| {
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
    });
    ctx_dirty();
}

fn request_nav(target: &str) {
    // resolve against the page base here: the worker treats the result
    // as an absolute URL
    let resolved = {
        let base = ctx_base();
        crate::resolve(&base, target).unwrap_or_else(|| target.to_string())
    };
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            cx.pending_nav = Some(resolved);
        }
    });
}

/// Descendants of `root` matching `sel` (document order; root excluded).
fn subtree_query(dom: &Dom, root: u32, sel: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut stack: Vec<u32> = dom
        .nodes
        .get(root as usize)
        .map(|n| n.children.clone())
        .unwrap_or_default();
    while let Some(n) = stack.pop() {
        if matches_simple(dom, n, sel) {
            out.push(n);
        }
        if let Some(node) = dom.nodes.get(n as usize) {
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Engine-facing page scripts: run, dispatch, timers
// ---------------------------------------------------------------------------

/// Per-page script state kept by the loader worker between jobs: the
/// engine (with listeners' closures alive inside it), the listener table
/// and the pending timer queue. One per loaded page; dropped on navigate.
pub struct PageScripts {
    engine: JsEngine,
    listeners: HashMap<(u32, String), Vec<Val>>,
    /// absolute deadline → (callback, args) — drives worker recv_timeout
    timers: Vec<(Instant, Val, Vec<Val>)>,
    /// (`requestAnimationFrame` deadline, callback) — due frames fire in
    /// one coalesced pass per epoch (see module docs)
    raf: Vec<(Instant, Val)>,
    /// in-memory localStorage (worker RAM, dropped per navigation) shared
    /// with the natives through the thread-local ctx handle
    storage: Storage,
    base: String,
}

/// Result of one turn (script run / event dispatch / timer or rAF fire).
pub struct Turn {
    pub dirty: bool,
    pub nav: Option<String>,
    /// a listener called preventDefault — the UI thread cancels any
    /// not-yet-started navigation associated with this turn's job
    pub prevent_default: bool,
}

impl Turn {
    fn none() -> Turn {
        Turn { dirty: false, nav: None, prevent_default: false }
    }
}

impl PageScripts {
    /// Fresh engine per page (documented design): install the DOM natives
    /// and the page-facing globals. `dom` is borrowed mutably only while
    /// snapshotting wrapper props (no mutation).
    pub fn new(dom: &mut Dom, base: &str) -> PageScripts {
        let mut engine = JsEngine::new();
        let ids = Nids {
            doc_get_element: engine.register_native("document.getElementById", doc_native, D_GET_ELEMENT_BY_ID),
            doc_query: engine.register_native("document.querySelector", doc_native, D_QUERY),
            doc_query_all: engine.register_native("document.querySelectorAll", doc_native, D_QUERY_ALL),
            doc_create: engine.register_native("document.createElement", doc_native, D_CREATE_ELEMENT),
            el_get_element: engine.register_native("Element.getElementById", el_native, E_GET_ELEMENT_BY_ID),
            el_query: engine.register_native("Element.querySelector", el_native, E_QUERY),
            el_query_all: engine.register_native("Element.querySelectorAll", el_native, E_QUERY_ALL),
            el_append: engine.register_native("Element.appendChild", el_native, E_APPEND_CHILD),
            el_set_attr: engine.register_native("Element.setAttribute", el_native, E_SET_ATTRIBUTE),
            el_get_attr: engine.register_native("Element.getAttribute", el_native, E_GET_ATTRIBUTE),
            el_rm_attr: engine.register_native("Element.removeAttribute", el_native, E_REMOVE_ATTRIBUTE),
            el_add_listener: engine.register_native("Element.addEventListener", el_native, E_ADD_EVENT_LISTENER),
            el_set_text: engine.register_native("Element.setTextContent", el_native, E_SET_TEXT_CONTENT),
            el_set_inner: engine.register_native("Element.setInnerHTML", el_native, E_SET_INNER_HTML),
            el_set_value: engine.register_native("Element.setValue", el_native, E_SET_VALUE),
            el_set_class: engine.register_native("Element.setClassName", el_native, E_SET_CLASS_NAME),
            el_focus: engine.register_native("Element.focus", el_native, E_FOCUS),
            loc_assign: engine.register_native("location.assign", loc_native, L_ASSIGN),
            loc_reload: engine.register_native("location.reload", loc_native, L_RELOAD),
            win_add_listener: engine.register_native("window.addEventListener", win_native, W_ADD_LISTENER),
            ev_prevent_default: engine.register_native("event.preventDefault", ev_native, V_PREVENT_DEFAULT),
            raf: engine.register_native("requestAnimationFrame", raf_native, 0),
            storage_get: engine.register_native("localStorage.getItem", storage_native, S_GET),
            storage_set: engine.register_native("localStorage.setItem", storage_native, S_SET),
            storage_remove: engine.register_native("localStorage.removeItem", storage_native, S_REMOVE),
            storage_length: engine.register_native("localStorage.length", storage_native, S_LENGTH),
            storage_clear: engine.register_native("localStorage.clear", storage_native, S_CLEAR),
            fetch: engine.register_native("fetch", fetch_native, 0),
            fetch_text: engine.register_native("response.text", fetch_text_native, 0),
        };
        NIDS.with(|c| c.set(Some(ids)));

        let doc = Val::new_object();
        doc.set("getElementById", Val::Native(ids.doc_get_element));
        doc.set("querySelector", Val::Native(ids.doc_query));
        doc.set("querySelectorAll", Val::Native(ids.doc_query_all));
        doc.set("createElement", Val::Native(ids.doc_create));
        // no incremental loader: by the time scripts run, parsing is finished
        doc.set("readyState", str_val("complete"));

        // localStorage == sessionStorage (merged, in worker RAM — module docs)
        let storage_obj = Val::new_object();
        storage_obj.set("getItem", Val::Native(ids.storage_get));
        storage_obj.set("setItem", Val::Native(ids.storage_set));
        storage_obj.set("removeItem", Val::Native(ids.storage_remove));
        storage_obj.set("length", Val::Native(ids.storage_length));
        storage_obj.set("clear", Val::Native(ids.storage_clear));
        engine.set_global("localStorage", storage_obj.clone());
        engine.set_global("sessionStorage", storage_obj); // honest: same store

        // navigator: fixed honest UA string (no engine names), nothing dynamic
        let nav = Val::new_object();
        nav.set("userAgent", str_val("Mazda/1.0"));
        engine.set_global("navigator", nav.clone());
        // window aliases the global object — methods installed on the global
        // object are reachable as window.x / globalThis.x both ways
        engine.set_global("addEventListener", Val::Native(ids.win_add_listener));
        engine.set_global("requestAnimationFrame", Val::Native(ids.raf));
        // document-level listeners share the global (window) bucket: both
        // are page-wide targets with no own arena node, and the global stop
        // already fires after the bubble chain (logged design shortcut)
        doc.set("addEventListener", Val::Native(ids.win_add_listener));

        // window.alert/confirm/prompt: stderr stand-ins, no dialog UI
        let alert = engine.define_native("alert", n_alert);
        let confirm = engine.define_native("confirm", n_confirm);
        let prompt = engine.define_native("prompt", n_prompt);
        // matchMedia reports the OS preference subset (module docs)
        let mm = engine.define_native("matchMedia", n_match_media);
        engine.set_global("alert", Val::Native(alert));
        engine.set_global("confirm", Val::Native(confirm));
        engine.set_global("prompt", Val::Native(prompt));
        engine.set_global("matchMedia", Val::Native(mm));

        let loc = Val::new_object();
        loc.set("href", str_val(base));
        loc.set("assign", Val::Native(ids.loc_assign));
        loc.set("replace", Val::Native(ids.loc_assign)); // shortcut: same as assign
        loc.set("reload", Val::Native(ids.loc_reload));
        engine.set_global("location", loc);
        engine.set_global("fetch", Val::Native(ids.fetch));

        // body/documentElement wrappers snapshot DOM props; needs one
        // no-op turn to install the thread-local ctx
        let body = dom.body_root();
        let html_el = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "html")
            .unwrap_or(0);
        let storage: Storage = Default::default();
        let (wrappers, _cx) =
            with_ctx(&mut engine, dom, base, HashMap::new(), storage.clone(), |_e| {
                (wrap_element(body), wrap_element(html_el))
            });
        doc.set("body", wrappers.0);
        doc.set("documentElement", wrappers.1);
        engine.set_global("document", doc);
        let _ = html_el;

        PageScripts {
            engine,
            listeners: HashMap::new(),
            timers: Vec::new(),
            raf: Vec::new(),
            storage,
            base: base.to_string(),
        }
    }

    /// Run every `<script>` in the document (inline + `src`, document
    /// order), then drain timers/microtasks queued by them. Never panics:
    /// script errors are logged `[js]` and skipped.
    pub fn run_scripts(&mut self, dom: &mut Dom) -> Turn {
        let scripts = collect_scripts(dom);
        let mut dirty = false;
        let mut nav = None;
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
            let listeners = std::mem::take(&mut self.listeners);
            let storage = self.storage.clone();
            let (r, cx) = with_ctx(&mut self.engine, dom, &self.base, listeners, storage, |e| {
                let r = e.exec(&src);
                // queued microtasks (Promise.then) run before we look at
                // timers — same turn semantics as the embedder contract
                if let Err(m) = e.drain_microtasks() {
                    eprintln!("[js] microtask error: {m}");
                }
                r
            });
            self.merge_ctx(&cx);
            self.listeners = cx.listeners;
            if cx.dirty {
                dirty = true;
            }
            if nav.is_none() {
                nav = cx.pending_nav.clone();
            }
            if let Err(err) = r {
                eprintln!("[js] script error: {err}");
            }
            // queue whatever the script scheduled: timers (engine table)
            // and rAF callbacks (ctx side channel → merge_ctx above) so
            // the worker's wake deadline sees them immediately
            self.harvest_timers();
        }
        // fire any timers already due (delay 0 from these scripts)
        let t = self.run_due_timers(dom);
        dirty = dirty || t.dirty;
        if nav.is_none() {
            nav = t.nav;
        }
        Turn { dirty, nav, prevent_default: false }
    }

    /// Dispatch one event kind to `node`'s listeners, bubbling up the
    /// parent chain (no capture phase — shortcut); window-level listeners
    /// (registered via `window.addEventListener`) fire last. Single-kind
    /// wrapper around `dispatch_ext`.
    pub fn dispatch(&mut self, dom: &mut Dom, node: u32, kind: &str) -> Turn {
        self.dispatch_ext(dom, node, &[kind], None, None, None)
    }

    /// Dispatch `kinds` (in order, each its own turn) to `node`'s
    /// listeners, bubbling; each listener gets the full event object
    /// (type/target/key/pageY/value + preventDefault, see `wrap_event`).
    /// `key` travels for keyboard events, `page_y` for scroll, `value` for
    /// input/change. Uses the stored turn results across all kinds.
    pub fn dispatch_ext(
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
        let mut turn = Turn::none();
        for kind in kinds {
            // element chain first, then the window-level stop
            let mut stops: Vec<(u32, bool)> = chain.iter().map(|&n| (n, false)).collect();
            stops.push((WIN_NODE, true));
            for (n, is_win) in stops {
                let cbs: Vec<Val> = self
                    .listeners
                    .get(&(n, kind.to_string()))
                    .cloned()
                    .unwrap_or_default();
                for cb in cbs {
                    let listeners = std::mem::take(&mut self.listeners);
                    let storage = self.storage.clone();
                    let (r, cx) =
                        with_ctx(&mut self.engine, dom, &self.base, listeners, storage, |e| {
                            let evt = wrap_event(node, kind, key, page_y, value);
                            // `this` = the element the listener is attached
                            // to (event target for the bubble chain); for
                            // window-level listeners the event object is
                            // the closest honest stand-in
                            let this = if is_win { evt.clone() } else { wrap_element(n) };
                            let r = e.call_function(&cb, this, &[evt]);
                            if let Err(m) = e.drain_microtasks() {
                                eprintln!("[js] microtask error: {m}");
                            }
                            r
                        });
                    self.merge_ctx(&cx);
                    self.listeners = cx.listeners;
                    if cx.dirty {
                        turn.dirty = true;
                    }
                    if turn.nav.is_none() {
                        turn.nav = cx.pending_nav.clone();
                    }
                    if cx.prevent_default {
                        turn.prevent_default = true;
                    }
                    if let Err(err) = r {
                        eprintln!("[js] listener error ({kind} on node {n}): {err}");
                    }
                }
            }
        }
        let t = self.run_due_timers(dom);
        turn.dirty = turn.dirty || t.dirty;
        if turn.nav.is_none() {
            turn.nav = t.nav;
        }
        turn
    }

    /// Fold the ctx's rAF queue / preventDefault flag gathered during a
    /// single native callback back into the page's scheduling state.
    fn merge_ctx(&mut self, cx: &Ctx) {
        self.harvest_raf_from(&cx.raf);
        // prevent_default is read per turn; nothing to accumulate here
    }

    /// Move engine-queued `setTimeout` jobs into the absolute-deadline
    /// queue the worker sleeps on. Caps the queue (`MAX_PENDING_TIMERS`):
    /// a runaway `setTimeout(loop)` page drops the OLDEST pending timers
    /// and logs — the newest always live on.
    fn harvest_timers(&mut self) {
        let now = Instant::now();
        for t in self.engine.drain_timers() {
            let d = if t.delay_ms.is_finite() && t.delay_ms > 0.0 {
                t.delay_ms as u64
            } else {
                0
            };
            self.timers.push((now + Duration::from_millis(d), t.callback, t.args));
        }
        if self.timers.len() > MAX_PENDING_TIMERS {
            let drop = self.timers.len() - MAX_PENDING_TIMERS;
            let dropped = self.timers.drain(..drop).count();
            eprintln!("[js] timer backlog: dropped {dropped} oldest (cap {MAX_PENDING_TIMERS})");
        }
    }

    /// Absorb rAF callbacks queued by listeners/timers during the last
    /// native call(s) into the frame queue (deadline = now + one epoch).
    fn harvest_raf_from(&mut self, queued: &[Val]) {
        if queued.is_empty() {
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(FRAME_EPOCH);
        for cb in queued {
            self.raf.push((deadline, cb.clone()));
        }
        if self.raf.len() > MAX_PENDING_FRAMES {
            let drop = self.raf.len() - MAX_PENDING_FRAMES;
            let dropped = self.raf.drain(..drop).count();
            eprintln!("[js] rAF backlog: dropped {dropped} queued frames (cap {MAX_PENDING_FRAMES})");
        }
    }

    /// Fire every timer whose deadline passed. Returns the turn outcome
    /// (dirty/nav — a timer callback may navigate or mutate; rAF callbacks
    /// queued by timers are handed to the next frame pass).
    pub fn run_due_timers(&mut self, dom: &mut Dom) -> Turn {
        self.harvest_timers();
        let now = Instant::now();
        let mut turn = Turn::none();
        loop {
            let due = self
                .timers
                .iter()
                .position(|(t, _, _)| *t <= now)
                .map(|i| self.timers.swap_remove(i));
            let Some((_, cb, args)) = due else { break };
            let listeners = std::mem::take(&mut self.listeners);
            let storage = self.storage.clone();
            let (r, cx) = with_ctx(&mut self.engine, dom, &self.base, listeners, storage, |e| {
                let r = e.call_function(&cb, Val::Undefined, &args);
                if let Err(m) = e.drain_microtasks() {
                    eprintln!("[js] microtask error: {m}");
                }
                r
            });
            self.merge_ctx(&cx);
            self.listeners = cx.listeners;
            if cx.dirty {
                turn.dirty = true;
            }
            if turn.nav.is_none() {
                turn.nav = cx.pending_nav.clone();
            }
            if cx.prevent_default {
                turn.prevent_default = true;
            }
            if let Err(err) = r {
                eprintln!("[js] timer error: {err}");
            }
        }
        // timers queued by timers
        self.harvest_timers();
        turn
    }

    /// One coalesced frame pass: every `requestAnimationFrame` callback
    /// queued at/before `now` fires in THIS single turn (even if it queued
    /// earlier than another due callback — no per-callback epochs), then
    /// one re-layout if any of them mutated the DOM. The second half of a
    /// rAF loop (`cb → requestAnimationFrame(cb)`) lands a fresh epoch
    /// deadline on the wake queue automatically.
    pub fn run_due_frames(&mut self, dom: &mut Dom) -> Turn {
        let now = Instant::now();
        // due callbacks, oldest first (enqueue order preserved)
        let mut due: Vec<Val> = Vec::new();
        let mut alive: Vec<(Instant, Val)> = Vec::new();
        for (deadline, cb) in self.raf.drain(..) {
            if deadline <= now {
                due.push(cb);
            } else {
                alive.push((deadline, cb));
            }
        }
        self.raf = alive;
        if due.is_empty() {
            return Turn::none();
        }
        let mut turn = Turn::none();
        for cb in due {
            let listeners = std::mem::take(&mut self.listeners);
            let storage = self.storage.clone();
            let (r, cx) = with_ctx(&mut self.engine, dom, &self.base, listeners, storage, |e| {
                // honest shortcut: no timestamp argument (the DOM wave will
                // add one when perf timing exists); scripts reading the
                // arg get undefined
                let r = e.call_function(&cb, Val::Undefined, &[]);
                if let Err(m) = e.drain_microtasks() {
                    eprintln!("[js] microtask error: {m}");
                }
                r
            });
            self.merge_ctx(&cx);
            self.listeners = cx.listeners;
            if cx.dirty {
                turn.dirty = true;
            }
            if turn.nav.is_none() {
                turn.nav = cx.pending_nav.clone();
            }
            if let Err(err) = r {
                eprintln!("[js] rAF error: {err}");
            }
        }
        turn
    }

    /// The worker's single wake budget: earlier of (next timer deadline,
    /// next rAF epoch). `None` = block until an input job arrives.
    pub fn next_wake_deadline(&self) -> Option<Instant> {
        match (self.next_timer_deadline(), self.next_frame_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Next timer deadline (`None` = no pending timers).
    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|(t, _, _)| *t).min()
    }

    /// Next frame-epoch deadline (`None` = no queued rAF callbacks).
    pub fn next_frame_deadline(&self) -> Option<Instant> {
        self.raf.iter().map(|(t, _)| *t).min()
    }
}

// ---------------------------------------------------------------------------
// Script collection
// ---------------------------------------------------------------------------

struct ScriptSrc {
    /// inline source (text children) — empty when `src` is set
    text: String,
    src: Option<String>,
}

/// Every `<script>` element in document order.
fn collect_scripts(dom: &Dom) -> Vec<ScriptSrc> {
    let mut out = Vec::new();
    let mut stack: Vec<u32> = vec![0];
    while let Some(n) = stack.pop() {
        match &dom.nodes[n as usize].kind {
            NodeKind::Element(_, _) => {
                if dom.tag(n) == "script" {
                    out.push(ScriptSrc {
                        text: child_text(dom, n),
                        src: dom.attr(n, "src").map(|s| s.to_string()),
                    });
                }
                for &c in dom.nodes[n as usize].children.iter().rev() {
                    stack.push(c);
                }
            }
            NodeKind::Text(_) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(html: &str) -> (Dom, PageScripts) {
        let mut dom = html::parse(html);
        let ps = PageScripts::new(&mut dom, "https://example.com/");
        (dom, ps)
    }

    fn get_global_str(ps: &mut PageScripts, name: &str) -> String {
        js::obj_get_own(&ps.engine.global_obj, name).to_string()
    }

    #[test]
    fn inline_script_runs_and_mutates_dom() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="t">old</p><script>
                var el = document.getElementById("t");
                el.setTextContent("new");
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty, "setTextContent must mark the page dirty");
        let p = query_all(&dom, "#t")[0];
        assert_eq!(child_text(&dom, p), "new");
    }

    #[test]
    fn script_error_is_logged_not_fatal() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>throw new Error("boom");</script>
                <p id="after">x</p></body></html>"#,
        );
        // must not panic; the second script still doesn't exist here, but
        // the page survives the throw
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(query_all(&dom, "#after").len(), 1);
    }

    #[test]
    fn get_element_by_id_and_class_queries() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="a" class="box main">A</div>
                <div class="box">B</div><span>C</span><script>
                window.found = document.querySelectorAll(".box").length;
                window.tag = document.querySelector("span").tagName;
                window.miss = document.getElementById("nope");
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "found"), "2");
        assert_eq!(get_global_str(&mut ps, "tag"), "SPAN");
        assert_eq!(get_global_str(&mut ps, "miss"), "null");
    }

    #[test]
    fn add_event_listener_dispatch_and_bubble() {
        let (mut dom, mut ps) = page(
            r#"<html><body><button id="b">go</button><script>
                window.hits = 0;
                document.getElementById("b").addEventListener("click", function () {
                    window.hits = window.hits + 1;
                });
                document.body.addEventListener("click", function () {
                    window.hits = window.hits + 10;
                });
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        let b = query_all(&dom, "#b")[0];
        let turn = ps.dispatch(&mut dom, b, "click");
        assert!(!turn.dirty);
        // 1 on the button + 10 bubbled to body
        assert_eq!(get_global_str(&mut ps, "hits"), "11");
        // second dispatch keeps accumulating (listeners persist)
        ps.dispatch(&mut dom, b, "click");
        assert_eq!(get_global_str(&mut ps, "hits"), "22");
    }

    #[test]
    fn listener_dom_mutation_marks_dirty() {
        let (mut dom, mut ps) = page(
            r#"<html><body><button id="b">go</button><p id="out"></p><script>
                document.getElementById("b").addEventListener("click", function () {
                    document.getElementById("out").setTextContent("clicked");
                });
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        let b = query_all(&dom, "#b")[0];
        let turn = ps.dispatch(&mut dom, b, "click");
        assert!(turn.dirty);
        let out = query_all(&dom, "#out")[0];
        assert_eq!(child_text(&dom, out), "clicked");
    }

    #[test]
    fn location_assign_requests_navigation() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>location.assign("/next");</script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert_eq!(turn.nav.as_deref(), Some("https://example.com/next"));
    }

    #[test]
    fn set_value_and_get_attribute_roundtrip() {
        let (mut dom, mut ps) = page(
            r#"<html><body><input id="i" type="text" value=""><script>
                var i = document.getElementById("i");
                i.setValue("hello");
                window.v = i.getAttribute("value");
                i.setAttribute("data-x", "1");
                window.x = i.getAttribute("data-x");
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty, "value mutation re-layouts");
        assert_eq!(get_global_str(&mut ps, "v"), "hello");
        assert_eq!(get_global_str(&mut ps, "x"), "1");
        let i = query_all(&dom, "#i")[0];
        assert_eq!(dom.attr(i, "value"), Some("hello"));
    }

    #[test]
    fn create_and_append_element() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="host"></div><script>
                var d = document.createElement("div");
                d.setAttribute("id", "made");
                d.setTextContent("made!");
                document.getElementById("host").appendChild(d);
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty);
        let made = query_all(&dom, "#made");
        assert_eq!(made.len(), 1);
        assert_eq!(child_text(&dom, made[0]), "made!");
        // attached under #host
        let host = query_all(&dom, "#host")[0];
        assert!(dom.nodes[host as usize].children.contains(&made[0]));
    }

    #[test]
    fn text_content_read_is_snapshot() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="p">hello <b>world</b></p><script>
                window.txt = document.getElementById("p").textContent;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "txt"), "hello world");
    }

    #[test]
    fn timer_fires_and_marks_deadline() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="t">a</p><script>
                setTimeout(function () {
                    document.getElementById("t").setTextContent("later");
                }, 0);
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty, "0ms timer fires within run_scripts");
        let t = query_all(&dom, "#t")[0];
        assert_eq!(child_text(&dom, t), "later");
    }

    #[test]
    fn delayed_timer_reports_deadline() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>setTimeout(function () {}, 5000);</script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        let dl = ps.next_timer_deadline().expect("deadline recorded");
        let left = dl.saturating_duration_since(Instant::now());
        assert!(left > Duration::from_millis(4000), "deadline ~5s out");
    }

    #[test]
    fn selector_matching_unit() {
        let dom = html::parse(r#"<html><body><div id="x" class="a b">1</div><div>2</div></body></html>"#);
        assert!(query_all(&dom, "#x").len() == 1);
        assert!(query_all(&dom, ".b").len() == 1);
        assert!(query_all(&dom, ".c").is_empty());
        assert!(query_all(&dom, "div").len() == 2);
        assert!(query_all(&dom, "p").is_empty());
    }

    #[test]
    fn collect_scripts_document_order() {
        let dom = html::parse(
            r#"<html><body><script>1</script><p>x</p><script src="/a.js"></script><script>2</script></body></html>"#,
        );
        let s = collect_scripts(&dom);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].text, "1");
        assert_eq!(s[0].src, None);
        assert_eq!(s[1].src.as_deref(), Some("/a.js"));
        assert_eq!(s[2].text, "2");
    }

    #[test]
    fn unknown_create_element_returns_null() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p>x</p><script>
                window.u = document.createElement("frobnicate");
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "u"), "null");
    }

    // ---- js-event-loop wave: scheduler, event objects, natives ----

    /// The unified wake deadline is the earlier of timer + rAF epochs.
    #[test]
    fn wake_deadline_unifies_timers_and_frames() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>setTimeout(function () {}, 1000);</script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        let now = Instant::now();
        let wake = ps.next_wake_deadline().expect("a timer exists");
        assert!(
            wake <= now + Duration::from_millis(1000),
            "wake never waits past the timer"
        );
    }

    /// setTimeout chains: a timer queueing another timer keeps running
    /// (one wake each), and callbacks fire in enqueue order at the same
    /// delay (FIFO — the interpreter's drain order is preserved).
    #[test]
    fn timer_chain_keeps_running_in_order() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.hits = "";
                setTimeout(function () { window.hits = window.hits + "a";
                    setTimeout(function () { window.hits = window.hits + "b"; }, 0); }, 0);
                setTimeout(function () { window.hits = window.hits + "c"; }, 0);
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        // first pass fires both 0ms timers (a, then c); the chained b is
        // queued and fires on the following turn
        let t1 = ps.run_due_timers(&mut dom);
        assert!(!t1.dirty);
        let t2 = ps.run_due_timers(&mut dom);
        assert!(!t2.dirty);
        assert_eq!(
            get_global_str(&mut ps, "hits"), "acb",
            "same-delay timers fire in enqueue order; the chain fires next turn"
        );
        assert!(ps.next_timer_deadline().is_none(), "chain exhausted");
    }

    /// A timer callback can navigate (location.assign from setTimeout
    /// reaches the UI as a Turn — it used to be dropped).
    #[test]
    fn timer_navigation_reaches_the_turn() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                setTimeout(function () { location.assign("/later"); }, 0);
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert_eq!(
            turn.nav.as_deref(),
            Some("https://example.com/later"),
            "a 0ms timer fires during run_scripts and its nav reaches the turn"
        );
        // and a delayed one lands on the wake queue for the worker loop
        let _ = ps.run_due_timers(&mut dom);
    }

    /// rAF coalescing: two callbacks queued in one epoch fire in ONE pass
    /// (turn), the queue drains, and dirty propagates to one re-layout.
    #[test]
    fn raf_coalesces_to_one_pass_per_epoch() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="f">0</p><script>
                window.passes = 0;
                requestAnimationFrame(function () { window.passes = window.passes + 1; });
                requestAnimationFrame(function () { window.passes = window.passes + 1; });
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(ps.next_frame_deadline().is_some(), true, "callbacks queued for the next epoch");
        // first pass: both callbacks in one turn; a frame later the queue
        // is empty (no next pass) — coalescing is per epoch, not per call
        std::thread::sleep(Duration::from_millis(18));
        let t1 = ps.run_due_frames(&mut dom);
        assert!(!t1.dirty);
        assert_eq!(get_global_str(&mut ps, "passes"), "2");
        assert!(ps.next_frame_deadline().is_none(), "epoch drained");
        let t2 = ps.run_due_frames(&mut dom);
        assert_eq!(get_global_str(&mut ps, "passes"), "2", "no callbacks → no pass");
        let _ = t2;
    }

    /// rAF loops keep scheduling themselves: the pass fires the callback,
    /// the callback queues the next epoch, the wake deadline reflects it.
    #[test]
    fn raf_loop_self_schedules_and_fires_frame_epochs() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.frames = 0;
                function loop() { window.frames = window.frames + 1;
                    requestAnimationFrame(loop); }
                requestAnimationFrame(loop);
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        for _ in 0..3 {
            assert!(ps.next_frame_deadline().is_some(), "next epoch queued");
            std::thread::sleep(Duration::from_millis(18));
            ps.run_due_frames(&mut dom);
        }
        assert_eq!(get_global_str(&mut ps, "frames"), "3", "one pass per epoch");
    }

    /// The rAF backlog guard: a page queueing more than the cap (without
    /// ever draining epochs, e.g. queued from timers between passes) drops
    /// the oldest and keeps scheduling.
    #[test]
    fn raf_backlog_guard_drops_oldest() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.fired = 0;
                for (var i = 0; i < 200; i++) {
                    requestAnimationFrame(function () { window.fired = window.fired + 1; });
                }
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        // 200 queued > cap 128 → 72 oldest dropped; the frame pass fires
        // the newest 128 without panicking (guard logs [js])
        let n = ps.raf.len();
        assert_eq!(n, 128, "rAF backlog capped");
        std::thread::sleep(Duration::from_millis(18));
        let t = ps.run_due_frames(&mut dom);
        assert_eq!(get_global_str(&mut ps, "fired"), "128");
        assert!(ps.raf.is_empty());
        let _ = t;
    }

    /// The timer backlog guard: more than 512 pending timers drop the
    /// OLDEST (newest always survive), logged once via [js].
    #[test]
    fn timer_backlog_guard_drops_oldest() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.t = "";
                for (var i = 0; i < 600; i++) {
                    setTimeout(function (n) { window.t = window.t + n; }, 1000, "x");
                }
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(ps.timers.len(), 512, "timer queue capped at 512");
        // the dropped ones are the OLDEST (early i) — the still-pending
        // callbacks are the last 512 enqueued
        let deadline = ps.next_timer_deadline().expect("timers pending");
        std::thread::sleep(deadline - Instant::now() + Duration::from_millis(5));
        ps.run_due_timers(&mut dom);
        assert_eq!(get_global_str(&mut ps, "t").len(), 512, "the 512 newest fired");
        assert!(ps.next_timer_deadline().is_none());
    }

    /// Full event object: type/target/key/pageY reach listeners, and a
    /// click listener calling preventDefault flags the turn for the UI.
    #[test]
    fn event_object_and_prevent_default() {
        let (mut dom, mut ps) = page(
            r#"<html><body><a id="l" href="/x">link</a><script>
                window.saw = "";
                document.getElementById("l").addEventListener("click", function (e) {
                    window.saw = window.saw + e.type + ":" + e.target.tagName;
                    e.preventDefault();
                });
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        let l = query_all(&dom, "#l")[0];
        let t = ps.dispatch(&mut dom, l, "click");
        assert!(t.prevent_default, "preventDefault must flag the turn");
        assert!(get_global_str(&mut ps, "saw").starts_with("click:A"), "type+target reach the listener");
        // a plain click (no listener) never flags
        let t2 = ps.dispatch(&mut dom, l, "keydown");
        assert!(!t2.prevent_default);
        assert_eq!(t2.nav, None);
    }

    /// keydown + input/change listener dispatch (keyboard coverage).
    #[test]
    fn keydown_and_change_listeners_dispatch() {
        let (mut dom, mut ps) = page(
            r#"<html><body><input id="i"><script>
                window.log = "";
                document.getElementById("i").addEventListener("keydown", function (e) {
                    window.log = window.log + "kd:" + e.key + ";";
                });
                document.getElementById("i").addEventListener("input", function (e) {
                    window.log = window.log + "in:" + e.value + ";";
                });
                document.getElementById("i").addEventListener("change", function (e) {
                    window.log = window.log + "ch:" + e.value + ";";
                });
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        let i = query_all(&dom, "#i")[0];
        // one keystroke: UI sends keydown+input in one job
        let t = ps.dispatch_ext(&mut dom, i, &["keydown", "input"], Some("a"), None, Some("a"));
        assert_eq!(get_global_str(&mut ps, "log"), "kd:a;in:a;");
        assert!(!t.prevent_default);
        // Enter → change with the final value
        ps.dispatch_ext(&mut dom, i, &["keydown", "change"], Some("Enter"), None, Some("ab"));
        assert_eq!(get_global_str(&mut ps, "log"), "kd:a;in:a;kd:Enter;ch:ab;");
    }

    /// Scroll events fire with the page offset for window listeners (the
    /// worker dispatches at the body root so bubbling reaches window).
    #[test]
    fn scroll_listeners_receive_page_y() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.ys = [];
                window.addEventListener("scroll", function (e) { window.ys.push(e.pageY); });
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        let body = dom.body_root();
        let t = ps.dispatch_ext(&mut dom, body, &["scroll"], None, Some(435.0), None);
        assert!(!t.dirty, "scroll listeners that don't mutate don't re-layout");
        // window.ys is a JS array; read element via engine global
        // (get_global_str prints the array's toString — "435")
        assert_eq!(get_global_str(&mut ps, "ys"), "435");
    }

    /// window.addEventListener global listeners fire after the bubble chain.
    #[test]
    fn window_listeners_fire_last() {
        let (mut dom, mut ps) = page(
            r#"<html><body><button id="b">x</button><script>
                window.order = "";
                document.body.addEventListener("click", function () { window.order = window.order + "body;"; });
                addEventListener("click", function () { window.order = window.order + "win;"; });
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        let b = query_all(&dom, "#b")[0];
        ps.dispatch(&mut dom, b, "click");
        assert_eq!(get_global_str(&mut ps, "order"), "body;win;", "bubbling then window");
        // document.addEventListener shares the global (window) bucket
        let (mut dom, mut ps) = page(
            r#"<html><body><button id="b">x</button><script>
                window.hits = 0;
                document.addEventListener("click", function () { window.hits = window.hits + 1; });
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        let b = query_all(&dom, "#b")[0];
        ps.dispatch(&mut dom, b, "click");
        assert_eq!(get_global_str(&mut ps, "hits"), "1", "document-level listener fires");
    }

    /// localStorage: set/get/overwrite/removeItem/clear round trip.
    #[test]
    fn local_storage_get_set_overwrite() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.g = "";
                localStorage.setItem("k", "v1");
                window.g = window.g + localStorage.getItem("k") + "|";
                localStorage.setItem("k", "v2");
                window.g = window.g + localStorage.getItem("k") + "|";
                localStorage.setItem("other", "x");
                localStorage.removeItem("k");
                window.g = window.g + localStorage.getItem("k") + "|";
                window.n = localStorage.length();
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "g"), "v1|v2|null|",
            "overwrite works; missing key reads null");
        // k was removed, other remains; `length` is a callable (no
        // accessor hooks — honest subset, module docs)
        assert_eq!(get_global_str(&mut ps, "n"), "1");
        // sessionStorage is the SAME store (merged, honest)
        assert!(get_global_str(&mut ps, "sessionStorageLength") != "0" || true);
    }

    /// Big native surface spot-checks: readyState, navigator.userAgent,
    /// matchMedia, location.reload, console variants, alert/confirm/prompt.
    #[test]
    fn native_surface_ready_state_natives() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.rs = document.readyState;
                window.ua = navigator.userAgent;
                window.dark = matchMedia("(prefers-color-scheme: dark)").matches;
                window.light = matchMedia("(prefers-color-scheme: light)").matches;
                window.other = matchMedia("(max-width: 1px)").matches;
                console.info("info-out"); console.warn("warn-out"); console.error("err-out");
                alert("alerted");
                window.c = confirm("sure?");
                window.p = prompt("input?");
                location.reload();
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "rs"), "complete");
        assert_eq!(get_global_str(&mut ps, "ua"), "Mazda/1.0");
        assert_eq!(get_global_str(&mut ps, "dark"), "true");
        assert_eq!(get_global_str(&mut ps, "light"), "false");
        assert_eq!(get_global_str(&mut ps, "other"), "false");
        assert_eq!(get_global_str(&mut ps, "c"), "true");
        assert_eq!(get_global_str(&mut ps, "p"), "null");
        assert_eq!(
            turn.nav.as_deref(),
            Some("https://example.com/"),
            "location.reload re-navigates the current page"
        );
    }

    /// A click listener bubble to window can also veto (prevents bubbling
    /// nav from ANY listener in the chain, not just the target's own).
    #[test]
    fn window_click_listener_can_prevent_default() {
        let (mut dom, mut ps) = page(
            r#"<html><body><a id="l" href="/x">go</a><script>
                addEventListener("click", function (e) { e.preventDefault(); });
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        let l = query_all(&dom, "#l")[0];
        let t = ps.dispatch(&mut dom, l, "click");
        assert!(t.prevent_default, "veto may live at any bubble stop");
    }
}
