// Regenerate deliberately with: INSTA_UPDATE=always cargo test -p vici --test editor_cases

use std::collections::BTreeSet;

use vici::{Editor, Effect, Indent, Viewport, render};

#[derive(Default)]
struct Settings {
    viewport: Option<Viewport>,
    indent: Option<Indent>,
}

struct Case {
    name: String,
    text: String,
    keys: String,
    settings: Settings,
}

#[test]
fn editor_cases() {
    let mut snapshot = String::new();
    for case in parse_cases(include_str!("fixtures/editor.vici")) {
        let mut editor = Editor::from_text(&case.text);
        if let Some(viewport) = case.settings.viewport {
            editor.set_viewport(viewport);
        }
        if let Some(indent) = case.settings.indent {
            editor.set_indent(indent);
        }
        let effects = editor
            .type_keys(&case.keys)
            .unwrap_or_else(|error| panic!("{}: invalid keys: {error}", case.name));
        render_case(&mut snapshot, &case.name, &editor, &effects);
    }
    insta::assert_snapshot!(snapshot);
}

fn parse_cases(fixture: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut names = BTreeSet::new();
    for chunk in fixture.split("\n---\n") {
        let lines: Vec<_> = chunk
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        if lines.is_empty() {
            continue;
        }
        let name = lines[0]
            .strip_prefix("case ")
            .filter(|name| valid_name(name))
            .unwrap_or_else(|| panic!("<unknown>: case must be first and kebab-case"));
        let mut text = None;
        let mut keys = None;
        let mut settings = None;
        for line in &lines[1..] {
            if let Some(value) = line.strip_prefix("text") {
                assert!(
                    value.is_empty() || value.starts_with(' '),
                    "{name}: malformed text"
                );
                assert!(text.is_none(), "{name}: duplicate text");
                text = Some(unescape(value.strip_prefix(' ').unwrap_or_default(), name));
            } else if let Some(value) = line.strip_prefix("keys ") {
                assert!(keys.is_none(), "{name}: duplicate keys");
                keys = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("with ") {
                assert!(settings.is_none(), "{name}: duplicate with");
                settings = Some(parse_settings(value, name));
            } else {
                panic!("{name}: unknown fixture prefix: {line}");
            }
        }
        assert!(text.is_some(), "{name}: missing text");
        assert!(keys.is_some(), "{name}: missing keys");
        assert!(names.insert(name), "{name}: duplicate case name");
        cases.push(Case {
            name: name.to_owned(),
            text: text.unwrap_or_default(),
            keys: keys.unwrap_or_default(),
            settings: settings.unwrap_or_default(),
        });
    }
    assert!(!cases.is_empty(), "<unknown>: fixture has no cases");
    cases
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn unescape(value: &str, name: &str) -> String {
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
            Some('\\') => out.push('\\'),
            Some(other) => panic!("{name}: unsupported escape \\{other}"),
            None => panic!("{name}: unsupported trailing backslash"),
        }
    }
    out
}

/// Settings are space-separated, their values comma-separated:
/// `with viewport=0,6 indent=4,8,spaces`.
fn parse_settings(value: &str, name: &str) -> Settings {
    let mut settings = Settings::default();
    for setting in value.split_whitespace() {
        let (key, values) = setting
            .split_once('=')
            .unwrap_or_else(|| panic!("{name}: setting needs a value: {setting}"));
        match (key, values.split(',').collect::<Vec<_>>().as_slice()) {
            ("viewport", [top_row, height]) => {
                assert!(settings.viewport.is_none(), "{name}: duplicate viewport");
                settings.viewport = Some(Viewport {
                    top_row: number(top_row, name),
                    height: number(height, name),
                });
            }
            ("indent", [shift_width, tab_width, kind]) => {
                assert!(settings.indent.is_none(), "{name}: duplicate indent");
                settings.indent = Some(Indent {
                    shift_width: number(shift_width, name),
                    tab_width: number(tab_width, name),
                    use_tabs: match *kind {
                        "tabs" => true,
                        "spaces" => false,
                        other => panic!("{name}: indent wants tabs or spaces, got {other}"),
                    },
                });
            }
            _ => panic!("{name}: unsupported setting {setting}"),
        }
    }
    settings
}

fn number(value: &str, name: &str) -> usize {
    value
        .parse()
        .unwrap_or_else(|_| panic!("{name}: invalid setting number {value}"))
}

fn render_case(snapshot: &mut String, name: &str, editor: &Editor, effects: &[Effect]) {
    use std::fmt::Write;

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
    write!(
        snapshot,
        "== {name} ==\n\
         text: {text:?}\n\
         cursor: {cursor} @ {row}:{col}\n\
         mode: {mode:?}; selection: {selection}\n\
         register: {register_kind} {register:?}\n\
         history: undo={undo} redo={redo}\n\
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
        Effect::SearchPrompt { backward } => {
            format!("search prompt {}", if *backward { '?' } else { '/' })
        }
        Effect::SearchRepeat { reverse } => {
            format!("search repeat {}", if *reverse { 'N' } else { 'n' })
        }
        Effect::CommandPrompt => "command prompt :".to_owned(),
        Effect::Bell => "bell".to_owned(),
        Effect::RecordingStarted(register) => format!("recording @{register}"),
        Effect::RecordingStopped(register) => format!("recorded @{register}"),
    }
}
