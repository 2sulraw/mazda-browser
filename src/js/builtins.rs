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
    e.set_global("Object", object);

    // Object.prototype + Array.prototype chain
    let obj_proto = Val::new_object();
    let ts = e.register_native("toString", n_objproto, 0);
    let ho = e.register_native("hasOwnProperty", n_objproto, 1);
    obj_set_own(own(&obj_proto), "toString", Val::Native(ts));
    obj_set_own(own(&obj_proto), "hasOwnProperty", Val::Native(ho));

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
    obj_set_own(own(&array_ctor), "prototype", arr_proto);
    e.set_global("Array", array_ctor);

    // -- Error family ------------------------------------------------------
    for (i, name) in ["Error", "TypeError", "RangeError", "SyntaxError"]
        .iter()
        .enumerate()
    {
        let id = e.register_native(name, n_error_ctor, i as u16);
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

    // -- timers (queue-only stub; embedder drains via drain_timers) --------
    let st = e.register_native("setTimeout", n_set_timeout, 0);
    e.set_global("setTimeout", Val::Native(st));

    // -- Promise (partial: resolve/reject/.then + manual drain) ------------
    let promise = Val::new_object();
    let pr = e.register_native("Promise.resolve", n_promise, 0);
    let pj = e.register_native("Promise.reject", n_promise, 1);
    obj_set_own(own(&promise), "resolve", Val::Native(pr));
    obj_set_own(own(&promise), "reject", Val::Native(pj));
    e.set_global("Promise", promise);

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
    let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect();
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
        Val::Undefined | Val::Func(_) | Val::Native(_) | Val::Method(_) => return Ok(false),
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

fn n_object(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
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
                    for (k, v) in pairs {
                        obj_set_own(own(&target), &k, v);
                    }
                }
            }
            Ok(target)
        }
        _ => Ok(Val::Undefined),
    }
}

const OP_TOSTRING: u16 = 0;
const OP_HASOWN: u16 = 1;

fn n_objproto(_e: &mut JsEngine, idx: u16, this: &Val, args: &[Val]) -> Result<Val, JsError> {
    match idx {
        OP_TOSTRING => Ok(Val::Str(Rc::from(match this {
            Val::Obj(o) => match &o.borrow().kind {
                ObjKind::Plain => "[object Object]",
                ObjKind::Array(_) => "[object Array]",
                ObjKind::Error { .. } => "[object Error]",
                ObjKind::Date(_) => "[object Date]",
                ObjKind::Promise(_) => "[object Promise]",
            },
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
                _ => false,
            }))
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
const A_ISARRAY: u16 = 100;

fn need_array(this: &Val) -> Result<Rc<RefCell<Obj>>, JsError> {
    match this {
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
        A_INDEXOF | A_INCLUDES => {
            let needle = args.first().cloned().unwrap_or(Val::Undefined);
            let from = args.get(1).map_or(0, |v| {
                let n = v.to_number();
                if n.is_nan() || n < 0.0 {
                    0
                } else {
                    n as usize
                }
            });
            let items = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items.clone(),
                    _ => Vec::new(),
                }
            };
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

fn n_error_ctor(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let name: Rc<str> = Rc::from(match idx {
        E_TYPE => "TypeError",
        E_RANGE => "RangeError",
        E_SYNTAX => "SyntaxError",
        _ => "Error",
    });
    let msg = Rc::from(args.first().map_or("".to_string(), |v| v.to_string()).as_str());
    Ok(Val::Obj(Rc::new(RefCell::new(Obj {
        kind: ObjKind::Error { name, msg },
        props: Vec::new(),
        proto: None,
    }))))
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

fn n_set_timeout(e: &mut JsEngine, _idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
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
        delay_ms: ms,
        callback: cb,
        args: args.iter().skip(2).cloned().collect(),
    });
    Ok(Val::Num(id))
}

// -- Promise (partial) ------------------------------------------------------

fn n_promise(_e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let v = args.first().cloned().unwrap_or(Val::Undefined);
    let state = PromiseState {
        settled: Some(if idx == 0 { Ok(v) } else { Err(v) }),
        handlers: Vec::new(),
    };
    Ok(Val::Obj(Rc::new(RefCell::new(Obj {
        kind: ObjKind::Promise(state),
        props: Vec::new(),
        proto: None,
    }))))
}
