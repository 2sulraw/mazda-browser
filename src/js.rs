//! JavaScript interpreter core â€” a from-scratch tree-walking evaluator for a
//! useful ES5+ subset. This is the foundation wave: pure logic, no DOM, no
//! event loop; the embedder (later waves) drives script execution and drains
//! the timer queue itself.
//!
//! # Architecture
//! - `lexer` -> tokens with byte spans; `parser` -> Box-based AST; `interp`
//!   -> tree-walking evaluator; `builtins` -> globals/prototypes (console,
//!   Math, JSON, Object, Array, Date, ...).
//! - Values (`Val`) are a compact 24-byte enum: primitives inline, heap
//!   payloads behind `Rc` (strings are fat `Rc<str>` pointers, saving one
//!   indirection on the hot path). Objects are `Rc<RefCell<Obj>>` with an
//!   insertion-ordered property `Vec` (linear scan â€” fine at this stage;
//!   shortcut comment) plus an optional dense array exotic.
//! - Scopes: an `Rc<Env>` chain; `let/const` get a child env per block,
//!   `var`/function decls hoist to the function env (naive walk at call
//!   time). RAM tradeoff: one small `Env` (~40B + binding vec) per call/block; that
//!   buys trivially-correct closures.
//! - Errors: `JsError` propagates via `Result`; script `throw` is
//!   `JsError::Throw(Val)`. The interpreter never panics on bad script
//!   input; call depth is capped (1000) so deep recursion throws a
//!   RangeError-style error instead of blowing the native stack.
//! - Lifetime/GC: there is NO garbage collector. Values held by the engine
//!   (globals, timer queue) live until the engine is dropped; values from
//!   one `exec()` that nothing references are freed by `Rc` refcounting.
//!   Closures that capture each other in cycles (e.g. an object method
//!   capturing its object) can leak until engine drop. That is acceptable
//!   for short-lived page scripts: navigating drops the whole engine.
//!
//! # Deferred (later waves), each with an honest syntax/runtime error rather
//! than silent misbehavior where possible: `class ... extends`, static
//! members, computed keys, labeled statements, `**`, BigInt, template-tagged
//! templates, lookbehind/named groups/`u`-flag regex, full ASI,
//! per-iteration `let` bindings in loops, iterator protocol (for-of/spread
//! only handle arrays/strings), boxed primitives, Date string parsing.
//! Async/await runs to completion (see `await_promise` â€” an `await` of a
//! promise settled only by a timer/macrotask errors honestly instead of
//! suspending the interpreter stack).
#![allow(dead_code)] // the embedder hasn't wired this module up yet

mod builtins;
mod gen;
mod interp;
mod lexer;
pub(crate) mod module;
mod parser;
mod regex;
pub(crate) mod url;

pub use module::ModuleLoader;

#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gen::GenVm;
use parser::{FnBody, FnDef, Program};
use regex::Regex;

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A JavaScript value. Kept to 24 bytes on 64-bit (tag/padding + the 16-byte
/// fat `Rc<str>` pointer; thinning strings would cost an extra indirection
/// per string): this enum is THE hot allocation unit, so heap payloads stay
/// behind `Rc`.
#[derive(Debug, Clone)]
pub enum Val {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(Rc<str>),
    Obj(Rc<RefCell<Obj>>),
    Func(Rc<Closure>),
    Native(usize),
    /// Built-in method reachable through property access
    /// (`"a".toUpperCase`, `(1.5).toFixed`, `fn.call`, ...). Calling one
    /// dispatches on `this`; extracting it and calling it unbound throws a
    /// TypeError (like real JS does for null/undefined `this`).
    Method(Meth),
    /// accessor pair stored as a property value (`get x(){}`, or installed
    /// via `Object.defineProperty`); reading the property invokes the
    /// getter, writing invokes the setter (both optional)
    Acc(Rc<AccEntry>),
}

/// getter/setter pair behind `Val::Acc`. Handlers are callable `Val`s
/// (usually script closures); an absent getter reads `undefined`, an absent
/// setter makes the property read-only through `set_member`.
#[derive(Debug)]
pub struct AccEntry {
    pub getter: Option<Val>,
    pub setter: Option<Val>,
}

/// Built-in method identifiers (no per-call allocation; see `Val::Method`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Meth {
    // string methods (this must be Str)
    StrToUpper,
    StrToLower,
    StrTrim,
    StrTrimStart,
    StrTrimEnd,
    StrSlice,
    StrIndexOf,
    StrLastIndexOf,
    StrIncludes,
    StrSplit,
    StrReplace,
    StrStartsWith,
    StrEndsWith,
    StrRepeat,
    StrPadStart,
    StrPadEnd,
    StrCharAt,
    StrCharCodeAt,
    StrToString,
    // regex-aware string additions
    StrMatch,
    StrSearch,
    StrMatchAll,
    // number methods (this must be Num)
    NumToString,
    NumToFixed,
    // date methods (this must be a Date object)
    DateGetTime,
    DateToISO,
    DateNow,
    // promise methods
    PromiseThen,
    PromiseCatch,
    PromiseFinally,
    // generator methods (this must be a generator object)
    GenNext,
    GenReturn,
    // regex methods (this must be a regex object)
    RegexTest,
    RegexExec,
    RegexToString,
    // function methods
    FnCall,
    FnApply,
    FnBind,
    FnToString,
}

#[derive(Debug)]
pub struct Obj {
    pub kind: ObjKind,
    /// Own properties in insertion order. shortcut: linear scan on lookup â€”
    /// O(n) per access; fine for typical script objects (tens of props), to
    /// be revisited with a real shape/map if profiles ever demand it.
    pub props: Vec<(Rc<str>, Val)>,
    pub proto: Option<Rc<RefCell<Obj>>>,
}

#[derive(Debug)]
pub enum ObjKind {
    Plain,
    /// array exotic: dense element storage; `length` and integer keys map
    /// onto it, everything else goes through `props`.
    Array(Vec<Val>),
    Error {
        name: Rc<str>,
        msg: Rc<str>,
    },
    /// milliseconds since epoch (NaN = invalid date)
    Date(f64),
    Promise(PromiseState),
    /// regex instance: one `Rc<Regex>` per (pattern, flags) is shared via the
    /// compile cache; only `lastIndex` (for `g`/`y`) is per-instance, stored
    /// as a Cell so exec can update it without cloning the compiled program.
    Regex(ReInst),
    /// live generator instance; `.next`/`.return` resume the mini-VM
    /// (`gen.rs`) until the next `yield`.
    Generator(Rc<RefCell<GenVm>>),
    /// host-backed `element.style` (a CSSStyleDeclaration stand-in): holds
    /// the arena node id; reads/writes forward through `JsEngine::style_get`
    /// / `style_set` (installed by the embedder, so this module stays
    /// DOM-free). Own props still hold the method natives and `__node`.
    Style(u32),
}

/// per-instance regex state (see `ObjKind::Regex`)
pub struct ReInst {
    pub re: Rc<Regex>,
    /// where the next `exec`/`test` scan starts from when `g`/`y` is set
    pub last_index: Cell<f64>,
}

impl std::fmt::Debug for ReInst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Regex(/{}/{})", self.re.src, self.re.flags)
    }
}

#[derive(Debug, Default)]
pub struct PromiseState {
    /// None = pending
    pub settled: Option<Result<Val, Val>>,
    /// reactions queued while pending (promise chaining); each fires as a
    /// FIFO `Job::Reaction` on settle
    pub reactions: Vec<Reaction>,
}

/// One queued reaction: what to do when the promise settles, and which
/// promise receives the reaction's own output (`.then`'s returned promise).
#[derive(Debug)]
pub(crate) struct Reaction {
    pub kind: ReactKind,
    /// the promise whose settlement carries this reaction's output
    /// (`.then`'s returned promise); `None` for pass-through deliveries
    pub ret: Option<Rc<RefCell<Obj>>>,
}

#[derive(Debug)]
pub(crate) enum ReactKind {
    /// `.then(onOk, onErr)` / `.catch(onErr)`: `Val::Undefined` = handler
    /// absent (pass-through semantics)
    Js {
        on_ok: Val,
        on_err: Val,
    },
    /// `.finally(cb)`: runs `cb()` with no args; its result is discarded but
    /// a throw from it rejects the derived promise
    Finally {
        cb: Val,
    },
    /// resolve-of-thenable: adopt the target promise's outcome into the
    /// derived promise (chaining `return promiseLike`)
    Identity,
    /// connects an element promise into a combinator board
    All {
        slot: usize,
        state: Rc<RefCell<ComboState>>,
    },
    Any {
        slot: usize,
        state: Rc<RefCell<ComboState>>,
    },
    Race {
        state: Rc<RefCell<ComboState>>,
    },
    AllSettled {
        slot: usize,
        state: Rc<RefCell<ComboState>>,
    },
}

/// A script closure. One `Rc<Closure>` per function *value*; calls allocate
/// one `Env` for the frame. `props` carries `.prototype` and user-assigned
/// function properties (`f.foo = 1`).
pub struct Closure {
    pub def: Rc<FnDef>,
    pub env: Rc<Env>,
    /// lexical `this`, captured by arrow functions at creation
    pub this: Val,
    pub is_arrow: bool,
    pub props: RefCell<Vec<(Rc<str>, Val)>>,
    /// [[Prototype]] of the function object itself (class `extends` sets it
    /// so static members inherit); objects use `Obj::proto`.
    pub proto: RefCell<Option<Val>>,
}

impl std::fmt::Debug for Closure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Closure({})", self.def.name)
    }
}

impl Val {
    pub fn is_undefined(&self) -> bool {
        matches!(self, Val::Undefined)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Val::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// Exact string value (no coercion).
    pub fn as_string(&self) -> Option<Rc<str>> {
        match self {
            Val::Str(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// `typeof` result string.
    pub fn type_of(&self) -> &'static str {
        match self {
            Val::Undefined => "undefined",
            Val::Null => "object",
            Val::Bool(_) => "boolean",
            Val::Num(_) => "number",
            Val::Str(_) => "string",
            Val::Obj(o) => {
                // symbol stand-ins: unique objects with a hidden `__symbol`
                // description (see builtins::install)
                if o.borrow().props.iter().any(|(k, _)| &**k == "__symbol") {
                    "symbol"
                } else {
                    "object"
                }
            }
            Val::Func(_) | Val::Native(_) | Val::Method(_) | Val::Acc(_) => "function",
        }
    }

    pub fn new_object() -> Val {
        Val::Obj(Rc::new(RefCell::new(Obj {
            kind: ObjKind::Plain,
            props: Vec::new(),
            proto: None,
        })))
    }

    pub fn new_array(items: Vec<Val>) -> Val {
        Val::Obj(Rc::new(RefCell::new(Obj {
            kind: ObjKind::Array(items),
            props: Vec::new(),
            proto: None,
        })))
    }

    /// Engine-free own-property read on object values (embedder helper; no
    /// prototype chain, no string-method dispatch).
    pub fn get(&self, key: &str) -> Val {
        match self {
            Val::Obj(o) => obj_get_own(o, key),
            _ => Val::Undefined,
        }
    }

    /// Engine-free own-property write on object values (embedder helper).
    pub fn set(&self, key: &str, v: Val) {
        if let Val::Obj(o) = self {
            obj_set_own(o, key, v);
        }
    }

    /// JS `ToBoolean`.
    pub fn to_bool(&self) -> bool {
        match self {
            Val::Undefined | Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0 && !n.is_nan(),
            Val::Str(s) => !s.is_empty(),
            Val::Obj(_) | Val::Func(_) | Val::Native(_) | Val::Method(_) | Val::Acc(_) => true,
        }
    }

    /// JS `ToNumber` (no engine needed).
    pub fn to_number(&self) -> f64 {
        match self {
            Val::Undefined => f64::NAN,
            Val::Null => 0.0,
            Val::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Val::Num(n) => *n,
            Val::Str(s) => str_to_number(s),
            Val::Obj(_) => str_to_number(&self.to_string()),
            _ => f64::NAN,
        }
    }

    /// JS `ToString`. Primitives stringify directly; an object or function
    /// coerces to its `Object.prototype.toString` tag, and an array joins its
    /// elements (what `Array.prototype.toString` does). Property *values* are
    /// never walked, so coercing a huge or cyclic value (the global object, a
    /// DOM node, `window`) stays O(1) - walking made `window == null` allocate
    /// gigabytes (exponential fan-out over the object graph).
    /// shortcut: no user `toString`/`valueOf` override is consulted; the
    /// exponential number formatting for very large/small values differs from
    /// JS's (`1e+21`), while every-day values match.
    pub fn to_string(&self) -> String {
        self.coerce(0)
    }

    fn coerce(&self, depth: u32) -> String {
        if depth > 32 {
            // cycle guard (e.g. a self-referencing array)
            return "[Object]".into();
        }
        match self {
            Val::Obj(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items
                        .iter()
                        .map(|v| match v {
                            Val::Undefined | Val::Null => String::new(),
                            v => v.coerce(depth + 1),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                    ObjKind::Error { name, msg } => format!("{}: {}", name, msg),
                    ObjKind::Date(ms) => format!("Date({}ms)", ms),
                    ObjKind::Regex(ri) => format!("/{}/{}", ri.re.src, ri.re.flags),
                    ObjKind::Promise(_) => "[object Promise]".into(),
                    ObjKind::Generator(_) => "[object Generator]".into(),
                    ObjKind::Style(_) => "[object CSSStyleDeclaration]".into(),
                    ObjKind::Plain => {
                        // symbols are unique objects with a hidden
                        // `__symbol` description (see builtins)
                        match b.props.iter().find(|(k, _)| &**k == "__symbol") {
                            Some((_, Val::Str(desc))) => format!("Symbol({})", desc),
                            _ => "[object Object]".into(),
                        }
                    }
                }
            }
            _ => self.to_string_d(0),
        }
    }

    /// Inspector rendering for `console.log`: objects are dumped with their
    /// properties (bounded depth), unlike the spec-shaped [`Val::to_string`].
    pub fn inspect(&self) -> String {
        self.to_string_d(0)
    }

    fn to_string_d(&self, depth: u32) -> String {
        if depth > 32 {
            // cycle guard (e.g. self-referencing arrays)
            return "[Object]".into();
        }
        match self {
            Val::Undefined => "undefined".into(),
            Val::Null => "null".into(),
            Val::Bool(b) => b.to_string(),
            Val::Num(n) => num_to_str(*n),
            Val::Str(s) => s.to_string(),
            Val::Obj(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => items
                        .iter()
                        .map(|v| match v {
                            Val::Undefined | Val::Null => String::new(),
                            v => v.to_string_d(depth + 1),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                    ObjKind::Error { name, msg } => format!("{}: {}", name, msg),
                    ObjKind::Date(ms) => format!("Date({}ms)", ms),
                    ObjKind::Promise(st) => match &st.settled {
                        None => "Promise { <pending> }".into(),
                        Some(Ok(_)) => "Promise { <fulfilled> }".into(),
                        Some(Err(_)) => "Promise { <rejected> }".into(),
                    },
                    ObjKind::Plain => {
                        // symbols are unique objects with a hidden
                        // `__symbol` description (see builtins)
                        if let Some((_, Val::Str(desc))) =
                            b.props.iter().find(|(k, _)| &**k == "__symbol")
                        {
                            format!("Symbol({})", desc)
                        } else if b.props.is_empty() {
                            "[object Object]".into()
                        } else {
                            let parts: Vec<String> = b
                                .props
                                .iter()
                                .map(|(k, v)| format!("{}: {}", k, v.to_string_d(depth + 1)))
                                .collect();
                            format!("{{ {} }}", parts.join(", "))
                        }
                    }
                    ObjKind::Regex(ri) => format!("/{}/{}", ri.re.src, ri.re.flags),
                    ObjKind::Generator(_) => "[object Generator]".into(),
                    ObjKind::Style(_) => "[object CSSStyleDeclaration]".into(),
                }
            }
            Val::Func(c) => format!("function {}() {{ [code] }}", c.def.name),
            Val::Native(_) | Val::Method(_) | Val::Acc(_) => {
                "function () { [native code] }".into()
            }
        }
    }
}

impl From<f64> for Val {
    fn from(v: f64) -> Val {
        Val::Num(v)
    }
}
impl From<i32> for Val {
    fn from(v: i32) -> Val {
        Val::Num(v as f64)
    }
}
impl From<usize> for Val {
    fn from(v: usize) -> Val {
        Val::Num(v as f64)
    }
}
impl From<bool> for Val {
    fn from(v: bool) -> Val {
        Val::Bool(v)
    }
}
impl From<&str> for Val {
    fn from(v: &str) -> Val {
        Val::Str(Rc::from(v))
    }
}
impl From<String> for Val {
    fn from(v: String) -> Val {
        Val::Str(Rc::from(v.as_str()))
    }
}
impl From<Rc<str>> for Val {
    fn from(v: Rc<str>) -> Val {
        Val::Str(v)
    }
}

/// number -> JS-formatted string
fn num_to_str(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n == f64::INFINITY {
        return "Infinity".into();
    }
    if n == f64::NEG_INFINITY {
        return "-Infinity".into();
    }
    if n == 0.0 {
        return "0".into(); // also normalizes -0
    }
    if n.fract() == 0.0 && n.abs() < 1e21 {
        return format!("{}", n as i64);
    }
    format!("{}", n)
}

/// string -> number with JS rules (greedy numeric prefix, hex/binary/octal)
fn str_to_number(s: &str) -> f64 {
    let s = s.trim();
    if s.is_empty() {
        return 0.0;
    }
    if s == "Infinity" || s == "+Infinity" {
        return f64::INFINITY;
    }
    if s == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    let (sign, rest) = if let Some(r) = s.strip_prefix('-') {
        (-1.0, r)
    } else if let Some(r) = s.strip_prefix('+') {
        (1.0, r)
    } else {
        (1.0, s)
    };
    if let Some(h) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        let digits: String = h.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        return match u64::from_str_radix(&digits, 16) {
            Ok(v) => sign * v as f64,
            Err(_) => f64::NAN,
        };
    }
    if let Some(h) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        let digits: String = h.chars().take_while(|c| *c == '0' || *c == '1').collect();
        return match u64::from_str_radix(&digits, 2) {
            Ok(v) => sign * v as f64,
            Err(_) => f64::NAN,
        };
    }
    if let Some(h) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
        let digits: String = h.chars().take_while(|c| ('0'..='7').contains(c)).collect();
        return match u64::from_str_radix(&digits, 8) {
            Ok(v) => sign * v as f64,
            Err(_) => f64::NAN,
        };
    }
    // greedy scan of the leading numeric prefix: [digits][.][digits][e[+-]digits]
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    if i == 0 {
        return f64::NAN;
    }
    match rest[..i].parse::<f64>() {
        Ok(v) => sign * v,
        Err(_) => f64::NAN,
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum JsError {
    /// script-level exception (`throw`, TypeError from builtins, stack cap)
    Throw(Val),
    Syntax {
        msg: String,
        line: u32,
        col: u32,
    },
    /// per-turn interpreter step budget exhausted (runaway loop — the
    /// bounded-resources rule; a page script must never peg the worker).
    /// `line`/`col` are (0, 0) when a dispatch turn (event handler/timer)
    /// trips the budget — only `exec` has the source to resolve them
    Budget { line: u32, col: u32 },
}

impl std::fmt::Display for JsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsError::Throw(v) => write!(f, "Uncaught {}", v.to_string()),
            JsError::Syntax { msg, line, col } => {
                write!(f, "SyntaxError: {} (line {}, col {})", msg, line, col)
            }
            JsError::Budget { line, col } if *line > 0 => {
                write!(
                    f,
                    "RangeError: script exceeded the step budget (runaway loop) at line {}, col {}",
                    line, col
                )
            }
            JsError::Budget { .. } => {
                write!(f, "RangeError: script exceeded the step budget (runaway loop)")
            }
        }
    }
}

impl std::error::Error for JsError {}

impl JsError {
    pub(crate) fn throw(msg: &str) -> JsError {
        JsError::Throw(Val::Str(Rc::from(msg)))
    }
}

// ---------------------------------------------------------------------------
// Environments
// ---------------------------------------------------------------------------

/// One lexical scope. `Rc<Env>` chain: parent pointers are shared, blocks
/// allocate one small env each (~40B + binding vec). shortcut: bindings are
/// a Vec scanned linearly; `is_const` rides along per binding.
pub struct Env {
    parent: Option<Rc<Env>>,
    bindings: RefCell<Vec<Binding>>,
}

struct Binding {
    name: Rc<str>,
    val: Val,
    is_const: bool,
}

impl Env {
    pub fn root() -> Rc<Env> {
        Rc::new(Env {
            parent: None,
            bindings: RefCell::new(Vec::new()),
        })
    }

    pub fn child(parent: &Rc<Env>) -> Rc<Env> {
        Rc::new(Env {
            parent: Some(parent.clone()),
            bindings: RefCell::new(Vec::new()),
        })
    }

    fn declare(&self, name: Rc<str>, v: Val, is_const: bool) {
        self.bindings.borrow_mut().push(Binding { name, val: v, is_const });
    }

    /// declare only if this exact env doesn't already have the name
    fn declare_if_absent(&self, name: Rc<str>, v: Val, is_const: bool) {
        if self.bindings.borrow().iter().any(|b| b.name == name) {
            return;
        }
        self.declare(name, v, is_const);
    }

    /// generator re-entry binding: the generator VM shares one env across
    /// loop iterations, so a re-run `let x = yield`/`let x = ...` must
    /// REPLACE the prior binding's value instead of appending a shadowed
    /// duplicate (lookup finds the first match, losing the new value)
    pub(crate) fn rebind(&self, name: Rc<str>, v: Val) {
        let mut bs = self.bindings.borrow_mut();
        for b in bs.iter_mut() {
            if b.name == name {
                b.val = v;
                return;
            }
        }
        bs.push(Binding { name, val: v, is_const: false });
    }

    /// set in this env only (no chain walk); returns Err if const/missing
    fn set_own(&self, name: &str, v: Val) -> Result<bool, JsError> {
        let mut bs = self.bindings.borrow_mut();
        for b in bs.iter_mut() {
            if b.name.as_ref() == name {
                if b.is_const {
                    return Err(JsError::throw("TypeError: Assignment to constant variable"));
                }
                b.val = v;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_own(&self, name: &str) -> bool {
        self.bindings.borrow().iter().any(|b| b.name.as_ref() == name)
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Hard cap on nested script calls (closure + native). Exceeding it throws a
/// RangeError-style JS error instead of overflowing the native stack (which
/// would take the whole browser down).
pub const MAX_CALL_DEPTH: usize = 1000;

pub(crate) type NativeFn = fn(&mut JsEngine, u16, &Val, &[Val]) -> Result<Val, JsError>;
pub(crate) type HostFn = fn(&mut JsEngine, &[Val]) -> Result<Val, JsError>;

pub(crate) struct NativeEntry {
    pub name: Rc<str>,
    pub f: NativeFn,
    pub idx: u16,
    /// embedder-registered host function (f is unused then)
    pub host: Option<HostFn>,
}

/// A pending `setTimeout`/`setInterval` job. The interpreter never runs
/// timers itself — it only queues them; the embedder decides threading and
/// drains the queue via `drain_timers`, invoking callbacks through
/// `call_function`.
pub struct TimerTask {
    /// the id `setTimeout`/`setInterval` returned to the script
    pub id: f64,
    pub delay_ms: f64,
    /// `Some(period)` repeats forever (`setInterval`); `None` fires once
    pub repeat_ms: Option<f64>,
    pub callback: Val,
    pub args: Vec<Val>,
}

// ---------------------------------------------------------------------------
// Microtasks (Promise reactions) + combinators
// ---------------------------------------------------------------------------

pub(crate) const MAX_MICROTASKS: u32 = 10_000; // drain-work cap (runaway `.then` loops fail honestly)

/// A queued microtask job.
pub(crate) enum Job {
    /// a reaction firing with the promise's settlement outcome
    Reaction(Reaction, Result<Val, Val>),
    /// a dynamic `import()` waiting for the loader: runs like any other
    /// microtask, so `await import(...)` pumps it and `.then` chains follow
    /// in order. The URL was resolved when `import()` was called.
    Import { url: Rc<str>, promise: Rc<RefCell<Obj>> },
}

/// Shared bookkeeping for `Promise.all/any/race/allSettled`: a board where
/// element reactions deposit their outcomes and decide when the combined
/// promise settles.
#[derive(Debug)]
pub(crate) struct ComboState {
    /// the combined promise; `None` once it has settled
    pub result: Option<Rc<RefCell<Obj>>>,
    /// combined promise already settled â€” later element outcomes are dropped
    pub done: bool,
    /// how many elements still owe an outcome
    pub remaining: usize,
    /// per-element outcome slot (order-preserving for all/allSettled)
    pub slots: Vec<Option<Result<Val, Val>>>,
    /// which combinator this board serves (deposit/reaction rules differ)
    pub mode: ComboMode,
}

/// element-outcome rules for a combinator board
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum ComboMode {
    /// collect every fulfillment; first rejection rejects
    All,
    /// never short-circuits; every element reports status
    AllSettled,
    /// first settlement (either way) wins
    Race,
    /// first fulfillment wins; all-rejected aggregates
    Any,
}

pub struct JsEngine {
    pub(crate) global_env: Rc<Env>,
    pub(crate) global_obj: Rc<RefCell<Obj>>,
    /// shared prototype for every array (lazy-attached on first access)
    pub(crate) array_proto: Option<Rc<RefCell<Obj>>>,
    /// shared `Object.prototype`: every plain object inherits it lazily
    pub(crate) object_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) natives: Vec<NativeEntry>,
    pub(crate) natives_by_name: HashMap<Rc<str>, usize>,
    pub(crate) call_depth: usize,
    pub(crate) timers: Vec<TimerTask>,
    /// ids passed to `clearTimeout`/`clearInterval` since the last drain;
    /// the embedder removes already-queued jobs with these ids
    pub(crate) cancelled_timers: Vec<f64>,
    pub(crate) microtasks: Vec<Job>,
    /// per-page ES module registry, owned by the engine so the dynamic
    /// `import()` job can run the loader from inside the microtask pump.
    /// Taken out around a load (running a module body needs `&mut self`).
    pub(crate) module_loader: Option<ModuleLoader>,
    /// deferred `new Promise` resolver slots: slot index -> the promise the
    /// registered `resolve`/`reject` natives settle. The slot is emptied on
    /// settle, so the engine holds no reference afterwards (script-level
    /// `Rc`s are the only refs from then on â€” normal refcount release).
    pub(crate) promise_slots: Vec<Option<Rc<RefCell<Obj>>>>,
    /// Promise.resolve/reject/... statics installed by builtins; reachable
    /// through member lookup on the constructor native
    pub(crate) promise_statics: Option<Rc<RefCell<Obj>>>,
    /// shared `Promise.prototype`
    pub(crate) promise_proto: Option<Rc<RefCell<Obj>>>,
    /// own data properties of native constructor values. Natives are ids, not
    /// objects, so `.prototype` and friends live in this side table (builtins
    /// fills it in; `get_member`/`instanceof` consult it).
    pub(crate) native_props: HashMap<usize, Vec<(Rc<str>, Val)>>,
    /// shared `URL.prototype` / `URLSearchParams.prototype`
    pub(crate) url_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) params_proto: Option<Rc<RefCell<Obj>>>,
    /// shared `Set.prototype` / `Map.prototype`
    pub(crate) set_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) map_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) weakmap_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) weakset_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) text_encoder_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) text_decoder_proto: Option<Rc<RefCell<Obj>>>,
    /// reused native ids for the Uint8Array/Buffer stand-ins (avoids
    /// registering a new native per created instance)
    pub(crate) uint8_to_b64: usize,
    pub(crate) buffer_to_string: usize,
    pub(crate) arraybuffer_proto: Option<Rc<RefCell<Obj>>>,
    /// `Function.prototype` — the fallback prototype for function values
    pub(crate) function_proto: Option<Rc<RefCell<Obj>>>,
    /// `Error.prototype` — parent of every error instance, so `extends
    /// Error` and borrowed `Error.prototype.toString` both work
    pub(crate) error_proto: Option<Rc<RefCell<Obj>>>,
    /// native returned by `Function(...)` (its body is not compiled)
    pub(crate) fn_body_native: usize,
    /// compiled-once regex literal cache, keyed by (pattern, flags). Regex
    /// literals are program text, so this cache is bounded by the number of
    /// distinct (pattern, flags) literals the page can create; still capped
    /// at `REGEX_CACHE_MAX` (RAM honesty) â€” beyond the cap compiles are
    /// per-eval (correct, just not cached).
    pub(crate) regex_cache: HashMap<(Rc<str>, Rc<str>), Rc<Regex>>,
    pub(crate) next_timer_id: f64,
    rng_state: u64,
    /// dispatches executed since the last turn reset (`exec` /
    /// `call_function` / `drain_microtasks` each start a fresh turn);
    /// bounded by `step_budget` — see [`JsEngine::tick`]
    pub(crate) steps: u64,
    /// per-turn dispatch cap: a runaway loop aborts its turn with an
    /// honest budget error instead of pegging the worker forever
    /// (bounded-resources rule). Generous — real page bundles run in the
    /// low millions; tests lower it directly
    pub(crate) step_budget: u64,
    /// byte offset of the statement currently evaluating — the budget
    /// error reports line/col through this (enriched in `exec`, which has
    /// the source; dispatch turns only carry the offset)
    pub(crate) cur_span: usize,
    /// label of the loop currently being entered (`outer: for (...)`); the
    /// loop arm takes it so `continue outer` knows its own label
    pub(crate) pending_label: Option<Rc<str>>,
    /// names of the JS functions currently on the call stack (innermost
    /// last). Only for diagnostics: a runtime TypeError names the function
    /// it happened in when the exact span is not enough.
    pub(crate) call_names: Vec<Rc<str>>,
    /// source text of every parsed script/module, indexed by
    /// `FnDef::src - 1` (see [`JsEngine::register_source`]). Keeping the text
    /// is what lets a throw report a location in the file that *defined* the
    /// running function, even when another script called it.
    pub(crate) sources: Vec<Rc<str>>,
    pub(crate) sources_bytes: usize,
    /// source the currently running code belongs to (set by `exec`/the module
    /// loader, then per call from `FnDef::src`)
    pub(crate) cur_src: u32,
    /// origin of the throw currently unwinding (span + source), recorded once
    /// by [`JsEngine::note_error`]
    pub(crate) throw_span: usize,
    pub(crate) throw_src: u32,
    /// a throw already noted its origin; later frames in the unwind must not
    /// overwrite it
    pub(crate) err_located: bool,
    /// the escaping error text already carries its `(line, col)`
    pub(crate) err_decorated: bool,
    /// CSSOM bridge installed by the embedder (`script.rs`): a `style`
    /// object (see [`ObjKind::Style`]) forwards member reads/writes here.
    /// `None` = no DOM attached (pure-engine unit tests): reads are
    /// `undefined`, writes are no-ops.
    pub(crate) style_get: Option<fn(u32, &str) -> String>,
    pub(crate) style_set: Option<fn(u32, &str, &str)>,
    /// Page URL used as the fallback base for dynamic `import()` outside a
    /// module body (module bodies use the module's own URL). Set by the
    /// embedder; `None` in pure-engine tests.
    pub(crate) import_base: Option<Rc<str>>,
    /// native id of the global `import` function (member reads of `.meta`
    /// build the import-meta object from the executing module's URL).
    pub(crate) import_native: Option<usize>,
    /// source id → absolute module URL. A closure calls back with
    /// `cur_src = def.src`, so `import()` and `import.meta.url` inside any
    /// function defined by a module resolve against that module's URL.
    pub(crate) module_urls: HashMap<u32, Rc<str>>,
}

pub(crate) const REGEX_CACHE_MAX: usize = 512;

/// Retained runtime-error sources per engine and their total byte budget.
/// Diagnostics are worth a few MB, never an unbounded amount: a page with
/// more/larger scripts than this simply reports errors without line/col.
pub(crate) const SOURCES_MAX: usize = 256;
pub(crate) const SOURCES_BYTES_MAX: usize = 4 << 20;

/// Default per-turn statement/expression dispatch cap. Sized so real
/// page bundles (70KB startup script ≈ low-millions of dispatches) run
/// untouched while a `for(;;)` spin stops within ~1s of CPU in release.
pub(crate) const DEFAULT_STEP_BUDGET: u64 = 100_000_000;

impl Default for JsEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl JsEngine {
    /// Create an engine with the standard global object installed
    /// (console, Math, JSON, Object, Array, Date, Error family, parseInt,
    /// timers stubs, ...).
    pub fn new() -> JsEngine {
        let mut e = JsEngine {
            global_env: Env::root(),
            global_obj: Rc::new(RefCell::new(Obj {
                kind: ObjKind::Plain,
                props: Vec::new(),
                proto: None,
            })),
            array_proto: None,
            object_proto: None,
            natives: Vec::new(),
            natives_by_name: HashMap::new(),
            call_depth: 0,
            timers: Vec::new(),
            cancelled_timers: Vec::new(),
            microtasks: Vec::new(),
            module_loader: None,
            promise_slots: Vec::new(),
            promise_statics: None,
            promise_proto: None,
            native_props: HashMap::new(),
            url_proto: None,
            params_proto: None,
            set_proto: None,
            map_proto: None,
            weakmap_proto: None,
            weakset_proto: None,
            text_encoder_proto: None,
            text_decoder_proto: None,
            uint8_to_b64: 0,
            buffer_to_string: 0,
            arraybuffer_proto: None,
            function_proto: None,
        error_proto: None,
            fn_body_native: 0,
            regex_cache: HashMap::new(),
            next_timer_id: 1.0,
            steps: 0,
            step_budget: DEFAULT_STEP_BUDGET,
            cur_span: 0,
            pending_label: None,
            call_names: Vec::new(),
            sources: Vec::new(),
            sources_bytes: 0,
            cur_src: 0,
            throw_span: 0,
            throw_src: 0,
            err_located: false,
            err_decorated: false,
            style_get: None,
            style_set: None,
            import_base: None,
            import_native: None,
            module_urls: HashMap::new(),
            rng_state: 0x9e37_79b9_7f4a_7c15 ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1),
        };
        builtins::install(&mut e);
        e
    }

    /// Compile (or fetch from the per-engine cache) a regex literal.
    pub(crate) fn regex_from_literal(
        &mut self,
        pattern: Rc<str>,
        flags: Rc<str>,
    ) -> Result<Rc<Regex>, JsError> {
        let key = (pattern.clone(), flags.clone());
        if let Some(re) = self.regex_cache.get(&key) {
            return Ok(re.clone());
        }
        let re = regex::compile(&pattern, &flags).map_err(|re| JsError::Syntax {
            msg: format!("/{}/{}: {}", pattern, flags, re.msg),
            line: 1,
            col: 1,
        })?;
        let re = Rc::new(re);
        if self.regex_cache.len() < REGEX_CACHE_MAX {
            self.regex_cache.insert(key, re.clone());
        }
        Ok(re)
    }

    /// Parse + run a script. State (globals) persists across `exec` calls on
    /// the same engine â€” that's what makes multi-`<script>` pages work.
    /// The returned value is the last evaluated expression statement's value
    /// (JS `eval` completion-value semantics; declarations contribute
    /// nothing). Handy for embedder/tests; not observable from script code.
    pub fn exec(&mut self, source: &str) -> Result<Val, JsError> {
        let src_id = self.register_source(source);
        let program: Program = parser::parse(source, src_id)?;
        self.call_depth = 0;
        self.steps = 0;
        self.cur_span = 0;
        self.cur_src = src_id;
        self.err_located = false;
        self.err_decorated = false;
        let env = self.global_env.clone();
        interp::hoist(self, &env, &program);
        let mut last = Val::Undefined;
        for st in program.iter() {
            // a Budget error from this turn names the spinning statement;
            // `finish_error` resolves the recorded offset to line/col
            let r = match &st.kind {
                parser::StmtKind::Expr(e) => interp::eval_expr(self, &env, e).map(|v| {
                    last = v;
                }),
                _ => interp::eval_stmt(self, &env, st).map(|_| ()),
            };
            r.map_err(|err| self.finish_error(err, source))?;
        }
        // microtask quiescence at the end of every execution turn (idempotent
        // with the embedder's own drain later)
        self.run_microtasks(MAX_MICROTASKS)?;
        Ok(last)
    }

    /// Run already-fetched module source on this engine's module registry
    /// (static `<script type="module">`). The loader is taken out around the
    /// call because a module body runs with `&mut self` (re-entrancy).
    pub(crate) fn load_module_source(&mut self, url: &str, src: &str) -> Result<(), String> {
        let mut loader = self.module_loader.take().unwrap_or_default();
        let r = loader.load_source(self, url, src);
        self.module_loader = Some(loader);
        r.map(|_| ())
    }

    /// Dynamic `import(url)`: fetch/execute through the same registry and
    /// return the namespace object. The loader is taken out for the same
    /// reason as above.
    pub(crate) fn load_module(&mut self, url: &str) -> Result<Val, String> {
        let mut loader = self.module_loader.take().unwrap_or_default();
        let r = loader.load(self, url);
        self.module_loader = Some(loader);
        r.map(|ns| module::namespace_value(&ns))
    }

    /// URL of the module whose code is executing (`None` for classic
    /// scripts): drives both `import()`'s specifier base and
    /// `import.meta.url`.
    pub(crate) fn module_url_for(&self, src: u32) -> Option<Rc<str>> {
        self.module_urls.get(&src).cloned()
    }

    /// Invoke a JS function value from the embedder (event handlers, timer
    /// callbacks, later: DOM callbacks). `this` is the method receiver
    /// (pass `Val::Undefined` for the sloppy-mode global).
    pub fn call_function(&mut self, func: &Val, this: Val, args: &[Val]) -> Result<Val, JsError> {
        // fresh turn: dispatchers (event handlers, timers) each get their
        // own budget — one runaway handler must not starve later turns
        self.steps = 0;
        self.cur_span = 0;
        self.err_located = false;
        self.err_decorated = false;
        let r = self.call(func.clone(), this, args.to_vec());
        r.map_err(|err| self.finish_error(err, ""))
    }

    /// Register a source text for runtime error locations; returns its id
    /// (1-based, 0 = not retained). Retention is bounded: a page pays at most
    /// [`SOURCES_BYTES_MAX`] of extra RAM for diagnostics, past which errors
    /// simply lose their line/col rather than the page losing memory.
    pub(crate) fn register_source(&mut self, src: &str) -> u32 {
        if self.sources.len() >= SOURCES_MAX || self.sources_bytes + src.len() > SOURCES_BYTES_MAX {
            return 0;
        }
        self.sources_bytes += src.len();
        self.sources.push(Rc::from(src));
        self.sources.len() as u32
    }

    fn source_text(&self, id: u32) -> Option<Rc<str>> {
        if id == 0 {
            None
        } else {
            self.sources.get(id as usize - 1).cloned()
        }
    }

    /// Record where a runtime throw originated, at most once per error: the
    /// deepest `eval_expr`/`eval_stmt` that saw it wins. The thrown value is
    /// never touched, so `catch (e)` still receives exactly what `throw`
    /// passed — the location is only added to the text an escaping error is
    /// logged with (see [`finish_error`]).
    pub(crate) fn note_error(&mut self, err: &JsError) {
        if self.err_located || !matches!(err, JsError::Throw(_)) {
            return;
        }
        self.err_located = true;
        self.throw_span = self.cur_span;
        self.throw_src = self.cur_src;
    }

    /// Turn an escaping error into what the embedder logs: `(line, col)` from
    /// the recorded origin, resolved against the source of the function that
    /// threw. Budget errors get theirs here too (a turn boundary is the last
    /// frame that knows the source).
    pub(crate) fn finish_error(&mut self, err: JsError, fallback: &str) -> JsError {
        match err {
            JsError::Budget { .. } => {
                let src = self.source_text(self.cur_src);
                match src {
                    Some(s) => {
                        let (line, col) = parser::line_col(&s, self.cur_span);
                        JsError::Budget { line, col }
                    }
                    None if fallback.is_empty() => JsError::Budget { line: 0, col: 0 },
                    None => {
                        let (line, col) = parser::line_col(fallback, self.cur_span);
                        JsError::Budget { line, col }
                    }
                }
            }
            JsError::Throw(Val::Str(msg)) => {
                if self.err_decorated {
                    return JsError::Throw(Val::Str(msg));
                }
                let Some(src) = self.source_text(self.throw_src) else {
                    return JsError::Throw(Val::Str(msg));
                };
                if self.throw_span == 0 || self.throw_span >= src.len() {
                    return JsError::Throw(Val::Str(msg));
                }
                let (line, col) = parser::line_col(&src, self.throw_span);
                if line == 0 {
                    return JsError::Throw(Val::Str(msg));
                }
                self.err_decorated = true;
                JsError::Throw(Val::Str(Rc::from(
                    format!("{msg} (line {line}, col {col})").as_str(),
                )))
            }
            other => other,
        }
    }

    /// One interpreter dispatch. Every `eval_stmt`/`eval_expr` (and the
    /// generator driver loop) passes through here, so any loop — including
    /// a `for(;;)` fed by a misbehaving regex scanner — is bounded. The
    /// error carries no line/col here; `exec` resolves the recorded
    /// `cur_span` offset against the source it owns.
    #[inline]
    pub(crate) fn tick(&mut self) -> Result<(), JsError> {
        self.steps += 1;
        if self.steps > self.step_budget {
            return Err(JsError::Budget { line: 0, col: 0 });
        }
        Ok(())
    }

    /// Define a host function visible to scripts under `name` (fetch, DOM
    /// shims, ...). Host functions don't receive `this`; if a script calls
    /// one as a method, the receiver is dropped (documented shortcut).
    pub fn define_native(
        &mut self,
        name: &str,
        f: fn(&mut JsEngine, &[Val]) -> Result<Val, JsError>,
    ) -> usize {
        fn wrap(_e: &mut JsEngine, _idx: u16, _this: &Val, _args: &[Val]) -> Result<Val, JsError> {
            unreachable!("host natives go through the host field")
        }
        let id = self.natives.len();
        self.natives.push(NativeEntry {
            name: Rc::from(name),
            f: wrap,
            idx: 0,
            host: Some(f),
        });
        self.set_global(name, Val::Native(id));
        id
    }

    /// Set a global. Script-declared bindings (`var`/`let`/`const`/function
    /// declarations) live in the global *environment*; anything else goes on
    /// the global object. Updating the right slot keeps script reads and
    /// embedder writes consistent.
    pub fn set_global(&mut self, name: &str, val: Val) {
        if self.global_env.set_own(name, val.clone()).ok() == Some(true) {
            return;
        }
        obj_set_own(&self.global_obj, name, val);
    }

    /// Read a global: script-declared bindings first, then the global object.
    pub fn get_global(&self, name: &str) -> Val {
        self.env_get(&self.global_env, name)
    }

    /// `globalThis` / `window` (until the DOM wave gives `window` real
    /// meaning, it aliases the global object).
    pub fn global_object(&self) -> Val {
        Val::Obj(self.global_obj.clone())
    }

    /// Take the queue of pending `setTimeout` jobs (FIFO). The embedder runs
    /// them however it likes â€” the interpreter has no timer thread.
    pub fn drain_timers(&mut self) -> Vec<TimerTask> {
        std::mem::take(&mut self.timers)
    }

    /// Ids cancelled with `clearTimeout`/`clearInterval` since the last call.
    pub fn drain_cancelled_timers(&mut self) -> Vec<f64> {
        std::mem::take(&mut self.cancelled_timers)
    }

    /// Run queued microtask jobs (Promise reactions) until the queue is
    /// empty, capped at `MAX_MICROTASKS` runs so a runaway `.then` loop
    /// fails with an honest RangeError instead of looping forever.
    /// Reactions fire FIFO, preserving microtask ordering semantics.
    pub fn drain_microtasks(&mut self) -> Result<(), JsError> {
        self.run_microtasks(MAX_MICROTASKS)
    }

    /// internal bounded drainer: runs up to `budget` jobs; returns Err if
    /// the queue is still not empty afterwards.
    pub(crate) fn run_microtasks(&mut self, budget: u32) -> Result<(), JsError> {
        // the drain is its own turn: reactions run against a fresh budget
        // (each is also individually bounded by `tick` while it runs)
        self.steps = 0;
        for _ in 0..budget {
            if self.microtasks.is_empty() {
                return Ok(());
            }
            let job = self.microtasks.remove(0);
            exec_job(self, job)?;
        }
        if self.microtasks.is_empty() {
            Ok(())
        } else {
            Err(JsError::throw(
                "RangeError: microtask queue did not settle within the step budget",
            ))
        }
    }

    /// Spin the microtask queue until `p` settles, bounded by work already
    /// documented on `run_microtasks`. This is the `await` of a NOT-yet
    /// settled promise. Design tradeoff (honest): the interpreter has no
    /// stack suspension in this wave, so `await` blocks the calling frame
    /// synchronously while pumping reactions; if the queue drains while `p`
    /// is still pending, the promise would only settle from a timer/macrotask
    /// and that is an honest error at this point.
    pub(crate) fn await_promise(
        &mut self,
        p: &Rc<RefCell<Obj>>,
    ) -> Result<Result<Val, Val>, JsError> {
        let mut budget = MAX_MICROTASKS;
        loop {
            if let Some(out) = promise_outcome(p) {
                return Ok(out);
            }
            if self.microtasks.is_empty() {
                return Err(JsError::throw(
                    "Error: await of a promise settled only by a timer/macrotask is unsupported in this wave (microtask-queue empty while promise pending)",
                ));
            }
            if budget == 0 {
                return Err(JsError::throw(
                    "RangeError: await exceeded the microtask step budget",
                ));
            }
            budget -= 1;
            let job = self.microtasks.remove(0);
            exec_job(self, job)?;
        }
    }

    pub(crate) fn register_native(&mut self, name: &str, f: NativeFn, idx: u16) -> usize {
        let id = self.natives.len();
        self.natives.push(NativeEntry {
            name: Rc::from(name),
            f,
            idx,
            host: None,
        });
        self.natives_by_name.insert(Rc::from(name), id);
        id
    }

    pub(crate) fn native_id(&self, name: &str) -> Option<usize> {
        self.natives_by_name.get(name).copied()
    }

    /// Attach an own data property to a native constructor value (natives are
    /// ids, so this is the only place their properties can live).
    pub(crate) fn set_native_prop(&mut self, id: usize, name: &str, val: Val) {
        let entry = self.native_props.entry(id).or_default();
        let key = Rc::from(name);
        if let Some(slot) = entry.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = val;
        } else {
            entry.push((key, val));
        }
    }

    pub(crate) fn native_prop(&self, id: usize, name: &str) -> Option<Val> {
        self.native_props
            .get(&id)?
            .iter()
            .find(|(k, _)| k.as_ref() == name)
            .map(|(_, v)| v.clone())
    }

    pub(crate) fn random_f64(&mut self) -> f64 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Core call dispatch (also used by call_function). Enforces the depth
    /// cap so runaway recursion throws instead of crashing the process.
    pub(crate) fn call(&mut self, callee: Val, this: Val, args: Vec<Val>) -> Result<Val, JsError> {
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(JsError::throw(
                "RangeError: Maximum call stack size exceeded",
            ));
        }
        self.call_depth += 1;
        // diagnostics only: remember the JS frames so a TypeError can name
        // them (see `stack_hint`) and the executing function's source so a
        // throw resolves against the file that defined it (see `finish_error`)
        let before = self.call_names.len();
        let outer_src = self.cur_src;
        if let Val::Func(c) = &callee {
            let n = if c.def.name.is_empty() {
                Rc::from("<anonymous>")
            } else {
                c.def.name.clone()
            };
            self.call_names.push(n);
            self.cur_src = c.def.src;
        }
        let r = self.call_inner(callee, this, args);
        self.call_names.truncate(before);
        self.cur_src = outer_src;
        self.call_depth -= 1;
        r
    }

    /// Innermost call-frame names, for error text: `" (in b <- a)"`.
    /// Bounded to three frames so a deep stack cannot bloat a log line.
    pub(crate) fn stack_hint(&self) -> String {
        if self.call_names.is_empty() {
            return String::new();
        }
        let mut names: Vec<&str> = self
            .call_names
            .iter()
            .rev()
            .take(3)
            .map(|n| n.as_ref())
            .collect();
        names.reverse();
        format!(" (in {})", names.join(" <- "))
    }

    fn call_inner(&mut self, callee: Val, this: Val, args: Vec<Val>) -> Result<Val, JsError> {
        match callee {
            Val::Func(c) => {
                let env = Env::child(&c.env);
                let this_val = if c.is_arrow {
                    c.this.clone() // lexical this
                } else if matches!(this, Val::Undefined) {
                    // sloppy-mode: plain calls get the global object as this
                    Val::Obj(self.global_obj.clone())
                } else {
                    this
                };
                env.declare(Rc::from("this"), this_val.clone(), false);
                if !c.is_arrow && c.def.uses_arguments {
                    let items = args.clone();
                    env.declare(Rc::from("arguments"), Val::new_array(items), false);
                }
                interp::bind_params(self, &env, &c.def, &args)?;
                if c.def.is_generator {
                    // generators never run the body at call time; the mini-VM
                    // (gen.rs) resumes it on `.next`
                    return gen::gen_object(self, &c.def, &env);
                }
                if c.def.is_async {
                    // run-to-completion tradeoff: the body runs NOW to
                    // completion; each `await` inside pumps the microtask
                    // queue (await_promise) and the result settles the
                    // promise this call returns.
                    let p = self.new_promise();
                    match interp::run_function_body(self, &c, &env) {
                        Ok(v) => resolve_promise(self, &p, v),
                        Err(JsError::Throw(v)) => settle_promise(self, &p, Err(v)),
                        Err(other) => return Err(other),
                    }
                    return Ok(Val::Obj(p));
                }
                match &c.def.body {
                    FnBody::Expr(x) => interp::eval_expr(self, &env, x),
                    FnBody::Block(_stmts) => interp::run_function_body(self, &c, &env),
                    // Function.prototype.bind partial application
                    FnBody::Bound {
                        target,
                        this: bthis,
                        args: bargs,
                    } => {
                        let mut all = bargs.clone();
                        all.extend(args);
                        self.call(target.clone(), bthis.clone(), all)
                    }
                }
            }
            Val::Native(id) => {
                let (f, idx, host) = {
                    let en = self
                        .natives
                        .get(id)
                        .ok_or_else(|| JsError::throw("ReferenceError: broken native binding"))?;
                    (en.f, en.idx, en.host)
                };
                match host {
                    Some(h) => h(self, &args),
                    None => f(self, idx, &this, &args),
                }
            }
            Val::Method(m) => interp::call_method(self, m, &this, &args),
            // shortcut: exotic callable objects. `Object`/`Array` are plain
            // objects with their statics hung off them, so calling one as a
            // function (`Object(x)`, `Array(3)`, `new Array(3)`) needs a
            // hidden `__call` native to take over. Nothing else is callable.
            Val::Obj(o) => {
                let hook = o
                    .borrow()
                    .props
                    .iter()
                    .find(|(k, _)| &**k == "__call")
                    .map(|(_, v)| v.clone());
                match hook {
                    Some(f) => self.call(f, this, args),
                    None => Err(JsError::Throw(Val::Str(Rc::from(
                        "TypeError: object is not a function".to_string(),
                    )))),
                }
            }
            other => {
                Err(JsError::Throw(Val::Str(Rc::from(format!(
                    "TypeError: {} is not a function",
                    other.type_of()
                )))))
            }
        }
    }

    // -- identifier resolution (env chain, then global object) --------------

    pub(crate) fn env_get(&self, env: &Rc<Env>, name: &str) -> Val {
        let mut cur: &Env = env;
        loop {
            if let Some(v) = cur.bindings.borrow().iter().find(|b| b.name.as_ref() == name) {
                return v.val.clone();
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        obj_get_own(&self.global_obj, name)
    }

    /// Assignment: walks the chain; const bindings throw; unknown names are
    /// (sloppily) created as globals â€” real-world scripts rely on that.
    pub(crate) fn env_set(&mut self, env: &Rc<Env>, name: &str, v: Val) -> Result<(), JsError> {
        let mut cur: &Env = env;
        loop {
            if cur.set_own(name, v.clone())? {
                return Ok(());
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        // implicit global (sloppy mode, documented)
        obj_set_own(&self.global_obj, name, v);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Property access
// ---------------------------------------------------------------------------

const PROTO_WALK_CAP: usize = 100; // prototype cycle guard

// ---------------------------------------------------------------------------
// Promise machinery (engine-level; builtins installs the constructor and
// statics, interp drives `await`)
// ---------------------------------------------------------------------------

/// fresh pending promise object
pub(crate) fn new_promise_obj() -> Rc<RefCell<Obj>> {
    Rc::new(RefCell::new(Obj {
        kind: ObjKind::Promise(PromiseState::default()),
        props: Vec::new(),
        proto: None,
    }))
}

impl JsEngine {
    /// A fresh promise chained to `Promise.prototype`, so `getPrototypeOf`
    /// and `instanceof Promise` see the prototype bundles install onto.
    pub(crate) fn new_promise(&mut self) -> Rc<RefCell<Obj>> {
        let p = new_promise_obj();
        if let Some(proto) = self.promise_proto.clone() {
            p.borrow_mut().proto = Some(proto);
        }
        p
    }
}

/// the inner promise object if `v` is one
pub(crate) fn promise_of(v: &Val) -> Option<Rc<RefCell<Obj>>> {
    match v {
        Val::Obj(o) if matches!(o.borrow().kind, ObjKind::Promise(_)) => Some(o.clone()),
        _ => None,
    }
}

/// current settlement (None = still pending)
pub(crate) fn promise_outcome(p: &Rc<RefCell<Obj>>) -> Option<Result<Val, Val>> {
    let b = p.borrow();
    match &b.kind {
        ObjKind::Promise(st) => st.settled.clone(),
        _ => None,
    }
}

/// settle `p` (no-op if already settled) and enqueue every reaction FIFO
pub(crate) fn settle_promise(e: &mut JsEngine, p: &Rc<RefCell<Obj>>, out: Result<Val, Val>) {
    let reactions = {
        let mut b = p.borrow_mut();
        match &mut b.kind {
            ObjKind::Promise(st) => {
                if st.settled.is_some() {
                    return; // second resolve/reject is ignored (JS semantics)
                }
                st.settled = Some(out.clone());
                std::mem::take(&mut st.reactions)
            }
            _ => return,
        }
    };
    for re in reactions {
        e.microtasks.push(Job::Reaction(re, out.clone()));
    }
}

/// the `resolve` function semantics: adopt our promises (chaining), else
/// fulfill directly. shortcut: foreign thenables (objects with a
/// user-defined `then`) resolve as plain values â€” documented.
pub(crate) fn resolve_promise(e: &mut JsEngine, p: &Rc<RefCell<Obj>>, v: Val) {
    if let Val::Obj(o) = &v {
        if Rc::ptr_eq(o, p) {
            settle_promise(
                e,
                p,
                Err(Val::Str(Rc::from(
                    "TypeError: Chaining cycle detected in promise",
                ))),
            );
            return;
        }
        if matches!(o.borrow().kind, ObjKind::Promise(_)) {
            match promise_outcome(o) {
                Some(out) => {
                    settle_promise(e, p, out);
                }
                None => {
                    let mut b = o.borrow_mut();
                    if let ObjKind::Promise(st) = &mut b.kind {
                        st.reactions.push(Reaction {
                            kind: ReactKind::Identity,
                            ret: Some(p.clone()),
                        });
                    }
                }
            }
            return;
        }
    }
    settle_promise(e, p, Ok(v));
}

/// `.then(onOk, onErr)` â€” creates the derived promise, queues (or fires)
/// the reaction; the derived promise is returned so `.then` chains.
pub(crate) fn promise_then(
    e: &mut JsEngine,
    p: &Rc<RefCell<Obj>>,
    on_ok: Val,
    on_err: Val,
) -> Result<Val, JsError> {
    let ret = e.new_promise();
    let re = Reaction {
        kind: ReactKind::Js { on_ok, on_err },
        ret: Some(ret.clone()),
    };
    if let Some(out) = promise_outcome(p) {
        e.microtasks.push(Job::Reaction(re, out));
    } else {
        let mut b = p.borrow_mut();
        if let ObjKind::Promise(st) = &mut b.kind {
            st.reactions.push(re);
        }
    }
    Ok(Val::Obj(ret))
}

/// `new Promise(executor)`: register resolve/reject natives over a fresh
/// slot, run the executor, reject on a synchronous throw.
pub(crate) fn new_promise_with_executor(e: &mut JsEngine, executor: Val) -> Result<Val, JsError> {
    let p = e.new_promise();
    let slot = e.promise_slots.len() as u16;
    e.promise_slots.push(Some(p.clone()));
    let rid = e.register_native("Promise.resolve#slot", n_p_resolve, slot);
    let jid = e.register_native("Promise.reject#slot", n_p_reject, slot);
    // honest shortcut: the two small dispatch-table entries per constructed
    // promise live until engine drop (bounded by script turn length)
    let run = if matches!(executor, Val::Undefined | Val::Null) {
        Ok(())
    } else {
        e.call(
            executor,
            Val::Undefined,
            vec![Val::Native(rid), Val::Native(jid)],
        )
        .map(|_| ())
    };
    match run {
        Ok(()) => {}
        Err(JsError::Throw(v)) => reject_slot(e, slot, v),
        Err(other) => return Err(other),
    }
    Ok(Val::Obj(p))
}

fn resolve_slot(e: &mut JsEngine, idx: u16, v: Val) {
    if let Some(p) = e.promise_slots.get(idx as usize).cloned().flatten() {
        resolve_promise(e, &p, v);
        e.promise_slots[idx as usize] = None; // free the engine-side ref
    }
}

fn reject_slot(e: &mut JsEngine, idx: u16, v: Val) {
    if let Some(p) = e.promise_slots.get(idx as usize).cloned().flatten() {
        settle_promise(e, &p, Err(v));
        e.promise_slots[idx as usize] = None; // free the engine-side ref
    }
}

fn n_p_resolve(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let v = args.first().cloned().unwrap_or(Val::Undefined);
    resolve_slot(e, idx, v);
    Ok(Val::Undefined)
}

fn n_p_reject(e: &mut JsEngine, idx: u16, _this: &Val, args: &[Val]) -> Result<Val, JsError> {
    let v = args.first().cloned().unwrap_or(Val::Undefined);
    reject_slot(e, idx, v);
    Ok(Val::Undefined)
}

fn is_callable(v: &Val) -> bool {
    matches!(v, Val::Func(_) | Val::Native(_) | Val::Method(_))
}

/// deliver an outcome into the derived promise (resolve adopting promises)
fn deliver_resolve(e: &mut JsEngine, ret: Option<Rc<RefCell<Obj>>>, v: Val) {
    if let Some(p) = ret {
        resolve_promise(e, &p, v);
    }
}

fn deliver(e: &mut JsEngine, ret: Option<Rc<RefCell<Obj>>>, out: Result<Val, Val>) {
    if let Some(p) = ret {
        match out {
            Ok(v) => resolve_promise(e, &p, v),
            Err(err) => settle_promise(e, &p, Err(err)),
        }
    }
}

/// run one queued reaction (see `Job::Reaction`) or dynamic import
fn exec_job(e: &mut JsEngine, job: Job) -> Result<(), JsError> {
    let (re, out) = match job {
        Job::Reaction(re, out) => (re, out),
        Job::Import { url, promise } => {
            // a module job: fetch/execute (cached after the first time), then
            // settle the promise `import()` handed back; `.then` follows in
            // the same drain and `await import(...)` pumps it here
            match e.load_module(&url) {
                Ok(ns) => {
                    eprintln!("[js] dynamic import {url}");
                    settle_promise(e, &promise, Ok(ns));
                }
                Err(err) => {
                    // unhandled unless the page attached a handler; log once
                    // (this engine has no `unhandledrejection` event)
                    eprintln!("[js] dynamic import failed: {err}");
                    settle_promise(e, &promise, Err(Val::Str(Rc::from(err.as_str()))));
                }
            }
            return Ok(());
        }
    };
    match re.kind {
        ReactKind::Js { on_ok, on_err } => {
            let handler = match &out {
                Ok(_) => on_ok,
                Err(_) => on_err,
            };
            if !is_callable(&handler) {
                // absent handler = pass-through (chain transparency)
                deliver(e, re.ret, out);
                return Ok(());
            }
            let value = match &out {
                Ok(v) | Err(v) => v.clone(),
            };
            // a callback throwing rejects the derived promise instead of
            // failing the whole drain (JS microtask semantics)
            match e.call(handler, Val::Undefined, vec![value]) {
                Ok(v) => deliver_resolve(e, re.ret, v),
                Err(JsError::Throw(thrown)) => deliver(e, re.ret, Err(thrown)),
                Err(other) => return Err(other),
            }
            Ok(())
        }
        ReactKind::Identity => Ok(()), // adoption handled at resolve time
        ReactKind::Finally { cb } => {
            let res = e.call(cb, Val::Undefined, vec![]);
            match res {
                // `.finally`: run cb, discard its result, propagate outcome
                Ok(_) => {
                    deliver(e, re.ret, out);
                    Ok(())
                }
                Err(JsError::Throw(thrown)) => {
                    deliver(e, re.ret, Err(thrown));
                    Ok(())
                }
                Err(other) => Err(other),
            }
        }
        ReactKind::All { slot, state } => {
            let finish = {
                let mut st = state.borrow_mut();
                if st.done {
                    None
                } else {
                    match &out {
                        Ok(v) => {
                            st.slots[slot] = Some(Ok(v.clone()));
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
                                st.result.take().map(|p| (p, Ok(Val::new_array(vals))))
                            } else {
                                None
                            }
                        }
                        Err(err) => {
                            st.done = true;
                            st.result.take().map(|p| (p, Err(err.clone())))
                        }
                    }
                }
            };
            deliver_combo(e, finish)
        }
        ReactKind::Any { slot, state } => {
            let finish = {
                let mut st = state.borrow_mut();
                if st.done {
                    None
                } else {
                    match &out {
                        Ok(v) => {
                            // a fulfillment wins immediately regardless of order
                            st.done = true;
                            st.result.take().map(|p| (p, Ok(v.clone())))
                        }
                        Err(_) => {
                            // rejection only aggregates when every element
                            // has settled (no slot okayed)
                            st.slots[slot] = Some(Err(Val::Undefined));
                            st.remaining -= 1;
                            if st.remaining == 0 {
                                st.done = true;
                                st.result.take().map(|p| {
                                    // honest shortcut: one shared aggregate
                                    // error message instead of a full
                                    // AggregateError reason list
                                    (
                                        p,
                                        Err(Val::Str(Rc::from(
                                            "Error: no promise from Promise.any was fulfilled",
                                        ))),
                                    )
                                })
                            } else {
                                None
                            }
                        }
                    }
                }
            };
            deliver_combo(e, finish)
        }
        ReactKind::Race { state } => {
            let finish = {
                let mut st = state.borrow_mut();
                if st.done {
                    None
                } else {
                    st.done = true;
                    match &out {
                        Ok(v) => st.result.take().map(|p| (p, Ok(v.clone()))),
                        Err(err) => st.result.take().map(|p| (p, Err(err.clone()))),
                    }
                }
            };
            deliver_combo(e, finish)
        }
        ReactKind::AllSettled { slot, state } => {
            let finish = {
                let mut st = state.borrow_mut();
                if st.done {
                    None
                } else {
                    st.slots[slot] = Some(match &out {
                        Ok(v) => Ok(v.clone()),
                        Err(err) => Err(err.clone()),
                    });
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
                        st.result.take().map(|p| (p, Ok(Val::new_array(vals))))
                    } else {
                        None
                    }
                }
            };
            deliver_combo(e, finish)
        }
    }
}

/// helper: settle the combined promise if a combinator stepper produced one
fn deliver_combo(
    e: &mut JsEngine,
    step: Option<(Rc<RefCell<Obj>>, Result<Val, Val>)>,
) -> Result<(), JsError> {
    match step {
        Some((p, out)) => {
            match out {
                Ok(v) => resolve_promise(e, &p, v),
                Err(err) => settle_promise(e, &p, Err(err)),
            }
            Ok(())
        }
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Property access
// ---------------------------------------------------------------------------

/// Own-property read (no prototype walk, no methods).
pub fn obj_get_own(o: &Rc<RefCell<Obj>>, key: &str) -> Val {
    let b = o.borrow();
    if let ObjKind::Array(items) = &b.kind {
        if let Ok(i) = key.parse::<usize>() {
            return items.get(i).cloned().unwrap_or(Val::Undefined);
        }
    }
    match b.props.iter().find(|(k, _)| k.as_ref() == key) {
        Some((_, v)) => v.clone(),
        None => Val::Undefined,
    }
}

/// the object handle behind a `Val::Obj` (internal use; panics otherwise)
pub(crate) fn obj_of(v: &Val) -> Rc<RefCell<Obj>> {
    match v {
        Val::Obj(o) => o.clone(),
        _ => unreachable!("obj_of on non-object"),
    }
}

/// Own-property write (array exotic handles integer keys + length).
pub fn obj_set_own(o: &Rc<RefCell<Obj>>, key: &str, v: Val) {
    let mut b = o.borrow_mut();
    if let ObjKind::Array(items) = &mut b.kind {
        if key == "length" {
            let n = v.to_number();
            if n.is_finite() && n >= 0.0 {
                let n = n as usize;
                items.resize(n, Val::Undefined);
            }
            return;
        }
        if let Ok(i) = key.parse::<usize>() {
            if i < 1 << 32 {
                if i >= items.len() {
                    items.resize(i + 1, Val::Undefined);
                }
                items[i] = v;
                return;
            }
        }
    }
    match b.props.iter_mut().find(|(k, _)| k.as_ref() == key) {
        Some((_, slot)) => {
            // `{ get x(){}, set x(v){} }` (and a class's get/set pair) must
            // become ONE accessor property, not the last half replacing the
            // first: merge the two halves instead of overwriting.
            if let (Val::Acc(old), Val::Acc(new)) = (&*slot, &v) {
                *slot = Val::Acc(Rc::new(AccEntry {
                    getter: new.getter.clone().or_else(|| old.getter.clone()),
                    setter: new.setter.clone().or_else(|| old.setter.clone()),
                }));
            } else {
                *slot = v;
            }
        }
        None => b.props.push((Rc::from(key), v)),
    }
}

pub fn obj_has(o: &Rc<RefCell<Obj>>, key: &str) -> bool {
    let mut cur = Some(o.clone());
    for _ in 0..PROTO_WALK_CAP {
        let c = match cur {
            None => break,
            Some(c) => c,
        };
        let b = c.borrow();
        if let ObjKind::Array(items) = &b.kind {
            if key == "length" {
                return true;
            }
            if let Ok(i) = key.parse::<usize>() {
                if i < items.len() {
                    return true;
                }
            }
        }
        if b.props.iter().any(|(k, _)| k.as_ref() == key) {
            return true;
        }
        cur = b.proto.clone();
    }
    false
}

/// Full member read including the prototype chain and built-in methods.
pub(crate) fn get_member(e: &mut JsEngine, base: &Val, key: &str) -> Result<Val, JsError> {
    get_member_impl(e, base, key)
}

fn get_member_impl(e: &mut JsEngine, base: &Val, key: &str) -> Result<Val, JsError> {
    match base {
        // accessor values never reach script land as plain member values
        // (get_member unwraps them); unreachable in practice
        Val::Acc(_) => Ok(Val::Undefined),
        Val::Undefined | Val::Null => Err(JsError::Throw(Val::Str(Rc::from(format!(
            "TypeError: Cannot read property '{}' of {}{}",
            key,
            base.type_of(),
            e.stack_hint()
        ))))),
        Val::Str(s) => {
            if key == "length" {
                return Ok(Val::Num(s.chars().count() as f64));
            }
            if let Ok(i) = key.parse::<usize>() {
                return Ok(match s.chars().nth(i) {
                    Some(c) => Val::Str(Rc::from(c.to_string())),
                    None => Val::Undefined,
                });
            }
            // string method?
            use Meth::*;
            let m = match key {
                "toUpperCase" => Some(StrToUpper),
                "toLowerCase" => Some(StrToLower),
                "trim" => Some(StrTrim),
                "trimStart" | "trimLeft" => Some(StrTrimStart),
                "trimEnd" | "trimRight" => Some(StrTrimEnd),
                "slice" => Some(StrSlice),
                "substring" => Some(StrSlice), // shortcut: maps to slice (clamping differs)
                "indexOf" => Some(StrIndexOf),
                "lastIndexOf" => Some(StrLastIndexOf),
                "includes" => Some(StrIncludes),
                "split" => Some(StrSplit),
                "replace" => Some(StrReplace),
                "match" => Some(StrMatch),
                "search" => Some(StrSearch),
                "matchAll" => Some(StrMatchAll),
                "startsWith" => Some(StrStartsWith),
                "endsWith" => Some(StrEndsWith),
                "repeat" => Some(StrRepeat),
                "padStart" => Some(StrPadStart),
                "padEnd" => Some(StrPadEnd),
                "charAt" => Some(StrCharAt),
                "charCodeAt" => Some(StrCharCodeAt),
                "toString" | "valueOf" => Some(StrToString),
                _ => None,
            };
            Ok(m.map(Val::Method).unwrap_or(Val::Undefined))
        }
        Val::Num(_) => {
            use Meth::*;
            let m = match key {
                "toString" | "valueOf" => Some(NumToString),
                "toFixed" => Some(NumToFixed),
                _ => None,
            };
            Ok(m.map(Val::Method).unwrap_or(Val::Undefined))
        }
        Val::Bool(_) => {
            let m = if key == "toString" || key == "valueOf" {
                Some(Meth::NumToString) // dispatches on this type
            } else {
                None
            };
            Ok(m.map(Val::Method).unwrap_or(Val::Undefined))
        }
        Val::Func(c) => {
            if let Some(v) = c.props.borrow().iter().find(|(k, _)| k.as_ref() == key) {
                return match &v.1 {
                    // static accessor (`static get x()`) on the class function
                    Val::Acc(acc) => {
                        let g = acc.getter.clone().unwrap_or(Val::Undefined);
                        if is_callable(&g) {
                            e.call(g, base.clone(), Vec::new())
                        } else {
                            Ok(Val::Undefined)
                        }
                    }
                    v => Ok(v.clone()),
                };
            }
            match key {
                // lazy prototype: reading `.prototype` materializes it so
                // `Foo.prototype.m = ...` assignment works before any `new`
                "prototype" => Ok(Val::Obj(interp::ensure_prototype(c))),
                "call" => Ok(Val::Method(Meth::FnCall)),
                "apply" => Ok(Val::Method(Meth::FnApply)),
                "bind" => Ok(Val::Method(Meth::FnBind)),
                "name" => Ok(Val::Str(c.def.name.clone())),
                "length" => Ok(Val::Num(c.def.params.len() as f64)),
                _ => {
                    // class static members inherited through the function's
                    // own [[Prototype]] (`class B extends A` links B -> A)
                    let mut cur = c.proto.borrow().clone();
                    for _ in 0..100 {
                        let p = match cur {
                            None => break,
                            Some(p) => p,
                        };
                        cur = match &p {
                            Val::Func(f) => {
                                let found = f
                                    .props
                                    .borrow()
                                    .iter()
                                    .find(|(k, _)| k.as_ref() == key)
                                    .map(|(_, v)| v.clone());
                                if let Some(v) = found {
                                    return match &v {
                                        Val::Acc(acc) => {
                                            let g =
                                                acc.getter.clone().unwrap_or(Val::Undefined);
                                            if is_callable(&g) {
                                                e.call(g, base.clone(), Vec::new())
                                            } else {
                                                Ok(Val::Undefined)
                                            }
                                        }
                                        v => Ok(v.clone()),
                                    };
                                }
                                f.proto.borrow().clone()
                            }
                            _ => None,
                        };
                    }
                    // functions inherit from Function.prototype (which chains
                    // to Object.prototype), so `fn.hasOwnProperty(...)` and
                    // borrowed `Function.prototype.*` methods resolve
                    if let Some(fp) = e.function_proto.clone() {
                        if let Some(v) = chain_get(e, base, fp, key)? {
                            return Ok(v);
                        }
                    }
                    Ok(Val::Undefined)
                }
            }
        }
        Val::Native(id) => {
            // `import.meta` (dynamic-import function): build the object at
            // read time so `.url` names the module whose code is running —
            // or the page URL for a classic script. Only `.url` exists
            // (no `resolve()`); honest shortcut.
            if Some(*id) == e.import_native && key == "meta" {
                let url = e
                    .module_url_for(e.cur_src)
                    .or_else(|| e.import_base.clone())
                    .unwrap_or_default();
                let meta = Val::new_object();
                meta.set("url", Val::Str(url));
                return Ok(meta);
            }
            match key {
                "call" => Ok(Val::Method(Meth::FnCall)),
                "apply" => Ok(Val::Method(Meth::FnApply)),
                "bind" => Ok(Val::Method(Meth::FnBind)),
                "now" => Ok(Val::Method(Meth::DateNow)), // Date.now
                _ => {
                    // own props of native constructors (`URL.prototype`, …)
                    if let Some(v) = e.native_prop(*id, key) {
                        return Ok(v);
                    }
                    // Promise statics hang off the constructor value
                    // (`Promise.resolve(5)` etc.) via a small engine-side
                    // table instead of per-native property storage
                    if let Some(st) = &e.promise_statics {
                        let found = st
                            .borrow()
                            .props
                            .iter()
                            .find(|(k, _)| k.as_ref() == key)
                            .map(|(_, v)| v.clone());
                        if let Some(v) = found {
                            return Ok(v);
                        }
                    }
                    // built-in constructors are functions: anything not on
                    // the native side table is inherited from
                    // Function.prototype (`Function.toString`, …)
                    if let Some(fp) = e.function_proto.clone() {
                        if let Some(v) = chain_get(e, base, fp, key)? {
                            return Ok(v);
                        }
                    }
                    Ok(Val::Undefined)
                }
            }
        }
        // A method value is callable, so `Function.prototype.call.call(f, t)`
        // — the `uncurryThis` idiom every polyfill bundle is built on — must
        // find `.call`/`.apply`/`.bind` on it too.
        Val::Method(_) => Ok(match key {
            "call" => Val::Method(Meth::FnCall),
            "apply" => Val::Method(Meth::FnApply),
            "bind" => Val::Method(Meth::FnBind),
            _ => Val::Undefined,
        }),
        Val::Obj(o) => {
            // lazy prototype attach: engine-created arrays (literals,
            // builtins, embedder-made) pick up Array.prototype on first
            // property access, so they never pay for it up front. Plain
            // objects pick up Object.prototype the same way.
            {
                let mut b = o.borrow_mut();
                if b.proto.is_none() {
                    if matches!(b.kind, ObjKind::Array(_)) {
                        if let Some(p) = &e.array_proto {
                            b.proto = Some(p.clone());
                        }
                    } else if let Some(p) = &e.object_proto {
                        // the prototype object itself never points at itself
                        if !Rc::ptr_eq(p, o) {
                            b.proto = Some(p.clone());
                        }
                    }
                }
            }
            // CSSStyleDeclaration stand-in: own props (method natives,
            // `__node`) win; everything else reads the element's `style`
            // attribute through the embedder hook. No prototype walk —
            // `style.width` must not accidentally resolve to Object.prototype.
            let style_node = o
                .try_borrow()
                .ok()
                .and_then(|b| match &b.kind {
                    ObjKind::Style(n) => Some(*n),
                    _ => None,
                });
            if let Some(node) = style_node {
                let own = o
                    .borrow()
                    .props
                    .iter()
                    .find(|(k, _)| k.as_ref() == key)
                    .map(|(_, v)| v.clone());
                if let Some(v) = own {
                    return Ok(v);
                }
                return Ok(match e.style_get {
                    Some(f) => Val::Str(Rc::from(f(node, key).as_str())),
                    None => Val::Undefined,
                });
            }
            {
                let b = o.borrow();
                if let ObjKind::Error { name, msg } = &b.kind {
                    // error objects expose .name/.message as own props
                    match key {
                        "name" => return Ok(Val::Str(name.clone())),
                        "message" => return Ok(Val::Str(msg.clone())),
                        // shortcut: no captured frames — the "stack" is just
                        // the head line a log/overlay would print
                        "stack" => {
                            return Ok(Val::Str(Rc::from(format!("{name}: {msg}").as_str())))
                        }
                        _ => {}
                    }
                }
                if let ObjKind::Date(_) = b.kind {
                    use Meth::*;
                    let m = match key {
                        "getTime" | "valueOf" => Some(DateGetTime),
                        "toISOString" => Some(DateToISO),
                        _ => None,
                    };
                    if let Some(m) = m {
                        return Ok(Val::Method(m));
                    }
                }
                if let ObjKind::Regex(_) = b.kind {
                    return regex_member(e, o, key);
                }
                if let ObjKind::Generator(_) = b.kind {
                    return match key {
                        "next" => Ok(Val::Method(Meth::GenNext)),
                        "return" => Ok(Val::Method(Meth::GenReturn)),
                        // honest gap: throwing into a suspend point (`.throw`,
                        // incl. try/finally interactions around yield) isn't
                        // supported in this wave
                        "throw" => Err(JsError::throw(
                            "TypeError: generator.throw is not supported",
                        )),
                        _ => Ok(Val::Undefined),
                    };
                }
                if let ObjKind::Promise(_) = b.kind {
                    match key {
                        "then" => return Ok(Val::Method(Meth::PromiseThen)),
                        "catch" => return Ok(Val::Method(Meth::PromiseCatch)),
                        "finally" => return Ok(Val::Method(Meth::PromiseFinally)),
                        _ => {}
                    }
                }
            }
            // prototype walk with accessor interception: an own OR inherited
            // `Val::Acc` property invokes its getter (receiver = `base`)
            Ok(chain_get(e, base, o.clone(), key)?.unwrap_or(Val::Undefined))
        }
    }
}

/// Walk `start`'s prototype chain for `key`, invoking accessor getters with
/// `base` as receiver. `None` means the key is absent everywhere.
fn chain_get(
    e: &mut JsEngine,
    base: &Val,
    start: Rc<RefCell<Obj>>,
    key: &str,
) -> Result<Option<Val>, JsError> {
    let mut cur = Some(start);
    for _ in 0..PROTO_WALK_CAP {
        let c = match cur {
            None => break,
            Some(c) => c,
        };
        let (found, next) = {
            let b = c.borrow();
            if let ObjKind::Array(items) = &b.kind {
                if key == "length" {
                    return Ok(Some(Val::Num(items.len() as f64)));
                }
                if let Ok(i) = key.parse::<usize>() {
                    if i < items.len() {
                        return Ok(Some(items[i].clone()));
                    }
                }
            }
            (
                b.props
                    .iter()
                    .find(|(k, _)| k.as_ref() == key)
                    .map(|(_, v)| v.clone()),
                b.proto.clone(),
            )
        };
        cur = next;
        if let Some(v) = found {
            if let Val::Acc(acc) = &v {
                let g = acc.getter.clone().unwrap_or(Val::Undefined);
                if is_callable(&g) {
                    return Ok(Some(e.call(g, base.clone(), Vec::new())?));
                }
                return Ok(Some(Val::Undefined)); // getter-less accessor
            }
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// member m.resolve on a `ObjKind::Regex` value (see `get_member`)
fn regex_member(_e: &JsEngine, o: &Rc<RefCell<Obj>>, key: &str) -> Result<Val, JsError> {
    let b = o.borrow();
    let ri = match &b.kind {
        ObjKind::Regex(ri) => ri,
        _ => return Ok(Val::Undefined),
    };
    match key {
        "source" => Ok(Val::Str(Rc::from(ri.re.src.as_str()))),
        "flags" => Ok(Val::Str(Rc::from(ri.re.flags.as_str()))),
        "global" => Ok(Val::Bool(ri.re.global)),
        "sticky" => Ok(Val::Bool(ri.re.sticky)),
        "ignoreCase" => Ok(Val::Bool(ri.re.ignore_case)),
        "multiline" => Ok(Val::Bool(ri.re.multiline)),
        "dotAll" => Ok(Val::Bool(ri.re.dot_all)),
        "lastIndex" => Ok(Val::Num(ri.last_index.get())),
        "test" => Ok(Val::Method(Meth::RegexTest)),
        "exec" => Ok(Val::Method(Meth::RegexExec)),
        "toString" => Ok(Val::Method(Meth::RegexToString)),
        _ => Ok(Val::Undefined),
    }
}

/// Full member write: accessor-aware (own or inherited `Val::Acc` invokes
/// its setter with `base` as receiver), regex `lastIndex` routed to its Cell.
pub(crate) fn set_member(e: &mut JsEngine, base: &Val, key: &str, v: Val) -> Result<(), JsError> {
    match base {
        Val::Obj(o) => {
            // CSSStyleDeclaration stand-in: every write is a declaration
            // update on the element's `style` attribute (empty value removes
            // the declaration; `cssText` replaces the whole text).
            let style_node = o
                .try_borrow()
                .ok()
                .and_then(|b| match &b.kind {
                    ObjKind::Style(n) => Some(*n),
                    _ => None,
                });
            if let Some(node) = style_node {
                if let Some(f) = e.style_set {
                    f(node, key, &v.to_string());
                }
                return Ok(());
            }
            // regex instances: `lastIndex` writes to the per-instance Cell
            if let Ok(b) = o.try_borrow() {
                if let ObjKind::Regex(ri) = &b.kind {
                    if key == "lastIndex" {
                        ri.last_index.set(v.to_number());
                        return Ok(());
                    }
                }
            }
            // accessor interception walk
            let mut cur = Some(o.clone());
            for _ in 0..PROTO_WALK_CAP {
                let c = match cur {
                    None => break,
                    Some(c) => c,
                };
                let (existing, next) = {
                    let b = c.borrow();
                    (
                        b.props
                            .iter()
                            .find(|(k, _)| k.as_ref() == key)
                            .map(|(_, x)| x.clone()),
                        b.proto.clone(),
                    )
                };
                cur = next;
                if let Some(ex) = existing {
                    if let Val::Acc(acc) = &ex {
                        let s = acc.setter.clone().unwrap_or(Val::Undefined);
                        if is_callable(&s) {
                            e.call(s, base.clone(), vec![v])?;
                        }
                        // setter absent: sloppy-mode silent no-op read-only
                        return Ok(());
                    }
                    break; // ordinary property found down the chain: shadow own
                }
            }
            obj_set_own(o, key, v);
            Ok(())
        }
        // sloppy-mode silent no-op on primitives (non-strict JS behavior)
        Val::Str(_) | Val::Num(_) | Val::Bool(_) => Ok(()),
        Val::Func(c) => {
            // static accessors: a setter may live on the function itself or
            // be inherited through its [[Prototype]] (class `extends`)
            let mut cur = Some(base.clone());
            for _ in 0..PROTO_WALK_CAP {
                let f = match cur {
                    Some(Val::Func(f)) => f,
                    _ => break,
                };
                let found = f
                    .props
                    .borrow()
                    .iter()
                    .find(|(k, _)| k.as_ref() == key)
                    .map(|(_, x)| x.clone());
                cur = f.proto.borrow().clone();
                if let Some(ex) = found {
                    if let Val::Acc(acc) = &ex {
                        let s = acc.setter.clone().unwrap_or(Val::Undefined);
                        if is_callable(&s) {
                            e.call(s, base.clone(), vec![v.clone()])?;
                        }
                        return Ok(());
                    }
                    break;
                }
            }
            let mut ps = c.props.borrow_mut();
            match ps.iter_mut().find(|(k, _)| k.as_ref() == key) {
                Some((_, slot)) => *slot = v,
                None => ps.push((Rc::from(key), v)),
            }
            Ok(())
        }
        Val::Undefined | Val::Null => Err(JsError::Throw(Val::Str(Rc::from(format!(
            "TypeError: Cannot set property '{}' of {}",
            key,
            base.type_of()
        ))))),
        _ => Ok(()),
    }
}

pub(crate) fn delete_member(base: &Val, key: &str) -> bool {
    match base {
        Val::Obj(o) => {
            let mut b = o.borrow_mut();
            let before = b.props.len();
            b.props.retain(|(k, _)| k.as_ref() != key);
            b.props.len() != before
        }
        _ => false, // primitives: silent false (sloppy)
    }
}

/// JS `ToPrimitive` (default hint): what `+`/`==` do with objects â€” objects
/// collapse to their string form (arrays join, errors "Name: msg", plain
/// objects "[object Object]").
fn to_primitive(v: &Val) -> Val {
    match v {
        Val::Obj(_) | Val::Func(_) => Val::Str(Rc::from(v.to_string())),
        _ => v.clone(),
    }
}

/// Loose equality (`==`) with the common coercions.
pub(crate) fn loose_eq(l: &Val, r: &Val) -> bool {
    use Val::*;
    match (l, r) {
        (Undefined, Null) | (Null, Undefined) => true,
        (Null, Null) | (Undefined, Undefined) => true,
        (Num(a), Num(b)) => a == b,
        (Str(a), Str(b)) => a == b,
        (Bool(a), Bool(b)) => a == b,
        (Num(_), Str(_)) | (Str(_), Num(_)) => l.to_number() == r.to_number(),
        (Bool(_), _) => loose_eq(&Val::Num(l.to_number()), r),
        (_, Bool(_)) => loose_eq(l, &Val::Num(r.to_number())),
        (Obj(_), Obj(_)) => strict_eq(l, r) || loose_eq(&to_primitive(l), r),
        (Obj(_), _) => loose_eq(&to_primitive(l), r),
        (_, Obj(_)) => loose_eq(l, &to_primitive(r)),
        _ => false,
    }
}

pub(crate) fn strict_eq(l: &Val, r: &Val) -> bool {
    use Val::*;
    match (l, r) {
        (Num(a), Num(b)) => a == b && !(a.is_nan() && b.is_nan()),
        (Str(a), Str(b)) => a == b,
        (Bool(a), Bool(b)) => a == b,
        (Obj(a), Obj(b)) => Rc::ptr_eq(a, b),
        (Func(a), Func(b)) => Rc::ptr_eq(a, b),
        (Native(a), Native(b)) => a == b,
        (Method(a), Method(b)) => a == b,
        (Acc(a), Acc(b)) => Rc::ptr_eq(a, b),
        (Undefined, Undefined) | (Null, Null) => true,
        _ => false,
    }
}

/// ToInt32 (bitwise ops). shortcut: values beyond i64 range saturate before
/// wrapping, which differs from the spec's modulo-2^32 only for |x| > 2^63.
pub(crate) fn to_int32(x: f64) -> i32 {
    if !x.is_finite() {
        return 0;
    }
    x.trunc() as i64 as u32 as i32
}

pub(crate) fn to_uint32(x: f64) -> u32 {
    if !x.is_finite() {
        return 0;
    }
    x.trunc() as i64 as u32
}
