//! Tree-walking evaluator: statements, expressions, operators, calls.
//!
//! Execution flow through statements is a small `Flow` enum (break/continue/
//! return); script `throw` travels as `Err(JsError::Throw(..))` so `try`
//! intercepts it naturally and error paths never allocate flow objects.
//! This wave adds: destructuring binding (`Pat` trees evaluated directly,
//! no temporary desugar), spread/rest argument expansion, regex literals
//! (compiled once per (pattern, flags) on the engine), `await` (run-to-
//! completion microtask pump — see `await_value`), and generator bodies
//! walking through the resumable VM in `gen.rs`.

use super::gen;
use super::parser::{
    ArrayElem, BinOp, BindTgt, CallArg, ClassKey, Declarator, Expr, ExprKind, FnBody, FnDef,
    ForTarget, LogOp, ObjElem, ObjPatPart, Pat, PatPart, Stmt, StmtKind, TplSeg, UnaryOp, VarKind,
};
use super::regex;
use super::*;
use std::cell::RefCell;

#[derive(Debug, Clone)]
pub(crate) enum Flow {
    Normal,
    /// `break` / `break label` (None = unlabeled)
    Break(Option<Rc<str>>),
    /// `continue` / `continue label`
    Continue(Option<Rc<str>>),
    Return(Val),
}

/// How a loop reacts to a flow value, given the label the loop was given
/// (`outer: for (...) { ... }`).
enum LoopFlow {
    Next,
    Stop,
    Out(Flow),
}

fn loop_flow(f: Flow, label: &Option<Rc<str>>) -> LoopFlow {
    match f {
        Flow::Normal => LoopFlow::Next,
        Flow::Break(None) => LoopFlow::Stop,
        Flow::Break(Some(l)) if label.as_deref() == Some(l.as_ref()) => LoopFlow::Stop,
        Flow::Continue(None) => LoopFlow::Next,
        Flow::Continue(Some(l)) if label.as_deref() == Some(l.as_ref()) => LoopFlow::Next,
        other => LoopFlow::Out(other),
    }
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
/// a function/program body — a naive AST walk (shortcut: no TDZ for `let`,
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
                // `export <decl>` still declares its name
                StmtKind::Export { inner: Some(st), .. } => {
                    walk(e, env, std::slice::from_ref(st))
                }
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
                StmtKind::ForIn {
                    kind,
                    target,
                    body,
                    ..
                }
                | StmtKind::ForOf {
                    kind,
                    target,
                    body,
                    ..
                } => {
                    // `for (var x in o)`: the loop binding is a
                    // function-scoped `var`. Without this the assignment
                    // resolves to an outer binding of the same name (the
                    // loop leaked into the enclosing closure).
                    if *kind == Some(VarKind::Var) {
                        match target {
                            ForTarget::Name(n) => {
                                env.declare_if_absent(n.clone(), Val::Undefined, false);
                            }
                            ForTarget::Pat(p) => {
                                for n in pat_leaf_names(p) {
                                    env.declare_if_absent(n, Val::Undefined, false);
                                }
                            }
                        }
                    }
                    walk(e, env, std::slice::from_ref(body))
                }
                StmtKind::Switch { cases, .. } => {
                    for c in cases {
                        walk(e, env, &c.body);
                    }
                }
                StmtKind::With { body, .. } => walk(e, env, std::slice::from_ref(body)),
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
                    super::parser::ObjPatPart::Prop { part, .. }
                    | super::parser::ObjPatPart::ComputedProp { part, .. } => {
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
    Val::Func(new_closure(f.clone(), env.clone(), None, is_arrow))
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
    // a throw that a previous statement caught must not label this one
    e.err_located = false;
    e.tick()?;
    let r = eval_stmt_inner(e, env, s);
    if let Err(err) = &r {
        e.note_error(err);
    }
    r
}

fn eval_stmt_inner(e: &mut JsEngine, env: &Rc<Env>, s: &Stmt) -> Result<Flow, JsError> {
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
        StmtKind::With { obj, body } => {
            let o = eval_expr(e, env, obj)?;
            let o = match o {
                Val::Obj(o) => o,
                _ => {
                    return Err(JsError::throw(
                        "TypeError: `with` requires an object",
                    ))
                }
            };
            let wenv = Env::with_obj(env, o);
            eval_stmt(e, &wenv, body)
        }
        StmtKind::List(stmts) => run_block(e, env, stmts),
        // A module loader consumes these before evaluation; reaching one
        // means a classic script (or engine B) declared an import/export.
        StmtKind::Import { .. } => Err(JsError::throw(
            "SyntaxError: `import` is only valid in a module script",
        )),
        StmtKind::Export { .. } => Err(JsError::throw(
            "SyntaxError: `export` is only valid in a module script",
        )),
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
            let label = e.pending_label.take();
            while eval_expr(e, env, test)?.to_bool() {
                match loop_flow(eval_stmt(e, env, body)?, &label) {
                    LoopFlow::Next => {}
                    LoopFlow::Stop => break,
                    LoopFlow::Out(f) => return Ok(f),
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::DoWhile { body, test } => {
            let label = e.pending_label.take();
            loop {
                match loop_flow(eval_stmt(e, env, body)?, &label) {
                    LoopFlow::Next => {}
                    LoopFlow::Stop => break,
                    LoopFlow::Out(f) => return Ok(f),
                }
                if !eval_expr(e, env, test)?.to_bool() {
                    break;
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::For { init, cond, upd, body } => {
            let label = e.pending_label.take();
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
                match loop_flow(eval_stmt(e, &lenv, body)?, &label) {
                    LoopFlow::Next => {}
                    LoopFlow::Stop => break,
                    LoopFlow::Out(f) => return Ok(f),
                }
                if let Some(u) = upd {
                    eval_expr(e, &lenv, u)?;
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::ForIn { kind, target, obj, body } => {
            let label = e.pending_label.take();
            let o = eval_expr(e, env, obj)?;
            for k in for_in_keys(&o) {
                let benv = bind_for_iter(e, env, kind.unwrap_or(VarKind::Var), target, Val::Str(k))?;
                match loop_flow(eval_stmt(e, &benv, body)?, &label) {
                    LoopFlow::Next => {}
                    LoopFlow::Stop => break,
                    LoopFlow::Out(f) => return Ok(f),
                }
            }
            Ok(Flow::Normal)
        }
        StmtKind::ForOf { kind, target, iter, body } => {
            let label = e.pending_label.take();
            let iterable = eval_expr(e, env, iter)?;
            // lazy `@@iterator` loop: one `.next()` per element, so early
            // `break` stops pulling values from an infinite generator
            let it = get_iterator(e, &iterable)?;
            let next = get_member(e, &it, "next")?;
            loop {
                let step = e.call(next.clone(), it.clone(), Vec::new())?;
                if get_member(e, &step, "done")?.to_bool() {
                    break;
                }
                let v = get_member(e, &step, "value")?;
                let benv = bind_for_iter(e, env, kind.unwrap_or(VarKind::Var), target, v)?;
                match loop_flow(eval_stmt(e, &benv, body)?, &label) {
                    LoopFlow::Next => {}
                    LoopFlow::Stop => break,
                    LoopFlow::Out(f) => return Ok(f),
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
                        Flow::Break(None) => return Ok(Flow::Normal),
                        r @ Flow::Break(Some(_)) => return Ok(r), // `break label`
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
            let caught = match &result {
                Err(JsError::Throw(v)) => Some(caught_value(e, v.clone())),
                _ => None,
            };
            if let (Some(v), Some(cat)) = (caught, catch) {
                // the caught error is gone; the next throw in this turn must
                // be able to record its own location
                e.err_located = false;
                let cenv = Env::child(env);
                match &cat.pat {
                    Some(p) => bind_pat(e, &cenv, p, v, BindKind::Decl(VarKind::Let))?,
                    None => cenv.declare(cat.name.clone(), v, false),
                }
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
        StmtKind::Break(label) => Ok(Flow::Break(label.clone())),
        StmtKind::Continue(label) => Ok(Flow::Continue(label.clone())),
        // `label: stmt`: a labeled loop takes the label for `continue label`;
        // any other statement only observes `break label`
        StmtKind::Label { name, body } => {
            let is_loop = matches!(
                body.kind,
                StmtKind::While { .. }
                    | StmtKind::DoWhile { .. }
                    | StmtKind::For { .. }
                    | StmtKind::ForIn { .. }
                    | StmtKind::ForOf { .. }
            );
            let r = if is_loop {
                let prev = e.pending_label.replace(name.clone());
                let r = eval_stmt(e, env, body);
                e.pending_label = prev;
                r?
            } else {
                eval_stmt(e, env, body)?
            };
            match r {
                Flow::Break(Some(l)) if l.as_ref() == name.as_ref() => Ok(Flow::Normal),
                other => Ok(other),
            }
        }
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
            let key = match &**key {
                ClassKey::Name(k) => k.clone(),
                // computed accessor key: ToPropertyKey, which this engine
                // models with `to_string` (symbols stringify to their stand-in
                // name, matching SYMBOL_ITERATOR).
                ClassKey::Computed(x) => Rc::from(eval_expr(e, env, x)?.to_string().as_str()),
            };
            let getter_v = getter
                .as_ref()
                .map(|f| make_val_closure(e, env, f, false));
            let setter_v = setter
                .as_ref()
                .map(|f| make_val_closure(e, env, f, false));
            let acc = Val::Acc(Rc::new(AccEntry {
                getter: getter_v,
                setter: setter_v,
            }));
            match &o {
                Val::Obj(ob) => obj_set_own(ob, &key, acc),
                // static accessor: installed on the class function object
                Val::Func(c) => c.props.borrow_mut().push((key.clone(), acc)),
                _ => {
                    return Err(JsError::throw(
                        "TypeError: accessors can only be installed on objects",
                    ))
                }
            }
            // class accessors are not own enumerable properties
            match &o {
                Val::Obj(ob) => obj_hide(ob, &key),
                Val::Func(c) => clo_hide(c, &key),
                _ => {}
            }
            Ok(Flow::Normal)
        }
        StmtKind::DefineMethod { owner, key, func } => {
            let o = eval_expr(e, env, owner)?;
            let key = match &**key {
                ClassKey::Name(k) => k.clone(),
                ClassKey::Computed(x) => Rc::from(eval_expr(e, env, x)?.to_string().as_str()),
            };
            let f = make_val_closure(e, env, func, false);
            match &o {
                Val::Obj(ob) => {
                    obj_set_own(ob, &key, f);
                    obj_hide(ob, &key);
                }
                Val::Func(c) => {
                    c.props.borrow_mut().push((key.clone(), f));
                    clo_hide(c, &key);
                }
                _ => {
                    return Err(JsError::throw(
                        "TypeError: methods can only be installed on objects",
                    ))
                }
            }
            Ok(Flow::Normal)
        }
    }
}

/// helper: build a closure Val from an FnDef (accessors etc.)
fn make_val_closure(e: &mut JsEngine, env: &Rc<Env>, f: &Rc<FnDef>, arrow: bool) -> Val {
    let _ = e;
    Val::Func(new_closure(f.clone(), env.clone(), None, arrow))
}

/// The value script sees when an engine-raised error is caught. Engine errors
/// travel as `Val::Str("TypeError: message")` (see `JsError::throw`), but real
/// code inspects `e.name`, `e.message` and `e.constructor`, and test262's
/// `assert.throws` compares `thrown.constructor` with the expected class, so a
/// caught engine error becomes a real Error object of the right class.
/// User-thrown strings are left alone unless they look exactly like an engine
/// message (no realistic page throws `"TypeError: ..."` as a value).
/// Direct `eval(source)` against the calling scope. The source runs in the
/// caller's environment (so `var`/function declarations land there) and its
/// completion value is the result. A non-string argument is returned
/// unchanged, per spec.
pub(crate) fn eval_source(e: &mut JsEngine, env: &Rc<Env>, arg: Val) -> Result<Val, JsError> {
    let Val::Str(src) = &arg else { return Ok(arg) };
    match e.run_program(src, env) {
        Ok(v) => Ok(v),
        // a syntax error inside eval is a catchable SyntaxError, not a
        // failed turn
        Err(JsError::Syntax { msg, line, col }) => {
            let text = format!("SyntaxError: {msg} (line {line}, col {col})");
            Err(JsError::Throw(caught_value(
                e,
                Val::Str(Rc::from(text.as_str())),
            )))
        }
        Err(other) => Err(other),
    }
}

pub(crate) fn caught_value(e: &mut JsEngine, v: Val) -> Val {
    let Val::Str(s) = v else { return v };
    let Some((name, msg)) = s.split_once(": ") else {
        return Val::Str(s);
    };
    let known = matches!(
        name,
        "Error"
            | "TypeError"
            | "RangeError"
            | "SyntaxError"
            | "ReferenceError"
            | "EvalError"
            | "URIError"
    );
    if !known {
        return Val::Str(s);
    }
    let Some(id) = e.native_id(name) else {
        return Val::Str(s);
    };
    match e.call(Val::Native(id), Val::Undefined, vec![Val::Str(Rc::from(msg))]) {
        Ok(v) => v,
        Err(_) => Val::Str(s),
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
            // only consume what the pattern needs; a rest element drains the
            // remainder. An iterator that is not exhausted is closed.
            let drain = parts.iter().any(|p| matches!(p, PatPart::Rest(_)));
            let limit = if drain { None } else { Some(parts.len()) };
            let (items, done, it) = iter_limited(e, &value, limit)?;
            let r = bind_arr(e, env, parts, &items, kind);
            if !done {
                close_iterator(e, &it)?;
            }
            r
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
    // RequireObjectCoercible: an object pattern throws for null/undefined even
    // when it has no properties to read (`{} = null`, `function f({})`).
    if matches!(value, Val::Undefined | Val::Null) {
        return Err(JsError::throw(&format!(
            "TypeError: Cannot destructure '{}' as an object",
            value.type_of()
        )));
    }
    // track keys already extracted so `...rest` doesn't repeat them
    let mut used: Vec<Rc<str>> = Vec::new();
    for op in parts {
        // `{[expr]: target}` evaluates its key against the current scope first
        let (key, part) = match op {
            ObjPatPart::Prop { key, part } => (key.clone(), part),
            ObjPatPart::ComputedProp { key: expr, part } => {
                let k: Rc<str> = Rc::from(eval_expr(e, env, expr)?.to_string().as_str());
                (k, part)
            }
            ObjPatPart::Rest(p) => {
                // remaining own *enumerable* properties as an object. Read
                // each one through get_member so an accessor runs exactly
                // once (and a non-enumerable prop is skipped entirely).
                let o = Val::new_object();
                let orc = obj_of(&o);
                for k in for_in_keys(value) {
                    if used.iter().any(|u| *u == k) {
                        continue;
                    }
                    let v = get_member(e, value, &k)?;
                    obj_set_own(&orc, &k, v);
                }
                bind_pat(e, env, p, o, kind)?;
                continue;
            }
        };
        let v = get_member(e, value, &key)?;
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
        used.push(key);
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
            let hidden = match &b.kind {
                ObjKind::Plain(Some(h)) => Some(h.as_slice()),
                _ => None,
            };
            let visible = |k: &Rc<str>| !hidden.is_some_and(|h| h.iter().any(|x| x == k));
            match &b.kind {
                ObjKind::Array(items) => (0..items.len())
                    .map(|i| Rc::from(i.to_string()))
                    .collect(),
                ObjKind::ModuleNs(xs) => xs.borrow().keys().cloned().collect(),
                _ => b
                    .props
                    .iter()
                    .map(|(k, _)| k.clone())
                    .filter(|k| visible(k))
                    .collect(),
            }
        }
        _ => Vec::new(),
    }
}

/// Build a built-in iterator object over `source`. `get_iterator` wraps
/// arrays/typed arrays/strings, live host collections (as a one-query
/// snapshot), Sets/Maps (hidden payload props) and user `@@iterator`
/// results; anything else throws a clear TypeError.
pub(crate) fn make_iterator(source: Val, kind: IterKind) -> Val {
    Val::Obj(new_obj(
        ObjKind::Iterator(Box::new(IterState {
            source,
            kind,
            index: 0,
            done: false,
        })),
        Vec::new(),
        None,
    ))
}

/// The raw sequence slot `i` (for Map sources this is the `[k, v]` pair).
fn iter_slot(source: &Val, i: usize) -> Option<Val> {
    match source {
        Val::Str(s) => s.chars().nth(i).map(|c| Val::Str(Rc::from(c.to_string()))),
        Val::Obj(o) => {
            let is_typed = matches!(o.borrow().kind, ObjKind::TypedArray(_));
            if is_typed {
                if let ObjKind::TypedArray(ta) = &o.borrow().kind {
                    return if i < ta.len {
                        Some(crate::js::typed_read(ta, i))
                    } else {
                        None
                    };
                }
            }
            let direct = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => Some(items.get(i).cloned()),
                    _ => None,
                }
            };
            if let Some(v) = direct {
                return v;
            }
            let hidden = o
                .borrow()
                .props
                .iter()
                .find(|(k, _)| &**k == "__set" || &**k == "__map")
                .map(|(_, v)| v.clone());
            match hidden {
                Some(Val::Obj(a)) => match &a.borrow().kind {
                    ObjKind::Array(items) => items.get(i).cloned(),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

/// One step of a built-in iterator: `Some(value)` or `None` when done.
pub(crate) fn iter_step(it: &mut IterState) -> Option<Val> {
    if it.done {
        return None;
    }
    let raw = iter_slot(&it.source, it.index)?;
    let idx = it.index;
    it.index += 1;
    Some(match it.kind {
        IterKind::Values => raw,
        IterKind::Keys => keys_value(&it.source, idx, raw),
        IterKind::Entries => entries_value(&it.source, idx, raw),
    })
}

/// `has_hidden` reports whether the source object carries a given hidden
/// payload prop (`__set` / `__map`).
fn has_hidden(source: &Val, name: &str) -> bool {
    matches!(source, Val::Obj(o) if o.borrow().props.iter().any(|(k, _)| k.as_ref() == name))
}

/// Keys: Map pairs expose their first element, Sets expose their values, and
/// arrays/typed arrays/strings expose their indices.
fn keys_value(source: &Val, idx: usize, raw: Val) -> Val {
    if has_hidden(source, "__map") {
        if let Val::Obj(p) = &raw {
            if let ObjKind::Array(kv) = &p.borrow().kind {
                return kv.first().cloned().unwrap_or(Val::Undefined);
            }
        }
        return Val::Undefined;
    }
    if has_hidden(source, "__set") {
        return raw;
    }
    Val::Num(idx as f64)
}

fn entries_value(source: &Val, idx: usize, raw: Val) -> Val {
    if has_hidden(source, "__map") {
        return raw;
    }
    if has_hidden(source, "__set") {
        return Val::new_array(vec![raw.clone(), raw]);
    }
    Val::new_array(vec![Val::Num(idx as f64), raw])
}

/// `{ value, done }` step object.
fn step_object(done: bool, value: Val) -> Val {
    let o = Val::new_object();
    obj_set_own(own(&o), "value", value);
    obj_set_own(own(&o), "done", Val::Bool(done));
    o
}

/// Advance an `ObjKind::Iterator` object (shared by `.next()` and `for..of`).
pub(crate) fn iterator_next(this: &Val) -> Result<Val, JsError> {
    let Val::Obj(o) = this else {
        return Err(JsError::throw(
            "TypeError: Iterator method called on incompatible receiver",
        ));
    };
    let step = {
        let mut b = o.borrow_mut();
        match &mut b.kind {
            ObjKind::Iterator(it) => {
                let v = iter_step(it);
                if v.is_none() {
                    it.done = true;
                }
                v
            }
            _ => {
                return Err(JsError::throw(
                    "TypeError: Iterator method called on incompatible receiver",
                ))
            }
        }
    };
    Ok(match step {
        Some(v) => step_object(false, v),
        None => step_object(true, Val::Undefined),
    })
}

/// The iterator object for an iterable value (`@@iterator()`).
pub(crate) fn get_iterator(e: &mut JsEngine, v: &Val) -> Result<Val, JsError> {
    match v {
        Val::Str(_) => return Ok(make_iterator(v.clone(), IterKind::Values)),
        Val::Obj(o) => {
            // live host collection: snapshot once through the embedder hook
            // (per-index reads would re-query the DOM for every element)
            let live = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Live(s) => Some(s.clone()),
                    _ => None,
                }
            };
            if let Some(spec) = live {
                let items = e.live_values.map(|f| f(&spec)).unwrap_or_default();
                return Ok(make_iterator(Val::new_array(items), IterKind::Values));
            }
            // arrays iterate through the *live* `Array.prototype[Symbol.iterator]`:
            // the fast path only applies while it is still the builtin
            // `values`. Deleting it makes arrays non-iterable; replacing it
            // with a user function must call that function.
            if matches!(o.borrow().kind, ObjKind::Array(_)) {
                let current = match &e.array_proto {
                    Some(p) => get_member(e, &Val::Obj(p.clone()), crate::js::SYMBOL_ITERATOR)?,
                    None => Val::Undefined,
                };
                if matches!(current, Val::Native(id) if id == e.array_values_native) {
                    return Ok(make_iterator(v.clone(), IterKind::Values));
                }
                if is_callable(&current) {
                    let it = e.call(current, v.clone(), Vec::new())?;
                    if matches!(it, Val::Obj(_)) {
                        return Ok(it);
                    }
                    return Err(JsError::throw(
                        "TypeError: Result of the Symbol.iterator method is not an object",
                    ));
                }
                return Err(JsError::throw(
                    "TypeError: object is not iterable (cannot read property Symbol(Symbol.iterator))",
                ));
            }
            let builtin = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::TypedArray(_) => Some(IterKind::Values),
                    // a generator is its own iterator: `[...gen]`,
                    // `for (const x of gen)`, destructuring a call result
                    ObjKind::Iterator(_) | ObjKind::Generator(_) => return Ok(v.clone()),
                    _ => {
                        if b.props.iter().any(|(k, _)| &**k == "__set") {
                            Some(IterKind::Values)
                        } else if b.props.iter().any(|(k, _)| &**k == "__map") {
                            Some(IterKind::Entries)
                        } else {
                            None
                        }
                    }
                }
            };
            if let Some(kind) = builtin {
                return Ok(make_iterator(v.clone(), kind));
            }
            // user object: consult `[Symbol.iterator]`
            let f = get_member(e, v, crate::js::SYMBOL_ITERATOR)?;
            if is_callable(&f) {
                let it = e.call(f, v.clone(), Vec::new())?;
                if matches!(it, Val::Obj(_)) {
                    return Ok(it);
                }
                return Err(JsError::throw(
                    "TypeError: Result of the Symbol.iterator method is not an object",
                ));
            }
            Err(JsError::throw(
                "TypeError: object is not iterable (cannot read property Symbol(Symbol.iterator))",
            ))
        }
        _ => Err(JsError::throw(&format!(
            "TypeError: {} is not iterable",
            v.type_of()
        ))),
    }
}

/// Collect every value of an iterable (used by spread / Array.from /
/// built-in constructors). Bounded so a runaway user iterator fails
/// honestly instead of hanging the page.
const ITER_CAP: usize = 100_000;

pub(crate) fn for_of_items(e: &mut JsEngine, v: &Val) -> Result<Vec<Val>, JsError> {
    let it = get_iterator(e, v)?;
    let mut out = Vec::new();
    // fast path: engine iterator object, no `.next()` call overhead
    if let Val::Obj(o) = &it {
        let mut fast = false;
        {
            let mut b = o.borrow_mut();
            if let ObjKind::Iterator(state) = &mut b.kind {
                fast = true;
                loop {
                    match iter_step(state) {
                        Some(v) => {
                            if out.len() >= ITER_CAP {
                                return Err(JsError::throw(
                                    "RangeError: iterator produced too many values",
                                ));
                            }
                            out.push(v);
                        }
                        None => break,
                    }
                }
            }
        }
        if fast {
            return Ok(out);
        }
    }
    // generic protocol: call `.next()` until `done`
    let next = get_member(e, &it, "next")?;
    for _ in 0..ITER_CAP {
        let step = e.call(next.clone(), it.clone(), Vec::new())?;
        if get_member(e, &step, "done")?.to_bool() {
            return Ok(out);
        }
        out.push(get_member(e, &step, "value")?);
    }
    Err(JsError::throw(
        "RangeError: iterator produced too many values",
    ))
}

/// Pull at most `limit` values from an iterable (destructuring must not drain
/// an iterator it does not need). Returns the values, whether the iterator
/// signalled `done`, and the iterator object so the caller can close it.
fn iter_limited(
    e: &mut JsEngine,
    v: &Val,
    limit: Option<usize>,
) -> Result<(Vec<Val>, bool, Val), JsError> {
    let it = get_iterator(e, v)?;
    let mut out = Vec::new();
    // fast path: engine iterator object, no `.next()` call overhead
    if let Val::Obj(o) = &it {
        let mut fast = false;
        let mut done = false;
        {
            let mut b = o.borrow_mut();
            if let ObjKind::Iterator(state) = &mut b.kind {
                fast = true;
                loop {
                    if limit.is_some_and(|l| out.len() >= l) {
                        break;
                    }
                    match iter_step(state) {
                        Some(v) => {
                            if out.len() >= ITER_CAP {
                                return Err(JsError::throw(
                                    "RangeError: iterator produced too many values",
                                ));
                            }
                            out.push(v);
                        }
                        None => {
                            done = true;
                            break;
                        }
                    }
                }
            }
        }
        if fast {
            return Ok((out, done, it));
        }
    }
    // generic protocol: call `.next()` as needed
    let next = get_member(e, &it, "next")?;
    loop {
        if limit.is_some_and(|l| out.len() >= l) {
            return Ok((out, false, it));
        }
        if out.len() >= ITER_CAP {
            return Err(JsError::throw(
                "RangeError: iterator produced too many values",
            ));
        }
        let step = e.call(next.clone(), it.clone(), Vec::new())?;
        if get_member(e, &step, "done")?.to_bool() {
            return Ok((out, true, it));
        }
        out.push(get_member(e, &step, "value")?);
    }
}

/// `IteratorClose`: call the iterator's `return` hook (a no-op for the
/// engine's built-in iterators, which have none).
fn close_iterator(e: &mut JsEngine, it: &Val) -> Result<(), JsError> {
    let ret = get_member(e, it, "return")?;
    if is_callable(&ret) {
        e.call(ret, it.clone(), Vec::new())?;
    }
    Ok(())
}


// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// Arguments for a tagged template call: the strings array (carrying `.raw`)
/// followed by the interpolated values. shortcut: `.raw` holds the cooked
/// text — the original escape spelling is not preserved.
fn tagged_args(e: &mut JsEngine, env: &Rc<Env>, segs: &[TplSeg]) -> Result<Vec<Val>, JsError> {
    let mut cooked: Vec<Val> = Vec::new();
    let mut vals: Vec<Val> = Vec::new();
    for seg in segs {
        match seg {
            TplSeg::Lit(s) => cooked.push(Val::Str(s.clone())),
            TplSeg::Expr(x) => vals.push(eval_expr(e, env, x)?),
        }
    }
    let raw = Val::Obj(new_obj(ObjKind::Array(cooked.clone()), Vec::new(), None));
    let strings = Val::Obj(new_obj(
        ObjKind::Array(cooked),
        vec![(Rc::from("raw"), raw)],
        None,
    ));
    let mut out = Vec::with_capacity(vals.len() + 1);
    out.push(strings);
    out.extend(vals);
    Ok(out)
}

pub(crate) fn eval_expr(e: &mut JsEngine, env: &Rc<Env>, x: &Expr) -> Result<Val, JsError> {
    if x.span.start > 0 {
        // the innermost expression start is the best runtime-error location;
        // `note_error` records it and `finish_error` resolves it against the
        // source of the running function
        e.cur_span = x.span.start as usize;
    }
    e.tick()?;
    let r = eval_expr_inner(e, env, x);
    if let Err(err) = &r {
        // remember the origin for the log line; the value stays untouched
        e.note_error(err);
    }
    r
}

fn eval_expr_inner(e: &mut JsEngine, env: &Rc<Env>, x: &Expr) -> Result<Val, JsError> {
    match &x.kind {
        ExprKind::Ident(name) => e.env_get_checked(env, name),
        ExprKind::BigInt(n) => Ok(Val::BigInt(Rc::new(*n))),
        ExprKind::NewTarget => Ok(e.env_get(env, "#new.target")),
        ExprKind::PrivateIn { name, obj } => {
            let v = eval_expr(e, env, obj)?;
            let found = match &v {
                Val::Obj(o) => obj_has(o, name),
                Val::Func(c) => c.props.borrow().iter().any(|(k, _)| k == name),
                _ => false,
            };
            Ok(Val::Bool(found))
        }
        ExprKind::Num(n) => Ok(Val::Num(*n)),
        ExprKind::Str(s) => Ok(Val::Str(s.clone())),
        ExprKind::Bool(b) => Ok(Val::Bool(*b)),
        ExprKind::Null => Ok(Val::Null),
        ExprKind::This => Ok(e.env_get(env, "this")),
        ExprKind::Unary { op, expr } => {
            let v = eval_expr(e, env, expr)?;
            if let Val::BigInt(n) = &v {
                let n = **n;
                let overflow =
                    || JsError::throw("RangeError: BigInt result is out of the supported i128 range");
                let nv = match op {
                    UnaryOp::Neg => Val::BigInt(Rc::new(n.checked_neg().ok_or_else(overflow)?)),
                    UnaryOp::Pos => {
                        return Err(JsError::throw(
                            "TypeError: Cannot convert a BigInt value to a number",
                        ))
                    }
                    UnaryOp::Not => return Ok(Val::Bool(n == 0)),
                    UnaryOp::BitNot => Val::BigInt(Rc::new(!n)),
                    UnaryOp::PreInc | UnaryOp::PreDec => {
                        let d = if matches!(op, UnaryOp::PreInc) { 1 } else { -1 };
                        let nv = Val::BigInt(Rc::new(n.checked_add(d).ok_or_else(overflow)?));
                        assign_target(e, env, expr, nv.clone())?;
                        nv
                    }
                };
                return Ok(nv);
            }
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
            if let Val::BigInt(n) = &v {
                let d = if *inc { 1 } else { -1 };
                let nv = Val::BigInt(Rc::new(
                    (**n)
                        .checked_add(d)
                        .ok_or_else(|| JsError::throw("RangeError: BigInt result is out of range"))?,
                ));
                assign_target(e, env, target, nv)?;
                return Ok(v); // postfix yields the OLD value
            }
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
        // `a ??= b`, `a ||= b`, `a &&= b`: read, decide, and only then
        // evaluate (and write) the right-hand side
        ExprKind::LogicalAssign { target, op, value } => {
            let cur = eval_target_value(e, env, target)?;
            let write = match op {
                LogOp::And => cur.to_bool(),
                LogOp::Or => !cur.to_bool(),
                LogOp::Coalesce => matches!(cur, Val::Undefined | Val::Null),
            };
            if !write {
                return Ok(cur);
            }
            let v = eval_expr(e, env, value)?;
            assign_target(e, env, target, v.clone())?;
            Ok(v)
        }
        // class expression: lower in a private scope so a named class
        // expression's binding never leaks to the enclosing scope
        ExprKind::ClassExpr { name, stmts, .. } => {
            let cenv = Env::child(env);
            for s in stmts {
                eval_stmt(e, &cenv, s)?;
            }
            Ok(e.env_get(&cenv, name))
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
                // `super.m(...)`: method found on the superclass, `this`
                // stays the current instance
                ExprKind::SuperMember {
                    class,
                    key,
                    static_,
                } => {
                    let sup = eval_expr(e, env, class)?;
                    let holder = if *static_ {
                        sup
                    } else {
                        get_member(e, &sup, "prototype")?
                    };
                    let f = get_member(e, &holder, key)?;
                    let this = e.env_get(env, "this");
                    e.call(f, this, argvals)
                }
                _ => {
                    let f = eval_expr(e, env, callee)?;
                    // direct eval: `eval(...)` with the intrinsic in scope
                    // runs against the calling environment
                    if let ExprKind::Ident(name) = &callee.kind {
                        if &**name == "eval"
                            && matches!(&f, Val::Native(id) if e.native_id("eval") == Some(*id))
                        {
                            let arg = argvals.into_iter().next().unwrap_or(Val::Undefined);
                            return eval_source(e, env, arg);
                        }
                    }
                    // sloppy mode: plain calls get the global object as this
                    let this = Val::Obj(e.global_obj.clone());
                    e.call(f, this, argvals)
                }
            }
        }
        ExprKind::New { callee, args } => {
            let f = eval_expr(e, env, callee)?;
            let argvals = eval_args(e, env, args)?;
            construct(e, f, argvals)
        }
        ExprKind::Member { base, key } => {
            let b = eval_expr(e, env, base)?;
            get_member(e, &b, key)
        }
        // `super(...)` / `super.m(...)`: the superclass binding is resolved
        // at parse time; `this` always comes from the current environment.
        ExprKind::SuperCall {
            class,
            key,
            static_,
            args,
        } => {
            let argvals = eval_args(e, env, args)?;
            let sup = eval_expr(e, env, class)?;
            let this = e.env_get(env, "this");
            match key {
                // constructor call: run the superclass ctor on the SAME
                // object `new` already created for us
                None => e.call(sup, this, argvals),
                Some(k) => {
                    let holder = if *static_ {
                        sup
                    } else {
                        get_member(e, &sup, "prototype")?
                    };
                    let f = get_member(e, &holder, k)?;
                    e.call(f, this, argvals)
                }
            }
        }
        ExprKind::SuperMember {
            class,
            key,
            static_,
        } => {
            let sup = eval_expr(e, env, class)?;
            let holder = if *static_ {
                sup
            } else {
                get_member(e, &sup, "prototype")?
            };
            get_member(e, &holder, key)
        }
        ExprKind::Index { base, index } => {
            let b = eval_expr(e, env, base)?;
            let k = eval_expr(e, env, index)?.to_string();
            get_member(e, &b, &k)
        }
        // `a?.b.c?.[d]?.()` - a nullish value at an OPTIONAL step ends the
        // whole chain (undefined); at a non-optional step it throws like a
        // plain member read.
        ExprKind::OptionalChain { base, steps } => {
            use super::parser::ChainStep;
            let mut cur = eval_expr(e, env, base)?;
            // `this` for a call is the object of the preceding member/index
            // step; a direct call uses the global object (sloppy mode).
            let mut this = Val::Obj(e.global_obj.clone());
            let mut after_prop = false;
            for step in steps {
                if step.optional() && matches!(cur, Val::Undefined | Val::Null) {
                    return Ok(Val::Undefined);
                }
                match step {
                    ChainStep::Member { key, .. } => {
                        let obj = cur;
                        cur = get_member(e, &obj, key)?;
                        this = obj;
                        after_prop = true;
                    }
                    ChainStep::Index { index, .. } => {
                        let obj = cur;
                        let k = eval_expr(e, env, index)?.to_string();
                        cur = get_member(e, &obj, &k)?;
                        this = obj;
                        after_prop = true;
                    }
                    ChainStep::Call { args, .. } => {
                        let argvals = eval_args(e, env, args)?;
                        let f = cur;
                        let recv = if after_prop {
                            this.clone()
                        } else {
                            Val::Obj(e.global_obj.clone())
                        };
                        cur = e.call(f, recv, argvals)?;
                        after_prop = false;
                    }
                    ChainStep::Tagged { segs } => {
                        let f = cur;
                        let args = tagged_args(e, env, segs)?;
                        cur = e.call(f, Val::Obj(e.global_obj.clone()), args)?;
                        after_prop = false;
                    }
                }
            }
            Ok(cur)
        }
        ExprKind::Array(items) => {
            let mut vals = Vec::with_capacity(items.len());
            for i in items {
                match i {
                    ArrayElem::Item(x) => vals.push(eval_expr(e, env, x)?),
                    ArrayElem::Hole => vals.push(Val::Undefined), // hole -> undefined (dense shortcut)
                    ArrayElem::Spread(sp, _) => {
                        let src = eval_expr(e, env, sp)?;
                        vals.extend(iter_src(e, &src)?);
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
                    // unreachable: a surviving cover form is a parse error, so
                    // an object literal in value position never carries one
                    ObjElem::CoverInit(k, v) => {
                        let val = eval_expr(e, env, v)?;
                        obj_set_own(&orc, k, val);
                    }
                    // `{...src}` copies own enumerable props (arrays copy
                    // their dense items as index keys; accessors invoke their
                    // getters — matching real spread semantics closely enough
                    // for the honest scope of this wave)
                    ObjElem::Spread(sp, _) => {
                        let src = eval_expr(e, env, sp)?;
                        // own enumerable props only; read through get_member
                        // so an accessor runs instead of copying its `Acc`
                        for k in for_in_keys(&src) {
                            let v = get_member(e, &src, &k)?;
                            obj_set_own(&orc, &k, v);
                        }
                    }
                    ObjElem::Computed { key, value } => {
                        let k = eval_expr(e, env, key)?.to_string();
                        let val = eval_expr(e, env, value)?;
                        obj_set_own(&orc, &k, val);
                    }
                    ObjElem::Accessor {
                        key,
                        getter,
                        setter,
                    } => {
                        let getter_v = getter
                            .as_ref()
                            .map(|f| make_val_closure(e, env, f, false));
                        let setter_v = setter
                            .as_ref()
                            .map(|f| make_val_closure(e, env, f, false));
                        obj_set_own(
                            &orc,
                            key,
                            Val::Acc(Rc::new(AccEntry {
                                getter: getter_v,
                                setter: setter_v,
                            })),
                        );
                    }
                    ObjElem::ComputedAccessor {
                        key,
                        getter,
                        setter,
                    } => {
                        let k = eval_expr(e, env, key)?.to_string();
                        let getter_v = getter
                            .as_ref()
                            .map(|f| make_val_closure(e, env, f, false));
                        let setter_v = setter
                            .as_ref()
                            .map(|f| make_val_closure(e, env, f, false));
                        obj_set_own(
                            &orc,
                            &k,
                            Val::Acc(Rc::new(AccEntry {
                                getter: getter_v,
                                setter: setter_v,
                            })),
                        );
                    }
                }
            }
            Ok(o)
        }
        ExprKind::Func(f) => {
            let c = new_closure(f.clone(), env.clone(), None, false);
            // a named function expression binds its own name inside its scope
            // (`let f = function fact() { fact() }`); declarations already
            // bind the name in the enclosing env, so this child env is
            // harmless there. The closure's env is the child from the start
            // so the bound name sees the self-binding too.
            if !f.name.is_empty() {
                let fenv = Env::child(env);
                let c = new_closure(f.clone(), fenv.clone(), None, false);
                // the self-binding is immutable: in sloppy mode an assignment
                // to it is silently ignored, not an overwrite
                fenv.declare_read_only(f.name.clone(), Val::Func(c.clone()));
                return Ok(Val::Func(c));
            }
            Ok(Val::Func(c))
        }
        ExprKind::Arrow(f) => {
            // lexical this captured at creation; the body is hoisted at
            // CALL time into the fresh call env (see `call_inner`)
            let lexical_this = e.env_get(env, "this");
            Ok(Val::Func(new_closure(
                f.clone(),
                env.clone(),
                Some(Box::new(lexical_this)),
                true,
            )))
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
        ExprKind::TaggedTemplate { tag, segs } => {
            let f = eval_expr(e, env, tag)?;
            let args = tagged_args(e, env, segs)?;
            e.call(f, Val::Obj(e.global_obj.clone()), args)
        }
        ExprKind::Regex { pattern, flags } => {
            let compiled = e.regex_from_literal(pattern.clone(), flags.clone())?;
            Ok(Val::Obj(new_obj(
                ObjKind::Regex(ReInst {
                    re: compiled,
                    last_index: std::cell::Cell::new(0.0),
                }),
                Vec::new(),
                None,
            )))
        }
        ExprKind::Yield(_) | ExprKind::YieldStar(_) => Err(JsError::throw(
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
            // a `with` object's property is deletable; an ordinary binding is
            // not, and an unresolvable name deletes successfully
            ExprKind::Ident(name) => e.env_delete(env, name),
            _ => false,
        })),
    }
}

/// `await v` under run-to-completion semantics:
/// - already-settled engine promise: unwrap (rejection throws into the frame)
/// - pending engine promise: pump the microtask queue (bounded) until it
///   settles; the honest exception is promises that only timers/macrotasks
///   can settle — those error out (see `await_promise`)
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
pub(crate) fn iter_src(e: &mut JsEngine, v: &Val) -> Result<Vec<Val>, JsError> {
    for_of_items(e, v)
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
                out.extend(iter_src(e, &src)?);
            }
        }
    }
    Ok(out)
}

/// current value of an assignment target (ident or member)
fn eval_target_value(e: &mut JsEngine, env: &Rc<Env>, target: &Expr) -> Result<Val, JsError> {
    match &target.kind {
        ExprKind::Ident(name) => e.env_get_checked(env, name),
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

/// `new callee(args)`: build the fresh `this`, run the call, and apply the
/// "an object result wins" rule. Shared by the `new` operator and
/// `Reflect.construct`; the latter ignores its `newTarget` argument (shortcut:
/// the constructed object always takes `callee.prototype`).
pub(crate) fn construct(e: &mut JsEngine, f: Val, argvals: Vec<Val>) -> Result<Val, JsError> {
    let fresh = match &f {
        Val::Func(c) => {
            let proto = ensure_prototype(c);
            Val::Obj(new_obj(ObjKind::Plain(None), Vec::new(), Some(proto)))
        }
        _ => Val::new_object(),
    };
    // `new Promise(executor)` needs to run its executor arg; handled
    // generically: natives get the fresh object as `this`
    e.pending_construct = Some(f.clone());
    let res = e.call(f, fresh.clone(), argvals)?;
    // a constructor returning an OBJECT overrides `this`. Functions are
    // objects too (this is how `new Function(...)` hands back the compiled
    // function instead of the discarded fresh object).
    Ok(if matches!(
        res,
        Val::Obj(_) | Val::Func(_) | Val::Native(_) | Val::Method(_)
    ) {
        res
    } else {
        fresh
    })
}

/// `.prototype` accessor for constructors: created lazily on first use so
/// plain (non-constructor) functions pay no prototype allocation.
pub(crate) fn ensure_prototype(c: &Rc<Closure>) -> Rc<RefCell<Obj>> {
    if let Some((_, v)) = c.props.borrow().iter().find(|(k, _)| k.as_ref() == "prototype") {
        if let Val::Obj(o) = v {
            return o.clone();
        }
    }
    let proto = new_obj(
        ObjKind::Plain(None),
        vec![(Rc::from("constructor"), Val::Func(c.clone()))],
        None,
    );
    // the implicit `constructor` back-reference is non-enumerable
    obj_hide(&proto, &Rc::from("constructor"));
    c.props
        .borrow_mut()
        .push((Rc::from("prototype"), Val::Obj(proto.clone())));
    proto
}

/// BigInt toString with an optional radix (2..=36).
fn bigint_radix(n: i128, radix: u32) -> String {
    if radix == 10 {
        return n.to_string();
    }
    let neg = n < 0;
    let mut v = n.unsigned_abs();
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    if v == 0 {
        out.push(b'0');
    }
    while v > 0 {
        out.push(digits[(v % radix as u128) as usize]);
        v /= radix as u128;
    }
    if neg {
        out.push(b'-');
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// BigInt arithmetic. `None` = neither side is a BigInt (or this is a
/// comparison / string concatenation, handled by the generic paths).
fn bigint_binary(op: BinOp, l: &Val, r: &Val) -> Option<Result<Val, JsError>> {
    use BinOp::*;
    let a = match l {
        Val::BigInt(n) => Some(**n),
        _ => None,
    };
    let b = match r {
        Val::BigInt(n) => Some(**n),
        _ => None,
    };
    if a.is_none() && b.is_none() {
        return None;
    }
    let arithmetic = matches!(
        op,
        Add | Sub | Mul | Div | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr
    );
    if !arithmetic {
        return None; // ==, ===, <, ... use the shared comparison helpers
    }
    // `1n + "x"` is string concatenation, not an arithmetic mix
    if op == Add && (matches!(l, Val::Str(_)) || matches!(r, Val::Str(_))) {
        return None;
    }
    let (Some(a), Some(b)) = (a, b) else {
        return Some(Err(JsError::throw(
            "TypeError: Cannot mix BigInt and other types, use explicit conversions",
        )));
    };
    let overflow = || JsError::throw("RangeError: BigInt result is out of the supported i128 range");
    let val = |n: i128| Ok(Val::BigInt(Rc::new(n)));
    Some(match op {
        Add => a.checked_add(b).map(val).unwrap_or_else(|| Err(overflow())),
        Sub => a.checked_sub(b).map(val).unwrap_or_else(|| Err(overflow())),
        Mul => a.checked_mul(b).map(val).unwrap_or_else(|| Err(overflow())),
        Div => {
            if b == 0 {
                Err(JsError::throw("RangeError: Division by zero"))
            } else {
                a.checked_div(b).map(val).unwrap_or_else(|| Err(overflow()))
            }
        }
        Mod => {
            if b == 0 {
                Err(JsError::throw("RangeError: Division by zero"))
            } else {
                a.checked_rem(b).map(val).unwrap_or_else(|| Err(overflow()))
            }
        }
        Pow => {
            if b < 0 {
                Err(JsError::throw("RangeError: BigInt exponent must be non-negative"))
            } else if b > u32::MAX as i128 {
                Err(overflow())
            } else {
                // |base| <= 1 never overflows, so the loop below would spin
                // for the full exponent (up to u32::MAX iterations — seconds
                // of frozen page, and the JS step budget does not cover
                // builtin loops). Answer those directly.
                match a {
                    0 => val(if b == 0 { 1 } else { 0 }),
                    1 => val(1),
                    -1 => val(if b % 2 == 0 { 1 } else { -1 }),
                    // any larger base leaves i128 by the 128th multiplication
                    _ if b > 127 => Err(overflow()),
                    _ => {
                        let mut acc: i128 = 1;
                        let mut ok = true;
                        for _ in 0..b {
                            match acc.checked_mul(a) {
                                Some(v) => acc = v,
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok { val(acc) } else { Err(overflow()) }
                    }
                }
            }
        }
        BitAnd => val(a & b),
        BitOr => val(a | b),
        BitXor => val(a ^ b),
        Shl => {
            if b < 0 || b > 127 {
                Err(overflow())
            } else {
                a.checked_shl(b as u32).map(val).unwrap_or_else(|| Err(overflow()))
            }
        }
        Shr => {
            if b < 0 || b > 127 {
                // shifting a BigInt right by more than its width is 0 / -1
                val(if a < 0 { -1 } else { 0 })
            } else {
                val(a >> b)
            }
        }
        _ => unreachable!(),
    })
}

#[derive(Clone, Copy, PartialEq)]
enum PrimHint {
    Default,
    Number,
    String,
}

fn is_primitive(v: &Val) -> bool {
    !matches!(
        v,
        Val::Obj(_) | Val::Func(_) | Val::Native(_) | Val::Method(_) | Val::Acc(_)
    )
}

/// JS `ToPrimitive` with observable coercions. `Symbol.toPrimitive` wins;
/// otherwise `valueOf`/`toString` run in hint order. Boxed primitives unwrap
/// directly (their primitive prototypes are not attached in this engine) and
/// `Date` behaves as if it had the spec `@@toPrimitive` (string for
/// `default`, so `date + date` concatenates).
fn to_primitive(e: &mut JsEngine, v: &Val, hint: PrimHint) -> Result<Val, JsError> {
    let mut hint = hint;
    match v {
        Val::Obj(o) => {
            let b = o.borrow();
            if let ObjKind::Boxed(p) = &b.kind {
                return Ok(p.clone());
            }
            if hint == PrimHint::Default && matches!(b.kind, ObjKind::Date(_)) {
                hint = PrimHint::String;
            }
        }
        Val::Func(_) | Val::Native(_) => {}
        _ => return Ok(v.clone()),
    }
    let sym = get_member(e, v, "Symbol(toPrimitive)")?;
    if is_callable(&sym) {
        let hint = match hint {
            PrimHint::Default => "default",
            PrimHint::Number => "number",
            PrimHint::String => "string",
        };
        let r = e.call(sym, v.clone(), vec![Val::Str(Rc::from(hint))])?;
        if is_primitive(&r) {
            return Ok(r);
        }
        return Err(JsError::throw(
            "TypeError: Cannot convert object to primitive value",
        ));
    }
    let order: [&str; 2] = match hint {
        PrimHint::String => ["toString", "valueOf"],
        _ => ["valueOf", "toString"],
    };
    for name in order {
        let f = get_member(e, v, name)?;
        if is_callable(&f) {
            let r = e.call(f, v.clone(), Vec::new())?;
            if is_primitive(&r) {
                return Ok(r);
            }
        }
    }
    Err(JsError::throw(
        "TypeError: Cannot convert object to primitive value",
    ))
}

fn binary_op(e: &mut JsEngine, op: BinOp, l: &Val, r: &Val) -> Result<Val, JsError> {
    use BinOp::*;
    if let Some(result) = bigint_binary(op, l, r) {
        return result;
    }
    Ok(match op {
        Add => {
            // ToPrimitive on both sides; a string operand (after coercion,
            // including a Date or an object with a custom `toString`) wins
            let lp = to_primitive(e, l, PrimHint::Default)?;
            let rp = to_primitive(e, r, PrimHint::Default)?;
            if let Some(result) = bigint_binary(Add, &lp, &rp) {
                return result;
            }
            if matches!(lp, Val::Str(_) | Val::BigInt(_))
                || matches!(rp, Val::Str(_))
            {
                Val::Str(Rc::from(format!("{}{}", lp.to_string(), rp.to_string())))
            } else {
                Val::Num(lp.to_number() + rp.to_number())
            }
        }
        Sub | Mul | Div | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr | UShr => {
            let lp = to_primitive(e, l, PrimHint::Number)?;
            let rp = to_primitive(e, r, PrimHint::Number)?;
            if let Some(result) = bigint_binary(op, &lp, &rp) {
                return result;
            }
            let a = lp.to_number();
            let b = rp.to_number();
            match op {
                Sub => Val::Num(a - b),
                Mul => Val::Num(a * b),
                Div => Val::Num(a / b),
                Mod => Val::Num(a % b),
                Pow => Val::Num(a.powf(b)),
                BitAnd => Val::Num((to_int32(a) & to_int32(b)) as f64),
                BitOr => Val::Num((to_int32(a) | to_int32(b)) as f64),
                BitXor => Val::Num((to_int32(a) ^ to_int32(b)) as f64),
                Shl => Val::Num((to_int32(a) << (to_uint32(b) & 31)) as f64),
                Shr => Val::Num((to_int32(a) >> (to_uint32(b) & 31)) as f64),
                UShr => Val::Num((to_uint32(a) >> (to_uint32(b) & 31)) as f64),
                _ => unreachable!(),
            }
        }
        Eq | Ne => {
            // object-vs-object stays identity; otherwise ToPrimitive first
            let eq = match (l, r) {
                (Val::Obj(_), Val::Obj(_)) => strict_eq(l, r),
                _ => {
                    let lp = to_primitive(e, l, PrimHint::Default)?;
                    let rp = to_primitive(e, r, PrimHint::Default)?;
                    loose_eq(&lp, &rp)
                }
            };
            Val::Bool(if op == Eq { eq } else { !eq })
        }
        StrictEq => Val::Bool(strict_eq(l, r)),
        StrictNe => Val::Bool(!strict_eq(l, r)),
        Lt | Le | Gt | Ge => {
            let lp = to_primitive(e, l, PrimHint::Number)?;
            let rp = to_primitive(e, r, PrimHint::Number)?;
            Val::Bool(relational(op, &lp, &rp))
        }
        In => {
            let key = to_primitive(e, l, PrimHint::String)?.to_string();
            Val::Bool(super::has_member(e, &r, &key)?)
        }
        InstanceOf => match r {
            Val::Func(c) => {
                // lazy prototype creation: `instanceof` works even before
                // anything touched `.prototype`
                let p = ensure_prototype(c);
                let yes = match l {
                    Val::Obj(o) => {
                        // start from the lazy chain: arrays/iterators have no
                        // explicit proto until first member access, and
                        // `[].values() instanceof Object` must still hold
                        let mut cur = e.proto_of(o);
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
                            cur = e.proto_of(&c);
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
            Val::Native(id) => Val::Bool(match l {
                Val::Obj(o) => {
                    // a registered `.prototype` (URL, URLSearchParams, …) is
                    // matched against the value's chain like a real constructor
                    let registered = match e.native_prop(*id, "prototype") {
                        Some(Val::Obj(p)) => {
                            let mut cur = e.proto_of(o);
                            let mut found = false;
                            for _ in 0..100 {
                                match cur {
                                    None => break,
                                    Some(c) => {
                                        if Rc::ptr_eq(&c, &p) {
                                            found = true;
                                            break;
                                        }
                                        cur = e.proto_of(&c);
                                    }
                                }
                            }
                            found
                        }
                        _ => false,
                    };
                    registered
                        || matches!(o.borrow().kind, ObjKind::Date(_))
                        || e
                            .array_proto
                            .as_ref()
                            .map_or(false, |ap| {
                                let mut cur = e.proto_of(o);
                                let mut found = false;
                                for _ in 0..100 {
                                    match cur {
                                        None => break,
                                        Some(c) => {
                                            if Rc::ptr_eq(&c, ap) {
                                                found = true;
                                                break;
                                            }
                                            cur = e.proto_of(&c);
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
                            let mut cur = e.proto_of(lhs);
                            let mut found = false;
                            for _ in 0..100 {
                                match cur {
                                    None => break,
                                    Some(c) => {
                                        if Rc::ptr_eq(&c, &p) {
                                            found = true;
                                            break;
                                        }
                                        cur = e.proto_of(&c);
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
    if let (Val::BigInt(a), Val::BigInt(b)) = (l, r) {
        return match op {
            BinOp::Lt => a < b,
            BinOp::Le => a <= b,
            BinOp::Gt => a > b,
            BinOp::Ge => a >= b,
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
    // a boxed primitive's prototype method sees the primitive as `this`
    let unboxed = match this {
        Val::Obj(o) => match &o.borrow().kind {
            ObjKind::Boxed(v) => Some(v.clone()),
            _ => None,
        },
        _ => None,
    };
    if let Some(v) = unboxed {
        return call_method(e, m, &v, args);
    }
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
        StrLastIndexOf => {
            let s = need_str(this)?;
            let needle = arg_str(args, 0);
            let chars: Vec<char> = s.chars().collect();
            let search: Vec<char> = needle.chars().collect();
            // `position` caps the START index of the match; absent (or NaN)
            // means "anywhere", so start from the last possible slot
            let max_start = chars.len().saturating_sub(search.len());
            let from = match args.get(1) {
                None | Some(Val::Undefined) => max_start,
                Some(v) => {
                    let n = v.to_number();
                    if n.is_nan() {
                        max_start
                    } else if n < 0.0 {
                        0
                    } else {
                        (n as usize).min(max_start)
                    }
                }
            };
            if search.is_empty() {
                return Ok(Val::Num(from.min(chars.len()) as f64));
            }
            let idx = (0..=from)
                .rev()
                .find(|&start| chars[start..start + search.len()] == search[..]);
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
        // `"".charCodeAt(i)`: UTF-16 code unit, NaN when out of range
        StrCharCodeAt => {
            let s = need_str(this)?;
            let i = arg_num(args, 0);
            let i = if i.is_nan() || i < 0.0 { usize::MAX } else { i as usize };
            Ok(match s.encode_utf16().nth(i) {
                Some(u) => Val::Num(u as f64),
                None => Val::Num(f64::NAN),
            })
        }
        StrToString => Ok(need_str(this).map(Val::Str)?),

        // -- number methods ------------------------------------------------
        NumToString => match this {
            Val::Num(n) => Ok(Val::Str(Rc::from(num_to_str(*n).as_str()))),
            Val::Bool(b) => Ok(Val::Str(Rc::from(b.to_string()))),
            Val::BigInt(n) => {
                let radix = args
                    .first()
                    .map(|v| v.to_number())
                    .filter(|r| *r >= 2.0 && *r <= 36.0)
                    .unwrap_or(10.0) as u32;
                Ok(Val::Str(Rc::from(bigint_radix(**n, radix).as_str())))
            }
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
            gen::gen_return(e, this, arg)
        }
        IterNext => iterator_next(this),
        IterSelf => Ok(this.clone()),
        IterReturn => {
            if let Val::Obj(o) = this {
                if let ObjKind::Iterator(it) = &mut o.borrow_mut().kind {
                    it.done = true;
                }
            }
            let v = args.first().cloned().unwrap_or(Val::Undefined);
            Ok(step_object(true, v))
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
        FnToString => Ok(Val::Str(Rc::from(
            match this {
                Val::Func(c) => format!("function {}() {{ [code] }}", c.def.name),
                _ => "function () { [native code] }".to_string(),
            }
            .as_str(),
        ))),
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
                        // the wrapped function's own frame supplies the source
                        src: 0,
                    });
                    Ok(Val::Func(new_closure(def, Env::root(), None, false)))
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

/// `str.replace(re, repl)` — string-or-function replacement with $-patterns
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

/// `.finally(cb)` — reaction that runs cb then propagates the outcome
fn finally_then(e: &mut JsEngine, o: &Rc<RefCell<Obj>>, cb: Val) -> Result<Val, JsError> {
    let ret = e.new_promise();
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
