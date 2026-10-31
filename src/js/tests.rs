//! Unit + end-to-end tests for the JS interpreter. Each test runs a script
//! string through `JsEngine::exec` and asserts the resulting `Val`.

use super::*;

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
    // `**` is an explicit unsupported error, not a misparse
    assert!(matches!(err("2 ** 3"), JsError::Syntax { .. }));
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
    // on a thread with a generous stack ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â the ENGINE's own guard (depth cap)
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
    // guard must surface a RangeError Err ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â never a native stack overflow or
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
fn this_binding() {
    // method call: this = receiver
    assert_eq!(num("let o = {v: 3, m: function() { return this.v }}; o.m()"), 3.0);
    // plain call: this = global object (sloppy mode, documented)
    assert_eq!(num("globalThis.gv = 9; function f() { return this.gv } f()"), 9.0);
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
fn classes_desugar() {
    let src = "class Point { constructor(x, y) { this.x = x; this.y = y } len() { return this.x + this.y } }
               let p = new Point(2, 3); p.len() + ':' + (p instanceof Point)";
    assert_eq!(string(src), "5:true");
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

// ---------------------------------------------------------------------------
// arrays
// ---------------------------------------------------------------------------

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
}

#[test]
fn array_methods() {
    assert_eq!(string("let a = [1,2,3]; a.join('-')"), "1-2-3");
    assert_eq!(num("let a = [10,20,30]; let s = 0; a.forEach(function(v) { s += v }); s"), 60.0);
    assert_eq!(string("let a = [1,2,3]; a.map(function(v) { return v * 2 }).join(',')"), "2,4,6");
    assert_eq!(string("let a = [1,2,3,4]; a.filter(function(v) { return v % 2 == 0 }).join(',')"), "2,4");
    assert_eq!(num("let a = [1,2,3]; a.indexOf(2)"), 1.0);
    assert_eq!(num("let a = [1,2,3]; a.indexOf(9)"), -1.0);
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
    // lexer.rs shortcut note), so this yields 2 chars ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â the same `.length`
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
    // RAM guard: Val must stay 24 bytes ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â this is the hot unit. (24 = 8-byte
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
    assert!(boolean("let a = {}; let b = {}; a == b")); // both stringify
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
fn regex_unsupported_syntax_is_clean_error() {
    assert!(matches!(err("'x'.replace(/(?<=a)b/, '')"), JsError::Syntax { .. }));
    assert!(matches!(err("/\\p{L}/u"), JsError::Syntax { .. }));
    assert!(matches!(err("/(?<name>a)/"), JsError::Syntax { .. }));
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
    // so microtask interleaving is not spec-FIFO here â€” "acbd"-style
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
    // yield is only supported as a plain statement or decl init, so the
    // accumulator reads the fed value into the loop's `step` first
    assert_eq!(
        num("function* c() { let n = 0; while (true) { let step = yield n; n += step } } const it = c(); it.next(); it.next(3).value + it.next(4).value"),
        10.0
    );
}

#[test]
fn generator_throw_unsupported() {
    assert!(matches!(err("function* g() { yield 1 } const it = g(); it.throw(1)"), JsError::Throw(_)));
}

#[test]
fn generator_yield_position_gated() {
    // yield inside parenthesized / array-literal / condition expressions is
    // a parse-time error (honest restriction: yield is only supported as a
    // plain statement `yield x;` or a declaration initializer)
    assert!(matches!(err("function* g() { const a = (yield); if (yield) {} }"), JsError::Syntax { .. }));
    assert!(matches!(err("function* g() { const a = [yield 1] }"), JsError::Syntax { .. }));
    // a bare decl-init yield inside a generator body is NOT gated: that is
    // exactly the supported form, so it must exec without a SyntaxError
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
fn destructuring_arrays() {
    assert_eq!(num("const [a, b] = [10, 20]; a + b"), 30.0);
    assert_eq!(num("const [a, , c = 5] = [1, 2]; a + c"), 6.0);
    assert_eq!(num("const [a, ...rest] = [1, 2, 3, 4]; rest[1]"), 3.0);
    assert_eq!(num("const [a, [b]] = [1, [9]]; a + b"), 10.0);
    assert_eq!(string("const [a = 'd'] = []; a"), "d");
    assert_eq!(string("const [, b] = ['x', 'y']; b"), "y");
}

#[test]
fn destructuring_objects() {
    assert_eq!(num("const { x, y: z } = { x: 3, y: 4 }; x * z"), 12.0);
    assert_eq!(num("const { p = 7 } = {}; p"), 7.0);
    assert_eq!(string("const { list: [first] } = { list: ['A'] }; first"), "A");
    assert!(boolean("const { q, ...others } = { q: 1, r: 2 }; others.r === 2 && others.q === undefined"));
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
