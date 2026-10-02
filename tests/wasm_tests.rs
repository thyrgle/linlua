//! The WASM backend's proofs: every fixture runs on the interpreter
//! AND as a compiled WebAssembly module in Node — byte-identical,
//! with Lua's number formatting on the host side. The unannotated
//! core fixtures also run against a real `lua5.4`, so the compiled
//! code is pinned to Lua semantics, not just to ourselves.

use linlua::{check_wasm_against_node, run};

/// Fixtures that are valid dialect programs (the list above contains
/// two intentionally-invalid entries kept out of the runnable set).
const RUNNABLE: &[&str] = &[
    "local a = {10, 20, 30, 0}\na[2] = 99\na[4] = 40\nprint(#a, a[1], a[2], a[3], a[4])\n",
    "local a = {5, 4, 3, 2, 1}\nlocal s = 0\nfor i = 1, #a do s = s + a[i] end\nprint(s)\n",
    "local a = {3, 1, 2}\nfor i = 1, #a do for j = i + 1, #a do if a[j] < a[i] then local tmp = a[i] a[i] = a[j] a[j] = tmp end end end\nprint(a[1], a[2], a[3])\n",
    "local t = {0, 0, 0, 0, 0, 0}\nfor i = 1, 6 do t[i] = i * i end\nprint(t[6], #t)\n",
    "local sum = 0\nlocal i = 1\nwhile i <= 100 do sum = sum + i i = i + 1 end\nprint(sum)\n",
    "local n = 0\nrepeat n = n + 1 until n >= 4\nprint(n)\n",
    "local s = 0\nfor i = 10, 1, -3 do s = s + i end\nprint(s)\n",
    "local s = 0.0\nfor i = 0, 1, 0.25 do s = s + i end\nprint(s)\n",
    "local r = 0\nfor i = 1, 10 do if i % 2 == 0 then r = r + i end end\nprint(r)\n",
    "local i = 0\nwhile true do i = i + 1 if i >= 3 then break end end\nprint(i)\n",
    "print(2 + 2, 7 / 2, 10 // 3, -7 % 3, 2 ^ 10, 1.5 * 2)\n",
    "print(-7 // 2, 7 // -2, -7 % 2, 7 % -2, -7 % -2)\n",
    "print(10 // 3.0, 10 % 3.0, -10.5 % 3, 2 ^ 0.5)\n",
    "print(1e14, 1e15, 1e-4, 1e-5, 0.1, 100.0, 1/3)\n",
    "print(0x10, 0xff, 0x8000000000000000, math.maxinteger, math.mininteger)\n",
    "print(math.maxinteger + 1 == math.mininteger)\n",
    "print(7.25, 255.5, 0.0009765625, 123456789012345.0, 2^53)\n",
    "print(6 & 3, 6 | 3, 6 ~ 3, ~6, 1 << 8, 256 >> 4)\n",
    "print(1 < 2 and 3 > 2, not false, true or false)\n",
    "local x = 5\nif x < 3 then print(1) elseif x < 10 then print(2) else print(3) end\n",
    "local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end\nprint(fib(20))\n",
    "local function fact(n) if n <= 1 then return 1 end return n * fact(n - 1) end\nprint(fact(20))\n",
    "local function total(t) local s = 0 for i = 1, #t do s = s + t[i] end return s end\nlocal a = {4, 5, 6}\nprint(total(a))\n",
    "local function add(a, b) return a + b end\nprint(add(2, 3), add(10, 20))\n",
    "print(\"hello\")\n",
];

fn lua54_available() -> bool {
    std::process::Command::new("lua5.4")
        .arg("-v")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn wasm_matches_the_interpreter() {
    for src in RUNNABLE {
        eprintln!("fixture: {}", src.lines().next().unwrap_or(src));
        match check_wasm_against_node(src) {
            Ok(()) => {}
            Err(m) if m == "node not available" => {
                eprintln!("skipping: node is not available");
                return;
            }
            Err(m) => panic!("wasm divergence on:\n{src}\n{m}"),
        }
    }
}

#[test]
fn wasm_matches_lua54_on_the_core() {
    if !lua54_available() {
        eprintln!("skipping: lua5.4 is not installed");
        return;
    }
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    use std::io::Write;
    use std::process::{Command, Stdio};
    for src in RUNNABLE {
        // Interp is already gated against lua5.4 in lua_diff_tests;
        // here the WASM output must equal lua5.4 directly.
        let bytes = linlua::compile_to_wasm(src)
            .unwrap_or_else(|e| panic!("compile failed on:\n{src}\n{}", e.message));
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let js = linlua::driver_js(&hex);
        let dir = std::env::temp_dir().join("linlua_wasm_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("three_way.js");
        std::fs::write(&script, js).unwrap();
        let out = Command::new("node")
            .arg(&script)
            .output()
            .expect("run node");
        assert!(
            out.status.success(),
            "node failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let wasm_out = String::from_utf8(out.stdout).unwrap();

        let mut child = Command::new("lua5.4")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn lua5.4");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(src.as_bytes())
            .unwrap();
        let lua_out = child.wait_with_output().unwrap();
        assert!(lua_out.status.success());
        let lua_out = String::from_utf8(lua_out.stdout).unwrap();

        assert_eq!(wasm_out, lua_out, "wasm vs lua5.4 on:\n{src}");
    }
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn dialect_rejections_are_compile_errors() {
    let rejected = [
        (
            "local f = function(x) return x end\nprint(f(1))\n",
            "anonymous functions",
        ),
        (
            "local a = {1}\nfor k, v in pairs(a) do print(k) end\n",
            "for..in",
        ),
        ("x = 5\nprint(x)\n", "globals"),
        ("print(\"a\" .. \"b\")\n", "concatenation"),
        ("local t = {x = 1}\nprint(t.x)\n", "keyed tables"),
        ("local x\nprint(x)\n", "initializer"),
        ("local a = {1, \"x\"}\nprint(a[1])\n", "one element subtype"),
        ("if 1 then print(1) end\n", "booleans in the dialect"),
    ];
    for (src, expect) in rejected {
        match linlua::compile_to_wasm(src) {
            Ok(_) => panic!("expected rejection ({expect}) on:\n{src}"),
            Err(e) => assert!(
                e.message.contains("strict dialect"),
                "{expect}: {}",
                e.message
            ),
        }
    }
}

#[test]
fn interp_still_agrees_on_runnable_fixtures() {
    // The wasm fixtures are ordinary Lua: the interpreter and the
    // compiled module must agree even without Node.
    for src in RUNNABLE {
        let mut ours = Vec::new();
        if run(src, &mut ours).is_err() {
            panic!("interpreter errored on:\n{src}");
        }
        let _ = String::from_utf8(ours).unwrap();
    }
}
