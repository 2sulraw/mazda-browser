//! Standard globals + prototypes installed by `JsEngine::new`.
//!
//! Natives share a small dispatch table: one Rust fn per GROUP with an index
//! (`u16`) so an entire family (Math.*, Array.prototype.*) costs one fn
//! pointer in the table. All console output goes to stderr via `eprintln!`
//! to match the engine's logging style.

use super::interp::{date_parts, fmt_date_iso, slice_bounds};
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
        "sin",
        "cos",
        "tan",
        "asin",
        "acos",
        "atan",
        "atan2",
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

    // -- performance -------------------------------------------------------
    // A monotonic millisecond clock. The epoch is fixed on first use, so
    // `now()` starts near zero and `timeOrigin` is the wall-clock instant
    // that zero corresponds to. `mark`/`measure`/`getEntries*` are no-op
    // stubs: analytics bundles probe them, nothing on a page renders from
    // their output.
    // shortcut: one process-wide epoch, not one per page/navigation.
    let perf = Val::new_object();
    let perf_now = e.register_native("performance.now", n_perf, 0);
    let perf_mark = e.register_native("performance.mark", n_perf, 1);
    let perf_measure = e.register_native("performance.measure", n_perf, 2);
    let perf_entries = e.register_native("performance.getEntries", n_perf, 3);
    let perf_entries_by_name = e.register_native("performance.getEntriesByName", n_perf, 3);
    obj_set_own(own(&perf), "now", Val::Native(perf_now));
    obj_set_own(own(&perf), "mark", Val::Native(perf_mark));
    obj_set_own(own(&perf), "measure", Val::Native(perf_measure));
    obj_set_own(own(&perf), "getEntries", Val::Native(perf_entries));
    obj_set_own(
        own(&perf),
        "getEntriesByName",
        Val::Native(perf_entries_by_name),
    );
    obj_set_own(own(&perf), "timeOrigin", Val::Num(perf_time_origin()));
    e.set_global("performance", perf);

    // -- Web Crypto (the two calls pages make at load) ---------------------
    // `crypto.randomUUID()` and `crypto.getRandomValues()`. GitHub's React
    // root mints ids with the first one. shortcut: the bytes come from the
    // same PRNG as `Math.random`, not a CSPRNG — nothing on a page can tell
    // the difference, and real entropy is not a browser-engine feature.
    let crypto = Val::new_object();
    let c_gfv = e.register_native("crypto.getRandomValues", n_crypto, 0);
    let c_uuid = e.register_native("crypto.randomUUID", n_crypto, 1);
    obj_set_own(own(&crypto), "getRandomValues", Val::Native(c_gfv));
    obj_set_own(own(&crypto), "randomUUID", Val::Native(c_uuid));
    e.set_global("crypto", crypto);

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

    // -- Reflect -----------------------------------------------------------
    // The standard meta-API. One dispatch table, installed after Object so it
    // shares every helper the Object statics already use.
    let reflect = Val::new_object();
    for (i, name) in [
        "apply",
        "construct",
        "defineProperty",
        "deleteProperty",
        "get",
        "getOwnPropertyDescriptor",
        "getPrototypeOf",
        "has",
        "isExtensible",
        "ownKeys",
        "preventExtensions",
        "set",
        "setPrototypeOf",
    ]
    .iter()
    .enumerate()
    {
        let id = e.register_native(&format!("Reflect.{name}"), n_reflect, i as u16);
        obj_set_own(own(&reflect), name, Val::Native(id));
    }
    e.set_global("Reflect", reflect);

    // -- Proxy -------------------------------------------------------------
    // shortcut: a proxy is a plain object carrying a hidden `__proxy`
    // { target, handler } slot. The `get` and `set` traps run; every other
    // operation (in, delete, prototype walk, instanceof) forwards to the
    // target, and own-key enumeration sees the proxy itself (empty), not the
    // target. Turbo's router wraps its API with a one-trap get proxy.
    let pc = e.register_native("Proxy", n_proxy_ctor, 0);
    e.set_global("Proxy", Val::Native(pc));

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
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "at",
        "fill",
        "copyWithin",
        "flat",
        "flatMap",
        "values",
        "keys",
        "entries",
    ]
    .iter()
    .enumerate()
    {
        let id = e.register_native(name, n_array, i as u16);
        obj_set_own(own(&arr_proto), name, Val::Native(id));
        if name == &"values" {
            e.array_values_native = id;
        }
    }
    // `Array.prototype[Symbol.iterator]` is `values` itself
    let arr_values = obj_get_own(own(&arr_proto), "values");
    obj_set_own(own(&arr_proto), crate::js::SYMBOL_ITERATOR, arr_values);
    // `Array.prototype.toString` is `join`: `[1, 2] + ''` -> "1,2"
    let arr_join = obj_get_own(own(&arr_proto), "join");
    obj_set_own(own(&arr_proto), "toString", arr_join);
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
    for (i, name) in [
        "Error",
        "TypeError",
        "RangeError",
        "SyntaxError",
        "ReferenceError",
        "EvalError",
        "URIError",
    ]
    .iter()
    .enumerate()
    {
        let id = e.register_native(name, n_error_ctor, i as u16);
        if i == 0 {
            // `Error.prototype` IS the shared base prototype, so an instance
            // of any subclass satisfies `x instanceof Error`.
            obj_set_own(own(&error_proto), "constructor", Val::Native(id));
            e.set_native_prop(id, "prototype", error_proto.clone());
            e.set_global(name, Val::Native(id));
            continue;
        }
        let proto = Val::new_object();
        obj_set_own(own(&proto), "name", Val::Str(Rc::from(*name)));
        obj_set_own(own(&proto), "message", Val::Str(Rc::from("")));
        // `err.constructor === TypeError` - the harness (and real code) uses
        // this to identify an error's class
        obj_set_own(own(&proto), "constructor", Val::Native(id));
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

    // -- Intl ---------------------------------------------------------------
    // shortcut: en-US formatting only and no locale data - the engine has no
    // timezone database and its `Date` is UTC throughout, so `timeZoneName`
    // answers "UTC" and month/day names are the English ones. Both
    // constructors keep their options under a hidden `__intl` prop on the
    // instance; `format` reads them back when it is called.
    let dtf_proto = Val::new_object();
    let dtf_fmt = e.register_native("Intl.DateTimeFormat.prototype.format", n_intl, 0);
    obj_set_own(own(&dtf_proto), "format", Val::Native(dtf_fmt));
    let dtf_ctor = e.register_native("Intl.DateTimeFormat", n_intl, 1);
    e.set_native_prop(dtf_ctor, "prototype", dtf_proto.clone());
    let nf_proto = Val::new_object();
    let nf_fmt = e.register_native("Intl.NumberFormat.prototype.format", n_intl, 2);
    obj_set_own(own(&nf_proto), "format", Val::Native(nf_fmt));
    let nf_ctor = e.register_native("Intl.NumberFormat", n_intl, 3);
    e.set_native_prop(nf_ctor, "prototype", nf_proto.clone());
    let intl = Val::new_object();
    obj_set_own(own(&intl), "DateTimeFormat", Val::Native(dtf_ctor));
    obj_set_own(own(&intl), "NumberFormat", Val::Native(nf_ctor));
    e.set_global("Intl", intl);
    e.intl_dtf_proto = Some(own(&dtf_proto).clone());
    e.intl_nf_proto = Some(own(&nf_proto).clone());

    // -- conversions + numeric globals --------------------------------------
    let str_id = e.register_native("String", n_convert, 0);
    let num_id = e.register_native("Number", n_convert, 1);
    let bool_id = e.register_native("Boolean", n_convert, 2);
    e.set_global("String", Val::Native(str_id));
    e.set_global("Number", Val::Native(num_id));
    e.set_global("Boolean", Val::Native(bool_id));
    // Number's own value properties (read-only reads only; the descriptors
    // are not modeled). MIN_VALUE is the smallest positive subnormal.
    for (name, v) in [
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", f64::from_bits(1)),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
        ("EPSILON", f64::EPSILON),
        ("MAX_SAFE_INTEGER", 9_007_199_254_740_991.0),
        ("MIN_SAFE_INTEGER", -9_007_199_254_740_991.0),
    ] {
        e.set_native_prop(num_id, name, Val::Num(v));
    }

    // -- BigInt ------------------------------------------------------------
    // i128-backed (the documented range); see the BigInt helpers in interp.
    let bigint_id = e.register_native("BigInt", n_bigint, 0);
    e.set_global("BigInt", Val::Native(bigint_id));
    let as_int = e.register_native("BigInt.asIntN", n_bigint, 1);
    let as_uint = e.register_native("BigInt.asUintN", n_bigint, 2);
    e.set_native_prop(bigint_id, "asIntN", Val::Native(as_int));
    e.set_native_prop(bigint_id, "asUintN", Val::Native(as_uint));

    // -- primitive prototypes ----------------------------------------------
    // Bundles probe `"trimStart" in String.prototype` to decide whether to
    // install a polyfill, and borrow the method (`String.prototype.slice
    // .call(x)`). The primitive dispatch table in `get_member` stays the real
    // implementation; these prototype objects expose the same natives as
    // borrowed callables, so both patterns work.
    let from_char_code = e.register_native("String.fromCharCode", n_str_static, 0);
    e.set_native_prop(str_id, "fromCharCode", Val::Native(from_char_code));
    let str_raw = e.register_native("String.raw", n_str_static, 1);
    e.set_native_prop(str_id, "raw", Val::Native(str_raw));
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
    let fn_ctor = e.register_native("Function", n_fn_ctor, 0);
    e.set_native_prop(fn_ctor, "prototype", fn_proto);
    e.set_global("Function", Val::Native(fn_ctor));

    e.set_global("undefined", Val::Undefined);
    e.set_global("NaN", Val::Num(f64::NAN));
    e.set_global("Infinity", Val::Num(f64::INFINITY));

    let pi = e.register_native("parseInt", n_num_globals, 0);
    let pf = e.register_native("parseFloat", n_num_globals, 1);
    let ie = e.register_native("isNaN", n_num_globals, 2);
    let ifn = e.register_native("isFinite", n_num_globals, 3);
    // `Number.isNaN` is the strict form (no coercion): `Number.isNaN("NaN")`
    // is false. GitHub's relative-time uses it to test a `Date.parse` result.
    let n_num_is_nan = e.register_native("Number.isNaN", n_num_globals, 4);
    e.set_native_prop(num_id, "isNaN", Val::Native(n_num_is_nan));
    // The rest of the strict Number statics. GitHub's predicate library in
    // `dp-*.js` calls `Number.isFinite` and `Number.isInteger` on every value
    // it inspects, and both were missing, so the code-view module threw
    // "undefined is not a function" while loading.
    let n_num_is_int = e.register_native("Number.isInteger", n_num_globals, 5);
    e.set_native_prop(num_id, "isInteger", Val::Native(n_num_is_int));
    let n_num_is_finite = e.register_native("Number.isFinite", n_num_globals, 6);
    e.set_native_prop(num_id, "isFinite", Val::Native(n_num_is_finite));
    let n_num_is_safe = e.register_native("Number.isSafeInteger", n_num_globals, 7);
    e.set_native_prop(num_id, "isSafeInteger", Val::Native(n_num_is_safe));
    // the spec makes these the *same* function objects as the globals
    e.set_native_prop(num_id, "parseInt", Val::Native(pi));
    e.set_native_prop(num_id, "parseFloat", Val::Native(pf));
    e.set_global("parseInt", Val::Native(pi));
    e.set_global("parseFloat", Val::Native(pf));
    e.set_global("isNaN", Val::Native(ie));
    e.set_global("isFinite", Val::Native(ifn));

    let ev = e.register_native("eval", n_eval, 0);
    e.set_global("eval", Val::Native(ev));

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

    // queue-only microtask: drains with the promise reactions at turn end
    let qm = e.register_native("queueMicrotask", n_queue_microtask, 0);
    e.set_global("queueMicrotask", Val::Native(qm));

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
    let pwr = e.register_native("Promise.withResolvers", n_promise, PS_WITH_RESOLVERS);
    e.set_global("Promise", Val::Native(pc));
    obj_set_own(own(&promise), "resolve", Val::Native(prs));
    obj_set_own(own(&promise), "reject", Val::Native(prj));
    obj_set_own(own(&promise), "all", Val::Native(pall));
    obj_set_own(own(&promise), "any", Val::Native(pany));
    obj_set_own(own(&promise), "race", Val::Native(pray));
    obj_set_own(own(&promise), "allSettled", Val::Native(pass));
    obj_set_own(own(&promise), "withResolvers", Val::Native(pwr));
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
    let set_values = obj_get_own(own(&set_proto), "values");
    obj_set_own(own(&set_proto), crate::js::SYMBOL_ITERATOR, set_values);
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
    let map_entries = obj_get_own(own(&map_proto), "entries");
    obj_set_own(own(&map_proto), crate::js::SYMBOL_ITERATOR, map_entries);
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
    // `Symbol.for` / `Symbol.keyFor`: the registry is what React's bundle
    // keys its element type on (`Symbol.for("react.transitional.element")`).
    let sf = e.register_native("Symbol.for", n_symbol_static, SYM_FOR);
    let kf = e.register_native("Symbol.keyFor", n_symbol_static, SYM_KEY_FOR);
    e.set_native_prop(sym, "for", Val::Native(sf));
    e.set_native_prop(sym, "keyFor", Val::Native(kf));
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

    // -- typed arrays / ArrayBuffer / Buffer --------------------------------
    // Real byte storage: `ArrayBuffer` owns a `Vec<u8>`; every typed array is
    // a fixed-width view (kind + byte offset + element count) into it.
    let typed_proto = Val::new_object();
    let typed_set = e.register_native("TypedArray.prototype.set", n_typed, T_SET);
    let typed_sub = e.register_native("TypedArray.prototype.subarray", n_typed, T_SUBARRAY);
    let typed_slice = e.register_native("TypedArray.prototype.slice", n_typed, T_SLICE);
    e.typed_set = typed_set;
    e.typed_subarray = typed_sub;
    e.typed_slice = typed_slice;
    obj_set_own(own(&typed_proto), "set", Val::Native(typed_set));
    obj_set_own(own(&typed_proto), "subarray", Val::Native(typed_sub));
    obj_set_own(own(&typed_proto), "slice", Val::Native(typed_slice));
    let typed_join_id = e.register_native("TypedArray.prototype.join", n_typed, T_JOIN);
    obj_set_own(own(&typed_proto), "join", Val::Native(typed_join_id));
    obj_set_own(own(&typed_proto), "toString", Val::Native(typed_join_id));

    let u8_to_b64 = e.register_native("Uint8Array.prototype.toBase64", n_u8, 0);
    let u8_from_b64 = e.register_native("Uint8Array.fromBase64", n_u8, 1);
    e.uint8_to_b64 = u8_to_b64;
    obj_set_own(own(&typed_proto), "toBase64", Val::Native(u8_to_b64));

    const TYPED_NAMES: [&str; 11] = [
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Uint32Array",
        "Float32Array",
        "Float64Array",
        "BigInt64Array",
        "BigUint64Array",
    ];
    for (i, name) in TYPED_NAMES.iter().enumerate() {
        let ctor = e.register_native(name, n_typed_ctor, i as u16);
        e.set_native_prop(ctor, "prototype", typed_proto.clone());
        let kind = TypedKind::from_index(i as u16).unwrap();
        e.set_native_prop(ctor, "BYTES_PER_ELEMENT", Val::Num(kind.size() as f64));
        if *name == "Uint8Array" {
            e.set_native_prop(ctor, "fromBase64", Val::Native(u8_from_b64));
        }
        e.set_global(name, Val::Native(ctor));
    }
    e.typed_proto = Some(own(&typed_proto).clone());

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
    let ab_proto = Val::new_object();
    let ab_len = e.register_native("get ArrayBuffer.prototype.byteLength", n_ab, 0);
    obj_set_own(
        own(&ab_proto),
        "byteLength",
        Val::Acc(Rc::new(AccEntry {
            getter: Some(Val::Native(ab_len)),
            setter: None,
        })),
    );
    let ab_slice = e.register_native("ArrayBuffer.prototype.slice", n_ab, 3);
    obj_set_own(own(&ab_proto), "slice", Val::Native(ab_slice));
    let ab_ctor = e.register_native("ArrayBuffer", n_ab, 1);
    e.set_native_prop(ab_ctor, "prototype", ab_proto.clone());
    let ab_view = e.register_native("ArrayBuffer.isView", n_ab, 2);
    e.set_native_prop(ab_ctor, "isView", Val::Native(ab_view));
    e.set_global("ArrayBuffer", Val::Native(ab_ctor));
    e.set_global("SharedArrayBuffer", Val::Native(ab_ctor));
    e.arraybuffer_proto = Some(own(&ab_proto).clone());

    // -- DataView -----------------------------------------------------------
    // One shared prototype holds every accessor; an instance carries
    // `__dv_buf` (ArrayBuffer), `__dv_off` and `__dv_len` (byte window).
    let dv_proto = Val::new_object();
    for (idx, name) in [
        (DV_GET_I8, "getInt8"),
        (DV_GET_U8, "getUint8"),
        (DV_GET_I16, "getInt16"),
        (DV_GET_U16, "getUint16"),
        (DV_GET_I32, "getInt32"),
        (DV_GET_U32, "getUint32"),
        (DV_GET_F32, "getFloat32"),
        (DV_GET_F64, "getFloat64"),
        (DV_GET_BIGI64, "getBigInt64"),
        (DV_GET_BIGU64, "getBigUint64"),
        (DV_SET_I8, "setInt8"),
        (DV_SET_U8, "setUint8"),
        (DV_SET_I16, "setInt16"),
        (DV_SET_U16, "setUint16"),
        (DV_SET_I32, "setInt32"),
        (DV_SET_U32, "setUint32"),
        (DV_SET_F32, "setFloat32"),
        (DV_SET_F64, "setFloat64"),
        (DV_SET_BIGI64, "setBigInt64"),
        (DV_SET_BIGU64, "setBigUint64"),
    ] {
        let id = e.register_native(name, n_data_view, idx);
        obj_set_own(own(&dv_proto), name, Val::Native(id));
    }
    for (idx, name) in [
        (DV_GET_BYTE_LENGTH, "byteLength"),
        (DV_GET_BYTE_OFFSET, "byteOffset"),
        (DV_GET_BUFFER, "buffer"),
    ] {
        let id = e.register_native(name, n_data_view, idx);
        obj_set_own(
            own(&dv_proto),
            name,
            Val::Acc(Rc::new(AccEntry {
                getter: Some(Val::Native(id)),
                setter: None,
            })),
        );
    }
    let dv_ctor = e.register_native("DataView", n_data_view, DV_CTOR);
    e.set_native_prop(dv_ctor, "prototype", dv_proto.clone());
    e.data_view_proto = Some(own(&dv_proto).clone());
    e.set_global("DataView", Val::Native(dv_ctor));

    // -- AbortController / AbortSignal --------------------------------------
    let sig_proto = Val::new_object();
    let sig_add = e.register_native("AbortSignal.prototype.addEventListener", n_abort, A_ADD);
    let sig_rm = e.register_native("AbortSignal.prototype.removeEventListener", n_abort, A_REMOVE);
    let sig_throw = e.register_native("AbortSignal.prototype.throwIfAborted", n_abort, A_THROW);
    for (id, name) in [
        (sig_add, "addEventListener"),
        (sig_rm, "removeEventListener"),
        (sig_throw, "throwIfAborted"),
    ] {
        obj_set_own(own(&sig_proto), name, Val::Native(id));
    }
    for (idx, name) in [
        (A_GET_ABORTED, "aborted"),
        (A_GET_REASON, "reason"),
    ] {
        let id = e.register_native(name, n_abort, idx);
        obj_set_own(
            own(&sig_proto),
            name,
            Val::Acc(Rc::new(AccEntry {
                getter: Some(Val::Native(id)),
                setter: None,
            })),
        );
    }
    let on_get = e.register_native("get AbortSignal.prototype.onabort", n_abort, A_GET_ON);
    let on_set = e.register_native("set AbortSignal.prototype.onabort", n_abort, A_SET_ON);
    obj_set_own(
        own(&sig_proto),
        "onabort",
        Val::Acc(Rc::new(AccEntry {
            getter: Some(Val::Native(on_get)),
            setter: Some(Val::Native(on_set)),
        })),
    );
    let sig_ctor = e.register_native("AbortSignal", n_abort, A_SIGNAL_CTOR);
    e.set_native_prop(sig_ctor, "prototype", sig_proto.clone());
    let sig_static = e.register_native("AbortSignal.abort", n_abort, A_STATIC_ABORT);
    e.set_native_prop(sig_ctor, "abort", Val::Native(sig_static));
    e.abort_signal_proto = Some(own(&sig_proto).clone());
    e.set_global("AbortSignal", Val::Native(sig_ctor));

    let ctrl_proto = Val::new_object();
    let ctrl_abort = e.register_native("AbortController.prototype.abort", n_abort, A_ABORT);
    obj_set_own(own(&ctrl_proto), "abort", Val::Native(ctrl_abort));
    let ctrl_ctor = e.register_native("AbortController", n_abort, A_CTOR);
    e.set_native_prop(ctrl_ctor, "prototype", ctrl_proto.clone());
    e.abort_controller_proto = Some(own(&ctrl_proto).clone());
    e.set_global("AbortController", Val::Native(ctrl_ctor));



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
const M_SIN: u16 = 11;
const M_COS: u16 = 12;
const M_TAN: u16 = 13;
const M_ASIN: u16 = 14;
const M_ACOS: u16 = 15;
const M_ATAN: u16 = 16;
const M_ATAN2: u16 = 17;

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
        M_SIN => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).sin()),
        M_COS => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).cos()),
        M_TAN => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).tan()),
        M_ASIN => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).asin()),
        M_ACOS => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).acos()),
        M_ATAN => Val::Num(args.first().map_or(f64::NAN, |v| v.to_number()).atan()),
        // Math.atan2(y, x): the y argument is the receiver of Rust's atan2
        M_ATAN2 => Val::Num(
            args.first()
                .map_or(f64::NAN, |v| v.to_number())
                .atan2(args.get(1).map_or(f64::NAN, |v| v.to_number())),
        ),
        _ => Val::Undefined,
    })
}

// -- performance ------------------------------------------------------------

/// Process-wide monotonic epoch for `performance.now()` and the rAF
/// timestamp. Fixed on first access so both agree forever.
fn perf_epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

/// Milliseconds since [`perf_epoch`]. This is the value `performance.now()`
/// returns and the sole argument a `requestAnimationFrame` callback gets.
pub(crate) fn perf_now_ms() -> f64 {
    perf_epoch().elapsed().as_secs_f64() * 1000.0
}

/// Wall-clock milliseconds since the Unix epoch that `performance.now()`
/// zero corresponds to (`timeOrigin`).
fn perf_time_origin() -> f64 {
    static ORIGIN: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *ORIGIN.get_or_init(|| {
        let since_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        since_unix - perf_now_ms()
    })
}

const PF_NOW: u16 = 0;
const PF_MARK: u16 = 1;
const PF_MEASURE: u16 = 2;
const PF_ENTRIES: u16 = 3;

fn n_perf(_e: &mut JsEngine, idx: u16, _this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    Ok(match idx {
        PF_NOW => Val::Num(perf_now_ms()),
        // mark/measure keep no entry list; returning undefined matches the
        // spec's void return
        PF_MEASURE | PF_MARK => Val::Undefined,
        _ => Val::new_array(Vec::new()),
    })
}

// -- Web Crypto -------------------------------------------------------------

/// `crypto.getRandomValues(typedArray)` / `crypto.randomUUID()`.
fn n_crypto(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == 0 {
        // Fill a typed array in place and return it. Copy the buffer handle
        // out first: writing needs a `borrow_mut` on the same RefCell.
        let Some(Val::Obj(o)) = args.first() else {
            return Ok(Val::Undefined);
        };
        let (len, bytes_each, offset, buf) = {
            let b = o.borrow();
            match &b.kind {
                ObjKind::TypedArray(ta) => (ta.len, ta.kind.size(), ta.offset, ta.buf.clone()),
                _ => return Ok(Val::Undefined),
            }
        };
        let mut bytes = Vec::with_capacity(len * bytes_each);
        while bytes.len() < len * bytes_each {
            bytes.extend_from_slice(&e.random_u32().to_le_bytes());
        }
        let cell = {
            let r = buf.borrow();
            match &r.kind {
                ObjKind::ArrayBuffer(c) => c.clone(),
                _ => return Ok(Val::Undefined),
            }
        };
        let mut all = cell.borrow_mut();
        for (i, &b) in bytes.iter().take(len * bytes_each).enumerate() {
            if offset + i < all.len() {
                all[offset + i] = b;
            }
        }
        return Ok(args.first().cloned().unwrap_or(Val::Undefined));
    }
    // RFC 4122 version 4: 16 random bytes, then the version and variant bits
    let mut b = [0u8; 16];
    for chunk in b.chunks_mut(4) {
        chunk.copy_from_slice(&e.random_u32().to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex = |s: &[u8]| s.iter().map(|x| format!("{x:02x}")).collect::<String>();
    Ok(Val::Str(Rc::from(
        format!(
            "{}-{}-{}-{}-{}",
            hex(&b[0..4]),
            hex(&b[4..6]),
            hex(&b[6..8]),
            hex(&b[8..10]),
            hex(&b[10..16])
        )
        .as_str(),
    )))
}

// -- JSON -------------------------------------------------------------------

const J_PARSE: u16 = 0;
const J_STRINGIFY: u16 = 1;

/// Parse JSON text into a value. Shared by `JSON.parse` and the
/// `<script type="importmap">` reader (which needs the object, not the global).
pub(crate) fn json_parse_value(s: &str) -> Result<Val, JsError> {
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

fn n_json(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        J_PARSE => {
            let s = args
                .first()
                .map_or("".to_string(), |v| v.to_string());
            json_parse_value(&s)
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
        // JSON has no BigInt representation (toJSON is not consulted here)
        Val::BigInt(_) => {
            return Err(JsError::throw(
                "TypeError: Do not know how to serialize a BigInt",
            ))
        }
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
                ObjKind::Error(_) | ObjKind::Promise(_) => {
                    // shortcut: no toJSON support; these serialize empty
                    out.push_str("{}");
                }
                // regex/generator instances: no own enumerable props -> {}
                ObjKind::Regex(_)
                | ObjKind::Generator(_)
                | ObjKind::Style(_)
                | ObjKind::Live(_) => {
                    // live collections have no enumerable own data props
                    out.push_str("{}");
                }
                // a namespace serializes as an object of its exports
                ObjKind::ModuleNs(xs) => {
                    out.push('{');
                    let entries: Vec<(Rc<str>, Val)> = xs
                        .borrow()
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    for (i, (k, v)) in entries.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        json_str(k, out);
                        out.push(':');
                        if !json_write(e, v, depth + 1, out)? {
                            out.push_str("null");
                        }
                    }
                    out.push('}');
                }
                ObjKind::Iterator(_) | ObjKind::ArrayBuffer(_) => {
                    out.push_str("{}");
                }
                // a boxed primitive serializes as the primitive itself
                ObjKind::Boxed(v) => return json_write(e, v, depth, out),
                // typed arrays serialize their indexed elements, like arrays
                ObjKind::TypedArray(ta) => {
                    out.push('{');
                    for i in 0..ta.len {
                        if i > 0 {
                            out.push(',');
                        }
                        json_str(&Rc::from(i.to_string().as_str()), out);
                        out.push(':');
                        let v = crate::js::typed_read(ta, i);
                        if !json_write(e, &v, depth + 1, out)? {
                            out.push_str("null");
                        }
                    }
                    out.push('}');
                }
                ObjKind::Plain(..) => {
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
    // `enumerable` defaults to false, as every property descriptor flag does:
    // `Object.defineProperty(o, "x", { value: 1 })` adds a property that
    // `for-in`, `Object.keys` and object-rest do not see. color-convert
    // depends on it (`defineProperty(space, "channels", { value })` must not
    // show up as a target color space).
    let hidden = match d {
        Some(Val::Obj(ob)) => ob
            .borrow()
            .props
            .iter()
            .find(|(k, _)| k.as_ref() == "enumerable")
            .map(|(_, v)| !v.to_bool())
            .unwrap_or(true),
        _ => true,
    };
    match target {
        Val::Obj(o) => {
            obj_set_own(o, key, v);
            if hidden {
                obj_hide(o, &intern_key(key));
            }
        }
        Val::Func(f) => {
            let mut props = f.props.borrow_mut();
            props.retain(|(k, _)| k.as_ref() != key);
            props.push((intern_key(key), v));
            drop(props);
            if hidden {
                clo_hide(f, &intern_key(key));
            }
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
                        ObjKind::ModuleNs(xs) => xs
                            .borrow()
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                        // class methods/accessors are own but not enumerable
                        _ => b
                            .props
                            .iter()
                            .filter(|(k, _)| {
                                !matches!(&b.kind, ObjKind::Plain(Some(h), _)
                                    if h.iter().any(|x| x == k))
                            })
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
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
                    let mut b = o.borrow_mut();
                    // remember an explicit null separately from "not attached
                    // yet" (`Obj.proto == None` means both)
                    match proto {
                        Val::Obj(p) => {
                            b.proto = Some(p);
                            if let ObjKind::Plain(h, _) = &b.kind {
                                b.kind = ObjKind::Plain(h.clone(), false);
                            }
                        }
                        Val::Null => {
                            b.proto = None;
                            if let ObjKind::Plain(h, _) = &b.kind {
                                b.kind = ObjKind::Plain(h.clone(), true);
                            }
                        }
                        // any other value leaves the prototype alone
                        _ => {}
                    }
                }
                Val::Func(f) => f.set_proto(next),
                _ => {}
            }
            Ok(target)
        }
        O_GET_PROTO => {
            let target = args.first().cloned().unwrap_or(Val::Undefined);
            Ok(match &target {
                // report the lazy chain too: a fresh array/iterator has no
                // explicit `proto` until first member access
                Val::Obj(o) => e.proto_of(o).map(Val::Obj).unwrap_or(Val::Null),
                // class `extends` stores a function as the [[Prototype]];
                // otherwise it is Function.prototype
                Val::Func(f) => f
                    .proto()
                    .or_else(|| e.function_proto.clone().map(Val::Obj))
                    .unwrap_or(Val::Null),
                Val::Native(_) | Val::Method(_) | Val::Acc(_) => {
                    e.function_proto.clone().map(Val::Obj).unwrap_or(Val::Null)
                }
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
            // a `null` prototype is recorded in the object kind: the object
            // has no prototype to walk, and `getPrototypeOf` must answer null
            // instead of lazily attaching Object.prototype
            match args.first() {
                Some(Val::Obj(p)) => Ok(Val::Obj(new_obj(
                    ObjKind::Plain(None, false),
                    Vec::new(),
                    Some(p.clone()),
                ))),
                Some(Val::Null) | None => Ok(Val::Obj(new_obj(
                    ObjKind::Plain(None, true),
                    Vec::new(),
                    None,
                ))),
                _ => Ok(Val::Obj(new_obj(ObjKind::Plain(None, false), Vec::new(), None))),
            }
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
                        ObjKind::TypedArray(ta) => (0..ta.len)
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

// -- Reflect ---------------------------------------------------------------
// Thin dispatch over the same helpers the Object statics and the operators
// use, so behavior cannot drift. Reflect returns booleans where the spec says
// so, while the Object.* forms stay lenient (they return the target).
//
// shortcut: no integrity levels — `isExtensible` is always true and
// `preventExtensions` only reports success (Object.freeze/seal are identity
// for the same reason).

const R_APPLY: u16 = 0;
const R_CONSTRUCT: u16 = 1;
const R_DEFINE_PROP: u16 = 2;
const R_DELETE_PROP: u16 = 3;
const R_GET: u16 = 4;
const R_GET_DESC: u16 = 5;
const R_GET_PROTO: u16 = 6;
const R_HAS: u16 = 7;
const R_IS_EXTENSIBLE: u16 = 8;
const R_OWN_KEYS: u16 = 9;
const R_PREVENT_EXTENSIONS: u16 = 10;
const R_SET: u16 = 11;
const R_SET_PROTO: u16 = 12;

/// The list argument of `Reflect.apply`/`Reflect.construct`. The engine has no
/// array-likes (and no `arguments` object), so a real array is the accepted
/// shape; a missing list is empty. shortcut: the spec requires the argument.
fn arg_list(v: Option<&Val>) -> Result<Vec<Val>, JsError> {
    match v {
        None | Some(Val::Undefined) | Some(Val::Null) => Ok(Vec::new()),
        Some(Val::Obj(o)) => match &o.borrow().kind {
            ObjKind::Array(items) => Ok(items.clone()),
            _ => Err(JsError::throw(
                "TypeError: Reflect argument list is not an array",
            )),
        },
        _ => Err(JsError::throw(
            "TypeError: Reflect argument list is not an array",
        )),
    }
}

/// `new Proxy(target, handler)` - see the Proxy shortcut in the installer.
fn n_proxy_ctor(
    _e: &mut JsEngine,
    _idx: u16,
    _this: &Val,
    args: &[Val],
) -> Result<Val, JsError> {
    let target = args.first().cloned().unwrap_or(Val::Undefined);
    if !matches!(target, Val::Obj(_) | Val::Func(_) | Val::Native(_)) {
        return Err(JsError::throw("TypeError: Proxy target must be an object"));
    }
    let handler = args.get(1).cloned().unwrap_or(Val::Undefined);
    if !matches!(handler, Val::Obj(_)) {
        return Err(JsError::throw("TypeError: Proxy handler must be an object"));
    }
    let slot = Val::new_object();
    slot.set("target", target);
    slot.set("handler", handler);
    let p = Val::new_object();
    p.set("__proxy", slot);
    if let Val::Obj(o) = &p {
        obj_hide(o, &Rc::from("__proxy"));
    }
    Ok(p)
}

fn n_reflect(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let target = args.first().cloned().unwrap_or(Val::Undefined);
    Ok(match idx {
        R_APPLY => {
            if !is_callable(&target) {
                return Err(JsError::throw(
                    "TypeError: Reflect.apply target is not a function",
                ));
            }
            let this_arg = args.get(1).cloned().unwrap_or(Val::Undefined);
            e.call(target, this_arg, arg_list(args.get(2))?)?
        }
        R_CONSTRUCT => {
            if !is_callable(&target) {
                return Err(JsError::throw(
                    "TypeError: Reflect.construct target is not a constructor",
                ));
            }
            super::interp::construct(e, target, arg_list(args.get(1))?)?
        }
        R_DEFINE_PROP => {
            let key = arg_str(args, 1);
            define_prop(e, &target, &key, args.get(2))?;
            Val::Bool(true)
        }
        R_DELETE_PROP => Val::Bool(delete_member(&target, &arg_str(args, 1))),
        R_GET => get_member(e, &target, &arg_str(args, 1))?,
        R_SET => {
            set_member(
                e,
                &target,
                &arg_str(args, 1),
                args.get(2).cloned().unwrap_or(Val::Undefined),
            )?;
            Val::Bool(true)
        }
        R_GET_DESC => n_object(e, O_GET_DESC, &Val::Undefined, args)?,
        R_GET_PROTO => n_object(e, O_GET_PROTO, &Val::Undefined, args)?,
        R_SET_PROTO => {
            n_object(e, O_SET_PROTO, &Val::Undefined, args)?;
            Val::Bool(true)
        }
        R_HAS => Val::Bool(has_member(e, &target, &arg_str(args, 1))?),
        R_IS_EXTENSIBLE | R_PREVENT_EXTENSIONS => Val::Bool(true),
        R_OWN_KEYS => n_object(e, O_PROP_NAMES, &Val::Undefined, args)?,
        _ => Val::Undefined,
    })
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
                ObjKind::Plain(..) => "[object Object]",
                ObjKind::Boxed(v) => match v {
                    Val::Str(_) => "[object String]",
                    Val::Num(_) => "[object Number]",
                    _ => "[object Boolean]",
                },
                ObjKind::Array(_) => "[object Array]",
                ObjKind::Error(_) => "[object Error]",
                ObjKind::Date(_) => "[object Date]",
                ObjKind::Promise(_) => "[object Promise]",
                ObjKind::Regex(_) => "[object RegExp]",
                ObjKind::Generator(_) => "[object Generator]",
                ObjKind::Style(_) => "[object CSSStyleDeclaration]",
                ObjKind::Live(_) => "[object HTMLCollection]",
                ObjKind::ModuleNs(_) => "[object Module]",
                ObjKind::Iterator(_) => "[object Iterator]",
                ObjKind::ArrayBuffer(_) => "[object ArrayBuffer]",
                ObjKind::TypedArray(ta) => match ta.kind {
                    TypedKind::I8 => "[object Int8Array]",
                    TypedKind::U8 => "[object Uint8Array]",
                    TypedKind::U8Clamp => "[object Uint8ClampedArray]",
                    TypedKind::I16 => "[object Int16Array]",
                    TypedKind::U16 => "[object Uint16Array]",
                    TypedKind::I32 => "[object Int32Array]",
                    TypedKind::U32 => "[object Uint32Array]",
                    TypedKind::F32 => "[object Float32Array]",
                    TypedKind::F64 => "[object Float64Array]",
                    TypedKind::BigI64 => "[object BigInt64Array]",
                    TypedKind::BigU64 => "[object BigUint64Array]",
                },
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
                    "name" | "length" => {
                        !c.synth_deleted(&key)
                            || c.props.borrow().iter().any(|(k, _)| k.as_ref() == key.as_str())
                    }
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
            let mut cur = e.proto_of(&start);
            for _ in 0..PROTO_WALK_CAP {
                let c = match cur {
                    None => break,
                    Some(c) => c,
                };
                if Rc::ptr_eq(&c, &want) {
                    return Ok(Val::Bool(true));
                }
                cur = e.proto_of(&c);
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
const A_FIND: u16 = 19;
const A_FINDINDEX: u16 = 20;
const A_FINDLAST: u16 = 21;
const A_FINDLASTINDEX: u16 = 22;
const A_AT: u16 = 23;
const A_FILL: u16 = 24;
const A_COPY_WITHIN: u16 = 25;
const A_FLAT: u16 = 26;
const A_FLATMAP: u16 = 27;
const A_VALUES: u16 = 28;
const A_KEYS: u16 = 29;
const A_ENTRIES: u16 = 30;
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
/// enumerability except the class-member hidden list) or an accessor pair.
fn desc_from_value(v: Val) -> Val {
    desc_with_enumerable(v, true)
}

fn desc_with_enumerable(v: Val, enumerable: bool) -> Val {
    desc_attrs(v, true, enumerable, true)
}

/// Descriptor object with explicit attribute values (the engine tracks no
/// writability/configurability per ordinary property, so callers pass the
/// spec answer for the handful of synthesized slots that need it).
fn desc_attrs(v: Val, writable: bool, enumerable: bool, configurable: bool) -> Val {
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
            obj_set_own(own(&d), "writable", Val::Bool(writable));
        }
    }
    obj_set_own(own(&d), "enumerable", Val::Bool(enumerable));
    obj_set_own(own(&d), "configurable", Val::Bool(configurable));
    d
}

/// `Object.getOwnPropertyDescriptor` shape: data properties always report
/// writable/configurable (the engine tracks neither); enumerability is false
/// for a class method/accessor.
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
    let hidden = match &b.kind {
        ObjKind::Plain(Some(h), _) => h.iter().any(|x| x.as_ref() == key),
        _ => false,
    };
    Some(desc_with_enumerable(v, !hidden))
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
    if let Some(v) = found {
        // an ordinary function property is writable/configurable; class
        // members are only non-enumerable
        return Some(desc_with_enumerable(v, !f.is_hidden(key)));
    }
    match key {
        // `name` and `length` are non-writable, non-enumerable, configurable
        "name" if !f.synth_deleted("name") => Some(desc_attrs(
            Val::Str(f.def.name.clone()),
            false,
            false,
            true,
        )),
        "length" if !f.synth_deleted("length") => {
            Some(desc_attrs(Val::Num(f.def.length() as f64), false, false, true))
        }
        // a real function's `prototype` is writable, non-enumerable and
        // non-configurable
        "prototype" if !f.is_arrow && !matches!(f.def.body, FnBody::Bound { .. }) => {
            Some(desc_attrs(
                Val::Obj(super::interp::ensure_prototype(f)),
                true,
                false,
                false,
            ))
        }
        _ => None,
    }
}

fn need_array(e: &mut JsEngine, this: &Val) -> Result<Rc<RefCell<Obj>>, JsError> {
    match this {
        Val::Obj(o) => {
            if matches!(o.borrow().kind, ObjKind::Array(_)) {
                return Ok(o.clone());
            }
            // generic `Array.prototype.*.call(collection, ...)`: a live host
            // collection behaves as an array-like, so hand the method a
            // one-shot snapshot (writes land in the snapshot, not the DOM)
            let live = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Live(s) => Some(s.clone()),
                    _ => None,
                }
            };
            if let Some(spec) = live {
                let items = e.live_values.map(|f| f(&spec)).unwrap_or_default();
                return Ok(new_obj(ObjKind::Array(items), Vec::new(), None));
            }
            // any other array-like, `length` and indices read through the
            // prototype chain: `Slice`/`Map` are generic in the spec, and
            // `Object.create(someArray).slice(0)` is how selector libraries
            // extend a matcher list
            if let Val::Num(n) = chain_get(o, "length") {
                if n >= 0.0 && n.fract() == 0.0 {
                    let n = n as usize;
                    let items: Vec<Val> = (0..n)
                        .map(|i| chain_get(o, &i.to_string()))
                        .collect();
                    return Ok(new_obj(ObjKind::Array(items), Vec::new(), None));
                }
            }
            // name the receiver: a wrapper or something else entirely -
            // "non-array" alone does not say which
            let what = match obj_get_own(o, "tagName") {
                Val::Str(s) => format!("<{}> wrapper", s),
                _ => "plain object".to_string(),
            };
            Err(JsError::throw(&format!(
                "TypeError: array method on non-array (this is a {what}){}",
                e.call_site_note()
            )))
        }
        _ => Err(JsError::throw(&format!(
            "TypeError: array method on non-array{}",
            e.call_site_note()
        ))),
    }
}

/// `Get` through the prototype chain for data props, an array's indexed items
/// and its `length` — enough for the generic array methods to accept
/// array-likes (`Object.create(array)`, DOM collections, `arguments`).
fn chain_get(o: &Rc<RefCell<Obj>>, key: &str) -> Val {
    let mut cur = Some(o.clone());
    for _ in 0..PROTO_WALK_CAP {
        let Some(c) = cur else { break };
        let b = c.borrow();
        if let ObjKind::Array(items) = &b.kind {
            if key == "length" {
                return Val::Num(items.len() as f64);
            }
            if let Ok(i) = key.parse::<usize>() {
                if let Some(v) = items.get(i) {
                    return v.clone();
                }
            }
        }
        if let Some((_, v)) = b.props.iter().find(|(k, _)| k.as_ref() == key) {
            return v.clone();
        }
        cur = b.proto.clone();
    }
    Val::Undefined
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
                Some(v) => match super::interp::for_of_items(e, v) {
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

/// Flatten `items` into `out` down to `depth` levels. `flat`/`flatMap` share
/// it; `depth` is clamped to 64 so a nested array built by script cannot
/// recurse without bound (shortcut: deeper nesting is flattened 64 levels and
/// keeps the rest as arrays, where the spec would keep going).
fn flatten_into(items: &[Val], depth: f64, out: &mut Vec<Val>) {
    for it in items {
        if depth > 0.0 {
            let nested = match it {
                Val::Obj(o) => match &o.borrow().kind {
                    ObjKind::Array(v) => Some(v.clone()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(v) = nested {
                flatten_into(&v, depth - 1.0, out);
                continue;
            }
        }
        out.push(it.clone());
    }
}

/// `ToIntegerOrInfinity` for the relative index arguments (`at`/`fill`/
/// `copyWithin`): negative counts from the end, out of range clamps.
fn rel_index(v: Option<&Val>, fallback: f64, len: usize) -> usize {
    let n = match v {
        None | Some(Val::Undefined) => fallback,
        Some(v) => v.to_number(),
    };
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    if n < 0.0 {
        (len as f64 + n).max(0.0) as usize
    } else {
        (n as usize).min(len)
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
    let o = need_array(e, this)?;
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
        A_FIND | A_FINDINDEX | A_FINDLAST | A_FINDLASTINDEX => {
            // Predicate callbacks get (element, index, thisArg) like the other
            // iterators above (shortcut: the engine passes the global object
            // as both `this` and the third argument, not the array).
            let f = args.first().cloned().unwrap_or(Val::Undefined);
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            let this = Val::Obj(e.global_obj.clone());
            let back = matches!(idx, A_FINDLAST | A_FINDLASTINDEX);
            let len = items.len();
            for k in 0..len {
                let i = if back { len - 1 - k } else { k };
                let r = e.call(
                    f.clone(),
                    this.clone(),
                    vec![items[i].clone(), Val::Num(i as f64), this.clone()],
                )?;
                if r.to_bool() {
                    return Ok(match idx {
                        A_FIND | A_FINDLAST => items[i].clone(),
                        _ => Val::Num(i as f64),
                    });
                }
            }
            match idx {
                A_FIND | A_FINDLAST => Val::Undefined,
                _ => Val::Num(-1.0),
            }
        }
        A_AT => {
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            let n = args.first().map(|v| v.to_number()).unwrap_or(0.0);
            let n = if n.is_nan() { 0.0 } else { n.trunc() };
            let i = if n < 0.0 { items.len() as f64 + n } else { n };
            if i < 0.0 || i >= items.len() as f64 {
                Val::Undefined
            } else {
                items[i as usize].clone()
            }
        }
        A_FILL => {
            let value = args.first().cloned().unwrap_or(Val::Undefined);
            let len = match &o.borrow().kind {
                ObjKind::Array(items) => items.len() as i64,
                _ => 0,
            };
            let (a, b) = slice_bounds(args.get(1).map_or(0.0, |v| v.to_number()), args.get(2), len);
            if let ObjKind::Array(slot) = &mut o.borrow_mut().kind {
                for it in slot.iter_mut().take(b).skip(a) {
                    *it = value.clone();
                }
            }
            Val::Obj(o.clone())
        }
        A_COPY_WITHIN => {
            let len = match &o.borrow().kind {
                ObjKind::Array(items) => items.len(),
                _ => 0,
            };
            let target = rel_index(args.first(), 0.0, len);
            let start = rel_index(args.get(1), 0.0, len);
            let end = rel_index(args.get(2), len as f64, len);
            let count = end.saturating_sub(start).min(len - target);
            if count > 0 {
                if let ObjKind::Array(slot) = &mut o.borrow_mut().kind {
                    // snapshot first: the ranges overlap when target > start
                    let src: Vec<Val> = slot[start..start + count].to_vec();
                    slot[target..target + count].clone_from_slice(&src);
                }
            }
            Val::Obj(o.clone())
        }
        A_FLAT | A_FLATMAP => {
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
            let mut out = Vec::new();
            if idx == A_FLAT {
                let depth = args.first().map(|v| v.to_number()).unwrap_or(1.0);
                let depth = if depth.is_nan() {
                    1.0
                } else {
                    depth.clamp(0.0, 64.0)
                };
                flatten_into(&items, depth, &mut out);
            } else {
                let f = args.first().cloned().unwrap_or(Val::Undefined);
                let this = Val::Obj(e.global_obj.clone());
                let mut mapped = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    mapped.push(e.call(
                        f.clone(),
                        this.clone(),
                        vec![item.clone(), Val::Num(i as f64), this.clone()],
                    )?);
                }
                flatten_into(&mapped, 1.0, &mut out);
            }
            Val::new_array(out)
        }
        A_VALUES | A_KEYS | A_ENTRIES => {
            let kind = match idx {
                A_KEYS => crate::js::IterKind::Keys,
                A_ENTRIES => crate::js::IterKind::Entries,
                _ => crate::js::IterKind::Values,
            };
            super::interp::make_iterator(Val::Obj(o.clone()), kind)
        }
        _ => Val::Undefined,
    })
}

// -- Error constructors -----------------------------------------------------

const E_ERROR: u16 = 0;
const E_TYPE: u16 = 1;
const E_RANGE: u16 = 2;
const E_SYNTAX: u16 = 3;
const E_REFERENCE: u16 = 4;
const E_EVAL: u16 = 5;
const E_URI: u16 = 6;

fn n_error_ctor(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let name: Rc<str> = Rc::from(match idx {
        E_TYPE => "TypeError",
        E_RANGE => "RangeError",
        E_SYNTAX => "SyntaxError",
        E_REFERENCE => "ReferenceError",
        E_EVAL => "EvalError",
        E_URI => "URIError",
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
            if !matches!(b.kind, ObjKind::Error(_)) {
                b.kind = ObjKind::Error(Rc::new(ErrorData {
                    name: name.clone(),
                    msg: msg.clone(),
                }));
                if b.proto.is_none() {
                    b.proto = proto.clone();
                }
                return Ok(this.clone());
            }
        }
    }
    Ok(Val::Obj(new_obj(
        ObjKind::Error(Rc::new(ErrorData { name, msg })),
        Vec::new(),
        proto,
    )))
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
        // `new Date()` / `new Date(ms)` / `new Date(string)`: mutate the fresh
        // object into a Date. The string form uses the same ISO parser as
        // `Date.parse`.
        let ms = match args.first() {
            None => now_ms,
            Some(v) => match v {
                Val::Str(s) => super::interp::parse_date_iso(s),
                _ => v.to_number(),
            },
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

// -- Intl -------------------------------------------------------------------

/// Hidden own prop on every `Intl.DateTimeFormat` / `Intl.NumberFormat`
/// instance; the value is a plain object holding the options that were
/// passed to the constructor.
const INTL_STATE: &str = "__intl";

fn n_intl(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    // idx 0/1 = DateTimeFormat format/ctor, 2/3 = NumberFormat format/ctor
    if idx % 2 == 1 {
        let inst = proto_instance(if idx == 1 {
            e.intl_dtf_proto.clone()
        } else {
            e.intl_nf_proto.clone()
        });
        let state = Val::new_object();
        let opts = args.get(1);
        let names: &[(&str, &str)] = if idx == 1 {
            &[
                ("year", "year"),
                ("month", "month"),
                ("day", "day"),
                ("weekday", "weekday"),
                ("hour", "hour"),
                ("minute", "minute"),
                ("second", "second"),
                ("tz", "timeZoneName"),
            ]
        } else {
            &[
                ("notation", "notation"),
                ("mfd", "maximumFractionDigits"),
                ("mnd", "minimumFractionDigits"),
                ("msd", "maximumSignificantDigits"),
            ]
        };
        for (key, name) in names {
            if let Some(v) = intl_option(e, opts, name)? {
                obj_set_own(own(&state), key, v);
            }
        }
        obj_set_own(own(&inst), INTL_STATE, state);
        return Ok(inst);
    }
    let state = intl_state(this)?;
    if idx == 0 {
        let ms = match args.first() {
            Some(Val::Obj(o)) => match &o.borrow().kind {
                ObjKind::Date(ms) => *ms,
                _ => args[0].to_number(),
            },
            Some(v) => v.to_number(),
            None => f64::NAN,
        };
        Ok(Val::Str(Rc::from(fmt_date_intl(e, &state, ms)?.as_str())))
    } else {
        let n = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
        Ok(Val::Str(Rc::from(fmt_num_intl(e, &state, n)?.as_str())))
    }
}

/// Read one option off the constructor's options object; `undefined`, `null`
/// and an absent object all mean "not given".
fn intl_option(e: &mut JsEngine, opts: Option<&Val>, name: &str) -> Result<Option<Val>, JsError> {
    let opts = match opts {
        Some(v) if !matches!(v, Val::Undefined | Val::Null) => v,
        _ => return Ok(None),
    };
    let v = get_member(e, opts, name)?;
    Ok(if matches!(v, Val::Undefined) {
        None
    } else {
        Some(v)
    })
}

fn intl_state(this: &Val) -> Result<Val, JsError> {
    if let Val::Obj(o) = this {
        let found = o
            .borrow()
            .props
            .iter()
            .find(|(k, _)| k.as_ref() == INTL_STATE)
            .map(|(_, v)| v.clone());
        if let Some(v) = found {
            return Ok(v);
        }
    }
    Err(JsError::throw("TypeError: Intl method called on an incompatible receiver"))
}

fn intl_get(e: &mut JsEngine, state: &Val, key: &str) -> Result<Option<Val>, JsError> {
    let v = get_member(e, state, key)?;
    Ok(if matches!(v, Val::Undefined) {
        None
    } else {
        Some(v)
    })
}

const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_LONG: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAYS_LONG: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// `Intl.DateTimeFormat.prototype.format` (shortcut: en-US names, UTC only).
fn fmt_date_intl(e: &mut JsEngine, state: &Val, ms: f64) -> Result<String, JsError> {
    let (y, mo, d, hh, mi, ss, _) =
        date_parts(ms).ok_or_else(|| JsError::throw("RangeError: Invalid time value"))?;
    let opt = |e: &mut JsEngine, key: &str| intl_get(e, state, key);
    let year = opt(e, "year")?;
    let month = opt(e, "month")?;
    let day = opt(e, "day")?;
    let weekday = opt(e, "weekday")?;
    let hour = opt(e, "hour")?;
    let minute = opt(e, "minute")?;
    let second = opt(e, "second")?;
    let tz = opt(e, "tz")?;

    let mut date = String::new();
    if let Some(wd) = &weekday {
        let days = (ms.floor() as i64).div_euclid(86_400_000);
        let i = (days + 4).rem_euclid(7) as usize;
        let names = if wd.to_string() == "long" {
            &WEEKDAYS_LONG
        } else {
            &WEEKDAYS_SHORT
        };
        date.push_str(names[i]);
        date.push_str(", ");
    }
    let month_s = month.as_ref().map(|v| v.to_string());
    let has_date = month_s.is_some() || year.is_some() || day.is_some() || weekday.is_some();
    if !has_date && hour.is_none() && minute.is_none() && second.is_none() {
        // no options at all: en-US default is a numeric date
        return Ok(format!("{}/{}/{}", mo, d, y));
    }
    if has_date {
        let numeric_month = matches!(month_s.as_deref(), Some("numeric") | Some("2-digit"));
        if numeric_month {
            date.push_str(&format!("{}/{}", mo, d));
            if let Some(_) = year {
                date.push_str(&format!("/{y}"));
            }
        } else if let Some(m) = &month_s {
            let name = if m == "long" {
                MONTHS_LONG[(mo - 1) as usize]
            } else if m == "narrow" {
                &MONTHS_SHORT[(mo - 1) as usize][..1]
            } else {
                MONTHS_SHORT[(mo - 1) as usize]
            };
            date.push_str(name);
            if day.is_some() {
                date.push_str(&format!(" {d}"));
            }
            if year.is_some() {
                date.push_str(&format!(", {y}"));
            }
        } else if day.is_some() {
            date.push_str(&d.to_string());
            if year.is_some() {
                date.push_str(&format!(", {y}"));
            }
        } else if let Some(_) = year {
            date.push_str(&y.to_string());
        }
    }

    let mut time = String::new();
    if hour.is_some() || minute.is_some() || second.is_some() {
        let h12 = if hh % 12 == 0 { 12 } else { hh % 12 };
        time.push_str(&format!("{h12}:{mi:02}"));
        if second.is_some() {
            time.push_str(&format!(":{ss:02}"));
        }
        time.push_str(if hh < 12 { " AM" } else { " PM" });
        if tz.is_some() {
            time.push_str(" UTC");
        }
    }
    Ok(match (date.is_empty(), time.is_empty()) {
        (false, false) => format!("{date}, {time}"),
        (false, true) => date,
        (true, false) => time,
        (true, true) => String::new(),
    })
}

/// `Intl.NumberFormat.prototype.format` (shortcut: en-US grouping only).
fn fmt_num_intl(e: &mut JsEngine, state: &Val, n: f64) -> Result<String, JsError> {
    if n.is_nan() {
        return Ok("NaN".into());
    }
    if n.is_infinite() {
        return Ok(if n < 0.0 { "-∞".into() } else { "∞".into() });
    }
    let notation = intl_get(e, state, "notation")?.map(|v| v.to_string());
    let num_opt = |e: &mut JsEngine, key: &str| -> Result<Option<f64>, JsError> {
        Ok(intl_get(e, state, key)?.and_then(|v| {
            let x = v.to_number();
            if x.is_finite() {
                Some(x)
            } else {
                None
            }
        }))
    };
    let mfd = num_opt(e, "mfd")?.map(|x| x.clamp(0.0, 20.0) as usize);
    if notation.as_deref() == Some("compact") {
        let msd = num_opt(e, "msd")?.map(|x| x.clamp(1.0, 21.0) as u32);
        return Ok(fmt_compact(n, mfd.unwrap_or(1), msd));
    }
    Ok(fmt_decimal(n, mfd.unwrap_or(3)))
}

/// Fixed-point-enough decimal with comma grouping, trailing fraction zeros
/// trimmed (the default `minimumFractionDigits` is 0).
fn fmt_decimal(n: f64, mfd: usize) -> String {
    let neg = n < 0.0;
    let s = format!("{:.*}", mfd, n.abs());
    let (int, frac) = match s.split_once('.') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (s, String::new()),
    };
    let mut out = group_int(&int);
    let frac = frac.trim_end_matches('0');
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    if neg && out != "0" {
        out.insert(0, '-');
    }
    out
}

fn fmt_compact(n: f64, mfd: usize, msd: Option<u32>) -> String {
    let neg = n < 0.0;
    let abs = n.abs();
    if abs < 1000.0 {
        return fmt_decimal(n, mfd);
    }
    let mut v = abs;
    let mut suffix = "";
    for (scale, unit) in [
        (1e3, "K"),
        (1e6, "M"),
        (1e9, "B"),
        (1e12, "T"),
    ] {
        if abs >= scale {
            v = abs / scale;
            suffix = unit;
        }
    }
    if let Some(sig) = msd {
        v = round_sig(v, sig);
    }
    let mut out = format!("{:.*}", mfd, v);
    if out.contains('.') {
        let trimmed = out.trim_end_matches('0').trim_end_matches('.');
        out = trimmed.to_string();
    }
    if neg {
        out.insert(0, '-');
    }
    out.push_str(suffix);
    out
}

fn round_sig(x: f64, sig: u32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let mag = x.abs().log10().floor();
    let factor = 10f64.powi(sig as i32 - 1 - mag as i32);
    (x * factor).round() / factor
}

/// Insert thousands separators into an unsigned integer string.
fn group_int(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
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
    ("concat", Meth::StrConcat),
    ("substr", Meth::StrSubstr),
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

/// The `Function` constructor compiles real source: `new Function("a", "return a + 1")`
/// builds a sloppy-mode function whose scope is the global one (no closure
/// over the caller). The wrapper's parameter list is parsed by the ordinary
/// parser, so destructuring/rest/default parameters work; the body stays
/// unexecuted until the function is called.
fn n_fn_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    use super::parser::{self, StmtKind};
    let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect();
    let (params, body) = match parts.split_last() {
        Some((body, params)) => (params.join(", "), body.clone()),
        None => (String::new(), String::new()),
    };
    let src = format!("function anonymous({params}\n) {{\n{body}\n}}");
    let src_id = e.register_source(&src);
    let prog = parser::parse(&src, src_id).map_err(|err| match err {
        // the reported position is inside our wrapper; the message is what
        // matters (real engines expose body-relative positions)
        JsError::Syntax { msg, .. } => JsError::throw(&format!("SyntaxError: {msg}")),
        other => other,
    })?;
    let def = prog
        .into_iter()
        .find_map(|st| match st.kind {
            StmtKind::FuncDecl(f) => Some(f),
            _ => None,
        })
        .ok_or_else(|| JsError::throw("SyntaxError: invalid function body"))?;
    Ok(Val::Func(new_closure(def, e.global_env.clone(), None, false)))
}

/// `String` static helpers. `fromCharCode` builds the string from UTF-16 code
/// units (surrogates are pairs; lone surrogates become U+FFFD because the
/// engine's `Val::Str` is UTF-8).
fn n_str_static(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == 1 {
        // `String.raw(strings, ...subs)`: the template's raw text with each
        // substitution placed between the segments. GitHub's search filter
        // builds its regex as ``RegExp(String.raw`\b(?:${words})\S*`, "gi")``.
        let strings = args.first().cloned().unwrap_or(Val::Undefined);
        let mut raw = get_member(e, &strings, "raw")?;
        if matches!(raw, Val::Undefined) {
            raw = strings;
        }
        let len = get_member(e, &raw, "length")?.to_number();
        let len = if len.is_finite() && len > 0.0 {
            len as usize
        } else {
            0
        };
        let mut out = String::new();
        for i in 0..len {
            if i > 0 {
                if let Some(v) = args.get(i) {
                    out.push_str(&v.to_string());
                }
            }
            let seg = get_member(e, &raw, &i.to_string())?;
            out.push_str(&seg.to_string());
        }
        return Ok(Val::Str(Rc::from(out.as_str())));
    }
    let units: Vec<u16> = args
        .iter()
        .map(|v| {
            let n = v.to_number();
            if n.is_finite() {
                ((n.trunc() as i64).rem_euclid(65536)) as u16
            } else {
                0
            }
        })
        .collect();
    Ok(Val::Str(Rc::from(String::from_utf16_lossy(&units))))
}

fn n_convert(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let v = args.first().cloned().unwrap_or(Val::Undefined);
    // `new String(..)` / `new Number(..)` / `new Boolean(..)` box the
    // primitive. Methods live on the ordinary prototype, so the box carries
    // that prototype explicitly (no lazy attach: a boxed primitive must not
    // pick up Object.prototype).
    if is_ctor_call(e, this) {
        let (prim, ctor) = match idx {
            0 => (Val::Str(Rc::from(v.to_string().as_str())), "String"),
            1 => (Val::Num(v.to_number()), "Number"),
            _ => (Val::Bool(v.to_bool()), "Boolean"),
        };
        let proto = e
            .native_id(ctor)
            .and_then(|id| e.native_prop(id, "prototype"))
            .and_then(|p| match p {
                Val::Obj(o) => Some(o),
                _ => None,
            });
        return Ok(Val::Obj(new_obj(ObjKind::Boxed(prim), Vec::new(), proto)));
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
        4 => Val::Bool(matches!(args.first(), Some(Val::Num(n)) if n.is_nan())),
        5 => Val::Bool(matches!(args.first(), Some(Val::Num(n)) if n.is_finite() && n.fract() == 0.0)),
        6 => Val::Bool(matches!(args.first(), Some(Val::Num(n)) if n.is_finite())),
        7 => Val::Bool(matches!(args.first(), Some(Val::Num(n))
            if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0)),
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

/// Indirect `eval`: reached when the intrinsic is called through any path
/// other than a direct `eval(...)` (a variable, `Reflect.apply`, ...). Spec
/// runs those in the global scope, which is also what the interpreter's
/// direct-eval hook leaves for this function.
fn n_eval(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let env = e.global_env.clone();
    let arg = args.first().cloned().unwrap_or(Val::Undefined);
    super::interp::eval_source(e, &env, arg)
}

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

fn n_queue_microtask(
    e: &mut JsEngine,
    _idx: u16,
    _this: &Val,
    args: &[Val],
) -> Result<Val, JsError> {
    let cb = args.first().cloned().unwrap_or(Val::Undefined);
    if !matches!(cb, Val::Func(_) | Val::Native(_) | Val::Method(_)) {
        return Err(JsError::throw(
            "TypeError: queueMicrotask callback is not a function",
        ));
    }
    e.microtasks.push(Job::Call(cb));
    Ok(Val::Undefined)
}

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
const PS_WITH_RESOLVERS: u16 = 7;

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
        PS_WITH_RESOLVERS => Ok(promise_with_resolvers(e)),
        PS_ALL | PS_ANY | PS_RACE | PS_ALL_SETTLED => {
            let items = interp::for_of_items(
                e,
                &args.first().cloned().unwrap_or(Val::Undefined),
            )?;
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
    Ok(Val::Obj(new_obj(
        ObjKind::Regex(ReInst {
            re,
            last_index: Cell::new(0.0),
        }),
        Vec::new(),
        None,
    )))
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
    Val::Obj(new_obj(ObjKind::Plain(None, false), Vec::new(), proto))
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

thread_local! {
    /// The global symbol registry behind `Symbol.for`: key -> symbol. One
    /// table per worker thread, cleared by `JsEngine::new` like the key
    /// intern table so a later page does not keep the previous page's symbols
    /// alive.
    static SYMBOL_REGISTRY: std::cell::RefCell<HashMap<String, Val>> =
        std::cell::RefCell::new(HashMap::new());
}

pub(crate) fn clear_symbol_registry() {
    SYMBOL_REGISTRY.with(|m| m.borrow_mut().clear());
}

/// The description of a symbol object (`None` for anything else).
fn symbol_desc(v: &Val) -> Option<String> {
    let Val::Obj(o) = v else { return None };
    let b = o.borrow();
    b.props
        .iter()
        .find(|(k, _)| &**k == "__symbol")
        .map(|(_, v)| v.to_string())
}

const SYM_FOR: u16 = 0;
const SYM_KEY_FOR: u16 = 1;

fn n_symbol_static(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        // Symbol.for(key): one symbol per key, shared by every caller
        SYM_FOR => {
            let key = arg_str(args, 0);
            Ok(SYMBOL_REGISTRY.with(|m| {
                let mut m = m.borrow_mut();
                if let Some(s) = m.get(&key) {
                    return s.clone();
                }
                let s = new_symbol(&key);
                m.insert(key, s.clone());
                s
            }))
        }
        // Symbol.keyFor(sym): the registry key, or undefined for a symbol
        // that Symbol.for did not make
        _ => {
            let Some(sym) = args.first() else {
                return Err(JsError::throw(
                    "TypeError: Symbol.keyFor called on non-symbol",
                ));
            };
            if symbol_desc(sym).is_none() {
                return Err(JsError::throw(
                    "TypeError: Symbol.keyFor called on non-symbol",
                ));
            }
            Ok(SYMBOL_REGISTRY.with(|m| {
                for (k, v) in m.borrow().iter() {
                    if let (Val::Obj(a), Val::Obj(b)) = (v, sym) {
                        if Rc::ptr_eq(a, b) {
                            return Val::Str(Rc::from(k.as_str()));
                        }
                    }
                }
                Val::Undefined
            }))
        }
    }
}

fn n_symbol_proto(_e: &mut JsEngine, idx: u16, this: &Val, _args: &[Val]) -> Result<Val, JsError> {
    let Some(desc) = symbol_desc(this) else {
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

// -- typed-array helpers ---------------------------------------------------

const T_SET: u16 = 0;
const T_SUBARRAY: u16 = 1;
const T_SLICE: u16 = 2;
const T_JOIN: u16 = 3;

fn new_arraybuffer(data: Vec<u8>) -> Val {
    Val::Obj(new_obj(
        ObjKind::ArrayBuffer(Rc::new(RefCell::new(data))),
        Vec::new(),
        None,
    ))
}

fn make_view(
    e: &JsEngine,
    kind: TypedKind,
    buf: Rc<RefCell<Obj>>,
    offset: usize,
    len: usize,
) -> Val {
    Val::Obj(new_obj(
        ObjKind::TypedArray(Box::new(TypedArray {
            kind,
            buf,
            offset,
            len,
        })),
        Vec::new(),
        e.typed_proto.clone(),
    ))
}

/// Allocate a fresh zero-filled buffer and wrap a view of `kind` over it.
/// Used by `new TypedArray(len)`: going through a `Vec<Val>` of length `len`
/// would cost ~24 bytes per element on top of the bytes themselves.
fn typed_zeroed(e: &JsEngine, kind: TypedKind, len: usize) -> Val {
    let buf = new_arraybuffer(vec![0u8; len * kind.size()]);
    let b = match &buf {
        Val::Obj(o) => o.clone(),
        _ => unreachable!(),
    };
    make_view(e, kind, b, 0, len)
}

/// Allocate a fresh buffer and fill a view of `kind` with `vals`.
fn typed_from_values(e: &JsEngine, kind: TypedKind, vals: &[Val]) -> Val {
    let buf = new_arraybuffer(vec![0u8; vals.len() * kind.size()]);
    let b = match &buf {
        Val::Obj(o) => o.clone(),
        _ => unreachable!(),
    };
    let v = make_view(e, kind, b, 0, vals.len());
    if let Val::Obj(o) = &v {
        if let ObjKind::TypedArray(ta) = &o.borrow().kind {
            for (i, val) in vals.iter().enumerate() {
                crate::js::typed_write(ta, i, val);
            }
        }
    }
    v
}

/// Byte copy of a typed array / Buffer (empty for other values).
fn typed_bytes_copy(v: &Val) -> Vec<u8> {
    if let Val::Obj(o) = v {
        let b = o.borrow();
        if let ObjKind::TypedArray(ta) = &b.kind {
            let r = ta.buf.borrow();
            if let ObjKind::ArrayBuffer(bytes) = &r.kind {
                let bytes = bytes.borrow();
                let n = ta.len * ta.kind.size();
                if ta.offset + n <= bytes.len() {
                    return bytes[ta.offset..ta.offset + n].to_vec();
                }
            }
        }
    }
    Vec::new()
}

fn kind_of(v: &Val) -> Option<TypedKind> {
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            ObjKind::TypedArray(ta) => Some(ta.kind),
            _ => None,
        },
        _ => None,
    }
}

fn n_typed_ctor(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let Some(kind) = TypedKind::from_index(idx) else {
        return Err(JsError::throw("TypeError: unknown typed-array kind"));
    };
    let Some(first) = args.first() else {
        return Ok(typed_from_values(e, kind, &[]));
    };
    match first {
        Val::Num(n) => {
            if !n.is_finite() {
                return Err(JsError::throw("RangeError: invalid typed array length"));
            }
            let len = (*n as i64).max(0) as usize;
            if len > (1 << 24) {
                return Err(JsError::throw("RangeError: typed array is too large"));
            }
            Ok(typed_zeroed(e, kind, len))
        }
        Val::Obj(o) => {
            let ab_len = {
                let ob = o.borrow();
                match &ob.kind {
                    ObjKind::ArrayBuffer(bytes) => Some(bytes.borrow().len()),
                    _ => None,
                }
            };
            if let Some(byte_len) = ab_len {
                let offset = args
                    .get(1)
                    .map(|v| v.to_number().max(0.0) as usize)
                    .unwrap_or(0);
                let len = args.get(2).map(|v| v.to_number().max(0.0) as usize);
                let max = byte_len.saturating_sub(offset) / kind.size();
                return Ok(make_view(
                    e,
                    kind,
                    o.clone(),
                    offset,
                    len.unwrap_or(max).min(max),
                ));
            }
            let src_kind = kind_of(first);
            if src_kind.is_some() {
                // copy element-wise (handles width/signedness conversion)
                let vals: Vec<Val> = {
                    let mut out = Vec::new();
                    if let Val::Obj(o) = first {
                        let b = o.borrow();
                        if let ObjKind::TypedArray(ta) = &b.kind {
                            for i in 0..ta.len {
                                out.push(crate::js::typed_read(ta, i));
                            }
                        }
                    }
                    out
                };
                return Ok(typed_from_values(e, kind, &vals));
            }
            let vals = super::interp::for_of_items(e, first)?;
            Ok(typed_from_values(e, kind, &vals))
        }
        Val::Str(_) => {
            let vals = super::interp::for_of_items(e, first)?;
            Ok(typed_from_values(e, kind, &vals))
        }
        _ => Ok(typed_from_values(e, kind, &[])),
    }
}

fn n_typed(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let (kind, src_len, src_offset) = match this {
        Val::Obj(o) => match &o.borrow().kind {
            ObjKind::TypedArray(ta) => (ta.kind, ta.len, ta.offset),
            _ => {
                return Err(JsError::throw(
                    "TypeError: typed-array method called on incompatible receiver",
                ))
            }
        },
        _ => {
            return Err(JsError::throw(
                "TypeError: typed-array method called on incompatible receiver",
            ))
        }
    };
    match idx {
        T_JOIN => {
            let mut parts = Vec::with_capacity(src_len);
            if let Val::Obj(o) = this {
                let b = o.borrow();
                if let ObjKind::TypedArray(ta) = &b.kind {
                    for i in 0..ta.len {
                        parts.push(crate::js::typed_read(ta, i).to_string());
                    }
                }
            }
            Ok(Val::Str(Rc::from(parts.join(",").as_str())))
        }
        T_SET => {
            let vals = args.first().cloned().unwrap_or(Val::Undefined);
            let items = super::interp::for_of_items(e, &vals)?;
            let offset = args
                .get(1)
                .map(|v| v.to_number().max(0.0) as usize)
                .unwrap_or(0);
            if let Val::Obj(o) = this {
                let b = o.borrow();
                if let ObjKind::TypedArray(ta) = &b.kind {
                    for (i, v) in items.iter().enumerate().take(src_len.saturating_sub(offset)) {
                        crate::js::typed_write(ta, offset + i, v);
                    }
                }
            }
            Ok(Val::Undefined)
        }
        T_SUBARRAY | T_SLICE => {
            let n = src_len as i64;
            let (a, b) = slice_bounds(args.first().map(|v| v.to_number()).unwrap_or(f64::NAN), args.get(1), n);
            let (a, b) = (a as usize, b as usize);
            let len = b.saturating_sub(a);
            if idx == T_SLICE {
                // slice copies into a fresh buffer
                let mut vals = Vec::with_capacity(len);
                if let Val::Obj(o) = this {
                    let ob = o.borrow();
                    if let ObjKind::TypedArray(ta) = &ob.kind {
                        for i in 0..len {
                            vals.push(crate::js::typed_read(ta, a + i));
                        }
                    }
                }
                Ok(typed_from_values(e, kind, &vals))
            } else {
                // subarray shares the same backing store
                let buf = match this {
                    Val::Obj(o) => match &o.borrow().kind {
                        ObjKind::TypedArray(ta) => Some(ta.buf.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                match buf {
                    Some(b) => Ok(make_view(
                        e,
                        kind,
                        b,
                        src_offset + a * kind.size(),
                        len,
                    )),
                    None => Err(JsError::throw("TypeError: typed array expected")),
                }
            }
        }
        _ => Err(JsError::throw("TypeError: bad typed-array method")),
    }
}

fn n_u8(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        0 => Ok(Val::Str(Rc::from(b64_encode(&typed_bytes_copy(this)).as_str()))),
        _ => {
            let bytes = b64_decode(&arg_str(args, 0));
            Ok(typed_from_values(
                e,
                TypedKind::U8,
                &bytes
                    .into_iter()
                    .map(|b| Val::Num(b as f64))
                    .collect::<Vec<_>>(),
            ))
        }
    }
}

fn n_buffer(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == 1 {
        // Buffer.from(str) -> Uint8Array bytes with a base64-aware toString
        let s = arg_str(args, 0);
        let vals: Vec<Val> = s.as_bytes().iter().map(|b| Val::Num(*b as f64)).collect();
        let v = typed_from_values(e, TypedKind::U8, &vals);
        if let Val::Obj(o) = &v {
            obj_set_own(o, "toString", Val::Native(e.buffer_to_string));
        }
        return Ok(v);
    }
    let out = if arg_str(args, 0) == "base64" {
        b64_encode(&typed_bytes_copy(this))
    } else {
        String::from_utf8_lossy(&typed_bytes_copy(this)).to_string()
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

fn n_ab(_e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        0 => {
            let n = match this {
                Val::Obj(o) => match &o.borrow().kind {
                    ObjKind::ArrayBuffer(bytes) => bytes.borrow().len(),
                    _ => 0,
                },
                _ => 0,
            };
            Ok(Val::Num(n as f64))
        }
        2 => {
            let is_view = match args.first() {
                Some(Val::Obj(o)) => {
                    let b = o.borrow();
                    matches!(b.kind, ObjKind::TypedArray(_))
                        || b.props.iter().any(|(k, _)| k.as_ref() == "__dv_len")
                }
                _ => false,
            };
            Ok(Val::Bool(is_view))
        }
        3 => {
            // slice(begin, end) copies the requested byte range
            let src = match this {
                Val::Obj(o) => match &o.borrow().kind {
                    ObjKind::ArrayBuffer(bytes) => bytes.borrow().clone(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            let n = src.len() as i64;
            let (a, b) = slice_bounds(args.first().map(|v| v.to_number()).unwrap_or(f64::NAN), args.get(1), n);
            Ok(new_arraybuffer(src[a..b].to_vec()))
        }
        _ => {
            let len = args.first().map(|v| v.to_number()).unwrap_or(0.0);
            if !len.is_finite() || len < 0.0 || len > (1 << 26) as f64 {
                return Err(JsError::throw("RangeError: invalid ArrayBuffer length"));
            }
            Ok(new_arraybuffer(vec![0u8; len as usize]))
        }
    }
}

// ---- DataView -------------------------------------------------------------
// Dispatch indices are shared by `register_native` calls in `install`.
const DV_CTOR: u16 = 0;
const DV_GET_I8: u16 = 1;
const DV_GET_U8: u16 = 2;
const DV_GET_I16: u16 = 3;
const DV_GET_U16: u16 = 4;
const DV_GET_I32: u16 = 5;
const DV_GET_U32: u16 = 6;
const DV_GET_F32: u16 = 7;
const DV_GET_F64: u16 = 8;
const DV_GET_BIGI64: u16 = 9;
const DV_GET_BIGU64: u16 = 10;
const DV_SET_I8: u16 = 11;
const DV_SET_U8: u16 = 12;
const DV_SET_I16: u16 = 13;
const DV_SET_U16: u16 = 14;
const DV_SET_I32: u16 = 15;
const DV_SET_U32: u16 = 16;
const DV_SET_F32: u16 = 17;
const DV_SET_F64: u16 = 18;
const DV_SET_BIGI64: u16 = 19;
const DV_SET_BIGU64: u16 = 20;
const DV_GET_BYTE_LENGTH: u16 = 21;
const DV_GET_BYTE_OFFSET: u16 = 22;
const DV_GET_BUFFER: u16 = 23;

/// The ArrayBuffer behind a DataView instance (`__dv_buf`).
fn dv_bytes(buf: &Val) -> Result<Rc<RefCell<Vec<u8>>>, JsError> {
    if let Val::Obj(o) = buf {
        let b = o.borrow();
        if let ObjKind::ArrayBuffer(bytes) = &b.kind {
            return Ok(bytes.clone());
        }
    }
    Err(JsError::throw(
        "TypeError: DataView buffer is not an ArrayBuffer",
    ))
}

/// Read `size` bytes at `at` as an unsigned integer (endianness applied).
fn dv_load(buf: &Val, at: usize, size: usize, little: bool) -> Result<u64, JsError> {
    let bytes = dv_bytes(buf)?;
    let b = bytes.borrow();
    let mut n = 0u64;
    for k in 0..size {
        n = if little {
            n | ((b[at + k] as u64) << (8 * k))
        } else {
            (n << 8) | b[at + k] as u64
        };
    }
    Ok(n)
}

/// Write the low `size` bytes of `n` at `at` (endianness applied).
fn dv_store(buf: &Val, at: usize, size: usize, n: u64, little: bool) -> Result<(), JsError> {
    let bytes = dv_bytes(buf)?;
    let mut b = bytes.borrow_mut();
    for k in 0..size {
        let shift = if little { 8 * k } else { 8 * (size - 1 - k) };
        b[at + k] = (n >> shift) as u8;
    }
    Ok(())
}

/// ToIndex for a DataView offset/length (NaN -> 0, negative -> RangeError).
fn dv_index(v: Option<&Val>, default: usize) -> Result<usize, JsError> {
    match v {
        None | Some(Val::Undefined) => Ok(default),
        Some(v) => {
            let n = v.to_number();
            if n.is_nan() {
                return Ok(0);
            }
            if !n.is_finite() || n < 0.0 {
                return Err(JsError::throw("RangeError: invalid DataView offset"));
            }
            Ok(n as usize)
        }
    }
}

/// Element width for a DataView setter index.
fn dv_size(idx: u16) -> usize {
    match idx {
        DV_GET_I8 | DV_GET_U8 | DV_SET_I8 | DV_SET_U8 => 1,
        DV_GET_I16 | DV_GET_U16 | DV_SET_I16 | DV_SET_U16 => 2,
        DV_GET_I32 | DV_GET_U32 | DV_GET_F32 | DV_SET_I32 | DV_SET_U32 | DV_SET_F32 => 4,
        _ => 8,
    }
}

fn n_data_view(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    if idx == DV_CTOR {
        let Some(buf) = args.first() else {
            return Err(JsError::throw(
                "TypeError: DataView: 1 argument required",
            ));
        };
        let blen = match buf {
            Val::Obj(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::ArrayBuffer(bytes) => bytes.borrow().len(),
                    _ => {
                        return Err(JsError::throw(
                            "TypeError: DataView: first argument is not an ArrayBuffer",
                        ))
                    }
                }
            }
            _ => {
                return Err(JsError::throw(
                    "TypeError: DataView: first argument is not an ArrayBuffer",
                ))
            }
        };
        let off = dv_index(args.get(1), 0)?;
        if off > blen {
            return Err(JsError::throw("RangeError: DataView offset is out of bounds"));
        }
        let len = match args.get(2) {
            Some(Val::Undefined) | None => blen - off,
            v => dv_index(v, 0)?,
        };
        if off.checked_add(len).map_or(true, |end| end > blen) {
            return Err(JsError::throw(
                "RangeError: DataView length is out of bounds",
            ));
        }
        let obj = proto_instance(e.data_view_proto.clone());
        obj.set("__dv_buf", buf.clone());
        obj.set("__dv_off", Val::Num(off as f64));
        obj.set("__dv_len", Val::Num(len as f64));
        return Ok(obj);
    }

    let (buf, off, len) = match this {
        Val::Obj(o) => (
            obj_get_own(o, "__dv_buf"),
            obj_get_own(o, "__dv_off").to_number() as usize,
            obj_get_own(o, "__dv_len").to_number() as usize,
        ),
        _ => return Err(JsError::throw("TypeError: DataView method on non-DataView")),
    };
    match idx {
        DV_GET_BYTE_LENGTH => return Ok(Val::Num(len as f64)),
        DV_GET_BYTE_OFFSET => return Ok(Val::Num(off as f64)),
        DV_GET_BUFFER => return Ok(buf),
        _ => {}
    }
    let size = dv_size(idx);
    let i = dv_index(args.first(), 0)?;
    if i.checked_add(size).map_or(true, |end| end > len) {
        return Err(JsError::throw("RangeError: DataView offset is out of bounds"));
    }
    let at = off + i;
    if idx >= DV_SET_I8 && idx <= DV_SET_BIGU64 {
        let v = args.get(1).cloned().unwrap_or(Val::Undefined);
        let raw = match idx {
            DV_SET_I8 | DV_SET_U8 => v.to_number() as i64 as u8 as u64,
            DV_SET_I16 | DV_SET_U16 => v.to_number() as i64 as u16 as u64,
            DV_SET_I32 | DV_SET_U32 => v.to_number() as i64 as u32 as u64,
            DV_SET_F32 => (v.to_number() as f32).to_bits() as u64,
            DV_SET_F64 => v.to_number().to_bits(),
            _ => match v {
                Val::BigInt(n) => {
                    if idx == DV_SET_BIGI64 {
                        *n as i64 as u64
                    } else {
                        *n as u64
                    }
                }
                _ => {
                    return Err(JsError::throw(
                        "TypeError: DataView BigInt setter requires a BigInt",
                    ))
                }
            },
        };
        let little = args.get(2).map(|v| v.to_bool()).unwrap_or(false);
        dv_store(&buf, at, size, raw, little)?;
        return Ok(Val::Undefined);
    }
    let little = args.get(1).map(|v| v.to_bool()).unwrap_or(false);
    let n = dv_load(&buf, at, size, little)?;
    Ok(match idx {
        DV_GET_I8 => Val::Num(n as u8 as i8 as f64),
        DV_GET_U8 => Val::Num(n as u8 as f64),
        DV_GET_I16 => Val::Num(n as u16 as i16 as f64),
        DV_GET_U16 => Val::Num(n as u16 as f64),
        DV_GET_I32 => Val::Num(n as u32 as i32 as f64),
        DV_GET_U32 => Val::Num(n as u32 as f64),
        DV_GET_F32 => Val::Num(f32::from_bits(n as u32) as f64),
        DV_GET_F64 => Val::Num(f64::from_bits(n)),
        DV_GET_BIGI64 => Val::BigInt(Rc::new(n as i64 as i128)),
        DV_GET_BIGU64 => Val::BigInt(Rc::new(n as i128)),
        _ => return Err(JsError::throw("TypeError: unknown DataView method")),
    })
}

// ---- AbortController / AbortSignal ----------------------------------------
const A_CTOR: u16 = 0;
const A_ABORT: u16 = 1;
const A_ADD: u16 = 2;
const A_REMOVE: u16 = 3;
const A_THROW: u16 = 4;
const A_GET_ABORTED: u16 = 5;
const A_GET_REASON: u16 = 6;
const A_GET_ON: u16 = 7;
const A_SET_ON: u16 = 8;
const A_STATIC_ABORT: u16 = 9;
const A_SIGNAL_CTOR: u16 = 10;

fn abort_error(e: &JsEngine) -> Val {
    Val::Obj(new_obj(
        ObjKind::Error(Rc::new(ErrorData {
            name: Rc::from("AbortError"),
            msg: Rc::from("This operation was aborted"),
        })),
        Vec::new(),
        e.error_proto.clone(),
    ))
}

fn make_signal(e: &JsEngine, aborted: bool, reason: Val) -> Val {
    let sig = proto_instance(e.abort_signal_proto.clone());
    sig.set("__abort_cbs", Val::new_array(Vec::new()));
    sig.set("__aborted", Val::Bool(aborted));
    sig.set("__reason", reason);
    sig
}

fn abort_event(sig: &Val) -> Val {
    let evt = Val::new_object();
    evt.set("type", Val::Str(Rc::from("abort")));
    evt.set("target", sig.clone());
    evt.set("currentTarget", sig.clone());
    evt.set("bubbles", Val::Bool(false));
    evt.set("cancelable", Val::Bool(false));
    evt.set("defaultPrevented", Val::Bool(false));
    evt
}

/// Mark `sig` aborted and run its listeners (FIFO). Callback exceptions are
/// reported like ordinary listener errors and never escape `abort()`.
fn abort_signal(e: &mut JsEngine, sig: &Val, reason: Val) {
    let Val::Obj(o) = sig else { return };
    if matches!(obj_get_own(o, "__aborted"), Val::Bool(true)) {
        return;
    }
    obj_set_own(o, "__aborted", Val::Bool(true));
    obj_set_own(o, "__reason", reason);
    let evt = abort_event(sig);
    let cbs = match obj_get_own(o, "__abort_cbs") {
        Val::Obj(a) => match &a.borrow().kind {
            ObjKind::Array(items) => items.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    for cb in cbs {
        if is_callable(&cb) {
            if let Err(err) = e.call(cb, sig.clone(), vec![evt.clone()]) {
                eprintln!("[js] abort listener error: {err}");
            }
        }
    }
    let on = obj_get_own(o, "__onabort");
    if is_callable(&on) {
        if let Err(err) = e.call(on, sig.clone(), vec![evt]) {
            eprintln!("[js] abort listener error: {err}");
        }
    }
}

fn n_abort(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        A_CTOR => {
            if !is_ctor_call(e, this) {
                return Err(JsError::throw(
                    "TypeError: AbortController is not a constructor function",
                ));
            }
            let sig = make_signal(e, false, Val::Undefined);
            let ctrl = proto_instance(e.abort_controller_proto.clone());
            ctrl.set("signal", sig);
            Ok(ctrl)
        }
        A_SIGNAL_CTOR => Err(JsError::throw("TypeError: Illegal constructor")),
        A_ABORT => {
            let Val::Obj(c) = this else {
                return Err(JsError::throw("TypeError: abort called on non-controller"));
            };
            let sig = obj_get_own(c, "signal");
            if !matches!(sig, Val::Obj(_)) {
                return Err(JsError::throw(
                    "TypeError: AbortController has no signal",
                ));
            }
            let reason = match args.first() {
                None | Some(Val::Undefined) => abort_error(e),
                Some(v) => v.clone(),
            };
            abort_signal(e, &sig, reason);
            Ok(Val::Undefined)
        }
        A_STATIC_ABORT => {
            let reason = match args.first() {
                None | Some(Val::Undefined) => abort_error(e),
                Some(v) => v.clone(),
            };
            Ok(make_signal(e, true, reason))
        }
        A_ADD => {
            let Val::Obj(o) = this else {
                return Err(JsError::throw("TypeError: signal method on non-signal"));
            };
            let kind = args.first().map(|v| v.to_string()).unwrap_or_default();
            let cb = args.get(1).cloned().unwrap_or(Val::Undefined);
            if kind == "abort" {
                if matches!(obj_get_own(o, "__aborted"), Val::Bool(true)) {
                    // shortcut: an already-aborted signal calls the listener
                    // synchronously, not from a queued microtask
                    if is_callable(&cb) {
                        e.call(cb, this.clone(), vec![abort_event(this)])?;
                    }
                } else if is_callable(&cb) {
                    let list = obj_get_own(o, "__abort_cbs");
                    if let Val::Obj(a) = list {
                        if let ObjKind::Array(items) = &mut a.borrow_mut().kind {
                            if !items.iter().any(|it| strict_eq(it, &cb)) {
                                items.push(cb);
                            }
                        }
                    }
                }
            }
            Ok(Val::Undefined)
        }
        A_REMOVE => {
            let Val::Obj(o) = this else {
                return Err(JsError::throw("TypeError: signal method on non-signal"));
            };
            let cb = args.get(1).cloned().unwrap_or(Val::Undefined);
            let list = obj_get_own(o, "__abort_cbs");
            if let Val::Obj(a) = list {
                if let ObjKind::Array(items) = &mut a.borrow_mut().kind {
                    items.retain(|it| !strict_eq(it, &cb));
                }
            }
            Ok(Val::Undefined)
        }
        A_THROW => {
            let Val::Obj(o) = this else {
                return Err(JsError::throw("TypeError: signal method on non-signal"));
            };
            if matches!(obj_get_own(o, "__aborted"), Val::Bool(true)) {
                return Err(JsError::Throw(obj_get_own(o, "__reason")));
            }
            Ok(Val::Undefined)
        }
        A_GET_ABORTED => match this {
            Val::Obj(o) => Ok(obj_get_own(o, "__aborted")),
            _ => Ok(Val::Bool(false)),
        },
        A_GET_REASON => match this {
            Val::Obj(o) => Ok(obj_get_own(o, "__reason")),
            _ => Ok(Val::Undefined),
        },
        A_GET_ON => match this {
            Val::Obj(o) => Ok(obj_get_own(o, "__onabort")),
            _ => Ok(Val::Undefined),
        },
        A_SET_ON => {
            if let Val::Obj(o) = this {
                obj_set_own(o, "__onabort", args.first().cloned().unwrap_or(Val::Undefined));
            }
            Ok(Val::Undefined)
        }
        _ => Err(JsError::throw("TypeError: unknown AbortSignal method")),
    }
}

fn n_bigint(e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {    let arg = args.first().cloned().unwrap_or(Val::Undefined);
    match idx {
        1 | 2 => {
            let bits = arg.to_number().max(0.0) as u64;
            let n = match args.get(1) {
                Some(v) => bigint_value(v)?,
                None => {
                    return Err(JsError::throw(
                        "TypeError: BigInt.asIntN requires a BigInt argument",
                    ))
                }
            };
            let bits = bits.min(128);
            let out = if bits == 0 {
                0i128
            } else if bits >= 128 {
                n
            } else {
                let mask = (1u128 << bits) - 1;
                let low = (n as u128) & mask;
                if idx == 1 && low >= (1u128 << (bits - 1)) {
                    (low as i128) - (1i128 << bits)
                } else {
                    low as i128
                }
            };
            Ok(Val::BigInt(Rc::new(out)))
        }
        _ => {
            if is_ctor_call(e, this) {
                return Err(JsError::throw("TypeError: BigInt is not a constructor"));
            }
            Ok(Val::BigInt(Rc::new(bigint_value(&arg)?)))
        }
    }
}

/// ToBigInt for the constructor / asIntN / asUintN arguments.
fn bigint_value(v: &Val) -> Result<i128, JsError> {
    match v {
        Val::BigInt(n) => Ok(**n),
        Val::Num(n) => {
            if n.is_finite() && n.fract() == 0.0 {
                if *n < i128::MIN as f64 || *n > i128::MAX as f64 {
                    return Err(JsError::throw(
                        "RangeError: BigInt value is out of the supported i128 range",
                    ));
                }
                Ok(*n as i128)
            } else {
                Err(JsError::throw(
                    "RangeError: The number cannot be converted to a BigInt because it is not an integer",
                ))
            }
        }
        Val::Str(s) => {
            let t = s.trim();
            let parsed = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                i128::from_str_radix(h, 16)
            } else if let Some(o) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
                i128::from_str_radix(o, 8)
            } else if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
                i128::from_str_radix(b, 2)
            } else {
                t.parse::<i128>()
            };
            parsed.map_err(|_| {
                JsError::throw(&format!("SyntaxError: Cannot convert {} to a BigInt", t))
            })
        }
        Val::Bool(b) => Ok(if *b { 1 } else { 0 }),
        _ => Err(JsError::throw(
            "TypeError: Cannot convert undefined or null to a BigInt",
        )),
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
            // decode(bytes) -> string. Real pages pass a *view*, not a plain
            // array: @bufbuild/protobuf decodes every length-delimited field
            // with `decode(new Uint8Array(...))`, so GitHub's descriptor
            // builder read every string (field names, type names) as "".
            let v = args.first().cloned().unwrap_or(Val::Undefined);
            let bytes = match &v {
                Val::Str(s) => return Ok(Val::Str(s.clone())),
                Val::Obj(o) => text_bytes(&o.borrow()),
                _ => Vec::new(),
            };
            Ok(Val::Str(Rc::from(String::from_utf8_lossy(&bytes).as_ref())))
        }
    }
}

/// The bytes a `TextDecoder` sees for `v`: a view's byte window
/// (`offset..offset + len * size`, so `subarray` decodes just its slice), a
/// whole `ArrayBuffer`, or a plain array of byte values.
fn text_bytes(o: &Obj) -> Vec<u8> {
    match &o.kind {
        ObjKind::Array(items) => items
            .iter()
            .map(|x| x.to_number().clamp(0.0, 255.0) as u8)
            .collect(),
        ObjKind::TypedArray(ta) => {
            let at = ta.offset;
            let end = at + ta.len * ta.kind.size();
            match &ta.buf.borrow().kind {
                ObjKind::ArrayBuffer(b) => {
                    let b = b.borrow();
                    b.get(at..end.min(b.len())).map(|s| s.to_vec()).unwrap_or_default()
                }
                _ => Vec::new(),
            }
        }
        ObjKind::ArrayBuffer(b) => b.borrow().clone(),
        _ => Vec::new(),
    }
}

fn n_weakmap_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let obj = proto_instance(e.weakmap_proto.clone());
    let pairs = map_entries_from(e, args.first())?;
    map_store(&obj, pairs);
    Ok(obj)
}

fn n_weakset_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let obj = proto_instance(e.weakset_proto.clone());
    let mut items: Vec<Val> = Vec::new();
    if let Some(init) = args.first() {
        for v in super::interp::for_of_items(e, init)? {
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
        for v in super::interp::for_of_items(e, init)? {
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
        5 | 6 => Ok(super::interp::make_iterator(this.clone(), crate::js::IterKind::Values)),
        7 => Ok(super::interp::make_iterator(this.clone(), crate::js::IterKind::Entries)),
        _ => Err(JsError::throw("TypeError: bad Set native")),
    }
}

fn n_map_ctor(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let obj = proto_instance(e.map_proto.clone());
    let pairs = map_entries_from(e, args.first())?;
    map_store(&obj, pairs);
    Ok(obj)
}

/// Entries from a Map/WeakMap iterable argument (`[[k, v], ...]`).
fn map_entries_from(e: &mut JsEngine, init: Option<&Val>) -> Result<Vec<(Val, Val)>, JsError> {
    let mut pairs: Vec<(Val, Val)> = Vec::new();
    if let Some(init) = init {
        for entry in super::interp::for_of_items(e, init)? {
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
        6 => Ok(super::interp::make_iterator(this.clone(), crate::js::IterKind::Keys)),
        7 => Ok(super::interp::make_iterator(this.clone(), crate::js::IterKind::Values)),
        8 => Ok(super::interp::make_iterator(this.clone(), crate::js::IterKind::Entries)),
        _ => Err(JsError::throw("TypeError: bad Map native")),
    }
}
