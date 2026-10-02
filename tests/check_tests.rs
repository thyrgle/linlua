//! The static type layer: Luau-flavored annotations checked at
//! compile time, erased at runtime. These tests pin both the
//! diagnostics and the erasure guarantee — types never change what a
//! program does.

use linlua::{check_program, parse_chunk, run, Interp};

fn errors(src: &str) -> Vec<String> {
    check_program(src)
        .expect("parse")
        .into_iter()
        .map(|e| e.render())
        .collect()
}

fn clean(src: &str) -> bool {
    let es = errors(src);
    if !es.is_empty() {
        eprintln!("unexpected errors: {es:?}");
    }
    es.is_empty()
}

fn first_error(src: &str) -> String {
    let es = errors(src);
    assert!(!es.is_empty(), "expected errors on:\n{src}");
    es[0].clone()
}

#[test]
fn annotated_declarations_check() {
    assert!(clean("local x: number = 5\nprint(x)\n"));
    assert!(clean("local s: string = \"hi\"\nprint(s)\n"));
    assert!(clean("local b: boolean = true\nprint(b)\n"));
    assert!(clean("local n: nil = nil\nprint(n)\n"));
    assert!(clean("local a: any = 5\na = \"now a string\"\nprint(a)\n"));
}

#[test]
fn assignment_mismatches_error() {
    let e = first_error("local x: number = 5\nx = \"oops\"\nprint(x)\n");
    assert!(e.contains("cannot assign string to `number`"), "{e}");
}

#[test]
fn initializer_mismatches_error() {
    let e = first_error("local x: number = \"not a number\"\nprint(x)\n");
    assert!(e.contains("cannot assign string to `number`"), "{e}");
    let e = first_error("local b: boolean = 42\nprint(b)\n");
    assert!(e.contains("cannot assign number to `boolean`"), "{e}");
    let e = first_error("local n: nil = 0\nprint(n)\n");
    assert!(e.contains("cannot assign number to `nil`"), "{e}");
}

#[test]
fn array_annotations_and_elements() {
    assert!(clean("local xs: {number} = {1, 2, 3}\nprint(xs[1])\n"));
    assert!(clean("local ss: {string} = {\"a\"}\nprint(ss[1])\n"));
    let e = first_error("local xs: {number} = {\"a\"}\nprint(xs[1])\n");
    assert!(e.contains("cannot assign {string} to `{number}`"), "{e}");
    // Mixed literal: the array degrades to {any}, which still
    // rejects against {number}.
    let e = first_error("local xs: {number} = {1, \"a\"}\nprint(xs[1])\n");
    assert!(e.contains("cannot assign {any} to `{number}`"), "{e}");
    // Nested arrays.
    assert!(clean("local g: {{number}} = {{1}, {2}}\nprint(g[1][1])\n"));
}

#[test]
fn array_index_and_store_checks() {
    let e = first_error("local xs: {number} = {1}\nprint(xs[\"k\"])\n");
    assert!(e.contains("array index must be a number"), "{e}");
    let e = first_error("local xs: {number} = {1}\nxs[1] = \"a\"\nprint(xs[1])\n");
    assert!(
        e.contains("cannot store string in an array of number"),
        "{e}"
    );
}

#[test]
fn function_param_and_return_checks() {
    assert!(clean(
        "local function f(a: number, b: string): boolean\n  return a < 1 and b ~= \"\"\nend\nprint(f(1, \"x\"))\n"
    ));
    // Return type mismatch.
    let e = first_error("local function f(): number\n  return \"no\"\nend\nprint(f())\n");
    assert!(
        e.contains("cannot return string from a function of number"),
        "{e}"
    );
    // Bare return in a non-nil function.
    let e = first_error("local function f(): number\n  return\nend\nprint(f())\n");
    assert!(e.contains("returns nil"), "{e}");
}

#[test]
fn param_types_are_enforced_inside_the_body() {
    // Numbers concat legally in Lua; booleans do not.
    let e = first_error("local function f(b: boolean)\n  return b .. \"x\"\nend\nprint(f(true))\n");
    assert!(e.contains("cannot concatenate boolean"), "{e}");
}

#[test]
fn arithmetic_and_concat_type_errors() {
    let e = first_error("local s: string = \"a\"\nprint(s + 1)\n");
    assert!(e.contains("cannot apply arithmetic to string"), "{e}");
    let e = first_error("local b: boolean = true\nprint(b .. \"x\")\n");
    assert!(e.contains("cannot concatenate boolean"), "{e}");
    let e = first_error("local f: boolean = true\nprint(#f)\n");
    assert!(e.contains("cannot take the length of boolean"), "{e}");
}

#[test]
fn comparison_checks() {
    let e = first_error("local s: string = \"a\"\nprint(s < 1)\n");
    assert!(e.contains("cannot compare string with number"), "{e}");
    assert!(clean("print(1 < 2, \"a\" < \"b\")\n"));
}

#[test]
fn for_loop_bounds_and_vars() {
    let e = first_error("for i = \"a\", 10 do print(i) end\n");
    assert!(e.contains("loop bounds must be numbers"), "{e}");
    assert!(clean("for i = 1, 10 do print(i) end\n"));
    // The loop variable is a number inside the body.
    assert!(clean("for i = 1, 3 do print(i + 1) end\n"));
}

#[test]
fn for_in_over_typed_arrays_ties_the_element_type() {
    assert!(clean(
        "local xs: {number} = {1, 2}\nfor i, v in ipairs(xs) do print(i, v + 1) end\n"
    ));
}

#[test]
fn inference_covers_obvious_initializers() {
    assert!(clean("local x = 5\nx = 6\nprint(x)\n"));
    let e = first_error("local x = 5\nx = \"s\"\nprint(x)\n");
    assert!(e.contains("cannot assign string to `number`"), "{e}");
    assert!(clean("local s = \"a\" .. \"b\"\nprint(s)\n"));
    assert!(clean("local b = 1 < 2\nprint(b)\n"));
    assert!(clean("local t = {1, 2, 3}\nprint(t[1])\n"));
}

#[test]
fn any_is_compatible_everywhere() {
    assert!(clean("local a: any = 5\nlocal n: number = a\nprint(n)\n"));
    assert!(clean("local n: number = 1\nlocal a: any = n\nprint(a)\n"));
}

#[test]
fn scopes_shadow_cleanly() {
    assert!(clean(
        "local x: number = 1\ndo local x: string = \"a\" print(x) end\nprint(x)\n"
    ));
    let e =
        first_error("local x: number = 1\ndo local x: string = \"a\"\n  x = 2\nend\nprint(x)\n");
    assert!(e.contains("cannot assign number to `string`"), "{e}");
}

#[test]
fn unannotated_code_never_errors() {
    // The dynamic core stays unannotated-compatible: any-typed.
    assert!(clean(
        "local t = {x = 1}\nprint(t.x, t[\"x\"], #t)\nlocal u = t\nprint(u)\n"
    ));
}

#[test]
fn unknown_types_are_parse_errors() {
    let e = linlua::parse_chunk("local x: Person = {}\n").unwrap_err();
    assert!(e.message.contains("unknown type `Person`"), "{}", e.message);
}

#[test]
fn types_are_erased_at_runtime() {
    // The same program with and without annotations runs identically.
    let untyped = "local x = 5\nlocal s = \"a\"\nlocal t = {1, 2}\nprint(x, s, t[2])\n";
    let typed = "local x: number = 5\nlocal s: string = \"a\"\nlocal t: {number} = {1, 2}\nprint(x, s, t[2])\n";
    let mut a = Vec::new();
    let mut b = Vec::new();
    Interp::new(&mut a)
        .run(&parse_chunk(untyped).unwrap())
        .unwrap();
    Interp::new(&mut b)
        .run(&parse_chunk(typed).unwrap())
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(String::from_utf8(a).unwrap(), "5\ta\t2\n");
    let _ = run(untyped, &mut Vec::new());
}
