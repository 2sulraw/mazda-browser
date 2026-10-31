//! Generator execution: a small resumable walker over the body shapes the
//! parser accepts for generators (the parser restricts `yield` to a whole
//! expression statement `yield v;` or a declarator initializer
//! `let x = yield v`, and rejects try/catch-with-yield and async generators).
//!
//! # Design
//! A `GenVm` holds an explicit frame stack (`GFrame`). `.next()` resumes by
//! driving the top frame until a `yield` fires (suspension) or the body
//! completes. Because ALL runtime state lives in the frame stack rather than
//! on the native call stack, suspension costs one `Vec<GenVm>` state and
//! resumption continues exactly where the cursor stopped:
//! - yield inside an if/loop body: the enclosing frames stay on the stack
//!   with their cursors, so a resumed `.next()` continues mid-branch.
//! - `let x = yield v` queues the declarator in `pending`; the value fed to
//!   the NEXT `.next(arg)` becomes the yield-expression's result and is
//!   bound at resume time.
//! RAM: frames clone statement lists (`Rc<Vec<Stmt>>`) once per loop
//! iteration push; capture-free, bounded by the generator body size. No
//! native-stack growth across yields.
//!
//! Honest gaps (parse-time enforced where noted): `yield` in loop/if-
//! conditions, `return yield`, try-with-yield, `.throw()`, nested generator
//! suspension inside for-in/of heads.

use super::interp::{self, eval_expr, Flow};
use super::*;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// VM state
// ---------------------------------------------------------------------------

/// value queued for the next `.next(arg)`: what the suspended yield
/// expression evaluates to (`let x = yield v` binds x = arg)
enum PendBind {
    Name {
        kind: parser::VarKind,
        name: Rc<str>,
    },
    Pat {
        kind: parser::VarKind,
        pat: parser::Pat,
    },
}

enum GFrame {
    /// a run of statements with a resume cursor
    Batch {
        stmts: Rc<Vec<parser::Stmt>>,
        i: usize,
        env: Rc<Env>,
    },
    /// a declarator list mid-flight (suspend happens per declarator)
    Decls {
        kind: parser::VarKind,
        ds: Rc<Vec<parser::Declarator>>,
        j: usize,
        env: Rc<Env>,
    },
    While {
        test: Rc<parser::Expr>,
        body: Rc<Vec<parser::Stmt>>,
        env: Rc<Env>,
    },
    DoWhile {
        first: bool,
        test: Rc<parser::Expr>,
        body: Rc<Vec<parser::Stmt>>,
        env: Rc<Env>,
    },
    For {
        first: bool,
        cond: Option<Rc<parser::Expr>>,
        upd: Option<Rc<parser::Expr>>,
        body: Rc<Vec<parser::Stmt>>,
        env: Rc<Env>,
    },
    ForEach {
        target: parser::ForTarget,
        kind: Option<parser::VarKind>,
        items: Vec<Val>,
        idx: usize,
        body: Rc<Vec<parser::Stmt>>,
        env: Rc<Env>,
    },
    /// stitched case bodies from the first matching case on; `break` here
    /// exits the switch (see the Break flow handling in `drive`)
    SwitchBody {
        batch: Rc<Vec<parser::Stmt>>,
        env: Rc<Env>,
    },
}

/// what one generator `.next` step produced
enum GOut {
    /// yielded (done = false)
    Yield(Val),
    /// body finished with a value (done = true)
    Ret(Val),
}

enum StepR {
    /// suspension; remnant frame already re-pushed
    Susp(Val),
    /// frame finished normally; drop and continue
    Finished,
    /// function `return` reached: unwind everything
    Retr(Val),
    BreakFlow,
    ContinueFlow,
    /// child frames were pushed; continue the outer drive loop
    NewFrame,
}

enum SStep {
    Done,
    /// statement delegated to interp flow (break/continue escaping)
    Flow(Flow),
    /// suspend on this statement's yield
    Yield(Val),
    /// declarator list containing a yield â€” needs frame treatment
    Decls {
        kind: parser::VarKind,
        ds: Rc<Vec<parser::Declarator>>,
    },
    /// child frame(s) pushed
    Pushed,
}

pub(crate) struct GenVm {
    def: Rc<FnDef>,
    frames: Vec<GFrame>,
    done: bool,
    running: bool,
    pending: Option<(PendBind, Rc<Env>)>,
}

impl std::fmt::Debug for GenVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GenVm({})", self.def.name)
    }
}

impl GenVm {
    fn root_stmts(def: &Rc<FnDef>) -> Option<Rc<Vec<parser::Stmt>>> {
        match &def.body {
            FnBody::Block(stmts) => Some(Rc::new(stmts.clone())),
            // generators always have block bodies from the parser; an
            // expression body here would mean synthetic FnDef::Bound
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// construction (called from the engine's call_inner)
// ---------------------------------------------------------------------------

/// Create the generator object for a `function*` call: params/binding env
/// already built by the caller; the body is NOT executed yet.
pub(crate) fn gen_object(e: &mut JsEngine, def: &Rc<FnDef>, env: &Rc<Env>) -> Result<Val, JsError> {
    let vm = match GenVm::root_stmts(def) {
        Some(stmts) => {
            // hoist var/function-decl names into the call env before first
            // resume (same textual hoisting the normal path does)
            let stmts_ref: &[parser::Stmt] = &stmts;
            interp::hoist(e, env, stmts_ref);
            GenVm {
                def: def.clone(),
                frames: vec![GFrame::Batch {
                    stmts,
                    i: 0,
                    env: env.clone(),
                }],
                done: false,
                running: false,
                pending: None,
            }
        }
        None => {
            return Err(JsError::throw(
                "TypeError: generators require a block body",
            ))
        }
    };
    let vm = Rc::new(RefCell::new(vm));
    Ok(Val::Obj(Rc::new(RefCell::new(Obj {
        kind: ObjKind::Generator(vm),
        props: Vec::new(),
        proto: None,
    }))))
}

// ---------------------------------------------------------------------------
// .next / .return protocol
// ---------------------------------------------------------------------------

fn result_val(v: Val, done: bool) -> Val {
    let o = Val::new_object();
    let orc = obj_of(&o);
    obj_set_own(&orc, "value", v);
    obj_set_own(&orc, "done", Val::Bool(done));
    o
}

fn gen_vm(this: &Val) -> Result<Rc<RefCell<GenVm>>, JsError> {
    match this {
        Val::Obj(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Generator(vm) => Ok(vm.clone()),
                _ => Err(JsError::throw("TypeError: generator method on non-generator")),
            }
        }
        _ => Err(JsError::throw("TypeError: generator method on non-generator")),
    }
}

/// `.next(arg)` â€” run until the next yield, or completion
pub(crate) fn gen_next(e: &mut JsEngine, this: &Val, arg: Val) -> Result<Val, JsError> {
    let vm = gen_vm(this)?;
    // reentrant `.next` on a running generator is an honest TypeError (a
    // RefCell borrow would panic â€” this check converts it to an error)
    if vm.try_borrow_mut().is_err() {
        return Err(JsError::throw(
            "TypeError: generator is already running (reentrant .next)",
        ));
    }
    let mut vm = vm.borrow_mut();
    if vm.done {
        return Ok(result_val(Val::Undefined, true));
    }
    vm.running = true;
    // deliver the resume value to the suspended yield's declarator (if any)
    if let Some((bind, env)) = vm.pending.take() {
        match bind {
            PendBind::Name { kind, name } => match kind {
                parser::VarKind::Var => e.env_set(&env, &name, arg)?,
                // let/const in a resumed loop body must replace the prior
                // iteration's binding, not append a shadowed duplicate
                k => env.rebind(name, arg),
            },
            PendBind::Pat { kind, pat } => {
                interp::bind_pat(e, &env, &pat, arg, interp::BindKind::Decl(kind))?;
            }
        }
    }
    let out = match drive(e, &mut vm) {
        Ok(out) => out,
        Err(err) => {
            // the generator body threw: mark it done and propagate (the
            // standard resume-time rethrow semantics)
            vm.done = true;
            vm.frames.clear();
            vm.running = false;
            return Err(err);
        }
    };
    vm.running = false;
    Ok(match out {
        GOut::Yield(v) => result_val(v, false),
        GOut::Ret(v) => {
            vm.done = true;
            vm.frames.clear();
            result_val(v, true)
        }
    })
}

/// `.return(v)` â€” minimal: terminate the body now with value v
/// (try/finally interaction cannot occur: the parser rejects try-with-yield)
pub(crate) fn gen_return(this: &Val, v: Val) -> Result<Val, JsError> {
    let vm = gen_vm(this)?;
    let mut vm = vm.borrow_mut();
    if vm.done || vm.running {
        // return on a completed generator reports { done: true, value: v }
        return Ok(result_val(v, true));
    }
    vm.done = true;
    vm.frames.clear();
    Ok(result_val(v, true))
}

// ---------------------------------------------------------------------------
// the driver
// ---------------------------------------------------------------------------

fn drive(e: &mut JsEngine, vm: &mut GenVm) -> Result<GOut, JsError> {
    loop {
        // own tick: a cond-less generator loop re-pushes frames without
        // passing through eval_stmt (empty body), so the driver bounds it
        e.tick()?;
        let frame = match vm.frames.pop() {
            None => {
                // body completed without an explicit return
                return Ok(GOut::Ret(Val::Undefined));
            }
            Some(f) => f,
        };
        match step_frame(e, vm, frame)? {
            StepR::Susp(v) => return Ok(GOut::Yield(v)),
            StepR::Finished | StepR::NewFrame => {}
            StepR::Retr(v) => return Ok(GOut::Ret(v)),
            StepR::BreakFlow => {
                // discard frames until the innermost loop/switch construct
                while let Some(f) = vm.frames.pop() {
                    let is_target = matches!(
                        f,
                        GFrame::SwitchBody { .. }
                            | GFrame::While { .. }
                            | GFrame::DoWhile { .. }
                            | GFrame::For { .. }
                            | GFrame::ForEach { .. }
                    );
                    if is_target {
                        break;
                    }
                }
            }
            StepR::ContinueFlow => {
                // the enclosing loop frame is right below; `continue` amounts
                // to re-stepping it (which re-evaluates update + condition),
                // and switch frames are transparent for continue flows
            }
        }
    }
}

fn step_frame(e: &mut JsEngine, vm: &mut GenVm, frame: GFrame) -> Result<StepR, JsError> {
    match frame {
        GFrame::Batch { stmts, mut i, env } => {
            while i < stmts.len() {
                let st = stmts[i].clone();
                i += 1;
                match step_stmt(e, vm, &st, &env)? {
                    SStep::Done | SStep::Pushed => {}
                    SStep::Flow(Flow::Return(v)) => return Ok(StepR::Retr(v)),
                    SStep::Flow(Flow::Break) => return Ok(StepR::BreakFlow),
                    SStep::Flow(Flow::Continue) => return Ok(StepR::ContinueFlow),
                    SStep::Flow(Flow::Normal) => {}
                    SStep::Yield(v) => {
                        // push the remnant so resumption lands after `i`
                        vm.frames.push(GFrame::Batch {
                            stmts: stmts.clone(),
                            i,
                            env: env.clone(),
                        });
                        return Ok(StepR::Susp(v));
                    }
                    SStep::Decls { kind, ds } => {
                        // push the remnant batch first so resumption (after
                        // a possibly-suspending declarator) lands on the
                        // statement following the declaration
                        vm.frames.push(GFrame::Batch {
                            stmts: stmts.clone(),
                            i,
                            env: env.clone(),
                        });
                        vm.frames.push(GFrame::Decls {
                            kind,
                            ds,
                            j: 0,
                            env: env.clone(),
                        });
                        return Ok(StepR::NewFrame);
                    }
                }
            }
            Ok(StepR::Finished)
        }
        GFrame::Decls {
            kind,
            ds,
            mut j,
            env,
        } => {
            while j < ds.len() {
                let d = ds[j].clone();
                j += 1;
                let yield_init = matches!(
                    d.init.as_ref().map(|x| &x.kind),
                    Some(parser::ExprKind::Yield(_))
                );
                if yield_init {
                    let v = match d.init.as_ref().map(|x| &x.kind) {
                        Some(parser::ExprKind::Yield(Some(op))) => eval_expr(e, &env, op)?,
                        None | Some(..) => Val::Undefined, // bare `let x = yield`
                    };
                    let bind = match &d.pat {
                        None => PendBind::Name {
                            kind,
                            name: d.name.clone(),
                        },
                        Some(p) => PendBind::Pat {
                            kind,
                            pat: p.clone(),
                        },
                    };
                    vm.pending = Some((bind, env.clone()));
                    vm.frames.push(GFrame::Decls {
                        kind,
                        ds: ds.clone(),
                        j,
                        env: env.clone(),
                    });
                    return Ok(StepR::Susp(v));
                }
                let v = match &d.init { 
                    Some(x) => eval_expr(e, &env, x)?,
                    None => Val::Undefined,
                };
                match (&d.pat, kind) {
                    (Some(pat), _) => {
                        interp::bind_pat(e, &env, pat, v, interp::BindKind::Decl(kind))?
                    }
                    (None, parser::VarKind::Var) => e.env_set(&env, &d.name, v)?,
                    (None, k) => {
                        // loop re-entry re-runs this decl each iteration in
                        // the shared body env: replace, don't append
                        env.rebind(d.name.clone(), v)
                    }
                }
            }
            Ok(StepR::Finished)
        }
        GFrame::While { test, body, env } => {
            let t = eval_expr(e, &env, &test)?.to_bool();
            if t {
                vm.frames.push(GFrame::While {
                    test: test.clone(),
                    body: body.clone(),
                    env: env.clone(),
                });
                vm.frames.push(GFrame::Batch {
                    stmts: body.clone(),
                    i: 0,
                    env: env.clone(),
                });
                Ok(StepR::NewFrame)
            } else {
                Ok(StepR::Finished)
            }
        }
        GFrame::DoWhile {
            mut first,
            test,
            body,
            env,
        } => {
            if !first {
                let t = eval_expr(e, &env, &test)?.to_bool();
                if !t {
                    return Ok(StepR::Finished);
                }
            }
            first = false;
            vm.frames.push(GFrame::DoWhile {
                first,
                test: test.clone(),
                body: body.clone(),
                env: env.clone(),
            });
            vm.frames.push(GFrame::Batch {
                stmts: body.clone(),
                i: 0,
                env: env.clone(),
            });
            Ok(StepR::NewFrame)
        }
        GFrame::For {
            first,
            cond,
            upd,
            body,
            env,
        } => {
            // re-entry runs the update expression before the condition
            // test (the loop var lives in the For frame's env)
            if !first {
                if let Some(u) = &upd {
                    eval_expr(e, &env, u)?;
                }
            }
            let go = match &cond {
                None => true,
                Some(c) => eval_expr(e, &env, c)?.to_bool(),
            };
            if !go {
                return Ok(StepR::Finished);
            }
            vm.frames.push(GFrame::For {
                first: false,
                cond: cond.clone(),
                upd,
                body: body.clone(),
                env: env.clone(),
            });
            vm.frames.push(GFrame::Batch {
                stmts: body.clone(),
                i: 0,
                env: env.clone(),
            });
            Ok(StepR::NewFrame)
        }
        GFrame::ForEach {
            target,
            kind,
            items,
            mut idx,
            body,
            env,
        } => {
            if idx >= items.len() {
                return Ok(StepR::Finished);
            }
            let v = items[idx].clone();
            idx += 1;
            // fresh env per iteration for let/const (closure capture sanity)
            let loop_env = match kind.unwrap_or(parser::VarKind::Var) {
                k @ (parser::VarKind::Let | parser::VarKind::Const) => {
                    let benv = Env::child(&env);
                    match &target {
                        parser::ForTarget::Name(n) => {
                            benv.declare(n.clone(), v, k == parser::VarKind::Const)
                        }
                        parser::ForTarget::Pat(_) => {
                            interp::bind_pat(
                                e,
                                &benv,
                                &target_pat(&target),
                                v,
                                interp::BindKind::Decl(k),
                            )?;
                        }
                    }
                    benv
                }
                parser::VarKind::Var => {
                    match &target {
                        parser::ForTarget::Name(n) => e.env_set(&env, n, v)?,
                        parser::ForTarget::Pat(_) => interp::bind_pat(
                            e,
                            &env,
                            &target_pat(&target),
                            v,
                            interp::BindKind::Decl(parser::VarKind::Var),
                        )?,
                    }
                    env.clone()
                }
            };
            vm.frames.push(GFrame::ForEach {
                target,
                kind,
                items,
                idx,
                body: body.clone(),
                env: env.clone(),
            });
            vm.frames.push(GFrame::Batch {
                stmts: body.clone(),
                i: 0,
                env: loop_env,
            });
            Ok(StepR::NewFrame)
        }
        GFrame::SwitchBody { .. } => {
            // reached only after a `continue` flowed past it (a break pops
            // the frame inside drive); post-continue the switch is finished
            Ok(StepR::Finished)
        }
    }
}

fn target_pat(t: &parser::ForTarget) -> parser::Pat {
    match t {
        parser::ForTarget::Pat(p) => p.clone(),
        parser::ForTarget::Name(_) => unreachable!("target_pat on Name"),
    }
}

// ---------------------------------------------------------------------------
// statement classification
// ---------------------------------------------------------------------------

/// does this statement tree contain a `yield` OUTSIDE nested functions?
fn stmt_has_yield(s: &parser::Stmt) -> bool {
    fn w(s: &parser::Stmt) -> bool {
        match &s.kind {
            parser::StmtKind::Expr(x) => expr_has_yield(x),
            parser::StmtKind::VarDecl(_, ds) => ds
                .iter()
                .any(|d| d.init.as_ref().map_or(false, expr_has_yield)),
            parser::StmtKind::Block(b) | parser::StmtKind::List(b) => b.iter().any(w),
            parser::StmtKind::If { test, then, other } => {
                expr_has_yield(test)
                    || w(then)
                    || other.as_deref().map_or(false, w)
            }
            parser::StmtKind::While { test, body } => expr_has_yield(test) || w(body),
            parser::StmtKind::DoWhile { body, test } => w(body) || expr_has_yield(test),
            parser::StmtKind::For { init, cond, upd, body } => {
                init.as_deref().map_or(false, w)
                    || cond.as_ref().map_or(false, expr_has_yield)
                    || upd.as_ref().map_or(false, expr_has_yield)
                    || w(body)
            }
            parser::StmtKind::ForIn { obj, body, .. } => {
                expr_has_yield(obj) || w(body)
            }
            parser::StmtKind::ForOf { iter, body, .. } => {
                expr_has_yield(iter) || w(body)
            }
            parser::StmtKind::Switch { disc, cases } => {
                expr_has_yield(disc)
                    || cases.iter().any(|c| {
                        c.test.as_ref().map_or(false, expr_has_yield)
                            || c.body.iter().any(w)
                    })
            }
            parser::StmtKind::Try {
                block,
                catch,
                finally,
            } => {
                block.iter().any(w)
                    || catch
                        .as_ref()
                        .map_or(false, |c| c.block.iter().any(w))
                    || finally.as_ref().map_or(false, |f| f.iter().any(w))
            }
            parser::StmtKind::Throw(x) => expr_has_yield(x),
            parser::StmtKind::Return(o) => o.as_ref().map_or(false, expr_has_yield),
            // nested function bodies own their yields
            parser::StmtKind::FuncDecl(_) | parser::StmtKind::Break | parser::StmtKind::Continue => {
                false
            }
            // per spec these cannot contain a legal yield; consistent
            parser::StmtKind::PatAssign { .. } | parser::StmtKind::DefineAcc { .. } => false,
        }
    }
    w(s)
}

fn expr_has_yield(x: &parser::Expr) -> bool {
    use parser::ExprKind::*;
    match &x.kind {
        Yield(_) => true,
        Ident(_) | Num(_) | Str(_) | Bool(_) | Null | This | Regex { .. } => false,
        Unary { expr, .. } | Typeof(expr) | Void(expr) | Delete(expr) | Await(expr) => {
            expr_has_yield(expr)
        }
        Postfix { target, .. } => expr_has_yield(target),
        Binary { l, r, .. } | Logical { l, r, .. } => expr_has_yield(l) || expr_has_yield(r),
        Assign {
            target,
            value,
            op: _,
        } => expr_has_yield(target) || expr_has_yield(value),
        Cond { test, then, other } => {
            expr_has_yield(test) || expr_has_yield(then) || expr_has_yield(other)
        }
        Call { callee, args } | New { callee, args } => {
            expr_has_yield(callee)
                || args.iter().any(|a| match a {
                    parser::CallArg::Arg(x) | parser::CallArg::Spread(x) => expr_has_yield(x),
                })
        }
        Member { base, .. } => expr_has_yield(base),
        Index { base, index } => expr_has_yield(base) || expr_has_yield(index),
        Array(items) => items
            .iter()
            .any(|i| match i {
                parser::ArrayElem::Item(x) | parser::ArrayElem::Spread(x) => expr_has_yield(x),
                parser::ArrayElem::Hole => false,
            }),
        Object(props) => props
            .iter()
            .any(|p| match p {
                parser::ObjElem::Prop(_, v) => expr_has_yield(v),
                parser::ObjElem::Spread(v) => expr_has_yield(v),
            }),
        PatAssign { pat: _, value } => expr_has_yield(value),
        Func(_) | Arrow(_) => false,
        Seq(list) => list.iter().any(expr_has_yield),
        Template(segs) => segs.iter().any(|s| match s {
            parser::TplSeg::Lit(_) => false,
            parser::TplSeg::Expr(x) => expr_has_yield(x),
        }),
    }
}

/// normalize a loop/if body statement into a batch of statements
fn batch_of(st: &parser::Stmt) -> Rc<Vec<parser::Stmt>> {
    match &st.kind {
        parser::StmtKind::Block(b) => Rc::new(b.clone()),
        _ => Rc::new(vec![st.clone()]),
    }
}

/// classify + execute one statement for the generator walker
fn step_stmt(
    e: &mut JsEngine,
    vm: &mut GenVm,
    st: &parser::Stmt,
    env: &Rc<Env>,
) -> Result<SStep, JsError> {
    if !stmt_has_yield(st) {
        // no suspension point: run it wholesale in the interpreter (this is
        // also the fast path â€” loops without yield pay no frame overhead)
        let f = interp::eval_stmt(e, env, st)?;
        return Ok(match f {
            Flow::Normal => SStep::Done,
            other => SStep::Flow(other),
        });
    }
    match &st.kind {
        parser::StmtKind::Expr(x) => {
            if let parser::ExprKind::Yield(op) = &x.kind {
                let v = match op {
                    Some(op) => eval_expr(e, env, op)?, // `yield` operand is a full assignment expression
                    None => Val::Undefined,
                };
                Ok(SStep::Yield(v))
            } else {
                // parser guarantees the only yield-bearing expression
                // statement is a plain `yield`
                unreachable!("nested yield leaked past the parser")
            }
        }
        parser::StmtKind::VarDecl(kind, ds) => Ok(SStep::Decls {
            kind: *kind,
            ds: Rc::new(ds.clone()),
        }),
        parser::StmtKind::If { test, then, other } => {
            if eval_expr(e, env, test)?.to_bool() {
                vm.frames.push(GFrame::Batch {
                    stmts: batch_of(then),
                    i: 0,
                    env: env.clone(),
                });
            } else if let Some(o) = other {
                vm.frames.push(GFrame::Batch {
                    stmts: batch_of(o),
                    i: 0,
                    env: env.clone(),
                });
            }
            Ok(SStep::Pushed)
        }
        parser::StmtKind::While { test, body } => {
            vm.frames.push(GFrame::While {
                test: Rc::new(test.clone()),
                body: batch_of(body),
                env: env.clone(),
            });
            Ok(SStep::Pushed)
        }
        parser::StmtKind::DoWhile { body, test } => {
            vm.frames.push(GFrame::DoWhile {
                first: true,
                test: Rc::new(test.clone()),
                body: batch_of(body),
                env: env.clone(),
            });
            Ok(SStep::Pushed)
        }
        parser::StmtKind::For {
            init,
            cond,
            upd,
            body,
        } => {
            let lenv = Env::child(env);
            if let Some(i) = init {
                // cannot contain a yield (parse-time restriction)
                interp::eval_stmt(e, &lenv, i)?;
            }
            vm.frames.push(GFrame::For {
                first: true,
                cond: cond.as_ref().map(|c| Rc::new(c.clone())),
                upd: upd.as_ref().map(|u| Rc::new(u.clone())),
                body: batch_of(body),
                env: lenv,
            });
            Ok(SStep::Pushed)
        }
        parser::StmtKind::ForIn {
            target,
            obj,
            body,
            kind,
        } => {
            let o = eval_expr(e, env, obj)?;
            let keys: Vec<Val> = interp::for_in_keys(&o)
                .into_iter()
                .map(Val::Str)
                .collect();
            vm.frames.push(GFrame::ForEach {
                target: target.clone(),
                kind: *kind,
                items: keys,
                idx: 0,
                body: batch_of(body),
                env: env.clone(),
            });
            Ok(SStep::Pushed)
        }
        parser::StmtKind::ForOf {
            target,
            iter,
            body,
            kind,
        } => {
            let it = eval_expr(e, env, iter)?;
            let items = interp::for_of_items(&it)?;
            vm.frames.push(GFrame::ForEach {
                target: target.clone(),
                kind: *kind,
                items,
                idx: 0,
                body: batch_of(body),
                env: env.clone(),
            });
            Ok(SStep::Pushed)
        }
        parser::StmtKind::Switch { disc, cases } => {
            let d = eval_expr(e, env, disc)?;
            // mirror the interpreter's entry-case resolution; `break` inside
            // exits via the BreakFlow handling in `drive`
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
                None => return Ok(SStep::Done),
                Some(i) => i,
            };
            let mut stitched: Vec<parser::Stmt> = Vec::new();
            for c in &cases[start..] {
                stitched.extend(c.body.iter().cloned());
            }
            let senv = Env::child(env);
            vm.frames.push(GFrame::SwitchBody {
                batch: Rc::new(stitched),
                env: senv,
            });
            Ok(SStep::Pushed)
        }
        // try-with-yield is a parse error; anything else with yield is
        // rejected at parse time as well
        _ => unreachable!("yield-bearing statement not in the walker's grammar"),
    }
}
