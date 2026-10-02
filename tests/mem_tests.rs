//! The memory layer's enforcement: moves, borrows, escapes, and
//! nesting are runtime errors. These behaviors are linlua's own (real
//! Lua tables alias freely), so they live outside the differential.

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
fn annotated_own_reads_and_writes() {
    assert_eq!(
        eval("-- @own\nlocal a = {10, 20, 30}\na[2] = 99\na[4] = 40\nprint(#a, a[1], a[2], a[3], a[4])"),
        "4\t10\t99\t30\t40\n"
    );
}

#[test]
fn aliasing_an_owned_value_moves_it() {
    assert_eq!(
        eval("-- @own\nlocal a = {1, 2}\nlocal b = a\nprint(b[1], b[2])"),
        "1\t2\n"
    );
    let e = eval_err("-- @own\nlocal a = {1, 2}\nlocal b = a\nprint(a[1])");
    assert!(e.contains("moved value"), "{e}");
}

#[test]
fn moves_leave_a_tombstone_even_for_globals() {
    let e = eval_err("-- @own\nlocal a = {1}\ng = a\nprint(a[1])");
    assert!(e.contains("moved value"), "{e}");
    assert_eq!(eval("-- @own\nlocal a = {7}\ng = a\nprint(g[1])"), "7\n");
}

#[test]
fn owned_values_cannot_return() {
    let e = eval_err("-- @own\nlocal a = {1}\nlocal function f() return a end\nprint(f())");
    assert!(e.contains("escape"), "{e}");
}

#[test]
fn owned_values_cannot_enter_containers() {
    let e = eval_err("-- @own\nlocal a = {1}\nlocal t = {}\nt[1] = a\nprint(t[1][1])");
    assert!(e.contains("cannot be stored"), "{e}");
    // Storing an ELEMENT is a copy and is fine.
    assert_eq!(
        eval("-- @own\nlocal a = {1}\n-- @own\nlocal b = {a[1]}\nprint(b[1])"),
        "1\n"
    );
}

#[test]
fn owned_values_cannot_nest() {
    let e = eval_err("-- @own\nlocal inner = {1}\n-- @own\nlocal outer = {2}\nouter[2] = inner\nprint(outer[2][1])");
    assert!(e.contains("stored"), "{e}");
}

#[test]
fn references_are_read_only_even_through_copies() {
    assert_eq!(
        eval("-- @own\nlocal a = {5, 6}\n-- @ref\nlocal r = a\nprint(r[1], #r)"),
        "5\t2\n"
    );
    let e = eval_err("-- @own\nlocal a = {5}\n-- @ref\nlocal r = a\nr[1] = 9");
    assert!(e.contains("write through a borrowed reference"), "{e}");
    // Aliasing the reference keeps it read-only.
    let e = eval_err("-- @own\nlocal a = {5}\n-- @ref\nlocal r = a\nlocal r2 = r\nr2[1] = 9");
    assert!(e.contains("write through a borrowed reference"), "{e}");
    // The owner stays alive under a reference.
    assert_eq!(
        eval("-- @own\nlocal a = {5}\n-- @ref\nlocal r = a\na[1] = 8\nprint(r[1], a[1])"),
        "8\t8\n"
    );
}

#[test]
fn calls_borrow_owned_values_and_writes_fail_loudly() {
    // Reads through the borrowed parameter work.
    assert_eq!(
        eval("-- @own\nlocal a = {3, 1}\nlocal function first(t) return t[1] end\nprint(first(a))"),
        "3\n"
    );
    // Writes through the parameter are the documented loud failure.
    let e = eval_err(
        "-- @own\nlocal a = {3, 1}\nlocal function set(t) t[1] = 9 end\nset(a)\nprint(a[1])",
    );
    assert!(e.contains("write through a borrowed reference"), "{e}");
}

#[test]
fn own_tables_are_pure_sequences() {
    let e = eval_err("-- @own\nlocal a = {x = 1}\nprint(a.x)");
    assert!(e.contains("pure sequence"), "{e}");
    let e = eval_err("-- @own\nlocal a = {1}\na.k = 2");
    assert!(e.contains("sequence"), "{e}");
}

#[test]
fn annotation_needs_a_table_literal() {
    let e = eval_err("-- @own\nlocal a = 5\nprint(a)");
    assert!(e.contains("table literal"), "{e}");
    let e = eval_err("-- @own\nlocal a, b = {1}, {2}\nprint(a[1])");
    assert!(e.contains("one name at a time"), "{e}");
}

#[test]
fn own_iteration_and_length_match_tables() {
    assert_eq!(
        eval("-- @own\nlocal a = {4, 5, 6}\nfor i, v in ipairs(a) do print(i, v) end\nprint(#a)"),
        "1\t4\n2\t5\n3\t6\n3\n"
    );
}

#[test]
fn the_cli_uses_inference_by_default() {
    // run_inferred and run agree byte-for-byte on kernel shapes.
    let src = "local a = {1, 2, 3}\nlocal s = 0\nfor i = 1, #a do s = s + a[i] end\nprint(s)\n";
    let mut a = Vec::new();
    let mut b = Vec::new();
    run(src, &mut a).unwrap();
    linlua::run_inferred(src, &mut b).unwrap();
    assert_eq!(a, b);
}
