//! Tree-walking evaluator: statements, expressions, operators, calls.
//!
//! Execution flow through statements is a small `Flow` enum (break/continue/
//! return); script `throw` travels as `Err(JsError::Throw(..))` so `try`
//! intercepts it naturally and error paths never allocate flow objects.
//! This wave adds: destructuring binding (`Pat` trees evaluated directly,
//! no temporary desugar), spread/rest argument expansion, regex literals
//! (compiled once per (pattern, flags) on the engine), `await` (run-to-
//! completion microtask pump â€” see `await_value`), and generator bodies
//! walking through the resumable VM in `gen.rs`.

use super::gen;
use super::parser::{
    ArrayElem, BinOp, BindTgt, CallArg, Declarator, Expr, ExprKind, FnBody, FnDef, ForTarget,
    LogOp, ObjElem, ObjPatPart, Pat, PatPart, Stmt, StmtKind, TplSeg, UnaryOp, VarKind,
};
use super::regex;
use super::*;
use std::cell::RefCell;

#[derive(Debug, Clone)]
pub(crate) enum Flow {
    Normal,
    Break,
    Continue,
    Return(Val),
}

/// How a `Pat` tree binds: a fresh declaration (`let`/`const`/`var`/call
/// param), or an existing lvalue (destructuring assignment `[a, b] = x`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum BindKind {
    Decl(VarKind),
    Assign,
    /// call parameters bind like `var` (names declared in the call env)
    Param,
}

/// Hoist `var` names (as undefined) and function declarations to the top of
/// a function/program body â€” a naive AST walk (shortcut: no TDZ for `let`,
/// no Annex-B block-level function semantics). Destructured `var` leaves
/// participate (hoisted undefined).
pub(crate) fn hoist(e: &mut JsEngine, env: &Rc<Env>, stmts: &[Stmt]) {
    fn walk(e: &mut JsEngine, env: &Rc<Env>, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::VarDecl(kind, ds) => {
                    if *kind == VarKind::Var {
                        for d in ds {
                            if let Some(p) = &d.pat {
                                for n in pat_leaf_names(p) {
                                    env.declare_if_absent(n, Val::Undefined, false);
                                }
                            } else {
                                env.declare_if_absent(d.name.clone(), Val::Undefined, false);
                            }
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

/// every binding leaf name of a pattern (for `var` hoisting)
fn pat_leaf_names(p: &Pat) -> Vec<Rc<str>> {
    // NOTE: Pat here is super::parser::Pat via the glob import
    match p {
        super::parser::Pat::Name(t) => match t {
            super::parser::BindTgt::Name(n) => vec![n.clone()],
            _ => Vec::new(), // Member/Index lvalues are not declarations
        },
        super::parser::Pat::Arr(parts) => {
            let mut out = Vec::new();
            for pp in parts {
                out.extend(pat_part_leaves(pp));
            }
            out
        }
        super::parser::Pat::Obj(parts) => {
            let mut out = Vec::new();
            for op in parts {
                match op {
                    super::parser::ObjPatPart::Prop { part, .. } => {
                        out.extend(pat_part_leaves(part))
                    }
                    super::parser::ObjPatPart::Rest(p) => out.extend(pat_leaf_names(p)),
                }
            }
            out
        }
    }
}

fn pat_part_leaves(pp: &super::parser::PatPart) -> Vec<Rc<str>> {
    match pp {
        super::parser::PatPart::Hole => Vec::new(),
        super::parser::PatPart::Bind(p, _) => pat_leaf_names(p),
        super::parser::PatPart::Rest(p) => pat_leaf_names(p),
    }
}

fn make_closure(_e: &mut JsEngine, env: &Rc<Env>, f: &Rc<FnDef>, is_arrow: bool) -> Val {
    Val::Func(Rc::new(Closure {
        def: f.clone(),
        env: env.clone(),
        this: Val::Undefined,
        is_arrow,
        props: RefCell::new(Vec::new()),
    }))
}

/// Run a function body to completion (used by plain calls and the async
/// run-to-completion path). `FnBody::Bound` routes back into `call`.
pub(crate) fn run_function_body(
    e: &mut JsEngine,
    c: &Rc<Closure>,
    env: &Rc<Env>,
) -> Result<Val, JsError> {
    match &c.def.body {
        FnBody::Block(stmts) => {
            hoist(e, env, stmts);
            match run_block(e, env, stmts)? {
                Flow::Return(v) => Ok(v),
                _ => Ok(Val::Undefined),
            }
        }
        FnBody::Expr(x) => eval_expr(e, env, x),
        FnBody::Bound { target, this, args } => {
            e.call(target.clone(), this.clone(), args.clone())
        }
    }
}

/// Bind call parameters: defaults, destructuring patterns (raw args under
/// hidden `#k` env names whose leaves can never collide) and rest arrays.
pub(crate) fn bind_params(
    e: &mut JsEngine,
    env: &Rc<Env>,
    def: &FnDef,
    args: &[Val],
) -> Result<(), JsError> {
    for (i, p) in def.params.iter().enumerate() {
        if p.rest {
            let rest: Vec<Val> = if i < args.len() {
                args[i..].to_vec()
            } else {
                Vec::new()
            };
            env.declare(p.name.clone(), Val::new_array(rest), false);
            continue;
        }
        let supplied = i < args.len();
        let mut v = if supplied {
            args[i].clone()
        } else if let Some(d) = &p.default {
            eval_expr(e, env, d)?
        } else {
            Val::Undefined
        };
        if supplied && matches!(v, Val::Undefined) {
            if let Some(d) = &p.default {
                v = eval_expr(e, env, d)?; // `f(a, b = 2)` with explicit undefined
            }
        }
        match &p.pat {
            Some(pat) => {
                // bind the raw arg under the hidden name, then destructure
                env.declare(p.name.clone(), v.clone(), false);
                bind_pat(e, env, pat, v, BindKind::Decl(VarKind::Var))?;
            }
            None => env.declare(p.name.clone(), v, false),
        }
    }
    Ok(())
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
    e.cur_span = s.span.start as usize; // budget error names this statement
    e.tick()?;
    match &s.kind {
        StmtKind::Expr(x) => {
            eval_expr(e, env, x)?;
            Ok(Flow::Normal)
        }
        StmtKind::VarDecl(kind, ds) => {
            for d in ds {
                match &d.pat {
                    Some(p) => {
                        let v = match &d.init {
                            Some(x) => eval_expr(e, env, x)?,
                            None => Val::Undefined,
                        };
                        bind_pat(e, env, p, v, BindKind::Decl(*kind))?;
                    }
                    None => {
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
        StmtKind::ForIn { kind, target, obj, body } => {
            let o = eval_expr(e, env, obj)?;
            for k in for_in_keys(&o) {
                let benv = bind_for_iter(e, env, kind.unwrap_or(VarKind::Var), target, Val::Str(k))?;
                match eval_stmt(e, &benv, body)? {
                    Flow::Normal => {}
                    Flow::Break => break,
                    Flow::Continue => continue,
                    r @ Flow::Return(_) => return Ok(r),
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::ForOf { kind, target, iter, body } => {
            let it = eval_expr(e, env, iter)?;
            for v in for_of_items(&it)? {
                let benv = bind_for_iter(e, env, kind.unwrap_or(VarKind::Var), target, v)?;
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
        // destructuring assignment statement form is impossible by
        // construction: the parser converts a leading `[..] =`/`{..} =` into
        // an expression. This variant carries the same op from `for` heads.
        StmtKind::PatAssign { pat, value } => {
            let v = eval_expr(e, env, value)?;
            bind_pat(e, env, pat, v, BindKind::Assign)?;
            Ok(Flow::Normal)
        }
        StmtKind::DefineAcc {
            owner,
            key,
            getter,
            setter,
        } => {
            let o = eval_expr(e, env, owner)?;
            let ob = match &o {
                Val::Obj(ob) => ob.clone(),
                _ => {
                    return Err(JsError::throw(
                        "TypeError: accessors can only be installed on objects",
                    ))
                }
            };
            let getter_v = getter
                .as_ref()
                .map(|f| make_val_closure(e, env, f, false));
            let setter_v = setter
                .as_ref()
                .map(|f| make_val_closure(e, env, f, false));
            obj_set_own(
                &ob,
                key,
                Val::Acc(Rc::new(AccEntry {
                    getter: getter_v,
                    setter: setter_v,
                })),
            );
            Ok(Flow::Normal)
        }
    }
}

/// helper: build a closure Val from an FnDef (accessors etc.)
fn make_val_closure(e: &mut JsEngine, env: &Rc<Env>, f: &Rc<FnDef>, arrow: bool) -> Val {
    let _ = e;
    Val::Func(Rc::new(Closure {
        def: f.clone(),
        env: env.clone(),
        this: Val::Undefined,
        is_arrow: arrow,
        props: RefCell::new(Vec::new()),
    }))
}

/// helper: `Err(JsError::Throw(v))` -> Some(v) without consuming
fn as_err_ref(r: &Result<Flow, JsError>) -> Option<&Val> {
    match r {
        Err(JsError::Throw(v)) => Some(v),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// destructuring binding
// ---------------------------------------------------------------------------

/// Bind a pattern tree against `value`.
/// - `Decl(kind)`: fresh bindings in `env` (var walks the chain like eval)
/// - `Assign`: writes through `env_set`/`set_member` (destructuring assignment)
/// - `Param`: like a `var` declaration in the call env
pub(crate) fn bind_pat(
    e: &mut JsEngine,
    env: &Rc<Env>,
    pat: &Pat,
    value: Val,
    kind: BindKind,
) -> Result<(), JsError> {
    match pat {
        Pat::Name(t) => bind_target(e, env, t, value, kind),
        Pat::Arr(parts) => {
            let items = iter_src(&value)?;
            bind_arr(e, env, parts, &items, kind)
        }
        Pat::Obj(parts) => bind_obj(e, env, parts, &value, kind),
    }
}

fn bind_target(
    e: &mut JsEngine,
    env: &Rc<Env>,
    t: &BindTgt,
    v: Val,
    kind: BindKind,
) -> Result<(), JsError> {
    match t {
        BindTgt::Name(n) => match kind {
            BindKind::Decl(VarKind::Var) | BindKind::Param => e.env_set(env, n, v),
            BindKind::Assign => e.env_set(env, n, v),
            BindKind::Decl(k) => {
                env.declare(n.clone(), v, k == VarKind::Const);
                Ok(())
            }
        },
        // lvalue forms: evaluate base/key in the bind scope, then write
        BindTgt::Member { base, key } => {
            let b = eval_expr(e, env, base)?;
            set_member(e, &b, key, v)
        }
        BindTgt::Index { base, index } => {
            let b = eval_expr(e, env, base)?;
            let k = eval_expr(e, env, index)?.to_string();
            set_member(e, &b, k.as_str(), v)
        }
    }
}

fn bind_arr(
    e: &mut JsEngine,
    env: &Rc<Env>,
    parts: &[PatPart],
    items: &[Val],
    kind: BindKind,
) -> Result<(), JsError> {
    let mut i = 0usize;
    for pp in parts {
        match pp {
            PatPart::Hole => {
                i += 1;
            }
            PatPart::Bind(p, default) => {
                let v = item_or_default(e, items.get(i).cloned(), default, env, items)?;
                bind_pat(e, env, p, v, kind)?;
                i += 1;
            }
            PatPart::Rest(p) => {
                // rest takes everything from the cursor on
                let rest: Vec<Val> = items[i.min(items.len())..].to_vec();
                bind_pat(e, env, p, Val::new_array(rest), kind)?;
                i = items.len();
            }
        }
    }
    Ok(())
}

/// value for a pattern element: missing/undefined -> default expr (which may
/// itself reference earlier bound names; evaluated in `env`)
fn item_or_default(
    e: &mut JsEngine,
    v: Option<Val>,
    default: &Option<Expr>,
    env: &Rc<Env>,
    _items: &[Val],
) -> Result<Val, JsError> {
    match v {
        Some(v) if !matches!(v, Val::Undefined) => Ok(v),
        _ => match default {
            Some(d) => eval_expr(e, env, d),
            None => Ok(Val::Undefined),
        },
    }
}

fn bind_obj(
    e: &mut JsEngine,
    env: &Rc<Env>,
    parts: &[ObjPatPart],
    value: &Val,
    kind: BindKind,
) -> Result<(), JsError> {
    // track keys already extracted so `...rest` doesn't repeat them
    let mut used: Vec<Rc<str>> = Vec::new();
    for op in parts {
        match op {
            ObjPatPart::Prop { key, part } => {
                let v = get_member(e, value, key)?;
                match part {
                    PatPart::Bind(p, default) => {
                        let v = if matches!(v, Val::Undefined) {
                            match default {
                                Some(d) => eval_expr(e, env, d)?,
                                None => Val::Undefined,
                            }
                        } else {
                            v
                        };
                        bind_pat(e, env, p, v, kind)?;
                    }
                    PatPart::Rest(_) => {
                        return Err(JsError::throw(
                            "TypeError: `{ key: ...rest }` is not supported ({...rest} only)",
                        ))
                    }
                    PatPart::Hole => {}
                }
                used.push(key.clone());
            }
            ObjPatPart::Rest(p) => {
                // remaining own properties as an object
                let o = Val::new_object();
                let orc = obj_of(&o);
                if let Val::Obj(src) = value {
                    let entries: Vec<(Rc<str>, Val)> = {
                        let b = src.borrow();
                        b.props.clone()
                    };
                    for (k, v) in entries {
                        if used.iter().any(|u| *u == k) {
                            continue;
                        }
                        obj_set_own(&orc, &k, v.clone());
                    }
                }
                bind_pat(e, env, p, o, kind)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// loop binding targets (for-in / for-of)
// ---------------------------------------------------------------------------

/// bind one iteration: fresh env for let/const (closures capture per-iter),
/// chain write for var; destructuring targets supported on both forms.
fn bind_for_iter(
    e: &mut JsEngine,
    env: &Rc<Env>,
    kind: VarKind,
    target: &ForTarget,
    v: Val,
) -> Result<Rc<Env>, JsError> {
    match target {
        ForTarget::Name(n) => match kind {
            VarKind::Let | VarKind::Const => {
                let benv = Env::child(env);
                benv.declare(n.clone(), v, kind == VarKind::Const);
                Ok(benv)
            }
            VarKind::Var => {
                e.env_set(env, n, v)?;
                Ok(env.clone())
            }
        },
        ForTarget::Pat(p) => match kind {
            VarKind::Let | VarKind::Const => {
                let benv = Env::child(env);
                bind_pat(e, &benv, p, v, BindKind::Decl(kind))?;
                Ok(benv)
            }
            VarKind::Var => {
                bind_pat(e, env, p, v, BindKind::Decl(VarKind::Var))?;
                Ok(env.clone())
            }
        },
    }
}

/// shortcut: for-in enumerates OWN props only (prototype-chain keys and
/// insertion-order vs source-order quirks are ignored), and primitives
/// enumerate nothing (real JS boxes them).
pub(crate) fn for_in_keys(v: &Val) -> Vec<Rc<str>> {
    match v {
        Val::Obj(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => (0..items.len())
                    .map(|i| Rc::from(i.to_string()))
                    .collect(),
                _ => b.props.iter().map(|(k, _)| k.clone()).collect(),
            }
        }
        _ => Vec::new(),
    }
}

/// shortcut: no iterator protocol â€” arrays and strings only; anything else
/// throws a clear TypeError (custom iterables, Maps, Sets are later waves).
pub(crate) fn for_of_items(v: &Val) -> Result<Vec<Val>, JsError> {
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
    e.tick()?;
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
            binary_op(e, *op, &lv, &rv)
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
                v = binary_op(e, *bop, &cur, &v)?;
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
            // `new Promise(executor)` needs to run its executor arg; handled
            // generically: natives get the fresh object as `this`
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
                    ArrayElem::Item(x) => vals.push(eval_expr(e, env, x)?),
                    ArrayElem::Hole => vals.push(Val::Undefined), // hole -> undefined (dense shortcut)
                    ArrayElem::Spread(sp) => {
                        // sparse: here strings expand to char-code arrays
                        vals.extend(iter_src(&eval_expr(e, env, sp)?)?);
                    }
                }
            }
            Ok(Val::new_array(vals))
        }
        ExprKind::Object(props) => {
            let o = Val::new_object();
            let orc = obj_of(&o);
            for p in props {
                match p {
                    ObjElem::Prop(k, v) => {
                        let val = eval_expr(e, env, v)?;
                        obj_set_own(&orc, k, val);
                    }
                    // `{...src}` copies own enumerable props (arrays copy
                    // their dense items as index keys; accessors invoke their
                    // getters â€” matching real spread semantics closely enough
                    // for the honest scope of this wave)
                    ObjElem::Spread(sp) => {
                        let src = eval_expr(e, env, sp)?;
                        if let Val::Obj(s) = &src {
                            let entries: Vec<(Rc<str>, Val)> = {
                                let b = s.borrow();
                                match &b.kind {
                                    ObjKind::Array(items) => {
                                        let mut v = Vec::with_capacity(items.len());
                                        for (i, item) in items.iter().enumerate() {
                                            v.push((
                                                Rc::from(i.to_string()),
                                                item.clone(),
                                            ));
                                        }
                                        v
                                    }
                                    _ => b.props.clone(),
                                }
                            };
                            for (k, v) in entries {
                                obj_set_own(&orc, &k, v);
                            }
                        }
                    }
                }
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
        ExprKind::Regex { pattern, flags } => {
            let compiled = e.regex_from_literal(pattern.clone(), flags.clone())?;
            Ok(Val::Obj(Rc::new(RefCell::new(Obj {
                kind: ObjKind::Regex(ReInst {
                    re: compiled,
                    last_index: std::cell::Cell::new(0.0),
                }),
                props: Vec::new(),
                proto: None,
            }))))
        }
        ExprKind::Yield(_) => Err(JsError::throw(
            "SyntaxError: yield is only valid inside generator functions",
        )),
        ExprKind::Await(inner) => {
            let v = eval_expr(e, env, inner)?;
            await_value(e, v)
        }
        ExprKind::PatAssign { pat, value } => {
            // destructuring assignment `([a, b] = pair)`
            let v = eval_expr(e, env, value)?;
            bind_pat(e, env, pat, v.clone(), BindKind::Assign)?;
            Ok(v)
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

/// `await v` under run-to-completion semantics:
/// - already-settled engine promise: unwrap (rejection throws into the frame)
/// - pending engine promise: pump the microtask queue (bounded) until it
///   settles; the honest exception is promises that only timers/macrotasks
///   can settle â€” those error out (see `await_promise`)
/// - non-promise values pass through unchanged
fn await_value(e: &mut JsEngine, v: Val) -> Result<Val, JsError> {
    let p = match promise_of(&v) {
        Some(p) => p,
        None => return Ok(v), // `await` of a non-promise is the value itself
    };
    match e.await_promise(&p)? {
        Ok(v) => Ok(v),
        Err(reason) => Err(JsError::Throw(reason)),
    }
}

/// spread sources: arrays expand element-wise, strings to chars; anything
/// else is an honest error (no iterator protocol in this wave)
pub(crate) fn iter_src(v: &Val) -> Result<Vec<Val>, JsError> {
    for_of_items(v)
}

fn own(v: &Val) -> &Rc<RefCell<Obj>> {
    match v {
        Val::Obj(o) => o,
        _ => unreachable!("own() on non-object"),
    }
}

/// call arguments with `...spread` expansion in source order
fn eval_args(e: &mut JsEngine, env: &Rc<Env>, args: &[CallArg]) -> Result<Vec<Val>, JsError> {
    let mut out: Vec<Val> = Vec::with_capacity(args.len());
    for a in args {
        match a {
            CallArg::Arg(x) => out.push(eval_expr(e, env, x)?),
            CallArg::Spread(sp) => {
                let src = eval_expr(e, env, sp)?;
                out.extend(iter_src(&src)?);
            }
        }
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
            set_member(e, &b, key, v)
        }
        ExprKind::Index { base, index } => {
            let b = eval_expr(e, env, base)?;
            let k = eval_expr(e, env, index)?.to_string();
            set_member(e, &b, &k, v)
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
        props: vec![(Rc::from("constructor"), Val::Func(c.clone()))],
        proto: None,
    }));
    c.props
        .borrow_mut()
        .push((Rc::from("prototype"), Val::Obj(proto.clone())));
    proto
}

fn binary_op(e: &JsEngine, op: BinOp, l: &Val, r: &Val) -> Result<Val, JsError> {
    use BinOp::*;
    Ok(match op {
        Add => {
            // string concat if either side is (or stringifies to) a string
            let strish = |v: &Val| {
                matches!(
                    v,
                    Val::Str(_) | Val::Obj(_) | Val::Func(_) | Val::Native(_) | Val::Method(_) | Val::Acc(_)
                )
            };
            if strish(l) || strish(r) {
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
            // or a plain array (checked against the shared array prototype)
            Val::Native(_) => Val::Bool(match l {
                Val::Obj(o) => {
                    matches!(o.borrow().kind, ObjKind::Date(_))
                        || e
                            .array_proto
                            .as_ref()
                            .map_or(false, |ap| {
                                let mut cur = o.borrow().proto.clone();
                                let mut found = false;
                                for _ in 0..100 {
                                    match cur {
                                        None => break,
                                        Some(c) => {
                                            if Rc::ptr_eq(&c, ap) {
                                                found = true;
                                                break;
                                            }
                                            cur = c.borrow().proto.clone();
                                        }
                                    }
                                }
                                found
                            })
                }
                _ => false,
            }),
            // native constructor "functions" are plain objects holding a
            // `prototype` prop (e.g. the global `Array` binding); answer by
            // walking the RHS's `prototype` up the LHS proto chain (capped)
            Val::Obj(rhs) => {
                let proto = rhs
                    .borrow()
                    .props
                    .iter()
                    .find(|(k, _)| k.as_ref() == "prototype")
                    .and_then(|(_, v)| match v {
                        Val::Obj(p) => Some(p.clone()),
                        _ => None,
                    });
                let Some(p) = proto else {
                    return Err(JsError::throw(
                        "TypeError: RHS of instanceof is not a function",
                    ));
                };
                let yes = match l {
                    Val::Obj(lhs) => {
                        // shortcut: array instances carry their methods
                        // per-object (proto is None), so `x instanceof
                        // Array` answers on the kind tag when the RHS is
                        // exactly the Array constructor
                        let is_arr = matches!(lhs.borrow().kind, ObjKind::Array(_))
                            && e
                                .array_proto
                                .as_ref()
                                .map_or(false, |ap| Rc::ptr_eq(&p, ap));
                        if is_arr {
                            true
                        } else {
                            let mut cur = lhs.borrow().proto.clone();
                            let mut found = false;
                            for _ in 0..100 {
                                match cur {
                                    None => break,
                                    Some(c) => {
                                        if Rc::ptr_eq(&c, &p) {
                                            found = true;
                                            break;
                                        }
                                        cur = c.borrow().proto.clone();
                                    }
                                }
                            }
                            found
                        }
                    }
                    _ => false,
                };
                Val::Bool(yes)
            }
            _ => {
                return Err(JsError::throw("TypeError: RHS of instanceof is not a function"))
            }
        },
    })
}

fn relational(op: BinOp, l: &Val, r: &Val) -> bool {
    // both strings compare lexicographically (UTF-8 byte order â‰ˆ JS's
    // UTF-16 code-unit order for BMP text; astral pairs differ â€” shortcut)
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

/// regex object if present at args[i] (regex-aware string methods)
fn arg_regex(e: &JsEngine, args: &[Val], i: usize) -> Option<Rc<regex::Regex>> {
    match args.get(i) {
        Some(Val::Obj(o)) => match &o.borrow().kind {
            ObjKind::Regex(ri) => Some(ri.re.clone()),
            _ => None,
        },
        _ => {
            let _ = e;
            None
        }
    }
}

/// chars of a string value (the working unit for regex scans)
fn chars_of(s: &str) -> Vec<char> {
    s.chars().collect()
}

/// build a JS match array for a MatchVal on the given input
fn match_obj(
    e: &mut JsEngine,
    input: &[char],
    m: &regex::MatchVal,
    with_index: bool,
) -> Result<Val, JsError> {
    let n_groups = m.caps.len() / 2 - 1;
    let mut items: Vec<Val> = Vec::with_capacity(n_groups + 1);
    items.push(Val::from(input[m.start..m.end].iter().collect::<String>()));
    for g in 1..=n_groups {
        items.push(match m.caps.get(2 * g + 1).copied().flatten() {
            Some((s, en)) => Val::from(input[s..en].iter().collect::<String>()),
            None => Val::Undefined,
        });
    }
    let arr = Val::new_array(items);
    if with_index {
        let ab = obj_of(&arr);
        obj_set_own(&ab, "index", Val::Num(m.start as f64));
        // `input` as the original string
        let s: String = input.iter().collect();
        obj_set_own(&ab, "input", Val::from(s));
    }
    let _ = e;
    Ok(arr)
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
            if let Some(re) = arg_regex(e, args, 0) {
                return Ok(regex_split(&re, &chars_of(&s))?);
            }
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
            if let Some(re) = arg_regex(e, args, 0) {
                // regex replace: `g` replaces every match, else the first
                return Ok(regex_replace(e, &re, &s, args.get(1).cloned())?);
            }
            let from = arg_str(args, 0);
            let to = arg_str(args, 1);
            if from.is_empty() {
                return Ok(Val::Str(Rc::from(format!("{}{}", to, s))));
            }
            // shortcut: string pattern; function replacers apply to regex
            // only (honest scope)
            Ok(match s.find(from.as_ref()) {
                Some(i) => Val::Str(Rc::from(
                    format!("{}{}{}", &s[..i], to, &s[i + from.len()..]).as_str(),
                )),
                None => Val::Str(s),
            })
        }
        StrMatch => {
            let s = need_str(this)?;
            let re = match arg_regex(e, args, 0) {
                Some(r) => r,
                None => {
                    return Ok(Val::Null) // non-regex arg: /converted/ out of scope; honest null
                }
            };
            let chars = chars_of(&s);
            if re.global {
                let mut out = Vec::new();
                let mut pos = 0usize;
                // scan all matches; zero-length matches advance to avoid
                // infinite loops (spec behavior)
                while pos <= chars.len() {
                    match scan_first(&re, &chars, pos) {
                        Some(rr) => {
                            out.push(Val::from(
                                chars[rr.start..rr.end].iter().collect::<String>(),
                            ));
                            pos = if rr.end > rr.start { rr.end } else { rr.end + 1 };
                        }
                        None => break,
                    }
                }
                Ok(if out.is_empty() { Val::Null } else { Val::new_array(out) })
            } else {
                match re.find(&chars, 0) {
                    Some(mm) => Ok(match_obj(e, &chars, &mm, true)?),
                    None => Ok(Val::Null),
                }
            }
        }
        StrMatchAll => {
            let s = need_str(this)?;
            let re = match arg_regex(e, args, 0) {
                Some(r) => r,
                None => return Ok(Val::Undefined),
            };
            let chars = chars_of(&s);
            let mut out = Vec::new();
            let mut pos = 0usize;
            while pos <= chars.len() {
                match scan_first(&re, &chars, pos) {
                    Some(rr) => {
                        out.push(match_obj(e, &chars, &rr, true)?);
                        pos = if rr.end > rr.start { rr.end } else { rr.end + 1 };
                    }
                    None => break,
                }
            }
            // honest shortcut: an array instead of the spec's iterator object
            Ok(Val::new_array(out))
        }
        StrSearch => {
            let s = need_str(this)?;
            let re = match arg_regex(e, args, 0) {
                Some(r) => r,
                None => return Ok(Val::Num(-1.0)),
            };
            let chars = chars_of(&s);
            Ok(Val::Num(match re.find(&chars, 0) {
                Some(mm) => mm.start as f64,
                None => -1.0,
            }))
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
        PromiseThen | PromiseCatch | PromiseFinally => {
            let o = promise_of(this).ok_or_else(|| {
                JsError::throw("TypeError: then/catch/finally on non-Promise")
            })?;
            match m {
                PromiseCatch => {
                    let h = args.first().cloned().unwrap_or(Val::Undefined);
                    promise_then(e, &o, Val::Undefined, h)
                }
                PromiseFinally => {
                    let cb = args.first().cloned().unwrap_or(Val::Undefined);
                    finally_then(e, &o, cb)
                }
                _ => {
                    let on_ok = args.first().cloned().unwrap_or(Val::Undefined);
                    let on_err = args.get(1).cloned().unwrap_or(Val::Undefined);
                    promise_then(e, &o, on_ok, on_err)
                }
            }
        }

        // -- generator methods ----------------------------------------------
        GenNext => {
            let arg = args.first().cloned().unwrap_or(Val::Undefined);
            gen::gen_next(e, this, arg)
        }
        GenReturn => {
            let arg = args.first().cloned().unwrap_or(Val::Undefined);
            gen::gen_return(this, arg)
        }

        // -- regex methods (this = regex object) ----------------------------
        RegexTest | RegexExec => {
            let (re, start) = match this {
                Val::Obj(o) => {
                    let b = o.borrow();
                    let (re, start) = match &b.kind {
                        ObjKind::Regex(ri) => {
                            // `y` scans exactly at lastIndex; `g` scans from
                            // lastIndex onward; neither flag: scan from 0 (JS
                            // leaves lastIndex alone for both)
                            let li = ri.last_index.get();
                            let start = if ri.re.global || ri.re.sticky {
                                if li.is_finite() && li >= 0.0 {
                                    li as usize
                                } else {
                                    0
                                }
                            } else {
                                0
                            };
                            (ri.re.clone(), start)
                        }
                        _ => return Err(JsError::throw("TypeError: regex method on non-regex")),
                    };
                    drop(b);
                    (re, start)
                }
                _ => return Err(JsError::throw("TypeError: regex method on non-regex")),
            };
            let s = arg_str(args, 0);
            let chars = chars_of(&s);
            match re.find(&chars, start) {
                Some(mm) => {
                    if re.global || re.sticky {
                        if let Val::Obj(o) = this {
                            if let ObjKind::Regex(ri) = &o.borrow_mut().kind {
                                ri.last_index.set(mm.end as f64);
                            }
                        }
                    }
                    if m == RegexTest {
                        Ok(Val::Bool(true))
                    } else {
                        Ok(match_obj(e, &chars, &mm, true)?)
                    }
                }
                None => {
                    if re.global || re.sticky {
                        // failed global/sticky exec resets lastIndex
                        if let Val::Obj(o) = this {
                            if let ObjKind::Regex(ri) = &o.borrow_mut().kind {
                                ri.last_index.set(0.0);
                            }
                        }
                    }
                    if m == RegexTest {
                        Ok(Val::Bool(false))
                    } else {
                        Ok(Val::Null)
                    }
                }
            }
        }
        RegexToString => match this {
            Val::Obj(o) => {
                let b = o.borrow();
                if let ObjKind::Regex(ri) = &b.kind {
                    Ok(Val::from(format!("/{}/{}", ri.re.src, ri.re.flags)))
                } else {
                    Err(JsError::throw("TypeError: toString on non-regex"))
                }
            }
            _ => Err(JsError::throw("TypeError: toString on non-regex")),
        },

        // -- function methods ----------------------------------------------
        FnCall | FnApply | FnBind => {
            let target = this.clone();
            if !matches!(
                target,
                Val::Func(_) | Val::Native(_) | Val::Method(_)
            ) {
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
                        is_async: false,
                        is_generator: false,
                        has_yield: false,
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

// ---------------------------------------------------------------------------
// regex-aware string helpers
// ---------------------------------------------------------------------------

/// first match at or after `from` (used by scanning loops for /g semantics)
fn scan_first(re: &regex::Regex, chars: &[char], from: usize) -> Option<regex::MatchVal> {
    // non-global scans are the same operation; sticky is exec/test-only
    re.find(chars, from)
}

/// `str.split(re)` with capture-group splicing (JS semantics)
fn regex_split(re: &regex::Regex, chars: &[char]) -> Result<Val, JsError> {
    let mut parts: Vec<Val> = Vec::new();
    let mut pos = 0usize; // scan cursor
    let mut last_end = 0usize; // end of previous match
    loop {
        if pos > chars.len() {
            break;
        }
        match re.find(chars, pos) {
            None => break,
            Some(m) => {
                parts.push(Val::from(
                    chars[last_end..m.start].iter().collect::<String>(),
                ));
                // captures splice in between
                let gs = m.caps.len() / 2 - 1;
                for g in 1..=gs {
                    parts.push(match m.caps.get(2 * g + 1).copied().flatten() {
                        Some((s, en)) => Val::from(chars[s..en].iter().collect::<String>()),
                        None => Val::Undefined,
                    });
                }
                last_end = m.end;
                pos = if m.end > m.start { m.end } else { m.end + 1 };
            }
        }
    }
    parts.push(Val::from(
        chars[last_end.min(chars.len())..].iter().collect::<String>(),
    ));
    Ok(Val::new_array(parts))
}

/// `str.replace(re, repl)` â€” string-or-function replacement with $-patterns
fn regex_replace(
    e: &mut JsEngine,
    re: &regex::Regex,
    s: &str,
    repl: Option<Val>,
) -> Result<Val, JsError> {
    let chars = chars_of(s);
    let mut out = String::new();
    let mut pos = 0usize;
    let mut last_end = 0usize;
    let mut replaced_any = false;
    loop {
        if pos > chars.len() {
            break;
        }
        match re.find(&chars, pos) {
            None => break,
            Some(m) => {
                out.push_str(&chars[last_end..m.start].iter().collect::<String>());
                out.push_str(&apply_repl(e, &chars, &m, repl.as_ref())?);
                last_end = m.end;
                pos = if m.end > m.start { m.end } else { m.end + 1 };
                replaced_any = true;
                if !re.global {
                    break; // one replacement only
                }
            }
        }
    }
    out.push_str(&chars[last_end.min(chars.len())..].iter().collect::<String>());
    let _ = replaced_any;
    Ok(Val::from(out))
}

/// string $-pattern expansion or function replacer invocation
fn apply_repl(
    e: &mut JsEngine,
    chars: &[char],
    m: &regex::MatchVal,
    repl: Option<&Val>,
) -> Result<String, JsError> {
    let matched: String = chars[m.start..m.end].iter().collect();
    match repl {
        // function replacer: (match, p1..p9, offset, input)
        Some(v) if is_callable(v) => {
            let mut aargs = vec![Val::from(matched.clone())];
            let gs = m.caps.len() / 2 - 1;
            for g in 1..=gs {
                aargs.push(match m.caps.get(2 * g + 1).copied().flatten() {
                    Some((s, en)) => Val::from(chars[s..en].iter().collect::<String>()),
                    None => Val::Undefined,
                });
            }
            aargs.push(Val::Num(m.start as f64));
            aargs.push(Val::from(chars.iter().collect::<String>()));
            let r = e.call(v.clone(), Val::Undefined, aargs)?;
            Ok(r.to_string())
        }
        repl => {
            let pattern = match repl {
                Some(v) => v.to_string(),
                None => String::new(),
            };
            Ok(subst_dollars(&pattern, chars, m, &matched))
        }
    }
}

/// `$&`/$1../$`/$'/$$ substitution (named groups unsupported honestly)
fn subst_dollars(pattern: &str, chars: &[char], m: &regex::MatchVal, matched: &str) -> String {
    let gs = m.caps.len() / 2 - 1;
    let b: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != '$' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            Some('$') => {
                out.push('$');
                i += 2;
            }
            Some('&') => {
                out.push_str(matched);
                i += 2;
            }
            Some('`') => {
                out.push_str(&chars[..m.start].iter().collect::<String>());
                i += 2;
            }
            Some('\'') => {
                out.push_str(&chars[m.end..].iter().collect::<String>());
                i += 2;
            }
            Some(c) if c.is_ascii_digit() => {
                let mut num = c.to_digit(10).unwrap() as usize;
                // two-digit group when valid (JS semantics)
                if num <= gs {
                    if let Some(d2) = b.get(i + 2).and_then(|c2| c2.to_digit(10)) {
                        let cand = num * 10 + d2 as usize;
                        if cand <= gs {
                            num = cand;
                            i += 1;
                        }
                    }
                    if num >= 1 && num <= gs {
                        if let Some((s, en)) = m.caps.get(2 * num + 1).copied().flatten() {
                            out.push_str(&chars[s..en].iter().collect::<String>());
                        }
                    }
                } else {
                    // `$10` referencing only 2 groups: JS emits "$1" + "0"
                    if num == 0 {
                        out.push('$'); // literal $0 fallback: e.g. (d1..9 only)
                    }
                    out.push(*c);
                }
                i += 2;
            }
            Some(c) => {
                out.push('$');
                out.push(*c);
                i += 2;
            }
            None => {
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

fn is_callable(v: &Val) -> bool {
    matches!(v, Val::Func(_) | Val::Native(_) | Val::Method(_))
}

/// `.finally(cb)` â€” reaction that runs cb then propagates the outcome
fn finally_then(e: &mut JsEngine, o: &Rc<RefCell<Obj>>, cb: Val) -> Result<Val, JsError> {
    let ret = new_promise_obj();
    let re = Reaction {
        kind: ReactKind::Finally { cb },
        ret: Some(ret.clone()),
    };
    if let Some(out) = promise_outcome(o) {
        e.microtasks.push(Job::Reaction(re, out));
    } else {
        let mut b = o.borrow_mut();
        if let ObjKind::Promise(st) = &mut b.kind {
            st.reactions.push(re);
        }
    }
    Ok(Val::Obj(ret))
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
