//! The `linlua` command-line tool: `linlua run program.lua`.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((cmd, rest)) if cmd == "check" => match rest.first() {
            Some(path) => check_file(path),
            _ => {
                eprintln!("usage: linlua check <file.lua>");
                ExitCode::from(2)
            }
        },
        Some((cmd, rest)) if cmd == "run" => {
            let no_infer = rest.iter().any(|a| a == "--no-infer");
            match rest.iter().find(|a| !a.starts_with('-')) {
                Some(path) => run_file(path, no_infer),
                _ => {
                    eprintln!("usage: linlua run [--no-infer] <file.lua>");
                    ExitCode::from(2)
                }
            }
        }
        Some((cmd, _)) => {
            eprintln!("unknown command `{cmd}` (v1 knows `run`)");
            ExitCode::from(2)
        }
        None => {
            println!(
                "linlua {} — the gradually-typed Lua subset

USAGE:
    linlua run <file.lua>",
                env!("CARGO_PKG_VERSION")
            );
            ExitCode::SUCCESS
        }
    }
}

/// `linlua check`: parse + type diagnostics, exit 1 on anything
/// wrong — the editor and CI entry point.
fn check_file(path: &str) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("linlua: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let chunk = match linlua::parse_chunk(&source) {
        Ok(c) => c,
        Err(e) => {
            let (line, col) = line_col(&source, e.offset);
            println!("{path}:{line}:{col}: parse error: {}", e.message);
            return ExitCode::FAILURE;
        }
    };
    let errors = linlua::check::check_program(&chunk);
    if errors.is_empty() {
        println!("{path}: ok");
        ExitCode::SUCCESS
    } else {
        for e in errors {
            println!("{path}: {}", e.render());
        }
        ExitCode::FAILURE
    }
}

fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
    let col = source[..offset]
        .rfind('\n')
        .map(|p| offset - p)
        .unwrap_or(offset + 1);
    (line, col)
}

fn run_file(path: &str, no_infer: bool) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("linlua: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    // Inference is output-invisible (the differential suite pins
    // this), so `run` infers by default; --no-infer keeps
    // annotations-only semantics for debugging the inferrer.
    let result = if no_infer {
        linlua::run(&source, &mut lock)
    } else {
        linlua::run_inferred(&source, &mut lock).map(|_| ())
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(lock);
            eprintln!("lua: {path}: {}", e.message);
            ExitCode::FAILURE
        }
    }
}
