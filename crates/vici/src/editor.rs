//! The reducer: `(state, Key) -> (state, Vec<Effect>)`.
//!
//! This is the only stateful type in the crate. It owns the cursor, the mode, the
//! pending parser, register and jump list, and it drives [`Document`] and
//! [`motion`].
//!
//! # Why the reducer shape earns its keep
//!
//! Because a keystroke is the unit of input and [`Resolution`] hands back the keys
//! it consumed, three features collapse into one mechanism:
//!
//! - **dot-repeat** stores the keys of the last change and re-feeds them,
//! - **macros** store the keys between `q{reg}` and `q` and re-feed them,
//! - **tests** are keystroke scripts, which is why every test below reads like
//!   something you would actually type.
//!
//! Replaying keys rather than re-executing commands is what makes `.` correct for
//! `ciwfoo<Esc>` without any special handling of the typed text.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::buffer::Buffer;
use crate::command::{Command, InsertAt, Mode, Motion, Operator, Scroll, Target, VisualKind};
use crate::document::Document;
use crate::edit::{Edit, Point};
use crate::history::Step;
use crate::host::{Indent, Viewport};
use crate::key::{Key, ParseError, keys};
use crate::keymap::Keymap;
use crate::motion::{self, Bound, Find, STICKY_END, Span};
use crate::pending::{Pending, Resolution};

/// How deep `.` and `@` may nest before the editor refuses, so a macro that plays
/// itself terminates instead of overflowing the stack.
const MAX_REPLAY_DEPTH: usize = 64;
/// Oldest jump entries are discarded once the list reaches this size.
const MAX_JUMPS: usize = 100;
/// Automatic marks occupy the slots after the lowercase user marks.
const AUTO_MARKS: [char; 5] = ['<', '>', '[', ']', '^'];
const MARK_COUNT: usize = 26 + AUTO_MARKS.len();

/// Something the host must act on.
///
/// Everything else — cursor position, mode, selection — is queryable, so it is not
/// duplicated here. These are the things the core cannot do itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Feed to tree-sitter, an LSP, a linter. One per keystroke while typing.
    Edit(Edit),
    ModeChanged(Mode),
    /// The core does not own the viewport.
    Scroll(Scroll),
    /// `:`
    CommandPrompt,
    /// Input was not a valid sequence.
    Bell,
    RecordingStarted(char),
    RecordingStopped(char),
}

/// The unnamed register.
///
/// `linewise` decides whether `p` pastes onto a new row or inline — the same text
/// behaves differently depending on how it was yanked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Register {
    pub text: String,
    pub linewise: bool,
}

/// A vi-like editor over a [`Document`].
#[derive(Debug, Clone)]
pub struct Editor {
    doc: Document,
    keymap: Keymap,
    indent: Indent,
    viewport: Viewport,
    pending: Pending,
    mode: Mode,
    cursor: usize,
    /// Remembered column for `j`/`k`. [`STICKY_END`] means "row end".
    sticky: usize,
    /// Visual-mode anchor; the other end of the selection.
    anchor: Option<usize>,
    register: Register,
    /// Places left by far motions and host-side jumps.
    jumps: Vec<usize>,
    /// An entry being visited, or `jumps.len()` when the caret is at the present.
    jump_at: usize,
    /// Named and automatic positions, shifted with jumps through every edit.
    marks: [Option<usize>; MARK_COUNT],
    last_find: Option<Find>,
    last_search: Option<(String, bool)>,
    /// Keys of the last buffer-changing command, for `.`.
    last_change: Vec<Key>,
    /// Keys accumulated while an insert session is open.
    change_keys: Option<Vec<Key>>,
    /// Keys that have shaped the current visual selection, for `.` to replay.
    visual_keys: Vec<Key>,
    recording: Option<(char, Vec<Key>)>,
    macros: BTreeMap<char, Vec<Key>>,
    replay_depth: usize,
    /// True while an insert session's undo group is open.
    insert_group: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self::from_text("")
    }
}

impl Editor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::with(text, Keymap::vim())
    }

    #[must_use]
    pub fn with(text: &str, keymap: Keymap) -> Self {
        Self {
            doc: Document::from_text(text),
            keymap,
            indent: Indent::default(),
            viewport: Viewport::default(),
            pending: Pending::new(),
            mode: Mode::Normal,
            cursor: 0,
            sticky: 0,
            anchor: None,
            register: Register::default(),
            jumps: Vec::new(),
            jump_at: 0,
            marks: [None; MARK_COUNT],
            last_find: None,
            last_search: None,
            last_change: Vec::new(),
            change_keys: None,
            visual_keys: Vec::new(),
            recording: None,
            macros: BTreeMap::new(),
            replay_depth: 0,
            insert_group: false,
        }
    }

    // -- queries ---------------------------------------------------------

    /// Configure indentation before returning this editor.
    #[must_use]
    pub fn with_indent(mut self, indent: Indent) -> Self {
        self.indent = indent;
        self
    }

    /// The host-supplied indentation policy used by shift operators.
    #[must_use]
    pub const fn indent(&self) -> Indent {
        self.indent
    }

    /// Replace the host-supplied indentation policy used by shift operators.
    pub fn set_indent(&mut self, indent: Indent) {
        self.indent = indent;
    }

    /// Configure viewport facts before returning this editor.
    #[must_use]
    pub fn with_viewport(mut self, viewport: Viewport) -> Self {
        self.viewport = viewport;
        self
    }

    /// The viewport facts most recently supplied by the host.
    #[must_use]
    pub const fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// Replace the viewport facts most recently supplied by the host.
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.viewport = viewport;
    }

    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        self.doc.buffer()
    }

    #[must_use]
    pub fn document(&self) -> &Document {
        &self.doc
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Cursor position as a byte offset.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Remembered jump positions, oldest first.
    #[must_use]
    pub fn jumps(&self) -> &[usize] {
        &self.jumps
    }

    /// The offset remembered under a named or automatic mark name.
    #[must_use]
    pub fn mark(&self, name: char) -> Option<usize> {
        self.marks[mark_index(name)?]
    }

    /// Move to a host-supplied location, remembering the position left behind.
    ///
    /// The destination is clamped to the buffer and pulled back to a grapheme
    /// boundary, so byte-oriented hosts cannot leave the caret mid-character.
    pub fn jump_to(&mut self, offset: usize) {
        self.push_jump();
        self.place_cursor(offset);
    }

    #[must_use]
    pub fn cursor_point(&self) -> Point {
        self.buffer().byte_to_point(self.cursor)
    }

    /// The visual selection as a byte range, inclusive of the character under the
    /// cursor — which is what makes `vd` delete what you can see.
    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        let buf = self.buffer();
        match self.mode {
            Mode::Visual(VisualKind::Char) => {
                let (start, end) = (anchor.min(self.cursor), anchor.max(self.cursor));
                Some(
                    start
                        ..motion::resolve(
                            buf,
                            end,
                            Motion::Right,
                            None,
                            0,
                            None,
                            None,
                            self.viewport,
                            Bound::PastEnd,
                        )
                        .unwrap_or(end),
                )
            }
            Mode::Visual(VisualKind::Line) => {
                let first = buf.byte_to_point(anchor.min(self.cursor)).row;
                let last = buf.byte_to_point(anchor.max(self.cursor)).row;
                Some(motion::row_span(buf, first, last))
            }
            _ => None,
        }
    }

    /// Keys of a partially-typed command, for a `showcmd` indicator.
    #[must_use]
    pub fn pending_keys(&self) -> &[Key] {
        self.pending.keys()
    }

    #[must_use]
    pub fn register(&self) -> &Register {
        &self.register
    }

    #[must_use]
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    pub fn keymap_mut(&mut self) -> &mut Keymap {
        &mut self.keymap
    }

    /// The register being recorded into, if any.
    #[must_use]
    pub fn recording(&self) -> Option<char> {
        self.recording.as_ref().map(|(register, _)| *register)
    }

    /// Keys `.` would replay.
    #[must_use]
    pub fn last_change(&self) -> &[Key] {
        &self.last_change
    }

    // -- input -----------------------------------------------------------

    /// Feed one key.
    pub fn handle_key(&mut self, key: Key) -> Vec<Effect> {
        // A bare `q` stops recording, so it must be caught before the parser can
        // treat it as "await a register". Recording is editor state, which is why
        // the parser cannot decide this.
        if self.replay_depth == 0
            && self.mode == Mode::Normal
            && self.pending.is_idle()
            && key == Key::char('q')
            && let Some((register, script)) = self.recording.take()
        {
            self.macros.insert(register, script);
            return vec![Effect::RecordingStopped(register)];
        }

        // Record raw keys, not resolved commands. Replayed keys are excluded, so
        // `@a` inside a recording stores `@a` rather than its expansion.
        if self.replay_depth == 0
            && let Some((_, script)) = &mut self.recording
        {
            script.push(key);
        }
        if let Some(script) = &mut self.change_keys {
            script.push(key);
        }

        match self.pending.feed(key, self.mode, &self.keymap) {
            Resolution::Pending | Resolution::Cancelled { .. } => Vec::new(),
            Resolution::Rejected { .. } => vec![Effect::Bell],
            Resolution::Command {
                command,
                count,
                keys: consumed,
            } => {
                let was_visual = self.mode.is_visual();
                let effects = self.run(command.clone(), count);
                // Everything typed since the selection opened, so that `.` can
                // replay the shape and not just the operator. The operator's own
                // key is not among them: by the time it runs, visual mode is over.
                if self.mode.is_visual() {
                    if !was_visual {
                        self.visual_keys.clear();
                    }
                    self.visual_keys.extend_from_slice(&consumed);
                }
                self.note_change(&command, &consumed);
                effects
            }
        }
    }

    /// Feed several keys.
    pub fn handle_keys(&mut self, sequence: &[Key]) -> Vec<Effect> {
        sequence
            .iter()
            .flat_map(|key| self.handle_key(*key))
            .collect()
    }

    /// Feed a key sequence written in vi notation.
    ///
    /// # Errors
    /// If `spec` is not valid key notation.
    pub fn type_keys(&mut self, spec: &str) -> Result<Vec<Effect>, ParseError> {
        Ok(self.handle_keys(&keys(spec)?))
    }

    /// Replace the whole buffer, resetting cursor and history-independent state.
    pub fn set_text(&mut self, text: &str) -> Edit {
        let edit = self.doc.replace(0..self.buffer().len_bytes(), text);
        self.cursor = 0;
        self.sticky = 0;
        self.anchor = None;
        self.pending.reset();
        self.mode = Mode::Normal;
        self.jumps.clear();
        self.jump_at = 0;
        self.marks = [None; MARK_COUNT];
        edit
    }

    // -- execution -------------------------------------------------------

    fn bound(&self) -> Bound {
        match self.mode {
            Mode::Insert | Mode::Replace => Bound::PastEnd,
            Mode::Normal | Mode::Visual(_) => Bound::OnChar,
        }
    }

    fn set_mode(&mut self, mode: Mode, effects: &mut Vec<Effect>) {
        if self.mode != mode {
            self.mode = mode;
            self.cursor = motion::clamp(self.buffer(), self.cursor, self.bound());
            effects.push(Effect::ModeChanged(mode));
        }
    }

    fn edit(&mut self, range: Range<usize>, text: &str, effects: &mut Vec<Effect>) {
        if range.is_empty() && text.is_empty() {
            return;
        }
        let edit = self.doc.replace(range, text);
        self.shift_positions(&edit);
        effects.push(Effect::Edit(edit));
    }

    fn yank(&mut self, span: &Span) {
        let buf = self.buffer();
        let (range, text, linewise) = match span {
            Span::Chars(range) => (range.clone(), buf.text_in(range.clone()), false),
            Span::Lines(rows) => {
                let first = *rows.start();
                let last = *rows.end();
                let range = buf.row_range(first).start..buf.row_range(last).end;
                let mut text = buf.text_in(range.clone());
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                (range, text, true)
            }
        };
        self.register = Register { text, linewise };
        self.set_mark('[', range.start);
        self.set_mark(']', self.previous_grapheme(range.end));
    }

    /// Every command runs inside an undo group, bracketed by the caret on either
    /// side so undo and redo can put it back where the user was.
    ///
    /// Groups nest, so this is safe while an insert session's group is already
    /// open: the inner begin/end pair changes nothing and the session still
    /// collapses to one step — and the caret recorded is the one from before the
    /// session opened, which is what makes undoing an `o` land where you pressed
    /// it. An empty group is never pushed, so non-editing commands cost nothing.
    fn run(&mut self, command: Command, count: Option<usize>) -> Vec<Effect> {
        let before = self.cursor;
        self.doc.history_mut().begin_group(Some(before));
        let effects = self.dispatch(command, count);
        self.remember_change(&effects);
        let after = self.cursor;
        self.doc.history_mut().end_group(Some(after));
        effects
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch(&mut self, command: Command, count: Option<usize>) -> Vec<Effect> {
        let mut effects = Vec::new();
        let repeat = count.unwrap_or(1);

        match command {
            Command::Move(target) => {
                let Some(target) = self.resolve_mark(target) else {
                    effects.push(Effect::Bell);
                    return effects;
                };
                // A submitted pattern becomes the last search even when it has
                // no match; `n` then repeats that same failed search, as vi does.
                self.remember_search(&target);
                let bound = self.bound();
                match motion::resolve(
                    self.buffer(),
                    self.cursor,
                    target.clone(),
                    count,
                    self.sticky,
                    self.last_find,
                    self.last_search
                        .as_ref()
                        .map(|(pattern, backward)| (pattern.as_str(), *backward)),
                    self.viewport,
                    bound,
                ) {
                    Some(landed) => {
                        if landed != self.cursor && pushes_jump(&target) {
                            self.push_jump();
                        }
                        self.cursor = landed;
                        self.remember_find(&target);
                        self.update_sticky(&target);
                    }
                    None => effects.push(Effect::Bell),
                }
            }

            Command::Operate { operator, target } => {
                self.remember_target_find(&target);
                self.remember_target_search(&target);
                match self.span_of(operator, target, count) {
                    Some(span) => {
                        let amount = if self.mode.is_visual() {
                            count.unwrap_or(1)
                        } else {
                            1
                        };
                        self.operate(operator, span, amount, &mut effects);
                    }
                    None => effects.push(Effect::Bell),
                }
            }

            Command::SelectObject { scope, object } => {
                match motion::object_span(self.buffer(), self.cursor, scope, object, repeat) {
                    Some(span) => {
                        let range = match span {
                            Span::Chars(range) => range,
                            Span::Lines(rows) => {
                                self.buffer().row_range(*rows.start()).start
                                    ..self.buffer().row_range(*rows.end()).end
                            }
                        };
                        self.anchor = Some(range.start);
                        let end = motion::clamp(
                            self.buffer(),
                            range.end.saturating_sub(1),
                            Bound::OnChar,
                        );
                        self.place_cursor(end);
                    }
                    None => effects.push(Effect::Bell),
                }
            }

            Command::EnterInsert(at) => self.enter_insert(at, &mut effects),

            Command::EnterReplace => {
                self.open_insert_group();
                self.set_mode(Mode::Replace, &mut effects);
            }

            Command::EnterVisual(kind) => {
                if self.mode == Mode::Visual(kind) {
                    self.leave_visual(true, &mut effects);
                } else {
                    self.anchor = Some(self.cursor);
                    self.set_mode(Mode::Visual(kind), &mut effects);
                }
            }

            Command::EnterNormal => {
                let leaving_insert = matches!(self.mode, Mode::Insert | Mode::Replace);
                self.close_insert_group();
                if leaving_insert {
                    // vi's insert cursor sits *between* characters, so leaving puts
                    // it on the character to the left. This has to happen before
                    // the mode switch: `set_mode` clamps to `OnChar`, and doing
                    // both would move the cursor twice.
                    self.cursor = self.step(self.cursor, Motion::Left, 1, Bound::PastEnd);
                    self.set_mark('^', self.cursor);
                }
                if self.mode.is_visual() {
                    self.leave_visual(true, &mut effects);
                } else {
                    self.anchor = None;
                    self.set_mode(Mode::Normal, &mut effects);
                }
                if leaving_insert {
                    // The cursor moved, so the column `j`/`k` aim for has to follow
                    // it. Insert advances the sticky column with every character
                    // typed; leaving it stale here lands the next `j` one column to
                    // the right of where the cursor visibly is.
                    self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
                }
            }

            Command::DeleteChar { before } => {
                let range = if before {
                    let start = self.step(self.cursor, Motion::Left, repeat, Bound::OnChar);
                    start..self.cursor
                } else {
                    let end = self.step(self.cursor, Motion::Right, repeat, Bound::PastEnd);
                    self.cursor..end
                };
                if range.is_empty() {
                    effects.push(Effect::Bell);
                } else {
                    self.yank(&Span::Chars(range.clone()));
                    let start = range.start;
                    self.edit(range, "", &mut effects);
                    self.place_cursor(start);
                }
            }

            Command::ReplaceChar(ch) => {
                let end = self.step(self.cursor, Motion::Right, repeat, Bound::PastEnd);
                if end == self.cursor {
                    effects.push(Effect::Bell);
                } else {
                    let replacement: String = core::iter::repeat_n(ch, repeat).collect();
                    self.edit(self.cursor..end, &replacement, &mut effects);
                }
            }

            Command::ChangeSurround { from, to } => {
                self.change_surround(from, to, &mut effects);
            }

            Command::DeleteSurround(target) => {
                self.delete_surround(target, &mut effects);
            }

            Command::SurroundSelection(delimiter) => {
                self.surround_selection(delimiter, &mut effects);
            }

            Command::SwapCase => {
                let end = self.step(self.cursor, Motion::Right, repeat, Bound::PastEnd);
                if end == self.cursor {
                    effects.push(Effect::Bell);
                } else {
                    let swapped: String = self
                        .buffer()
                        .text_in(self.cursor..end)
                        .chars()
                        .map(swap_case)
                        .collect();
                    self.edit(self.cursor..end, &swapped, &mut effects);
                    self.place_cursor(end);
                }
            }

            Command::JoinRows => self.join_rows(repeat.max(2), &mut effects),

            Command::Put { before } => self.put(before, repeat, &mut effects),

            Command::Undo => {
                let step = self.doc.undo();
                self.revert(&step, &mut effects);
            }

            Command::Redo => {
                let step = self.doc.redo();
                self.revert(&step, &mut effects);
            }

            Command::Repeat => {
                let script = self.last_change.clone();
                if script.is_empty() {
                    effects.push(Effect::Bell);
                } else {
                    effects.extend(self.replay(&script, repeat));
                }
            }

            Command::RecordMacro(register) => {
                self.recording = Some((register, Vec::new()));
                effects.push(Effect::RecordingStarted(register));
            }

            Command::PlayMacro(register) => match self.macros.get(&register).cloned() {
                Some(script) => effects.extend(self.replay(&script, repeat)),
                None => effects.push(Effect::Bell),
            },

            Command::SetMark(name) => match mark_index(name) {
                Some(index) => self.marks[index] = Some(self.cursor),
                None => effects.push(Effect::Bell),
            },

            Command::JumpBack => self.jump_back(&mut effects),

            Command::JumpForward => self.jump_forward(&mut effects),

            Command::Scroll(scroll) => {
                if self.viewport.height != 0 {
                    let (motion, rows) = match scroll {
                        Scroll::HalfPageDown => (Motion::Down, (self.viewport.height / 2).max(1)),
                        Scroll::HalfPageUp => (Motion::Up, (self.viewport.height / 2).max(1)),
                        // vi preserves two rows of overlap between full pages.
                        Scroll::PageDown => {
                            (Motion::Down, self.viewport.height.saturating_sub(2).max(1))
                        }
                        Scroll::PageUp => {
                            (Motion::Up, self.viewport.height.saturating_sub(2).max(1))
                        }
                        // These only ask the host to position its window; they do
                        // not move the caret.
                        Scroll::Center | Scroll::Top | Scroll::Bottom => {
                            effects.push(Effect::Scroll(scroll));
                            return effects;
                        }
                    };
                    let landed = self.step(self.cursor, motion, rows, self.bound());
                    if landed != self.cursor {
                        self.push_jump();
                        self.cursor = landed;
                    }
                }
                effects.push(Effect::Scroll(scroll));
            }
            Command::CommandPrompt => effects.push(Effect::CommandPrompt),

            Command::InsertText(ch) => {
                let text = ch.to_string();
                if self.mode == Mode::Replace {
                    let end = self.step(self.cursor, Motion::Right, 1, Bound::PastEnd);
                    self.edit(self.cursor..end, &text, &mut effects);
                } else {
                    self.edit(self.cursor..self.cursor, &text, &mut effects);
                }
                self.cursor += text.len();
                self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
            }

            Command::InsertNewline => {
                self.edit(self.cursor..self.cursor, "\n", &mut effects);
                self.cursor += 1;
                self.sticky = 0;
            }

            Command::DeleteBack => {
                let start = self.prev_position();
                if start == self.cursor {
                    effects.push(Effect::Bell);
                } else {
                    self.edit(start..self.cursor, "", &mut effects);
                    self.cursor = start;
                    self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
                }
            }

            Command::DeleteWordBack => {
                let start = motion::resolve(
                    self.buffer(),
                    self.cursor,
                    Motion::WordBackward { big: false },
                    None,
                    self.sticky,
                    None,
                    None,
                    self.viewport,
                    Bound::PastEnd,
                )
                .unwrap_or(self.cursor);
                if start >= self.cursor {
                    effects.push(Effect::Bell);
                } else {
                    self.edit(start..self.cursor, "", &mut effects);
                    self.cursor = start;
                    self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
                }
            }
        }

        effects
    }

    // -- helpers ---------------------------------------------------------

    /// Apply `motion` `times` from `at`, under `bound`.
    fn step(&self, at: usize, motion: Motion, times: usize, bound: Bound) -> usize {
        motion::resolve(
            self.buffer(),
            at,
            motion,
            Some(times),
            self.sticky,
            self.last_find,
            self.last_search
                .as_ref()
                .map(|(pattern, backward)| (pattern.as_str(), *backward)),
            self.viewport,
            bound,
        )
        .unwrap_or(at)
    }

    /// One position back, crossing a row boundary if need be — which plain `h`
    /// deliberately will not do.
    fn prev_position(&self) -> usize {
        let point = self.cursor_point();
        if point.col > 0 {
            return self.step(self.cursor, Motion::Left, 1, Bound::PastEnd);
        }
        if point.row == 0 {
            return self.cursor;
        }
        self.buffer().row_content_range(point.row - 1).end
    }

    /// The grapheme immediately before an exclusive selection or edit endpoint.
    ///
    /// `h` deliberately stops at a row boundary, but a linewise selection's end
    /// can be the start of the following row after its trailing newline.
    fn previous_grapheme(&self, byte: usize) -> usize {
        let byte = byte.min(self.buffer().len_bytes());
        let point = self.buffer().byte_to_point(byte);
        if byte > 0 && point.col == 0 {
            // The row above ends at its content end, not at `byte - 1`: a `\r\n`
            // terminator is two bytes and one grapheme, so stepping back a single
            // byte would land between them.
            self.buffer().row_content_range(point.row - 1).end
        } else {
            self.step(byte, Motion::Left, 1, Bound::PastEnd)
        }
    }

    fn set_mark(&mut self, name: char, offset: usize) {
        if let Some(index) = mark_index(name) {
            self.marks[index] = Some(offset);
        }
    }

    /// Remember the outer extent of every edit emitted by one command.
    fn remember_change(&mut self, effects: &[Effect]) {
        let edits: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Edit(edit) => Some(edit),
                _ => None,
            })
            .collect();
        if edits.is_empty() {
            return;
        }
        // `shift_rows` edits from bottom to top. Each edit's coordinates are in
        // the buffer as it stood when that one happened, so carry its endpoints
        // through later edits before comparing the command's final extent.
        let mut start = usize::MAX;
        let mut end = 0;
        for (index, edit) in edits.iter().enumerate() {
            let later = &edits[index + 1..];
            let edit_start = later
                .iter()
                .fold(edit.start_byte, |offset, later| later.shift(offset));
            let edit_end = later
                .iter()
                .fold(edit.new_end_byte, |offset, later| later.shift(offset));
            start = start.min(edit_start);
            end = end.max(edit_end);
        }
        self.set_mark('[', start);
        self.set_mark(']', self.previous_grapheme(end));
    }

    /// Capture visual marks before an operator changes the selection's buffer.
    fn remember_visual_selection(&mut self) {
        let Some(selection) = self.selection() else {
            return;
        };
        self.set_mark('<', selection.start);
        self.set_mark('>', self.previous_grapheme(selection.end));
    }

    /// The operator semantics `;` or `,` inherits from the remembered find.
    ///
    /// Any other motion is already concrete and passes through. Without this,
    /// [`Motion::is_inclusive`] has no direction to answer from and falls back to
    /// exclusive, which silently costs `d;` a character.
    fn motion_semantics(&self, motion: &Motion) -> (bool, bool) {
        match (motion, self.last_find) {
            (Motion::RepeatFind { reverse }, Some(find)) => {
                let effective = Motion::Find(Find {
                    backward: find.backward != *reverse,
                    ..find
                });
                (effective.is_linewise(), effective.is_inclusive())
            }
            _ => (motion.is_linewise(), motion.is_inclusive()),
        }
    }

    /// Turn an editor-owned mark into the pure resolver's concrete vocabulary.
    fn resolve_mark(&self, motion: Motion) -> Option<Motion> {
        match motion {
            Motion::Mark { name, exact } => {
                // `''` and ``` `` ``` return to where the latest jump started,
                // which is the newest ring entry. Going there pushes the position
                // being left in turn, so repeating the key toggles between the
                // two rather than walking further back as `<C-o>` does.
                let offset = if matches!(name, '\'' | '`') {
                    *self.jumps.last()?
                } else {
                    self.mark(name)?
                };
                Some(Motion::ToOffset {
                    offset,
                    linewise: !exact,
                })
            }
            motion => Some(motion),
        }
    }

    fn remember_find(&mut self, motion: &Motion) {
        if let Motion::Find(find) = motion {
            self.last_find = Some(*find);
        }
    }

    fn remember_target_find(&mut self, target: &Target) {
        if let Target::Motion(motion) = target {
            self.remember_find(motion);
        }
    }

    fn remember_search(&mut self, motion: &Motion) {
        if let Motion::Search { pattern, backward } = motion {
            self.last_search = Some((pattern.clone(), *backward));
        }
    }

    fn remember_target_search(&mut self, target: &Target) {
        if let Target::Motion(motion) = target {
            self.remember_search(motion);
        }
    }

    /// Reposition the cursor and refresh the remembered column with it.
    ///
    /// Anything that moves the cursor *other than a motion* has to come through
    /// here. Leaving `sticky` behind makes the next `j`/`k` aim at where the cursor
    /// used to be, which shows up as the cursor drifting sideways a keystroke later.
    ///
    /// Motions are the deliberate exception: [`Self::update_sticky`] preserves
    /// `$`'s stickiness, which an unconditional refresh would destroy.
    ///
    /// `byte` is clamped to whatever the current mode allows, so a caller that has
    /// just deleted the text it was aiming at need not work out where the end of the
    /// row went.
    fn place_cursor(&mut self, byte: usize) {
        self.cursor = motion::clamp(self.buffer(), byte, self.bound());
        self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
    }

    fn update_sticky(&mut self, motion: &Motion) {
        match motion {
            // Vertical movement consumes the sticky column without changing it.
            Motion::Up | Motion::Down => {}
            // `$` sticks to row ends, so subsequent `j` stays at the end.
            Motion::LastColumn => self.sticky = STICKY_END,
            _ => self.sticky = motion::grapheme_col(self.buffer(), self.cursor),
        }
    }

    /// Remember the current position and discard destinations beyond it.
    fn push_jump(&mut self) {
        self.jumps.truncate(self.jump_at);
        self.jumps.push(self.cursor);
        if self.jumps.len() > MAX_JUMPS {
            self.jumps.remove(0);
        }
        self.jump_at = self.jumps.len();
    }

    fn jump_back(&mut self, effects: &mut Vec<Effect>) {
        if self.jumps.is_empty() {
            effects.push(Effect::Bell);
            return;
        }
        if self.jump_at == self.jumps.len() {
            // Add the present so `<C-i>` can return here, then skip it to reach
            // the position before the jump that started this traversal.
            self.push_jump();
            self.jump_at = self.jumps.len() - 2;
        } else if self.jump_at == 0 {
            effects.push(Effect::Bell);
            return;
        } else {
            self.jump_at -= 1;
        }
        self.place_cursor(self.jumps[self.jump_at]);
    }

    /// Return towards the present, ringing at the newest position as
    /// [`Self::jump_back`] does at the oldest.
    ///
    /// Indexing through `get` rather than `jumps[next]` keeps this sound
    /// without depending on the invariant that `jump_at` is never one short of
    /// the end — which holds today, but is not worth making load-bearing.
    fn jump_forward(&mut self, effects: &mut Vec<Effect>) {
        let next = self.jump_at + 1;
        let Some(&offset) = self.jumps.get(next) else {
            effects.push(Effect::Bell);
            return;
        };
        self.place_cursor(offset);
        // Landing on the last entry means we are back at the present.
        self.jump_at = if next + 1 == self.jumps.len() {
            self.jumps.len()
        } else {
            next
        };
    }

    /// Shift every editor-owned remembered position through an applied edit.
    fn shift_positions(&mut self, edit: &Edit) {
        for jump in &mut self.jumps {
            *jump = edit.shift(*jump);
        }
        for mark in self.marks.iter_mut().flatten() {
            *mark = edit.shift(*mark);
        }
    }

    /// Apply the outcome of an undo or redo.
    ///
    /// The caret goes back to where the history says it was. Failing that — a
    /// change recorded outside a group — fall back to the last edit's site,
    /// which is at least where the text moved.
    fn revert(&mut self, step: &Step, effects: &mut Vec<Effect>) {
        if step.is_empty() {
            effects.push(Effect::Bell);
            return;
        }
        for change in &step.changes {
            self.shift_positions(&change.edit);
            effects.push(Effect::Edit(change.edit));
        }
        let at = step
            .cursor
            .unwrap_or_else(|| step.changes[step.changes.len() - 1].edit.start_byte);
        self.place_cursor(at);
    }

    // -- operators -------------------------------------------------------

    /// Resolve an operator's target to a span.
    fn span_of(&self, operator: Operator, target: Target, count: Option<usize>) -> Option<Span> {
        let buf = self.buffer();
        let span = match target {
            Target::Motion(motion) => {
                let motion = self.resolve_mark(motion)?;
                // vi's one famous irregularity: `cw` behaves like `ce`, so that
                // changing a word does not swallow the space after it.
                let motion = if operator == Operator::Change
                    && matches!(motion, Motion::WordForward { .. })
                {
                    match motion {
                        Motion::WordForward { big } => Motion::WordEnd { big },
                        other => other,
                    }
                } else {
                    motion
                };

                // Resolution keeps the `RepeatFind` form, because that is what tells
                // `motion::resolve` to skip the target a `t` is already parked on.
                // Operator semantics have to come from the concrete find it stands
                // for, or `d;` after `f,` stops one character short.
                let (linewise, inclusive) = self.motion_semantics(&motion);
                // An exclusive motion's landing place is the span's *end boundary*,
                // not somewhere the cursor has to be able to sit, so it may be one
                // past the last character — otherwise `dw` on the last word of the
                // file leaves its final character behind. An inclusive motion does
                // land on a character, and extends over it below.
                let bound = if inclusive {
                    Bound::OnChar
                } else {
                    Bound::PastEnd
                };
                let landed = motion::resolve(
                    buf,
                    self.cursor,
                    motion,
                    count,
                    self.sticky,
                    self.last_find,
                    self.last_search
                        .as_ref()
                        .map(|(pattern, backward)| (pattern.as_str(), *backward)),
                    self.viewport,
                    bound,
                )?;
                if linewise {
                    let first = buf.byte_to_point(self.cursor.min(landed)).row;
                    let last = buf.byte_to_point(self.cursor.max(landed)).row;
                    Span::Lines(first..=last)
                } else {
                    let (start, mut end) = (self.cursor.min(landed), self.cursor.max(landed));
                    if inclusive {
                        end = motion::resolve(
                            buf,
                            end,
                            Motion::Right,
                            None,
                            0,
                            None,
                            None,
                            self.viewport,
                            Bound::PastEnd,
                        )
                        .unwrap_or(end);
                    }
                    Span::Chars(start..end)
                }
            }
            Target::CurrentRow => {
                let first = self.cursor_point().row;
                let last = first
                    .saturating_add(count.unwrap_or(1).saturating_sub(1))
                    .min(buf.len_rows() - 1);
                Span::Lines(first..=last)
            }
            Target::Object { scope, object } => {
                motion::object_span(buf, self.cursor, scope, object, count.unwrap_or(1))?
            }
            Target::Selection => match self.mode {
                Mode::Visual(VisualKind::Char) => Span::Chars(self.selection()?),
                Mode::Visual(VisualKind::Line) => {
                    let anchor = self.anchor?;
                    let first = buf.byte_to_point(anchor.min(self.cursor)).row;
                    let last = buf.byte_to_point(anchor.max(self.cursor)).row;
                    Span::Lines(first..=last)
                }
                _ => return None,
            },
        };
        if operator.forces_linewise() {
            match span {
                Span::Chars(range) => {
                    let first = buf.byte_to_point(range.start).row;
                    let last = buf
                        .byte_to_point(range.end.saturating_sub(1).max(range.start))
                        .row;
                    Some(Span::Lines(first..=last))
                }
                Span::Lines(_) => Some(span),
            }
        } else {
            Some(span)
        }
    }

    fn operate(
        &mut self,
        operator: Operator,
        span: Span,
        amount: usize,
        effects: &mut Vec<Effect>,
    ) {
        let was_visual = self.mode.is_visual();
        if was_visual {
            self.remember_visual_selection();
        }
        let span_is_empty = match &span {
            Span::Chars(range) => range.is_empty(),
            Span::Lines(_) => self.buffer().len_bytes() == 0,
        };
        if span_is_empty && operator != Operator::Change && !operator.forces_linewise() {
            effects.push(Effect::Bell);
            // Still drop the selection: a no-op operator must not strand the
            // editor in visual mode, or the next keystroke is interpreted against
            // a selection the user thinks they have dismissed.
            if self.mode.is_visual() {
                self.leave_visual(false, effects);
            }
            return;
        }
        if operator.yanks() {
            self.yank(&span);
        }
        match operator {
            Operator::ShiftRight | Operator::ShiftLeft => {
                let Span::Lines(rows) = span else {
                    unreachable!("shift spans are widened to rows by span_of")
                };
                let first = *rows.start();
                let last = *rows.end();
                self.shift_rows(first, last, operator, amount, effects);
                self.cursor = self.buffer().row_content_range(first).start;
                self.cursor = self.step(self.cursor, Motion::FirstNonBlank, 1, Bound::OnChar);
            }
            Operator::Lower | Operator::Upper | Operator::SwapCase => {
                let range = span.content_range(self.buffer());
                let start = span.home(self.buffer());
                let linewise = span.is_linewise();
                let text = self.buffer().text_in(range.clone());
                let recased = recase(&text, operator);
                self.edit(range, &recased, effects);
                self.cursor = motion::clamp(self.buffer(), start, self.bound());
                if linewise {
                    self.cursor = self.step(self.cursor, Motion::FirstNonBlank, 1, Bound::OnChar);
                }
            }
            Operator::Yank => {
                let home = span.home(self.buffer());
                self.cursor = motion::clamp(self.buffer(), home, self.bound());
            }
            Operator::Delete => {
                let range = span.delete_range(self.buffer());
                let start = span.home(self.buffer());
                let linewise = span.is_linewise();
                self.edit(range, "", effects);
                self.cursor = motion::clamp(self.buffer(), start, self.bound());
                if linewise {
                    self.cursor = self.step(self.cursor, Motion::FirstNonBlank, 1, Bound::OnChar);
                }
            }
            Operator::Change => {
                // Linewise change empties the rows but keeps one, so insert begins
                // on a blank row rather than joining the next one up. That is
                // exactly the content range: it stops short of the terminator.
                let range = span.content_range(self.buffer());
                let start = range.start;
                self.edit(range, "", effects);
                self.cursor = start;
                self.open_insert_group();
                self.set_mode(Mode::Insert, effects);
            }
        }

        if was_visual && self.mode.is_visual() {
            self.leave_visual(false, effects);
        }
        self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
    }

    /// Shift rows from bottom to top so replacing one indent cannot invalidate an
    /// offset still needed by an earlier row. Empty rows stay untouched, while a
    /// whitespace-only row is deliberately still an indent worth changing.
    fn shift_rows(
        &mut self,
        first: usize,
        last: usize,
        operator: Operator,
        amount: usize,
        effects: &mut Vec<Effect>,
    ) {
        let columns = self.indent.shift_width.saturating_mul(amount);
        let tab_width = self.indent.tab_width.max(1);
        for row in (first..=last).rev() {
            let content = self.buffer().row_content_range(row);
            if content.is_empty() {
                continue;
            }
            let text = self.buffer().text_in(content.clone());
            let indent_len = text
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            let old =
                text.as_bytes()[..indent_len]
                    .iter()
                    .fold(0_usize, |width, byte| match byte {
                        b' ' => width + 1,
                        b'\t' => width + (tab_width - width % tab_width),
                        _ => width,
                    });
            let new = match operator {
                Operator::ShiftRight => old.saturating_add(columns),
                Operator::ShiftLeft => old.saturating_sub(columns),
                _ => unreachable!("shift_rows only receives shift operators"),
            };
            let rendered = if self.indent.use_tabs {
                format!(
                    "{}{}",
                    "\t".repeat(new / tab_width),
                    " ".repeat(new % tab_width)
                )
            } else {
                " ".repeat(new)
            };
            if rendered != text[..indent_len] {
                self.edit(
                    content.start..content.start + indent_len,
                    &rendered,
                    effects,
                );
            }
        }
    }

    fn leave_visual(&mut self, remember_selection: bool, effects: &mut Vec<Effect>) {
        if remember_selection {
            self.remember_visual_selection();
        }
        self.anchor = None;
        self.set_mode(Mode::Normal, effects);
    }

    // -- insert ----------------------------------------------------------

    fn enter_insert(&mut self, at: InsertAt, effects: &mut Vec<Effect>) {
        self.open_insert_group();
        match at {
            InsertAt::Cursor => {}
            InsertAt::After => {
                self.cursor = self.step(self.cursor, Motion::Right, 1, Bound::PastEnd);
            }
            InsertAt::FirstNonBlank => {
                self.cursor = self.step(self.cursor, Motion::FirstNonBlank, 1, Bound::OnChar);
            }
            InsertAt::EndOfRow => {
                self.cursor = self.buffer().row_content_range(self.cursor_point().row).end;
            }
            InsertAt::RowBelow => {
                let end = self.buffer().row_content_range(self.cursor_point().row).end;
                self.edit(end..end, "\n", effects);
                self.cursor = end + 1;
            }
            InsertAt::RowAbove => {
                let start = self.buffer().row_range(self.cursor_point().row).start;
                self.edit(start..start, "\n", effects);
                self.cursor = start;
            }
        }
        self.set_mode(Mode::Insert, effects);
        self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
    }

    /// Open the undo group that spans a whole insert session.
    ///
    /// This is the coarser of the two granularities: the [`Edit`] stream still
    /// reports one edit per keystroke so highlighting keeps up, but the user gets
    /// one `u`.
    fn open_insert_group(&mut self) {
        if !self.insert_group {
            // Nested inside the current command's group, so the caret passed here
            // is never the one recorded — see `run`.
            let at = self.cursor;
            self.doc.history_mut().begin_group(Some(at));
            self.insert_group = true;
        }
    }

    fn close_insert_group(&mut self) {
        if self.insert_group {
            let at = self.cursor;
            self.doc.history_mut().end_group(Some(at));
            self.insert_group = false;
        }
    }

    // -- edits that need more than a span --------------------------------

    fn change_surround(&mut self, from: char, to: char, effects: &mut Vec<Effect>) {
        let Some((new_open, new_close, new_padding)) = surround_pair(to) else {
            effects.push(Effect::Bell);
            return;
        };
        let Some((open, close, open_width, close_width)) = self.surround_offsets(from) else {
            effects.push(Effect::Bell);
            return;
        };
        let old_padding = self.surround_has_padding(open, close, open_width);
        let close_start = if old_padding { close - 1 } else { close };
        let close_text = if new_padding {
            format!(" {new_close}")
        } else {
            new_close.to_string()
        };
        // Offsets belong to the original buffer. Editing the later delimiter
        // first keeps the opening edit from shifting the closing one.
        self.edit(close_start..close + close_width, &close_text, effects);

        let open_end = open + open_width + usize::from(old_padding);
        let open_text = if new_padding {
            format!("{new_open} ")
        } else {
            new_open.to_string()
        };
        self.edit(open..open_end, &open_text, effects);
        self.place_cursor(open);
    }

    fn delete_surround(&mut self, target: char, effects: &mut Vec<Effect>) {
        let Some((open, close, open_width, close_width)) = self.surround_offsets(target) else {
            effects.push(Effect::Bell);
            return;
        };
        // Open and close target spellings deliberately behave alike. Padding is
        // a property of the existing pair, not of the key used to name it.
        let padding = self.surround_has_padding(open, close, open_width);
        let close_start = if padding { close - 1 } else { close };
        self.edit(close_start..close + close_width, "", effects);
        let open_end = open + open_width + usize::from(padding);
        self.edit(open..open_end, "", effects);
        self.place_cursor(open);
    }

    fn surround_selection(&mut self, delimiter: char, effects: &mut Vec<Effect>) {
        let Some((open, close, padding)) = surround_pair(delimiter) else {
            effects.push(Effect::Bell);
            return;
        };
        let Some(selection) = self.selection() else {
            effects.push(Effect::Bell);
            return;
        };
        self.remember_visual_selection();
        // A linewise selection puts its delimiters on rows of their own, as
        // surround.vim does. Wrapping the raw span instead would leave the
        // closing delimiter prefixed to the row *after* the selection, since a
        // linewise span carries the trailing newline. Padding is meaningless
        // once each delimiter owns a row, so `S(` and `S)` agree here.
        let home = if self.mode == Mode::Visual(VisualKind::Line) {
            let anchor = self.anchor.unwrap_or(self.cursor);
            let buf = self.buffer();
            let first = buf.byte_to_point(anchor.min(self.cursor)).row;
            let last = buf.byte_to_point(anchor.max(self.cursor)).row;
            let start = buf.row_range(first).start;
            let end = buf.row_content_range(last).end;
            self.edit(end..end, &format!("\n{close}"), effects);
            self.edit(start..start, &format!("{open}\n"), effects);
            start
        } else {
            let close_text = if padding {
                format!(" {close}")
            } else {
                close.to_string()
            };
            self.edit(selection.end..selection.end, &close_text, effects);
            let open_text = if padding {
                format!("{open} ")
            } else {
                open.to_string()
            };
            self.edit(selection.start..selection.start, &open_text, effects);
            selection.start
        };
        self.leave_visual(false, effects);
        self.place_cursor(home);
    }

    fn surround_offsets(&self, target: char) -> Option<(usize, usize, usize, usize)> {
        let object = self.keymap.object(Key::char(target))?;
        let (open_width, close_width) = match object {
            crate::TextObject::Delimited { open, close } => (open.len_utf8(), close.len_utf8()),
            crate::TextObject::Quoted(quote) => (quote.len_utf8(), quote.len_utf8()),
            crate::TextObject::Word { .. } | crate::TextObject::Paragraph => return None,
        };
        let (open, close) = motion::delimiters(self.buffer(), self.cursor, object)?;
        Some((open, close, open_width, close_width))
    }

    fn surround_has_padding(&self, open: usize, close: usize, open_width: usize) -> bool {
        let inner_start = open + open_width;
        // Remove one space from each edge only when both exist. Requiring two
        // distinct bytes avoids treating the sole payload of `( )` twice.
        inner_start < close.saturating_sub(1)
            && self.buffer().byte(inner_start) == b' '
            && self.buffer().byte(close - 1) == b' '
    }

    fn join_rows(&mut self, rows: usize, effects: &mut Vec<Effect>) {
        for _ in 1..rows.max(2) {
            let row = self.cursor_point().row;
            if row + 1 >= self.buffer().len_rows() {
                effects.push(Effect::Bell);
                return;
            }
            let end = self.buffer().row_content_range(row).end;
            let next = self.buffer().row_range(row + 1);
            let next_text = self.buffer().text_in(next.clone());
            let trimmed = next_text.trim_start();
            let leading = next_text.len() - trimmed.len();
            // A single space replaces the newline and the next row's indent.
            let separator = if trimmed.is_empty() || end == self.buffer().row_range(row).start {
                ""
            } else {
                " "
            };
            self.edit(end..next.start + leading, separator, effects);
            self.cursor = end;
        }
        self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
    }

    fn put(&mut self, before: bool, repeat: usize, effects: &mut Vec<Effect>) {
        if self.register.text.is_empty() {
            effects.push(Effect::Bell);
            return;
        }
        let text = self.register.text.repeat(repeat);
        if self.register.linewise {
            let row = self.cursor_point().row;
            let rows = self.buffer().row_range(row);
            // Ensure the pasted block is newline-terminated so rows stay whole.
            let text = if text.ends_with('\n') {
                text
            } else {
                format!("{text}\n")
            };
            // A file that does not end in a newline has no row break to paste
            // after, so putting below its final row has to supply one — and give
            // up its own trailing newline in exchange, so the file ends as it
            // began.
            let last_byte = self.buffer().len_bytes();
            let break_first = !before
                && rows.end == last_byte
                && last_byte > 0
                && self.buffer().byte(last_byte - 1) != b'\n';
            let (at, text) = if before {
                (rows.start, text)
            } else if break_first {
                (
                    rows.end,
                    format!("\n{}", text.strip_suffix('\n').unwrap_or(&text)),
                )
            } else {
                (rows.end, text)
            };
            self.edit(at..at, &text, effects);
            // The pasted rows start after the break this had to open.
            let home = if break_first { at + 1 } else { at };
            self.cursor = self.step(home, Motion::FirstNonBlank, 1, Bound::OnChar);
        } else {
            let at = if before {
                self.cursor
            } else {
                self.step(self.cursor, Motion::Right, 1, Bound::PastEnd)
            };
            self.edit(at..at, &text, effects);
            self.cursor = motion::clamp(self.buffer(), at + text.len() - 1, Bound::OnChar);
        }
        self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
    }

    // -- replay ----------------------------------------------------------

    fn replay(&mut self, script: &[Key], times: usize) -> Vec<Effect> {
        if self.replay_depth >= MAX_REPLAY_DEPTH {
            return vec![Effect::Bell];
        }
        self.replay_depth += 1;
        let mut effects = Vec::new();
        for _ in 0..times {
            for key in script {
                effects.extend(self.handle_key(*key));
            }
        }
        self.replay_depth -= 1;
        effects
    }

    /// Track what `.` should replay.
    ///
    /// Commands that enter insert mode open a *session*: keys accumulate until the
    /// mode ends, so `.` after `ciwfoo<Esc>` replays the typed text too. One-shot
    /// changes record immediately.
    fn note_change(&mut self, command: &Command, consumed: &[Key]) {
        // A visual operator is only half of what happened: replaying a bare `>`
        // would leave an operator pending for whatever gets typed next. The keys
        // that opened and shaped the selection go in front of it, so `Vj>` repeats
        // as `Vj>` — the same two rows, from wherever the caret now is.
        let mut script = match command {
            Command::Operate {
                target: Target::Selection,
                ..
            }
            | Command::SurroundSelection(_) => core::mem::take(&mut self.visual_keys),
            _ => Vec::new(),
        };
        script.extend_from_slice(consumed);

        match command {
            Command::EnterInsert(_)
            | Command::EnterReplace
            | Command::Operate {
                operator: Operator::Change,
                ..
            } => {
                self.change_keys = Some(script);
            }
            Command::EnterNormal => {
                if let Some(script) = self.change_keys.take() {
                    self.last_change = script;
                }
            }
            // One-shot changes record immediately — but only outside a session,
            // whose keys are already accumulating.
            Command::Operate {
                operator:
                    Operator::Delete
                    | Operator::Lower
                    | Operator::Upper
                    | Operator::SwapCase
                    | Operator::ShiftRight
                    | Operator::ShiftLeft,
                ..
            }
            | Command::DeleteChar { .. }
            | Command::ReplaceChar(_)
            | Command::ChangeSurround { .. }
            | Command::DeleteSurround(_)
            | Command::SurroundSelection(_)
            | Command::JoinRows
            | Command::Put { .. }
            | Command::SwapCase
                if self.change_keys.is_none() =>
            {
                self.last_change = script;
            }
            _ => {}
        }
    }
}

/// The delimiters and padding convention selected by a surround key.
const fn surround_pair(delimiter: char) -> Option<(char, char, bool)> {
    match delimiter {
        '(' => Some(('(', ')', true)),
        '[' => Some(('[', ']', true)),
        '{' => Some(('{', '}', true)),
        '<' => Some(('<', '>', true)),
        ')' => Some(('(', ')', false)),
        ']' => Some(('[', ']', false)),
        '}' => Some(('{', '}', false)),
        '>' => Some(('<', '>', false)),
        quote @ ('"' | '\'' | '`') => Some((quote, quote, false)),
        _ => None,
    }
}

/// Motions that cross enough of the buffer to deserve a return point.
const fn pushes_jump(motion: &Motion) -> bool {
    matches!(
        motion,
        Motion::GotoRow
            | Motion::GotoFirstRow
            | Motion::Paragraph { .. }
            | Motion::MatchPair
            | Motion::ScreenTop
            | Motion::ScreenMiddle
            | Motion::ScreenBottom
            | Motion::ToOffset { .. }
            | Motion::Search { .. }
            | Motion::RepeatSearch { .. }
    )
}

const fn mark_index(name: char) -> Option<usize> {
    if name.is_ascii_lowercase() {
        Some((name as u8 - b'a') as usize)
    } else {
        match name {
            '<' => Some(26),
            '>' => Some(27),
            '[' => Some(28),
            ']' => Some(29),
            '^' => Some(30),
            _ => None,
        }
    }
}

/// Apply a case-changing operator to a stretch of text.
///
/// Lowering and raising go through `char::to_lowercase`/`to_uppercase`, which are
/// one-to-many — `ß` uppercases to `SS` — so the result can be longer than the
/// input. Swapping stays one-to-one, since there is no sensible reverse of that.
fn recase(text: &str, operator: Operator) -> String {
    match operator {
        Operator::Lower => text.chars().flat_map(char::to_lowercase).collect(),
        Operator::Upper => text.chars().flat_map(char::to_uppercase).collect(),
        Operator::SwapCase => text.chars().map(swap_case).collect(),
        Operator::Delete
        | Operator::Change
        | Operator::Yank
        | Operator::ShiftRight
        | Operator::ShiftLeft => unreachable!("recase only receives case operators"),
    }
}

fn swap_case(ch: char) -> char {
    if ch.is_uppercase() {
        ch.to_lowercase().next().unwrap_or(ch)
    } else if ch.is_lowercase() {
        ch.to_uppercase().next().unwrap_or(ch)
    } else {
        ch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Binding, Layer};

    #[test]
    fn a_rebound_key_changes_behaviour_end_to_end() {
        let mut keymap = Keymap::vim();
        keymap
            .bind_spec(Layer::Normal, "j", Binding::Motion(Motion::Up))
            .bind_spec(Layer::Normal, "k", Binding::Motion(Motion::Down));
        let mut editor = Editor::with("one\ntwo\nthree", keymap);
        editor.type_keys("k").expect("valid test keys");
        assert_eq!(editor.cursor_point(), Point::new(1, 0));
        editor.type_keys("gg").expect("valid test keys");
        editor.type_keys("dk").expect("valid test keys");
        assert_eq!(editor.buffer().to_string(), "three");
    }

    #[test]
    fn host_jumps_clamp_and_the_list_is_capped() {
        let mut editor = Editor::from_text("a🇦🇺");
        for _ in 0..=MAX_JUMPS {
            editor.jump_to(usize::MAX);
        }
        assert_eq!(editor.cursor(), 1);
        assert_eq!(editor.jumps().len(), MAX_JUMPS);
        assert!(editor.jumps().iter().all(|&jump| jump == 1));
        editor.set_text("new");
        assert!(editor.jumps().is_empty());
        assert_eq!(editor.mark('a'), None);
    }
}
