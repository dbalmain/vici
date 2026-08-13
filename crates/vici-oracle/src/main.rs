//! Drive the Rust editor the same way `editor_cases` does.
//!
//! The TypeScript engine and the alignment fuzzer treat this binary as the
//! oracle: same `text` + `keys` in, same snapshot block out. On a mismatch the
//! fuzzer prints a new `editor.vici` case — the fixture file is the product.

use std::env;
use std::io::{self, Read};
use std::process::ExitCode;

use vici::fixtures::{Case, Settings, format_case, parse_settings, run_case};

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
    if args.iter().any(|arg| arg == "--fixture") {
        return emit_case_grammar(&args);
    }
    let name = flag(&args, "--name").unwrap_or_else(|| "case".to_owned());
    let text = text_arg(&args)?;
    let keys = flag(&args, "--keys").ok_or("missing --keys")?;
    let mut settings = Settings::default();
    if let Some(with) = flag(&args, "--with") {
        settings = parse_settings(&with, &name);
    }
    let case = Case {
        name,
        text,
        keys,
        settings,
    };
    let snapshot = run_case(&case).map_err(|error| error.to_string())?;
    print!("{snapshot}");
    Ok(())
}

/// `--fixture` reprints the case in editor.vici grammar (for fuzzer dumps).
fn emit_case_grammar(args: &[String]) -> Result<(), String> {
    let name = flag(args, "--name").unwrap_or_else(|| "case".to_owned());
    let text = text_arg(args)?;
    let keys = flag(args, "--keys").ok_or("missing --keys")?;
    let mut settings = Settings::default();
    if let Some(with) = flag(args, "--with") {
        settings = parse_settings(&with, &name);
    }
    print!(
        "{}",
        format_case(&Case {
            name,
            text,
            keys,
            settings,
        })
    );
    Ok(())
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
Usage: vici-oracle --text <buf> --keys <script> [--name <id>] [--with <settings>]
       vici-oracle --fixture --text <buf> --keys <script> [--name <id>]

Prints one editor.vici snapshot block (the rust oracle).
--fixture prints the input back as a pasteable case instead.

Stdin is ignored unless --text=- , in which case the buffer is read from stdin."
    );
}

fn read_stdin() -> io::Result<String> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}
