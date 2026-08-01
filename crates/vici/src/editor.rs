//! The reducer: `(state, Key) -> (state, Vec<Effect>)`.
//!
//! This is the only stateful type in the crate. It owns the cursor, the mode, the
//! pending parser and the register, and it drives [`Document`] and [`motion`].
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
use crate::document::{Document, Revert};
use crate::edit::{Edit, Point};
use crate::history::{History, LinearHistory};
use crate::key::{Key, ParseError, keys};
use crate::keymap::Keymap;
use crate::motion::{self, Bound, Find, STICKY_END, Span};
use crate::pending::{Pending, Resolution};

/// How deep `.` and `@` may nest before the editor refuses, so a macro that plays
/// itself terminates instead of overflowing the stack.
const MAX_REPLAY_DEPTH: usize = 64;

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
    /// `/` and `?`: open a prompt and call back.
    SearchPrompt {
        backward: bool,
    },
    /// `n` / `N`
    SearchRepeat {
        reverse: bool,
    },
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
pub struct Editor<H: History = LinearHistory> {
    doc: Document<H>,
    keymap: Keymap,
    pending: Pending,
    mode: Mode,
    cursor: usize,
    /// Remembered column for `j`/`k`. [`STICKY_END`] means "row end".
    sticky: usize,
    /// Visual-mode anchor; the other end of the selection.
    anchor: Option<usize>,
    register: Register,
    last_find: Option<Find>,
    /// Keys of the last buffer-changing command, for `.`.
    last_change: Vec<Key>,
    /// Keys accumulated while an insert session is open.
    change_keys: Option<Vec<Key>>,
    recording: Option<(char, Vec<Key>)>,
    macros: BTreeMap<char, Vec<Key>>,
    replay_depth: usize,
    /// True while an insert session's undo group is open.
    insert_group: bool,
}

impl Default for Editor<LinearHistory> {
    fn default() -> Self {
        Self::from_text("")
    }
}

impl Editor<LinearHistory> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::with(text, Keymap::vim(), LinearHistory::new())
    }
}

impl<H: History> Editor<H> {
    pub fn with(text: &str, keymap: Keymap, history: H) -> Self {
        Self {
            doc: Document::with_history(text, history),
            keymap,
            pending: Pending::new(),
            mode: Mode::Normal,
            cursor: 0,
            sticky: 0,
            anchor: None,
            register: Register::default(),
            last_find: None,
            last_change: Vec::new(),
            change_keys: None,
            recording: None,
            macros: BTreeMap::new(),
            replay_depth: 0,
            insert_group: false,
        }
    }

    // -- queries ---------------------------------------------------------

    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        self.doc.buffer()
    }

    #[must_use]
    pub fn document(&self) -> &Document<H> {
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
                        ..motion::resolve(buf, end, Motion::Right, None, 0, None, Bound::PastEnd)
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
                let effects = self.run(command, count);
                self.note_change(command, &consumed);
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
        effects.push(Effect::Edit(self.doc.replace(range, text)));
    }

    fn yank(&mut self, range: &Range<usize>, linewise: bool) {
        self.register = Register {
            text: self.buffer().text_in(range.clone()),
            linewise,
        };
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
                let bound = self.bound();
                match motion::resolve(
                    self.buffer(),
                    self.cursor,
                    target,
                    count,
                    self.sticky,
                    self.last_find,
                    bound,
                ) {
                    Some(landed) => {
                        self.cursor = landed;
                        self.remember_find(target);
                        self.update_sticky(target);
                    }
                    None => effects.push(Effect::Bell),
                }
            }

            Command::Operate { operator, target } => {
                self.remember_target_find(target);
                match self.span_of(operator, target, count) {
                    Some(span) => self.operate(operator, span, &mut effects),
                    None => effects.push(Effect::Bell),
                }
            }

            Command::SelectObject { scope, object } => {
                match motion::object_span(self.buffer(), self.cursor, scope, object) {
                    Some(span) => {
                        self.anchor = Some(span.range.start);
                        let end = motion::clamp(
                            self.buffer(),
                            span.range.end.saturating_sub(1),
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
                    self.leave_visual(&mut effects);
                } else {
                    self.anchor = Some(self.cursor);
                    self.set_mode(Mode::Visual(kind), &mut effects);
                }
            }

            Command::EnterNormal => {
                let leaving_insert = matches!(self.mode, Mode::Insert | Mode::Replace);
                self.close_insert_group();
                self.anchor = None;
                if leaving_insert {
                    // vi's insert cursor sits *between* characters, so leaving puts
                    // it on the character to the left. This has to happen before
                    // the mode switch: `set_mode` clamps to `OnChar`, and doing
                    // both would move the cursor twice.
                    self.cursor = self.step(self.cursor, Motion::Left, 1, Bound::PastEnd);
                }
                self.set_mode(Mode::Normal, &mut effects);
                if leaving_insert {
                    // The cursor moved, so the column `j`/`k` aim for has to follow
                    // it. Insert advances the sticky column with every character
                    // typed; leaving it stale here lands the next `j` one column to
                    // the right of where the cursor visibly is.
                    self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
                }
            }

            Command::DeleteChar { before } => {
                let buf = self.buffer();
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
                    let _ = buf;
                    self.yank(&range, false);
                    let start = range.start;
                    self.edit(range, "", &mut effects);
                    let at = motion::clamp(self.buffer(), start, Bound::OnChar);
                    self.place_cursor(at);
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
                    let at = motion::clamp(self.buffer(), end, Bound::OnChar);
                    self.place_cursor(at);
                }
            }

            Command::JoinRows => self.join_rows(repeat.max(2), &mut effects),

            Command::Put { before } => self.put(before, repeat, &mut effects),

            Command::Undo => {
                let revert = self.doc.undo();
                self.revert(&revert, &mut effects);
            }

            Command::Redo => {
                let revert = self.doc.redo();
                self.revert(&revert, &mut effects);
            }

            Command::UndoRow => {
                let revert = self.doc.undo_row();
                self.revert(&revert, &mut effects);
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

            Command::StopRecording => {
                if let Some((register, script)) = self.recording.take() {
                    self.macros.insert(register, script);
                    effects.push(Effect::RecordingStopped(register));
                }
            }

            Command::PlayMacro(register) => match self.macros.get(&register).cloned() {
                Some(script) => effects.extend(self.replay(&script, repeat)),
                None => effects.push(Effect::Bell),
            },

            Command::Scroll(scroll) => effects.push(Effect::Scroll(scroll)),
            Command::SearchPrompt { backward } => effects.push(Effect::SearchPrompt { backward }),
            Command::SearchRepeat { reverse } => effects.push(Effect::SearchRepeat { reverse }),
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

    fn remember_find(&mut self, motion: Motion) {
        if let Motion::Find {
            target,
            backward,
            till,
        } = motion
        {
            self.last_find = Some(Find {
                target,
                backward,
                till,
            });
        }
    }

    fn remember_target_find(&mut self, target: Target) {
        if let Target::Motion(motion) = target {
            self.remember_find(motion);
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
    fn place_cursor(&mut self, byte: usize) {
        self.cursor = byte;
        self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
    }

    fn update_sticky(&mut self, motion: Motion) {
        match motion {
            // Vertical movement consumes the sticky column without changing it.
            Motion::Up | Motion::Down => {}
            // `$` sticks to row ends, so subsequent `j` stays at the end.
            Motion::LastColumn => self.sticky = STICKY_END,
            _ => self.sticky = motion::grapheme_col(self.buffer(), self.cursor),
        }
    }

    /// Apply the outcome of an undo, redo or `U`.
    ///
    /// The caret goes back to where the history says it was. Failing that — a
    /// history that does not track it, or a change recorded outside a group — fall
    /// back to the last edit's site, which is at least where the text moved.
    fn revert(&mut self, revert: &Revert, effects: &mut Vec<Effect>) {
        if revert.is_empty() {
            effects.push(Effect::Bell);
            return;
        }
        for edit in &revert.edits {
            effects.push(Effect::Edit(*edit));
        }
        let at = revert
            .cursor
            .unwrap_or_else(|| revert.edits[revert.edits.len() - 1].start_byte);
        let at = motion::clamp(self.buffer(), at, self.bound());
        self.place_cursor(at);
    }

    // -- operators -------------------------------------------------------

    /// Resolve an operator's target to a span.
    fn span_of(&self, operator: Operator, target: Target, count: Option<usize>) -> Option<Span> {
        let buf = self.buffer();
        match target {
            Target::Motion(motion) => {
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

                let landed = motion::resolve(
                    buf,
                    self.cursor,
                    motion,
                    count,
                    self.sticky,
                    self.last_find,
                    Bound::OnChar,
                )?;
                if motion.is_linewise() {
                    let first = buf.byte_to_point(self.cursor.min(landed)).row;
                    let last = buf.byte_to_point(self.cursor.max(landed)).row;
                    return Some(Span {
                        range: motion::row_span(buf, first, last),
                        linewise: true,
                    });
                }
                let (start, mut end) = (self.cursor.min(landed), self.cursor.max(landed));
                if motion.is_inclusive() {
                    end = motion::resolve(buf, end, Motion::Right, None, 0, None, Bound::PastEnd)
                        .unwrap_or(end);
                }
                Some(Span {
                    range: start..end,
                    linewise: false,
                })
            }
            Target::CurrentRow => {
                let first = self.cursor_point().row;
                let last = first + count.unwrap_or(1) - 1;
                Some(Span {
                    range: motion::row_span(buf, first, last),
                    linewise: true,
                })
            }
            Target::Object { scope, object } => {
                motion::object_span(buf, self.cursor, scope, object)
            }
            Target::Selection => self.selection().map(|range| Span {
                range,
                linewise: self.mode == Mode::Visual(VisualKind::Line),
            }),
        }
    }

    fn operate(&mut self, operator: Operator, span: Span, effects: &mut Vec<Effect>) {
        let Span { range, linewise } = span;
        if range.is_empty() && operator != Operator::Change {
            effects.push(Effect::Bell);
            // Still drop the selection: a no-op operator must not strand the
            // editor in visual mode, or the next keystroke is interpreted against
            // a selection the user thinks they have dismissed.
            if self.mode.is_visual() {
                self.leave_visual(effects);
            }
            return;
        }
        self.yank(&range, linewise);
        let was_visual = self.mode.is_visual();

        match operator {
            Operator::Yank => {
                self.cursor = motion::clamp(self.buffer(), range.start, Bound::OnChar);
            }
            Operator::Delete => {
                let start = range.start;
                self.edit(range, "", effects);
                self.cursor = motion::clamp(self.buffer(), start, Bound::OnChar);
                if linewise {
                    self.cursor = self.step(self.cursor, Motion::FirstNonBlank, 1, Bound::OnChar);
                }
            }
            Operator::Change => {
                // Linewise change empties the rows but keeps one, so insert begins
                // on a blank row rather than joining the next one up.
                let range = if linewise {
                    let first = self.buffer().byte_to_point(range.start).row;
                    let last_row = self.buffer().byte_to_point(range.end.saturating_sub(1)).row;
                    self.buffer().row_range(first).start
                        ..self.buffer().row_content_range(last_row).end
                } else {
                    range
                };
                let start = range.start;
                self.edit(range, "", effects);
                self.cursor = start;
                self.open_insert_group();
                self.set_mode(Mode::Insert, effects);
            }
        }

        if was_visual && self.mode.is_visual() {
            self.leave_visual(effects);
        }
        self.sticky = motion::grapheme_col(self.buffer(), self.cursor);
    }

    fn leave_visual(&mut self, effects: &mut Vec<Effect>) {
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
            let at = if before {
                self.buffer().row_range(row).start
            } else {
                self.buffer().row_range(row).end
            };
            // Ensure the pasted block is newline-terminated so rows stay whole.
            let text = if text.ends_with('\n') {
                text
            } else {
                format!("{text}\n")
            };
            self.edit(at..at, &text, effects);
            self.cursor = self.step(at, Motion::FirstNonBlank, 1, Bound::OnChar);
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
    fn note_change(&mut self, command: Command, consumed: &[Key]) {
        match command {
            Command::EnterInsert(_)
            | Command::EnterReplace
            | Command::Operate {
                operator: Operator::Change,
                ..
            } => {
                self.change_keys = Some(consumed.to_vec());
            }
            Command::EnterNormal => {
                if let Some(script) = self.change_keys.take() {
                    self.last_change = script;
                }
            }
            // One-shot changes record immediately — but only outside a session,
            // whose keys are already accumulating.
            Command::Operate {
                operator: Operator::Delete,
                ..
            }
            | Command::DeleteChar { .. }
            | Command::ReplaceChar(_)
            | Command::JoinRows
            | Command::Put { .. }
            | Command::SwapCase
                if self.change_keys.is_none() =>
            {
                self.last_change = consumed.to_vec();
            }
            _ => {}
        }
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

    /// Type a script and return the resulting text. Reads like something you would
    /// actually type, which is the point.
    #[track_caller]
    fn typed(text: &str, script: &str) -> String {
        let mut editor = Editor::from_text(text);
        editor.type_keys(script).expect("valid key notation");
        editor.buffer().to_string()
    }

    #[track_caller]
    fn editor(text: &str, script: &str) -> Editor {
        let mut editor = Editor::from_text(text);
        editor.type_keys(script).expect("valid key notation");
        editor
    }

    const SQL: &str = "select id, name\nfrom users\nwhere id = 1";

    // -- movement --------------------------------------------------------

    #[test]
    fn basic_movement() {
        assert_eq!(editor(SQL, "lll").cursor(), 3);
        assert_eq!(editor(SQL, "jj").cursor_point(), Point::new(2, 0));
        assert_eq!(editor(SQL, "3l").cursor(), 3);
        assert_eq!(editor(SQL, "$").cursor(), 14);
        assert_eq!(editor(SQL, "$0").cursor(), 0);
        assert_eq!(editor(SQL, "G").cursor_point(), Point::new(2, 0));
        assert_eq!(editor(SQL, "Ggg").cursor(), 0);
        assert_eq!(editor(SQL, "2G").cursor_point(), Point::new(1, 0));
    }

    #[test]
    fn dollar_sticks_to_row_ends() {
        // After `$`, `j` stays at the end even on a longer row.
        let ed = editor("ab\nlonger row\nxy", "$jj");
        assert_eq!(ed.cursor_point(), Point::new(2, 1));
        let ed = editor("ab\nlonger row\nxy", "$j");
        assert_eq!(ed.cursor_point(), Point::new(1, 9));
    }

    #[test]
    fn the_cursor_cannot_rest_past_the_last_character() {
        let ed = editor("ab", "lll");
        assert_eq!(ed.cursor(), 1);
    }

    // -- operators -------------------------------------------------------

    #[test]
    fn delete_word() {
        assert_eq!(typed(SQL, "dw"), "id, name\nfrom users\nwhere id = 1");
        assert_eq!(typed(SQL, "d2w"), ", name\nfrom users\nwhere id = 1");
        assert_eq!(typed(SQL, "2dw"), ", name\nfrom users\nwhere id = 1");
    }

    #[test]
    fn delete_row_and_counts() {
        assert_eq!(typed(SQL, "dd"), "from users\nwhere id = 1");
        assert_eq!(typed(SQL, "2dd"), "where id = 1");
        assert_eq!(typed(SQL, "jdd"), "select id, name\nwhere id = 1");
    }

    #[test]
    fn deleting_the_last_row_leaves_no_blank() {
        assert_eq!(typed(SQL, "Gdd"), "select id, name\nfrom users");
    }

    #[test]
    fn delete_to_row_end_and_find() {
        assert_eq!(typed(SQL, "d$"), "\nfrom users\nwhere id = 1");
        // `df,` includes the comma.
        assert_eq!(typed(SQL, "df,"), " name\nfrom users\nwhere id = 1");
        // `dt,` stops before it.
        assert_eq!(typed(SQL, "dt,"), ", name\nfrom users\nwhere id = 1");
    }

    #[test]
    fn delete_is_linewise_over_a_linewise_motion() {
        assert_eq!(typed(SQL, "dj"), "where id = 1");
    }

    #[test]
    fn text_objects() {
        assert_eq!(typed(SQL, "diw"), " id, name\nfrom users\nwhere id = 1");
        assert_eq!(typed(SQL, "daw"), "id, name\nfrom users\nwhere id = 1");
        assert_eq!(
            typed("where name = 'dave'", "fdci'bob<Esc>"),
            "where name = 'bob'"
        );
        // `i(` needs the cursor inside or on a delimiter, as in vi; on the `f` it
        // resolves to nothing.
        assert_eq!(typed("f(a, b)", "lci(x<Esc>"), "f(x)");
    }

    #[test]
    fn change_word_behaves_like_change_to_word_end() {
        // vi's famous irregularity: `cw` must not swallow the following space.
        assert_eq!(
            typed(SQL, "cwSELECT<Esc>"),
            "SELECT id, name\nfrom users\nwhere id = 1"
        );
    }

    #[test]
    fn linewise_change_keeps_the_row() {
        assert_eq!(
            typed(SQL, "ccselect 1<Esc>"),
            "select 1\nfrom users\nwhere id = 1"
        );
    }

    #[test]
    fn yank_and_put() {
        // Charwise.
        assert_eq!(
            typed(SQL, "yiwP"),
            "selectselect id, name\nfrom users\nwhere id = 1"
        );
        // Linewise put lands on a new row.
        assert_eq!(
            typed(SQL, "yyp"),
            "select id, name\nselect id, name\nfrom users\nwhere id = 1"
        );
        assert_eq!(
            typed(SQL, "yyP"),
            "select id, name\nselect id, name\nfrom users\nwhere id = 1"
        );
    }

    #[test]
    fn delete_then_put_moves_text() {
        assert_eq!(
            typed(SQL, "ddp"),
            "from users\nselect id, name\nwhere id = 1"
        );
    }

    // -- simple edits ----------------------------------------------------

    #[test]
    fn simple_edits() {
        assert_eq!(typed(SQL, "x"), "elect id, name\nfrom users\nwhere id = 1");
        assert_eq!(typed(SQL, "3x"), "ect id, name\nfrom users\nwhere id = 1");
        assert_eq!(typed(SQL, "lX"), "elect id, name\nfrom users\nwhere id = 1");
        assert_eq!(
            typed(SQL, "rS"),
            "Select id, name\nfrom users\nwhere id = 1"
        );
        assert_eq!(
            typed(SQL, "3rx"),
            "xxxect id, name\nfrom users\nwhere id = 1"
        );
        assert_eq!(typed(SQL, "~"), "Select id, name\nfrom users\nwhere id = 1");
        assert_eq!(
            typed(SQL, "6~"),
            "SELECT id, name\nfrom users\nwhere id = 1"
        );
    }

    #[test]
    fn join_rows() {
        assert_eq!(typed(SQL, "J"), "select id, name from users\nwhere id = 1");
        assert_eq!(typed("a\n    b", "J"), "a b");
        assert_eq!(typed(SQL, "3J"), "select id, name from users where id = 1");
    }

    // -- insert mode -----------------------------------------------------

    #[test]
    fn insert_modes() {
        assert_eq!(
            typed(SQL, "iX<Esc>"),
            "Xselect id, name\nfrom users\nwhere id = 1"
        );
        assert_eq!(
            typed(SQL, "aX<Esc>"),
            "sXelect id, name\nfrom users\nwhere id = 1"
        );
        assert_eq!(
            typed(SQL, "AX<Esc>"),
            "select id, nameX\nfrom users\nwhere id = 1"
        );
        assert_eq!(typed("  x", "IY<Esc>"), "  Yx");
        assert_eq!(
            typed(SQL, "oX<Esc>"),
            "select id, name\nX\nfrom users\nwhere id = 1"
        );
        assert_eq!(
            typed(SQL, "OX<Esc>"),
            "X\nselect id, name\nfrom users\nwhere id = 1"
        );
    }

    #[test]
    fn escape_leaves_the_cursor_on_the_last_typed_character() {
        let ed = editor("", "iabc<Esc>");
        assert_eq!(ed.mode(), Mode::Normal);
        assert_eq!(ed.cursor(), 2);
    }

    #[test]
    fn escape_refreshes_the_sticky_column() {
        // Insert advances the sticky column with the cursor, so leaving insert has
        // to pull it back too — otherwise `j` remembers where the cursor *was*
        // before `<Esc>` corrected it, and lands one column to the right.
        let ed = editor("xx\nyyyyy", "iabc<Esc>");
        assert_eq!(ed.cursor_point(), Point { row: 0, col: 2 });

        let ed = editor("xx\nyyyyy", "iabc<Esc>j");
        assert_eq!(ed.cursor_point(), Point { row: 1, col: 2 });

        // Two rows down and back up must not drift either.
        let ed = editor("xx\nyyyyy\nzzzzz", "iabc<Esc>jjk");
        assert_eq!(ed.cursor_point(), Point { row: 1, col: 2 });
    }

    #[test]
    fn edits_that_move_the_cursor_refresh_the_sticky_column() {
        // Every one of these repositions the cursor without going through a motion,
        // so each has to bring the remembered column along with it.

        // `x` at the end of a row pulls the cursor back a column.
        let ed = editor("ab\nwxyz", "lxj");
        assert_eq!(ed.cursor_point(), Point { row: 1, col: 0 });

        // `3~` advances past what it swapped.
        let ed = editor("abc\nwxyz", "3~j");
        assert_eq!(ed.cursor_point(), Point { row: 1, col: 2 });

        // A visual text object moves the cursor to the object's end.
        let ed = editor("abc def\nwxyzwxyz", "viwj");
        assert_eq!(ed.cursor_point(), Point { row: 1, col: 2 });
    }

    #[test]
    fn insert_editing_keys() {
        assert_eq!(typed("", "iab<BS>c<Esc>"), "ac");
        assert_eq!(typed("", "ione two<C-w>three<Esc>"), "one three");
        assert_eq!(typed("", "ia<CR>b<Esc>"), "a\nb");
    }

    #[test]
    fn backspace_crosses_a_row_boundary() {
        // `h` will not leave a row, but backspace must.
        assert_eq!(typed("ab\ncd", "ji<BS><Esc>"), "abcd");
    }

    #[test]
    fn replace_mode_overwrites() {
        assert_eq!(typed("abcdef", "RXY<Esc>"), "XYcdef");
        // And extends at the row end rather than eating the newline.
        assert_eq!(typed("ab\ncd", "$RXYZ<Esc>"), "aXYZ\ncd");
    }

    // -- visual mode -----------------------------------------------------

    #[test]
    fn visual_char_selection_is_inclusive() {
        let ed = editor(SQL, "vll");
        assert_eq!(ed.selection(), Some(0..3));
        assert_eq!(typed(SQL, "vlld"), "ect id, name\nfrom users\nwhere id = 1");
    }

    #[test]
    fn visual_line_selection() {
        assert_eq!(typed(SQL, "Vd"), "from users\nwhere id = 1");
        assert_eq!(typed(SQL, "Vjd"), "where id = 1");
        let ed = editor(SQL, "Vj");
        assert_eq!(ed.selection(), Some(0..27));
    }

    #[test]
    fn visual_operators_return_to_normal_mode() {
        let ed = editor(SQL, "vlld");
        assert_eq!(ed.mode(), Mode::Normal);
        assert_eq!(ed.selection(), None);
    }

    #[test]
    fn visual_toggles_off() {
        let ed = editor(SQL, "vv");
        assert_eq!(ed.mode(), Mode::Normal);
        let ed = editor(SQL, "v<Esc>");
        assert_eq!(ed.mode(), Mode::Normal);
    }

    #[test]
    fn visual_object_selection() {
        let ed = editor(SQL, "viw");
        assert_eq!(ed.selection(), Some(0..6));
        assert_eq!(typed(SQL, "viwd"), " id, name\nfrom users\nwhere id = 1");
    }

    #[test]
    fn visual_change_enters_insert() {
        let ed = editor(SQL, "viwc");
        assert_eq!(ed.mode(), Mode::Insert);
        assert_eq!(
            typed(SQL, "viwcSELECT<Esc>"),
            "SELECT id, name\nfrom users\nwhere id = 1"
        );
    }

    // -- history ---------------------------------------------------------

    #[test]
    fn undo_and_redo() {
        assert_eq!(typed(SQL, "ddu"), SQL);
        assert_eq!(typed(SQL, "ddu<C-r>"), "from users\nwhere id = 1");
    }

    #[test]
    fn undo_puts_the_caret_back_where_the_change_started() {
        // `o` appends a newline at the *end* of the row, so the edit's own geometry
        // says nothing useful about where the user was. Only the caret the history
        // bracketed the group with gets this right.
        let ed = editor("select id\nfrom users", "lllo-- note<Esc>u");
        assert_eq!(ed.buffer().to_string(), "select id\nfrom users");
        assert_eq!(ed.cursor_point(), Point { row: 0, col: 3 });

        // With the caret *inside* a word, `ciw` starts the change before it. The
        // caret has to come back to where it was, not to where the edit began.
        let ed = editor("select id, name", "wwwllciwX<Esc>u");
        assert_eq!(ed.buffer().to_string(), "select id, name");
        assert_eq!(ed.cursor_point(), Point { row: 0, col: 13 });

        // `O` opens above, so undoing it must not leave the caret on the row that
        // shifted down.
        let ed = editor("aaa\nbbb", "jlO-- note<Esc>u");
        assert_eq!(ed.buffer().to_string(), "aaa\nbbb");
        assert_eq!(ed.cursor_point(), Point { row: 1, col: 1 });
    }

    #[test]
    fn redo_puts_the_caret_where_the_change_left_it() {
        let ed = editor("select id\nfrom users", "lllo-- note<Esc>u<C-r>");
        assert_eq!(ed.buffer().to_string(), "select id\n-- note\nfrom users");
        // Where `<Esc>` left it: on the last typed character.
        assert_eq!(ed.cursor_point(), Point { row: 1, col: 6 });
    }

    #[test]
    fn an_insert_session_undoes_as_one_step() {
        assert_eq!(typed("", "ihello world<Esc>u"), "");
        let ed = editor("", "ihello<Esc>");
        assert_eq!(ed.document().history().undo_depth(), 1);
    }

    #[test]
    fn change_and_its_typed_text_are_one_undo_step() {
        assert_eq!(typed(SQL, "cwSELECT<Esc>u"), SQL);
    }

    #[test]
    fn u_restores_a_row_after_several_changes() {
        // Two separate changes on row 0, then `U`.
        assert_eq!(typed(SQL, "xxx U"), SQL);
    }

    #[test]
    fn u_only_touches_the_last_changed_row() {
        let text = "aaa\nbbb";
        // Change row 0, then row 1, then `U` — only row 1 comes back.
        assert_eq!(typed(text, "xjxU"), "aa\nbbb");
    }

    #[test]
    fn u_is_undone_by_lowercase_u() {
        assert_eq!(
            typed(SQL, "xUu"),
            "elect id, name\nfrom users\nwhere id = 1"
        );
    }

    #[test]
    fn undo_with_nothing_to_undo_rings() {
        let mut ed = Editor::from_text(SQL);
        assert_eq!(ed.type_keys("u").unwrap(), vec![Effect::Bell]);
    }

    // -- dot repeat ------------------------------------------------------

    #[test]
    fn dot_repeats_a_delete() {
        assert_eq!(typed(SQL, "dw."), ", name\nfrom users\nwhere id = 1");
        assert_eq!(typed(SQL, "dd."), "where id = 1");
    }

    #[test]
    fn dot_repeats_a_change_including_the_typed_text() {
        // The reason `.` stores keys rather than commands.
        assert_eq!(typed("one two", "ciwX<Esc>wciwY<Esc>"), "X Y");
        assert_eq!(typed("one two", "ciwX<Esc>w."), "X X");
    }

    #[test]
    fn dot_repeats_an_insert_session() {
        assert_eq!(typed("", "iab<Esc>."), "aabb");
    }

    #[test]
    fn dot_with_a_count_repeats_that_many_times() {
        assert_eq!(typed("a b c d e", "dw3."), "e");
    }

    #[test]
    fn dot_does_not_repeat_movement_or_undo() {
        let ed = editor(SQL, "x");
        let recorded = ed.last_change().to_vec();
        let ed2 = editor(SQL, "xjjllu");
        assert_eq!(ed2.last_change(), recorded.as_slice());
    }

    #[test]
    fn dot_with_nothing_recorded_rings() {
        let mut ed = Editor::from_text(SQL);
        assert_eq!(ed.type_keys(".").unwrap(), vec![Effect::Bell]);
    }

    // -- macros ----------------------------------------------------------

    #[test]
    fn record_and_play_a_macro() {
        // Uppercase the first letter of each row. The `0` is load-bearing: `~`
        // advances the cursor, so the macro has to come back to the first column
        // before stepping down.
        assert_eq!(typed("aa\nbb\ncc", "qa~0jq@a@a"), "Aa\nBb\nCc");
    }

    #[test]
    fn a_macro_replays_with_a_count() {
        assert_eq!(typed("aa\nbb\ncc", "qa~0jq2@a"), "Aa\nBb\nCc");
    }

    #[test]
    fn swap_case_carries_the_column_with_it() {
        // Without the `0`, the run walks diagonally, because `~` leaves the cursor
        // one column further right and `j` keeps that column. vi does the same.
        assert_eq!(typed("aa\nbb\ncc", "qa~jq@a@a"), "Aa\nbB\ncC");
    }

    #[test]
    fn recording_reports_itself() {
        let mut ed = Editor::from_text(SQL);
        assert_eq!(
            ed.type_keys("qa").unwrap(),
            vec![Effect::RecordingStarted('a')]
        );
        assert_eq!(ed.recording(), Some('a'));
        assert_eq!(
            ed.type_keys("q").unwrap(),
            vec![Effect::RecordingStopped('a')]
        );
        assert_eq!(ed.recording(), None);
    }

    #[test]
    fn the_closing_q_is_not_part_of_the_macro() {
        let mut ed = Editor::from_text("abc");
        ed.type_keys("qax q").unwrap();
        // Recorded `x` and a space motion, not the terminating `q`.
        ed.type_keys("@a").unwrap();
        assert!(!ed.buffer().to_string().contains('q'));
    }

    #[test]
    fn playing_an_unrecorded_macro_rings() {
        let mut ed = Editor::from_text(SQL);
        assert_eq!(ed.type_keys("@z").unwrap(), vec![Effect::Bell]);
    }

    #[test]
    fn a_self_referential_macro_terminates() {
        let mut ed = Editor::from_text("abc");
        // Record `@a` into `a`, then play it. Must hit the depth guard, not the
        // stack.
        ed.type_keys("qa@aq").unwrap();
        let effects = ed.type_keys("@a").unwrap();
        assert!(effects.contains(&Effect::Bell));
    }

    // -- effects ---------------------------------------------------------

    #[test]
    fn typing_reports_one_edit_per_keystroke() {
        let mut ed = Editor::from_text("");
        let effects = ed.type_keys("iabc").unwrap();
        let edits: Vec<_> = effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Edit(_)))
            .collect();
        // Three edits for the parser...
        assert_eq!(edits.len(), 3);
        // ...and one undo step for the user.
        ed.type_keys("<Esc>").unwrap();
        assert_eq!(ed.document().history().undo_depth(), 1);
    }

    #[test]
    fn edits_carry_tree_sitter_geometry() {
        let mut ed = Editor::from_text(SQL);
        let effects = ed.type_keys("jdd").unwrap();
        let Some(Effect::Edit(edit)) = effects
            .into_iter()
            .find(|effect| matches!(effect, Effect::Edit(_)))
        else {
            panic!("expected an edit");
        };
        assert_eq!(edit.start_byte, 16);
        assert_eq!(edit.old_end_byte, 27);
        assert_eq!(edit.new_end_byte, 16);
        assert_eq!(edit.start_point, Point::new(1, 0));
        assert_eq!(edit.old_end_point, Point::new(2, 0));
    }

    #[test]
    fn mode_changes_are_reported() {
        let mut ed = Editor::from_text(SQL);
        assert_eq!(
            ed.type_keys("i").unwrap(),
            vec![Effect::ModeChanged(Mode::Insert)]
        );
        assert_eq!(
            ed.type_keys("<Esc>").unwrap(),
            vec![Effect::ModeChanged(Mode::Normal)]
        );
    }

    #[test]
    fn viewport_and_prompts_are_delegated() {
        let mut ed = Editor::from_text(SQL);
        assert_eq!(
            ed.type_keys("zz").unwrap(),
            vec![Effect::Scroll(Scroll::Center)]
        );
        assert_eq!(
            ed.type_keys("<C-d>").unwrap(),
            vec![Effect::Scroll(Scroll::HalfPageDown)]
        );
        assert_eq!(
            ed.type_keys("/").unwrap(),
            vec![Effect::SearchPrompt { backward: false }]
        );
        assert_eq!(ed.type_keys(":").unwrap(), vec![Effect::CommandPrompt]);
        assert_eq!(
            ed.type_keys("n").unwrap(),
            vec![Effect::SearchRepeat { reverse: false }]
        );
    }

    #[test]
    fn invalid_sequences_ring() {
        let mut ed = Editor::from_text(SQL);
        assert_eq!(ed.type_keys("d!").unwrap(), vec![Effect::Bell]);
        // A cancelled sequence is silent.
        assert_eq!(ed.type_keys("d<Esc>").unwrap(), Vec::new());
    }

    #[test]
    fn showcmd_exposes_partial_input() {
        let mut ed = Editor::from_text(SQL);
        ed.type_keys("2d").unwrap();
        assert_eq!(crate::render(ed.pending_keys()), "2d");
        ed.type_keys("w").unwrap();
        assert!(ed.pending_keys().is_empty());
    }

    // -- extensibility ---------------------------------------------------

    #[test]
    fn a_rebound_key_changes_behaviour_end_to_end() {
        use crate::keymap::{Binding, Layer};
        let mut keymap = Keymap::vim();
        // Swap `j` and `k`.
        keymap
            .bind_spec(Layer::Normal, "j", Binding::Motion(Motion::Up))
            .bind_spec(Layer::Normal, "k", Binding::Motion(Motion::Down));
        let mut ed = Editor::with(SQL, keymap, LinearHistory::new());
        ed.type_keys("k").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(1, 0));
        // And the operator layer inherits it, so `dj` follows the remap.
        ed.type_keys("gg").unwrap();
        ed.type_keys("dk").unwrap();
        assert_eq!(ed.buffer().to_string(), "where id = 1");
    }

    #[test]
    fn an_editor_without_history_still_edits() {
        use crate::history::NoHistory;
        let mut ed = Editor::with(SQL, Keymap::vim(), NoHistory);
        ed.type_keys("dd").unwrap();
        assert_eq!(ed.buffer().to_string(), "from users\nwhere id = 1");
        assert_eq!(ed.type_keys("u").unwrap(), vec![Effect::Bell]);
    }

    // -- robustness ------------------------------------------------------

    #[test]
    fn an_empty_buffer_survives_everything() {
        for script in [
            "x", "X", "dd", "dw", "diw", "D", "p", "P", "J", "~", "u", "<C-r>", "U", ".", "$", "G",
            "gg", "vd", "Vd", "ciw<Esc>", "rx", "@a",
        ] {
            let mut ed = Editor::new();
            let _ = ed.type_keys(script);
            assert_eq!(ed.buffer().to_string(), "", "`{script}` on an empty buffer");
            assert_eq!(ed.cursor(), 0);
        }
    }

    #[test]
    fn multibyte_text_survives_editing() {
        // `é` as e + combining acute: one grapheme, two chars, three bytes.
        let mut ed = Editor::from_text("caf\u{65}\u{301} au lait");
        ed.type_keys("3lx").unwrap();
        // Removed the whole grapheme, not half of it.
        assert_eq!(ed.buffer().to_string(), "caf au lait");
        assert!(ed.buffer().to_string().is_char_boundary(3));
    }

    #[test]
    fn cursor_stays_on_a_char_boundary_after_every_key() {
        let mut ed = Editor::from_text("aé\u{301}b\ncafé\nx");
        for script in ["l", "l", "j", "$", "j", "k", "x", "w", "b", "e", "dw", "u"] {
            ed.type_keys(script).unwrap();
            let text = ed.buffer().to_string();
            assert!(
                text.is_char_boundary(ed.cursor()),
                "cursor {} off boundary after `{script}` in {text:?}",
                ed.cursor()
            );
        }
    }
}
