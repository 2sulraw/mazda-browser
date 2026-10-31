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
//! - listeners bubble up the parent chain (no capture phase).
//! - `fetch` is synchronous and returns a plain `{ok, status, text()}`
//!   object, not a Promise — the interpreter has no async/await.
//! - `createElement` clones the TagId of a tag already in the parsed
//!   fragment table (see `element_tid`); unknown tags log and return null.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::html::{self, Dom, NodeKind};
use crate::js::{self, JsEngine, JsError, Val};

// ---------------------------------------------------------------------------
// Thread-local script context (worker-thread only)
// ---------------------------------------------------------------------------

/// What the natives need while a script turn is running: a raw pointer to
/// the worker-owned DOM, the page base URL, and the side channels scripts
/// write through (listeners, requested navigation, DOM-mutated flag).
struct Ctx {
    dom: *mut Dom,
    base: String,
    /// (node id, event kind) → registered handler functions
    listeners: HashMap<(u32, String), Vec<Val>>,
    /// `location.assign(...)` / `location.href = ...` target
    pending_nav: Option<String>,
    /// DOM mutated during this turn → caller must re-layout
    dirty: bool,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

/// Point the thread-local context at `dom` for the duration of a turn.
/// `None` tears the context down (no native may run outside a turn).
fn ctx_set(dom: &mut Dom, base: &str) {
    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            dom: dom as *mut Dom,
            base: base.to_string(),
            listeners: HashMap::new(),
            pending_nav: None,
            dirty: false,
        })
    });
}

fn ctx_take() -> Option<Ctx> {
    CTX.with(|c| c.borrow_mut().take())
}

/// Run `f` as one script turn: install the ctx (carrying the listeners
/// registered in earlier turns), run `f` with the engine, then tear the
/// ctx down and hand back its side channels (listeners / nav / dirty).
fn with_ctx<R>(
    engine: &mut JsEngine,
    dom: &mut Dom,
    base: &str,
    listeners: HashMap<(u32, String), Vec<Val>>,
    f: impl FnOnce(&mut JsEngine) -> R,
) -> (R, Ctx) {
    ctx_set(dom, base);
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
        _ => Err(JsError::throw("TypeError: bad location native")),
    }
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
    base: String,
}

/// Result of one turn (script run / event dispatch / timer fire).
pub struct Turn {
    pub dirty: bool,
    pub nav: Option<String>,
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
            fetch: engine.register_native("fetch", fetch_native, 0),
            fetch_text: engine.register_native("response.text", fetch_text_native, 0),
        };
        NIDS.with(|c| c.set(Some(ids)));

        let doc = Val::new_object();
        doc.set("getElementById", Val::Native(ids.doc_get_element));
        doc.set("querySelector", Val::Native(ids.doc_query));
        doc.set("querySelectorAll", Val::Native(ids.doc_query_all));
        doc.set("createElement", Val::Native(ids.doc_create));

        let loc = Val::new_object();
        loc.set("href", str_val(base));
        loc.set("assign", Val::Native(ids.loc_assign));
        loc.set("replace", Val::Native(ids.loc_assign)); // shortcut: same as assign
        engine.set_global("location", loc);
        engine.set_global("fetch", Val::Native(ids.fetch));

        // body/documentElement wrappers snapshot DOM props; needs one
        // no-op turn to install the thread-local ctx
        let body = dom.body_root();
        let html_el = (1..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "html")
            .unwrap_or(0);
        let (wrappers, _cx) =
            with_ctx(&mut engine, dom, base, HashMap::new(), |_e| {
                (wrap_element(body), wrap_element(html_el))
            });
        doc.set("body", wrappers.0);
        doc.set("documentElement", wrappers.1);
        engine.set_global("document", doc);

        PageScripts {
            engine,
            listeners: HashMap::new(),
            timers: Vec::new(),
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
            let (r, cx) = with_ctx(&mut self.engine, dom, &self.base, listeners, |e| {
                let r = e.exec(&src);
                // queued microtasks (Promise.then) run before we look at
                // timers — same turn semantics as the embedder contract
                if let Err(m) = e.drain_microtasks() {
                    eprintln!("[js] microtask error: {m}");
                }
                r
            });
            self.listeners = cx.listeners;
            if cx.dirty {
                dirty = true;
            }
            if nav.is_none() {
                nav = cx.pending_nav;
            }
            if let Err(err) = r {
                eprintln!("[js] script error: {err}");
            }
            self.harvest_timers();
        }
        // fire any timers already due (delay 0 from these scripts)
        if self.run_due_timers(dom) {
            dirty = true;
        }
        Turn { dirty, nav }
    }

    /// Dispatch one event to `node`'s listeners, bubbling up the parent
    /// chain (no capture phase — shortcut). Returns the turn outcome.
    pub fn dispatch(&mut self, dom: &mut Dom, node: u32, kind: &str) -> Turn {
        let mut chain = Vec::new();
        let mut cur = Some(node);
        while let Some(n) = cur {
            chain.push(n);
            cur = dom
                .nodes
                .get(n as usize)
                .and_then(|nd| if nd.parent == n { None } else { Some(nd.parent) });
        }
        let mut dirty = false;
        let mut nav = None;
        for n in chain {
            let cbs: Vec<Val> = self
                .listeners
                .get(&(n, kind.to_string()))
                .cloned()
                .unwrap_or_default();
            for cb in cbs {
                let listeners = std::mem::take(&mut self.listeners);
                let (r, cx) =
                    with_ctx(&mut self.engine, dom, &self.base, listeners, |e| {
                        let this = wrap_element(n);
                        let r = e.call_function(&cb, this.clone(), &[this.clone()]);
                        if let Err(m) = e.drain_microtasks() {
                            eprintln!("[js] microtask error: {m}");
                        }
                        r
                    });
                self.listeners = cx.listeners;
                if cx.dirty {
                    dirty = true;
                }
                if nav.is_none() {
                    nav = cx.pending_nav;
                }
                if let Err(err) = r {
                    eprintln!("[js] listener error ({kind} on node {n}): {err}");
                }
            }
        }
        if self.run_due_timers(dom) {
            dirty = true;
        }
        Turn { dirty, nav }
    }

    /// Move engine-queued `setTimeout` jobs into the absolute-deadline
    /// queue the worker sleeps on.
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
    }

    /// Fire every timer whose deadline passed. Returns dirty.
    pub fn run_due_timers(&mut self, dom: &mut Dom) -> bool {
        self.harvest_timers();
        let now = Instant::now();
        let mut dirty = false;
        loop {
            let due = self
                .timers
                .iter()
                .position(|(t, _, _)| *t <= now)
                .map(|i| self.timers.swap_remove(i));
            let Some((_, cb, args)) = due else { break };
            let listeners = std::mem::take(&mut self.listeners);
            let (r, cx) = with_ctx(&mut self.engine, dom, &self.base, listeners, |e| {
                let r = e.call_function(&cb, Val::Undefined, &args);
                if let Err(m) = e.drain_microtasks() {
                    eprintln!("[js] microtask error: {m}");
                }
                r
            });
            self.listeners = cx.listeners;
            if cx.dirty {
                dirty = true;
            }
            if let Err(err) = r {
                eprintln!("[js] timer error: {err}");
            }
        }
        // timers queued by timers
        self.harvest_timers();
        dirty
    }

    /// Next timer deadline — the worker's `recv_timeout` budget.
    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|(t, _, _)| *t).min()
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
}
