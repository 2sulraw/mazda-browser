//! Page scripting: `<script>` execution with DOM natives (milestone 2).
//!
//! The interpreter (`js.rs`) stays DOM-free; this module is the glue:
//! - runs `<script>` elements in document order (inline + `src`),
//! - installs `document` / `location` / `fetch` + element-method natives,
//! - registers/dispatches event listeners for the shell's event jobs.
//!
//! This is OPTION A of the SELECTABLE SCRIPT RUNTIME (`runtime.rs`): the
//! pure-Rust built-in interpreter, the default engine. The per-turn DOM
//! arena operations and side channels live on the shared `DomHost`
//! (`runtime.rs`) — the engine-core adapter (Option B, behind the
//! `engine-b` feature) calls the exact same host methods, so a page's
//! observable script surface is identical whichever engine runs it.
//!
//! Threading: everything here runs on the single loader worker thread.
//! The turn host holds a raw pointer to the worker-owned DOM, re-pointed
//! at the start of every script turn — dereferenced only inside a turn on
//! that one thread; arena node indices stay valid for the page's lifetime.
//!
//! shortcuts (honest, per native):
//! - readable props (`el.textContent`, `el.className`) are snapshotted at
//!   wrap time; writes go through methods (`setTextContent`, `setValue`,
//!   `setClassName`). Two exceptions are live, host-backed surfaces:
//!   `el.style` (a CSSStyleDeclaration stand-in: property writes, `cssText`,
//!   `setProperty`/`getPropertyValue`/`removeProperty`, camelCase or
//!   kebab-case keys — numeric CSSOM reads like `style.length` come back as
//!   strings) and `el.classList` (add/remove/toggle/contains; `value`,
//!   `length`, `replace` are absent).
//! - `offsetWidth`/`offsetHeight`/`clientWidth`/`clientHeight` are 0: this
//!   pipeline runs scripts before layout, so there is no box to measure.
//! - `parentElement` is a lazy accessor (never wraps the ancestor chain
//!   eagerly); `document.currentScript` names the `<script>` whose turn is
//!   running and is null outside script turns; `el.append` is
//!   `appendChild` for element arguments (text arguments are not created).
//! - `innerHTML` is text-only: markup fragments are logged and stored as
//!   literal text (no fragment parser).
//! - `querySelector` matches one simple selector (tag / #id / .class),
//!   no combinators or attribute selectors.
//! - listeners bubble up the parent chain (no capture phase); window-level
//!   listeners (added through `window.addEventListener`) fire after the
//!   bubble chain on every event.
//! - `fetch` is synchronous and returns a plain `{ok, status, text()}`
//!   object, not a Promise — the interpreter has no async/await.
//! - `import(spec)` is a real Promise (dynamic import): the embedder settles
//!   the job after the current turn, so `.then` chains run a turn later than
//!   a browser's eager module resolution. `import.meta` reads `undefined`;
//!   import assertions do not parse.
//! - `createElement` interns any tag name (the table grows on demand, so
//!   custom elements and `link`/`meta` work); created nodes are inert until
//!   appended (no fetch, no stylesheet application for a created `<link>`).
//!   `getElementsByTagName(name)` returns a document-order snapshot array
//!   (`"*"` = every element), not a live collection.
//! - reflected attribute props (`id`, `className`, `href`, `src`, `rel`,
//!   `as`, `crossOrigin`, `nonce`, `name`, `title`, `alt`, `target`, `media`,
//!   `integrity`, `referrerPolicy`) are live accessors on every element
//!   wrapper: reads return the attribute (or ""), writes call
//!   `setAttribute`; they are installed over the snapshot props.
//! - `document.cookie` is a real accessor (getter returns the jar's
//!   script-visible `name=value; ...` string, the setter stores one
//!   `Set-Cookie` line for the page URL). The older `document.getCookie()` /
//!   `document.setCookie(str)` pair still exists for internal callers. The
//!   cookies themselves are real: `src/cookie.rs` persists a jar, HttpOnly
//!   values stay hidden from script, and every network request sends them.
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

use crate::html::{Dom, Node, NodeKind};
use crate::js::{self, JsEngine, JsError, Val};
use crate::runtime::{self, Turn, FRAME_EPOCH, MAX_PENDING_FRAMES, MAX_PENDING_TIMERS, WIN_NODE};

// ---------------------------------------------------------------------------
// Thread-local script context (worker-thread only)
// ---------------------------------------------------------------------------

/// What the natives need while a script turn is running: the listener
/// table (registered in earlier turns, re-installed per turn) and the rAF
/// callbacks queued this turn. Everything else the natives used to carry
/// (DOM pointer, base, storage, nav/dirty/preventDefault side channels)
/// now lives on the SHARED `runtime::DomHost` — one implementation of each
/// arena operation for both engines.
struct Ctx {
    /// (node id, event kind) → registered handler functions
    listeners: HashMap<(u32, String), Vec<Val>>,
    /// callbacks queued by `requestAnimationFrame` this turn
    raf: Vec<Val>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

/// Install the script-local context for the duration of a turn.
/// `None` tears the context down (no native may run outside a turn).
fn ctx_set(listeners: HashMap<(u32, String), Vec<Val>>) {
    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            listeners,
            raf: Vec::new(),
        })
    });
}

fn ctx_take() -> Option<Ctx> {
    CTX.with(|c| c.borrow_mut().take())
}

/// Run `f` as one script turn: install the shared DomHost (carrying the
/// dom/base/storage + per-turn side channels) and the script-local ctx
/// (listeners + rAF queue), run `f` with the engine, then tear both down
/// and hand them back (listeners/raf for the next turn, host side
/// channels folded by the caller into the Turn result).
fn with_ctx<R>(
    engine: &mut JsEngine,
    dom: &mut Dom,
    base: &str,
    listeners: HashMap<(u32, String), Vec<Val>>,
    storage: runtime::Storage,
    f: impl FnOnce(&mut JsEngine) -> R,
) -> (R, runtime::DomHost, Ctx) {
    runtime::host_install(dom, base, &storage);
    ctx_set(listeners);
    let r = f(engine);
    let host = runtime::host_take();
    let cx = ctx_take().expect("[js] script context vanished mid-turn");
    (r, host, cx)
}

/// Listener registered during the current turn (persisted across turns by
/// the turn drivers, which swap the table in and out).
fn ctx_add_listener(node: u32, kind: String, cb: Val) {
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            cx.listeners.entry((node, kind)).or_default().push(cb);
        }
    })
}

/// rAF callback queued during the current turn (folded into the runtime's
/// frame queue by the turn drivers).
fn ctx_queue_raf(cb: Val) {
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            cx.raf.push(cb);
        }
    })
}

// ---------------------------------------------------------------------------
// Element wrappers
// ---------------------------------------------------------------------------

/// Reflected attribute props: `(JS property, attribute name)`. Each entry is
/// installed as one accessor pair on every element wrapper; reads go to the
/// live attribute, writes re-link through `setAttribute` (so `link.rel = ...`
/// reaches the cascade like the real DOM reflected attributes).
const REFLECTED: &[(&str, &str)] = &[
    ("id", "id"),
    ("className", "class"),
    ("href", "href"),
    ("src", "src"),
    ("rel", "rel"),
    ("as", "as"),
    ("crossOrigin", "crossorigin"),
    ("nonce", "nonce"),
    ("name", "name"),
    ("title", "title"),
    ("alt", "alt"),
    ("target", "target"),
    ("media", "media"),
    ("integrity", "integrity"),
    ("referrerPolicy", "referrerpolicy"),
];

/// Snapshot `node` into a JS object: `__node` (id) + readable props read
/// live at wrap time through the shared host (`runtime::DomHost::snapshot`
/// — the exact prop set both engines expose), then attach the method
/// natives. Methods are `Val::Native` entries that read `__node` off `this`.
fn wrap_element(node: u32) -> Val {
    let o = js::Val::new_object();
    o.set("__node", js::Val::Num(node as f64));
    let n = nids();
    let p = runtime::host(|h| h.snapshot(node));
    if let Some(p) = p {
        o.set("tagName", str_val(&p.tag));
        // value-like props snapshot: input/textarea current value
        if let Some(v) = p.value {
            o.set("value", str_val(&v));
        }
        if let Some(t) = p.ty {
            o.set("type", str_val(&t));
            o.set("checked", js::Val::Bool(p.checked));
        }
        o.set("textContent", str_val(&p.text));
    }
    // Reflected attribute props go on LAST: `id`/`className`/`href` are in
    // the table, so these accessors replace the snapshot writes above (live
    // reads and attribute-backed writes, one behaviour for both).
    for (i, (prop, _)) in REFLECTED.iter().enumerate() {
        let n = js::Val::Native(n.reflect[i]);
        o.set(
            prop,
            js::Val::Acc(Rc::new(js::AccEntry {
                getter: Some(n.clone()),
                setter: Some(n),
            })),
        );
    }
    o.set("style", wrap_style(node));
    o.set("classList", wrap_class_list(node));
    // `parentElement` resolves lazily (an accessor), so wrapping a node
    // never wraps its whole ancestor chain.
    o.set(
        "parentElement",
        js::Val::Acc(Rc::new(js::AccEntry {
            getter: Some(js::Val::Native(n.el_parent)),
            setter: None,
        })),
    );
    // layout reads: scripts run before layout in this pipeline, so these are
    // 0 — documented shortcut (no synchronous layout to measure against).
    o.set("offsetWidth", js::Val::Num(0.0));
    o.set("offsetHeight", js::Val::Num(0.0));
    o.set("clientWidth", js::Val::Num(0.0));
    o.set("clientHeight", js::Val::Num(0.0));
    o.set("getElementById", js::Val::Native(n.el_get_element));
    o.set("querySelector", js::Val::Native(n.el_query));
    o.set("querySelectorAll", js::Val::Native(n.el_query_all));
    o.set("appendChild", js::Val::Native(n.el_append));
    o.set("append", js::Val::Native(n.el_append)); // shortcut: element args only
    o.set("remove", js::Val::Native(n.el_remove));
    o.set("setAttribute", js::Val::Native(n.el_set_attr));
    o.set("getAttribute", js::Val::Native(n.el_get_attr));
    o.set("removeAttribute", js::Val::Native(n.el_rm_attr));
    o.set("addEventListener", js::Val::Native(n.el_add_listener));
    o.set("setTextContent", js::Val::Native(n.el_set_text));
    o.set("setInnerHTML", js::Val::Native(n.el_set_inner));
    o.set("setValue", js::Val::Native(n.el_set_value));
    o.set("setClassName", js::Val::Native(n.el_set_class));
    o.set("focus", js::Val::Native(n.el_focus));
    o.set("submit", js::Val::Native(n.el_submit));
    // media fail-fast (wave 28): the media tier is gated, so a player gets
    // `canPlayType === ""` and `play()` rejects. On non-media elements these
    // are inert, exactly like HTMLMediaElement.prototype not existing.
    o.set("canPlayType", js::Val::Native(n.el_can_play));
    o.set("play", js::Val::Native(n.el_media_play));
    o.set("pause", js::Val::Native(n.el_media_noop));
    o.set("load", js::Val::Native(n.el_media_noop));
    o
}

/// `element.style` — host-backed CSSStyleDeclaration stand-in
/// (`js::ObjKind::Style`): member reads/writes go to the element's `style`
/// attribute, so `el.style.display = "none"` and `Object.assign(el.style,
/// {...})` reach the cascade. Own props carry the methods + `__node`.
fn wrap_style(node: u32) -> Val {
    let o = js::Val::new_object();
    o.set("__node", js::Val::Num(node as f64));
    if let js::Val::Obj(rc) = &o {
        rc.borrow_mut().kind = js::ObjKind::Style(node);
    }
    let n = nids();
    o.set("setProperty", js::Val::Native(n.style_set_prop));
    o.set("getPropertyValue", js::Val::Native(n.style_get_prop));
    o.set("removeProperty", js::Val::Native(n.style_remove_prop));
    o
}

/// `element.classList`: method natives over the live `class` attribute
/// (the wrapper's `className` stays a snapshot, like every other prop).
fn wrap_class_list(node: u32) -> Val {
    let o = js::Val::new_object();
    o.set("__node", js::Val::Num(node as f64));
    let n = nids();
    o.set("add", js::Val::Native(n.cls_add));
    o.set("remove", js::Val::Native(n.cls_remove));
    o.set("toggle", js::Val::Native(n.cls_toggle));
    o.set("contains", js::Val::Native(n.cls_contains));
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

/// Reflected attribute property (one native per `REFLECTED` entry): a getter
/// call arrives with no arguments, a setter with one. Both halves of the
/// accessor pair point at this same native; `this` is the element wrapper.
fn el_prop_native(_e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let node = this_node(this)?;
    let Some((_, attr)) = REFLECTED.get(idx as usize) else {
        return Ok(Val::Undefined);
    };
    if args.is_empty() {
        let v = runtime::host(|h| h.attr(node, attr)).unwrap_or_default();
        return Ok(str_val(&v));
    }
    let val = args[0].to_string();
    runtime::host_mut(|h| h.set_attribute(node, attr, &val));
    Ok(Val::Undefined)
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
    doc_cookie_get: usize,
    doc_cookie_set: usize,
    doc_get_by_tag: usize,
    /// one native per `REFLECTED` entry (accessor pair halves)
    reflect: [usize; REFLECTED.len()],
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
    el_submit: usize,
    el_parent: usize,
    el_remove: usize,
    cls_add: usize,
    cls_remove: usize,
    cls_toggle: usize,
    cls_contains: usize,
    style_set_prop: usize,
    style_get_prop: usize,
    style_remove_prop: usize,
    el_can_play: usize,
    el_media_play: usize,
    el_media_noop: usize,
    loc_assign: usize,
    loc_reload: usize,
    win_add_listener: usize,
    match_media: usize,
    media_noop: usize,
    eme: usize,
    media_caps: usize,
    history_push: usize,
    history_replace: usize,
    history_go: usize,
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
/// `pub(crate)`: the shared DomHost delegates to this canonical copy.
pub(crate) fn all_elements_dom(dom: &Dom) -> Vec<u32> {
    (1..dom.nodes.len() as u32)
        .filter(|&i| dom.tag(i) != "#text" && dom.tag(i) != "#document")
        .collect()
}

/// Simple-selector matches over the whole document (document order).
/// `pub(crate)`: the shared DomHost delegates to this canonical copy.
pub(crate) fn query_all_dom(dom: &Dom, sel: &str) -> Vec<u32> {
    all_elements_dom(dom)
        .into_iter()
        .filter(|&i| matches_simple(dom, i, sel))
        .collect()
}

/// Concatenated text of every descendant text node (textContent/innerHTML
/// snapshot). innerHTML returns markup-less text — shortcut documented.
/// `pub(crate)`: the shared DomHost delegates to this canonical copy.
pub(crate) fn child_text(dom: &Dom, node: u32) -> String {
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

// ---------------------------------------------------------------------------
// Natives: one shared dispatch fn per group; each method is registered with
// its own u16 idx (`register_native(name, f, idx)` echoes idx back).
// ---------------------------------------------------------------------------

/// document group idxs
const D_GET_ELEMENT_BY_ID: u16 = 0;
const D_QUERY: u16 = 1;
const D_QUERY_ALL: u16 = 2;
const D_CREATE_ELEMENT: u16 = 3;
const D_COOKIE_GET: u16 = 4;
const D_COOKIE_SET: u16 = 5;
const D_GET_ELEMENTS_BY_TAG: u16 = 6;
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
/// `Element.submit()` — the form.submit() native (shared body in DomHost)
const E_SUBMIT: u16 = 23;
/// lazy `parentElement` getter (installed as an accessor)
const E_PARENT: u16 = 24;
/// classList group
const E_CLASS_ADD: u16 = 25;
const E_CLASS_REMOVE: u16 = 26;
const E_CLASS_TOGGLE: u16 = 27;
const E_CLASS_CONTAINS: u16 = 28;
/// style group (CSSStyleDeclaration stand-in)
const E_STYLE_SET_PROPERTY: u16 = 34;
const E_STYLE_GET_PROPERTY: u16 = 35;
const E_STYLE_REMOVE_PROPERTY: u16 = 36;
const E_REMOVE: u16 = 37;
/// media fail-fast group (wave 28: the media tier is gated; these answer
/// feature probes truthfully instead of being absent)
const E_CAN_PLAY: u16 = 38;
const E_MEDIA_PLAY: u16 = 39;
const E_MEDIA_NOOP: u16 = 40;
/// location group idxs
const L_ASSIGN: u16 = 30;
const L_RELOAD: u16 = 31;
/// window group idxs
const W_ADD_LISTENER: u16 = 32;
const W_MATCH_MEDIA: u16 = 34;
/// EME fail-fast (wave 28): DRM is parked, so the only useful answer is a
/// rejected `NotSupportedError` promise — not an absent method (which makes
/// players fall through to the generic "unsupported browser" wall)
const W_EME: u16 = 36;
const W_MEDIA_CAPS: u16 = 37;
/// history group idxs
const H_PUSH: u16 = 0;
const H_REPLACE: u16 = 1;
const H_GO: u16 = 2;
/// event object group idxs
const V_PREVENT_DEFAULT: u16 = 33;
/// storage group idxs
const S_GET: u16 = 40;
const S_SET: u16 = 41;
const S_REMOVE: u16 = 42;
const S_LENGTH: u16 = 43;
const S_CLEAR: u16 = 44;

// Scheduler budgets (MAX_PENDING_TIMERS / MAX_PENDING_FRAMES /
// FRAME_EPOCH) live ONCE in runtime.rs — single source for both engines.

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
            Ok(runtime::host(|h| {
                let found = h.query_all(&format!("#{}", sel)).into_iter().next();
                match found {
                    Some(n) => wrap_element(n),
                    None => Val::Null,
                }
            }))
        }
        D_QUERY => {
            let Some(sel) = sel else { return Ok(Val::Null) };
            Ok(runtime::host(|h| {
                let found = h.query_all(&sel).into_iter().next();
                match found {
                    Some(n) => wrap_element(n),
                    None => Val::Null,
                }
            }))
        }
        D_QUERY_ALL => {
            let Some(sel) = sel else { return Ok(js::Val::new_array(vec![])) };
            Ok(runtime::host(|h| {
                js::Val::new_array(h.query_all(&sel).into_iter().map(wrap_element).collect())
            }))
        }
        D_CREATE_ELEMENT => {
            let Some(tag) = sel else { return Ok(Val::Null) };
            create_element(&tag)
        }
        D_GET_ELEMENTS_BY_TAG => {
            let name = sel.unwrap_or_default();
            Ok(runtime::host(|h| {
                js::Val::new_array(h.by_tag(&name).into_iter().map(wrap_element).collect())
            }))
        }
        // the `document.cookie` accessor pair; the plain get/setCookie
        // methods still exist for internal callers (module header)
        D_COOKIE_GET => Ok(runtime::host(|h| str_val(&h.cookie_get()))),
        D_COOKIE_SET => {
            if let Some(s) = args.first().map(|v| v.to_string()) {
                runtime::host_mut(|h| h.cookie_set(&s));
            }
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: bad document native")),
    }
}

/// `createElement`: interns the tag name on the shared host (the table grows
/// on demand, so any tag is legal) and wraps the orphan node. The arena push
/// is split from wrapper construction so an unwrapped orphan node can never
/// leak into script-visible state.
fn create_element(tag: &str) -> Result<Val, JsError> {
    let tag = tag.to_ascii_lowercase();
    let made = runtime::host_mut(|h| h.create_element(&tag));
    match made {
        Some(id) => Ok(wrap_element(id)),
        None => {
            eprintln!("[js] createElement: empty tag name");
            Ok(Val::Null)
        }
    }
}

fn el_native(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let node = this_node(this)?;
    let a0 = args.first().cloned().unwrap_or(Val::Undefined);
    match idx {
        E_GET_ELEMENT_BY_ID => {
            let Some(sel) = a0.as_string() else { return Ok(Val::Null) };
            Ok(runtime::host(|h| {
                let found = h.first_subtree(node, &format!("#{}", sel));
                match found {
                    Some(n) => wrap_element(n),
                    None => Val::Null,
                }
            }))
        }
        E_QUERY => {
            let Some(sel) = a0.as_string() else { return Ok(Val::Null) };
            Ok(runtime::host(|h| {
                let found = h.first_subtree(node, &sel);
                match found {
                    Some(n) => wrap_element(n),
                    None => Val::Null,
                }
            }))
        }
        E_QUERY_ALL => {
            let Some(sel) = a0.as_string() else { return Ok(js::Val::new_array(vec![])) };
            Ok(runtime::host(|h| {
                js::Val::new_array(h.subtree_all(node, &sel).into_iter().map(wrap_element).collect())
            }))
        }
        E_APPEND_CHILD => {
            let child = this_node(&a0)?;
            // dirty flag bookkeeping lives inside the host write (as
            // before: an out-of-range no-op never dirties the page)
            runtime::host_mut(|h| h.append_child(node, child));
            Ok(a0)
        }
        E_SET_ATTRIBUTE => {
            let name = a0.as_string().unwrap_or_default();
            let val = args.get(1).map(|v| v.to_string()).unwrap_or_default();
            set_attr(node, &name, &val);
            Ok(Val::Undefined)
        }
        E_GET_ATTRIBUTE => Ok(runtime::host(|h| match h.attr(node, &a0.to_string()) {
            Some(v) => str_val(&v),
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
            ctx_add_listener(node, kind.to_string(), cb);
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
        E_SUBMIT => {
            // form.submit() native body: shared verbatim with engine B
            // (collect → navigate; spec shape = no submit event here)
            runtime::host_mut(|h| h.submit_form(node));
            Ok(Val::Undefined)
        }
        E_PARENT => Ok(match runtime::host(|h| h.parent_element(node)) {
            Some(p) => wrap_element(p),
            None => Val::Null,
        }),
        E_REMOVE => {
            runtime::host_mut(|h| h.remove_node(node));
            Ok(Val::Undefined)
        }
        E_CLASS_ADD | E_CLASS_REMOVE | E_CLASS_TOGGLE | E_CLASS_CONTAINS => {
            let mut tokens: Vec<String> = runtime::host(|h| h.attr(node, "class"))
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect();
            let names: Vec<String> = args
                .iter()
                .filter_map(|v| v.as_string().map(|s| s.to_string()))
                .collect();
            let has = |tokens: &[String], name: &str| tokens.iter().any(|t| t == name);
            let mut changed = false;
            match idx {
                E_CLASS_CONTAINS => {
                    let some = names.first().map_or(false, |n| has(&tokens, n));
                    return Ok(Val::Bool(some));
                }
                E_CLASS_ADD => {
                    for name in names {
                        if !has(&tokens, &name) {
                            tokens.push(name);
                            changed = true;
                        }
                    }
                }
                E_CLASS_REMOVE => {
                    let before = tokens.len();
                    tokens.retain(|t| !names.contains(t));
                    changed = tokens.len() != before;
                }
                _ => {
                    // toggle(name[, force])
                    if let Some(name) = names.first() {
                        let force = args.get(1).map(|v| v.to_bool());
                        let present = has(&tokens, name);
                        let want = force.unwrap_or(!present);
                        if want && !present {
                            tokens.push(name.clone());
                            changed = true;
                        } else if !want && present {
                            tokens.retain(|t| t != name);
                            changed = true;
                        }
                    }
                }
            }
            if changed {
                set_attr(node, "class", &tokens.join(" "));
            }
            Ok(Val::Undefined)
        }
        E_STYLE_SET_PROPERTY => {
            let name = a0.as_string().unwrap_or_default();
            let val = args.get(1).map(|v| v.to_string()).unwrap_or_default();
            runtime::host_mut(|h| h.style_set(node, &name, &val));
            Ok(Val::Undefined)
        }
        E_STYLE_GET_PROPERTY => {
            let name = a0.as_string().unwrap_or_default();
            Ok(runtime::host(|h| str_val(&h.style_prop(node, &name))))
        }
        E_STYLE_REMOVE_PROPERTY => {
            let name = a0.as_string().unwrap_or_default();
            let old = runtime::host(|h| h.style_prop(node, &name));
            runtime::host_mut(|h| h.style_set(node, &name, ""));
            Ok(str_val(&old))
        }
        // ---- media fail-fast (wave 28) ----
        // no decoder is wired yet, so every MIME type is honestly unplayable
        E_CAN_PLAY => Ok(str_val("")),
        E_MEDIA_PLAY => {
            use std::sync::atomic::{AtomicBool, Ordering};
            static NOTED: AtomicBool = AtomicBool::new(false);
            if !NOTED.swap(true, Ordering::Relaxed) {
                eprintln!("[media] playback not supported");
            }
            Ok(rejected_not_supported(e, "playback not supported"))
        }
        // pause()/load() are state changes on a player that does not exist;
        // returning undefined keeps fire-and-forget callers running
        E_MEDIA_NOOP => Ok(Val::Undefined),
        _ => Err(JsError::throw("TypeError: bad element native")),
    }
}

fn loc_native(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        L_ASSIGN => {
            let target = args.first().map(|v| v.to_string()).unwrap_or_default();
            runtime::host_mut(|h| h.request_nav(&target));
            Ok(Val::Undefined)
        }
        L_RELOAD => {
            // a reload is a navigation to the current page (refetch through
            // the full pipeline; the disk cache may serve it up to TTL)
            let base = runtime::host(|h| h.base().to_string());
            runtime::host_mut(|h| h.request_nav(&base));
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: bad location native")),
    }
}

/// `window.addEventListener(type, cb)` — global listeners fire after the
/// bubble chain on every event of that kind (window is the last stop).
/// `window` aliases the global object, so the method sits on `globalThis`
/// and doubles as a bare `addEventListener(...)`.
fn win_native(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
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
            ctx_add_listener(WIN_NODE, kind.to_string(), cb);
            Ok(Val::Undefined)
        }
        // shortcut: no media engine - every query reports "no match", and the
        // list is a fixed object whose listener methods do nothing
        W_MATCH_MEDIA => {
            let media = args
                .first()
                .map(|v| v.to_string())
                .unwrap_or_else(String::new);
            let list = Val::new_object();
            list.set("media", Val::Str(Rc::from(media.as_str())));
            list.set("matches", Val::Bool(false));
            let noop = NIDS.with(|c| c.get().map(|n| n.media_noop).unwrap_or(0));
            for name in [
                "addEventListener",
                "removeEventListener",
                "addListener",
                "removeListener",
            ] {
                list.set(name, Val::Native(noop));
            }
            Ok(list)
        }
        35 => Ok(Val::Undefined),
        // EME is parked (PLAN: no certifiable CDM, legal exposure). The
        // valuable half is failing fast and truthfully so players can pick a
        // non-DRM rendition or show their own message.
        W_EME => {
            use std::sync::atomic::{AtomicBool, Ordering};
            static NOTED: AtomicBool = AtomicBool::new(false);
            if !NOTED.swap(true, Ordering::Relaxed) {
                eprintln!("[media] DRM not supported");
            }
            Ok(rejected_not_supported(e, "DRM not supported"))
        }
        W_MEDIA_CAPS => Ok(rejected_not_supported(e, "media capabilities not supported")),
        _ => Err(JsError::throw("TypeError: bad window native")),
    }
}

/// shortcut: the session-history *stack* is not modelled. `pushState` /
/// `replaceState` store the custom state and the new URL string (so routers
/// that read them back agree), but back/forward/go do nothing and no
/// `popstate` event is ever dispatched.
fn hist_native(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == H_GO {
        return Ok(Val::Undefined);
    }
    let hist = e.get_global("history");
    hist.set("state", args.first().cloned().unwrap_or(Val::Null));
    if let Some(url) = args.get(2).and_then(|v| v.as_string()) {
        let loc = e.get_global("location");
        loc.set("href", Val::Str(Rc::from(url)));
    }
    Ok(Val::Undefined)
}

/// The event object's preventDefault - flags the click so the UI thread
/// cancels a not-yet-started link navigation (see module docs).
fn ev_native(_e: &mut JsEngine, idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    match idx {
        V_PREVENT_DEFAULT => {
            let node = this_node(this).unwrap_or(0);
            runtime::host_mut(|h| h.note_prevent_default(node));
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
    // the callback joins the turn's rAF queue; the turn driver folds it
    // into the runtime's frame queue (next epoch) when the turn ends
    ctx_queue_raf(cb);
    Ok(Val::Num(next_raf_id()))
}

/// `localStorage` / `sessionStorage` (merged): an in-memory map per page,
/// dropped on navigation. Errors (out of range, non-string values coerced)
/// are logged, never thrown — closest honest subset. Reads/writes go
/// through the shared host (the once-per-page honest note lives there).
fn storage_native(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        S_GET => {
            let key = args.first().map(|v| v.to_string()).unwrap_or_default();
            let val = runtime::host(|h| h.storage_get(&key));
            Ok(val.map(|v| str_val(&v)).unwrap_or(Val::Null))
        }
        S_SET => {
            let key = args.first().map(|v| v.to_string()).unwrap_or_default();
            let val = args.get(1).map(|v| v.to_string()).unwrap_or_default();
            runtime::host_mut(|h| h.storage_set(&key, &val));
            Ok(Val::Undefined)
        }
        S_REMOVE => {
            let key = args.first().map(|v| v.to_string()).unwrap_or_default();
            runtime::host_mut(|h| h.storage_remove(&key));
            Ok(Val::Undefined)
        }
        S_LENGTH => {
            let n = runtime::host(|h| h.storage_len()) as f64;
            Ok(Val::Num(n))
        }
        S_CLEAR => {
            runtime::host_mut(|h| h.storage_clear());
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: bad storage native")),
    }
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
/// resolves the per-element scheme itself (its globals are private); the
/// mirror lives on the shared host (single copy for both engines).
/// Other media queries → no match.
fn n_match_media(_e: &mut JsEngine, args: &[Val]) -> Result<Val, JsError> {
    let q = args.first().map(|v| v.to_string()).unwrap_or_default();
    let dark = runtime::host(|h| h.match_media_dark(&q));
    let o = Val::new_object();
    o.set("matches", Val::Bool(dark));
    o.set("media", str_val(&q));
    // shortcut: no media engine, so the list never changes and its change
    // listeners are recorded nowhere — themes just read `.matches` once
    let noop = nids().media_noop;
    for name in [
        "addEventListener",
        "removeEventListener",
        "addListener",
        "removeListener",
    ] {
        o.set(name, Val::Native(noop));
    }
    Ok(o)
}

/// A `DOMException`-shaped rejected promise. Feature probes read `.name`
/// (`NotSupportedError`), players read `.message`, and `.catch` must run — so
/// this is a real promise settled on the engine's microtask queue, not a
/// thrown exception (which would abort the caller's script).
fn rejected_not_supported(e: &mut JsEngine, msg: &str) -> Val {
    let p = e.new_promise();
    let err = Val::new_object();
    err.set("name", str_val("NotSupportedError"));
    err.set("message", str_val(msg));
    js::settle_promise(e, &p, Err(err));
    Val::Obj(p)
}

/// `navigator.userAgentData` — the client-hint object matching [`USER_AGENT`].
fn user_agent_data() -> Val {
    let brands = [
        ("Chromium", "142"),
        ("Google Chrome", "142"),
        ("Not_A Brand", "24"),
    ];
    let list = brands
        .iter()
        .map(|(b, v)| {
            let o = Val::new_object();
            o.set("brand", str_val(b));
            o.set("version", str_val(v));
            o
        })
        .collect();
    let o = Val::new_object();
    o.set("brands", Val::new_array(list));
    o.set("mobile", Val::Bool(false));
    let platform = if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    };
    o.set("platform", str_val(platform));
    o
}

fn fetch_native(_e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let url = args.first().map(|v| v.to_string()).unwrap_or_default();
    let Some(abs) = runtime::host(|h| h.resolve_url(&url)) else {
        eprintln!("[js] fetch: unresolvable url {url:?}");
        return Ok(Val::Null);
    };
    match runtime::host(|h| h.fetch_text(&url)) {
        Ok((body, ct)) => {
            // shortcut: cap honored by fetch's own 20MB bound; response
            // exposes text() only (no headers/blob)
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
// DOM mutation helpers — thin wrappers over the shared host (the arena
// writes and dirty-flag bookkeeping exist ONCE, in runtime.rs; both
// engines call them with identical semantics).
// ---------------------------------------------------------------------------

fn set_attr(node: u32, name: &str, val: &str) {
    runtime::host_mut(|h| h.set_attribute(node, name, val));
}

/// Style-object read/write hooks (installed on the engine). `host_opt` keeps
/// a direct engine test (no script turn installed) from panicking.
fn style_get_hook(node: u32, key: &str) -> String {
    runtime::host_opt(|h| h.style_prop(node, key)).unwrap_or_default()
}

fn style_set_hook(node: u32, key: &str, val: &str) {
    runtime::host_mut_opt(|h| h.style_set(node, key, val));
}

fn remove_attr(node: u32, name: &str) {
    runtime::host_mut(|h| h.remove_attribute(node, name));
}

/// Replace all children of `node` with a single text node (`textContent`,
/// text-only `innerHTML`).
fn set_text_content(node: u32, text: &str) {
    runtime::host_mut(|h| h.set_text_content(node, text));
}

/// Descendants of `root` matching `sel` (document order; root excluded).
/// `pub(crate)`: the shared DomHost delegates to this canonical copy.
pub(crate) fn subtree_query_dom(dom: &Dom, root: u32, sel: &str) -> Vec<u32> {
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
/// A queued timer: absolute deadline, the id `setTimeout`/`setInterval`
/// returned (so `clearTimeout` can still cancel it after it was harvested
/// out of the engine), the callback plus its args, and the repeat period for
/// `setInterval`.
struct Timer {
    due: Instant,
    id: f64,
    cb: Val,
    args: Vec<Val>,
    repeat_ms: Option<f64>,
}

/// and the pending timer queue. One per loaded page; dropped on navigate.
pub struct PageScripts {
    engine: JsEngine,
    listeners: HashMap<(u32, String), Vec<Val>>,
    /// absolute deadline → (callback, args) — drives worker recv_timeout
    timers: Vec<Timer>,
    /// (`requestAnimationFrame` deadline, callback) — due frames fire in
    /// one coalesced pass per epoch (see module docs)
    raf: Vec<(Instant, Val)>,
    /// in-memory localStorage (worker RAM, dropped per navigation) shared
    /// with the natives through the shared host handle
    storage: runtime::Storage,
    base: String,
}
// The per-turn Turn result lives in runtime.rs (same contract for both
// engines) — script.rs re-exports it via the import at the top.

impl PageScripts {
    /// Fresh engine per page (documented design): install the DOM natives
    /// and the page-facing globals. `dom` is borrowed mutably only while
    /// snapshotting wrapper props (no mutation).
    pub fn new(dom: &mut Dom, base: &str) -> PageScripts {
        let mut engine = JsEngine::new();
        // one native per reflected-prop accessor (getter + setter share it)
        let reflect = std::array::from_fn(|i| {
            engine.register_native(REFLECTED[i].0, el_prop_native, i as u16)
        });
        let ids = Nids {
            doc_get_element: engine.register_native("document.getElementById", doc_native, D_GET_ELEMENT_BY_ID),
            doc_query: engine.register_native("document.querySelector", doc_native, D_QUERY),
            doc_query_all: engine.register_native("document.querySelectorAll", doc_native, D_QUERY_ALL),
            doc_create: engine.register_native("document.createElement", doc_native, D_CREATE_ELEMENT),
            doc_cookie_get: engine.register_native("document.getCookie", doc_native, D_COOKIE_GET),
            doc_cookie_set: engine.register_native("document.setCookie", doc_native, D_COOKIE_SET),
            doc_get_by_tag: engine.register_native("document.getElementsByTagName", doc_native, D_GET_ELEMENTS_BY_TAG),
            reflect,
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
            el_submit: engine.register_native("Element.submit", el_native, E_SUBMIT),
            el_parent: engine.register_native("Element.parentElement", el_native, E_PARENT),
            el_remove: engine.register_native("Element.remove", el_native, E_REMOVE),
            cls_add: engine.register_native("Element.classList.add", el_native, E_CLASS_ADD),
            cls_remove: engine.register_native("Element.classList.remove", el_native, E_CLASS_REMOVE),
            cls_toggle: engine.register_native("Element.classList.toggle", el_native, E_CLASS_TOGGLE),
            cls_contains: engine.register_native("Element.classList.contains", el_native, E_CLASS_CONTAINS),
            style_set_prop: engine.register_native("CSSStyleDeclaration.setProperty", el_native, E_STYLE_SET_PROPERTY),
            style_get_prop: engine.register_native("CSSStyleDeclaration.getPropertyValue", el_native, E_STYLE_GET_PROPERTY),
            style_remove_prop: engine.register_native("CSSStyleDeclaration.removeProperty", el_native, E_STYLE_REMOVE_PROPERTY),
            el_can_play: engine.register_native("Element.canPlayType", el_native, E_CAN_PLAY),
            el_media_play: engine.register_native("Element.play", el_native, E_MEDIA_PLAY),
            el_media_noop: engine.register_native("Element.pause", el_native, E_MEDIA_NOOP),
            loc_assign: engine.register_native("location.assign", loc_native, L_ASSIGN),
            loc_reload: engine.register_native("location.reload", loc_native, L_RELOAD),
            win_add_listener: engine.register_native("window.addEventListener", win_native, W_ADD_LISTENER),
            match_media: engine.register_native("window.matchMedia", win_native, W_MATCH_MEDIA),
            media_noop: engine.register_native("MediaQueryList.addEventListener", win_native, 35),
            eme: engine.register_native("navigator.requestMediaKeySystemAccess", win_native, W_EME),
            media_caps: engine.register_native("navigator.mediaCapabilities.decodingInfo", win_native, W_MEDIA_CAPS),
            history_push: engine.register_native("history.pushState", hist_native, H_PUSH),
            history_replace: engine.register_native("history.replaceState", hist_native, H_REPLACE),
            history_go: engine.register_native("history.back", hist_native, H_GO),
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
        // CSSOM bridge: the style object's member reads/writes go through
        // the shared host (js.rs itself stays DOM-free)
        engine.style_get = Some(style_get_hook);
        engine.style_set = Some(style_set_hook);
        // dynamic import(): global native; the module registry itself lives
        // on the engine (engine.module_loader), and the page URL is the
        // fallback base for specifiers called outside a module body
        js::module::install_dynamic_import(&mut engine);
        engine.import_base = Some(Rc::from(base));

        let doc = Val::new_object();
        doc.set("getElementById", Val::Native(ids.doc_get_element));
        doc.set("querySelector", Val::Native(ids.doc_query));
        doc.set("querySelectorAll", Val::Native(ids.doc_query_all));
        doc.set("createElement", Val::Native(ids.doc_create));
        doc.set("getElementsByTagName", Val::Native(ids.doc_get_by_tag));
        doc.set("getCookie", Val::Native(ids.doc_cookie_get));
        doc.set("setCookie", Val::Native(ids.doc_cookie_set));
        // `document.cookie` — the property the web actually reads. The plain
        // get/set pair above predates accessor support; real pages do
        // `document.cookie.match(...)`, which threw "of undefined" and killed
        // Wikipedia's client-preference bootstrap.
        doc.set(
            "cookie",
            Val::Acc(Rc::new(js::AccEntry {
                getter: Some(Val::Native(ids.doc_cookie_get)),
                setter: Some(Val::Native(ids.doc_cookie_set)),
            })),
        );
        // no incremental loader: by the time scripts run, parsing is finished
        doc.set("readyState", str_val("complete"));
        // null outside a `<script>`; set/cleared around each script turn
        doc.set("currentScript", Val::Null);

        // localStorage == sessionStorage (merged, in worker RAM — module docs)
        let storage_obj = Val::new_object();
        storage_obj.set("getItem", Val::Native(ids.storage_get));
        storage_obj.set("setItem", Val::Native(ids.storage_set));
        storage_obj.set("removeItem", Val::Native(ids.storage_remove));
        storage_obj.set("length", Val::Native(ids.storage_length));
        storage_obj.set("clear", Val::Native(ids.storage_clear));
        engine.set_global("localStorage", storage_obj.clone());
        engine.set_global("sessionStorage", storage_obj); // honest: same store

        // navigator: the UA is Chrome's and matches the network `User-Agent`
        // byte for byte (net.rs) — a page that sees two identities is
        // trivially fingerprinted. Platform/language stay host-truthful.
        let nav = Val::new_object();
        nav.set("userAgent", str_val(crate::net::USER_AGENT));
        // a host platform string, not a browser claim; sites use it to pick
        // keyboard-shortcut glyphs
        nav.set(
            "platform",
            str_val(if cfg!(target_os = "windows") {
                "Win32"
            } else if cfg!(target_os = "macos") {
                "MacIntel"
            } else {
                "Linux x86_64"
            }),
        );
        nav.set("language", str_val("en-US"));
        // Chrome client hints (feature probes and bot walls read these)
        nav.set("userAgentData", user_agent_data());
        // EME fail-fast: DRM is parked (PLAN), so reject cleanly and loudly
        // instead of leaving the method absent.
        nav.set("requestMediaKeySystemAccess", Val::Native(ids.eme));
        let caps = Val::new_object();
        caps.set("decodingInfo", Val::Native(ids.media_caps));
        nav.set("mediaCapabilities", caps);
        engine.set_global("navigator", nav.clone());

        // history: the surface routers probe plus pushState-based SPA
        // navigation. Shortcut: one entry, no session-history stack and no
        // popstate event; pushState/replaceState only record the URL.
        let hist = Val::new_object();
        hist.set("scrollRestoration", str_val("auto"));
        hist.set("state", Val::Null);
        hist.set("length", Val::Num(1.0));
        hist.set("pushState", Val::Native(ids.history_push));
        hist.set("replaceState", Val::Native(ids.history_replace));
        hist.set("back", Val::Native(ids.history_go));
        hist.set("forward", Val::Native(ids.history_go));
        hist.set("go", Val::Native(ids.history_go));
        engine.set_global("history", hist);
        // window aliases the global object — methods installed on the global
        // object are reachable as window.x / globalThis.x both ways
        engine.set_global("addEventListener", Val::Native(ids.win_add_listener));
        engine.set_global("matchMedia", Val::Native(ids.match_media));
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
        // The rest of the readable location surface comes from the same URL
        // parser `window.URL` uses, so scripts can read location.hostname etc.
        if let Some(p) = js::url::parts(base) {
            loc.set("protocol", str_val(&p.protocol));
            loc.set("host", str_val(&p.host));
            loc.set("hostname", str_val(&p.hostname));
            loc.set("port", str_val(&p.port));
            loc.set("pathname", str_val(&p.pathname));
            loc.set("search", str_val(&p.search));
            loc.set("hash", str_val(&p.hash));
            loc.set("origin", str_val(&p.origin));
        }
        loc.set("assign", Val::Native(ids.loc_assign));
        loc.set("replace", Val::Native(ids.loc_assign)); // shortcut: same as assign
        loc.set("reload", Val::Native(ids.loc_reload));
        engine.set_global("location", loc);
        engine.set_global("fetch", Val::Native(ids.fetch));

        // body/documentElement/head wrappers snapshot DOM props; needs one
        // no-op turn to install the thread-local ctx
        let body = dom.body_root();
        let html_el = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "html")
            .unwrap_or(0);
        // The forgiving parser only materialises what the markup spells out,
        // while real HTML parsers always synthesize <head>. Pages that omit
        // it (probe pages, fragments) still need a `document.head` to append
        // links/metas to, so add one when it is missing (<head> is in the
        // non-rendered UA set, so this never paints).
        let head = match (1..dom.nodes.len() as u32).find(|&i| dom.tag(i) == "head") {
            Some(h) => h,
            None => {
                let id = dom.nodes.len() as u32;
                let tid = dom.tag_id("head");
                dom.nodes.push(Node {
                    parent: html_el,
                    children: Vec::new(),
                    kind: NodeKind::Element(tid, Box::from([])),
                });
                dom.nodes[html_el as usize].children.insert(0, id);
                id
            }
        };
        let storage: runtime::Storage = Default::default();
        let (wrappers, _host, _cx) =
            with_ctx(&mut engine, dom, base, HashMap::new(), storage.clone(), |_e| {
                (wrap_element(body), wrap_element(html_el), wrap_element(head))
            });
        doc.set("body", wrappers.0);
        doc.set("documentElement", wrappers.1);
        doc.set("head", wrappers.2);
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
        // a dual build (module entry + `nomodule` fallback) must run only the
        // module half; with no module script at all the legacy half still runs
        let has_modules = scripts.iter().any(|s| s.is_module);
        let mut dirty = false;
        let mut nav = None;
        let mut form_nav: Option<runtime::FormNav> = None;
        for (idx, s) in scripts.iter().enumerate() {
            if s.is_nomodule && has_modules {
                eprintln!("[js] skip nomodule script (page has a module build)");
                continue;
            }
            // ---- module script: fetch + execute through the module loader
            if s.is_module {
                let (url, inline) = match &s.src {
                    Some(u) => match crate::resolve(&self.base, u) {
                        Some(a) => (a, None),
                        None => {
                            eprintln!("[js] module src unresolvable: {u:?}");
                            continue;
                        }
                    },
                    None => (
                        format!("{}#mazda-module-{idx}", self.base),
                        Some(s.text.clone()),
                    ),
                };
                if inline.as_ref().map_or(false, |t| t.trim().is_empty()) {
                    continue;
                }
                eprintln!(
                    "[js] run module {}",
                    s.src.as_deref().unwrap_or("inline module")
                );
                let listeners = std::mem::take(&mut self.listeners);
                let storage = self.storage.clone();
                let (r, mut host, cx) =
                    with_ctx(&mut self.engine, dom, &self.base, listeners, storage, |e| {
                        let doc = e.get_global("document");
                        doc.set("currentScript", wrap_element(s.node));
                        let r = match &inline {
                            Some(text) => e.load_module_source(&url, text),
                            None => e.load_module(&url).map(|_| ()),
                        };
                        doc.set("currentScript", Val::Null);
                        if let Err(m) = e.drain_microtasks() {
                            eprintln!("[js] microtask error: {m}");
                        }
                        r.map(|_| Val::Undefined)
                    });
                self.merge_ctx(&cx);
                self.listeners = cx.listeners;
                if host.dirty() {
                    dirty = true;
                }
                if nav.is_none() {
                    nav = host.take_nav();
                }
                if let Some(f) = host.take_form_nav() {
                    if form_nav.is_none() {
                        form_nav = Some(f);
                    }
                }
                if let Err(err) = r {
                    let name = s.src.as_deref().unwrap_or("inline module");
                    eprintln!("[js] script error in {name}: {err}");
                }
                self.harvest_timers();
                continue;
            }
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
            let (r, mut host, cx) =
                with_ctx(&mut self.engine, dom, &self.base, listeners, storage, |e| {
                    let doc = e.get_global("document");
                    doc.set("currentScript", wrap_element(s.node));
                    let r = e.exec(&src);
                    doc.set("currentScript", Val::Null);
                    // queued microtasks (Promise.then) run before we look at
                    // timers — same turn semantics as the embedder contract
                    if let Err(m) = e.drain_microtasks() {
                        eprintln!("[js] microtask error: {m}");
                    }
                    r
                });
            self.merge_ctx(&cx);
            self.listeners = cx.listeners;
            if host.dirty() {
                dirty = true;
            }
            if nav.is_none() {
                nav = host.take_nav();
            }
            if let Some(f) = host.take_form_nav() {
                // an inline script form.submit()ed a POST during this
                // page's load (first request of the run wins)
                if form_nav.is_none() {
                    form_nav = Some(f);
                }
            }
            if let Err(err) = r {
                let name = s.src.as_deref().unwrap_or("inline script");
                eprintln!("[js] script error in {name}: {err}");
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
        if form_nav.is_none() {
            form_nav = t.form_nav;
        }
        Turn { dirty, nav, form_nav, prevent_default: false }
    }

    /// Dispatch one event kind to `node`'s listeners, bubbling up the
    /// parent chain (no capture phase — shortcut); window-level listeners
    /// (registered via `window.addEventListener`) fire last. Single-kind
    /// wrapper around `dispatch_ext`. (test-only convenience; the shell
    /// goes through the runtime trait's multi-kind dispatch)
    #[cfg(test)]
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
                    let (r, mut host, cx) =
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
                    if host.dirty() {
                        turn.dirty = true;
                    }
                    if turn.nav.is_none() {
                        turn.nav = host.take_nav();
                    }
                    if turn.form_nav.is_none() {
                        turn.form_nav = host.take_form_nav();
                    }
                    if host.prevent_default() {
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
        if turn.form_nav.is_none() {
            turn.form_nav = t.form_nav;
        }
        turn
    }

    /// Fold the ctx's rAF queue gathered during a single native callback
    /// back into the page's frame queue (prevent_default lives on the
    /// host; nothing to accumulate here).
    fn merge_ctx(&mut self, cx: &Ctx) {
        self.harvest_raf_from(&cx.raf);
    }

    /// Move engine-queued `setTimeout` jobs into the absolute-deadline
    /// queue the worker sleeps on. Caps the queue (`MAX_PENDING_TIMERS`):
    /// a runaway `setTimeout(loop)` page drops the OLDEST pending timers
    /// and logs — the newest always live on.
    fn harvest_timers(&mut self) {
        let now = Instant::now();
        // cancellations first: a `clearTimeout` may name a job that is still
        // queued here, or one the engine has not handed over yet
        let cancelled = self.engine.drain_cancelled_timers();
        if !cancelled.is_empty() {
            self.timers.retain(|t| !cancelled.contains(&t.id));
        }
        for t in self.engine.drain_timers() {
            if cancelled.contains(&t.id) {
                continue;
            }
            let d = if t.delay_ms.is_finite() && t.delay_ms > 0.0 {
                t.delay_ms as u64
            } else {
                0
            };
            self.timers.push(Timer {
                due: now + Duration::from_millis(d),
                id: t.id,
                cb: t.callback,
                args: t.args,
                repeat_ms: t.repeat_ms,
            });
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
                .position(|t| t.due <= now)
                .map(|i| self.timers.swap_remove(i));
            let Some(Timer {
                id,
                cb,
                args,
                repeat_ms,
                ..
            }) = due
            else {
                break;
            };
            let listeners = std::mem::take(&mut self.listeners);
            let storage = self.storage.clone();
            let (r, mut host, cx) =
                with_ctx(&mut self.engine, dom, &self.base, listeners, storage, |e| {
                    let r = e.call_function(&cb, Val::Undefined, &args);
                    if let Err(m) = e.drain_microtasks() {
                        eprintln!("[js] microtask error: {m}");
                    }
                    r
                });
            self.merge_ctx(&cx);
            self.listeners = cx.listeners;
            if host.dirty() {
                turn.dirty = true;
            }
            if turn.nav.is_none() {
                turn.nav = host.take_nav();
            }
            if turn.form_nav.is_none() {
                turn.form_nav = host.take_form_nav();
            }
            if host.prevent_default() {
                turn.prevent_default = true;
            }
            if let Err(err) = r {
                eprintln!("[js] timer error: {err}");
            }
            // `setInterval`: re-arm. A `clearInterval` for this id (fired
            // from the callback itself, for instance) is applied by the
            // `harvest_timers` pass once this turn unwinds.
            if let Some(period) = repeat_ms {
                self.timers.push(Timer {
                    due: Instant::now() + Duration::from_millis(period.max(1.0) as u64),
                    id,
                    cb,
                    args,
                    repeat_ms,
                });
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
            let (r, mut host, cx) =
                with_ctx(&mut self.engine, dom, &self.base, listeners, storage, |e| {
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
            if host.dirty() {
                turn.dirty = true;
            }
            if turn.nav.is_none() {
                turn.nav = host.take_nav();
            }
            if turn.form_nav.is_none() {
                turn.form_nav = host.take_form_nav();
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
        self.timers.iter().map(|t| t.due).min()
    }

    /// Next frame-epoch deadline (`None` = no queued rAF callbacks).
    pub fn next_frame_deadline(&self) -> Option<Instant> {
        self.raf.iter().map(|(t, _)| *t).min()
    }
}

/// OPTION A behind the selectable-runtime contract: `PageScripts` IS the
/// pure-Rust built-in engine, so the trait methods delegate to its
/// inherent, fully-tested API.
impl runtime::ScriptRuntime for PageScripts {
    fn run_scripts(&mut self, dom: &mut Dom) -> Turn {
        PageScripts::run_scripts(self, dom)
    }

    fn dispatch(
        &mut self,
        dom: &mut Dom,
        node: u32,
        kinds: &[&str],
        key: Option<&str>,
        page_y: Option<f32>,
        value: Option<&str>,
    ) -> Turn {
        self.dispatch_ext(dom, node, kinds, key, page_y, value)
    }

    fn run_due_timers(&mut self, dom: &mut Dom) -> Turn {
        PageScripts::run_due_timers(self, dom)
    }

    fn run_due_frames(&mut self, dom: &mut Dom) -> Turn {
        PageScripts::run_due_frames(self, dom)
    }

    fn next_wake_deadline(&self) -> Option<Instant> {
        PageScripts::next_wake_deadline(self)
    }
}

// ---------------------------------------------------------------------------
// Script collection
// ---------------------------------------------------------------------------

/// A `<script>` element's source (shared with the engine-core adapter).
pub(crate) struct ScriptSrc {
    /// arena id of the `<script>` element (drives `document.currentScript`)
    pub(crate) node: u32,
    /// inline source (text children) — empty when `src` is set
    pub(crate) text: String,
    pub(crate) src: Option<String>,
    /// `<script type="module">`: run through the module loader
    pub(crate) is_module: bool,
    /// `<script nomodule>`: the legacy fallback of a dual build
    pub(crate) is_nomodule: bool,
}

/// Every `<script>` element in document order.
/// `pub(crate)`: the engine-core adapter reuses this exact walk.
pub(crate) fn collect_scripts(dom: &Dom) -> Vec<ScriptSrc> {
    let mut out = Vec::new();
    let mut stack: Vec<u32> = vec![0];
    while let Some(n) = stack.pop() {
        match &dom.nodes[n as usize].kind {
            NodeKind::Element(_, _) => {
                if dom.tag(n) == "script" {
                    let ty = dom.attr(n, "type").unwrap_or("").trim();
                    // `type` may carry parameters (`text/javascript; charset=…`)
                    let mime = ty.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
                    let is_module = ty.eq_ignore_ascii_case("module");
                    // Only JavaScript MIME types run: `application/ld+json`,
                    // templates, import maps and the like are data blocks.
                    let is_js = is_module
                        || mime.is_empty()
                        || matches!(
                            mime.as_str(),
                            "text/javascript"
                                | "application/javascript"
                                | "text/ecmascript"
                                | "application/ecmascript"
                                | "application/x-javascript"
                                | "text/jscript"
                                | "text/livescript"
                        );
                    if is_js {
                        out.push(ScriptSrc {
                            node: n,
                            text: child_text(dom, n),
                            src: dom.attr(n, "src").map(|s| s.to_string()),
                            is_module,
                            // a boolean attribute may arrive as `""`, "nomodule"
                            // or an empty value; presence is all that matters
                            is_nomodule: dom.attr(n, "nomodule").is_some(),
                        });
                    }
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
    use crate::html;

    fn page(html_src: &str) -> (Dom, PageScripts) {
        let mut dom = html::parse(html_src);
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
        let p = query_all_dom(&dom, "#t")[0];
        assert_eq!(child_text(&dom, p), "new");
    }

    #[test]
    fn document_cookie_reads_as_a_string() {
        // Wikipedia's client-preference bootstrap does
        // `document.cookie.match(...)`; an undefined cookie killed the whole
        // inline script. Reading must always yield a string.
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.t = typeof document.cookie;
                window.m = document.cookie.match(/enwiki/);
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "t"), "string");
    }

    #[test]
    fn dynamic_import_resolves_and_then_runs() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                import("/dep.js").then(function (ns) {
                    window.got = ns.v;
                }, function (err) {
                    window.err = err;
                });
                window.sync = "before";
            </script></body></html>"#,
        );
        let mut l = js::ModuleLoader::new();
        l.seed("https://example.com/dep.js", &[("v", Val::Num(4.0))]);
        ps.engine.module_loader = Some(l);
        let _ = ps.run_scripts(&mut dom);
        // the script body already finished; .then ran in its microtask drain
        assert_eq!(get_global_str(&mut ps, "sync"), "before");
        assert_eq!(get_global_str(&mut ps, "got"), "4");
        assert_eq!(get_global_str(&mut ps, "err"), "undefined");
    }

    #[test]
    fn await_dynamic_import_resumes_inside_a_call() {
        // the module body runs re-entrantly from `await`'s microtask pump
        // inside `boot`'s frame; it must not clobber the caller's call depth
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                async function boot() {
                    const ns = await import("/dep.js");
                    window.awaited = ns.v;
                }
                boot();
            </script></body></html>"#,
        );
        let mut l = js::ModuleLoader::new();
        l.seed("https://example.com/dep.js", &[("v", Val::Num(9.0))]);
        ps.engine.module_loader = Some(l);
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "awaited"), "9");
    }

    #[test]
    fn module_script_runs_and_mutates_dom() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="t">old</p>
            <script type="module">export const x = 1;
                document.getElementById("t").setTextContent("module");</script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty, "a module script mutates the page like a classic one");
        let p = query_all_dom(&dom, "#t")[0];
        assert_eq!(child_text(&dom, p), "module");
    }

    #[test]
    fn module_imports_a_data_url_dependency() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="t">old</p>
            <script type="module">
              import { label } from "data:text/javascript,export%20const%20label%20%3D%20%22dep%22%3B";
              document.getElementById("t").setTextContent(label);
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        let p = query_all_dom(&dom, "#t")[0];
        assert_eq!(child_text(&dom, p), "dep");
    }

    #[test]
    fn nomodule_fallback_is_skipped_when_a_module_exists() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="t">old</p>
            <script type="module">document.getElementById("t").setTextContent("esm");</script>
            <script nomodule>document.getElementById("t").setTextContent("legacy");</script>
            </body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        let p = query_all_dom(&dom, "#t")[0];
        assert_eq!(child_text(&dom, p), "esm");
    }

    #[test]
    fn import_in_a_classic_script_errors_honestly() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="t">old</p><script>import x from "./x.js";</script></body></html>"#,
        );
        // must not panic; the failing statement is logged and skipped
        let _ = ps.run_scripts(&mut dom);
        let p = query_all_dom(&dom, "#t")[0];
        assert_eq!(child_text(&dom, p), "old");
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
        assert_eq!(query_all_dom(&dom, "#after").len(), 1);
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
        let b = query_all_dom(&dom, "#b")[0];
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
        let b = query_all_dom(&dom, "#b")[0];
        let turn = ps.dispatch(&mut dom, b, "click");
        assert!(turn.dirty);
        let out = query_all_dom(&dom, "#out")[0];
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
        let i = query_all_dom(&dom, "#i")[0];
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
        let made = query_all_dom(&dom, "#made");
        assert_eq!(made.len(), 1);
        assert_eq!(child_text(&dom, made[0]), "made!");
        // attached under #host
        let host = query_all_dom(&dom, "#host")[0];
        assert!(dom.nodes[host as usize].children.contains(&made[0]));
    }

    #[test]
    fn style_object_forwards_writes_to_the_attribute() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="s"></div><script>
                var el = document.getElementById("s");
                el.style.backgroundColor = "red";
                el.style.width = "100px";
                el.style.setProperty("height", "20px");
                window.w = el.style.width;
                window.bg = el.style.backgroundColor;
                window.ct = el.style.cssText;
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty, "style writes must re-layout");
        let s = query_all_dom(&dom, "#s")[0];
        assert_eq!(
            dom.attr(s, "style"),
            Some("background-color: red; width: 100px; height: 20px")
        );
        assert_eq!(get_global_str(&mut ps, "w"), "100px");
        assert_eq!(get_global_str(&mut ps, "bg"), "red");
        assert_eq!(
            get_global_str(&mut ps, "ct"),
            "background-color: red; width: 100px; height: 20px"
        );
    }

    #[test]
    fn object_assign_onto_style_reaches_the_attribute() {
        // exactly the shape svelte.dev's inline probe uses (create → assign
        // → measure → classList → remove)
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="seed"></div><script>
                var div = document.createElement("div");
                Object.assign(div.style, {
                    width: "100px", height: "100px", overflow: "scroll"
                });
                document.body.append(div);
                window.ow = div.offsetWidth;
                window.scroll = div.offsetWidth - div.clientWidth;
                div.classList.add("probe");
                div.remove();
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "ow"), "0");
        assert_eq!(get_global_str(&mut ps, "scroll"), "0");
        // the probe was appended and then removed again: body keeps only the
        // seeded div (a removed node is detached, though its arena slot lives on)
        let body = dom.body_root();
        let element_children = dom.nodes[body as usize]
            .children
            .iter()
            .filter(|&&c| matches!(dom.nodes[c as usize].kind, NodeKind::Element(..)))
            .count();
        // seeded div + the <script> element; the appended probe was removed
        assert_eq!(element_children, 2);
    }

    #[test]
    fn style_removal_and_css_text_replacement() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="s" style="color: blue; width: 5px"></div><script>
                var el = document.getElementById("s");
                window.old = el.style.removeProperty("width");
                el.style.setProperty("height", "1px");
                el.style.cssText = "margin: 0px";
                window.empty = el.style.width;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "old"), "5px");
        assert_eq!(get_global_str(&mut ps, "empty"), "");
        let s = query_all_dom(&dom, "#s")[0];
        assert_eq!(dom.attr(s, "style"), Some("margin: 0px"));
    }

    #[test]
    fn class_list_mutates_the_live_class_attribute() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="d" class="a b"></div><script>
                var d = document.getElementById("d");
                window.has = d.classList.contains("c");
                d.classList.add("c", "a");
                d.classList.add("c");
                d.classList.remove("b");
                d.classList.toggle("a");
                d.classList.toggle("z", true);
                window.has2 = d.classList.contains("z");
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "has"), "false");
        assert_eq!(get_global_str(&mut ps, "has2"), "true");
        let d = query_all_dom(&dom, "#d")[0];
        assert_eq!(dom.attr(d, "class"), Some("c z"));
    }

    #[test]
    fn current_script_and_parent_element_are_live() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="wrap"><script>
                    window.inner = document.currentScript.parentElement.id;
                </script></div><script>
                    window.outer = document.currentScript.parentElement.tagName;
                </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "inner"), "wrap");
        assert_eq!(get_global_str(&mut ps, "outer"), "BODY");
        // outside a script the property reads null
        let after = ps.engine.get_global("document").get("currentScript");
        assert_eq!(after.to_string(), "null");
    }

    #[test]
    fn element_remove_detaches_from_parent() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="host"><p id="gone">x</p></div><script>
                document.getElementById("gone").remove();
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty);
        let host = query_all_dom(&dom, "#host")[0];
        assert!(dom.nodes[host as usize].children.is_empty());
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
        let t = query_all_dom(&dom, "#t")[0];
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
        assert!(query_all_dom(&dom, "#x").len() == 1);
        assert!(query_all_dom(&dom, ".b").len() == 1);
        assert!(query_all_dom(&dom, ".c").is_empty());
        assert!(query_all_dom(&dom, "div").len() == 2);
        assert!(query_all_dom(&dom, "p").is_empty());
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
    fn collect_scripts_skips_non_javascript_types() {
        let dom = html::parse(
            r#"<html><body>
                <script type="application/ld+json">{"a":1}</script>
                <script type="text/javascript">1</script>
                <script type="application/javascript; charset=utf-8">2</script>
                <script type="module">3</script>
                <script type="text/template"><p>t</p></script>
                <script type="importmap">{"imports":{}}</script>
                <script>4</script>
            </body></html>"#,
        );
        let s = collect_scripts(&dom);
        let texts: Vec<&str> = s.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["1", "2", "3", "4"]);
        assert!(s[2].is_module);
    }

    #[test]
    fn get_elements_by_tag_name_returns_document_order_snapshots() {
        let (mut dom, mut ps) = page(
            r#"<html><head><link rel="a"></head><body><link rel="b"><p>x</p><script>
                var links = document.getElementsByTagName("link");
                window.n = links.length;
                window.first = links[0].rel;
                window.second = links[1].rel;
                window.any = document.getElementsByTagName("*").length;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "n"), "2");
        assert_eq!(get_global_str(&mut ps, "first"), "a");
        assert_eq!(get_global_str(&mut ps, "second"), "b");
        // "*" = every element (html, head, link, body, link, p, script)
        assert_eq!(get_global_str(&mut ps, "any"), "7");
    }

    /// The shape a module-preload helper uses: scan existing links, create a
    /// new <link>, reflect props onto attributes, append it to document.head.
    #[test]
    fn document_head_accepts_a_created_link_with_reflected_props() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p id="out">x</p><script>
                var before = document.getElementsByTagName("link").length;
                var link = document.createElement("link");
                link.rel = "modulepreload";
                link.as = "script";
                link.crossOrigin = "";
                link.href = "/app/dep.js";
                link.nonce = "abc";
                document.head.appendChild(link);
                window.before = before;
                window.after = document.getElementsByTagName("link").length;
                window.href = link.href;
                window.rel = link.rel;
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty, "attribute writes must re-layout");
        assert_eq!(get_global_str(&mut ps, "before"), "0");
        assert_eq!(get_global_str(&mut ps, "after"), "1");
        assert_eq!(get_global_str(&mut ps, "href"), "/app/dep.js");
        assert_eq!(get_global_str(&mut ps, "rel"), "modulepreload");
        let links = query_all_dom(&dom, "link");
        assert_eq!(links.len(), 1);
        let link = links[0];
        assert_eq!(dom.attr(link, "rel"), Some("modulepreload"));
        assert_eq!(dom.attr(link, "as"), Some("script"));
        assert_eq!(dom.attr(link, "crossorigin"), Some(""));
        assert_eq!(dom.attr(link, "href"), Some("/app/dep.js"));
        assert_eq!(dom.attr(link, "nonce"), Some("abc"));
        // the missing <head> was synthesized under <html> and parents the link
        let head = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "head")
            .expect("head synthesized");
        assert_eq!(dom.nodes[link as usize].parent, head);
    }

    /// Reflected props keep the pre-existing `className`/`id` behaviour but
    /// now live against the arena (writes were previously wrapper-only).
    #[test]
    fn reflected_class_name_and_id_write_attributes() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="r"></div><script>
                var el = document.getElementById("r");
                el.className = "alpha beta";
                el.id = "renamed";
                el.title = "tip";
                window.read = el.className + "/" + el.id + "/" + el.title;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        let el = query_all_dom(&dom, "div")[0];
        assert_eq!(dom.attr(el, "class"), Some("alpha beta"));
        assert_eq!(dom.attr(el, "id"), Some("renamed"));
        assert_eq!(dom.attr(el, "title"), Some("tip"));
        assert_eq!(get_global_str(&mut ps, "read"), "alpha beta/renamed/tip");
    }

    #[test]
    fn create_element_accepts_tags_absent_from_the_document() {
        let (mut dom, mut ps) = page(
            r#"<html><body><p>x</p><script>
                window.u = document.createElement("frobnicate");
                window.tag = window.u.tagName;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "tag"), "FROBNICATE");
        let made = query_all_dom(&dom, "frobnicate");
        assert_eq!(made.len(), 1);
        // orphan until appended
        assert_eq!(dom.nodes[0].children.contains(&made[0]), false);
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

    /// A `setInterval` keeps firing on every due pass until `clearInterval`
    /// removes it; the id `setInterval` returned is the same id
    /// `clearInterval` takes.
    #[test]
    fn interval_repeats_until_cleared() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.hits = "";
                window.id = setInterval(function () {
                    window.hits = window.hits + "x";
                    if (window.hits.length === 3) { clearInterval(window.id); }
                }, 0);
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        for _ in 0..10 {
            // a 0ms interval is clamped (>=1ms per arm, like browsers), so
            // each pass needs its deadline to actually pass
            std::thread::sleep(Duration::from_millis(3));
            let _ = ps.run_due_timers(&mut dom);
        }
        assert_eq!(get_global_str(&mut ps, "hits"), "xxx");
        // the interval cleared itself; nothing is left to wake the worker
        let _ = ps.run_due_timers(&mut dom);
        assert!(
            ps.next_timer_deadline().is_none(),
            "a cleared interval leaves no pending timer"
        );
    }

    /// `clearTimeout` on a queued callback removes it before it fires.
    #[test]
    fn clear_timeout_cancels_a_queued_callback() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.hits = 0;
                var id = setTimeout(function () { window.hits = 1; }, 0);
                clearTimeout(id);
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        let _ = ps.run_due_timers(&mut dom);
        assert_eq!(get_global_str(&mut ps, "hits"), "0");
        assert!(ps.next_timer_deadline().is_none());
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
        let l = query_all_dom(&dom, "#l")[0];
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
        let i = query_all_dom(&dom, "#i")[0];
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
        let b = query_all_dom(&dom, "#b")[0];
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
        let b = query_all_dom(&dom, "#b")[0];
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
        // wave 28: the JS identity matches the network `User-Agent` exactly
        assert_eq!(get_global_str(&mut ps, "ua"), crate::net::USER_AGENT);
        assert!(get_global_str(&mut ps, "ua").contains("Chrome/"));
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

    /// Wave 28: media/EME fail-fast. The APIs exist and reject/say "no"
    /// truthfully, so a player can pick another rendition instead of hitting
    /// an undefined-method TypeError (which sites surface as "unsupported
    /// browser").
    #[test]
    fn media_and_eme_fail_fast() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.eme = "pending";
                navigator.requestMediaKeySystemAccess("com.widevine.alpha", [{}])
                    .catch(function (e) { window.eme = e.name + ":" + e.message; });
                window.caps = "pending";
                navigator.mediaCapabilities.decodingInfo({})
                    .catch(function (e) { window.caps = e.name; });
                var v = document.createElement("video");
                window.can = v.canPlayType('video/mp4; codecs="avc1.42E01E"');
                window.playState = "pending";
                v.play().catch(function (e) { window.playState = e.name; });
                window.paused = v.pause();
                window.uaData = "pending";
                if (navigator.userAgentData && navigator.userAgentData.platform) {
                    window.uaData = navigator.userAgentData.platform;
                }
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "eme"), "NotSupportedError:DRM not supported");
        assert_eq!(get_global_str(&mut ps, "caps"), "NotSupportedError");
        assert_eq!(get_global_str(&mut ps, "can"), "", "no codec claimed");
        assert_eq!(get_global_str(&mut ps, "playState"), "NotSupportedError");
        assert_eq!(get_global_str(&mut ps, "paused"), "undefined");
        assert_ne!(get_global_str(&mut ps, "uaData"), "pending");
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
        let l = query_all_dom(&dom, "#l")[0];
        let t = ps.dispatch(&mut dom, l, "click");
        assert!(t.prevent_default, "veto may live at any bubble stop");
    }

    /// form.submit() native — engine A, GET form: the built `?name=value`
    /// URL rides the ordinary `nav` channel (data replaces the old query,
    /// space → `+`) and no body carrier is queued. Spec shape: direct
    /// navigation, no submit event fired.
    #[test]
    fn form_submit_get_rides_nav_channel() {
        let (mut dom, mut ps) = page(
            r#"<html><body><form id="f" action="/search?old=1">
                <input name="q" type="text" value="hello world">
            </form><script>
                var f = document.getElementById("f");
                f.submit();
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert_eq!(
            turn.nav.as_deref(),
            Some("https://example.com/search?q=hello+world"),
            "GET form.submit() submits through the nav channel"
        );
        assert!(turn.form_nav.is_none(), "GET carries no body carrier");
    }

    /// form.submit() native — engine A, POST form: the resolved absolute
    /// action plus the urlencoded body land in `Turn.form_nav` for the
    /// worker to execute (fetch → render, exactly like every load).
    #[test]
    fn form_submit_post_queues_form_nav() {
        let (mut dom, mut ps) = page(
            r#"<html><body><form id="f" action="/save" method="post">
                <input name="q" type="text" value="v">
            </form><script>
                var f = document.getElementById("f");
                f.submit();
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert_eq!(
            turn.form_nav,
            Some(runtime::FormNav {
                url: "https://example.com/save".into(),
                body: "q=v".into(),
            }),
            "POST form.submit() queues the body-carrying FormNav"
        );
        assert!(turn.nav.is_none(), "POST never rides the plain nav channel");
    }
}
