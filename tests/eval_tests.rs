//! Interpreter-only tests: error cases, and behaviors whose shape is
//! linlua's own (v1 dialect limits) rather than Lua-differential
//! material.

use linlua::{parse_chunk, run, Interp};

fn eval(src: &str) -> String {
    let mut out = Vec::new();
    Interp::new(&mut out)
        .run(&parse_chunk(src).expect("parse"))
        .expect("run");
    String::from_utf8(out).expect("utf8")
}

fn eval_err(src: &str) -> String {
    let mut out = Vec::new();
    match Interp::new(&mut out).run(&parse_chunk(src).expect("parse")) {
        Ok(_) => panic!("expected an error on:\n{src}"),
        Err(e) => e.message,
    }
}

#[test]
fn arithmetic_on_nil_is_an_error() {
    let e = eval_err("local x; print(x + 1)");
    assert!(e.contains("arithmetic on a nil"), "{e}");
}

#[test]
fn indexing_nil_is_an_error() {
    let e = eval_err("local t; print(t.x)");
    assert!(e.contains("index a nil"), "{e}");
}

#[test]
fn calling_non_functions_is_an_error() {
    let e = eval_err("local x = 3; x()");
    assert!(e.contains("call a number"), "{e}");
}

#[test]
fn comparing_number_with_string_is_an_error() {
    let e = eval_err("print(1 < \"a\")");
    assert!(e.contains("compare"), "{e}");
}

#[test]
fn concatenating_nil_is_an_error() {
    let e = eval_err("local x; print(\"a\" .. x)");
    assert!(e.contains("concatenate a nil"), "{e}");
}

#[test]
fn integer_modulo_by_zero_errors_like_lua() {
    let e = eval_err("print(1 % 0)");
    assert!(e.contains("n%%0"), "{e}");
    let e = eval_err("print(1 // 0)");
    assert!(e.contains("n//0"), "{e}");
}

#[test]
fn float_modulo_by_zero_is_nan_like_lua() {
    // Lua: 1.0 % 0 is nan (IEEE), no error.
    assert_eq!(eval("print(1.0 % 0)"), "nan\n");
    assert_eq!(eval("print(1.0 / 0)"), "inf\n");
}

#[test]
fn for_step_zero_is_an_error() {
    let e = eval_err("for i = 1, 10, 0 do print(i) end");
    assert!(e.contains("'for' step is zero"), "{e}");
}

#[test]
fn bitwise_ops_reject_fractional_floats() {
    let e = eval_err("print(2.5 | 1)");
    assert!(e.contains("no integer representation"), "{e}");
    assert_eq!(eval("print(2.0 | 1)"), "3\n");
}

#[test]
fn globals_and_locals_shadow() {
    assert_eq!(
        eval("x = 1 local x = 2 print(x) local function f() return x end print(f())"),
        "2\n2\n"
    );
    assert_eq!(
        eval("x = 1 local function f() x = x + 1 end f() print(x)"),
        "2\n"
    );
    assert_eq!(eval("do local x = 5 end x = 9 print(x)"), "9\n");
}

#[test]
fn recursion_through_local_function() {
    assert_eq!(
        eval(
            "local function f(n) if n <= 0 then return 0 end return n + f(n - 1) end print(f(100))"
        ),
        "5050\n"
    );
}

#[test]
fn table_identity_not_content_equality() {
    assert_eq!(
        eval("local a = {1} local b = {1} print(a == b, a == a)"),
        "false\ttrue\n"
    );
}

#[test]
fn nan_is_not_equal_to_itself() {
    assert_eq!(eval("local x = 0/0 print(x == x, x ~= x)"), "false\ttrue\n");
}

#[test]
fn string_keys_and_normalized_float_keys() {
    assert_eq!(
        eval("local t = {} t[\"k\"] = 1 t[2.0] = 2 print(t.k, t[2])"),
        "1\t2\n"
    );
}

#[test]
fn varargs_are_rejected_in_the_v1_dialect() {
    let e = parse_chunk("local function f(...) end").unwrap_err();
    assert!(e.message.contains("varargs"), "{}", e.message);
}

#[test]
fn parse_errors_carry_positions() {
    let err = parse_chunk("local x = \nreturn").unwrap_err();
    assert!(err.offset > 0);
    assert!(!err.message.is_empty());
}

#[test]
fn semicolons_are_tolerated() {
    assert_eq!(eval("local x = 1; print(x);"), "1\n");
}

#[test]
fn cli_run_is_available() {
    // The `linlua run` binary path shares this entry point.
    let mut out = Vec::new();
    run("print(6 * 7)", &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "42\n");
}
