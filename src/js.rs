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
mod parser;
mod regex;

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
    StrIncludes,
    StrSplit,
    StrReplace,
    StrStartsWith,
    StrEndsWith,
    StrRepeat,
    StrPadStart,
    StrPadEnd,
    StrCharAt,
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
            Val::Obj(_) => "object",
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
            Val::Obj(_) => str_to_number(&self.to_string_d(0)),
            _ => f64::NAN,
        }
    }

    /// JS `ToString`. shortcut: exponential formatting for very large/small
    /// numbers differs from JS's (`1e+21`); every-day values match.
    pub fn to_string(&self) -> String {
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
                        if b.props.is_empty() {
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
}

impl std::fmt::Display for JsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsError::Throw(v) => write!(f, "Uncaught {}", v.to_string()),
            JsError::Syntax { msg, line, col } => {
                write!(f, "SyntaxError: {} (line {}, col {})", msg, line, col)
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

/// A pending `setTimeout` job. The interpreter never runs timers itself â€”
/// it only queues them; the embedder decides threading and drains the queue
/// via `drain_timers`, invoking callbacks through `call_function`.
pub struct TimerTask {
    pub delay_ms: f64,
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
    pub(crate) natives: Vec<NativeEntry>,
    pub(crate) natives_by_name: HashMap<Rc<str>, usize>,
    pub(crate) call_depth: usize,
    pub(crate) timers: Vec<TimerTask>,
    pub(crate) microtasks: Vec<Job>,
    /// deferred `new Promise` resolver slots: slot index -> the promise the
    /// registered `resolve`/`reject` natives settle. The slot is emptied on
    /// settle, so the engine holds no reference afterwards (script-level
    /// `Rc`s are the only refs from then on â€” normal refcount release).
    pub(crate) promise_slots: Vec<Option<Rc<RefCell<Obj>>>>,
    /// Promise.resolve/reject/... statics installed by builtins; reachable
    /// through member lookup on the constructor native
    pub(crate) promise_statics: Option<Rc<RefCell<Obj>>>,
    /// compiled-once regex literal cache, keyed by (pattern, flags). Regex
    /// literals are program text, so this cache is bounded by the number of
    /// distinct (pattern, flags) literals the page can create; still capped
    /// at `REGEX_CACHE_MAX` (RAM honesty) â€” beyond the cap compiles are
    /// per-eval (correct, just not cached).
    pub(crate) regex_cache: HashMap<(Rc<str>, Rc<str>), Rc<Regex>>,
    pub(crate) next_timer_id: f64,
    rng_state: u64,
}

pub(crate) const REGEX_CACHE_MAX: usize = 512;

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
            natives: Vec::new(),
            natives_by_name: HashMap::new(),
            call_depth: 0,
            timers: Vec::new(),
            microtasks: Vec::new(),
            promise_slots: Vec::new(),
            promise_statics: None,
            regex_cache: HashMap::new(),
            next_timer_id: 1.0,
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
            msg: format!("Invalid regular expression: {}", re.msg),
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
        let program: Program = parser::parse(source)?;
        self.call_depth = 0;
        let env = self.global_env.clone();
        interp::hoist(self, &env, &program);
        let mut last = Val::Undefined;
        for st in program.iter() {
            match &st.kind {
                parser::StmtKind::Expr(e) => {
                    last = interp::eval_expr(self, &env, e)?;
                }
                _ => {
                    interp::eval_stmt(self, &env, st)?;
                }
            }
        }
        // microtask quiescence at the end of every execution turn (idempotent
        // with the embedder's own drain later)
        self.run_microtasks(MAX_MICROTASKS)?;
        Ok(last)
    }

    /// Invoke a JS function value from the embedder (event handlers, timer
    /// callbacks, later: DOM callbacks). `this` is the method receiver
    /// (pass `Val::Undefined` for the sloppy-mode global).
    pub fn call_function(&mut self, func: &Val, this: Val, args: &[Val]) -> Result<Val, JsError> {
        self.call(func.clone(), this, args.to_vec())
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
        let r = self.call_inner(callee, this, args);
        self.call_depth -= 1;
        r
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
                    let p = new_promise_obj();
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
            other => Err(JsError::Throw(Val::Str(Rc::from(format!(
                "TypeError: {} is not a function",
                other.type_of()
            ))))),
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
    let ret = new_promise_obj();
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
    let p = new_promise_obj();
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

/// run one queued reaction (see `Job::Reaction`)
fn exec_job(e: &mut JsEngine, job: Job) -> Result<(), JsError> {
    let Job::Reaction(re, out) = job;
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
        Some((_, slot)) => *slot = v,
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
    match base {
        // accessor values never reach script land as plain member values
        // (get_member unwraps them); unreachable in practice
        Val::Acc(_) => Ok(Val::Undefined),
        Val::Undefined | Val::Null => Err(JsError::Throw(Val::Str(Rc::from(format!(
            "TypeError: Cannot read property '{}' of {}",
            key,
            base.type_of()
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
                return Ok(v.1.clone());
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
                _ => Ok(Val::Undefined),
            }
        }
        Val::Native(_) => {
            match key {
                "call" => Ok(Val::Method(Meth::FnCall)),
                "apply" => Ok(Val::Method(Meth::FnApply)),
                "bind" => Ok(Val::Method(Meth::FnBind)),
                "now" => Ok(Val::Method(Meth::DateNow)), // Date.now
                _ => {
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
                    Ok(Val::Undefined)
                }
            }
        }
        Val::Method(_) => Ok(Val::Undefined),
        Val::Obj(o) => {
            // lazy prototype attach: engine-created arrays (literals,
            // builtins, embedder-made) pick up Array.prototype on first
            // property access, so they never pay for it up front.
            if matches!(o.borrow().kind, ObjKind::Array(_)) && o.borrow().proto.is_none() {
                if let Some(p) = &e.array_proto {
                    o.borrow_mut().proto = Some(p.clone());
                }
            }
            {
                let b = o.borrow();
                if let ObjKind::Error { name, msg } = &b.kind {
                    // error objects expose .name/.message as own props
                    match key {
                        "name" => return Ok(Val::Str(name.clone())),
                        "message" => return Ok(Val::Str(msg.clone())),
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
            let mut cur = Some(o.clone());
            for _ in 0..PROTO_WALK_CAP {
                let c = match cur {
                    None => break,
                    Some(c) => c,
                };
                let (found, next) = {
                    let b = c.borrow();
                    if let ObjKind::Array(items) = &b.kind {
                        if key == "length" {
                            return Ok(Val::Num(items.len() as f64));
                        }
                        if let Ok(i) = key.parse::<usize>() {
                            if i < items.len() {
                                return Ok(items[i].clone());
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
                            return e.call(g, base.clone(), Vec::new());
                        }
                        return Ok(Val::Undefined); // getter-less accessor
                    }
                    return Ok(v);
                }
            }
            Ok(Val::Undefined)
        }
    }
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
        Val::Obj(_) | Val::Func(_) => Val::Str(Rc::from(v.to_string_d(0))),
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
