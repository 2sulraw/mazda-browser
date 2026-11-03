//! Standard globals + prototypes installed by `JsEngine::new`.
//!
//! Natives share a small dispatch table: one Rust fn per GROUP with an index
//! (`u16`) so an entire family (Math.*, Array.prototype.*) costs one fn
//! pointer in the table. All console output goes to stderr via `eprintln!`
//! to match the engine's logging style.

use super::interp::{fmt_date_iso, slice_bounds};
use super::*;

/// `new` vs plain call distinction for constructor natives: during a
/// constructor call `this` is a fresh empty object, never the global.
fn is_ctor_call(e: &JsEngine, this: &Val) -> bool {
    match this {
        Val::Obj(o) => !Rc::ptr_eq(o, &e.global_obj),
        _ => false,
    }
}

fn need_obj<'a>(this: &'a Val, what: &str) -> Result<&'a Rc<RefCell<Obj>>, JsError> {
    match this {
        Val::Obj(o) => Ok(o),
        _ => Err(JsError::throw(&format!(
            "TypeError: {} called on non-object",
            what
        ))),
    }
}

pub(crate) fn install(e: &mut JsEngine) {
    // top-level `this` === global object
    e.global_env
        .declare(Rc::from("this"), Val::Obj(e.global_obj.clone()), false);

    // -- console -----------------------------------------------------------
    let console = Val::new_object();
    let console_log = e.register_native("console.log", n_console, 0);
    for name in ["log", "info", "debug", "warn", "error", "eprintln"] {
        obj_set_own(own(&console), name, Val::Native(console_log));
    }
    e.set_global("console", console);

    // -- Math --------------------------------------------------------------
    let math = Val::new_object();
    for (i, name) in [
        "floor",
        "ceil",
        "round",
        "abs",
        "max",
        "min",
        "pow",
        "sqrt",
        "trunc",
        "random",
        "sign",
    ]
    .iter()
    .enumerate()
    {
        let id = e.register_native(name, n_math, i as u16);
        obj_set_own(own(&math), name, Val::Native(id));
    }
    obj_set_own(own(&math), "PI", Val::Num(std::f64::consts::PI));
    obj_set_own(own(&math), "E", Val::Num(std::f64::consts::E));
    e.set_global("Math", math);

    // -- JSON --------------------------------------------------------------
    let json = Val::new_object();
    let p = e.register_native("JSON.parse", n_json, 0);
    let s = e.register_native("JSON.stringify", n_json, 1);
    obj_set_own(own(&json), "parse", Val::Native(p));
    obj_set_own(own(&json), "stringify", Val::Native(s));
    e.set_global("JSON", json);

    // -- Object ------------------------------------------------------------
    let object = Val::new_object();
    for (i, name) in ["keys", "values", "entries", "assign"].iter().enumerate() {
        let id = e.register_native(name, n_object, i as u16);
        obj_set_own(own(&object), name, Val::Native(id));
    }
    let dp = e.register_native("Object.defineProperty", n_object, O_DEFINE_PROP);
    obj_set_own(own(&object), "defineProperty", Val::Native(dp));
    let sp = e.register_native("Object.setPrototypeOf", n_object, O_SET_PROTO);
    obj_set_own(own(&object), "setPrototypeOf", Val::Native(sp));
    let gp = e.register_native("Object.getPrototypeOf", n_object, O_GET_PROTO);
    obj_set_own(own(&object), "getPrototypeOf", Val::Native(gp));
    let cr = e.register_native("Object.create", n_object, O_CREATE);
    obj_set_own(own(&object), "create", Val::Native(cr));
    let opn = e.register_native("Object.getOwnPropertyNames", n_object, O_PROP_NAMES);
    obj_set_own(own(&object), "getOwnPropertyNames", Val::Native(opn));
    let fe = e.register_native("Object.fromEntries", n_object, O_FROM_ENTRIES);
    obj_set_own(own(&object), "fromEntries", Val::Native(fe));
    for (name, idx) in [
        ("freeze", O_FREEZE),
        ("seal", O_SEAL),
        ("is", O_IS),
        ("isFrozen", O_IS_FROZEN),
        ("isSealed", O_IS_SEALED),
        ("hasOwn", O_HAS_OWN),
        ("getOwnPropertyDescriptor", O_GET_DESC),
        ("getOwnPropertyDescriptors", O_GET_DESCS),
        ("defineProperties", O_DEFINE_PROPS),
    ] {
        let id = e.register_native(&format!("Object.{name}"), n_object, idx);
        obj_set_own(own(&object), name, Val::Native(id));
    }
    e.set_global("Object", object.clone());

    // `Object(x)` — the constructor is an ordinary object here, so the call
    // form goes through the hidden `__call` hook (see `JsEngine::call`)
    let oc = e.register_native("Object", n_object, O_CALL);
    obj_set_own(own(&object), "__call", Val::Native(oc));

    // Object.prototype + Array.prototype chain
    let obj_proto = Val::new_object();
    let ts = e.register_native("toString", n_objproto, 0);
    let ho = e.register_native("hasOwnProperty", n_objproto, 1);
    let pie = e.register_native("propertyIsEnumerable", n_objproto, 2);
    let vof = e.register_native("valueOf", n_objproto, 3);
    let ipo = e.register_native("isPrototypeOf", n_objproto, 4);
    obj_set_own(own(&obj_proto), "toString", Val::Native(ts));
    obj_set_own(own(&obj_proto), "hasOwnProperty", Val::Native(ho));
    obj_set_own(own(&obj_proto), "propertyIsEnumerable", Val::Native(pie));
    obj_set_own(own(&obj_proto), "valueOf", Val::Native(vof));
    obj_set_own(own(&obj_proto), "isPrototypeOf", Val::Native(ipo));
    // `Object.prototype` is reachable as a property, so libraries that borrow
    // methods (`Object.prototype.hasOwnProperty.call(o, k)`) work. Plain
    // objects also lazily inherit it on first member access (get_member), so
    // `({}).hasOwnProperty` resolves.
    if let Val::Obj(o) = &obj_proto {
        e.object_proto = Some(o.clone());
    }
    obj_set_own(own(&object), "prototype", obj_proto.clone());

    let arr_proto = Val::new_object();
    if let Val::Obj(o) = &arr_proto {
        o.borrow_mut().proto = match &obj_proto {
            Val::Obj(p) => Some(p.clone()),
            _ => None,
        };
    }
    for (i, name) in [
        "push",
        "pop",
        "shift",
        "unshift",
        "slice",
        "concat",
        "join",
        "indexOf",
        "includes",
        "map",
        "filter",
        "forEach",
        "reduce",
        "some",
        "every",
        "sort",
        "reverse",
        "splice",
        "lastIndexOf",
    ]
    .iter()
    .enumerate()
    {
        let id = e.register_native(name, n_array, i as u16);
        obj_set_own(own(&arr_proto), name, Val::Native(id));
    }
    if let Val::Obj(o) = &arr_proto {
        e.array_proto = Some(o.clone());
    }
    let is_array = e.register_native("Array.isArray", n_array, 100);
    let array_ctor = Val::new_object();
    obj_set_own(own(&array_ctor), "isArray", Val::Native(is_array));
    let ac = e.register_native("Array", n_array_call, 0);
    let a_of = e.register_native("Array.of", n_array_call, 1);
    let a_from = e.register_native("Array.from", n_array_call, 2);
    obj_set_own(own(&array_ctor), "__call", Val::Native(ac));
    obj_set_own(own(&array_ctor), "of", Val::Native(a_of));
    obj_set_own(own(&array_ctor), "from", Val::Native(a_from));
    obj_set_own(own(&array_ctor), "prototype", arr_proto);
    e.set_global("Array", array_ctor);

    // -- Error family ------------------------------------------------------
    // A real `Error.prototype` (name/message/toString) so `class X extends
    // Error` builds a working chain and `err.toString()` is inherited. Each
    // subclass gets its own prototype object chained to it.
    let err_to_str = e.register_native("Error.prototype.toString", n_error_to_string, 0);
    let error_proto = Val::new_object();
    obj_set_own(own(&error_proto), "name", Val::Str(Rc::from("Error")));
    obj_set_own(own(&error_proto), "message", Val::Str(Rc::from("")));
    obj_set_own(own(&error_proto), "toString", Val::Native(err_to_str));
    if let (Val::Obj(o), Some(parent)) = (&error_proto, &e.object_proto) {
        o.borrow_mut().proto = Some(parent.clone());
    }
    if let Val::Obj(o) = &error_proto {
        e.error_proto = Some(o.clone());
    }
    for (i, name) in ["Error", "TypeError", "RangeError", "SyntaxError"]
        .iter()
        .enumerate()
    {
        let id = e.register_native(name, n_error_ctor, i as u16);
        if i == 0 {
            // `Error.prototype` IS the shared base prototype, so an instance
            // of any subclass satisfies `x instanceof Error`.
            e.set_native_prop(id, "prototype", error_proto.clone());
            e.set_global(name, Val::Native(id));
            continue;
        }
        let proto = Val::new_object();
        obj_set_own(own(&proto), "name", Val::Str(Rc::from(*name)));
        obj_set_own(own(&proto), "message", Val::Str(Rc::from("")));
        if let (Val::Obj(o), Val::Obj(parent)) = (&proto, &error_proto) {
            o.borrow_mut().proto = Some(parent.clone());
        }
        e.set_native_prop(id, "prototype", proto);
        e.set_global(name, Val::Native(id));
    }

    // -- Date --------------------------------------------------------------
    // `Date` is a callable native; `Date.now` is reached through method
    // dispatch on native values (see get_member/call_method).
    let dc = e.register_native("Date", n_date, 0);
    e.set_global("Date", Val::Native(dc));

    // -- conversions + numeric globals --------------------------------------
    let str_id = e.register_native("String", n_convert, 0);
    let num_id = e.register_native("Number", n_convert, 1);
    let bool_id = e.register_native("Boolean", n_convert, 2);
    e.set_global("String", Val::Native(str_id));
    e.set_global("Number", Val::Native(num_id));
    e.set_global("Boolean", Val::Native(bool_id));

    // -- primitive prototypes ----------------------------------------------
    // Bundles probe `"trimStart" in String.prototype` to decide whether to
    // install a polyfill, and borrow the method (`String.prototype.slice
    // .call(x)`). The primitive dispatch table in `get_member` stays the real
    // implementation; these prototype objects expose the same natives as
    // borrowed callables, so both patterns work.
    let str_proto = Val::new_object();
    for (i, (name, _m)) in STR_METHODS.iter().enumerate() {
        let id = e.register_native(&format!("String.prototype.{name}"), n_str_proto, i as u16);
        obj_set_own(own(&str_proto), name, Val::Native(id));
    }
    e.set_native_prop(str_id, "prototype", str_proto.clone());
    let num_proto = Val::new_object();
    for (i, (name, _m)) in NUM_METHODS.iter().enumerate() {
        let id = e.register_native(&format!("Number.prototype.{name}"), n_num_proto, i as u16);
        obj_set_own(own(&num_proto), name, Val::Native(id));
    }
    e.set_native_prop(num_id, "prototype", num_proto.clone());
    let bool_proto = Val::new_object();
    let bts = e.register_native("Boolean.prototype.toString", n_num_proto, 0);
    obj_set_own(own(&bool_proto), "toString", Val::Native(bts));
    e.set_native_prop(bool_id, "prototype", bool_proto.clone());
    if let Val::Obj(o) = &str_proto {
        o.borrow_mut().proto = match &obj_proto {
            Val::Obj(p) => Some(p.clone()),
            _ => None,
        };
    }

    // -- Function.prototype -------------------------------------------------
    // Function values already answer `call`/`apply`/`bind` directly; the
    // prototype object exposes the same three as borrowed callables
    // (`Function.prototype.call.call(f, thisArg, ...)`), which core-js uses.
    let fn_proto = Val::new_object();
    for (i, name) in ["call", "apply", "bind", "toString"].iter().enumerate() {
        let id = e.register_native(&format!("Function.prototype.{name}"), n_fn_proto, i as u16);
        obj_set_own(own(&fn_proto), name, Val::Native(id));
    }
    if let Val::Obj(o) = &fn_proto {
        o.borrow_mut().proto = match &obj_proto {
            Val::Obj(p) => Some(p.clone()),
            _ => None,
        };
        e.function_proto = Some(o.clone());
    }
    let fn_body = e.register_native("Function(...) body", n_fn_body, 0);
    let fn_ctor = e.register_native("Function", n_fn_ctor, 0);
    e.set_native_prop(fn_ctor, "prototype", fn_proto);
    e.fn_body_native = fn_body;
    e.set_global("Function", Val::Native(fn_ctor));

    e.set_global("NaN", Val::Num(f64::NAN));
    e.set_global("Infinity", Val::Num(f64::INFINITY));

    let pi = e.register_native("parseInt", n_num_globals, 0);
    let pf = e.register_native("parseFloat", n_num_globals, 1);
    let ie = e.register_native("isNaN", n_num_globals, 2);
    let ifn = e.register_native("isFinite", n_num_globals, 3);
    e.set_global("parseInt", Val::Native(pi));
    e.set_global("parseFloat", Val::Native(pf));
    e.set_global("isNaN", Val::Native(ie));
    e.set_global("isFinite", Val::Native(ifn));

    let enc = e.register_native("encodeURIComponent", n_uri, 0);
    let dec = e.register_native("decodeURIComponent", n_uri, 1);
    e.set_global("encodeURIComponent", Val::Native(enc));
    e.set_global("decodeURIComponent", Val::Native(dec));

    // -- timers (queue-only; embedder drains via drain_timers) --------------
    let st = e.register_native("setTimeout", n_set_timeout, 0);
    e.set_global("setTimeout", Val::Native(st));
    let si = e.register_native("setInterval", n_set_timeout, 1);
    e.set_global("setInterval", Val::Native(si));
    let ct = e.register_native("clearTimeout", n_set_timeout, 2);
    e.set_global("clearTimeout", Val::Native(ct));
    let ci = e.register_native("clearInterval", n_set_timeout, 2);
    e.set_global("clearInterval", Val::Native(ci));

    // -- Promise (constructor + statics) ------------------------------------
    // `new Promise(executor)` and all five static combinators below; the
    // constructor's resolve/reject twin natives live in `js.rs` (slots).
    let promise = Val::new_object();
    let pc = e.register_native("Promise", n_promise, PS_CTOR);
    let prs = e.register_native("Promise.resolve", n_promise, PS_RESOLVE);
    let prj = e.register_native("Promise.reject", n_promise, PS_REJECT);
    let pall = e.register_native("Promise.all", n_promise, PS_ALL);
    let pany = e.register_native("Promise.any", n_promise, PS_ANY);
    let pray = e.register_native("Promise.race", n_promise, PS_RACE);
    let pass = e.register_native("Promise.allSettled", n_promise, PS_ALL_SETTLED);
    e.set_global("Promise", Val::Native(pc));
    obj_set_own(own(&promise), "resolve", Val::Native(prs));
    obj_set_own(own(&promise), "reject", Val::Native(prj));
    obj_set_own(own(&promise), "all", Val::Native(pall));
    obj_set_own(own(&promise), "any", Val::Native(pany));
    obj_set_own(own(&promise), "race", Val::Native(pray));
    obj_set_own(own(&promise), "allSettled", Val::Native(pass));
    e.promise_statics = Some(obj_of(&promise));
    // `Promise.prototype` must exist: feature probes borrow its `then`
    // (`var Vy = Promise.prototype.then`) and re-apply it to real promises.
    let promise_proto = Val::new_object();
    obj_set_own(own(&promise_proto), "then", Val::Method(Meth::PromiseThen));
    obj_set_own(own(&promise_proto), "catch", Val::Method(Meth::PromiseCatch));
    obj_set_own(own(&promise_proto), "finally", Val::Method(Meth::PromiseFinally));
    if let (Val::Obj(o), Some(parent)) = (&promise_proto, &e.object_proto) {
        o.borrow_mut().proto = Some(parent.clone());
    }
    e.set_native_prop(pc, "prototype", promise_proto.clone());
    if let Val::Obj(o) = &promise_proto {
        e.promise_proto = Some(o.clone());
    }

    // -- RegExp (callable constructor; same behavior plain or `new`) --------
    let rc = e.register_native("RegExp", n_regexp, 0);
    // `RegExp.prototype` is an ordinary object holding the methods. Polyfill
    // bundles borrow them through `Function.prototype.call` (`b(ws.exec)`),
    // so these must be real function values, not the receiver-dispatched
    // shortcuts `re.exec` uses.
    let re_proto = Val::new_object();
    for (i, name) in ["exec", "test", "toString"].iter().enumerate() {
        let id = e.register_native(
            &format!("RegExp.prototype.{name}"),
            n_regex_proto,
            i as u16,
        );
        obj_set_own(own(&re_proto), name, Val::Native(id));
    }
    e.set_native_prop(rc, "prototype", re_proto);
    e.set_global("RegExp", Val::Native(rc));

    // -- URL / URLSearchParams --------------------------------------------
    // Both are ordinary prototype-based constructors: the shared prototype
    // object holds the methods, every instance points at it, and the
    // constructor native carries `.prototype` in the native side table.
    let url_proto = Val::new_object();
    for (i, name) in ["toString", "toJSON"].iter().enumerate() {
        let id = e.register_native(&format!("URL.prototype.{name}"), n_url, i as u16);
        obj_set_own(own(&url_proto), name, Val::Native(id));
    }
    let params_proto = Val::new_object();
    for (i, name) in [
        "get",
        "getAll",
        "has",
        "set",
        "append",
        "delete",
        "toString",
        "sort",
        "forEach",
        "entries",
        "keys",
        "values",
    ]
    .iter()
    .enumerate()
    {
        let id = e.register_native(&format!("URLSearchParams.prototype.{name}"), n_params, i as u16);
        obj_set_own(own(&params_proto), name, Val::Native(id));
    }
    let uc = e.register_native("URL", n_url_ctor, 0);
    e.set_native_prop(uc, "prototype", url_proto.clone());
    e.set_global("URL", Val::Native(uc));
    e.url_proto = Some(own(&url_proto).clone());
    let upc = e.register_native("URLSearchParams", n_params_ctor, 0);
    e.set_native_prop(upc, "prototype", params_proto.clone());
    e.set_global("URLSearchParams", Val::Native(upc));
    e.params_proto = Some(own(&params_proto).clone());

    // -- Set / Map ---------------------------------------------------------
    // Prototype-based constructors like URL above: the payload rides in a
    // hidden own property (`__set` / `__map`), methods live on the shared
    // prototype, and `instanceof` matches `.prototype` through the chain.
    // shortcut: no iterator protocol — `values()`/`keys()`/`entries()` return
    // plain arrays, and the hidden payload prop is visible to `Object.keys`
    // and JSON.stringify (a real Set serializes to `{}`).
    let set_proto = Val::new_object();
    for (i, name) in [
        "add", "has", "delete", "clear", "forEach", "values", "keys", "entries",
    ]
    .iter()
    .enumerate()
    {
        let id = e.register_native(&format!("Set.prototype.{name}"), n_set, i as u16);
        obj_set_own(own(&set_proto), name, Val::Native(id));
    }
    let set_size = e.register_native("get Set.prototype.size", n_set, SET_SIZE);
    obj_set_own(
        own(&set_proto),
        "size",
        Val::Acc(Rc::new(AccEntry {
            getter: Some(Val::Native(set_size)),
            setter: None,
        })),
    );
    let sc = e.register_native("Set", n_set_ctor, 0);
    e.set_native_prop(sc, "prototype", set_proto.clone());
    e.set_global("Set", Val::Native(sc));
    e.set_proto = Some(own(&set_proto).clone());

    let map_proto = Val::new_object();
    for (i, name) in [
        "get", "set", "has", "delete", "clear", "forEach", "keys", "values", "entries",
    ]
    .iter()
    .enumerate()
    {
        let id = e.register_native(&format!("Map.prototype.{name}"), n_map, i as u16);
        obj_set_own(own(&map_proto), name, Val::Native(id));
    }
    let map_size = e.register_native("get Map.prototype.size", n_map, MAP_SIZE);
    obj_set_own(
        own(&map_proto),
        "size",
        Val::Acc(Rc::new(AccEntry {
            getter: Some(Val::Native(map_size)),
            setter: None,
        })),
    );
    let mc = e.register_native("Map", n_map_ctor, 0);
    e.set_native_prop(mc, "prototype", map_proto.clone());
    e.set_global("Map", Val::Native(mc));
    e.map_proto = Some(own(&map_proto).clone());

    // -- Symbol ------------------------------------------------------------
    // shortcut: a symbol is a unique plain object carrying a hidden
    // `__symbol` description. Identity comparisons (`===`) and use as a
    // computed property key work because the engine stringifies it as
    // `Symbol(desc)`; there is no real primitive symbol type.
    let sym = e.register_native("Symbol", n_symbol, 0);
    // `Symbol.prototype` exists so feature probes (`"description" in
    // Symbol.prototype`) and `defineProperty` on it do not throw.
    let sym_proto = Val::new_object();
    e.set_native_prop(sym, "prototype", sym_proto.clone());
    if let Val::Obj(o) = &sym_proto {
        let ts = e.register_native("Symbol.prototype.toString", n_symbol_proto, 0);
        let vo = e.register_native("Symbol.prototype.valueOf", n_symbol_proto, 1);
        obj_set_own(o, "toString", Val::Native(ts));
        obj_set_own(o, "valueOf", Val::Native(vo));
        // the real accessor; a data property keeps `"description" in
        // Symbol.prototype` true (feature probes only)
        obj_set_own(o, "description", Val::Undefined);
    }
    for name in [
        "iterator",
        "asyncIterator",
        "toStringTag",
        "hasInstance",
        "species",
        "isConcatSpreadable",
        "match",
        "matchAll",
        "replace",
        "search",
        "split",
        "toPrimitive",
        "unscopables",
        "asyncDispose",
        "dispose",
        "metadata",
    ] {
        let s = new_symbol(name);
        e.set_native_prop(sym, name, s);
    }
    e.set_global("Symbol", Val::Native(sym));

    // -- WeakMap / WeakSet --------------------------------------------------
    // shortcut: same hidden `__map`/`__set` payload as Map/Set (identity
    // comparison, no weak semantics, keys need not be objects).
    let wm_proto = Val::new_object();
    for (i, name) in ["get", "set", "has", "delete"].iter().enumerate() {
        let id = e.register_native(&format!("WeakMap.prototype.{name}"), n_map, i as u16);
        obj_set_own(own(&wm_proto), name, Val::Native(id));
    }
    let wmc = e.register_native("WeakMap", n_weakmap_ctor, 0);
    e.set_native_prop(wmc, "prototype", wm_proto.clone());
    e.set_global("WeakMap", Val::Native(wmc));
    e.weakmap_proto = Some(own(&wm_proto).clone());

    let ws_proto = Val::new_object();
    for (i, name) in ["add", "has", "delete"].iter().enumerate() {
        let id = e.register_native(&format!("WeakSet.prototype.{name}"), n_set, i as u16);
        obj_set_own(own(&ws_proto), name, Val::Native(id));
    }
    let wsc = e.register_native("WeakSet", n_weakset_ctor, 0);
    e.set_native_prop(wsc, "prototype", ws_proto.clone());
    e.set_global("WeakSet", Val::Native(wsc));
    e.weakset_proto = Some(own(&ws_proto).clone());

    // -- TextEncoder / TextDecoder -----------------------------------------
    // shortcut: UTF-8 only (the default), no `stream`/`ignoreBOM` options.
    // `encode` returns a plain array of byte values (no TypedArray type).
    let te_proto = Val::new_object();
    let te_enc = e.register_native("TextEncoder.prototype.encode", n_text, 0);
    obj_set_own(own(&te_proto), "encode", Val::Native(te_enc));
    let tec = e.register_native("TextEncoder", n_text_ctor, 0);
    e.set_native_prop(tec, "prototype", te_proto.clone());
    e.set_global("TextEncoder", Val::Native(tec));
    e.text_encoder_proto = Some(own(&te_proto).clone());

    let td_proto = Val::new_object();
    let td_dec = e.register_native("TextDecoder.prototype.decode", n_text, 1);
    obj_set_own(own(&td_proto), "decode", Val::Native(td_dec));
    obj_set_own(own(&td_proto), "encoding", Val::Str(Rc::from("utf-8")));
    let tdc = e.register_native("TextDecoder", n_text_ctor, 1);
    e.set_native_prop(tdc, "prototype", td_proto.clone());
    e.set_global("TextDecoder", Val::Native(tdc));
    e.text_decoder_proto = Some(own(&td_proto).clone());

    // -- base64 + Uint8Array/Buffer stand-ins -------------------------------
    // shortcut: a "typed array" is a plain JS array of byte values with
    // `toBase64`/`buffer`/`byteLength` own props; there is no ArrayBuffer
    // type and no element-size coercion beyond `& 0xff`.
    let u8_to_b64 = e.register_native("Uint8Array.prototype.toBase64", n_u8, 0);
    let u8_from_b64 = e.register_native("Uint8Array.fromBase64", n_u8, 1);
    e.uint8_to_b64 = u8_to_b64;
    let u8_proto = Val::new_object();
    let u8c = e.register_native("Uint8Array", n_u8_ctor, 0);
    e.set_native_prop(u8c, "prototype", u8_proto);
    e.set_native_prop(u8c, "fromBase64", Val::Native(u8_from_b64));
    e.set_global("Uint8Array", Val::Native(u8c));

    let buf_to_string = e.register_native("Buffer.prototype.toString", n_buffer, 0);
    e.buffer_to_string = buf_to_string;
    let buf_from = e.register_native("Buffer.from", n_buffer, 1);
    let buf = Val::new_object();
    obj_set_own(own(&buf), "from", Val::Native(buf_from));
    e.set_global("Buffer", buf);

    let atob_id = e.register_native("atob", n_b64_global, 0);
    let btoa_id = e.register_native("btoa", n_b64_global, 1);
    e.set_global("atob", Val::Native(atob_id));
    e.set_global("btoa", Val::Native(btoa_id));

    // -- ArrayBuffer / SharedArrayBuffer ------------------------------------
    // shortcut: a byteLength-bearing shell; no real backing store. The
    // accessor exists so `Object.getOwnPropertyDescriptor(...).get` works.
    let ab_len = e.register_native("get ArrayBuffer.prototype.byteLength", n_ab, 0);
    let ab_proto = Val::new_object();
    obj_set_own(
        own(&ab_proto),
        "byteLength",
        Val::Acc(Rc::new(AccEntry {
            getter: Some(Val::Native(ab_len)),
            setter: None,
        })),
    );
    let ab_ctor = e.register_native("ArrayBuffer", n_ab, 1);
    e.set_native_prop(ab_ctor, "prototype", ab_proto.clone());
    let ab_view = e.register_native("ArrayBuffer.isView", n_ab, 2);
    e.set_native_prop(ab_ctor, "isView", Val::Native(ab_view));
    e.set_global("ArrayBuffer", Val::Native(ab_ctor));
    e.set_global("SharedArrayBuffer", Val::Native(ab_ctor));
    e.arraybuffer_proto = Some(own(&ab_proto).clone());


    // -- global aliases ----------------------------------------------------
    let g = Val::Obj(e.global_obj.clone());
    e.set_global("globalThis", g.clone());
    e.set_global("window", g.clone());
    e.set_global("self", g);
}

fn own(v: &Val) -> &Rc<RefCell<Obj>> {
    match v {
        Val::Obj(o) => o,
        _ => unreachable!("builtin installed on non-object"),
    }
}

/// The array prototype object, for engine-side array creation.
pub(crate) fn array_proto(e: &JsEngine) -> Option<Rc<RefCell<Obj>>> {
    match e.get_global("__arrayProto") {
        Val::Obj(o) => Some(o),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// natives
// ---------------------------------------------------------------------------

fn n_console(_e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    // match engine logging style: script console output goes to stderr
    let parts: Vec<String> = args.iter().map(|v| v.inspect()).collect();
    eprintln!("{}", parts.join(" "));
    Ok(Val::Undefined)
}

const M_FLOOR: u16 = 0;
const M_CEIL: u16 = 1;
const M_ROUND: u16 = 2;
const M_ABS: u16 = 3;
const M_MAX: u16 = 4;
const M_MIN: u16 = 5;
const M_POW: u16 = 6;
const M_SQRT: u16 = 7;
const M_TRUNC: u16 = 8;
const M_RANDOM: u16 = 9;
const M_SIGN: u16 = 10;

fn n_math(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    Ok(match idx {
        M_FLOOR => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).floor()),
        M_CEIL => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).ceil()),
        // Math.round: rounds half UP toward +inf (JS semantics, not banker's)
        M_ROUND => Val::Num(args.first().map_or(f64::NAN, |v| (v.to_number() + 0.5).floor())),
        M_ABS => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).abs()),
        M_MAX => Val::Num(
            args.iter()
                .map(|v| v.to_number())
                .fold(f64::NEG_INFINITY, f64::max),
        ),
        M_MIN => Val::Num(
            args.iter()
                .map(|v| v.to_number())
                .fold(f64::INFINITY, f64::min),
        ),
        M_POW => Val::Num(
            args.first().map_or(f64::NAN, |v| v.to_number())
                .powf(args.get(1).map_or(f64::NAN, |v| v.to_number())),
        ),
        M_SQRT => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).sqrt()),
        M_TRUNC => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).trunc()),
        M_RANDOM => Val::Num(e.random_f64()),
        M_SIGN => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).signum()),
        _ => Val::Undefined,
    })
}

// -- JSON -------------------------------------------------------------------

const J_PARSE: u16 = 0;
const J_STRINGIFY: u16 = 1;

fn n_json(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        J_PARSE => {
            let s = args
                .first()
                .map_or("".to_string(), |v| v.to_string());
            let mut jp = Jp {
                b: s.as_bytes(),
                i: 0,
            };
            jp.ws();
            let v = jp.value()?;
            jp.ws();
            if jp.i != jp.b.len() {
                return Err(JsError::throw("SyntaxError: JSON.parse trailing characters"));
            }
            Ok(v)
        }
        J_STRINGIFY => {
            let v = match args.first() {
                None => return Ok(Val::Undefined),
                Some(v) => v,
            };
            let mut out = String::new();
            // depth cap doubles as the cycle guard (no real cycle detection)
            if json_write(e, v, 0, &mut out)? {
                Ok(Val::Str(Rc::from(out.as_str())))
            } else {
                Ok(Val::Undefined)
            }
        }
        _ => Ok(Val::Undefined),
    }
}

/// write JSON for v; returns false if v is not serializable (undefined etc.)
fn json_write(e: &mut JsEngine, v: &Val, depth: u32, out: &mut String) -> Result<bool, JsError> {
    if depth > 200 {
        return Err(JsError::throw(
            "RangeError: circular or too deeply nested structure in JSON.stringify",
        ));
    }
    match v {
        Val::Undefined | Val::Func(_) | Val::Native(_) | Val::Method(_) | Val::Acc(_) => {
            return Ok(false)
        }
        Val::Null => out.push_str("null"),
        Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Val::Num(n) => {
            if n.is_finite() {
                out.push_str(&n.to_string());
            } else {
                out.push_str("null"); // NaN/Infinity -> null (JS behavior)
            }
        }
        Val::Str(s) => json_str(s, out),
        Val::Obj(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => {
                    out.push('[');
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        if !json_write(e, item, depth + 1, out)? {
                            out.push_str("null"); // holes/undefined -> null
                        }
                    }
                    out.push(']');
                }
                ObjKind::Date(ms) => match fmt_date_iso(*ms) {
                    // matches real engines' Date#toJSON path closely enough
                    Some(s) => json_str(&Rc::from(s.as_str()), out),
                    None => out.push_str("null"),
                },
                ObjKind::Error { .. } | ObjKind::Promise(_) => {
                    // shortcut: no toJSON support; these serialize empty
                    out.push_str("{}");
                }
                // regex/generator instances: no own enumerable props -> {}
                ObjKind::Regex(_) | ObjKind::Generator(_) | ObjKind::Style(_) => {
                    out.push_str("{}");
                }
                ObjKind::Plain => {
                    out.push('{');
                    let mut first = true;
                    for (k, val) in b.props.iter() {
                        if matches!(val, Val::Undefined | Val::Func(_) | Val::Native(_)) {
                            continue; // skipped like real JSON.stringify
                        }
                        if !first {
                            out.push(',');
                        }
                        first = false;
                        json_str(k, out);
                        out.push(':');
                        if !json_write(e, val, depth + 1, out)? {
                            out.push_str("null");
                        }
                    }
                    out.push('}');
                }
            }
        }
    }
    Ok(true)
}

fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// minimal recursive-descent JSON reader (numbers, strings w/ escapes,
/// arrays, objects, true/false/null)
struct Jp<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Jp<'a> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn err<T>(&self, msg: &str) -> Result<T, JsError> {
        Err(JsError::throw(&format!("SyntaxError: {}", msg)))
    }

    fn lit(&mut self, s: &str) -> bool {
        if self.b[self.i..].starts_with(s.as_bytes()) {
            self.i += s.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Val, JsError> {
        match self.b.get(self.i) {
            None => self.err("unexpected end of JSON"),
            Some(b'{') => {
                self.i += 1;
                let mut props = Vec::new();
                self.ws();
                if self.lit("}") {
                    return Ok(Val::new_object());
                }
                loop {
                    self.ws();
                    let k = match self.string()? {
                        Val::Str(s) => s,
                        _ => return self.err("bad key"),
                    };
                    self.ws();
                    if !self.lit(":") {
                        return self.err("expected `:`");
                    }
                    self.ws();
                    let v = self.value()?;
                    props.push((k, v));
                    self.ws();
                    if self.lit(",") {
                        continue;
                    }
                    if self.lit("}") {
                        break;
                    }
                    return self.err("expected `,` or `}`");
                }
                let o = Val::new_object();
                for (k, v) in props {
                    obj_set_own(own(&o), &k, v);
                }
                Ok(o)
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.lit("]") {
                    return Ok(Val::new_array(items));
                }
                loop {
                    self.ws();
                    items.push(self.value()?);
                    self.ws();
                    if self.lit(",") {
                        continue;
                    }
                    if self.lit("]") {
                        break;
                    }
                    return self.err("expected `,` or `]`");
                }
                Ok(Val::new_array(items))
            }
            Some(b'"') => self.string(),
            Some(b't') if self.lit("true") => Ok(Val::Bool(true)),
            Some(b'f') if self.lit("false") => Ok(Val::Bool(false)),
            Some(b'n') if self.lit("null") => Ok(Val::Null),
            Some(_) => self.number(),
        }
    }

    fn number(&mut self) -> Result<Val, JsError> {
        let start = self.i;
        if matches!(self.b.get(self.i), Some(b'-' | b'+')) {
            self.i += 1;
        }
        while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit() || *c == b'.' || *c == b'e' || *c == b'E' || *c == b'-' || *c == b'+') {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(Val::Num)
            .ok_or_else(|| JsError::throw("SyntaxError: invalid number in JSON"))
    }

    fn string(&mut self) -> Result<Val, JsError> {
        if !self.lit("\"") {
            return self.err("expected string");
        }
        let mut out = String::new();
        loop {
            match self.b.get(self.i) {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(Val::Str(Rc::from(out.as_str())));
                }
                Some(b'\\') => {
                    self.i += 1;
                    let e = *self.b.get(self.i).ok_or_else(|| JsError::throw("SyntaxError: bad escape"))?;
                    self.i += 1;
                    match e {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'u' => {
                            let h = std::str::from_utf8(
                                self.b.get(self.i..self.i + 4).ok_or_else(|| JsError::throw("SyntaxError: bad \\u"))?,
                            )
                            .map_err(|_| JsError::throw("SyntaxError: bad \\u"))?;
                            let u = u32::from_str_radix(h, 16)
                                .map_err(|_| JsError::throw("SyntaxError: bad \\u"))?;
                            self.i += 4;
                            // combine surrogate pairs
                            if (0xD800..0xDC00).contains(&u)
                                && self.b.get(self.i) == Some(&b'\\')
                                && self.b.get(self.i + 1) == Some(&b'u')
                            {
                                let h2 = std::str::from_utf8(
                                    self.b.get(self.i + 2..self.i + 6).ok_or_else(|| JsError::throw("SyntaxError: bad \\u"))?,
                                )
                                .map_err(|_| JsError::throw("SyntaxError: bad \\u"))?;
                                if let Ok(lo) = u32::from_str_radix(h2, 16) {
                                    if (0xDC00..0xE000).contains(&lo) {
                                        self.i += 6;
                                        let c = 0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00);
                                        out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                                        continue;
                                    }
                                }
                            }
                            out.push(char::from_u32(u).unwrap_or('\u{fffd}'));
                        }
                        _ => return self.err("unknown escape"),
                    }
                }
                Some(_) => {
                    // copy one UTF-8 char
                    let rest = &self.b[self.i..];
                    let s = std::str::from_utf8(rest)
                        .map_err(|_| JsError::throw("SyntaxError: invalid UTF-8 in JSON"))?;
                    let c = s.chars().next().unwrap();
                    out.push(c);
                    self.i += c.len_utf8();
                }
            }
        }
    }
}

// -- Object statics ---------------------------------------------------------

const O_KEYS: u16 = 0;
const O_VALUES: u16 = 1;
const O_ENTRIES: u16 = 2;
const O_ASSIGN: u16 = 3;
const O_DEFINE_PROP: u16 = 4;
const O_SET_PROTO: u16 = 5;
const O_GET_PROTO: u16 = 6;
const O_CREATE: u16 = 7;
const O_PROP_NAMES: u16 = 8;
const O_FROM_ENTRIES: u16 = 9;
const O_FREEZE: u16 = 10;
const O_SEAL: u16 = 11;
const O_IS: u16 = 12;
const O_IS_FROZEN: u16 = 13;
const O_IS_SEALED: u16 = 14;
const O_HAS_OWN: u16 = 15;
const O_GET_DESC: u16 = 16;
const O_GET_DESCS: u16 = 17;
const O_DEFINE_PROPS: u16 = 18;
const O_CALL: u16 = 19;

/// Apply one `Object.defineProperty` descriptor to `target`.
fn define_prop(
    e: &mut JsEngine,
    target: &Val,
    key: &str,
    d: Option<&Val>,
) -> Result<(), JsError> {
    let value = match d {
        Some(Val::Obj(ob)) => {
            let b = ob.borrow();
            b.props
                .iter()
                .find(|(k, _)| k.as_ref() == "value")
                .map(|(_, v)| v.clone())
        }
        _ => None,
    };
    // accessors read through get_member (callable `get`/`set`)
    let get = match d {
        Some(d) => get_member(e, d, "get")?,
        None => Val::Undefined,
    };
    let set = match d {
        Some(d) => get_member(e, d, "set")?,
        None => Val::Undefined,
    };
    let acc = if is_callable(&get) || is_callable(&set) {
        Some(Val::Acc(Rc::new(AccEntry {
            getter: if is_callable(&get) { Some(get) } else { None },
            setter: if is_callable(&set) { Some(set) } else { None },
        })))
    } else {
        None
    };
    // A descriptor without `value` and without `get`/`set` modifies flags
    // only: the current value has to survive. `Object.defineProperty(fn,
    // "prototype", { writable: false })` used to wipe the prototype to
    // `undefined` — exactly what a published polyfill bundle does, which
    // then broke its own `t.prototype` reads. Only an explicit `value`
    // (including `value: undefined`) replaces the property.
    let v = match (acc, value) {
        (Some(a), _) => a,
        (None, Some(v)) => v,
        (None, None) => {
            let existing = match target {
                Val::Obj(o) => o
                    .borrow()
                    .props
                    .iter()
                    .find(|(k, _)| k.as_ref() == key)
                    .map(|(_, x)| x.clone()),
                Val::Func(f) => f
                    .props
                    .borrow()
                    .iter()
                    .find(|(k, _)| k.as_ref() == key)
                    .map(|(_, x)| x.clone())
                    .filter(|x| !matches!(x, Val::Undefined)),
                _ => None,
            };
            match existing {
                Some(x) => x,
                // every non-arrow function owns a `prototype` object, even
                // before anyone read it (it is materialized lazily)
                None => match target {
                    Val::Func(f) if key == "prototype" => {
                        Val::Obj(super::interp::ensure_prototype(f))
                    }
                    _ => Val::Undefined,
                },
            }
        }
    };
    match target {
        Val::Obj(o) => obj_set_own(o, key, v),
        Val::Func(f) => {
            let mut props = f.props.borrow_mut();
            props.retain(|(k, _)| k.as_ref() != key);
            props.push((Rc::from(key), v));
        }
        // built-in constructors keep their own props in the native side
        // table; polyfills legitimately patch them (`Object.defineProperty(
        // Symbol, "dispose", …)`)
        Val::Native(id) => e.set_native_prop(*id, key, v),
        _ => {
            return Err(JsError::throw(
                "TypeError: Object.defineProperty called on non-object",
            ))
        }
    }
    Ok(())
}

fn n_object(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        // `Object(x)` — objects pass through unchanged, nullish builds a
        // fresh object. shortcut: a primitive comes back unwrapped (no boxed
        // String/Number wrappers in this engine).
        O_CALL => Ok(match args.first() {
            None | Some(Val::Null) | Some(Val::Undefined) => {
                proto_instance(e.object_proto.clone())
            }
            Some(v) => v.clone(),
        }),
        O_KEYS | O_VALUES | O_ENTRIES => {
            // shortcut: arrays yield index strings (matches real engines)
            let o = args.first().cloned().unwrap_or(Val::Undefined);
            let pairs: Vec<(Rc<str>, Val)> = match &o {
                Val::Obj(obj) => {
                    let b = obj.borrow();
                    match &b.kind {
                        ObjKind::Array(items) => (0..items.len())
                            .map(|i| (Rc::from(i.to_string().as_str()), items[i].clone()))
                            .collect(),
                        _ => b.props.clone(),
                    }
                }
                _ => Vec::new(),
            };
            Ok(match idx {
                O_KEYS => Val::new_array(pairs.into_iter().map(|(k, _)| Val::Str(k)).collect()),
                O_VALUES => Val::new_array(pairs.into_iter().map(|(_, v)| v).collect()),
                _ => Val::new_array(
                    pairs
                        .into_iter()
                        .map(|(k, v)| Val::new_array(vec![Val::Str(k), v]))
                        .collect(),
                ),
            })
        }
        O_ASSIGN => {
            let target = args.first().cloned().unwrap_or_else(Val::new_object);
            for src in args.iter().skip(1) {
                if let Val::Obj(s) = src {
                    let pairs: Vec<(Rc<str>, Val)> = {
                        let b = s.borrow();
                        b.props.clone()
                    };
                    // set_member, not own(): the target may be a class
                    // function (`Object.assign(Cls, {...})`) or a primitive
                    for (k, v) in pairs {
                        set_member(e, &target, &k, v)?;
                    }
                }
            }
            Ok(target)
        }
        O_DEFINE_PROP => {
            // Object.defineProperty(target, key, { value?, get?, set? })
            // shortcut: writable/enumerable/configurable flags are accepted
            // and ignored - accessors and values install normally
            let target = args.first().cloned().unwrap_or(Val::Undefined);
            let key = args.get(1).map(|v| v.to_string()).unwrap_or_default();
            define_prop(e, &target, &key, args.get(2))?;
            Ok(target)
        }
        O_DEFINE_PROPS => {
            // Object.defineProperties(target, { key: descriptor, ... })
            let target = args.first().cloned().unwrap_or(Val::Undefined);
            let descs: Vec<(Rc<str>, Val)> = match args.get(1) {
                Some(Val::Obj(d)) => d.borrow().props.clone(),
                _ => Vec::new(),
            };
            for (k, d) in descs {
                define_prop(e, &target, &k, Some(&d))?;
            }
            Ok(target)
        }
        O_SET_PROTO => {
            // Object.setPrototypeOf(target, proto) - the only way a function
            // object gets a [[Prototype]] (class `extends` lowering)
            let target = args.first().cloned().unwrap_or(Val::Undefined);
            let proto = args.get(1).cloned().unwrap_or(Val::Null);
            let next = if matches!(proto, Val::Null) {
                None
            } else {
                Some(proto.clone())
            };
            match &target {
                Val::Obj(o) => {
                    o.borrow_mut().proto = match proto {
                        Val::Obj(p) => Some(p),
                        _ => None,
                    }
                }
                Val::Func(f) => *f.proto.borrow_mut() = next,
                _ => {}
            }
            Ok(target)
        }
        O_GET_PROTO => {
            let target = args.first().cloned().unwrap_or(Val::Undefined);
            Ok(match &target {
                Val::Obj(o) => o.borrow().proto.clone().map(Val::Obj).unwrap_or(Val::Null),
                Val::Func(f) => f.proto.borrow().clone().unwrap_or(Val::Null),
                _ => Val::Null,
            })
        }
        O_FROM_ENTRIES => {
            // Object.fromEntries(iterable of [key, value] pairs). The engine
            // has no generic iterators, so arrays of 2-item arrays are the
            // accepted shape (what `new Map(...)`-free code and Vite's
            // `Object.entries(...).map(...)` produce).
            let src = args.first().cloned().unwrap_or(Val::Undefined);
            let pairs: Vec<Val> = match &src {
                Val::Obj(o) => match &o.borrow().kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            let out = Val::new_object();
            for pair in pairs {
                let pair = match &pair {
                    Val::Obj(o) => match &o.borrow().kind {
                        ObjKind::Array(items) if items.len() >= 2 => {
                            (items[0].to_string(), items[1].clone())
                        }
                        _ => {
                            return Err(JsError::throw(
                                "TypeError: Object.fromEntries entry is not a [key, value] pair",
                            ))
                        }
                    },
                    _ => {
                        return Err(JsError::throw(
                            "TypeError: Object.fromEntries entry is not a [key, value] pair",
                        ))
                    }
                };
                set_member(e, &out, &pair.0, pair.1)?;
            }
            Ok(out)
        }
        O_FREEZE | O_SEAL => {
            // shortcut: no integrity levels — freeze/seal are identity
            Ok(args.first().cloned().unwrap_or(Val::Undefined))
        }
        O_IS => {
            // SameValue: NaN equals NaN, +0 differs from -0
            let (a, b) = (
                args.first().cloned().unwrap_or(Val::Undefined),
                args.get(1).cloned().unwrap_or(Val::Undefined),
            );
            let same = match (&a, &b) {
                (Val::Num(x), Val::Num(y)) => {
                    (x.is_nan() && y.is_nan()) || (x == y && x.is_sign_negative() == y.is_sign_negative())
                }
                _ => strict_eq(&a, &b),
            };
            Ok(Val::Bool(same))
        }
        O_IS_FROZEN | O_IS_SEALED => Ok(Val::Bool(!matches!(
            args.first(),
            Some(Val::Obj(_))
        ))),
        O_HAS_OWN => {
            let o = args.first().cloned().unwrap_or(Val::Undefined);
            let k = arg_str(args, 1);
            Ok(Val::Bool(match &o {
                Val::Obj(b) => obj_has_own(b, &k),
                _ => false,
            }))
        }
        O_GET_DESC => {
            let o = args.first().cloned().unwrap_or(Val::Undefined);
            let k = arg_str(args, 1);
            Ok(match &o {
                Val::Obj(b) => own_desc(b, &k).unwrap_or(Val::Undefined),
                Val::Func(f) => fn_desc(f, &k).unwrap_or(Val::Undefined),
                // built-in constructors keep their properties in the native
                // side table (`Symbol.asyncDispose`, `String.prototype`, …)
                Val::Native(id) => match e.native_prop(*id, &k) {
                    Some(v) => desc_from_value(v),
                    None => Val::Undefined,
                },
                _ => Val::Undefined,
            })
        }
        O_GET_DESCS => {
            let o = args.first().cloned().unwrap_or(Val::Undefined);
            let out = Val::new_object();
            if let Val::Obj(b) = &o {
                let keys: Vec<Rc<str>> = match &b.borrow().kind {
                    ObjKind::Array(items) => (0..items.len())
                        .map(|i| Rc::from(i.to_string().as_str()))
                        .collect(),
                    _ => b.borrow().props.iter().map(|(k, _)| k.clone()).collect(),
                };
                for k in keys {
                    if let Some(d) = own_desc(b, &k) {
                        set_member(e, &out, &k, d)?;
                    }
                }
            }
            Ok(out)
        }
        O_CREATE => {
            // shortcut: a `null` prototype is stored as "not attached yet", so
            // the new object still picks up Object.prototype on first access
            // (this engine only enumerates own props, so dictionaries behave).
            let proto = match args.first() {
                Some(Val::Obj(p)) => Some(p.clone()),
                _ => None,
            };
            Ok(Val::Obj(Rc::new(RefCell::new(Obj {
                kind: ObjKind::Plain,
                props: Vec::new(),
                proto,
            }))))
        }
        O_PROP_NAMES => {
            let o = args.first().cloned().unwrap_or(Val::Undefined);
            let keys: Vec<Val> = match &o {
                Val::Obj(b) => {
                    let b = b.borrow();
                    match &b.kind {
                        ObjKind::Array(items) => (0..items.len())
                            .map(|i| Val::Str(Rc::from(i.to_string().as_str())))
                            .collect(),
                        _ => b.props.iter().map(|(k, _)| Val::Str(k.clone())).collect(),
                    }
                }
                _ => Vec::new(),
            };
            Ok(Val::new_array(keys))
        }
        _ => Ok(Val::Undefined),
    }
}

const OP_TOSTRING: u16 = 0;
const OP_HASOWN: u16 = 1;
const OP_IS_ENUM: u16 = 2;
const OP_VALUEOF: u16 = 3;
const OP_IS_PROTO: u16 = 4;

fn n_objproto(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        OP_TOSTRING => Ok(Val::Str(Rc::from(match this {
            Val::Obj(o) => match &o.borrow().kind {
                ObjKind::Plain => "[object Object]",
                ObjKind::Array(_) => "[object Array]",
                ObjKind::Error { .. } => "[object Error]",
                ObjKind::Date(_) => "[object Date]",
                ObjKind::Promise(_) => "[object Promise]",
                ObjKind::Regex(_) => "[object RegExp]",
                ObjKind::Generator(_) => "[object Generator]",
                ObjKind::Style(_) => "[object CSSStyleDeclaration]",
            },
            // callables report their tag too (`classof`-style feature tests
            // in published bundles rely on it)
            Val::Func(_) | Val::Native(_) | Val::Method(_) | Val::Acc(_) => {
                "[object Function]"
            }
            _ => "[object Object]",
        }))),
        OP_HASOWN => {
            let key = args.first().map_or("".to_string(), |v| v.to_string());
            Ok(Val::Bool(match this {
                Val::Obj(o) => {
                    let b = o.borrow();
                    b.props.iter().any(|(k, _)| k.as_ref() == key.as_str())
                        || matches!(&b.kind, ObjKind::Array(items) if key.parse::<usize>().map_or(false, |i| i < items.len()))
                }
                // function own props: `name`/`length` always, `prototype`
                // only on real functions (arrows and bound wrappers lack it)
                Val::Func(c) => match key.as_str() {
                    "name" | "length" => true,
                    "prototype" => {
                        !c.is_arrow && !matches!(c.def.body, FnBody::Bound { .. })
                    }
                    _ => c.props.borrow().iter().any(|(k, _)| k.as_ref() == key.as_str()),
                },
                _ => false,
            }))
        }
        // every own prop this engine stores is enumerable (arrays included)
        OP_IS_ENUM => {
            let key = args.first().map_or("".to_string(), |v| v.to_string());
            Ok(Val::Bool(match this {
                Val::Obj(o) => {
                    let b = o.borrow();
                    b.props.iter().any(|(k, _)| k.as_ref() == key.as_str())
                        || matches!(&b.kind, ObjKind::Array(items) if key.parse::<usize>().map_or(false, |i| i < items.len()))
                }
                Val::Func(c) => c.props.borrow().iter().any(|(k, _)| k.as_ref() == key.as_str()),
                _ => false,
            }))
        }
        OP_VALUEOF => Ok(this.clone()),
        OP_IS_PROTO => {
            let want = match this {
                Val::Obj(o) => o.clone(),
                _ => return Ok(Val::Bool(false)),
            };
            let start = match args.first() {
                Some(Val::Obj(o)) => o.clone(),
                _ => return Ok(Val::Bool(false)),
            };
            // an object is never its own prototype
            if Rc::ptr_eq(&start, &want) {
                return Ok(Val::Bool(false));
            }
            let mut cur = start.borrow().proto.clone();
            for _ in 0..PROTO_WALK_CAP {
                let c = match cur {
                    None => break,
                    Some(c) => c,
                };
                if Rc::ptr_eq(&c, &want) {
                    return Ok(Val::Bool(true));
                }
                cur = c.borrow().proto.clone();
            }
            // plain objects attach Object.prototype lazily; a chain that ends
            // without one still stands for `Object.prototype`
            if let Some(op) = &e.object_proto {
                if Rc::ptr_eq(op, &want) {
                    return Ok(Val::Bool(true));
                }
            }
            Ok(Val::Bool(false))
        }
        _ => Ok(Val::Undefined),
    }
}

// -- Array.prototype --------------------------------------------------------

const A_PUSH: u16 = 0;
const A_POP: u16 = 1;
const A_SHIFT: u16 = 2;
const A_UNSHIFT: u16 = 3;
const A_SLICE: u16 = 4;
const A_CONCAT: u16 = 5;
const A_JOIN: u16 = 6;
const A_INDEXOF: u16 = 7;
const A_INCLUDES: u16 = 8;
const A_MAP: u16 = 9;
const A_FILTER: u16 = 10;
const A_FOREACH: u16 = 11;
const A_REDUCE: u16 = 12;
const A_SOME: u16 = 13;
const A_EVERY: u16 = 14;
const A_SORT: u16 = 15;
const A_REVERSE: u16 = 16;
const A_SPLICE: u16 = 17;
const A_LASTINDEXOF: u16 = 18;
const A_ISARRAY: u16 = 100;

/// Own-property check that also understands array indices/length.
fn obj_has_own(o: &Rc<RefCell<Obj>>, key: &str) -> bool {
    let b = o.borrow();
    if let ObjKind::Array(items) = &b.kind {
        if key == "length" {
            return true;
        }
        if let Ok(i) = key.parse::<usize>() {
            return i < items.len();
        }
    }
    b.props.iter().any(|(k, _)| k.as_ref() == key)
}

/// Descriptor object for a data property (the engine tracks no writability /
/// enumerability, so those always report `true`) or an accessor pair.
fn desc_from_value(v: Val) -> Val {
    let d = Val::new_object();
    match v {
        Val::Acc(a) => {
            obj_set_own(
                own(&d),
                "get",
                a.getter.clone().unwrap_or(Val::Undefined),
            );
            obj_set_own(
                own(&d),
                "set",
                a.setter.clone().unwrap_or(Val::Undefined),
            );
        }
        other => {
            obj_set_own(own(&d), "value", other);
            obj_set_own(own(&d), "writable", Val::Bool(true));
        }
    }
    obj_set_own(own(&d), "enumerable", Val::Bool(true));
    obj_set_own(own(&d), "configurable", Val::Bool(true));
    d
}

/// `Object.getOwnPropertyDescriptor` shape: data properties always report
/// writable/enumerable/configurable (the engine tracks none of those).
fn own_desc(o: &Rc<RefCell<Obj>>, key: &str) -> Option<Val> {
    let b = o.borrow();
    let v = if let ObjKind::Array(items) = &b.kind {
        if key == "length" {
            Some(Val::Num(items.len() as f64))
        } else {
            key.parse::<usize>().ok().and_then(|i| items.get(i).cloned())
        }
    } else {
        None
    }
    .or_else(|| {
        b.props
            .iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| v.clone())
    })?;
    Some(desc_from_value(v))
}

/// Own descriptor on a closure (function) — its props plus the implicit
/// `prototype` every non-arrow function owns.
fn fn_desc(f: &Rc<Closure>, key: &str) -> Option<Val> {
    let found = f
        .props
        .borrow()
        .iter()
        .find(|(k, _)| k.as_ref() == key)
        .map(|(_, v)| v.clone());
    match found {
        Some(v) => Some(desc_from_value(v)),
        None if key == "prototype" => {
            Some(desc_from_value(Val::Obj(super::interp::ensure_prototype(f))))
        }
        None => None,
    }
}

fn need_array(this: &Val) -> Result<Rc<RefCell<Obj>>, JsError> {    match this {
        Val::Obj(o) => {
            if matches!(o.borrow().kind, ObjKind::Array(_)) {
                Ok(o.clone())
            } else {
                Err(JsError::throw("TypeError: array method on non-array"))
            }
        }
        _ => Err(JsError::throw("TypeError: array method on non-array")),
    }
}

/// `Array(n)`, `new Array(...)`, `Array.of(...)`, `Array.from(items, mapFn?)`.
/// shortcut: arrays are dense, so a huge `Array(n)` length is clamped to
/// `MAX_ARRAY_FILL` holes instead of building a sparse array.
fn n_array_call(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    const MAX_ARRAY_FILL: usize = 100_000;
    match idx {
        // Array(n) / Array(a, b, ...): one number means a length
        0 => {
            if args.len() == 1 {
                if let Val::Num(n) = args[0] {
                    let len = if n.is_finite() && n > 0.0 { n as usize } else { 0 };
                    return Ok(Val::new_array(vec![Val::Undefined; len.min(MAX_ARRAY_FILL)]));
                }
            }
            Ok(Val::new_array(args.to_vec()))
        }
        // Array.of(a, b, ...): the arguments ARE the elements
        1 => Ok(Val::new_array(args.to_vec())),
        // Array.from(iterable, mapFn?, thisArg?): array-likes and strings
        // go through the same spread source as `for..of`
        _ => {
            let mut items = match args.first() {
                None => Vec::new(),
                Some(v) => match super::interp::for_of_items(v) {
                    Ok(items) => items,
                    // not iterable: accept an array-like (`length` + index
                    // keys), which `for..of` rightly rejects
                    Err(_) => {
                        let n = get_member(e, v, "length")?.to_number();
                        let mut out = Vec::new();
                        if n.is_finite() && n > 0.0 {
                            for i in 0..(n as usize).min(MAX_ARRAY_FILL) {
                                out.push(get_member(e, v, &i.to_string())?);
                            }
                        }
                        out
                    }
                },
            };
            if let Some(f) = args.get(1).filter(|f| is_callable(f)).cloned() {
                let mut mapped = Vec::with_capacity(items.len());
                for (i, v) in items.into_iter().enumerate() {
                    mapped.push(e.call(f.clone(), Val::Undefined, vec![v, Val::Num(i as f64)])?);
                }
                items = mapped;
            }
            Ok(Val::new_array(items))
        }
    }
}

fn n_array(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == A_ISARRAY {
        // static: inspects the ARGUMENT, not `this`
        return Ok(Val::Bool(match args.first() {
            Some(Val::Obj(o)) => matches!(o.borrow().kind, ObjKind::Array(_)),
            _ => false,
        }));
    }
    let o = need_array(this)?;
    Ok(match idx {
        A_SPLICE => {
            let len = match &o.borrow().kind {
                ObjKind::Array(items) => items.len(),
                _ => 0,
            };
            let n = len as f64;
            let start = args.first().map_or(0.0, |v| v.to_number());
            let start = if start.is_nan() {
                0
            } else if start < 0.0 {
                (n + start).max(0.0) as usize
            } else {
                (start as usize).min(len)
            };
            let del = if args.len() < 2 {
                len - start
            } else {
                let d = args[1].to_number();
                if d.is_nan() || d < 0.0 {
                    0
                } else {
                    (d as usize).min(len - start)
                }
            };
            let inserted: Vec<Val> = args.iter().skip(2).cloned().collect();
            let (removed, newlen) = {
                let mut b = o.borrow_mut();
                match &mut b.kind {
                    ObjKind::Array(items) => {
                        let removed: Vec<Val> = items.splice(start..start + del, inserted).collect();
                        (removed, items.len())
                    }
                    _ => (Vec::new(), 0),
                }
            };
            let _ = newlen;
            Val::new_array(removed)
        }
        A_PUSH => {
            // borrow, extend, drop — never hold a borrow across a JS call
            let len = {
                let mut b = o.borrow_mut();
                match &mut b.kind {
                    ObjKind::Array(items) => {
                        items.extend(args.iter().cloned());
                        items.len()
                    }
                    _ => 0,
                }
            };
            Val::Num(len as f64)
        }
        A_POP => {
            let v = {
                let mut b = o.borrow_mut();
                match &mut b.kind {
                    ObjKind::Array(items) => items.pop(),
                    _ => None,
                }
            };
            v.unwrap_or(Val::Undefined)
        }
        A_SHIFT => {
            let v = {
                let mut b = o.borrow_mut();
                match &mut b.kind {
                    ObjKind::Array(items) => if items.is_empty() { None } else { Some(items.remove(0)) },
                    _ => None,
                }
            };
            v.unwrap_or(Val::Undefined)
        }
        A_UNSHIFT => {
            let len = {
                let mut b = o.borrow_mut();
                match &mut b.kind {
                    ObjKind::Array(items) => {
                        for (i, a) in args.iter().enumerate() {
                            items.insert(i, a.clone());
                        }
                        items.len()
                    }
                    _ => 0,
                }
            };
            Val::Num(len as f64)
        }
        A_SLICE => {
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            let (a, bnd) = slice_bounds(
                args.first().map_or(0.0, |v| v.to_number()),
                args.get(1),
                items.len() as i64,
            );
            Val::new_array(items[a..bnd].to_vec())
        }
        A_CONCAT => {
            let mut out = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            for a in args {
                match a {
                    Val::Obj(x) => {
                        if let ObjKind::Array(items) = &x.borrow().kind {
                            out.extend(items.iter().cloned());
                        } else {
                            out.push(a.clone());
                        }
                    }
                    _ => out.push(a.clone()),
                }
            }
            Val::new_array(out)
        }
        A_JOIN => {
            let sep = match args.first() {
                Some(v) => v.to_string(),
                None => ",".to_string(),
            };
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            let parts: Vec<String> = items
                .iter()
                .map(|v| match v {
                    Val::Undefined | Val::Null => String::new(),
                    v => v.to_string(),
                })
                .collect();
            Val::Str(Rc::from(parts.join(&sep).as_str()))
        }
        A_INDEXOF | A_INCLUDES | A_LASTINDEXOF => {
            let needle = args.first().cloned().unwrap_or(Val::Undefined);
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            if idx == A_LASTINDEXOF {
                // `position` caps the START index of the match (default: end)
                let last = items.len().saturating_sub(1);
                let from = match args.get(1) {
                    None | Some(Val::Undefined) => last,
                    Some(v) => {
                        let n = v.to_number();
                        if n.is_nan() {
                            last
                        } else if n < 0.0 {
                            // relative to the end, like the spec
                            ((items.len() as f64 + n).max(-1.0)) as usize
                        } else {
                            (n as usize).min(last)
                        }
                    }
                };
                if items.is_empty() {
                    return Ok(Val::Num(-1.0));
                }
                let found = (0..=from)
                    .rev()
                    .find(|&i| strict_eq(&items[i], &needle));
                return Ok(Val::Num(found.map(|i| i as f64).unwrap_or(-1.0)));
            }
            let from = args.get(1).map_or(0, |v| {
                let n = v.to_number();
                if n.is_nan() || n < 0.0 {
                    0
                } else {
                    n as usize
                }
            });
            let found = items
                .iter()
                .skip(from)
                .position(|v| strict_eq(v, &needle))
                .map(|p| p + from);
            if idx == A_INDEXOF {
                Val::Num(found.map(|i| i as f64).unwrap_or(-1.0))
            } else {
                Val::Bool(found.is_some())
            }
        }
        A_MAP | A_FILTER | A_FOREACH | A_SOME | A_EVERY => {
            let f = args.first().cloned().unwrap_or(Val::Undefined);
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            let this = Val::Obj(e.global_obj.clone());
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let cb_args = vec![item.clone(), Val::Num(i as f64), this.clone()];
                let r = e.call(f.clone(), this.clone(), cb_args)?;
                match idx {
                    A_MAP => out.push(r),
                    A_FILTER => {
                        if r.to_bool() {
                            out.push(item.clone());
                        }
                    }
                    A_SOME => {
                        if r.to_bool() {
                            return Ok(Val::Bool(true));
                        }
                    }
                    A_EVERY => {
                        if !r.to_bool() {
                            return Ok(Val::Bool(false));
                        }
                    }
                    _ => {} // forEach discards
                }
            }
            match idx {
                A_MAP | A_FILTER => Val::new_array(out),
                A_SOME => Val::Bool(false),
                A_EVERY => Val::Bool(true),
                _ => Val::Undefined,
            }
        }
        A_SORT => {
            // default: lexicographic by string; with a comparator, a stable
            // insertion sort (bounded by typical script array sizes)
            let mut items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            match args.first() {
                Some(cmp) if !matches!(cmp, Val::Undefined) => {
                    let cmp = cmp.clone();
                    let mut out: Vec<Val> = Vec::with_capacity(items.len());
                    for it in items.drain(..) {
                        let mut pos = out.len();
                        for i in 0..out.len() {
                            let r = e.call(
                                cmp.clone(),
                                Val::Undefined,
                                vec![it.clone(), out[i].clone()],
                            )?;
                            if r.to_number() < 0.0 {
                                pos = i;
                                break;
                            }
                        }
                        out.insert(pos, it);
                    }
                    items = out;
                }
                _ => items.sort_by(|a, b| a.to_string().cmp(&b.to_string())),
            }
            if let ObjKind::Array(slot) = &mut o.borrow_mut().kind {
                *slot = items;
            }
            Val::Obj(o.clone())
        }
        A_REVERSE => {
            if let ObjKind::Array(slot) = &mut o.borrow_mut().kind {
                slot.reverse();
            }
            Val::Obj(o.clone())
        }
        A_REDUCE => {
            let f = args.first().cloned().unwrap_or(Val::Undefined);
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            let this = Val::Obj(e.global_obj.clone());
            let (mut acc, start) = match args.get(1) {
                Some(v) => (v.clone(), 0),
                None => {
                    if items.is_empty() {
                        return Err(JsError::throw(
                            "TypeError: Reduce of empty array with no initial value",
                        ));
                    }
                    (items[0].clone(), 1)
                }
            };
            for item in &items[start..] {
                let cb_args = vec![acc.clone(), item.clone(), this.clone()];
                acc = e.call(f.clone(), this.clone(), cb_args)?;
            }
            acc
        }
        _ => Val::Undefined,
    })
}

// -- Error constructors -----------------------------------------------------

const E_ERROR: u16 = 0;
const E_TYPE: u16 = 1;
const E_RANGE: u16 = 2;
const E_SYNTAX: u16 = 3;

fn n_error_ctor(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let name: Rc<str> = Rc::from(match idx {
        E_TYPE => "TypeError",
        E_RANGE => "RangeError",
        E_SYNTAX => "SyntaxError",
        _ => "Error",
    });
    let msg: Rc<str> = Rc::from(args.first().map_or("".to_string(), |v| v.to_string()).as_str());
    // the instance chains to its own class prototype (Error.prototype
    // itself when idx == 0), so `instanceof` holds for the right class
    let proto = e
        .native_id(&name)
        .and_then(|id| e.native_prop(id, "prototype"))
        .and_then(|v| match v {
            Val::Obj(o) => Some(o),
            _ => None,
        })
        .or_else(|| e.error_proto.clone());
    // `class MyError extends Error`: `super(msg)` runs on the object `new`
    // already made for the subclass, so turn that object into an error in
    // place instead of returning a detached one the caller would drop.
    // shortcut: `is_ctor_call` is the same this != global heuristic the other
    // native constructors use (Error.call(obj) would mutate obj too).
    if is_ctor_call(e, this) {
        if let Val::Obj(o) = this {
            let mut b = o.borrow_mut();
            if !matches!(b.kind, ObjKind::Error { .. }) {
                b.kind = ObjKind::Error {
                    name: name.clone(),
                    msg: msg.clone(),
                };
                if b.proto.is_none() {
                    b.proto = proto.clone();
                }
                return Ok(this.clone());
            }
        }
    }
    Ok(Val::Obj(Rc::new(RefCell::new(Obj {
        kind: ObjKind::Error { name, msg },
        props: Vec::new(),
        proto,
    }))))
}

/// `Error.prototype.toString`: `Name: message`, or just the name when the
/// message is empty. The two fields are read through the prototype chain (not
/// from the internal slot) so borrowing it for an error-like object works:
/// libraries do `Error.prototype.toString.call(err)`.
fn n_error_to_string(e: &mut JsEngine, _idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    let name = match get_member(e, this, "name")? {
        Val::Undefined => "Error".to_string(),
        v => v.to_string(),
    };
    let msg = match get_member(e, this, "message")? {
        Val::Undefined => String::new(),
        v => v.to_string(),
    };
    Ok(Val::Str(Rc::from(
        if msg.is_empty() {
            name
        } else {
            format!("{name}: {msg}")
        }
        .as_str(),
    )))
}

// -- Date -------------------------------------------------------------------

fn n_date(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == 1 {
        // Date.now()
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        return Ok(Val::Num(ms));
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    if is_ctor_call(e, this) {
        // `new Date()` / `new Date(ms)`: mutate the fresh object into a Date
        // shortcut: `new Date(string)` (ISO date parsing) is NOT supported —
        // it yields an invalid date (NaN).
        let ms = match args.first() {
            None => now_ms,
            Some(v) => {
                let n = v.to_number();
                if matches!(v, Val::Str(_)) {
                    f64::NAN
                } else {
                    n
                }
            }
        };
        if let Val::Obj(o) = this {
            o.borrow_mut().kind = ObjKind::Date(ms);
            return Ok(this.clone());
        }
        Ok(Val::Undefined)
    } else {
        // `Date()` as a function: shortcut returns epoch-ms string instead
        // of the spec's human-readable date string
        Ok(Val::Str(Rc::from(Val::Num(now_ms).to_string().as_str())))
    }
}

// -- conversions ------------------------------------------------------------

/// Names exposed on `String.prototype` (mirrors the primitive dispatch table
/// in `get_member`; `substring` maps to `slice`, an honest shortcut).
const STR_METHODS: &[(&str, Meth)] = &[
    ("toUpperCase", Meth::StrToUpper),
    ("toLowerCase", Meth::StrToLower),
    ("trim", Meth::StrTrim),
    ("trimStart", Meth::StrTrimStart),
    ("trimLeft", Meth::StrTrimStart),
    ("trimEnd", Meth::StrTrimEnd),
    ("trimRight", Meth::StrTrimEnd),
    ("slice", Meth::StrSlice),
    ("substring", Meth::StrSlice),
    ("indexOf", Meth::StrIndexOf),
    ("lastIndexOf", Meth::StrLastIndexOf),
    ("includes", Meth::StrIncludes),
    ("split", Meth::StrSplit),
    ("replace", Meth::StrReplace),
    ("match", Meth::StrMatch),
    ("search", Meth::StrSearch),
    ("matchAll", Meth::StrMatchAll),
    ("startsWith", Meth::StrStartsWith),
    ("endsWith", Meth::StrEndsWith),
    ("repeat", Meth::StrRepeat),
    ("padStart", Meth::StrPadStart),
    ("padEnd", Meth::StrPadEnd),
    ("charAt", Meth::StrCharAt),
    ("charCodeAt", Meth::StrCharCodeAt),
    ("toString", Meth::StrToString),
    ("valueOf", Meth::StrToString),
];

const NUM_METHODS: &[(&str, Meth)] = &[
    ("toString", Meth::NumToString),
    ("valueOf", Meth::NumToString),
    ("toFixed", Meth::NumToFixed),
];

fn n_str_proto(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    super::interp::call_method(e, STR_METHODS[idx as usize].1, this, args)
}

fn n_num_proto(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    super::interp::call_method(e, NUM_METHODS[idx as usize].1, this, args)
}

const FN_METHODS: &[Meth] = &[Meth::FnCall, Meth::FnApply, Meth::FnBind, Meth::FnToString];

fn n_fn_proto(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    super::interp::call_method(e, FN_METHODS[idx as usize], this, args)
}

/// shortcut: the `Function` constructor does not compile source. The callable
/// it returns ignores its body and yields the global object — enough for the
/// widespread `Function("return this")()` global probe.
fn n_fn_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    Ok(Val::Native(e.fn_body_native))
}

fn n_fn_body(e: &mut JsEngine, _idx: u16, _this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    Ok(Val::Obj(e.global_obj.clone()))
}

fn n_convert(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let v = args.first().cloned().unwrap_or(Val::Undefined);
    // boxed primitives (`new String(..)`) are unsupported: throw honestly
    if is_ctor_call(e, this) {
        return Err(JsError::throw(
            "TypeError: String/Number/Boolean are not constructors (boxed primitives unsupported)",
        ));
    }
    Ok(match idx {
        0 => Val::Str(Rc::from(v.to_string().as_str())),
        1 => Val::Num(v.to_number()),
        _ => Val::Bool(v.to_bool()),
    })
}

// -- parseInt / parseFloat / isNaN / isFinite -------------------------------

fn n_num_globals(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    Ok(match idx {
        0 => {
            let s = args.first().map_or("".to_string(), |v| v.to_string());
            let s = s.trim();
            let (sign, rest) = if let Some(r) = s.strip_prefix('-') {
                (-1.0, r)
            } else if let Some(r) = s.strip_prefix('+') {
                (1.0, r)
            } else {
                (1.0, s)
            };
            let radix = match args.get(1) {
                Some(v) if v.to_number() != 0.0 && v.to_number().is_finite() => {
                    v.to_number() as u32
                }
                _ => {
                    // auto-detect 0x/0b/0o prefixes
                    if rest.len() > 2 && rest.starts_with('0') {
                        match rest.as_bytes()[1] {
                            b'x' | b'X' => 16,
                            b'b' | b'B' => 2,
                            b'o' | b'O' => 8,
                            _ => 10,
                        }
                    } else {
                        10
                    }
                }
            };
            let digits = if radix == 16 {
                rest.trim_start_matches("0x").trim_start_matches("0X")
            } else if radix == 8 {
                rest.trim_start_matches("0o").trim_start_matches("0O")
            } else if radix == 2 {
                rest.trim_start_matches("0b").trim_start_matches("0B")
            } else {
                rest
            };
            // shortcut: accumulate as f64, so integers beyond 2^53 lose
            // precision (real engines use BigInt internally)
            let mut val = 0.0f64;
            let mut any = false;
            for c in digits.chars() {
                let d = match c.to_digit(radix) {
                    Some(d) => d as f64,
                    None => break,
                };
                val = val * radix as f64 + d;
                any = true;
            }
            Val::Num(if any { sign * val } else { f64::NAN })
        }
        1 => {
            let s = args.first().map_or("".to_string(), |v| v.to_string());
            let s = s.trim();
            let b = s.as_bytes();
            let mut i = 0;
            if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
                i += 1;
            }
            let mut seen_dot = false;
            let mut seen_exp = false;
            while i < b.len() {
                match b[i] {
                    c if c.is_ascii_digit() => i += 1,
                    b'.' if !seen_dot && !seen_exp => {
                        seen_dot = true;
                        i += 1;
                    }
                    b'e' | b'E' if !seen_exp => {
                        seen_exp = true;
                        i += 1;
                        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
                            i += 1;
                        }
                    }
                    _ => break,
                }
            }
            Val::Num(if i == 0 || (i == 1 && (b[0] == b'-' || b[0] == b'+')) {
                f64::NAN
            } else {
                s[..i].parse::<f64>().unwrap_or(f64::NAN)
            })
        }
        2 => Val::Bool(args.first().map_or(true, |v| v.to_number().is_nan())),
        _ => Val::Bool(args.first().map_or(true, |v| v.to_number().is_finite())),
    })
}

// -- URI encoding -----------------------------------------------------------

fn n_uri(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let s = args.first().map_or("".to_string(), |v| v.to_string());
    if idx == 0 {
        let mut out = String::with_capacity(s.len());
        for b in s.as_bytes() {
            let keep = b.is_ascii_alphanumeric()
                || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')');
            if keep {
                out.push(*b as char);
            } else {
                out.push_str(&format!("%{:02X}", b));
            }
        }
        Ok(Val::Str(Rc::from(out.as_str())))
    } else {
        let b = s.as_bytes();
        let mut bytes: Vec<u8> = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                let h = b
                    .get(i + 1..i + 3)
                    .ok_or_else(|| JsError::throw("URIError: malformed URI"))?;
                let hs = std::str::from_utf8(h)
                    .map_err(|_| JsError::throw("URIError: malformed URI"))?;
                let v = u8::from_str_radix(hs, 16)
                    .map_err(|_| JsError::throw("URIError: malformed URI"))?;
                bytes.push(v);
                i += 3;
            } else {
                bytes.push(b[i]);
                i += 1;
            }
        }
        // lossy is a deliberate shortcut: malformed UTF-8 sequences in the
        // decoded bytes become U+FFFD instead of throwing
        Ok(Val::Str(Rc::from(
            String::from_utf8_lossy(&bytes).as_ref(),
        )))
    }
}

// -- timers -----------------------------------------------------------------

fn n_set_timeout(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    // idx 0 = setTimeout, 1 = setInterval, 2 = clearTimeout/clearInterval.
    // String callbacks ("alert()") are not compiled: they are a legacy form no
    // real page uses any more, and they would need a compile step per tick.
    if idx == 2 {
        let id = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
        if id.is_finite() {
            e.cancelled_timers.push(id);
        }
        return Ok(Val::Undefined);
    }
    let cb = args.first().cloned().unwrap_or(Val::Undefined);
    if !matches!(cb, Val::Func(_) | Val::Native(_) | Val::Method(_)) {
        return Err(JsError::throw("TypeError: setTimeout callback is not a function"));
    }
    let ms = args.get(1).map_or(0.0, |v| {
        let n = v.to_number();
        if n.is_nan() || n < 0.0 {
            0.0
        } else {
            n
        }
    });
    let id = e.next_timer_id;
    e.next_timer_id += 1.0;
    e.timers.push(TimerTask {
        id,
        delay_ms: ms,
        // a 0ms interval would spin the worker forever at one callback per
        // turn; browsers clamp too (nested-timeout clamp), so do the same
        repeat_ms: if idx == 1 { Some(ms.max(4.0)) } else { None },
        callback: cb,
        args: args.iter().skip(2).cloned().collect(),
    });
    Ok(Val::Num(id))
}

// -- Promise (constructor + statics) ----------------------------------------

const PS_CTOR: u16 = 0;
const PS_RESOLVE: u16 = 1;
const PS_REJECT: u16 = 2;
const PS_ALL: u16 = 3;
const PS_ANY: u16 = 4;
const PS_RACE: u16 = 5;
const PS_ALL_SETTLED: u16 = 6;

/// register a reaction on an element promise (pending -> queue; settled ->
/// enqueue immediately); `ret` stays None for combinator elements because
/// their outcome is deposited into the shared ComboState
fn reg_reaction(e: &mut JsEngine, p: &Rc<RefCell<Obj>>, kind: ReactKind) {
    let re = Reaction { kind, ret: None };
    if let Some(out) = promise_outcome(p) {
        e.microtasks.push(Job::Reaction(re, out));
    } else {
        let mut b = p.borrow_mut();
        if let ObjKind::Promise(st) = &mut b.kind {
            st.reactions.push(re);
        }
    }
}

fn n_promise(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        PS_CTOR => {
            let executor = args.first().cloned().unwrap_or(Val::Undefined);
            if !is_callable(&executor) {
                return Err(JsError::throw(
                    "TypeError: Promise executor must be a function",
                ));
            }
            new_promise_with_executor(e, executor)
        }
        PS_RESOLVE => {
            let v = args.first().cloned().unwrap_or(Val::Undefined);
            if let Some(p) = promise_of(&v) {
                return Ok(Val::Obj(p)); // `Promise.resolve(p)` is identity for promises
            }
            let p = e.new_promise();
            resolve_promise(e, &p, v);
            Ok(Val::Obj(p))
        }
        PS_REJECT => {
            let p = e.new_promise();
            settle_promise(
                e,
                &p,
                Err(args.first().cloned().unwrap_or(Val::Undefined)),
            );
            Ok(Val::Obj(p))
        }
        PS_ALL | PS_ANY | PS_RACE | PS_ALL_SETTLED => {
            let items = interp::for_of_items(&args.first().cloned().unwrap_or(Val::Undefined))?;
            let result = e.new_promise();
            let n = items.len();
            let mode = match idx {
                PS_ALL => ComboMode::All,
                PS_ANY => ComboMode::Any,
                PS_RACE => ComboMode::Race,
                _ => ComboMode::AllSettled,
            };
            let state = Rc::new(RefCell::new(ComboState {
                result: Some(result.clone()),
                done: false,
                remaining: n,
                slots: (0..n).map(|_| None).collect(),
                mode,
            }));
            for (slot, v) in items.into_iter().enumerate() {
                match promise_of(&v) {
                    Some(p) => {
                        let kind = match idx {
                            PS_ALL => ReactKind::All {
                                slot,
                                state: state.clone(),
                            },
                            PS_ANY => ReactKind::Any {
                                slot,
                                state: state.clone(),
                            },
                            PS_RACE => ReactKind::Race {
                                state: state.clone(),
                            },
                            _ => ReactKind::AllSettled {
                                slot,
                                state: state.clone(),
                            },
                        };
                        reg_reaction(e, &p, kind);
                    }
                    None => {
                        // non-promise element: deposit directly (with
                        // `all(race)` semantics a fulfill/fail decision)
                        let out: Result<Val, Val> = Ok(v.clone());
                        deposit_combo(e, &state, slot, out);
                    }
                }
                let _ = slot;
            }
            // all-non-promise elements may have settled the result already;
            // an empty input settles `all`/`allSettled` immediately
            if !state.borrow().done && n == 0 {
                let empty: Vec<Val> = Vec::new();
                let payload = if idx == PS_ALL_SETTLED || idx == PS_ALL {
                    Val::new_array(empty)
                } else if idx == PS_RACE {
                    return Err(JsError::throw(
                        "Error: Promise.race of an empty iterable stays pending (unsupported)",
                    ));
                } else {
                    Val::Undefined // `any` of empty -> AggregateError path below
                };
                if idx == PS_ANY {
                    state.borrow_mut().done = true;
                    settle_promise(
                        e,
                        &result,
                        Err(Val::Str(Rc::from(
                            "Error: no promise from Promise.any was fulfilled",
                        ))),
                    );
                } else {
                    state.borrow_mut().done = true;
                    resolve_promise(e, &result, payload);
                }
            }
            Ok(Val::Obj(result))
        }
        _ => Ok(Val::Undefined),
    }
}

/// deposit one element outcome into the combinator board (non-queue path for
/// already-non-promise elements; mirrors the Reaction arms in `exec_job`)
fn deposit_combo(
    e: &mut JsEngine,
    state: &Rc<RefCell<ComboState>>,
    slot: usize,
    out: Result<Val, Val>,
) {
    let mut finish = None;
    {
        let mut st = state.borrow_mut();
        if st.done {
            return;
        }
        match &st.mode {
            ComboMode::Race => {
                // first settlement (fulfill or reject) decides
                st.done = true;
                finish = st.result.take().map(|p| (p, out.clone()));
            }
            ComboMode::All => match &out {
                Ok(v) => {
                    if st.slots.len() > slot {
                        st.slots[slot] = Some(Ok(v.clone()));
                    }
                    st.remaining -= 1;
                    if st.remaining == 0 {
                        st.done = true;
                        let vals: Vec<Val> = st
                            .slots
                            .iter()
                            .filter_map(|s| s.as_ref())
                            .map(|r| match r {
                                Ok(v) => v.clone(),
                                // an Err slot never reaches `all`'s finish
                                Err(_) => Val::Undefined,
                            })
                            .collect();
                        finish = st.result.take().map(|p| (p, Ok(Val::new_array(vals))));
                    }
                }
                Err(err) => {
                    st.done = true;
                    finish = st.result.take().map(|p| (p, Err(err.clone())));
                }
            },
            ComboMode::Any => match &out {
                Ok(v) => {
                    // a fulfillment wins immediately regardless of order
                    st.done = true;
                    finish = st.result.take().map(|p| (p, Ok(v.clone())));
                }
                Err(_) => {
                    // rejection only aggregates when every element rejected
                    st.slots[slot] = Some(Err(Val::Undefined));
                    st.remaining -= 1;
                    if st.remaining == 0 {
                        st.done = true;
                        // honest shortcut: one shared aggregate error message
                        // instead of a full AggregateError reason list
                        finish = st.result.take().map(|p| {
                            (
                                p,
                                Err(Val::Str(Rc::from(
                                    "Error: no promise from Promise.any was fulfilled",
                                ))),
                            )
                        });
                    }
                }
            },
            ComboMode::AllSettled => {
                if st.slots.len() > slot {
                    st.slots[slot] = Some(out.clone());
                }
                st.remaining -= 1;
                if st.remaining == 0 {
                    st.done = true;
                    let vals: Vec<Val> = st
                        .slots
                        .iter()
                        .filter_map(|s| s.as_ref())
                        .map(|r| {
                            let d = Val::new_object();
                            let dob = obj_of(&d);
                            match r {
                                Ok(v) => {
                                    obj_set_own(&dob, "status", Val::from("fulfilled"));
                                    obj_set_own(&dob, "value", v.clone());
                                }
                                Err(reason) => {
                                    obj_set_own(&dob, "status", Val::from("rejected"));
                                    obj_set_own(&dob, "reason", reason.clone());
                                }
                            }
                            d
                        })
                        .collect();
                    finish = st.result.take().map(|p| (p, Ok(Val::new_array(vals))));
                }
            }
        }
    }
    if let Some((p, o)) = finish {
        match o {
            Ok(v) => resolve_promise(e, &p, v),
            Err(err) => settle_promise(e, &p, Err(err)),
        }
    }
}

/// move a stack-built ComboState into the Rc cell (helper keeps `st` local)

// -- RegExp ---------------------------------------------------------------

/// `RegExp.prototype.exec` / `.test` / `.toString` as real function values:
/// they forward to the same receiver-dispatched implementations `re.exec`
/// uses, so borrowing them (`RegExp.prototype.exec.call(re, s)`) works.
fn n_regex_proto(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let m = match idx {
        0 => Meth::RegexExec,
        1 => Meth::RegexTest,
        _ => Meth::RegexToString,
    };
    super::interp::call_method(e, m, this, args)
}

fn n_regexp(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    // `new RegExp(pat, flags)` / `RegExp(pat)` — same behavior both ways.
    // A regex argument without explicit flags inherits its source+flags
    // (real JS behavior); a string compiles fresh.
    let (src, flags): (Rc<str>, Val) = match args.first() {
        Some(Val::Obj(o)) => {
            let is_re = matches!(o.try_borrow().map(|b| matches!(b.kind, ObjKind::Regex(_))), Ok(true));
            if is_re {
                let b = o.borrow();
                let (s, f) = match &b.kind {
                    ObjKind::Regex(ri) => (
                        Rc::from(ri.re.src.as_str()),
                        Rc::from(ri.re.flags.as_str()),
                    ),
                    _ => unreachable!(),
                };
                (s, args.get(1).cloned().unwrap_or(Val::Str(f)))
            } else {
                let s = args.first().map(|v| v.to_string()).unwrap_or_default();
                (Rc::from(s.as_str()), args.get(1).cloned().unwrap_or(Val::Undefined))
            }
        }
        Some(other) => {
            let s = other.to_string();
            (Rc::from(s.as_str()), args.get(1).cloned().unwrap_or(Val::Undefined))
        }
        None => (
            Rc::from(""),
            args.get(1).cloned().unwrap_or(Val::Undefined),
        ),
    };
    let flags_s: Rc<str> = match flags {
        Val::Undefined => Rc::from(""),
        v => Rc::from(v.to_string().as_str()),
    };
    let re = match e.regex_from_literal(src.clone(), flags_s.clone()) {
        Ok(re) => re,
        // `new RegExp(bad)` throws a normal, catchable SyntaxError; only a
        // regex *literal* is a parse-time error. Published bundles probe
        // engine support with `try { new RegExp("(?<a>b)") } catch {}`, so
        // this must not abort the whole script.
        Err(JsError::Syntax { msg, .. }) => {
            return Err(JsError::throw(&format!(
                "SyntaxError: Invalid regular expression: /{}/{}/: {}",
                src, flags_s, msg
            )))
        }
        Err(other) => return Err(other),
    };
    Ok(Val::Obj(Rc::new(RefCell::new(Obj {
        kind: ObjKind::Regex(ReInst {
            re,
            last_index: Cell::new(0.0),
        }),
        props: Vec::new(),
        proto: None,
    }))))
}

// -- end regex helper stub

// ---------------------------------------------------------------------------
// URL / URLSearchParams
// ---------------------------------------------------------------------------

/// Hidden own property holding a URLSearchParams' serialized query.
/// `\u{1}` cannot appear in an identifier, so page code never collides with it
/// by accident (a computed string key still could — harmless).
const USP_DATA: &str = "\u{1}pairs";

const U_TO_STRING: u16 = 0;
const U_TO_JSON: u16 = 1;

const P_GET: u16 = 0;
const P_GET_ALL: u16 = 1;
const P_HAS: u16 = 2;
const P_SET: u16 = 3;
const P_APPEND: u16 = 4;
const P_DELETE: u16 = 5;
const P_TO_STRING: u16 = 6;
const P_SORT: u16 = 7;
const P_FOREACH: u16 = 8;
const P_ENTRIES: u16 = 9;
const P_KEYS: u16 = 10;
const P_VALUES: u16 = 11;

/// A fresh plain object pointing at `proto`.
fn proto_instance(proto: Option<Rc<RefCell<Obj>>>) -> Val {
    Val::Obj(Rc::new(RefCell::new(Obj {
        kind: ObjKind::Plain,
        props: Vec::new(),
        proto,
    })))
}

/// The serialized query of a URLSearchParams value.
fn usp_data(v: &Val) -> Option<String> {
    match v {
        Val::Obj(o) => o
            .borrow()
            .props
            .iter()
            .find(|(k, _)| k.as_ref() == USP_DATA)
            .and_then(|(_, v)| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            }),
        _ => None,
    }
}

fn usp_store(v: &Val, query: &str) {
    if let Val::Obj(o) = v {
        obj_set_own(o, USP_DATA, Val::Str(Rc::from(query)));
    }
}

/// Build a URLSearchParams instance (shared by its constructor and by
/// `url.searchParams`).
fn make_params(e: &JsEngine, query: &str) -> Val {
    let v = proto_instance(e.params_proto.clone());
    usp_store(&v, query);
    v
}

fn arg_str(args: &[Val], i: usize) -> String {
    args.get(i).map(|v| v.to_string()).unwrap_or_default()
}

fn n_url_ctor(e: &mut JsEngine, _idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if !is_ctor_call(e, this) {
        return Err(JsError::throw("TypeError: URL is not a constructor function"));
    }
    let input = match args.first() {
        Some(Val::Undefined) | None => {
            return Err(JsError::throw("TypeError: URL: 1 argument required"))
        }
        Some(v) => v.to_string(),
    };
    let abs = match args.get(1) {
        Some(base) if !matches!(base, Val::Undefined) => {
            match crate::js::url::join(&url_base(base), &input) {
                Some(u) => u,
                None => return Err(JsError::throw(&format!("TypeError: URL: invalid URL '{input}'"))),
            }
        }
        _ => input.clone(),
    };
    let p = match crate::js::url::parts(&abs) {
        Some(p) => p,
        None => return Err(JsError::throw(&format!("TypeError: URL: invalid URL '{input}'"))),
    };
    let obj = proto_instance(e.url_proto.clone());
    let o = own(&obj);
    for (k, v) in [
        ("href", p.href),
        ("protocol", p.protocol),
        ("host", p.host),
        ("hostname", p.hostname),
        ("port", p.port),
        ("pathname", p.pathname),
        ("search", p.search.clone()),
        ("hash", p.hash),
        ("origin", p.origin),
        ("username", p.username),
        ("password", p.password),
    ] {
        obj_set_own(o, k, Val::Str(Rc::from(v)));
    }
    let params = make_params(e, &p.search);
    obj_set_own(o, "searchParams", params);
    Ok(obj)
}

/// The string a base argument contributes. A URL instance stringifies to its
/// `href` (that is what its prototype `toString` returns), so
/// `new URL('.', u)` must not fall back to the engine's structural object
/// text.
fn url_base(v: &Val) -> String {
    if let Val::Obj(o) = v {
        let href = o
            .borrow()
            .props
            .iter()
            .find(|(k, _)| k.as_ref() == "href")
            .and_then(|(_, v)| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            });
        if let Some(h) = href {
            return h;
        }
    }
    v.to_string()
}

fn n_url(_e: &mut JsEngine, _idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    match this {
        Val::Obj(o) => {
            let href = o
                .borrow()
                .props
                .iter()
                .find(|(k, _)| k.as_ref() == "href")
                .and_then(|(_, v)| match v {
                    Val::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| Rc::from(""));
            Ok(Val::Str(href))
        }
        _ => Err(JsError::throw("TypeError: URL method called on non-URL")),
    }
}

fn n_params_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let query = match args.first() {
        None | Some(Val::Undefined) => String::new(),
        Some(Val::Str(s)) => s.strip_prefix('?').unwrap_or(s).to_string(),
        Some(v @ Val::Obj(o)) => {
            if let Some(q) = usp_data(v) {
                q
            } else {
                // array of pairs, or a plain object's own enumerable props
                let b = o.borrow();
                let pairs: Vec<(String, String)> = match &b.kind {
                    ObjKind::Array(items) => items
                        .iter()
                        .map(|it| match it {
                            Val::Obj(p) => match &p.borrow().kind {
                                ObjKind::Array(kv) => (
                                    kv.first().map(|v| v.to_string()).unwrap_or_default(),
                                    kv.get(1).map(|v| v.to_string()).unwrap_or_default(),
                                ),
                                _ => (it.to_string(), String::new()),
                            },
                            v => (v.to_string(), String::new()),
                        })
                        .collect(),
                    _ => b
                        .props
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                };
                drop(b);
                crate::js::url::encode_pairs(&pairs)
            }
        }
        Some(other) => other.to_string(),
    };
    Ok(make_params(e, &query))
}

fn n_params(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let query = match usp_data(this) {
        Some(q) => q,
        None => return Err(JsError::throw("TypeError: URLSearchParams method called on non-params")),
    };
    let mut pairs = crate::js::url::parse_query(&query);
    let name = || arg_str(args, 0);
    let value = || arg_str(args, 1);
    match idx {
        P_GET => Ok(match pairs.iter().find(|(k, _)| *k == name()) {
            Some((_, v)) => Val::Str(Rc::from(v.as_str())),
            None => Val::Null,
        }),
        P_GET_ALL => Ok(Val::new_array(
            pairs
                .iter()
                .filter(|(k, _)| *k == name())
                .map(|(_, v)| Val::Str(Rc::from(v.as_str())))
                .collect(),
        )),
        P_HAS => Ok(Val::Bool(pairs.iter().any(|(k, _)| *k == name()))),
        P_SET => {
            let (n, v) = (name(), value());
            pairs.retain(|(k, _)| *k != n);
            pairs.push((n, v));
            let s = crate::js::url::encode_pairs(&pairs);
            usp_store(this, &s);
            Ok(Val::Undefined)
        }
        P_APPEND => {
            pairs.push((name(), value()));
            let s = crate::js::url::encode_pairs(&pairs);
            usp_store(this, &s);
            Ok(Val::Undefined)
        }
        P_DELETE => {
            let n = name();
            pairs.retain(|(k, _)| *k != n);
            let s = crate::js::url::encode_pairs(&pairs);
            usp_store(this, &s);
            Ok(Val::Undefined)
        }
        P_TO_STRING => Ok(Val::Str(Rc::from(
            crate::js::url::encode_pairs(&pairs).as_str(),
        ))),
        P_SORT => {
            pairs.sort_by(|(a1, a2), (b1, b2)| a1.cmp(b1).then_with(|| a2.cmp(b2)));
            let s = crate::js::url::encode_pairs(&pairs);
            usp_store(this, &s);
            Ok(Val::Undefined)
        }
        P_FOREACH => {
            let cb = args.first().cloned().unwrap_or(Val::Undefined);
            let this_arg = args.get(1).cloned().unwrap_or(Val::Undefined);
            for (k, v) in &pairs {
                e.call(
                    cb.clone(),
                    this_arg.clone(),
                    vec![
                        Val::Str(Rc::from(v.as_str())),
                        Val::Str(Rc::from(k.as_str())),
                        this.clone(),
                    ],
                )?;
            }
            Ok(Val::Undefined)
        }
        P_ENTRIES => Ok(Val::new_array(
            pairs
                .iter()
                .map(|(k, v)| {
                    Val::new_array(vec![
                        Val::Str(Rc::from(k.as_str())),
                        Val::Str(Rc::from(v.as_str())),
                    ])
                })
                .collect(),
        )),
        P_KEYS => Ok(Val::new_array(
            pairs
                .iter()
                .map(|(k, _)| Val::Str(Rc::from(k.as_str())))
                .collect(),
        )),
        P_VALUES => Ok(Val::new_array(
            pairs
                .iter()
                .map(|(_, v)| Val::Str(Rc::from(v.as_str())))
                .collect(),
        )),
        _ => Ok(Val::Undefined),
    }
}

// -- Set / Map methods ------------------------------------------------------
// Payload helpers: the hidden `__set` array / `__map` array-of-pairs lives as
// an own prop on the instance (see the install block above).

const SET_SIZE: u16 = 8;
const MAP_SIZE: u16 = 9;

fn set_items(this: &Val) -> Result<Vec<Val>, JsError> {
    if let Val::Obj(o) = this {
        if let Val::Obj(a) = obj_get_own(o, "__set") {
            if let ObjKind::Array(items) = &a.borrow().kind {
                return Ok(items.clone());
            }
        }
    }
    Err(JsError::throw("TypeError: Set method called on incompatible receiver"))
}

fn set_store(this: &Val, items: Vec<Val>) {
    if let Val::Obj(o) = this {
        obj_set_own(o, "__set", Val::new_array(items));
    }
}

fn map_pairs(this: &Val) -> Result<Vec<(Val, Val)>, JsError> {
    if let Val::Obj(o) = this {
        if let Val::Obj(a) = obj_get_own(o, "__map") {
            if let ObjKind::Array(entries) = &a.borrow().kind {
                return entries
                    .iter()
                    .map(|e| match e {
                        Val::Obj(p) => match &p.borrow().kind {
                            ObjKind::Array(kv) if kv.len() >= 2 => Ok((kv[0].clone(), kv[1].clone())),
                            _ => Err(JsError::throw("TypeError: corrupt Map entry")),
                        },
                        _ => Err(JsError::throw("TypeError: corrupt Map entry")),
                    })
                    .collect();
            }
        }
    }
    Err(JsError::throw("TypeError: Map method called on incompatible receiver"))
}

fn map_store(this: &Val, pairs: Vec<(Val, Val)>) {
    if let Val::Obj(o) = this {
        obj_set_own(
            o,
            "__map",
            Val::new_array(
                pairs
                    .into_iter()
                    .map(|(k, v)| Val::new_array(vec![k, v]))
                    .collect(),
            ),
        );
    }
}

// -- Symbol ----------------------------------------------------------------

/// A unique symbol stand-in: plain object + hidden `__symbol` description.
fn new_symbol(desc: &str) -> Val {
    let o = Val::new_object();
    obj_set_own(own(&o), "__symbol", Val::Str(Rc::from(desc)));
    o
}

fn n_symbol_proto(_e: &mut JsEngine, idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    let desc = match this {
        Val::Obj(o) => o
            .borrow()
            .props
            .iter()
            .find(|(k, _)| &**k == "__symbol")
            .map(|(_, v)| v.to_string()),
        _ => None,
    };
    let Some(desc) = desc else {
        return Err(JsError::throw(
            "TypeError: Symbol.prototype method called on non-symbol",
        ));
    };
    Ok(match idx {
        0 => Val::Str(Rc::from(format!("Symbol({desc})").as_str())),
        _ => this.clone(),
    })
}

fn n_symbol(e: &mut JsEngine, _idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if is_ctor_call(e, this) {
        return Err(JsError::throw("TypeError: Symbol is not a constructor"));
    }
    Ok(new_symbol(&arg_str(args, 0)))
}

// -- base64 ----------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue, // padding, whitespace, garbage
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Byte view of a typed-array stand-in (plain array or string).
fn u8_bytes(v: &Val) -> Vec<u8> {
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            ObjKind::Array(items) => items
                .iter()
                .map(|x| (x.to_number() as i64 & 0xff) as u8)
                .collect(),
            _ => Vec::new(),
        },
        Val::Str(s) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    }
}

fn u8_value(e: &JsEngine, bytes: Vec<u8>) -> Val {
    let len = bytes.len();
    let arr = Val::new_array(bytes.into_iter().map(|b| Val::Num(b as f64)).collect());
    if let Val::Obj(o) = &arr {
        obj_set_own(o, "toBase64", Val::Native(e.uint8_to_b64));
        obj_set_own(o, "byteLength", Val::Num(len as f64));
        let buf = Val::new_object();
        obj_set_own(own(&buf), "byteLength", Val::Num(len as f64));
        obj_set_own(o, "buffer", buf);
    }
    arr
}

fn n_u8_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let bytes = match args.first() {
        Some(Val::Num(n)) => vec![0u8; (*n as i64).clamp(0, 1 << 24) as usize],
        Some(v) => u8_bytes(v),
        None => Vec::new(),
    };
    Ok(u8_value(e, bytes))
}

fn n_u8(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        0 => Ok(Val::Str(Rc::from(b64_encode(&u8_bytes(this)).as_str()))),
        _ => Ok(u8_value(e, b64_decode(&arg_str(args, 0)))),
    }
}

fn n_buffer(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == 1 {
        // Buffer.from(str)
        let s = arg_str(args, 0);
        let o = Val::new_object();
        obj_set_own(own(&o), "__buf", Val::Str(Rc::from(s.as_str())));
        obj_set_own(own(&o), "toString", Val::Native(e.buffer_to_string));
        return Ok(o);
    }
    let s = match this {
        Val::Obj(o) => match o.borrow().props.iter().find(|(k, _)| &**k == "__buf") {
            Some((_, Val::Str(s))) => s.to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let out = if arg_str(args, 0) == "base64" {
        b64_encode(s.as_bytes())
    } else {
        s
    };
    Ok(Val::Str(Rc::from(out.as_str())))
}

fn n_b64_global(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let s = arg_str(args, 0);
    if idx == 0 {
        Ok(Val::Str(Rc::from(
            String::from_utf8_lossy(&b64_decode(&s)).as_ref(),
        )))
    } else {
        Ok(Val::Str(Rc::from(b64_encode(s.as_bytes()).as_str())))
    }
}

fn n_ab(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        0 => {
            let n = match this {
                Val::Obj(o) => o
                    .borrow()
                    .props
                    .iter()
                    .find(|(k, _)| &**k == "__ab")
                    .map(|(_, v)| v.to_number())
                    .unwrap_or(0.0),
                _ => 0.0,
            };
            Ok(Val::Num(n))
        }
        2 => Ok(Val::Bool(false)),
        _ => {
            let len = args.first().map(|v| v.to_number()).unwrap_or(0.0);
            let o = proto_instance(e.arraybuffer_proto.clone());
            obj_set_own(own(&o), "__ab", Val::Num(len));
            Ok(o)
        }
    }
}

fn n_text_ctor(e: &mut JsEngine, idx: u16, _this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    Ok(proto_instance(if idx == 0 {
        e.text_encoder_proto.clone()
    } else {
        e.text_decoder_proto.clone()
    }))
}

fn n_text(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        0 => {
            // encode(str) -> array of UTF-8 byte values
            let s = arg_str(args, 0);
            Ok(Val::new_array(
                s.as_bytes().iter().map(|b| Val::Num(*b as f64)).collect(),
            ))
        }
        _ => {
            // decode(bytes) -> string; accepts an array of byte values
            let v = args.first().cloned().unwrap_or(Val::Undefined);
            let bytes: Vec<u8> = match &v {
                Val::Obj(o) => match &o.borrow().kind {
                    ObjKind::Array(items) => items
                        .iter()
                        .map(|x| x.to_number().clamp(0.0, 255.0) as u8)
                        .collect(),
                    _ => Vec::new(),
                },
                Val::Str(s) => return Ok(Val::Str(s.clone())),
                _ => Vec::new(),
            };
            Ok(Val::Str(Rc::from(String::from_utf8_lossy(&bytes).as_ref())))
        }
    }
}

fn n_weakmap_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let obj = proto_instance(e.weakmap_proto.clone());
    let pairs = map_entries_from(args.first())?;
    map_store(&obj, pairs);
    Ok(obj)
}

fn n_weakset_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let obj = proto_instance(e.weakset_proto.clone());
    let mut items: Vec<Val> = Vec::new();
    if let Some(init) = args.first() {
        for v in super::interp::for_of_items(init)? {
            if !items.iter().any(|x| strict_eq(x, &v)) {
                items.push(v);
            }
        }
    }
    obj_set_own(own(&obj), "__set", Val::new_array(items));
    Ok(obj)
}

fn n_set_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let obj = proto_instance(e.set_proto.clone());
    let mut items: Vec<Val> = Vec::new();
    if let Some(init) = args.first() {
        for v in super::interp::for_of_items(init)? {
            if !items.iter().any(|x| strict_eq(x, &v)) {
                items.push(v);
            }
        }
    }
    obj_set_own(own(&obj), "__set", Val::new_array(items));
    Ok(obj)
}

fn n_set(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let mut items = set_items(this)?;
    match idx {
        SET_SIZE => Ok(Val::Num(items.len() as f64)),
        0 => {
            // add(v)
            let v = args.first().cloned().unwrap_or(Val::Undefined);
            if !items.iter().any(|x| strict_eq(x, &v)) {
                items.push(v);
                set_store(this, items);
            }
            Ok(this.clone())
        }
        1 => Ok(Val::Bool(
            items
                .iter()
                .any(|x| strict_eq(x, &args.first().cloned().unwrap_or(Val::Undefined))),
        )),
        2 => {
            // delete(v)
            let v = args.first().cloned().unwrap_or(Val::Undefined);
            let before = items.len();
            items.retain(|x| !strict_eq(x, &v));
            let removed = items.len() != before;
            if removed {
                set_store(this, items);
            }
            Ok(Val::Bool(removed))
        }
        3 => {
            set_store(this, Vec::new());
            Ok(Val::Undefined)
        }
        4 => {
            // forEach(cb[, thisArg]) — cb(value, value, set)
            let cb = args.first().cloned().unwrap_or(Val::Undefined);
            let this_arg = args.get(1).cloned().unwrap_or(Val::Undefined);
            for v in items {
                e.call(
                    cb.clone(),
                    this_arg.clone(),
                    vec![v.clone(), v, this.clone()],
                )?;
            }
            Ok(Val::Undefined)
        }
        5 | 6 => Ok(Val::new_array(items)),
        7 => Ok(Val::new_array(
            items
                .into_iter()
                .map(|v| Val::new_array(vec![v.clone(), v]))
                .collect(),
        )),
        _ => Err(JsError::throw("TypeError: bad Set native")),
    }
}

fn n_map_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let obj = proto_instance(e.map_proto.clone());
    let pairs = map_entries_from(args.first())?;
    map_store(&obj, pairs);
    Ok(obj)
}

/// Entries from a Map/WeakMap iterable argument (`[[k, v], ...]`).
fn map_entries_from(init: Option<&Val>) -> Result<Vec<(Val, Val)>, JsError> {
    let mut pairs: Vec<(Val, Val)> = Vec::new();
    if let Some(init) = init {
        for entry in super::interp::for_of_items(init)? {
            let (k, v) = match &entry {
                Val::Obj(p) => match &p.borrow().kind {
                    ObjKind::Array(kv) if kv.len() >= 2 => (kv[0].clone(), kv[1].clone()),
                    _ => {
                        return Err(JsError::throw(
                            "TypeError: Map constructor entry is not a [key, value] pair",
                        ))
                    }
                },
                _ => {
                    return Err(JsError::throw(
                        "TypeError: Map constructor entry is not a [key, value] pair",
                    ))
                }
            };
            match pairs.iter_mut().find(|(old, _)| strict_eq(old, &k)) {
                Some(slot) => slot.1 = v,
                None => pairs.push((k, v)),
            }
        }
    }
    Ok(pairs)
}

fn n_map(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let mut pairs = map_pairs(this)?;
    let key = || args.first().cloned().unwrap_or(Val::Undefined);
    match idx {
        MAP_SIZE => Ok(Val::Num(pairs.len() as f64)),
        0 => Ok(match pairs.iter().find(|(k, _)| strict_eq(k, &key())) {
            Some((_, v)) => v.clone(),
            None => Val::Undefined,
        }),
        1 => {
            // set(k, v) — updates in place, keeps insertion order
            let (k, v) = (key(), args.get(1).cloned().unwrap_or(Val::Undefined));
            match pairs.iter_mut().find(|(old, _)| strict_eq(old, &k)) {
                Some(slot) => slot.1 = v,
                None => pairs.push((k, v)),
            }
            map_store(this, pairs);
            Ok(this.clone())
        }
        2 => Ok(Val::Bool(pairs.iter().any(|(k, _)| strict_eq(k, &key())))),
        3 => {
            let k = key();
            let before = pairs.len();
            pairs.retain(|(old, _)| !strict_eq(old, &k));
            let removed = pairs.len() != before;
            if removed {
                map_store(this, pairs);
            }
            Ok(Val::Bool(removed))
        }
        4 => {
            map_store(this, Vec::new());
            Ok(Val::Undefined)
        }
        5 => {
            // forEach(cb[, thisArg]) — cb(value, key, map)
            let cb = args.first().cloned().unwrap_or(Val::Undefined);
            let this_arg = args.get(1).cloned().unwrap_or(Val::Undefined);
            for (k, v) in pairs {
                e.call(cb.clone(), this_arg.clone(), vec![v, k, this.clone()])?;
            }
            Ok(Val::Undefined)
        }
        6 => Ok(Val::new_array(pairs.into_iter().map(|(k, _)| k).collect())),
        7 => Ok(Val::new_array(pairs.into_iter().map(|(_, v)| v).collect())),
        8 => Ok(Val::new_array(
            pairs
                .into_iter()
                .map(|(k, v)| Val::new_array(vec![k, v]))
                .collect(),
        )),
        _ => Err(JsError::throw("TypeError: bad Map native")),
    }
}
