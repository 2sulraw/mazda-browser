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
    // on a thread with a generous stack — the ENGINE's own guard (depth cap)
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
    // guard must surface a RangeError Err — never a native stack overflow or
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
    // lexer.rs shortcut note), so this yields 2 chars — the same `.length`
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
    assert_eq!(string("encodeURIComponent('héllo')"), "h%C3%A9llo");
    assert_eq!(string("decodeURIComponent('a%20b')"), "a b");
    assert_eq!(string("decodeURIComponent('h%C3%A9llo')"), "héllo");
    // round trip
    assert_eq!(string("decodeURIComponent(encodeURIComponent('π/π'))"), "π/π");
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
    // `new Promise(executor)` is honestly unsupported
    assert!(matches!(
        e.exec("new Promise(function(res) { res(1) })"),
        Err(JsError::Throw(_))
    ));
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
    // RAM guard: Val must stay 24 bytes — this is the hot unit. (24 = 8-byte
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
