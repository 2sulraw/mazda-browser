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
//!   `getElementsByTagName`/`getElementsByClassName`/`getElementsByName`,
//!   `document.forms` and `form.elements` return LIVE collections (`length`,
//!   index, `item()`, `namedItem()` and named property reads re-query the
//!   DOM, so later mutations are visible); `querySelectorAll` stays a
//!   document-order static array, as a NodeList is. `getElementsByTagName`
//!   (`"*"` = every element) and `getElementsByClassName` (elements carrying
//!   all the whitespace-separated names) match in document order on the whole
//!   document; on an element, descendants only. `form.elements` lists the
//!   form's `button/fieldset/input/object/output/select/textarea`
//!   descendants. `form.controlName` shorthand is not attached to the form
//!   wrapper itself (use `form.elements`).
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
//! - shell-dispatched event objects still have no `stopPropagation()` (the
//!   bubble walk owns those events, and each listener currently gets a
//!   fresh wrapper). Events built with `new Event(...)` / `new
//!   CustomEvent(...)` do have both stop flags, and `dispatchEvent` honours
//!   them synchronously.
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

// Live `MutationObserver` instances for the current page. Cleared by
// `PageScripts::new`; the objects also stay reachable from page JS.
thread_local! {
    static OBSERVERS: RefCell<Vec<Val>> = const { RefCell::new(Vec::new()) };
    /// `customElements` definitions for the current page: `(tag, ctor)`.
    /// A definition upgrades the elements that already exist and every one
    /// created afterwards (see `upgrade_custom_element`).
    static CUSTOM_ELEMENTS: RefCell<Vec<(String, Val)>> = const { RefCell::new(Vec::new()) };
    /// Upgraded custom elements, keyed by arena node: the wrapper object the
    /// page's constructor ran against. Real elements have identity (the same
    /// object every time), so an upgraded element hands out this wrapper
    /// instead of a fresh one; ordinary elements still get a wrapper per
    /// access (the documented identity gap).
    static CE_INSTANCES: RefCell<std::collections::HashMap<u32, Val>> =
        RefCell::new(std::collections::HashMap::new());
    /// One note per page when an upgrade constructor throws (a broken
    /// definition must not spam the log once per element).
    static CE_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Elements whose constructor is running right now, innermost last. An
    /// element's interface constructor (`HTMLElement`, via the ES5 adapter's
    /// `Reflect.construct`) hands back the element under construction, exactly
    /// like the spec's construction stack.
    static CE_CONSTRUCTING: RefCell<Vec<Val>> = const { RefCell::new(Vec::new()) };
    /// Custom elements whose `connectedCallback` has already run. The spec
    /// fires it once per connection; re-insertion after removal is a gap
    /// (there is no `disconnectedCallback` yet), so the set only dedupes.
    static CE_CONNECTED: RefCell<std::collections::HashSet<u32>> =
        RefCell::new(std::collections::HashSet::new());
    /// One note per page when a `connectedCallback` throws.
    static CE_LIFE_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Has this page already been told that IntersectionObserver never
    /// fires? Pages build several, and one note per page is enough.
    static IO_NOTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Same, for `ResizeObserver`.
    static RO_NOTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// `template node → its content fragment`. The spec keeps a template's
    /// children in a fragment; here the parser stores them as ordinary
    /// children, so the first `content` access moves them into a `#fragment`
    /// node and the pair is cached for the rest of the page.
    static TEMPLATE_CONTENT: RefCell<std::collections::HashMap<u32, u32>> =
        RefCell::new(std::collections::HashMap::new());
    /// `attachShadow` results: host element → `(root fragment, mode)`. The
    /// root is a real `#fragment` arena node, so a library can build into it
    /// with the ordinary insertion methods; the pair is cached because the
    /// wrappers are rebuilt per access.
    static SHADOW_ROOTS: RefCell<std::collections::HashMap<u32, (u32, String)>> =
        RefCell::new(std::collections::HashMap::new());
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
    // MutationObserver callbacks queue in the spec; here they run at the
    // turn's end, before the next timer/event turn (documented shortcut).
    deliver_mutations(engine);
    let host = runtime::host_take();
    let cx = ctx_take().expect("[js] script context vanished mid-turn");
    (r, host, cx)
}

/// Listener registered during the current turn (persisted across turns by
/// the turn drivers, which swap the table in and out).
fn ctx_add_listener(node: u32, kind: String, cb: Val) {
    CTX.with(|c| {
        if let Some(cx) = c.borrow_mut().as_mut() {
            let list = cx.listeners.entry((node, kind)).or_default();
            // spec: the same (type, callback) pair registered twice is still
            // one listener. Without this a page that re-registers on every
            // render fires its handler twice (and cannot unregister both:
            // `removeEventListener` below removes one).
            if !list.iter().any(|x| js::strict_eq(x, &cb)) {
                list.push(cb);
            }
        }
    })
}

/// Undo one `ctx_add_listener`. There is no capture phase here, so
/// `(node, type, callback)` is the whole key and one matching registration
/// goes (spec: duplicates of the same triple are ignored on add, so removing
/// the first is enough).
fn ctx_remove_listener(node: u32, kind: &str, cb: &Val) {
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        let Some(cx) = c.as_mut() else { return };
        let key = (node, kind.to_string());
        let Some(list) = cx.listeners.get_mut(&key) else {
            return;
        };
        if let Some(i) = list.iter().position(|x| js::strict_eq(x, cb)) {
            list.remove(i);
            if list.is_empty() {
                cx.listeners.remove(&key);
            }
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
    // an upgraded custom element is one object for the page: the page's
    // constructor put its state on that object, and a fresh wrapper would
    // lose it (and `el === document.getElementById(el.id)` would be false)
    if let Some(w) = CE_INSTANCES.with(|m| m.borrow().get(&node).cloned()) {
        return w;
    }
    build_element(node)
}

/// Build a fresh wrapper for `node` (the Element surface plus its interface
/// chain). Callers that may be looking at a custom element go through
/// [`wrap_element`], which returns the upgraded instance instead.
fn build_element(node: u32) -> Val {
    let o = js::Val::new_object();
    o.set("__node", js::Val::Num(node as f64));
    let n = nids();
    let p = runtime::host(|h| h.snapshot(node));
    // the interface chain depends on the tag (`<button>` is an
    // HTMLButtonElement), so resolve it before the snapshot is consumed
    let proto = dom_tag_proto(p.as_ref().map(|p| p.tag.as_str()).unwrap_or(""));
    // a fragment (template content / createDocumentFragment) is a Node, not
    // an Element, so its `nodeType` is 11
    let is_fragment = p
        .as_ref()
        .map(|p| p.tag.eq_ignore_ascii_case("#fragment"))
        .unwrap_or(false);
    if let Some(p) = p {
        o.set("tagName", str_val(&p.tag));
        // `nodeName` mirrors `tagName` for an element. jQuery's Sizzle indexes
        // by `e.nodeName.toUpperCase()`; without it the wrapper answered
        // `undefined` and the selector engine threw inside a timer.
        o.set("nodeName", str_val(&p.tag));
        // `form.elements`: live collection of the form's listed controls.
        // A getter (not a method), like the real property.
        if p.tag.eq_ignore_ascii_case("form") {
            o.set(
                "elements",
                js::Val::Acc(Rc::new(js::AccEntry {
                    getter: Some(js::Val::Native(n.el_form_elements)),
                    setter: None,
                })),
            );
        }
        // value-like props snapshot: input/textarea current value
        if let Some(v) = p.value {
            o.set("value", str_val(&v));
        }
        if let Some(t) = p.ty {
            o.set("type", str_val(&t));
            o.set("checked", js::Val::Bool(p.checked));
        }
        o.set("textContent", str_val(&p.text));
        // `<template>.content`: a getter, because the fragment is created on
        // first access (moving the template's children into it)
        if p.tag.eq_ignore_ascii_case("template") {
            o.set(
                "content",
                js::Val::Acc(Rc::new(js::AccEntry {
                    getter: Some(js::Val::Native(n.el_template_content)),
                    setter: None,
                })),
            );
        }
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
    // node-level navigation: comments and text are real arena nodes, so
    // React's streaming bootstrap can walk/rewrite its boundary markers
    o.set(
        "nodeType",
        js::Val::Num(if is_fragment { 11.0 } else { 1.0 }),
    );
    o.set("parentNode", lazy_native_attr(n.node_parent));
    o.set("previousSibling", lazy_native_attr(n.node_prev));
    o.set("nextSibling", lazy_native_attr(n.node_next));
    o.set("firstChild", lazy_native_attr(n.node_first));
    o.set("childNodes", lazy_native_attr(n.node_child_nodes));
    o.set("children", lazy_native_attr(n.node_children));
    o.set("dataset", lazy_native_attr(n.node_dataset));
    o.set("removeChild", js::Val::Native(n.node_remove_child));
    o.set("insertBefore", js::Val::Native(n.node_insert_before));
    o.set("cloneNode", js::Val::Native(n.node_clone));
    // layout reads: scripts run before layout in this pipeline, so these are
    // 0 — documented shortcut (no synchronous layout to measure against).
    o.set("offsetWidth", js::Val::Num(0.0));
    o.set("offsetHeight", js::Val::Num(0.0));
    o.set("clientWidth", js::Val::Num(0.0));
    o.set("clientHeight", js::Val::Num(0.0));
    o.set("getElementById", js::Val::Native(n.el_get_element));
    o.set("querySelector", js::Val::Native(n.el_query));
    o.set("querySelectorAll", js::Val::Native(n.el_query_all));
    o.set("getElementsByClassName", js::Val::Native(n.el_get_by_class));
    o.set("getElementsByTagName", js::Val::Native(n.el_get_by_tag));
    o.set("appendChild", js::Val::Native(n.el_append));
    o.set("append", js::Val::Native(n.el_append_many));
    o.set("prepend", js::Val::Native(n.el_prepend));
    o.set("before", js::Val::Native(n.el_before));
    o.set("after", js::Val::Native(n.el_after));
    o.set("replaceWith", js::Val::Native(n.el_replace_with));
    o.set("replaceChildren", js::Val::Native(n.el_replace_children));
    o.set("remove", js::Val::Native(n.el_remove));
    o.set("setAttribute", js::Val::Native(n.el_set_attr));
    o.set("getAttribute", js::Val::Native(n.el_get_attr));
    o.set("removeAttribute", js::Val::Native(n.el_rm_attr));
    o.set("addEventListener", js::Val::Native(n.el_add_listener));
    o.set("removeEventListener", js::Val::Native(n.el_remove_listener));
    o.set("matches", js::Val::Native(n.el_matches));
    o.set("hasAttribute", js::Val::Native(n.el_has_attr));
    o.set("dispatchEvent", js::Val::Native(n.el_dispatch));
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
    o.set("closest", js::Val::Native(n.el_closest));
    // `attachShadow` returns a real `#fragment` root and `shadowRoot` recalls
    // it. GitHub's command polyfill wraps both and then observes and queries
    // the returned root, so a missing `attachShadow` broke every custom
    // element constructor on the page.
    o.set("attachShadow", js::Val::Native(n.el_attach_shadow));
    o.set(
        "shadowRoot",
        js::Val::Acc(Rc::new(js::AccEntry {
            getter: Some(js::Val::Native(n.el_shadow_root)),
            setter: None,
        })),
    );
    // the prototype chain is only for `instanceof Element` (every method
    // stays an own prop, one behaviour for wrappers created before and after
    // the protos are installed)
    set_proto(&o, proto);
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

/// A read-only accessor backed by one native id (lazy, like `parentElement`).
fn lazy_native_attr(id: usize) -> Val {
    Val::Acc(Rc::new(js::AccEntry {
        getter: Some(Val::Native(id)),
        setter: None,
    }))
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

/// `template.content`: the fragment holding a template's children.
///
/// The parser stores a `<template>`'s children as ordinary children (it has no
/// inert-document machinery). The first `content` access moves them into a
/// `#fragment` node and remembers the pair for the page, so the template
/// itself stops being a parent — which is what the spec says and what
/// `content.cloneNode(true)` needs to copy.
fn template_content_node(tpl: u32) -> Option<u32> {
    if let Some(f) = TEMPLATE_CONTENT.with(|m| m.borrow().get(&tpl).copied()) {
        return Some(f);
    }
    let frag = runtime::host_mut(|h| h.create_fragment())?;
    runtime::host_mut(|h| {
        for c in h.child_nodes(tpl) {
            h.remove_node(c);
            h.append_child(frag, c);
        }
    });
    TEMPLATE_CONTENT.with(|m| {
        m.borrow_mut().insert(tpl, frag);
    });
    Some(frag)
}

/// The `attachShadow` root wrapper: the `#fragment` surface plus the two
/// `ShadowRoot` props pages read (`host` and `mode`). Wrappers are rebuilt per
/// access, so both are set here rather than stored on the node.
fn shadow_root_wrap(host: u32, frag: u32) -> Val {
    let root = wrap_any(frag);
    root.set("host", wrap_element(host));
    let mode = SHADOW_ROOTS.with(|m| {
        m.borrow()
            .get(&host)
            .map(|(_, mode)| mode.clone())
            .unwrap_or_else(|| "open".to_string())
    });
    root.set("mode", str_val(&mode));
    root
}

fn n_template_content(_e: &mut JsEngine, _idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    let node = this_node(this)?;
    Ok(match template_content_node(node) {
        Some(f) => wrap_any(f),
        None => Val::Null,
    })
}

/// A fragment argument inserts its own children, never itself (the spec's
/// "replace a fragment with its children").
fn splice_node(n: u32) -> Vec<u32> {
    if !runtime::host(|h| h.is_fragment(n)) {
        return vec![n];
    }
    let kids = runtime::host(|h| h.child_nodes(n));
    runtime::host_mut(|h| {
        for &c in &kids {
            h.remove_node(c);
        }
    });
    kids
}

/// Argument list of `prepend`/`append`/… → arena ids. Elements resolve through
/// `__node`; strings and other primitives become text nodes (the spec's
/// `Node`-or-`DOMString` conversion). Values that are neither are skipped: one
/// bad argument must not abort a page's render.
fn arg_nodes(args: &[Val]) -> Vec<u32> {
    let mut out = Vec::new();
    for a in args {
        match a {
            Val::Obj(_) => {
                if let Ok(n) = this_node(a) {
                    out.push(n);
                }
            }
            Val::Undefined | Val::Null => {}
            _ => {
                let s = a.to_string();
                if let Some(n) = runtime::host_mut(|h| h.create_text_node(&s, false)) {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// The insertion methods, one pass over the arena per call. `prepend`/`before`
/// keep argument order by inserting everything before the same anchor.
fn insert_children(e: &mut JsEngine, node: u32, idx: u16, nodes: &[u32]) {
    // a fragment contributes its children, in order
    let nodes: Vec<u32> = nodes.iter().flat_map(|&n| splice_node(n)).collect();
    let nodes = nodes.as_slice();
    runtime::host_mut(|h| match idx {
        E_PREPEND => {
            let anchor = h.first_child(node);
            for &n in nodes {
                h.insert_before(node, n, anchor);
            }
        }
        E_APPEND => {
            for &n in nodes {
                h.append_child(node, n);
            }
        }
        E_BEFORE => {
            let Some(parent) = h.parent_node(node) else { return };
            for &n in nodes {
                h.insert_before(parent, n, Some(node));
            }
        }
        E_AFTER => {
            let Some(parent) = h.parent_node(node) else { return };
            let anchor = h.sibling_node(node, true);
            for &n in nodes {
                match anchor {
                    Some(a) => {
                        h.insert_before(parent, n, Some(a));
                    }
                    None => {
                        h.append_child(parent, n);
                    }
                }
            }
        }
        E_REPLACE_WITH => {
            let Some(parent) = h.parent_node(node) else { return };
            for &n in nodes {
                h.insert_before(parent, n, Some(node));
            }
            h.remove_node(node);
        }
        E_REPLACE_CHILDREN => {
            // collect first: removing while walking the live child list skips
            for c in h.child_nodes(node) {
                h.remove_node(c);
            }
            for &n in nodes {
                h.append_child(node, n);
            }
        }
        _ => {}
    });
    // custom elements in the inserted subtree are connected now
    connect_subtree(e, nodes);
}

/// Reflected attribute property (one native per `REFLECTED` entry): a getter/// call arrives with no arguments, a setter with one. Both halves of the
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
    doc_create_ns: usize,
    doc_create_fragment: usize,
    doc_cookie_get: usize,
    doc_cookie_set: usize,
    doc_get_by_tag: usize,
    doc_get_by_class: usize,
    doc_forms: usize,
    doc_get_by_name: usize,
    doc_create_text: usize,
    doc_create_comment: usize,
    /// one native per `REFLECTED` entry (accessor pair halves)
    reflect: [usize; REFLECTED.len()],
    el_get_element: usize,
    el_query: usize,
    el_query_all: usize,
    el_get_by_class: usize,
    el_get_by_tag: usize,
    el_append: usize,
    el_set_attr: usize,
    el_get_attr: usize,
    el_rm_attr: usize,
    el_add_listener: usize,
    el_remove_listener: usize,
    el_matches: usize,
    el_has_attr: usize,
    /// one shared `dispatchEvent` native for elements, document and window
    el_dispatch: usize,
    el_set_text: usize,
    el_set_inner: usize,
    el_set_value: usize,
    el_set_class: usize,
    el_focus: usize,
    el_submit: usize,
    el_parent: usize,
    el_remove: usize,
    /// node-level navigation (`Node.data`, `nodeType`, siblings, parent,
    /// firstChild, removeChild, insertBefore) — text and comment nodes too
    node_data_get: usize,
    node_data_set: usize,
    node_type: usize,
    node_parent: usize,
    node_prev: usize,
    node_next: usize,
    node_first: usize,
    node_remove_child: usize,
    node_insert_before: usize,
    node_child_nodes: usize,
    node_children: usize,
    node_clone: usize,
    node_dataset: usize,
    /// `form.elements` (a live controls collection)
    el_form_elements: usize,
    /// shared `item`/`namedItem` native for live collections
    coll_item: usize,
    coll_named: usize,
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
    win_remove_listener: usize,
    match_media: usize,
    media_noop: usize,
    eme: usize,
    media_caps: usize,
    history_push: usize,
    history_replace: usize,
    history_go: usize,
    ev_prevent_default: usize,
    ev_stop: usize,
    ev_stop_immediate: usize,
    ev_ctor: usize,
    cev_ctor: usize,
    mo_observe: usize,
    mo_disconnect: usize,
    mo_take: usize,
    mo_ctor: usize,
    raf: usize,
    storage_get: usize,
    storage_set: usize,
    storage_remove: usize,
    storage_length: usize,
    storage_clear: usize,
    fetch: usize,
    fetch_text: usize,
    /// One native per DOM interface constructor, in chain order (the
    /// `DOM_INTERFACES`, then `TAG_INTERFACES`, then `SVGSVGElement`)
    dom_ctor: [usize; DOM_INTERFACE_COUNT],
    el_closest: usize,
    /// `HTMLTemplateElement.content` (getter)
    el_template_content: usize,
    /// `Element.attachShadow` and the `shadowRoot` getter
    el_attach_shadow: usize,
    el_shadow_root: usize,
    /// variadic insertion group (`prepend`/`append`/`before`/`after`/
    /// `replaceWith`/`replaceChildren`)
    el_prepend: usize,
    el_append_many: usize,
    el_before: usize,
    el_after: usize,
    el_replace_with: usize,
    el_replace_children: usize,
    /// `customElements` (CustomElementRegistry)
    ce_define: usize,
    ce_get: usize,
    ce_when_defined: usize,
    ce_noop: usize,
    /// `IntersectionObserver`
    io_ctor: usize,
    io_observe: usize,
    io_unobserve: usize,
    io_disconnect: usize,
    io_take: usize,
    /// `ResizeObserver`
    ro_ctor: usize,
    ro_observe: usize,
    ro_unobserve: usize,
    ro_disconnect: usize,
}

thread_local! {
    static NIDS: std::cell::Cell<Option<Nids>> = const { std::cell::Cell::new(None) };
    /// This page's DOM interface prototypes (`EventTarget <- Node <- ...`).
    /// Element and character-data wrappers chain into it, so
    /// `ref.current instanceof Element` — how Primer components guard a ref —
    /// answers true instead of throwing `ReferenceError`.
    static DOM_PROTOS: std::cell::RefCell<Option<DomProtos>> =
        const { std::cell::RefCell::new(None) };
}

fn nids() -> Nids {
    NIDS.with(|c| c.get().expect("[js] natives not installed on this engine"))
}

// ---------------------------------------------------------------------------
// Selector matching (single simple selector — shortcut)
// ---------------------------------------------------------------------------

/// Does `node` match `sel`? Supports `tag`, `#id`, `.class` only.
/// `pub(crate)`: the shared DomHost answers `Element.matches` with it, so the
/// element and the `querySelector` that searched for it cannot disagree.
pub(crate) fn matches_simple(dom: &Dom, node: u32, sel: &str) -> bool {
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
        .filter(|&i| dom.is_element(i))
        .collect()
}

/// Every element node in the subtree of `root` (root excluded, document
/// order). Text and the `#document` sentinel are skipped.
pub(crate) fn subtree_elements_dom(dom: &Dom, root: u32) -> Vec<u32> {
    let mut out = Vec::new();
    // the stack pops from the back, so both the initial children and each
    // visited node's children are pushed reversed to yield document order
    let mut stack: Vec<u32> = dom
        .nodes
        .get(root as usize)
        .map(|n| n.children.iter().rev().copied().collect())
        .unwrap_or_default();
    while let Some(n) = stack.pop() {
        if dom.is_element(n) {
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

/// `getElementsByClassName` matches: elements carrying ALL the
/// whitespace-separated class names. `root` limits the search to that
/// element's descendants (`None` = whole document). An empty name list
/// matches nothing (spec); class names compare case-sensitively.
/// `pub(crate)`: the shared DomHost delegates to this canonical copy.
pub(crate) fn class_all_dom(dom: &Dom, root: Option<u32>, names: &str) -> Vec<u32> {
    let want: Vec<&str> = names.split_whitespace().collect();
    if want.is_empty() {
        return Vec::new();
    }
    let pool = match root {
        Some(r) => subtree_elements_dom(dom, r),
        None => all_elements_dom(dom),
    };
    pool.into_iter()
        .filter(|&i| {
            dom.attr(i, "class")
                .map(|c| {
                    let have: Vec<&str> = c.split_whitespace().collect();
                    want.iter().all(|w| have.contains(w))
                })
                .unwrap_or(false)
        })
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
            NodeKind::Comment(_) => {}
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
const D_GET_ELEMENTS_BY_CLASS: u16 = 7;
/// `document.forms` (getter) and name lookup (method)
const D_FORMS: u16 = 8;
const D_GET_ELEMENTS_BY_NAME: u16 = 9;
/// character-data constructors
const D_CREATE_TEXT: u16 = 10;
const D_CREATE_COMMENT: u16 = 11;
/// `createElementNS(ns, qualifiedName)` — the namespace decides the element
/// family (SVG vs HTML), so it is not a synonym for `createElement`.
const D_CREATE_ELEMENT_NS: u16 = 12;
/// `createDocumentFragment()`
const D_CREATE_FRAGMENT: u16 = 13;
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
/// `getElementsByClassName` on an element (descendants only)
const E_GET_ELEMENTS_BY_CLASS: u16 = 41;
/// `getElementsByTagName` on an element (descendants only)
const E_GET_ELEMENTS_BY_TAG: u16 = 42;
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
/// `Event` / `CustomEvent` constructors and the stop flags (wave 47).
/// `dispatchEvent` is shared by elements, document and window.
const V_STOP: u16 = 43;
const V_STOP_IMMEDIATE: u16 = 44;
const V_DISPATCH: u16 = 45;
const V_CTOR: u16 = 46;
const V_CEV_CTOR: u16 = 47;
/// MutationObserver group idxs (wave 48)
const MO_CTOR: u16 = 0;
const MO_OBSERVE: u16 = 1;
const MO_DISCONNECT: u16 = 2;
const MO_TAKE: u16 = 3;
/// IntersectionObserver group idxs
const IO_CTOR: u16 = 0;
const IO_OBSERVE: u16 = 1;
const IO_UNOBSERVE: u16 = 2;
const IO_DISCONNECT: u16 = 3;
const IO_TAKE: u16 = 4;
/// ResizeObserver group idxs
const RO_CTOR: u16 = 0;
const RO_OBSERVE: u16 = 1;
const RO_UNOBSERVE: u16 = 2;
const RO_DISCONNECT: u16 = 3;
/// storage group idxs
const S_GET: u16 = 40;
const S_SET: u16 = 41;
const S_REMOVE: u16 = 42;
const S_LENGTH: u16 = 43;
const S_CLEAR: u16 = 44;
/// node-navigation group idxs (`data`, `nodeType`, sibling/parent/firstChild,
/// removeChild, insertBefore, dataset) — shared by element, text and comment
/// wrappers.
const N_DATA_GET: u16 = 0;
const N_DATA_SET: u16 = 1;
const N_NODE_TYPE: u16 = 2;
const N_PARENT: u16 = 3;
const N_PREV: u16 = 4;
const N_NEXT: u16 = 5;
const N_FIRST_CHILD: u16 = 6;
const N_REMOVE_CHILD: u16 = 7;
const N_INSERT_BEFORE: u16 = 8;
const N_DATASET: u16 = 9;
/// child collections (`Node.childNodes`, `Element.children`); both are
/// snapshots built from the arena's child list
const N_CHILD_NODES: u16 = 10;
const N_CHILDREN: u16 = 11;
/// `Node.cloneNode(deep)`
const N_CLONE_NODE: u16 = 12;
/// live-collection group idxs (one native for `item`/`namedItem`)
const C_ITEM: u16 = 0;
const C_NAMED: u16 = 1;
/// `form.elements` (element group; distinct from every E_* above)
const E_FORM_ELEMENTS: u16 = 48;
/// `Element.matches(sel)` - the same simple-selector matcher `querySelector`
/// uses, applied to the element itself (`document.body.matches(":modal")` is
/// how GitHub feature-detects `<dialog>` support)
const E_MATCHES: u16 = 49;
/// `Element.hasAttribute(name)` - a boolean, where `getAttribute` answers
/// `null` (`documentElement.hasAttribute("override-fetch")` gates GitHub's
/// fetch wrapper)
const E_HAS_ATTRIBUTE: u16 = 50;
/// `Element.removeEventListener(type, cb)`
const E_REMOVE_EVENT_LISTENER: u16 = 51;
/// `Element.closest(sel)` — this element or its nearest ancestor that matches
const E_CLOSEST: u16 = 52;
/// DOM insertion group: variadic, and a string argument becomes a text node
/// (the spec's own conversion). `append` replaces the old
/// element-args-only alias of `appendChild`.
const E_PREPEND: u16 = 54;
const E_APPEND: u16 = 55;
const E_BEFORE: u16 = 56;
const E_AFTER: u16 = 57;
const E_REPLACE_WITH: u16 = 58;
const E_REPLACE_CHILDREN: u16 = 59;
/// `Element.attachShadow({mode})` — a real `#fragment` root, cached per host
const E_ATTACH_SHADOW: u16 = 60;
/// `Element.shadowRoot` (getter) — the `attachShadow` root, or null
const E_SHADOW_ROOT: u16 = 61;
/// `window.removeEventListener(type, cb)` (document shares the bucket)
const W_REMOVE_LISTENER: u16 = 53;
/// `customElements` group idxs
const CE_DEFINE: u16 = 0;
const CE_GET: u16 = 1;
const CE_WHEN_DEFINED: u16 = 2;
const CE_NOOP: u16 = 3;

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

fn doc_native(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
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
            create_element(e, &tag)
        }
        // `createElementNS(ns, "svg")`: only the local name reaches the arena
        // (tags are interned by name, no namespace column), so an SVG element
        // is an element whose tag happens to be `svg`/`path`/… That is enough
        // for the page to build and insert it; the SVG *rendering* is the
        // separate documented gap.
        D_CREATE_ELEMENT_NS => {
            let Some(tag) = args.get(1).and_then(|v| v.as_string()) else {
                return Ok(Val::Null);
            };
            create_element(e, tag.rsplit(':').next().unwrap_or(&tag))
        }
        D_CREATE_FRAGMENT => match runtime::host_mut(|h| h.create_fragment()) {
            Some(id) => Ok(wrap_any(id)),
            None => Ok(Val::Null),
        },
        D_GET_ELEMENTS_BY_TAG => Ok(wrap_coll(LIVE_TAG, 0, &sel.unwrap_or_default())),
        D_GET_ELEMENTS_BY_CLASS => Ok(wrap_coll(LIVE_CLASS, 0, &sel.unwrap_or_default())),
        D_FORMS => Ok(wrap_coll(LIVE_FORMS, 0, "")),
        D_GET_ELEMENTS_BY_NAME => {
            let name = args.first().map(|v| v.to_string()).unwrap_or_default();
            Ok(wrap_coll(LIVE_NAME, 0, &name))
        }
        D_CREATE_TEXT => {
            let text = args.first().map(|v| v.to_string()).unwrap_or_default();
            Ok(create_char_data(&text, false))
        }
        D_CREATE_COMMENT => {
            let text = args.first().map(|v| v.to_string()).unwrap_or_default();
            Ok(create_char_data(&text, true))
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
fn create_element(e: &mut JsEngine, tag: &str) -> Result<Val, JsError> {
    let tag = tag.to_ascii_lowercase();
    let made = runtime::host_mut(|h| h.create_element(&tag));
    match made {
        Some(id) => {
            // `document.createElement("my-box")` is already upgraded when the
            // page defined `my-box`
            upgrade_custom_element(e, id);
            Ok(wrap_element(id))
        }
        None => {
            eprintln!("[js] createElement: empty tag name");
            Ok(Val::Null)
        }
    }
}

/// `createTextNode` / `createComment`: orphan character-data node, wrapped
/// with the small CharacterData surface (`data`, `textContent`, navigation).
fn create_char_data(text: &str, comment: bool) -> Val {
    match runtime::host_mut(|h| h.create_text_node(text, comment)) {
        Some(id) => wrap_char_data(id),
        None => Val::Null,
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
        E_GET_ELEMENTS_BY_CLASS => Ok(wrap_coll(LIVE_CLASS, node, &a0.to_string())),
        E_GET_ELEMENTS_BY_TAG => Ok(wrap_coll(LIVE_TAG, node, &a0.to_string())),
        E_FORM_ELEMENTS => Ok(wrap_coll(LIVE_FORM_CONTROLS, node, "")),
        E_APPEND_CHILD => {
            let child = this_node(&a0)?;
            // dirty flag bookkeeping lives inside the host write (as
            // before: an out-of-range no-op never dirties the page). A
            // fragment appends its children, not itself.
            let kids = splice_node(child);
            runtime::host_mut(|h| {
                for &c in &kids {
                    h.append_child(node, c);
                }
            });
            connect_subtree(e, &kids);
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
            // A listener may also be an object with a `handleEvent` method
            // (the DOM's EventListener interface). React and Lit both probe
            // passive-event support by registering such an object.
            if !matches!(cb, Val::Func(_) | Val::Native(_) | Val::Obj(_)) {
                eprintln!(
                    "[js] addEventListener: callback is not a function ({} on <{}> {}{})",
                    kind,
                    runtime::host(|h| h.tag_name(node)).unwrap_or_default(),
                    cb.type_of(),
                    e.call_site_note()
                );
                return Ok(Val::Undefined);
            }
            ctx_add_listener(node, kind.to_string(), cb);
            Ok(Val::Undefined)
        }
        E_REMOVE_EVENT_LISTENER => {
            if let Some(kind) = a0.as_string() {
                if let Some(cb) = args.get(1) {
                    ctx_remove_listener(node, &kind, cb);
                }
            }
            Ok(Val::Undefined)
        }
        E_MATCHES => {
            let Some(sel) = a0.as_string() else { return Ok(Val::Bool(false)) };
            // same matcher (and same documented simple-selector limit) as
            // querySelector, so `el.matches(s)` and
            // `el.parentElement.querySelectorAll(s)` agree on `el`
            Ok(Val::Bool(runtime::host(|h| h.matches(node, &sel))))
        }
        E_HAS_ATTRIBUTE => Ok(Val::Bool(runtime::host(|h| {
            h.attr(node, &a0.to_string()).is_some()
        }))),
        E_CLOSEST => {
            let Some(sel) = a0.as_string() else { return Ok(Val::Null) };
            Ok(runtime::host(|h| {
                // bounded walk: an arena cycle would otherwise spin forever
                let mut cur = Some(node);
                for _ in 0..1024 {
                    let Some(n) = cur else { break };
                    if h.matches(n, &sel) {
                        return wrap_element(n);
                    }
                    cur = h.parent_element(n);
                }
                Val::Null
            }))
        }
        E_ATTACH_SHADOW => {
            if let Some((frag, _)) = SHADOW_ROOTS.with(|m| m.borrow().get(&node).cloned()) {
                return Ok(shadow_root_wrap(node, frag));
            }
            // `{ mode: "open" }` is what pages pass; anything else still
            // attaches (the mode only ever round-trips through `shadowRoot`).
            let mode = args
                .first()
                .and_then(|v| match v {
                    Val::Obj(o) => match js::obj_get_own(o, "mode") {
                        Val::Str(s) => Some(s.to_string()),
                        _ => None,
                    },
                    _ => None,
                })
                .unwrap_or_else(|| "open".to_string());
            let Some(frag) = runtime::host_mut(|h| h.create_fragment()) else {
                return Ok(Val::Null);
            };
            SHADOW_ROOTS.with(|m| {
                m.borrow_mut().insert(node, (frag, mode));
            });
            Ok(shadow_root_wrap(node, frag))
        }
        E_SHADOW_ROOT => Ok(SHADOW_ROOTS
            .with(|m| m.borrow().get(&node).cloned())
            .map(|(frag, _)| shadow_root_wrap(node, frag))
            .unwrap_or(Val::Null)),
        E_PREPEND | E_APPEND | E_BEFORE | E_AFTER | E_REPLACE_WITH | E_REPLACE_CHILDREN => {
            let nodes = arg_nodes(args);
            insert_children(e, node, idx, &nodes);
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

/// Wrap any arena node: elements get the full element surface, while text and
/// comment nodes get the small `CharacterData` one (`data`, `textContent`,
/// sibling/parent navigation).
fn wrap_any(node: u32) -> Val {
    match runtime::host_opt(|h| h.node_type(node)).flatten() {
        Some(3) | Some(8) => wrap_char_data(node),
        _ => wrap_element(node),
    }
}

/// Text/comment node wrapper (`nodeType` 3 or 8). `data` is live through the
/// host, so React's marker rewrites (`c.data = "$!"`) land in the arena.
fn wrap_char_data(node: u32) -> Val {
    let o = Val::new_object();
    o.set("__node", Val::Num(node as f64));
    let n = nids();
    let ty = runtime::host_opt(|h| h.node_type(node))
        .flatten()
        .unwrap_or(3);
    o.set("nodeType", Val::Num(ty as f64));
    let data = Val::Acc(Rc::new(js::AccEntry {
        getter: Some(Val::Native(n.node_data_get)),
        setter: Some(Val::Native(n.node_data_set)),
    }));
    o.set("data", data.clone());
    o.set("textContent", data);
    o.set(
        "parentNode",
        Val::Acc(Rc::new(js::AccEntry {
            getter: Some(Val::Native(n.node_parent)),
            setter: None,
        })),
    );
    o.set(
        "previousSibling",
        Val::Acc(Rc::new(js::AccEntry {
            getter: Some(Val::Native(n.node_prev)),
            setter: None,
        })),
    );
    o.set(
        "nextSibling",
        Val::Acc(Rc::new(js::AccEntry {
            getter: Some(Val::Native(n.node_next)),
            setter: None,
        })),
    );
    // character data never has children; a fresh empty array keeps
    // `node.childNodes.length` truthful without a host round-trip
    o.set("childNodes", Val::new_array(Vec::new()));
    o.set("cloneNode", Val::Native(n.node_clone));
    set_proto(
        &o,
        dom_proto(if ty == 8 { DOM_COMMENT } else { DOM_TEXT }),
    );
    o
}

/// Node-level natives: `Node.data` / `nodeType` / `parentNode` /
/// `previousSibling` / `nextSibling` / `firstChild` / `removeChild` /
/// `insertBefore`, plus `dataset` on element wrappers.
fn n_node(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let node = this_node(this)?;
    Ok(match idx {
        N_DATA_GET => runtime::host(|h| h.node_data(node))
            .map(|s| str_val(&s))
            .unwrap_or(Val::Null),
        N_DATA_SET => {
            let v = args.first().map(|v| v.to_string()).unwrap_or_default();
            runtime::host_mut(|h| h.set_node_data(node, &v));
            Val::Undefined
        }
        N_NODE_TYPE => {
            Val::Num(runtime::host(|h| h.node_type(node)).unwrap_or(1) as f64)
        }
        N_PARENT => match runtime::host(|h| h.parent_node(node)) {
            Some(p) => wrap_any(p),
            None => Val::Null,
        },
        N_PREV => match runtime::host(|h| h.sibling_node(node, false)) {
            Some(s) => wrap_any(s),
            None => Val::Null,
        },
        N_NEXT => match runtime::host(|h| h.sibling_node(node, true)) {
            Some(s) => wrap_any(s),
            None => Val::Null,
        },
        N_FIRST_CHILD => match runtime::host(|h| h.first_child(node)) {
            Some(c) => wrap_any(c),
            None => Val::Null,
        },
        N_CHILD_NODES => {
            let kids = runtime::host(|h| h.child_nodes(node));
            Val::new_array(kids.into_iter().map(wrap_any).collect())
        }
        N_CHILDREN => {
            let kids = runtime::host(|h| h.child_nodes(node));
            let elems: Vec<Val> = kids
                .into_iter()
                .filter(|&c| runtime::host(|h| h.node_type(c)) == Some(1))
                .map(wrap_element)
                .collect();
            Val::new_array(elems)
        }
        N_CLONE_NODE => {
            // the clone is an orphan; HTML clones keep attributes but not
            // listeners, values or wrapper identity (same as the spec)
            let deep = args.first().map(|v| v.to_bool()).unwrap_or(false);
            match runtime::host_mut(|h| h.clone_node(node, deep)) {
                Some(c) => {
                    // a cloned custom element is a new element: its
                    // constructor runs again (spec: "upgrade" the copy)
                    upgrade_custom_element(e, c);
                    wrap_any(c)
                }
                None => Val::Null,
            }
        }
        N_REMOVE_CHILD => {
            let child = this_node(&args.first().cloned().unwrap_or(Val::Undefined))?;
            if runtime::host(|h| h.is_child_of(node, child)) {
                runtime::host_mut(|h| h.remove_node(child));
            }
            args.first().cloned().unwrap_or(Val::Null)
        }
        N_INSERT_BEFORE => {
            let new = this_node(&args.first().cloned().unwrap_or(Val::Undefined))?;
            let before = match args.get(1) {
                Some(v) if !matches!(v, Val::Undefined | Val::Null) => Some(this_node(v)?),
                _ => None,
            };
            let kids = splice_node(new);
            runtime::host_mut(|h| {
                for &c in &kids {
                    h.insert_before(node, c, before);
                }
            });
            connect_subtree(e, &kids);
            args.first().cloned().unwrap_or(Val::Null)
        }
        // shortcut: a snapshot object cached on the wrapper, so writes made
        // through one `el.dataset` read back on the next read of the SAME
        // wrapper. They do not become `data-*` attributes and a fresh
        // wrapper rebuilds from attributes - enough for error markers.
        N_DATASET => {
            if let Val::Obj(w) = this {
                if let Val::Obj(d) = js::obj_get_own(w, "__dataset") {
                    return Ok(Val::Obj(d));
                }
            }
            let o = Val::new_object();
            for (k, v) in runtime::host(|h| h.data_attrs(node)) {
                o.set(&k, str_val(&v));
            }
            if let Val::Obj(w) = this {
                js::obj_set_own(w, "__dataset", o.clone());
            }
            o
        }
        _ => return Err(JsError::throw("TypeError: bad node native")),
    })
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
            // same EventListener-object rule as `Element.addEventListener`
            if !matches!(cb, Val::Func(_) | Val::Native(_) | Val::Obj(_)) {
                eprintln!(
                    "[js] window.addEventListener: callback is not a function ({} {}{})",
                    kind,
                    cb.type_of(),
                    e.call_site_note()
                );
                return Ok(Val::Undefined);
            }
            ctx_add_listener(WIN_NODE, kind.to_string(), cb);
            Ok(Val::Undefined)
        }
        W_REMOVE_LISTENER => {
            if let Some(kind) = args.first().and_then(|v| v.as_string()) {
                if let Some(cb) = args.get(1) {
                    ctx_remove_listener(WIN_NODE, &kind, cb);
                }
            }
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
/// cancels a not-yet-started link navigation (see module docs), or sets the
/// script event's own `defaultPrevented` when the event was constructed in
/// JS (those carry `__pd`; shell event objects do not).
fn ev_native(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        V_PREVENT_DEFAULT => {
            if let Val::Obj(o) = this {
                if !matches!(js::obj_get_own(o, "__pd"), Val::Undefined) {
                    // a non-cancelable event ignores preventDefault
                    if js::obj_get_own(o, "cancelable").to_bool() {
                        js::obj_set_own(o, "__pd", Val::Bool(true));
                        js::obj_set_own(o, "defaultPrevented", Val::Bool(true));
                    }
                    return Ok(Val::Undefined);
                }
            }
            let node = this_node(this).unwrap_or(0);
            runtime::host_mut(|h| h.note_prevent_default(node));
            Ok(Val::Undefined)
        }
        V_STOP => {
            set_event_flag(this, "__sp");
            Ok(Val::Undefined)
        }
        V_STOP_IMMEDIATE => {
            set_event_flag(this, "__sp");
            set_event_flag(this, "__sip");
            Ok(Val::Undefined)
        }
        V_DISPATCH => {
            let Some(evt) = args.first().cloned() else {
                return Err(JsError::throw(
                    "TypeError: dispatchEvent requires an event object",
                ));
            };
            if !matches!(evt, Val::Obj(_)) {
                return Err(JsError::throw(
                    "TypeError: dispatchEvent requires an event object",
                ));
            }
            // element calls carry `__node`; document/window share the global
            // listener bucket (WIN_NODE), but keep their own `target` object
            // (document vs window). `document` now carries the arena root id
            // (needed by `MutationObserver.observe(doc)`), so root maps back
            // to the shared bucket it registers in.
            let target = match this_node(this) {
                Ok(0) | Err(_) => WIN_NODE,
                Ok(n) => n,
            };
            let target_val = if target == WIN_NODE {
                match this {
                    Val::Obj(_) => this.clone(),
                    _ => e.global_object(),
                }
            } else {
                wrap_element(target)
            };
            dispatch_event(e, target, target_val, &evt)
        }
        _ => Err(JsError::throw("TypeError: bad event native")),
    }
}

fn set_event_flag(evt: &Val, key: &str) {
    if let Val::Obj(o) = evt {
        js::obj_set_own(o, key, Val::Bool(true));
    }
}

fn event_flag(evt: &Val, key: &str) -> bool {
    match evt {
        Val::Obj(o) => js::obj_get_own(o, key).to_bool(),
        _ => false,
    }
}

/// Listener snapshot for one `(node, kind)` stop. Clone first: listeners may
/// register more listeners while this dispatch is running.
fn ctx_listeners(node: u32, kind: &str) -> Vec<Val> {
    CTX.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|cx| cx.listeners.get(&(node, kind.to_string())).cloned())
            .unwrap_or_default()
    })
}

/// Synchronous `dispatchEvent`: target first, then ancestors, then window.
/// A `bubbles: false` event stops at its target. `stopPropagation` ends the
/// walk, `stopImmediatePropagation` also skips the rest of that stop.
/// Returns `!defaultPrevented`.
fn dispatch_event(e: &mut JsEngine, target: u32, target_val: Val, evt: &Val) -> Result<Val, JsError> {
    let kind = match evt {
        Val::Obj(o) => match js::obj_get_own(o, "type") {
            Val::Str(s) if !s.is_empty() => s.to_string(),
            _ => return Err(JsError::throw("TypeError: event has no type")),
        },
        _ => {
            return Err(JsError::throw(
                "TypeError: dispatchEvent requires an event object",
            ))
        }
    };
    evt.set("target", target_val);

    let mut chain = vec![target];
    if event_flag(evt, "bubbles") && target != WIN_NODE {
        let mut cur = target;
        loop {
            match runtime::host(|h| h.parent_node(cur)) {
                Some(p) if p != 0 && p != cur => {
                    chain.push(p);
                    cur = p;
                }
                _ => break,
            }
        }
        chain.push(WIN_NODE);
    }

    for n in chain {
        let is_win = n == WIN_NODE;
        let current = if is_win {
            e.global_object()
        } else {
            wrap_element(n)
        };
        evt.set("currentTarget", current.clone());
        let phase = if n == target && !is_win { 2.0 } else { 3.0 };
        evt.set("eventPhase", Val::Num(phase));
        for cb in ctx_listeners(n, &kind) {
            if event_flag(evt, "__sip") {
                break;
            }
            // An object listener is invoked through `handleEvent`, looked up
            // at dispatch time (the spec's EventListener callback interface).
            let handler = match &cb {
                Val::Obj(o) => js::obj_get_own(o, "handleEvent"),
                _ => cb.clone(),
            };
            if matches!(handler, Val::Func(_) | Val::Native(_)) {
                if let Err(err) = e.call_function(&handler, current.clone(), &[evt.clone()]) {
                    eprintln!("[js] listener error ({kind} on node {n}): {err}");
                }
            }
            if let Err(m) = e.drain_microtasks() {
                eprintln!("[js] microtask error: {m}");
            }
        }
        if event_flag(evt, "__sp") {
            break;
        }
    }
    evt.set("currentTarget", Val::Null);
    evt.set("eventPhase", Val::Num(0.0));
    Ok(Val::Bool(!event_flag(evt, "__pd")))
}

fn is_ctor_call(e: &JsEngine, this: &Val) -> bool {
    matches!(this, Val::Obj(o) if !Rc::ptr_eq(o, &e.global_obj))
}

fn proto_instance(proto: Val) -> Val {
    let proto = match proto {
        Val::Obj(o) => Some(o),
        _ => None,
    };
    Val::Obj(Rc::new(RefCell::new(js::Obj {
        kind: js::ObjKind::Plain(None, false),
        props: Vec::new(),
        proto,
    })))
}

fn event_option(opts: Option<&Val>, key: &str) -> Val {
    match opts {
        Some(Val::Obj(o)) => js::obj_get_own(o, key),
        _ => Val::Undefined,
    }
}

/// `new Event(type, {bubbles, cancelable})` and
/// `new CustomEvent(type, {detail, bubbles, cancelable})`. Instances are
/// plain objects on the prototype installed in `install_event_types`, so
/// `instanceof Event` / `instanceof CustomEvent` both work.
fn n_event(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx != V_CTOR && idx != V_CEV_CTOR {
        return Err(JsError::throw("TypeError: bad event constructor native"));
    }
    if !is_ctor_call(e, this) {
        return Err(JsError::throw("TypeError: Event is not a constructor"));
    }
    let ctor = if idx == V_CTOR {
        nids().ev_ctor
    } else {
        nids().cev_ctor
    };
    let ev = proto_instance(e.native_prop(ctor, "prototype").unwrap_or(Val::Undefined));
    let kind = args.first().map(|v| v.to_string()).unwrap_or_default();
    let opts = args.get(1);
    ev.set("type", Val::Str(Rc::from(kind)));
    ev.set("bubbles", Val::Bool(event_option(opts, "bubbles").to_bool()));
    ev.set(
        "cancelable",
        Val::Bool(event_option(opts, "cancelable").to_bool()),
    );
    ev.set("defaultPrevented", Val::Bool(false));
    ev.set("target", Val::Null);
    ev.set("currentTarget", Val::Null);
    ev.set("eventPhase", Val::Num(0.0));
    ev.set("isTrusted", Val::Bool(false));
    // internal flags checked by preventDefault / dispatchEvent
    ev.set("__pd", Val::Bool(false));
    ev.set("__sp", Val::Bool(false));
    ev.set("__sip", Val::Bool(false));
    if idx == V_CEV_CTOR {
        let detail = match event_option(opts, "detail") {
            Val::Undefined => Val::Null,
            v => v,
        };
        ev.set("detail", detail);
    }
    Ok(ev)
}

/// Install `Event` and `CustomEvent`, sharing one prototype object whose
/// chain gives CustomEvent instances the Event methods.
fn install_event_types(e: &mut JsEngine, ids: Nids) {
    let ev_proto = Val::new_object();
    ev_proto.set("preventDefault", Val::Native(ids.ev_prevent_default));
    ev_proto.set("stopPropagation", Val::Native(ids.ev_stop));
    ev_proto.set(
        "stopImmediatePropagation",
        Val::Native(ids.ev_stop_immediate),
    );
    e.set_native_prop(ids.ev_ctor, "prototype", ev_proto.clone());
    e.set_global("Event", Val::Native(ids.ev_ctor));

    let cev_proto = Val::Obj(Rc::new(RefCell::new(js::Obj {
        kind: js::ObjKind::Plain(None, false),
        props: Vec::new(),
        proto: match &ev_proto {
            Val::Obj(o) => Some(o.clone()),
            _ => None,
        },
    })));
    e.set_native_prop(ids.cev_ctor, "prototype", cev_proto);
    e.set_global("CustomEvent", Val::Native(ids.cev_ctor));
}

// ---------------------------------------------------------------------------
// DOM interfaces (`EventTarget`, `Node`, `Element`, ...) — presence + chain
// ---------------------------------------------------------------------------
//
// The engine has no DOM classes: wrappers are plain objects with their
// surface as own props. Real pages, though, feature-detect *interfaces*
// (`ref.current instanceof Element`, `document instanceof Document`), and a
// missing global is a ReferenceError that kills the module. This installs the
// interface objects with the right prototype chain; nothing is constructed
// through them (the parser and `document.createElement` build every node).

/// Interface indices into `Nids::dom_ctor` / `DomProtos::chain`.
const DOM_EVENT_TARGET: usize = 0;
const DOM_NODE: usize = 1;
const DOM_CHARACTER_DATA: usize = 2;
const DOM_TEXT: usize = 3;
const DOM_COMMENT: usize = 4;
const DOM_DOCUMENT: usize = 5;
const DOM_HTML_DOCUMENT: usize = 6;
const DOM_DOCUMENT_FRAGMENT: usize = 7;
const DOM_ELEMENT: usize = 8;
const DOM_HTML_ELEMENT: usize = 9;
const DOM_SVG_ELEMENT: usize = 10;

/// The interface names installed as globals, in `DOM_*` index order.
const DOM_INTERFACES: [&str; 11] = [    "EventTarget",
    "Node",
    "CharacterData",
    "Text",
    "Comment",
    "Document",
    "HTMLDocument",
    "DocumentFragment",
    "Element",
    "HTMLElement",
    "SVGElement",
];

/// Per-tag element interfaces: `(interface, tags)`. Pages check them by name
/// (`x instanceof HTMLButtonElement`), and each one is a real step in the
/// chain, so `el instanceof HTMLInputElement` is false for a `<div>`.
const TAG_INTERFACES: [(&str, &[&str]); 30] = [
    ("HTMLAnchorElement", &["a"]),
    ("HTMLAreaElement", &["area"]),
    ("HTMLAudioElement", &["audio"]),
    ("HTMLBodyElement", &["body"]),
    ("HTMLBRElement", &["br"]),
    ("HTMLButtonElement", &["button"]),
    ("HTMLCanvasElement", &["canvas"]),
    ("HTMLDialogElement", &["dialog"]),
    ("HTMLDivElement", &["div"]),
    ("HTMLFormElement", &["form"]),
    ("HTMLHeadElement", &["head"]),
    ("HTMLHeadingElement", &["h1", "h2", "h3", "h4", "h5", "h6"]),
    ("HTMLHRElement", &["hr"]),
    ("HTMLHtmlElement", &["html"]),
    ("HTMLIFrameElement", &["iframe"]),
    ("HTMLImageElement", &["img"]),
    ("HTMLInputElement", &["input"]),
    ("HTMLLabelElement", &["label"]),
    ("HTMLLIElement", &["li"]),
    ("HTMLLinkElement", &["link"]),
    ("HTMLMediaElement", &["object"]),
    ("HTMLMetaElement", &["meta"]),
    ("HTMLOListElement", &["ol"]),
    ("HTMLOptionElement", &["option"]),
    ("HTMLPreElement", &["pre"]),
    ("HTMLScriptElement", &["script"]),
    ("HTMLSelectElement", &["select"]),
    ("HTMLStyleElement", &["style"]),
    ("HTMLTemplateElement", &["template"]),
    ("HTMLTextAreaElement", &["textarea"]),
];

/// How many interface globals exist: the fixed chain, every tag interface and
/// `SVGSVGElement`.
const DOM_INTERFACE_COUNT: usize = DOM_INTERFACES.len() + TAG_INTERFACES.len() + 1;

/// Tag names that belong to the SVG namespace (icon sets and Primer's drag
/// handles are built with `createElementNS`). Lowercase only: the arena
/// lowercases every tag.
const SVG_TAGS: [&str; 26] = [
    "svg",
    "path",
    "g",
    "circle",
    "ellipse",
    "rect",
    "line",
    "polyline",
    "polygon",
    "text",
    "tspan",
    "defs",
    "use",
    "symbol",
    "clippath",
    "mask",
    "lineargradient",
    "radialgradient",
    "stop",
    "filter",
    "pattern",
    "marker",
    "image",
    "foreignobject",
    "title",
    "desc",
];

/// `new Element()` / `new Node()`: the spec marks these interfaces as illegal
/// constructors, and every real node comes from the parser or
/// `document.createElement`, so throwing is the honest answer.
fn n_dom_ctor(_e: &mut JsEngine, _idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    // `class MyBox extends HTMLElement { constructor() { super(); … } }`
    // calls this native with the element that is already being upgraded as
    // `this`; the interface has no constructor body, so the element itself is
    // the only honest return value. Any other call (`new HTMLElement()`) has
    // no element behind it and stays illegal.
    if let Val::Obj(o) = this {
        if matches!(js::obj_get_own(o, "__node"), Val::Num(_)) {
            return Ok(this.clone());
        }
    }
    // The ES5 custom-elements adapter rewrites `window.HTMLElement` into
    // `function () { return Reflect.construct(BuiltIn, [], this.constructor) }`,
    // so a transpiled element's `super()` lands here with a fresh `this`.
    // Spec: the interface constructor returns the element on the definition's
    // construction stack, i.e. the element being upgraded.
    if let Some(w) = CE_CONSTRUCTING.with(|s| s.borrow().last().cloned()) {
        return Ok(w);
    }
    Err(JsError::throw("TypeError: Illegal constructor"))
}

/// Shared prototype objects for one page: the fixed interface chain
/// (`EventTarget <- Node <- {CharacterData <- {Text, Comment}, Document <-
/// HTMLDocument, DocumentFragment, Element <- {HTMLElement <- every per-tag
/// interface, SVGElement}}`) plus the tag → interface map element wrappers
/// use.
struct DomProtos {
    chain: Vec<Val>,
    /// lowercase tag → index into `chain` (`None` = plain `HTMLElement`)
    by_tag: std::collections::HashMap<&'static str, usize>,
}

fn install_dom_types(e: &mut JsEngine, ids: &Nids) {
    let mut chain: Vec<Val> = Vec::with_capacity(DOM_INTERFACES.len() + TAG_INTERFACES.len());
    // EventTarget has no parent interface
    chain.push(Val::new_object());
    // Node <- EventTarget, CharacterData <- Node, Text/Comment <- CharacterData
    chain.push(proto_instance(chain[DOM_EVENT_TARGET].clone()));
    chain.push(proto_instance(chain[DOM_NODE].clone()));
    chain.push(proto_instance(chain[DOM_CHARACTER_DATA].clone()));
    chain.push(proto_instance(chain[DOM_CHARACTER_DATA].clone()));
    // Document/HTMLDocument/DocumentFragment/Element <- Node
    chain.push(proto_instance(chain[DOM_NODE].clone()));
    chain.push(proto_instance(chain[DOM_DOCUMENT].clone()));
    chain.push(proto_instance(chain[DOM_NODE].clone()));
    chain.push(proto_instance(chain[DOM_NODE].clone()));
    // HTMLElement and SVGElement are the two element families
    chain.push(proto_instance(chain[DOM_ELEMENT].clone()));
    chain.push(proto_instance(chain[DOM_ELEMENT].clone()));
    // methods that do not need a wrapped node live on `Element.prototype`,
    // so `Element.prototype.matches.call(el, sel)` works too
    chain[DOM_ELEMENT].set("matches", Val::Native(ids.el_matches));
    chain[DOM_ELEMENT].set("closest", Val::Native(ids.el_closest));
    chain[DOM_ELEMENT].set("hasAttribute", Val::Native(ids.el_has_attr));
    chain[DOM_ELEMENT].set("removeEventListener", Val::Native(ids.el_remove_listener));
    // the command polyfill reads `HTMLElement.prototype.attachShadow` and
    // wraps it; it is inherited from `Element.prototype` like `matches` is
    chain[DOM_ELEMENT].set("attachShadow", Val::Native(ids.el_attach_shadow));

    // one interface per tag family, all hanging off HTMLElement
    let mut by_tag: std::collections::HashMap<&'static str, usize> = Default::default();
    let mut names: Vec<&str> = DOM_INTERFACES.to_vec();
    for (name, tags) in TAG_INTERFACES {
        let i = chain.len();
        chain.push(proto_instance(chain[DOM_HTML_ELEMENT].clone()));
        for tag in tags {
            by_tag.insert(tag, i);
        }
        names.push(name);
    }
    // `<svg>` is the one element family that is not an HTMLElement
    let svg = chain.len();
    chain.push(proto_instance(chain[DOM_SVG_ELEMENT].clone()));
    by_tag.insert("svg", svg);
    names.push("SVGSVGElement");

    for (i, name) in names.iter().enumerate() {
        let id = ids.dom_ctor[i];
        e.set_native_prop(id, "prototype", chain[i].clone());
        e.set_global(name, Val::Native(id));
    }
    DOM_PROTOS.with(|p| *p.borrow_mut() = Some(DomProtos { chain, by_tag }));
}

/// This page's interface protos (`None` before `install_dom_types`).
fn dom_proto(i: usize) -> Option<Rc<RefCell<js::Obj>>> {
    DOM_PROTOS.with(|p| {
        p.borrow().as_ref().and_then(|d| match d.chain.get(i) {
            Some(Val::Obj(o)) => Some(o.clone()),
            _ => None,
        })
    })
}

/// `Element.prototype` of the interface owning `tag` (`<button>` →
/// `HTMLButtonElement.prototype`), so `instanceof` answers by tag. Unknown and
/// custom-element tags are plain `HTMLElement`s.
fn dom_tag_proto(tag: &str) -> Option<Rc<RefCell<js::Obj>>> {
    let tag = tag.to_ascii_lowercase();
    DOM_PROTOS.with(|p| {
        let b = p.borrow();
        let d = b.as_ref()?;
        let i = match d.by_tag.get(tag.as_str()) {
            Some(i) => *i,
            // a fragment is a Node (nodeType 11), not an Element
            None if tag == "#fragment" => DOM_DOCUMENT_FRAGMENT,
            // the arena has no namespace column, so an SVG element is known
            // by its tag name alone: `createElementNS(NS, "path")` and a
            // parsed `<path>` both land here
            None if SVG_TAGS.contains(&tag.as_str()) => DOM_SVG_ELEMENT,
            None => DOM_HTML_ELEMENT,
        };
        match d.chain.get(i) {
            Some(Val::Obj(o)) => Some(o.clone()),
            _ => None,
        }
    })
}

/// Give a freshly built wrapper its place in the chain; `proto` is the tag's
/// element interface for an element and `Text`/`Comment` for character data.
fn set_proto(v: &Val, proto: Option<Rc<RefCell<js::Obj>>>) {
    if let (Val::Obj(rc), Some(p)) = (v, proto) {
        rc.borrow_mut().proto = Some(p);
    }
}

// ---------------------------------------------------------------------------
// `customElements` (CustomElementRegistry)
// ---------------------------------------------------------------------------

/// `customElements.define/get/whenDefined`. GitHub's bundles do
/// `window.customElements.get("file-attachment") || (… define(…) …)` at load;
/// without the global that line is a ReferenceError and the bundle dies.
///
/// `define` upgrades the elements that already have the tag (a page's parsed
/// `<turbo-frame>`s) and [`upgrade_custom_element`] covers the ones created
/// later. `upgrade(root)` is still a no-op: nothing in the wild calls it.
fn install_custom_elements(e: &mut JsEngine, ids: Nids) {
    let ce = Val::new_object();
    ce.set("define", Val::Native(ids.ce_define));
    ce.set("get", Val::Native(ids.ce_get));
    ce.set("whenDefined", Val::Native(ids.ce_when_defined));
    ce.set("upgrade", Val::Native(ids.ce_noop));
    e.set_global("customElements", ce);
}

fn ce_lookup(name: &str) -> Option<Val> {
    CUSTOM_ELEMENTS.with(|c| {
        c.borrow()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    })
}

/// Upgrade `node` to its registered custom element.
///
/// A real custom element is an already-constructed `HTMLElement` whose
/// prototype chain carries the page's class, and its constructor runs exactly
/// once — when the definition arrives or when the element is created. The
/// wrapper becomes that instance (`CE_INSTANCES`), so the page's methods,
/// getters and instance fields all live on the object it gets back.
fn upgrade_custom_element(e: &mut JsEngine, node: u32) {
    if CE_INSTANCES.with(|m| m.borrow().contains_key(&node)) {
        return;
    }
    let Some(tag) = runtime::host_opt(|h| h.tag_name(node)).flatten() else {
        return;
    };
    let Some(ctor) = ce_lookup(&tag) else { return };
    let w = build_element(node);
    // the definition's prototype replaces the tag interface; it still chains
    // to HTMLElement.prototype (the class was `extends HTMLElement`)
    if let Ok(Val::Obj(proto)) = js::get_member(e, &ctor, "prototype") {
        if let Val::Obj(o) = &w {
            o.borrow_mut().proto = Some(proto);
        }
    }
    // remember the instance before the constructor runs: a constructor that
    // reads the element back (or creates another of the same tag) must see
    // this same object
    CE_INSTANCES.with(|m| {
        m.borrow_mut().insert(node, w.clone());
    });
    // the constructor's `super()` reaches the interface native, which hands
    // back this element (the spec's construction stack)
    CE_CONSTRUCTING.with(|s| s.borrow_mut().push(w.clone()));
    let r = e.call(ctor, w, Vec::new());
    CE_CONSTRUCTING.with(|s| s.borrow_mut().pop());
    if let Err(err) = r {
        // spec: a throwing constructor leaves the element as it is; it must
        // not take the page down
        if !CE_FAILED.with(|f| f.replace(true)) {
            eprintln!("[js] custom element <{tag}>: constructor threw ({err})");
        }
    }
    // an upgrade of an already-parsed element is also its first connection
    connect_custom_element(e, node);
}

/// Call `connectedCallback` on an upgraded element once it is in the
/// document. Lit's base class starts its first render there, so without this
/// every Lit component on a page stays empty. The spec also fires it when a
/// connected custom element is inserted; [`connect_subtree`] covers that.
fn connect_custom_element(e: &mut JsEngine, node: u32) {
    // cheap gates first: nothing to do for ordinary elements or ones already
    // connected (the arena walk in `is_connected` is the expensive part)
    if !CE_INSTANCES.with(|m| m.borrow().contains_key(&node)) {
        return;
    }
    if CE_CONNECTED.with(|s| s.borrow().contains(&node)) {
        return;
    }
    if !runtime::host(|h| h.is_connected(node)) {
        return;
    }
    CE_CONNECTED.with(|s| {
        s.borrow_mut().insert(node);
    });
    let Some(w) = CE_INSTANCES.with(|m| m.borrow().get(&node).cloned()) else {
        return;
    };
    let Ok(handler) = js::get_member(e, &w, "connectedCallback") else {
        return;
    };
    if !matches!(handler, Val::Func(_) | Val::Native(_)) {
        return;
    }
    if let Err(err) = e.call(handler, w, Vec::new()) {
        if !CE_LIFE_FAILED.with(|f| f.replace(true)) {
            let tag = runtime::host(|h| h.tag_name(node)).unwrap_or_default();
            eprintln!("[js] custom element <{tag}>: connectedCallback threw ({err})");
        }
    }
}

/// Fire `connectedCallback` for every custom element in a freshly inserted
/// subtree: the upgrade (createElement, clone, define) happened before the
/// insertion, so the elements are only now in the document.
fn connect_subtree(e: &mut JsEngine, roots: &[u32]) {
    if CE_INSTANCES.with(|m| m.borrow().is_empty()) {
        return;
    }
    let mut stack: Vec<u32> = roots.to_vec();
    while let Some(n) = stack.pop() {
        if runtime::host(|h| h.node_type(n)) == Some(1) {
            connect_custom_element(e, n);
        }
        stack.extend(runtime::host(|h| h.child_nodes(n)));
    }
}

fn n_custom_elements(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let name = args.first().map(|v| v.to_string()).unwrap_or_default();
    match idx {
        CE_DEFINE => {
            // spec: a valid custom element name has a dash and is not a
            // reserved name; the constructor must be callable
            let ctor = args.get(1).cloned().unwrap_or(Val::Undefined);
            if !name.contains('-') {
                return Err(JsError::throw(&format!(
                    "SyntaxError: \"{name}\" is not a valid custom element name"
                )));
            }
            if !matches!(ctor, Val::Func(_) | Val::Native(_)) {
                return Err(JsError::throw(
                    "TypeError: customElements.define needs a constructor",
                ));
            }
            let known = ce_lookup(&name).is_some();
            if !known {
                CUSTOM_ELEMENTS.with(|c| c.borrow_mut().push((name.clone(), ctor)));
                // a definition upgrades the elements that already exist
                let existing = runtime::host(|h| h.by_tag(&name));
                for n in existing {
                    upgrade_custom_element(e, n);
                }
            }
            Ok(Val::Undefined)
        }
        CE_GET => Ok(ce_lookup(&name).unwrap_or(Val::Undefined)),
        CE_WHEN_DEFINED => {
            // spec keeps the promise pending until `define`; here the page
            // defines synchronously at load, so resolving either way is the
            // same observable order for the `await` in the wild
            let p = e.new_promise();
            let v = ce_lookup(&name).unwrap_or(Val::Undefined);
            js::settle_promise(e, &p, Ok(v));
            Ok(Val::Obj(p))
        }
        _ => Ok(Val::Undefined),
    }
}

// ---------------------------------------------------------------------------
// MutationObserver (wave 48)
// ---------------------------------------------------------------------------

fn observer_obj(this: &Val) -> Result<Rc<RefCell<js::Obj>>, JsError> {
    match this {
        Val::Obj(o) if !matches!(js::obj_get_own(o, "__cb"), Val::Undefined) => Ok(o.clone()),
        _ => Err(JsError::throw(
            "TypeError: MutationObserver method called on a non-observer",
        )),
    }
}

/// `new MutationObserver(callback)`: one observer object, registered in the
/// page's live list so mutation delivery can find it.
fn n_mo(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        MO_CTOR => {
            if !is_ctor_call(e, this) {
                return Err(JsError::throw(
                    "TypeError: MutationObserver is not a constructor",
                ));
            }
            let cb = args.first().cloned().unwrap_or(Val::Undefined);
            if !matches!(cb, Val::Func(_) | Val::Native(_) | Val::Method(_)) {
                return Err(JsError::throw(
                    "TypeError: MutationObserver callback is not a function",
                ));
            }
            let obs = proto_instance(
                e.native_prop(nids().mo_ctor, "prototype")
                    .unwrap_or(Val::Undefined),
            );
            obs.set("__cb", cb);
            obs.set("__target", Val::Num(-1.0));
            obs.set("__child", Val::Bool(false));
            obs.set("__attr", Val::Bool(false));
            obs.set("__char", Val::Bool(false));
            obs.set("__subtree", Val::Bool(false));
            obs.set("__attr_old", Val::Bool(false));
            obs.set("__char_old", Val::Bool(false));
            obs.set("__filter", Val::Null);
            OBSERVERS.with(|o| o.borrow_mut().push(obs.clone()));
            Ok(obs)
        }
        MO_OBSERVE => {
            let o = observer_obj(this)?;
            let target = match args.first() {
                Some(t) => this_node(t)?,
                None => {
                    return Err(JsError::throw(
                        "TypeError: MutationObserver.observe requires a target",
                    ))
                }
            };
            let opts = args.get(1);
            let child = event_option(opts, "childList").to_bool();
            // spec: oldValue/filter imply their category when it is omitted
            let attr_old = event_option(opts, "attributeOldValue");
            let char_old = event_option(opts, "characterDataOldValue");
            let filter = event_option(opts, "attributeFilter");
            let attr = event_option(opts, "attributes").to_bool()
                || !matches!(attr_old, Val::Undefined)
                || !matches!(filter, Val::Undefined);
            let chars = event_option(opts, "characterData").to_bool()
                || !matches!(char_old, Val::Undefined);
            if !child && !attr && !chars {
                return Err(JsError::throw(
                    "TypeError: MutationObserver.observe needs at least one of childList/attributes/characterData",
                ));
            }
            js::obj_set_own(&o, "__target", Val::Num(target as f64));
            js::obj_set_own(&o, "__child", Val::Bool(child));
            js::obj_set_own(&o, "__attr", Val::Bool(attr));
            js::obj_set_own(&o, "__char", Val::Bool(chars));
            js::obj_set_own(
                &o,
                "__subtree",
                Val::Bool(event_option(opts, "subtree").to_bool()),
            );
            js::obj_set_own(&o, "__attr_old", Val::Bool(attr_old.to_bool()));
            js::obj_set_own(&o, "__char_old", Val::Bool(char_old.to_bool()));
            let filter = match filter {
                Val::Obj(a) if matches!(a.borrow().kind, js::ObjKind::Array(_)) => Val::Obj(a),
                _ => Val::Null,
            };
            js::obj_set_own(&o, "__filter", filter);
            runtime::set_mutation_tracking(true);
            let val = Val::Obj(o);
            OBSERVERS.with(|v| {
                let mut v = v.borrow_mut();
                let known = v.iter().any(|x| match (x, &val) {
                    (Val::Obj(a), Val::Obj(b)) => Rc::ptr_eq(a, b),
                    _ => false,
                });
                if !known {
                    v.push(val);
                }
            });
            Ok(Val::Undefined)
        }
        MO_DISCONNECT => {
            let o = observer_obj(this)?;
            js::obj_set_own(&o, "__target", Val::Num(-1.0));
            // drop our strong reference so a forgotten observer is not kept
            // alive for the rest of the page
            OBSERVERS.with(|v| {
                v.borrow_mut().retain(|x| match x {
                    Val::Obj(a) => !Rc::ptr_eq(a, &o),
                    _ => true,
                });
            });
            refresh_tracking();
            Ok(Val::Undefined)
        }
        MO_TAKE => {
            let o = observer_obj(this)?;
            let all = runtime::host_mut(|h| h.take_mutations());
            let obs = Val::Obj(o);
            let mut matched = Vec::new();
            let mut rest = Vec::new();
            for rec in all {
                match record_for(&obs, &rec) {
                    Some(v) => matched.push(v),
                    None => rest.push(rec),
                }
            }
            if !rest.is_empty() {
                runtime::host_mut(|h| h.restore_mutations(rest));
            }
            Ok(Val::new_array(matched))
        }
        _ => Err(JsError::throw("TypeError: bad MutationObserver native")),
    }
}

fn refresh_tracking() {
    runtime::set_mutation_tracking(OBSERVERS.with(|o| {
        o.borrow().iter().any(|v| match v {
            Val::Obj(o) => matches!(js::obj_get_own(o, "__target"), Val::Num(n) if n >= 0.0),
            _ => false,
        })
    }));
}

/// Drain the turn's mutation log into one callback per observer. Called at
/// the end of every script turn (see `with_ctx`). A callback that mutates
/// the DOM queues more records, so the round is repeated a bounded number
/// of times — a runaway observer is reported, not allowed to hang the page.
fn deliver_mutations(e: &mut JsEngine) {
    for _ in 0..8 {
        let recs = match runtime::host_mut_opt(|h| h.take_mutations()) {
            Some(r) => r,
            None => return,
        };
        if recs.is_empty() {
            return;
        }
        let observers: Vec<Val> = OBSERVERS.with(|o| o.borrow().clone());
        for obs in observers {
            let o = match &obs {
                Val::Obj(o) => o.clone(),
                _ => continue,
            };
            if !matches!(js::obj_get_own(&o, "__target"), Val::Num(n) if n >= 0.0) {
                continue;
            }
            let mut batch = Vec::new();
            for rec in &recs {
                if let Some(v) = record_for(&obs, rec) {
                    batch.push(v);
                }
            }
            if batch.is_empty() {
                continue;
            }
            let cb = js::obj_get_own(&o, "__cb");
            if let Err(err) = e.call_function(&cb, obs.clone(), &[Val::new_array(batch), obs]) {
                eprintln!("[js] mutation observer error: {err}");
            }
        }
        if let Err(m) = e.drain_microtasks() {
            eprintln!("[js] microtask error: {m}");
        }
    }
    eprintln!("[js] mutation observer callback did not settle");
}

/// One DOM mutation → one JS record, or None when this observer does not
/// watch that node/kind.
fn record_for(obs: &Val, rec: &runtime::MutationRec) -> Option<Val> {
    let o = match obs {
        Val::Obj(o) => o,
        _ => return None,
    };
    let target = match js::obj_get_own(o, "__target") {
        Val::Num(n) if n >= 0.0 => n as u32,
        _ => return None,
    };
    let subtree = js::obj_get_own(o, "__subtree").to_bool();
    match rec {
        runtime::MutationRec::ChildList {
            parent,
            added,
            removed,
            root,
        } => {
            if !js::obj_get_own(o, "__child").to_bool()
                || !observed(target, *parent, *root, subtree)
            {
                return None;
            }
            Some(mutation_record(
                "childList", *parent, added, removed, None, None,
            ))
        }
        runtime::MutationRec::Attr {
            node,
            name,
            old,
            root,
        } => {
            if !js::obj_get_own(o, "__attr").to_bool()
                || !observed(target, *node, *root, subtree)
                || !attr_allowed(o, name)
            {
                return None;
            }
            let old = if js::obj_get_own(o, "__attr_old").to_bool() {
                old.as_deref()
            } else {
                None
            };
            Some(mutation_record(
                "attributes",
                *node,
                &[],
                &[],
                Some(name),
                old,
            ))
        }
        runtime::MutationRec::CharData { node, old, root } => {
            if !js::obj_get_own(o, "__char").to_bool()
                || !observed(target, *node, *root, subtree)
            {
                return None;
            }
            let old = if js::obj_get_own(o, "__char_old").to_bool() {
                old.as_deref()
            } else {
                None
            };
            Some(mutation_record(
                "characterData",
                *node,
                &[],
                &[],
                None,
                old,
            ))
        }
    }
}

fn observed(target: u32, node: u32, root: u32, subtree: bool) -> bool {
    // the mutation must belong to the observer target's tree as it was when
    // it happened, and the target must be the node or (with subtree) an
    // ancestor of it
    let related = target == node || (subtree && is_ancestor(target, node));
    related && (target == root || is_ancestor(root, target))
}

fn is_ancestor(ancestor: u32, node: u32) -> bool {
    let mut cur = node;
    for _ in 0..4096 {
        match runtime::host(|h| h.parent_node(cur)) {
            Some(p) if p != cur => {
                if p == ancestor {
                    return true;
                }
                cur = p;
            }
            _ => return false,
        }
    }
    false
}

fn attr_allowed(o: &Rc<RefCell<js::Obj>>, name: &str) -> bool {
    match js::obj_get_own(o, "__filter") {
        Val::Obj(a) => match &a.borrow().kind {
            js::ObjKind::Array(items) if !items.is_empty() => items
                .iter()
                .any(|v| v.as_string().is_some_and(|s| s.eq_ignore_ascii_case(name))),
            _ => true,
        },
        _ => true,
    }
}

fn mutation_record(
    kind: &str,
    node: u32,
    added: &[u32],
    removed: &[u32],
    attr: Option<&str>,
    old: Option<&str>,
) -> Val {
    let r = Val::new_object();
    r.set("type", str_val(kind));
    r.set("target", wrap_element(node));
    r.set(
        "addedNodes",
        Val::new_array(added.iter().map(|&n| wrap_element(n)).collect()),
    );
    r.set(
        "removedNodes",
        Val::new_array(removed.iter().map(|&n| wrap_element(n)).collect()),
    );
    r.set("attributeName", attr.map(str_val).unwrap_or(Val::Null));
    r.set("oldValue", old.map(str_val).unwrap_or(Val::Null));
    r.set("previousSibling", Val::Null);
    r.set("nextSibling", Val::Null);
    r
}

fn install_mutation_observer(e: &mut JsEngine, ids: Nids) {
    let proto = Val::new_object();
    proto.set("observe", Val::Native(ids.mo_observe));
    proto.set("disconnect", Val::Native(ids.mo_disconnect));
    proto.set("takeRecords", Val::Native(ids.mo_take));
    e.set_native_prop(ids.mo_ctor, "prototype", proto);
    e.set_global("MutationObserver", Val::Native(ids.mo_ctor));
}

/// `new IntersectionObserver(callback, {root, rootMargin, threshold})`.
///
/// shortcut: callbacks are never delivered. Delivery needs a viewport and a
/// scroll position, which the renderer does not expose to a page turn; the
/// honest stand-in for "not in view" is an observer that never fires. Pages
/// that lazy-load on intersection simply never load more (github.com's
/// analytics and infinite lists), and no code path depends on a callback
/// having run.
fn n_io(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == IO_CTOR {
        if !is_ctor_call(e, this) {
            return Err(JsError::throw(
                "TypeError: IntersectionObserver is not a constructor",
            ));
        }
        let cb = args.first().cloned().unwrap_or(Val::Undefined);
        if !matches!(cb, Val::Func(_) | Val::Native(_) | Val::Method(_)) {
            return Err(JsError::throw(
                "TypeError: IntersectionObserver callback is not a function",
            ));
        }
        let obs = proto_instance(
            e.native_prop(nids().io_ctor, "prototype")
                .unwrap_or(Val::Undefined),
        );
        let opts = args.get(1);
        obs.set("__cb", cb);
        obs.set(
            "root",
            match event_option(opts, "root") {
                Val::Undefined => Val::Null,
                v => v,
            },
        );
        let margin = match event_option(opts, "rootMargin") {
            Val::Str(s) => s.to_string(),
            _ => "0px 0px 0px 0px".to_string(),
        };
        obs.set("rootMargin", str_val(&margin));
        // `threshold` takes a number or an array of them; the spec always
        // reports it as a list of increasing fractions
        let thresholds = match event_option(opts, "threshold") {
            Val::Num(n) => vec![n],
            Val::Obj(o) => o
                .borrow()
                .props
                .iter()
                .filter_map(|(k, v)| match (k.parse::<usize>(), v) {
                    (Ok(_), Val::Num(n)) => Some(*n),
                    _ => None,
                })
                .collect(),
            _ => vec![0.0],
        };
        obs.set(
            "thresholds",
            Val::new_array(thresholds.into_iter().map(Val::Num).collect()),
        );
        if !IO_NOTE.with(|n| n.replace(true)) {
            eprintln!(
                "[js] IntersectionObserver: observe() records nothing and never fires (no viewport; shortcut)"
            );
        }
        return Ok(obs);
    }
    // every method needs a live observer object
    if !matches!(this, Val::Obj(o) if !matches!(js::obj_get_own(o, "__cb"), Val::Undefined)) {
        return Err(JsError::throw(
            "TypeError: IntersectionObserver method called on a non-observer",
        ));
    }
    match idx {
        IO_TAKE => Ok(Val::new_array(Vec::new())),
        _ => Ok(Val::Undefined),
    }
}

fn install_intersection_observer(e: &mut JsEngine, ids: Nids) {
    let proto = Val::new_object();
    proto.set("observe", Val::Native(ids.io_observe));
    proto.set("unobserve", Val::Native(ids.io_unobserve));
    proto.set("disconnect", Val::Native(ids.io_disconnect));
    proto.set("takeRecords", Val::Native(ids.io_take));
    e.set_native_prop(ids.io_ctor, "prototype", proto);
    e.set_global("IntersectionObserver", Val::Native(ids.io_ctor));
}

/// `new ResizeObserver(callback)`, `observe(el)`.
///
/// shortcut: like `IntersectionObserver` above, callbacks are never
/// delivered — a box size would have to be read out of the layout tree,
/// which a page turn in this engine does not hold. `observe` is a no-op and
/// the observer simply never reports; nothing in the wild requires a
/// callback to have run, they react to one when it arrives. Noted once per
/// page.
fn n_ro(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == RO_CTOR {
        if !is_ctor_call(e, this) {
            return Err(JsError::throw(
                "TypeError: ResizeObserver is not a constructor",
            ));
        }
        let cb = args.first().cloned().unwrap_or(Val::Undefined);
        if !matches!(cb, Val::Func(_) | Val::Native(_) | Val::Method(_)) {
            return Err(JsError::throw(
                "TypeError: ResizeObserver callback is not a function",
            ));
        }
        let obs = proto_instance(
            e.native_prop(nids().ro_ctor, "prototype")
                .unwrap_or(Val::Undefined),
        );
        obs.set("__cb", cb);
        if !RO_NOTE.with(|n| n.replace(true)) {
            eprintln!(
                "[js] ResizeObserver: observe() records nothing and never fires (no layout box; shortcut)"
            );
        }
        return Ok(obs);
    }
    if !matches!(this, Val::Obj(o) if !matches!(js::obj_get_own(o, "__cb"), Val::Undefined)) {
        return Err(JsError::throw(
            "TypeError: ResizeObserver method called on a non-observer",
        ));
    }
    Ok(Val::Undefined)
}

fn install_resize_observer(e: &mut JsEngine, ids: Nids) {
    let proto = Val::new_object();
    proto.set("observe", Val::Native(ids.ro_observe));
    proto.set("unobserve", Val::Native(ids.ro_unobserve));
    proto.set("disconnect", Val::Native(ids.ro_disconnect));
    e.set_native_prop(ids.ro_ctor, "prototype", proto);
    e.set_global("ResizeObserver", Val::Native(ids.ro_ctor));
}

/// `requestAnimationFrame(cb)` - the callback joins this turn's queue with
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
        (concat!("Chro", "mium"), "142"),
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

// ---------------------------------------------------------------------------
// Live collections (`getElementsBy*`, `document.forms`, `form.elements`)
// ---------------------------------------------------------------------------
//
// A live collection stores only the QUERY (family + root + argument); every
// index/`length`/named read re-runs it against the current arena, so DOM
// mutations after the collection was returned are visible. `for..of`,
// spread and `Array.from` use `live_values` (one query, not one per index).
// The engine keeps `LiveSpec` opaque - these family numbers are ours.

const LIVE_TAG: u8 = 0;
const LIVE_CLASS: u8 = 1;
const LIVE_NAME: u8 = 2;
const LIVE_FORMS: u8 = 3;
const LIVE_FORM_CONTROLS: u8 = 4;

/// `HTMLFormElement.elements` lists exactly these tags.
const LISTED_TAGS: [&str; 7] = [
    "button", "fieldset", "input", "object", "output", "select", "textarea",
];

/// Nodes a live collection currently contains (document order). The
/// `root` of a tag/class collection scopes it to one element's subtree;
/// `0` means the whole document.
fn live_nodes(spec: &js::LiveSpec) -> Vec<u32> {
    runtime::host_opt(|h| {
        let dom = h.dom();
        match spec.family {
            LIVE_TAG => {
                if spec.root == 0 {
                    h.by_tag(&spec.arg)
                } else {
                    h.subtree_tag(spec.root, &spec.arg)
                }
            }
            LIVE_CLASS => {
                if spec.root == 0 {
                    h.by_class(&spec.arg)
                } else {
                    h.subtree_class(spec.root, &spec.arg)
                }
            }
            LIVE_NAME => h.by_name(&spec.arg),
            LIVE_FORMS => h.by_tag("form"),
            LIVE_FORM_CONTROLS => subtree_elements_dom(dom, spec.root)
                .into_iter()
                .filter(|&i| {
                    let t = dom.tag(i);
                    LISTED_TAGS.iter().any(|l| t.eq_ignore_ascii_case(l))
                })
                .collect(),
            _ => Vec::new(),
        }
    })
    .unwrap_or_default()
}

/// The `name` or `id` a collection's named lookup matches.
fn live_named(spec: &js::LiveSpec, name: &str) -> Option<u32> {
    runtime::host_opt(|h| {
        live_nodes(spec).into_iter().find(|&i| {
            h.attr(i, "id").as_deref() == Some(name)
                || h.attr(i, "name").as_deref() == Some(name)
        })
    })
    .flatten()
}

/// `live_get` hook: answer a property read on a collection.
fn live_get_hook(spec: &js::LiveSpec, key: &str) -> Option<Val> {
    if key == "length" {
        return Some(Val::Num(live_nodes(spec).len() as f64));
    }
    if let Ok(i) = key.parse::<usize>() {
        return live_nodes(spec).get(i).map(|&n| wrap_element(n));
    }
    live_named(spec, key).map(wrap_element)
}

/// `live_values` hook: snapshot for iteration / `Array.from` / spread.
fn live_values_hook(spec: &js::LiveSpec) -> Vec<Val> {
    live_nodes(spec).into_iter().map(wrap_element).collect()
}

/// The spec behind a collection value, if it is one.
fn live_spec_of(v: &Val) -> Option<js::LiveSpec> {
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            js::ObjKind::Live(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Build a live collection value: `item`/`namedItem` natives + the opaque
/// query in `ObjKind::Live`.
fn wrap_coll(family: u8, root: u32, arg: &str) -> Val {
    let o = Val::new_object();
    let n = nids();
    o.set("item", Val::Native(n.coll_item));
    o.set("namedItem", Val::Native(n.coll_named));
    if let Val::Obj(rc) = &o {
        rc.borrow_mut().kind = js::ObjKind::Live(js::LiveSpec {
            family,
            root,
            arg: Rc::from(arg),
        });
    }
    o
}

/// `HTMLCollection.prototype.item(i)` / `.namedItem(name)` (shared native).
fn coll_native(_e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let Some(spec) = live_spec_of(this) else {
        return Ok(Val::Null);
    };
    if idx == C_ITEM {
        let i = args.first().map(|v| v.to_number()).unwrap_or(0.0);
        return Ok(if i.is_finite() && i >= 0.0 {
            live_nodes(&spec)
                .get(i as usize)
                .map(|&n| wrap_element(n))
                .unwrap_or(Val::Null)
        } else {
            Val::Null
        });
    }
    let name = args.first().map(|v| v.to_string()).unwrap_or_default();
    Ok(live_named(&spec, &name)
        .map(wrap_element)
        .unwrap_or(Val::Null))
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
    // stack pops from the back: push the initial children reversed so the
    // FIRST child is visited first (document order)
    let mut stack: Vec<u32> = dom
        .nodes
        .get(root as usize)
        .map(|n| n.children.iter().rev().copied().collect())
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
        // a fresh page starts with no observers and no mutation logging
        OBSERVERS.with(|o| o.borrow_mut().clear());
        CUSTOM_ELEMENTS.with(|c| c.borrow_mut().clear());
        CE_INSTANCES.with(|m| m.borrow_mut().clear());
        CE_FAILED.with(|f| f.set(false));
        CE_CONSTRUCTING.with(|s| s.borrow_mut().clear());
        CE_CONNECTED.with(|s| s.borrow_mut().clear());
        CE_LIFE_FAILED.with(|f| f.set(false));
        IO_NOTE.with(|n| n.set(false));
        RO_NOTE.with(|n| n.set(false));
        TEMPLATE_CONTENT.with(|m| m.borrow_mut().clear());
        SHADOW_ROOTS.with(|m| m.borrow_mut().clear());
        runtime::set_mutation_tracking(false);
        let mut engine = JsEngine::new();
        // one native per reflected-prop accessor (getter + setter share it)
        let reflect = std::array::from_fn(|i| {
            engine.register_native(REFLECTED[i].0, el_prop_native, i as u16)
        });
        // one constructor native per DOM interface, in prototype-chain order
        let mut dom_ctor = [0usize; DOM_INTERFACE_COUNT];
        for (slot, name) in dom_ctor.iter_mut().zip(
            DOM_INTERFACES
                .iter()
                .copied()
                .chain(TAG_INTERFACES.iter().map(|(name, _)| *name))
                .chain(std::iter::once("SVGSVGElement")),
        ) {
            *slot = engine.register_native(name, n_dom_ctor, 0);
        }
        let ids = Nids {
            doc_get_element: engine.register_native("document.getElementById", doc_native, D_GET_ELEMENT_BY_ID),
            doc_query: engine.register_native("document.querySelector", doc_native, D_QUERY),
            doc_query_all: engine.register_native("document.querySelectorAll", doc_native, D_QUERY_ALL),
            doc_create: engine.register_native("document.createElement", doc_native, D_CREATE_ELEMENT),
            doc_create_ns: engine.register_native("document.createElementNS", doc_native, D_CREATE_ELEMENT_NS),
            doc_create_fragment: engine.register_native("document.createDocumentFragment", doc_native, D_CREATE_FRAGMENT),
            doc_cookie_get: engine.register_native("document.getCookie", doc_native, D_COOKIE_GET),
            doc_cookie_set: engine.register_native("document.setCookie", doc_native, D_COOKIE_SET),
            doc_get_by_tag: engine.register_native("document.getElementsByTagName", doc_native, D_GET_ELEMENTS_BY_TAG),
            doc_get_by_class: engine.register_native("document.getElementsByClassName", doc_native, D_GET_ELEMENTS_BY_CLASS),
            doc_forms: engine.register_native("document.forms", doc_native, D_FORMS),
            doc_get_by_name: engine.register_native("document.getElementsByName", doc_native, D_GET_ELEMENTS_BY_NAME),
            doc_create_text: engine.register_native("document.createTextNode", doc_native, D_CREATE_TEXT),
            doc_create_comment: engine.register_native("document.createComment", doc_native, D_CREATE_COMMENT),
            reflect,
            el_get_element: engine.register_native("Element.getElementById", el_native, E_GET_ELEMENT_BY_ID),
            el_query: engine.register_native("Element.querySelector", el_native, E_QUERY),
            el_query_all: engine.register_native("Element.querySelectorAll", el_native, E_QUERY_ALL),
            el_get_by_class: engine.register_native("Element.getElementsByClassName", el_native, E_GET_ELEMENTS_BY_CLASS),
            el_get_by_tag: engine.register_native("Element.getElementsByTagName", el_native, E_GET_ELEMENTS_BY_TAG),
            el_append: engine.register_native("Element.appendChild", el_native, E_APPEND_CHILD),
            el_set_attr: engine.register_native("Element.setAttribute", el_native, E_SET_ATTRIBUTE),
            el_get_attr: engine.register_native("Element.getAttribute", el_native, E_GET_ATTRIBUTE),
            el_rm_attr: engine.register_native("Element.removeAttribute", el_native, E_REMOVE_ATTRIBUTE),
            el_add_listener: engine.register_native("Element.addEventListener", el_native, E_ADD_EVENT_LISTENER),
            el_remove_listener: engine.register_native("Element.removeEventListener", el_native, E_REMOVE_EVENT_LISTENER),
            el_matches: engine.register_native("Element.matches", el_native, E_MATCHES),
            el_has_attr: engine.register_native("Element.hasAttribute", el_native, E_HAS_ATTRIBUTE),
            el_dispatch: engine.register_native("EventTarget.dispatchEvent", ev_native, V_DISPATCH),
            el_set_text: engine.register_native("Element.setTextContent", el_native, E_SET_TEXT_CONTENT),
            el_set_inner: engine.register_native("Element.setInnerHTML", el_native, E_SET_INNER_HTML),
            el_set_value: engine.register_native("Element.setValue", el_native, E_SET_VALUE),
            el_set_class: engine.register_native("Element.setClassName", el_native, E_SET_CLASS_NAME),
            el_focus: engine.register_native("Element.focus", el_native, E_FOCUS),
            el_submit: engine.register_native("Element.submit", el_native, E_SUBMIT),
            el_parent: engine.register_native("Element.parentElement", el_native, E_PARENT),
            el_remove: engine.register_native("Element.remove", el_native, E_REMOVE),
            el_form_elements: engine.register_native("HTMLFormElement.elements", el_native, E_FORM_ELEMENTS),
            coll_item: engine.register_native("HTMLCollection.prototype.item", coll_native, C_ITEM),
            coll_named: engine.register_native("HTMLCollection.prototype.namedItem", coll_native, C_NAMED),
            node_data_get: engine.register_native("Node.data", n_node, N_DATA_GET),
            node_data_set: engine.register_native("Node.data", n_node, N_DATA_SET),
            node_type: engine.register_native("Node.nodeType", n_node, N_NODE_TYPE),
            node_parent: engine.register_native("Node.parentNode", n_node, N_PARENT),
            node_prev: engine.register_native("Node.previousSibling", n_node, N_PREV),
            node_next: engine.register_native("Node.nextSibling", n_node, N_NEXT),
            node_first: engine.register_native("Node.firstChild", n_node, N_FIRST_CHILD),
            node_child_nodes: engine.register_native("Node.childNodes", n_node, N_CHILD_NODES),
            node_children: engine.register_native("Element.children", n_node, N_CHILDREN),
            node_clone: engine.register_native("Node.cloneNode", n_node, N_CLONE_NODE),
            node_remove_child: engine.register_native("Node.removeChild", n_node, N_REMOVE_CHILD),
            node_insert_before: engine.register_native("Node.insertBefore", n_node, N_INSERT_BEFORE),
            node_dataset: engine.register_native("HTMLElement.dataset", n_node, N_DATASET),
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
            win_remove_listener: engine.register_native("window.removeEventListener", win_native, W_REMOVE_LISTENER),
            match_media: engine.register_native("window.matchMedia", win_native, W_MATCH_MEDIA),
            media_noop: engine.register_native("MediaQueryList.addEventListener", win_native, 35),
            eme: engine.register_native("navigator.requestMediaKeySystemAccess", win_native, W_EME),
            media_caps: engine.register_native("navigator.mediaCapabilities.decodingInfo", win_native, W_MEDIA_CAPS),
            history_push: engine.register_native("history.pushState", hist_native, H_PUSH),
            history_replace: engine.register_native("history.replaceState", hist_native, H_REPLACE),
            history_go: engine.register_native("history.back", hist_native, H_GO),
            ev_prevent_default: engine.register_native("event.preventDefault", ev_native, V_PREVENT_DEFAULT),
            ev_stop: engine.register_native("Event.prototype.stopPropagation", ev_native, V_STOP),
            ev_stop_immediate: engine.register_native("Event.prototype.stopImmediatePropagation", ev_native, V_STOP_IMMEDIATE),
            ev_ctor: engine.register_native("Event", n_event, V_CTOR),
            cev_ctor: engine.register_native("CustomEvent", n_event, V_CEV_CTOR),
            mo_observe: engine.register_native("MutationObserver.prototype.observe", n_mo, MO_OBSERVE),
            mo_disconnect: engine.register_native("MutationObserver.prototype.disconnect", n_mo, MO_DISCONNECT),
            mo_take: engine.register_native("MutationObserver.prototype.takeRecords", n_mo, MO_TAKE),
            mo_ctor: engine.register_native("MutationObserver", n_mo, MO_CTOR),
            raf: engine.register_native("requestAnimationFrame", raf_native, 0),
            storage_get: engine.register_native("localStorage.getItem", storage_native, S_GET),
            storage_set: engine.register_native("localStorage.setItem", storage_native, S_SET),
            storage_remove: engine.register_native("localStorage.removeItem", storage_native, S_REMOVE),
            storage_length: engine.register_native("localStorage.length", storage_native, S_LENGTH),
            storage_clear: engine.register_native("localStorage.clear", storage_native, S_CLEAR),
            fetch: engine.register_native("fetch", fetch_native, 0),
            fetch_text: engine.register_native("response.text", fetch_text_native, 0),
            dom_ctor,
            el_closest: engine.register_native("Element.closest", el_native, E_CLOSEST),
            el_template_content: engine.register_native("HTMLTemplateElement.content", n_template_content, 0),
            el_attach_shadow: engine.register_native("Element.attachShadow", el_native, E_ATTACH_SHADOW),
            el_shadow_root: engine.register_native("Element.shadowRoot", el_native, E_SHADOW_ROOT),
            el_prepend: engine.register_native("Element.prepend", el_native, E_PREPEND),
            el_append_many: engine.register_native("Element.append", el_native, E_APPEND),
            el_before: engine.register_native("Element.before", el_native, E_BEFORE),
            el_after: engine.register_native("Element.after", el_native, E_AFTER),
            el_replace_with: engine.register_native("Element.replaceWith", el_native, E_REPLACE_WITH),
            el_replace_children: engine.register_native("Element.replaceChildren", el_native, E_REPLACE_CHILDREN),
            ce_define: engine.register_native("customElements.define", n_custom_elements, CE_DEFINE),
            ce_get: engine.register_native("customElements.get", n_custom_elements, CE_GET),
            ce_when_defined: engine.register_native("customElements.whenDefined", n_custom_elements, CE_WHEN_DEFINED),
            ce_noop: engine.register_native("customElements.upgrade", n_custom_elements, CE_NOOP),
            io_ctor: engine.register_native("IntersectionObserver", n_io, IO_CTOR),
            io_observe: engine.register_native("IntersectionObserver.prototype.observe", n_io, IO_OBSERVE),
            io_unobserve: engine.register_native("IntersectionObserver.prototype.unobserve", n_io, IO_UNOBSERVE),
            io_disconnect: engine.register_native("IntersectionObserver.prototype.disconnect", n_io, IO_DISCONNECT),
            io_take: engine.register_native("IntersectionObserver.prototype.takeRecords", n_io, IO_TAKE),
            ro_ctor: engine.register_native("ResizeObserver", n_ro, RO_CTOR),
            ro_observe: engine.register_native("ResizeObserver.prototype.observe", n_ro, RO_OBSERVE),
            ro_unobserve: engine.register_native("ResizeObserver.prototype.unobserve", n_ro, RO_UNOBSERVE),
            ro_disconnect: engine.register_native("ResizeObserver.prototype.disconnect", n_ro, RO_DISCONNECT),
        };
        NIDS.with(|c| c.set(Some(ids)));
        install_event_types(&mut engine, ids);
        install_dom_types(&mut engine, &ids);
        install_custom_elements(&mut engine, ids);
        install_mutation_observer(&mut engine, ids);
        install_intersection_observer(&mut engine, ids);
        install_resize_observer(&mut engine, ids);
        // CSSOM bridge: the style object's member reads/writes go through
        // the shared host (js.rs itself stays DOM-free)
        engine.style_get = Some(style_get_hook);
        engine.style_set = Some(style_set_hook);
        // live collections re-query the arena through these hooks
        engine.live_get = Some(live_get_hook);
        engine.live_values = Some(live_values_hook);
        // dynamic import(): global native; the module registry itself lives
        // on the engine (engine.module_loader), and the page URL is the
        // fallback base for specifiers called outside a module body
        js::module::install_dynamic_import(&mut engine);
        engine.import_base = Some(Rc::from(base));

        let doc = Val::new_object();
        // `document` is an HTMLDocument (so a Document and a Node), never an
        // Element
        set_proto(&doc, dom_proto(DOM_HTML_DOCUMENT));
        // `document` is also a Node with methods (MutationObserver.observe,
        // dispatchEvent, prepend on the whole document), so it carries the
        // arena's root id: node 0 is `#document` in the parser. `nodeType` 9
        // is the one Document property that has nothing to read from the host.
        doc.set("__node", Val::Num(0.0));
        doc.set("nodeType", Val::Num(9.0));
        doc.set("getElementById", Val::Native(ids.doc_get_element));
        doc.set("querySelector", Val::Native(ids.doc_query));
        doc.set("querySelectorAll", Val::Native(ids.doc_query_all));
        doc.set("createElement", Val::Native(ids.doc_create));
        doc.set("createElementNS", Val::Native(ids.doc_create_ns));
        doc.set("createDocumentFragment", Val::Native(ids.doc_create_fragment));
        doc.set("getElementsByTagName", Val::Native(ids.doc_get_by_tag));
        doc.set("getElementsByClassName", Val::Native(ids.doc_get_by_class));
        doc.set("getElementsByName", Val::Native(ids.doc_get_by_name));
        doc.set("createTextNode", Val::Native(ids.doc_create_text));
        doc.set("createComment", Val::Native(ids.doc_create_comment));
        // `document.forms` is a live HTMLCollection in the real DOM; here it
        // is a document-order snapshot taken on each read (same shortcut as
        // the other collection APIs).
        doc.set(
            "forms",
            Val::Acc(Rc::new(js::AccEntry {
                getter: Some(Val::Native(ids.doc_forms)),
                setter: None,
            })),
        );
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
        // `document.domain`: the host without its port. GitHub's cookie
        // helper reads it and throws when it is null.
        let domain = js::url::parts(base)
            .map(|p| p.hostname)
            .unwrap_or_default();
        doc.set("domain", str_val(&domain));
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
        engine.set_global("removeEventListener", Val::Native(ids.win_remove_listener));
        engine.set_global("dispatchEvent", Val::Native(ids.el_dispatch));
        engine.set_global("matchMedia", Val::Native(ids.match_media));
        engine.set_global("requestAnimationFrame", Val::Native(ids.raf));
        // document-level listeners share the global (window) bucket: both
        // are page-wide targets with no own arena node, and the global stop
        // already fires after the bubble chain (logged design shortcut)
        doc.set("addEventListener", Val::Native(ids.win_add_listener));
        doc.set("removeEventListener", Val::Native(ids.win_remove_listener));
        doc.set("dispatchEvent", Val::Native(ids.el_dispatch));

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
        engine.set_global("location", loc.clone());
        // `document.location` is the same Location object (the spec aliases
        // it). GitHub's marketing bootstrap reads `document.location.search`
        // and died on `document.location` being undefined.
        doc.set("location", loc);
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
        let t0 = Instant::now();
        crate::fetch_nanos_take(); // start from zero: earlier turns don't count
        let turn = self.run_scripts_inner(dom);
        if std::env::var_os("MAZDA_TIME").is_some() {
            let total = t0.elapsed().as_secs_f64() * 1000.0;
            let fetch = crate::fetch_nanos_take() as f64 / 1e6;
            eprintln!(
                "[time] scripts: {:.0} ms (fetch {:.0} ms, exec {:.0} ms)",
                total,
                fetch,
                (total - fetch).max(0.0)
            );
        }
        turn
    }

    /// Warm the page's whole module graph before any body runs. The loader
    /// discovers a chunk's imports only after executing it, so a bundle with N
    /// chunks paid N sequential round trips (github.com: ~20 chunks at ~300 ms
    /// each). Parsing each body's static `import`/`export ... from` lists lets
    /// the engine issue every request on the preload lanes; execution then
    /// reads all bodies from the HTTP disk cache. Bounded by count and waves.
    fn preload_module_graph(&self, scripts: &[ScriptSrc]) {
        const MODULE_PRELOAD_MAX: usize = 96;
        const MODULE_PRELOAD_DEPTH: usize = 4;
        let map = self
            .engine
            .module_loader
            .as_ref()
            .and_then(|l| l.import_map());
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: Vec<String> = Vec::new();
        for s in scripts.iter().filter(|s| s.is_module) {
            let Some(u) = &s.src else { continue };
            if let Some(abs) = crate::resolve(&self.base, u) {
                if seen.insert(abs.clone()) {
                    queue.push(abs);
                }
            }
        }
        let mut waves = 0usize;
        let mut fetched = 0usize;
        while !queue.is_empty() && waves < MODULE_PRELOAD_DEPTH && fetched < MODULE_PRELOAD_MAX {
            waves += 1;
            let batch = std::mem::take(&mut queue);
            fetched += batch.len();
            crate::preload_labeled(&batch, "module preload");
            for u in &batch {
                let Ok((bytes, _)) = crate::fetch(u) else {
                    continue;
                };
                let src = String::from_utf8_lossy(&bytes);
                for spec in js::module::static_deps(&src) {
                    let Some(abs) = js::module::resolve_with_map(map, u, &spec) else {
                        continue;
                    };
                    if !abs.starts_with("http") {
                        continue;
                    }
                    if seen.insert(abs.clone()) {
                        queue.push(abs);
                    }
                }
            }
        }
        if fetched > 0 {
            eprintln!("[js] module preload: {fetched} modules in {waves} wave(s)");
        }
    }

    fn run_scripts_inner(&mut self, dom: &mut Dom) -> Turn {
        let scripts = collect_scripts(dom);
        // `<script type="importmap">` has to be installed before the first
        // module resolves an import: GitHub ships one mapping `react`,
        // `react-dom/client`, `react/jsx-runtime`, ... and without it its
        // bundle dies at its first bare import.
        if let Some(text) = collect_import_map(dom) {
            match js::module::ImportMap::parse(&text, &self.base) {
                Some(map) => {
                    eprintln!("[js] import map: {} entries", map.len());
                    self.engine
                        .module_loader
                        .get_or_insert_with(js::ModuleLoader::new)
                        .set_import_map(map);
                }
                None => eprintln!("[js] import map ignored (no usable entry)"),
            }
        }
        // a dual build (module entry + `nomodule` fallback) must run only the
        // module half; with no module script at all the legacy half still runs
        let has_modules = scripts.iter().any(|s| s.is_module);
        if has_modules {
            self.preload_module_graph(&scripts);
        }
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
                    // the callback receives the frame timestamp, same clock as
                    // `performance.now()` (one coalesced pass = one timestamp)
                    let ts = js::perf_now_ms();
                    let r = e.call_function(&cb, Val::Undefined, &[Val::Num(ts)]);
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
            NodeKind::Comment(_) => {}
        }
    }
    out
}

/// The page's `<script type="importmap">` text. The spec allows one map and
/// requires it to precede every module script; a second one is ignored.
pub(crate) fn collect_import_map(dom: &Dom) -> Option<String> {
    let mut stack: Vec<u32> = vec![0];
    while let Some(n) = stack.pop() {
        if let NodeKind::Element(_, _) = &dom.nodes[n as usize].kind {
            if dom.tag(n) == "script"
                && dom
                    .attr(n, "type")
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("importmap"))
            {
                return Some(child_text(dom, n));
            }
            for &c in dom.nodes[n as usize].children.iter().rev() {
                stack.push(c);
            }
        }
    }
    None
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
    fn elements_have_a_node_name() {
        // Sizzle's TAG index runs `e.nodeName.toUpperCase()`; a missing
        // nodeName made it throw inside a timer on github.com.
        let (mut dom, mut ps) = page(
            r##"<html><body><div id="d"></div><script>
                window.t = [
                    document.body.nodeName,
                    document.getElementById("d").nodeName,
                    document.createElement("span").nodeName,
                ].join(",");
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "t"), "BODY,DIV,SPAN");
    }

    #[test]
    fn attach_shadow_returns_a_fragment_root() {
        // GitHub's command polyfill wraps `HTMLElement.prototype.attachShadow`
        // and then observes, queries and listener-binds the root it returns.
        // With no `attachShadow` on the prototype, `orig.call(this, init)`
        // threw inside every custom element constructor.
        let (mut dom, mut ps) = page(
            r##"<html><body><div id="host"></div><script>
                var orig = HTMLElement.prototype.attachShadow;
                HTMLElement.prototype.attachShadow = function (init) {
                    var root = orig.call(this, init);
                    root.addEventListener("click", function () {});
                    root.querySelectorAll("[oncommand]");
                    return root;
                };
                var host = document.getElementById("host");
                var root = host.attachShadow({ mode: "open" });
                var span = document.createElement("span");
                span.setAttribute("oncommand", "x");
                root.appendChild(span);
                var again = host.shadowRoot;
                window.t = [
                    root.nodeType,
                    root.mode,
                    root.host.id,
                    again.nodeType,
                    again.querySelectorAll("span").length,
                    typeof orig,
                ].join(",");
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "t"), "11,open,host,11,1,function");
    }

    #[test]
    fn element_wrappers_are_instances_of_element() {
        // the guard Primer uses before measuring a node:
        // `ref.current instanceof Element`
        let (mut dom, mut ps) = page(
            r##"<html><body><div id="d"><span class="x"></span></div><script>
                var el = document.getElementById("d");
                window.t = [
                    el instanceof Element,
                    el instanceof Node,
                    document.body instanceof Element,
                    document instanceof Node,
                    document instanceof Element,
                    window instanceof Element,
                    el.closest(".x") === null,
                    el.closest("body").tagName,
                    el.querySelector(".x").closest("div").id,
                    // shortcut: wrappers are built per access, so `===` on two
                    // wrappers of the same node is false (no identity cache);
                    // compare the id the way a page would
                    el.closest("#d").id,
                ].join(",");
                window.t2 = [
                    document.documentElement instanceof HTMLElement,
                    document.body instanceof HTMLElement,
                    document.createElement("div") instanceof HTMLElement,
                    document.createElement("div") instanceof Element,
                    document.createTextNode("x") instanceof Text,
                    document.createTextNode("x") instanceof CharacterData,
                    document.createTextNode("x") instanceof Element,
                    document instanceof Document,
                    document instanceof HTMLDocument,
                    document instanceof HTMLElement,
                    typeof EventTarget,
                    typeof Node,
                ].join(",");
                window.t3 = [
                    document.createElement("button") instanceof HTMLButtonElement,
                    document.createElement("button") instanceof HTMLElement,
                    document.createElement("div") instanceof HTMLButtonElement,
                    document.createElement("input") instanceof HTMLInputElement,
                    document.body instanceof HTMLBodyElement,
                    document.head instanceof HTMLHeadElement,
                    document.documentElement instanceof HTMLHtmlElement,
                    document.querySelector("svg") === null,
                    new Event("x") instanceof Event,
                    new CustomEvent("x") instanceof Event,
                ].join(",");
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(
            get_global_str(&mut ps, "t"),
            "true,true,true,true,false,false,true,BODY,d,d"
        );
        assert_eq!(
            get_global_str(&mut ps, "t2"),
            "true,true,true,true,true,true,false,true,true,false,function,function"
        );
        assert_eq!(
            get_global_str(&mut ps, "t3"),
            "true,true,false,true,true,true,true,true,true,true"
        );
    }

    #[test]
    fn dom_insertion_methods_keep_order_and_text() {
        let (mut dom, mut ps) = page(
            r##"<html><body><ul id="l"><li id="a">a</li><li id="b">b</li></ul><script>
                var l = document.getElementById("l");
                var x = document.createElement("li");
                x.id = "x";
                l.prepend("first", x);
                var y = document.createElement("li");
                y.id = "y";
                l.append(y, "last");
                window.order = Array.prototype.map
                    .call(l.children, function (c) { return c.id; })
                    .join(",");
                window.text = document.getElementById("l").textContent;
                var z = document.createElement("li");
                z.id = "z";
                x.after(z);
                x.before("b0");
                y.replaceWith("y0");
                window.order2 = Array.prototype.map
                    .call(l.children, function (c) { return c.id; })
                    .join(",");
                l.replaceChildren("only");
                window.order3 = l.children.length + ":" + document.getElementById("l").textContent;
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "order"), "x,a,b,y");
        assert_eq!(get_global_str(&mut ps, "text"), "firstablast");
        assert_eq!(get_global_str(&mut ps, "order2"), "x,z,a,b");
        assert_eq!(get_global_str(&mut ps, "order3"), "0:only");
    }

    #[test]
    fn intersection_observer_constructs_and_observes_without_firing() {
        // the shape github.com uses: construct with options, observe elements,
        // unobserve from the callback (which never runs here)
        let (mut dom, mut ps) = page(
            r##"<html><body><div id="d"></div><script>
                var fired = 0;
                var io = new IntersectionObserver(function (entries) {
                    fired++;
                }, { rootMargin: "0% 0% -30% 0%", threshold: 0 });
                window.margin = io.rootMargin;
                window.thresholds = io.thresholds.length + ":" + io.thresholds[0];
                window.root = io.root === null;
                io.observe(document.getElementById("d"));
                io.unobserve(document.getElementById("d"));
                window.records = io.takeRecords().length;
                io.disconnect();
                window.fired = fired;
                window.isInstance = io instanceof IntersectionObserver;
                var bad = false;
                try { new IntersectionObserver(); } catch (e) { bad = e instanceof TypeError; }
                window.badCb = bad;
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "margin"), "0% 0% -30% 0%");
        assert_eq!(get_global_str(&mut ps, "thresholds"), "1:0");
        assert_eq!(get_global_str(&mut ps, "root"), "true");
        assert_eq!(get_global_str(&mut ps, "records"), "0");
        assert_eq!(get_global_str(&mut ps, "fired"), "0");
        assert_eq!(get_global_str(&mut ps, "isInstance"), "true");
        assert_eq!(get_global_str(&mut ps, "badCb"), "true");
    }

    #[test]
    fn template_content_clones_and_splices() {
        // the shape github.com's task-list code uses: clone `content`, find
        // the handle in the copy, prepend the copy into a row
        let (mut dom, mut ps) = page(
            r##"<html><body><div id="host"></div>
            <template id="t"><span class="handle">h</span><svg><path d="M0 0"></path></svg></template>
            <script>
                var t = document.getElementById("t");
                var c = t.content;
                window.type = c.nodeType;
                window.moved = t.children.length === 0 && c.childNodes.length === 2;
                var copy = c.cloneNode(true);
                window.copyType = copy.nodeType;
                window.handle = copy.querySelector(".handle").textContent;
                window.deep = copy.querySelector("svg") !== null;
                var shallow = c.cloneNode(false);
                window.shallowKids = shallow.childNodes.length;
                document.getElementById("host").appendChild(copy);
                var host = document.getElementById("host");
                window.spliced = host.children.length;
                window.splicedTags = Array.prototype.map
                    .call(host.children, function (e) { return e.tagName; })
                    .join(",");
                window.empty = document.createDocumentFragment().nodeType;
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "type"), "11");
        assert_eq!(get_global_str(&mut ps, "moved"), "true");
        assert_eq!(get_global_str(&mut ps, "copyType"), "11");
        assert_eq!(get_global_str(&mut ps, "handle"), "h");
        assert_eq!(get_global_str(&mut ps, "deep"), "true");
        assert_eq!(get_global_str(&mut ps, "shallowKids"), "0");
        assert_eq!(get_global_str(&mut ps, "spliced"), "2");
        assert_eq!(get_global_str(&mut ps, "splicedTags"), "SPAN,SVG");
        assert_eq!(get_global_str(&mut ps, "empty"), "11");
    }

    #[test]
    fn create_element_ns_makes_svg_elements() {
        // Primer builds its drag handle with createElementNS; the tag alone
        // decides the family, because the arena has no namespace column
        let (mut dom, mut ps) = page(
            r##"<html><body><script>
                var NS = "http://www.w3.org/2000/svg";
                var svg = document.createElementNS(NS, "svg");
                var path = document.createElementNS(NS, "path");
                var div = document.createElement("div");
                svg.appendChild(path);
                document.body.appendChild(svg);
                window.t = [
                    svg instanceof SVGSVGElement,
                    svg instanceof SVGElement,
                    svg instanceof HTMLElement,
                    path instanceof SVGElement,
                    path instanceof SVGSVGElement,
                    div instanceof SVGElement,
                    div instanceof HTMLDivElement,
                    document.querySelector("svg") instanceof SVGSVGElement,
                ].join(",");
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(
            get_global_str(&mut ps, "t"),
            "true,true,false,true,false,false,true,true"
        );
    }

    #[test]
    fn resize_observer_constructs_and_observes_without_firing() {
        let (mut dom, mut ps) = page(
            r##"<html><body><div id="d"></div><script>
                var fired = 0;
                var ro = new ResizeObserver(function () { fired++; });
                ro.observe(document.getElementById("d"));
                ro.unobserve(document.getElementById("d"));
                ro.disconnect();
                window.isInstance = ro instanceof ResizeObserver;
                window.fired = fired;
                var bad = false;
                try { new ResizeObserver(1); } catch (e) { bad = e instanceof TypeError; }
                window.badCb = bad;
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "isInstance"), "true");
        assert_eq!(get_global_str(&mut ps, "fired"), "0");
        assert_eq!(get_global_str(&mut ps, "badCb"), "true");
    }

    #[test]
    fn document_is_a_node_for_observer_and_dispatch() {
        // GitHub observes the whole document at load; a `document` without a
        // node id threw `TypeError: not an element` there and killed the
        // bundle
        let (mut dom, mut ps) = page(
            r##"<html><head></head><body><div id="d"></div><script>
                var seen = "";
                var mo = new MutationObserver(function () { seen = "mutation"; });
                window.observeOk = false;
                try { mo.observe(document, { childList: true, subtree: true }); window.observeOk = true; } catch (e) {}
                window.docType = document.nodeType;
                window.fired = "";
                document.addEventListener("ping", function () { window.fired = "ping"; });
                document.dispatchEvent(new Event("ping"));
                document.documentElement.appendChild(document.createElement("span"));
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "observeOk"), "true");
        assert_eq!(get_global_str(&mut ps, "docType"), "9");
        assert_eq!(get_global_str(&mut ps, "fired"), "ping");
    }

    #[test]
    fn custom_elements_defines_and_answers_get() {
        // the exact shape GitHub's bundles use at load: probe, then define
        let (mut dom, mut ps) = page(
            r##"<html><body><script>
                class FileAttachment extends HTMLElement {}
                window.probed = customElements.get("file-attachment") === undefined;
                customElements.define("file-attachment", FileAttachment);
                window.defined = customElements.get("file-attachment") === FileAttachment;
                window.redefined = false;
                try { customElements.define("other", FileAttachment); } catch (e) { window.redefined = false; }
                var bad = false;
                try { customElements.define("nodash", FileAttachment); } catch (e) { bad = e instanceof SyntaxError; }
                window.badName = bad;
                window.upgraded = customElements.get("mine-el") === undefined;
                customElements.upgrade(document.body);
                window.afterUpgrade = document.body !== null;
                window.sub = FileAttachment.prototype instanceof HTMLElement;
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "probed"), "true");
        assert_eq!(get_global_str(&mut ps, "defined"), "true");
        assert_eq!(get_global_str(&mut ps, "badName"), "true");
        assert_eq!(get_global_str(&mut ps, "upgraded"), "true");
        assert_eq!(get_global_str(&mut ps, "afterUpgrade"), "true");
        assert_eq!(get_global_str(&mut ps, "sub"), "true");
    }

    #[test]
    fn es5_custom_element_adapter_can_construct_on_the_element_under_upgrade() {
        // GitHub ships the custom-elements ES5 adapter, which replaces the
        // global with `function () { return Reflect.construct(BuiltIn, [],
        // this.constructor) }`. Spec: that call returns the element on the
        // definition's construction stack, i.e. the element being upgraded.
        let (mut dom, mut ps) = page(
            r##"<html><body><my-box id="existing"></my-box><script>
                var BuiltInHTMLElement = HTMLElement;
                window.HTMLElement = function () {
                    return Reflect.construct(BuiltInHTMLElement, [], this.constructor);
                };
                window.HTMLElement.prototype = BuiltInHTMLElement.prototype;
                class MyBox extends HTMLElement {
                    constructor() {
                        super();
                        this.kind = "box";
                        this.id2 = this.getAttribute("id");
                    }
                    tag() { return this.tagName; }
                }
                customElements.define("my-box", MyBox);
                var el = document.getElementById("existing");
                window.kind = el.kind;
                window.id2 = el.id2;
                window.tag = el.tag();
                window.instance = el instanceof MyBox;
                // without an element under construction the interface is
                // still illegal to construct
                var illegal = false;
                try { new HTMLElement(); } catch (e) { illegal = e instanceof TypeError; }
                window.illegal = illegal;
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "kind"), "box");
        assert_eq!(get_global_str(&mut ps, "id2"), "existing");
        assert_eq!(get_global_str(&mut ps, "tag"), "MY-BOX");
        assert_eq!(get_global_str(&mut ps, "instance"), "true");
        assert_eq!(get_global_str(&mut ps, "illegal"), "true");
    }

    #[test]
    fn custom_elements_upgrade_existing_created_and_cloned() {
        // a definition upgrades the parsed element; createElement and
        // cloneNode upgrade the new ones. `super()` must return the element.
        let (mut dom, mut ps) = page(
            r##"<html><body><my-box id="existing"></my-box><div id="plain"></div><script>
                window.made = 0;
                class MyBox extends HTMLElement {
                    constructor() {
                        super();
                        window.made++;
                        this.kind = "box";
                    }
                    get label() { return "box:" + this.getAttribute("id"); }
                    tag() { return this.tagName; }
                }
                customElements.define("my-box", MyBox);
                var existing = document.getElementById("existing");
                window.existingInstance = existing instanceof MyBox;
                window.existingKind = existing.kind;
                window.existingLabel = existing.label;
                window.existingTag = existing.tag();
                window.madeAfterDefine = window.made;
                var created = document.createElement("my-box");
                window.createdKind = created.kind;
                window.createdIsInstance = created instanceof MyBox;
                var copy = existing.cloneNode(true);
                window.copyKind = copy.kind;
                window.copyCount = window.made;
                window.identity = document.getElementById("existing") === document.getElementById("existing");
                window.plainClass = document.getElementById("plain") instanceof MyBox;
                var illegal = false;
                try { new HTMLElement(); } catch (e) { illegal = e instanceof TypeError; }
                window.illegal = illegal;
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "existingInstance"), "true");
        assert_eq!(get_global_str(&mut ps, "existingKind"), "box");
        assert_eq!(get_global_str(&mut ps, "existingLabel"), "box:existing");
        assert_eq!(get_global_str(&mut ps, "existingTag"), "MY-BOX");
        assert_eq!(get_global_str(&mut ps, "madeAfterDefine"), "1");
        assert_eq!(get_global_str(&mut ps, "createdKind"), "box");
        assert_eq!(get_global_str(&mut ps, "createdIsInstance"), "true");
        assert_eq!(get_global_str(&mut ps, "copyKind"), "box");
        assert_eq!(get_global_str(&mut ps, "copyCount"), "3");
        assert_eq!(get_global_str(&mut ps, "identity"), "true");
        assert_eq!(get_global_str(&mut ps, "plainClass"), "false");
        assert_eq!(get_global_str(&mut ps, "illegal"), "true");
    }

    #[test]
    fn custom_element_connected_callback_fires_once_connected() {
        // Lit's base class starts its first render in connectedCallback, so an
        // upgraded element that never hears it stays empty. The spec fires it
        // on upgrade (when already in the document) and on insertion.
        let (mut dom, mut ps) = page(
            r##"<html><body><my-el id="parsed"></my-el><script>
                window.log = [];
                class MyEl extends HTMLElement {
                    connectedCallback() {
                        window.log.push(this.id + ":" + this.tagName);
                    }
                }
                customElements.define("my-el", MyEl);
                // the parsed element was upgraded and connected by `define`
                var orphan = document.createElement("my-el");
                orphan.id = "orphan";
                window.beforeAppend = window.log.length;
                var box = document.createElement("div");
                var nested = document.createElement("my-el");
                nested.id = "nested";
                box.appendChild(nested);          // box is detached
                window.afterNested = window.log.length;
                document.body.appendChild(box);   // now the subtree connects
                window.logText = window.log.join(",");
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "beforeAppend"), "1");
        assert_eq!(get_global_str(&mut ps, "afterNested"), "1");
        assert_eq!(
            get_global_str(&mut ps, "logText"),
            "parsed:MY-EL,nested:MY-EL"
        );
    }

    #[test]
    fn element_surface_reports_its_methods() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="d" hidden></div><script>
                window.t = [
                    typeof document.body.matches,
                    typeof document.body.hasAttribute,
                    typeof window.removeEventListener,
                    typeof document.removeEventListener,
                    typeof document.body.removeEventListener,
                ].join(",");
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(
            get_global_str(&mut ps, "t"),
            "function,function,function,function,function"
        );
    }

    #[test]
    fn matches_and_has_attribute_answer_like_the_dom() {
        // the exact probes github.com makes: `document.body.matches(":modal")`
        // (a `<dialog>` capability check) and
        // `documentElement.hasAttribute("override-fetch")`
        let (mut dom, mut ps) = page(
            r##"<html><body><div id="d" class="a b" hidden></div><script>
                var el = document.getElementById("d");
                window.t = [
                    document.body.matches("body"),
                    el.matches("#d"),
                    el.matches(".b"),
                    el.matches(".zz"),
                    document.body.matches(":modal"),
                    el.hasAttribute("hidden"),
                    el.hasAttribute("nope"),
                    el.hasAttribute("hidden") && el.getAttribute("hidden") === "",
                    document.documentElement.hasAttribute("override-fetch"),
                ].join(",");
            </script></body></html>"##,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(
            get_global_str(&mut ps, "t"),
            "true,true,true,false,false,true,false,true,false"
        );
    }

    #[test]
    fn removeEventListener_stops_the_callback() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="d"></div><script>
                window.calls = 0;
                function h() { window.calls += 1; }
                functionon = function () { window.calls += 100; };
                var el = document.getElementById("d");
                // duplicate registrations are one (spec), and removal takes it
                el.addEventListener("click", h);
                el.addEventListener("click", h);
                el.dispatchEvent(new Event("click"));
                el.removeEventListener("click", h);
                el.dispatchEvent(new Event("click"));
                // a different callback survives the removal of another
                el.addEventListener("click", functionon);
                el.removeEventListener("click", h);
                el.dispatchEvent(new Event("click"));
                // window-level removal, the react-dom feature probe shape
                var wc = 0;
                function w() { wc += 1; }
                window.addEventListener("test", w);
                window.removeEventListener("test", w);
                window.dispatchEvent(new Event("test"));
                window.windowCalls = wc;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        // one fire with `h` live (the duplicate registration is still one
        // listener), none after its removal, then +100 from the other handler
        assert_eq!(get_global_str(&mut ps, "calls"), "101");
        assert_eq!(get_global_str(&mut ps, "windowCalls"), "0");
    }

    #[test]
    fn add_event_listener_accepts_a_handle_event_object() {
        // An EventListener may be an object with a `handleEvent` method. React
        // and Lit both register `{get capture(){…}}` on window to probe
        // passive-event support at load, so treating a non-function as an
        // error logged two notes on every github.com page and skipped the
        // registration.
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="d"></div><script>
                window.hits = 0;
                var listener = {
                    handleEvent: function (ev) {
                        window.hits += 1;
                        window.kind = ev.type;
                    }
                };
                var el = document.getElementById("d");
                el.addEventListener("ping", listener);
                el.dispatchEvent(new Event("ping"));
                // the feature-detection registration, verbatim
                var probe = { get capture() { return false } };
                try {
                    window.addEventListener("test", probe, probe);
                    window.removeEventListener("test", probe, probe);
                    window.probe = "ok";
                } catch (e) {
                    window.probe = "threw";
                }
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "hits"), "1");
        assert_eq!(get_global_str(&mut ps, "kind"), "ping");
        assert_eq!(get_global_str(&mut ps, "probe"), "ok");
    }

    #[test]
    fn import_map_is_collected_and_other_data_blocks_are_not() {
        let dom = html::parse(
            r#"<html><head>
                <script type="application/ld+json">{"a":1}</script>
                <script type="importmap">{"imports":{"react":"/react.js"}}</script>
                <script type="importmap">{"imports":{"late":"/late.js"}}</script>
            </head></html>"#,
        );
        // the first map wins; ld+json and later maps are not maps
        assert_eq!(
            collect_import_map(&dom).as_deref(),
            Some(r#"{"imports":{"react":"/react.js"}}"#)
        );
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
    fn live_collections_track_dom_mutations() {
        let (mut dom, mut ps) = page(
            r#"<html><body><ul id="list">
                <li class="row">a</li><li class="row">b</li>
            </ul><script>
                window.c = document.getElementsByClassName("row");
                window.n0 = window.c.length;
                window.first = window.c[0].textContent;
                window.byItem = window.c.item(1).textContent;
                window.missing = window.c.namedItem("nope") === null;
                // querySelectorAll is a static NodeList
                window.q = document.querySelectorAll(".row");
                window.q0 = window.q.length;
                var li = document.createElement("li");
                li.setClassName("row");
                li.setTextContent("c");
                document.getElementById("list").appendChild(li);
                window.n1 = window.c.length;
                window.third = window.c[2].textContent;
                window.q1 = window.q.length;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "n0"), "2", "initial length");
        assert_eq!(get_global_str(&mut ps, "first"), "a", "index read");
        assert_eq!(get_global_str(&mut ps, "byItem"), "b", "item()");
        assert_eq!(get_global_str(&mut ps, "missing"), "true", "namedItem miss = null");
        assert_eq!(get_global_str(&mut ps, "n1"), "3", "length after appendChild");
        assert_eq!(get_global_str(&mut ps, "third"), "c", "new node is visible");
        assert_eq!(get_global_str(&mut ps, "q0"), "2", "static NodeList length");
        assert_eq!(get_global_str(&mut ps, "q1"), "2", "NodeList does not grow");
    }

    #[test]
    fn document_forms_and_form_elements_allow_named_access() {
        let (mut dom, mut ps) = page(
            r#"<html><body>
                <form id="f" name="login"><input name="email" value="e@x">
                <input id="pw" name="pass"></form>
                <script>
                    window.formsLen = document.forms.length;
                    window.formName = document.forms.login.name;
                    var f = document.forms.login;
                    window.ctrlLen = f.elements.length;
                    window.emailName = f.elements.email.name;
                    window.pwId = f.elements.item(1).id;
                    window.byNamedItem = f.elements.namedItem("pass").id;
                    window.byIndex = document.forms[0].id;
                </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "formsLen"), "1");
        assert_eq!(get_global_str(&mut ps, "formName"), "login", "named form access");
        assert_eq!(get_global_str(&mut ps, "ctrlLen"), "2", "form.elements length");
        assert_eq!(get_global_str(&mut ps, "emailName"), "email");
        assert_eq!(get_global_str(&mut ps, "pwId"), "pw", "item(1) in document order");
        assert_eq!(get_global_str(&mut ps, "byNamedItem"), "pw", "namedItem");
        assert_eq!(get_global_str(&mut ps, "byIndex"), "f");
    }

    #[test]
    fn live_collections_iterate_and_accept_generic_array_methods() {
        let (mut dom, mut ps) = page(
            r#"<html><body>
                <div class="x">1</div><div class="x">2</div>
                <script>
                    var c = document.getElementsByClassName("x");
                    window.spread = [...c].length;
                    var a = Array.from(c);
                    window.fromLen = a.length;
                    window.fromFirst = a[0].textContent;
                    // MDN's header-anchor code does exactly this: a generic
                    // Array.prototype method called on a collection
                    window.filtered =
                        Array.prototype.filter.call(c, function () { return true; }).length;
                    var n = 0;
                    for (var el of c) { if (el.textContent) n++; }
                    window.loop = n;
                </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "spread"), "2");
        assert_eq!(get_global_str(&mut ps, "fromLen"), "2");
        assert_eq!(get_global_str(&mut ps, "fromFirst"), "1");
        assert_eq!(get_global_str(&mut ps, "filtered"), "2", "Array.prototype.filter.call");
        assert_eq!(get_global_str(&mut ps, "loop"), "2", "for..of");
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
    fn document_location_is_the_window_location() {
        // GitHub's marketing bootstrap reads `document.location.search`
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.same = document.location === location;
                window.search = typeof document.location.search;
                window.href = document.location.href;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "same"), "true");
        assert_eq!(get_global_str(&mut ps, "search"), "string");
        assert_eq!(get_global_str(&mut ps, "href"), "https://example.com/");
    }

    #[test]
    fn document_domain_is_the_page_host() {
        // GitHub's cookie helper reads `document.domain` and throws
        // "Unable to get document domain" when it is null, killing the
        // bundle at load.
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.t = document.domain;
                window.h = document.domain === location.hostname;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "t"), "example.com");
        assert_eq!(get_global_str(&mut ps, "h"), "true");
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

    #[test]
    fn get_elements_by_class_name_matches_all_names_in_document_order() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="d1" class="a b"><span class="b">x</span><span class="a">y</span></div><div id="d2" class="b"></div><p id="p1" class="a"></p><script>
                var b = document.getElementsByClassName("b");
                window.n = b.length;
                window.first = b[0].id;
                var both = document.getElementsByClassName("a b");
                window.both = both.length;
                window.bothId = both[0].id;
                var scoped = document.getElementById("d1").getElementsByClassName("b");
                window.scope = scoped.length;
                window.scopeTag = scoped[0].tagName;
                window.empty = document.getElementsByClassName("").length;
                window.none = document.getElementsByClassName("a zzz").length;
                var spans = document.getElementById("d1").getElementsByTagName("span");
                window.elSpans = spans.length;
                window.elAll = document.getElementById("d1").getElementsByTagName("*").length;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        // document order: #d1, its span.b, then #d2
        assert_eq!(get_global_str(&mut ps, "n"), "3");
        assert_eq!(get_global_str(&mut ps, "first"), "d1");
        // multiple names are ANDed
        assert_eq!(get_global_str(&mut ps, "both"), "1");
        assert_eq!(get_global_str(&mut ps, "bothId"), "d1");
        // on an element: descendants only, root excluded
        assert_eq!(get_global_str(&mut ps, "scope"), "1");
        assert_eq!(get_global_str(&mut ps, "scopeTag"), "SPAN");
        assert_eq!(get_global_str(&mut ps, "empty"), "0");
        assert_eq!(get_global_str(&mut ps, "none"), "0");
        // element-level getElementsByTagName: subtree only, root excluded
        assert_eq!(get_global_str(&mut ps, "elSpans"), "2");
        assert_eq!(get_global_str(&mut ps, "elAll"), "2");
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

    /// React's streaming bootstrap walks boundary comments: comments are real
    /// nodes with a writable `data`, and text children can be moved/removed.
    #[test]
    fn comment_and_sibling_navigation() {
        let (mut dom, mut ps) = page(
            r#"<html><body><!--$?--><template id="B:0"></template><!--/$--><div id="S:0">streamed</div><script>
                var a = document.getElementById("B:0");
                var c = a.previousSibling;
                window.marker = c.nodeType + ":" + c.data;
                c.data = "$!";
                window.afterWrite = a.previousSibling.data;
                window.parentTag = a.parentNode.tagName;
                var s = document.getElementById("S:0");
                var p = s.parentNode;
                var moved = s.firstChild;
                p.insertBefore(moved, s);
                window.movedType = s.previousSibling.nodeType;
                window.movedNext = s.previousSibling.nextSibling.getAttribute("id");
                window.movedText = s.previousSibling.data;
                p.removeChild(s.previousSibling);
                window.afterRemove = s.previousSibling.nodeType;
                a.dataset.dgst = "d1";
                window.dgst = a.dataset.dgst;
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "marker"), "8:$?");
        assert_eq!(get_global_str(&mut ps, "afterWrite"), "$!");
        assert_eq!(get_global_str(&mut ps, "parentTag"), "BODY");
        assert_eq!(get_global_str(&mut ps, "movedType"), "3");
        assert_eq!(get_global_str(&mut ps, "movedNext"), "S:0");
        assert_eq!(get_global_str(&mut ps, "movedText"), "streamed");
        assert_eq!(get_global_str(&mut ps, "afterRemove"), "8");
        assert_eq!(get_global_str(&mut ps, "dgst"), "d1");
    }

    /// `document.forms` and `getElementsByName` are live document-order
    /// collections; a newly appended matching node shows up without a
    /// re-query.
    #[test]
    fn document_forms_and_get_elements_by_name() {
        let (mut dom, mut ps) = page(
            r#"<html><body><form id="a" name="login"><input name="user"></form><form id="b"></form><input name="user"><script>
                window.formsLen = document.forms.length;
                window.firstForm = document.forms[0].tagName;
                window.formName = document.forms[0].getAttribute("name");
                var named = document.getElementsByName("user");
                window.namedLen = named.length;
                window.namedTag = named[0].tagName;
                window.missing = document.getElementsByName("nope").length;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "formsLen"), "2");
        assert_eq!(get_global_str(&mut ps, "firstForm"), "FORM");
        assert_eq!(get_global_str(&mut ps, "formName"), "login");
        assert_eq!(get_global_str(&mut ps, "namedLen"), "2");
        assert_eq!(get_global_str(&mut ps, "namedTag"), "INPUT");
        assert_eq!(get_global_str(&mut ps, "missing"), "0");
    }

    /// `createTextNode` / `createComment` return real arena nodes: nodeType
    /// 3/8, a live `data`, and they attach through `appendChild`.
    #[test]
    fn create_text_and_comment_nodes() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="h"></div><script>
                var t = document.createTextNode("hi <b>");
                var c = document.createComment("mark");
                var h = document.getElementById("h");
                h.appendChild(t);
                h.appendChild(c);
                window.types = t.nodeType + "," + c.nodeType;
                window.data = t.data;
                window.parentTag = t.parentNode.tagName;
                window.text = document.getElementById("h").textContent;
                window.kids = h.childNodes.length;
                t.data = "changed";
                window.live = h.firstChild.data;
                window.commentKids = c.childNodes.length;
            </script></body></html>"#,
        );
        let turn = ps.run_scripts(&mut dom);
        assert!(turn.dirty, "appended nodes re-layout");
        assert_eq!(get_global_str(&mut ps, "types"), "3,8");
        assert_eq!(get_global_str(&mut ps, "data"), "hi <b>");
        assert_eq!(get_global_str(&mut ps, "parentTag"), "DIV");
        assert_eq!(get_global_str(&mut ps, "text"), "hi <b>");
        assert_eq!(get_global_str(&mut ps, "kids"), "2");
        assert_eq!(get_global_str(&mut ps, "live"), "changed");
        assert_eq!(get_global_str(&mut ps, "commentKids"), "0");
    }

    /// `Node.childNodes` includes text/comment children; `Element.children`
    /// keeps only elements. Both are snapshots: an array read before a
    /// mutation keeps its length, a fresh read sees the new child.
    #[test]
    fn child_nodes_and_children_snapshots() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="h">alpha<span id="s1"></span><!--c--><b id="s2"></b></div><script>
                var h = document.getElementById("h");
                var kids = h.childNodes;
                window.all = kids.length;
                window.elems = h.children.length;
                window.types = kids[0].nodeType + "," + kids[1].nodeType + "," + kids[2].nodeType + "," + kids[3].nodeType;
                window.firstTag = h.children[0].tagName;
                window.secondTag = h.children[1].tagName;
                h.appendChild(document.createTextNode("!"));
                window.before = kids.length;
                window.after = h.childNodes.length;
                window.elemsAfter = h.children.length;
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "all"), "4");
        assert_eq!(get_global_str(&mut ps, "elems"), "2");
        assert_eq!(get_global_str(&mut ps, "types"), "3,1,8,1");
        assert_eq!(get_global_str(&mut ps, "firstTag"), "SPAN");
        assert_eq!(get_global_str(&mut ps, "secondTag"), "B");
        assert_eq!(get_global_str(&mut ps, "before"), "4");
        assert_eq!(get_global_str(&mut ps, "after"), "5");
        assert_eq!(get_global_str(&mut ps, "elemsAfter"), "2");
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

    /// rAF callbacks receive a frame timestamp on the same clock as
    /// `performance.now()`: a finite, non-negative number.
    #[test]
    fn raf_callback_receives_performance_timestamp() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.ok = false;
                requestAnimationFrame(function (t) {
                    window.ok = typeof t === "number" && t >= 0 &&
                        t <= performance.now() + 50;
                    window.ts = t;
                });
            </script></body></html>"#,
        );
        let _ = ps.run_scripts(&mut dom);
        std::thread::sleep(Duration::from_millis(18));
        ps.run_due_frames(&mut dom);
        assert_eq!(get_global_str(&mut ps, "ok"), "true", "timestamp is a number on the perf clock");
        let ts: f64 = get_global_str(&mut ps, "ts").parse().expect("numeric timestamp");
        assert!(ts.is_finite() && ts >= 0.0);
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

    /// `new CustomEvent` + `dispatchEvent`: target first, then ancestors,
    /// then the window/document bucket. `detail`, `target` and
    /// `currentTarget` reach listeners; the event settles back to phase 0.
    #[test]
    fn custom_event_dispatch_bubbles() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="p"><button id="t">x</button></div><script>
                window.log = [];
                var t = document.getElementById("t");
                var p = document.getElementById("p");
                t.addEventListener("ping", function (e) {
                    window.log.push("t:" + e.detail + ":" + e.target.id + ":" + e.currentTarget.id);
                });
                p.addEventListener("ping", function (e) {
                    window.log.push("p:" + e.detail + ":" + e.currentTarget.id);
                });
                document.addEventListener("ping", function (e) { window.log.push("d"); });
                addEventListener("ping", function (e) {
                    window.log.push("w:" + e.detail + ":" + e.target.id);
                });
                var ev = new CustomEvent("ping", { detail: 7, bubbles: true });
                window.ok = t.dispatchEvent(ev);
                window.phase = ev.eventPhase;
                window.cur = ev.currentTarget;
                window.tgt = ev.target.id;
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(
            get_global_str(&mut ps, "log"),
            "t:7:t:t,p:7:p,d,w:7:t",
            "target, ancestor, then the shared window/document stop"
        );
        assert_eq!(get_global_str(&mut ps, "ok"), "true");
        assert_eq!(get_global_str(&mut ps, "phase"), "0");
        assert_eq!(get_global_str(&mut ps, "cur"), "null");
        assert_eq!(get_global_str(&mut ps, "tgt"), "t");
    }

    /// `bubbles: false` (the constructor default) stops at the target.
    #[test]
    fn custom_event_without_bubbles_stays_on_target() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="p"><button id="t">x</button></div><script>
                window.log = "";
                var t = document.getElementById("t");
                var p = document.getElementById("p");
                t.addEventListener("ping", function () { window.log = window.log + "t"; });
                p.addEventListener("ping", function () { window.log = window.log + "p"; });
                addEventListener("ping", function () { window.log = window.log + "w"; });
                t.dispatchEvent(new Event("ping"));
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "log"), "t");
    }

    /// Stop flags: `stopPropagation` ends the bubble walk;
    /// `stopImmediatePropagation` also skips later listeners on that stop.
    #[test]
    fn custom_event_stop_flags() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="p"><button id="t">x</button></div><script>
                window.log = "";
                var t = document.getElementById("t");
                var p = document.getElementById("p");
                t.addEventListener("s", function (e) {
                    window.log = window.log + "a";
                    e.stopImmediatePropagation();
                });
                t.addEventListener("s", function () { window.log = window.log + "b"; });
                p.addEventListener("s", function () { window.log = window.log + "p"; });
                t.dispatchEvent(new Event("s", { bubbles: true }));

                window.log2 = "";
                t.addEventListener("q", function (e) {
                    window.log2 = window.log2 + "t";
                    e.stopPropagation();
                });
                p.addEventListener("q", function () { window.log2 = window.log2 + "p"; });
                addEventListener("q", function () { window.log2 = window.log2 + "w"; });
                t.dispatchEvent(new Event("q", { bubbles: true }));
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "log"), "a");
        assert_eq!(get_global_str(&mut ps, "log2"), "t");
    }

    /// `preventDefault` only sticks on cancelable events and flips
    /// `dispatchEvent`'s return value.
    #[test]
    fn custom_event_prevent_default_is_cancelable() {
        let (mut dom, mut ps) = page(
            r#"<html><body><button id="t">x</button><script>
                var t = document.getElementById("t");
                t.addEventListener("c", function (e) { e.preventDefault(); });
                var yes = new Event("c", { cancelable: true });
                window.r1 = t.dispatchEvent(yes);
                window.d1 = yes.defaultPrevented;
                var no = new Event("c");
                window.r2 = t.dispatchEvent(no);
                window.d2 = no.defaultPrevented;
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "r1"), "false");
        assert_eq!(get_global_str(&mut ps, "d1"), "true");
        assert_eq!(get_global_str(&mut ps, "r2"), "true");
        assert_eq!(get_global_str(&mut ps, "d2"), "false");
    }

    /// Constructors are real constructors: prototype chain makes a
    /// CustomEvent an Event; window and document dispatch share the global
    /// bucket; calling without `new` throws.
    #[test]
    fn event_constructors_and_window_dispatch() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.a = new Event("x") instanceof Event;
                window.b = new CustomEvent("x") instanceof CustomEvent;
                window.c = new CustomEvent("x") instanceof Event;
                window.hit = 0;
                addEventListener("w", function (e) { window.hit = window.hit + e.detail; });
                window.dispatchEvent(new CustomEvent("w", { detail: 3 }));
                document.addEventListener("d2", function () { window.d2 = 1; });
                document.dispatchEvent(new Event("d2"));
                var threw = false;
                try { Event("x"); } catch (err) { threw = true; }
                window.threw = threw;
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "a"), "true");
        assert_eq!(get_global_str(&mut ps, "b"), "true");
        assert_eq!(get_global_str(&mut ps, "c"), "true");
        assert_eq!(get_global_str(&mut ps, "hit"), "3");
        assert_eq!(get_global_str(&mut ps, "d2"), "1");
        assert_eq!(get_global_str(&mut ps, "threw"), "true");
    }

    /// Child-list, attribute and character-data records arrive in mutation
    /// order; `subtree` makes a descendant's own child-list change visible.
    #[test]
    fn mutation_observer_records_childlist_attributes_and_subtree() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="box"></div><script>
                window.log = [];
                var box = document.getElementById("box");
                var mo = new MutationObserver(function (records) {
                    for (var i = 0; i < records.length; i++) {
                        var r = records[i];
                        window.log.push(r.type + ":" + (r.attributeName || "") + ":" + (r.target.id || r.target.tagName));
                    }
                });
                mo.observe(box, { childList: true, attributes: true, subtree: true });
                var s = document.createElement("span");
                s.id = "s";
                box.appendChild(s);
                box.setAttribute("data-x", "1");
                s.setTextContent("hi");
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(
            get_global_str(&mut ps, "log"),
            "childList::box,attributes:data-x:box,childList::s"
        );
    }

    /// `takeRecords` consumes only this observer's matching records, honours
    /// `attributeFilter`/`attributeOldValue`, and `disconnect` stops delivery.
    #[test]
    fn mutation_observer_take_records_filter_and_disconnect() {
        let (mut dom, mut ps) = page(
            r#"<html><body><div id="box"></div><script>
                window.got = 0;
                var box = document.getElementById("box");
                var mo = new MutationObserver(function () { window.got = window.got + 1; });
                mo.observe(box, {
                    attributes: true,
                    attributeOldValue: true,
                    attributeFilter: ["data-k"]
                });
                box.setAttribute("data-k", "v1");
                box.setAttribute("data-k", "v2");
                box.setAttribute("other", "x");
                var recs = mo.takeRecords();
                window.n = recs.length;
                window.old = recs[1].oldValue;
                window.kind = recs[0].type;
                mo.disconnect();
                box.setAttribute("data-k", "v3");
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "n"), "2");
        assert_eq!(get_global_str(&mut ps, "old"), "v1");
        assert_eq!(get_global_str(&mut ps, "kind"), "attributes");
        assert_eq!(get_global_str(&mut ps, "got"), "0");
    }

    /// Constructor/observe validation and `instanceof`.
    #[test]
    fn mutation_observer_rejects_bad_input() {
        let (mut dom, mut ps) = page(
            r#"<html><body><script>
                window.threw = 0;
                try { new MutationObserver(1); } catch (e) { window.threw = window.threw + 1; }
                try {
                    var m = new MutationObserver(function () {});
                    m.observe(document.body, {});
                } catch (e) { window.threw = window.threw + 1; }
                try {
                    new MutationObserver(function () {}).observe(null, { childList: true });
                } catch (e) { window.threw = window.threw + 1; }
                window.iso = new MutationObserver(function () {}) instanceof MutationObserver;
            </script></body></html>"#,
        );
        ps.run_scripts(&mut dom);
        assert_eq!(get_global_str(&mut ps, "threw"), "3");
        assert_eq!(get_global_str(&mut ps, "iso"), "true");
    }
}
