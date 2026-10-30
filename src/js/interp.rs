//! Tree-walking evaluator: statements, expressions, operators, calls.
//!
//! Execution flow through statements is a small `Flow` enum (break/continue/
//! return); script `throw` travels as `Err(JsError::Throw(..))` so `try`
//! intercepts it naturally and error paths never allocate flow objects.

use super::parser::{
    BinOp, Expr, ExprKind, FnBody, FnDef, LogOp, Stmt, StmtKind, TplSeg, UnaryOp, VarKind,
};
use super::*;
use std::cell::RefCell;

#[derive(Debug, Clone)]
pub(crate) enum Flow {
    Normal,
    Break,
    Continue,
    Return(Val),
}

/// Hoist `var` names (as undefined) and function declarations to the top of
/// a function/program body — a naive AST walk (shortcut: no TDZ for `let`,
/// no Annex-B block-level function semantics).
pub(crate) fn hoist(e: &mut JsEngine, env: &Rc<Env>, stmts: &[Stmt]) {
    fn walk(e: &mut JsEngine, env: &Rc<Env>, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::VarDecl(kind, ds) => {
                    if *kind == VarKind::Var {
                        for d in ds {
                            env.declare_if_absent(d.name.clone(), Val::Undefined, false);
                        }
                    }
                }
                StmtKind::FuncDecl(f) => {
                    let c = make_closure(e, env, f, false);
                    env.declare_if_absent(f.name.clone(), c, false);
                }
                StmtKind::Block(b) | StmtKind::List(b) => walk(e, env, b),
                StmtKind::If { then, other, .. } => {
                    walk(e, env, std::slice::from_ref(then));
                    if let Some(o) = other {
                        walk(e, env, std::slice::from_ref(o));
                    }
                }
                StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
                    walk(e, env, std::slice::from_ref(body))
                }
                StmtKind::For { init, body, .. } => {
                    if let Some(i) = init {
                        walk(e, env, std::slice::from_ref(i));
                    }
                    walk(e, env, std::slice::from_ref(body));
                }
                StmtKind::ForIn { body, .. } | StmtKind::ForOf { body, .. } => {
                    walk(e, env, std::slice::from_ref(body))
                }
                StmtKind::Switch { cases, .. } => {
                    for c in cases {
                        walk(e, env, &c.body);
                    }
                }
                StmtKind::Try {
                    block,
                    catch,
                    finally,
                } => {
                    walk(e, env, block);
                    if let Some(c) = catch {
                        walk(e, env, &c.block);
                    }
                    if let Some(f) = finally {
                        walk(e, env, f);
                    }
                }
                _ => {}
            }
        }
    }
    walk(e, env, stmts);
}

fn make_closure(e: &mut JsEngine, env: &Rc<Env>, f: &Rc<FnDef>, is_arrow: bool) -> Val {
    let _ = e;
    Val::Func(Rc::new(Closure {
        def: f.clone(),
        env: env.clone(),
        this: Val::Undefined,
        is_arrow,
        props: RefCell::new(Vec::new()),
    }))
}

pub(crate) fn run_block(e: &mut JsEngine, env: &Rc<Env>, stmts: &[Stmt]) -> Result<Flow, JsError> {
    for s in stmts {
        let f = eval_stmt(e, env, s)?;
        if !matches!(f, Flow::Normal) {
            return Ok(f);
        }
    }
    Ok(Flow::Normal)
}

pub(crate) fn eval_stmt(e: &mut JsEngine, env: &Rc<Env>, s: &Stmt) -> Result<Flow, JsError> {
    match &s.kind {
        StmtKind::Expr(x) => {
            eval_expr(e, env, x)?;
            Ok(Flow::Normal)
        }
        StmtKind::VarDecl(kind, ds) => {
            for d in ds {
                let v = match &d.init {
                    Some(x) => eval_expr(e, env, x)?,
                    None => Val::Undefined,
                };
                match kind {
                    // var: chain-walk so it lands on the (hoisted) function env
                    VarKind::Var => e.env_set(env, &d.name, v)?,
                    VarKind::Let => env.declare(d.name.clone(), v, false),
                    // shortcut: `const` without initializer just declares
                    // undefined (real JS makes that a TDZ error)
                    VarKind::Const => env.declare(d.name.clone(), v, true),
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::Block(stmts) => {
            let benv = Env::child(env);
            run_block(e, &benv, stmts)
        }
        StmtKind::List(stmts) => run_block(e, env, stmts),
        StmtKind::If { test, then, other } => {
            if eval_expr(e, env, test)?.to_bool() {
                eval_stmt(e, env, then)
            } else if let Some(o) = other {
                eval_stmt(e, env, o)
            } else {
                Ok(Flow::Normal)
            }
        }
        StmtKind::While { test, body } => {
            while eval_expr(e, env, test)?.to_bool() {
                match eval_stmt(e, env, body)? {
                    Flow::Normal => {}
                    Flow::Break => break,
                    Flow::Continue => {}
                    r @ Flow::Return(_) => return Ok(r),
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::DoWhile { body, test } => {
            loop {
                match eval_stmt(e, env, body)? {
                    Flow::Normal => {}
                    Flow::Break => break,
                    Flow::Continue => {}
                    r @ Flow::Return(_) => return Ok(r),
                }
                if !eval_expr(e, env, test)?.to_bool() {
                    break;
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::For { init, cond, upd, body } => {
            let lenv = Env::child(env);
            if let Some(i) = init {
                eval_stmt(e, &lenv, i)?;
            }
            loop {
                if let Some(c) = cond {
                    if !eval_expr(e, &lenv, c)?.to_bool() {
                        break;
                    }
                }
                match eval_stmt(e, &lenv, body)? {
                    Flow::Normal => {}
                    Flow::Break => break,
                    Flow::Continue => {}
                    r @ Flow::Return(_) => return Ok(r),
                }
                if let Some(u) = upd {
                    eval_expr(e, &lenv, u)?;
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::ForIn { kind, name, obj, body } => {
            let o = eval_expr(e, env, obj)?;
            let keys = for_in_keys(&o);
            for k in keys {
                let benv = bind_loop(e, env, kind.unwrap_or(VarKind::Var), name, Val::Str(k))?;
                match eval_stmt(e, &benv, body)? {
                    Flow::Normal => {}
                    Flow::Break => break,
                    Flow::Continue => continue,
                    r @ Flow::Return(_) => return Ok(r),
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::ForOf { kind, name, iter, body } => {
            let it = eval_expr(e, env, iter)?;
            let items = for_of_items(&it)?;
            for v in items {
                let benv = bind_loop(e, env, kind.unwrap_or(VarKind::Var), name, v)?;
                match eval_stmt(e, &benv, body)? {
                    Flow::Normal => {}
                    Flow::Break => break,
                    Flow::Continue => continue,
                    r @ Flow::Return(_) => return Ok(r),
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::Switch { disc, cases } => {
            let d = eval_expr(e, env, disc)?;
            // find the entry point: first matching case, else default, else nothing
            let mut start = None;
            let mut default_at = None;
            for (i, c) in cases.iter().enumerate() {
                match &c.test {
                    None => default_at = Some(i),
                    Some(t) => {
                        let tv = eval_expr(e, env, t)?;
                        if start.is_none() && strict_eq(&d, &tv) {
                            start = Some(i);
                        }
                    }
                }
            }
            let start = match start.or(default_at) {
                None => return Ok(Flow::Normal),
                Some(i) => i,
            };
            let senv = Env::child(env);
            for c in &cases[start..] {
                for st in &c.body {
                    match eval_stmt(e, &senv, st)? {
                        Flow::Normal => {}
                        Flow::Break => return Ok(Flow::Normal),
                        r => return Ok(r), // return/continue propagate
                    }
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::Try {
            block,
            catch,
            finally,
        } => {
            let mut result = run_block(e, env, block);
            if let (Some(v), Some(cat)) = (as_err_ref(&result), catch) {
                let v = v.clone();
                let cenv = Env::child(env);
                cenv.declare(cat.name.clone(), v, false);
                result = run_block(e, &cenv, &cat.block);
            }
            if let Some(fin) = finally {
                let fr = run_block(e, env, fin);
                match fr {
                    // a finally that returns/breaks/continues/throws replaces
                    // whatever was pending (standard JS semantics)
                    Ok(Flow::Normal) => {}
                    other => result = other,
                }
            }
            result
        }
        StmtKind::Throw(x) => {
            let v = eval_expr(e, env, x)?;
            Err(JsError::Throw(v))
        }
        StmtKind::Return(o) => {
            let v = match o {
                Some(x) => eval_expr(e, env, x)?,
                None => Val::Undefined,
            };
            Ok(Flow::Return(v))
        }
        StmtKind::Break => Ok(Flow::Break),
        StmtKind::Continue => Ok(Flow::Continue),
        StmtKind::FuncDecl(f) => {
            let c = make_closure(e, env, f, false);
            env.declare_if_absent(f.name.clone(), c, false);
            Ok(Flow::Normal)
        }
    }
}

/// helper: `Err(JsError::Throw(v))` -> Some(v) without consuming
fn as_err_ref(r: &Result<Flow, JsError>) -> Option<&Val> {
    match r {
        Err(JsError::Throw(v)) => Some(v),
        _ => None,
    }
}

/// Bind a loop variable for one iteration and return the env the body runs
/// in. `let/const` get a fresh env per iteration (closures capture the right
/// value); `var`/expression targets write through the chain.
fn bind_loop(
    e: &mut JsEngine,
    env: &Rc<Env>,
    kind: VarKind,
    name: &Rc<str>,
    v: Val,
) -> Result<Rc<Env>, JsError> {
    match kind {
        VarKind::Let | VarKind::Const => {
            let benv = Env::child(env);
            benv.declare(name.clone(), v, kind == VarKind::Const);
            Ok(benv)
        }
        VarKind::Var => {
            e.env_set(env, name, v)?;
            Ok(env.clone())
        }
    }
}

/// shortcut: for-in enumerates OWN props only (prototype-chain keys and
/// insertion-order vs source-order quirks are ignored), and primitives
/// enumerate nothing (real JS boxes them).
fn for_in_keys(v: &Val) -> Vec<Rc<str>> {
    match v {
        Val::Obj(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => {
                    (0..items.len()).map(|i| Rc::from(i.to_string())).collect()
                }
                _ => b.props.iter().map(|(k, _)| k.clone()).collect(),
            }
        }
        _ => Vec::new(),
    }
}

/// shortcut: no iterator protocol — for-of handles dense arrays and strings
/// only; anything else throws a clear TypeError (custom iterables, Maps,
/// Sets are later waves).
fn for_of_items(v: &Val) -> Result<Vec<Val>, JsError> {
    match v {
        Val::Obj(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => Ok(items.clone()),
                _ => Err(JsError::throw(
                    "TypeError: for...of supports arrays and strings only (iterator protocol: later wave)",
                )),
            }
        }
        Val::Str(s) => Ok(s.chars().map(|c| Val::Str(Rc::from(c.to_string()))).collect()),
        _ => Err(JsError::throw(
            "TypeError: for...of supports arrays and strings only (iterator protocol: later wave)",
        )),
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

pub(crate) fn eval_expr(e: &mut JsEngine, env: &Rc<Env>, x: &Expr) -> Result<Val, JsError> {
    match &x.kind {
        ExprKind::Ident(name) => Ok(e.env_get(env, name)),
        ExprKind::Num(n) => Ok(Val::Num(*n)),
        ExprKind::Str(s) => Ok(Val::Str(s.clone())),
        ExprKind::Bool(b) => Ok(Val::Bool(*b)),
        ExprKind::Null => Ok(Val::Null),
        ExprKind::This => Ok(e.env_get(env, "this")),
        ExprKind::Unary { op, expr } => {
            let v = eval_expr(e, env, expr)?;
            Ok(match op {
                UnaryOp::Neg => Val::Num(-v.to_number()),
                UnaryOp::Pos => Val::Num(v.to_number()),
                UnaryOp::Not => Val::Bool(!v.to_bool()),
                UnaryOp::BitNot => Val::Num(!to_int32(v.to_number()) as f64),
                UnaryOp::PreInc | UnaryOp::PreDec => {
                    let n = v.to_number()
                        + if matches!(op, UnaryOp::PreInc) { 1.0 } else { -1.0 };
                    let nv = Val::Num(n);
                    assign_target(e, env, expr, nv.clone())?;
                    nv
                }
            })
        }
        ExprKind::Postfix { target, inc } => {
            let v = eval_target_value(e, env, target)?;
            let n = v.to_number() + if *inc { 1.0 } else { -1.0 };
            assign_target(e, env, target, Val::Num(n))?;
            Ok(v) // postfix yields the OLD value
        }
        ExprKind::Binary { op, l, r } => {
            let lv = eval_expr(e, env, l)?;
            let rv = eval_expr(e, env, r)?;
            binary_op(*op, &lv, &rv)
        }
        ExprKind::Logical { op, l, r } => {
            let lv = eval_expr(e, env, l)?;
            Ok(match op {
                LogOp::And => {
                    if lv.to_bool() {
                        eval_expr(e, env, r)?
                    } else {
                        lv
                    }
                }
                LogOp::Or => {
                    if lv.to_bool() {
                        lv
                    } else {
                        eval_expr(e, env, r)?
                    }
                }
                LogOp::Coalesce => {
                    if matches!(lv, Val::Undefined | Val::Null) {
                        eval_expr(e, env, r)?
                    } else {
                        lv
                    }
                }
            })
        }
        ExprKind::Assign { target, op, value } => {
            let mut v = eval_expr(e, env, value)?;
            if let Some(bop) = op {
                let cur = eval_target_value(e, env, target)?;
                v = binary_op(*bop, &cur, &v)?;
            }
            assign_target(e, env, target, v.clone())?;
            Ok(v)
        }
        ExprKind::Cond { test, then, other } => {
            if eval_expr(e, env, test)?.to_bool() {
                eval_expr(e, env, then)
            } else {
                eval_expr(e, env, other)
            }
        }
        ExprKind::Call { callee, args } => {
            let argvals = eval_args(e, env, args)?;
            match &callee.kind {
                ExprKind::Member { base, key } => {
                    let b = eval_expr(e, env, base)?;
                    let f = get_member(e, &b, key)?;
                    e.call(f, b, argvals)
                }
                ExprKind::Index { base, index } => {
                    let b = eval_expr(e, env, base)?;
                    let k = eval_expr(e, env, index)?.to_string();
                    let f = get_member(e, &b, &k)?;
                    e.call(f, b, argvals)
                }
                _ => {
                    let f = eval_expr(e, env, callee)?;
                    // sloppy mode: plain calls get the global object as this
                    let this = Val::Obj(e.global_obj.clone());
                    e.call(f, this, argvals)
                }
            }
        }
        ExprKind::New { callee, args } => {
            let f = eval_expr(e, env, callee)?;
            let argvals = eval_args(e, env, args)?;
            let fresh = match &f {
                Val::Func(c) => {
                    let proto = ensure_prototype(c);
                    Val::Obj(Rc::new(RefCell::new(Obj {
                        kind: ObjKind::Plain,
                        props: Vec::new(),
                        proto: Some(proto),
                    })))
                }
                _ => Val::new_object(),
            };
            let res = e.call(f, fresh.clone(), argvals)?;
            // constructor returning an object overrides `this`
            Ok(if matches!(res, Val::Obj(_)) { res } else { fresh })
        }
        ExprKind::Member { base, key } => {
            let b = eval_expr(e, env, base)?;
            get_member(e, &b, key)
        }
        ExprKind::Index { base, index } => {
            let b = eval_expr(e, env, base)?;
            let k = eval_expr(e, env, index)?.to_string();
            get_member(e, &b, &k)
        }
        ExprKind::Array(items) => {
            let mut vals = Vec::with_capacity(items.len());
            for i in items {
                match i {
                    Some(x) => vals.push(eval_expr(e, env, x)?),
                    None => vals.push(Val::Undefined), // hole -> undefined (dense shortcut)
                }
            }
            Ok(Val::new_array(vals))
        }
        ExprKind::Object(props) => {
            let o = Val::new_object();
            for (k, v) in props {
                let val = eval_expr(e, env, v)?;
                obj_set_own(own(&o), k, val);
            }
            Ok(o)
        }
        ExprKind::Func(f) => {
            let c = Rc::new(Closure {
                def: f.clone(),
                env: env.clone(),
                this: Val::Undefined,
                is_arrow: false,
                props: RefCell::new(Vec::new()),
            });
            // a named function expression binds its own name inside its scope
            // (`let f = function fact() { fact() }`); declarations already
            // bind the name in the enclosing env, so this child env is
            // harmless there. The closure's env is the child from the start
            // so the bound name sees the self-binding too.
            if !f.name.is_empty() {
                let fenv = Env::child(env);
                let c = Rc::new(Closure {
                    def: f.clone(),
                    env: fenv.clone(),
                    this: Val::Undefined,
                    is_arrow: false,
                    props: RefCell::new(Vec::new()),
                });
                fenv.declare(f.name.clone(), Val::Func(c.clone()), false);
                return Ok(Val::Func(c));
            }
            Ok(Val::Func(c))
        }
        ExprKind::Arrow(f) => {
            // lexical this captured at creation; the body is hoisted at
            // CALL time into the fresh call env (see `call_inner`)
            let lexical_this = e.env_get(env, "this");
            Ok(Val::Func(Rc::new(Closure {
                def: f.clone(),
                env: env.clone(),
                this: lexical_this,
                is_arrow: true,
                props: RefCell::new(Vec::new()),
            })))
        }
        ExprKind::Seq(list) => {
            let mut last = Val::Undefined;
            for x in list {
                last = eval_expr(e, env, x)?;
            }
            Ok(last)
        }
        ExprKind::Template(segs) => {
            let mut out = String::new();
            for seg in segs {
                match seg {
                    TplSeg::Lit(s) => out.push_str(s),
                    TplSeg::Expr(x) => {
                        let v = eval_expr(e, env, x)?;
                        out.push_str(&v.to_string());
                    }
                }
            }
            Ok(Val::Str(Rc::from(out.as_str())))
        }
        ExprKind::Typeof(expr) => {
            // `typeof undeclaredIdent` must not throw
            let v = match &expr.kind {
                ExprKind::Ident(name) => e.env_get(env, name),
                _ => eval_expr(e, env, expr)?,
            };
            Ok(Val::Str(Rc::from(v.type_of())))
        }
        ExprKind::Void(expr) => {
            eval_expr(e, env, expr)?;
            Ok(Val::Undefined)
        }
        ExprKind::Delete(expr) => Ok(Val::Bool(match &expr.kind {
            ExprKind::Member { base, key } => {
                let b = eval_expr(e, env, base)?;
                delete_member(&b, key)
            }
            ExprKind::Index { base, index } => {
                let b = eval_expr(e, env, base)?;
                let k = eval_expr(e, env, index)?.to_string();
                delete_member(&b, &k)
            }
            // sloppy mode: `delete ident` is a no-op returning false
            _ => false,
        })),
    }
}

fn own(v: &Val) -> &Rc<RefCell<Obj>> {
    match v {
        Val::Obj(o) => o,
        _ => unreachable!("own() on non-object"),
    }
}

fn eval_args(e: &mut JsEngine, env: &Rc<Env>, args: &[Expr]) -> Result<Vec<Val>, JsError> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        out.push(eval_expr(e, env, a)?);
    }
    Ok(out)
}

/// current value of an assignment target (ident or member)
fn eval_target_value(e: &mut JsEngine, env: &Rc<Env>, target: &Expr) -> Result<Val, JsError> {
    match &target.kind {
        ExprKind::Ident(name) => Ok(e.env_get(env, name)),
        ExprKind::Member { base, key } => {
            let b = eval_expr(e, env, base)?;
            get_member(e, &b, key)
        }
        ExprKind::Index { base, index } => {
            let b = eval_expr(e, env, base)?;
            let k = eval_expr(e, env, index)?.to_string();
            get_member(e, &b, &k)
        }
        _ => Err(JsError::throw("TypeError: invalid assignment target")),
    }
}

fn assign_target(
    e: &mut JsEngine,
    env: &Rc<Env>,
    target: &Expr,
    v: Val,
) -> Result<(), JsError> {
    match &target.kind {
        ExprKind::Ident(name) => e.env_set(env, name, v),
        ExprKind::Member { base, key } => {
            let b = eval_expr(e, env, base)?;
            set_member(&b, key, v)
        }
        ExprKind::Index { base, index } => {
            let b = eval_expr(e, env, base)?;
            let k = eval_expr(e, env, index)?.to_string();
            set_member(&b, &k, v)
        }
        _ => Err(JsError::throw("TypeError: invalid assignment target")),
    }
}

/// `.prototype` accessor for constructors: created lazily on first use so
/// plain (non-constructor) functions pay no prototype allocation.
pub(crate) fn ensure_prototype(c: &Rc<Closure>) -> Rc<RefCell<Obj>> {
    if let Some((_, v)) = c.props.borrow().iter().find(|(k, _)| k.as_ref() == "prototype") {
        if let Val::Obj(o) = v {
            return o.clone();
        }
    }
    let proto = Rc::new(RefCell::new(Obj {
        kind: ObjKind::Plain,
        props: vec![(
            Rc::from("constructor"),
            Val::Func(c.clone()),
        )],
        proto: None,
    }));
    c.props
        .borrow_mut()
        .push((Rc::from("prototype"), Val::Obj(proto.clone())));
    proto
}

fn binary_op(op: BinOp, l: &Val, r: &Val) -> Result<Val, JsError> {
    use BinOp::*;
    Ok(match op {
        Add => {
            // string concat if either side is (or stringifies to) a string
            if matches!(l, Val::Str(_) | Val::Obj(_) | Val::Func(_) | Val::Native(_) | Val::Method(_))
                || matches!(r, Val::Str(_) | Val::Obj(_) | Val::Func(_) | Val::Native(_) | Val::Method(_))
            {
                Val::Str(Rc::from(format!("{}{}", l.to_string(), r.to_string())))
            } else {
                Val::Num(l.to_number() + r.to_number())
            }
        }
        Sub => Val::Num(l.to_number() - r.to_number()),
        Mul => Val::Num(l.to_number() * r.to_number()),
        Div => Val::Num(l.to_number() / r.to_number()),
        Mod => Val::Num(l.to_number() % r.to_number()),
        Eq => Val::Bool(loose_eq(l, r)),
        Ne => Val::Bool(!loose_eq(l, r)),
        StrictEq => Val::Bool(strict_eq(l, r)),
        StrictNe => Val::Bool(!strict_eq(l, r)),
        Lt | Le | Gt | Ge => Val::Bool(relational(op, l, r)),
        BitAnd => Val::Num((to_int32(l.to_number()) & to_int32(r.to_number())) as f64),
        BitOr => Val::Num((to_int32(l.to_number()) | to_int32(r.to_number())) as f64),
        BitXor => Val::Num((to_int32(l.to_number()) ^ to_int32(r.to_number())) as f64),
        Shl => Val::Num(
            (to_int32(l.to_number()) << (to_uint32(r.to_number()) & 31)) as f64,
        ),
        Shr => Val::Num(
            (to_int32(l.to_number()) >> (to_uint32(r.to_number()) & 31)) as f64,
        ),
        UShr => Val::Num(
            (to_uint32(l.to_number()) >> (to_uint32(r.to_number()) & 31)) as f64,
        ),
        In => {
            let key = l.to_string();
            match r {
                Val::Obj(o) => Val::Bool(obj_has(o, &key)),
                Val::Str(s) => {
                    // `"0" in "abc"` -> true (index/length/method names)
                    let idx = key.parse::<usize>().ok().and_then(|i| s.chars().nth(i));
                    Val::Bool(idx.is_some() || key == "length")
                }
                _ => {
                    return Err(JsError::throw(&format!(
                        "TypeError: Cannot use `in` operator on {}",
                        r.type_of()
                    )))
                }
            }
        }
        InstanceOf => match r {
            Val::Func(c) => {
                // lazy prototype creation: `instanceof` works even before
                // anything touched `.prototype`
                let p = ensure_prototype(c);
                let yes = match l {
                    Val::Obj(o) => {
                        let mut cur = o.borrow().proto.clone();
                        let mut found = false;
                        for _ in 0..100 {
                            let c = match cur {
                                None => break,
                                Some(c) => c,
                            };
                            if Rc::ptr_eq(&c, &p) {
                                found = true;
                                break;
                            }
                            cur = c.borrow().proto.clone();
                        }
                        found
                    }
                    _ => false,
                };
                Val::Bool(yes)
            }
            // shortcut: natives aren't real constructors; the only native
            // `instanceof` that answers true is Date (kind-tagged objects)
            Val::Native(_) => Val::Bool(matches!(
                l,
                Val::Obj(o) if matches!(o.borrow().kind, ObjKind::Date(_))
            )),
            _ => {
                return Err(JsError::throw("TypeError: RHS of instanceof is not a function"))
            }
        },
    })
}

fn relational(op: BinOp, l: &Val, r: &Val) -> bool {
    // both strings compare lexicographically (UTF-8 byte order ≈ JS's
    // UTF-16 code-unit order for BMP text; astral pairs differ — shortcut)
    if let (Val::Str(a), Val::Str(b)) = (l, r) {
        return match op {
            BinOp::Lt => a.as_bytes() < b.as_bytes(),
            BinOp::Le => a.as_bytes() <= b.as_bytes(),
            BinOp::Gt => a.as_bytes() > b.as_bytes(),
            BinOp::Ge => a.as_bytes() >= b.as_bytes(),
            _ => unreachable!(),
        };
    }
    let a = l.to_number();
    let b = r.to_number();
    if a.is_nan() || b.is_nan() {
        return false; // every relational comparison with NaN is false
    }
    match op {
        BinOp::Lt => a < b,
        BinOp::Le => a <= b,
        BinOp::Gt => a > b,
        BinOp::Ge => a >= b,
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Built-in method dispatch (`Val::Method`)
// ---------------------------------------------------------------------------

fn need_str(this: &Val) -> Result<Rc<str>, JsError> {
    match this {
        Val::Str(s) => Ok(s.clone()),
        _ => Err(JsError::throw("TypeError: String method called on non-string")),
    }
}

fn arg_str(args: &[Val], i: usize) -> Rc<str> {
    match args.get(i) {
        Some(v) => Rc::from(v.to_string().as_str()),
        None => Rc::from(""),
    }
}

fn arg_num(args: &[Val], i: usize) -> f64 {
    match args.get(i) {
        Some(v) => v.to_number(),
        None => f64::NAN,
    }
}

pub(crate) fn call_method(
    e: &mut JsEngine,
    m: Meth,
    this: &Val,
    args: &[Val],
) -> Result<Val, JsError> {
    use Meth::*;
    match m {
        // -- string methods ------------------------------------------------
        StrToUpper => Ok(Val::Str(Rc::from(
            need_str(this)?.to_uppercase().as_str(),
        ))),
        StrToLower => Ok(Val::Str(Rc::from(
            need_str(this)?.to_lowercase().as_str(),
        ))),
        StrTrim => Ok(Val::Str(Rc::from(need_str(this)?.trim()))),
        StrTrimStart => Ok(Val::Str(Rc::from(need_str(this)?.trim_start()))),
        StrTrimEnd => Ok(Val::Str(Rc::from(need_str(this)?.trim_end()))),
        StrSlice => {
            let s = need_str(this)?;
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let (a, b) = slice_bounds(arg_num(args, 0), args.get(1), n);
            Ok(Val::Str(Rc::from(chars[a..b].iter().collect::<String>().as_str())))
        }
        StrIndexOf => {
            let s = need_str(this)?;
            let needle = arg_str(args, 0);
            let from = arg_num(args, 1);
            let from = if from.is_nan() || from < 0.0 { 0 } else { from as usize };
            let idx = char_index_of(&s, &needle, from);
            Ok(Val::Num(idx.map(|i| i as f64).unwrap_or(-1.0)))
        }
        StrIncludes => {
            let s = need_str(this)?;
            let needle = arg_str(args, 0);
            let from = arg_num(args, 1);
            let from = if from.is_nan() || from < 0.0 { 0 } else { from as usize };
            Ok(Val::Bool(char_index_of(&s, &needle, from).is_some()))
        }
        StrSplit => {
            let s = need_str(this)?;
            let sep = arg_str(args, 0);
            let parts: Vec<Val> = if sep.is_empty() {
                s.chars().map(|c| Val::Str(Rc::from(c.to_string()))).collect()
            } else {
                s.as_ref()
                    .split(sep.as_ref())
                    .map(|p| Val::Str(Rc::from(p)))
                    .collect()
            };
            Ok(Val::new_array(parts))
        }
        StrReplace => {
            let s = need_str(this)?;
            let from = arg_str(args, 0);
            let to = arg_str(args, 1);
            if from.is_empty() {
                return Ok(Val::Str(Rc::from(format!("{}{}", to, s))));
            }
            // shortcut: string pattern only; function replacers are a later wave
            Ok(match s.find(from.as_ref()) {
                Some(i) => Val::Str(Rc::from(
                    format!("{}{}{}", &s[..i], to, &s[i + from.len()..]).as_str(),
                )),
                None => Val::Str(s),
            })
        }
        StrStartsWith => {
            let s = need_str(this)?;
            Ok(Val::Bool(s.starts_with(arg_str(args, 0).as_ref())))
        }
        StrEndsWith => {
            let s = need_str(this)?;
            Ok(Val::Bool(s.ends_with(arg_str(args, 0).as_ref())))
        }
        StrRepeat => {
            let s = need_str(this)?;
            let n = arg_num(args, 0);
            if n.is_nan() || n < 0.0 {
                return Err(JsError::throw("RangeError: Invalid count value"));
            }
            let n = n as usize;
            // RAM guard: scripts don't build strings bigger than 16 MB here
            if s.len().saturating_mul(n) > 16_000_000 {
                return Err(JsError::throw("RangeError: repeat count too large"));
            }
            Ok(Val::Str(Rc::from(s.repeat(n).as_str())))
        }
        StrPadStart | StrPadEnd => {
            let s = need_str(this)?;
            let target = arg_num(args, 0);
            let pad_rc = arg_str(args, 1);
            let pad: &str = if pad_rc.is_empty() { " " } else { &pad_rc };
            let cur = s.chars().count();
            let target = if target.is_nan() { cur } else { target as usize };
            if cur >= target {
                return Ok(Val::Str(s));
            }
            let need = target - cur;
            let pad_chars: Vec<char> = pad.chars().collect();
            let mut fill = String::new();
            while fill.chars().count() < need {
                fill.extend(pad_chars.iter().copied());
            }
            let fill: String = fill.chars().take(need).collect();
            let padded = if m == StrPadStart {
                format!("{}{}", fill, s)
            } else {
                format!("{}{}", s, fill)
            };
            Ok(Val::Str(Rc::from(padded.as_str())))
        }
        StrCharAt => {
            let s = need_str(this)?;
            let i = arg_num(args, 0);
            let i = if i.is_nan() || i < 0.0 { usize::MAX } else { i as usize };
            Ok(match s.chars().nth(i) {
                Some(c) => Val::Str(Rc::from(c.to_string())),
                None => Val::Str(Rc::from("")),
            })
        }
        StrToString => Ok(need_str(this).map(Val::Str)?),

        // -- number methods ------------------------------------------------
        NumToString => match this {
            Val::Num(n) => Ok(Val::Str(Rc::from(num_to_str(*n).as_str()))),
            Val::Bool(b) => Ok(Val::Str(Rc::from(b.to_string()))),
            _ => Err(JsError::throw("TypeError: Number method called on wrong type")),
        },
        NumToFixed => {
            let n = match this {
                Val::Num(n) => *n,
                _ => return Err(JsError::throw("TypeError: Number method called on wrong type")),
            };
            let d = arg_num(args, 0);
            if d.is_nan() {
                return Err(JsError::throw("RangeError: digits argument must be a number"));
            }
            let d = d.trunc();
            if !(0.0..=100.0).contains(&d) || !n.is_finite() {
                return Err(JsError::throw("RangeError: toFixed digits out of range / not finite"));
            }
            Ok(Val::Str(Rc::from(format!("{:.*}", d as usize, n).as_str())))
        }

        // -- date methods --------------------------------------------------
        DateNow => {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0);
            Ok(Val::Num(ms))
        }
        DateGetTime => match this {
            Val::Obj(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Date(ms) => Ok(Val::Num(*ms)),
                    _ => Err(JsError::throw("TypeError: getTime on non-Date")),
                }
            }
            _ => Err(JsError::throw("TypeError: getTime on non-Date")),
        },
        DateToISO => match this {
            Val::Obj(o) => {
                let ms = {
                    let b = o.borrow();
                    match &b.kind {
                        ObjKind::Date(ms) => *ms,
                        _ => return Err(JsError::throw("TypeError: toISOString on non-Date")),
                    }
                };
                match fmt_date_iso(ms) {
                    Some(s) => Ok(Val::Str(Rc::from(s.as_str()))),
                    None => Err(JsError::throw("RangeError: Invalid time value")),
                }
            }
            _ => Err(JsError::throw("TypeError: toISOString on non-Date")),
        },

        // -- promise methods ----------------------------------------------
        PromiseThen | PromiseCatch => {
            let o = match this {
                Val::Obj(o) => o.clone(),
                _ => return Err(JsError::throw("TypeError: then on non-Promise")),
            };
            // `.then(onOk, onErr)` vs `.catch(onErr)` — catch's single arg
            // is the REJECTION handler
            let (on_ok, on_err) = if m == PromiseCatch {
                (
                    Val::Undefined,
                    args.first().cloned().unwrap_or(Val::Undefined),
                )
            } else {
                (
                    args.first().cloned().unwrap_or(Val::Undefined),
                    args.get(1).cloned().unwrap_or(Val::Undefined),
                )
            };
            let settled = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Promise(st) => st.settled.clone(),
                    _ => None,
                }
            };
            match settled {
                Some(Ok(v)) => {
                    if !matches!(on_ok, Val::Undefined | Val::Null) {
                        e.microtasks.push(Microtask {
                            func: on_ok,
                            arg: v,
                        });
                    }
                }
                Some(Err(err)) => {
                    if !matches!(on_err, Val::Undefined | Val::Null) {
                        e.microtasks.push(Microtask {
                            func: on_err,
                            arg: err,
                        });
                    }
                }
                None => {
                    // still pending: queue handlers (they fire when a future
                    // wave grows a real resolve/reject path)
                    let mut b = o.borrow_mut();
                    if let ObjKind::Promise(st) = &mut b.kind {
                        st.handlers.push((Some(on_ok), Some(on_err)));
                    }
                }
            }
            // shortcut: no chaining — `.then` returns the same promise, so
            // `p.then(f).then(g)` does NOT chain g off f's result; real
            // promise chaining is a later wave.
            Ok(Val::Obj(o))
        }

        // -- function methods ----------------------------------------------
        FnCall | FnApply | FnBind => {
            let target = this.clone();
            if !matches!(target, Val::Func(_) | Val::Native(_) | Val::Method(_)) {
                return Err(JsError::throw("TypeError: function method on non-function"));
            }
            match m {
                FnCall => {
                    let bound_this = args.first().cloned().unwrap_or(Val::Undefined);
                    let rest: Vec<Val> = args.iter().skip(1).cloned().collect();
                    e.call(target, bound_this, rest)
                }
                FnApply => {
                    let bound_this = args.first().cloned().unwrap_or(Val::Undefined);
                    let rest: Vec<Val> = match args.get(1) {
                        Some(Val::Obj(o)) => {
                            let b = o.borrow();
                            match &b.kind {
                                ObjKind::Array(items) => items.clone(),
                                _ => Vec::new(),
                            }
                        }
                        _ => Vec::new(),
                    };
                    e.call(target, bound_this, rest)
                }
                FnBind => {
                    let bound_this = args.first().cloned().unwrap_or(Val::Undefined);
                    let bound_args: Vec<Val> = args.iter().skip(1).cloned().collect();
                    // synthesize a bound closure around the target
                    let def = Rc::new(FnDef {
                        name: Rc::from("bound"),
                        params: Vec::new(),
                        body: FnBody::Bound {
                            target,
                            this: bound_this,
                            args: bound_args,
                        },
                        uses_arguments: false,
                    });
                    Ok(Val::Func(Rc::new(Closure {
                        def,
                        env: Env::root(),
                        this: Val::Undefined,
                        is_arrow: false,
                        props: RefCell::new(Vec::new()),
                    })))
                }
                _ => unreachable!(),
            }
        }
    }
}

/// char-index based `indexOf` (JS string indices count chars, not bytes)
fn char_index_of(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let chars: Vec<char> = hay.chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() {
        return Some(from.min(chars.len()));
    }
    if needle.len() > chars.len() {
        return None;
    }
    for start in from..=(chars.len() - needle.len()) {
        if chars[start..start + needle.len()] == needle[..] {
            return Some(start);
        }
    }
    None
}

/// shared start/end clamping for slice-like ops
pub(crate) fn slice_bounds(start: f64, end: Option<&Val>, len: i64) -> (usize, usize) {
    let norm = |v: f64| -> i64 {
        if v.is_nan() {
            0
        } else {
            let v = v as i64;
            if v < 0 {
                (v + len).max(0)
            } else {
                v.min(len)
            }
        }
    };
    let a = norm(if start.is_nan() { 0.0 } else { start });
    let b = norm(match end {
        None => len as f64,
        Some(v) => v.to_number(),
    });
    (a.max(0) as usize, b.max(a) as usize)
}

/// days-since-epoch -> (year, month, day); standard civil-from-days math
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// ms since epoch -> ISO-8601 UTC string (used by Date + JSON.stringify)
pub(crate) fn fmt_date_iso(ms: f64) -> Option<String> {
    if !ms.is_finite() {
        return None;
    }
    let total = ms.floor() as i64;
    let days = total.div_euclid(86_400_000);
    let rem = total.rem_euclid(86_400_000);
    let (y, mo, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3_600_000, (rem / 60_000) % 60, (rem / 1000) % 60);
    let millis = rem % 1000;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, hh, mm, ss, millis
    ))
}
