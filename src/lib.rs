//! linlua: a Luau-flavored Lua subset with gradual types and gradual
//! linear memory, on the linjs engine architecture.
//!
//! M1: lexer, parser, and a tree-walking interpreter with Lua 5.4
//! semantics — integer/float subtyping, floored division, insertion-
//! ordered tables, and closures — differential-tested against a real
//! `lua5.4`.

pub mod ast;
pub mod check;
pub mod infer;
pub mod interp;
pub mod lexer;
pub mod mem;
pub mod parser;
pub mod value;
pub mod wasm;

pub use ast::{BinOp, Chunk, Expr, Stmt, Target, TypeAnn, UnOp};
pub use interp::{Interp, InterpError};
pub use lexer::{LexError, NumLit};
pub use parser::{parse_chunk, ParseError};
pub use value::{Key, Table, Value};

/// Parses and runs `source` on the interpreter, writing `print`
/// output to `out`.
pub fn run(source: &str, out: &mut dyn std::io::Write) -> Result<(), InterpError> {
    let chunk = parse_chunk(source).map_err(|e| InterpError {
        message: format!("parse error: {}", e.message),
    })?;
    Interp::new(out).run(&chunk)
}

/// Typechecks `source` statically and returns every type error.
///
/// Annotations are Luau-flavored: checked here, erased at runtime.
/// Unannotated code is inferred where the initializer is obvious and
/// treated as `any` elsewhere; `any` is compatible with everything.
pub fn check_program(source: &str) -> Result<Vec<check::TypeError>, String> {
    let chunk = parse_chunk(source).map_err(|e| e.message)?;
    Ok(check::check_program(&chunk))
}

/// Parses, infers ownership, and runs. Declarations that provably
/// never escape allocate into the arena — output-identical to
/// [`run`] by construction, which the differential suite pins.
pub fn run_inferred(
    source: &str,
    out: &mut dyn std::io::Write,
) -> Result<infer::Inference, InterpError> {
    let mut chunk = parse_chunk(source).map_err(|e| InterpError {
        message: format!("parse error: {}", e.message),
    })?;
    let inference = infer::infer(&mut chunk);
    Interp::new(out).run(&chunk)?;
    Ok(inference)
}

/// Compiles the strict dialect to a WebAssembly module: Lua's
/// integer/float subtypes as i64/f64, sequences in linear memory,
/// direct calls to top-level functions. The module exports `run()`
/// and `memory`; `print` arrives through `env.log*` imports.
pub fn compile_to_wasm(source: &str) -> Result<Vec<u8>, wasm::WasmError> {
    wasm::compile(source)
}

/// The full proof: the program runs on the interpreter AND as a
/// compiled WebAssembly module in Node — the outputs must match
/// byte-for-byte, with Lua-style number formatting on the host side.
/// Returns an explanation when they disagree or Node is unavailable.
pub fn check_wasm_against_node(source: &str) -> Result<(), String> {
    let node = which_node().ok_or("node not available")?;
    let bytes = compile_to_wasm(source).map_err(|e| e.message)?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    let mut ours = Vec::new();
    run(source, &mut ours).map_err(|e| format!("linlua error: {}", e.message))?;
    let ours = String::from_utf8(ours).map_err(|e| e.to_string())?;

    let js = DRIVER_JS.replace("__HEX__", &hex);
    let dir = std::env::temp_dir().join("linlua_wasm_tests");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let script = dir.join("driver.js");
    std::fs::write(&script, js).map_err(|e| e.to_string())?;

    let out = std::process::Command::new(&node)
        .arg(&script)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "node errored: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let theirs = String::from_utf8(out.stdout).map_err(|e| e.to_string())?;
    if ours != theirs {
        return Err(format!(
            "outputs diverge:\n-- interpreter --\n{ours}\n-- wasm --\n{theirs}"
        ));
    }
    Ok(())
}

/// The Node driver for a module's hex: Lua's %.14g float formatting
/// with the .0 suffix rule, BigInt integers, and the len+bytes string
/// layout. Exposed for the three-way test harness.
pub fn driver_js(hex: &str) -> String {
    DRIVER_JS.replace("__HEX__", hex)
}

/// The Node driver: Lua's %.14g float formatting with the .0 suffix
/// rule, BigInt integers, and the len+bytes string layout.
const DRIVER_JS: &str = r#"let b0 = null;
let cur = null;
const lines = [];
const add = (s) => { cur = cur === null ? s : cur + '\t' + s; };
function g14(x) {
  if (Number.isNaN(x)) return 'nan';
  if (!isFinite(x)) return x > 0 ? 'inf' : '-inf';
  if (x === 0) return Object.is(x, -0) ? '-0' : '0';
  // %.14g with C's round-half-even: toExponential(14) yields the
  // exactly-rounded 15-digit form (ties to even, per spec), then we
  // round to 14 significant digits ourselves.
  let neg = x < 0;
  let parts = (neg ? -x : x).toExponential(14).split('e');
  let m = parts[0];
  let exp10 = parseInt(parts[1], 10);
  let digs = (m[0] + m.slice(2)).split('').map(Number);
  let roundUp;
  if (digs[14] > 5) roundUp = true;
  else if (digs[14] < 5) roundUp = false;
  else roundUp = digs[13] % 2 === 1; // exact tie: half-to-even
  if (roundUp) {
    let i = 13;
    while (i >= 0) {
      digs[i]++;
      if (digs[i] < 10) break;
      digs[i] = 0;
      i--;
    }
    if (i < 0) {
      digs.unshift(1);
      exp10 += 1;
    }
  }
  digs = digs.slice(0, 14); // 14 significant digits
  let end = digs.length;
  while (end > 1 && digs[end - 1] === 0) end--;
  digs = digs.slice(0, end);
  const mant = digs[0] + (digs.length > 1 ? '.' + digs.slice(1).join('') : '');
  let s;
  if (exp10 < -4 || exp10 >= 14) {
    const sign = exp10 < 0 ? '-' : '+';
    s = mant + 'e' + sign + String(Math.abs(exp10)).padStart(2, '0');
  } else if (exp10 >= 0) {
    if (digs.length > exp10 + 1) {
      s = digs.slice(0, exp10 + 1).join('') + '.' + digs.slice(exp10 + 1).join('');
    } else {
      s = digs.join('') + '0'.repeat(exp10 + 1 - digs.length);
    }
  } else {
    s = '0.' + '0'.repeat(-exp10 - 1) + digs.join('');
  }
  if (!/[.eEn]/.test(s)) s = s + '.0';
  return (neg ? '-' : '') + s;
}
WebAssembly.instantiate(
  Buffer.from('__HEX__', 'hex'),
  { env: {
      logi: (v) => add(v.toString()),
      logf: (v) => add(g14(v)),
      logb: (v) => add(v ? 'true' : 'false'),
      logstr: (p) => {
        const dv = new DataView(b0.exports.memory.buffer);
        const len = dv.getUint32(p, true);
        add(new TextDecoder().decode(new Uint8Array(b0.exports.memory.buffer, p + 8, len)));
      },
      logtab: () => {},
      logflush: () => { lines.push(cur === null ? '\n' : cur + '\n'); cur = null; },
      pow: (a, e) => Math.pow(a, e),
  } }
).then(({ instance }) => {
  b0 = instance;
  instance.exports.run();
  if (cur !== null) lines.push(cur + '\n');
  process.stdout.write(lines.join(''));
}).catch((e) => { console.error('wasm error: ' + e.message); process.exit(1); });
"#;

fn which_node() -> Option<std::path::PathBuf> {
    use std::process::Command;
    let ok = Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        Some(std::path::PathBuf::from("node"))
    } else {
        None
    }
}

/// Runs `source` on this interpreter and on a real `lua5.4`, then
/// compares the outputs byte-for-byte. The differential is the
/// semantics oracle: any disagreement is a linlua bug. Returns an
/// explanation when they disagree or `lua5.4` is not installed.
pub fn check_against_lua(source: &str) -> Result<(), String> {
    let lua = which_lua().ok_or("lua5.4 not available")?;
    let mut ours = Vec::new();
    run(source, &mut ours).map_err(|e| format!("linlua error: {}", e.message))?;
    let ours = String::from_utf8(ours).map_err(|e| e.to_string())?;

    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&lua)
        .arg("-") // read the program from stdin
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(source.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "lua5.4 errored: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let theirs = String::from_utf8(out.stdout).map_err(|e| e.to_string())?;
    if ours != theirs {
        return Err(format!(
            "outputs diverge:
-- linlua --
{ours}
-- lua5.4 --
{theirs}"
        ));
    }
    Ok(())
}

fn which_lua() -> Option<std::path::PathBuf> {
    use std::process::Command;
    for name in ["lua5.4", "lua"] {
        let ok = Command::new(name)
            .arg("-v")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(std::path::PathBuf::from(name));
        }
    }
    None
}
