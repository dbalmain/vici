//! Replay a fixture file and print each case's state block.
//!
//! This is the Rust side of the differential harness: another implementation
//! generates cases in `editor.vici` format, runs them itself, and diffs its
//! blocks against these. Because the format is the one `tests/editor_cases.rs`
//! already uses, any case that diverges can be pasted straight into
//! `tests/fixtures/editor.vici` and becomes a permanent regression test for
//! both engines.
//!
//! ```sh
//! cargo run -q --example replay -- cases.vici
//! ```

use std::fmt::Write as _;

use vici::{Editor, Effect, Indent, Viewport, render};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: replay <fixture.vici>");
        std::process::exit(2);
    });
    let fixture = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        eprintln!("{path}: {error}");
        std::process::exit(2);
    });

    let mut out = String::new();
    for case in parse(&fixture) {
        let mut editor = Editor::from_text(&case.text);
        if let Some(viewport) = case.viewport {
            editor.set_viewport(viewport);
        }
        if let Some(indent) = case.indent {
            editor.set_indent(indent);
        }
        match editor.type_keys(&case.keys) {
            Ok(effects) => render_case(&mut out, &case.name, &editor, &effects),
            // A generator that emits unparseable notation should hear about it
            // in the diff rather than in a panic.
            Err(error) => {
                let _ = writeln!(out, "== {} ==\ninvalid keys: {error}\n", case.name);
            }
        }
    }
    print!("{out}");
}

struct Case {
    name: String,
    text: String,
    keys: String,
    viewport: Option<Viewport>,
    indent: Option<Indent>,
}

fn parse(fixture: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    for chunk in fixture.split("\n---\n") {
        let lines: Vec<_> = chunk
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        let Some(name) = lines.first().and_then(|line| line.strip_prefix("case ")) else {
            continue;
        };
        let mut case = Case {
            name: name.to_owned(),
            text: String::new(),
            keys: String::new(),
            viewport: None,
            indent: None,
        };
        for line in &lines[1..] {
            if let Some(value) = line.strip_prefix("text") {
                case.text = unescape(value.strip_prefix(' ').unwrap_or_default());
            } else if let Some(value) = line.strip_prefix("keys ") {
                value.clone_into(&mut case.keys);
            } else if let Some(value) = line.strip_prefix("with ") {
                settings(value, &mut case);
            }
        }
        cases.push(case);
    }
    cases
}

fn unescape(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

fn settings(value: &str, case: &mut Case) {
    for setting in value.split_whitespace() {
        let Some((key, values)) = setting.split_once('=') else {
            continue;
        };
        let parts: Vec<_> = values.split(',').collect();
        match (key, parts.as_slice()) {
            ("viewport", [top_row, height]) => {
                case.viewport = Some(Viewport {
                    top_row: top_row.parse().unwrap_or(0),
                    height: height.parse().unwrap_or(0),
                });
            }
            ("indent", [shift_width, tab_width, kind]) => {
                case.indent = Some(Indent {
                    shift_width: shift_width.parse().unwrap_or(4),
                    tab_width: tab_width.parse().unwrap_or(8),
                    use_tabs: *kind == "tabs",
                });
            }
            _ => {}
        }
    }
}

/// Byte for byte the block `tests/editor_cases.rs` writes.
fn render_case(snapshot: &mut String, name: &str, editor: &Editor, effects: &[Effect]) {
    let selection = editor.selection().map_or_else(
        || "-".to_owned(),
        |range| format!("{}..{}", range.start, range.end),
    );
    let register_kind = if editor.register().linewise {
        "line"
    } else {
        "char"
    };
    let recording = editor
        .recording()
        .map_or_else(|| "-".to_owned(), |name| name.to_string());
    let point = editor.cursor_point();
    let history = editor.document().history();
    let marks: Vec<_> = ('a'..='z')
        .chain(['<', '>', '[', ']', '^'])
        .filter_map(|name| editor.mark(name).map(|offset| format!("{name}:{offset}")))
        .collect();
    let marks = if marks.is_empty() {
        "[]".to_owned()
    } else {
        format!("[{}]", marks.join(", "))
    };
    write!(
        snapshot,
        "== {name} ==\n\
         text: {text:?}\n\
         cursor: {cursor} @ {row}:{col}\n\
         mode: {mode:?}; selection: {selection}\n\
         register: {register_kind} {register:?}\n\
         history: undo={undo} redo={redo}\n\
         jumps: {jumps:?}\n\
         marks: {marks}\n\
         pending: {pending:?}; last-change: {last_change:?}; recording: {recording}\n\
         effects:\n",
        text = editor.buffer().to_string(),
        cursor = editor.cursor(),
        row = point.row,
        col = point.col,
        mode = editor.mode(),
        register = editor.register().text,
        undo = history.undo_depth(),
        redo = history.redo_depth(),
        jumps = editor.jumps(),
        pending = render(editor.pending_keys()),
        last_change = render(editor.last_change()),
    )
    .expect("writing to a String cannot fail");
    for effect in effects {
        snapshot.push_str("  ");
        snapshot.push_str(&render_effect(effect));
        snapshot.push('\n');
    }
    snapshot.push('\n');
}

fn render_effect(effect: &Effect) -> String {
    match effect {
        Effect::Edit(edit) => format!(
            "edit {}..{} -> {}; ({},{})..({},{}) -> ({},{})",
            edit.start_byte,
            edit.old_end_byte,
            edit.new_end_byte,
            edit.start_point.row,
            edit.start_point.col,
            edit.old_end_point.row,
            edit.old_end_point.col,
            edit.new_end_point.row,
            edit.new_end_point.col
        ),
        Effect::ModeChanged(mode) => format!("mode {mode:?}"),
        Effect::Scroll(scroll) => format!("scroll {scroll:?}"),
        Effect::CommandPrompt => "command prompt :".to_owned(),
        Effect::Bell => "bell".to_owned(),
        Effect::RecordingStarted(register) => format!("recording @{register}"),
        Effect::RecordingStopped(register) => format!("recorded @{register}"),
    }
}
