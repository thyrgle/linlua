//! The QBE backend's proofs: every fixture compiles to a native
//! binary (linlua -> WASM -> QBE IL -> assembly -> cc) and the
//! binary's output must match the interpreter AND a real `lua5.4`,
//! byte-for-byte. Skips gracefully when `qbe`, `cc`, or `lua5.4` are
//! absent.

use linlua::{qbe, run};

const FIXTURES: &[&str] = &[
    "local a = {10, 20, 30, 0}\na[2] = 99\na[4] = 40\nprint(#a, a[1], a[2], a[3], a[4])\n",
    "print(2 + 2, 7 / 2, 10 // 3, -7 % 3, 2 ^ 10, 1.5 * 2)\n",
    "print(-7 // 2, 7 // -2, -7 % 2, 7 % -2, -7 % -2)\n",
    "print(10 // 3.0, 10 % 3.0, -10.5 % 3, 2 ^ 0.5)\n",
    "print(1e14, 1e15, 1e-4, 1e-5, 0.1, 100.0, 1/3)\n",
    "print(7.25, 255.5, 0.0009765625, 123456789012345.0, 2^53)\n",
    "print(0x10, 0xff, 0x8000000000000000, math.maxinteger, math.mininteger)\n",
    "print(math.maxinteger + 1 == math.mininteger)\n",
    "print(6 & 3, 6 | 3, 6 ~ 3, ~6, 1 << 8, 256 >> 4)\n",
    "print(1 < 2 and 3 > 2, not false, true or false)\n",
    "local x = 5\nif x < 3 then print(1) elseif x < 10 then print(2) else print(3) end\n",
    "local s = 0\nfor i = 10, 1, -3 do s = s + i end\nprint(s)\n",
    "local s = 0.0\nfor i = 0, 1, 0.25 do s = s + i end\nprint(s)\n",
    "local n = 0\nrepeat n = n + 1 until n >= 4\nprint(n)\n",
    "local i = 0\nwhile true do i = i + 1 if i >= 3 then break end end\nprint(i)\n",
    "local r = 0\nfor i = 1, 10 do if i % 2 == 0 then r = r + i end end\nprint(r)\n",
    "local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end\nprint(fib(20))\n",
    "local function fact(n) if n <= 1 then return 1 end return n * fact(n - 1) end\nprint(fact(20))\n",
    "local function total(t) local s = 0 for i = 1, #t do s = s + t[i] end return s end\nlocal a = {4, 5, 6}\nprint(total(a))\n",
    "local function add(a, b) return a + b end\nprint(add(2, 3), add(10, 20))\n",
    "print(\"hello\", \"linlua\")\n",
    "print(0x10, 0xff, 0x8000000000000000)\n",
    "local a = {3, 1, 2}\nfor i = 1, #a do for j = i + 1, #a do if a[j] < a[i] then local tmp = a[i] a[i] = a[j] a[j] = tmp end end end\nprint(a[1], a[2], a[3])\n",
    "local sum = 0\nlocal i = 1\nwhile i <= 100 do sum = sum + i i = i + 1 end\nprint(sum)\n",
];

fn tool_available(cmd: &str, flag: &str) -> bool {
    std::process::Command::new(cmd)
        .arg(flag)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn native_matches_the_interpreter_and_lua() {
    if !tool_available("qbe", "-h") && std::env::var("LINLUA_QBE").is_err() {
        eprintln!("skipping: qbe is not installed");
        return;
    }
    if !tool_available("cc", "--version") {
        eprintln!("skipping: cc is not installed");
        return;
    }
    let lua54 = tool_available("lua5.4", "-v");

    for src in FIXTURES {
        eprintln!("fixture: {}", src.lines().next().unwrap_or(src));

        let mut interp = Vec::new();
        run(src, &mut interp).expect("interp");
        let interp = String::from_utf8(interp).unwrap();

        let native = match qbe::run_native(src) {
            Ok(o) => o,
            Err(e) => panic!("native failed on:\n{src}\n{e}"),
        };

        assert_eq!(native, interp, "native vs interpreter on:\n{src}");

        if lua54 {
            use std::io::Write;
            use std::process::{Command, Stdio};
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
            let out = child.wait_with_output().unwrap();
            let lua_out = String::from_utf8(out.stdout).unwrap();
            assert_eq!(native, lua_out, "native vs lua5.4 on:\n{src}");
        }
    }
}

#[test]
fn il_is_deterministic() {
    // The same program lowers to the same IL — the compiler has no
    // hidden state.
    let src = "local a = {1, 2}\nprint(a[1] + a[2])\n";
    let a = qbe::wasm_to_qbe(&linlua::compile_to_wasm(src).unwrap()).unwrap();
    let b = qbe::wasm_to_qbe(&linlua::compile_to_wasm(src).unwrap()).unwrap();
    assert_eq!(a, b);
}
