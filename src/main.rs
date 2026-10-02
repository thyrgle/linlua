//! The `linlua` command-line tool: `linlua run program.lua`.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((cmd, rest)) if cmd == "run" => match rest.first() {
            Some(path) => run_file(path),
            _ => {
                eprintln!("usage: linlua run <file.lua>");
                ExitCode::from(2)
            }
        },
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

fn run_file(path: &str) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("linlua: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match linlua::run(&source, &mut lock) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(lock);
            eprintln!("lua: {path}: {}", e.message);
            ExitCode::FAILURE
        }
    }
}
