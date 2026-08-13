//! Drive the Rust editor the same way `editor_cases` does.
//!
//! The JavaScript engine and the differential fuzzer treat this binary as the
//! oracle: same `text` + `keys` in, same snapshot block out. Because the format
//! is the one `crates/vici/tests/fixtures/editor.vici` already uses, a case
//! that diverges can be pasted straight into the fixture file and becomes a
//! permanent regression test for both engines.
//!
//! ```sh
//! cargo run -q -p vici-oracle -- cases.vici          # a whole file, in order
//! cargo run -q -p vici-oracle -- --text 'one two' --keys dw
//! ```
//!
//! The file form matters for fuzzing: thousands of cases cost one process
//! rather than thousands.

use std::env;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::process::ExitCode;

use vici::fixtures::{Case, Settings, parse_cases, parse_settings, run_case};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vici-oracle: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }
    let out = match path_arg(&args) {
        Some(path) => replay(&path)?,
        None => one_case(&args)?,
    };
    print!("{out}");
    Ok(())
}

/// A fixture file, either positional or behind `--replay`.
fn path_arg(args: &[String]) -> Option<String> {
    flag(args, "--replay").or_else(|| {
        args.first()
            .filter(|arg| !arg.starts_with('-'))
            .map(ToOwned::to_owned)
    })
}

fn replay(path: &str) -> Result<String, String> {
    let fixture = std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
    let mut out = String::new();
    for case in parse_cases(&fixture) {
        match run_case(&case) {
            Ok(block) => out.push_str(&block),
            // A generator that emits unparseable notation should hear about it
            // in the diff rather than in a failed process.
            Err(error) => {
                let _ = writeln!(out, "== {} ==\ninvalid keys: {error}\n", case.name);
            }
        }
    }
    Ok(out)
}

fn one_case(args: &[String]) -> Result<String, String> {
    let name = flag(args, "--name").unwrap_or_else(|| "case".to_owned());
    let text = text_arg(args)?;
    let keys = flag(args, "--keys").ok_or("missing --keys")?;
    let settings = match flag(args, "--with") {
        Some(with) => parse_settings(&with, &name),
        None => Settings::default(),
    };
    let case = Case {
        name,
        text,
        keys,
        settings,
    };
    run_case(&case).map_err(|error| error.to_string())
}

fn text_arg(args: &[String]) -> Result<String, String> {
    match flag(args, "--text").ok_or("missing --text")?.as_str() {
        "-" => read_stdin().map_err(|error| error.to_string()),
        other => Ok(other.to_owned()),
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        if arg == name {
            return items.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

fn print_help() {
    eprintln!(
        "\
Usage: vici-oracle <fixture.vici>
       vici-oracle --text <buf> --keys <script> [--name <id>] [--with <settings>]

Prints editor.vici snapshot blocks: one per case for a fixture file, or one for
a single buffer and key script.

Stdin is ignored unless --text=- , in which case the buffer is read from stdin."
    );
}

fn read_stdin() -> io::Result<String> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}
