//! Unit + end-to-end tests for the JS interpreter. Each test runs a script
//! string through `JsEngine::exec` and asserts the resulting `Val`.

use super::*;

/// A page's engine must release its global environment when it is dropped.
/// Functions installed on the global object capture the global env, so an
/// unbroken `Rc` cycle would leak the whole engine heap on every navigation
/// (the reload-soak test caught exactly that: ~40 KB per reload).
#[test]
fn dropping_the_engine_releases_the_global_env() {
    let mut e = JsEngine::new();
    // a global function captures the global env -> env -> binding -> closure
    e.exec("function f() { return f; }").unwrap();
    let weak_env = Rc::downgrade(&e.global_env);
    let weak_obj = Rc::downgrade(&e.global_obj);
    drop(e);
    assert!(
        weak_env.upgrade().is_none(),
        "global env leaked through the closure cycle"
    );
    assert!(
        weak_obj.upgrade().is_none(),
        "global object leaked through the closure cycle"
    );
}

/// Page scripts also build cycles that never touch an environment binding:
/// `a.b = b; b.a = a`, or a function that stores itself on its own property.
/// `Rc` cannot collect those, so `JsEngine::drop` must clear the page's
/// object and function edges too, not only scope bindings.
#[test]
fn dropping_the_engine_releases_object_cycles() {
    let mut e = JsEngine::new();
    e.exec(
        "var a = {}; var b = { a: a }; a.b = b; \
         function f() { return f; } f.self = f; a.f = f;",
    )
    .unwrap();
    let a = match e.get_global("a") {
        Val::Obj(o) => o,
        v => panic!("expected object, got {v:?}"),
    };
    let f = match e.get_global("f") {
        Val::Func(c) => c,
        v => panic!("expected function, got {v:?}"),
    };
    let weak_a = Rc::downgrade(&a);
    let weak_f = Rc::downgrade(&f);
    drop(a);
    drop(f);
    drop(e);
    assert!(weak_a.upgrade().is_none(), "object cycle leaked");
    assert!(weak_f.upgrade().is_none(), "function self-cycle leaked");
}

fn run(src: &str) -> Val {
    let mut e = JsEngine::new();
    e.exec(src).unwrap_or_else(|err| panic!("exec failed: {}\nsrc: {}", err, src))
}

fn num(src: &str) -> f64 {
    match run(src) {
        Val::Num(n) => n,
        v => panic!("expected number, got {:?} (src: {})", v, src),
    }
}

fn string(src: &str) -> String {
    run(src).to_string()
}

fn boolean(src: &str) -> bool {
    match run(src) {
        Val::Bool(b) => b,
        v => panic!("expected bool, got {:?}", v),
    }
}

fn err(src: &str) -> JsError {
    let mut e = JsEngine::new();
    e.exec(src).err().unwrap_or_else(|| panic!("expected error for: {}", src))
}

/// Microtask effects are asynchronous by definition: `exec` returns the
/// *synchronous* completion value before queued reactions run. Promise/
/// async tests therefore store their answer in a global named `m` and
/// probe it with a follow-up exec (which itself runs after the drain).
fn mrun(src: &str) -> Val {
    let mut e = JsEngine::new();
    e.exec(src)
        .unwrap_or_else(|err| panic!("exec failed: {}\nsrc: {}", err, src));
    match e.exec("m") {
        Ok(v) => v,
        Err(err) => panic!("probe failed: {}\nsrc: {}", err, src),
    }
}

fn mnum(src: &str) -> f64 {
    match mrun(src) {
        Val::Num(n) => n,
        v => panic!("expected number, got {:?} (src: {})", v, src),
    }
}

fn mstring(src: &str) -> String {
    mrun(src).to_string()
}

fn mbool(src: &str) -> bool {
    match mrun(src) {
        Val::Bool(b) => b,
        v => panic!("expected bool, got {:?} (src: {})", v, src),
    }
}

// ---------------------------------------------------------------------------
// lexer/parser errors
// ---------------------------------------------------------------------------

#[test]
fn syntax_error_is_err_not_panic() {
    match err("let = ;") {
        JsError::Syntax { .. } => {}
        other => panic!("expected SyntaxError, got {:?}", other),
    }
}

#[test]
fn deep_expression_nesting_is_err_not_panic() {
    // 50k nested parens must be a clean SyntaxError, not a native stack
    // overflow. Debug-build parser frames are fat, so run this on a thread
    // with a generous stack: the parser's own MAX_NEST_DEPTH guard must fire
    // first, exactly like it will in the release browser build.
    let src = format!("{}1{}", "(".repeat(50_000), ")".repeat(50_000));
    let h = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let mut e = JsEngine::new();
            assert!(e.exec(&src).is_err());
        })
        .unwrap();
    h.join().unwrap();
}

#[test]
fn unterminated_string_is_err() {
    match err("let x = \"abc") {
        JsError::Syntax { .. } => {}
        other => panic!("expected SyntaxError, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// values, operators
// ---------------------------------------------------------------------------

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(num("1 + 2 * 3"), 7.0);
    assert_eq!(num("(1 + 2) * 3"), 9.0);
    assert_eq!(num("10 % 3"), 1.0);
    assert_eq!(num("7 / 2"), 3.5);
    // `**` is right-associative and binds tighter than `*`
    assert_eq!(num("2 ** 3"), 8.0);
    assert_eq!(num("2 ** 3 ** 2"), 512.0);
    assert_eq!(num("2 * 3 ** 2"), 18.0);
    assert_eq!(num("2 ** -1"), 0.5);
    assert_eq!(num("var a = 3; a **= 2; a"), 9.0);
    assert_eq!(num("var a = 5; a += 3; a"), 8.0);
    assert_eq!(num("var a = 5; a -= 3; a"), 2.0);
    assert_eq!(num("var a = 5; a *= 3; a"), 15.0);
    assert_eq!(num("var a = 6; a /= 3; a"), 2.0);
    assert_eq!(num("var a = 7; a %= 4; a"), 3.0);
}

#[test]
fn unary_and_incr_decr() {
    assert_eq!(num("-5 + +2"), -3.0);
    assert_eq!(num("var a = 5; a++; a"), 6.0);
    assert_eq!(num("var a = 5; a++"), 5.0); // postfix returns old value
    assert_eq!(num("var a = 5; ++a"), 6.0);
    assert_eq!(num("var a = 5; a--"), 5.0);
    assert_eq!(num("var a = 5; --a"), 4.0);
    assert!(boolean("!0"));
    assert!(boolean("!''"));
    assert!(!boolean("!1"));
    assert_eq!(num("~5"), -6.0);
}

#[test]
fn comparisons_and_equality() {
    assert!(boolean("1 < 2"));
    assert!(boolean("2 <= 2"));
    assert!(boolean("3 > 2"));
    assert!(boolean("2 >= 3 == false"));
    assert!(boolean("'abc' < 'abd'"));
    // strict
    assert!(boolean("1 === 1"));
    assert!(!boolean("1 === '1'"));
    assert!(boolean("NaN !== NaN"));
    // loose coercions
    assert!(boolean("1 == '1'"));
    assert!(boolean("null == undefined"));
    assert!(!boolean("null === undefined"));
    assert!(boolean("0 == false"));
    assert!(boolean("'' == false"));
    assert!(boolean("1 == true"));
    assert!(boolean("'1' == true"));
    assert!(!boolean("1 != '1'"));
}

#[test]
fn truthiness_and_logical() {
    assert!(boolean("[] && true")); // empty array is truthy
    assert_eq!(num("0 || 5"), 5.0);
    assert_eq!(num("3 || 5"), 3.0);
    assert_eq!(num("null ?? 42"), 42.0);
    assert_eq!(num("0 ?? 42"), 0.0); // 0 is NOT nullish
    assert_eq!(num("undefined ?? 7"), 7.0);
    assert_eq!(string("null || 'x'"), "x");
    // && returns operand, not bool
    assert_eq!(num("5 && 3"), 3.0);
}

#[test]
fn ternary_comma_void_typeof() {
    assert_eq!(num("true ? 1 : 2"), 1.0);
    assert_eq!(num("false ? 1 : 2"), 2.0);
    assert_eq!(num("(1, 2, 3)"), 3.0);
    assert!(run("void 0").is_undefined());
    assert_eq!(string("typeof 1"), "number");
    assert_eq!(string("typeof 'a'"), "string");
    assert_eq!(string("typeof true"), "boolean");
    assert_eq!(string("typeof undefined"), "undefined");
    assert_eq!(string("typeof null"), "object");
    assert_eq!(string("typeof {}"), "object");
    assert_eq!(string("typeof function(){}"), "function");
    assert_eq!(string("typeof notDeclaredAnywhere"), "undefined"); // no throw
}

#[test]
fn bitwise_ops() {
    assert_eq!(num("5 & 3"), 1.0);
    assert_eq!(num("5 | 3"), 7.0);
    assert_eq!(num("5 ^ 3"), 6.0);
    assert_eq!(num("1 << 4"), 16.0);
    assert_eq!(num("-16 >> 2"), -4.0);
    assert_eq!(num("-1 >>> 28"), 15.0);
}

// ---------------------------------------------------------------------------
// declarations & control flow
// ---------------------------------------------------------------------------

#[test]
fn var_let_const() {
    assert_eq!(num("var a = 1; a"), 1.0);
    assert_eq!(num("let a = 2; a"), 2.0);
    assert_eq!(num("const a = 3; a"), 3.0);
    // var hoisting: readable (as undefined) before the declaration line
    assert_eq!(string("typeof x; var x = 5; typeof x"), "number");
    assert!(boolean("var y; typeof y === 'undefined'"));
    // const reassignment throws
    match err("const a = 1; a = 2;") {
        JsError::Throw(_) => {}
        other => panic!("expected Throw, got {:?}", other),
    }
    // sloppy implicit globals
    assert_eq!(num("undeclaredGlobal = 9; undeclaredGlobal"), 9.0);
}

#[test]
fn block_scoping() {
    // a block doesn't leak its lets
    assert_eq!(num("{ let x = 5; } let x = 7; x"), 7.0);
    // var escapes the block
    assert_eq!(num("{ var y = 4; } y"), 4.0);
    // let does not
    assert_eq!(run("let z = 1; { let z = 2; } z").as_number(), Some(1.0));
}

#[test]
fn if_else() {
    assert_eq!(num("let r; if (true) { r = 1 } else { r = 2 }; r"), 1.0);
    assert_eq!(num("let r; if (false) { r = 1 } else if (true) { r = 3 } else { r = 2 }; r"), 3.0);
    assert!(run("if (0) 1;").is_undefined());
    // single-statement bodies
    assert_eq!(num("let r = 0; if (true) r = 5; r"), 5.0);
}

#[test]
fn while_do_for_loops() {
    assert_eq!(num("let i = 0; while (i < 5) { i++ } i"), 5.0);
    assert_eq!(num("let i = 0; do { i++ } while (i < 3); i"), 3.0);
    assert_eq!(num("let s = 0; for (let i = 0; i <= 10; i++) { s += i } s"), 55.0);
    // break/continue
    assert_eq!(
        num("let s = 0; for (let i = 0; i < 10; i++) { if (i > 3) break; if (i == 2) continue; s += i } s"),
        4.0 // 0+1+3
    );
    // empty for clauses
    assert_eq!(num("let i = 0; for (;;) { i++; if (i > 2) break; } i"), 3.0);
}

#[test]
fn switch_statement() {
    let src = "function f(x) { switch (x) { case 1: return 'one'; case 2: return 'two'; default: return 'many'; } }
               f(2) + ',' + f(9) + ',' + f(1)";
    assert_eq!(string(src), "two,many,one");
    // fallthrough
    let src2 = "let out = ''; switch (2) { case 1: out += 'a'; case 2: out += 'b'; case 3: out += 'c'; break; default: out += 'd'; } out";
    assert_eq!(string(src2), "bc");
}

#[test]
fn for_in_and_for_of() {
    assert_eq!(
        string("let ks = []; let o = {a: 1, b: 2}; for (let k in o) { ks.push(k) } ks.join(',')"),
        "a,b"
    );
    assert_eq!(
        num("let s = 0; let a = [10, 20, 30]; for (let v of a) { s += v } s"),
        60.0
    );
    assert_eq!(
        string("let out = ''; for (let c of 'hi') { out += c + '.' } out"),
        "h.i."
    );
    // for-in over array gives indices
    assert_eq!(
        string("let ks = ''; for (let k in [7, 8]) { ks += k } ks"),
        "01"
    );
}

// ---------------------------------------------------------------------------
// functions
// ---------------------------------------------------------------------------

#[test]
fn function_declaration_and_call() {
    assert_eq!(num("function f(x) { return x * 2 } let a = 1; f(a)"), 2.0);
    // hoisting: call before declaration
    assert_eq!(num("g(); function g() { return 42 }"), 42.0);
    // function expression
    assert_eq!(num("let f = function(x) { return x + 1 }; f(1)"), 2.0);
    // named function expression sees its own name
    assert_eq!(num("let f = function fact(n) { return n <= 1 ? 1 : n * fact(n - 1) }; f(5)"), 120.0);
    // default params
    assert_eq!(num("function f(a, b = 10) { return a + b } f(5)"), 15.0);
    assert_eq!(num("function f(a, b = 10) { return a + b } f(5, 1)"), 6.0);
    // a trailing comma is legal in a call or `new` argument list
    assert_eq!(num("function f(a, b) { return a + b } f(1, 2,)"), 3.0);
    assert_eq!(num("function C(v) { this.v = v } new C(7,).v"), 7.0);
    assert_eq!(num("var o = { f(a) { return a * 3 } }; o.f(4,)"), 12.0);
}

#[test]
fn anonymous_function_name_inference() {
    assert_eq!(string("var fn = function () {}; fn.name"), "fn");
    assert_eq!(string("var g = function* () {}; g.name"), "g");
    assert_eq!(string("var a = () => {}; a.name"), "a");
    assert_eq!(string("const [x = function () {}] = []; x.name"), "x");
    assert_eq!(string("const { y = () => {} } = {}; y.name"), "y");
    assert_eq!(string("let z; z = function () {}; z.name"), "z");
    assert_eq!(
        string("function f(cb = function () {}) { return cb.name } f()"),
        "cb"
    );
}

#[test]
fn closures() {
    // counter closure
    let src = "function makeCounter() { let n = 0; return function() { n = n + 1; return n } }
               let c1 = makeCounter(); let c2 = makeCounter();
               c1(); c1(); c2();
               c1() * 10 + c2()";
    assert_eq!(num(src), 32.0); // c1=3, c2=2
    // closure over param
    assert_eq!(num("function adder(x) { return function(y) { return x + y } } adder(3)(4)"), 7.0);
}

#[test]
fn recursion() {
    assert_eq!(num("function fib(n) { return n < 2 ? n : fib(n-1) + fib(n-2) } fib(15)"), 610.0);
}

#[test]
fn stack_overflow_guard_returns_err_not_panic() {
    // debug-build interpreter frames are fat: run the runaway-recursion case
    // on a thread with a generous stack ÃƒÂ¢Ã¢‚Â¬Ã¢€Â the ENGINE's own guard (depth cap)
    // must fire long before this thread's stack runs out, exactly like the
    // release browser build.
    let h = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let mut e = JsEngine::new();
            let r = e.exec("function f(n) { return f(n + 1) } f(0)");
            match r {
                Err(JsError::Throw(v)) => {
                    assert!(v.to_string().contains("RangeError"), "got: {}", v.to_string());
                }
                other => panic!(
                    "expected stack-overflow Throw, got {:?}",
                    other.map(|v| v.to_string())
                ),
            }
        })
        .unwrap();
    h.join().unwrap();
}

#[test]
fn recursion_10000_deep_is_err_not_panic() {
    // A 10000-frame chain is 10x over MAX_CALL_DEPTH (1000): the engine's
    // guard must surface a RangeError Err ÃƒÂ¢Ã¢‚Â¬Ã¢€Â never a native stack overflow or
    // panic. Same generous-stack thread rationale as the runaway case above.
    let h = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            match err("function f(n) { return n <= 0 ? 0 : f(n - 1) + 1 } f(10000)") {
                JsError::Throw(v) => {
                    assert!(v.to_string().contains("RangeError"), "got: {}", v.to_string())
                }
                other => panic!("expected RangeError Err, got {:?}", other),
            }
            // a deep chain just under the cap still evaluates normally
            assert_eq!(
                num("function g(n) { return n <= 0 ? 0 : g(n - 1) + 1 } g(500)"),
                500.0
            );
        })
        .unwrap();
    h.join().unwrap();
}

#[test]
fn arrow_functions() {
    assert_eq!(num("let f = (x) => x * 3; f(3)"), 9.0);
    assert_eq!(num("let f = x => x + 1; f(1)"), 2.0);
    assert_eq!(num("let f = (a, b) => a * b; f(3, 4)"), 12.0);
    assert_eq!(num("let f = () => 5; f()"), 5.0);
    // expression vs block body
    assert_eq!(run("let f = () => { return 1 }; f()").as_number(), Some(1.0));
    // lexical this: arrow inside method keeps the method's this
    let src = "let o = { v: 7, get: function() { let f = () => this.v; return f() } }; o.get()";
    assert_eq!(num(src), 7.0);
}

#[test]
fn arguments_object() {
    assert_eq!(num("function f() { return arguments.length } f(1, 2, 3)"), 3.0);
    assert_eq!(num("function f() { return arguments[1] } f(1, 2)"), 2.0);
    // arrow functions don't get their own arguments (falls through to none)
    assert_eq!(run("let f = () => typeof arguments; f()").to_string(), "undefined");
}

// ---------------------------------------------------------------------------
// objects
// ---------------------------------------------------------------------------

#[test]
fn object_literals_and_members() {
    assert_eq!(num("let o = {a: 1, b: {c: 42}}; o.b.c"), 42.0);
    assert_eq!(num("let o = {a: 1}; o['a']"), 1.0);
    assert_eq!(num("let k = 'a'; let o = {a: 3}; o[k]"), 3.0);
    // shorthand + computed-ish via assignment
    assert_eq!(num("let a = 5; let o = {a}; o.a"), 5.0);
    // method shorthand
    assert_eq!(num("let o = { m() { return this.v + 1 }, v: 1 }; o.m()"), 2.0);
    // property write
    assert_eq!(num("let o = {}; o.x = 1; o['y'] = 2; o.x + o.y"), 3.0);
    // delete
    assert!(boolean("let o = {a: 1}; delete o.a; !('a' in o)"));
}

#[test]
fn computed_object_keys() {
    assert_eq!(num("let k = 'a'; let o = {[k]: 1}; o.a"), 1.0);
    assert_eq!(num("let i = 2; let o = {['x' + i]: 5}; o.x2"), 5.0);
    // computed method shorthand
    assert_eq!(num("let k = 'm'; let o = {[k]() { return 7 }}; o.m()"), 7.0);
    // the key expression is evaluated at object-creation time, in order
    assert_eq!(string("let log = ''; let o = {[log += 'a']: 1, [log += 'b']: 2}; log"), "ab");
}

#[test]
fn object_literal_accessors() {
    assert_eq!(num("let o = { get x() { return 42 } }; o.x"), 42.0);
    assert_eq!(num("let o = { get x() { return this._v }, set x(v) { this._v = v * 2 } }; o.x = 3; o.x"), 6.0);
    // a getter is not a data property: reading twice calls it twice
    assert_eq!(num("let n = 0; let o = { get x() { n++; return n } }; o.x + o.x"), 3.0);
    // setters run for member assignment, and `in` still sees the key
    assert!(boolean("let o = { set x(v) {} }; o.x = 1; 'x' in o"));
    // ordinary properties named get/set still work
    assert_eq!(num("let o = {get: 1, set: 2}; o.get + o.set"), 3.0);
    assert_eq!(num("let o = { get() { return 3 } }; o.get()"), 3.0);
}

#[test]
fn this_binding() {
    // method call: this = receiver
    assert_eq!(num("let o = {v: 3, m: function() { return this.v }}; o.m()"), 3.0);
    // plain call: this = global object (sloppy mode, documented)
    assert_eq!(num("globalThis.gv = 9; function f() { return this.gv } f()"), 9.0);
    // top-level `this` is the global object too
    assert!(boolean("this === globalThis"));
    // call/apply/bind
    assert_eq!(num("let o = {v: 4}; function f() { return this.v } f.call(o)"), 4.0);
    assert_eq!(num("let o = {v: 5}; function f(x) { return this.v + x } f.apply(o, [2])"), 7.0);
    assert_eq!(num("let o = {v: 6}; function f() { return this.v } f.bind(o)()"), 6.0);
    // partial application via bind
    assert_eq!(num("function f(a, b) { return a + b } f.bind(null, 1)(2)"), 3.0);
}

#[test]
fn functions_as_objects_prototype_pattern() {
    let src = "function Animal(name) { this.name = name }
               Animal.prototype.speak = function() { return this.name + ' speaks' };
               let a = new Animal('dog');
               a.speak()";
    assert_eq!(string(src), "dog speaks");
    // instanceof
    assert!(boolean("function A(){} let a = new A(); a instanceof A"));
    assert!(!boolean("function A(){} let a = new A(); a instanceof String"));
    // constructor replacement via explicit this
    assert_eq!(num("function F() { this.x = 1 } function G() { return new F() } new G().x"), 1.0);
}

#[test]
fn get_prototype_of_reports_lazy_prototypes() {
    // every object kind inherits from a shared prototype, but the link is
    // attached lazily on first member access. `Object.getPrototypeOf` must
    // report it anyway: core-js's iterator helper reads `.next` off the
    // result and threw on `null`.
    assert!(boolean("Object.getPrototypeOf({}) === Object.prototype"));
    assert!(boolean("Object.getPrototypeOf([]) === Array.prototype"));
    assert!(boolean("Object.getPrototypeOf(Object.prototype) === null"));
    assert!(boolean("Object.getPrototypeOf(Array.prototype) === Object.prototype"));
    assert!(boolean("typeof Object.getPrototypeOf(new Set([1]).values()) === 'object'"));
    // shortcut: built-in iterators have no own prototype object, so they
    // report the same lazy default as any other engine object
    assert!(boolean("Object.getPrototypeOf(new Set([1]).values()) === Object.prototype"));
    assert!(boolean("Object.getPrototypeOf(function(){}) === Function.prototype"));
    assert!(boolean("Object.getPrototypeOf(new Error('x')) === Error.prototype"));
    // instanceof follows the same lazy chain
    assert!(boolean("({}) instanceof Object"));
    assert!(boolean("[] instanceof Object"));
    assert!(boolean("new Set([1]).values() instanceof Object"));
    assert!(boolean("function A(){} new A() instanceof Object"));
    // and instances reach Object.prototype through a user prototype object
    assert!(boolean("function A(){} new A().hasOwnProperty('x') === false"));
}

#[test]
fn classes_desugar() {
    let src = "class Point { constructor(x, y) { this.x = x; this.y = y } len() { return this.x + this.y } }
               let p = new Point(2, 3); p.len() + ':' + (p instanceof Point)";
    assert_eq!(string(src), "5:true");
}

#[test]
fn class_extends_and_super() {
    // derived constructor, inherited method via super, instanceof
    let src = "class A { constructor() { this.a = 1 } hi() { return 'A' } }
               class B extends A { constructor() { super(); this.b = 2 } hi() { return 'B' + super.hi() } }
               const b = new B();
               b.a + ',' + b.b + ',' + b.hi() + ',' + (b instanceof A)";
    assert_eq!(string(src), "1,2,BA,true");
    // inherited method without an override
    assert_eq!(
        num("class A { m() { return 5 } } class B extends A {} new B().m()"),
        5.0
    );
    // default derived constructor forwards arguments
    assert_eq!(
        num("class A { constructor(x) { this.x = x } } class B extends A {} new B(7).x"),
        7.0
    );
    // super.m() keeps `this` bound to the instance
    assert_eq!(
        string(
            "class A { m() { return this.n } } class B extends A { m() { this.n = 3; return super.m() } } new B().m()"
        ),
        "3"
    );
}

#[test]
fn class_static_members() {
    // static method / field / accessor, and static inheritance
    assert_eq!(num("class C { static s() { return 7 } } C.s()"), 7.0);
    assert_eq!(num("class C { static v = 9 } C.v"), 9.0);
    assert_eq!(
        num("class C { static v = 9; static get g() { return 4 } } C.v + C.g"),
        13.0
    );
    assert_eq!(
        num("class A { static s() { return 1 } } class B extends A {} B.s()"),
        1.0
    );
    assert_eq!(
        num("class A { static s() { return 1 } } class B extends A { static s() { return super.s() + 1 } } B.s()"),
        2.0
    );
    // static setter
    assert_eq!(
        num("class T { static _v = 1; static get v() { return T._v } static set v(x) { T._v = x } } T.v = 5; T.v"),
        5.0
    );
}

#[test]
fn class_members_are_not_enumerable() {
    // a class method is an own, non-enumerable property of the prototype
    assert!(boolean(
        "let C = class { m() { return 5 } }; new C().m() === 5"
    ));
    assert!(!boolean(
        "let C = class { m() {} }; Object.getOwnPropertyDescriptor(C.prototype, 'm').enumerable"
    ));
    assert_eq!(
        string("let C = class { m() {} }; Object.keys(C.prototype).length"),
        "0"
    );
    // for-in skips it, while an ordinary own property still shows up
    assert_eq!(
        string(
            "let C = class { m() {} }; let o = new C(); o.x = 1; let s = []; for (let k in o) s.push(k); s.join(',')"
        ),
        "x"
    );
    // accessors and static members are hidden the same way
    assert!(!boolean(
        "let C = class { get g() { return 1 } }; Object.getOwnPropertyDescriptor(C.prototype, 'g').enumerable"
    ));
    assert!(!boolean(
        "let C = class { static s() {} }; Object.getOwnPropertyDescriptor(C, 's').enumerable"
    ));
    // plain object literals keep their enumerable properties
    assert!(boolean(
        "Object.getOwnPropertyDescriptor({ a: 1 }, 'a').enumerable"
    ));
    assert_eq!(string("Object.keys({ a: 1 }).join(',')"), "a");
}

#[test]
fn dynamic_import_call_grammar() {
    // ImportCall is not an identifier expression and takes exactly one
    // argument plus optional import attributes (no spread, no third arg).
    for src in [
        "import()",
        "import('a', {}, '')",
        "import(...xs)",
        "typeof import",
        "import.UNKNOWN('x')",
        "new import('x')",
        "new import('x').prop",
    ] {
        let msg = err(src).to_string();
        assert!(msg.contains("import"), "expected an import error for {src}, got {msg}");
    }
}

#[test]
fn object_assign_accepts_function_target() {
    // a class/function target is a legitimate assignee (`Object.assign(C, ...)`);
    // it must not hit the "builtin installed on non-object" path
    assert_eq!(num("function F() {} Object.assign(F, { a: 4 }); F.a"), 4.0);
    assert_eq!(
        num("class C {} Object.assign(C, { x: 1 }); Object.assign(C.prototype, { y: 2 }); C.x + new C().y"),
        3.0
    );
}

#[test]
fn optional_catch_binding() {
    assert_eq!(num("let n = 0; try { throw 1 } catch { n = 5 } n"), 5.0);
}

#[test]
fn logical_assignment() {
    assert_eq!(num("let a = null; a ??= 5; a"), 5.0);
    assert_eq!(num("let a = 1; a ??= 5; a"), 1.0);
    assert_eq!(num("let o = {}; o.x ||= 2; o.x"), 2.0);
    assert_eq!(num("let o = { x: 3 }; o.x ||= 2; o.x"), 3.0);
    assert_eq!(num("let o = { x: 0 }; o.x &&= 4; o.x"), 0.0);
    assert_eq!(num("let o = { x: 1 }; o.x &&= 4; o.x"), 4.0);
    // the right-hand side is NOT evaluated when the write is skipped
    assert_eq!(num("let a = 1; let n = 0; a ||= (n = 1); n"), 0.0);
    assert_eq!(num("let a = null; let n = 0; a ??= (n = 1); n"), 1.0);
    // member target
    assert_eq!(num("let o = { n: null }; o.n ??= 7; o.n"), 7.0);
}

#[test]
fn labeled_statements() {
    // `continue label` restarts the labeled (outer) loop
    assert_eq!(
        num(
            "let c = 0; outer: for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { if (j === 1) continue outer; c++ } } c"
        ),
        3.0
    );
    // `break label` leaves both loops
    assert_eq!(
        num(
            "let c = 0; outer: for (let i = 0; i < 5; i++) { for (let j = 0; j < 5; j++) { if (i === 2) break outer; c++ } } c"
        ),
        10.0
    );
    // a label on a plain block only observes `break label`
    assert_eq!(num("let c = 0; block: { c = 1; break block; c = 2 } c"), 1.0);
    // unlabeled break still exits the innermost loop
    assert_eq!(
        num("let c = 0; for (let i = 0; i < 3; i++) { if (i === 1) break; c++ } c"),
        1.0
    );
}

#[test]
fn class_expressions() {
    assert_eq!(num("const C = class { m() { return 3 } }; new C().m()"), 3.0);
    assert_eq!(num("const C = class D { m() { return 4 } }; new C().m()"), 4.0);
    assert_eq!(
        num("class A { m() { return 5 } } const B = class extends A {}; new B().m()"),
        5.0
    );
    // a named class expression's binding does not leak out
    assert_eq!(string("const C = class D {}; typeof D"), "undefined");
}

#[test]
fn class_fields() {
    // instance fields initialize before the constructor body runs
    assert_eq!(
        string(
            "class F { x = 1; y; constructor() { this.z = 3 } } const f = new F(); f.x + ',' + f.y + ',' + f.z"
        ),
        "1,undefined,3"
    );
    // derived class: fields run after super(), so they can use `this`
    assert_eq!(
        string(
            "class A { constructor() { this.a = 1 } }
             class B extends A { b = this.a + 1; constructor() { super(); this.c = 3 } }
             const b = new B(); b.a + ',' + b.b + ',' + b.c"
        ),
        "1,2,3"
    );
}

#[test]
fn null_undefined_property_access_throws() {
    match err("let o = null; o.a") {
        JsError::Throw(v) => assert!(v.to_string().contains("TypeError")),
        other => panic!("expected Throw, got {:?}", other),
    }
    match err("undefined.x = 1") {
        JsError::Throw(v) => assert!(v.to_string().contains("TypeError")),
        other => panic!("expected Throw, got {:?}", other),
    }
}

#[test]
fn optional_chaining() {
    // short-circuits a nullish base to undefined, no throw
    assert!(boolean("let o = null; o?.a === undefined"));
    assert!(boolean("let o = undefined; o?.a.b.c === undefined"));
    // a present base behaves like plain member access
    assert_eq!(num("let o = {a: {b: 2}}; o?.a.b"), 2.0);
    // `?.[i]` and `?.()`
    assert_eq!(num("let a = [9]; a?.[0]"), 9.0);
    assert_eq!(num("let o = {v: 4, m() { return this.v }}; o.m?.()"), 4.0);
    assert!(boolean("let f = null; f?.() === undefined"));
    // a non-optional step after a present optional one still throws
    match err("let o = {a: null}; o?.a.b") {
        JsError::Throw(v) => assert!(v.to_string().contains("TypeError")),
        other => panic!("expected Throw, got {:?}", other),
    }
    // the base is evaluated exactly once even when it short-circuits
    assert_eq!(num("let n = 0; function f() { n++; return null } f()?.x; n"), 1.0);
    // chain after a call keeps `this` on the receiver
    assert_eq!(num("let o = {v: 5, get() { return this }}; o.get()?.v"), 5.0);
}

// ---------------------------------------------------------------------------
// arrays
// ---------------------------------------------------------------------------

#[test]
fn boxed_primitives() {
    assert_eq!(num("new Boolean(true) + true"), 2.0);
    assert_eq!(num("true + new Boolean(true)"), 2.0);
    assert_eq!(num("new Boolean(true) + new Boolean(true)"), 2.0);
    assert_eq!(num("new Number(2) * 3"), 6.0);
    assert_eq!(num("new Number(1) + new Number(2)"), 3.0);
    assert_eq!(string("new String('a') + 'b'"), "ab");
    assert_eq!(string("String(new String('q'))"), "q");
    assert_eq!(num("new String('abc').length"), 3.0);
    assert_eq!(string("new String('abc')[1]"), "b");
    assert_eq!(string("new String('abc').toUpperCase()"), "ABC");
    assert_eq!(string("typeof new String('x')"), "object");
    assert!(boolean("new String('x') instanceof String"));
    assert!(boolean("new Number(4) !== 4 && new Number(4) == 4"));
    assert!(boolean("Boolean(new Boolean(false))"));
    assert_eq!(
        string("Object.prototype.toString.call(new Number(1))"),
        "[object Number]"
    );
    assert_eq!(string("JSON.stringify(new Number(3))"), "3");
}

#[test]
fn eval_runs_in_the_calling_scope() {
    assert_eq!(num("eval('1 + 2')"), 3.0);
    // `var` from a direct eval lands in the caller's environment
    assert_eq!(num("function f() { eval('var x = 7'); return x } f()"), 7.0);
    assert_eq!(num("eval('var y = 4'); y"), 4.0);
    // a non-string argument is returned unchanged
    assert_eq!(num("eval(42)"), 42.0);
    assert_eq!(string("eval('\"a\" + \"b\"')"), "ab");
    // a syntax error inside eval is a catchable SyntaxError
    assert!(boolean(
        "var ok = false; try { eval('var') } catch (e) { ok = e instanceof SyntaxError } ok"
    ));
    // indirect eval (through a variable) also runs, in global scope
    assert_eq!(num("var e2 = eval; e2('var z = 9'); z"), 9.0);
    // a shadowed `eval` is an ordinary call
    assert_eq!(
        num("function g(ev) { return ev('abc') } g(function (s) { return s.length })"),
        3.0
    );
    assert_eq!(string("typeof eval"), "function");
}

#[test]
fn strict_directive_needs_a_simple_parameter_list() {
    // a default/rest/destructuring parameter list may not opt into strict
    // mode with a `"use strict"` directive
    for src in [
        "function f(a = 1) { 'use strict'; }",
        "var f = ([a]) => { 'use strict' }",
        "var f = (a, ...r) => { 'use strict' }",
        "({ m(a = 1) { 'use strict' } })",
        "class C { m(a = 1) { 'use strict' } }",
        "class C { set x({ a }) { 'use strict' } }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected SyntaxError for {src}"
        );
    }
    // a simple list is fine, and a string after a real statement is not a
    // directive
    assert_eq!(num("function f(a) { 'use strict'; return a } f(2)"), 2.0);
    assert_eq!(
        num("function f(a = 1) { var x; 'use strict'; return a } f()"),
        1.0
    );
}

#[test]
fn hashbang_comment_at_source_start() {
    assert_eq!(num("#! /usr/bin/env node\nvar a = 1; a"), 1.0);
    // only at position 0; a `#` later in the source is still an error
    assert!(matches!(err("var a = 1; #!x"), JsError::Syntax { .. }));
}

#[test]
fn array_basics() {
    assert_eq!(num("let a = [1, 2, 3]; a.length"), 3.0);
    assert_eq!(num("let a = [1, 2, 3]; a[1]"), 2.0);
    assert_eq!(num("let a = [1, 2]; a[5] = 9; a.length"), 6.0);
    assert_eq!(num("let a = [1, 2]; a.push(3); a.length"), 3.0);
    assert_eq!(num("let a = [1, 2]; a.push(3); a.pop()"), 3.0);
    assert_eq!(num("let a = [1, 2]; a.pop(); a.length"), 1.0);
    assert!(boolean("Array.isArray([1, 2])"));
    assert!(!boolean("Array.isArray({})"));
    // holes/length write
    assert_eq!(num("let a = [1,2,3]; a.length = 1; a.length"), 1.0);
    // elisions: leading `[,a]` (a webpack bundle uses it), interior, `[,]`,
    // and a trailing comma that does NOT add a hole
    assert_eq!(num("[,1].length"), 2.0);
    assert_eq!(string("String([,1][0])"), "undefined");
    assert_eq!(num("[1,,3].length"), 3.0);
    assert_eq!(num("[,].length"), 1.0);
    assert_eq!(num("[,,].length"), 2.0);
    assert_eq!(num("[1,].length"), 1.0);
}

#[test]
fn array_methods() {
    assert_eq!(string("let a = [1,2,3]; a.join('-')"), "1-2-3");
    assert_eq!(num("let a = [10,20,30]; let s = 0; a.forEach(function(v) { s += v }); s"), 60.0);
    assert_eq!(string("let a = [1,2,3]; a.map(function(v) { return v * 2 }).join(',')"), "2,4,6");
    assert_eq!(string("let a = [1,2,3,4]; a.filter(function(v) { return v % 2 == 0 }).join(',')"), "2,4");
    assert_eq!(num("let a = [1,2,3]; a.indexOf(2)"), 1.0);
    assert_eq!(num("let a = [1,2,3]; a.indexOf(9)"), -1.0);
    assert_eq!(num("let a = [1,2,1]; a.lastIndexOf(1)"), 2.0);
    assert_eq!(num("let a = [1,2,1]; a.lastIndexOf(1, 1)"), 0.0);
    assert_eq!(num("let a = [1,2,1]; a.lastIndexOf(9)"), -1.0);
    assert_eq!(string("let a = [1,2,3]; a.slice(1).join(',')"), "2,3");
    assert_eq!(string("let a = [1,2]; a.concat([3], 4).join(',')"), "1,2,3,4");
    assert!(boolean("let a = [1,2]; a.includes(2)"));
    assert_eq!(num("let a = [1,2,3]; a.reduce(function(s, v) { return s + v }, 0)"), 6.0);
    assert_eq!(num("let a = [1,2,3]; a.reduce(function(s, v) { return s + v })"), 6.0); // no init
    assert!(boolean("let a = [1,2]; a.some(function(v) { return v == 2 })"));
    assert!(boolean("let a = [2,4]; a.every(function(v) { return v % 2 == 0 })"));
    // shift/unshift
    assert_eq!(num("let a = [1,2]; a.shift()"), 1.0);
    assert_eq!(string("let a = [2]; a.unshift(1); a.join(',')"), "1,2");
    // callbacks receive (value, index)
    assert_eq!(num("let a = [5]; a.map(function(v, i) { return v + i })[0]"), 5.0);
}

// ---------------------------------------------------------------------------
// strings
// ---------------------------------------------------------------------------

#[test]
fn string_methods() {
    assert_eq!(string("'  hi  '.trim()"), "hi");
    assert_eq!(string("'abc'.toUpperCase()"), "ABC");
    assert_eq!(string("'ABC'.toLowerCase()"), "abc");
    assert_eq!(num("'hello'.length"), 5.0);
    assert_eq!(string("'hello'.slice(1, 3)"), "el");
    assert_eq!(string("'hello'.slice(-2)"), "lo");
    assert_eq!(num("'hello'.indexOf('ll')"), 2.0);
    assert_eq!(num("'hello'.indexOf('z')"), -1.0);
    assert_eq!(num("'hello'.lastIndexOf('l')"), 3.0);
    assert_eq!(num("'hello'.lastIndexOf('l', 2)"), 2.0);
    assert_eq!(num("'hello'.lastIndexOf('z')"), -1.0);
    assert_eq!(num("'abc'.lastIndexOf('')"), 3.0);
    assert!(boolean("'hello'.includes('ell')"));
    assert!(boolean("'hello'.startsWith('he')"));
    assert!(boolean("'hello'.endsWith('lo')"));
    assert_eq!(string("'a,b,c'.split(',').join('|')"), "a|b|c");
    assert_eq!(num("'aaa'.split('a').length"), 4.0); // ['', '', '', '']
    assert_eq!(string("'foo bar'.replace('bar', 'baz')"), "foo baz");
    assert_eq!(string("'aaa'.replace('a', 'z')"), "zaa"); // first occurrence only
    assert_eq!(string("'ab'.repeat(3)"), "ababab");
    assert_eq!(string("'5'.padStart(3, '0')"), "005");
    assert_eq!(string("'5'.padEnd(3, '-')"), "5--");
    assert_eq!(string("'hello'.charAt(1)"), "e");
    assert_eq!(string("'hello'.charAt(99)"), "");
    // indexing reads chars; writes are silent no-ops (sloppy shortcut)
    assert_eq!(string("'hello'[1]"), "e");
    assert_eq!(string("'hello'[99]"), "undefined");
    // concat via +
    assert_eq!(string("'a' + 'b' + 1"), "ab1");
    assert_eq!(string("1 + 'a'"), "1a");
}

#[test]
fn string_repeat_is_ram_capped() {
    match err("'x'.repeat(1e9)") {
        JsError::Throw(v) => assert!(v.to_string().contains("RangeError")),
        other => panic!("expected Throw, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// try/catch/finally, throw
// ---------------------------------------------------------------------------

#[test]
fn throw_catch_string() {
    let src = "let out = ''; try { throw 'boom' } catch (e) { out = 'caught:' + e } out";
    assert_eq!(string(src), "caught:boom");
}

#[test]
fn throw_catch_error_object() {
    let src = "let msg = ''; try { throw new Error('bad') } catch (e) { msg = e.message } msg";
    assert_eq!(string(src), "bad");
    // Error coercion includes name
    assert_eq!(string("String(new TypeError('t'))"), "TypeError: t");
}

#[test]
fn finally_runs_and_overrides() {
    // finally runs on normal completion
    assert_eq!(num("let n = 0; try { n = 1 } finally { n += 10 } n"), 11.0);
    // finally runs even when returning
    let src = "function f() { try { return 1 } finally { } } f()";
    assert_eq!(num(src), 1.0);
    // a return inside finally overrides the pending throw
    let src2 = "function f() { try { throw 'x' } finally { return 7 } } f()";
    assert_eq!(num(src2), 7.0);
}

#[test]
fn uncaught_throw_is_err() {
    match err("throw new Error('top')") {
        JsError::Throw(v) => assert!(v.to_string().contains("top")),
        other => panic!("expected Throw, got {:?}", other),
    }
}

#[test]
fn try_in_real_world_shape() {
    // typical site pattern: guarded feature use
    let src = "let result; try { result = JSON.parse('{\"a\": 1}').a } catch (e) { result = -1 } result";
    assert_eq!(num(src), 1.0);
    let src2 = "let result; try { result = JSON.parse('not json').a } catch (e) { result = -1 } result";
    assert_eq!(num(src2), -1.0);
}

// ---------------------------------------------------------------------------
// builtins
// ---------------------------------------------------------------------------

#[test]
fn math_builtins() {
    assert_eq!(num("Math.floor(1.7)"), 1.0);
    assert_eq!(num("Math.ceil(1.2)"), 2.0);
    assert_eq!(num("Math.round(1.5)"), 2.0);
    assert_eq!(num("Math.round(-1.5)"), -1.0); // JS rounds half toward +inf
    assert_eq!(num("Math.abs(-4)"), 4.0);
    assert_eq!(num("Math.max(1, 9, 3)"), 9.0);
    assert_eq!(num("Math.min(1, -9, 3)"), -9.0);
    assert_eq!(num("Math.pow(2, 10)"), 1024.0);
    assert_eq!(num("Math.sqrt(16)"), 4.0);
    assert_eq!(num("Math.trunc(1.9)"), 1.0);
    assert!(boolean("Math.PI > 3.14 && Math.PI < 3.15"));
    assert!(boolean("Math.random() >= 0 && Math.random() < 1"));
    assert_eq!(num("Math.sign(-9)"), -1.0);
}

#[test]
fn json_round_trip() {
    let src = "let o = {s: 'hi', n: 1.5, b: true, nil: null, arr: [1, 'two', {deep: true}]};
               let j = JSON.stringify(o);
               let back = JSON.parse(j);
               back.s + ':' + back.n + ':' + back.b + ':' + back.nil + ':' + back.arr[1] + ':' + back.arr[2].deep";
    assert_eq!(string(src), "hi:1.5:true:null:two:true");
    // escaping: JS source `JSON.stringify('a"b\nc')` (real newline in string)
    assert_eq!(string("JSON.stringify('a\"b\\nc')"), "\"a\\\"b\\nc\"");
    // NaN/Infinity -> null; undefined props dropped
    assert_eq!(string("JSON.stringify({a: NaN, b: Infinity, c: undefined})"), "{\"a\":null,\"b\":null}");
    // top-level primitives
    assert_eq!(string("JSON.stringify(42)"), "42");
    assert_eq!(string("JSON.stringify('x')"), "\"x\"");
    assert!(run("JSON.stringify(undefined)").is_undefined());
    // arrays with holes
    assert_eq!(string("JSON.stringify([1, undefined, 3])"), "[1,null,3]");
    // unicode escape round-trip
    assert_eq!(string("JSON.parse(JSON.stringify('\\u0041'))"), "A");
    // surrogate pair: our lexer maps lone surrogate escapes to U+FFFD (see
    // lexer.rs shortcut note), so this yields 2 chars ÃƒÂ¢Ã¢‚Â¬Ã¢€Â the same `.length`
    // real JS reports (2 UTF-16 units) for this input.
    assert_eq!(num("JSON.parse('\"\\ud83d\\ude00\"').length"), 2.0);
}

#[test]
fn json_parse_errors_throw() {
    match err("JSON.parse('{')") {
        JsError::Throw(_) => {}
        other => panic!("expected Throw, got {:?}", other),
    }
}

#[test]
fn object_builtins() {
    assert_eq!(string("Object.keys({a: 1, b: 2}).join(',')"), "a,b");
    assert_eq!(string("Object.values({a: 1, b: 2}).join(',')"), "1,2");
    assert_eq!(string("Object.entries({a: 1})[0].join('=')"), "a=1");
    assert_eq!(num("Object.assign({}, {a: 1}, {b: 2}).a + Object.assign({}, {a: 1}, {b: 2}).b"), 3.0);
    // keys of array = indices
    assert_eq!(string("Object.keys([9, 8]).join(',')"), "0,1");
}

#[test]
fn async_generator_functions_parse_and_suspend_like_generators() {
    // shortcut: an async generator's `.next()` yields plain values (no
    // promise wrapping) and `await` runs blocking inside the walker.
    assert_eq!(
        string(
            "async function* g(){ yield 1; yield 2; } var it = g(); \
             it.next().value + ',' + it.next().value"
        ),
        "1,2"
    );
    assert_eq!(
        string(
            "var g = async function*(){ yield 'a'; }; var it = g(); it.next().value"
        ),
        "a"
    );
}

#[test]
fn in_operator_accepts_function_objects() {
    assert!(boolean("'name' in function f() {}"));
    assert!(boolean("'prototype' in function f() {}"));
    assert!(!boolean("'nope' in function f() {}"));
    // built-ins are function objects too: the WordPress runtime does
    // `… in someNative` capability probes
    assert!(boolean("'call' in Object"));
    assert!(boolean("'apply' in Array"));
    assert!(boolean("'name' in JSON.parse"));
    assert!(boolean("'apply' in JSON.parse"));
    assert!(boolean("'call' in function f() {}"));
    // a method value answers the Function.prototype surface, not Object's
    assert!(boolean("'apply' in JSON.parse.call"));
    assert!(!boolean("'toUpperCase' in JSON.parse.call"));
    assert!(!boolean("'nope' in JSON.parse"));
    // ... and still throw on a primitive
    assert!(matches!(err("'x' in 1"), JsError::Throw(_)));
}

#[test]
fn async_generator_parsing_and_for_await() {
    // `for await` parses as for-of (values are not awaited)
    assert_eq!(
        string("var out = ''; async function f(){ for await (var x of [1, 2]) out += x; } f(); out"),
        "12"
    );
    // a yield buried in a logical expression is lifted to statement-level
    // temps, so an async generator now suspends there instead of erroring
    assert_eq!(
        string(
            "var a = true; async function* g(){ (a && (yield 1)); } \
             var it = g(); it.next().value"
        ),
        "1"
    );
}

#[test]
fn set_builtin_dedupes_and_iterates() {
    assert_eq!(num("var s = new Set([1, 2, 2, 3]); s.size"), 3.0);
    assert!(boolean("var s = new Set([1]); s.has(1) && !s.has(2)"));
    assert_eq!(
        string("var s = new Set([1, 2]); s.add(3); s.delete(1); [...s.values()].join(',')"),
        "2,3"
    );
    // real iterator: `.next()` handshake and `keys()`/`entries()`
    assert_eq!(
        string("var it = new Set(['a']).entries().next(); it.value.join(':') + it.done"),
        "a:afalse"
    );
    assert!(boolean("var it = new Set([1]).values(); it[Symbol.iterator]() === it"));
    assert_eq!(
        string("var s = new Set(['a', 'b']); var out = ''; for (var x of s) out += x; out"),
        "ab"
    );
    assert_eq!(string("var s = new Set([1, 2]); [...s].join('+')"), "1+2");
    assert!(boolean("new Set() instanceof Set"));
}

#[test]
fn map_builtin_holds_key_value_pairs() {
    assert_eq!(
        string("var m = new Map([['a', 1], ['b', 2]]); m.get('a') + ',' + m.size"),
        "1,2"
    );
    assert_eq!(
        string("var m = new Map(); m.set('x', 1).set('y', 2); m.get('y')"),
        "2"
    );
    assert_eq!(
        string("var m = new Map(); m.set('x', 1); m.set('x', 9); m.get('x')"),
        "9"
    );
    assert_eq!(
        string("var m = new Map([['a', 1]]); var out = ''; m.forEach(function (v, k) { out += k + v; }); out"),
        "a1"
    );
    assert_eq!(
        string("var m = new Map([['a', 1], ['b', 2]]); Array.from(m.entries()).map(function (e) { return e[0] + e[1]; }).join(',')"),
        "a1,b2"
    );
    assert_eq!(
        string("var m = new Map([['a', 1], ['b', 2]]); var out = ''; for (var k of m.keys()) out += k; out"),
        "ab"
    );
    assert!(boolean("new Map() instanceof Map"));
    assert_eq!(
        string("var m = new Map([['a', 1]]); var out = ''; for (var e of m) out += e[0] + e[1]; out"),
        "a1"
    );
}

#[test]
fn async_and_generator_object_method_shorthand() {
    assert_eq!(
        string("var o = { async m() { return 7; } }; typeof o.m()"),
        "object"
    );
    assert_eq!(
        string("var o = { async m() { return 7; } }; var p = o.m(); typeof p.then"),
        "function"
    );
    assert_eq!(
        string("var o = { *g() { yield 1; yield 2; } }; var it = o.g(); it.next().value"),
        "1"
    );
    assert_eq!(
        string("var o = { async *g() { yield 'z'; } }; var it = o.g(); it.next().value"),
        "z"
    );
    // a plain property named `async` still parses
    assert_eq!(string("var o = { async: 3 }; o.async"), "3");
    assert_eq!(string("var o = { async() { return 4; } }; o.async()"), "4");
}

#[test]
fn arraybuffer_byte_length_accessor() {
    assert_eq!(num("new ArrayBuffer(8).byteLength"), 8.0);
    assert_eq!(
        string("typeof Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, 'byteLength').get"),
        "function"
    );
    assert!(boolean("typeof SharedArrayBuffer.prototype.byteLength !== 'undefined'"));
}

#[test]
fn object_integrity_and_descriptor_helpers() {
    assert!(boolean("Object.is(NaN, NaN) && !Object.is(0, -0) && Object.is(1, 1)"));
    assert!(boolean("var o = Object.freeze({ a: 1 }); o.a === 1"));
    assert!(boolean("Object.hasOwn({ a: 1 }, 'a') && !Object.hasOwn({}, 'a')"));
    assert_eq!(
        string("Object.getOwnPropertyDescriptor({ a: 5 }, 'a').value"),
        "5"
    );
    assert!(boolean("Object.getOwnPropertyDescriptor({}, 'x') === undefined"));
    assert_eq!(
        string("Object.getOwnPropertyDescriptors({ a: 1 }).a.value"),
        "1"
    );
}

#[test]
fn base64_and_typed_arrays() {
    assert_eq!(string("btoa('hi')"), "aGk=");
    assert_eq!(string("atob('aGk=')"), "hi");
    assert_eq!(string("new Uint8Array([104, 105]).toBase64()"), "aGk=");
    assert_eq!(
        string("Uint8Array.fromBase64('aGk=').join(',')"),
        "104,105"
    );
    assert_eq!(num("new Uint8Array(3).length"), 3.0);
    assert_eq!(string("Buffer.from('hi').toString('base64')"), "aGk=");
    assert_eq!(num("Uint8Array.fromBase64('aGk=').buffer.byteLength"), 2.0);
}

#[test]
fn typed_array_views_share_storage() {
    // writes coerce to the element width and views alias the buffer
    assert_eq!(num("var a = new Uint8Array(2); a[0] = 300; a[0]"), 44.0);
    assert_eq!(num("var a = new Int16Array(2); a[1] = -2; a[1]"), -2.0);
    assert_eq!(
        string("var b = new ArrayBuffer(4); var a = new Uint8Array(b); a[0] = 7; new Uint8Array(b)[0]"),
        "7"
    );
    assert_eq!(
        num("var a = new Float64Array([1.5, 2.5]); a.slice(1)[0]"),
        2.5
    );
    assert_eq!(
        string("var b = new ArrayBuffer(8); var a = new Uint16Array(b); a.set([1, 2], 1); a[0] + ',' + a[1] + ',' + a[2]"),
        "0,1,2"
    );
    assert!(boolean("ArrayBuffer.isView(new Int8Array(1)) && !ArrayBuffer.isView({})"));
    assert_eq!(num("Uint16Array.BYTES_PER_ELEMENT"), 2.0);
    assert_eq!(
        string("var a = new Uint8Array([1, 2, 3, 4]); var s = a.subarray(1, 3); s[0] = 9; a[1]"),
        "9"
    );
}

#[test]
fn bigint_arithmetic_and_conversions() {
    assert_eq!(string("typeof 10n"), "bigint");
    assert_eq!(string("10n + 5n"), "15");
    assert_eq!(string("2n ** 10n"), "1024");
    assert_eq!(string("(7n / 2n) + ',' + (7n % 2n)"), "3,1");
    assert!(boolean("1n === 1n && 1n !== 1 && 1n == 1"));
    assert!(boolean("1n < 2n && 3n >= 3n"));
    assert_eq!(string("BigInt('42') + 1n"), "43");
    assert_eq!(string("BigInt.asIntN(8, 255n)"), "-1");
    assert_eq!(string("BigInt.asUintN(8, -1n)"), "255");
    assert_eq!(string("(255n).toString(16)"), "ff");
    // mixing is an honest TypeError
    err("1n + 1");
    err("+1n");
}

#[test]
fn bigint_pow_does_not_spin_on_huge_exponents() {
    // |base| <= 1 never overflows, so the multiply loop would run the whole
    // exponent (u32::MAX here) — must be answered directly instead
    assert_eq!(string("1n ** 4294967295n"), "1");
    assert_eq!(string("0n ** 4294967295n"), "0");
    assert_eq!(string("0n ** 0n"), "1");
    assert_eq!(string("(-1n) ** 4294967295n"), "-1");
    assert_eq!(string("(-1n) ** 4294967294n"), "1");
    // anything bigger than 1 overflows i128 long before the 128th squaring
    err("2n ** 400n");
    err("2n ** -1n");
}

#[test]
fn typed_array_length_ctor_zero_fills_without_a_value_vector() {
    assert_eq!(
        string("var a = new Uint8Array(3); a.length + ',' + a[0] + ',' + (a instanceof Uint8Array)"),
        "3,0,true"
    );
    assert_eq!(string("new Int32Array(2).byteLength"), "8");
    // the element-count cap still applies
    err("new Uint8Array(1 << 25)");
}

#[test]
fn crypto_random_uuid_and_get_random_values() {
    // GitHub's React root mints its element ids with `crypto.randomUUID()`;
    // the missing global was a ReferenceError out of connectedCallback.
    assert!(boolean("typeof crypto.randomUUID() == 'string'"));
    assert!(boolean(
        "/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID())"
    ));
    assert!(boolean("crypto.randomUUID() != crypto.randomUUID()"));
    // getRandomValues fills the view in place and returns it
    assert!(boolean("var b = new Uint8Array(16); crypto.getRandomValues(b) === b"));
    assert_eq!(
        string("var b = new Uint8Array(16); crypto.getRandomValues(b); b.length"),
        "16"
    );
    assert!(boolean(
        "var b = new Uint8Array(16); crypto.getRandomValues(b); var nz = false, ok = true; \
         for (var i = 0; i < b.length; i++) { if (b[i] !== 0) nz = true; if (b[i] < 0 || b[i] > 255) ok = false; } nz && ok"
    ));
}

#[test]
fn new_target_and_private_brand_checks() {
    assert!(boolean(
        "function F() { this.ok = new.target === F } new F().ok"
    ));
    assert!(boolean("function F() { return new.target } F() === undefined"));
    assert_eq!(
        string("class A { #x = 1; static has(o) { return #x in o } } A.has(new A()) + ',' + A.has({})"),
        "true,false"
    );
    // an arrow inside a constructor sees the constructor's new.target
    assert!(boolean(
        "function F() { this.ok = (() => new.target === F)(); } new F().ok"
    ));
    // a private name may use `\u` escapes (`#\u{6F}_`)
    assert_eq!(
        num("class C { #\\u{6F}_ = 7; get() { return this.#\\u{6F}_ } } new C().get()"),
        7.0
    );
    // a plain nested call shadows it with undefined rather than inheriting
    assert!(boolean(
        "function F() { function g() { return new.target } this.ok = g() === undefined } new F().ok"
    ));
}

#[test]
fn user_defined_iterator_protocol() {
    // for..of is lazy: it stops calling `next` at `break`
    assert_eq!(
        num("var n = 0; var it = { [Symbol.iterator]() { return { next() { n++; return { value: n, done: n > 10 } } } } }; for (var v of it) { if (v === 2) break } n"),
        2.0
    );
    assert_eq!(
        string("var it = { [Symbol.iterator]() { var i = 0; return { next() { return { value: i++, done: i > 3 } } } } }; [...it].join(',')"),
        "0,1,2"
    );
    assert_eq!(
        string("var it = { [Symbol.iterator]() { var i = 0; return { next() { return { value: i++, done: i > 2 } } } } }; Array.from(it).join('+')"),
        "0+1"
    );
}

#[test]
fn of_is_a_plain_identifier() {
    // `of` is contextual: only the for-head separator, never a keyword
    assert_eq!(num("var of = 3; of + 1"), 4.0);
    assert_eq!(num("function of(x) { return x * 2 } of(4)"), 8.0);
    assert_eq!(num("({ of: 7 }).of"), 7.0);
    assert_eq!(num("var o = { of: 5 }; o.of"), 5.0);
    assert_eq!(num("var a = [1, 2]; var n = 0; for (var of of a) n += of; n"), 3.0);
}

#[test]
fn object_and_array_are_callable() {
    // `Object(x)` / `Array(n)` / `new Array(n)`: the constructors are plain
    // objects here, so the hidden `__call` hook makes the call form work
    assert!(boolean("typeof Object({ a: 1 }) === 'object'"));
    assert_eq!(string("Object({ a: 1 }).a"), "1");
    assert_eq!(string("Object(null).toString()"), "[object Object]");
    assert_eq!(string("new Array(3).length"), "3");
    assert_eq!(string("Array(3).length"), "3");
    assert_eq!(string("Array(1, 2).join('-')"), "1-2");
    assert_eq!(string("Array.of(1, 2).length"), "2");
    assert_eq!(string("Array.from('abc').join('')"), "abc");
    assert_eq!(string("Array.from([1, 2], function (v) { return v * 2 }).join(',')"), "2,4");
    assert_eq!(string("Array.from({ length: 2, 0: 'a', 1: 'b' }).join('')"), "ab");
}

#[test]
fn object_to_string_is_a_tag_not_a_dump() {
    // `ToString` on a plain object is its tag, never a walk of its properties:
    // walking is exponential on a cyclic graph like `window`, which used to
    // blow up to gigabytes when gtag.js evaluated `window == null`.
    assert_eq!(string("String({ a: 1 })"), "[object Object]");
    assert_eq!(string("var w = {}; w.self = w; String(w)"), "[object Object]");
    assert_eq!(string("String([1, [2, 3]])"), "1,2,3");
    assert!(boolean("(globalThis == null) === false"));
    assert!(boolean("String(JSON.parse('{\"a\":1}')) === '[object Object]'"));
}

#[test]
fn primitive_and_function_prototypes_exist() {
    // feature probes core-js-style bundles run before installing polyfills
    assert!(boolean("'trimStart' in String.prototype && 'trim' in String.prototype"));
    assert!(boolean("'toUpperCase' in String.prototype"));
    assert!(boolean("'toString' in Number.prototype"));
    assert!(boolean("'toString' in Boolean.prototype"));
    assert!(boolean("typeof Symbol.prototype === 'object'"));
    // Symbol.for shares one symbol per key across calls; Symbol.keyFor finds
    // the registry key (React keys its element type on Symbol.for)
    assert!(boolean("Symbol.for('x') === Symbol.for('x')"));
    assert!(boolean("Symbol.for('x') !== Symbol('x')"));
    assert_eq!(string("String(Symbol.for('x'))"), "Symbol(x)");
    assert_eq!(string("Symbol.keyFor(Symbol.for('y'))"), "y");
    assert!(boolean("Symbol.keyFor(Symbol('y')) === undefined"));
    // borrowed methods work through the prototype object
    assert_eq!(string("String.prototype.trimStart.call('  x  ')"), "x  ");
    assert_eq!(string("String.prototype.trim.call('  x  ')"), "x");
    assert_eq!(string("String.prototype.toUpperCase.call('ab')"), "AB");
    assert_eq!(string("Number.prototype.toFixed.call(1.5, 1)"), "1.5");
    // String statics
    assert_eq!(string("String.fromCharCode(65, 66, 67)"), "ABC");
    assert_eq!(string("String.fromCharCode(0x000A)"), "\n");
    assert_eq!(string("String.fromCharCode(0xD83D, 0xDE00)"), "\u{1f600}");
    assert_eq!(string("String.fromCharCode()"), "");
    // Number's own value properties
    assert!(boolean("Number.POSITIVE_INFINITY === Infinity"));
    assert!(boolean("Number.NEGATIVE_INFINITY === -Infinity"));
    assert!(boolean("Number.NaN !== Number.NaN"));
    assert!(boolean("Number.MAX_VALUE > 0 && Number.MAX_VALUE * 1 === Number.MAX_VALUE"));
    assert!(boolean("Number.MIN_VALUE > 0 && Number.MIN_VALUE < 1e-300"));
    assert!(boolean("Number.MAX_SAFE_INTEGER === 9007199254740991"));
    assert!(boolean("Number.MIN_SAFE_INTEGER === -9007199254740991"));
    assert!(boolean("Number.EPSILON > 0 && Number.EPSILON < 1e-15"));
    // Number.isNaN is the strict form: no coercion, so a numeric string is not
    assert!(boolean("Number.isNaN(NaN)"));
    assert!(boolean("Number.isNaN(0 / 0)"));
    assert!(boolean("!Number.isNaN('NaN')"));
    assert!(boolean("!Number.isNaN(0)"));
    assert!(boolean("!Number.isNaN()"));
    assert_eq!(
        string("Function.prototype.call.call(function (a) { return a; }, null, 4)"),
        "4"
    );
    // functions inherit Object.prototype through Function.prototype
    assert!(boolean("(function () {}).hasOwnProperty('prototype')"));
    assert!(boolean("!(function () {}).bind(null).hasOwnProperty('prototype')"));
    // shortcut: the Function constructor returns the global-getter idiom
    assert!(boolean("Function('return this')() === globalThis"));
    // Object.prototype surface core-js probes before installing polyfills
    assert!(boolean("({ a: 1 }).propertyIsEnumerable('a')"));
    assert!(boolean("![1].propertyIsEnumerable('length')"));
    assert!(boolean("({}).valueOf() !== null"));
    // Object.defineProperties applies every descriptor
    assert_eq!(
        string("var o = {}; Object.defineProperties(o, { a: { value: 1 }, b: { value: 2 } }); o.a + o.b"),
        "3"
    );
    assert!(boolean("Object.prototype.isPrototypeOf({})"));
    assert!(boolean("!Object.prototype.isPrototypeOf(Object.prototype)"));
}

#[test]
fn to_primitive_uses_observable_methods() {
    // valueOf first (number/default hint)
    assert_eq!(num("({ valueOf: function () { return 1 } }) + 1"), 2.0);
    assert_eq!(num("1 + ({ valueOf: function () { return 41 } })"), 42.0);
    // a non-primitive valueOf falls through to toString
    assert_eq!(
        string("({ valueOf: function () { return {} }, toString: function () { return 'x' } }) + ''"),
        "x"
    );
    // Symbol.toPrimitive wins and receives the hint
    assert_eq!(
        num("var o = {}; o[Symbol.toPrimitive] = function (h) { return h === 'default' ? 40 : 0 }; o + 2"),
        42.0
    );
    assert_eq!(
        num("var o = {}; o[Symbol.toPrimitive] = function (h) { return h === 'number' ? 10 : 0 }; o * 2"),
        20.0
    );
    // both ordinary methods returning objects is a TypeError
    assert!(matches!(
        err("({ valueOf: function () { return {} }, toString: function () { return {} } }) + 1"),
        JsError::Throw(_)
    ));
    // arrays stringify through join; objects compare by identity
    assert_eq!(string("[1, 2] + ''"), "1,2");
    assert!(boolean("var d = new Date(0); d + d === d.toString() + d.toString()"));
    assert!(boolean("({ valueOf: function () { return 5 } }) == 5"));
    assert!(boolean("({ valueOf: function () { return 5 } }) < 6"));
}

#[test]
fn numeric_separators() {
    assert_eq!(num("1_000"), 1000.0);
    assert_eq!(num("1_000.000_1"), 1000.0001);
    assert_eq!(num("1e1_0"), 1e10);
    assert_eq!(num("0x1_0"), 16.0);
    assert_eq!(num("0b1_0"), 2.0);
    assert_eq!(num("0o1_0"), 8.0);
    assert!(boolean("1_0n === 10n"));
    // separators hold no value of their own
    assert_eq!(num("1_2_3"), 123.0);
    // legacy leading-zero forms cannot be BigInts
    for src in ["var x = 00n", "var x = 01n", "var x = 08n", "var x = 0008n"] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    assert!(boolean("0n === 0n"));
    // illegal placements are syntax errors
    for src in [
        "var x = 1_",
        "var x = 1__0",
        "var x = 0_1",
        "var x = 00_0",
        "var x = 0_8",
        "var x = 0x_1",
        "var x = 0b0__0",
        "var x = 10._1",
        "var x = 10.0_e1",
        "var x = 1_e1",
        "var x = 0x0_",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
}

#[test]
fn numeric_literal_edges() {
    // Annex B legacy octal (sloppy), and non-octal decimals
    assert_eq!(num("070"), 56.0);
    assert_eq!(num("001"), 1.0);
    assert_eq!(num("077"), 63.0);
    assert_eq!(num("08"), 8.0);
    assert_eq!(num("0o70"), 56.0);
    assert_eq!(string("1..toString()"), "1");
    // a numeric literal may not run straight into an identifier
    for src in ["3in []", "var x = 1foo", "var x = 0x1g", "var x = 1.toString()"] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
}

#[test]
fn unicode_whitespace_and_line_terminators() {
    // NBSP, BOM and the U+2028/U+2029 terminators are not identifiers
    assert_eq!(num("1\u{a0}+ 2"), 3.0);
    assert_eq!(num("\u{feff}1 + 2"), 3.0);
    assert_eq!(num("1;\u{2028}2"), 2.0);
    assert_eq!(num("1;\u{2029}2"), 2.0);
    assert_eq!(num("1\u{2009}+\u{2009}2"), 3.0);
}

#[test]
fn in_operator_survives_nested_grammar_in_for_heads() {
    assert!(boolean("'a' in { a: 1 }"));
    assert!(boolean(
        "var o = { a: 1 }; var q = false; if (o && 'a' in o) q = true; q"
    ));
    // function body inside a for-init: `in` suppression must not leak
    assert!(boolean(
        "var r = []; for (var f = function (o) { return 'a' in o; }; r.length < 1;) { r.push(f({ a: 1 })); break; } r[0]"
    ));
    // object/array literal inside a for-init
    assert!(boolean("for (var o = { k: 'a' in { a: 1 } }; o.k;) { break; } o.k"));
    assert!(boolean("for (var a = ['b' in { b: 1 }]; a[0];) { break; } a[0]"));
    // `in` in the condition of a classic for
    assert_eq!(num("var n = 0; for (var i = 0; 'a' in { a: 1 }; i++) { n++; break; } n"), 1.0);
}

#[test]
fn array_sort_and_reverse() {
    assert_eq!(string("[3, 1, 2].sort().join(',')"), "1,2,3");
    assert_eq!(string("['b', 'a'].sort().join(',')"), "a,b");
    assert_eq!(string("[3, 1, 2].sort(function (a, b) { return b - a; }).join(',')"), "3,2,1");
    assert_eq!(string("[1, 2, 3].reverse().join(',')"), "3,2,1");
    assert_eq!(num("var a = [2, 1]; a.sort(); a[0]"), 1.0);
}

#[test]
fn text_encoder_decoder_round_trip() {
    assert_eq!(
        string("var e = new TextEncoder(); e.encode('hi').join(',')"),
        "104,105"
    );
    assert_eq!(
        string("var d = new TextDecoder(); d.decode([104, 105])"),
        "hi"
    );
    assert_eq!(string("new TextDecoder().encoding"), "utf-8");
}

#[test]
fn weak_collections_store_by_identity() {
    assert_eq!(
        string("var k = {}; var m = new WeakMap(); m.set(k, 5); m.get(k)"),
        "5"
    );
    assert!(boolean("var k = {}; var m = new WeakMap([[k, 1]]); m.has(k)"));
    assert!(boolean("var k = {}; var m = new WeakMap(); m.set(k, 1); m.delete(k) && !m.has(k)"));
    assert!(boolean("var k = {}; var s = new WeakSet([k]); s.has(k)"));
    assert!(boolean("new WeakMap([[{}, 1]]) instanceof WeakMap"));
}

#[test]
fn symbol_builtin_is_unique_and_usable_as_a_key() {
    assert_eq!(string("typeof Symbol('x')"), "symbol");
    assert!(boolean("Symbol('x') === Symbol('x') === false"));
    assert_eq!(string("var s = Symbol('a'); var o = {}; o[s] = 1; o[s]"), "1");
    assert_eq!(string("String(Symbol('a'))"), "Symbol(a)");
    assert_eq!(string("typeof Symbol.iterator"), "symbol");
    err("new Symbol('x')");
}

#[test]
fn object_from_entries_builds_objects() {
    assert_eq!(
        string("JSON.stringify(Object.fromEntries([['a', 1], ['b', 2]]))"),
        "{\"a\":1,\"b\":2}"
    );
    assert_eq!(num("Object.fromEntries([[1, 2]])[1]"), 2.0);
    assert_eq!(
        string("Object.fromEntries(Object.entries({ x: 'y' })).x"),
        "y"
    );
}

#[test]
fn object_from_entries_rejects_non_pairs() {
    err("Object.fromEntries([[1]])");
}

#[test]
fn conversions_and_number_globals() {
    assert_eq!(string("String(42)"), "42");
    assert_eq!(string("String(true)"), "true");
    assert_eq!(string("String(null)"), "null");
    assert_eq!(string("String(undefined)"), "undefined");
    assert_eq!(num("Number('12px')"), 12.0); // greedy prefix parse
    assert_eq!(num("Number('')"), 0.0);
    assert_eq!(num("Number(true)"), 1.0);
    assert!(run("Number('abc')").as_number().unwrap().is_nan());
    assert!(boolean("Boolean('') == false"));
    assert!(boolean("Boolean('x')"));
    assert_eq!(num("parseInt('42px')"), 42.0);
    assert_eq!(num("parseInt('0x1f')"), 31.0);
    assert_eq!(num("parseInt('ff', 16)"), 255.0);
    assert_eq!(num("parseInt('-3')"), -3.0);
    assert_eq!(num("parseFloat('3.14abc')"), 3.14);
    assert!(run("parseInt('xyz')").as_number().unwrap().is_nan());
    assert!(boolean("isNaN('abc')"));
    assert!(!boolean("isNaN('12')"));
    assert!(boolean("isFinite(1e308 * 10) == false")); // overflows to Infinity
    // globals NaN/Infinity exist
    assert!(run("NaN").as_number().unwrap().is_nan());
    assert_eq!(num("Infinity * 2"), f64::INFINITY);
}

#[test]
fn uri_encoding() {
    assert_eq!(string("encodeURIComponent('a b/c')"), "a%20b%2Fc");
    assert_eq!(string("encodeURIComponent('h\u{e9}llo')"), "h%C3%A9llo");
    assert_eq!(string("decodeURIComponent('a%20b')"), "a b");
    assert_eq!(string("decodeURIComponent('h%C3%A9llo')"), "h\u{e9}llo");
    // round trip
    assert_eq!(string("decodeURIComponent(encodeURIComponent('\u{3c0}/\u{3c0}'))"), "\u{3c0}/\u{3c0}");
    match err("decodeURIComponent('%zz')") {
        JsError::Throw(_) => {}
        other => panic!("expected Throw, got {:?}", other),
    }
}

#[test]
fn date_minimal() {
    assert!(boolean("typeof Date.now() == 'number' && Date.now() > 1.6e9"));
    assert!(boolean("let d = new Date(1000); d.getTime() == 1000"));
    assert!(boolean("let d = new Date(0); d.toISOString() == '1970-01-01T00:00:00.000Z'"));
    assert!(boolean("new Date(1000) instanceof Date"));
}

#[test]
fn date_parse_reads_iso_forms() {
    // GitHub's <relative-time> parses its `datetime` attribute with
    // `Date.parse`; it was missing, so the whole `date` getter threw.
    assert_eq!(num("Date.parse('1970-01-01T00:00:00Z')"), 0.0);
    assert_eq!(num("Date.parse('2020-02-29T12:00:00Z')"), 1_582_977_600_000.0);
    assert_eq!(num("Date.parse('2020-02-29')"), 1_582_934_400_000.0);
    // an offset shifts the instant; +02:00 is two hours earlier in UTC
    assert_eq!(num("Date.parse('2020-01-01T02:00:00+02:00')"), 1_577_836_800_000.0);
    assert_eq!(num("Date.parse('2020-01-01T00:00:00.250Z')"), 1_577_836_800_250.0);
    assert!(boolean("isNaN(Date.parse('2020-02-30'))"));
    assert!(boolean("isNaN(Date.parse('not a date'))"));
    // the constructor shares the parser
    assert_eq!(num("new Date('2020-02-29T12:00:00Z').getTime()"), 1_582_977_600_000.0);
    assert!(boolean("isNaN(new Date('nope').getTime())"));
    // and the round trip both ways
    assert!(boolean("let d = new Date('2021-06-01T03:04:05Z'); d.toISOString() == '2021-06-01T03:04:05.000Z'"));
}

#[test]
fn timers_queue_only() {
    let mut e = JsEngine::new();
    e.exec("let calls = 0; setTimeout(function(x) { calls += x }, 100, 5); setTimeout(function() {}, 0)")
        .unwrap();
    let q = e.drain_timers();
    assert_eq!(q.len(), 2);
    assert_eq!(q[0].delay_ms, 100.0);
    assert_eq!(q[0].args.len(), 1);
    // embedder invokes the callback itself
    e.call_function(&q[0].callback, Val::Undefined, &q[0].args).unwrap();
    assert_eq!(e.get_global("calls").as_number(), Some(5.0));
    // queue is drained
    assert!(e.drain_timers().is_empty());
}

#[test]
fn promise_partial() {
    let mut e = JsEngine::new();
    e.exec("let out = 0; Promise.resolve(21).then(function(v) { out = v * 2 })").unwrap();
    e.drain_microtasks().unwrap();
    assert_eq!(e.get_global("out").as_number(), Some(42.0));
    // rejection path
    e.exec("let got = ''; Promise.reject('nope').catch(function(e) { got = e })").unwrap();
    e.drain_microtasks().unwrap();
    assert_eq!(e.get_global("got").to_string(), "nope");
    // `new Promise(executor)` now works: the executor runs synchronously and
    // its resolve call settles the promise; `.then` reactions fire on drain
    e.exec("let s = 0; new Promise(function(res) { res(9) }).then(function(v) { s = v * 3 + 1 })").unwrap();
    e.drain_microtasks().unwrap();
    assert_eq!(e.get_global("s").as_number(), Some(28.0));
}

// ---------------------------------------------------------------------------
// template literals
// ---------------------------------------------------------------------------

#[test]
fn template_literals() {
    assert_eq!(string("let n = 3; `a${n}b`"), "a3b");
    assert_eq!(string("`plain`"), "plain");
    assert_eq!(string("``"), "");
    // expression + nested braces inside interpolation
    assert_eq!(string("let o = {x: 1}; `${ o.x + ({y: 2}).y }`"), "3");
    // multiple + function call
    assert_eq!(string("function f() { return 'F' } `${f()}-${f()}`"), "F-F");
    // escapes
    assert_eq!(string("`a\\nb`"), "a\nb");
    // a literal `$` NOT starting an interpolation must be literal text —
    // previously the lexer broke at the `$` without consuming it and spun
    // forever (this exact shape hung a real page: `` `$${r}` ``)
    assert_eq!(string("let r = 'X'; `$${r}`"), "$X");
    assert_eq!(string("`cost: $5 and $$`"), "cost: $5 and $$");
    assert_eq!(string("`$`"), "$");
}

// ---------------------------------------------------------------------------
// embedder API
// ---------------------------------------------------------------------------

#[test]
fn unicode_identifier_escapes() {
    // property names like Angular's `ɵ` prefix are written as escapes
    assert_eq!(
        num(r"var o = {}; o.\u0275prov = 7; o.\u0275prov"),
        7.0
    );
    assert_eq!(num(r"var \u0061bc = 3; abc"), 3.0);
    // an escaped reserved word is still a ReservedWord: invalid as a binding
    // name even though the lexer decoded it to an identifier
    assert!(matches!(err(r"var \u0069f = 1; typeof \u0069f"), JsError::Syntax { .. }));
    // `\u{...}` form, including a char outside the BMP
    assert_eq!(num(r"var \u{1d49c} = 4; \u{1d49c}"), 4.0);
    // a bad escape is a syntax error, not a panic
    assert!(matches!(err(r"var o = {}; o.\u12"), JsError::Syntax { .. }));
}

#[test]
fn escaped_await_yield_in_function_contexts() {
    // `await` is reserved in an async function and `yield` in a generator;
    // the escaped spellings lex as identifiers, so the parser's function
    // context has to reject them as bindings, references and labels.
    for src in [
        r"var f = async function () { var \u0061wait; };",
        r"var f = async function () { void \u0061wait; };",
        r"var f = async function () { \u0061wait: ; };",
        r"var f = async () => { var \u0061wait; };",
        r"var f = async () => { void \u0061wait; };",
        r"var g = function *() { var \u0079ield; };",
        r"var g = function *() { void \u0079ield; };",
        r"var g = function *() { \u0079ield: ; };",
        r"var o = { *m() { var \u0079ield; } };",
        r"var o = { async m() { void \u0061wait; } };",
        r"class C { *m() { var \u0079ield; } }",
        r"class C { async m() { void \u0061wait; } }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // a nested non-async/non-generator function gets a fresh context
    assert_eq!(
        num(r"var f = async function () { function g() { var \u0061wait = 1; return \u0061wait } return g() }; f(); 7"),
        7.0
    );
    assert_eq!(
        num(r"var g = function *() { function h() { var \u0079ield = 2; return \u0079ield } }; g(); 7"),
        7.0
    );
    // property names and sloppy-mode labels stay legal
    assert_eq!(num(r"var o = { \u0061wait: 5 }; o.\u0061wait"), 5.0);
    assert_eq!(num(r"var o = {}; o.\u0079ield = 6; o.\u0079ield"), 6.0);
}

#[test]
fn delete_removes_own_function_properties() {
    assert!(boolean("class C { static m() {} }; delete C.m"));
    assert!(boolean(
        "class C { static m() {} }; delete C.m; !Object.prototype.hasOwnProperty.call(C, 'm')"
    ));
}

#[test]
fn trailing_comma_in_parameter_lists() {
    assert_eq!(num("function f(a, b,) { return a + b } f(1, 2)"), 3.0);
    assert_eq!(num("((a,) => a)(7)"), 7.0);
    assert_eq!(num("((a, b = 2,) => a + b)(1)"), 3.0);
    assert_eq!(num("var o = { m(a,) { return a } }; o.m(4)"), 4.0);
    // a rest parameter cannot be followed by a comma
    assert!(matches!(
        err("function f(...a,) {}"),
        JsError::Syntax { .. }
    ));
    // ...and must be the last parameter
    for src in [
        "function f(a, ...b, c) {}",
        "var f = (a, ...b, c) => 0",
        "var o = { m(a, ...b, c) {} }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
}

#[test]
fn private_names_are_lexically_scoped_and_unique() {
    // forward references are legal
    assert_eq!(
        num("class C { m() { return this.#x } #x = 1 }; new C().m()"),
        1.0
    );
    // a private reference with no declaration in the class is an early error
    for src in [
        "class C { m() { return this.#x } }",
        "class C extends Object { m() { return something.#x } }",
        "class C { #y; m() { return this.#y.#z } }",
        "class C { f = this.#missing }",
        "class C { #x; m() { return #y in this } }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // duplicate private names are an early error (get/set pair is not)
    for src in [
        "class C { #x; #x; }",
        "class C { #x() {} #x; }",
        "class C { static #x; #x; }",
        "class C { get #x() {} #x() {} }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // private names may not appear through super or as pattern keys
    assert!(matches!(
        err("class A { #m() {} }; class C extends A { m() { return super.#m } }"),
        JsError::Syntax { .. }
    ));
    assert!(matches!(
        err("class C { #x = 1; m() { const { #x: y } = this; } }"),
        JsError::Syntax { .. }
    ));
    // a getter/setter pair and ordinary private members still work
    assert_eq!(
        num("class C { get #x() { return 4 } set #x(v) {} m() { return this.#x } }; new C().m()"),
        4.0
    );
}

#[test]
fn with_statement_scopes_identifier_lookup() {
    // a property of the with object shadows outer bindings
    assert_eq!(num("var x = 1; var o = { x: 2 }; var r; with (o) { r = x } r"), 2.0);
    // assignment hits the object property when it exists, otherwise falls out
    assert_eq!(num("var o = { x: 1 }; with (o) { x = 5 } o.x"), 5.0);
    assert_eq!(num("var x = 1; var o = {}; with (o) { x = 3 } x"), 3.0);
    // `var` in the body still hoists to the function/global scope
    assert_eq!(num("with ({}) { var y = 4 } y"), 4.0);
    // `typeof` consults the with object, and closures capture it
    assert_eq!(string("var o = { x: 'v' }; var r; with (o) { r = typeof x } r"), "string");
    assert_eq!(
        num("var o = { x: 7 }; var f; with (o) { f = function () { return x } } f()"),
        7.0
    );
}

#[test]
fn block_scope_lexical_and_var_redeclaration() {
    for src in [
        "{ let f; var f }",
        "{ var f; let f }",
        "{ let f; { var f } }",
        "{ let f; let f }",
        "{ const f = 1; var f }",
        "{ class f {} var f }",
        "{ function f() {} let f }",
        "{ function f() {} function f() {} }",
        "{ function f() {} { var f } }",
        "switch (0) { case 0: let f; case 1: var f }",
        "switch (0) { case 0: let f; case 1: let f }",
        "function g() { let f; var f }",
        "let f; var f",
        "let f; let f",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // legal: repeated `var`, sibling/nested scopes, a nested function body,
    // and a for-head `let` shadowing an enclosing `let`
    for src in [
        "{ var f; var f }",
        "{ let f; { let f } }",
        "{ let f; function g() { var f } }",
        "{ let f; for (let f = 0; f < 1; f++) {} }",
        "function f() {} function f() {}",
        "switch (0) { case 0: var f; case 1: var f }",
        "{ let f; } { let f; }",
        "for (let f = 0; f < 1; f++) { var g; }",
    ] {
        run(src);
    }
}

#[test]
fn super_call_only_inside_constructor() {
    for src in [
        "class C extends Object { m() { super(); } }",
        "class C extends Object { static m() { super(); } }",
        "class C extends Object { *m() { super(); } }",
        "class C extends Object { async m() { super(); } }",
        "class C extends Object { get x() { super(); } }",
        "class C extends Object { x = super(); }",
        "class C extends Object { static x = super(); }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // inside the constructor it still works, including a derived class chain
    assert_eq!(
        num("class A { constructor() { this.a = 1 } } class B extends A { constructor() { super(); this.b = 2 } } new B().a + new B().b"),
        3.0
    );
    // `super.m()` in an ordinary method remains legal
    assert_eq!(
        num("class A { m() { return 5 } } class B extends A { m() { return super.m() + 1 } } new B().m()"),
        6.0
    );
}

#[test]
fn class_element_reserved_names() {
    for src in [
        "class C { static prototype() {} }",
        "class C { static *prototype() {} }",
        "class C { static async prototype() {} }",
        "class C { static get prototype() {} }",
        "class C { static set prototype(v) {} }",
        "class C { static constructor() {} }",
        "class C { get constructor() {} }",
        "class C { set constructor(v) {} }",
        "class C { constructor; }",
        "class C { static prototype; }",
        "class C { static constructor; }",
        "class C { #constructor() {} }",
        "class C { static #constructor() {} }",
        "class C { #constructor; }",
        "class C { *constructor() {} }",
        "class C { async constructor() {} }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // instance members named `prototype` and `constructor` methods are legal
    assert_eq!(num("class C { prototype() { return 1 } }; new C().prototype()"), 1.0);
    assert_eq!(num("class C { constructor() { this.x = 2 } }; new C().x"), 2.0);
    assert_eq!(num("class C { get prototype() { return 3 } }; new C().prototype"), 3.0);
}

#[test]
fn unresolved_identifier_reads_throw_reference_error() {
    assert!(matches!(err("void missingName"), JsError::Throw(_)));
    assert!(matches!(err("missingName + 1"), JsError::Throw(_)));
    assert!(matches!(err("missingName += 1"), JsError::Throw(_)));
    // typeof is the exception: an undeclared name is legal there
    assert_eq!(string("typeof missingName"), "undefined");
    // sloppy assignment to an undeclared name still creates a global
    assert_eq!(num("missingName = 3; missingName"), 3.0);
    // a global property that exists holding undefined is resolvable
    assert_eq!(string("this.foo = undefined; typeof foo"), "undefined");
}

#[test]
fn object_destructuring_requires_coercible_value() {
    // an object pattern throws for null/undefined even with no properties
    for src in [
        "var {} = null",
        "var {} = undefined",
        "function f({}) {} f()",
        "function f({} = null) {} f()",
        "var {x} = null",
        "var {x = 1} = undefined",
    ] {
        assert!(
            matches!(err(src), JsError::Throw(_)),
            "expected a throw for {src}"
        );
    }
    // primitives still coerce (`{} = 5` reads no properties)
    assert_eq!(num("var {} = 5; 1"), 1.0);
}

#[test]
fn catch_parameter_destructuring() {
    assert_eq!(
        num("var r; try { throw [1, 2] } catch ([a, b]) { r = a + b } r"),
        3.0
    );
    assert_eq!(
        string("var r; try { throw { x: 'hi' } } catch ({ x }) { r = x } r"),
        "hi"
    );
    assert_eq!(num("var r = 9; try { throw 1 } catch { r = 0 } r"), 0.0);
    assert_eq!(num("var r; try { throw 5 } catch (e) { r = e } r"), 5.0);
}

#[test]
fn delete_of_a_private_reference_is_an_early_error() {    for src in [
        r"class C { #x; m() { delete this.#x; } }",
        r"class C { #m() {}; x = delete this.#m(); }",
        r"class C { #x; x = delete (this.#x); }",
        r"class C { #x; x = delete ((this.#x)); }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // a public member still deletes normally
    assert!(boolean("var o = { x: 1 }; delete o.x"));
}

#[test]
fn template_line_continuation_and_unicode_escapes() {
    // `\` + U+2028/U+2029 is a line continuation. The lexer used to advance
    // one byte into the three-byte terminator and panic while slicing.
    assert_eq!(string("`a\\\u{2028}b`"), "ab");
    assert_eq!(string("`a\\\u{2029}b`"), "ab");
    // identity escape of a non-ASCII char drops the backslash
    assert_eq!(string("`\\é`"), "é");
    // malformed `\x` followed by a multi-byte char is an error, not a panic
    let bad = err("`\\x\u{2028}`");
    assert!(matches!(bad, JsError::Syntax { .. }));
}

#[test]
fn state_persists_across_exec() {
    let mut e = JsEngine::new();
    e.exec("var counter = 0; function bump() { counter += 1 }").unwrap();
    e.exec("bump(); bump();").unwrap();
    assert_eq!(e.get_global("counter").as_number(), Some(2.0));
}

#[test]
fn set_global_and_call_function() {
    let mut e = JsEngine::new();
    e.set_global("hostValue", Val::Num(10.0));
    e.exec("function triple(x) { return x * hostValue / 10 * 3 }").unwrap();
    let f = e.get_global("triple");
    let r = e.call_function(&f, Val::Undefined, &[Val::Num(4.0)]).unwrap();
    assert_eq!(r.as_number(), Some(12.0));
    // as_string/as_number round trips
    assert_eq!(e.get_global("triple").type_of(), "function");
}

#[test]
fn define_native_host_function() {
    fn join_two(_e: &mut JsEngine, args: &[Val]) -> Result<Val, JsError> {
        let a = args.first().map_or(String::new(), |v| v.to_string());
        let b = args.get(1).map_or(String::new(), |v| v.to_string());
        Ok(Val::Str(Rc::from(format!("{}|{}", a, b).as_str())))
    }
    let mut e = JsEngine::new();
    e.define_native("hostJoin", join_two);
    assert_eq!(string2(&mut e, "hostJoin('a', 'b')"), "a|b");
}

fn string2(e: &mut JsEngine, src: &str) -> String {
    e.exec(src).unwrap().to_string()
}

#[test]
fn exec_returns_last_expression_value() {
    assert_eq!(run("let a = 1; a + 1").as_number(), Some(2.0));
    assert_eq!(run("'x' + 'y'").to_string(), "xy");
    assert!(run("let a = 1;").is_undefined());
}

#[test]
fn val_is_compact() {
    // RAM guard: Val must stay 24 bytes ÃƒÂ¢Ã¢‚Â¬Ã¢€Â this is the hot unit. (24 = 8-byte
    // tag/padding + the 16-byte fat Rc<str> pointer; thinning strings would
    // cost an extra indirection per string.)
    assert!(
        std::mem::size_of::<Val>() <= 24,
        "Val grew to {} bytes",
        std::mem::size_of::<Val>()
    );
}

// ---------------------------------------------------------------------------
// misc real-world-shaped snippets
// ---------------------------------------------------------------------------

#[test]
fn real_world_snippets() {
    // IIFE module pattern
    assert_eq!(
        num("let mod = (function() { let hidden = 1; return { get: function() { return hidden } } })(); mod.get()"),
        1.0
    );
    // prototype method iteration
    let src = "function C() { this.a = 1; this.b = 2 }
               C.prototype.sum = function() { return this.a + this.b };
               let c = new C(); c.sum()";
    assert_eq!(num(src), 3.0);
    // for-loop collecting into array with closures (pre-let semantics)
    let src2 = "let fns = []; for (var i = 0; i < 3; i++) { fns.push((function(i) { return function() { return i } })(i)) }
                fns[0]() + fns[1]() + fns[2]()";
    assert_eq!(num(src2), 3.0);
    // feature detection with typeof
    let src3 = "let t = typeof window === 'undefined' ? 'none' : 'win'; t";
    assert_eq!(string(src3), "win"); // our window alias exists
    // chained array building
    let src4 = "'1,2,3'.split(',').map(function(s) { return parseInt(s) * 10 }).filter(function(n) { return n > 15 }).join('+')";
    assert_eq!(string(src4), "20+30");
    // object identity
    assert!(boolean("let a = {}; let b = a; a === b"));
    assert!(!boolean("let a = {}; let b = {}; a === b"));
    assert!(!boolean("let a = {}; let b = {}; a == b")); // identity, not stringified
}

// ---------------------------------------------------------------------------
// regex literals (compiled-once, step-bounded backtracking engine)
// ---------------------------------------------------------------------------

#[test]
fn regex_test_method_and_anchors() {
    assert!(boolean("/oo/.test('foobar')"));
    assert!(!boolean("/oo/.test('bar')"));
    assert!(boolean("/^cat$/.test('cat') && !/^cat$/.test('a cat')"));
    assert_eq!(num("'cat dog cow'.search(/dog/)"), 4.0);
}

#[test]
fn regex_greedy_vs_lazy() {
    assert_eq!(string("'aXXbYYc'.replace(/X.*?Y/, '-')"), "a-Yc");
    assert_eq!(string("'aXXbYYc'.replace(/X.*Y/, '-')"), "a-c");
}

#[test]
fn regex_alternation_and_backrefs() {
    assert!(boolean("/(ab)\\1/.test('abab')"));
    assert!(!boolean("/(ab)\\1/.test('aba')"));
    assert_eq!(string("'cat,bat,hat'.match(/(b|h)at/g).join('|')"), "bat|hat");
}

#[test]
fn regex_exec_groups_index_input() {
    assert_eq!(string("'2026-10-31'.match(/(\\d{4})-(\\d{2})-(\\d{2})/)[2]"), "10");
    assert!(boolean(
        "(function() { const m = /B/.exec('aBc'); return m.index === 1 && m.input === 'aBc' && m[0] === 'B'; })()"
    ));
    assert_eq!(string("'a1b2c'.split(/(\\d)/).join('|')"), "a|1|b|2|c");
}

#[test]
fn regex_exec_g_advances_last_index() {
    // /g exec/test walk lastIndex; a failed call resets it to 0
    assert!(boolean(
        "(() => { const r = /ab/g; let hits = 0; while (r.exec('ababab')) { hits += 1 } return hits === 3 && r.lastIndex === 0; })()"
    ));
    assert!(boolean(
        "(() => { const r = /c/g; r.lastIndex = 9; return r.test('abc') === false && r.lastIndex === 0; })()"
    ));
}

#[test]
fn regex_replace_dollar_patterns() {
    assert_eq!(string("'a-b-c'.replace(/(b|c)/g, '<$1>')"), "a-<b>-<c>");
    assert_eq!(string("'foo bar'.replace(/bar/, \"[$`]\")"), "foo [foo ]");
    assert_eq!(string("'xABy'.replace(/AB/, \"'$&'\")"), "x'AB'y");
    assert_eq!(string("'abc'.replace(/b/, '$$')"), "a$c");
}

#[test]
fn regex_replace_function_replacer() {
    assert_eq!(
        string("'x1y2'.replace(/\\d/g, function(d) { return String(Number(d) * 2) })"),
        "x2y4"
    );
    assert_eq!(
        string("'ab-11'.replace(/(\\w)(\\w)/, function(m, a, b, off) { return a + off })"),
        "a0-11"
    );
}

#[test]
fn regex_named_capture_groups() {
    // the pattern from GitHub's react-core bundle
    assert!(boolean(r"/Minified React error #(?<invariant>\d+)/.test('Minified React error #418')"));
    // the group is numbered like a plain capture and shows up in `groups`
    assert_eq!(
        string(r"(() => { const m = /(?<y>\d{4})-(?<mo>\d{2})/.exec('2026-10'); return m.groups.y + '/' + m.groups.mo; })()"),
        "2026/10"
    );
    // `\k<name>` is the same backreference as `\1`
    assert!(boolean(r"/(?<w>ab)\k<w>/.test('abab') && !/(?<w>ab)\k<w>/.test('abac')"));
    // a named group keeps its number for positional access too
    assert_eq!(string(r"/(?<a>x)(y)/.exec('xy')[2]"), "y");
    // no named group: `groups` is undefined, not an empty object
    assert!(boolean(r"/x/.exec('x').groups === undefined"));
    // duplicate names and unknown names are syntax errors, and a `new
    // RegExp` error is catchable (bundles probe with try/catch)
    assert!(boolean(r"(() => { try { new RegExp('(?<a>x)(?<a>y)') } catch (e) { return e instanceof SyntaxError } return false })()"));
    assert!(boolean(r"(() => { try { new RegExp('\\k<nope>') } catch (e) { return e instanceof SyntaxError } return false })()"));
}

#[test]
fn regex_unicode_property_escapes() {
    assert!(boolean(r"/\p{L}+/u.test('abcé')"));
    assert!(!boolean(r"/^\p{L}+$/u.test('ab1')"));
    assert!(boolean(r"/\p{N}+/u.test('x42')"));
    assert!(boolean(r"/\p{Lu}/u.test('A') && !/\p{Lu}/u.test('a')"));
    assert!(boolean(r"/\p{Ll}/u.test('a') && !/\p{Ll}/u.test('A')"));
    // negated property, and properties mixed with ranges in one class
    // (the exact shape `wgy-*.js` builds)
    assert!(!boolean(r"/[^\p{L}\p{N}\s_-]/u.test('café_1-2')"));
    assert!(boolean(r"/[^\p{L}\p{N}\s_-]/u.test('café!')"));
    assert!(!boolean(r"/\P{L}/u.test('a')"));
    assert!(boolean(r"/\p{ASCII}/u.test('a') && !/\p{ASCII}/u.test('é')"));
    // an unknown property is an honest, catchable SyntaxError
    assert!(boolean(
        r"(() => { try { new RegExp('\\p{So}', 'u') } catch (e) { return e instanceof SyntaxError } return false })()"
    ));
}

#[test]
fn regex_flags() {
    assert!(boolean("/HELLO/i.test('hello')"));
    assert!(!boolean("/HELLO/.test('hello')"));
    assert!(boolean("/^b$/m.test('a\\nb')"));
    assert!(!boolean("/^b$/.test('a\\nb')"));
    assert!(boolean("/a.b/s.test('a\\nb')"));
    assert!(!boolean("/a.b/.test('a\\nb')"));
    assert!(boolean("/\\bcat\\b/.test('a cat on a mat') && !/\\bcat\\b/.test('scatter')"));
    assert!(boolean("/[a-f2-4]/.test('x3') && !/[a-f]/.test('zzz')"));
    assert!(boolean("/[^a-z]/.test('abc9') && !/[^a-z]/.test('abcxyz')"));
    assert!(boolean("/a{2,3}/.test('aaaa') && !/^a{2,3}$/.test('a')"));
    assert!(boolean("/^a+a$/.test('aa') && /^a+$/.test('aaaa')"));
}

#[test]
fn regex_sticky_flag() {
    // `lastIndex` anchors the (only) attempt: at 2 the `c` matches and the
    // match advances lastIndex to 3; from 3 (past the end) it fails and
    // sticky-with-global-style bookkeeping resets lastIndex to 0
    assert!(boolean(
        "(() => { const r = /c/y; r.lastIndex = 2; const ok = r.test('abc'); const l1 = r.lastIndex; const retry = r.test('abc'); return ok && l1 === 3 && !retry; })()"
    ));
    // sticky scans exactly at lastIndex: at 1 there is no `c`, so no match
    assert!(boolean(
        "(() => { const r = /c/y; r.lastIndex = 1; return !r.test('abc'); })()"
    ));
}

#[test]
fn regex_object_props_and_to_string() {
    assert_eq!(string("/ab+/i.toString()"), "/ab+/i");
    assert!(boolean(
        "(() => { const r = /ab/gi; return r.source === 'ab' && r.flags === 'gi' && r.global && r.ignoreCase && !r.sticky; })()"
    ));
    assert_eq!(string("new RegExp('a+', 'i').source"), "a+");
    assert!(boolean("new RegExp('a+', 'i').test('AA')"));
}

#[test]
fn regex_match_all_scan() {
    assert_eq!(
        string("'a1b22c333'.matchAll(/\\d+/g).map(m => m[0]).join(',')"),
        "1,22,333"
    );
    assert!(boolean("'xyz'.match(/q/) === null"));
    assert!(boolean("'ab1'.match(/\\d/).index === 2"));
    assert_eq!(string("'aXbXc'.split(/X/).join('-')"), "a-b-c");
}

#[test]
fn regex_unicode_and_indices_flags_are_accepted() {
    // `u`/`v` change the pattern grammar, not the match result for the
    // escapes supported here; `d` only adds index information.
    assert!(boolean("/[A-Z]/u.test('A')"));
    assert!(boolean("/a/d.test('a')"));
    assert!(boolean("/b/v.test('b')"));
    assert_eq!(string("'aXb'.split(/X/u).join('-')"), "a-b");
    assert_eq!(string("/a/gu.flags"), "gu");
}

#[test]
fn unicode_identifiers_and_object_keys() {
    // lodash's deburr table is written with accented keys
    assert_eq!(num("var \u{00C4} = 1; \u{00C4}"), 1.0);
    assert_eq!(string("({a\u{0301}: 'x'})['a\u{0301}']"), "x");
    assert_eq!(
        string("var m = {\u{00C4}:'A', \u{00E9}:'e'}; m['\u{00C4}'] + m['\u{00E9}']"),
        "Ae"
    );
}

#[test]
fn computed_keys_in_destructuring_patterns() {
    assert_eq!(num("var {['a' + 'b']: x} = {ab: 7}; x"), 7.0);
    assert_eq!(
        num("(function () { let k = 'a'; let {[k]: v, ...rest} = {a: 1, b: 2}; return v + rest.b; })()"),
        3.0
    );
    assert_eq!(
        string("(function () { let out = ''; let {[1]: v} = {1: 'n'}; out += v; return out; })()"),
        "n"
    );
}

#[test]
fn performance_now_is_a_monotonic_clock() {
    let first = num("performance.now()");
    let second = num("performance.now()");
    assert!(first.is_finite() && first >= 0.0);
    assert!(second >= first, "clock is monotonic");
    assert!(num("performance.timeOrigin") > 0.0);
    assert!(boolean("typeof performance.mark === 'function'"));
    assert!(boolean("typeof performance.measure === 'function'"));
    assert_eq!(num("performance.getEntries().length"), 0.0);
    assert_eq!(num("performance.getEntriesByName('x').length"), 0.0);
    // a bare global read must not be `undefined`
    assert!(boolean("performance.now !== undefined"));
}

#[test]
fn regex_unsupported_syntax_is_clean_error() {
    // lookbehind used to be the representative gap; it is implemented now
    // (see `regex_lookbehind`), so this pins a form that is still unknown
    assert!(matches!(err("'x'.replace(/(?|a)/, '')"), JsError::Syntax { .. }));
    assert!(matches!(err("/\\p{So}/u"), JsError::Syntax { .. }));
    assert!(matches!(err("/(?<1bad>a)/"), JsError::Syntax { .. }));
    assert!(matches!(err("/[unclosed/"), JsError::Syntax { .. }));
    assert!(matches!(err("/a+/q"), JsError::Syntax { .. })); // bad flag
}

#[test]
fn regex_catastrophic_backtracking_bounded() {
    // (a+)+$ on a long no-match input is the classic exponential blowup;
    // the per-call step budget must turn it into a quick honest "no match"
    // instead of hanging. The trailing `x` guarantees the match never
    // succeeds, which is what drives unbounded backtrackers to explode.
    let long = "a".repeat(400);
    let src = format!("/(a+)+$/.test('{}x')", long);
    assert_eq!(run(&src).as_bool(), Some(false));
    // nested-stars are the same shape
    let src2 = format!("/(a*)*b/.test('{}x')", long);
    assert_eq!(run(&src2).as_bool(), Some(false));
    // ...and the honest engine still works on the normal path
    // (all-(a+) match can succeed: (a+)+ consumes 'aaa', then $ holds)
    assert_eq!(run("/(a+)+$/.test('aaa')").as_bool(), Some(true));
}

#[test]
fn regex_lookahead() {
    assert!(boolean("/foo(?=bar)/.exec('foobar')[0] === 'foo'"));
    assert!(boolean("!/foo(?!bar)/.test('foobar') && /foo(?!bar)/.test('foobaz')"));
}

#[test]
fn regex_lookbehind() {
    // GitHub's code-view bundle rejects a line that starts or ends with `-`
    // with /^(?!-).*(?<!-)$/; the lookbehind was a SyntaxError, so the whole
    // module never compiled.
    assert!(boolean("/^(?!-).*(?<!-)$/.test('plain')"));
    assert!(boolean("!/^(?!-).*(?<!-)$/.test('-leading')"));
    assert!(boolean("!/^(?!-).*(?<!-)$/.test('trailing-')"));
    // a lookbehind must match ending exactly at the assertion's position
    assert!(boolean("/foo(?<=foo)bar/.test('foobar')"));
    assert!(boolean("/foo(?<=(foo))bar/.test('foobar')"));
    assert!(boolean("!/foo(?<=bar)bar/.test('foobar')"));
    assert!(boolean("/foo(?<!bar)bar/.test('foobar')"));
    assert!(boolean("/(?<=a)b/.exec('ab')[0] === 'b'"));
    assert!(boolean("!(/(?<=ab)c/.test('bc'))"));
    assert!(boolean("/(?<=\\d{2})px/.test('10px') && !/(?<=\\d{2})px/.test('5px')"));
    // zero-width: splitting after each dash is the classic use
    assert_eq!(
        string("'a-b-c'.split(/(?<=-)/).join('|')"),
        "a-|b-|c"
    );
    // a quantifier still applies to the group the lookbehind sits in
    assert!(boolean("/(?<=(a))\\1/.exec('aa')[0] === 'a'"));
}

// ---------------------------------------------------------------------------
// promises (real chaining + FIFO microtask jobs)
// ---------------------------------------------------------------------------

#[test]
fn promise_microtask_ordering() {
    // sync code runs first, then queued reactions fire FIFO
    assert_eq!(
        mstring(
            "let m = ''; Promise.resolve().then(() => m += '2'); m += '1'; Promise.resolve().then(() => m += '3').then(() => m += '4')"
        ),
        "1234"
    );
}

#[test]
fn promise_then_chains() {
    assert_eq!(mnum("let m = 0; Promise.resolve(2).then(v => v * 3).then(v => m = v)"), 6.0);
    // returning a promise from a handler chains (waits for adoption)
    assert_eq!(
        mnum("let m = 0; Promise.resolve(1).then(() => Promise.resolve(7)).then(v => m = v)"),
        7.0
    );
    // adopting an already-thenable resolves through it
    assert_eq!(
        mnum("let m = 0; Promise.resolve(Promise.resolve(4)).then(v => m = v)"),
        4.0
    );
    // .then on a rejected promise skips the fulfill handler, resumes at catch
    assert_eq!(
        mstring("let m = ''; Promise.reject('bad').then(() => m = 'fulfilled').catch(e => m = String(e))"),
        "bad"
    );
    // throw inside a fulfill handler rejects the derived promise
    assert_eq!(
        mstring("let m = ''; Promise.resolve().then(() => { throw 'boom'; }).catch(e => m = String(e))"),
        "boom"
    );
    // catch returns a fulfilled chain: further .then sees the handler result
    assert_eq!(
        mstring("let m = ''; Promise.reject('x').catch(e => e + '!!').then(v => m = String(v))"),
        "x!!"
    );
}

#[test]
fn promise_ctor_executor() {
    assert_eq!(mnum("let m = 0; new Promise(res => res(9)).then(v => m = v)"), 9.0);
    // synchronous executor throw rejects
    assert_eq!(
        mstring("let m = ''; new Promise(() => { throw 'fail'; }).catch(e => m = String(e))"),
        "fail"
    );
    // second resolve call is ignored
    assert_eq!(mnum("let m = 0; new Promise(res => { res(1); res(2); }).then(v => m = v)"), 1.0);
    // rejecting takes precedence over a later resolve
    assert_eq!(
        mstring("let m = ''; new Promise((res, rej) => { rej('no'); res('yes'); }).catch(e => m = String(e))"),
        "no"
    );
}

#[test]
fn promise_identity_and_cycle() {
    assert!(boolean("const p = Promise.resolve(3); Promise.resolve(p) === p"));
    // resolving a pending promise with itself is a TypeError rejection
    assert!(mbool(
        "let m = false; let res0 = null; const p = new Promise(res => { res0 = res }); p.then(null, () => { m = true }); res0(p)"
    ));
}

#[test]
fn promise_finally() {
    assert_eq!(
        mstring("let m = ''; Promise.resolve('v').finally(() => { m += 'F' }).then(v => { m += String(v) })"),
        "Fv"
    );
    // finally runs on rejection too and propagates the rejection
    assert_eq!(
        mstring("let m = ''; Promise.reject('e').finally(() => { m += 'F' }).catch(e => { m += String(e) })"),
        "Fe"
    );
}

#[test]
fn promise_all() {
    assert_eq!(
        mstring("let m = 0; Promise.all([1, Promise.resolve(2), 3]).then(vs => { m = vs.join('-') })"),
        "1-2-3"
    );
    assert_eq!(
        mstring("let m = ''; Promise.all([Promise.resolve(1), Promise.reject('stop')]).catch(e => { m = String(e) })"),
        "stop"
    );
    // empty input fulfills with an empty array
    assert_eq!(
        mstring("let m = 0; Promise.all([]).then(() => { m = 99 })"),
        "99"
    );
}

#[test]
fn promise_race_any_all_settled() {
    assert_eq!(mnum("let m = 0; Promise.race([9, 8]).then(v => { m = v })"), 9.0);
    assert_eq!(
        mstring("let m = ''; Promise.race([Promise.reject('L'), Promise.resolve('W')]).catch(e => { m = String(e) })"),
        "L"
    );
    // any: first fulfillment wins, rejections are skipped
    assert_eq!(
        mnum("let m = 0; Promise.any([Promise.reject('r'), Promise.resolve(5)]).then(v => { m = v })"),
        5.0
    );
    assert_eq!(
        mstring("let m = ''; Promise.any([Promise.reject('a'), Promise.reject('b')]).catch(() => { m = 'agg' })"),
        "agg"
    );
    assert_eq!(
        mstring(
            "let m = ''; Promise.allSettled([1, Promise.reject('x')]).then(vs => { m = vs.map(d => d.status[0]).join('') })"
        ),
        "fr"
    );
}

#[test]
fn promise_runaway_then_loop_is_bounded() {
    // a .then that re-enqueues forever must fail with the drain cap, not hang
    let e = err("function loop() { Promise.resolve().then(loop) } loop()");
    match e {
        JsError::Throw(v) => assert!(v.to_string().contains("RangeError")),
        other => panic!("expected Throw, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// async / await (run-to-completion microtask pump)
// ---------------------------------------------------------------------------

#[test]
fn async_returns_promise() {
    assert_eq!(mnum("let m = 0; async function f() { return 5 } f().then(v => { m = v })"), 5.0);
    // returned promise from async fn is adopted
    assert_eq!(
        mnum("let m = 0; async function f() { return Promise.resolve(3) } f().then(v => { m = v })"),
        3.0
    );
}

#[test]
fn await_values() {
    assert_eq!(
        mnum("let m = 0; async function f() { return await Promise.resolve(9) } f().then(v => { m = v })"),
        9.0
    );
    // await of a non-promise passes the value through
    assert_eq!(
        mnum("let m = 0; async function f() { let x = await 20; return x + 1 } f().then(v => { m = v })"),
        21.0
    );
    // await inside a loop pumps each pending step
    assert_eq!(
        mnum(
            "let m = 0; async function f() { let s = 0; for (let i = 1; i <= 3; i++) { s += await Promise.resolve(i) } return s } f().then(v => { m = v })"
        ),
        6.0
    );
    // async fn + plain chaining both refill a shared queue; each pending
    // await pumps what's queued (sequential awaits keep order). Note (honest
    // design tradeoff): awaiting an ALREADY-settled promise unwraps inline,
    // so microtask interleaving is not spec-FIFO here — "acbd"-style
    // interleaving assertions are out of scope for this wave
    assert_eq!(
        mstring(
            "let m = ''; async function f() { await Promise.resolve(); m += '1'; await Promise.resolve(); m += '2' } f().then(() => { m += '3' })"
        ),
        "123"
    );
}

#[test]
fn await_rejection_throws_into_frame() {
    assert_eq!(
        mstring(
            "let m = ''; async function f() { try { await Promise.reject('no') } catch (e) { return 'caught:' + String(e) } } f().then(v => { m = String(v) })"
        ),
        "caught:no"
    );
    // an async fn that throws rejects its promise; caller catches
    assert_eq!(
        mstring("let m = ''; async function f() { throw 'bang' } f().catch(e => { m = String(e) })"),
        "bang"
    );
}

#[test]
fn await_starved_by_macrotask_is_honest_error() {
    // an await of a promise that can only be settled by a timer/macrotask
    // cannot suspend in this wave: it fails with an honest error that the
    // caller can catch
    assert_eq!(
        mstring(
            "let m = ''; async function f() { await new Promise(() => {}) } f().catch(e => { m = String(e).slice(0, 15) })"
        ),
        "Error: await of"
    );
}

// ---------------------------------------------------------------------------
// generators (resumable mini-VM)
// ---------------------------------------------------------------------------

#[test]
fn generator_next_and_done() {
    assert_eq!(
        string(
            "function* g() { yield 1; yield 2; return 3 } const it = g(); const a = [it.next(), it.next(), it.next()]; a.map(x => x.done).join(',')"
        ),
        "false,false,true"
    );
    assert_eq!(
        num(
            "function* g() { yield 1; yield 2; return 3 } const it = g(); it.next().value + it.next().value + it.next().value"
        ),
        6.0
    );
    // done generators keep reporting done
    assert!(boolean(
        "function* g() { yield 1 } const it = g(); it.next(); it.next(); it.next().done"
    ));
}

#[test]
fn generator_feed_next_arg() {
    assert_eq!(
        num("function* g() { let x = yield 1; return x * 2 } const it = g(); it.next(); it.next(5).value"),
        10.0
    );
}

#[test]
fn generator_return() {
    assert_eq!(
        string("function* g() { yield 1; yield 2 } const it = g(); it.next(); it.return('stop').value"),
        "stop"
    );
}

#[test]
fn generator_yield_inside_if_and_loop() {
    assert_eq!(
        string(
            "function* g() { for (let i = 0; i < 3; i++) { if (i % 2) { yield 'o' + i } else { yield 'e' + i } } } const it = g(); [0, 1, 2].map(() => it.next().value).join(';')"
        ),
        "e0;o1;e2"
    );
    // generator holds state across suspensions (locals survive .next);
    // a feed value is bound by `let step = yield n` inside the loop body
    assert_eq!(
        num("function* c() { let n = 0; while (true) { let step = yield n; n += step } } const it = c(); it.next(); it.next(3).value + it.next(4).value"),
        10.0
    );
}

#[test]
fn escaped_accessor_and_async_modifiers_are_not_keywords() {
    for src in [
        "({ \\u0067\\u0065\\u0074 m() {} })",
        "({ \\u0073et m(v) {} })",
        "({ \\u0061sync m() {} })",
        "({ \\u0061sync *m() {} })",
        "class C { g\\u0065t value() {} }",
        "class C { s\\u0065t value(v) {} }",
        "class C { a\\u0073ync value() {} }",
        "class C { static a\\u0073ync value() {} }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // unescaped forms still work, and an escaped `get`/`async` stays a legal
    // property name when used as one
    assert_eq!(num("({ get x() { return 1 } }).x"), 1.0);
    assert_eq!(num("({ \\u0067et: 7 }).get"), 7.0);
}

#[test]
fn class_field_initializer_arguments_is_early_error() {
    for src in [
        "class C { x = arguments }",
        "class C { static x = arguments }",
        "class C { x = true ? {} : arguments }",
        "class C { [x] = arguments }",
        "class C { x = () => arguments }",
        "class C { x = () => { var t = () => arguments } }",
        "class C { #x = typeof arguments }",
    ] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
    // a nested plain function owns its `arguments`, so it is legal
    run("class C { x = function () { return arguments.length } }");
    // and an arrow outside a field initializer still sees the function's
    // arguments object
    assert_eq!(
        num("function f(){ var g = () => arguments.length; return g() } f(1, 2, 3)"),
        3.0
    );
}

#[test]
fn generator_yield_star_delegates() {
    // delegated values are forwarded in order, then the outer body continues
    assert_eq!(
        string(
            "function* inner(){ yield 1; yield 2 } \
             function* outer(){ yield* inner(); yield 3 } \
             [...outer()].join(',')"
        ),
        "1,2,3"
    );
    // the `.next(arg)` value is forwarded to the delegate; its return value
    // becomes the value of the `yield*` expression
    assert_eq!(
        num(
            "function* inner(){ var x = yield 1; return x * 2 } \
             function* outer(){ var r = yield* inner(); return r + 1 } \
             var it = outer(); it.next(); it.next(5).value"
        ),
        11.0
    );
    // any iterable works, and the delegate's final value binds normally
    assert_eq!(
        string(
            "function* g(){ var r = yield* [7, 8]; return 'r' + r } \
             var it = g(); it.next().value + ',' + it.next().value + ',' + it.next().done"
        ),
        "7,8,true"
    );
    assert_eq!(string("function* g(){ yield* 'ab' } [...g()].join('-')"), "a-b");
    // a non-iterable operand throws a TypeError at first resume
    assert!(matches!(
        err("function* g(){ yield* 5 } g().next()"),
        JsError::Throw(_)
    ));
    // outside a generator, sloppy `yield` is an ordinary identifier, so
    // `yield* [1]` is a multiplication rather than a delegation
    assert_eq!(num("var yield = 2; yield * 3"), 6.0);
}

#[test]
fn generator_throw_unsupported() {
    assert!(matches!(err("function* g() { yield 1 } const it = g(); it.throw(1)"), JsError::Throw(_)));
}

#[test]
fn generator_yield_lifted_out_of_expressions() {
    // call arguments: operands before the suspension are frozen first, and
    // everything after it still evaluates in source order on resume
    assert_eq!(
        string(
            "function* g(){ var log=''; function t(x){ log+=x; return x } \
             var r = t('a') + (yield t('b')) + t('c'); return log + '|' + r } \
             var it = g(); it.next().value + ';' + it.next(5).value"
        ),
        "b;abc|a5c"
    );
    // `&&` / `||` return operand values and only evaluate the right side
    // when the operator actually reaches it
    assert_eq!(
        string(
            "function* g(){ var r = false && (yield 1); return 'v' + r } \
             var it = g(); var a = it.next(); a.done + ',' + a.value"
        ),
        "true,vfalse"
    );
    assert_eq!(
        string(
            "function* g(){ var r = true && (yield 1); return r + 10 } \
             var it = g(); it.next().value + ',' + it.next(5).value"
        ),
        "1,15"
    );
    assert_eq!(
        string(
            "function* g(){ var r = 7 || (yield 1); return r } \
             var it = g(); var a = it.next(); a.done + ',' + a.value"
        ),
        "true,7"
    );
    // conditional expression
    assert_eq!(
        string(
            "function* g(c){ var r = c ? (yield 'y') : 'n'; return r } \
             var n = g(false); var a = n.next(); var t = g(true); \
             a.done + ',' + a.value + ',' + t.next().value + ',' + t.next(9).value"
        ),
        "true,n,y,9"
    );
    // assignment target base is evaluated before the suspension
    assert_eq!(
        string(
            "function* g(){ var o = {v:1}; o.v = (yield 3); return o.v } \
             var it = g(); it.next().value + ',' + it.next(9).value"
        ),
        "3,9"
    );
    // a member call keeps its receiver across the lift
    assert_eq!(
        num(
            "function* g(){ var o = { k: 4, m: function(x){ return this.k + x } }; \
             var r = o.m(yield 1); return r } var it = g(); it.next(); it.next(3).value"
        ),
        7.0
    );
    // nested yields suspend in source order
    assert_eq!(
        string(
            "function* g(){ var r = (yield 1) + (yield 2); return r } var it = g(); \
             it.next().value + ',' + it.next(10).value + ',' + it.next(20).value"
        ),
        "1,2,30"
    );
    // an operand before a suspending one is read before its side effect
    assert_eq!(
        num(
            "function* g(){ var x = 1; var r = x + (yield (x = 2)); return r } \
             var it = g(); it.next(); it.next(5).value"
        ),
        6.0
    );
    // the reference-site shape: short-circuit + yield inside a loop body
    assert_eq!(
        string(
            "function* g(a){ for (var i = 0; i < 2; i++) { a && (yield i) } } \
             var it = g(true); it.next().value + ',' + it.next().value"
        ),
        "0,1"
    );
    assert!(boolean(
        "function* g(a){ a && (yield 1) } var it = g(false); it.next().done"
    ));
    // declarator lists keep their order (and split when a later one suspends)
    assert_eq!(
        string(
            "function* g(){ var a = 1, b = a + (yield 2); return a + ',' + b } \
             var it = g(); it.next().value + ',' + it.next(3).value"
        ),
        "2,1,4"
    );
    // a yield in an `if` condition is evaluated once, before the branch
    assert_eq!(
        string(
            "function* g(){ if (yield 1) { return 't' } return 'f' } \
             var it = g(); it.next().value + ',' + it.next(true).value"
        ),
        "1,t"
    );
    // array literal with an element that suspends
    assert_eq!(
        string(
            "function* g(){ var a = [yield 1, 2]; return a.join('-') } \
             var it = g(); it.next(); it.next(9).value"
        ),
        "9-2"
    );
}

#[test]
fn generator_try_resumption() {
    // a yield inside the try block suspends; the frame stays live and the
    // finally runs when the block finishes normally
    assert_eq!(
        string(
            "var log=''; function* g(){ try { yield 1; yield 2 } finally { log += 'f' } } \
             var it = g(); var a = it.next(), b = it.next(), c = it.next(); \
             a.value + ',' + b.value + ',' + c.done + ',' + log"
        ),
        "1,2,true,f"
    );
    // a throw resumes in the same try scope and lands in its catch
    assert_eq!(
        string(
            "function* g(){ try { yield 1; throw 'boom' } catch (e) { return 'caught:' + e } } \
             var it = g(); it.next(); it.next().value"
        ),
        "caught:boom"
    );
    // the catch block may itself suspend
    assert_eq!(
        string(
            "function* g(){ try { throw 'e' } catch (e) { yield e; yield 'after' } } \
             var it = g(); it.next().value + ',' + it.next().value"
        ),
        "e,after"
    );
    // `return` runs the finally before the generator completes
    assert_eq!(
        string(
            "var log=''; function* g(){ try { yield 1; return 'r' } finally { log += 'f' } } \
             var it = g(); var a = it.next(), b = it.next(); \
             a.value + ',' + b.value + ',' + b.done + ',' + log"
        ),
        "1,r,true,f"
    );
    // yield inside finally: the try frame waits under it
    assert_eq!(
        string(
            "function* g(){ try { yield 1 } finally { yield 2 } } \
             var it = g(); var a = it.next(), b = it.next(), c = it.next(); \
             a.value + ',' + b.value + ',' + b.done + ',' + c.done"
        ),
        "1,2,false,true"
    );
    // break out of a loop runs the finally on the way
    assert_eq!(
        string(
            "var log=''; function* g(){ for (var i=0;i<3;i++){ try { if (i===1) break; yield i } \
             finally { log += i } } return 'end' } \
             var it = g(); var a = it.next(), b = it.next(); \
             a.value + ',' + b.done + ',' + b.value + ',' + log"
        ),
        "0,true,end,01"
    );
    // `.return(v)` runs the finally, then completes with v
    assert_eq!(
        string(
            "var log=''; function* g(){ try { yield 1 } finally { log += 'f' } } \
             var it = g(); it.next(); var r = it.return('v'); \
             r.value + ',' + r.done + ',' + log"
        ),
        "v,true,f"
    );
    // `.return(v)` stops at a yield in the finally and stays resumable; the
    // pending return is applied once the finally completes
    assert_eq!(
        string(
            "var log=''; function* g(){ try { yield 1 } finally { yield 'fin'; log += 'x' } } \
             var it = g(); it.next(); var r = it.return('v'); \
             r.value + ',' + r.done + ',' + log + '|' + it.next().value + ',' + log"
        ),
        "fin,false,|v,x"
    );
    // a throw in finally replaces the pending completion
    assert!(matches!(
        err(
            "function* g(){ try { yield 1; throw 'a' } finally { throw 'b' } } \
             var it = g(); it.next(); it.next()"
        ),
        JsError::Throw(_)
    ));
}

#[test]
fn generator_yield_positions_still_gated() {
    // positions the lift does not rewrite stay honest parse errors
    assert!(matches!(
        err("function* g() { while (yield 1) {} }"),
        JsError::Syntax { .. }
    ));
    assert!(matches!(
        err("function* g() { for (; yield 1; ) {} }"),
        JsError::Syntax { .. }
    ));
    assert!(matches!(
        err("function* g() { return yield 1 }"),
        JsError::Syntax { .. }
    ));
    assert!(matches!(
        err("function* g() { throw yield 1 }"),
        JsError::Syntax { .. }
    ));
    // a bare decl-init yield inside a generator body is exactly the
    // supported form, so it must exec without a SyntaxError
    let mut e = JsEngine::new();
    assert!(matches!(
        e.exec("function* g() { const y = yield; return y } const it = g(); it.next().done"),
        Ok(_)
    ));
}

// ---------------------------------------------------------------------------
// destructuring + spread/rest
// ---------------------------------------------------------------------------

#[test]
fn generator_is_iterable() {
    // a generator object is its own iterator: spread, for-of, destructuring
    assert_eq!(
        string("function* g() { yield 1; yield 2 } [...g()].join(',')"),
        "1,2"
    );
    assert_eq!(
        num("function* g() { yield 3; yield 4 } const [, b] = g(); b"),
        4.0
    );
    assert_eq!(
        num("function* g() { yield 3; yield 4 } var t = 0; for (var v of g()) t += v; t"),
        7.0
    );
}

#[test]
fn destructuring_arrays() {
    assert_eq!(num("const [a, b] = [10, 20]; a + b"), 30.0);
    assert_eq!(num("const [a, , c = 5] = [1, 2]; a + c"), 6.0);
    assert_eq!(num("const [a, ...rest] = [1, 2, 3, 4]; rest[1]"), 3.0);
    assert_eq!(num("const [a, [b]] = [1, [9]]; a + b"), 10.0);
    assert_eq!(string("const [a = 'd'] = []; a"), "d");
    assert_eq!(string("const [, b] = ['x', 'y']; b"), "y");
    // a rest element may bind a nested pattern, not just a name
    assert_eq!(num("const [a, ...[b, c]] = [1, 2, 3]; a + b + c"), 6.0);
    assert_eq!(num("var f = ([...[x, y, z]]) => x + y + z; f([3, 4, 5])"), 12.0);
    assert_eq!(num("const [...{length}] = [1, 2]; length"), 2.0);
    assert_eq!(num("let p, q; [...[p, q]] = [8, 9]; p + q"), 17.0);
    // a rest element must be last (and takes no trailing comma)
    assert!(matches!(
        err("function f([...a, b]) {}"),
        JsError::Syntax { .. }
    ));
    assert!(matches!(
        err("var f = ([...a,]) => a"),
        JsError::Syntax { .. }
    ));
}

#[test]
fn destructuring_objects() {
    assert_eq!(num("const { x, y: z } = { x: 3, y: 4 }; x * z"), 12.0);
    assert_eq!(num("const { p = 7 } = {}; p"), 7.0);
    assert_eq!(string("const { list: [first] } = { list: ['A'] }; first"), "A");
    assert!(boolean("const { q, ...others } = { q: 1, r: 2 }; others.r === 2 && others.q === undefined"));
    // a trailing comma is allowed in an object pattern (decl, param, arrow)
    assert_eq!(num("const { a, } = { a: 1 }; a"), 1.0);
    assert_eq!(num("function fn1({ x, }) { return x } fn1({ x: 5 })"), 5.0);
    assert_eq!(num("var f = ({ x, }) => x; f({ x: 23 })"), 23.0);
    // object rest must be last
    assert!(matches!(
        err("const { ...a, b } = {}"),
        JsError::Syntax { .. }
    ));
    assert!(matches!(
        err("0, { ...a, b } = {}"),
        JsError::Syntax { .. }
    ));
    assert!(matches!(
        err("0, { ...a, } = {}"),
        JsError::Syntax { .. }
    ));
    // ...but both are legal expressions
    assert_eq!(num("var a = { x: 1 }; ({ ...a, }).x"), 1.0);
    assert_eq!(num("var a = [1, 2]; [...a,].length"), 2.0);
}

#[test]
fn string_line_continuations() {
    // LF-only and CRLF continuations both vanish from the value
    assert_eq!(string("var s = 'a\\\nb'; s"), "ab");
    assert_eq!(string("var s = 'a\\\r\nb'; s"), "ab");
    assert_eq!(string("var s = \"x\\\r\ny\\\rz\"; s"), "xyz");
    // an escaped quote still terminates the escape, not the string
    assert_eq!(string("var s = 'a\\'b'; s"), "a'b");
}

#[test]
fn regex_literals_validate_at_parse_time() {
    // an invalid pattern is a SyntaxError even if the literal never runs
    assert!(matches!(err("var r = /(/;"), JsError::Syntax { .. }));
    assert!(matches!(err("var r = /[a/;"), JsError::Syntax { .. }));
    assert!(matches!(
        err("if (false) { var r = /(?<a>x/; }"),
        JsError::Syntax { .. }
    ));
    // a valid pattern still parses and matches
    assert!(boolean("var r = /a+b/; r.test('aab')"));
}

#[test]
fn function_length_counts_leading_params() {
    assert_eq!(num("(function (a, b) {}).length"), 2.0);
    assert_eq!(num("(function (a, b = 1, c) {}).length"), 1.0);
    assert_eq!(num("(function (a, ...r) {}).length"), 1.0);
    assert_eq!(num("(function (...r) {}).length"), 0.0);
    assert_eq!(num("((a, b = 1) => {}).length"), 1.0);
    assert_eq!(num("var C = class { *m(a, b = 42, z) {} }; C.prototype.m.length"), 1.0);
    assert_eq!(
        num("var o = { set m(v = 1) {} }; Object.getOwnPropertyDescriptor(o, 'm').set.length"),
        0.0
    );
}

#[test]
fn named_function_self_binding_is_immutable() {
    // a named function expression's own name cannot be reassigned (sloppy)
    assert!(boolean(
        "var f = function g() { g = 1; return typeof g }; f() === 'function'"
    ));
    assert!(boolean(
        "var f = function* g() { g = 1; yield typeof g }; f().next().value === 'function'"
    ));
    // ...but a function *declaration* binding stays mutable
    assert_eq!(num("function f() { f = 1; return f } f()"), 1.0);
}

#[test]
fn lexical_declarations_need_a_block() {
    // LexicalDeclaration and ClassDeclaration are not Statements
    for src in [
        "if (true) let x = 1;",
        "if (true) const x = 1;",
        "if (true) {} else let x = 1;",
        "while (false) let x;",
        "do let x; while (false)",
        "for (;;) const x = 1;",
        "with ({}) let x = 1;",
        "l: let x = 1;",
        "if (true) class C {}",
    ] {
        assert!(
            err(src).to_string().contains("SyntaxError"),
            "expected SyntaxError for: {src}"
        );
    }
    // a block body may contain one
    assert!(boolean("if (true) { let x = 1; } true"));
    assert!(boolean("while (false) { const x = 1; } true"));
}

#[test]
fn const_requires_an_initializer() {
    for src in ["const x;", "const x = 1, y;", "for (const x;;) {}"] {
        assert!(
            err(src).to_string().contains("SyntaxError"),
            "expected SyntaxError for: {src}"
        );
    }
    // a for-in/of binding is not a LexicalDeclaration
    assert_eq!(num("var n = 0; for (const x in {a: 1}) n++; n"), 1.0);
    assert_eq!(num("var n = 0; for (const x of [1, 2]) n++; n"), 2.0);
}

#[test]
fn arrow_parameters_cannot_be_split_from_the_arrow() {
    // ArrowParameters [no LineTerminator here] =>
    for src in [
        "var af = x\n=> {};",
        "var af = x\n=> x;",
        "var af = ()\n=> {};",
        "var af = (a, b)\n=> {};",
    ] {
        assert!(
            err(src).to_string().contains("SyntaxError"),
            "expected SyntaxError for: {src:?}"
        );
    }
    // same line is fine, and a newline in the *body* is fine
    assert_eq!(num("var af = x => x + 1; af(2)"), 3.0);
    assert_eq!(num("var af = (a, b) =>\n a + b; af(2, 3)"), 5.0);
}

#[test]
fn arrow_parameters_reject_duplicate_names() {
    // arrows reject duplicate bound names in every mode
    for src in [
        "var af = (x, x) => 1;",
        "var af = (x, [x]) => 1;",
        "var af = ({y: x, x}) => 1;",
        "var af = (x, ...x) => 1;",
    ] {
        assert!(
            err(src).to_string().contains("SyntaxError"),
            "expected SyntaxError for: {src:?}"
        );
    }
    assert_eq!(num("var af = (x, y) => x + y; af(1, 2)"), 3.0);
}

#[test]
fn nested_pattern_elements_with_a_default() {
    // a nested pattern element with a default was parsed as a PatAssign and
    // must still convert into a loop/assignment pattern
    for src in [
        "for ([{} = 1] of [[2]]) {}",
        "for ([{} = 1] of []) {}",
        "var a = []; [{} = 1] = a;",
        "var a = [1]; var x; [{} = 1, x = 2] = a;",
        "for ({a: {} = 1} of []) {}",
        "for ({a: [ {} = 1 ]} of []) {}",
    ] {
        let mut e = JsEngine::new();
        if let Err(err) = e.exec(src) {
            panic!("failed: {src}\n  {err}");
        }
    }
}

#[test]
fn object_literal_proto_setter() {
    // the colon form sets the prototype instead of creating an own property
    assert_eq!(num("var o = { __proto__: { a: 1 } }; o.a"), 1.0);
    assert_eq!(
        num("var o = { __proto__: null }; Object.getPrototypeOf(o) === null ? 1 : 0"),
        1.0
    );
    assert_eq!(num("var o = { '__proto__': { a: 2 } }; o.a"), 2.0);
    assert_eq!(
        num("var o = { __proto__: 5 }; o.hasOwnProperty('__proto__') ? 1 : 0"),
        0.0
    );
    // a symbol is a primitive, not a prototype
    assert!(boolean(
        "var o = { __proto__: Symbol('s') }; \
         Object.getPrototypeOf(o) === Object.prototype"
    ));
    // an inherited `__proto__` setter must not run
    assert_eq!(
        num("Object.defineProperty(Object.prototype, '__proto__', \
             { set: function(){ throw 'bad'; } }); \
             var o = { __proto__: { a: 5 } }; o.a"),
        5.0
    );
    // shorthand and computed forms are ordinary properties
    assert_eq!(
        num("var __proto__ = { a: 3 }; var o = { __proto__ }; \
             o.hasOwnProperty('__proto__') ? o.__proto__.a : -1"),
        3.0
    );
    assert_eq!(
        num("var o = { __proto__: {a:1}, ['__proto__']: {b:2} }; o.__proto__.b + o.a"),
        3.0
    );
    // duplicate colon-form `__proto__` is an early error
    assert!(matches!(
        err("({ __proto__: 1, __proto__: 2 })"),
        JsError::Syntax { .. }
    ));
    assert!(matches!(
        err("({ __proto__: 1, 'other': 0, '__proto__': 2 })"),
        JsError::Syntax { .. }
    ));
    // in a pattern it is an ordinary property key, and may repeat
    assert_eq!(
        num("var src = {}; src.__proto__ = 7; var a, b; \
             ({ __proto__: a, __proto__: b } = src); a + b"),
        14.0
    );
    // an explicitly null prototype is remembered, not lazily filled in
    assert!(boolean("Object.getPrototypeOf(Object.create(null)) === null"));
    assert!(boolean(
        "var o = {}; Object.setPrototypeOf(o, null); \
         Object.getPrototypeOf(o) === null"
    ));
}

#[test]
fn yield_is_an_identifier_outside_generators() {
    // sloppy mode: `yield` is an ordinary IdentifierReference / BindingIdentifier
    assert_eq!(num("var yield = 5; yield + 1"), 6.0);
    assert_eq!(num("var yield; yield = 7; yield"), 7.0);
    assert_eq!(num("function f(yield) { return yield; } f(3)"), 3.0);
    assert_eq!(num("var f; (function (yield = 4) { f = yield; }()); f"), 4.0);
    assert_eq!(num("var yield = 9; var o = { yield }; o.yield"), 9.0);
    assert_eq!(num("var yield = 2; var x; ({x = yield} = {}); x"), 2.0);
    assert_eq!(num("var yield = 8; var r; r = {yield} = {yield: 1}; yield"), 1.0);
    assert!(!boolean("var yield = 1; !yield"));
    assert_eq!(num("var yield = 1; typeof yield === 'number' ? 1 : 0"), 1.0);
    // inside a generator it is still a yield expression
    assert_eq!(
        num("var g = function*() { var x = yield 1; return x; }; \
             var it = g(); it.next(); it.next(5).value"),
        5.0
    );
    // a class body (and every nested function) is strict mode code, where
    // `yield` stays reserved
    assert!(matches!(err("class A { m() { var yield; } }"), JsError::Syntax { .. }));
    assert!(matches!(err("class A { m(yield) {} }"), JsError::Syntax { .. }));
    assert!(matches!(err("class yield {}"), JsError::Syntax { .. }));
    assert!(matches!(err("var C = class yield {};"), JsError::Syntax { .. }));
    assert!(matches!(
        err("class A { m() { function h() { yield = 1; } } }"),
        JsError::Syntax { .. }
    ));
    // an object-literal method is not strict code
    assert_eq!(num("var o = { m(yield) { return yield; } }; o.m(4)"), 4.0);
    // a function expression may be named `yield` in sloppy code
    assert_eq!(num("var f = function yield() { return 1; }; f()"), 1.0);
    assert_eq!(
        string("(function yield() { return typeof yield; })()"),
        "function"
    );
    // an enclosing generator does not reserve the nested function's name
    assert_eq!(
        num("var g = function*() { return (function yield() { return 3; })(); }; \
             g().next().value"),
        3.0
    );
    // but a generator function expression cannot itself be named `yield`
    assert!(matches!(
        err("var g = function* yield() {};"),
        JsError::Syntax { .. }
    ));
}

#[test]
fn compound_assignment_keeps_its_with_reference() {
    // the reference is captured before GetValue: a getter that deletes its
    // own property must not send the write to an outer binding
    assert_eq!(
        num("(function(){ var x = 0; var scope = { get x() { delete this.x; return 2; } }; \
             with (scope) { x ^= 3; } return scope.x * 10 + x; })()"),
        10.0
    );
    assert_eq!(
        num("(function(){ var x = 0; var o = { x: 5 }; \
             with (o) { x += 1; } return o.x * 10 + x; })()"),
        60.0
    );
    // a name the with object does not hold still writes the outer binding
    assert_eq!(
        num("(function(){ var x = 1; var o = {}; \
             with (o) { x += 1; } return x; })()"),
        2.0
    );
    // the right-hand side is evaluated after the target is read
    assert_eq!(
        string("(function(){ var log = ''; var o = { get x() { log += 'g'; return 1; }, \
             set x(v) { log += 's'; } }; \
             with (o) { x += (log += 'r', 1); } return log; })()"),
        "grs"
    );
    // logical assignment shares the resolution
    assert_eq!(
        num("(function(){ var x = 0; var scope = { get x() { delete this.x; return 0; } }; \
             with (scope) { x ||= 9; } return scope.x * 10 + x; })()"),
        90.0
    );
}

#[test]
fn computed_keys_in_assignment_patterns() {
    // `{[expr]: target}` in an assignment/loop destructuring pattern
    assert_eq!(
        num("var x, n = 0; for ({['x']: x} of [{x: 7}]) n += x; n"),
        7.0
    );
    assert_eq!(
        num("var a = 'foo', b, rest, n = 0; \
             for ({[a]: b, ...rest} of [{foo: 1, bar: 2, baz: 3}]) n = b + rest.bar + rest.baz; n"),
        6.0
    );
    assert_eq!(num("var k = 'a', v = 0; ({[k]: v} = {a: 9}); v"), 9.0);
    assert_eq!(num("var k = 'a', v = 0; ({[k]: v = 5} = {}); v"), 5.0);
    // a nested pattern element with a default
    assert_eq!(
        num("var n = 0; for ([ {} = 1 ] of [[2]]) n++; n"),
        1.0
    );
}

#[test]
fn non_ascii_whitespace_ends_an_identifier() {
    // a non-ASCII space or line terminator right after an identifier must not
    // be swallowed into it (`x\u00A0+=`, `x\u2028+=`, ...)
    assert_eq!(num("var x = -1; x\u{2028}+= -1; x"), -2.0);
    assert_eq!(num("var x = -1; x\u{2029}+= -1; x"), -2.0);
    assert_eq!(num("var x = -1; x\u{a0}+= -1; x"), -2.0);
    assert_eq!(num("var x = -1; x \u{b}\u{c} += -1; x"), -2.0);
    assert_eq!(num("var x = 1; x\u{a0}++ ; x"), 2.0);
    // real non-ASCII identifiers still work
    assert_eq!(num("var \u{c4} = 4; \u{c4}"), 4.0);
    assert_eq!(num("var \u{3b1}\u{3b2} = 2; \u{3b1}\u{3b2}"), 2.0);
}

#[test]
fn method_parameters_must_be_unique() {
    // MethodDefinition uses UniqueFormalParameters: no duplicate bound names
    // even in sloppy mode, unlike a plain function declaration.
    for src in [
        "({ foo(a, a) {} });",
        "({ async foo(a, a) {} });",
        "({ *foo(a, a) {} });",
        "({ ['x'](a, a) {} });",
        "({ foo({a}, [a]) {} });",
        "(class { m(a, a) {} });",
        "(class { constructor(a, a) {} });",
    ] {
        assert!(
            err(src).to_string().contains("SyntaxError"),
            "expected SyntaxError for: {src:?}"
        );
    }
    // plain functions still allow it in sloppy mode
    let mut e = JsEngine::new();
    assert!(e.exec("function f(a, a) { return a; }").is_ok());
    // a method may still use distinct names, including a non-simple list
    assert_eq!(num("({ m(a, b = 2) { return a + b; } }).m(1)"), 3.0);
}

#[test]
fn direct_eval_cannot_hoist_var_over_param_bindings() {
    // a parameter default's eval must not declare a var over the implicit
    // `arguments` object (EvalDeclarationInstantiation's collision walk)
    assert!(boolean(
        "(function () { function f(p = eval('var arguments')) {} \
         try { f(); return false; } catch (e) { return e instanceof SyntaxError; } })()"
    ));
    // ... nor over a parameter binding, even one declared after the default
    assert!(boolean(
        "(function () { function f(p = eval('var arguments'), arguments) {} \
         try { f(); return false; } catch (e) { return e instanceof SyntaxError; } })()"
    ));
    // a plain body eval may still declare `arguments`
    assert!(boolean(
        "(function () { function f() { try { eval('var arguments'); return true; } \
         catch (e) { return false; } } return f(); })()"
    ));
    // an arrow has no implicit arguments object, so this one is allowed
    assert!(boolean(
        "(function () { var f = (p = eval('var arguments')) => {}; \
         try { f(); return true; } catch (e) { return false; } })()"
    ));
    // a block `let` between the eval and the function environment collides
    assert!(boolean(
        "(function () { function f() { { let x; try { eval('var x'); return false; } \
         catch (e) { return e instanceof SyntaxError; } } } return f(); })()"
    ));
}

#[test]
fn with_primitive_boxes_to_an_object() {
    // ToObject: the wrapper has no `foo`, so the assignment falls through
    assert!(boolean("var o = 2, foo = 1; with (o) { foo = 42; } foo === 42"));
    assert!(boolean(
        "var o = true, foo = 1; with (o) { foo = 42; } foo === 42"
    ));
    assert!(boolean(
        "var o = 'str', foo = 1; with (o) { foo = 42; } foo === 42"
    ));
    // null/undefined still throw (ToObject has no nullish object)
    assert!(err("with (null) {}").to_string().contains("TypeError"));
    assert!(err("with (void 0) {}").to_string().contains("TypeError"));
}

#[test]
fn delete_resolves_through_with() {
    // `delete p` inside `with (o)` deletes o's property
    assert!(boolean(
        "var o = {a: 1}; with (o) { delete a; } o.a === undefined && !('a' in o)"
    ));
    // an ordinary binding is not deletable
    assert!(boolean(
        "var x = 1; var r; with ({}) { r = delete x; } r === false && x === 1"
    ));
    // an unresolvable name deletes successfully
    assert!(boolean("delete notDeclaredAnywhere"));
}

#[test]
fn object_cover_initialized_names() {
    // `{x = default}` in assignment destructuring
    assert_eq!(num("var x; ({x = 5} = {}); x"), 5.0);
    assert_eq!(num("var x; ({x = 5} = {x: 1}); x"), 1.0);
    assert!(boolean("var x; ({x = 5} = {x: null}); x === null"));
    // nested object and array elements
    assert_eq!(num("var x; [{x = 3}] = [{}]; x"), 3.0);
    assert_eq!(num("var x; ({a: {x = 4}} = {a: {}}); x"), 4.0);
    // for-of head assignment pattern
    assert_eq!(
        num("var x, r; for ({x = 9} of [{}]) r = x; r"),
        9.0
    );
    // the shorthand initializer gives an anonymous function the bound name
    assert!(boolean(
        "var fn; ({fn = function () {}} = {}); fn.name === 'fn'"
    ));
    // a surviving cover form in value position is a SyntaxError
    assert!(matches!(err("var o = {x = 1};"), JsError::Syntax { .. }));
    assert!(matches!(err("var o = [{x = 1}];"), JsError::Syntax { .. }));
    assert!(matches!(err("f({x = 1});"), JsError::Syntax { .. }));
    // ...but `{x: y = 1}` is an ordinary value expression
    assert_eq!(num("var y; var o = {x: y = 1}; o.x"), 1.0);
    // ordinary object/array literals and classic for-inits still parse
    assert_eq!(num("var o = {a: {b: 1}}; o.a.b"), 1.0);
    assert_eq!(num("var n = 0; for (var i = {v: 1}; i && n < 1; n++) ; n"), 1.0);
    assert_eq!(num("var x; for (var {x: x} of [{x: 7}]) ; x"), 7.0);
    assert_eq!(num("var x; let {x: y = 2} = {}; y"), 2.0);
}

#[test]
fn function_name_and_length_descriptors() {
    // name/length are non-writable, non-enumerable, configurable own props
    assert!(boolean(
        "var f = function foo() {}; var d = Object.getOwnPropertyDescriptor(f, 'name'); d.value === 'foo' && d.writable === false && d.enumerable === false && d.configurable === true"
    ));
    assert!(boolean(
        "var f = function (a, b) {}; var d = Object.getOwnPropertyDescriptor(f, 'length'); d.value === 2 && d.writable === false && d.enumerable === false && d.configurable === true"
    ));
    // a sloppy assignment to a non-writable slot is a silent no-op
    assert!(boolean(
        "var f = function () {}; f.name = 'other'; f.length = 9; f.name === 'f' && f.length === 0"
    ));
    assert!(boolean(
        "var f = function () {}; !Object.prototype.propertyIsEnumerable.call(f, 'name')"
    ));
    // deleting the synthesized slot removes it for good
    assert!(boolean(
        "var f = function () {}; delete f.name; !Object.prototype.hasOwnProperty.call(f, 'name') && f.name === undefined && Object.getOwnPropertyDescriptor(f, 'name') === undefined"
    ));
}

#[test]
fn array_destructuring_pulls_lazily_and_closes() {
    // only as many `next()` calls as the pattern needs
    assert!(boolean(
        "var c = 0; function* g() { c++; yield 1; c++; yield 2 } var [a] = g(); a === 1 && c === 1"
    ));
    // an elision steps once, it does not drain the iterator
    assert!(boolean(
        "var c = 0; function* g() { c++; yield; c++; yield } var [,] = g(); c === 1"
    ));
    // an exhausted iterator is not closed; an unfinished one is
    assert!(boolean(
        "var closed = 0; var iter = {}; iter[Symbol.iterator] = function () { return { next: function () { return { value: 1, done: false } }, return: function () { closed++; return {} } } }; var [x] = iter; x === 1 && closed === 1"
    ));
    assert!(boolean(
        "var closed = 0; var iter = {}; iter[Symbol.iterator] = function () { return { next: function () { return { value: 1, done: true } }, return: function () { closed++; return {} } } }; var [x] = iter; x === undefined && closed === 0"
    ));
    // elision counts: a trailing comma is not extra hole
    assert_eq!(num("[1,].length"), 1.0);
    assert_eq!(num("[,].length"), 1.0);
    assert_eq!(num("[,,].length"), 2.0);
    assert_eq!(num("var [a, b,] = [1, 2]; a + b"), 3.0);
    assert_eq!(num("[1, 2,].length"), 2.0);
}

#[test]
fn object_rest_reads_getters_and_skips_hidden() {
    // an accessor runs once, and its value lands on the rest object
    assert_eq!(
        num("var c = 0; var r; ({ ...r } = { get v() { c++; return 2 } }); c === 1 && r.v === 2 ? 1 : 0"),
        1.0
    );
    // non-enumerable own props are not copied by object rest
    assert!(boolean(
        "var o = { a: 1 }; Object.defineProperty(o, 'x', { value: 4, enumerable: false }); var r = { ...o }; !('x' in r) && r.a === 1"
    ));
    // ...nor by Object.keys / for-in
    assert!(boolean(
        "var o = {}; Object.defineProperty(o, 'x', { value: 4, enumerable: false }); Object.keys(o).length === 0"
    ));
}

#[test]
fn anonymous_class_takes_binding_name() {
    assert_eq!(string("var cls = class {}; cls.name"), "cls");
    assert_eq!(string("let cls; cls = class {}; cls.name"), "cls");
    assert_eq!(string("const [cls = class {}] = []; cls.name"), "cls");
    assert_eq!(string("const { cls = class {} } = {}; cls.name"), "cls");
    assert_eq!(
        string("function f(cls = class {}) { return cls.name } f()"),
        "cls"
    );
    // an explicit name is kept
    assert_eq!(string("var cls = class Named {}; cls.name"), "Named");
    // shortcut: parentheses are not preserved in the AST, so a parenthesized
    // class is named too (the function path has the same limitation)
}

#[test]
fn computed_object_accessors() {
    assert_eq!(
        num("var k = 'x'; var o = { get [k]() { return 5 } }; o.x"),
        5.0
    );
    assert_eq!(
        num("var k = 'y'; var o = { set [k](v) { this.v = v } }; o.y = 7; o.v"),
        7.0
    );
    // the key expression is evaluated once, at object creation
    assert_eq!(
        num("var n = 0; var o = { get ['a' + (n++)]() { return 1 } }; n"),
        1.0
    );
    // a computed accessor pair shares the key
    assert_eq!(
        num("var k = 'z'; var o = { get [k]() { return this._v }, set [k](v) { this._v = v } }; o.z = 9; o.z"),
        9.0
    );
}

#[test]
fn destructuring_params_and_rest() {
    assert_eq!(
        num("function f({ a, b }, [c, d], ...rest) { return a + b + c + d + rest.length } f({ a: 1, b: 2 }, [3, 4], 9, 9, 9)"),
        13.0
    );
    assert_eq!(num("function g([x, y]) { return x * y } g([6, 7])"), 42.0);
    // parameter defaults evaluate when undefined is passed
    assert_eq!(num("function h(a = 3) { return a } h(1) + h()"), 4.0);
}

#[test]
fn destructuring_assignment() {
    assert_eq!(num("let a, b; [a, b] = [7, 8]; a * b"), 56.0);
    assert_eq!(num("let p, q; ({ p, q } = { p: 2, q: 5 }); p * q"), 10.0);
    // member targets work as leaves
    assert_eq!(num("const o = {}; [o.a, o.b] = [3, 4]; o.a + o.b"), 7.0);
    // an array rest target must be last and takes no trailing comma
    for src in ["0, [...a, b] = []", "0, [...a,] = []"] {
        assert!(
            matches!(err(src), JsError::Syntax { .. }),
            "expected a SyntaxError for {src}"
        );
    }
}

#[test]
fn spread_syntax() {
    assert_eq!(num("const a = [1, 2]; const b = [...a, 3]; b[2]"), 3.0);
    assert_eq!(num("function f(x, y, z) { return x * y * z } f(...[2, 3], 4)"), 24.0);
    assert_eq!(num("const ch = [...'ab']; ch.length"), 2.0);
    assert_eq!(string("Object.keys({ ...{ a: 1 }, b: 2 }).join('')"), "ab");
    assert_eq!(num("const r = [...[0, 1], ...[2]]; r[2]"), 2.0);
    assert!(boolean("[...'ab'] instanceof Array"));
}

// ---------------------------------------------------------------------------
// accessors (class get/set + Object.defineProperty)
// ---------------------------------------------------------------------------

#[test]
fn class_accessors() {
    assert_eq!(
        num(
            "class T { constructor() { this.n = 0 } get d() { return this.n + 1 } set d(v) { this.n = v * 2 } } const t = new T(); t.d = 4; t.d"
        ),
        9.0
    );
}

#[test]
fn computed_class_accessors() {
    // `get [expr]()` / `set [expr]()` and the static form
    assert_eq!(
        num("var k = 'v'; class C { get [k]() { return 7 } } new C().v"),
        7.0
    );
    assert_eq!(
        num("var k = 'v'; class C { set [k](x) { this._x = x } } var c = new C(); c.v = 3; c._x"),
        3.0
    );
    assert_eq!(
        num("var k = 's'; class C { static get [k]() { return 5 } } C.s"),
        5.0
    );
    // duplicate computed accessors are not detected statically (the spec
    // throws at class definition); ours installs in order, so the last wins
    assert_eq!(
        num("var k = 'v'; class C { get [k]() { return 1 } get [k]() { return 2 } } new C().v"),
        2.0
    );
}

#[test]
fn object_define_property() {
    assert_eq!(num("const o = {}; Object.defineProperty(o, 'k', { value: 5 }); o.k"), 5.0);
    assert_eq!(
        num("const o = { a: 1 }; Object.defineProperty(o, 'g', { get: function() { return 42 } }); o.g"),
        42.0
    );
    // set/get pair through defineProperty
    assert_eq!(
        num(
            "const o = {}; Object.defineProperty(o, 'v', { get: function() { return this.s || 0 }, set: function(x) { this.s = x * 2 } }); o.v = 5; o.v"
        ),
        10.0
    );
    // write to a getter-only property is a sloppy-mode no-op
    assert_eq!(
        string(
            "const o = {}; Object.defineProperty(o, 'r', { get: function() { return 'R' } }); o.r = 9; o.r"
        ),
        "R"
    );
    // shortcut: no object-literal getters / `__proto__` or setPrototypeOf
    // wiring in this wave, so inherited accessors are exercised via class
    // accessors on the prototype chain (see class_accessors) instead
}

// ---------------------------------------------------------------------------
// per-turn interpreter step budget (bounded resources: a runaway loop must
// fail honestly instead of pegging the worker forever)
// ---------------------------------------------------------------------------

#[test]
fn for_semi_spin_aborts_with_budget_error() {
    // the exact shape that hung a real page's scanner loop: `for(;;)` whose
    // termination depends on state that (due to a bug) never changes
    let mut e = JsEngine::new();
    e.step_budget = 50_000; // tests run debug: keep the abort quick
    let err = e.exec("for(;;) { 1 + 1 }").err().expect("spin must abort");
    assert!(matches!(err, JsError::Budget { .. }), "got {err:?}");
    // exec resolves the recorded statement offset to line/col
    if let JsError::Budget { line, col } = err {
        assert_eq!(line, 1, "single-line script");
        assert!(col >= 1);
    }
    assert!(err.to_string().contains("step budget"), "display: {err}");
}

#[test]
fn budget_error_points_at_the_spinning_statement() {
    let mut e = JsEngine::new();
    e.step_budget = 50_000;
    match e.exec("let a = 1;\nlet b = 2;\nfor(;;) { a }").err() {
        Some(JsError::Budget { line, col }) => {
            assert_eq!(line, 3, "spinning for is on line 3");
            assert!(col >= 1, "col={col}");
        }
        other => panic!("expected Budget, got {other:?}"),
    }
}

#[test]
fn budget_error_is_not_catchable_by_script() {
    // a page must not be able to swallow the abort and keep spinning:
    // try/catch only absorbs thrown values, so the budget error propagates
    let e_src = "let n = 0; try { for(;;) { n++ } } catch (e) { n = -1 } n";
    let mut e = JsEngine::new();
    e.step_budget = 50_000;
    let r = e.exec(e_src);
    assert!(matches!(r, Err(JsError::Budget { .. })), "got {:?}", r);
    // ...and the engine is usable again next turn (budget resets per turn)
    e.step_budget = DEFAULT_STEP_BUDGET;
    assert_eq!(e.exec("1 + 1").unwrap().to_number(), 2.0);
}

#[test]
fn budget_stops_a_spinning_generator_loop() {
    // cond-less generator loop with an EMPTY body (no yield): the mini-VM
    // re-pushes frames without ever re-entering eval_stmt (an empty Batch
    // finishes without dispatching a statement), so only `drive`'s own tick
    // can bound it. Contrast: `for(;;) yield 1` suspends each iteration and
    // never spins within a turn.
    let mut e = JsEngine::new();
    e.step_budget = 20_000;
    let src = "function* g() { for(;;) {} } const it = g(); it.next()";
    let r = e.exec(src);
    assert!(matches!(r, Err(JsError::Budget { .. })), "got {:?}", r);
}

#[test]
fn budget_spares_normal_workloads() {
    // the default budget must be far above real work: a 1M-iteration loop
    // with a function call per iteration (well into the millions of
    // dispatches) completes untouched
    let src = "function f(x) { return x + 1 } let s = 0; for (let i = 0; i < 200000; i++) { s = f(s) } s";
    assert_eq!(num(src), 200000.0);
}

#[test]
fn budget_is_per_turn_not_leaking_into_later_turns() {
    // turn 1 spins and aborts; turn 2 (fresh exec on the same engine) must
    // get a full budget even though `steps` overflowed it
    let mut e = JsEngine::new();
    e.step_budget = 30_000;
    assert!(matches!(e.exec("for(;;) {}"), Err(JsError::Budget { .. })));
    e.step_budget = DEFAULT_STEP_BUDGET;
    assert_eq!(e.exec("let i = 0; while (i < 1000) { i++ } i").unwrap().to_number(), 1000.0);
}

#[test]
fn import_export_need_a_module_script() {
    // the parser understands both, the interpreter refuses them outside a
    // module (a classic script gets an honest error, not silent success)
    assert!(err("export const a = 1;").to_string().contains("module script"));
    assert!(err("import x from \"./x.js\";").to_string().contains("module script"));
}

#[test]
fn dynamic_import_fails_at_runtime_not_parse_time() {
    // `import()` is not implemented. It must stay a *runtime* failure so the
    // statements around it still run (a parse error would kill the script).
    assert_eq!(
        num("var a = 1; try { import('./x.js') } catch (e) { a = 2 } a"),
        2.0
    );
    assert_eq!(num("var b = 1; try { import('./x.js') } catch (e) {} b + 1"), 2.0);
}

#[test]
fn import_dot_meta_does_not_stop_parsing() {
    // `import.meta` reaches the expression path too (alternative of a runtime
    // TypeError, never a SyntaxError that aborts the whole script)
    assert_eq!(num("var n = 0; try { import.meta.url } catch (e) { n = 3 } n"), 3.0);
}

// -- wave 16: for heads, async/computed/private class members, tagged
// templates ---------------------------------------------------------------

#[test]
fn for_head_declarator_list_without_initializer_on_the_first() {
    // `for (var a, b, c = x; ...)` — the comma used to be a syntax error when
    // the first declarator had no initializer
    assert_eq!(num("var s = 0; for (var a, b, c = 3; c < 5; c++) s += c; s"), 7.0);
    assert_eq!(num("var s = 0; for (var i = 0, j; i < 3; i++) s += i; s"), 3.0);
}

#[test]
fn for_head_comma_expression_init() {
    // non-declaration head: `for (n = f(), i = 0; ...)` is one comma
    // expression for the init slot
    assert_eq!(
        num("var a = 0; var i; for (a = 5, i = 0; i < 2; i++) {} a + i"),
        7.0
    );
}

#[test]
fn for_in_with_a_member_target() {
    assert_eq!(
        num("var o = {}; for (o['k' + 1] in {p: 1}) {} o.k1 === 'p' ? 1 : 0"),
        1.0
    );
    assert_eq!(num("var n; for (n in {a: 1, b: 2}) {} n === 'b' ? 1 : 0"), 1.0);
}

#[test]
fn in_suppression_stops_at_parentheses() {
    // the head grammar suppresses `in`, but a parenthesized sub-expression
    // is a full Expression again
    assert_eq!(
        num("var n = 0; for (var x = ('b' in {a: 1}); n < 1; n++) {} x ? 1 : 0"),
        0.0
    );
    assert_eq!(
        num("var n = 0; for (var y = ('a' in {a: 1}); n < 2; n++) {} y ? 1 : 0"),
        1.0
    );
}

#[test]
fn async_and_generator_class_methods() {
    assert_eq!(
        string("class A { async m() { return 7 } } typeof new A().m().then"),
        "function"
    );
    assert_eq!(
        num("class B { *g() { yield 4; yield 5 } } var it = new B().g(); it.next().value + it.next().value"),
        9.0
    );
    // a field named `async` still works
    assert_eq!(num("class C { async = 3 } new C().async"), 3.0);
}

#[test]
fn computed_class_member_names() {
    assert_eq!(
        num("class A { ['m' + 'x']() { return 8 } } new A().mx()"),
        8.0
    );
    assert_eq!(
        num("var k = 'v'; class B { [k] = 4; static [k + 's'] = 5 } new B().v + B.vs"),
        9.0
    );
}

#[test]
fn private_class_fields_and_methods() {
    assert_eq!(
        num("class A { #v = 2; #m() { return this.#v + 1 } run() { return this.#m() } } new A().run()"),
        3.0
    );
    // private state is not visible under its plain name
    assert_eq!(
        num("class B { #v = 1 } 'v' in new B() ? 1 : 0"),
        0.0
    );
}

#[test]
fn template_hex_escape_is_decoded() {
    // a template used to cut `\xHH` in half and report "bad \x escape"
    assert_eq!(num("`\\xa9`.length"), 1.0);
    assert_eq!(num("`\\u00e9\\n`.length"), 2.0);
    assert_eq!(string("`a\\x41b`"), "aAb");
}

#[test]
fn tagged_template_gets_strings_and_values() {
    assert_eq!(
        string("function t(s, v) { return s.length + '/' + s.raw.length + '/' + s[0] + v + s[1] } t`a${5}b`"),
        "2/2/a5b"
    );
    assert_eq!(
        num("var o = { t: function (s) { return s[0].length } }; o.t`abcd`"),
        4.0
    );
    assert_eq!(
        string("function u(s) { return s[0] }; u`x${1}y`.toUpperCase()"),
        "X"
    );
}

// -- wave 17: URL / URLSearchParams ---------------------------------------

#[test]
fn url_components() {
    assert_eq!(string("new URL('https://example.com/a/b?x=1#f').protocol"), "https:");
    assert_eq!(string("new URL('https://example.com/a/b?x=1#f').hostname"), "example.com");
    assert_eq!(string("new URL('https://example.com/a/b?x=1#f').pathname"), "/a/b");
    assert_eq!(string("new URL('https://example.com/a/b?x=1#f').search"), "?x=1");
    assert_eq!(string("new URL('https://example.com/a/b?x=1#f').hash"), "#f");
    assert_eq!(string("new URL('https://example.com/a/b?x=1#f').origin"), "https://example.com");
    assert_eq!(
        string("new URL('http://example.com:8080/').host"),
        "example.com:8080"
    );
    assert_eq!(string("new URL('http://example.com:80/').port"), "");
}

#[test]
fn url_resolves_against_a_base() {
    assert_eq!(
        string("new URL('../c?q=1', 'https://example.com/a/b').href"),
        "https://example.com/c?q=1"
    );
    assert_eq!(
        string("new URL('/x', 'https://example.com/a/b').pathname"),
        "/x"
    );
    assert_eq!(
        string("new URL('https://example.com/a b').pathname"),
        "/a%20b"
    );
}

#[test]
fn url_rejects_bad_input_and_requires_new() {
    assert!(matches!(
        err("new URL('not a url')"),
        JsError::Throw(_)
    ));
    assert!(matches!(err("URL('https://example.com/')"), JsError::Throw(_)));
    assert!(boolean("new URL('https://example.com/') instanceof URL"));
}

#[test]
fn url_search_params() {
    assert_eq!(
        string("new URL('https://e.com/?a=1&b=2&a=3').searchParams.get('a')"),
        "1"
    );
    assert_eq!(
        num("new URL('https://e.com/?a=1&b=2&a=3').searchParams.getAll('a').length"),
        2.0
    );
    assert!(boolean("new URL('https://e.com/?a=1').searchParams.has('a')"));
    assert!(!boolean("new URL('https://e.com/?a=1').searchParams.has('z')"));
}

#[test]
fn url_search_params_mutate_and_serialize() {
    // `set` deletes the old entries and appends the new one at the end
    // (spec behavior), hence b=2&a=9 rather than a=9&b=2
    assert_eq!(
        string(
            "var p = new URLSearchParams('a=1'); \
             p.append('b', '2'); p.set('a', '9'); p.toString()"
        ),
        "b=2&a=9"
    );
    assert_eq!(
        string("var p = new URLSearchParams('a=1&b=2&a=3'); p.delete('a'); p.toString()"),
        "b=2"
    );
    assert_eq!(
        string("var p = new URLSearchParams('b=2&a=3&a=1'); p.sort(); p.toString()"),
        "a=1&a=3&b=2"
    );
    assert_eq!(
        string("var p = new URLSearchParams('a=1'); p.append('b', 'hello world'); p.toString()"),
        "a=1&b=hello+world"
    );
}

#[test]
fn url_search_params_from_object_and_arrays() {
    assert_eq!(
        string("new URLSearchParams({a: 1, b: 'x y'}).toString()"),
        "a=1&b=x+y"
    );
    assert_eq!(
        string("var p = new URLSearchParams(''); p.append('k', 'v'); p.get('k')"),
        "v"
    );
    assert_eq!(
        string("var p = new URLSearchParams('a=1&b=2'); var out = []; p.forEach(function (v, k) { out.push(k + v) }); out.join(',')"),
        "a1,b2"
    );
    assert_eq!(
        string("new URLSearchParams('a=1&b=2').entries().map(function (e) { return e[0] }).join('')"),
        "ab"
    );
}

// -- wave 17: Object.prototype inheritance --------------------------------

#[test]
fn object_prototype_is_reachable_and_inherited() {
    // borrowing the method off the prototype is how most libraries test keys
    assert!(boolean(
        "Object.prototype.hasOwnProperty.call({a: 1}, 'a')"
    ));
    assert!(!boolean(
        "Object.prototype.hasOwnProperty.call({a: 1}, 'b')"
    ));
    // plain objects inherit it lazily as well
    assert!(boolean("({a: 1}).hasOwnProperty('a')"));
    assert!(boolean("({}).hasOwnProperty('x') === false"));
    // arrays inherit Object.prototype through Array.prototype
    assert!(boolean("[1, 2].hasOwnProperty(0)"));
    assert!(boolean("Object.getPrototypeOf(Object.prototype) === null"));
}

#[test]
fn object_create_and_property_names() {
    assert_eq!(
        num("var p = {greet: function () { return 1 }}; var o = Object.create(p); o.greet()"),
        1.0
    );
    // shortcut: a null prototype is stored as "not attached yet" (see
    // Object.create) — a dictionary still works for its documented use
    assert_eq!(
        num("var d = Object.create(null); d.k = 1; d.k + Object.keys(d).length"),
        2.0
    );
    assert_eq!(
        string("Object.getOwnPropertyNames({a: 1, b: 2}).join(',')"),
        "a,b"
    );
    assert_eq!(num("Object.getOwnPropertyNames([7, 8]).length"), 2.0);
}

// -- wave 17: runtime error locations --------------------------------------

#[test]
fn escaping_error_names_the_source_that_threw() {
    let mut e = JsEngine::new();
    // the thrower lives in its own script; the member read is on line 2 of
    // that script, not in whoever calls it
    e.exec("var boom = function () {\n    return null.length;\n};")
        .unwrap();
    let msg = format!("{}", e.exec("\n\n\n\n\nboom();").unwrap_err());
    assert!(msg.contains("line 2"), "{msg}");
    assert!(msg.contains("col"), "{msg}");
}

#[test]
fn caught_errors_keep_their_value() {
    // the location is appended to the log line only: a caught value must be
    // exactly what `throw` passed
    assert_eq!(num("var v; try { throw 'boom' } catch (e) { v = e } v.length"), 4.0);
    assert_eq!(string("var v; try { throw 'boom' } catch (e) { v = e } v"), "boom");
}

#[test]
fn a_caught_error_does_not_block_the_next_location() {
    let mut e = JsEngine::new();
    e.exec("try { throw 'first' } catch (x) { }").unwrap();
    let msg = format!("{}", e.exec("\nnull.x").unwrap_err());
    assert!(msg.contains("line 2"), "{msg}");
}

#[test]
fn syntax_errors_report_their_own_position() {
    match err("var a = 1;\nvar b = ;") {
        JsError::Syntax { line, .. } => assert_eq!(line, 2),
        other => panic!("expected SyntaxError, got {other}"),
    }
}

#[test]
fn url_accepts_a_url_instance_as_base() {
    assert_eq!(
        string("new URL('.', new URL('https://example.com/a/b')).href"),
        "https://example.com/a/"
    );
    assert_eq!(
        string("new URL('c', new URL('https://example.com/a/b')).pathname"),
        "/a/c"
    );
    assert_eq!(string("new URL('https://example.com/x').toString()"), "https://example.com/x");
}

// -- wave 21: error family, function/promise prototypes, array/string nits --

#[test]
fn error_objects_have_names_prototypes_and_stack() {
    assert_eq!(string("new Error('boom').name"), "Error");
    assert_eq!(string("new TypeError('bad').name"), "TypeError");
    assert_eq!(string("new Error('boom').message"), "boom");
    assert_eq!(string("new Error('boom').toString()"), "Error: boom");
    assert_eq!(string("new Error().toString()"), "Error");
    assert!(boolean("new TypeError('x') instanceof Error"));
    assert!(boolean("new RangeError('x') instanceof Error"));
    assert!(boolean("new Error('x') instanceof Object"));
    assert!(boolean("Object.getPrototypeOf(TypeError.prototype) === Error.prototype"));
    // shortcut: no captured frames - `stack` is the head line only
    assert!(boolean("typeof new Error('x').stack === 'string'"));
    // borrowed by libraries: reads this.name / this.message, not the slot
    assert_eq!(
        string("Error.prototype.toString.call({ name: 'X', message: 'y' })"),
        "X: y"
    );
    assert_eq!(string("Error.prototype.toString.call({})"), "Error");
    // an error logged by the engine keeps its own message
    assert!(err("throw new TypeError('bad')")
        .to_string()
        .contains("TypeError: bad"));
    // engine-raised errors are caught as real Error objects, so `instanceof`
    // and `e.constructor` let code tell error classes apart
    assert!(boolean(
        "var r = false; try { null.x } catch (e) { r = e instanceof TypeError } r"
    ));
    assert_eq!(
        string("var n; try { null.x } catch (e) { n = e.name } n"),
        "TypeError"
    );
    assert!(boolean(
        "var r = false; try { null.x } catch (e) { r = e.constructor === TypeError } r"
    ));
    assert!(boolean(
        "var r = false; try { null.x } catch (e) { r = e.message.length > 0 } r"
    ));
    assert!(boolean("new ReferenceError('r') instanceof Error"));
    assert!(boolean("new EvalError('e') instanceof Error"));
    assert!(boolean("new URIError('u') instanceof Error"));
    assert!(boolean("new ReferenceError('r').constructor === ReferenceError"));
}

#[test]
fn subclasses_of_error_stay_errors() {
    assert!(boolean("class MyError extends Error {}; new MyError('m') instanceof Error"));
    assert_eq!(
        string("class MyError extends Error {}; new MyError('m').message"),
        "m"
    );
    assert_eq!(
        string("class MyError extends Error {}; new MyError('m').toString()"),
        "Error: m"
    );
}

#[test]
fn promise_prototype_is_an_object_with_borrowable_methods() {
    // core-js-style bundles do `pm = Promise.prototype` then define `then`:
    // the object must exist and expose the methods
    assert!(boolean("typeof Promise.prototype === 'object'"));
    assert!(boolean("typeof Promise.prototype.then === 'function'"));
    assert!(boolean("typeof Promise.prototype.catch === 'function'"));
    assert!(boolean("typeof Promise.prototype.finally === 'function'"));
    assert!(boolean(
        "Object.getPrototypeOf(Promise.resolve()) === Promise.prototype"
    ));
    // borrowed through the prototype object
    assert_eq!(
        mnum(
            "let m = 0; let pm = Promise.prototype; \
             pm.then.call(Promise.resolve(5), function (v) { m = v * 2 })"
        ),
        10.0
    );
}

#[test]
fn string_char_code_at() {
    assert_eq!(num("'A'.charCodeAt(0)"), 65.0);
    assert_eq!(num("'abc'.charCodeAt(2)"), 99.0);
    // EUR sign U+20AC
    assert_eq!(num("'\\u20ac'.charCodeAt(0)"), 8364.0);
    assert!(num("'abc'.charCodeAt(9)").is_nan());
}

#[test]
fn array_splice_removes_and_inserts() {
    assert_eq!(
        string("var a = [1, 2, 3, 4]; a.splice(1, 2).join(',') + '|' + a.join(',')"),
        "2,3|1,4"
    );
    assert_eq!(
        string("var a = [1, 2, 3]; a.splice(1, 0, 'x', 'y'); a.join(',')"),
        "1,x,y,2,3"
    );
    assert_eq!(string("var a = [1, 2, 3]; a.splice(-1, 1); a.join(',')"), "1,2");
    assert_eq!(num("var a = [1, 2, 3]; a.splice(1); a.length"), 1.0);
}

#[test]
fn function_to_string_reports_a_function() {
    assert_eq!(
        string("(function foo() {}).toString()"),
        "function foo() { [code] }"
    );
    assert_eq!(
        string("(function () {}).toString()"),
        "function () { [code] }"
    );
    // natives advertise themselves as native, as feature probes expect
    assert!(boolean("String(Math.max).indexOf('native code') > 0"));
}

#[test]
fn caught_errors_do_not_shift_later_error_locations() {
    // a caught throw leaves `err_located` set; the next statement's throw
    // must still resolve against its own line (MDN's length-of-undefined
    // reports came from errors labelled with an earlier position)
    let mut e = JsEngine::new();
    let err = e
        .exec("try { throw new Error('caught') } catch (e) {}\nvar a = 1;\nnull.foo;")
        .err()
        .expect("script must throw");
    let s = err.to_string();
    assert!(s.contains("line 3"), "expected line 3, got: {s}");
}

#[test]
fn module_body_error_locations_stay_per_statement() {
    let mut e = JsEngine::new();
    let err = e
        .load_module_source(
            "https://example.test/m.js",
            "try { throw new Error('first') } catch (e) {}\nnull.foo;",
        )
        .expect_err("module must throw");
    assert!(err.contains("line 2"), "expected line 2, got: {err}");
}

#[test]
fn function_constructor_compiles_its_body() {
    assert!(matches!(
        run("new Function('return true;')()"),
        Val::Bool(true)
    ));
    assert_eq!(num("Function('a', 'b', 'return a + b;')(2, 3)"), 5.0);
    // every argument but the last is a parameter
    assert_eq!(num("new Function('x', 'return x * 2')(21)"), 42.0);
    // spec: the compiled function closes over the GLOBAL scope only
    assert_eq!(
        run("(function () { let hidden = 1; return new Function('return typeof hidden')(); })()")
            .to_string(),
        "undefined"
    );
    // ... and a plain call gets the global object as `this`
    assert!(matches!(run("Function('return this')()"), Val::Obj(_)));
    // the spec name for a constructor-built function
    assert_eq!(run("new Function('return 1').name").to_string(), "anonymous");
}

#[test]
fn function_constructor_syntax_error_is_throwable() {
    let caught = run(
        "(function () { try { new Function('return )'); return false } \
         catch (e) { return String(e).startsWith('SyntaxError:') } })()",
    );
    assert!(
        matches!(caught, Val::Bool(true)),
        "caught: {caught:?} / {}",
        caught.to_string()
    );
}

#[test]
fn property_keys_are_interned() {
    // `a.shared` and `b.shared` must point at ONE key allocation: a React
    // render inserts the same handful of names into thousands of objects.
    fn key_of(e: &mut JsEngine, src: &str) -> std::rc::Rc<str> {
        match e.exec(src) {
            Ok(Val::Obj(o)) => o
                .borrow()
                .props
                .iter()
                .find(|(k, _)| k.as_ref() == "shared")
                .map(|(k, _)| k.clone())
                .unwrap_or_else(|| panic!("{src} has no shared prop")),
            other => panic!("{src}: {other:?}"),
        }
    }
    let mut e = JsEngine::new();
    e.exec("var a = {}; var b = {}; a.shared = 1; b.shared = 2;")
        .expect("setup");
    let ka = key_of(&mut e, "a");
    let kb = key_of(&mut e, "b");
    assert!(
        std::rc::Rc::ptr_eq(&ka, &kb),
        "property names should share one Rc"
    );
    match e.exec("a.shared + b.shared") {
        Ok(v) => assert_eq!(v.to_string(), "3"),
        other => panic!("sum: {other:?}"),
    }
}

#[test]
fn queue_microtask_is_fifo_with_promise_reactions() {
    let mut e = JsEngine::new();
    e.exec(
        "var order = [];\n\
         queueMicrotask(function () { order.push('a'); });\n\
         Promise.resolve().then(function () { order.push('b'); });\n\
         queueMicrotask(function () { order.push('c'); });",
    )
    .expect("queue microtasks");
    // the turn-end drain runs the queued callbacks and promise reactions in
    // FIFO order; nested queueing appends behind the existing jobs
    match e.exec("order.join(',')") {
        Ok(v) => assert_eq!(v.to_string(), "a,b,c"),
        other => panic!("order: {other:?}"),
    }
}

#[test]
fn queue_microtask_rejects_non_callables() {
    let mut e = JsEngine::new();
    let err = e
        .exec("queueMicrotask(1)")
        .err()
        .expect("non-callable must throw");
    assert!(
        err.to_string().contains("queueMicrotask callback is not a function"),
        "err: {err}"
    );
}

#[test]
fn not_a_function_names_the_call_site() {
    // a minified bundle's "undefined is not a function" names nothing; the
    // callee offset is the only location the engine can report
    let mut e = JsEngine::new();
    let err = e
        .exec("var o = {};\nvar x = o.missing();")
        .err()
        .expect("calling undefined must throw");
    let msg = err.to_string();
    assert!(msg.contains("is not a function"), "err: {msg}");
    assert!(msg.contains("(at 2:"), "no call site in: {msg}");
}

#[test]
fn data_view_reads_and_writes_with_endianness() {
    // default endianness is big-endian; the littleEndian flag is per call
    assert_eq!(
        num(
            "var b = new ArrayBuffer(16); var dv = new DataView(b);\n\
             dv.setUint16(0, 0x1234); dv.getUint16(0)"
        ),
        0x1234 as f64
    );
    assert_eq!(
        num(
            "var b = new ArrayBuffer(16); var dv = new DataView(b);\n\
             dv.setUint16(0, 0x1234); dv.getUint16(0, true)"
        ),
        0x3412 as f64
    );
    assert_eq!(
        num(
            "var b = new ArrayBuffer(16); var dv = new DataView(b);\n\
             dv.setInt32(2, -2); dv.getInt32(2)"
        ),
        -2.0
    );
    assert_eq!(
        num(
            "var b = new ArrayBuffer(16); var dv = new DataView(b);\n\
             dv.setFloat64(4, 1.5); dv.getFloat64(4)"
        ),
        1.5
    );
    assert_eq!(
        num(
            "var b = new ArrayBuffer(16); var dv = new DataView(b);\n\
             dv.setFloat32(8, 1.5); dv.getFloat32(8)"
        ),
        1.5
    );
    // byte order is visible through a typed-array view of the same buffer
    assert_eq!(
        num(
            "var b = new ArrayBuffer(4); var dv = new DataView(b);\n\
             dv.setUint16(0, 0x1234); new Uint8Array(b)[0]"
        ),
        0x12 as f64
    );
    assert_eq!(
        num(
            "var b = new ArrayBuffer(4); var dv = new DataView(b);\n\
             dv.setUint16(0, 0x1234, true); new Uint8Array(b)[0]"
        ),
        0x34 as f64
    );
}

#[test]
fn data_view_window_and_bounds_are_checked() {
    assert_eq!(
        num(
            "var b = new ArrayBuffer(8); var dv = new DataView(b, 4, 2);\n\
             dv.byteOffset + dv.byteLength"
        ),
        6.0
    );
    assert_eq!(
        num(
            "var dv = new DataView(new ArrayBuffer(8), 4, 2);\n\
             dv.setUint8(1, 7); dv.getUint8(1)"
        ),
        7.0
    );
    // the view window is enforced, not the backing buffer
    assert!(err("var dv = new DataView(new ArrayBuffer(8), 4, 2); dv.getUint16(1)")
        .to_string()
        .contains("RangeError"));
    assert!(err("new DataView(new ArrayBuffer(8), 9)")
        .to_string()
        .contains("RangeError"));
    assert!(err("new DataView({})").to_string().contains("TypeError"));
    // DataView is an ArrayBuffer view
    assert!(boolean("ArrayBuffer.isView(new DataView(new ArrayBuffer(4)))"));
}

#[test]
fn data_view_bigint_accessors() {
    assert!(boolean(
        "var dv = new DataView(new ArrayBuffer(8));\n\
         dv.setBigUint64(0, 18446744073709551615n);\n\
         dv.getBigInt64(0) === -1n"
    ));
    assert!(boolean(
        "var dv = new DataView(new ArrayBuffer(8));\n\
         dv.setBigInt64(0, -2n); dv.getBigUint64(0) === 18446744073709551614n"
    ));
    assert!(err("var dv = new DataView(new ArrayBuffer(8)); dv.setBigInt64(0, 1)")
        .to_string()
        .contains("TypeError"));
}

#[test]
fn abort_controller_fires_listeners_with_reason() {
    assert_eq!(
        string(
            "var c = new AbortController(); var hits = [];\n\
             var before = c.signal.aborted;\n\
             c.signal.addEventListener('abort', function (e) { hits.push('l:' + e.type); });\n\
             c.signal.onabort = function (e) { hits.push('on:' + e.type); };\n\
             c.abort();\n\
             before + '|' + c.signal.aborted + '|' + hits.join(',') + '|' + c.signal.reason.name"
        ),
        "false|true|l:abort,on:abort|AbortError"
    );
    assert_eq!(
        string(
            "var c = new AbortController(); var why = { code: 7 };\n\
             c.abort(why); c.signal.reason.code"
        ),
        "7"
    );
    assert_eq!(
        string("var c = new AbortController(); typeof c.signal.throwIfAborted"),
        "function"
    );
    assert_eq!(
        string(
            "var c = new AbortController(); c.abort(1);\n\
             var out = 'none';\n\
             try { c.signal.throwIfAborted(); } catch (e) { out = 'caught:' + e; }\n\
             out"
        ),
        "caught:1"
    );
    assert_eq!(
        string(
            "var c = new AbortController(); var why = { code: 7 };\n\
             c.abort(why);\n\
             var out = 'none';\n\
             try { c.signal.throwIfAborted(); } catch (e) { out = e.code; }\n\
             out"
        ),
        "7"
    );
    // abort is idempotent: listeners fire once
    assert_eq!(
        num(
            "var c = new AbortController(); var n = 0;\n\
             c.signal.addEventListener('abort', function () { n++; });\n\
             c.abort(); c.abort(1); n"
        ),
        1.0
    );
}

#[test]
fn abort_signal_static_and_listener_removal() {
    assert!(boolean("AbortSignal.abort().aborted"));
    assert_eq!(string("AbortSignal.abort().reason.name"), "AbortError");
    assert_eq!(
        num(
            "var c = new AbortController(); var n = 0;\n\
             function f() { n++; }\n\
             c.signal.addEventListener('abort', f);\n\
             c.signal.removeEventListener('abort', f);\n\
             c.abort(); n"
        ),
        0.0
    );
    assert!(err("new AbortSignal()").to_string().contains("TypeError"));
    // instanceof works through the registered prototypes
    assert!(boolean("new AbortController() instanceof AbortController"));
    assert!(boolean("new AbortController().signal instanceof AbortSignal"));
}

#[test]
fn js_allocation_units_stay_compact() {
    // RAM guards for waves 63-64: rare `ObjKind` payloads and statement
    // expressions live behind pointers, and `Param`'s optional
    // pattern/default are boxed, so ordinary objects, parameters, statements
    // and closures stay small. Raise these ceilings only with a measured
    // reason.
    assert!(
        std::mem::size_of::<ObjKind>() <= 32,
        "ObjKind grew to {} bytes",
        std::mem::size_of::<ObjKind>()
    );
    assert!(
        std::mem::size_of::<Obj>() <= 64,
        "Obj grew to {} bytes",
        std::mem::size_of::<Obj>()
    );
    assert!(
        std::mem::size_of::<parser::Param>() <= 40,
        "Param grew to {} bytes",
        std::mem::size_of::<parser::Param>()
    );
    assert!(
        std::mem::size_of::<parser::Stmt>() <= 72,
        "Stmt grew to {} bytes",
        std::mem::size_of::<parser::Stmt>()
    );
    assert!(
        std::mem::size_of::<Closure>() <= 96,
        "Closure grew to {} bytes",
        std::mem::size_of::<Closure>()
    );
}

// ---------------------------------------------------------------------------
// observed-miss census (MAZDA_MISS)
// ---------------------------------------------------------------------------
//
// These drive the pure local-state helpers (`miss_bucket`, `record_js_miss`,
// `report_js_bucket`) rather than the process-wide census, so parallel tests
// cannot race on the shared mutex or depend on the env gate.

#[test]
fn miss_bucket_classifies_host_shapes() {
    let e = JsEngine::new();
    // the global object itself: a read that resolved nowhere is a missing global
    assert_eq!(miss_bucket(&e, &e.global_obj), Some("global"));
    // a plain script object's undefined read is normal JS, not a gap
    let plain = Val::new_object();
    let Val::Obj(p) = &plain else {
        panic!("new_object is an object")
    };
    assert_eq!(miss_bucket(&e, p), None);
    // element wrapper: `__node` marker + method natives
    let el = Val::new_object();
    el.set("__node", Val::Num(1.0));
    el.set("click", Val::Native(0));
    let Val::Obj(w) = &el else {
        panic!("new_object is an object")
    };
    assert_eq!(miss_bucket(&e, w), Some("member"));
    // singleton wrapper (document/navigator/...): built from natives alone
    let solo = Val::new_object();
    solo.set("getElementById", Val::Native(0));
    let Val::Obj(s) = &solo else {
        panic!("new_object is an object")
    };
    assert_eq!(miss_bucket(&e, s), Some("member"));
    // live collection kind answers through the embedder every time
    let live = Rc::new(RefCell::new(Obj {
        kind: ObjKind::Live(LiveSpec {
            family: 0,
            root: 0,
            arg: "".into(),
        }),
        props: Vec::new(),
        proto: None,
    }));
    assert_eq!(miss_bucket(&e, &live), Some("member"));
}

#[test]
fn js_miss_report_ranks_caps_and_excludes() {
    // an empty bucket with nothing over the cap prints no section
    let empty = std::collections::HashMap::new();
    assert_eq!(report_js_bucket("global", &empty, 0), "");
    // rank by frequency then name; the two exclusions never record
    let mut m = JsMisses::default();
    record_js_miss(&mut m, "global", "b");
    record_js_miss(&mut m, "global", "a");
    record_js_miss(&mut m, "global", "a");
    record_js_miss(&mut m, "global", "undefined");
    record_js_miss(&mut m, "global", "#new.target");
    record_js_miss(&mut m, "member", "scrollTop");
    let report = report_js_bucket("global", &m.global, m.over_global);
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines[0], "[js-miss] 2 distinct missing global names");
    assert_eq!(lines[1], "[js-miss] global 2 a");
    assert_eq!(lines[2], "[js-miss] global 1 b");
    assert!(!report.contains("undefined"));
    assert!(!report.contains("#new.target"));
    // the member bucket is its own section
    let mreport = report_js_bucket("member", &m.member, m.over_member);
    assert!(mreport.contains("[js-miss] member 1 scrollTop"));
    // past the cap: seen names keep counting, new names tally the over-counter
    let mut big = JsMisses::default();
    for i in 0..JS_MISS_CAP {
        record_js_miss(&mut big, "member", &format!("k{i}"));
    }
    record_js_miss(&mut big, "member", "k0");
    record_js_miss(&mut big, "member", "newname");
    assert_eq!(big.member.len(), JS_MISS_CAP);
    assert_eq!(big.member["k0"], 2);
    assert_eq!(big.over_member, 1);
    let cap_report = report_js_bucket("member", &big.member, big.over_member);
    assert!(cap_report.contains(&format!(
        "[js-miss] member: 1 further distinct names over the {JS_MISS_CAP} cap not recorded"
    )));
}

// ---------------------------------------------------------------------------
// Reflect
// ---------------------------------------------------------------------------

#[test]
fn reflect_apply_and_construct_run_calls() {
    assert_eq!(string("typeof Reflect"), "object");
    assert_eq!(
        num("function f(a) { return this.n + a } Reflect.apply(f, { n: 2 }, [3])"),
        5.0
    );
    // construct builds the instance from `target.prototype` and returns it
    assert_eq!(
        num("class A { constructor(x) { this.x = x } } Reflect.construct(A, [7]).x"),
        7.0
    );
    assert!(boolean(
        "class B { constructor() { this.b = 1 } } Reflect.construct(B, []) instanceof B"
    ));
    // an object returned from the constructor wins, exactly like `new`
    assert!(boolean(
        "function C() { return { hot: true } } Reflect.construct(C, []).hot"
    ));
    err("Reflect.apply(1, null, [])");
    err("Reflect.apply(function () {}, null, 1)");
    err("Reflect.construct(1, [])");
}

#[test]
fn reflect_object_operations_match_the_operators() {
    assert_eq!(num("var o = { a: 1 }; Reflect.get(o, 'a')"), 1.0);
    assert!(boolean("var o = { a: 1 }; Reflect.set(o, 'a', 2) && o.a === 2"));
    assert!(boolean("Reflect.has({ x: 1 }, 'x')"));
    assert!(
        boolean("Reflect.has([], 'push')"),
        "inherited methods are members"
    );
    assert!(boolean("'x' in { x: 1 }"), "the operator shares the helper");
    assert!(!boolean("Reflect.has({ x: 1 }, 'y')"));
    assert!(boolean(
        "var o = { a: 1 }; Reflect.deleteProperty(o, 'a') && !('a' in o)"
    ));
    assert_eq!(
        num("Reflect.getOwnPropertyDescriptor({ x: 1 }, 'x').value"),
        1.0
    );
    assert_eq!(
        string("JSON.stringify(Reflect.ownKeys({ a: 1, b: 2 }).sort())"),
        "[\"a\",\"b\"]"
    );
    assert!(boolean("Reflect.getPrototypeOf([]) === Array.prototype"));
    assert!(boolean(
        "var p = {}; Reflect.setPrototypeOf(p, Array.prototype); \
         Reflect.getPrototypeOf(p) === Array.prototype"
    ));
    assert!(boolean("Reflect.isExtensible({})"));
    // shortcut: no integrity levels — preventExtensions reports success only
    assert!(boolean(
        "var e = {}; Reflect.preventExtensions(e) && Reflect.isExtensible(e)"
    ));
    assert!(boolean(
        "var t = {}; Reflect.defineProperty(t, 'n', { value: 5 }) && t.n === 5"
    ));
    // both methods throw on a non-object target (unlike the lenient Object.*)
    err("Reflect.get(null, 'x')");
    err("Reflect.has(1, 'x')");
}

// ---------------------------------------------------------------------------
// Array extras
// ---------------------------------------------------------------------------

#[test]
fn array_find_and_find_last_scan_both_directions() {
    assert_eq!(num("[1, 2, 3, 4].find(function (x) { return x > 2 })"), 3.0);
    assert_eq!(
        num("[1, 2, 3, 4].findIndex(function (x) { return x > 2 })"),
        2.0
    );
    // the index callback arg is the real one, including on a reverse scan
    assert_eq!(
        num("[1, 2, 3, 4].findLastIndex(function (x, i) { return i < 3 })"),
        2.0
    );
    assert_eq!(
        num("[1, 2, 3, 4].findLast(function (x) { return x < 4 })"),
        3.0
    );
    assert!(boolean(
        "[1, 2].find(function () { return false }) === undefined"
    ));
    assert_eq!(
        num("[1, 2].findIndex(function () { return false })"),
        -1.0
    );
    assert_eq!(
        num("[1, 2].findLastIndex(function () { return false })"),
        -1.0
    );
}

#[test]
fn array_at_indexes_from_either_end() {
    assert_eq!(string("['a', 'b', 'c'].at(0)"), "a");
    assert_eq!(string("['a', 'b', 'c'].at(-1)"), "c");
    assert_eq!(string("['a', 'b', 'c'].at(-3)"), "a");
    assert!(boolean("['a', 'b', 'c'].at(3) === undefined"));
    assert!(boolean("['a', 'b', 'c'].at(-4) === undefined"));
}

#[test]
fn array_fill_and_copy_within_resolve_relative_bounds() {
    assert_eq!(string("[1, 2, 3].fill(0).join(',')"), "0,0,0");
    assert_eq!(string("[1, 2, 3, 4].fill(9, 1, 3).join(',')"), "1,9,9,4");
    assert_eq!(string("[1, 2, 3, 4].fill(9, -2).join(',')"), "1,2,9,9");
    // fill returns the same array, not a copy
    assert!(boolean("var a = [1]; a.fill(2) === a"));
    // copyWithin shifts the source range, overlap-safe
    assert_eq!(
        string("[1, 2, 3, 4, 5].copyWithin(0, 3).join(',')"),
        "4,5,3,4,5"
    );
    assert_eq!(
        string("[1, 2, 3, 4, 5].copyWithin(1, 0, 3).join(',')"),
        "1,1,2,3,5"
    );
    assert!(boolean("var a = [1, 2]; a.copyWithin(0, 1) === a"));
}

#[test]
fn array_flat_and_flat_map_respect_depth() {
    assert_eq!(
        string("[1, [2, [3, [4]]]].flat().join(',')"),
        "1,2,3,4"
    );
    assert_eq!(
        string("[1, [2, [3, [4]]]].flat(1).join(',')"),
        "1,2,3,4"
    );
    // depth 1 leaves the deeper array in place; join flattens it textually
    assert_eq!(num("[1, [2, [3]]].flat(1)[1]"), 2.0);
    assert!(boolean("[1, [2, [3]]].flat(1)[2] instanceof Array"));
    assert!(boolean("[1, [2, [3]]].flat(2)[2] === 3"));
    assert!(boolean("[1, [2]].flat(0)[1] instanceof Array"));
    assert_eq!(string("[1, 2].flatMap(function (x) { return [x, x] }).join(',')"), "1,1,2,2");
    // a non-array return is kept as one element, not spread
    assert_eq!(
        string("[1, 2].flatMap(function (x) { return x * 2 }).join(',')"),
        "2,4"
    );
    assert_eq!(num("[].flat().length"), 0.0);
}

#[test]
fn array_iterator_methods() {
    // `values`/`keys`/`entries` exist on Array.prototype and `[Symbol.iterator]`
    // is the same function as `values`.
    assert_eq!(string("typeof [][Symbol.iterator]"), "function");
    assert_eq!(string("Array.prototype[Symbol.iterator] === Array.prototype.values"), "true");
    assert_eq!(num("[7, 8].values().next().value"), 7.0);
    assert_eq!(num("[7, 8].keys().next().value"), 0.0);
    assert_eq!(num("[7, 8].entries().next().value[1]"), 7.0);
    // for-of / spread / destructuring go through the same iterator
    assert_eq!(num("var s = 0; for (const x of [1, 2, 3]) s += x; s"), 6.0);
    assert_eq!(num("var [a, b] = [4, 5]; a + b"), 9.0);
}

#[test]
fn deleting_array_iterator_makes_arrays_non_iterable() {
    assert!(matches!(
        err("delete Array.prototype[Symbol.iterator]; [...[1, 2]]"),
        JsError::Throw(_)
    ));
    // a replaced iterator is honoured too
    assert_eq!(
        num("Array.prototype[Symbol.iterator] = function () { return { i: 0, next() { return this.i++ < 1 ? { value: 42, done: false } : { done: true } } } }; [...[1, 2]][0]"),
        42.0
    );
}
#[test]
fn var_without_initializer_does_not_reset_the_binding() {
    // minified TypeScript enums are set up as
    //   (eM = eW || (eW = {}))[eM.initial = 0] = "initial", …; var … eW …;
    // the trailing `var eW;` must be a no-op, not a reset (GitHub's Turbo
    // bundle reads its stage table back later and got `undefined`)
    assert_eq!(
        run("var eW;\n(eM = eW || (eW = {}))[eM.initial = 0] = 'initial';\nvar eW;\neW.initial + ':' + eW[0]")
            .to_string(),
        "0:initial"
    );
    // the same rule keeps a parameter's value: `var x;` is not an assignment
    assert_eq!(num("function f(x) { var x; return x; } f(5)"), 5.0);
    // a leading `var` still declares the name (undefined until assigned)
    assert_eq!(run("var y; y").to_string(), "undefined");
}

#[test]
fn defined_properties_are_non_enumerable_by_default() {
    // color-convert hides `channels`/`labels` with
    //   delete space.channels; Object.defineProperty(space, "channels", { value })
    // and then walks `Object.keys(space)`; a visible property made the page
    // treat "channels" as another color space and read `.distance` off
    // undefined.
    assert_eq!(
        num("var o = {}; Object.defineProperty(o, 'x', { value: 1 }); Object.keys(o).length"),
        0.0
    );
    assert_eq!(
        num("var o = {}; Object.defineProperty(o, 'x', { value: 1, enumerable: true }); Object.keys(o).length"),
        1.0
    );
    // the value is still readable, and for-in skips it
    assert_eq!(
        num("var o = {}; Object.defineProperty(o, 'x', { value: 7 }); o.x"),
        7.0
    );
    assert_eq!(
        run("var o = { a: 1 }; Object.defineProperty(o, 'x', { value: 2 }); var s = ''; for (var k in o) s += k; s")
            .to_string(),
        "a"
    );
}

#[test]
fn a_destructured_parameter_binds_in_the_call_frame() {
    // minified code names destructured parameters like its own imports:
    //   constructor({ size: e, min: t, ttl: n, onDelete: r })
    // binding `n` through the scope chain overwrote the module's webpack
    // `require` (called `n`) with undefined, and the next `n("MC")` died
    assert_eq!(
        num("var n = function () { return 7; }; (function ({ ttl: n }) {}) ({}); n()"),
        7.0
    );
    // the binding is the parameter's, inside the function
    assert_eq!(
        num("var x = 1; function f({ x }) { return x; } var r = f({ x: 5 }); r + (x - 1)"),
        5.0
    );
    // defaults see earlier parameters
    assert_eq!(
        num("function f(a, { b = a + 1 } = {}) { return b; } f(4)"),
        5.0
    );
}

#[test]
fn a_symbol_key_works_with_the_in_operator() {
    // @bufbuild/protobuf brands its reflection wrappers with
    // `Symbol.for("reflect unsafe local") in value`; the key was lost, so its
    // list `add()` stored the wrapper instead of the wrapped message and
    // GitHub's file descriptor held a Type where a DescriptorProto belonged.
    assert!(boolean("var s = Symbol.for('q'); var o = {}; o[s] = 1; s in o"));
    assert!(boolean(
        "var s = Symbol.for('q'); var o = {}; o[s] = 1; Object.prototype.hasOwnProperty.call(o, s)"
    ));
    // the key is the same one a computed member writes and reads
    assert_eq!(
        string("var s = Symbol.for('q'); var o = {}; o[s] = 'v'; o[s]"),
        "v"
    );
    // a non-symbol object key still coerces through ToPrimitive
    assert!(boolean("var o = {}; o.a = 1; ({ toString: function () { return 'a'; } }) in o"));
}

#[test]
fn null_equality_does_not_coerce_the_other_side() {
    // React's `__COMPILER_RUNTIME` is a null-prototype object, and its
    // memo-cache helper guards with `null == r`. A loose comparison must not
    // run ToPrimitive on the object (it has no valueOf/toString and threw
    // "Cannot convert object to primitive value").
    assert!(boolean("null == undefined"));
    assert!(boolean("undefined == null"));
    assert!(boolean("null == null"));
    // everything else is false without touching the object
    assert!(boolean("!(null == Object.create(null))"));
    assert!(boolean("!(Object.create(null) == null)"));
    assert!(boolean("!(null == 0)"));
    assert!(boolean("!(null == '')"));
    assert!(boolean("null != Object.create(null)"));
    // an object with side-effecting coercions is never asked
    assert!(boolean(
        "var asked = false; var o = { valueOf: function () { asked = true; return 1; } }; null == o; !asked"
    ));
    // ordinary loose equality still coerces
    assert!(boolean("1 == '1'"));
    assert!(boolean("0 == false"));
}

#[test]
fn proxy_get_trap_overrides_and_forwards_to_the_target() {
    // Turbo wraps its router in `new Proxy(router, { get: … "navigate" … })`;
    // `Proxy` was missing and the landing bundle died with
    // "ReferenceError: Proxy is not defined".
    assert_eq!(
        string("var t = { a: 1 }; var p = new Proxy(t, { get: function (o, k) { return k === 'navigate' ? 'soft' : o[k]; } }); p.navigate + ':' + p.a"),
        "soft:1"
    );
    // a method read through the proxy keeps the proxy as `this`
    assert_eq!(
        string("var t = { a: 7, m: function () { return this.a; } }; var p = new Proxy(t, { get: function (o, k) { return o[k]; } }); String(p.m())"),
        "7"
    );
    // writes forward without a set trap
    assert_eq!(
        num("var t = { a: 1 }; var p = new Proxy(t, {}); p.a = 4; t.a"),
        4.0
    );
    // `in`, delete and the prototype walk forward to the target
    assert!(boolean("var t = { a: 1 }; var p = new Proxy(t, {}); 'a' in p"));
    assert!(boolean(
        "var t = { a: 1 }; var p = new Proxy(t, {}); delete p.a; !('a' in t)"
    ));
    assert!(boolean(
        "function C() {} var p = new Proxy(new C(), {}); p instanceof C"
    ));
    // a set trap runs instead of forwarding
    assert!(boolean("var got = 0; var p = new Proxy({}, { set: function (o, k, v) { got = v; return true; } }); p.x = 5; got === 5"));
    // a non-object target is rejected
    assert!(boolean("var threw = false; try { new Proxy(1, {}) } catch (e) { threw = e instanceof TypeError } threw"));
}

#[test]
fn string_concat_appends_each_argument() {
    // Minified bundles build a literal out of escapes with `"".concat(...)`;
    // GitHub's landing chunk started its char-class table that way and died
    // with "undefined is not a function".
    assert_eq!(string("'a'.concat('b', 'c')"), "abc");
    assert_eq!(string("'a'.concat()"), "a");
    // each argument goes through ToString
    assert_eq!(string("''.concat(1, null, undefined)"), "1nullundefined");
    assert_eq!(string("'x'.concat(['y', 'z'])"), "xy,z");
    // borrowed through the prototype, like the polyfills do
    assert_eq!(string("String.prototype.concat.call('p', 'q')"), "pq");
}

#[test]
fn promise_with_resolvers_returns_the_three_parts() {
    // `let R = Promise.withResolvers()` is how a batching helper at the top
    // of GitHub's landing bundle starts: it exposes the deferred promise and
    // settles it later.
    assert_eq!(
        mnum("let R = Promise.withResolvers(); let m = 0; R.promise.then(v => m = v); R.resolve(6); m"),
        6.0
    );
    assert_eq!(
        mstring("let R = Promise.withResolvers(); let m = ''; R.promise.catch(e => m = 'caught:' + e); R.reject('x'); m"),
        "caught:x"
    );
    // the parts are functions, and the promise is a real thenable
    assert_eq!(
        string("let R = Promise.withResolvers(); typeof R.resolve + ',' + typeof R.reject + ',' + typeof R.promise.then"),
        "function,function,function"
    );
}

#[test]
fn math_trigonometry_exists() {
    // The landing chunk's easing curve calls Math.asin, Math.sin/cos/tan and
    // Math.atan2; only the arithmetic half of Math existed.
    assert_eq!(num("Math.sin(0)"), 0.0);
    assert!((num("Math.cos(0)") - 1.0).abs() < 1e-12);
    assert!(num("Math.tan(0)").abs() < 1e-12);
    assert!((num("Math.asin(1)") - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
    assert!(num("Math.acos(1)").abs() < 1e-12);
    assert_eq!(num("Math.atan(0)"), 0.0);
    // atan2 takes (y, x)
    assert!((num("Math.atan2(1, 1)") - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
}

#[test]
fn string_substr_takes_a_length() {
    // `"css".charAt(0).toUpperCase() + e.substr(1) + "Plugin"` names a DOM
    // plugin at GitHub's chunk top; `substr` was missing.
    assert_eq!(string("'abcdef'.substr(2)"), "cdef");
    assert_eq!(string("'abcdef'.substr(2, 3)"), "cde");
    assert_eq!(string("'abcdef'.substr(-2)"), "ef");
    assert_eq!(string("'abcdef'.substr(-2, 1)"), "e");
    assert_eq!(string("'abcdef'.substr(99)"), "");
    assert_eq!(string("'abcdef'.substr(1, 0)"), "");
    // borrowed through the prototype
    assert_eq!(string("String.prototype.substr.call('xyz', 1)"), "yz");
}

#[test]
fn regex_emoji_property_escapes() {
    // GitHub's grapheme splitter is
    // `/\p{RI}\p{RI}|\p{Emoji}(\p{EMod}|…)?(…)*|./gu`; RI, Emoji and EMod
    // were unknown names, so the whole landing module was a SyntaxError.
    let ri = |a: u32, b: u32| format!("String.fromCharCode(0x{a:X}, 0x{b:X})");
    assert!(boolean(&format!(
        "var e = {}; /^\\p{{Emoji}}$/u.test(e)",
        ri(0xD83D, 0xDE00)
    )));
    assert!(boolean(&format!(
        "var a = {}, b = {}; /^\\p{{RI}}\\p{{RI}}$/u.test(a + b)",
        ri(0xD83C, 0xDDE6),
        ri(0xD83C, 0xDDE7)
    )));
    assert!(boolean(&format!(
        "var e = {}; /^\\p{{EMod}}$/u.test(e)",
        ri(0xD83C, 0xDFFB)
    )));
    assert!(!boolean(r"/\p{Emoji}/u.test('abc')"));
    assert!(!boolean(r"/\p{RI}/u.test('A')"));
    // the splitter's own alternation compiles
    assert!(boolean(
        "(() => { try { new RegExp('\\\\p{RI}|\\\\p{Emoji}|\\\\p{EMod}|.', 'gu'); return true } catch (e) { return false } })()"
    ));
}

#[test]
fn regex_bidi_control_property_escape() {
    // GitHub's code-view bundle classifies bidi controls with
    // /[\p{Bidi_Control}…]/u to render source files; the unknown property
    // name made the whole bundle a SyntaxError.
    assert!(boolean(r"/^\p{Bidi_Control}$/u.test(String.fromCharCode(0x200F))"));
    assert!(boolean(r"/^\p{Bidi_Control}$/u.test(String.fromCharCode(0x2066))"));
    assert!(boolean(r"/^\p{Bidi_C}$/u.test('\u061C')"));
    assert!(!boolean(r"/\p{Bidi_Control}/u.test('A')"));
    // zero-width space is not a bidi control
    assert!(!boolean(
        r"/\p{Bidi_Control}/u.test(String.fromCharCode(0x200B))"
    ));
}

#[test]
fn regex_unicode_escape_at_class_range_bound() {
    // The splitter's tag-sequence class is `[\u{E0020}-\u{E007E}]`: the
    // range's upper bound used the fixed `\uXXXX` form only, so a `{...}`
    // escape there was "bad hexadecimal escape in pattern".
    assert!(boolean(
        "var lo = String.fromCharCode(0xDB40, 0xDC20), hi = String.fromCharCode(0xDB40, 0xDC7E); /^[\\u{E0020}-\\u{E007E}]$/u.test(lo) && /^[\\u{E0020}-\\u{E007E}]$/u.test(hi)"
    ));
    assert!(!boolean(
        "var out = String.fromCharCode(0xDB40, 0xDC1F); /^[\\u{E0020}-\\u{E007E}]$/u.test(out)"
    ));
}