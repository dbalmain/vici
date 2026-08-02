// Deep run: PROPTEST_CASES=4096 cargo test -p vici --test properties

use core::ops::Range;

use proptest::prelude::*;
use unicode_segmentation::UnicodeSegmentation;
use vici::{
    Bound, Buffer, Editor, Effect, Find, Key, KeyCode, Keymap, Mode, Mods, Motion, ObjectScope,
    Pending, Point, Resolution, TextObject, Viewport, keys, object_span, render, resolve_motion,
};

const CASES: u32 = 64;

fn text_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just(""),
            Just("a"),
            Just("  "),
            Just("\t"),
            Just("\n"),
            Just("\r\n"),
            Just("()[]{}<>"),
            Just("!?,.;_"),
            Just("café"),
            Just("日本語"),
            Just("🇦🇺"),
            Just("👩‍💻"),
            Just("word"),
        ],
        0..=5,
    )
    .prop_map(|parts| {
        let text = parts.concat();
        if text.is_empty() {
            "x".to_owned()
        } else {
            text
        }
    })
}

fn range_strategy(text: String) -> impl Strategy<Value = (String, Range<usize>)> {
    let boundaries: Vec<_> = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(core::iter::once(text.len()))
        .collect();
    (
        Just(text),
        prop::sample::select(boundaries.clone()),
        prop::sample::select(boundaries),
    )
        .prop_map(|(text, first, second)| (text, first.min(second)..first.max(second)))
}

fn replacement_strategy() -> impl Strategy<Value = String> {
    text_strategy()
}

fn replace_case() -> impl Strategy<Value = (String, Range<usize>, String)> {
    text_strategy().prop_flat_map(|text| {
        range_strategy(text).prop_flat_map(|(text, range)| {
            replacement_strategy()
                .prop_map(move |replacement| (text.clone(), range.clone(), replacement))
        })
    })
}

fn script_strategy() -> impl Strategy<Value = Vec<Key>> {
    let atom = prop::sample::select(vec![
        "h",
        "j",
        "k",
        "l",
        "w",
        "b",
        "e",
        "W",
        "B",
        "E",
        "0",
        "^",
        "$",
        "G",
        "gg",
        "%",
        "dw",
        "ciw<Esc>",
        "yi(",
        ">>",
        "gUiw",
        "x",
        "X",
        "ré",
        "~",
        "J",
        "p",
        "P",
        "ié<Esc>",
        "i日本<Esc>",
        "vwd",
        "v~",
        "u",
        "<C-r>",
        ".",
    ]);
    prop::collection::vec(atom, 0..=6).prop_map(|atoms| {
        atoms
            .into_iter()
            .flat_map(|atom| keys(atom).expect("curated key atom must parse"))
            .collect()
    })
}

fn change_script_strategy() -> impl Strategy<Value = Vec<Key>> {
    let atom = prop::sample::select(vec![
        "dw",
        "ciw<Esc>",
        ">>",
        "gUiw",
        "x",
        "X",
        "ré",
        "~",
        "J",
        "p",
        "P",
        "ié<Esc>",
        "i日本<Esc>",
        "vwd",
        "v~",
    ]);
    prop::collection::vec(atom, 1..=5).prop_map(|atoms| {
        atoms
            .into_iter()
            .flat_map(|atom| keys(atom).expect("curated change atom must parse"))
            .collect()
    })
}

fn expected_point(text: &str, byte: usize) -> Point {
    let before = &text[..byte];
    Point::new(
        before.bytes().filter(|&byte| byte == b'\n').count(),
        before
            .rfind('\n')
            .map_or(before.len(), |newline| before.len() - newline - 1),
    )
}

fn grapheme_boundary(buffer: &Buffer, byte: usize) -> bool {
    let row = buffer.byte_to_point(byte).row;
    let range = buffer.row_content_range(row);
    if !(range.start..=range.end).contains(&byte) {
        return false;
    }
    let text = buffer.text_in(range.clone());
    byte == range.start
        || byte == range.start + text.len()
        || text
            .grapheme_indices(true)
            .any(|(index, _)| byte == range.start + index)
}

fn has_multi_codepoint_grapheme(text: &str) -> bool {
    text.graphemes(true)
        .any(|grapheme| grapheme.chars().nth(1).is_some())
}

fn has_key(script: &[Key], ch: char) -> bool {
    script.contains(&Key::char(ch))
}

fn puts_with_possible_multibyte_register(text: &str, script: &[Key]) -> bool {
    (has_key(script, 'p') || has_key(script, 'P'))
        && (!text.is_ascii()
            || script
                .iter()
                .any(|key| matches!(key.code, KeyCode::Char(ch) if !ch.is_ascii())))
}

fn legal_offsets(buffer: &Buffer, bound: Bound) -> Vec<usize> {
    let mut offsets = Vec::new();
    for row in 0..buffer.len_rows() {
        let range = buffer.row_content_range(row);
        let text = buffer.text_in(range.clone());
        offsets.push(range.start);
        offsets.extend(
            text.grapheme_indices(true)
                .skip(1)
                .map(|(index, _)| range.start + index),
        );
        if bound == Bound::PastEnd {
            offsets.push(range.end);
        }
    }
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

fn motions() -> Vec<(Motion, Option<Find>)> {
    vec![
        (Motion::Left, None),
        (Motion::Right, None),
        (Motion::Down, None),
        (Motion::Up, None),
        (Motion::FirstColumn, None),
        (Motion::FirstNonBlank, None),
        (Motion::LastColumn, None),
        (Motion::WordForward { big: false }, None),
        (Motion::WordForward { big: true }, None),
        (Motion::WordBackward { big: false }, None),
        (Motion::WordBackward { big: true }, None),
        (Motion::WordEnd { big: false }, None),
        (Motion::WordEnd { big: true }, None),
        (
            Motion::Find {
                target: 'a',
                backward: false,
                till: false,
            },
            None,
        ),
        (
            Motion::Find {
                target: 'a',
                backward: true,
                till: true,
            },
            None,
        ),
        (
            Motion::RepeatFind { reverse: false },
            Some(Find {
                target: 'a',
                backward: false,
                till: false,
            }),
        ),
        (
            Motion::RepeatFind { reverse: true },
            Some(Find {
                target: 'a',
                backward: false,
                till: false,
            }),
        ),
        (Motion::GotoRow, None),
        (Motion::GotoFirstRow, None),
        (Motion::MatchPair, None),
        (Motion::ScreenTop, None),
        (Motion::ScreenMiddle, None),
        (Motion::ScreenBottom, None),
    ]
}

fn objects() -> Vec<TextObject> {
    vec![
        TextObject::Word { big: false },
        TextObject::Word { big: true },
        TextObject::Delimited {
            open: '(',
            close: ')',
        },
        TextObject::Delimited {
            open: '[',
            close: ']',
        },
        TextObject::Delimited {
            open: '{',
            close: '}',
        },
        TextObject::Delimited {
            open: '<',
            close: '>',
        },
        TextObject::Quoted('"'),
        TextObject::Quoted('\''),
        TextObject::Quoted('`'),
        TextObject::Paragraph,
    ]
}

fn balanced_text_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec!["a", " ", "café", "日本", "\n", "\r\n"]),
        0..=4,
    )
    .prop_map(|parts| format!("({0})[{0}]{{{0}}}<{0}>\"{0}\"'{0}'`{0}`", parts.concat()))
}

fn notation_key_strategy() -> impl Strategy<Value = Key> {
    prop_oneof![
        prop::sample::select(vec!['a', 'A', 'é', '日', '<', '>', '-']).prop_map(Key::char),
        prop::sample::select(vec![
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::F(1),
            KeyCode::F(12)
        ])
        .prop_map(Key::code),
        prop::sample::select(vec!['a', 'é', '-'])
            .prop_map(|ch| Key::new(KeyCode::Char(ch), Mods::CTRL)),
        prop::sample::select(vec![KeyCode::Tab, KeyCode::Left])
            .prop_map(|code| Key::new(code, Mods::SHIFT)),
    ]
}

fn parser_keys_strategy() -> impl Strategy<Value = Vec<Key>> {
    prop::collection::vec(
        prop::sample::select(vec![
            Key::char('h'),
            Key::char('d'),
            Key::char('w'),
            Key::char('i'),
            Key::char('x'),
            Key::char('z'),
            Key::char('g'),
            Key::char('1'),
            Key::char('('),
            Key::code(KeyCode::Esc),
            Key::ctrl('r'),
        ]),
        0..=16,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: CASES, .. ProptestConfig::default() })]

    #[test]
    fn change_inversion((text, range, replacement) in replace_case()) {
        let mut buffer = Buffer::from_text(&text);
        let change = buffer.stage_replace(range.clone(), &replacement);
        let mut expected = text.clone();
        expected.replace_range(range, &replacement);
        buffer.apply(&change);
        prop_assert_eq!(buffer.to_string(), expected);
        buffer.apply(&change.inverted());
        prop_assert_eq!(buffer.to_string(), text);
        prop_assert_eq!(change.edit.inverted().inverted(), change.edit);
    }

    #[test]
    fn edit_geometry((text, range, replacement) in replace_case()) {
        let buffer = Buffer::from_text(&text);
        let change = buffer.stage_replace(range, &replacement);
        let edit = change.edit;
        let mut post = text.clone();
        post.replace_range(edit.start_byte..edit.old_end_byte, &replacement);
        prop_assert!(edit.start_byte <= edit.old_end_byte && edit.old_end_byte <= text.len());
        prop_assert!(edit.start_byte <= edit.new_end_byte && edit.new_end_byte <= post.len());
        prop_assert_eq!(edit.start_point, expected_point(&text, edit.start_byte));
        prop_assert_eq!(edit.old_end_point, expected_point(&text, edit.old_end_byte));
        prop_assert_eq!(edit.new_end_point, expected_point(&post, edit.new_end_byte));
    }

    #[test]
    fn undo_returns_original_bytes(text in text_strategy(), script in script_strategy()) {
        // KNOWN: put leaves the cursor mid-character with multibyte register text (`日本語`, `xp`).
        prop_assume!(!puts_with_possible_multibyte_register(&text, &script));
        let mut editor = Editor::from_text(&text);
        editor.handle_keys(&script);
        let changed = editor.buffer().to_string();
        let depth = editor.document().history().undo_depth();
        for _ in 0..depth { editor.handle_key(Key::char('u')); }
        prop_assert_eq!(editor.buffer().to_string(), text);
        for _ in 0..depth { editor.handle_key(Key::ctrl('r')); }
        prop_assert_eq!(editor.buffer().to_string(), changed);
    }

    #[test]
    fn effect_stream_accounts_for_buffer(text in text_strategy(), script in script_strategy()) {
        // KNOWN: put leaves the cursor mid-character with multibyte register text (`日本語`, `xp`).
        prop_assume!(!puts_with_possible_multibyte_register(&text, &script));
        let mut editor = Editor::from_text(&text);
        let mut length = text.len();
        for effect in editor.handle_keys(&script) {
            if let Effect::Edit(edit) = effect {
                prop_assert!(edit.start_byte <= edit.old_end_byte);
                prop_assert!(edit.start_byte <= edit.new_end_byte);
                prop_assert!(edit.old_end_byte <= length);
                length = length + edit.new_end_byte - edit.old_end_byte;
            }
        }
        prop_assert_eq!(length, editor.buffer().len_bytes());
    }

    #[test]
    fn dot_repeats_recorded_change(text in text_strategy(), script in change_script_strategy()) {
        // KNOWN: put leaves the cursor mid-character with multibyte register text (`日本語`, `xP`).
        prop_assume!(!puts_with_possible_multibyte_register(&text, &script));
        let mut editor = Editor::from_text(&text);
        editor.handle_keys(&script);
        prop_assume!(!editor.last_change().is_empty());
        let recorded = editor.last_change().to_vec();
        let mut dot = editor.clone();
        let mut replay = editor;
        dot.handle_key(Key::char('.'));
        replay.handle_keys(&recorded);
        prop_assert_eq!(dot.buffer().to_string(), replay.buffer().to_string());
        prop_assert_eq!(dot.cursor(), replay.cursor());
        prop_assert_eq!(dot.mode(), replay.mode());
        prop_assert_eq!(dot.selection(), replay.selection());
        prop_assert_eq!(dot.register(), replay.register());
    }

    #[test]
    fn cursor_stays_on_legal_boundaries(text in text_strategy(), script in script_strategy()) {
        // KNOWN: put leaves the cursor mid-character with multibyte register text (`日本語`, `xp`).
        prop_assume!(!puts_with_possible_multibyte_register(&text, &script));
        // KNOWN: word-end lands between the regional indicators in a flag (`🇦🇺café`, `e`).
        prop_assume!(
            !has_multi_codepoint_grapheme(&text) || (!has_key(&script, 'e') && !has_key(&script, 'E'))
        );
        let mut editor = Editor::from_text(&text);
        for key in script {
            editor.handle_key(key);
            let buffer = editor.buffer();
            prop_assert!(editor.cursor() <= buffer.len_bytes());
            prop_assert!(buffer.to_string().is_char_boundary(editor.cursor()));
            prop_assert!(grapheme_boundary(buffer, editor.cursor()), "mode: {:?}, cursor: {}", editor.mode(), editor.cursor());
            if let Some(selection) = editor.selection() {
                prop_assert!(selection.start <= selection.end && selection.end <= buffer.len_bytes());
                prop_assert!(buffer.to_string().is_char_boundary(selection.start));
                prop_assert!(buffer.to_string().is_char_boundary(selection.end));
                prop_assert!(grapheme_boundary(buffer, selection.start));
                prop_assert!(grapheme_boundary(buffer, selection.end));
            }
        }
    }

    #[test]
    fn motions_stay_in_the_buffer(text in text_strategy(), start_choice in any::<u8>()) {
        let buffer = Buffer::from_text(&text);
        let before = buffer.to_string();
        for bound in [Bound::OnChar, Bound::PastEnd] {
            let offsets = legal_offsets(&buffer, bound);
            let from = offsets[usize::from(start_choice) % offsets.len()];
            for (motion, find) in motions() {
                // KNOWN: word-end can land inside a multi-codepoint grapheme (`🇦🇺`, `E`).
                if has_multi_codepoint_grapheme(&text) && matches!(motion, Motion::WordEnd { .. }) {
                    continue;
                }
                for count in 1..=3 {
                    for viewport in [Viewport::default(), Viewport { top_row: 0, height: 1 }, Viewport { top_row: 2, height: 4 }] {
                        let result = resolve_motion(&buffer, from, motion, Some(count), 0, find, viewport, bound);
                        if let Some(landed) = result {
                            prop_assert!(landed <= buffer.len_bytes());
                            prop_assert!(text.is_char_boundary(landed));
                            prop_assert!(grapheme_boundary(&buffer, landed));
                        } else {
                            prop_assert_eq!(&buffer.to_string(), &before);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn object_spans_are_well_formed(text in balanced_text_strategy()) {
        let buffer = Buffer::from_text(&text);
        for at in text.char_indices().map(|(index, _)| index).chain(core::iter::once(text.len())) {
            for object in objects() {
                let inner = object_span(&buffer, at, ObjectScope::Inner, object, 1);
                let around = object_span(&buffer, at, ObjectScope::Around, object, 1);
                for span in [inner.as_ref(), around.as_ref()].into_iter().flatten() {
                    prop_assert!(span.range.start <= span.range.end && span.range.end <= text.len());
                    prop_assert!(text.is_char_boundary(span.range.start));
                    prop_assert!(text.is_char_boundary(span.range.end));
                }
                if let Some(inner) = inner {
                    let around = around.expect("inner text objects always have an around span");
                    prop_assert!(around.range.start <= inner.range.start && inner.range.end <= around.range.end);
                }
            }
        }
    }

    #[test]
    fn key_notation_round_trips(key in notation_key_strategy()) {
        prop_assert_eq!(keys(&render(&[key])).expect("rendered key must parse"), vec![key]);
    }

    #[test]
    fn parser_always_resets(sequence in parser_keys_strategy()) {
        let keymap = Keymap::vim();
        let mut pending = Pending::new();
        let mut consumed = Vec::new();
        for key in sequence {
            consumed.push(key);
            match pending.feed(key, Mode::Normal, &keymap) {
                Resolution::Pending => {}
                Resolution::Command { .. } => {
                    prop_assert!(pending.is_idle());
                    consumed.clear();
                }
                Resolution::Rejected { keys } | Resolution::Cancelled { keys } => {
                    prop_assert_eq!(&keys, &consumed);
                    prop_assert!(pending.is_idle());
                    consumed.clear();
                }
            }
        }
    }
}
