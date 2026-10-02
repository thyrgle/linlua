//! Differential tests: every fixture runs on the linlua interpreter
//! AND on a real `lua5.4`; the outputs must match byte-for-byte.
//! This is the semantics oracle for the whole project — the same role
//! Node plays for linjs. Skips gracefully when `lua5.4` is absent.

use linlua::check_against_lua;

const FIXTURES: &[&str] = &[
    // Number subtyping and formatting: the %.14g emulation is the
    // most delicate part of M1 — pin it from every angle.
    "print(1, 1.0, 0.5, 100.0, 0.0, -0.0)",
    "print(1e14, 1e15, 1e-4, 1e-5, -1e-5, 1e20, 1e100)",
    "print(0.1, 1/3, 100/3, 2^53, 2^63)",
    "print(1/0, -1/0)",
    "print(0x10, 0xff, 0x8000000000000000, 0x7fffffffffffffff)",
    "print(0x1p4, 0xA.8p-2, 0x.8p1)",
    "print(7.25, 255.5, 0.0009765625, 5e-324, 123456789012345.0)",
    // Arithmetic subtyping: int×int stays int, / is float, ^ is
    // float, // and % are floored.
    "print(2 + 2, 2 - 3, 2 * 3, 7 / 2, 7 // 2, 7 % 3)",
    "print(-7 // 2, 7 // -2, -7 // -2, -7 % 2, 7 % -2, -7 % -2)",
    "print(2 ^ 10, 2 ^ 0.5, 10 // 3.0, 10 % 3.0, -10.5 % 3)",
    "print(1 + 2.0, 2.5 * 4, 9223372036854775807 + 1, math.mininteger // -1)",
    "print(math.maxinteger, math.mininteger, math.pi, math.huge)",
    // Integer wrap-around is documented Lua behavior.
    "print(math.maxinteger + 1 == math.mininteger)",
    "local x = 0x7fffffffffffffff; print(x * 2)",
    // Strings: escapes, long strings, concat with numbers, length.
    r##"print("tab\there", "nl\nline", "q\"q", 'sq\'sq')"##,
    "print([[line one\nline two]])",
    "print(#\"hello\", #\"\", #\"a\\n\")",
    r##"print("n=" .. 42 .. " f=" .. 2.5 .. " s=" .. tostring(true))"##,
    "print(1 .. 2, \"a\" .. \"b\" .. \"c\")",
    // Tables: construction, fields, indexing, insert-order borders.
    "local t = {10, 20, 30} print(#t, t[1], t[2], t[3])",
    "local t = {10, 20, 30} t[2] = 99 print(t[2], #t)",
    "local t = {name = \"linlua\", v = 2} print(t.name, t[\"v\"])",
    "local t = {[3] = \"c\", [1] = \"a\", [2] = \"b\"} print(t[1], t[2], t[3], #t)",
    "local t = {} t.x = 1 t[2.0] = \"two\" print(t.x, t[2], #t)",
    "local t = {1, 2, 3} print(t[4])",
    // Control flow.
    "local s = 0 for i = 1, 5 do s = s + i end print(s)",
    "local s = 0 for i = 10, 1, -3 do s = s + i end print(s)",
    "local s = 0 for i = 0, 1, 0.25 do s = s + i end print(s)",
    "for i = 1, 3 do print(i) end",
    "local n = 0 while n < 5 do n = n + 1 end print(n)",
    "local n = 0 repeat n = n + 1 until n >= 4 print(n)",
    "local r = 0 for i = 1, 10 do if i % 2 == 0 then r = r + i end end print(r)",
    "if 1 > 2 then print(\"no\") elseif 2 > 3 then print(\"no\") else print(\"yes\") end",
    // break lands exactly one loop.
    "local i = 0 while true do i = i + 1 if i >= 3 then break end end print(i)",
    "local s = 0 for i = 1, 10 do if i > 4 then break end s = s + i end print(s)",
    // Truthiness: 0 is truthy; and/or return operands.
    "print(0 or \"x\", false or 2, nil and 1, 1 and 2, nil == false, 0 == false)",
    "print(not nil, not 0, not \"\", not not nil)",
    // Comparisons: numbers across subtypes, strings bytewise.
    "print(1 == 1.0, 1 < 1.5, 2.0 <= 2, \"a\" < \"b\", \"abc\" < \"abd\", \"Z\" < \"a\")",
    "print(1 ~= 2, \"x\" == \"x\", 3 >= 3, 3 > 3.5)",
    // Closures: counter, shared upvalue, recursion.
    "local function make() local n = 0 return function() n = n + 1 return n end end local t = make() print(t(), t(), t())",
    "local a, b = 0, 0 local function inc() a = a + 1 end local function get() return a end inc() inc() b = get() print(a, b)",
    "local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end print(fib(20))",
    "local function fact(n) if n <= 1 then return 1 end return n * fact(n - 1) end print(fact(20), fact(25))",
    // Generic for over sequence tables: insertion order matches
    // Lua's array-part order.
    "for k, v in pairs({10, 20, 30}) do print(k, v) end",
    "for i, v in ipairs({10, 20, 30}) do print(i, v) end",
    // pairs over sequences only: Lua randomizes string-hash seeds
    // per run, so dict iteration order is not differential material.
    "local t = {10, 20, 30} for k, v in pairs(t) do print(k, v) end",
    "local sum = 0 for _, v in ipairs({1, 2, 3, 4}) do sum = sum + v end print(sum)",
    // Kernels: the shapes the WASM backend will target.
    "local a = {1, 2, 3, 4, 5} local s = 0 for i = 0, #a - 1 do s = s + a[i + 1] end print(s)",
    "local a = {4, 1, 3, 2} for i = 1, #a do for j = i + 1, #a do if a[j] < a[i] then local tmp = a[i] a[i] = a[j] a[j] = tmp end end end print(a[1], a[2], a[3], a[4])",
    "local t = {} for i = 1, 5 do t[i] = i * i end print(t[5], #t)",
    "local sum = 0 local i = 1 while i <= 100 do sum = sum + i i = i + 1 end print(sum)",
];

const MEM_FIXTURES: &[(&str, bool)] = &[
    // (source, run_inferred) — annotations are plain comments to
    // lua5.4, so both modes must match it byte-for-byte.
    ("-- @own\nlocal a = {10, 20, 30}\na[2] = 99\na[4] = 40\nprint(#a, a[1], a[2], a[3], a[4])\n", false),
    ("-- @own\nlocal a = {5, 4, 3, 2, 1}\nlocal s = 0\nfor i = 1, #a do s = s + a[i] end\nprint(s)\n", false),
    ("-- @own\nlocal a = {3, 1, 2}\nfor i = 1, #a do for j = i + 1, #a do if a[j] < a[i] then local tmp = a[i] a[i] = a[j] a[j] = tmp end end end\nprint(a[1], a[2], a[3])\n", false),
    ("-- @own\nlocal a = {1, 2, 3}\n-- @ref\nlocal r = a\nprint(r[1], #r, r == a)\n", false),
    ("-- @own\nlocal a = {2, 4, 6}\nlocal function total(t) local s = 0 for i = 1, #t do s = s + t[i] end return s end\nprint(total(a))\n", false),
    // Inferred mode: same programs without annotations.
    ("local a = {10, 20, 30}\nlocal s = 0\nfor i = 1, #a do s = s + a[i] end\nprint(s)\n", true),
    ("local a = {3, 1, 2}\nfor i = 1, #a do a[i] = a[i] * 2 end\nprint(a[1], a[2], a[3])\n", true),
    ("local a = {9, 5, 7}\nlocal function total(t) local s = 0 for i = 1, #t do s = s + t[i] end return s end\nprint(total(a))\n", true),
    ("local a = {}\nfor i = 1, 6 do a[i] = i ^ 2 end\nprint(a[6], #a)\n", true),
];

fn lua54_available() -> bool {
    std::process::Command::new("lua5.4")
        .arg("-v")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn memory_layer_matches_lua54() {
    if !lua54_available() {
        eprintln!("skipping: lua5.4 is not installed");
        return;
    }
    for (src, inferred) in MEM_FIXTURES {
        eprintln!("fixture ({inferred}): {src}");
        let mut ours = Vec::new();
        let result = if *inferred {
            linlua::run_inferred(src, &mut ours)
                .map(|_| ())
                .map_err(|e| e.message)
        } else {
            linlua::run(src, &mut ours).map_err(|e| e.message)
        };
        if let Err(e) = result {
            panic!("linlua errored on:\n{src}\n{e}");
        }
        let ours = String::from_utf8(ours).unwrap();
        let mut child = std::process::Command::new("lua5.4")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn lua5.4");
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(src.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let theirs = String::from_utf8(out.stdout).unwrap();
        assert_eq!(ours, theirs, "divergence on:\n{src}");
    }
}

fn interpreter_matches_lua54() {
    for src in FIXTURES {
        eprintln!("fixture: {src}");
        match check_against_lua(src) {
            Ok(()) => {}
            Err(msg) if msg == "lua5.4 not available" => {
                eprintln!("skipping: lua5.4 is not installed");
                return;
            }
            Err(msg) => panic!("differential failed on:\n{src}\n{msg}"),
        }
    }
}

#[test]
fn float_formatting_matches_lua_per_value() {
    // One value per line, straight from the formatter vs `%.14g` in
    // real Lua — the fastest way to bisect a formatting mismatch.
    let values = [
        "0.0", "-0.0", "1.0", "0.5", "1e-5", "1e14", "1e15", "1/3", "2^53", "5e-324", "1e308*10",
        "-1e-5", "100.0", "1.25e-8",
    ];
    let prog: String = values.iter().map(|v| format!("print({v})\n")).collect();
    match check_against_lua(&prog) {
        Ok(()) => {}
        Err(msg) if msg == "lua5.4 not available" => {
            eprintln!("skipping: lua5.4 is not installed");
        }
        Err(msg) => panic!("formatter divergence:\n{msg}"),
    }
}
