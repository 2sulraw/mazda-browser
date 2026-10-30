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
        args: Vec<Expr>,
    },
    New {
        callee: Box<Expr>,
        args: Vec<Expr>,
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
    /// elements: None = hole
    Array(Vec<Option<Expr>>),
    Object(Vec<(Rc<str>, Expr)>),
    Func(Rc<FnDef>),
    Arrow(Rc<FnDef>),
    /// comma operator / expression lists
    Seq(Vec<Expr>),
    Template(Vec<TplSeg>),
    Typeof(Box<Expr>),
    Void(Box<Expr>),
    Delete(Box<Expr>),
}

#[derive(Clone, Debug)]
pub enum TplSeg {
    Lit(Rc<str>),
    Expr(Expr),
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
        name: Rc<str>,
        obj: Expr,
        body: Box<Stmt>,
    },
    ForOf {
        kind: Option<VarKind>,
        name: Rc<str>,
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

#[derive(Clone, Debug)]
pub struct Declarator {
    pub name: Rc<str>,
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
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: Rc<str>,
    pub default: Option<Expr>,
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
                let f = self.function_rest(true)?;
                StmtKind::FuncDecl(f)
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
                    ForHead::In { kind, name, obj } => StmtKind::ForIn { kind, name, obj, body },
                    ForHead::Of { kind, name, iter } => StmtKind::ForOf { kind, name, iter, body },
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
            let name = self.expect_ident()?;
            let init = if self.eat_op(Op::Assign) {
                Some(self.assign_expr()?)
            } else {
                None
            };
            out.push(Declarator { name, init });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.finish_stmt()?;
        Ok(out)
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
            // declaration form: name, then `in`/`of`/`;`
            let name = self.expect_ident()?;
            if self.eat_op(Op::Assign) {
                // classic for with initializer: `for (var i = 0; ...)`
                let init_expr = self.assign_expr()?;
                let mut decls = vec![Declarator {
                    name,
                    init: Some(init_expr),
                }];
                while self.eat_punct(Punct::Comma) {
                    let name = self.expect_ident()?;
                    let init = if self.eat_op(Op::Assign) {
                        Some(self.assign_expr()?)
                    } else {
                        None
                    };
                    decls.push(Declarator { name, init });
                }
                let init = Some(Box::new(Stmt {
                    kind: StmtKind::VarDecl(kind.unwrap_or(VarKind::Var), decls),
                    span: self.span_from(self.pos),
                }));
                return self.for_classic_tail(init);
            }
            if self.eat_kw(Kw::In) {
                let obj = self.expr()?;
                return Ok(ForHead::In { kind, name, obj });
            }
            if self.eat_kw(Kw::Of) {
                let iter = self.assign_expr()?; // no comma in for-of head
                return Ok(ForHead::Of { kind, name, iter });
            }
            // `for (let i; ...)` — declaration without initializer
            let init = Some(Box::new(Stmt {
                kind: StmtKind::VarDecl(kind.unwrap_or(VarKind::Var), vec![Declarator { name, init: None }]),
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
        let e = self.expr()?;
        if self.eat_kw(Kw::In) {
            let name = match e.kind {
                ExprKind::Ident(s) => s,
                _ => return self.err("invalid for-in target"),
            };
            let obj = self.expr()?;
            return Ok(ForHead::In {
                kind: None,
                name,
                obj,
            });
        }
        if self.eat_kw(Kw::Of) {
            let name = match e.kind {
                ExprKind::Ident(s) => s,
                _ => return self.err("invalid for-of target"),
            };
            let iter = self.assign_expr()?;
            return Ok(ForHead::Of {
                kind: None,
                name,
                iter,
            });
        }
        let init = Some(Box::new(Stmt {
            kind: StmtKind::Expr(Box::new(e)),
            span: self.span_from(self.pos),
        }));
        self.for_classic_tail(init)
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
    fn function_rest(&mut self, named: bool) -> Result<Rc<FnDef>, JsError> {
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
        let uses_arguments = scan_uses_arguments_stmts(&body_stmts);
        Ok(Rc::new(FnDef {
            name,
            params,
            body: FnBody::Block(body_stmts),
            uses_arguments,
        }))
    }

    fn param_list(&mut self) -> Result<Vec<Param>, JsError> {
        self.expect_punct(Punct::LParen)?;
        let mut params = Vec::new();
        while !self.eat_punct(Punct::RParen) {
            if !params.is_empty() {
                self.expect_punct(Punct::Comma)?;
            }
            let name = match self.peek().clone() {
                Tok::Ident(s) => {
                    self.next();
                    s
                }
                _ => return self.err("expected parameter name (destructuring/rest: later wave)"),
            };
            let default = if self.eat_op(Op::Assign) {
                Some(self.assign_expr()?)
            } else {
                None
            };
            params.push(Param { name, default });
        }
        Ok(params)
    }

    /// `class Name { constructor(){} method(){} }` desugared into:
    /// `function Name(...){...}; Name.prototype.method = function(){...}; ...`
    fn class_rest(&mut self) -> Result<Vec<Stmt>, JsError> {
        let name = self.expect_ident()?;
        if self.eat_kw(Kw::Extends) || self.eat_kw(Kw::Static) {
            return self.err("class inheritance/static members are not supported (later wave)");
        }
        self.expect_punct(Punct::LBrace)?;
        let mut ctor: Option<Rc<FnDef>> = None;
        let mut methods: Vec<Rc<FnDef>> = Vec::new();
        while !self.eat_punct(Punct::RBrace) {
            if self.eat_punct(Punct::Semi) {
                continue; // class fields are a later wave; tolerate `;`
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
                }));
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
        // arrow with single ident param: `x => ...`
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
                            default: None,
                        }],
                        body,
                        uses_arguments: false, // arrows have no arguments
                    })),
                    span: self.span_from(start),
                });
            }
        }
        // arrow with parenthesized params: `(a, b) => ...`
        if *self.peek() == Tok::Punct(Punct::LParen) {
            if let Some(params) = self.try_arrow_params()? {
                let body = self.arrow_body()?;
                return Ok(Expr {
                    kind: ExprKind::Arrow(Rc::new(FnDef {
                        name: Rc::from(""),
                        params,
                        body,
                        uses_arguments: false,
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

    /// Lookahead: if the upcoming `( ... )` is followed by `=>`, parse its
    /// contents as an arrow parameter list and return it; otherwise leave
    /// the cursor untouched and return None (callers parse it as a
    /// parenthesized expression).
    fn try_arrow_params(&mut self) -> Result<Option<Vec<Param>>, JsError> {
        debug_assert_eq!(*self.peek(), Tok::Punct(Punct::LParen));
        // scan to the `)` matching the `(` at self.pos
        let mut depth = 0usize;
        let mut i = self.pos;
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
        // parse the slice between parens as a parameter list
        let mut inner: Vec<Token> = self.toks[self.pos + 1..close].to_vec();
        // sub-slices must end with Eof: `Parser::next()` clamps at len-1, so
        // without a terminator a 1-token slice would never advance past it
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
        let mut params = Vec::new();
        while sub.pos < sub.toks.len() && sub.toks[sub.pos].tok != Tok::Eof {
            if !params.is_empty() {
                sub.expect_punct(Punct::Comma)?;
            }
            let name = match sub.peek().clone() {
                Tok::Ident(s) => {
                    sub.next();
                    s
                }
                _ => {
                    return sub.err("arrow parameters must be identifiers (destructuring: later wave)")
                }
            };
            let default = if sub.eat_op(Op::Assign) {
                Some(sub.assign_expr()?)
            } else {
                None
            };
            params.push(Param { name, default });
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
            // new callee(args) — callee is a member chain WITHOUT a call
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

    fn call_args(&mut self) -> Result<Vec<Expr>, JsError> {
        let mut args = Vec::new();
        while !self.eat_punct(Punct::RParen) {
            if !args.is_empty() {
                self.expect_punct(Punct::Comma)?;
            }
            args.push(self.assign_expr()?);
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
            Tok::Kw(Kw::Function) => {
                self.next();
                let f = self.function_rest(false)?;
                ExprKind::Func(f)
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
                    }
                    if *self.peek() == Tok::Punct(Punct::RBrack) {
                        break; // trailing comma
                    }
                    items.push(Some(self.assign_expr()?));
                }
                ExprKind::Array(items)
            }
            Tok::Punct(Punct::LBrace) => {
                self.next();
                let mut props: Vec<(Rc<str>, Expr)> = Vec::new();
                while !self.eat_punct(Punct::RBrace) {
                    if !props.is_empty() {
                        self.expect_punct(Punct::Comma)?;
                    }
                    if self.eat_punct(Punct::RBrace) {
                        break; // trailing comma
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
                        });
                        props.push((
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
                        props.push((key, v));
                    } else {
                        // shorthand `{a}` -> `{a: a}`
                        props.push((
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
        name: Rc<str>,
        obj: Expr,
    },
    Of {
        kind: Option<VarKind>,
        name: Rc<str>,
        iter: Expr,
    },
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

/// Does the function body reference `arguments` outside nested functions?
fn scan_uses_arguments_stmts(stmts: &[Stmt]) -> bool {
    stmts.iter().any(scan_stmt_arguments)
}

fn scan_stmt_arguments(s: &Stmt) -> bool {
    use StmtKind::*;
    match &s.kind {
        Expr(e) => scan_expr_arguments(e),
        VarDecl(_, ds) => ds
            .iter()
            .any(|d| d.init.as_ref().map_or(false, scan_expr_arguments)),
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
    }
}

fn scan_expr_arguments(e: &Expr) -> bool {
    use ExprKind::*;
    match &e.kind {
        Ident(s) => s.as_ref() == "arguments",
        Num(_) | Str(_) | Bool(_) | Null | This => false,
        Unary { expr, .. } | Typeof(expr) | Void(expr) | Delete(expr) => scan_expr_arguments(expr),
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
            scan_expr_arguments(callee) || args.iter().any(scan_expr_arguments)
        }
        New { callee, args } => scan_expr_arguments(callee) || args.iter().any(scan_expr_arguments),
        Member { base, .. } => scan_expr_arguments(base),
        Index { base, index } => scan_expr_arguments(base) || scan_expr_arguments(index),
        Array(items) => items.iter().any(|i| i.as_ref().map_or(false, scan_expr_arguments)),
        Object(props) => props.iter().any(|(_, v)| scan_expr_arguments(v)),
        Func(_) | Arrow(_) => false,
        Seq(list) => list.iter().any(scan_expr_arguments),
        Template(segs) => segs.iter().any(|s| match s {
            TplSeg::Lit(_) => false,
            TplSeg::Expr(e) => scan_expr_arguments(e),
        }),
    }
}
