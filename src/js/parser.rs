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
//! destructuring, spread/rest, generators, async/await, modules, `extends`,
//! static class members, getters/setters, computed keys `[k]: v`, labeled
//! statements, exponent `**`, BigInt, regex literals.

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
    Str(Rc<str>),
    Bool(bool),
    Null,
    This,
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
    /// elements: Hole = hole, Spread = `...expr`
    Array(Vec<ArrayElem>),
    Object(Vec<ObjElem>),
    /// regex literal `/pat/flags`, compiled once per (pattern, flags) and
    /// cached on the engine â€” not per evaluation
    Regex {
        pattern: Rc<str>,
        flags: Rc<str>,
    },
    /// `yield` â€” legal only where the generator walker supports it (plain
    /// expression statement or a declarator initializer)
    Yield(Option<Box<Expr>>),
    /// `await expr` â€” only inside async functions
    Await(Box<Expr>),
    Func(Rc<FnDef>),
    Arrow(Rc<FnDef>),
    /// comma operator / expression lists
    Seq(Vec<Expr>),
    Template(Vec<TplSeg>),
    Typeof(Box<Expr>),
    Void(Box<Expr>),
    Delete(Box<Expr>),
    /// destructuring assignment `([a, b] = pair)` â€” an expression-level
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
    Spread(Expr),
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
    Spread(Expr),
}

// ---------------------------------------------------------------------------
// Destructuring patterns â€” desugared at eval time into binds, not into temps
// ---------------------------------------------------------------------------

/// Where a pattern leaf binds a value: a fresh name (declaration forms) or an
/// existing lvalue (assignment destructuring).
#[derive(Clone, Debug)]
pub enum BindTgt {
    Name(Rc<str>),
    /// a.b â€” base/key evaluate in the scope where the bind happens
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
    /// `[ ,, b]` â€” skip an element
    Hole,
    Bind(Box<Pat>, Option<Expr>),
    /// `...rest` â€” must bind a Name/Member/Index leaf (honest restriction)
    Rest(Box<Pat>),
}

#[derive(Clone, Debug)]
pub enum ObjPatPart {
    Prop {
        key: Rc<str>,
        part: PatPart,
    },
    /// `{...rest}` â€” remaining own properties
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
        test: Expr,
        then: Box<Stmt>,
        other: Option<Box<Stmt>>,
    },
    While {
        test: Expr,
        body: Box<Stmt>,
    },
    DoWhile {
        body: Box<Stmt>,
        test: Expr,
    },
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        upd: Option<Expr>,
        body: Box<Stmt>,
    },
    ForIn {
        kind: Option<VarKind>,
        target: ForTarget,
        obj: Expr,
        body: Box<Stmt>,
    },
    ForOf {
        kind: Option<VarKind>,
        target: ForTarget,
        iter: Expr,
        body: Box<Stmt>,
    },
    Switch {
        disc: Expr,
        cases: Vec<SwitchCase>,
    },
    Try {
        block: Vec<Stmt>,
        catch: Option<CatchClause>,
        finally: Option<Vec<Stmt>>,
    },
    Throw(Expr),
    Return(Option<Expr>),
    Break,
    Continue,
    FuncDecl(Rc<FnDef>),
    /// destructuring assignment `([a, b] = pair)` â€” produced when an
    /// array/object literal turns out to be an assignment target
    PatAssign {
        pat: Pat,
        value: Box<Expr>,
    },
    /// class accessor desugar: install a getter/setter pair on an object
    /// (`Object`-style, no full property-descriptor model â€” shortcut)
    DefineAcc {
        owner: Expr,
        key: Rc<str>,
        getter: Option<Rc<FnDef>>,
        setter: Option<Rc<FnDef>>,
    },
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
    /// `async function` â€” calls return a promise; `await` legal in body
    pub is_async: bool,
    /// `function*` â€” body executes on a resumable walker (see gen.rs)
    pub is_generator: bool,
    /// parse-time rollup: does the body contain a `yield` in an evaluable
    /// position? Gates the cheap walk when no generator machinery is needed.
    pub has_yield: bool,
}

#[derive(Clone, Debug)]
pub struct Param {
    /// hidden env name for the received argument; for destructuring params it
    /// is a generated non-lexable `#k` name (cannot collide with source idents)
    pub name: Rc<str>,
    /// `Some` when written `{a, b}` / `[x, y]`; the value is destructured
    /// after the param env is built. `rest` params bind remaining args.
    pub pat: Option<Pat>,
    pub default: Option<Expr>,
    pub rest: bool,
}

#[derive(Clone, Debug)]
pub enum FnBody {
    Block(Vec<Stmt>),
    Expr(Expr),
    /// runtime-only, produced by `Function.prototype.bind` â€” never parsed
    Bound {
        target: super::Val,
        this: super::Val,
        args: Vec<super::Val>,
    },
}

pub type Program = Vec<Stmt>;

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub fn parse(source: &str) -> Result<Program, JsError> {
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
        toks,
        pos: 0,
        depth: 0,
    };
    p.program()
}

/// byte offset -> 1-based (line, col); col counted in chars
fn line_col(source: &str, offset: usize) -> (u32, u32) {
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
    toks: Vec<Token>,
    pos: usize,
    /// recursion guard: pathological nesting (`((((...))))`, `[[[[...]]]]`,
    /// `{{{{...}}}}`) must be a SyntaxError, not a native stack overflow
    depth: usize,
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

    fn expect_punct(&mut self, p: Punct) -> Result<(), JsError> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            self.err(format!("expected `{:?}`, found `{:?}`", p, self.peek()))
        }
    }

    fn expect_ident(&mut self) -> Result<Rc<str>, JsError> {
        match self.peek().clone() {
            Tok::Ident(s) => {
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
        let mut out = Vec::new();
        while *self.peek() != Tok::Eof {
            out.push(self.statement()?);
        }
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
                StmtKind::VarDecl(VarKind::Var, self.declarators()?)
            }
            Tok::Kw(Kw::Let) => {
                self.next();
                StmtKind::VarDecl(VarKind::Let, self.declarators()?)
            }
            Tok::Kw(Kw::Const) => {
                self.next();
                StmtKind::VarDecl(VarKind::Const, self.declarators()?)
            }
            Tok::Kw(Kw::Function) => {
                self.next();
                let is_gen = matches!(self.peek(), Tok::Op(Op::Mul));
                if is_gen {
                    self.next(); // `*`
                }
                let f = self.function_rest(true, false, is_gen)?;
                StmtKind::FuncDecl(f)
            }
            Tok::Kw(Kw::Async) => {
                if *self.peek_at(1) == Tok::Kw(Kw::Function) {
                    self.next(); // async
                    self.next(); // function
                    let f = self.function_rest(true, true, false)?;
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
                StmtKind::If { test, then, other }
            }
            Tok::Kw(Kw::While) => {
                self.next();
                self.expect_punct(Punct::LParen)?;
                let test = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.statement()?);
                StmtKind::While { test, body }
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
                StmtKind::DoWhile { body, test }
            }
            Tok::Kw(Kw::For) => {
                self.next();
                self.expect_punct(Punct::LParen)?;
                let f = self.for_rest()?;
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.statement()?);
                match f {
                    ForHead::Classic { init, cond, upd } => StmtKind::For {
                        init,
                        cond,
                        upd,
                        body,
                    },
                    ForHead::In { kind, target, obj } => StmtKind::ForIn { kind, target, obj, body },
                    ForHead::Of { kind, target, iter } => StmtKind::ForOf { kind, target, iter, body },
                }
            }
            Tok::Kw(Kw::Switch) => {
                self.next();
                self.expect_punct(Punct::LParen)?;
                let disc = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                self.expect_punct(Punct::LBrace)?;
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
                StmtKind::Switch { disc, cases }
            }
            Tok::Kw(Kw::Try) => {
                self.next();
                self.expect_punct(Punct::LBrace)?;
                let block = self.block_body()?;
                let mut catch = None;
                let mut finally = None;
                if self.eat_kw(Kw::Catch) {
                    self.expect_punct(Punct::LParen)?;
                    let name = self.expect_ident()?;
                    self.expect_punct(Punct::RParen)?;
                    self.expect_punct(Punct::LBrace)?;
                    let cbody = self.block_body()?;
                    catch = Some(CatchClause { name, block: cbody });
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
                StmtKind::Throw(e)
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
                StmtKind::Return(v)
            }
            Tok::Kw(Kw::Break) => {
                self.next();
                self.finish_stmt()?;
                StmtKind::Break
            }
            Tok::Kw(Kw::Continue) => {
                self.next();
                self.finish_stmt()?;
                StmtKind::Continue
            }
            Tok::Kw(Kw::Class) => {
                self.next();
                let list = self.class_rest()?;
                return Ok(Stmt {
                    kind: StmtKind::List(list),
                    span: self.span_from(start),
                });
            }
            Tok::Kw(Kw::Super) | Tok::Kw(Kw::Extends) | Tok::Kw(Kw::Static) => {
                return self.err("class inheritance/static members are not supported (later wave)")
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
        let mut out = Vec::new();
        while !self.eat_punct(Punct::RBrace) {
            if *self.peek() == Tok::Eof {
                return self.err("unterminated block (missing `}`)");
            }
            out.push(self.statement()?);
        }
        Ok(out)
    }

    fn declarators(&mut self) -> Result<Vec<Declarator>, JsError> {
        let mut out = Vec::new();
        loop {
            let (name, pat) = self.pattern_or_ident()?;
            let init = if self.eat_op(Op::Assign) {
                Some(self.assign_expr()?)
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
                while !self.eat_punct(Punct::RBrack) {
                    if !parts.is_empty() {
                        self.expect_punct(Punct::Comma)?;
                    }
                    let trailing = matches!(
                        self.peek(),
                        Tok::Punct(Punct::RBrack) | Tok::Punct(Punct::Comma)
                    );
                    if trailing {
                        parts.push(PatPart::Hole); // leading/trailing hole
                        continue;
                    }
                    if *self.peek() == Tok::Spread {
                        self.next();
                        let t = self.parse_binding_leaf(decl_form)?;
                        if !matches!(t, Pat::Name(_)) {
                            return self.err("array rest must bind a simple name");
                        }
                        parts.push(PatPart::Rest(Box::new(t)));
                        continue;
                    }
                    parts.push(self.pat_part(decl_form)?);
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
                        break; // trailing comma
                    }
                    if *self.peek() == Tok::Spread {
                        self.next();
                        let t = self.parse_binding_leaf(decl_form)?;
                        if !matches!(t, Pat::Name(_)) {
                            return self.err("object rest must bind a simple name");
                        }
                        parts.push(ObjPatPart::Rest(t));
                        continue;
                    }
                    let key = self.prop_name()?;
                    let part = if self.eat_punct(Punct::Colon) {
                        self.pat_part(decl_form)?
                    } else {
                        // shorthand: `{a}` / `{a = d}` binds the name `a`
                        if *self.peek() == Tok::Op(Op::Assign) {
                            self.next();
                            let d = self.assign_expr()?;
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
                    parts.push(ObjPatPart::Prop { key, part });
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
        let default = if self.eat_op(Op::Assign) {
            Some(self.assign_expr()?)
        } else {
            None
        };
        Ok(PatPart::Bind(Box::new(sub), default))
    }

    /// leaf binding target: `a`, `a.b`, `a[b]`, `o[k]`
    fn parse_binding_leaf(&mut self, decl_form: bool) -> Result<Pat, JsError> {
        let start = self.pos;
        let mut e = match self.peek().clone() {
            Tok::Ident(s) => {
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
            // declaration form: name-or-pattern, then `in`/`of`/`;`
            let target = self.pattern_or_ident()?;
            let target = match &target.1 {
                Some(p) => ForTarget::Pat(p.clone()),
                None => ForTarget::Name(target.0),
            };
            if self.eat_op(Op::Assign) {
                // classic for with initializer: `for (var i = 0; ...)`
                let init_expr = self.assign_expr()?;
                let mut decls = vec![Declarator {
                    name: match &target {
                        ForTarget::Name(n) => n.clone(),
                        ForTarget::Pat(_) => Rc::from(""),
                    },
                    pat: match target {
                        ForTarget::Pat(p) => Some(p),
                        _ => None,
                    },
                    init: Some(init_expr),
                }];
                while self.eat_punct(Punct::Comma) {
                    let (name, pat) = self.pattern_or_ident()?;
                    let init = if self.eat_op(Op::Assign) {
                        Some(self.assign_expr()?)
                    } else {
                        None
                    };
                    decls.push(Declarator { name, pat, init });
                }
                let init = Some(Box::new(Stmt {
                    kind: StmtKind::VarDecl(kind.unwrap_or(VarKind::Var), decls),
                    span: self.span_from(self.pos),
                }));
                return self.for_classic_tail(init);
            }
            if self.eat_kw(Kw::In) {
                let obj = self.expr()?;
                return Ok(ForHead::In { kind, target, obj });
            }
            if self.eat_kw(Kw::Of) {
                let iter = self.assign_expr()?; // no comma in for-of head
                return Ok(ForHead::Of { kind, target, iter });
            }
            // `for (let i; ...)` â€” declaration without initializer
            let init = Some(Box::new(Stmt {
                kind: StmtKind::VarDecl(
                    kind.unwrap_or(VarKind::Var),
                    vec![Declarator {
                        name: match &target {
                            ForTarget::Name(n) => n.clone(),
                            ForTarget::Pat(_) => Rc::from(""),
                        },
                        pat: match target {
                            ForTarget::Pat(p) => Some(p),
                            _ => None,
                        },
                        init: None,
                    }],
                ),
                span: self.span_from(self.pos),
            }));
            return self.for_classic_tail(init);
        }
        // expression forms
        if *self.peek() == Tok::Punct(Punct::Semi) {
            // empty init: `for (; cond; upd)` / `for (;;)` â€” for_classic_tail
            // consumes the init-terminating `;` itself
            return self.for_classic_tail(None);
        }
        let e = self.expr()?;
        if self.eat_kw(Kw::In) {
            let name = match e.kind {
                ExprKind::Ident(s) => s,
                _ => return self.err("invalid for-in target"),
            };
            let obj = self.expr()?;
            return Ok(ForHead::In {
                kind: None,
                target: ForTarget::Name(name),
                obj,
            });
        }
        if self.eat_kw(Kw::Of) {
            // assignment-destructuring form: `for ([a, b] of pairs)`
            let target = if matches!(e.kind, ExprKind::Array(_)) {
                ForTarget::Pat(expr_to_pat(&e).map_err(|m| {
                    let t = self.cur();
                    let (line, col) = line_col(self.src, t.start as usize);
                    JsError::Syntax { msg: m, line, col }
                })?)
            } else {
                match e.kind {
                    ExprKind::Ident(s) => ForTarget::Name(s),
                    _ => return self.err("invalid for-of target"),
                }
            };
            let iter = self.assign_expr()?;
            return Ok(ForHead::Of { kind: None, target, iter });
        }
        let init = Some(Box::new(Stmt {
            kind: StmtKind::Expr(Box::new(e)),
            span: self.span_from(self.pos),
        }));
        self.for_classic_tail(init)
    }

    fn for_classic_tail(&mut self, init: Option<Box<Stmt>>) -> Result<ForHead, JsError> {
        self.expect_punct(Punct::Semi)?;
        // empty condition: `for (;; ...)` â€” `;` or `)` means omitted
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
            self.next();
            s
        } else {
            Rc::from("")
        };
        let params = self.param_list()?;
        self.expect_punct(Punct::LBrace)?;
        let body_stmts = self.block_body()?;
        if is_generator {
            self.validate_yields(&body_stmts, is_async)?;
        } else if stmts_use(&body_stmts, SusKind::Yield) {
            return self.err("`yield` is only valid inside generator functions");
        }
        if !is_async && stmts_use(&body_stmts, SusKind::Await) {
            return self.err("`await` is only valid inside async functions");
        }
        let uses_arguments = scan_uses_arguments_stmts(&body_stmts);
        Ok(Rc::new(FnDef {
            name,
            params,
            body: FnBody::Block(body_stmts),
            uses_arguments,
            is_async,
            is_generator,
            has_yield: is_generator,
        }))
    }

    /// Position check for `yield` inside a generator body: only allowed as a
    /// whole expression statement (`yield x;`) or directly as a declarator
    /// initializer (`let x = yield v`). Anywhere else (conditions, calls,
    /// operators, return-expr, try blocks) is an honest SyntaxError â€” the
    /// resumable walker suspends exactly at those two positions.
    fn validate_yields(&mut self, stmts: &[Stmt], is_async: bool) -> Result<(), JsError> {
        if is_async {
            return self.err("generator + async combination is not supported");
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
                    self.next();
                    (s, None)
                }
                Tok::Punct(Punct::LBrack) | Tok::Punct(Punct::LBrace) => {
                    let pat = self.parse_pattern(true)?;
                    // synthetic non-lexable env name; destructured after the
                    // param env is built
                    let hidden = Rc::from(format!("#{}", *hidden_idx));
                    *hidden_idx += 1;
                    (hidden, Some(pat))
                }
                _ => {
                    return self.err("expected parameter name");
                }
            };
            let default = if rest {
                None // rest params take no default
            } else if self.eat_op(Op::Assign) {
                Some(self.assign_expr()?)
            } else {
                None
            };
            params.push(Param { name, pat, default, rest });
        }
        Ok(params)
    }

    fn param_list(&mut self) -> Result<Vec<Param>, JsError> {
        self.expect_punct(Punct::LParen)?;
        let mut idx = 0usize;
        self.params_after_lparen(&mut idx)
    }

    /// `class Name { constructor(){} method(){} get x(){} set x(v){} }`
    /// desugared into:
    /// `function Name(...){...}; Name.prototype.method = function(){...}; ...`
    /// plus accessor-definition statements for get/set pairs.
    fn class_rest(&mut self) -> Result<Vec<Stmt>, JsError> {
        let name = self.expect_ident()?;
        if self.eat_kw(Kw::Extends) || self.eat_kw(Kw::Static) {
            return self.err("class inheritance/static members are not supported (later wave)");
        }
        self.expect_punct(Punct::LBrace)?;
        let mut ctor: Option<Rc<FnDef>> = None;
        let mut methods: Vec<Rc<FnDef>> = Vec::new();
        // accumulated accessors, in source order (getter/setter may split)
        let mut accs: Vec<(Rc<str>, Option<Rc<FnDef>>, Option<Rc<FnDef>>)> = Vec::new();
        while !self.eat_punct(Punct::RBrace) {
            if self.eat_punct(Punct::Semi) {
                continue; // class fields are a later wave; tolerate `;`
            }
            // accessor detection: `get X(...)`/`set X(...)` where X is a
            // property name followed by the parameter list (a method actually
            // NAMED get/set has `(` directly after the name)
            let is_acc = matches!(
                self.peek(),
                Tok::Ident(s) if s.as_ref() == "get" || s.as_ref() == "set"
            ) && matches!(
                self.peek_at(1),
                Tok::Ident(_) | Tok::Kw(_) | Tok::Str(_) | Tok::Num(_)
            ) && *self.peek_at(2) == Tok::Punct(Punct::LParen);
            if is_acc {
                let getset = self.expect_ident()?;
                let key = self.prop_name()?;
                let params = self.param_list()?;
                self.expect_punct(Punct::LBrace)?;
                let body = self.block_body()?;
                let uses_arguments = scan_uses_arguments_stmts(&body);
                let f = Rc::new(FnDef {
                    name: key.clone(),
                    params,
                    body: FnBody::Block(body),
                    uses_arguments,
                    is_async: false,
                    is_generator: false,
                    has_yield: false,
                });
                let slot = accs.iter_mut().find(|(k, _, _)| *k == key);
                match slot {
                    Some((_, g, s)) => {
                        if getset.as_ref() == "get" {
                            if g.is_some() {
                                return self.err("duplicate accessor getter");
                            }
                            *g = Some(f);
                        } else {
                            if s.is_some() {
                                return self.err("duplicate accessor setter");
                            }
                            *s = Some(f);
                        }
                    }
                    None => {
                        let (g, s) = if getset.as_ref() == "get" {
                            (Some(f), None)
                        } else {
                            (None, Some(f))
                        };
                        accs.push((key, g, s));
                    }
                }
                continue;
            }
            let mname = self.prop_name()?;
            if mname.as_ref() == "constructor" {
                if ctor.is_some() {
                    return self.err("duplicate class constructor");
                }
                let params = self.param_list()?;
                self.expect_punct(Punct::LBrace)?;
                let body = self.block_body()?;
                let uses_arguments = scan_uses_arguments_stmts(&body);
                // the declared function is bound under the CLASS name
                ctor = Some(Rc::new(FnDef {
                    name: name.clone(),
                    params,
                    body: FnBody::Block(body),
                    uses_arguments,
                    is_async: false,
                    is_generator: false,
                    has_yield: false,
                }));
            } else if mname.as_ref() == "get" || mname.as_ref() == "set" {
                return self.err("accessor shorthand requires a property name");
            } else {
                let params = self.param_list()?;
                self.expect_punct(Punct::LBrace)?;
                let body = self.block_body()?;
                let uses_arguments = scan_uses_arguments_stmts(&body);
                methods.push(Rc::new(FnDef {
                    name: mname,
                    params,
                    body: FnBody::Block(body),
                    uses_arguments,
                    is_async: false,
                    is_generator: false,
                    has_yield: false,
                }));
            }
        }
        let span = Span { start: 0, end: 0 };
        let mut out = Vec::new();
        let ctor_def = ctor.unwrap_or_else(|| {
            Rc::new(FnDef {
                name: name.clone(),
                params: Vec::new(),
                body: FnBody::Block(Vec::new()),
                uses_arguments: false,
                is_async: false,
                is_generator: false,
                has_yield: false,
            })
        });
        out.push(Stmt {
            kind: StmtKind::FuncDecl(ctor_def),
            span,
        });
        for m in methods {
            let proto_member = Expr {
                kind: ExprKind::Member {
                    base: Box::new(Expr {
                        kind: ExprKind::Member {
                            base: Box::new(Expr {
                                kind: ExprKind::Ident(name.clone()),
                                span,
                            }),
                            key: Rc::from("prototype"),
                        },
                        span,
                    }),
                    key: m.name.clone(),
                },
                span,
            };
            out.push(Stmt {
                kind: StmtKind::Expr(Box::new(Expr {
                    kind: ExprKind::Assign {
                        target: Box::new(proto_member),
                        op: None,
                        value: Box::new(Expr {
                            kind: ExprKind::Func(m),
                            span,
                        }),
                    },
                    span,
                })),
                span,
            });
        }
        for (key, getter, setter) in accs {
            out.push(Stmt {
                kind: StmtKind::DefineAcc {
                    owner: Expr {
                        kind: ExprKind::Member {
                            base: Box::new(Expr {
                                kind: ExprKind::Ident(name.clone()),
                                span,
                            }),
                            key: Rc::from("prototype"),
                        },
                        span,
                    },
                    key,
                    getter,
                    setter,
                },
                span,
            });
        }
        Ok(out)
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
        r
    }

    fn assign_expr_inner(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        // arrow with single ident param: `x => ...` / `async x => ...`
        if let Tok::Ident(s) = self.peek().clone() {
            if *self.peek_at(1) == Tok::Op(Op::Arrow) {
                self.next();
                self.next(); // =>
                let body = self.arrow_body()?;
                return Ok(Expr {
                    kind: ExprKind::Arrow(Rc::new(FnDef {
                        name: Rc::from(""),
                        params: vec![Param {
                            name: s,
                            pat: None,
                            default: None,
                            rest: false,
                        }],
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
                    self.next(); // async
                    self.next(); // ident
                    self.next(); // =>
                    let body = self.arrow_body()?;
                    return Ok(Expr {
                        kind: ExprKind::Arrow(Rc::new(FnDef {
                            name: Rc::from(""),
                            params: vec![Param {
                                name: s,
                                pat: None,
                                default: None,
                                rest: false,
                            }],
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
                    let body = self.arrow_body()?;
                    return Ok(Expr {
                        kind: ExprKind::Arrow(Rc::new(FnDef {
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
                let body = self.arrow_body()?;
                return Ok(Expr {
                    kind: ExprKind::Arrow(Rc::new(FnDef {
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
            | Op::AndAssign
            | Op::OrAssign
            | Op::XorAssign
            | Op::ShlAssign
            | Op::ShrAssign
            | Op::UShrAssign)) => Some(*o),
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
            let value = self.assign_expr()?;
            let binop = match op {
                Op::Assign => None,
                Op::AddAssign => Some(BinOp::Add),
                Op::SubAssign => Some(BinOp::Sub),
                Op::MulAssign => Some(BinOp::Mul),
                Op::DivAssign => Some(BinOp::Div),
                Op::ModAssign => Some(BinOp::Mod),
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
            Ok(FnBody::Block(self.block_body()?))
        } else {
            Ok(FnBody::Expr(self.assign_expr()?))
        }
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
            toks: inner,
            pos: 0,
            depth: 0,
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
            let op = match self.peek() {
                Tok::Op(Op::Lt) => BinOp::Lt,
                Tok::Op(Op::Le) => BinOp::Le,
                Tok::Op(Op::Gt) => BinOp::Gt,
                Tok::Op(Op::Ge) => BinOp::Ge,
                Tok::Kw(Kw::In) => BinOp::In,
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
        let mut l = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Op(Op::Mul) => BinOp::Mul,
                Tok::Op(Op::Div) => BinOp::Div,
                Tok::Op(Op::Mod) => BinOp::Mod,
                _ => break,
            };
            self.next();
            let r = self.unary_expr()?;
            l = Expr {
                kind: ExprKind::Binary {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                span: self.span_from(start),
            };
        }
        // postfix ++/-- binds tighter than binary operators
        match self.peek() {
            Tok::Op(Op::Inc) => {
                self.next();
                check_assign_target(&l)?;
                l = Expr {
                    kind: ExprKind::Postfix {
                        target: Box::new(l),
                        inc: true,
                    },
                    span: self.span_from(start),
                };
            }
            Tok::Op(Op::Dec) => {
                self.next();
                check_assign_target(&l)?;
                l = Expr {
                    kind: ExprKind::Postfix {
                        target: Box::new(l),
                        inc: false,
                    },
                    span: self.span_from(start),
                };
            }
            _ => {}
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
            let expr = self.unary_expr()?;
            return Ok(Expr {
                kind: ExprKind::Typeof(Box::new(expr)),
                span: self.span_from(start),
            });
        }
        if self.eat_kw(Kw::Void) {
            let expr = self.unary_expr()?;
            return Ok(Expr {
                kind: ExprKind::Void(Box::new(expr)),
                span: self.span_from(start),
            });
        }
        if self.eat_kw(Kw::Delete) {
            let expr = self.unary_expr()?;
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
            // new callee(args) â€” callee is a member chain WITHOUT a call
            let mut c = self.primary()?;
            while self.eat_punct(Punct::Dot) {
                let key = self.prop_name()?;
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
        } else {
            self.primary()?
        };
        loop {
            match self.peek() {
                Tok::Punct(Punct::Dot) => {
                    self.next();
                    let key = self.prop_name()?;
                    e = Expr {
                        kind: ExprKind::Member {
                            base: Box::new(e),
                            key,
                        },
                        span: self.span_from(start),
                    };
                }
                Tok::Punct(Punct::LBrack) => {
                    self.next();
                    let idx = self.expr()?;
                    self.expect_punct(Punct::RBrack)?;
                    e = Expr {
                        kind: ExprKind::Index {
                            base: Box::new(e),
                            index: Box::new(idx),
                        },
                        span: self.span_from(start),
                    };
                }
                Tok::Punct(Punct::LParen) => {
                    self.next();
                    let args = self.call_args()?;
                    e = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                        },
                        span: self.span_from(start),
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn call_args(&mut self) -> Result<Vec<CallArg>, JsError> {
        let mut args = Vec::new();
        while !self.eat_punct(Punct::RParen) {
            if !args.is_empty() {
                self.expect_punct(Punct::Comma)?;
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

    fn primary(&mut self) -> Result<Expr, JsError> {
        let start = self.pos;
        let kind = match self.peek().clone() {
            Tok::Num(n) => {
                self.next();
                ExprKind::Num(n)
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
                self.next();
                ExprKind::Ident(s)
            }
            Tok::Template(parts) => {
                self.next();
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
                                toks,
                                pos: 0,
                                depth: 0,
                            };
                            let e = sub.expr()?;
                            segs.push(TplSeg::Expr(e));
                        }
                    }
                }
                ExprKind::Template(segs)
            }
            Tok::Regex(p, f) => {
                self.next();
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
            Tok::Kw(Kw::Await) => {
                self.next();
                let inner = self.unary_expr()?;
                ExprKind::Await(Box::new(inner))
            }
            Tok::Kw(Kw::Async) => {
                if *self.peek_at(1) == Tok::Kw(Kw::Function) {
                    self.next();
                    self.next();
                    let f = self.function_rest(false, true, false)?;
                    ExprKind::Func(f)
                } else if *self.peek_at(1) == Tok::Op(Op::Mul) {
                    return self.err("async generator functions are not supported");
                } else {
                    // `async => ...` (async arrow whose single param is named
                    // `async`) falls through to the ident/arrow paths; a bare
                    // `async` expression is invalid JS
                    return self.err("unexpected `async` keyword (not an async arrow function)");
                }
            }
            Tok::Kw(Kw::Class) => {
                return self.err("class expressions are not supported yet (use a statement)")
            }
            Tok::Punct(Punct::LParen) => {
                self.next();
                let e = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                return Ok(e);
            }
            Tok::Punct(Punct::LBrack) => {
                self.next();
                let mut items = Vec::new();
                while !self.eat_punct(Punct::RBrack) {
                    if !items.is_empty() {
                        self.expect_punct(Punct::Comma)?;
                        if matches!(self.peek(), Tok::Punct(Punct::Comma | Punct::RBrack)) {
                            items.push(ArrayElem::Hole); // interior hole `[a,,b]`
                            continue;
                        }
                    }
                    if *self.peek() == Tok::Spread {
                        self.next();
                        items.push(ArrayElem::Spread(self.assign_expr()?));
                    } else {
                        items.push(ArrayElem::Item(self.assign_expr()?));
                    }
                }
                ExprKind::Array(items)
            }
            Tok::Punct(Punct::LBrace) => {
                self.next();
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
                        props.push(ObjElem::Spread(self.assign_expr()?));
                        continue;
                    }
                    // method shorthand: `m(){}` / `"m"(){}`
                    let save = self.pos;
                    let key = self.prop_name()?;
                    if *self.peek() == Tok::Punct(Punct::LParen) {
                        let params = self.param_list()?;
                        self.expect_punct(Punct::LBrace)?;
                        let body = self.block_body()?;
                        let uses_arguments = scan_uses_arguments_stmts(&body);
                        let f = Rc::new(FnDef {
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
                        let v = self.assign_expr()?;
                        props.push(ObjElem::Prop(key, v));
                    } else {
                        // shorthand `{a}` -> `{a: a}`
                        props.push(ObjElem::Prop(
                            key.clone(),
                            Expr {
                                kind: ExprKind::Ident(key),
                                span: self.span_from(start),
                            },
                        ));
                    }
                }
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
            for el in elems {
                let part = match el {
                    ArrayElem::Hole => PatPart::Hole,
                    ArrayElem::Spread(s) => PatPart::Rest(Box::new(
                        expr_to_pat(s).map_err(|_| String::from("rest target must be a simple binding"))?,
                    )),
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
            for el in props {
                match el {
                    ObjElem::Spread(s) => {
                        parts.push(ObjPatPart::Rest(
                            expr_to_pat(s).map_err(|_| String::from("rest target must be a simple binding"))?,
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

fn tok_str(t: &Tok) -> String {
    match t {
        Tok::Num(n) => format!("{}", n),
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
        ExprKind::Yield(_) => k == SusKind::Yield,
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
        Array(items) => items.iter().any(|i| match i {
            ArrayElem::Item(x) | ArrayElem::Spread(x) => expr_uses(x, k),
            ArrayElem::Hole => false,
        }),
        Object(props) => props.iter().any(|p| match p {
            ObjElem::Prop(_, v) => expr_uses(v, k),
            ObjElem::Spread(v) => expr_uses(v, k),
        }),
        PatAssign { pat, value } => pat_uses(pat, k) || expr_uses(value, k),
        Seq(list) => list.iter().any(|x| expr_uses(x, k)),
        Template(segs) => segs.iter().any(|s| match s {
            TplSeg::Lit(_) => false,
            TplSeg::Expr(x) => expr_uses(x, k),
        }),
        // literals / nested functions own their trees
        Ident(_) | Num(_) | Str(_) | Bool(_) | Null | This | Regex { .. } | Func(_)
        | Arrow(_) => false,
    }
}

/// does this stmt tree contain a yield/await anywhere (excluding nested
/// function bodies, which carry their own) â€” used for honest SyntaxErrors
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
        Break | Continue => false,
        FuncDecl(_) => false, // nested function bodies own their yield/await
        PatAssign { pat, value } => pat_uses(pat, k) || expr_uses(value, k),
        DefineAcc { owner, getter, setter, .. } => {
            expr_uses(owner, k)
                || getter.as_ref().map_or(false, |f| fnbody_uses(&f.body, k))
                || setter.as_ref().map_or(false, |f| fnbody_uses(&f.body, k))
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
            ExprKind::Yield(_) => Ok(()),
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
                    Some(x) if matches!(x.kind, ExprKind::Yield(_)) => {}
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
            if stmts_use(block, SusKind::Yield)
                || catch.as_ref().map_or(false, |c| stmts_use(&c.block, SusKind::Yield))
                || finally.as_ref().map_or(false, |f| stmts_use(f, SusKind::Yield))
            {
                return Err(
                    "try/catch/finally containing a yield is not supported in generators".into(),
                );
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
        Break | Continue | FuncDecl(_) | DefineAcc { .. } => Ok(()),
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
        If { test, then, other } => {
            scan_expr_arguments(test)
                || scan_stmt_arguments(then)
                || other.as_deref().map_or(false, scan_stmt_arguments)
        }
        While { test, body } => scan_expr_arguments(test) || scan_stmt_arguments(body),
        DoWhile { body, test } => scan_stmt_arguments(body) || scan_expr_arguments(test),
        For { init, cond, upd, body } => {
            init.as_deref().map_or(false, scan_stmt_arguments)
                || cond.as_ref().map_or(false, scan_expr_arguments)
                || upd.as_ref().map_or(false, scan_expr_arguments)
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
        Return(o) => o.as_ref().map_or(false, scan_expr_arguments),
        Break | Continue => false,
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
            ObjPatPart::Rest(_) => false,
        }),
    }
}

fn scan_expr_arguments(e: &Expr) -> bool {
    use ExprKind::*;
    match &e.kind {
        Ident(s) => s.as_ref() == "arguments",
        Num(_) | Str(_) | Bool(_) | Null | This | Regex { .. } => false,
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
        Array(items) => items.iter().any(|i| match i {
            ArrayElem::Item(x) | ArrayElem::Spread(x) => scan_expr_arguments(x),
            ArrayElem::Hole => false,
        }),
        Object(props) => props.iter().any(|p| match p {
            ObjElem::Prop(_, v) => scan_expr_arguments(v),
            ObjElem::Spread(v) => scan_expr_arguments(v),
        }),
        Yield(Some(v)) => scan_expr_arguments(v),
        Yield(None) => false,
        PatAssign { pat, value } => pat_scan_args(pat) || scan_expr_arguments(value),
        Func(_) | Arrow(_) => false,
        Seq(list) => list.iter().any(scan_expr_arguments),
        Template(segs) => segs.iter().any(|s| match s {
            TplSeg::Lit(_) => false,
            TplSeg::Expr(e) => scan_expr_arguments(e),
        }),
    }
}
