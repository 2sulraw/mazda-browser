//! JavaScript interpreter core — a from-scratch tree-walking evaluator for a
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
//!   insertion-ordered property `Vec` (linear scan — fine at this stage;
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
//! Async/await runs to completion (see `await_promise` — an `await` of a
//! promise settled only by a timer/macrotask errors honestly instead of
//! suspending the interpreter stack).
#![allow(dead_code)] // the embedder hasn't wired this module up yet

mod builtins;
pub(crate) use builtins::perf_now_ms;
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
    /// `123n`: i128-backed BigInt (documented range; operations that leave
    /// i128 throw a RangeError instead of silently losing precision)
    BigInt(Rc<i128>),
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
    StrConcat,
    StrSubstr,
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
    DateParse,
    /// `getFullYear`/`getUTCFullYear` and the rest of the local-time getters.
    /// shortcut: no timezone database, so local time IS UTC (same as
    /// `date_parts`), which is why the UTC aliases share these.
    DateGetYear,
    DateGetMonth,
    DateGetDate,
    /// `getDay`/`getUTCDay` - the weekday index
    DateGetWeekday,
    DateGetHours,
    DateGetMinutes,
    DateGetSeconds,
    DateGetMillis,
    /// `toLocaleDateString` / `toLocaleTimeString` / `toLocaleString`
    DateLocaleDate,
    DateLocaleTime,
    DateLocaleBoth,
    // promise methods
    PromiseThen,
    PromiseCatch,
    PromiseFinally,
    // generator methods (this must be a generator object)
    GenNext,
    GenReturn,
    // built-in iterator methods (this must be an Iterator object)
    IterNext,
    IterSelf,
    IterReturn,
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
    /// Own properties in insertion order. shortcut: linear scan on lookup -
    /// O(n) per access; fine for typical script objects (tens of props), to
    /// be revisited with a real shape/map if profiles ever demand it.
    pub props: Vec<(Rc<str>, Val)>,
    pub proto: Option<Rc<RefCell<Obj>>>,
}

/// `name`/`message` of an error object, shared behind one [`std::rc::Rc`] so
/// [`ObjKind::Error`] costs a single pointer instead of two fat string
/// pointers. Errors are immutable after construction, so sharing is safe.
#[derive(Debug)]
pub struct ErrorData {
    pub name: Rc<str>,
    pub msg: Rc<str>,
}

#[derive(Debug)]
pub enum ObjKind {
    /// `Some` carries own keys that are non-enumerable (class methods and
    /// accessors, the implicit `constructor` back-reference). Kept inside
    /// the existing variant so `Obj` itself does not grow. The flag marks an
    /// explicitly null [[Prototype]] (`Object.create(null)`,
    /// `{__proto__: null}`); `Obj.proto == None` alone cannot say so, because
    /// `None` also means "the default prototype attaches lazily on access".
    Plain(Option<Box<Vec<Rc<str>>>>, bool),
    /// boxed primitive: `new String("x")`, `new Number(1)`, `new Boolean(true)`
    /// (and `Object(prim)`). `Val::to_string`/`to_number` unwrap it, and
    /// String boxes answer `length`/index reads. Methods live on the ordinary
    /// prototype (`String.prototype` etc.), so `call_method` unwraps `this`.
    Boxed(Val),
    /// array exotic: dense element storage; `length` and integer keys map
    /// onto it, everything else goes through `props`.
    Array(Vec<Val>),
    /// error data behind one `Rc`: packed so the enum (and therefore every
    /// [`Obj`]) stays small. `Rc<ErrorData>` is one pointer instead of two
    /// fat `Rc<str>` pointers.
    Error(Rc<ErrorData>),
    /// milliseconds since epoch (NaN = invalid date)
    Date(f64),
    /// Boxed: `PromiseState` is 56 B and promises are far rarer than plain
    /// objects, so keeping it out of line shrinks `ObjKind` for every object.
    Promise(Box<PromiseState>),
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
    /// live ES-module namespace: every read consults the module's export map
    /// at access time. A snapshot taken while the module is still evaluating
    /// (an entry chunk that imports itself — rspack/webpack emit exactly
    /// that) must see the exports its body records later.
    ModuleNs(Rc<RefCell<std::collections::HashMap<Rc<str>, Val>>>),
    /// live iterator object (`array.values()`, `set.entries()`, a user
    /// `Symbol.iterator` result is a plain object with `next`; this is the
    /// built-in fast path). `next()` reads the CURRENT source by index, so
    /// mutation during iteration follows the spec's live-view behavior.
    /// Boxed: `IterState` is 40 B and iterators are far rarer than plain
    /// objects, so it stays out of the `ObjKind` size budget.
    Iterator(Box<IterState>),
    /// real byte buffer shared by every view over it
    ArrayBuffer(Rc<RefCell<Vec<u8>>>),
    /// fixed-width view into an ArrayBuffer. Boxed for the same reason as
    /// [`ObjKind::Iterator`] / [`ObjKind::Promise`].
    TypedArray(Box<TypedArray>),
    /// live host-backed collection (`getElementsBy*`, `document.forms`,
    /// `form.elements`): index/`length`/named reads re-query the host at
    /// access time through `JsEngine::live_get`, so DOM mutations that
    /// happened after the collection was returned are visible. Own props
    /// carry `item`/`namedItem`; `JsEngine::live_values` snapshots it for
    /// iteration. This module stays DOM-free - the spec only names the
    /// query, the embedder owns its meaning.
    Live(LiveSpec),
}

/// Query behind an [`ObjKind::Live`] collection. `family`/`root`/`arg` are
/// opaque to the engine and interpreted by the embedder hook.
#[derive(Debug, Clone)]
pub struct LiveSpec {
    /// embedder-defined collection kind (tag / class / name / forms / ...)
    pub family: u8,
    /// scope node id; 0 = whole document
    pub root: u32,
    /// query argument (tag name, class list, name, ...)
    pub arg: Rc<str>,
}

/// Which built-in iterator this is (keys/values/entries).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IterKind {
    Keys,
    Values,
    Entries,
}

#[derive(Debug)]
pub struct IterState {
    /// array / typed array / Set / Map / string being walked
    pub source: Val,
    pub kind: IterKind,
    pub index: usize,
    pub done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TypedKind {
    I8,
    U8,
    U8Clamp,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
    BigI64,
    BigU64,
}

impl TypedKind {
    pub fn size(self) -> usize {
        match self {
            TypedKind::I8 | TypedKind::U8 | TypedKind::U8Clamp => 1,
            TypedKind::I16 | TypedKind::U16 => 2,
            TypedKind::I32 | TypedKind::U32 | TypedKind::F32 => 4,
            TypedKind::F64 | TypedKind::BigI64 | TypedKind::BigU64 => 8,
        }
    }
    pub fn is_big(self) -> bool {
        matches!(self, TypedKind::BigI64 | TypedKind::BigU64)
    }
    pub fn from_index(i: u16) -> Option<TypedKind> {
        Some(match i {
            0 => TypedKind::I8,
            1 => TypedKind::U8,
            2 => TypedKind::U8Clamp,
            3 => TypedKind::I16,
            4 => TypedKind::U16,
            5 => TypedKind::I32,
            6 => TypedKind::U32,
            7 => TypedKind::F32,
            8 => TypedKind::F64,
            9 => TypedKind::BigI64,
            10 => TypedKind::BigU64,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub struct TypedArray {
    pub kind: TypedKind,
    pub buf: Rc<RefCell<Obj>>,
    /// byte offset into the buffer
    pub offset: usize,
    /// element count
    pub len: usize,
}

pub(crate) const SYMBOL_ITERATOR: &str = "Symbol(iterator)";

impl TypedArray {
    /// Element count (the `.length` value).
    pub fn length(&self) -> usize {
        self.len
    }
}

/// Read element `i` (bounds-checked by the caller) as a JS value.
pub(crate) fn typed_read(ta: &TypedArray, i: usize) -> Val {
    if i >= ta.len {
        return Val::Undefined;
    }
    let size = ta.kind.size();
    let at = ta.offset + i * size;
    let r = ta.buf.borrow();
    let bcell = match &r.kind {
        ObjKind::ArrayBuffer(b) => b,
        _ => return Val::Undefined,
    };
    let bytes = bcell.borrow();
    if at + size > bytes.len() {
        return Val::Undefined;
    }
    let b = &bytes[at..at + size];
    match ta.kind {
        TypedKind::I8 => Val::Num(b[0] as i8 as f64),
        TypedKind::U8 | TypedKind::U8Clamp => Val::Num(b[0] as f64),
        TypedKind::I16 => Val::Num(i16::from_le_bytes([b[0], b[1]]) as f64),
        TypedKind::U16 => Val::Num(u16::from_le_bytes([b[0], b[1]]) as f64),
        TypedKind::I32 => Val::Num(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64),
        TypedKind::U32 => Val::Num(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64),
        TypedKind::F32 => Val::Num(f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64),
        TypedKind::F64 => Val::Num(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ])),
        TypedKind::BigI64 => Val::BigInt(Rc::new(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]) as i128)),
        TypedKind::BigU64 => Val::BigInt(Rc::new(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]) as i128)),
    }
}

/// Write element `i` with the spec's conversion for the element type.
pub(crate) fn typed_write(ta: &TypedArray, i: usize, v: &Val) {
    if i >= ta.len {
        return;
    }
    let size = ta.kind.size();
    let at = ta.offset + i * size;
    let r = ta.buf.borrow();
    let bcell = match &r.kind {
        ObjKind::ArrayBuffer(b) => b,
        _ => return,
    };
    let mut bytes = bcell.borrow_mut();
    if at + size > bytes.len() {
        return;
    }
    let n = v.to_number();
    let big = |v: &Val| -> i128 {
        match v {
            Val::BigInt(n) => **n,
            _ => {
                let n = v.to_number();
                if n.is_finite() {
                    n.trunc() as i128
                } else {
                    0
                }
            }
        }
    };
    match ta.kind {
        TypedKind::I8 => bytes[at] = (n as i64 as i8) as u8,
        TypedKind::U8 => bytes[at] = n as i64 as u8,
        TypedKind::U8Clamp => {
            // ToUint8Clamp: round half to even, clamp 0..255
            let c = if n.is_nan() {
                0.0
            } else {
                let f = n.floor();
                let diff = n - f;
                let r = if diff < 0.5 {
                    f
                } else if diff > 0.5 {
                    f + 1.0
                } else if (f as i64) % 2 == 0 {
                    f
                } else {
                    f + 1.0
                };
                r.clamp(0.0, 255.0)
            };
            bytes[at] = c as u8;
        }
        TypedKind::I16 => bytes[at..at + 2].copy_from_slice(&(n as i64 as i16).to_le_bytes()),
        TypedKind::U16 => bytes[at..at + 2].copy_from_slice(&(n as i64 as u16).to_le_bytes()),
        TypedKind::I32 => bytes[at..at + 4].copy_from_slice(&(n as i64 as i32).to_le_bytes()),
        TypedKind::U32 => bytes[at..at + 4].copy_from_slice(&(n as i64 as u32).to_le_bytes()),
        TypedKind::F32 => bytes[at..at + 4].copy_from_slice(&(n as f32).to_le_bytes()),
        TypedKind::F64 => bytes[at..at + 8].copy_from_slice(&n.to_le_bytes()),
        TypedKind::BigI64 => {
            let x = big(v) as u64;
            bytes[at..at + 8].copy_from_slice(&x.to_le_bytes());
        }
        TypedKind::BigU64 => {
            let x = big(v) as u64;
            bytes[at..at + 8].copy_from_slice(&x.to_le_bytes());
        }
    }
}

/// Join a typed array's elements (its `toString`).
fn typed_join(ta: &TypedArray) -> String {
    (0..ta.len)
        .map(|i| typed_read(ta, i).to_string())
        .collect::<Vec<_>>()
        .join(",")
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
    /// lexical `this`, captured by arrow functions at creation. Boxed and
    /// optional: normal functions get `this` from the call site, so only
    /// arrows pay for the payload. `None` reads as `undefined`.
    pub this: Option<Box<Val>>,
    pub is_arrow: bool,
    pub props: RefCell<Vec<(Rc<str>, Val)>>,
    /// Rare function-object metadata: the `extends` [[Prototype]] chain and
    /// own non-enumerable keys (a class's static methods). Combined behind one
    /// pointer so the common closure pays a single slot, not two.
    pub meta: RefCell<Option<Box<ClosureMeta>>>,
}

/// See [`Closure::meta`].
#[derive(Debug, Default)]
pub struct ClosureMeta {
    /// [[Prototype]] of the function object itself (class `extends` links a
    /// subclass constructor to its superclass). `None` inherits from
    /// `Function.prototype`.
    pub proto: Option<Val>,
    /// Own keys that are non-enumerable (static class methods/accessors).
    pub hidden: Vec<Rc<str>>,
    /// Synthesized own keys that were deleted (`delete fn.name`). Bit 0 is
    /// `name`, bit 1 is `length`: the slots are normally materialized on read,
    /// so a later descriptor/hasOwn must remember they were removed.
    pub deleted: u8,
}

impl Closure {
    /// [[Prototype]] of the function object itself.
    pub(crate) fn proto(&self) -> Option<Val> {
        self.meta.borrow().as_ref().and_then(|m| m.proto.clone())
    }

    pub(crate) fn set_proto(&self, p: Option<Val>) {
        let mut meta = self.meta.borrow_mut();
        match meta.as_mut() {
            Some(m) => m.proto = p,
            None if p.is_some() => {
                *meta = Some(Box::new(ClosureMeta {
                    proto: p,
                    hidden: Vec::new(),
                    deleted: 0,
                }))
            }
            None => {}
        }
    }

    /// Record `key` as an own non-enumerable property.
    pub(crate) fn hide(&self, key: &Rc<str>) {
        let mut meta = self.meta.borrow_mut();
        let m = meta.get_or_insert_with(|| Box::new(ClosureMeta::default()));
        if !m.hidden.iter().any(|k| k == key) {
            m.hidden.push(key.clone());
        }
    }

    pub(crate) fn is_hidden(&self, key: &str) -> bool {
        self.meta
            .borrow()
            .as_ref()
            .is_some_and(|m| m.hidden.iter().any(|k| k.as_ref() == key))
    }

    /// The synthesized-slot bit for `key` (`name` = 0, `length` = 1).
    fn synth_bit(key: &str) -> Option<u8> {
        match key {
            "name" => Some(0),
            "length" => Some(1),
            _ => None,
        }
    }

    /// Has the synthesized own slot `key` been deleted from this function?
    pub(crate) fn synth_deleted(&self, key: &str) -> bool {
        match Self::synth_bit(key) {
            Some(b) => self
                .meta
                .borrow()
                .as_ref()
                .is_some_and(|m| m.deleted & (1 << b) != 0),
            None => false,
        }
    }

    /// Record that the synthesized own slot `key` was deleted.
    pub(crate) fn mark_synth_deleted(&self, key: &str) {
        if let Some(b) = Self::synth_bit(key) {
            let mut meta = self.meta.borrow_mut();
            let m = meta.get_or_insert_with(|| Box::new(ClosureMeta::default()));
            m.deleted |= 1 << b;
        }
    }
}

impl std::fmt::Debug for Closure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Closure({})", self.def.name)
    }
}

// Diagnostic object accounting (`count-alloc`): count destructions only, so a
// leaking cycle shows up as a per-reload drop delta that decays to zero while
// the live heap keeps climbing.
#[cfg(feature = "count-alloc")]
pub(crate) static OBJ_FREED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(feature = "count-alloc")]
pub(crate) static CLOSURE_FREED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(feature = "count-alloc")]
pub(crate) static ENV_FREED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "count-alloc")]
impl Drop for Obj {
    fn drop(&mut self) {
        OBJ_FREED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(feature = "count-alloc")]
impl Drop for Closure {
    fn drop(&mut self) {
        CLOSURE_FREED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            Val::BigInt(_) => "bigint",
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
        Val::Obj(new_obj(ObjKind::Plain(None, false), Vec::new(), None))
    }

    pub fn new_array(items: Vec<Val>) -> Val {
        Val::Obj(new_obj(ObjKind::Array(items), Vec::new(), None))
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
            Val::BigInt(n) => **n != 0,
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
            // ToNumber on a BigInt throws in the spec; arithmetic paths that
            // may legally see one branch before calling this (see binary_op),
            // and `Number(1n)` is an explicit conversion handled by the
            // builtin, so this is only the lossy fallback.
            Val::BigInt(n) => **n as f64,
            Val::Obj(o) => match &o.borrow().kind {
                ObjKind::Boxed(v) => v.to_number(),
                _ => str_to_number(&self.to_string()),
            },
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
                    ObjKind::Boxed(v) => v.coerce(depth + 1),
                    ObjKind::Array(items) => items
                        .iter()
                        .map(|v| match v {
                            Val::Undefined | Val::Null => String::new(),
                            v => v.coerce(depth + 1),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                    ObjKind::Error(ed) => format!("{}: {}", ed.name, ed.msg),
                    ObjKind::Date(ms) => format!("Date({}ms)", ms),
                    ObjKind::Regex(ri) => format!("/{}/{}", ri.re.src, ri.re.flags),
                    ObjKind::Promise(_) => "[object Promise]".into(),
                    ObjKind::Generator(_) => "[object Generator]".into(),
                    ObjKind::Style(_) => "[object CSSStyleDeclaration]".into(),
                    ObjKind::Live(_) => "[object HTMLCollection]".into(),
                    ObjKind::ModuleNs(_) => "[object Module]".into(),
                    ObjKind::Iterator(_) => "[object Iterator]".into(),
                    ObjKind::ArrayBuffer(_) => "[object ArrayBuffer]".into(),
                    ObjKind::TypedArray(ta) => typed_join(ta),
                    ObjKind::Plain(..) => {
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
            Val::BigInt(n) => n.to_string(),
            Val::Obj(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Boxed(v) => v.to_string_d(depth + 1),
                    ObjKind::Array(items) => items
                        .iter()
                        .map(|v| match v {
                            Val::Undefined | Val::Null => String::new(),
                            v => v.to_string_d(depth + 1),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                    ObjKind::Error(ed) => format!("{}: {}", ed.name, ed.msg),
                    ObjKind::Date(ms) => format!("Date({}ms)", ms),
                    ObjKind::Promise(st) => match &st.settled {
                        None => "Promise { <pending> }".into(),
                        Some(Ok(_)) => "Promise { <fulfilled> }".into(),
                        Some(Err(_)) => "Promise { <rejected> }".into(),
                    },
                    ObjKind::Plain(..) => {
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
                    ObjKind::Live(_) => "[object HTMLCollection]".into(),
                    ObjKind::ModuleNs(_) => "[object Module]".into(),
                    ObjKind::Iterator(_) => "[object Iterator]".into(),
                    ObjKind::ArrayBuffer(_) => "[object ArrayBuffer]".into(),
                    ObjKind::TypedArray(ta) => typed_join(ta),
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
    /// `with (obj) { ... }`: identifier lookups consult this object before
    /// the enclosing scopes (an object environment record)
    with: Option<Rc<RefCell<Obj>>>,
    /// `this` of the nearest function frame. Block scopes leave it `None`
    /// and resolution walks to the frame; arrows store their lexical `this`
    /// so the binding exists exactly where the language says it does.
    this: Option<Val>,
    /// `new.target` of a non-arrow frame (`Some(Undefined)` for a plain
    /// call, which must shadow an outer constructor). Arrow frames leave it
    /// `None` so they see the enclosing function's value.
    new_target: Option<Val>,
    /// Set while a call frame is binding its parameter list: the function
    /// whose parameters are being initialized. An eval in a default
    /// expression must not hoist a `var` over those bindings, because the
    /// spec puts the parameter environment between the eval's lexical and
    /// variable environments. `None` for every other scope.
    params: RefCell<Option<Rc<FnDef>>>,
    /// page that owns this scope: lets `JsEngine::drop` clear exactly its own
    /// page's environments (see `clear_page_envs`).
    page: u64,
    /// Set when a closure captures this scope. Only captured scopes can join
    /// a reference cycle (the only `Rc<Env>` owner besides the call stack is
    /// `Closure`), so only they need the page-drop registry. Uncaptured scopes
    /// stay registry-free, which also lets the call-frame pool recycle them
    /// (`Rc::get_mut` refuses a value that still has a `Weak`).
    registered: Cell<bool>,
}

thread_local! {
    /// Every environment, object and function created on this worker thread,
    /// tagged with the page that owns it. Only `Weak` refs are held, so the
    /// registries cannot keep a page alive by themselves.
    static LIVE_ENVS: RefCell<Vec<(u64, std::rc::Weak<Env>)>> =
        const { RefCell::new(Vec::new()) };
    static LIVE_OBJS: RefCell<Vec<(u64, std::rc::Weak<RefCell<Obj>>)>> =
        const { RefCell::new(Vec::new()) };
    static LIVE_FNS: RefCell<Vec<(u64, std::rc::Weak<Closure>)>> =
        const { RefCell::new(Vec::new()) };
    /// next page id handed to a `JsEngine`
    static NEXT_PAGE: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
    /// page tag for allocations made right now (0 before the first engine)
    static CUR_PAGE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Start a new page: every later allocation is tagged with this id and will
/// be swept by that page's `JsEngine::drop`.
fn page_start() -> u64 {
    let page = NEXT_PAGE.with(|p| {
        let v = p.get();
        p.set(v + 1);
        v
    });
    CUR_PAGE.with(|c| c.set(page));
    // drop entries whose owners are already gone; entries of the *previous*
    // page are still alive here (the new page is built before the old one is
    // dropped), so this must never prune by page id
    LIVE_ENVS.with(|r| r.borrow_mut().retain(|(_, w)| w.strong_count() > 0));
    LIVE_OBJS.with(|r| r.borrow_mut().retain(|(_, w)| w.strong_count() > 0));
    LIVE_FNS.with(|r| r.borrow_mut().retain(|(_, w)| w.strong_count() > 0));
    page
}

fn current_page() -> u64 {
    CUR_PAGE.with(|c| c.get())
}

/// A fresh plain object, registered with the current page for cycle sweeping.
pub(crate) fn new_obj(
    kind: ObjKind,
    props: Vec<(Rc<str>, Val)>,
    proto: Option<Rc<RefCell<Obj>>>,
) -> Rc<RefCell<Obj>> {
    let o = Rc::new(RefCell::new(Obj { kind, props, proto }));
    let page = current_page();
    LIVE_OBJS.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() % 8192 == 8191 {
            r.retain(|(_, w)| w.strong_count() > 0);
        }
        r.push((page, Rc::downgrade(&o)));
    });
    o
}

/// A fresh closure, registered with the current page (see [`new_obj`]).
pub(crate) fn new_closure(
    def: Rc<FnDef>,
    env: Rc<Env>,
    this: Option<Box<Val>>,
    is_arrow: bool,
) -> Rc<Closure> {
    ensure_env_registered(&env);
    let c = Rc::new(Closure {
        def,
        env,
        this,
        is_arrow,
        props: RefCell::new(Vec::new()),
        meta: RefCell::new(None),
    });
    let page = current_page();
    LIVE_FNS.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() % 8192 == 8191 {
            r.retain(|(_, w)| w.strong_count() > 0);
        }
        r.push((page, Rc::downgrade(&c)));
    });
    c
}

struct Binding {
    name: Rc<str>,
    val: Val,
    is_const: bool,
    /// immutable but not an error to assign in sloppy mode (a named function
    /// expression's self-binding): writes are silently ignored
    read_only: bool,
}

#[cfg(feature = "count-alloc")]
impl Drop for Env {
    fn drop(&mut self) {
        ENV_FREED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Destruction counters for the JS object graph (`count-alloc` builds only).
/// A per-reload delta that decays toward zero means objects of that kind are
/// stuck in reference cycles; a steady delta means they are collected.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapCounters {
    pub objs_freed: usize,
    pub closures_freed: usize,
    pub envs_freed: usize,
    /// environments still alive (strong count > 0) on this thread
    pub envs_live: usize,
    /// objects/functions still alive (strong count > 0) on this thread
    pub objs_live: usize,
    pub fns_live: usize,
}

pub fn heap_counters() -> HeapCounters {
    #[cfg(feature = "count-alloc")]
    {
        use std::sync::atomic::Ordering::Relaxed;
        let envs_live = LIVE_ENVS.with(|r| {
            let mut r = r.borrow_mut();
            r.retain(|(_, w)| w.strong_count() > 0);
            r.len()
        });
        let objs_live = LIVE_OBJS.with(|r| {
            let mut r = r.borrow_mut();
            r.retain(|(_, w)| w.strong_count() > 0);
            r.len()
        });
        let fns_live = LIVE_FNS.with(|r| {
            let mut r = r.borrow_mut();
            r.retain(|(_, w)| w.strong_count() > 0);
            r.len()
        });
        HeapCounters {
            objs_freed: OBJ_FREED.load(Relaxed),
            closures_freed: CLOSURE_FREED.load(Relaxed),
            envs_freed: ENV_FREED.load(Relaxed),
            envs_live,
            objs_live,
            fns_live,
        }
    }
    #[cfg(not(feature = "count-alloc"))]
    {
        HeapCounters::default()
    }
}

fn register_env(env: &Rc<Env>) {
    LIVE_ENVS.with(|r| {
        let mut r = r.borrow_mut();
        // drop dead entries periodically so a long page does not grow the
        // registry with every scope it ever created
        if r.len() % 4096 == 4095 {
            r.retain(|(_, w)| w.strong_count() > 0);
        }
        r.push((env.page, Rc::downgrade(env)));
    });
}

/// Register a scope the first time a closure captures it. The registration
/// costs a `Weak` plus a thread-local push, so it is paid per captured scope
/// instead of per call; nothing else can put an `Env` into a cycle.
fn ensure_env_registered(env: &Rc<Env>) {
    if !env.registered.replace(true) {
        register_env(env);
    }
}

/// Break every reference cycle owned by `page`, then forget its registry
/// entries. Called from `JsEngine::drop`.
///
/// `Rc` cannot collect cycles, and page scripts create plenty of them:
/// `a.b = b; b.a = a`, `f.prototype.constructor = f`, handlers stored on the
/// object they close over, ... Clearing *all* bindings, object properties and
/// function properties of the page leaves no edge pointing back into the
/// graph, so the whole page heap unwinds at once. Values that a discarded
/// page still owns are dead by definition: nothing can observe them again.
fn clear_page_graph(page: u64) {
    LIVE_ENVS.with(|r| {
        let mut r = r.borrow_mut();
        r.retain(|(p, w)| {
            if *p != page {
                return w.strong_count() > 0;
            }
            if let Some(env) = w.upgrade() {
                if let Ok(mut b) = env.bindings.try_borrow_mut() {
                    b.clear();
                }
            }
            false
        });
    });
    LIVE_OBJS.with(|r| {
        let mut r = r.borrow_mut();
        r.retain(|(p, w)| {
            if *p != page {
                return w.strong_count() > 0;
            }
            if let Some(o) = w.upgrade() {
                if let Ok(mut o) = o.try_borrow_mut() {
                    o.props.clear();
                    o.proto = None;
                }
            }
            false
        });
    });
    LIVE_FNS.with(|r| {
        let mut r = r.borrow_mut();
        r.retain(|(p, w)| {
            if *p != page {
                return w.strong_count() > 0;
            }
            if let Some(c) = w.upgrade() {
                if let Ok(mut props) = c.props.try_borrow_mut() {
                    props.clear();
                }
                if let Ok(mut meta) = c.meta.try_borrow_mut() {
                    *meta = None;
                }
            }
            false
        });
    });
}

impl Env {
    /// Root scope of the current page (bound functions, tests).
    pub fn root() -> Rc<Env> {
        Self::with_page(current_page(), None)
    }

    fn with_page(page: u64, this: Option<Val>) -> Rc<Env> {
        Rc::new(Env {
            parent: None,
            bindings: RefCell::new(Vec::new()),
            with: None,
            this,
            new_target: None,
            params: RefCell::new(None),
            page,
            registered: Cell::new(false),
        })
    }

    pub fn child(parent: &Rc<Env>) -> Rc<Env> {
        Rc::new(Env {
            parent: Some(parent.clone()),
            bindings: RefCell::new(Vec::new()),
            with: None,
            this: None,
            new_target: None,
            params: RefCell::new(None),
            page: parent.page,
            registered: Cell::new(false),
        })
    }

    /// Scope of a `with` statement body: identifier lookups consult `obj`
    /// before walking to the parent.
    pub(crate) fn with_obj(parent: &Rc<Env>, obj: Rc<RefCell<Obj>>) -> Rc<Env> {
        Rc::new(Env {
            parent: Some(parent.clone()),
            bindings: RefCell::new(Vec::new()),
            with: Some(obj),
            this: None,
            new_target: None,
            params: RefCell::new(None),
            page: parent.page,
            registered: Cell::new(false),
        })
    }

    /// Function-call frame. `new_target` is `None` for an arrow (it inherits
    /// through the env chain) and `Some` for every ordinary function.
    fn call_frame(parent: &Rc<Env>, this: Val, new_target: Option<Val>) -> Rc<Env> {
        Rc::new(Env {
            parent: Some(parent.clone()),
            bindings: RefCell::new(Vec::new()),
            with: None,
            this: Some(this),
            new_target,
            params: RefCell::new(None),
            page: parent.page,
            registered: Cell::new(false),
        })
    }

    fn declare(&self, name: Rc<str>, v: Val, is_const: bool) {
        self.bindings.borrow_mut().push(Binding {
            name,
            val: v,
            is_const,
            read_only: false,
        });
    }

    /// declare an immutable binding whose sloppy-mode assignment is a silent
    /// no-op (named function expression self-binding)
    pub(crate) fn declare_read_only(&self, name: Rc<str>, v: Val) {
        self.bindings.borrow_mut().push(Binding {
            name,
            val: v,
            is_const: false,
            read_only: true,
        });
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
        bs.push(Binding {
            name,
            val: v,
            is_const: false,
            read_only: false,
        });
    }

    /// set in this env only (no chain walk); returns Err if const/missing
    fn set_own(&self, name: &str, v: Val) -> Result<bool, JsError> {
        let mut bs = self.bindings.borrow_mut();
        for b in bs.iter_mut() {
            if b.name.as_ref() == name {
                if b.is_const {
                    return Err(JsError::throw("TypeError: Assignment to constant variable"));
                }
                if b.read_only {
                    // sloppy mode: assignment to a named function expression's
                    // own name is silently ignored
                    return Ok(true);
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
    /// `queueMicrotask(cb)`: run a plain callback in FIFO order with the
    /// promise reactions.
    Call(Val),
}

/// Shared bookkeeping for `Promise.all/any/race/allSettled`: a board where
/// element reactions deposit their outcomes and decide when the combined
/// promise settles.
#[derive(Debug)]
pub(crate) struct ComboState {
    /// the combined promise; `None` once it has settled
    pub result: Option<Rc<RefCell<Obj>>>,
    /// combined promise already settled — later element outcomes are dropped
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
    /// environment page tag: `drop` sweeps every scope of this page
    page: u64,
    pub(crate) global_obj: Rc<RefCell<Obj>>,
    /// shared prototype for every array (lazy-attached on first access)
    pub(crate) array_proto: Option<Rc<RefCell<Obj>>>,
    /// native id of `Array.prototype.values`, so `get_iterator` can tell the
    /// builtin `@@iterator` apart from a deleted or replaced one
    pub(crate) array_values_native: usize,
    /// shared `Object.prototype`: every plain object inherits it lazily
    pub(crate) object_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) natives: Vec<NativeEntry>,
    pub(crate) natives_by_name: HashMap<Rc<str>, usize>,
    pub(crate) call_depth: usize,
    /// Recycled call frames. A frame returns here only when its `Rc` is
    /// unique after the body (no closure or generator captured it), so
    /// recursive and call-heavy scripts stop paying an allocation per call.
    pub(crate) env_pool: Vec<Rc<Env>>,
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
    /// `Rc`s are the only refs from then on — normal refcount release).
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
    /// `Intl.DateTimeFormat` / `Intl.NumberFormat` prototypes; their
    /// instances keep the options under a hidden `__intl` prop
    pub(crate) intl_dtf_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) intl_nf_proto: Option<Rc<RefCell<Obj>>>,
    /// `Intl.Segmenter.prototype`; instances keep the granularity under the
    /// same hidden `__intl` prop as the formatting constructors
    pub(crate) intl_seg_proto: Option<Rc<RefCell<Obj>>>,
    /// reused native ids for the Uint8Array/Buffer stand-ins (avoids
    /// registering a new native per created instance)
    pub(crate) uint8_to_b64: usize,
    pub(crate) buffer_to_string: usize,
    pub(crate) arraybuffer_proto: Option<Rc<RefCell<Obj>>>,
    /// shared natives for typed-array methods (registered once by builtins)
    pub(crate) typed_set: usize,
    pub(crate) typed_subarray: usize,
    pub(crate) typed_slice: usize,
    /// typed-array prototype (`Uint8Array.prototype`, `%TypedArray%.prototype`)
    pub(crate) typed_proto: Option<Rc<RefCell<Obj>>>,
    /// shared `DataView.prototype`; instances carry a byte offset/length
    pub(crate) data_view_proto: Option<Rc<RefCell<Obj>>>,
    /// shared `AbortSignal.prototype` / `AbortController.prototype`
    pub(crate) abort_signal_proto: Option<Rc<RefCell<Obj>>>,
    pub(crate) abort_controller_proto: Option<Rc<RefCell<Obj>>>,
    /// `Function.prototype` — the fallback prototype for function values
    pub(crate) function_proto: Option<Rc<RefCell<Obj>>>,
    /// `Error.prototype` — parent of every error instance, so `extends
    /// Error` and borrowed `Error.prototype.toString` both work
    pub(crate) error_proto: Option<Rc<RefCell<Obj>>>,
    /// Synthetic binding name every value-using call frame declares. Cached
    /// per engine so a call clones the `Rc` instead of allocating a fresh
    /// string; it is on the hottest path in the interpreter.
    pub(crate) arguments_key: Rc<str>,
    /// compiled-once regex literal cache, keyed by (pattern, flags). Regex
    /// literals are program text, so this cache is bounded by the number of
    /// distinct (pattern, flags) literals the page can create; still capped
    /// at `REGEX_CACHE_MAX` (RAM honesty) — beyond the cap compiles are
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
    /// start offset of the callee expression of the call being made: the only
    /// location a "not a function" error can name (see `call_site_note`).
    /// Set by every `ExprKind::Call`, read on the error path.
    pub(crate) call_span: usize,
    /// Source id the span belongs to. Without it an error raised inside one
    /// module was reported against whatever module happened to be current
    /// (minified bundles make that a wrong line either way, but this one is
    /// silently wrong). Diagnostic only.
    pub(crate) call_src: u32,
    /// constructor value set by an enclosing `new` expression immediately
    /// before the call; `call_inner` takes it and binds `#new.target` in the
    /// callee's environment
    pub(crate) pending_construct: Option<Val>,
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
    /// live-collection bridge installed by the embedder: a member read that
    /// is not an own prop (`length`, an index, a named item) forwards here.
    /// `None` = no DOM attached: such reads are `undefined`.
    pub(crate) live_get: Option<fn(&LiveSpec, &str) -> Option<Val>>,
    /// live-collection snapshot for `for..of`, spread and `Array.from`
    /// (one host query, not one per index).
    pub(crate) live_values: Option<fn(&LiveSpec) -> Vec<Val>>,
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

impl Drop for JsEngine {
    /// Break the `Rc` cycles rooted at the global object/environment.
    ///
    /// A page's functions capture their environment (`Closure::env`), and the
    /// global environment's bindings hold those functions: env → binding →
    /// closure → env. `Rc` cannot collect that, so without this hook every
    /// navigation/reload leaked the consumed page's whole engine heap (the
    /// reload soak measured ~40 KB per reload before this).
    ///
    /// Only the roots are cleared. A self-contained cycle inside a nested
    /// function scope (a local `function f(){ return f; }`) is still
    /// unreachable garbage; tracing it would need a real GC, and the leak
    /// budget is set against what pages actually do.
    fn drop(&mut self) {
        // clear every scope, object and function this page created: page
        // scripts build plenty of `Rc` cycles (`a.b = b; b.a = a`,
        // `f.prototype.constructor = f`, ...) that plain refcounting cannot
        // collect, and a heavy site leaked megabytes per reload without this.
        clear_page_graph(self.page);
        if let Ok(mut b) = self.global_env.bindings.try_borrow_mut() {
            b.clear();
        }
        if let Ok(mut o) = self.global_obj.try_borrow_mut() {
            o.props.clear();
            o.proto = None;
            o.kind = ObjKind::Plain(None, false);
        }
        self.native_props.clear();
        self.timers.clear();
        self.microtasks.clear();
        self.promise_slots.clear();
        self.promise_statics = None;
        self.module_loader = None;
        self.regex_cache.clear();
        self.pending_construct = None;
        // prototypes can hold constructor methods whose closures capture the
        // global env; release the table entries as well as the roots
        self.array_proto = None;
        self.array_values_native = 0;
        self.object_proto = None;
        self.promise_proto = None;
        self.url_proto = None;
        self.params_proto = None;
        self.set_proto = None;
        self.map_proto = None;
        self.weakmap_proto = None;
        self.weakset_proto = None;
        self.text_encoder_proto = None;
        self.text_decoder_proto = None;
        self.intl_dtf_proto = None;
        self.intl_nf_proto = None;
        self.intl_seg_proto = None;
        self.arraybuffer_proto = None;
        self.typed_proto = None;
        self.data_view_proto = None;
        self.abort_signal_proto = None;
        self.abort_controller_proto = None;
        self.function_proto = None;
        self.error_proto = None;
    }
}

/// Canonical `Rc<str>` for property names. Every object property insert used
/// to allocate its own copy of the key: a React render creates the same
/// handful of names across thousands of objects, so a heavy page paid one
/// string allocation per property slot. Interning makes each distinct name a
/// single allocation shared by every object that uses it.
///
/// shortcut: one table per worker thread, capped at [`KEY_INTERN_MAX`]
/// distinct names. Past the cap a fresh `Rc` is made (correct, just not
/// shared), and `JsEngine::new` clears the table so a later page does not
/// keep the previous page's names.
thread_local! {
    static KEY_INTERN: std::cell::RefCell<HashMap<Box<str>, Rc<str>>> =
        std::cell::RefCell::new(HashMap::new());
}
pub(crate) const KEY_INTERN_MAX: usize = 4096;

/// Intern a property name, returning a shared `Rc<str>` when possible.
pub(crate) fn intern_key(key: &str) -> Rc<str> {
    KEY_INTERN.with(|m| {
        let mut m = m.borrow_mut();
        if let Some(k) = m.get(key) {
            return k.clone();
        }
        let k: Rc<str> = Rc::from(key);
        if m.len() < KEY_INTERN_MAX {
            m.insert(k.as_ref().into(), k.clone());
        }
        k
    })
}

fn clear_key_intern() {
    KEY_INTERN.with(|m| m.borrow_mut().clear());
}

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
        // one page's property names are not the next page's; drop the old
        // table so a long session keeps interning the CURRENT page's keys
        clear_key_intern();
        builtins::clear_symbol_registry();
        let page = page_start();
        let global_obj = new_obj(ObjKind::Plain(None, false), Vec::new(), None);
        // global code's `this` is the global object (sloppy mode)
        let global_env = Env::with_page(page, Some(Val::Obj(global_obj.clone())));
        let mut e = JsEngine {
            global_env,
            page,
            global_obj,
            array_proto: None,
            array_values_native: 0,
            object_proto: None,
            natives: Vec::new(),
            natives_by_name: HashMap::new(),
            call_depth: 0,
            env_pool: Vec::new(),
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
            intl_dtf_proto: None,
            intl_nf_proto: None,
            intl_seg_proto: None,
            uint8_to_b64: 0,
            buffer_to_string: 0,
            arraybuffer_proto: None,
            typed_set: 0,
            typed_subarray: 0,
            typed_slice: 0,
            typed_proto: None,
            data_view_proto: None,
            abort_signal_proto: None,
            abort_controller_proto: None,
            function_proto: None,
            error_proto: None,
            arguments_key: Rc::from("arguments"),
            regex_cache: HashMap::new(),
            next_timer_id: 1.0,
            steps: 0,
            step_budget: DEFAULT_STEP_BUDGET,
            cur_span: 0,
            call_span: 0,
            call_src: 0,
            pending_construct: None,
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
            live_get: None,
            live_values: None,
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
    /// the same engine — that's what makes multi-`<script>` pages work.
    /// The returned value is the last evaluated expression statement's value
    /// (JS `eval` completion-value semantics; declarations contribute
    /// nothing). Handy for embedder/tests; not observable from script code.
    pub fn exec(&mut self, source: &str) -> Result<Val, JsError> {
        self.call_depth = 0;
        self.steps = 0;
        let env = self.global_env.clone();
        let last = self.run_program(source, &env)?;
        // microtask quiescence at the end of every execution turn (idempotent
        // with the embedder's own drain later)
        self.run_microtasks(MAX_MICROTASKS)?;
        Ok(last)
    }

    /// Parse, hoist and run `source` against `env`. Shared by a top-level
    /// turn (`exec`, global scope) and direct `eval` (the calling scope);
    /// neither resets the step budget nor drains microtasks, so an eval runs
    /// inside the caller's turn.
    pub(crate) fn run_program(&mut self, source: &str, env: &Rc<Env>) -> Result<Val, JsError> {
        let src_id = self.register_source(source);
        let program: Program = parser::parse(source, src_id)?;
        // EvalDeclarationInstantiation: a sloppy direct eval hoists its vars
        // into the caller's variable environment, but not over a binding that
        // sits between its lexical and variable environments (a block `let`,
        // a catch parameter, or a parameter binding while a default runs).
        // `exec` always targets the global environment, so it never collides.
        if Self::eval_var_collides(env, &program) {
            return Err(JsError::Syntax {
                msg: "A direct eval will not hoist a var declaration over a like-named declaration"
                    .into(),
                line: 0,
                col: 0,
            });
        }
        self.cur_span = 0;
        self.cur_src = src_id;
        self.err_located = false;
        self.err_decorated = false;
        interp::hoist(self, env, &program);
        let mut last = Val::Undefined;
        for st in program.iter() {
            // a Budget error from this turn names the spinning statement;
            // `finish_error` resolves the recorded offset to line/col
            self.err_located = false;
            let r = match &st.kind {
                parser::StmtKind::Expr(e) => interp::eval_expr(self, env, e).map(|v| {
                    last = v;
                }),
                _ => interp::eval_stmt(self, env, st).map(|_| ()),
            };
            r.map_err(|err| self.finish_error(err, source))?;
        }
        Ok(last)
    }

    /// Would this direct eval hoist a `var` name over an existing binding?
    /// Walks the scope chain from the eval site out to the variable
    /// environment (the nearest function frame, or the global environment).
    /// A frame still binding its parameters is the parameter environment, so
    /// its (pending) parameter names and the implicit `arguments` object
    /// count; object (`with`) environments carry no bindings and are skipped.
    /// Strict eval gets a fresh variable environment, so it never collides.
    fn eval_var_collides(env: &Rc<Env>, prog: &Program) -> bool {
        if parser::has_use_strict(prog) {
            return false;
        }
        let names = interp::var_declared_names(prog);
        if names.is_empty() {
            return false;
        }
        let mut cur = env.clone();
        loop {
            let pending = cur.params.borrow().clone();
            if let Some(def) = pending {
                // parameter environment: the variable environment is its
                // parent, so every binding here is "in between"
                if cur.new_target.is_some() && names.iter().any(|n| n.as_ref() == "arguments") {
                    return true;
                }
                return names.iter().any(|n| {
                    def.params.iter().any(|p| interp::param_binds(p, n))
                        || cur.bindings.borrow().iter().any(|b| b.name == *n)
                });
            }
            if cur.this.is_some() {
                // reached the variable environment itself
                return false;
            }
            if names
                .iter()
                .any(|n| cur.bindings.borrow().iter().any(|b| b.name == *n))
            {
                return true;
            }
            match &cur.parent {
                Some(p) => cur = p.clone(),
                None => return false,
            }
        }
    }

    /// Human-readable text for a thrown value, for embedder logs and tools.
    /// Plain `Val::to_string` prints `[object Object]` for script errors that
    /// keep their text in a `message` property (test262's `Test262Error`
    /// does), which hides the actual assertion from a failure report. Reads
    /// the property directly instead of calling `toString`, so a diagnostic
    /// can never run user code or throw.
    pub fn describe_thrown(&mut self, v: &Val) -> String {
        if let Val::Obj(o) = v {
            let msg = obj_get_own(o, "message");
            if !msg.is_undefined() {
                let name = obj_get_own(o, "name");
                if !name.is_undefined() {
                    return format!("{}: {}", name.to_string(), msg.to_string());
                }
                return msg.to_string();
            }
        }
        v.to_string()
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
    /// them however it likes — the interpreter has no timer thread.
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

    /// Effective [[Prototype]] of `o` without attaching it: the explicit
    /// `proto` when set, else the shared prototype its kind inherits from
    /// (`get_member` attaches that one lazily on first access). `None` means
    /// a genuinely null prototype: module namespaces, and `Object.prototype`
    /// itself. `Object.getPrototypeOf` and `instanceof` both report this
    /// chain, so fresh built-ins never look prototype-less (core-js read
    /// `u.next` off a `null` iterator prototype and threw).
    pub(crate) fn proto_of(&self, o: &Rc<RefCell<Obj>>) -> Option<Rc<RefCell<Obj>>> {
        // Proxy: the prototype comes from the target, so `instanceof` and
        // inherited method reads behave as if the proxy were the target.
        if let Some((target, _)) = proxy_parts(o) {
            return match target {
                Val::Obj(t) => self.proto_of(&t),
                Val::Func(c) => match c.proto() {
                    Some(Val::Obj(t)) => self.proto_of(&t),
                    _ => None,
                },
                _ => None,
            };
        }
        let b = o.borrow();
        // an explicitly null [[Prototype]] must not fall through to the lazy
        // default below
        if matches!(b.kind, ObjKind::Plain(_, true)) {
            return None;
        }
        if let Some(p) = &b.proto {
            return Some(p.clone());
        }
        let p = match &b.kind {
            // the export map IS the namespace object's storage; it has no
            // prototype to walk
            ObjKind::ModuleNs(_) => return None,
            ObjKind::Array(_) => self.array_proto.clone(),
            ObjKind::Promise(_) => self.promise_proto.clone(),
            ObjKind::TypedArray(_) => self.typed_proto.clone(),
            ObjKind::Error(_) => self.error_proto.clone(),
            _ => self.object_proto.clone(),
        };
        // a prototype object is never its own prototype
        match p {
            Some(p) if !Rc::ptr_eq(&p, o) => Some(p),
            _ => None,
        }
    }

    /// `key in o` over the same lazy chain `get_member` walks: a fresh array
    /// or iterator has not attached its prototype yet, but `length` /
    /// `Symbol.iterator` must still answer.
    pub(crate) fn chain_has(&self, o: &Rc<RefCell<Obj>>, key: &str) -> bool {
        let mut cur = Some(o.clone());
        for _ in 0..PROTO_WALK_CAP {
            let c = match cur {
                None => break,
                Some(c) => c,
            };
            {
                let b = c.borrow();
                if let ObjKind::ModuleNs(xs) = &b.kind {
                    return xs.borrow().contains_key(key);
                }
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
            }
            cur = self.proto_of(&c);
        }
        false
    }

    pub(crate) fn random_f64(&mut self) -> f64 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Next 32 random bits from the same generator as `Math.random`. Used by
    /// `crypto.getRandomValues`/`randomUUID`; a PRNG, not a CSPRNG, so a page
    /// that needs real entropy would notice (documented shortcut).
    pub(crate) fn random_u32(&mut self) -> u32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        (x >> 32) as u32
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

    /// Where the call currently being made sits in its source:
    /// `" (at 285:155 of https://…/chunk.js)"`. `"undefined is not a function"`
    /// from a minified bundle names nothing on its own, and this engine keeps
    /// no AST for a stack trace — the callee offset is the one reliable
    /// pointer a page author (or we) can read. Error path only.
    pub(crate) fn call_site_note(&self) -> String {
        let mut note = self.site_note_in(self.call_src, self.call_span);
        // A column in a minified bundle names nothing on its own, and the
        // failing callee is often an unnamed expression. Quote the tokens just
        // before the span so the error is diagnosable without the bundle.
        if let Some(src) = self.source_text(self.call_src) {
            let mut end = self.call_span.min(src.len());
            while !src.is_char_boundary(end) {
                end -= 1;
            }
            let start = src[..end]
                .char_indices()
                .rev()
                .nth(60)
                .map(|(i, _)| i)
                .unwrap_or(0);
            let snippet: String = src[start..end].chars().filter(|c| *c != '\n').collect();
            let snippet = snippet.trim_start();
            if !snippet.is_empty() {
                note.push_str(&format!(" near `{snippet}`"));
            }
        }
        note
    }

    /// `" (at line:col of url)"` for any offset in the current source.
    pub(crate) fn site_note(&self, span: usize) -> String {
        self.site_note_in(self.cur_src, span)
    }

    /// The same, for an explicit source: an error raised while executing one
    /// module's code must not be located in whatever module is current.
    fn site_note_in(&self, src_id: u32, span: usize) -> String {
        let src = match self.source_text(src_id) {
            Some(s) => s,
            None => return String::new(),
        };
        let (l, c) = parser::line_col(&src, span);
        let url = match self.module_url_for(src_id) {
            Some(u) => u.to_string(),
            None => format!("src {src_id}"),
        };
        format!(" (at {l}:{c} of {url})")
    }

    /// A call frame from the pool when one is free, otherwise a fresh one.
    fn take_env(&mut self, parent: &Rc<Env>, this: Val, new_target: Option<Val>) -> Rc<Env> {
        while let Some(mut env) = self.env_pool.pop() {
            // pooled frames are unregistered, so `get_mut` succeeds unless a
            // `Weak` appeared; then drop it and take the next (or a new) one
            if let Some(e) = Rc::get_mut(&mut env) {
                e.parent = Some(parent.clone());
                e.this = Some(this);
                e.new_target = new_target;
                return env;
            }
        }
        Env::call_frame(parent, this, new_target)
    }

    /// Return a frame to the pool when nothing escaped into it. A closure or
    /// generator that captured the frame holds its own `Rc`, so the strong
    /// count is the whole safety argument.
    fn recycle_env(&mut self, mut env: Rc<Env>) {
        const ENV_POOL_MAX: usize = 64;
        if Rc::strong_count(&env) != 1 || self.env_pool.len() >= ENV_POOL_MAX {
            return;
        }
        let Some(e) = Rc::get_mut(&mut env) else { return };
        e.bindings.get_mut().clear();
        e.parent = None;
        e.this = None;
        e.new_target = None;
        e.params.get_mut().take();
        self.env_pool.push(env);
    }

    /// Run a closure body after its frame and parameters are set up.
    fn run_closure_body(
        &mut self,
        c: &Rc<Closure>,
        env: &Rc<Env>,
        args: Vec<Val>,
    ) -> Result<Val, JsError> {
        if c.def.is_generator {
            // generators never run the body at call time; the mini-VM
            // (gen.rs) resumes it on `.next`
            return gen::gen_object(self, &c.def, env);
        }
        if c.def.is_async {
            // run-to-completion tradeoff: the body runs NOW to completion;
            // each `await` inside pumps the microtask queue and the result
            // settles the promise this call returns.
            let p = self.new_promise();
            match interp::run_function_body(self, c, env) {
                Ok(v) => resolve_promise(self, &p, v),
                Err(JsError::Throw(v)) => {
                    let v = interp::caught_value(self, v);
                    settle_promise(self, &p, Err(v))
                }
                Err(other) => return Err(other),
            }
            return Ok(Val::Obj(p));
        }
        match &c.def.body {
            FnBody::Expr(x) => interp::eval_expr(self, env, x),
            FnBody::Block(_stmts) => interp::run_function_body(self, c, env),
            // Function.prototype.bind partial application
            FnBody::Bound {
                target,
                this,
                args: bargs,
            } => {
                let mut all = bargs.clone();
                all.extend(args);
                self.call(target.clone(), this.clone(), all)
            }
        }
    }

    fn call_inner(&mut self, callee: Val, this: Val, args: Vec<Val>) -> Result<Val, JsError> {
        // only the direct callee of `new` sees this; a plain call made from
        // inside the constructor takes it during ITS own call_inner, so the
        // binding never leaks into helper functions
        let construct_target = self.pending_construct.take();
        match callee {
            Val::Func(c) => {
                let this_val = if c.is_arrow {
                    c.this.as_deref().cloned().unwrap_or(Val::Undefined) // lexical this
                } else if matches!(this, Val::Undefined) {
                    // sloppy-mode: plain calls get the global object as this
                    Val::Obj(self.global_obj.clone())
                } else {
                    this
                };
                // arrows see the enclosing function's `new.target`, so they
                // leave the slot empty; every ordinary frame shadows an outer
                // constructor with its own value (undefined for a plain call)
                let new_target = if c.is_arrow {
                    None
                } else {
                    Some(construct_target.unwrap_or(Val::Undefined))
                };
                let env = self.take_env(&c.env, this_val, new_target);
                if !c.is_arrow && c.def.uses_arguments {
                    let items = args.clone();
                    env.declare(self.arguments_key.clone(), Val::new_array(items), false);
                }
                // while the parameter list is being bound the frame is the
                // parameter environment: a direct eval in a default must not
                // hoist a var over these bindings
                *env.params.borrow_mut() = Some(c.def.clone());
                let bound = interp::bind_params(self, &env, &c.def, &args);
                env.params.borrow_mut().take();
                let result = match bound {
                    Ok(()) => self.run_closure_body(&c, &env, args),
                    Err(err) => Err(err),
                };
                self.recycle_env(env);
                result
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
                    None => Err(JsError::Throw(Val::Str(Rc::from(format!(
                        "TypeError: object is not a function{}",
                        self.call_site_note()
                    ))))),
                }
            }
            other => {
                Err(JsError::Throw(Val::Str(Rc::from(format!(
                    "TypeError: {} is not a function{}",
                    other.type_of(),
                    self.call_site_note()
                )))))
            }
        }
    }

    // -- identifier resolution (env chain, then global object) --------------

    pub(crate) fn env_get(&self, env: &Rc<Env>, name: &str) -> Val {
        // `this` / `new.target` are frame slots, not bindings: no per-call
        // `Rc<str>` allocation and no entries in the binding vec. These two
        // compares run before the normal chain walk, so ordinary identifiers
        // pay nothing extra per scope.
        if name == "this" {
            return Self::frame_slot(env, "this");
        }
        if name == "#new.target" {
            return Self::frame_slot(env, "#new.target");
        }
        let mut cur: &Env = env;
        loop {
            if let Some(w) = &cur.with {
                if let Some(v) = obj_lookup_chain(w, name) {
                    return v;
                }
            }
            if let Some(v) = cur.bindings.borrow().iter().find(|b| b.name.as_ref() == name) {
                return v.val.clone();
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        let v = obj_get_own(&self.global_obj, name);
        // MAZDA_MISS census: an identifier that resolved NOWHERE (no binding,
        // no global-object property) reads as `undefined` - a name the page
        // expected and the engine does not have. Gate first: this is the
        // hottest read path in the interpreter.
        if js_miss_enabled() && v.is_undefined() {
            note_js_miss("global", name);
        }
        v
    }

    /// Identifier read for expression positions: an unresolvable reference is
    /// a ReferenceError. `typeof ident` and `get_global` keep using `env_get`
    /// (an undeclared name is legal there).
    pub(crate) fn env_get_checked(&mut self, env: &Rc<Env>, name: &str) -> Result<Val, JsError> {
        if name == "this" {
            return Ok(Self::frame_slot(env, "this"));
        }
        if name == "#new.target" {
            return Ok(Self::frame_slot(env, "#new.target"));
        }
        let mut cur: &Rc<Env> = env;
        loop {
            // a `with` object's property wins over every outer binding
            if let Some(w) = &cur.with {
                if obj_has(w, name) {
                    return get_member(self, &Val::Obj(w.clone()), name);
                }
            }
            if let Some(v) = cur.bindings.borrow().iter().find(|b| b.name.as_ref() == name) {
                return Ok(v.val.clone());
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        // a global property that exists but holds undefined is resolvable
        // (`window.foo = undefined`); only absence throws
        if self.chain_has(&self.global_obj, name) {
            let v = obj_get_own(&self.global_obj, name);
            if js_miss_enabled() && v.is_undefined() {
                note_js_miss("global", name);
            }
            return Ok(v);
        }
        if js_miss_enabled() {
            note_js_miss("global", name);
        }
        Err(JsError::throw(&format!(
            "ReferenceError: {name} is not defined"
        )))
    }

    /// Resolve a function-frame slot (`this` or `new.target`) through the
    /// scope chain. Block scopes carry `None`, so the walk stops at the
    /// nearest function frame; an arrow leaves `new.target` empty on purpose.
    fn frame_slot(env: &Rc<Env>, name: &str) -> Val {
        let want_this = name == "this";
        let mut cur: &Env = env;
        loop {
            let hit = if want_this {
                cur.this.as_ref()
            } else {
                cur.new_target.as_ref()
            };
            if let Some(v) = hit {
                return v.clone();
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        Val::Undefined
    }

    /// Assignment: walks the chain; const bindings throw; unknown names are
    /// (sloppily) created as globals — real-world scripts rely on that.
    pub(crate) fn env_set(&mut self, env: &Rc<Env>, name: &str, v: Val) -> Result<(), JsError> {
        let mut cur: &Rc<Env> = env;
        loop {
            // `with (obj) { name = v }` assigns the property when the object
            // has it (HasProperty), otherwise the assignment falls through
            if let Some(w) = &cur.with {
                if obj_has(w, name) {
                    return set_member(self, &Val::Obj(w.clone()), name, v);
                }
            }
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

    /// `delete ident`: a `with` object's property is deleted (a reference to
    /// it wins over any outer binding), a lexical binding is not deletable,
    /// and an unresolvable name deletes successfully. Sloppy-mode only; the
    /// parser has no strict-mode `delete ident` early error yet.
    pub(crate) fn env_delete(&self, env: &Rc<Env>, name: &str) -> bool {
        let mut cur: &Rc<Env> = env;
        loop {
            if let Some(w) = &cur.with {
                if obj_has(w, name) {
                    // the engine treats every property as configurable, so
                    // deleting an own property (or a prototype hit) is true
                    delete_member(&Val::Obj(w.clone()), name);
                    return true;
                }
            }
            if cur.bindings.borrow().iter().any(|b| b.name.as_ref() == name) {
                return false;
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        true
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
    new_obj(
        ObjKind::Promise(Box::new(PromiseState::default())),
        Vec::new(),
        None,
    )
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
/// user-defined `then`) resolve as plain values — documented.
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

/// `.then(onOk, onErr)` — creates the derived promise, queues (or fires)
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
        Err(JsError::Throw(v)) => {
            let v = interp::caught_value(e, v);
            reject_slot(e, slot, v)
        }
        Err(other) => return Err(other),
    }
    Ok(Val::Obj(p))
}

/// `Promise.withResolvers()`: a fresh promise plus its resolve/reject pair,
/// with no executor to run. Same slot mechanism as the `new Promise` form.
pub(crate) fn promise_with_resolvers(e: &mut JsEngine) -> Val {
    let p = e.new_promise();
    let slot = e.promise_slots.len() as u16;
    e.promise_slots.push(Some(p.clone()));
    let rid = e.register_native("Promise.resolve#slot", n_p_resolve, slot);
    let jid = e.register_native("Promise.reject#slot", n_p_reject, slot);
    let out = Val::new_object();
    out.set("promise", Val::Obj(p));
    out.set("resolve", Val::Native(rid));
    out.set("reject", Val::Native(jid));
    out
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
        Job::Call(cb) => {
            e.call(cb, Val::Undefined, Vec::new())?;
            return Ok(());
        }
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
                Err(JsError::Throw(thrown)) => {
                    let thrown = interp::caught_value(e, thrown);
                    deliver(e, re.ret, Err(thrown))
                }
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
                    let thrown = interp::caught_value(e, thrown);
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

/// Data-property read walking the prototype chain, without invoking accessors.
/// Used by the lenient `env_get` for `with` objects; `env_get_checked` uses
/// `get_member` so getters and proxy-ish behaviour still work.
fn obj_lookup_chain(o: &Rc<RefCell<Obj>>, key: &str) -> Option<Val> {
    let mut cur = Some(o.clone());
    for _ in 0..PROTO_WALK_CAP {
        let c = cur?;
        let b = c.borrow();
        if let Some((_, v)) = b.props.iter().find(|(k, _)| k.as_ref() == key) {
            return Some(v.clone());
        }
        cur = b.proto.clone();
    }
    None
}

/// Own-property read (no prototype walk, no methods).
pub fn obj_get_own(o: &Rc<RefCell<Obj>>, key: &str) -> Val {    let b = o.borrow();
    if let ObjKind::ModuleNs(xs) = &b.kind {
        return xs.borrow().get(key).cloned().unwrap_or(Val::Undefined);
    }
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

/// Record an own property as non-enumerable (class methods and accessors).
/// The property still lives in `props`; readers that care (descriptors,
/// `for-in`, `Object.keys`) consult this list.
pub(crate) fn obj_hide(o: &Rc<RefCell<Obj>>, key: &Rc<str>) {
    let mut b = o.borrow_mut();
    let ObjKind::Plain(slot, _) = &mut b.kind else {
        return;
    };
    let list = slot.get_or_insert_with(|| Box::new(Vec::new()));
    if !list.iter().any(|k| k == key) {
        list.push(key.clone());
    }
}

/// The function-object counterpart of [`obj_hide`] (static class members).
pub(crate) fn clo_hide(c: &Rc<Closure>, key: &Rc<str>) {
    c.hide(key);
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
        None => b.props.push((intern_key(key), v)),
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
        if let ObjKind::ModuleNs(xs) = &b.kind {
            // a namespace has no prototype; unknown names are absent
            return xs.borrow().contains_key(key);
        }
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

/// Proxy stand-in plumbing: a proxy is a plain object carrying a hidden
/// `__proxy` slot `{ target, handler }`. Only the `get`/`set` traps run; every
/// other operation forwards to the target (see the `Proxy` shortcut in
/// `builtins.rs`).
pub(crate) fn proxy_parts(o: &Rc<RefCell<Obj>>) -> Option<(Val, Val)> {
    let b = o.borrow();
    let slot = b
        .props
        .iter()
        .find(|(k, _)| &**k == "__proxy")
        .map(|(_, v)| v.clone())?;
    let Val::Obj(s) = slot else { return None };
    let sb = s.borrow();
    let target = sb
        .props
        .iter()
        .find(|(k, _)| &**k == "target")
        .map(|(_, v)| v.clone())?;
    let handler = sb
        .props
        .iter()
        .find(|(k, _)| &**k == "handler")
        .map(|(_, v)| v.clone())?;
    Some((target, handler))
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
            "TypeError: Cannot read property '{}' of {}{}{}",
            key,
            base.type_of(),
            e.stack_hint(),
            e.site_note(e.cur_span)
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
                "concat" => Some(StrConcat),
                "substr" => Some(StrSubstr),
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
        Val::BigInt(_) => {
            let m = if key == "toString" || key == "valueOf" {
                Some(Meth::NumToString)
            } else {
                None
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
                // a deleted `name`/`length` no longer resolves as an own slot
                "name" if !c.synth_deleted("name") => Ok(Val::Str(c.def.name.clone())),
                "length" if !c.synth_deleted("length") => Ok(Val::Num(c.def.length() as f64)),
                "name" | "length" => Ok(Val::Undefined),
                _ => {
                    // class static members inherited through the function's
                    // own [[Prototype]] (`class B extends A` links B -> A)
                    let mut cur = c.proto();
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
                                f.proto()
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
                // Date.parse (relative-time's `date` getter is the caller)
                "parse" => Ok(Val::Method(Meth::DateParse)),
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
            // Fast path: an own data property on a plain object. This is the
            // common case for script objects and skips the lazy-prototype
            // borrow, every exotic-kind probe and a second scan in
            // `chain_get`. Accessors fall through so the getter runs with the
            // receiver, and a miss still takes the full path.
            {
                let b = o.borrow();
                if matches!(b.kind, ObjKind::Plain(..)) {
                    if let Some((_, v)) = b.props.iter().find(|(k, _)| k.as_ref() == key) {
                        if !matches!(v, Val::Acc(_)) {
                            return Ok(v.clone());
                        }
                    }
                }
            }
            // Proxy: the handler's `get` trap wins, otherwise the read
            // forwards to the target.
            if let Some((target, handler)) = proxy_parts(o) {
                let trap = get_member(e, &handler, "get")?;
                if is_callable(&trap) {
                    return e.call(
                        trap,
                        handler.clone(),
                        vec![target, Val::Str(Rc::from(key)), Val::Obj(o.clone())],
                    );
                }
                return get_member(e, &target, key);
            }
            // boxed String: `length` and index reads behave like the
            // primitive; everything else (methods) falls through to the
            // String.prototype chain.
            let boxed_str = match &o.borrow().kind {
                ObjKind::Boxed(Val::Str(s)) => Some(s.clone()),
                _ => None,
            };
            if let Some(s) = boxed_str {
                if key == "length" {
                    return Ok(Val::Num(s.chars().count() as f64));
                }
                if let Ok(i) = key.parse::<usize>() {
                    return Ok(match s.chars().nth(i) {
                        Some(c) => Val::Str(Rc::from(c.to_string())),
                        None => Val::Undefined,
                    });
                }
            }
            // lazy prototype attach: engine-created arrays (literals,
            // builtins, embedder-made) pick up Array.prototype on first
            // property access, so they never pay for it up front. Plain
            // objects pick up Object.prototype the same way.
            {
                let mut b = o.borrow_mut();
                if b.proto.is_none() && !matches!(b.kind, ObjKind::ModuleNs(_)) {
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
            // Live host collection: own props (`item`/`namedItem`) win, then
            // every other key (`length`, an index, a named item) is answered
            // by the embedder against the CURRENT DOM. A miss falls through
            // to the prototype chain so `toString` still works.
            let live = o
                .try_borrow()
                .ok()
                .and_then(|b| match &b.kind {
                    ObjKind::Live(s) => Some(s.clone()),
                    _ => None,
                });
            if let Some(spec) = live {
                let own = o
                    .borrow()
                    .props
                    .iter()
                    .find(|(k, _)| k.as_ref() == key)
                    .map(|(_, v)| v.clone());
                if let Some(v) = own {
                    return Ok(v);
                }
                if let Some(f) = e.live_get {
                    if let Some(v) = f(&spec, key) {
                        return Ok(v);
                    }
                }
            }
            {
                let b = o.borrow();
                if let ObjKind::Error(ed) = &b.kind {
                    // error objects expose .name/.message as own props
                    match key {
                        "name" => return Ok(Val::Str(ed.name.clone())),
                        "message" => return Ok(Val::Str(ed.msg.clone())),
                        // shortcut: no captured frames - the "stack" is just
                        // the head line a log/overlay would print
                        "stack" => {
                            return Ok(Val::Str(Rc::from(format!("{}: {}", ed.name, ed.msg).as_str())))
                        }
                        _ => {}
                    }
                }
                if let ObjKind::Date(_) = b.kind {
                    use Meth::*;
                    let m = match key {
                        "getTime" | "valueOf" => Some(DateGetTime),
                        "toISOString" => Some(DateToISO),
                        // the local-time getters; UTC aliases answer the same
                        // numbers because this engine has no timezone database
                        "getFullYear" | "getUTCFullYear" => Some(DateGetYear),
                        "getMonth" | "getUTCMonth" => Some(DateGetMonth),
                        "getDate" | "getUTCDate" => Some(DateGetDate),
                        "getDay" | "getUTCDay" => Some(DateGetWeekday),
                        "getHours" | "getUTCHours" => Some(DateGetHours),
                        "getMinutes" | "getUTCMinutes" => Some(DateGetMinutes),
                        "getSeconds" | "getUTCSeconds" => Some(DateGetSeconds),
                        "getMilliseconds" | "getUTCMilliseconds" => Some(DateGetMillis),
                        "toLocaleDateString" => Some(DateLocaleDate),
                        "toLocaleTimeString" => Some(DateLocaleTime),
                        "toLocaleString" => Some(DateLocaleBoth),
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
                if let ObjKind::Iterator(_) = b.kind {
                    match key {
                        "next" => return Ok(Val::Method(Meth::IterNext)),
                        "return" => return Ok(Val::Method(Meth::IterReturn)),
                        // `it[Symbol.iterator]() === it`
                        k if k == SYMBOL_ITERATOR => {
                            return Ok(Val::Method(Meth::IterSelf))
                        }
                        _ => {}
                    }
                }
                if let ObjKind::ArrayBuffer(bytes) = &b.kind {
                    if key == "byteLength" {
                        return Ok(Val::Num(bytes.borrow().len() as f64));
                    }
                }
                if let ObjKind::TypedArray(ta) = &b.kind {
                    match key {
                        "length" => return Ok(Val::Num(ta.len as f64)),
                        "byteLength" => {
                            return Ok(Val::Num((ta.len * ta.kind.size()) as f64))
                        }
                        "byteOffset" => return Ok(Val::Num(ta.offset as f64)),
                        "BYTES_PER_ELEMENT" => {
                            return Ok(Val::Num(ta.kind.size() as f64))
                        }
                        "buffer" => return Ok(Val::Obj(ta.buf.clone())),
                        "set" => return Ok(Val::Native(e.typed_set)),
                        "subarray" => return Ok(Val::Native(e.typed_subarray)),
                        "slice" => return Ok(Val::Native(e.typed_slice)),
                        k if k == SYMBOL_ITERATOR => {
                            return Ok(Val::Method(Meth::IterSelf))
                        }
                        _ => {}
                    }
                    if let Ok(i) = key.parse::<usize>() {
                        return Ok(typed_read(ta, i));
                    }
                }
            }
            // prototype walk with accessor interception: an own OR inherited
            // `Val::Acc` property invokes its getter (receiver = `base`)
            let got = chain_get(e, base, o.clone(), key)?;
            if got.is_none() {
                // MAZDA_MISS census: a host read that resolved nowhere
                note_member_miss(e, &o, key);
            }
            Ok(got.unwrap_or(Val::Undefined))
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
            if let ObjKind::ModuleNs(xs) = &b.kind {
                // live namespace: the export map is the object's own storage
                // (and it has no prototype)
                return Ok(xs.borrow().get(key).cloned());
            }
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
                e.proto_of(&c),
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

// ---------------------------------------------------------------------------
// Observed JS misses (`MAZDA_MISS=1`, dev-only) — the JS half of `css.rs`'s
// census: a read that falls through to `undefined` on the global object or a
// host wrapper is a name the page wanted and the engine does not have.
// Two buckets: `global` (bare identifiers that resolved nowhere + reads on
// the `window` object) and `member` (reads on embedder-built wrappers).
// Off by default — every recording call is one `OnceLock` load and a branch;
// nothing is allocated.
// ---------------------------------------------------------------------------

/// Distinct names kept per bucket. A page can synthesize unbounded distinct
/// keys (`window['k' + i]`), so past the cap only counts for already-seen
/// names keep growing.
const JS_MISS_CAP: usize = 512;

#[derive(Default)]
struct JsMisses {
    global: std::collections::HashMap<String, u32>,
    member: std::collections::HashMap<String, u32>,
    /// distinct names skipped per bucket after [`JS_MISS_CAP`]
    over_global: u32,
    over_member: u32,
}

fn js_miss_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("MAZDA_MISS").is_some())
}

fn js_misses() -> &'static std::sync::Mutex<JsMisses> {
    static MISSES: std::sync::OnceLock<std::sync::Mutex<JsMisses>> = std::sync::OnceLock::new();
    MISSES.get_or_init(|| std::sync::Mutex::new(JsMisses::default()))
}

/// One bucket's record: bump the count, or admit a new name while under the
/// cap (past it, new names are tallied in `over` instead of allocated).
fn add_js_miss(counts: &mut std::collections::HashMap<String, u32>, over: &mut u32, name: &str) {
    if let Some(n) = counts.get_mut(name) {
        *n += 1;
        return;
    }
    if counts.len() >= JS_MISS_CAP {
        *over += 1;
        return;
    }
    counts.insert(name.to_string(), 1);
}

/// Record one miss. `bucket` is [`miss_bucket`]'s answer; the two exclusions
/// are names that are not gaps — `undefined` reads as itself by definition
/// (both bare and as `window.undefined`), and `#`-prefixed keys are the
/// engine's internal slots (`#new.target`), not page-facing names.
fn note_js_miss(bucket: &str, name: &str) {
    if let Ok(mut m) = js_misses().lock() {
        record_js_miss(&mut m, bucket, name);
    }
}

/// Pure record (exclusions + bucket dispatch) — tests drive this on local
/// state; the process-wide census would race across parallel tests.
fn record_js_miss(m: &mut JsMisses, bucket: &str, name: &str) {
    if name == "undefined" || name.starts_with('#') {
        return;
    }
    match bucket {
        "global" => add_js_miss(&mut m.global, &mut m.over_global, name),
        _ => add_js_miss(&mut m.member, &mut m.over_member, name),
    }
}

/// Classify a read that resolved nowhere (the `get_member` fallthrough):
/// the global object's own misses are missing globals, a wrapper the
/// embedder built is a missing DOM/JS member, and a plain script object's
/// undefined read is normal JS that records nothing. Host shapes: a
/// `__node` marker (element wrappers), any native method (the singleton
/// wrappers — document, navigator, location, history, storage — are built
/// from natives alone), or a live-collection kind. `None` = not counted.
/// Pure classification — the env gate lives at the call sites.
fn miss_bucket(e: &JsEngine, o: &Rc<RefCell<Obj>>) -> Option<&'static str> {
    if Rc::ptr_eq(o, &e.global_obj) {
        return Some("global");
    }
    let b = o.borrow();
    // Style objects answer upstream (through `style_get`, never `undefined`)
    // but a Live collection that missed there falls through to here
    if matches!(b.kind, ObjKind::Live(_) | ObjKind::Style(_)) {
        return Some("member");
    }
    let host = b.props.iter().any(|(k, v)| {
        k.as_ref() == "__node"
            || matches!(v, Val::Native(_))
            || matches!(
                v,
                Val::Acc(a)
                    if matches!(a.getter, Some(Val::Native(_)))
                        || matches!(a.setter, Some(Val::Native(_)))
            )
    });
    host.then_some("member")
}

/// Gate + classify + record for a `get_member` fallthrough.
fn note_member_miss(e: &JsEngine, o: &Rc<RefCell<Obj>>, key: &str) {
    if !js_miss_enabled() {
        return;
    }
    if let Some(b) = miss_bucket(e, o) {
        note_js_miss(b, key);
    }
}

/// Counts sorted by frequency, then name (deterministic output).
fn ranked_js(counts: &std::collections::HashMap<String, u32>) -> Vec<(&str, u32)> {
    let mut v: Vec<(&str, u32)> = counts.iter().map(|(k, n)| (k.as_str(), *n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    v
}

/// One bucket's report section (pure — exercised directly by tests).
fn report_js_bucket(bucket: &str, counts: &std::collections::HashMap<String, u32>, over: u32) -> String {
    if counts.is_empty() && over == 0 {
        return String::new();
    }
    let mut s = format!("[js-miss] {} distinct missing {bucket} names\n", counts.len());
    if over > 0 {
        s.push_str(&format!(
            "[js-miss] {bucket}: {over} further distinct names over the {JS_MISS_CAP} cap not recorded\n"
        ));
    }
    for (name, n) in ranked_js(counts) {
        s.push_str(&format!("[js-miss] {bucket} {n} {name}\n"));
    }
    s
}

/// The cumulative observed-miss table as text (empty when nothing recorded).
fn js_miss_report() -> String {
    let Ok(m) = js_misses().lock() else {
        return String::new();
    };
    report_js_bucket("global", &m.global, m.over_global)
        + &report_js_bucket("member", &m.member, m.over_member)
}

/// Print the cumulative observed-miss table (no-op unless `MAZDA_MISS` is set).
pub fn dump_misses() {
    if !js_miss_enabled() {
        return;
    }
    let report = js_miss_report();
    if !report.is_empty() {
        eprint!("{report}");
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
            // Fast path: overwrite an existing own data property on a plain
            // object. New keys and accessors take the full path, which checks
            // the prototype for setters and handles array exotics.
            {
                let mut b = o.borrow_mut();
                if matches!(b.kind, ObjKind::Plain(..)) {
                    if let Some((_, slot)) = b.props.iter_mut().find(|(k, _)| k.as_ref() == key) {
                        if !matches!(&*slot, Val::Acc(_)) {
                            *slot = v;
                            return Ok(());
                        }
                    }
                }
            }
            // Proxy: the handler's `set` trap wins, otherwise the write
            // forwards to the target.
            if let Some((target, handler)) = proxy_parts(o) {
                let trap = get_member(e, &handler, "set")?;
                if is_callable(&trap) {
                    e.call(
                        trap,
                        handler.clone(),
                        vec![target, Val::Str(Rc::from(key)), v, Val::Obj(o.clone())],
                    )?;
                    return Ok(());
                }
                return set_member(e, &target, key, v);
            }
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
            // Live host collections are read-only views: `length`/indices
            // have no setter (sloppy mode silently ignores the write), and
            // an own property must not shadow the live lookup.
            if o.try_borrow()
                .map(|b| matches!(b.kind, ObjKind::Live(_)))
                .unwrap_or(false)
            {
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
            // typed arrays: an index write goes through the element
            // conversion into the shared buffer (and never creates an own
            // property on the view)
            if let Ok(b) = o.try_borrow() {
                if let ObjKind::TypedArray(ta) = &b.kind {
                    if let Ok(i) = key.parse::<usize>() {
                        typed_write(ta, i, &v);
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
                        e.proto_of(&c),
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
                cur = f.proto();
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
                None => {
                    // `name`/`length` are non-writable own slots: a sloppy
                    // assignment is a silent no-op until the slot is deleted
                    if (key == "name" || key == "length") && !c.synth_deleted(key) {
                        return Ok(());
                    }
                    ps.push((intern_key(key), v));
                }
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
    // Proxy: `delete` forwards to the target (the trap is not run)
    if let Val::Obj(o) = base {
        if let Some((target, _)) = proxy_parts(o) {
            return delete_member(&target, key);
        }
    }
    match base {
        Val::Obj(o) => {
            let mut b = o.borrow_mut();
            let before = b.props.len();
            b.props.retain(|(k, _)| k.as_ref() != key);
            b.props.len() != before
        }
        // functions are objects too: `delete C.staticMethod` must remove the
        // own property (verifyProperty's configurable check relies on it)
        Val::Func(c) => {
            // deleting a synthesized slot must also hide the synthesized read
            if key == "name" || key == "length" {
                let mut p = c.props.borrow_mut();
                p.retain(|(k, _)| k.as_ref() != key);
                if c.synth_deleted(key) {
                    return true;
                }
                c.mark_synth_deleted(key);
                return true;
            }
            let mut p = c.props.borrow_mut();
            let before = p.len();
            p.retain(|(k, _)| k.as_ref() != key);
            p.len() != before
        }
        _ => false, // primitives: silent false (sloppy)
    }
}

/// `key in v` — shared by the operator and `Reflect.has` so the two cannot
/// drift. Functions answer through their synthesized slots (`name`/`length`/
/// `prototype`) plus the prototype chain; strings count indices and `length`;
/// every other primitive throws the operator's TypeError.
pub(crate) fn has_member(e: &JsEngine, v: &Val, key: &str) -> Result<bool, JsError> {
    // Proxy: `in` forwards to the target (the `has` trap is not run)
    if let Val::Obj(o) = v {
        if let Some((target, _)) = proxy_parts(o) {
            return has_member(e, &target, key);
        }
    }
    Ok(match v {
        // a callable object (`Object`, `Array` are plain objects with a
        // hidden `__call`) is a function for `in` too: it owns `name`/
        // `length` and inherits Function.prototype
        Val::Obj(o) if o.borrow().props.iter().any(|(k, _)| &**k == "__call") => {
            key == "name"
                || key == "length"
                || e.chain_has(o, key)
                || e.function_proto
                    .as_ref()
                    .is_some_and(|fp| e.chain_has(fp, key))
        }
        Val::Obj(o) => e.chain_has(o, key),
        // functions are objects: `"name" in f`, `"prototype" in f`
        Val::Func(c) => {
            let mut found = ((key == "name" || key == "length") && !c.synth_deleted(key))
                || (key == "prototype" && !c.is_arrow)
                || c.props.borrow().iter().any(|(k, _)| k.as_ref() == key);
            let mut cur = c.proto();
            if !found {
                for _ in 0..100 {
                    match cur {
                        Some(Val::Obj(o)) => {
                            if e.chain_has(&o, key) {
                                found = true;
                                break;
                            }
                            cur = e.proto_of(&o).map(Val::Obj);
                        }
                        _ => break,
                    }
                }
            }
            // every function inherits Function.prototype (`"call" in f`)
            found
                || e.function_proto
                    .as_ref()
                    .is_some_and(|fp| e.chain_has(fp, key))
        }
        // `"0" in "abc"` -> true (index/length/method names)
        Val::Str(s) => key == "length" || key.parse::<usize>().ok().and_then(|i| s.chars().nth(i)).is_some(),
        // built-in functions are objects too: every function has `name` and
        // `length`, a constructor's statics live on the native side table,
        // and the rest inherits from Function.prototype
        Val::Native(id) => {
            key == "name"
                || key == "length"
                || e.native_prop(*id, key).is_some()
                || (Some(*id) == e.import_native && key == "meta")
                || e.function_proto
                    .as_ref()
                    .is_some_and(|fp| e.chain_has(fp, key))
        }
        // built-in methods are callable: they answer the Function.prototype
        // surface (`"call" in [].push`), nothing more
        Val::Method(_) => matches!(key, "name" | "length" | "call" | "apply" | "bind" | "toString"),
        _ => {
            return Err(JsError::throw(&format!(
                "TypeError: Cannot use `in` operator on {}",
                v.type_of()
            )))
        }
    })
}

/// JS `ToPrimitive` (default hint): what `+`/`==` do with objects — objects
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
        (BigInt(a), BigInt(b)) => a == b,
        (BigInt(a), Num(b)) | (Num(b), BigInt(a)) => {
            b.is_finite() && b.fract() == 0.0 && **a as f64 == *b
        }
        (BigInt(a), Str(b)) | (Str(b), BigInt(a)) => {
            b.trim().parse::<i128>().map_or(false, |v| v == **a)
        }
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
        (BigInt(a), BigInt(b)) => a == b,
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
