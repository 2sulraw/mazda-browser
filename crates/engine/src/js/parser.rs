//! AST + recursive-descent parser for the JS subset.
//!
//! AST shape: Box-based tree; every node carries a byte `Span` for future
//! error reporting / tooling. shortcut: spans exist but v1 only uses them to
//! report line/col of syntax errors.
//!
//! ASI (semicolon insertion) is approximated: after a statement, `;` is
//! optional if the next token starts on a new line, is `}`, or is EOF.
//! `return`/`throw` only take an expression on the same line (spec-ish).
//! shortcut: `return\n x` returns undefined instead of the spec's exact
//! lookahead rules; good enough for real code.
//!
//! deferred to later waves (all produce a clear SyntaxError, never a panic):
//! destructuring in weird positions, generators, modules, `extends`, static
//! class members, BigInt, labeled statements, computed class keys.
//! Supported: optional chaining `?.`, computed object keys, object-literal
//! getters/setters, `**`/`**=`, spread, destructuring, async/await, regex.

use super::lexer::{tokenize, Kw, Op, Punct, Span, Tok, TplPart, Token};
use super::JsError;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Ident(Rc<str>),
    Num(f64),
    /// `123n` — i128-backed (see the BigInt builtins for the documented range)
    BigInt(i128),
    Str(Rc<str>),
    Bool(bool),
    Null,
    This,
    /// `new.target`: the constructor being run by `new`, or undefined
    NewTarget,
    /// `#field in obj` — private-brand check (our privates are ordinary
    /// `#name` keys, so this is a chain has-property test)
    PrivateIn {
        name: Rc<str>,
        obj: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    /// `a++` / `a--`; target is Ident/Member/Index.
    Postfix {
        target: Box<Expr>,
        inc: bool,
    },
    Binary {
        op: BinOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    /// `op: None` = plain `=`; Some(op) for `a += b` etc.
    Logical {
        op: LogOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    Assign {
        target: Box<Expr>,
        op: Option<BinOp>,
        value: Box<Expr>,
    },
    Cond {
        test: Box<Expr>,
        then: Box<Expr>,
        other: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<CallArg>,
    },
    New {
        callee: Box<Expr>,
        args: Vec<CallArg>,
    },
    /// a.b
    Member {
        base: Box<Expr>,
        key: Rc<str>,
    },
    /// a[b]
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    /// `a?.b.c?.[d]?.()` � one node for the WHOLE chain so a nullish
    /// short-circuit skips every remaining step (a per-step `Member` node
    /// would have to throw on what follows). The first step that carries
    /// `optional` is the one written with `?.`.
    OptionalChain {
        base: Box<Expr>,
        steps: Vec<ChainStep>,
    },
    /// `a ??= b` / `a ||= b` / `a &&= b` � the value expression is evaluated
    /// ONLY when the operator decides to write (short-circuit assignment)
    LogicalAssign {
        target: Box<Expr>,
        op: LogOp,
        value: Box<Expr>,
    },
    /// `class ... { }` in expression position: runs the same lowering as a
    /// class declaration in a private scope and yields the constructor
    ClassExpr {
        name: Rc<str>,
        /// the source wrote `class { ... }` with no name: NamedEvaluation may
        /// still give it the binding name (`var x = class {}` -> `x`)
        anon: bool,
        stmts: Vec<Stmt>,
    },
    /// `super(...)` / `super.m(...)` � resolved at parse time against the
    /// class's hidden superclass binding, so eval only needs the current
    /// `this` from the environment.
    SuperCall {
        class: Box<Expr>,
        /// None = constructor call; Some(key) = superclass method
        key: Option<Rc<str>>,
        /// look the method up on the class itself (static) instead of its
        /// prototype
        static_: bool,
        args: Vec<CallArg>,
    },
    /// `super.m` without a call (returns the unbound function)
    SuperMember {
        class: Box<Expr>,
        key: Rc<str>,
        static_: bool,
    },
    /// elements: Hole = hole, Spread = `...expr`
    Array(Vec<ArrayElem>),
    Object(Vec<ObjElem>),
    /// regex literal `/pat/flags`, compiled once per (pattern, flags) and
    /// cached on the engine — not per evaluation
    Regex {
        pattern: Rc<str>,
        flags: Rc<str>,
    },
    /// `yield` — legal only where the generator walker supports it (plain
    /// expression statement or a declarator initializer)
    Yield(Option<Box<Expr>>),
    /// `yield* iterable` — delegate the iteration to another iterator
    YieldStar(Box<Expr>),
    /// `await expr` — only inside async functions
    Await(Box<Expr>),
    Func(Rc<FnDef>),
    Arrow(Rc<FnDef>),
    /// comma operator / expression lists
    Seq(Vec<Expr>),
    Template(Vec<TplSeg>),
    /// tagged template: `` tag`a${x}b` `` — the tag is called with a frozen-
    /// cookie strings array (`raw` is the cooked text: escape spelling is not
    /// kept) followed by the interpolated values
    TaggedTemplate {
        tag: Box<Expr>,
        segs: Vec<TplSeg>,
    },
    Typeof(Box<Expr>),
    Void(Box<Expr>),
    Delete(Box<Expr>),
    /// destructuring assignment `([a, b] = pair)` — an expression-level
    /// marker so `assign_expr` can return it in expression position
    PatAssign {
        pat: Pat,
        value: Box<Expr>,
    },
}

#[derive(Clone, Debug)]
pub enum TplSeg {
    Lit(Rc<str>),
    Expr(Expr),
}

/// one element of an array literal
#[derive(Clone, Debug)]
pub enum ArrayElem {
    Item(Expr),
    Hole,
    /// `...expr`. The flag records a trailing comma right after the spread
    /// (`[...a,]`): legal in an array literal, an early error once the
    /// literal is used as a destructuring pattern.
    Spread(Expr, bool),
}

/// one postfix step of an optional chain (`?.` may appear on any of them)
#[derive(Clone, Debug)]
pub enum ChainStep {
    Member { key: Rc<str>, optional: bool },
    Index { index: Expr, optional: bool },
    Call {
        args: Vec<CallArg>,
        optional: bool,
    },
    Tagged { segs: Vec<TplSeg> },
}

impl ChainStep {
    pub fn optional(&self) -> bool {
        match self {
            ChainStep::Member { optional, .. }
            | ChainStep::Index { optional, .. }
            | ChainStep::Call { optional, .. } => *optional,
            ChainStep::Tagged { .. } => false,
        }
    }
}

/// one argument, plain or `...spread`
#[derive(Clone, Debug)]
pub enum CallArg {
    Arg(Expr),
    Spread(Expr),
}

/// one property of an object literal
#[derive(Clone, Debug)]
pub enum ObjElem {
    /// key -> value (shorthand `{a}` desugars to Prop("a", Ident("a")))
    Prop(Rc<str>, Expr),
    /// `{x = default}` — the CoverInitializedName form. Only legal when the
    /// enclosing object literal becomes a destructuring pattern; the parser
    /// rejects a surviving CoverInit in value position. `expr_to_pat` turns it
    /// into a `Prop` with a default.
    CoverInit(Rc<str>, Expr),
    /// `...expr`; the flag marks a trailing comma after the spread
    /// (`{...a,}`) — legal in an object literal, an early error in a pattern.
    Spread(Expr, bool),
    /// `[expr]: v` � key evaluated at object-creation time
    Computed { key: Expr, value: Expr },
    /// `get x(){}` / `set x(v){}` (both sides optional when a later literal
    /// defines the other half of the pair)
    Accessor {
        key: Rc<str>,
        getter: Option<Rc<FnDef>>,
        setter: Option<Rc<FnDef>>,
    },
    /// `get [expr](){}` / `set [expr](v){}` — key evaluated at creation time
    ComputedAccessor {
        key: Expr,
        getter: Option<Rc<FnDef>>,
        setter: Option<Rc<FnDef>>,
    },
}

// ---------------------------------------------------------------------------
// Destructuring patterns — desugared at eval time into binds, not into temps
// ---------------------------------------------------------------------------

/// Where a pattern leaf binds a value: a fresh name (declaration forms) or an
/// existing lvalue (assignment destructuring).
#[derive(Clone, Debug)]
pub enum BindTgt {
    Name(Rc<str>),
    /// a.b — base/key evaluate in the scope where the bind happens
    Member {
        base: Box<Expr>,
        key: Rc<str>,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
}

#[derive(Clone, Debug)]
pub enum Pat {
    Name(BindTgt),
    /// `[a, b = 1, [c], ...rest]`; a leading hole is a Hole part
    Arr(Vec<PatPart>),
    /// `{a, b: c, d: {e} = x, ...rest}`
    Obj(Vec<ObjPatPart>),
}

#[derive(Clone, Debug)]
pub enum PatPart {
    /// `[ ,, b]` — skip an element
    Hole,
    Bind(Box<Pat>, Option<Expr>),
    /// `...rest` — must bind a Name/Member/Index leaf (honest restriction)
    Rest(Box<Pat>),
}

#[derive(Clone, Debug)]
pub enum ObjPatPart {
    Prop {
        key: Rc<str>,
        part: PatPart,
    },
    /// `{[expr]: target}` - the key is evaluated when the pattern runs
    ComputedProp {
        key: Expr,
        part: PatPart,
    },
    /// `{...rest}` - remaining own properties
    Rest(Pat),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UnaryOp {
    Neg,
    Pos,
    Not,
    BitNot,
    PreInc,
    PreDec,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    Lt,
    Le,
    Gt,
    Ge,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    In,
    InstanceOf,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogOp {
    And,
    Or,
    Coalesce,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Expr(Box<Expr>),
    VarDecl(VarKind, Vec<Declarator>),
    Block(Vec<Stmt>),
    /// class desugaring emits a list of statements
    List(Vec<Stmt>),
    If {
        test: Box<Expr>,
        then: Box<Stmt>,
        other: Option<Box<Stmt>>,
    },
    While {
        test: Box<Expr>,
        body: Box<Stmt>,
    },
    DoWhile {
        body: Box<Stmt>,
        test: Box<Expr>,
    },
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Box<Expr>>,
        upd: Option<Box<Expr>>,
        body: Box<Stmt>,
    },
    ForIn {
        kind: Option<VarKind>,
        target: ForTarget,
        obj: Box<Expr>,
        body: Box<Stmt>,
    },
    ForOf {
        kind: Option<VarKind>,
        target: ForTarget,
        iter: Box<Expr>,
        body: Box<Stmt>,
    },
    Switch {
        disc: Box<Expr>,
        cases: Vec<SwitchCase>,
    },
    /// `with (obj) stmt` (sloppy mode only; the engine has no strict mode)
    With {
        obj: Box<Expr>,
        body: Box<Stmt>,
    },
    Try {
        block: Vec<Stmt>,
        catch: Option<Box<CatchClause>>,
        finally: Option<Vec<Stmt>>,
    },
    Throw(Box<Expr>),
    Return(Option<Box<Expr>>),
    /// `break;` / `break label;` (and continue likewise); the label is
    /// resolved by the enclosing `Label` statement or labeled loop
    Break(Option<Rc<str>>),
    Continue(Option<Rc<str>>),
    /// `label: stmt` � only observable through `break label` / `continue label`
    Label {
        name: Rc<str>,
        body: Box<Stmt>,
    },
    FuncDecl(Rc<FnDef>),
    /// destructuring assignment `([a, b] = pair)` — produced when an
    /// array/object literal turns out to be an assignment target
    PatAssign {
        pat: Pat,
        value: Box<Expr>,
    },
    /// class accessor desugar: install a getter/setter pair on an object
    /// (`Object`-style, no full property-descriptor model — shortcut)
    DefineAcc {
        owner: Box<Expr>,
        /// boxed: `Stmt` is kept compact, and accessor definitions are rare
        key: Box<ClassKey>,
        getter: Option<Rc<FnDef>>,
        setter: Option<Rc<FnDef>>,
    },
    /// class method desugar: install `func` on `owner[key]` and mark the
    /// property non-enumerable (class methods are not own enumerable props)
    DefineMethod {
        owner: Box<Expr>,
        key: Box<ClassKey>,
        func: Rc<FnDef>,
    },
    /// `import ... from "..."`. Only a module loader resolves these; the
    /// interpreter rejects one so a classic script fails honestly instead of
    /// silently skipping it.
    Import {
        source: Rc<str>,
        bindings: Vec<ImportBinding>,
    },
    /// `export ...`. `names` are `(exported, local)` pairs; `inner` wraps the
    /// declaration for `export <decl>` / `export default`. The interpreter
    /// runs `inner` (the declaration still happens) and the module loader
    /// records the values.
    Export {
        kind: ExportKind,
        names: Vec<(Rc<str>, Rc<str>)>,
        source: Option<Rc<str>>,
        inner: Option<Box<Stmt>>,
    },
}

/// one bound name of an `import` declaration
#[derive(Clone, Debug)]
pub enum ImportBinding {
    /// `import x from "m"` / `import { a as x } from "m"`
    Named { imported: Rc<str>, local: Rc<str> },
    /// `import * as ns from "m"`
    Namespace { local: Rc<str> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExportKind {
    /// `export <decl>` - export every name the declaration binds
    Decl,
    /// `export default ...`
    Default,
    /// `export { a, b as c }` (optionally `from "m"`)
    Named,
    /// `export * from "m"`
    All,
}

/// loop-variable binding target of for-in/for-of
#[derive(Clone, Debug)]
pub enum ForTarget {
    Name(Rc<str>),
    Pat(Pat),
}

#[derive(Clone, Debug)]
pub struct SwitchCase {
    /// None = `default`
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub struct CatchClause {
    pub name: Rc<str>,
    /// destructuring catch parameter (`catch ([a, b])`); `name` is a hidden
    /// placeholder then
    pub pat: Option<Pat>,
    pub block: Vec<Stmt>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

/// var/let/const declarator: `name` for plain names, `pat` for destructuring
/// `let {a, b} = x` (name is ignored when pat is present; RHS value binds all
/// pattern leaves)
#[derive(Clone, Debug)]
pub struct Declarator {
    pub name: Rc<str>,
    pub pat: Option<Pat>,
    pub init: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct FnDef {
    pub name: Rc<str>,
    pub params: Vec<Param>,
    pub body: FnBody,
    /// parsed lazily-computed: does the body reference `arguments` (outside
    /// nested functions)? If so the interpreter materializes an arguments
    /// object per call; otherwise we skip that per-call allocation (RAM).
    pub uses_arguments: bool,
    /// `async function` — calls return a promise; `await` legal in body
    pub is_async: bool,
    /// `function*` — body executes on a resumable walker (see gen.rs)
    pub is_generator: bool,
    /// parse-time rollup: does the body contain a `yield` in an evaluable
    /// position? Gates the cheap walk when no generator machinery is needed.
    pub has_yield: bool,
    /// engine source id this function was parsed from (see
    /// [`JsEngine::register_source`]). A function can be called from a script
    /// or module other than the one that defined it, so this is the only way
    /// a runtime error can report a location in the right file. 0 = unknown
    /// (dynamically built functions such as `new Function`).
    pub src: u32,
}

impl FnDef {
    /// The function's `length`: the number of formal parameters before the
    /// first one with a default initializer; a rest parameter never counts.
    pub fn length(&self) -> usize {
        self.params
            .iter()
            .take_while(|p| p.default.is_none() && !p.rest)
            .count()
    }
}

#[derive(Clone, Debug)]
pub struct Param {
    /// hidden env name for the received argument; for destructuring params it
    /// is a generated non-lexable `#k` name (cannot collide with source idents)
    pub name: Rc<str>,
    /// `Some` when written `{a, b}` / `[x, y]`; the value is destructured
    /// after the param env is built. `rest` params bind remaining args.
    /// Boxed: `Pat`/`Expr` are much larger than the common no-default,
    /// no-pattern parameter, and params are cloned into every function
    /// definition.
    pub pat: Option<Box<Pat>>,
    pub default: Option<Box<Expr>>,
    pub rest: bool,
}

#[derive(Clone, Debug)]
pub enum FnBody {
    Block(Vec<Stmt>),
    Expr(Expr),
    /// runtime-only, produced by `Function.prototype.bind` — never parsed
    Bound {
        target: super::Val,
        this: super::Val,
        args: Vec<super::Val>,
    },
}

pub type Program = Vec<Stmt>;

/// Does a function body's directive prologue contain `"use strict"`? Only the
/// leading run of expression statements made of string literals counts.
fn has_use_strict(body: &[Stmt]) -> bool {
    for st in body {
        match &st.kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Str(s) => {
                    if &**s == "use strict" {
                        return true;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
    }
    false
}

/// Always-reserved words. `await`/`yield`/`let`/`static`/`async` are
/// contextual and deliberately absent: sloppy code may use them as names.
/// Escaped spellings (`\u0062reak`) decode to the same StringValue, so this
/// predicate catches both forms.
fn is_reserved_word(s: &str) -> bool {
    matches!(
        s,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
    )
}

/// NamedEvaluation for a binding initializer: `fn = function () {}`,
/// `gen = function* () {}` and `arrow = () => {}` give the anonymous
/// function the bound name, so `fn.name === 'fn'`. Applied at parse time,
/// which covers the literal shapes real code uses; an initializer that is a
/// parenthesized/computed expression keeps its empty name.
fn name_anon_fn(name: &str, init: &mut Expr) {
    // anonymous class expression: `var x = class {}` gives it the name `x`.
    // Its constructor and prototype references were lowered under a synthetic
    // name, so rewrite that scaffolding (bodies are user code and stay put).
    if let ExprKind::ClassExpr { name: cname, anon, stmts } = &mut init.kind {
        if *anon {
            let new_name = Rc::from(name);
            let old = cname.clone();
            for s in stmts.iter_mut() {
                rename_class_scaffold_stmt(&mut s.kind, &old, &new_name);
            }
            *cname = new_name;
        }
        return;
    }
    let (def, is_arrow) = match &init.kind {
        ExprKind::Func(f) => (f.clone(), false),
        ExprKind::Arrow(f) => (f.clone(), true),
        _ => return,
    };
    if !def.name.is_empty() {
        return;
    }
    let renamed = Rc::new(FnDef {
        name: Rc::from(name),
        ..(*def).clone()
    });
    init.kind = if is_arrow {
        ExprKind::Arrow(renamed)
    } else {
        ExprKind::Func(renamed)
    };
}

/// Rewrite the generated class-name references in one lowered statement.
/// Only the class scaffolding is touched; method/constructor bodies (user
/// code) are opaque because an anonymous class's synthetic name is not
/// visible to them.
fn rename_class_scaffold_stmt(kind: &mut StmtKind, old: &Rc<str>, new_name: &Rc<str>) {
    match kind {
        StmtKind::FuncDecl(f) => {
            if f.name == *old {
                *f = Rc::new(FnDef {
                    name: new_name.clone(),
                    ..(**f).clone()
                });
            }
        }
        StmtKind::Expr(e) => rename_class_scaffold_expr(e, old, new_name),
        StmtKind::DefineMethod { owner, .. } | StmtKind::DefineAcc { owner, .. } => {
            rename_class_scaffold_expr(owner, old, new_name);
        }
        StmtKind::Block(b) | StmtKind::List(b) => {
            for s in b.iter_mut() {
                rename_class_scaffold_stmt(&mut s.kind, old, new_name);
            }
        }
        _ => {}
    }
}

fn rename_class_scaffold_expr(e: &mut Expr, old: &Rc<str>, new_name: &Rc<str>) {
    match &mut e.kind {
        ExprKind::Ident(n) => {
            if n == old {
                *n = new_name.clone();
            }
        }
        ExprKind::Member { base, .. } => rename_class_scaffold_expr(base, old, new_name),
        ExprKind::Index { base, index } => {
            rename_class_scaffold_expr(base, old, new_name);
            rename_class_scaffold_expr(index, old, new_name);
        }
        ExprKind::Call { callee, args } => {
            rename_class_scaffold_expr(callee, old, new_name);
            for a in args {
                match a {
                    CallArg::Arg(x) | CallArg::Spread(x) => {
                        rename_class_scaffold_expr(x, old, new_name)
                    }
                }
            }
        }
        // static field assignment target; the initializer is user code
        ExprKind::Assign { target, .. } => rename_class_scaffold_expr(target, old, new_name),
        _ => {}
    }
}

/// What a `class` body accumulates. Lowered to statements once the whole
/// body is parsed, in source order per list (instance members and static
/// members keep their own relative order).
/// A class member name: a literal key or `[expr]`.
#[derive(Clone, Debug)]
pub(crate) enum ClassKey {
    Name(Rc<str>),
    Computed(Expr),
}

#[derive(Clone, Debug)]
enum ClassItem {
    Method(ClassKey, Rc<FnDef>),
    Accessor(ClassKey, Option<Rc<FnDef>>, Option<Rc<FnDef>>),
    /// field: `Class.name = init` (static) / `this.name = init` (instance)
    Field(ClassKey, Option<Expr>),
}

/// Class context while parsing a method body: where `super` points and
/// whether the current member is static (look `super.x` up on the class
/// itself rather than on its prototype).
#[derive(Clone)]
struct ClassCtx {
    super_name: Option<Rc<str>>,
    is_static: bool,
    /// private names declared in this class body (`#x`, `#m`)
    private_names: Vec<Rc<str>>,
    /// subset of `private_names` that came from get/set accessors: a getter
    /// and setter may share one private name, a field or method may not
    private_accessor_names: Vec<Rc<str>>,
    /// private references parsed while this class is innermost; checked once
    /// the whole class body is known (forward references are legal)
    private_refs: Vec<Rc<str>>,
    /// true while parsing the constructor body: `super()` is only legal there
    in_constructor: bool,
}

/// Function context while parsing a body: `await` is a reserved word only
/// inside async functions and `yield` only inside generators. Escaped
/// spellings (`\u0061wait`) lex as identifiers, so they need this context to
/// be rejected.
#[derive(Clone, Copy, Default)]
struct FnCtx {
    is_async: bool,
    is_generator: bool,
}

/// One StatementList's declaration bookkeeping for the early error
/// "LexicallyDeclaredNames of StatementList must not intersect
/// VarDeclaredNames, and must not contain duplicates" (blocks, switch
/// CaseBlocks, function bodies and the program).
struct BlockScope {
    lex: Vec<Rc<str>>,
    vars: Vec<Rc<str>>,
    /// program / function body: `var` names stop here and a function
    /// declaration at this level is var-scoped
    top: bool,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub fn parse(source: &str, src_id: u32) -> Result<Program, JsError> {
    let toks = tokenize(source).map_err(|le| {
        let (line, col) = line_col(source, le.offset as usize);
        JsError::Syntax {
            msg: le.msg,
            line,
            col,
        }
    })?;
    let mut p = Parser {
        src: source,
        src_id,
        toks,
        pos: 0,
        depth: 0,
        class_stack: Vec::new(),
        fn_ctx: Vec::new(),
        no_in: false,
        block_scopes: Vec::new(),
        cover_depth: 0,
    };
    p.program()
}

/// byte offset -> 1-based (line, col); col counted in chars
pub(crate) fn line_col(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let mut line = 1u32;
    let mut line_start = 0usize;
    for (i, b) in source.as_bytes().iter().enumerate().take(offset) {
        if *b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let col = source[line_start..offset].chars().count() as u32 + 1;
    (line, col)
}

struct Parser<'a> {
    src: &'a str,
    /// engine source id stamped onto every `FnDef` this parse produces
    src_id: u32,
    toks: Vec<Token>,
    pos: usize,
    /// recursion guard: pathological nesting (`((((...))))`, `[[[[...]]]]`,
    /// `{{{{...}}}}`) must be a SyntaxError, not a native stack overflow
    depth: usize,
    /// enclosing class contexts (innermost last); `super` resolves against
    /// the top entry
    class_stack: Vec<ClassCtx>,
    /// enclosing function contexts (innermost last): which escaped names are
    /// reserved words here
    fn_ctx: Vec<FnCtx>,
    /// inside a `for` head: `in` is part of the head grammar, so it must not
    /// be consumed as the binary operator (cleared inside parentheses)
    no_in: bool,
    /// open StatementLists, innermost last, for the lexical/var redeclaration
    /// early error
    block_scopes: Vec<BlockScope>,
    /// >0 while parsing a position where a CoverInitializedName may still be
    /// resolved into a pattern (parentheses, array elements, object property
    /// values, a for-in/of head). A CoverInit that survives to an
    /// `assign_expr` at depth 0 is a SyntaxError.
    cover_depth: usize,
}

/// nesting cap; deep enough for any real page, shallow enough to stay far
/// from the native stack limit
const MAX_NEST_DEPTH: usize = 400;

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn cur(&self) -> &Token {
        &self.toks[self.pos]
    }

    fn next(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn eat_punct(&mut self, p: Punct) -> bool {
        if *self.peek() == Tok::Punct(p) {
            self.next();
            true
        } else {
            false
        }
    }

    fn eat_op(&mut self, o: Op) -> bool {
        if *self.peek() == Tok::Op(o) {
            self.next();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: Kw) -> bool {
        if *self.peek() == Tok::Kw(k) {
            self.next();
            true
        } else {
            false
        }
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, JsError> {
        let tok = self.cur();
        let (line, col) = line_col(self.src, tok.start as usize);
        Err(JsError::Syntax {
            msg: msg.into(),
            line,
            col,
        })
    }

    /// Early error: `"use strict"` in the directive prologue of a function
    /// whose parameter list is not simple (default, rest or destructuring).
    fn check_strict_body(&self, params: &[Param], body: &[Stmt]) -> Result<(), JsError> {
        if params
            .iter()
            .any(|p| p.pat.is_some() || p.default.is_some() || p.rest)
            && has_use_strict(body)
        {
            return self.err(
                "illegal 'use strict' directive in a function with a non-simple parameter list",
            );
        }
        Ok(())
    }

    /// Arrow-function form of [`Parser::check_strict_body`] (a concise body
    /// has no directive prologue).
    fn check_strict_arrow(&self, params: &[Param], body: &FnBody) -> Result<(), JsError> {
        if let FnBody::Block(stmts) = body {
            self.check_strict_body(params, stmts)?;
        }
        Ok(())
    }

    fn expect_punct(&mut self, p: Punct) -> Result<(), JsError> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            self.err(format!("expected `{:?}`, found `{:?}`", p, self.peek()))
        }
    }

    /// Reject an escaped identifier that is a reserved word in the given
    /// function context (`\u0061wait` in async code, `\u0079ield` in a
    /// generator). Unescaped spellings lex as keywords and never reach here.
    fn check_reserved_in(&self, s: &str, ctx: FnCtx) -> Result<(), JsError> {
        if ctx.is_async && s == "await" {
            return self.err("`await` is not a valid identifier in an async function");
        }
        if ctx.is_generator && s == "yield" {
            return self.err("`yield` is not a valid identifier in a generator function");
        }
        Ok(())
    }

    fn check_ctx_ident(&self, s: &str) -> Result<(), JsError> {
        self.check_reserved_in(s, self.fn_ctx.last().copied().unwrap_or_default())
    }

    /// Record a private member declaration (`#x` / `#m`). A duplicate name in
    /// the same class body is an early error; the two halves of a private
    /// accessor are the one exception.
    fn note_private_decl(&mut self, key: &ClassKey, is_accessor: bool) -> Result<(), JsError> {
        let ClassKey::Name(k) = key else {
            return Ok(());
        };
        if !k.starts_with('#') {
            return Ok(());
        }
        if let Some(ctx) = self.class_stack.last_mut() {
            let taken = ctx.private_names.iter().any(|n| n == k);
            if taken && !(is_accessor && ctx.private_accessor_names.iter().any(|n| n == k)) {
                return self.err(format!("private name `{k}` is declared more than once"));
            }
            if !taken {
                ctx.private_names.push(k.clone());
            }
            if is_accessor && !ctx.private_accessor_names.iter().any(|n| n == k) {
                ctx.private_accessor_names.push(k.clone());
            }
        }
        Ok(())
    }

    /// Record a private reference (`obj.#x`, `#x in obj`). It is validated when
    /// the enclosing class body ends.
    fn note_private_ref(&mut self, name: &str) -> Result<(), JsError> {
        if !name.starts_with('#') {
            return Ok(());
        }
        match self.class_stack.last_mut() {
            Some(ctx) => {
                ctx.private_refs.push(Rc::from(name));
                Ok(())
            }
            None => self.err("a private name is only valid inside a class"),
        }
    }

    /// True when the identifier token at `pos` was written with a `\u` escape.
    /// Escaped spellings never lex as keywords, so `\u0067et` / `\u0061sync`
    /// are ordinary property names, not accessor/modifier keywords.
    fn ident_escaped(&self, pos: usize) -> bool {
        match self.toks.get(pos) {
            Some(t) if matches!(t.tok, Tok::Ident(_)) => self
                .src
                .get(t.start as usize..t.end as usize)
                .is_some_and(|s| s.contains('\\')),
            _ => false,
        }
    }

    fn expect_ident(&mut self) -> Result<Rc<str>, JsError> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                // an escaped identifier whose StringValue is a reserved word is
                // still a ReservedWord and cannot be used as an Identifier
                if is_reserved_word(&s) {
                    return self.err(format!("`{s}` is a reserved word"));
                }
                self.check_ctx_ident(&s)?;
                self.next();
                Ok(s)
            }
            _ => self.err(format!("expected identifier, found `{:?}`", self.peek())),
        }
    }

    /// an identifier or reserved word usable as a property/method name
    fn prop_name(&mut self) -> Result<Rc<str>, JsError> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.next();
                Ok(s)
            }
            Tok::Kw(k) => {
                self.next();
                Ok(Rc::from(format!("{:?}", k).to_lowercase()))
            }
            Tok::Str(s) => {
                self.next();
                Ok(s)
            }
            Tok::Num(n) => {
                self.next();
                Ok(Rc::from(num_key(n)))
            }
            _ => self.err(format!("expected property name, found `{:?}`", self.peek())),
        }
    }

    fn span_from(&self, start_tok: usize) -> Span {
        let start = self.toks[start_tok].start;
        let end = self.toks[self.pos.saturating_sub(1)].end.max(start);
        Span { start, end }
    }

    fn program(&mut self) -> Result<Program, JsError> {
        self.block_scopes.push(BlockScope {
            lex: Vec::new(),
            vars: Vec::new(),
            top: true,
        });
        let mut out = Vec::new();
        while *self.peek() != Tok::Eof {
            match self.statement() {
                Ok(s) => out.push(s),
                Err(e) => {
                    self.block_scopes.pop();
                    return Err(e);
                }
            }
        }
        let scope = self.block_scopes.pop().expect("program scope");
        self.check_block_scope(&scope)?;
        Ok(out)
    }

    /// Consume an optional statement terminator (explicit `;` or ASI).
    fn finish_stmt(&mut self) -> Result<(), JsError> {
        if self.eat_punct(Punct::Semi) {
            return Ok(());
        }
        let t = self.cur();
        if t.nl || t.tok == Tok::Punct(Punct::RBrace) || t.tok == Tok::Eof {
            return Ok(()); // automatic semicolon insertion
        }
        self.err(format!("expected `;` before `{:?}`", t.tok))
    }

    /// optional label after `break`/`continue` (must be on the same line)
    fn stmt_label(&mut self) -> Result<Option<Rc<str>>, JsError> {
        if self.cur().nl {
            return Ok(None);
        }
        match self.peek() {
            Tok::Ident(name) => {
                let name = name.clone();
                self.next();
                Ok(Some(name))
            }
            _ => Ok(None),
        }
    }

    /// contextual keyword (`from`, `as`): an identifier token with this text
    fn eat_word(&mut self, w: &str) -> bool {
        let hit = matches!(self.peek(), Tok::Ident(n) if n.as_ref() == w);
        if hit {
            self.next();
        }
        hit
    }

    fn expect_string(&mut self) -> Result<Rc<str>, JsError> {
        match self.peek().clone() {
            Tok::Str(s) => {
                self.next();
                Ok(s)
            }
            _ => self.err(format!("expected a string literal, found `{:?}`", self.peek())),
        }
    }

    /// `{ a, b as c }` -> `(source name, alias)`; imports read it as
    /// (imported, local) and exports as (local, exported)
    fn named_specifiers(&mut self) -> Result<Vec<(Rc<str>, Rc<str>)>, JsError> {
        self.expect_punct(Punct::LBrace)?;
        let mut out = Vec::new();
        while *self.peek() != Tok::Punct(Punct::RBrace) {
            let first = self.prop_name()?;
            let second = if self.eat_word("as") {
                Some(self.prop_name()?)
            } else {
                None
            };
            out.push(match second {
                Some(local) => (first, local),
                None => (first.clone(), first),
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(out)
    }

    /// `import` already consumed.
    fn import_rest(&mut self) -> Result<StmtKind, JsError> {
        let mut bindings = Vec::new();
        let source: Rc<str>;
        if let Tok::Str(s) = self.peek().clone() {
            // `import "m"` - run for side effects only
            self.next();
            source = s;
        } else {
            if *self.peek() == Tok::Op(Op::Mul) {
                self.next();
                bindings.push(ImportBinding::Namespace {
                    local: self.import_ns_name()?,
                });
            } else if *self.peek() == Tok::Punct(Punct::LBrace) {
                for (imported, local) in self.named_specifiers()? {
                    bindings.push(ImportBinding::Named { imported, local });
                }
            } else {
                let local = self.expect_ident()?;
                bindings.push(ImportBinding::Named {
                    imported: Rc::from("default"),
                    local,
                });
                if self.eat_punct(Punct::Comma) {
                    if *self.peek() == Tok::Op(Op::Mul) {
                        self.next();
                        bindings.push(ImportBinding::Namespace {
                            local: self.import_ns_name()?,
                        });
                    } else {
                        for (imported, local) in self.named_specifiers()? {
                            bindings.push(ImportBinding::Named { imported, local });
                        }
                    }
                }
            }
            if !self.eat_word("from") {
                return self.err("expected `from` in import declaration");
            }
            source = self.expect_string()?;
        }
        self.finish_stmt()?;
        Ok(StmtKind::Import { source, bindings })
    }

    fn import_ns_name(&mut self) -> Result<Rc<str>, JsError> {
        if !self.eat_word("as") {
            return self.err("expected `as` after `*` in import");
        }
        self.expect_ident()
    }

    /// `export` already consumed.
    fn export_rest(&mut self) -> Result<StmtKind, JsError> {
        if self.eat_kw(Kw::Default) {
            let (inner, local) = self.export_default_rest()?;
            return Ok(StmtKind::Export {
                kind: ExportKind::Default,
                names: vec![(Rc::from("default"), local)],
                source: None,
                inner: Some(Box::new(inner)),
            });
        }
        if *self.peek() == Tok::Op(Op::Mul) {
            self.next();
            if !self.eat_word("from") {
                return self.err("expected `from` after `export *`");
            }
            let source = self.expect_string()?;
            self.finish_stmt()?;
            return Ok(StmtKind::Export {
                kind: ExportKind::All,
                names: Vec::new(),
                source: Some(source),
                inner: None,
            });
        }
        if *self.peek() == Tok::Punct(Punct::LBrace) {
            let names: Vec<(Rc<str>, Rc<str>)> = self
                .named_specifiers()?
                .into_iter()
                // specifier order is (local/source name, exported alias) but the
                // Export node stores (exported, local)
                .map(|(local, exported)| (exported, local))
                .collect();
            let source = if self.eat_word("from") {
                Some(self.expect_string()?)
            } else {
                None
            };
            self.finish_stmt()?;
            return Ok(StmtKind::Export {
                kind: ExportKind::Named,
                names,
                source,
                inner: None,
            });
        }
        let (inner, names) = self.export_decl_rest()?;
        Ok(StmtKind::Export {
            kind: ExportKind::Decl,
            names,
            source: None,
            inner: Some(Box::new(inner)),
        })
    }

    /// `export default <expr | function | class>`; returns the statement and
    /// the local name the value lives under.
    fn export_default_rest(&mut self) -> Result<(Stmt, Rc<str>), JsError> {
        let start = self.pos;
        let is_async_fn =
            *self.peek() == Tok::Kw(Kw::Async) && *self.peek_at(1) == Tok::Kw(Kw::Function);
        if is_async_fn || *self.peek() == Tok::Kw(Kw::Function) {
            if is_async_fn {
                self.next(); // async
            }
            self.next(); // function
            let is_gen = self.eat_op(Op::Mul);
            let f = self.function_rest(false, is_async_fn, is_gen)?;
            let (f, name) = if f.name.is_empty() {
                let name: Rc<str> =
                    Rc::from(format!("__default{}", next_class_id()).as_str());
                (
                    Rc::new(FnDef {
                        src: self.src_id,
                        name: name.clone(),
                        ..(*f).clone()
                    }),
                    name,
                )
            } else {
                (f.clone(), f.name.clone())
            };
            return Ok((
                Stmt {
                    kind: StmtKind::FuncDecl(f),
                    span: self.span_from(start),
                },
                name,
            ));
        }
        if *self.peek() == Tok::Kw(Kw::Class) {
            self.next();
            let name = match self.peek().clone() {
                Tok::Ident(n) => {
                    self.next();
                    n
                }
                _ => Rc::from(format!("__default{}", next_class_id()).as_str()),
            };
            let stmts = self.class_rest_named(name.clone())?;
            return Ok((
                Stmt {
                    kind: StmtKind::List(stmts),
                    span: self.span_from(start),
                },
                name,
            ));
        }
        let e = self.expr()?;
        self.finish_stmt()?;
        let name: Rc<str> = Rc::from(format!("__default{}", next_class_id()).as_str());
        Ok((
            Stmt {
                kind: StmtKind::VarDecl(
                    VarKind::Const,
                    vec![Declarator {
                        name: name.clone(),
                        pat: None,
                        init: Some(e),
                    }],
                ),
                span: self.span_from(start),
            },
            name,
        ))
    }

    /// `export const/let/var`, `export function ...`, `export class ...`
    fn export_decl_rest(&mut self) -> Result<(Stmt, Vec<(Rc<str>, Rc<str>)>), JsError> {
        let start = self.pos;
        let decl_kind = match self.peek() {
            Tok::Kw(Kw::Var) => Some(VarKind::Var),
            Tok::Kw(Kw::Let) => Some(VarKind::Let),
            Tok::Kw(Kw::Const) => Some(VarKind::Const),
            _ => None,
        };
        if let Some(k) = decl_kind {
            self.next();
            let ds = self.declarators()?;
            let mut leaves = Vec::new();
            for d in &ds {
                match &d.pat {
                    Some(p) => pat_binding_names(p, &mut leaves),
                    None => leaves.push(d.name.clone()),
                }
            }
            let names = leaves.into_iter().map(|n| (n.clone(), n)).collect();
            return Ok((
                Stmt {
                    kind: StmtKind::VarDecl(k, ds),
                    span: self.span_from(start),
                },
                names,
            ));
        }
        let is_async_fn =
            *self.peek() == Tok::Kw(Kw::Async) && *self.peek_at(1) == Tok::Kw(Kw::Function);
        if is_async_fn || *self.peek() == Tok::Kw(Kw::Function) {
            if is_async_fn {
                self.next(); // async
            }
            self.next(); // function
            let is_gen = self.eat_op(Op::Mul);
            let f = self.function_rest(true, is_async_fn, is_gen)?;
            let name = f.name.clone();
            return Ok((
                Stmt {
                    kind: StmtKind::FuncDecl(f),
                    span: self.span_from(start),
                },
                vec![(name.clone(), name)],
            ));
        }
        if *self.peek() == Tok::Kw(Kw::Class) {
            self.next();
            let name = self.expect_ident()?;
            let stmts = self.class_rest_named(name.clone())?;
            return Ok((
                Stmt {
                    kind: StmtKind::List(stmts),
                    span: self.span_from(start),
                },
                vec![(name.clone(), name)],
            ));
        }
        self.err("expected a declaration after `export`")
    }

    // -- statements ---------------------------------------------------------

    fn statement(&mut self) -> Result<Stmt, JsError> {
        self.depth += 1;
        if self.depth > MAX_NEST_DEPTH {
            return self.err("statement nesting too deep");
        }
        let r = self.statement_inner();
        self.depth -= 1;
        r
    }

    fn statement_inner(&mut self) -> Result<Stmt, JsError> {
        let start = self.pos;
        let kind = match self.peek().clone() {
            Tok::Punct(Punct::Semi) => {
                self.next();
                StmtKind::Expr(Box::new(Expr {
                    kind: ExprKind::Null, // empty statement: evaluates to null, discarded
                    span: self.span_from(start),
                }))
            }
            Tok::Punct(Punct::LBrace) => {
                self.next();
                let block = self.block_body()?;
                StmtKind::Block(block)
            }
            Tok::Kw(Kw::Var) => {
                self.next();
                let ds = self.declarators()?;
                self.note_declarators(VarKind::Var, &ds);
                StmtKind::VarDecl(VarKind::Var, ds)
            }
            Tok::Kw(Kw::Let) => {
                self.next();
                let ds = self.declarators()?;
                self.note_declarators(VarKind::Let, &ds);
                StmtKind::VarDecl(VarKind::Let, ds)
            }
            Tok::Kw(Kw::Const) => {
                self.next();
                let ds = self.declarators()?;
                self.note_declarators(VarKind::Const, &ds);
                StmtKind::VarDecl(VarKind::Const, ds)
            }
            Tok::Kw(Kw::Function) => {
                self.next();
                let is_gen = matches!(self.peek(), Tok::Op(Op::Mul));
                if is_gen {
                    self.next(); // `*`
                }
                let f = self.function_rest(true, false, is_gen)?;
                self.declare_fn(&f.name);
                StmtKind::FuncDecl(f)
            }
            Tok::Kw(Kw::Async) => {
                if *self.peek_at(1) == Tok::Kw(Kw::Function) {
                    self.next(); // async
                    self.next(); // function
                    // `async function*` parses as a generator: the resumable
                    // walker runs the body blocking (see gen.rs docs)
                    let is_gen = self.eat_op(Op::Mul);
                    let f = self.function_rest(true, true, is_gen)?;
                    self.declare_fn(&f.name);
                    StmtKind::FuncDecl(f)
                } else {
                    // async arrow / plain expression starting with `async`
                    let e = self.expr()?;
                    self.finish_stmt()?;
                    StmtKind::Expr(Box::new(e))
                }
            }
            Tok::Kw(Kw::If) => {
                self.next();
                self.expect_punct(Punct::LParen)?;
                let test = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                let then = Box::new(self.statement()?);
                let other = if self.eat_kw(Kw::Else) {
                    Some(Box::new(self.statement()?))
                } else {
                    None
                };
                StmtKind::If {
                    test: Box::new(test),
                    then,
                    other,
                }
            }
            Tok::Kw(Kw::While) => {
                self.next();
                self.expect_punct(Punct::LParen)?;
                let test = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.statement()?);
                StmtKind::While {
                    test: Box::new(test),
                    body,
                }
            }
            Tok::Kw(Kw::Do) => {
                self.next();
                let body = Box::new(self.statement()?);
                if !self.eat_kw(Kw::While) {
                    return self.err("expected `while` after do-body");
                }
                self.expect_punct(Punct::LParen)?;
                let test = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                self.eat_punct(Punct::Semi); // optional per spec
                StmtKind::DoWhile {
                    body,
                    test: Box::new(test),
                }
            }
            Tok::Kw(Kw::For) => {
                self.next();
                // `for await (...)` parses as plain `for-of` (shortcut: values
                // are not awaited; iterable = array/string/Set/Map)
                if *self.peek() == Tok::Kw(Kw::Await) {
                    self.next();
                }
                self.expect_punct(Punct::LParen)?;
                let f = self.for_rest()?;
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.statement()?);
                match f {
                    ForHead::Classic { init, cond, upd } => StmtKind::For {
                        init,
                        cond: cond.map(Box::new),
                        upd: upd.map(Box::new),
                        body,
                    },
                    ForHead::In { kind, target, obj } => StmtKind::ForIn {
                        kind,
                        target,
                        obj: Box::new(obj),
                        body,
                    },
                    ForHead::Of { kind, target, iter } => StmtKind::ForOf {
                        kind,
                        target,
                        iter: Box::new(iter),
                        body,
                    },
                }
            }
            Tok::Kw(Kw::Switch) => {
                self.next();
                self.expect_punct(Punct::LParen)?;
                let disc = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                self.expect_punct(Punct::LBrace)?;
                // all cases share one lexical scope (CaseBlock early error)
                self.block_scopes.push(BlockScope {
                    lex: Vec::new(),
                    vars: Vec::new(),
                    top: false,
                });
                let mut cases = Vec::new();
                while !self.eat_punct(Punct::RBrace) {
                    let test = if self.eat_kw(Kw::Case) {
                        let e = self.expr()?;
                        self.expect_punct(Punct::Colon)?;
                        Some(e)
                    } else if self.eat_kw(Kw::Default) {
                        self.expect_punct(Punct::Colon)?;
                        None
                    } else {
                        return self.err("expected `case` or `default` in switch");
                    };
                    let mut body = Vec::new();
                    while !matches!(
                        self.peek(),
                        Tok::Kw(Kw::Case) | Tok::Kw(Kw::Default) | Tok::Punct(Punct::RBrace)
                    ) {
                        body.push(self.statement()?);
                    }
                    cases.push(SwitchCase { test, body });
                }
                let scope = self.block_scopes.pop().expect("switch scope");
                self.check_block_scope(&scope)?;
                StmtKind::Switch {
                    disc: Box::new(disc),
                    cases,
                }
            }
            Tok::Kw(Kw::With) => {
                self.next();
                self.expect_punct(Punct::LParen)?;
                let obj = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.statement()?);
                StmtKind::With {
                    obj: Box::new(obj),
                    body,
                }
            }
            Tok::Kw(Kw::Try) => {
                self.next();
                self.expect_punct(Punct::LBrace)?;
                let block = self.block_body()?;
                let mut catch = None;
                let mut finally = None;
                if self.eat_kw(Kw::Catch) {
                    // `catch { }` (no binding) is legal since ES2019; the
                    // hidden non-lexable name keeps the block shape uniform
                    let (name, pat) = if self.eat_punct(Punct::LParen) {
                        let (n, p) = match self.peek() {
                            Tok::Punct(Punct::LBrack) | Tok::Punct(Punct::LBrace) => {
                                (Rc::from("#catch"), Some(self.parse_pattern(true)?))
                            }
                            _ => (self.expect_ident()?, None),
                        };
                        self.expect_punct(Punct::RParen)?;
                        (n, p)
                    } else {
                        (Rc::from("#catch"), None)
                    };
                    self.expect_punct(Punct::LBrace)?;
                    let cbody = self.block_body()?;
                    catch = Some(Box::new(CatchClause {
                        name,
                        pat,
                        block: cbody,
                    }));
                }
                if self.eat_kw(Kw::Finally) {
                    self.expect_punct(Punct::LBrace)?;
                    finally = Some(self.block_body()?);
                }
                if catch.is_none() && finally.is_none() {
                    return self.err("`try` requires `catch` or `finally`");
                }
                StmtKind::Try {
                    block,
                    catch,
                    finally,
                }
            }
            Tok::Kw(Kw::Throw) => {
                self.next();
                if self.cur().nl {
                    return self.err("illegal newline after `throw`");
                }
                let e = self.expr()?;
                self.finish_stmt()?;
                StmtKind::Throw(Box::new(e))
            }
            Tok::Kw(Kw::Return) => {
                self.next();
                let v = if self.cur().nl
                    || *self.peek() == Tok::Punct(Punct::Semi)
                    || *self.peek() == Tok::Punct(Punct::RBrace)
                    || *self.peek() == Tok::Eof
                {
                    None
                } else {
                    Some(self.expr()?)
                };
                self.finish_stmt()?;
                StmtKind::Return(v.map(Box::new))
            }
            Tok::Kw(Kw::Break) => {
                self.next();
                let label = self.stmt_label()?;
                self.finish_stmt()?;
                StmtKind::Break(label)
            }
            Tok::Kw(Kw::Continue) => {
                self.next();
                let label = self.stmt_label()?;
                self.finish_stmt()?;
                StmtKind::Continue(label)
            }
            Tok::Kw(Kw::Class) => {
                self.next();
                let name = self.expect_ident()?;
                self.declare_lexical(&name);
                let list = self.class_rest_named(name)?;
                return Ok(Stmt {
                    kind: StmtKind::List(list),
                    span: self.span_from(start),
                });
            }
            // `import` / `export` (contextual words: neither is lexed as a
            // keyword so they stay usable as identifiers elsewhere).
            // `import(`/`import.` are NOT declarations: those fall through to
            // the expression path. `import(...)` is the dynamic-import native
            // the embedder installs (a real Promise); `import.meta` reads
            // undefined. Parsing them here would kill the whole script instead.
            Tok::Ident(name)
                if name.as_ref() == "import"
                    && !matches!(
                        *self.peek_at(1),
                        Tok::Punct(Punct::LParen) | Tok::Punct(Punct::Dot)
                    ) =>
            {
                self.next();
                return Ok(Stmt {
                    kind: self.import_rest()?,
                    span: self.span_from(start),
                });
            }
            Tok::Ident(name) if name.as_ref() == "export" => {
                self.next();
                return Ok(Stmt {
                    kind: self.export_rest()?,
                    span: self.span_from(start),
                });
            }
            // `label: stmt` (only a bare identifier can start a label)
            Tok::Ident(name) if *self.peek_at(1) == Tok::Punct(Punct::Colon) => {
                self.check_ctx_ident(&name)?;
                let name = name.clone();
                self.next(); // the label
                self.next(); // `:`
                let body = self.statement()?;
                StmtKind::Label {
                    name,
                    body: Box::new(body),
                }
            }
            _ => {
                let e = self.expr()?;
                self.finish_stmt()?;
                StmtKind::Expr(Box::new(e))
            }
        };
        Ok(Stmt {
            kind,
            span: self.span_from(start),
        })
    }

    fn block_body(&mut self) -> Result<Vec<Stmt>, JsError> {
        self.stmt_list(false)
    }

    /// A function body: same grammar as a block, but `var` names stop here and
    /// a function declaration at this level is var-scoped.
    fn fn_block_body(&mut self) -> Result<Vec<Stmt>, JsError> {
        self.stmt_list(true)
    }

    fn stmt_list(&mut self, top: bool) -> Result<Vec<Stmt>, JsError> {
        // A function body is its own grammar: `in` is a normal operator again
        // even when the function expression sits in a for-head initializer
        // (`for (var f = function () { "x" in y; };;)`). Same for class bodies.
        let prev_no_in = self.no_in;
        self.no_in = false;
        self.block_scopes.push(BlockScope {
            lex: Vec::new(),
            vars: Vec::new(),
            top,
        });
        let mut out = Vec::new();
        while !self.eat_punct(Punct::RBrace) {
            if *self.peek() == Tok::Eof {
                self.block_scopes.pop();
                self.no_in = prev_no_in;
                return self.err("unterminated block (missing `}`)");
            }
            match self.statement() {
                Ok(s) => out.push(s),
                Err(e) => {
                    self.block_scopes.pop();
                    self.no_in = prev_no_in;
                    return Err(e);
                }
            }
        }
        self.no_in = prev_no_in;
        let scope = self.block_scopes.pop().expect("block scope stack");
        self.check_block_scope(&scope)?;
        Ok(out)
    }

    /// Mark a name as lexically declared in the innermost StatementList.
    fn declare_lexical(&mut self, name: &Rc<str>) {
        if let Some(s) = self.block_scopes.last_mut() {
            s.lex.push(name.clone());
        }
    }

    /// Mark a `var` name in the innermost StatementList and every enclosing
    /// one up to (and including) the nearest function body or program.
    fn declare_var(&mut self, name: &Rc<str>) {
        for s in self.block_scopes.iter_mut().rev() {
            s.vars.push(name.clone());
            if s.top {
                break;
            }
        }
    }

    /// A function declaration: lexical in a block, var-scoped at the top of a
    /// function body or the program.
    fn declare_fn(&mut self, name: &Rc<str>) {
        match self.block_scopes.last() {
            Some(s) if !s.top => self.declare_lexical(name),
            _ => self.declare_var(name),
        }
    }

    /// LexicallyDeclaredNames must be duplicate-free and disjoint from
    /// VarDeclaredNames (Block / CaseBlock / FunctionBody early error).
    fn check_block_scope(&mut self, scope: &BlockScope) -> Result<(), JsError> {
        for (i, l) in scope.lex.iter().enumerate() {
            if scope.lex[..i].contains(l) {
                return self.err(format!("`{l}` is declared more than once"));
            }
            if scope.vars.iter().any(|v| v == l) {
                return self.err(format!(
                    "`{l}` is declared both lexically and with `var` in the same block"
                ));
            }
        }
        Ok(())
    }

    /// Record the names bound by a variable declaration in the open
    /// StatementList (lexical for `let`/`const`, var-propagating for `var`).
    fn note_declarators(&mut self, kind: VarKind, ds: &[Declarator]) {
        let mut names = Vec::new();
        for d in ds {
            match &d.pat {
                Some(p) => pat_binding_names(p, &mut names),
                None => names.push(d.name.clone()),
            }
        }
        for n in names {
            match kind {
                VarKind::Var => self.declare_var(&n),
                _ => self.declare_lexical(&n),
            }
        }
    }

    fn declarators(&mut self) -> Result<Vec<Declarator>, JsError> {
        let mut out = Vec::new();
        loop {
            let (name, pat) = self.pattern_or_ident()?;
            let init = if self.eat_op(Op::Assign) {
                let mut e = self.assign_expr()?;
                // `var fn = function () {}` takes the declarator name
                if pat.is_none() {
                    name_anon_fn(&name, &mut e);
                }
                Some(e)
            } else {
                None
            };
            out.push(Declarator { name, pat, init });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.finish_stmt()?;
        Ok(out)
    }

    /// a destructuring pattern (declaration form: leaf bindings must be
    /// fresh simple names) or a plain identifier
    fn pattern_or_ident(&mut self) -> Result<(Rc<str>, Option<Pat>), JsError> {
        match self.peek() {
            Tok::Punct(Punct::LBrack) | Tok::Punct(Punct::LBrace) => {
                let pat = self.parse_pattern(true)?;
                Ok((Rc::from(""), Some(pat)))
            }
            _ => {
                let name = self.expect_ident()?;
                Ok((name, None))
            }
        }
    }

    /// Parse a destructuring pattern. `decl_form` (let/const/var/params/
    /// for-head declarations) demands fresh names for rest leaf bindings;
    /// assignment destructuring may bind member/index lvalues.
    fn parse_pattern(&mut self, decl_form: bool) -> Result<Pat, JsError> {
        self.depth += 1;
        if self.depth > MAX_NEST_DEPTH {
            return self.err("pattern nesting too deep");
        }
        let r = self.pattern_inner(decl_form);
        self.depth -= 1;
        r
    }

    fn pattern_inner(&mut self, decl_form: bool) -> Result<Pat, JsError> {
        match self.peek() {
            Tok::Punct(Punct::LBrack) => {
                self.next();
                let mut parts = Vec::new();
                loop {
                    if self.eat_punct(Punct::RBrack) {
                        break;
                    }
                    // a comma at an element position is an elision (leading
                    // `[,a]`, interior `[a,,b]`, `[,]`); the comma separator
                    // after an element is consumed below
                    if self.eat_punct(Punct::Comma) {
                        parts.push(PatPart::Hole);
                        continue;
                    }
                    if *self.peek() == Tok::Spread {
                        self.next();
                        // the rest target may be a nested pattern:
                        // `...[a, b]`, `...{a}` (the runtime binds the rest
                        // array against that pattern)
                        let t = if matches!(
                            self.peek(),
                            Tok::Punct(Punct::LBrack) | Tok::Punct(Punct::LBrace)
                        ) {
                            self.parse_pattern(decl_form)?
                        } else {
                            self.parse_binding_leaf(decl_form)?
                        };
                        parts.push(PatPart::Rest(Box::new(t)));
                        // `BindingRestElement` must be the last element;
                        // `[...a, b]` and `[...a,]` are early errors.
                        if !matches!(self.peek(), Tok::Punct(Punct::RBrack)) {
                            return self.err("rest element must be last in a pattern");
                        }
                        continue;
                    }
                    parts.push(self.pat_part(decl_form)?);
                    // after an element: `,` starts the next slot, `]` ends
                    if !self.eat_punct(Punct::Comma)
                        && !matches!(self.peek(), Tok::Punct(Punct::RBrack))
                    {
                        self.expect_punct(Punct::Comma)?;
                    }
                }
                Ok(Pat::Arr(parts))
            }
            Tok::Punct(Punct::LBrace) => {
                self.next();
                let mut parts: Vec<ObjPatPart> = Vec::new();
                while !self.eat_punct(Punct::RBrace) {
                    if !parts.is_empty() {
                        self.expect_punct(Punct::Comma)?;
                    }
                    if matches!(self.peek(), Tok::Punct(Punct::RBrace)) {
                        // trailing comma: consume the `}` too (the loop's own
                        // eat only runs when the comma was not consumed here)
                        self.next();
                        break;
                    }
                    if *self.peek() == Tok::Spread {
                        self.next();
                        let t = self.parse_binding_leaf(decl_form)?;
                        if !matches!(t, Pat::Name(_)) {
                            return self.err("object rest must bind a simple name");
                        }
                        parts.push(ObjPatPart::Rest(t));
                        // object rest must be last too (`{...a, b}`)
                        if *self.peek() == Tok::Punct(Punct::Comma) {
                            return self.err("rest element must be last in a pattern");
                        }
                        continue;
                    }
                    // `{[expr]: target}` - computed property name
                    let computed = if self.eat_punct(Punct::LBrack) {
                        let e = self.assign_expr()?;
                        self.expect_punct(Punct::RBrack)?;
                        Some(e)
                    } else {
                        None
                    };
                    let key = match computed {
                        Some(_) => None,
                        None => Some(self.prop_name()?),
                    };
                    if let Some(k) = &key {
                        if k.starts_with('#') {
                            return self.err("a private name is not valid in an object pattern");
                        }
                    }
                    let part = if self.eat_punct(Punct::Colon) {
                        self.pat_part(decl_form)?
                    } else if computed.is_some() {
                        return self.err("expected `:` after a computed property name");
                    } else {
                        // shorthand: `{a}` / `{a = d}` binds the name `a`
                        let key = key.clone().expect("shorthand key");
                        if is_reserved_word(&key) {
                            return self.err(format!("`{key}` is a reserved word"));
                        }
                        self.check_ctx_ident(&key)?;
                        if *self.peek() == Tok::Op(Op::Assign) {
                            self.next();
                            let mut d = self.assign_expr()?;
                            name_anon_fn(&key, &mut d);
                            PatPart::Bind(
                                Box::new(Pat::Name(BindTgt::Name(key.clone()))),
                                Some(d),
                            )
                        } else {
                            PatPart::Bind(
                                Box::new(Pat::Name(BindTgt::Name(key.clone()))),
                                None,
                            )
                        }
                    };
                    match (computed, key) {
                        (Some(key), _) => parts.push(ObjPatPart::ComputedProp { key, part }),
                        (None, Some(key)) => parts.push(ObjPatPart::Prop { key, part }),
                        (None, None) => unreachable!(),
                    }
                }
                Ok(Pat::Obj(parts))
            }
            _ => self.err("expected destructuring pattern (array or object)"),
        }
    }

    /// one pattern element: target leaf (nested pattern allowed), with an
    /// optional `= default`
    fn pat_part(&mut self, decl_form: bool) -> Result<PatPart, JsError> {
        let sub = match self.peek() {
            Tok::Punct(Punct::LBrack) | Tok::Punct(Punct::LBrace) => self.parse_pattern(decl_form)?,
            _ => self.parse_binding_leaf(decl_form)?,
        };
        let mut default = if self.eat_op(Op::Assign) {
            Some(self.assign_expr()?)
        } else {
            None
        };
        // `[fn = function () {}]`: the anonymous function takes the bound
        // name (NamedEvaluation), so `fn.name === 'fn'`
        if let (Some(d), Pat::Name(BindTgt::Name(n))) = (default.as_mut(), &sub) {
            name_anon_fn(n, d);
        }
        Ok(PatPart::Bind(Box::new(sub), default))
    }

    /// leaf binding target: `a`, `a.b`, `a[b]`, `o[k]`
    fn parse_binding_leaf(&mut self, decl_form: bool) -> Result<Pat, JsError> {
        let start = self.pos;
        let mut e = match self.peek().clone() {
            Tok::Ident(s) => {
                if is_reserved_word(&s) {
                    return self.err(format!("`{s}` is a reserved word"));
                }
                self.next();
                Expr {
                    kind: ExprKind::Ident(s),
                    span: Span { start: 0, end: 0 },
                }
            }
            _ => return self.err("expected binding name in pattern"),
        };
        // assignment forms may extend to a member/index chain
        if !decl_form {
            loop {
                match self.peek() {
                    Tok::Punct(Punct::Dot) => {
                        self.next();
                        let key = self.prop_name()?;
                        self.note_private_ref(&key)?;
                        e = Expr {
                            kind: ExprKind::Member { base: Box::new(e), key },
                            span: Span { start: 0, end: 0 },
                        };
                    }
                    Tok::Punct(Punct::LBrack) => {
                        self.next();
                        let idx = self.expr()?;
                        self.expect_punct(Punct::RBrack)?;
                        e = Expr {
                            kind: ExprKind::Index { base: Box::new(e), index: Box::new(idx) },
                            span: Span { start: 0, end: 0 },
                        };
                    }
                    _ => break,
                }
            }
        }
        expr_to_pat(&e).map_err(|m| {
            let t = self.cur();
            let (line, col) = line_col(self.src, t.start as usize);
            JsError::Syntax { msg: m, line, col }
        })
    }

    // -- for heads ----------------------------------------------------------

fn for_rest(&mut self) -> Result<ForHead, JsError> {
        // for (decl ...; cond; upd) | for (decl k in o) | for (decl k of o) | for (e; ..)
        let mut kind: Option<VarKind> = None;
        match self.peek().clone() {
            Tok::Kw(Kw::Var) => {
                self.next();
                kind = Some(VarKind::Var);
            }
            Tok::Kw(Kw::Let) => {
                self.next();
                kind = Some(VarKind::Let);
            }
            Tok::Kw(Kw::Const) => {
                self.next();
                kind = Some(VarKind::Const);
            }
            _ => {}
        }
        if kind.is_some() {
            // declaration form. Parse the whole declarator list with `in`
            // suppressed so `for (var a, b, c = x; cond; upd)` works; a lone
            // declarator may instead be a for-in / for-of target.
            let mut decls: Vec<Declarator> = Vec::new();
            let mut first: Option<ForTarget> = None;
            loop {
                let (name, pat) = self.pattern_or_ident()?;
                if decls.is_empty() {
                    first = Some(match &pat {
                        Some(p) => ForTarget::Pat(p.clone()),
                        None => ForTarget::Name(name.clone()),
                    });
                }
                let init = if self.eat_op(Op::Assign) {
                    Some(self.no_in_assign_expr()?)
                } else {
                    None
                };
                decls.push(Declarator { name, pat, init });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            if decls.len() == 1 {
                if let Some(target) = first {
                    if self.eat_kw(Kw::In) {
                        let obj = self.expr()?;
                        return Ok(ForHead::In { kind, target, obj });
                    }
                    if self.eat_word("of") {
                        let iter = self.assign_expr()?; // no comma in for-of head
                        return Ok(ForHead::Of { kind, target, iter });
                    }
                }
            }
            let init = Some(Box::new(Stmt {
                kind: StmtKind::VarDecl(kind.unwrap_or(VarKind::Var), decls),
                span: self.span_from(self.pos),
            }));
            return self.for_classic_tail(init);
        }
        // expression forms
        if *self.peek() == Tok::Punct(Punct::Semi) {
            // empty init: `for (; cond; upd)` / `for (;;)` — for_classic_tail
            // consumes the init-terminating `;` itself
            return self.for_classic_tail(None);
        }
        // the head may be an assignment destructuring target: allow the object
        // cover form until we know whether an `in`/`of` follows
        self.cover_depth += 1;
        let e = self.no_in_expr();
        self.cover_depth -= 1;
        let e = e?;
        if self.eat_kw(Kw::In) {
            let target = self.expr_to_loop_target(&e, "invalid for-in target")?;
            let obj = self.expr()?;
            return Ok(ForHead::In {
                kind: None,
                target,
                obj,
            });
        }
        if self.eat_word("of") {
            // assignment-destructuring form: `for ([a, b] of pairs)`
            let target = self.expr_to_loop_target(&e, "invalid for-of target")?;
            let iter = self.assign_expr()?;
            return Ok(ForHead::Of { kind: None, target, iter });
        }
        // a classic `for` init is a plain expression, not a pattern
        if expr_has_cover_init(&e) {
            return self.err("`{x = default}` is only valid in a destructuring pattern");
        }
        let init = Some(Box::new(Stmt {
            kind: StmtKind::Expr(Box::new(e)),
            span: self.span_from(self.pos),
        }));
        self.for_classic_tail(init)
    }

    /// Parse a full expression (comma sequence included) with `in` treated as
    /// a head delimiter (see `Parser::no_in`).
    fn no_in_expr(&mut self) -> Result<Expr, JsError> {
        let prev = self.no_in;
        self.no_in = true;
        let r = self.expr();
        self.no_in = prev;
        r
    }

    /// Plan an assignment expression (no comma) with `in` suppressed: used
    /// for each declarator initializer, where a comma starts the next
    /// declarator rather than a sequence expression.
    fn no_in_assign_expr(&mut self) -> Result<Expr, JsError> {
        let prev = self.no_in;
        self.no_in = true;
        let r = self.assign_expr();
        self.no_in = prev;
        r
    }

    /// A for-in / for-of head target: a bare name (fast path) or any
    /// assignment target / destructuring pattern.
    fn expr_to_loop_target(&mut self, e: &Expr, msg: &str) -> Result<ForTarget, JsError> {
        if let ExprKind::Ident(s) = &e.kind {
            return Ok(ForTarget::Name(s.clone()));
        }
        match expr_to_pat(e) {
            Ok(p) => Ok(ForTarget::Pat(p)),
            Err(_) => self.err(msg),
        }
    }

    fn for_classic_tail(&mut self, init: Option<Box<Stmt>>) -> Result<ForHead, JsError> {
        self.expect_punct(Punct::Semi)?;
        // empty condition: `for (;; ...)` — `;` or `)` means omitted
        let cond = if matches!(
            self.peek(),
            Tok::Punct(Punct::Semi) | Tok::Punct(Punct::RParen)
        ) {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect_punct(Punct::Semi)?;
        let upd = if *self.peek() == Tok::Punct(Punct::RParen) {
            None
        } else {
            Some(self.expr()?)
        };
        Ok(ForHead::Classic { init, cond, upd })
    }

    // -- functions ----------------------------------------------------------

    /// after `function` keyword; `named` = declaration (name required)
    fn function_rest(
        &mut self,
        named: bool,
        is_async: bool,
        is_generator: bool,
    ) -> Result<Rc<FnDef>, JsError> {
        let name = if named {
            self.expect_ident()?
        } else if let Tok::Ident(s) = self.peek().clone() {
            self.check_ctx_ident(&s)?;
            self.next();
            s
        } else {
            Rc::from("")
        };
        self.fn_ctx.push(FnCtx {
            is_async,
            is_generator,
        });
        let params = self.param_list()?;
        self.expect_punct(Punct::LBrace)?;
        let mut body_stmts = self.fn_block_body()?;
        self.fn_ctx.pop();
        self.check_strict_body(&params, &body_stmts)?;
        if is_generator {
            self.prepare_generator_body(&mut body_stmts, is_async)?;
        } else if stmts_use(&body_stmts, SusKind::Yield) {
            return self.err("`yield` is only valid inside generator functions");
        }
        if !is_async && stmts_use(&body_stmts, SusKind::Await) {
            return self.err("`await` is only valid inside async functions");
        }
        let uses_arguments = scan_uses_arguments_stmts(&body_stmts);
        Ok(Rc::new(FnDef {
            src: self.src_id,
            name,
            params,
            body: FnBody::Block(body_stmts),
            uses_arguments,
            is_async,
            is_generator,
            has_yield: is_generator,
        }))
    }

    /// Position check for `yield` inside a generator body *after* the lift
    /// pass: only a whole expression statement (`yield x;`) or a declarator
    /// initializer (`let x = yield v`) may remain. Anything the lift cannot
    /// rewrite (loop conditions, `return yield`, ...) is an honest
    /// SyntaxError. Try/catch/finally bodies are allowed: the walker keeps a
    /// try frame on its stack (step 19.2).
    ///
    /// Async generators skip the parse-time check: their bodies often mix
    /// `yield` into logical expressions (stream splitters). The body parses,
    /// and the walker reports an honest error at CALL time if it reaches an
    /// unsupported yield position — module evaluation still succeeds.
    /// Generator bodies get the yield-lift pass (below) before the position
    /// check: yields buried in expressions are rewritten to the statement
    /// forms the resumable walker executes, so the check only ever sees what
    /// the walker cannot reach.
    fn prepare_generator_body(
        &mut self,
        stmts: &mut Vec<Stmt>,
        is_async: bool,
    ) -> Result<(), JsError> {
        let mut lift = YieldLift { next: 0 };
        lift.stmts(stmts);
        if is_async {
            return Ok(());
        }
        check_yield_stmts(stmts).map_err(|m| {
            let t = self.cur();
            let (line, col) = line_col(self.src, t.start as usize);
            JsError::Syntax { msg: m, line, col }
        })
    }

    /// Consume the parameter list after an already-consumed `(`.
    fn params_after_lparen(&mut self, hidden_idx: &mut usize) -> Result<Vec<Param>, JsError> {
        let mut params = Vec::new();
        while !self.eat_punct(Punct::RParen) {
            if !params.is_empty() {
                self.expect_punct(Punct::Comma)?;
                // trailing comma: `(a, b,)` is legal, but not after a rest
                // parameter (`(...a,)` is an early error)
                if self.eat_punct(Punct::RParen) {
                    if params.iter().any(|p: &Param| p.rest) {
                        return self.err("a rest parameter cannot have a trailing comma");
                    }
                    break;
                }
            }
            let rest = match self.peek() {
                Tok::Spread => {
                    self.next();
                    true
                }
                _ => false,
            };
            let (name, pat) = match self.peek().clone() {
                Tok::Ident(s) => {
                    self.check_ctx_ident(&s)?;
                    self.next();
                    (s, None)
                }
                Tok::Punct(Punct::LBrack) | Tok::Punct(Punct::LBrace) => {
                    let pat = self.parse_pattern(true)?;
                    // synthetic non-lexable env name; destructured after the
                    // param env is built
                    let hidden = Rc::from(format!("#{}", *hidden_idx));
                    *hidden_idx += 1;
                    (hidden, Some(Box::new(pat)))
                }
                _ => {
                    return self.err("expected parameter name");
                }
            };
            let mut default = if rest {
                None // rest params take no default
            } else if self.eat_op(Op::Assign) {
                Some(Box::new(self.assign_expr()?))
            } else {
                None
            };
            // `function f(fn = function () {})`: NamedEvaluation
            if pat.is_none() {
                if let Some(d) = default.as_mut() {
                    name_anon_fn(&name, d);
                }
            }
            params.push(Param { name, pat, default, rest });
            // a rest parameter must be the last one (`f(a, ...b, c)` is an
            // early error); the trailing-comma case `(...a,)` lands here too
            if rest && !matches!(self.peek(), Tok::Punct(Punct::RParen)) {
                return self.err("a rest parameter must be last");
            }
        }
        Ok(params)
    }

    fn param_list(&mut self) -> Result<Vec<Param>, JsError> {
        self.expect_punct(Punct::LParen)?;
        let mut idx = 0usize;
        let params = self.params_after_lparen(&mut idx)?;
        // BoundNames of a non-simple parameter list must not repeat
        // (`(x = 0, x) => {}`); a simple list may (`function f(x, x)` in
        // sloppy mode).
        let simple = params
            .iter()
            .all(|p| p.pat.is_none() && p.default.is_none() && !p.rest);
        if !simple {
            let mut names = Vec::new();
            for p in &params {
                match &p.pat {
                    Some(pat) => pat_binding_names(pat, &mut names),
                    None => names.push(p.name.clone()),
                }
            }
            let mut seen = std::collections::HashSet::new();
            for n in names {
                if !seen.insert(n.clone()) {
                    return self.err(format!("duplicate parameter name `{n}`"));
                }
            }
        }
        Ok(params)
    }

    /// `class Name { constructor(){} method(){} get x(){} set x(v){} }`
    /// desugared into:
    /// `function Name(...){...}; Name.prototype.method = function(){...}; ...`
    /// plus accessor-definition statements for get/set pairs.
    fn class_rest(&mut self) -> Result<Vec<Stmt>, JsError> {
        let name = self.expect_ident()?;
        self.class_rest_named(name)
    }

    /// Lower a class body whose binding name is already known (declaration
    /// and expression forms share everything below this point).
    fn class_rest_named(&mut self, name: Rc<str>) -> Result<Vec<Stmt>, JsError> {
        let superclass = if self.eat_kw(Kw::Extends) {
            Some(self.lhs_expr()?)
        } else {
            None
        };
        self.expect_punct(Punct::LBrace)?;
        // Hidden binding for the superclass. It lives in the same scope as
        // the class declaration, so the constructor and every method (which
        // are closures created there) can reach it.
        let super_name: Option<Rc<str>> = superclass
            .as_ref()
            .map(|_| Rc::from(format!("__super{}", next_class_id()).as_str()));
        self.class_stack.push(ClassCtx {
            super_name: super_name.clone(),
            is_static: false,
            private_names: Vec::new(),
            private_accessor_names: Vec::new(),
            private_refs: Vec::new(),
            in_constructor: false,
        });
        let parsed = self.class_body(&name);
        let ctx = self.class_stack.pop();
        // every private reference must name a private member of this class or
        // of an enclosing one (forward references inside the body are legal)
        if let Some(ctx) = &ctx {
            for r in &ctx.private_refs {
                let mut found = ctx.private_names.iter().any(|n| n == r);
                if !found {
                    for outer in self.class_stack.iter().rev() {
                        if outer.private_names.iter().any(|n| n == r) {
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    return self.err(format!("private name `{r}` is not declared in this class"));
                }
            }
        }
        let (ctor, instance, statics, fields) = parsed?;

        let span = Span { start: 0, end: 0 };
        let ident = |n: &Rc<str>| Expr {
            kind: ExprKind::Ident(n.clone()),
            span,
        };
        let proto_of = |n: &Rc<str>| Expr {
            kind: ExprKind::Member {
                base: Box::new(ident(n)),
                key: Rc::from("prototype"),
            },
            span,
        };
        let member = |base: Expr, key: &Rc<str>| Expr {
            kind: ExprKind::Member {
                base: Box::new(base),
                key: key.clone(),
            },
            span,
        };
        let undefined = || Expr {
            kind: ExprKind::Void(Box::new(Expr {
                kind: ExprKind::Num(0.0),
                span,
            })),
            span,
        };
        let assign = |target: Expr, value: Expr| Stmt {
            kind: StmtKind::Expr(Box::new(Expr {
                kind: ExprKind::Assign {
                    target: Box::new(target),
                    op: None,
                    value: Box::new(value),
                },
                span,
            })),
            span,
        };

        // Default constructor. A derived class with no explicit constructor
        // forwards every argument to the superclass (spec behavior).
        let mut ctor_def = ctor.unwrap_or_else(|| {
            let body = match &super_name {
                Some(sn) => vec![Stmt {
                    kind: StmtKind::Expr(Box::new(Expr {
                        kind: ExprKind::SuperCall {
                            class: Box::new(ident(sn)),
                            key: None,
                            static_: false,
                            args: vec![CallArg::Spread(Expr {
                                kind: ExprKind::Ident(Rc::from("arguments")),
                                span,
                            })],
                        },
                        span,
                    })),
                    span,
                }],
                None => Vec::new(),
            };
            Rc::new(FnDef {
                src: self.src_id,
                name: name.clone(),
                params: Vec::new(),
                body: FnBody::Block(body),
                uses_arguments: super_name.is_some(),
                is_async: false,
                is_generator: false,
                has_yield: false,
            })
        });
        // Instance fields initialize inside the constructor. In a derived
        // class they run right after the first top-level `super(...)` call
        // (before it when the constructor has no top-level super call -
        // shortcut: a super call nested in an arrow is not detected).
        if !fields.is_empty() {
            let mut body = match &ctor_def.body {
                FnBody::Block(b) => b.clone(),
                _ => Vec::new(),
            };
            let inits: Vec<Stmt> = fields
                .iter()
                .map(|(k, init)| {
                    let this = Expr {
                        kind: ExprKind::This,
                        span,
                    };
                    let target = match k {
                        ClassKey::Name(k) => member(this, k),
                        ClassKey::Computed(k) => Expr {
                            kind: ExprKind::Index {
                                base: Box::new(this),
                                index: Box::new(k.clone()),
                            },
                            span,
                        },
                    };
                    assign(target, init.clone().unwrap_or_else(&undefined))
                })
                .collect();
            let at = if super_name.is_some() {
                body.iter()
                    .position(|s| {
                        matches!(&s.kind, StmtKind::Expr(e) if matches!(e.kind, ExprKind::SuperCall { .. }))
                    })
                    .map(|i| i + 1)
                    .unwrap_or(0)
            } else {
                0
            };
            body.splice(at..at, inits);
            ctor_def = Rc::new(FnDef {
                src: self.src_id,
                body: FnBody::Block(body),
                ..(*ctor_def).clone()
            });
        }

        let mut out = Vec::new();
        if let (Some(sn), Some(sc)) = (&super_name, superclass) {
            out.push(Stmt {
                kind: StmtKind::VarDecl(
                    VarKind::Let,
                    vec![Declarator {
                        name: sn.clone(),
                        pat: None,
                        init: Some(sc),
                    }],
                ),
                span,
            });
        }
        out.push(Stmt {
            kind: StmtKind::FuncDecl(ctor_def),
            span,
        });
        if let Some(sn) = &super_name {
            // Prototype chains: instance methods via `.prototype`, static
            // members via the class function's own [[Prototype]].
            let set_proto = |target: Expr, proto: Expr| Stmt {
                kind: StmtKind::Expr(Box::new(Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(Expr {
                            kind: ExprKind::Member {
                                base: Box::new(Expr {
                                    kind: ExprKind::Ident(Rc::from("Object")),
                                    span,
                                }),
                                key: Rc::from("setPrototypeOf"),
                            },
                            span,
                        }),
                        args: vec![CallArg::Arg(target), CallArg::Arg(proto)],
                    },
                    span,
                })),
                span,
            };
            out.push(set_proto(ident(&name), ident(sn)));
            out.push(set_proto(proto_of(&name), proto_of(sn)));
        }

        // Lower members in source order within each list.
        let lower = |items: Vec<ClassItem>, is_static: bool| -> Vec<Stmt> {
            items
                .into_iter()
                .map(|it| match it {
                    ClassItem::Method(key, f) => Stmt {
                        kind: StmtKind::DefineMethod {
                            owner: Box::new(if is_static {
                                ident(&name)
                            } else {
                                proto_of(&name)
                            }),
                            key: Box::new(key),
                            func: f,
                        },
                        span,
                    },
                    ClassItem::Accessor(key, getter, setter) => Stmt {
                        kind: StmtKind::DefineAcc {
                            owner: Box::new(if is_static {
                                ident(&name)
                            } else {
                                proto_of(&name)
                            }),
                            key: Box::new(key),
                            getter,
                            setter,
                        },
                        span,
                    },
                    ClassItem::Field(key, init) => {
                        let value = init.unwrap_or_else(&undefined);
                        let target = match key {
                            ClassKey::Name(k) => member(ident(&name), &k),
                            ClassKey::Computed(k) => Expr {
                                kind: ExprKind::Index {
                                    base: Box::new(ident(&name)),
                                    index: Box::new(k),
                                },
                                span,
                            },
                        };
                        assign(target, value)
                    }
                })
                .collect()
        };
        out.extend(lower(instance, false));
        out.extend(lower(statics, true));
        Ok(out)
    }

    /// `get [expr]()`: scan to the matching `]` (tokens are already flat here,
    /// so nested brackets inside the expression still count) and require `(`
    /// after it. Without this, `get [Symbol.toStringTag]()` reads as a field
    /// named `get` and then fails on the bracket.
    fn computed_accessor_ahead(&self) -> bool {
        let mut depth = 1usize;
        let mut i = self.pos + 2; // after `get`/`set` and `[`
        while let Some(t) = self.toks.get(i) {
            match &t.tok {
                Tok::Punct(Punct::LBrack) => depth += 1,
                Tok::Punct(Punct::RBrack) => {
                    depth -= 1;
                    if depth == 0 {
                        return self
                            .toks
                            .get(i + 1)
                            .map_or(false, |t| matches!(&t.tok, Tok::Punct(Punct::LParen)));
                    }
                }
                Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Class member name: `[expr]` or a literal/property name.
    fn class_member_key(&mut self) -> Result<ClassKey, JsError> {
        if *self.peek() == Tok::Punct(Punct::LBrack) {
            // computed member name: `[Symbol.iterator]() {}`
            self.next();
            let e = self.expr()?;
            self.expect_punct(Punct::RBrack)?;
            Ok(ClassKey::Computed(e))
        } else {
            Ok(ClassKey::Name(self.prop_name()?))
        }
    }

    /// Parse the inside of a class body. Callers have pushed the class
    /// context so `super` resolves; this only collects members.
    #[allow(clippy::type_complexity)]
    fn class_body(
        &mut self,
        name: &Rc<str>,
    ) -> Result<
        (
            Option<Rc<FnDef>>,
            Vec<ClassItem>,
            Vec<ClassItem>,
            Vec<(ClassKey, Option<Expr>)>,
        ),
        JsError,
    > {
        let mut ctor: Option<Rc<FnDef>> = None;
        let mut instance: Vec<ClassItem> = Vec::new();
        let mut statics: Vec<ClassItem> = Vec::new();
        // instance fields (static fields are ClassItem::Field in `statics`)
        let mut fields: Vec<(ClassKey, Option<Expr>)> = Vec::new();
        while !self.eat_punct(Punct::RBrace) {
            if self.eat_punct(Punct::Semi) {
                continue;
            }
            // `super()` is only legal in the constructor body, never in a
            // static block, field initializer or ordinary method
            if let Some(c) = self.class_stack.last_mut() {
                c.in_constructor = false;
            }
            // `static` modifier: not a method named `static` (`static()`)
            let is_static = *self.peek() == Tok::Kw(Kw::Static)
                && *self.peek_at(1) != Tok::Punct(Punct::LParen);
            if is_static {
                self.next();
            }
            if let Some(c) = self.class_stack.last_mut() {
                c.is_static = is_static;
            }
            // `async` method modifier: `async foo()`, `async *foo()`. A field
            // named `async` has `=`, `;`, a newline or the closing `}` next;
            // `async()` is a method named `async`.
            let mut is_async = false;
            let mut is_gen = false;
            let async_ident = match self.peek() {
                Tok::Kw(Kw::Async) => true,
                Tok::Ident(s) if s.as_ref() == "async" => !self.ident_escaped(self.pos),
                _ => false,
            };
            if async_ident {
                let is_async_method = !self.toks[(self.pos + 1).min(self.toks.len() - 1)].nl
                    && matches!(
                        self.peek_at(1),
                        Tok::Ident(_) | Tok::Kw(_) | Tok::Str(_) | Tok::Num(_) | Tok::Op(Op::Mul)
                    );
                if is_async_method {
                    self.next(); // `async`
                    is_async = true;
                }
            }
            if *self.peek() == Tok::Op(Op::Mul) {
                self.next(); // `*`
                is_gen = true;
            }
            // accessor: `get X(...)` / `set X(...)` (a method NAMED get/set
            // has `(` directly after the name). Also `get [expr]()`, which is
            // how bundles install `Symbol.toStringTag` getters.
            let is_acc = matches!(
                self.peek(),
                Tok::Ident(s) if s.as_ref() == "get" || s.as_ref() == "set"
            ) && !self.ident_escaped(self.pos)
                && (matches!(
                    self.peek_at(1),
                    Tok::Ident(_) | Tok::Kw(_) | Tok::Str(_) | Tok::Num(_)
                ) && *self.peek_at(2) == Tok::Punct(Punct::LParen)
                    || *self.peek_at(1) == Tok::Punct(Punct::LBrack)
                        && self.computed_accessor_ahead());
            if is_acc {
                let getset = self.expect_ident()?;
                let private_key =
                    matches!(self.peek(), Tok::Ident(s) if s.starts_with('#'));
                let key = self.class_member_key()?;
                if private_key {
                    self.note_private_decl(&key, true)?;
                }
                if let ClassKey::Name(k) = &key {
                    if &**k == "#constructor" {
                        return self.err("`#constructor` is not a valid private name");
                    }
                    if is_static && (&**k == "prototype" || &**k == "constructor") {
                        return self.err("a static accessor cannot be named `prototype` or `constructor`");
                    }
                    if !is_static && &**k == "constructor" {
                        return self.err("an accessor cannot be named `constructor`");
                    }
                }
                self.fn_ctx.push(FnCtx::default());
                let params = self.param_list()?;
                self.expect_punct(Punct::LBrace)?;
                let body = self.fn_block_body()?;
                self.fn_ctx.pop();
                self.check_strict_body(&params, &body)?;
                let uses_arguments = scan_uses_arguments_stmts(&body);
                let f = Rc::new(FnDef {
                    src: self.src_id,
                    name: match &key {
                        ClassKey::Name(k) => k.clone(),
                        ClassKey::Computed(_) => Rc::from(""),
                    },
                    params,
                    body: FnBody::Block(body),
                    uses_arguments,
                    is_async: false,
                    is_generator: false,
                    has_yield: false,
                });
                let list = if is_static { &mut statics } else { &mut instance };
                let mut found = false;
                for it in list.iter_mut() {
                    if let ClassItem::Accessor(k, g, s) = it {
                        // computed keys cannot be compared statically; they
                        // always get their own slot (first one wins at install)
                        if let (ClassKey::Name(a), ClassKey::Name(b)) = (&*k, &key) {
                            if a == b {
                                if getset.as_ref() == "get" {
                                    if g.is_some() {
                                        return self.err("duplicate accessor getter");
                                    }
                                    *g = Some(f.clone());
                                } else {
                                    if s.is_some() {
                                        return self.err("duplicate accessor setter");
                                    }
                                    *s = Some(f.clone());
                                }
                                found = true;
                                break;
                            }
                        }
                    }
                }
                if !found {
                    let (g, s) = if getset.as_ref() == "get" {
                        (Some(f), None)
                    } else {
                        (None, Some(f))
                    };
                    list.push(ClassItem::Accessor(key, g, s));
                }
                continue;
            }
            let private_key = matches!(self.peek(), Tok::Ident(s) if s.starts_with('#'));
            let key = self.class_member_key()?;
            if private_key {
                self.note_private_decl(&key, false)?;
            }
            let key_name: Option<Rc<str>> = match &key {
                ClassKey::Name(k) => Some(k.clone()),
                ClassKey::Computed(_) => None,
            };
            if let Some(k) = &key_name {
                if &**k == "#constructor" {
                    return self.err("`#constructor` is not a valid private name");
                }
            }
            if *self.peek() == Tok::Punct(Punct::LParen) {
                if let Some(k) = &key_name {
                    if is_static && (&**k == "prototype" || &**k == "constructor") {
                        return self.err(
                            "a static method cannot be named `prototype` or `constructor`",
                        );
                    }
                    if !is_static && &**k == "constructor" && (is_async || is_gen) {
                        return self.err("a constructor cannot be async or a generator");
                    }
                }
                self.fn_ctx.push(FnCtx {
                    is_async,
                    is_generator: is_gen,
                });
                let is_ctor = !is_static && key_name.as_deref() == Some("constructor");
                if is_ctor {
                    if let Some(c) = self.class_stack.last_mut() {
                        c.in_constructor = true;
                    }
                }
                let params = self.param_list()?;
                self.expect_punct(Punct::LBrace)?;
                let mut body = self.fn_block_body()?;
                if is_ctor {
                    if let Some(c) = self.class_stack.last_mut() {
                        c.in_constructor = false;
                    }
                }
                self.fn_ctx.pop();
                self.check_strict_body(&params, &body)?;
                if is_gen {
                    self.prepare_generator_body(&mut body, is_async)?;
                }
                let uses_arguments = scan_uses_arguments_stmts(&body);
                let f = Rc::new(FnDef {
                    src: self.src_id,
                    name: if is_ctor {
                        name.clone()
                    } else {
                        key_name.clone().unwrap_or_else(|| Rc::from(""))
                    },
                    params,
                    body: FnBody::Block(body),
                    uses_arguments,
                    is_async: is_async && !is_ctor,
                    is_generator: is_gen,
                    has_yield: is_gen,
                });
                if is_ctor {
                    if ctor.is_some() {
                        return self.err("duplicate class constructor");
                    }
                    ctor = Some(f);
                } else if is_static {
                    statics.push(ClassItem::Method(key, f));
                } else {
                    instance.push(ClassItem::Method(key, f));
                }
            } else {
                // public field: `x = init` / `x`
                if let Some(k) = &key_name {
                    if &**k == "constructor" || (is_static && &**k == "prototype") {
                        return self.err(
                            "a class field cannot be named `constructor` (or static `prototype`)",
                        );
                    }
                }
                let init = if self.eat_op(Op::Assign) {
                    let e = self.assign_expr()?;
                    // FieldDefinition : ClassElementName Initializer — the
                    // initializer runs with no `arguments` binding, so a
                    // reference (even through an arrow) is an early error
                    if scan_expr_arguments(&e) {
                        return self.err(
                            "`arguments` is not allowed in a class field initializer",
                        );
                    }
                    Some(e)
                } else {
                    None
                };
                let terminated = self.eat_punct(Punct::Semi)
                    || *self.peek() == Tok::Punct(Punct::RBrace)
                    || self.cur().nl;
                if !terminated {
                    return self.err("expected `;` after class field");
                }
                if is_static {
                    statics.push(ClassItem::Field(key, init));
                } else {
                    fields.push((key, init));
                }
            }
        }
        Ok((ctor, instance, statics, fields))
    }

    // -- expressions --------------------------------------------------------

    fn expr(&mut self) -> Result<Expr, JsError> {
        self.depth += 1;
        if self.depth > MAX_NEST_DEPTH {
            return self.err("expression nesting too deep");
        }
        let r = self.expr_inner();
        self.depth -= 1;
        r
    }

    fn expr_inner(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut e = self.assign_expr()?;
        if *self.peek() == Tok::Punct(Punct::Comma) {
            let mut list = vec![e];
            while self.eat_punct(Punct::Comma) {
                list.push(self.assign_expr()?);
            }
            e = Expr {
                kind: ExprKind::Seq(list),
                span: self.span_from(start),
            };
        }
        Ok(e)
    }

    fn assign_expr(&mut self) -> Result<Expr, JsError> {
        self.depth += 1;
        if self.depth > MAX_NEST_DEPTH {
            return self.err("expression nesting too deep");
        }
        let r = self.assign_expr_inner();
        self.depth -= 1;
        let e = r?;
        // a `{x = default}` cover form that was never resolved into a pattern
        // (its literal stayed an expression) is a SyntaxError
        if self.cover_depth == 0 && expr_has_cover_init(&e) {
            return self.err("`{x = default}` is only valid in a destructuring pattern");
        }
        Ok(e)
    }

    fn assign_expr_inner(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        // arrow with single ident param: `x => ...` / `async x => ...`
        if let Tok::Ident(s) = self.peek().clone() {
            if *self.peek_at(1) == Tok::Op(Op::Arrow) {
                self.check_ctx_ident(&s)?;
                self.next();
                self.next(); // =>
                let body = self.arrow_body_ctx(false)?;
                let params = vec![Param {
                    name: s,
                    pat: None,
                    default: None,
                    rest: false,
                }];
                self.check_strict_arrow(&params, &body)?;
                return Ok(Expr {
                    kind: ExprKind::Arrow(Rc::new(FnDef {
                        src: self.src_id,
                        name: Rc::from(""),
                        params,
                        body,
                        uses_arguments: false, // arrows have no arguments
                        is_async: false,
                        is_generator: false,
                        has_yield: false,
                    })),
                    span: self.span_from(start),
                });
            }
        }
        if *self.peek() == Tok::Kw(Kw::Async) {
            // `async x => ...`
            if let Tok::Ident(s) = self.peek_at(1).clone() {
                if *self.peek_at(2) == Tok::Op(Op::Arrow) {
                    let mut pc = self.fn_ctx.last().copied().unwrap_or_default();
                    pc.is_async = true;
                    self.check_reserved_in(&s, pc)?;
                    self.next(); // async
                    self.next(); // ident
                    self.next(); // =>
                    let body = self.arrow_body_ctx(true)?;
                    let params = vec![Param {
                        name: s,
                        pat: None,
                        default: None,
                        rest: false,
                    }];
                    self.check_strict_arrow(&params, &body)?;
                    return Ok(Expr {
                        kind: ExprKind::Arrow(Rc::new(FnDef {
                            src: self.src_id,
                            name: Rc::from(""),
                            params,
                            body,
                            uses_arguments: false,
                            is_async: true,
                            is_generator: false,
                            has_yield: false,
                        })),
                        span: self.span_from(start),
                    });
                }
            }
            // `async (a, b) => ...`
            if *self.peek_at(1) == Tok::Punct(Punct::LParen) {
                if let Some(params) = self.try_arrow_params(true)? {
                    let body = self.arrow_body_ctx(true)?;
                    self.check_strict_arrow(&params, &body)?;
                    return Ok(Expr {
                        kind: ExprKind::Arrow(Rc::new(FnDef {
                            src: self.src_id,
                            name: Rc::from(""),
                            params,
                            body,
                            uses_arguments: false,
                            is_async: true,
                            is_generator: false,
                            has_yield: false,
                        })),
                        span: self.span_from(start),
                    });
                }
            }
        }
        // arrow with parenthesized params: `(a, b) => ...`
        if *self.peek() == Tok::Punct(Punct::LParen) {
            if let Some(params) = self.try_arrow_params(false)? {
                let body = self.arrow_body_ctx(false)?;
                self.check_strict_arrow(&params, &body)?;
                return Ok(Expr {
                    kind: ExprKind::Arrow(Rc::new(FnDef {
                        src: self.src_id,
                        name: Rc::from(""),
                        params,
                        body,
                        uses_arguments: false,
                        is_async: false,
                        is_generator: false,
                        has_yield: false,
                    })),
                    span: self.span_from(start),
                });
            }
        }
        let l = self.cond_expr()?;
        let op = match self.peek() {
            Tok::Op(o @ (Op::Assign
            | Op::AddAssign
            | Op::SubAssign
            | Op::MulAssign
            | Op::DivAssign
            | Op::ModAssign
            | Op::PowAssign
            | Op::AndAssign
            | Op::OrAssign
            | Op::XorAssign
            | Op::ShlAssign
            | Op::ShrAssign
            | Op::UShrAssign
            | Op::LogicalAndAssign
            | Op::LogicalOrAssign
            | Op::CoalesceAssign)) => Some(*o),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            match l.kind {
                ExprKind::Ident(_) | ExprKind::Member { .. } | ExprKind::Index { .. } => {}
                // destructuring assignment: `([a, b] = pair)` / `({a} = obj)`
                ExprKind::Array(_) | ExprKind::Object(_) if op == Op::Assign => {
                    let pattern = expr_to_pat(&l).map_err(|m| {
                        let t = self.cur();
                        let (line, col) = line_col(self.src, t.start as usize);
                        JsError::Syntax {
                            msg: m,
                            line,
                            col,
                        }
                    })?;
                    let value = self.assign_expr()?;
                    return Ok(Expr {
                        kind: ExprKind::PatAssign {
                            pat: pattern,
                            value: Box::new(value),
                        },
                        span: self.span_from(start),
                    });
                }
                _ => return self.err("invalid assignment target"),
            }
            // `&&=`/`||=`/`??=` short-circuit: the RHS is only evaluated
            // when the current value says so, so it gets its own node
            let logical = match op {
                Op::LogicalAndAssign => Some(LogOp::And),
                Op::LogicalOrAssign => Some(LogOp::Or),
                Op::CoalesceAssign => Some(LogOp::Coalesce),
                _ => None,
            };
            if let Some(lop) = logical {
                let value = self.assign_expr()?;
                return Ok(Expr {
                    kind: ExprKind::LogicalAssign {
                        target: Box::new(l),
                        op: lop,
                        value: Box::new(value),
                    },
                    span: self.span_from(start),
                });
            }
            let mut value = self.assign_expr()?;
            // `fn = function () {}`: NamedEvaluation
            if op == Op::Assign {
                if let ExprKind::Ident(n) = &l.kind {
                    name_anon_fn(n, &mut value);
                }
            }
            let binop = match op {
                Op::Assign => None,
                Op::AddAssign => Some(BinOp::Add),
                Op::SubAssign => Some(BinOp::Sub),
                Op::MulAssign => Some(BinOp::Mul),
                Op::DivAssign => Some(BinOp::Div),
                Op::ModAssign => Some(BinOp::Mod),
                Op::PowAssign => Some(BinOp::Pow),
                Op::AndAssign => Some(BinOp::BitAnd),
                Op::OrAssign => Some(BinOp::BitOr),
                Op::XorAssign => Some(BinOp::BitXor),
                Op::ShlAssign => Some(BinOp::Shl),
                Op::ShrAssign => Some(BinOp::Shr),
                Op::UShrAssign => Some(BinOp::UShr),
                _ => unreachable!(),
            };
            return Ok(Expr {
                kind: ExprKind::Assign {
                    target: Box::new(l),
                    op: binop,
                    value: Box::new(value),
                },
                span: self.span_from(start),
            });
        }
        Ok(l)
    }

    fn arrow_body(&mut self) -> Result<FnBody, JsError> {
        if self.eat_punct(Punct::LBrace) {
            Ok(FnBody::Block(self.fn_block_body()?))
        } else {
            Ok(FnBody::Expr(self.assign_expr()?))
        }
    }

    /// Arrow bodies are their own function context: `await` is reserved only
    /// in an async arrow, and `yield` never (ConciseBody is `~Yield, ~Await`).
    fn arrow_body_ctx(&mut self, is_async: bool) -> Result<FnBody, JsError> {
        self.fn_ctx.push(FnCtx {
            is_async,
            is_generator: false,
        });
        let r = self.arrow_body();
        self.fn_ctx.pop();
        r
    }

    /// Lookahead: if the upcoming `( ... )` is `=>`-headed, parse its contents
    /// as an arrow parameter list and return it; otherwise leave the cursor
    /// untouched and return None (callers parse it as a parenthesized
    /// expression). `after_async` = the `async` keyword was already consumed
    /// by the caller's lookahead (peek position sits at the `( ... )` either
    /// way; scanning skips one token only in the non-async case).
    fn try_arrow_params(&mut self, after_async: bool) -> Result<Option<Vec<Param>>, JsError> {
        let lp = if after_async { self.pos + 1 } else { self.pos };
        debug_assert_eq!(self.toks[lp].tok, Tok::Punct(Punct::LParen));
        // scan to the `)` matching that `(`
        let mut depth = 0usize;
        let mut i = lp;
        let close = loop {
            match self.toks.get(i).map(|t| &t.tok) {
                Some(Tok::Punct(Punct::LParen)) | Some(Tok::Punct(Punct::LBrace))
                | Some(Tok::Punct(Punct::LBrack)) => depth += 1,
                Some(Tok::Punct(Punct::RParen)) | Some(Tok::Punct(Punct::RBrace))
                | Some(Tok::Punct(Punct::RBrack)) => {
                    depth -= 1;
                    if depth == 0 {
                        break i;
                    }
                }
                Some(Tok::Eof) | None => return Ok(None),
                _ => {}
            }
            i += 1;
        };
        if self.toks.get(close + 1).map(|t| &t.tok) != Some(&Tok::Op(Op::Arrow)) {
            return Ok(None);
        }
        // parse the slice between parens as a parameter list; append the
        // closing `)` (plus Eof) so params_after_lparen terminates naturally
        let mut inner: Vec<Token> = self.toks[lp + 1..close].to_vec();
        inner.push(Token {
            tok: Tok::Punct(Punct::RParen),
            start: 0,
            end: 0,
            nl: false,
        });
        inner.push(Token {
            tok: Tok::Eof,
            start: 0,
            end: 0,
            nl: false,
        });
        let mut sub = Parser {
            src: self.src,
            src_id: self.src_id,
            toks: inner,
            pos: 0,
            depth: 0,
            class_stack: self.class_stack.clone(),
            // arrow parameters inherit `yield` from the enclosing function;
            // an async arrow reserves `await` in its parameters too
            fn_ctx: vec![FnCtx {
                is_async: after_async,
                is_generator: self.fn_ctx.last().is_some_and(|c| c.is_generator),
            }],
            no_in: false,
            block_scopes: Vec::new(),
            cover_depth: 0,
        };
        let mut idx = 0usize;
        let params = sub.params_after_lparen(&mut idx)?;
        if sub.pos < sub.toks.len() && sub.toks[sub.pos].tok != Tok::Eof {
            return self.err("trailing tokens in arrow parameter list");
        }
        // advance main parser past `) =>`
        self.pos = close + 2;
        Ok(Some(params))
    }

    fn cond_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let test = self.coalesce_expr()?;
        if self.eat_punct(Punct::Question) {
            let then = self.assign_expr()?;
            self.expect_punct(Punct::Colon)?;
            let other = self.assign_expr()?;
            return Ok(Expr {
                kind: ExprKind::Cond {
                    test: Box::new(test),
                    then: Box::new(then),
                    other: Box::new(other),
                },
                span: self.span_from(start),
            });
        }
        Ok(test)
    }

    fn coalesce_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.or_expr()?;
        while self.eat_op(Op::Coalesce) {
            let r = self.or_expr()?;
            l = Expr {
                kind: ExprKind::Logical {
                    op: LogOp::Coalesce,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn or_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.and_expr()?;
        while self.eat_op(Op::OrOr) {
            let r = self.and_expr()?;
            l = Expr {
                kind: ExprKind::Logical {
                    op: LogOp::Or,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.bit_or_expr()?;
        while self.eat_op(Op::AndAnd) {
            let r = self.bit_or_expr()?;
            l = Expr {
                kind: ExprKind::Logical {
                    op: LogOp::And,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn bit_or_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.bit_xor_expr()?;
        while self.eat_op(Op::Or) {
            let r = self.bit_xor_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::BitOr,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn bit_xor_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.bit_and_expr()?;
        while self.eat_op(Op::Xor) {
            let r = self.bit_and_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::BitXor,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn bit_and_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.eq_expr()?;
        while self.eat_op(Op::And) {
            let r = self.eq_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::BitAnd,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn eq_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.rel_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Op(Op::Eq) => BinOp::Eq,
                Tok::Op(Op::Ne) => BinOp::Ne,
                Tok::Op(Op::StrictEq) => BinOp::StrictEq,
                Tok::Op(Op::StrictNe) => BinOp::StrictNe,
                _ => break,
            };
            self.next();
            let r = self.rel_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn rel_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.shift_expr()?;
        loop {
            // `#x in obj`: private brand check, not the generic `in`
            // operator (the left side is lexed as a private identifier).
            if matches!(self.peek(), Tok::Kw(Kw::In) if !self.no_in) {
                if let ExprKind::Ident(name) = &l.kind {
                    if name.starts_with('#') {
                        let pname = name.clone();
                        self.next(); // `in`
                        self.note_private_ref(&pname)?;
                        let obj = self.shift_expr()?;
                        l = Expr {
                            kind: ExprKind::PrivateIn {
                                name: pname,
                                obj: Box::new(obj),
                            },
                            span: self.span_from(start),
                        };
                        continue;
                    }
                }
            }
            let op = match self.peek() {
                Tok::Op(Op::Lt) => BinOp::Lt,
                Tok::Op(Op::Le) => BinOp::Le,
                Tok::Op(Op::Gt) => BinOp::Gt,
                Tok::Op(Op::Ge) => BinOp::Ge,
                Tok::Kw(Kw::In) if !self.no_in => BinOp::In,
                Tok::Kw(Kw::Instanceof) => BinOp::InstanceOf,
                _ => break,
            };
            self.next();
            let r = self.shift_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn shift_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.add_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Op(Op::Shl) => BinOp::Shl,
                Tok::Op(Op::Shr) => BinOp::Shr,
                Tok::Op(Op::UShr) => BinOp::UShr,
                _ => break,
            };
            self.next();
            let r = self.add_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn add_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Op(Op::Add) => BinOp::Add,
                Tok::Op(Op::Sub) => BinOp::Sub,
                _ => break,
            };
            self.next();
            let r = self.mul_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    fn mul_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut l = self.pow_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Op(Op::Mul) => BinOp::Mul,
                Tok::Op(Op::Div) => BinOp::Div,
                Tok::Op(Op::Mod) => BinOp::Mod,
                _ => break,
            };
            self.next();
            let r = self.pow_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        Ok(l)
    }

    /// `**` binds tighter than `*`/`/` and is right-associative
    /// (`2 ** 3 ** 2` == `2 ** (3 ** 2)`). shortcut: `-2 ** 2` yields
    /// `(-2) ** 2` (the spec rejects it) because parentheses are not tracked.
    fn pow_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let l = self.unary_expr()?;
        if self.eat_op(Op::Pow) {
            let r = self.pow_expr()?;
            return Ok(Expr {
                kind: ExprKind::Binary {
                    op: BinOp::Pow,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            });
        }
        Ok(l)
    }

    fn unary_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let op = match self.peek() {
            Tok::Op(Op::Sub) => Some(UnaryOp::Neg),
            Tok::Op(Op::Add) => Some(UnaryOp::Pos),
            Tok::Op(Op::Not) => Some(UnaryOp::Not),
            Tok::Op(Op::BitNot) => Some(UnaryOp::BitNot),
            Tok::Op(Op::Inc) => Some(UnaryOp::PreInc),
            Tok::Op(Op::Dec) => Some(UnaryOp::PreDec),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            if *self.peek() == Tok::Kw(Kw::Yield) {
                return self.err("`yield` cannot be the operand of a unary operator");
            }
            let expr = self.unary_expr()?;
            if matches!(op, UnaryOp::PreInc | UnaryOp::PreDec) {
                check_assign_target(&expr)?;
            }
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                span: self.span_from(start),
            });
        }
        if self.eat_kw(Kw::Typeof) {
            if *self.peek() == Tok::Kw(Kw::Yield) {
                return self.err("`yield` cannot be the operand of a unary operator");
            }
            let expr = self.unary_expr()?;
            return Ok(Expr {
                kind: ExprKind::Typeof(Box::new(expr)),
                span: self.span_from(start),
            });
        }
        if self.eat_kw(Kw::Void) {
            if *self.peek() == Tok::Kw(Kw::Yield) {
                return self.err("`yield` cannot be the operand of a unary operator");
            }
            let expr = self.unary_expr()?;
            return Ok(Expr {
                kind: ExprKind::Void(Box::new(expr)),
                span: self.span_from(start),
            });
        }
        if self.eat_kw(Kw::Delete) {
            if *self.peek() == Tok::Kw(Kw::Yield) {
                return self.err("`yield` cannot be the operand of a unary operator");
            }
            let expr = self.unary_expr()?;
            // A private reference is never a valid delete operand:
            // `delete obj.#x` and `delete obj.#m()` are early errors (also
            // when parenthesized, since the AST drops the parens).
            let private = match &expr.kind {
                ExprKind::Member { key, .. } => key.starts_with('#'),
                ExprKind::Call { callee, .. } => {
                    matches!(&callee.kind, ExprKind::Member { key, .. } if key.starts_with('#'))
                }
                _ => false,
            };
            if private {
                return self.err("`delete` cannot take a private reference");
            }
            return Ok(Expr {
                kind: ExprKind::Delete(Box::new(expr)),
                span: self.span_from(start),
            });
        }
        self.lhs_expr()
    }

    fn lhs_expr(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let mut e = if self.eat_kw(Kw::New) {
            // `new.target` (meta-property) before the constructor grammar
            if self.eat_punct(Punct::Dot) {
                let key = self.prop_name()?;
                if key.as_ref() != "target" {
                    return self.err("expected `new.target`");
                }
                Expr {
                    kind: ExprKind::NewTarget,
                    span: self.span_from(start),
                }
            } else {
            // new callee(args) — callee is a member chain WITHOUT a call
            let mut c = self.primary()?;
            // `new import(...)` parses as `import` called, but ImportCall is a
            // CallExpression and cannot be a constructor expression.
            if matches!(&c.kind, ExprKind::Ident(n) if n.as_ref() == "import") {
                return self.err("`new import(...)` is not valid syntax");
            }
            while self.eat_punct(Punct::Dot) {
                let key = self.prop_name()?;
                self.note_private_ref(&key)?;
                c = Expr {
                    kind: ExprKind::Member {
                        base: Box::new(c),
                        key,
                    },
                    span: self.span_from(start),
                };
            }
            if self.eat_punct(Punct::LBrack) {
                let idx = self.expr()?;
                self.expect_punct(Punct::RBrack)?;
                c = Expr {
                    kind: ExprKind::Index {
                        base: Box::new(c),
                        index: Box::new(idx),
                    },
                    span: self.span_from(start),
                };
            }
            let args = if self.eat_punct(Punct::LParen) {
                self.call_args()?
            } else {
                Vec::new()
            };
            Expr {
                kind: ExprKind::New {
                    callee: Box::new(c),
                    args,
                },
                span: self.span_from(start),
            }
            }
        } else {
            self.primary()?
        };
        // Postfix terms of the whole chain, in source order. They are folded
        // back into nested `Member`/`Index`/`Call` nodes unless a `?.`
        // appeared, in which case one `OptionalChain` node keeps the
        // short-circuit from node in one place.
        let mut steps: Vec<ChainStep> = Vec::new();
        let mut has_optional = false;
        loop {
            match self.peek() {
                Tok::Punct(Punct::Dot) => {
                    self.next();
                    let key = self.prop_name()?;
                    self.note_private_ref(&key)?;
                    if steps.is_empty()
                        && matches!(&e.kind, ExprKind::Ident(n) if n.as_ref() == "import")
                        && key.as_ref() != "meta"
                    {
                        return self.err("`import` only supports `import.meta` property access");
                    }
                    steps.push(ChainStep::Member {
                        key,
                        optional: false,
                    });
                }
                // `?.` is lexed as `Question` then `Dot`; `a?.5:b` (a
                // conditional) has a number after `?`, so it stays a
                // conditional. `a?.[i]` / `a?.(x)` are also `?` `.`.
                Tok::Punct(Punct::Question)
                    if *self.peek_at(1) == Tok::Punct(Punct::Dot) =>
                {
                    self.next();
                    self.next();
                    has_optional = true;
                    match self.peek() {
                        Tok::Punct(Punct::LBrack) => {
                            self.next();
                            let index = self.expr()?;
                            self.expect_punct(Punct::RBrack)?;
                            steps.push(ChainStep::Index {
                                index,
                                optional: true,
                            });
                        }
                        Tok::Punct(Punct::LParen) => {
                            self.next();
                            let args = self.call_args()?;
                            steps.push(ChainStep::Call {
                                args,
                                optional: true,
                            });
                        }
                        _ => {
                            // `a?.b`: a property name follows the dot
                            let key = self.prop_name()?;
                            self.note_private_ref(&key)?;
                            steps.push(ChainStep::Member {
                                key,
                                optional: true,
                            });
                        }
                    }
                }
                Tok::Punct(Punct::LBrack) => {
                    self.next();
                    let index = self.expr()?;
                    self.expect_punct(Punct::RBrack)?;
                    steps.push(ChainStep::Index {
                        index,
                        optional: false,
                    });
                }
                Tok::Punct(Punct::LParen) => {
                    self.next();
                    let args = self.call_args()?;
                    steps.push(ChainStep::Call {
                        args,
                        optional: false,
                    });
                }
                Tok::Template(parts) => {
                    let parts = parts.clone();
                    self.next();
                    steps.push(ChainStep::Tagged {
                        segs: self.template_segs(parts)?,
                    });
                }
                _ => break,
            }
        }
        let mut e = if has_optional {
            Expr {
                kind: ExprKind::OptionalChain {
                    base: Box::new(e),
                    steps,
                },
                span: self.span_from(start),
            }
        } else {
            for st in steps {
                e = match st {
                    ChainStep::Member { key, .. } => Expr {
                        kind: ExprKind::Member {
                            base: Box::new(e),
                            key,
                        },
                        span: self.span_from(start),
                    },
                    ChainStep::Index { index, .. } => Expr {
                        kind: ExprKind::Index {
                            base: Box::new(e),
                            index: Box::new(index),
                        },
                        span: self.span_from(start),
                    },
                    ChainStep::Call { args, .. } => {
                        // ImportCall takes one AssignmentExpression and an
                        // optional second (import attributes) argument; no
                        // spread, no third argument.
                        if matches!(&e.kind, ExprKind::Ident(n) if n.as_ref() == "import") {
                            if args.is_empty()
                                || args.len() > 2
                                || args.iter().any(|a| matches!(a, CallArg::Spread(_)))
                            {
                                return self.err(
                                    "import() takes one argument (plus optional import attributes)",
                                );
                            }
                        }
                        Expr {
                            kind: ExprKind::Call {
                                callee: Box::new(e),
                                args,
                            },
                            span: self.span_from(start),
                        }
                    }
                    ChainStep::Tagged { segs } => Expr {
                        kind: ExprKind::TaggedTemplate {
                            tag: Box::new(e),
                            segs,
                        },
                        span: self.span_from(start),
                    },
                };
            }
            e
        };
        // postfix ++/-- binds tighter than any binary operator
        match self.peek() {
            Tok::Op(Op::Inc) => {
                self.next();
                check_assign_target(&e)?;
                e = Expr {
                    kind: ExprKind::Postfix {
                        target: Box::new(e),
                        inc: true,
                    },
                    span: self.span_from(start),
                };
            }
            Tok::Op(Op::Dec) => {
                self.next();
                check_assign_target(&e)?;
                e = Expr {
                    kind: ExprKind::Postfix {
                        target: Box::new(e),
                        inc: false,
                    },
                    span: self.span_from(start),
                };
            }
            _ => {}
        }
        Ok(e)
    }

    fn call_args(&mut self) -> Result<Vec<CallArg>, JsError> {
        let mut args = Vec::new();
        while !self.eat_punct(Punct::RParen) {
            if !args.is_empty() {
                self.expect_punct(Punct::Comma)?;
                // trailing comma in an argument list is legal: `f(a,)`
                if self.eat_punct(Punct::RParen) {
                    break;
                }
            }
            if *self.peek() == Tok::Spread {
                self.next();
                args.push(CallArg::Spread(self.assign_expr()?));
            } else {
                args.push(CallArg::Arg(self.assign_expr()?));
            }
        }
        Ok(args)
    }

    /// Turn lexed template parts into parsed segments (interpolations are
    /// parsed by a sub-parser over the token slice the lexer produced).
    fn template_segs(&mut self, parts: Vec<TplPart>) -> Result<Vec<TplSeg>, JsError> {
        let mut segs = Vec::new();
        for p in parts {
            match p {
                TplPart::Lit(s) => segs.push(TplSeg::Lit(s)),
                TplPart::Expr(toks) => {
                    let mut toks = toks;
                    toks.push(Token {
                        tok: Tok::Eof,
                        start: 0,
                        end: 0,
                        nl: false,
                    });
                    let mut sub = Parser {
                        src: self.src,
                        src_id: self.src_id,
                        toks,
                        pos: 0,
                        depth: 0,
                        class_stack: self.class_stack.clone(),
                        fn_ctx: self.fn_ctx.clone(),
                        no_in: false,
                        block_scopes: Vec::new(),
                        cover_depth: 0,
                    };
                    let e = sub.expr()?;
                    segs.push(TplSeg::Expr(e));
                }
            }
        }
        Ok(segs)
    }

    fn primary(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let kind = match self.peek().clone() {
            Tok::Num(n) => {
                self.next();
                ExprKind::Num(n)
            }
            Tok::BigInt(s) => {
                self.next();
                match s.parse::<i128>() {
                    Ok(n) => ExprKind::BigInt(n),
                    Err(_) => return self.err("BigInt literal too large"),
                }
            }
            Tok::Str(s) => {
                self.next();
                ExprKind::Str(s)
            }
            Tok::Kw(Kw::True) => {
                self.next();
                ExprKind::Bool(true)
            }
            Tok::Kw(Kw::False) => {
                self.next();
                ExprKind::Bool(false)
            }
            Tok::Kw(Kw::Null) => {
                self.next();
                ExprKind::Null
            }
            Tok::Kw(Kw::This) => {
                self.next();
                ExprKind::This
            }
            Tok::Kw(Kw::Super) => {
                self.next();
                let ctx = match self.class_stack.last() {
                    Some(c) => c.clone(),
                    None => return self.err("`super` is only valid inside a class"),
                };
                let sn = match ctx.super_name {
                    Some(sn) => sn,
                    None => return self.err("`super` requires the class to have `extends`"),
                };
                let class = Box::new(Expr {
                    kind: ExprKind::Ident(sn),
                    span: self.span_from(start),
                });
                if self.eat_punct(Punct::LParen) {
                    if !ctx.in_constructor {
                        return self.err(
                            "`super()` is only valid inside a class constructor",
                        );
                    }
                    let args = self.call_args()?;
                    ExprKind::SuperCall {
                        class,
                        key: None,
                        static_: ctx.is_static,
                        args,
                    }
                } else if self.eat_punct(Punct::Dot) {
                    let key = self.prop_name()?;
                    if key.starts_with('#') {
                        return self.err("a private name cannot be accessed through `super`");
                    }
                    ExprKind::SuperMember {
                        class,
                        key,
                        static_: ctx.is_static,
                    }
                } else {
                    return self.err("expected `(` or `.` after `super`");
                }
            }
            Tok::Kw(Kw::Function) => {
                self.next();
                let is_gen = matches!(self.peek(), Tok::Op(Op::Mul));
                if is_gen {
                    self.next(); // `*`
                }
                let f = self.function_rest(false, false, is_gen)?;
                ExprKind::Func(f)
            }
            Tok::Ident(s) => {
                // `import` is not an identifier expression: it may only start a
                // dynamic-import call or `import.meta`. Accepting it as a bare
                // identifier made `typeof import`, `import.name` and
                // `new import('')` parse when the spec makes them early errors.
                if s.as_ref() == "import"
                    && !matches!(
                        *self.peek_at(1),
                        Tok::Punct(Punct::LParen) | Tok::Punct(Punct::Dot)
                    )
                {
                    return self.err("`import` is only valid as `import(...)` or `import.meta`");
                }
                if is_reserved_word(&s) {
                    return self.err(format!("`{s}` is a reserved word"));
                }
                self.check_ctx_ident(&s)?;
                self.next();
                ExprKind::Ident(s)
            }
            Tok::Template(parts) => {
                self.next();
                ExprKind::Template(self.template_segs(parts)?)
            }
            Tok::Regex(p, f) => {
                self.next();
                // a regex literal is validated at parse time: an invalid
                // pattern is a SyntaxError even if the literal never runs
                // (test262's parse-phase negative cases rely on this)
                if let Err(e) = super::regex::compile(&p, &f) {
                    return self.err(&format!("/{}/{}: {}", p, f, e.msg));
                }
                ExprKind::Regex {
                    pattern: p,
                    flags: f,
                }
            }
            Tok::Kw(Kw::Yield) => {
                self.next();
                if *self.peek() == Tok::Eof {
                    return self.err("unterminated yield expression");
                }
                // `yield* x` delegates to another iterator; `*` cannot start
                // an operand, so it is never ambiguous with multiplication
                if self.eat_op(Op::Mul) {
                    if *self.peek() == Tok::Eof {
                        return self.err("unterminated `yield*` expression");
                    }
                    let v = self.assign_expr()?;
                    ExprKind::YieldStar(Box::new(v))
                } else {
                    // yield's operand is an AssignmentExpression (`yield a + b`
                    // yields a+b) per JS grammar
                    let v = match self.peek() {
                        Tok::Punct(Punct::Semi)
                        | Tok::Punct(Punct::RParen)
                        | Tok::Punct(Punct::RBrace)
                        | Tok::Punct(Punct::RBrack)
                        | Tok::Punct(Punct::Comma) => None,
                        _ => Some(Box::new(self.assign_expr()?)),
                    };
                    ExprKind::Yield(v)
                }
            }
            Tok::Kw(Kw::Await) => {
                self.next();
                let inner = self.unary_expr()?;
                ExprKind::Await(Box::new(inner))
            }
            Tok::Kw(Kw::Async) => {
                if *self.peek_at(1) == Tok::Kw(Kw::Function) {
                    self.next();
                    self.next();
                    let is_gen = self.eat_op(Op::Mul);
                    let f = self.function_rest(false, true, is_gen)?;
                    ExprKind::Func(f)
                } else {
                    // `async => ...` (async arrow whose single param is named
                    // `async`) falls through to the ident/arrow paths; a bare
                    // `async` expression is invalid JS
                    return self.err("unexpected `async` keyword (not an async arrow function)");
                }
            }
            Tok::Kw(Kw::Class) => {
                // class expression: `const C = class extends B { ... }`
                self.next();
                let (name, anon) = match self.peek() {
                    Tok::Ident(n) => {
                        let n = n.clone();
                        self.next();
                        (n, false)
                    }
                    _ => (
                        Rc::from(format!("__class{}", next_class_id()).as_str()),
                        true,
                    ),
                };
                let stmts = self.class_rest_named(name.clone())?;
                ExprKind::ClassExpr { name, anon, stmts }
            }
            Tok::Punct(Punct::LParen) => {
                self.next();
                // the head grammar's `in` suppression does not reach inside
                // parentheses: `for (var x = (a in b); ;)` is valid
                let prev_no_in = self.no_in;
                self.no_in = false;
                // `({x = 1} = obj)` / `({x = 1}) => {}` start from a cover form
                self.cover_depth += 1;
                let e = self.expr();
                self.cover_depth -= 1;
                self.no_in = prev_no_in;
                let e = e?;
                self.expect_punct(Punct::RParen)?;
                return Ok(e);
            }
            Tok::Punct(Punct::LBrack) => {
                self.next();
                // array elements are their own grammar: `for (var x = [a in b];;)`
                let prev_no_in = self.no_in;
                self.no_in = false;
                // `[{x = 1}] = arr` needs the element cover form to parse
                self.cover_depth += 1;
                let mut items = Vec::new();
                loop {
                    if self.eat_punct(Punct::RBrack) {
                        break;
                    }
                    // a comma at an element position is an elision: leading
                    // `[,a]`, interior `[a,,b]`, and `[,]` all produce holes
                    if self.eat_punct(Punct::Comma) {
                        items.push(ArrayElem::Hole);
                        continue;
                    }
                    if *self.peek() == Tok::Spread {
                        self.next();
                        let e = self.assign_expr()?;
                        // `[...a,]`: the trailing comma is legal here but not
                        // once this literal is a destructuring pattern
                        let trailing = matches!(self.peek(), Tok::Punct(Punct::Comma))
                            && matches!(
                                self.toks.get(self.pos + 1).map(|t| &t.tok),
                                Some(Tok::Punct(Punct::RBrack))
                            );
                        items.push(ArrayElem::Spread(e, trailing));
                    } else {
                        items.push(ArrayElem::Item(self.assign_expr()?));
                    }
                    // after an element: `,` starts the next slot, `]` ends
                    if !self.eat_punct(Punct::Comma)
                        && !matches!(self.peek(), Tok::Punct(Punct::RBrack))
                    {
                        self.expect_punct(Punct::Comma)?;
                    }
                }
                self.cover_depth -= 1;
                self.no_in = prev_no_in;
                ExprKind::Array(items)
            }
            Tok::Punct(Punct::LBrace) => {
                self.next();
                // property values reset `in` suppression (object-literal grammar)
                let prev_no_in = self.no_in;
                self.no_in = false;
                let mut props: Vec<ObjElem> = Vec::new();
                while !self.eat_punct(Punct::RBrace) {
                    if !props.is_empty() {
                        self.expect_punct(Punct::Comma)?;
                    }
                    if self.eat_punct(Punct::RBrace) {
                        break; // trailing comma
                    }
                    if *self.peek() == Tok::Spread {
                        self.next();
                        let e = self.assign_expr()?;
                        // `{...a,}`: trailing comma is legal as an expression
                        // but not when the literal becomes a pattern
                        let trailing = matches!(self.peek(), Tok::Punct(Punct::Comma))
                            && matches!(
                                self.toks.get(self.pos + 1).map(|t| &t.tok),
                                Some(Tok::Punct(Punct::RBrace))
                            );
                        props.push(ObjElem::Spread(e, trailing));
                        continue;
                    }
                    // `async m(){}` / `async *m(){}` / `*m(){}` method
                    // shorthand (mirrors the class-method modifiers). A
                    // property literally named `async` has `:`, `(`, `,` or
                    // `}` next, so it is not consumed as a modifier.
                    let mut is_async = false;
                    let mut is_gen = false;
                    let async_ident = match self.peek() {
                        Tok::Kw(Kw::Async) => true,
                        Tok::Ident(s) if s.as_ref() == "async" => {
                            !self.ident_escaped(self.pos)
                        }
                        _ => false,
                    };
                    if async_ident {
                        let is_async_method =
                            !self.toks[(self.pos + 1).min(self.toks.len() - 1)].nl
                                && matches!(
                                    self.peek_at(1),
                                    Tok::Ident(_)
                                        | Tok::Kw(_)
                                        | Tok::Str(_)
                                        | Tok::Num(_)
                                        | Tok::Op(Op::Mul)
                                );
                        if is_async_method {
                            self.next(); // `async`
                            is_async = true;
                        }
                    }
                    if *self.peek() == Tok::Op(Op::Mul) {
                        self.next(); // `*`
                        is_gen = true;
                    }
                    if is_async || is_gen {
                        let key_tok = self.pos;
                        let key = self.prop_name()?;
                        self.fn_ctx.push(FnCtx {
                            is_async,
                            is_generator: is_gen,
                        });
                        let params = self.param_list()?;
                        self.expect_punct(Punct::LBrace)?;
                        let mut body = self.fn_block_body()?;
                        self.fn_ctx.pop();
                        self.check_strict_body(&params, &body)?;
                        if is_gen {
                            // sync generators keep the strict yield-position
                            // check; async ones defer to call time
                            self.prepare_generator_body(&mut body, is_async)?;
                        }
                        let uses_arguments = scan_uses_arguments_stmts(&body);
                        let f = Rc::new(FnDef {
                            src: self.src_id,
                            name: key,
                            params,
                            body: FnBody::Block(body),
                            uses_arguments,
                            is_async,
                            is_generator: is_gen,
                            has_yield: is_gen,
                        });
                        props.push(ObjElem::Prop(
                            self.toks[key_tok].clone().tok_string(),
                            Expr {
                                kind: ExprKind::Func(f),
                                span: self.span_from(start),
                            },
                        ));
                        continue;
                    }
                    // computed key: `[expr]: v` / `[expr](){}`
                    if *self.peek() == Tok::Punct(Punct::LBrack) {
                        self.next();
                        let key_e = self.assign_expr()?;
                        self.expect_punct(Punct::RBrack)?;
                        let value = if *self.peek() == Tok::Punct(Punct::LParen) {
                            // computed method shorthand
                            self.fn_ctx.push(FnCtx::default());
                            let params = self.param_list()?;
                            self.expect_punct(Punct::LBrace)?;
                            let body = self.fn_block_body()?;
                            self.fn_ctx.pop();
                            self.check_strict_body(&params, &body)?;
                            let uses_arguments = scan_uses_arguments_stmts(&body);
                            Expr {
                                kind: ExprKind::Func(Rc::new(FnDef {
                                    src: self.src_id,
                                    name: Rc::from(""),
                                    params,
                                    body: FnBody::Block(body),
                                    uses_arguments,
                                    is_async: false,
                                    is_generator: false,
                                    has_yield: false,
                                })),
                                span: self.span_from(start),
                            }
                        } else {
                            self.expect_punct(Punct::Colon)?;
                            // `{a: {x = 1}} = obj` — the value may be a nested
                            // pattern, so the cover form is still allowed here
                            self.cover_depth += 1;
                            let v = self.assign_expr();
                            self.cover_depth -= 1;
                            v?
                        };
                        props.push(ObjElem::Computed {
                            key: key_e,
                            value,
                        });
                        continue;
                    }
                    // accessor: `get x(){}` / `set x(v){}` � only when a
                    // property name follows, so `{get: 1}` and `{get(){}}`
                    // stay ordinary properties/methods.
                    let accessor = match self.peek() {
                        Tok::Ident(s) if (&**s == "get" || &**s == "set") => {
                            !self.ident_escaped(self.pos)
                                && matches!(
                                    self.peek_at(1),
                                    Tok::Ident(_)
                                        | Tok::Str(_)
                                        | Tok::Num(_)
                                        | Tok::Kw(_)
                                        | Tok::Punct(Punct::LBrack)
                                )
                        }
                        _ => false,
                    };
                    if accessor {
                        let is_setter = matches!(self.peek(), Tok::Ident(s) if &**s == "set");
                        self.next(); // `get` / `set`
                        let computed_key = if *self.peek() == Tok::Punct(Punct::LBrack) {
                            self.next();
                            let e = self.assign_expr()?;
                            self.expect_punct(Punct::RBrack)?;
                            Some(e)
                        } else {
                            None
                        };
                        let key = match &computed_key {
                            Some(_) => Rc::from(if is_setter { "set" } else { "get" }),
                            None => self.prop_name()?,
                        };
                        self.fn_ctx.push(FnCtx::default());
                        let params = self.param_list()?;
                        self.expect_punct(Punct::LBrace)?;
                        let body = self.fn_block_body()?;
                        self.fn_ctx.pop();
                        self.check_strict_body(&params, &body)?;
                        let uses_arguments = scan_uses_arguments_stmts(&body);
                        let f = Rc::new(FnDef {
                            src: self.src_id,
                            name: key.clone(),
                            params,
                            body: FnBody::Block(body),
                            uses_arguments,
                            is_async: false,
                            is_generator: false,
                            has_yield: false,
                        });
                        let getter = if is_setter { None } else { Some(f.clone()) };
                        let setter = if is_setter { Some(f) } else { None };
                        match computed_key {
                            Some(key) => props.push(ObjElem::ComputedAccessor {
                                key,
                                getter,
                                setter,
                            }),
                            None => props.push(ObjElem::Accessor {
                                key,
                                getter,
                                setter,
                            }),
                        }
                        continue;
                    }
                    // method shorthand: `m(){}` / `"m"(){}`
                    let save = self.pos;
                    let key = self.prop_name()?;
                    if *self.peek() == Tok::Punct(Punct::LParen) {
                        self.fn_ctx.push(FnCtx::default());
                        let params = self.param_list()?;
                        self.expect_punct(Punct::LBrace)?;
                        let body = self.fn_block_body()?;
                        self.fn_ctx.pop();
                        self.check_strict_body(&params, &body)?;
                        let uses_arguments = scan_uses_arguments_stmts(&body);
                        let f = Rc::new(FnDef {
                            src: self.src_id,
                            name: key,
                            params,
                            body: FnBody::Block(body),
                            uses_arguments,
                            is_async: false,
                            is_generator: false,
                            has_yield: false,
                        });
                        props.push(ObjElem::Prop(
                            self.toks[save].clone().tok_string(),
                            Expr {
                                kind: ExprKind::Func(f),
                                span: self.span_from(start),
                            },
                        ));
                        continue;
                    }
                    if *self.peek() == Tok::Punct(Punct::Colon) {
                        self.next();
                        // property values may themselves be patterns
                        self.cover_depth += 1;
                        let v = self.assign_expr();
                        self.cover_depth -= 1;
                        props.push(ObjElem::Prop(key, v?));
                    } else if *self.peek() == Tok::Op(Op::Assign) {
                        // `{x = default}` — CoverInitializedName. Only a valid
                        // pattern element; the assign_expr guard rejects it if
                        // this literal is not converted to a pattern.
                        if is_reserved_word(&key) {
                            return self.err(format!("`{key}` is a reserved word"));
                        }
                        self.check_ctx_ident(&key)?;
                        self.next();
                        let v = self.assign_expr()?;
                        props.push(ObjElem::CoverInit(key, v));
                    } else {
                        // shorthand `{a}` -> `{a: a}`
                        if is_reserved_word(&key) {
                            return self.err(format!("`{key}` is a reserved word"));
                        }
                        self.check_ctx_ident(&key)?;
                        props.push(ObjElem::Prop(
                            key.clone(),
                            Expr {
                                kind: ExprKind::Ident(key),
                                span: self.span_from(start),
                            },
                        ));
                    }
                }
                self.no_in = prev_no_in;
                ExprKind::Object(props)
            }
            other => {
                return self.err(format!("unexpected token `{}` in expression", tok_str(&other)))
            }
        };
        Ok(Expr {
            kind,
            span: self.span_from(start),
        })
    }
}

enum ForHead {
    Classic {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        upd: Option<Expr>,
    },
    In {
        kind: Option<VarKind>,
        target: ForTarget,
        obj: Expr,
    },
    Of {
        kind: Option<VarKind>,
        target: ForTarget,
        iter: Expr,
    },
}

/// Does this expression still carry an unresolved `{x = default}` cover form?
/// Only the walk needed for the parse-time rejection: a form that escaped a
/// pattern position. Functions/classes are opaque (their bodies were parsed
/// and checked separately), so the fallback is `false`.
fn expr_has_cover_init(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Object(props) => props.iter().any(|p| match p {
            ObjElem::CoverInit(..) => true,
            ObjElem::Prop(_, v) | ObjElem::Spread(v, _) => expr_has_cover_init(v),
            ObjElem::Computed { key, value } => {
                expr_has_cover_init(key) || expr_has_cover_init(value)
            }
            ObjElem::ComputedAccessor { key, .. } => expr_has_cover_init(key),
            ObjElem::Accessor { .. } => false,
        }),
        ExprKind::Array(items) => items.iter().any(|i| match i {
            ArrayElem::Item(x) | ArrayElem::Spread(x, _) => expr_has_cover_init(x),
            ArrayElem::Hole => false,
        }),
        ExprKind::Assign { target, value, .. } => {
            expr_has_cover_init(target) || expr_has_cover_init(value)
        }
        ExprKind::LogicalAssign { target, value, .. } => {
            expr_has_cover_init(target) || expr_has_cover_init(value)
        }
        ExprKind::Logical { l, r, .. } | ExprKind::Binary { l, r, .. } => {
            expr_has_cover_init(l) || expr_has_cover_init(r)
        }
        ExprKind::Unary { expr, .. }
        | ExprKind::Postfix { target: expr, .. }
        | ExprKind::Typeof(expr)
        | ExprKind::Void(expr)
        | ExprKind::Delete(expr)
        | ExprKind::Await(expr)
        | ExprKind::YieldStar(expr)
        | ExprKind::PrivateIn { obj: expr, .. } => expr_has_cover_init(expr),
        ExprKind::Yield(Some(expr)) => expr_has_cover_init(expr),
        ExprKind::Yield(None) => false,
        ExprKind::Cond { test, then, other } => {
            expr_has_cover_init(test)
                || expr_has_cover_init(then)
                || expr_has_cover_init(other)
        }
        ExprKind::Seq(list) => list.iter().any(expr_has_cover_init),
        ExprKind::Call { callee, args } | ExprKind::New { callee, args } => {
            expr_has_cover_init(callee) || args.iter().any(arg_has_cover_init)
        }
        ExprKind::Member { base, .. } => expr_has_cover_init(base),
        ExprKind::Index { base, index } => {
            expr_has_cover_init(base) || expr_has_cover_init(index)
        }
        ExprKind::OptionalChain { base, steps } => {
            expr_has_cover_init(base)
                || steps.iter().any(|s| match s {
                    ChainStep::Member { .. } => false,
                    ChainStep::Index { index, .. } => expr_has_cover_init(index),
                    ChainStep::Call { args, .. } => args.iter().any(arg_has_cover_init),
                    ChainStep::Tagged { segs } => segs.iter().any(tpl_has_cover_init),
                })
        }
        ExprKind::SuperCall { class, args, .. } => {
            expr_has_cover_init(class) || args.iter().any(arg_has_cover_init)
        }
        ExprKind::SuperMember { class, .. } => expr_has_cover_init(class),
        ExprKind::Template(segs) => segs.iter().any(tpl_has_cover_init),
        ExprKind::TaggedTemplate { tag, segs } => {
            expr_has_cover_init(tag) || segs.iter().any(tpl_has_cover_init)
        }
        ExprKind::PatAssign { value, .. } => expr_has_cover_init(value),
        // a function/class body was parsed and validated on its own
        ExprKind::Func(_) | ExprKind::Arrow(_) | ExprKind::ClassExpr { .. } => false,
        _ => false,
    }
}

fn arg_has_cover_init(a: &CallArg) -> bool {
    match a {
        CallArg::Arg(e) | CallArg::Spread(e) => expr_has_cover_init(e),
    }
}

fn tpl_has_cover_init(s: &TplSeg) -> bool {
    match s {
        TplSeg::Lit(_) => false,
        TplSeg::Expr(e) => expr_has_cover_init(e),
    }
}

/// Convert an already-parsed array/object literal expression into the
/// equivalent destructuring pattern, for assignment destructuring
/// (`[a, b] = pair`) and the for-of assignment form. Rejects anything that is
/// not a valid binding/assignment target.
pub fn expr_to_pat(e: &Expr) -> Result<Pat, String> {
    match &e.kind {
        ExprKind::Ident(s) => Ok(Pat::Name(BindTgt::Name(s.clone()))),
        ExprKind::Member { base, key } => Ok(Pat::Name(BindTgt::Member {
            base: base.clone(),
            key: key.clone(),
        })),
        ExprKind::Index { base, index } => Ok(Pat::Name(BindTgt::Index {
            base: base.clone(),
            index: index.clone(),
        })),
        ExprKind::Array(elems) => {
            let mut parts = Vec::new();
            for (i, el) in elems.iter().enumerate() {
                let part = match el {
                    ArrayElem::Hole => PatPart::Hole,
                    ArrayElem::Spread(s, trailing) => {
                        // AssignmentRestElement must be the last element and
                        // takes no trailing comma (`[...a, b]`, `[...a,]`).
                        if *trailing || i + 1 != elems.len() {
                            return Err(String::from("rest element must be last"));
                        }
                        PatPart::Rest(Box::new(
                            expr_to_pat(s)
                                .map_err(|_| String::from("rest target must be a simple binding"))?,
                        ))
                    }
                    ArrayElem::Item(x) => match &x.kind {
                        ExprKind::Ident(_) | ExprKind::Member { .. } | ExprKind::Index { .. } => {
                            PatPart::Bind(Box::new(expr_to_pat(x)?), None)
                        }
                        ExprKind::Assign {
                            target,
                            op: None,
                            value,
                        } => PatPart::Bind(Box::new(expr_to_pat(target)?), Some((**value).clone())),
                        ExprKind::Array(_) | ExprKind::Object(_) => {
                            PatPart::Bind(Box::new(expr_to_pat(x)?), None)
                        }
                        _ => return Err(String::from("invalid destructuring target")),
                    },
                };
                parts.push(part);
            }
            Ok(Pat::Arr(parts))
        }
        ExprKind::Object(props) => {
            let mut parts = Vec::new();
            for (i, el) in props.iter().enumerate() {
                match el {
                    ObjElem::Spread(s, trailing) => {
                        // AssignmentRestProperty must be last and takes no
                        // trailing comma (`{...a, b}`, `{...a,}`).
                        if *trailing || i + 1 != props.len() {
                            return Err(String::from("rest element must be last"));
                        }
                        parts.push(ObjPatPart::Rest(
                            expr_to_pat(s)
                                .map_err(|_| String::from("rest target must be a simple binding"))?,
                        ));
                    }
                    ObjElem::Prop(key, v) => {
                        let part = match &v.kind {
                            ExprKind::Ident(_) | ExprKind::Member { .. } | ExprKind::Index { .. } => {
                                PatPart::Bind(Box::new(expr_to_pat(v)?), None)
                            }
                            ExprKind::Assign {
                                target,
                                op: None,
                                value,
                            } => PatPart::Bind(Box::new(expr_to_pat(target)?), Some((**value).clone())),
                            ExprKind::Array(_) | ExprKind::Object(_) => {
                                PatPart::Bind(Box::new(expr_to_pat(v)?), None)
                            }
                            _ => return Err("invalid destructuring binding".into()),
                        };
                        parts.push(ObjPatPart::Prop { key: key.clone(), part });
                    }
                    ObjElem::CoverInit(key, v) => {
                        // `{x = default}` -> `{x: x = default}`
                        let mut value = v.clone();
                        name_anon_fn(key, &mut value);
                        parts.push(ObjPatPart::Prop {
                            key: key.clone(),
                            part: PatPart::Bind(
                                Box::new(Pat::Name(BindTgt::Name(key.clone()))),
                                Some(value),
                            ),
                        });
                    }
                    ObjElem::Computed { .. }
                    | ObjElem::Accessor { .. }
                    | ObjElem::ComputedAccessor { .. } => {
                        return Err(
                            "computed/accessor keys are not supported in a destructuring pattern"
                                .into(),
                        )
                    }
                }
            }
            Ok(Pat::Obj(parts))
        }
        _ => Err("not a destructuring pattern".into()),
    }
}

fn check_assign_target(e: &Expr) -> Result<(), JsError> {
    match e.kind {
        ExprKind::Ident(_) | ExprKind::Member { .. } | ExprKind::Index { .. } => Ok(()),
        _ => Err(JsError::Syntax {
            msg: "invalid assignment target".into(),
            line: 0,
            col: 0,
        }),
    }
}

fn num_key(n: f64) -> Rc<str> {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        Rc::from(format!("{}", n as i64))
    } else {
        Rc::from(format!("{}", n))
    }
}

fn next_class_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

/// every identifier a declaration pattern binds (`export const {a, b: c} = o`)
fn pat_binding_names(p: &Pat, out: &mut Vec<Rc<str>>) {
    match p {
        Pat::Name(BindTgt::Name(n)) => out.push(n.clone()),
        Pat::Name(_) => {}
        Pat::Arr(parts) => {
            for part in parts {
                match part {
                    PatPart::Hole => {}
                    PatPart::Bind(p, _) | PatPart::Rest(p) => pat_binding_names(p, out),
                }
            }
        }
        Pat::Obj(props) => {
            for prop in props {
                match prop {
                    ObjPatPart::Prop { part, .. } | ObjPatPart::ComputedProp { part, .. } => {
                        match part {
                            PatPart::Hole => {}
                            PatPart::Bind(p, _) | PatPart::Rest(p) => pat_binding_names(p, out),
                        }
                    }
                    ObjPatPart::Rest(p) => pat_binding_names(p, out),
                }
            }
        }
    }
}

fn tok_str(t: &Tok) -> String {
    match t {
        Tok::Num(n) => format!("{}", n),
        Tok::BigInt(s) => format!("{}n", s),
        Tok::Str(s) => format!("{:?}", s),
        Tok::Template(_) => "template literal".into(),
        Tok::Regex(..) => "regex literal".into(),
        Tok::Spread => "...".into(),
        Tok::Ident(s) => s.to_string(),
        Tok::Kw(k) => format!("{:?}", k).to_lowercase(),
        Tok::Op(o) => format!("{:?}", o),
        Tok::Punct(p) => format!("{:?}", p),
        Tok::Eof => "end of input".into(),
    }
}

impl Token {
    /// property name from a token (used by object-literal method shorthand)
    fn tok_string(&self) -> Rc<str> {
        match &self.tok {
            Tok::Ident(s) | Tok::Str(s) => s.clone(),
            Tok::Num(n) => num_key(*n),
            Tok::Kw(k) => Rc::from(format!("{:?}", k).to_lowercase()),
            _ => Rc::from("?"),
        }
    }
}

// ---------------------------------------------------------------------------
// yield/await scans
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum SusKind {
    Yield,
    Await,
}

fn scan_kind(e: &Expr, k: SusKind) -> bool {
    match &e.kind {
        ExprKind::Yield(_) | ExprKind::YieldStar(_) => k == SusKind::Yield,
        ExprKind::Await(_) => k == SusKind::Await,
        _ => false,
    }
}

/// does this expression tree contain a yield/await anywhere (excluding
/// nested function bodies, which own their suspension points)?
fn expr_uses(e: &Expr, k: SusKind) -> bool {
    use ExprKind::*;
    match &e.kind {
        Yield(op) => k == SusKind::Yield || op.as_deref().map_or(false, |x| expr_uses(x, k)),
        YieldStar(x) => k == SusKind::Yield || expr_uses(x, k),
        Await(inner) => k == SusKind::Await || expr_uses(inner, k),
        Unary { expr, .. } | Typeof(expr) | Void(expr) | Delete(expr) => expr_uses(expr, k),
        Postfix { target, .. } => expr_uses(target, k),
        Binary { l, r, .. } | Logical { l, r, .. } => expr_uses(l, k) || expr_uses(r, k),
        Assign { target, value, .. } => expr_uses(target, k) || expr_uses(value, k),
        Cond { test, then, other } => {
            expr_uses(test, k) || expr_uses(then, k) || expr_uses(other, k)
        }
        Call { callee, args } | New { callee, args } => {
            expr_uses(callee, k)
                || args.iter().any(|a| match a {
                    CallArg::Arg(x) | CallArg::Spread(x) => expr_uses(x, k),
                })
        }
        Member { base, .. } => expr_uses(base, k),
        Index { base, index } => expr_uses(base, k) || expr_uses(index, k),
        SuperCall { class, args, .. } => {
            expr_uses(class, k)
                || args.iter().any(|a| match a {
                    CallArg::Arg(x) | CallArg::Spread(x) => expr_uses(x, k),
                })
        }
        SuperMember { class, .. } => expr_uses(class, k),
        OptionalChain { base, steps } => {
            expr_uses(base, k)
                || steps.iter().any(|s| match s {
                    ChainStep::Member { .. } => false,
                    ChainStep::Index { index, .. } => expr_uses(index, k),
                    ChainStep::Call { args, .. } => args.iter().any(|a| match a {
                        CallArg::Arg(x) | CallArg::Spread(x) => expr_uses(x, k),
                    }),
                    ChainStep::Tagged { segs } => segs.iter().any(|s| match s {
                        TplSeg::Lit(_) => false,
                        TplSeg::Expr(x) => expr_uses(x, k),
                    }),
                })
        }
        Array(items) => items.iter().any(|i| match i {
            ArrayElem::Item(x) | ArrayElem::Spread(x, _) => expr_uses(x, k),
            ArrayElem::Hole => false,
        }),
        Object(props) => props.iter().any(|p| match p {
            ObjElem::Prop(_, v) => expr_uses(v, k),
            ObjElem::CoverInit(_, v) => expr_uses(v, k),
            ObjElem::Spread(v, _) => expr_uses(v, k),
            ObjElem::Computed { key, value } => expr_uses(key, k) || expr_uses(value, k),
            // computed accessor keys are evaluated here; bodies are nested
            ObjElem::ComputedAccessor { key, .. } => expr_uses(key, k),
            ObjElem::Accessor { .. } => false,
        }),
        PatAssign { pat, value } => pat_uses(pat, k) || expr_uses(value, k),
        LogicalAssign { target, value, .. } => expr_uses(target, k) || expr_uses(value, k),
        // class bodies are their own scopes for yield/await purposes
        ClassExpr { .. } => false,
        Seq(list) => list.iter().any(|x| expr_uses(x, k)),
        Template(segs) => segs.iter().any(|s| match s {
            TplSeg::Lit(_) => false,
            TplSeg::Expr(x) => expr_uses(x, k),
        }),
        TaggedTemplate { tag, segs } => {
            expr_uses(tag, k)
                || segs.iter().any(|s| match s {
                    TplSeg::Lit(_) => false,
                    TplSeg::Expr(x) => expr_uses(x, k),
                })
        }
        // literals / nested functions own their trees
        Ident(_) | Num(_) | BigInt(_) | Str(_) | Bool(_) | Null | This | Regex { .. } | Func(_)
        | Arrow(_) => false,
        // `new.target` is a plain meta-property
        NewTarget => false,
        PrivateIn { obj, .. } => expr_uses(obj, k),
    }
}

/// does this stmt tree contain a yield/await anywhere (excluding nested
/// function bodies, which carry their own) — used for honest SyntaxErrors
fn stmts_use(stmts: &[Stmt], k: SusKind) -> bool {
    stmts.iter().any(|s| stmt_uses(s, k))
}

fn stmt_uses(s: &Stmt, k: SusKind) -> bool {
    use StmtKind::*;
    match &s.kind {
        Expr(e) => expr_uses(e, k),
        VarDecl(_, ds) => ds.iter().any(|d| {
            d.init.as_ref().map_or(false, |x| expr_uses(x, k))
                || d.pat.as_ref().map_or(false, |p| pat_uses(p, k))
        }),
        Block(b) | List(b) => stmts_use(b, k),
        Import { .. } => false,
        Export { inner, .. } => inner.as_deref().map_or(false, |i| stmt_uses(i, k)),
        If { test, then, other } => {
            expr_uses(test, k)
                || stmt_uses(then, k)
                || other.as_deref().map_or(false, |o| stmt_uses(o, k))
        }
        While { test, body } => expr_uses(test, k) || stmt_uses(body, k),
        DoWhile { body, test } => stmt_uses(body, k) || expr_uses(test, k),
        For { init, cond, upd, body } => {
            init.as_deref().map_or(false, |i| stmt_uses(i, k))
                || cond.as_ref().map_or(false, |c| expr_uses(c, k))
                || upd.as_ref().map_or(false, |u| expr_uses(u, k))
                || stmt_uses(body, k)
        }
        ForIn { obj, body, .. } => expr_uses(obj, k) || stmt_uses(body, k),
        ForOf { iter, body, .. } => expr_uses(iter, k) || stmt_uses(body, k),
        Switch { disc, cases } => {
            expr_uses(disc, k)
                || cases.iter().any(|c| {
                    c.test.as_ref().map_or(false, |t| expr_uses(t, k))
                        || stmts_use(&c.body, k)
                })
        }
        Try {
            block,
            catch,
            finally,
        } => {
            stmts_use(block, k)
                || catch.as_ref().map_or(false, |c| stmts_use(&c.block, k))
                || finally.as_ref().map_or(false, |f| stmts_use(f, k))
        }
        Throw(e) => expr_uses(e, k),
        Return(o) => o.as_ref().map_or(false, |e| expr_uses(e, k)),
        Break(_) | Continue(_) => false,
        Label { body, .. } => stmt_uses(body, k),
        With { obj, body } => expr_uses(obj, k) || stmt_uses(body, k),
        FuncDecl(_) => false, // nested function bodies own their yield/await
        PatAssign { pat, value } => pat_uses(pat, k) || expr_uses(value, k),
        DefineAcc { owner, getter, setter, .. } => {
            expr_uses(owner, k)
                || getter.as_ref().map_or(false, |f| fnbody_uses(&f.body, k))
                || setter.as_ref().map_or(false, |f| fnbody_uses(&f.body, k))
        }
        DefineMethod { owner, func, .. } => {
            expr_uses(owner, k) || fnbody_uses(&func.body, k)
        }
    }
}

fn fnbody_uses(b: &FnBody, k: SusKind) -> bool {
    match b {
        FnBody::Block(s) => stmts_use(s, k),
        FnBody::Expr(e) => expr_uses(e, k),
        FnBody::Bound { .. } => false,
    }
}

fn pat_uses(p: &Pat, k: SusKind) -> bool {
    match p {
        Pat::Name(_) => false,
        Pat::Arr(parts) => parts.iter().any(|pp| pat_part_uses(pp, k)),
        Pat::Obj(parts) => parts.iter().any(|op| match op {
            ObjPatPart::Prop { part, .. } => pat_part_uses(part, k),
            ObjPatPart::ComputedProp { key, part } => {
                expr_uses(key, k) || pat_part_uses(part, k)
            }
            ObjPatPart::Rest(pat) => pat_uses(pat, k),
        }),
    }
}

fn pat_part_uses(p: &PatPart, k: SusKind) -> bool {
    match p {
        PatPart::Hole => false,
        PatPart::Bind(pat, default) => {
            pat_uses(pat, k) || default.as_ref().map_or(false, |d| expr_uses(d, k))
        }
        PatPart::Rest(pat) => pat_uses(pat, k),
    }
}

fn body_has_any_yield(stmts: &[Stmt]) -> bool {
    stmts_use(stmts, SusKind::Yield) || stmts_use(stmts, SusKind::Await)
}

/// Generator-body position validation: `yield` may appear only as the whole
/// expression statement or directly as a declarator initializer. Everything
/// else in this body tree is an honest error (the resumable walker suspends
/// only at those two positions).
fn check_yield_stmts(stmts: &[Stmt]) -> Result<(), String> {
    for s in stmts {
        check_y_clean(s)?;
    }
    Ok(())
}

fn check_y_clean(s: &Stmt) -> Result<(), String> {
    use StmtKind::*;
    match &s.kind {
        Expr(e) => match &e.kind {
            ExprKind::Yield(_) | ExprKind::YieldStar(_) => Ok(()),
            _ => {
                if expr_uses(e, SusKind::Yield) {
                    Err("yield is only supported as a plain statement (`yield x;`)".into())
                } else {
                    Ok(())
                }
            }
        },
        VarDecl(_, ds) => {
            for d in ds {
                match &d.init {
                    Some(x)
                        if matches!(x.kind, ExprKind::Yield(_) | ExprKind::YieldStar(_)) => {}
                    Some(x) if expr_uses(x, SusKind::Yield) => {
                        return Err("yield is only supported as `let x = yield v`".into())
                    }
                    _ => {}
                }
                if let Some(p) = &d.pat {
                    if pat_uses(p, SusKind::Yield) {
                        return Err("yield inside a destructuring pattern is not supported".into());
                    }
                }
            }
            Ok(())
        }
        Block(b) | List(b) => check_yield_stmts(b),
        Import { .. } => Ok(()),
        Export { inner, .. } => match inner {
            Some(i) => check_y_clean(i),
            None => Ok(()),
        },
        If { test, then, other } => {
            if expr_uses(test, SusKind::Yield) {
                return Err("yield cannot appear in an if condition".into());
            }
            check_y_clean(then)?;
            if let Some(o) = other {
                check_y_clean(o)?;
            }
            Ok(())
        }
        While { test, body } | DoWhile { body, test } => {
            if expr_uses(test, SusKind::Yield) {
                return Err("yield cannot appear in a loop condition".into());
            }
            check_y_clean(body)
        }
        For { init, cond, upd, body } => {
            for x in [
                init.as_deref().map(|i| stmt_uses(i, SusKind::Yield)).unwrap_or(false),
                cond.as_ref().map_or(false, |c| expr_uses(c, SusKind::Yield)),
                upd.as_ref().map_or(false, |u| expr_uses(u, SusKind::Yield)),
            ] {
                if x {
                    return Err("yield cannot appear in a for header".into());
                }
            }
            check_y_clean(body)
        }
        ForIn { obj, body, .. } | ForOf { iter: obj, body, .. } => {
            if expr_uses(obj, SusKind::Yield) {
                return Err("yield cannot appear in a for-in/of head".into());
            }
            check_y_clean(body)
        }
        Switch { disc, cases } => {
            if expr_uses(disc, SusKind::Yield) {
                return Err("yield cannot appear in a switch discriminant".into());
            }
            for c in cases {
                if let Some(t) = &c.test {
                    if expr_uses(t, SusKind::Yield) {
                        return Err("yield cannot appear in a case test".into());
                    }
                }
                check_yield_stmts(&c.body)?;
            }
            Ok(())
        }
        Try { block, catch, finally } => {
            // resumable from 19.2: the walker keeps a try frame on its stack,
            // so a yield may appear in any of the three blocks
            check_yield_stmts(block)?;
            if let Some(c) = catch {
                check_yield_stmts(&c.block)?;
            }
            if let Some(f) = finally {
                check_yield_stmts(f)?;
            }
            Ok(())
        }
        Throw(e) => {
            if expr_uses(e, SusKind::Yield) {
                return Err("yield cannot appear inside throw".into());
            }
            Ok(())
        }
        Return(o) => {
            if o.as_ref().map_or(false, |e| expr_uses(e, SusKind::Yield)) {
                return Err("return yield is not supported (assign then return)".into());
            }
            Ok(())
        }
        Break(_) | Continue(_) | FuncDecl(_) | DefineAcc { .. } | DefineMethod { .. } => Ok(()),
        Label { body, .. } => check_y_clean(body),
        With { obj, body } => {
            if expr_uses(obj, SusKind::Yield) {
                return Err("yield cannot appear in a `with` head".into());
            }
            check_y_clean(body)
        }
        PatAssign { pat, value } => {
            if pat_uses(pat, SusKind::Yield) || expr_uses(value, SusKind::Yield) {
                return Err("yield cannot appear in destructuring assignment".into());
            }
            Ok(())
        }
    }
}

fn expr_has_await_or_call_yield(e: &Expr) -> bool {
    expr_uses(e, SusKind::Yield) || expr_uses(e, SusKind::Await)
}

// ---------------------------------------------------------------------------
// yield lifting: turn a yield inside an expression into statement-level temps
// ---------------------------------------------------------------------------
//
// The resumable walker suspends only at `yield v;` and `let x = yield v`, so a
// yield buried in an expression is rewritten into that shape before the walker
// ever sees it:
//
//     f(g(), yield x, h())  ->  let $t0 = g(); let $t1 = yield x;
//                               f($t0, $t1, h())
//
// Evaluation stays left to right: once a later operand can suspend, every
// earlier operand is frozen in a temp first. `&&`/`||`/`??`/`?:` lower to
// `if` statements that assign the result, never to booleans, because they
// return operand values. Anything the lift cannot rewrite (yield in a loop
// condition, a compound assignment target, `return yield`, ...) is left alone
// and the position check below reports it honestly.

/// No identifier written in source can start with NUL (the lexer rejects it),
/// so a lifted temporary can never collide with or shadow user bindings.
const LIFT_TEMP: &str = "\u{0}$y";

fn is_lift_temp(e: &Expr) -> bool {
    matches!(&e.kind, ExprKind::Ident(n) if n.starts_with(LIFT_TEMP))
}

fn lift_ident(name: Rc<str>, span: Span) -> Expr {
    Expr {
        kind: ExprKind::Ident(name),
        span,
    }
}

fn lift_let(name: Rc<str>, init: Option<Expr>, span: Span) -> Stmt {
    Stmt {
        kind: StmtKind::VarDecl(
            VarKind::Let,
            vec![Declarator {
                name,
                pat: None,
                init,
            }],
        ),
        span,
    }
}

fn lift_expr_stmt(e: Expr, span: Span) -> Stmt {
    Stmt {
        kind: StmtKind::Expr(Box::new(e)),
        span,
    }
}

fn lift_block(stmts: Vec<Stmt>, span: Span) -> Stmt {
    Stmt {
        kind: StmtKind::Block(stmts),
        span,
    }
}

/// placeholder while a node is moved out for rewriting; always replaced
fn lift_noop_expr(span: Span) -> Expr {
    Expr {
        kind: ExprKind::Null,
        span,
    }
}

fn lift_noop_stmt(span: Span) -> Stmt {
    Stmt {
        kind: StmtKind::List(Vec::new()),
        span,
    }
}

struct YieldLift {
    next: usize,
}

impl YieldLift {
    fn temp_name(&mut self) -> Rc<str> {
        let name = format!("{}{}", LIFT_TEMP, self.next);
        self.next += 1;
        Rc::from(name.as_str())
    }

    /// `let $t = value;` and answer `$t`
    fn bind(&mut self, value: Expr, pre: &mut Vec<Stmt>, span: Span) -> Expr {
        let name = self.temp_name();
        pre.push(lift_let(name.clone(), Some(value), span));
        lift_ident(name, span)
    }

    /// `let $t;` and answer `$t` (assigned by a branch)
    fn declare(&mut self, pre: &mut Vec<Stmt>, span: Span) -> Expr {
        let name = self.temp_name();
        pre.push(lift_let(name.clone(), None, span));
        lift_ident(name, span)
    }

    // -- statements ---------------------------------------------------------

    fn stmts(&mut self, stmts: &mut Vec<Stmt>) {
        let old = std::mem::take(stmts);
        let mut out = Vec::with_capacity(old.len());
        for s in old {
            self.stmt(s, &mut out);
        }
        *stmts = out;
    }

    /// Rewrite one statement, appending zero or more statements to `out`.
    fn stmt(&mut self, mut s: Stmt, out: &mut Vec<Stmt>) {
        let span = s.span;
        match &mut s.kind {
            StmtKind::Expr(e) => {
                // the two shapes the walker already suspends on stay as-is
                if matches!(e.kind, ExprKind::Yield(_) | ExprKind::YieldStar(_))
                    || !expr_uses(e, SusKind::Yield)
                {
                    out.push(s);
                    return;
                }
                let mut pre = Vec::new();
                let lowered =
                    self.expr(std::mem::replace(e, lift_noop_expr(span)), &mut pre);
                out.append(&mut pre);
                **e = lowered;
                out.push(s);
            }
            StmtKind::VarDecl(kind, ds) => {
                let needs = ds.iter().any(|d| {
                    d.init.as_ref().map_or(false, |i| {
                        !matches!(i.kind, ExprKind::Yield(_) | ExprKind::YieldStar(_))
                            && expr_uses(i, SusKind::Yield)
                    })
                });
                if !needs {
                    out.push(s);
                    return;
                }
                // split per declarator so initializer order (and any
                // suspension) stays exactly in source order
                let kind = *kind;
                let ds = std::mem::take(ds);
                for mut d in ds {
                    let mut pre = Vec::new();
                    if let Some(init) = d.init.take() {
                        if !matches!(init.kind, ExprKind::Yield(_) | ExprKind::YieldStar(_)) {
                            d.init = Some(if expr_uses(&init, SusKind::Yield) {
                                self.expr(init, &mut pre)
                            } else {
                                init
                            });
                        } else {
                            d.init = Some(init);
                        }
                    }
                    out.append(&mut pre);
                    out.push(Stmt {
                        kind: StmtKind::VarDecl(kind, vec![d]),
                        span,
                    });
                }
            }
            StmtKind::Block(b) | StmtKind::List(b) => {
                self.stmts(b);
                out.push(s);
            }
            StmtKind::If { test, then, other } => {
                // the test runs exactly once, so a yield inside it lifts to
                // statements before the `if` (loop conditions stay a gap)
                if expr_uses(test, SusKind::Yield) {
                    let mut pre = Vec::new();
                    let lowered =
                        self.expr(std::mem::replace(&mut **test, lift_noop_expr(span)), &mut pre);
                    out.append(&mut pre);
                    **test = lowered;
                }
                self.body(then);
                if let Some(o) = other {
                    self.body(o);
                }
                out.push(s);
            }
            StmtKind::While { body, .. }
            | StmtKind::DoWhile { body, .. }
            | StmtKind::For { body, .. }
            | StmtKind::ForIn { body, .. }
            | StmtKind::ForOf { body, .. } => {
                self.body(body);
                out.push(s);
            }
            StmtKind::Switch { cases, .. } => {
                for c in cases {
                    self.stmts(&mut c.body);
                }
                out.push(s);
            }
            StmtKind::Try {
                block,
                catch,
                finally,
            } => {
                self.stmts(block);
                if let Some(c) = catch {
                    self.stmts(&mut c.block);
                }
                if let Some(f) = finally {
                    self.stmts(f);
                }
                out.push(s);
            }
            StmtKind::Label { body, .. } => {
                self.body(body);
                out.push(s);
            }
            _ => out.push(s),
        }
    }

    /// A nested body is one statement, so its lifted pre-statements are folded
    /// into a block in place. `continue label` is unaffected: a labeled loop
    /// keeps its label (only its body is rewritten).
    fn body(&mut self, body: &mut Stmt) {
        let span = body.span;
        let old = std::mem::replace(body, lift_noop_stmt(span));
        let mut seq = Vec::new();
        self.stmt(old, &mut seq);
        *body = if seq.len() == 1 {
            seq.pop().unwrap()
        } else {
            lift_block(seq, span)
        };
    }

    // -- expressions --------------------------------------------------------

    fn expr(&mut self, x: Expr, pre: &mut Vec<Stmt>) -> Expr {
        if !expr_uses(&x, SusKind::Yield) {
            return x;
        }
        let span = x.span;
        match x.kind {
            ExprKind::Yield(op) => {
                let op = op.map(|o| Box::new(self.expr(*o, pre)));
                let y = Expr {
                    kind: ExprKind::Yield(op),
                    span,
                };
                self.bind(y, pre, span)
            }
            ExprKind::YieldStar(x) => {
                let x = Box::new(self.expr(*x, pre));
                let y = Expr {
                    kind: ExprKind::YieldStar(x),
                    span,
                };
                self.bind(y, pre, span)
            }
            ExprKind::Binary { op, l, r } => {
                let l = self.expr(*l, pre);
                let l = if expr_uses(&r, SusKind::Yield) && !is_lift_temp(&l) {
                    self.bind(l, pre, span)
                } else {
                    l
                };
                let r = self.expr(*r, pre);
                Expr {
                    kind: ExprKind::Binary {
                        op,
                        l: Box::new(l),
                        r: Box::new(r),
                    },
                    span,
                }
            }
            ExprKind::Logical { op, l, r } => self.logical(op, *l, *r, pre, span),
            ExprKind::Cond { test, then, other } => self.cond(*test, *then, *other, pre, span),
            ExprKind::Call { callee, args } => {
                if !expr_uses(&callee, SusKind::Yield)
                    && args.iter().any(|a| match a {
                        CallArg::Arg(e) | CallArg::Spread(e) => expr_uses(e, SusKind::Yield),
                    })
                    && matches!(
                        &callee.kind,
                        ExprKind::SuperMember { .. }
                            | ExprKind::SuperCall { .. }
                            | ExprKind::OptionalChain { .. }
                    )
                {
                    // binding these to a temp would lose `this`; leave the
                    // call intact so the position check reports it
                    return Expr {
                        kind: ExprKind::Call { callee, args },
                        span,
                    };
                }
                let callee = if expr_uses(&callee, SusKind::Yield) {
                    Box::new(self.expr(*callee, pre))
                } else if args.iter().any(|a| match a {
                    CallArg::Arg(e) | CallArg::Spread(e) => expr_uses(e, SusKind::Yield),
                }) {
                    Box::new(self.hold(*callee, pre, span))
                } else {
                    callee
                };
                let args = self.args(args, pre);
                Expr {
                    kind: ExprKind::Call { callee, args },
                    span,
                }
            }
            ExprKind::New { callee, args } => {
                let callee = self.expr(*callee, pre);
                let callee = if args.iter().any(|a| match a {
                    CallArg::Arg(e) | CallArg::Spread(e) => expr_uses(e, SusKind::Yield),
                }) && !is_lift_temp(&callee)
                {
                    self.bind(callee, pre, span)
                } else {
                    callee
                };
                let args = self.args(args, pre);
                Expr {
                    kind: ExprKind::New {
                        callee: Box::new(callee),
                        args,
                    },
                    span,
                }
            }
            ExprKind::Member { base, key } => {
                let base = self.expr(*base, pre);
                Expr {
                    kind: ExprKind::Member {
                        base: Box::new(base),
                        key,
                    },
                    span,
                }
            }
            ExprKind::Index { base, index } => {
                let base = self.expr(*base, pre);
                let base = if expr_uses(&index, SusKind::Yield) && !is_lift_temp(&base) {
                    self.bind(base, pre, span)
                } else {
                    base
                };
                let index = self.expr(*index, pre);
                Expr {
                    kind: ExprKind::Index {
                        base: Box::new(base),
                        index: Box::new(index),
                    },
                    span,
                }
            }
            ExprKind::Unary { op, expr } => Expr {
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(self.expr(*expr, pre)),
                },
                span,
            },
            ExprKind::Typeof(e) => Expr {
                kind: ExprKind::Typeof(Box::new(self.expr(*e, pre))),
                span,
            },
            ExprKind::Void(e) => Expr {
                kind: ExprKind::Void(Box::new(self.expr(*e, pre))),
                span,
            },
            ExprKind::Delete(e) => Expr {
                kind: ExprKind::Delete(Box::new(self.expr(*e, pre))),
                span,
            },
            ExprKind::Await(e) => Expr {
                kind: ExprKind::Await(Box::new(self.expr(*e, pre))),
                span,
            },
            ExprKind::Assign {
                target,
                op: None,
                value,
            } => self.assign(*target, *value, pre, span),
            ExprKind::Seq(list) => {
                let last = list.len().saturating_sub(1);
                for (i, e) in list.into_iter().enumerate() {
                    let e = self.expr(e, pre);
                    if i == last {
                        return e;
                    }
                    pre.push(lift_expr_stmt(e, span));
                }
                lift_noop_expr(span)
            }
            ExprKind::Template(segs) => Expr {
                kind: ExprKind::Template(self.segs(segs, pre)),
                span,
            },
            ExprKind::TaggedTemplate { tag, segs } => {
                let tag = self.expr(*tag, pre);
                let segs_suspend = segs.iter().any(|s| match s {
                    TplSeg::Lit(_) => false,
                    TplSeg::Expr(e) => expr_uses(e, SusKind::Yield),
                });
                let tag = if segs_suspend && !is_lift_temp(&tag) {
                    self.bind(tag, pre, span)
                } else {
                    tag
                };
                Expr {
                    kind: ExprKind::TaggedTemplate {
                        tag: Box::new(tag),
                        segs: self.segs(segs, pre),
                    },
                    span,
                }
            }
            ExprKind::Array(items) => Expr {
                kind: ExprKind::Array(self.array(items, pre)),
                span,
            },
            ExprKind::Object(props) => Expr {
                kind: ExprKind::Object(self.object(props, pre)),
                span,
            },
            ExprKind::SuperCall {
                class,
                key,
                static_,
                args,
            } => Expr {
                kind: ExprKind::SuperCall {
                    class: Box::new(self.expr(*class, pre)),
                    key,
                    static_,
                    args: self.args(args, pre),
                },
                span,
            },
            ExprKind::SuperMember {
                class,
                key,
                static_,
            } => Expr {
                kind: ExprKind::SuperMember {
                    class: Box::new(self.expr(*class, pre)),
                    key,
                    static_,
                },
                span,
            },
            // positions the lift does not rewrite: the caller's position
            // check turns any remaining yield into an honest SyntaxError
            other => Expr { kind: other, span },
        }
    }

    /// Freeze a call target without losing its reference (`obj.m(...)` must
    /// still see `this === obj`): the base is held, the member is rebuilt.
    fn hold(&mut self, e: Expr, pre: &mut Vec<Stmt>, span: Span) -> Expr {
        match e.kind {
            ExprKind::Member { base, key } => {
                let base = self.hold_value(*base, pre, span);
                Expr {
                    kind: ExprKind::Member {
                        base: Box::new(base),
                        key,
                    },
                    span,
                }
            }
            ExprKind::Index { base, index } => {
                let base = self.hold_value(*base, pre, span);
                let index = self.hold_value(*index, pre, span);
                Expr {
                    kind: ExprKind::Index {
                        base: Box::new(base),
                        index: Box::new(index),
                    },
                    span,
                }
            }
            other => self.bind(Expr { kind: other, span }, pre, span),
        }
    }

    /// `this` and our own temps cannot change while later operands run.
    fn hold_value(&mut self, e: Expr, pre: &mut Vec<Stmt>, span: Span) -> Expr {
        match e.kind {
            ExprKind::This => Expr {
                kind: ExprKind::This,
                span,
            },
            ExprKind::Ident(n) if n.starts_with(LIFT_TEMP) => Expr {
                kind: ExprKind::Ident(n),
                span,
            },
            other => self.bind(Expr { kind: other, span }, pre, span),
        }
    }

    fn logical(
        &mut self,
        op: LogOp,
        l: Expr,
        r: Expr,
        pre: &mut Vec<Stmt>,
        span: Span,
    ) -> Expr {
        let l = self.expr(l, pre);
        let l = if is_lift_temp(&l) {
            l
        } else {
            self.bind(l, pre, span)
        };
        let out = self.temp_name();
        pre.push(lift_let(out.clone(), Some(l.clone()), span));
        let mut body = Vec::new();
        let r = self.expr(r, &mut body);
        body.push(lift_expr_stmt(
            Expr {
                kind: ExprKind::Assign {
                    target: Box::new(lift_ident(out.clone(), span)),
                    op: None,
                    value: Box::new(r),
                },
                span,
            },
            span,
        ));
        let test = match op {
            LogOp::And => l,
            LogOp::Or => Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(l),
                },
                span,
            },
            LogOp::Coalesce => Expr {
                kind: ExprKind::Binary {
                    op: BinOp::Eq,
                    l: Box::new(l),
                    r: Box::new(Expr {
                        kind: ExprKind::Null,
                        span,
                    }),
                },
                span,
            },
        };
        pre.push(Stmt {
            kind: StmtKind::If {
                test: Box::new(test),
                then: Box::new(lift_block(body, span)),
                other: None,
            },
            span,
        });
        lift_ident(out, span)
    }

    fn cond(
        &mut self,
        test: Expr,
        then: Expr,
        other: Expr,
        pre: &mut Vec<Stmt>,
        span: Span,
    ) -> Expr {
        let test = self.expr(test, pre);
        let test = if is_lift_temp(&test) {
            test
        } else {
            self.bind(test, pre, span)
        };
        let out = self.declare(pre, span);
        let mut then_body = Vec::new();
        let then = self.expr(then, &mut then_body);
        then_body.push(lift_expr_stmt(
            Expr {
                kind: ExprKind::Assign {
                    target: Box::new(out.clone()),
                    op: None,
                    value: Box::new(then),
                },
                span,
            },
            span,
        ));
        let mut other_body = Vec::new();
        let other = self.expr(other, &mut other_body);
        other_body.push(lift_expr_stmt(
            Expr {
                kind: ExprKind::Assign {
                    target: Box::new(out.clone()),
                    op: None,
                    value: Box::new(other),
                },
                span,
            },
            span,
        ));
        pre.push(Stmt {
            kind: StmtKind::If {
                test: Box::new(test),
                then: Box::new(lift_block(then_body, span)),
                other: Some(Box::new(lift_block(other_body, span))),
            },
            span,
        });
        out
    }

    fn assign(&mut self, target: Expr, value: Expr, pre: &mut Vec<Stmt>, span: Span) -> Expr {
        if expr_uses(&target, SusKind::Yield) {
            // e.g. `(yield o).x = v`: leave it for the position check
            return Expr {
                kind: ExprKind::Assign {
                    target: Box::new(target),
                    op: None,
                    value: Box::new(value),
                },
                span,
            };
        }
        let target = match target.kind {
            ExprKind::Ident(n) => lift_ident(n, span),
            ExprKind::This => Expr {
                kind: ExprKind::This,
                span,
            },
            ExprKind::Member { base, key } => Expr {
                kind: ExprKind::Member {
                    base: Box::new(self.hold_value(*base, pre, span)),
                    key,
                },
                span,
            },
            ExprKind::Index { base, index } => Expr {
                kind: ExprKind::Index {
                    base: Box::new(self.hold_value(*base, pre, span)),
                    index: Box::new(self.hold_value(*index, pre, span)),
                },
                span,
            },
            other => {
                return Expr {
                    kind: ExprKind::Assign {
                        target: Box::new(Expr { kind: other, span }),
                        op: None,
                        value: Box::new(value),
                    },
                    span,
                }
            }
        };
        let value = self.expr(value, pre);
        Expr {
            kind: ExprKind::Assign {
                target: Box::new(target),
                op: None,
                value: Box::new(value),
            },
            span,
        }
    }

    fn args(&mut self, args: Vec<CallArg>, pre: &mut Vec<Stmt>) -> Vec<CallArg> {
        let last = match args
            .iter()
            .rposition(|a| match a {
                CallArg::Arg(e) | CallArg::Spread(e) => expr_uses(e, SusKind::Yield),
            }) {
            Some(i) => i,
            None => return args,
        };
        let mut out = Vec::with_capacity(args.len());
        for (i, a) in args.into_iter().enumerate() {
            match a {
                CallArg::Arg(e) => {
                    let span = e.span;
                    let e = self.expr(e, pre);
                    let e = if i < last && !is_lift_temp(&e) {
                        self.bind(e, pre, span)
                    } else {
                        e
                    };
                    out.push(CallArg::Arg(e));
                }
                CallArg::Spread(e) => {
                    let span = e.span;
                    let e = self.expr(e, pre);
                    let e = if i < last && !is_lift_temp(&e) {
                        self.bind(e, pre, span)
                    } else {
                        e
                    };
                    out.push(CallArg::Spread(e));
                }
            }
        }
        out
    }

    fn segs(&mut self, segs: Vec<TplSeg>, pre: &mut Vec<Stmt>) -> Vec<TplSeg> {
        let last = match segs.iter().rposition(|s| match s {
            TplSeg::Lit(_) => false,
            TplSeg::Expr(e) => expr_uses(e, SusKind::Yield),
        }) {
            Some(i) => i,
            None => return segs,
        };
        segs.into_iter()
            .enumerate()
            .map(|(i, s)| match s {
                TplSeg::Lit(t) => TplSeg::Lit(t),
                TplSeg::Expr(e) => {
                    let span = e.span;
                    let e = self.expr(e, pre);
                    let e = if i < last && !is_lift_temp(&e) {
                        self.bind(e, pre, span)
                    } else {
                        e
                    };
                    TplSeg::Expr(e)
                }
            })
            .collect()
    }

    fn array(&mut self, items: Vec<ArrayElem>, pre: &mut Vec<Stmt>) -> Vec<ArrayElem> {
        let last = match items.iter().rposition(|it| match it {
            ArrayElem::Item(e) | ArrayElem::Spread(e, _) => expr_uses(e, SusKind::Yield),
            ArrayElem::Hole => false,
        }) {
            Some(i) => i,
            None => return items,
        };
        items
            .into_iter()
            .enumerate()
            .map(|(i, it)| match it {
                ArrayElem::Hole => ArrayElem::Hole,
                ArrayElem::Item(e) => {
                    let span = e.span;
                    let e = self.expr(e, pre);
                    let e = if i < last && !is_lift_temp(&e) {
                        self.bind(e, pre, span)
                    } else {
                        e
                    };
                    ArrayElem::Item(e)
                }
                ArrayElem::Spread(e, trailing) => {
                    let span = e.span;
                    let e = self.expr(e, pre);
                    let e = if i < last && !is_lift_temp(&e) {
                        self.bind(e, pre, span)
                    } else {
                        e
                    };
                    ArrayElem::Spread(e, trailing)
                }
            })
            .collect()
    }

    fn object(&mut self, props: Vec<ObjElem>, pre: &mut Vec<Stmt>) -> Vec<ObjElem> {
        let suspends = |p: &ObjElem| match p {
            ObjElem::Prop(_, v) | ObjElem::Spread(v, _) => expr_uses(v, SusKind::Yield),
            ObjElem::CoverInit(_, v) => expr_uses(v, SusKind::Yield),
            ObjElem::Computed { key, value } => {
                expr_uses(key, SusKind::Yield) || expr_uses(value, SusKind::Yield)
            }
            ObjElem::ComputedAccessor { key, .. } => expr_uses(key, SusKind::Yield),
            ObjElem::Accessor { .. } => false,
        };
        let last = match props.iter().rposition(suspends) {
            Some(i) => i,
            None => return props,
        };
        props
            .into_iter()
            .enumerate()
            .map(|(i, p)| match p {
                ObjElem::Prop(key, v) => {
                    let span = v.span;
                    let v = self.expr(v, pre);
                    let v = if i < last && !is_lift_temp(&v) {
                        self.bind(v, pre, span)
                    } else {
                        v
                    };
                    ObjElem::Prop(key, v)
                }
                ObjElem::CoverInit(key, v) => {
                    let span = v.span;
                    let v = self.expr(v, pre);
                    let v = if i < last && !is_lift_temp(&v) {
                        self.bind(v, pre, span)
                    } else {
                        v
                    };
                    ObjElem::CoverInit(key, v)
                }
                ObjElem::Spread(v, trailing) => {
                    let span = v.span;
                    let v = self.expr(v, pre);
                    let v = if i < last && !is_lift_temp(&v) {
                        self.bind(v, pre, span)
                    } else {
                        v
                    };
                    ObjElem::Spread(v, trailing)
                }
                ObjElem::Computed { key, value } => {
                    let span = key.span;
                    let key = self.expr(key, pre);
                    let key = if (expr_uses(&value, SusKind::Yield) || i < last)
                        && !is_lift_temp(&key)
                    {
                        self.bind(key, pre, span)
                    } else {
                        key
                    };
                    let value = self.expr(value, pre);
                    let value = if i < last && !is_lift_temp(&value) {
                        self.bind(value, pre, span)
                    } else {
                        value
                    };
                    ObjElem::Computed { key, value }
                }
                ObjElem::ComputedAccessor {
                    key,
                    getter,
                    setter,
                } => {
                    let span = key.span;
                    let key = self.expr(key, pre);
                    let key = if i < last && !is_lift_temp(&key) {
                        self.bind(key, pre, span)
                    } else {
                        key
                    };
                    ObjElem::ComputedAccessor {
                        key,
                        getter,
                        setter,
                    }
                }
                ObjElem::Accessor {
                    key,
                    getter,
                    setter,
                } => ObjElem::Accessor {
                    key,
                    getter,
                    setter,
                },
            })
            .collect()
    }
}

/// Does the function body reference `arguments` outside nested functions?
fn scan_uses_arguments_stmts(stmts: &[Stmt]) -> bool {
    stmts.iter().any(scan_stmt_arguments)
}

fn scan_stmt_arguments(s: &Stmt) -> bool {
    use StmtKind::*;
    match &s.kind {
        Expr(e) => scan_expr_arguments(e),
        VarDecl(_, ds) => ds.iter().any(|d| {
            d.init.as_ref().map_or(false, scan_expr_arguments)
                || d.pat
                    .as_ref()
                    .map_or(false, |p| pat_scan_args(p))
        }),
        Block(b) | List(b) => scan_uses_arguments_stmts(b),
        Import { .. } => false,
        Export { inner, .. } => inner.as_deref().map_or(false, scan_stmt_arguments),
        If { test, then, other } => {
            scan_expr_arguments(test)
                || scan_stmt_arguments(then)
                || other.as_deref().map_or(false, scan_stmt_arguments)
        }
        While { test, body } => scan_expr_arguments(test) || scan_stmt_arguments(body),
        DoWhile { body, test } => scan_stmt_arguments(body) || scan_expr_arguments(test),
        For { init, cond, upd, body } => {
            init.as_deref().map_or(false, scan_stmt_arguments)
                || cond.as_ref().map_or(false, |x| scan_expr_arguments(x))
                || upd.as_ref().map_or(false, |x| scan_expr_arguments(x))
                || scan_stmt_arguments(body)
        }
        ForIn { obj, body, .. } => scan_expr_arguments(obj) || scan_stmt_arguments(body),
        ForOf { iter, body, .. } => scan_expr_arguments(iter) || scan_stmt_arguments(body),
        Switch { disc, cases } => {
            scan_expr_arguments(disc)
                || cases
                    .iter()
                    .any(|c| c.test.as_ref().map_or(false, scan_expr_arguments) || scan_uses_arguments_stmts(&c.body))
        }
        Try {
            block,
            catch,
            finally,
        } => {
            scan_uses_arguments_stmts(block)
                || catch.as_ref().map_or(false, |c| scan_uses_arguments_stmts(&c.block))
                || finally
                    .as_ref()
                    .map_or(false, |f| scan_uses_arguments_stmts(f))
        }
        Throw(e) => scan_expr_arguments(e),
        Return(o) => o.as_ref().map_or(false, |x| scan_expr_arguments(x)),
        Break(_) | Continue(_) => false,
        Label { body, .. } => scan_stmt_arguments(body),
        With { obj, body } => scan_expr_arguments(obj) || scan_stmt_arguments(body),
        // nested function bodies have their own `arguments`; don't descend
        FuncDecl(_) => false,
        PatAssign { pat, value } => {
            pat_scan_args(pat) || scan_expr_arguments(value)
        }
        DefineAcc { owner, getter, setter, .. } => {
            scan_expr_arguments(owner)
                || getter.as_ref().map_or(false, |f| match &f.body {
                    FnBody::Block(b) => scan_uses_arguments_stmts(b),
                    FnBody::Expr(e) => scan_expr_arguments(e),
                    _ => false,
                })
                || setter.as_ref().map_or(false, |f| match &f.body {
                    FnBody::Block(b) => scan_uses_arguments_stmts(b),
                    FnBody::Expr(e) => scan_expr_arguments(e),
                    _ => false,
                })
        }
        DefineMethod { owner, func, .. } => {
            scan_expr_arguments(owner)
                || match &func.body {
                    FnBody::Block(b) => scan_uses_arguments_stmts(b),
                    FnBody::Expr(e) => scan_expr_arguments(e),
                    _ => false,
                }
        }
    }
}

fn pat_scan_args(p: &Pat) -> bool {
    match p {
        Pat::Name(_) => false,
        // only pattern defaults can reference `arguments`
        Pat::Arr(parts) => parts.iter().any(|pp| match pp {
            PatPart::Bind(_, Some(d)) => scan_expr_arguments(d),
            _ => false,
        }),
        Pat::Obj(parts) => parts.iter().any(|op| match op {
            ObjPatPart::Prop { part, .. } => match part {
                PatPart::Bind(_, Some(d)) => scan_expr_arguments(d),
                _ => false,
            },
            ObjPatPart::ComputedProp { key, part } => {
                scan_expr_arguments(key)
                    || match part {
                        PatPart::Bind(_, Some(d)) => scan_expr_arguments(d),
                        _ => false,
                    }
            }
            ObjPatPart::Rest(_) => false,
        }),
    }
}

fn scan_expr_arguments(e: &Expr) -> bool {
    use ExprKind::*;
    match &e.kind {
        Ident(s) => s.as_ref() == "arguments",
        Num(_) | BigInt(_) | Str(_) | Bool(_) | Null | This | Regex { .. } => false,
        Unary { expr, .. } | Typeof(expr) | Void(expr) | Delete(expr) | Await(expr) => {
            scan_expr_arguments(expr)
        }
        Postfix { target, .. } => scan_expr_arguments(target),
        Binary { l, r, .. }
        | Logical { l, r, .. }
        | Assign {
            target: l,
            value: r,
            ..
        } => scan_expr_arguments(l) || scan_expr_arguments(r),
        Cond { test, then, other } => {
            scan_expr_arguments(test) || scan_expr_arguments(then) || scan_expr_arguments(other)
        }
        Call { callee, args } => {
            scan_expr_arguments(callee)
                || args.iter().any(|a| match a {
                    CallArg::Arg(x) => scan_expr_arguments(x),
                    CallArg::Spread(x) => scan_expr_arguments(x),
                })
        }
        New { callee, args } => {
            scan_expr_arguments(callee)
                || args.iter().any(|a| match a {
                    CallArg::Arg(x) => scan_expr_arguments(x),
                    CallArg::Spread(x) => scan_expr_arguments(x),
                })
        }
        Member { base, .. } => scan_expr_arguments(base),
        Index { base, index } => scan_expr_arguments(base) || scan_expr_arguments(index),
        SuperCall { class, args, .. } => {
            scan_expr_arguments(class)
                || args.iter().any(|a| match a {
                    CallArg::Arg(x) | CallArg::Spread(x) => scan_expr_arguments(x),
                })
        }
        SuperMember { class, .. } => scan_expr_arguments(class),
        OptionalChain { base, steps } => {
            scan_expr_arguments(base)
                || steps.iter().any(|s| match s {
                    ChainStep::Member { .. } => false,
                    ChainStep::Index { index, .. } => scan_expr_arguments(index),
                    ChainStep::Call { args, .. } => args.iter().any(|a| match a {
                        CallArg::Arg(x) | CallArg::Spread(x) => scan_expr_arguments(x),
                    }),
                    ChainStep::Tagged { segs } => segs.iter().any(|s| match s {
                        TplSeg::Lit(_) => false,
                        TplSeg::Expr(x) => scan_expr_arguments(x),
                    }),
                })
        }
        Array(items) => items.iter().any(|i| match i {
            ArrayElem::Item(x) | ArrayElem::Spread(x, _) => scan_expr_arguments(x),
            ArrayElem::Hole => false,
        }),
        Object(props) => props.iter().any(|p| match p {
            ObjElem::Prop(_, v) => scan_expr_arguments(v),
            ObjElem::CoverInit(_, v) => scan_expr_arguments(v),
            ObjElem::Spread(v, _) => scan_expr_arguments(v),
            ObjElem::Computed { key, value } => {
                scan_expr_arguments(key) || scan_expr_arguments(value)
            }
            ObjElem::ComputedAccessor { key, .. } => scan_expr_arguments(key),
            ObjElem::Accessor { .. } => false,
        }),
        Yield(Some(v)) => scan_expr_arguments(v),
        Yield(None) => false,
        YieldStar(v) => scan_expr_arguments(v),
        PatAssign { pat, value } => pat_scan_args(pat) || scan_expr_arguments(value),
        LogicalAssign { target, value, .. } => {
            scan_expr_arguments(target) || scan_expr_arguments(value)
        }
        ClassExpr { stmts, .. } => scan_uses_arguments_stmts(stmts),
        // an arrow has no `arguments` of its own, so a reference inside its
        // body belongs to the enclosing non-arrow function (which must then
        // materialize one); a plain function body is opaque
        Arrow(f) => match &f.body {
            FnBody::Block(b) => scan_uses_arguments_stmts(b),
            FnBody::Expr(e) => scan_expr_arguments(e),
            FnBody::Bound { .. } => false,
        },
        Func(_) => false,
        Seq(list) => list.iter().any(scan_expr_arguments),
        Template(segs) => segs.iter().any(|s| match s {
            TplSeg::Lit(_) => false,
            TplSeg::Expr(e) => scan_expr_arguments(e),
        }),
        TaggedTemplate { tag, segs } => {
            scan_expr_arguments(tag)
                || segs.iter().any(|s| match s {
                    TplSeg::Lit(_) => false,
                    TplSeg::Expr(e) => scan_expr_arguments(e),
                })
        }
        NewTarget => false,
        PrivateIn { obj, .. } => scan_expr_arguments(obj),
    }
}
