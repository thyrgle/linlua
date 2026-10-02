//! Ownership inference: which declarations land in the arena, and the
//! guarantee that inference is output-invisible.

use linlua::{infer, parse_chunk, run, run_inferred, Interp};

fn verdicts(src: &str) -> (usize, usize) {
    let mut chunk = parse_chunk(src).expect("parse");
    let info = infer::infer(&mut chunk);
    (info.own, info.gc)
}

#[test]
fn plain_reads_infer_own() {
    let (own, _) = verdicts(
        "local a = {10, 20, 30}\nlocal s = 0\nfor i = 1, #a do s = s + a[i] end\nprint(s)\n",
    );
    assert_eq!(own, 1, "element reads, #a, and loops qualify");
}

#[test]
fn aliasing_stays_gc() {
    let (own, _) = verdicts("local b = {1, 2, 3}\nlocal alias = b\nprint(#alias)\n");
    assert_eq!(own, 0, "aliasing shares the value");
}

#[test]
fn closure_capture_stays_gc() {
    let (own, _) =
        verdicts("local c = {5, 6, 7}\nlocal function get() return c[1] end\nprint(get())\n");
    assert_eq!(own, 0, "closures outlive the scope");
}

#[test]
fn container_store_keeps_the_value_gc() {
    let (_, gc) = verdicts("local d = {5}\nlocal t = {}\nt[1] = d\nprint(t[1][1])\n");
    assert_eq!(gc, 1, "the stored value must stay GC");
}

#[test]
fn returning_the_name_stays_gc_but_element_reads_are_fine() {
    let (own, _) = verdicts("local e = {9}\nprint(e[1])\n");
    assert_eq!(own, 1);
    let (own, _) = verdicts("local f = {1, 2}\nlocal x = f[2]\nprint(x)\n");
    assert_eq!(own, 1, "element reads at the top level qualify");
    // A closure reading f IS a capture: it outlives the scope.
    let (own, _) =
        verdicts("local f = {1, 2}\nlocal function pick() return f[2] end\nprint(pick())\n");
    assert_eq!(own, 0, "any closure capture stays GC");
    let (own, _) = verdicts("local h = {1, 2}\nlocal function bad() return h end\nprint(bad())\n");
    assert_eq!(own, 0, "returning the name escapes");
}

#[test]
fn calls_borrow_but_arithmetic_args_do_not() {
    let (own, _) =
        verdicts("local g = {3, 1, 2}\nlocal function first(t) return t[1] end\nprint(first(g))\n");
    assert_eq!(own, 1, "f(a) borrows");
    let (own, _) = verdicts("local k = {1}\nprint(#k + 0)\n");
    assert_eq!(own, 1, "#a in arithmetic reads");
    // Note: programs that PRINT table addresses are outside the
    // invisibility contract — GC and arena storage use different
    // (equally arbitrary) address formats, and GC addresses are
    // ASLR-randomized anyway, so no such program can be
    // differential-tested at all.
}

#[test]
fn reassignment_and_shadowing_stay_gc() {
    let (own, _) = verdicts("local n = {1}\nn = {2}\nprint(n[1])\n");
    assert_eq!(own, 0, "reassignment replaces the value");
}

#[test]
fn inference_is_output_invisible() {
    let programs = [
        "local a = {10, 20, 30}\nlocal s = 0\nfor i = 1, #a do s = s + a[i] end\nprint(s)\n",
        "local a = {4, 1, 3}\nfor i = 1, #a do for j = i + 1, #a do if a[j] < a[i] then local tmp = a[i] a[i] = a[j] a[j] = tmp end end end\nprint(a[1], a[2], a[3])\n",
        "local t = {}\nfor i = 1, 5 do t[i] = i * i end\nprint(t[5], #t)\n",
    ];
    for src in programs {
        let mut plain = Vec::new();
        run(src, &mut plain).expect("run");
        let mut inferred = Vec::new();
        run_inferred(src, &mut inferred).expect("run_inferred");
        assert_eq!(plain, inferred, "inference changed output on:\n{src}");
    }
}

#[test]
fn inference_actually_allocates_into_the_arena() {
    let src = "local a = {10, 20, 30}\nlocal s = 0\nfor i = 1, #a do s = s + a[i] end\nprint(s)\n";
    let mut out = Vec::new();
    let mut chunk = parse_chunk(src).expect("parse");
    infer::infer(&mut chunk);
    let mut interp = Interp::new(&mut out);
    interp.run(&chunk).expect("run");
    assert_eq!(interp.arenas().borrow().len(linlua::mem::OwnHandle(0)), 3);
}
