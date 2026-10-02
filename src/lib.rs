//! linlua: a Luau-flavored Lua subset with gradual types and gradual
//! linear memory, on the linjs engine architecture.
//!
//! M1: lexer, parser, and a tree-walking interpreter with Lua 5.4
//! semantics — integer/float subtyping, floored division, insertion-
//! ordered tables, and closures — differential-tested against a real
//! `lua5.4`.

pub mod ast;
pub mod infer;
pub mod interp;
pub mod lexer;
pub mod mem;
pub mod parser;
pub mod value;

pub use ast::{BinOp, Chunk, Expr, Stmt, Target, UnOp};
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
