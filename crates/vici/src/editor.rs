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
use crate::host::{Indent, Viewport};
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
    /// The mode a `<C-o>` will hand back to once its one command has run.
    resume: Option<Mode>,
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
            indent: Indent::default(),
            viewport: Viewport::default(),
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
            resume: None,
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
    pub fn document(&self) -> &Document<H> {
        &self.doc
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The mode a `<C-o>` is holding open, if one is in flight.
    ///
    /// [`Self::mode`] reports [`Mode::Normal`] during a `<C-o>`, because that is
    /// the grammar in force. This is how a host knows to show vi's
    /// `-- (insert) --` instead of plain `-- INSERT --`.
    #[must_use]
    pub fn resuming(&self) -> Option<Mode> {
        self.resume
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
                        ..motion::resolve(
                            buf,
                            end,
                            Motion::Right,
                            None,
                            0,
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

        // Whether a `<C-o>` was already in flight *before* this key, so that the
        // `<C-o>` itself does not immediately spend the turn it just bought.
        let one_shot = self.resume.is_some();
        match self.pending.feed(key, self.mode, &self.keymap) {
            Resolution::Pending => Vec::new(),
            // `<C-o>d<Esc>` abandons the command. Spend the `<C-o>` on it rather
            // than leaving it armed for whatever gets typed next.
            Resolution::Cancelled { .. } => self.spend_one_shot(one_shot, Vec::new()),
            // A `<C-o>` aimed at a key that means nothing beeps and hands you back,
            // rather than stranding you in a mode you never asked for.
            Resolution::Rejected { .. } => self.spend_one_shot(one_shot, vec![Effect::Bell]),
            Resolution::Command {
                command,
                count,
                keys: consumed,
            } => {
                let effects = self.run(command, count);
                self.note_change(command, &consumed);
                self.spend_one_shot(one_shot, effects)
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
        self.resume = None;
        edit
    }

    // -- execution -------------------------------------------------------

    fn bound(&self) -> Bound {
        match self.mode {
            Mode::Insert | Mode::Replace => Bound::PastEnd,
            // A `<C-o>` command keeps insert's past-the-end caret, so typing at the
            // end of a row and reaching for one command comes back to the column
            // you left rather than one short of it.
            Mode::Normal if self.resume.is_some() => Bound::PastEnd,
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
        let buf = self.buffer();
        let text = if linewise {
            // A linewise register holds whole rows, newline-terminated. The span's
            // own text will not do: on the last row it opens with the newline `dd`
            // needs to take, and `p` would paste that as a blank row.
            let (first, last) = motion::span_rows(buf, range);
            let mut text = buf.text_in(buf.row_range(first).start..buf.row_range(last).end);
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text
        } else {
            buf.text_in(range.clone())
        };
        self.register = Register { text, linewise };
    }

    /// Where the caret belongs after a linewise operator: the start of the first
    /// row the span covered.
    ///
    /// Not `range.start`, which is the newline ending the row *above* when the
    /// span reaches the last row — see [`motion::span_rows`].
    fn linewise_home(&self, range: &Range<usize>) -> usize {
        let (first, _) = motion::span_rows(self.buffer(), range);
        self.buffer().row_content_range(first).start
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

        // TODO: Record page and screen motions in the jump list once it exists.
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
                    self.viewport,
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

            Command::OneShotNormal => {
                // Bound in the insert layer alone, so the mode recorded here is
                // always the one to come back to.
                self.resume = Some(self.mode);
                // The session's undo group closes: vi breaks the undo sequence at a
                // `<C-o>`, so what you typed before it, the command itself, and what
                // you type after are three steps. `finish_one_shot` reopens it.
                self.close_insert_group();
                // Deliberately none of `EnterNormal`'s leaving-insert work — no step
                // left, no sticky refresh. Staying put is the whole point.
                self.set_mode(Mode::Normal, &mut effects);
            }

            Command::EnterNormal => {
                // `<C-o><Esc>` leaves insert for good rather than resuming it, and
                // the caret is still sitting where insert left it — so this counts
                // as leaving insert even though the mode is already Normal.
                let leaving_insert = matches!(self.mode, Mode::Insert | Mode::Replace)
                    || self.resume.take().is_some();
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
                    self.cursor = self.step(self.cursor, motion, rows, self.bound());
                }
                effects.push(Effect::Scroll(scroll));
            }
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

    /// The concrete find `;` or `,` stands for, given what was remembered.
    ///
    /// Any other motion is already concrete and passes through. Without this,
    /// [`Motion::is_inclusive`] has no direction to answer from and falls back to
    /// exclusive, which silently costs `d;` a character.
    fn effective(&self, motion: Motion) -> Motion {
        match (motion, self.last_find) {
            (Motion::RepeatFind { reverse }, Some(find)) => Motion::Find {
                target: find.target,
                backward: find.backward != reverse,
                till: find.till,
            },
            _ => motion,
        }
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
    ///
    /// `byte` is clamped to whatever the current mode allows, so a caller that has
    /// just deleted the text it was aiming at need not work out where the end of the
    /// row went — and a `<C-o>` command keeps insert's past-the-end caret.
    fn place_cursor(&mut self, byte: usize) {
        self.cursor = motion::clamp(self.buffer(), byte, self.bound());
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
        self.place_cursor(at);
    }

    // -- operators -------------------------------------------------------

    /// Resolve an operator's target to a span.
    fn span_of(&self, operator: Operator, target: Target, count: Option<usize>) -> Option<Span> {
        let buf = self.buffer();
        let span = match target {
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

                // Resolution keeps the `RepeatFind` form, because that is what tells
                // `motion::resolve` to skip the target a `t` is already parked on.
                // Operator semantics have to come from the concrete find it stands
                // for, or `d;` after `f,` stops one character short.
                let semantics = self.effective(motion);
                // An exclusive motion's landing place is the span's *end boundary*,
                // not somewhere the cursor has to be able to sit, so it may be one
                // past the last character — otherwise `dw` on the last word of the
                // file leaves its final character behind. An inclusive motion does
                // land on a character, and extends over it below.
                let bound = if semantics.is_inclusive() {
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
                    self.viewport,
                    bound,
                )?;
                if semantics.is_linewise() {
                    let first = buf.byte_to_point(self.cursor.min(landed)).row;
                    let last = buf.byte_to_point(self.cursor.max(landed)).row;
                    Span {
                        range: motion::row_span(buf, first, last),
                        linewise: true,
                    }
                } else {
                    let (start, mut end) = (self.cursor.min(landed), self.cursor.max(landed));
                    if semantics.is_inclusive() {
                        end = motion::resolve(
                            buf,
                            end,
                            Motion::Right,
                            None,
                            0,
                            None,
                            self.viewport,
                            Bound::PastEnd,
                        )
                        .unwrap_or(end);
                    }
                    Span {
                        range: start..end,
                        linewise: false,
                    }
                }
            }
            Target::CurrentRow => {
                let first = self.cursor_point().row;
                let last = first + count.unwrap_or(1) - 1;
                Span {
                    range: motion::row_span(buf, first, last),
                    linewise: true,
                }
            }
            Target::Object { scope, object } => {
                motion::object_span(buf, self.cursor, scope, object, count.unwrap_or(1))?
            }
            Target::Selection => self.selection().map(|range| Span {
                range,
                linewise: self.mode == Mode::Visual(VisualKind::Line),
            })?,
        };
        if operator.forces_linewise() {
            // Via `span_rows`, because a span already covering the last row starts
            // on the newline of the row above it: reading the row off that byte
            // would widen the shift by a row it was never aimed at.
            let (first, last) = motion::span_rows(buf, &span.range);
            Some(Span {
                range: motion::row_span(buf, first, last),
                linewise: true,
            })
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
        let Span { range, linewise } = span;
        if range.is_empty() && operator != Operator::Change && !operator.forces_linewise() {
            effects.push(Effect::Bell);
            // Still drop the selection: a no-op operator must not strand the
            // editor in visual mode, or the next keystroke is interpreted against
            // a selection the user thinks they have dismissed.
            if self.mode.is_visual() {
                self.leave_visual(effects);
            }
            return;
        }
        if operator.yanks() {
            self.yank(&range, linewise);
        }
        let was_visual = self.mode.is_visual();

        match operator {
            Operator::ShiftRight | Operator::ShiftLeft => {
                let (first, last) = motion::span_rows(self.buffer(), &range);
                self.shift_rows(first, last, operator, amount, effects);
                self.cursor = self.buffer().row_content_range(first).start;
                self.cursor = self.step(self.cursor, Motion::FirstNonBlank, 1, Bound::OnChar);
            }
            Operator::Lower | Operator::Upper | Operator::SwapCase => {
                let text = self.buffer().text_in(range.clone());
                let recased = recase(&text, operator);
                let start = if linewise {
                    self.linewise_home(&range)
                } else {
                    range.start
                };
                self.edit(range, &recased, effects);
                self.cursor = motion::clamp(self.buffer(), start, self.bound());
                if linewise {
                    self.cursor = self.step(self.cursor, Motion::FirstNonBlank, 1, Bound::OnChar);
                }
            }
            Operator::Yank => {
                let home = if linewise {
                    self.linewise_home(&range)
                } else {
                    range.start
                };
                self.cursor = motion::clamp(self.buffer(), home, self.bound());
            }
            Operator::Delete => {
                let start = range.start;
                self.edit(range, "", effects);
                // `self.bound()`, not `OnChar`: an operator run from a `<C-o>` leaves
                // the caret where insert wants it, which at the end of a row is one
                // column further along than normal mode would allow.
                self.cursor = motion::clamp(self.buffer(), start, self.bound());
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

    /// Hand control back to insert once a `<C-o>` command has had its turn.
    ///
    /// `armed` is read before the key was fed, so the `<C-o>` that set it up is not
    /// the command that consumes it.
    fn spend_one_shot(&mut self, armed: bool, mut effects: Vec<Effect>) -> Vec<Effect> {
        if !armed {
            return effects;
        }
        // Gone already: `<C-o><Esc>` took it on the way out and meant it.
        let Some(mode) = self.resume.take() else {
            return effects;
        };
        // A command that opened a mode of its own supersedes the plan: `<C-o>cw` is
        // already inserting, `<C-o>v` is selecting. Neither should be overridden.
        if self.mode == Mode::Normal {
            self.open_insert_group();
            self.set_mode(mode, &mut effects);
        }
        effects
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
    fn viewport_pages_carry_the_cursor_when_the_host_has_reported_one() {
        let viewport = Viewport {
            top_row: 0,
            height: 6,
        };
        let mut ed = Editor::from_text("0\n1\n2\n3\n4\n5\n6\n7").with_viewport(viewport);
        ed.type_keys("j<C-d>").unwrap();
        assert_eq!(ed.cursor_point().row, 4);

        let mut ed = Editor::from_text("0\n1\n2\n3\n4\n5\n6\n7").with_viewport(viewport);
        ed.type_keys("j<C-f>").unwrap();
        // Full pages retain two rows of overlap, as vi does.
        assert_eq!(ed.cursor_point().row, 5);
    }

    #[test]
    fn viewport_is_a_host_supplied_fact() {
        let mut ed = Editor::from_text("one");
        assert_eq!(ed.viewport(), Viewport::default());
        ed.set_viewport(Viewport {
            top_row: 7,
            height: 9,
        });
        assert_eq!(ed.viewport().top_row, 7);
        assert_eq!(ed.viewport().height, 9);
    }

    #[test]
    fn viewport_pages_preserve_the_zero_height_scroll_only_contract() {
        for key in ["<C-d>", "<C-u>", "<C-f>", "<C-b>"] {
            let mut ed = Editor::from_text("0\n1\n2");
            ed.type_keys("j").unwrap();
            let effects = ed.type_keys(key).unwrap();
            assert_eq!(ed.cursor_point(), Point::new(1, 0), "{key}");
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Scroll(_))),
                "{key} must still tell the host to scroll"
            );
        }
    }

    #[test]
    fn viewport_pages_clamp_at_both_buffer_ends() {
        let viewport = Viewport {
            top_row: 0,
            height: 6,
        };
        let mut ed = Editor::from_text("0\n1\n2").with_viewport(viewport);
        ed.type_keys("<C-u><C-b>").unwrap();
        assert_eq!(ed.cursor_point().row, 0);
        ed.type_keys("G<C-d><C-f>").unwrap();
        assert_eq!(ed.cursor_point().row, 2);
    }

    #[test]
    fn viewport_page_moves_keep_dollars_sticky_column() {
        let mut ed = Editor::from_text("xx\nlong second\nlong third\nx").with_viewport(Viewport {
            top_row: 0,
            height: 4,
        });
        ed.type_keys("$<C-d>").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(2, 9));
    }

    #[test]
    fn screen_motions_follow_the_reported_viewport() {
        let text = "zero\n one\n two\n three\n four\n five\n six\nseven";
        let viewport = Viewport {
            top_row: 2,
            height: 5,
        };
        let mut ed = Editor::from_text(text).with_viewport(viewport);
        ed.type_keys("H").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(2, 1));
        ed.type_keys("2H").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(3, 1));
        ed.type_keys("M").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(4, 1));
        ed.type_keys("3M").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(4, 1));
        ed.type_keys("L").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(6, 1));
        ed.type_keys("2L").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(5, 1));
    }

    #[test]
    fn screen_motions_are_linewise_operator_targets() {
        let viewport = Viewport {
            top_row: 2,
            height: 3,
        };
        let mut ed = Editor::from_text("0\n1\n2\n3\n4\n5").with_viewport(viewport);
        ed.type_keys("GdH").unwrap();
        assert_eq!(ed.buffer().to_string(), "0\n1");

        let mut ed = Editor::from_text("0\n1\n2\n3\n4\n5").with_viewport(viewport);
        ed.type_keys("dL").unwrap();
        assert_eq!(ed.buffer().to_string(), "5");
    }

    #[test]
    fn screen_motions_bell_without_a_viewport() {
        for key in ["H", "M", "L"] {
            let mut ed = Editor::from_text("zero\none");
            let effects = ed.type_keys(key).unwrap();
            assert_eq!(effects, vec![Effect::Bell], "{key}");
        }
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
    fn shift_operators_are_linewise_and_repeatable() {
        assert_eq!(typed("one\ntwo\nthree", ">>"), "    one\ntwo\nthree");
        assert_eq!(typed("    one\ntwo", "<lt><lt>"), "one\ntwo");
        assert_eq!(
            typed("one\ntwo\nthree", "3>>"),
            "    one\n    two\n    three"
        );
        assert_eq!(typed("one\ntwo\nthree", ">j"), "    one\n    two\nthree");
        assert_eq!(typed("one\n\ntwo", ">ip"), "    one\n\ntwo");
        assert_eq!(typed("{ one }", "l>i{"), "    { one }");
        assert_eq!(typed("one\n\ntwo", "3>>"), "    one\n\n    two");
        // A characterwise target is widened once in `span_of`, not per target.
        assert_eq!(typed("one two", ">w"), "    one two");
        assert_eq!(typed("one\ntwo\nthree", "Vj>"), "    one\n    two\nthree");
        assert_eq!(
            typed("one\ntwo\nthree", "2>>."),
            "        one\n        two\nthree"
        );
    }

    #[test]
    fn shifts_respect_host_indent_and_history() {
        let tabs = Indent {
            shift_width: 4,
            tab_width: 8,
            use_tabs: true,
        };
        let mut ed = Editor::from_text("\tword").with_indent(tabs);
        ed.type_keys(">>").unwrap();
        assert_eq!(ed.buffer().to_string(), "\t    word");
        ed.type_keys("<lt><lt>").unwrap();
        assert_eq!(ed.buffer().to_string(), "\tword");

        let mut ed = Editor::from_text("one\ntwo");
        let effects = ed.type_keys(">j").unwrap();
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Edit(_)))
                .count(),
            2
        );
        ed.type_keys("u").unwrap();
        assert_eq!(ed.buffer().to_string(), "one\ntwo");

        // Like case operators, shifting leaves the unnamed register alone.
        let ed = editor("one two", "yw>>p");
        assert_eq!(ed.register().text, "one ");
    }

    #[test]
    fn shift_noops_do_not_emit_edits_and_land_on_the_first_non_blank() {
        let mut ed = Editor::from_text("one");
        assert!(
            ed.type_keys("<lt><lt>")
                .unwrap()
                .iter()
                .all(|effect| !matches!(effect, Effect::Edit(_)))
        );
        assert_eq!(typed("    ", "<lt><lt>"), "");

        let mut ed = Editor::from_text("");
        assert!(
            ed.type_keys(">>")
                .unwrap()
                .iter()
                .all(|effect| !matches!(effect, Effect::Edit(_)))
        );

        let ed = editor("  one\ntwo", ">j");
        assert_eq!(ed.cursor_point(), Point::new(0, 6));
    }

    #[test]
    fn visual_shift_count_is_an_indent_amount() {
        assert_eq!(
            typed("one\ntwo", "Vj3>"),
            "            one\n            two"
        );
    }

    #[test]
    fn linewise_operators_on_the_last_row_stay_on_it() {
        // A linewise span that reaches the last row opens with the newline ending
        // the row *above* — the one `dd` has to take. Every other linewise
        // operator has to allow for that, or it works a row too high.
        assert_eq!(typed("a\nb\nc", "G>>"), "a\nb\n    c");
        assert_eq!(typed("a\nb\nc", "G>k"), "a\n    b\n    c");

        // A linewise register holds whole rows, so a put does not paste that
        // leading newline as a blank row...
        assert_eq!(typed("aa\nbb\ncc", "Gyyp"), "aa\nbb\ncc\ncc");
        assert_eq!(typed("aa\nbb\ncc", "GyyP"), "aa\nbb\ncc\ncc");
        // ...and putting below a file with no trailing newline opens a row break
        // rather than joining onto the row that is already there.
        assert_eq!(typed("aa\nbb\ncc", "yyGp"), "aa\nbb\ncc\naa");
        assert_eq!(typed("aa\nbb\ncc", "yyGP"), "aa\nbb\naa\ncc");

        // And the caret lands on a row the operator covered.
        let ed = editor("aa\nbb\ncc", "GgUU");
        assert_eq!(ed.buffer().to_string(), "aa\nbb\nCC");
        assert_eq!(ed.cursor_point().row, 2);
        assert_eq!(editor("aa\nbb\ncc", "j2gUU").cursor_point().row, 1);
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
        // On the delimiter itself counts as inside, as in vi.
        assert_eq!(typed("f(a, b)", "lci(x<Esc>"), "f(x)");
    }

    #[test]
    fn a_delimited_object_seeks_forward_from_outside() {
        // On the `f`, in no pair at all: vi looks forward for one rather than
        // failing, and it looks past the end of the row.
        assert_eq!(typed("f(a, b)", "ci(x<Esc>"), "f(x)");
        // The braces own their rows, so the body's rows go and the braces stay put
        // rather than being pulled together.
        assert_eq!(typed("fn f()\n{\n    body\n}", "di{"), "fn f()\n{\n}");
        assert_eq!(
            typed("outer { mid { deep } here } end", "da{"),
            "outer  end"
        );
    }

    #[test]
    fn a_count_descends_when_the_object_had_to_seek() {
        const NESTED: &str = "outer { mid { deep } here } end";

        // From outside, the count is still the nesting level you want — but the
        // levels now run inward from the pair the seek found, since there is no
        // enclosing pair to climb out of.
        assert_eq!(typed(NESTED, "di{"), "outer {} end");
        assert_eq!(typed(NESTED, "2di{"), "outer { mid {} here } end");
        assert_eq!(typed(NESTED, "d2i{"), "outer { mid {} here } end");
        assert_eq!(typed(NESTED, "2da{"), "outer { mid  here } end");

        // Nothing nested that deep, so it rings and changes nothing.
        let ed = editor(NESTED, "3di{");
        assert_eq!(ed.buffer().to_string(), NESTED);

        // And a visual object seeks the same way.
        let ed = editor(NESTED, "v2i{");
        assert_eq!(
            ed.selection().map(|range| ed.buffer().text_in(range)),
            Some(" deep ".to_string())
        );
    }

    #[test]
    fn counts_apply_to_text_objects() {
        const NESTED: &str = "outer { mid { deep } here } end";

        // A count on a delimited object climbs out that many levels of nesting.
        assert_eq!(typed(NESTED, "fpda{"), "outer { mid  here } end");
        assert_eq!(typed(NESTED, "fp2di{"), "outer {} end");
        assert_eq!(typed(NESTED, "fp2da{"), "outer  end");
        // Either side of the operator, as vi's grammar allows.
        assert_eq!(typed(NESTED, "fpd2i{"), "outer {} end");

        // Past the outermost pair there is nothing to delete, so it rings.
        let ed = editor(NESTED, "fp3di{");
        assert_eq!(ed.buffer().to_string(), NESTED);

        // Words count runs, so `2diw` takes the word and the space after it while
        // `2daw` takes two whole words.
        assert_eq!(typed("one two three", "2diw"), "two three");
        assert_eq!(typed("one two three", "3diw"), " three");
        assert_eq!(typed("one two three", "2daw"), "three");

        // And a count on a visual object extends the selection the same way.
        let ed = editor(NESTED, "fpv2i{");
        assert_eq!(
            ed.selection().map(|range| ed.buffer().text_in(range)),
            Some(" mid { deep } here ".to_string())
        );
    }

    #[test]
    fn an_exclusive_operator_reaches_the_end_of_the_file() {
        // The end of an exclusive span is a boundary, not a place the cursor has to
        // be able to rest, so it may sit one past the last character. Clamping it as
        // though it were a cursor position left the file's final character behind.
        assert_eq!(typed("one two", "wdw"), "one ");
        assert_eq!(typed("one two", "d2w"), "");
        assert_eq!(typed("one two", "wdW"), "one ");
        assert_eq!(typed("one two", "wgUw"), "one TWO");
        // `l` on the last character of the file has the same shape.
        assert_eq!(typed("abc", "$dl"), "ab");
        assert_eq!(typed("abc", "$x"), "ab");
        // Inclusive motions are unaffected: they land *on* a character and extend
        // over it, so `d$` must still stop before the newline.
        assert_eq!(typed("ab\ncd", "d$"), "\ncd");
        assert_eq!(typed("one two", "de"), " two");
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

    // -- case ------------------------------------------------------------

    #[test]
    fn case_operators_over_motions() {
        assert_eq!(typed("one two", "gUw"), "ONE two");
        assert_eq!(typed("ONE TWO", "guw"), "one TWO");
        assert_eq!(typed("One Two", "g~w"), "oNE Two");
        // Counts multiply through the motion, either side of the operator.
        assert_eq!(typed("one two three", "2gUw"), "ONE TWO three");
        assert_eq!(typed("one two three", "gU2w"), "ONE TWO three");
        assert_eq!(typed("one two three", "2gU2w"), "ONE TWO THREE");
        // And over text objects and finds, like any other operator.
        assert_eq!(typed("one two", "wgUiw"), "one TWO");
        assert_eq!(typed("one two", "gUt "), "ONE two");
        assert_eq!(typed("a (b c) d", "fbgUi("), "a (B C) d");
    }

    #[test]
    fn case_operators_over_whole_rows() {
        // Doubled, and vi's short second half — `gUU` as well as `gUgU`.
        assert_eq!(typed("one two\nthree", "gUU"), "ONE TWO\nthree");
        assert_eq!(typed("one two\nthree", "gUgU"), "ONE TWO\nthree");
        assert_eq!(typed("One Two\nthree", "g~~"), "oNE tWO\nthree");
        assert_eq!(typed("one\ntwo\nthree", "2guu"), "one\ntwo\nthree");
        assert_eq!(typed("ONE\nTWO\nthree", "2guu"), "one\ntwo\nthree");
        // Mismatched halves are a syntax error, as `dc` is.
        let ed = editor("one two", "gUd");
        assert_eq!(ed.buffer().to_string(), "one two");
    }

    #[test]
    fn case_operators_leave_the_register_alone() {
        // `yw` then `gUW` then `p` pastes what was yanked, not what was recased.
        let ed = editor("one two", "ywwgUWP");
        assert_eq!(ed.buffer().to_string(), "one one TWO");
        assert_eq!(ed.register().text, "one ");
    }

    #[test]
    fn case_changes_on_a_visual_selection() {
        // In visual mode `u` and `U` are case changes rather than undo, and `~`
        // covers the whole selection rather than one character.
        assert_eq!(typed("one two three", "vwU"), "ONE Two three");
        assert_eq!(typed("ONE TWO three", "vwu"), "one tWO three");
        assert_eq!(typed("One Two", "v$~"), "oNE tWO");
        // Linewise, and the selection is dropped afterwards.
        let ed = editor("one\ntwo\nthree", "VjU");
        assert_eq!(ed.buffer().to_string(), "ONE\nTWO\nthree");
        assert_eq!(ed.mode(), Mode::Normal);
        assert_eq!(ed.selection(), None);
        // `gU` still works there too, and so does a count on the motion. The
        // selection includes the character under the cursor, so `2w` takes the `t`
        // it landed on as well.
        assert_eq!(typed("one two three", "v2wgU"), "ONE TWO Three");
    }

    #[test]
    fn case_changes_are_undoable_and_repeatable() {
        let mut ed = Editor::from_text("one two three");
        ed.type_keys("gUw").expect("valid keys");
        assert_eq!(ed.buffer().to_string(), "ONE two three");
        // One step, and the caret comes back to where the change started.
        ed.type_keys("u").expect("valid keys");
        assert_eq!(ed.buffer().to_string(), "one two three");
        assert_eq!(ed.cursor(), 0);

        // `.` repeats a case change like any other.
        assert_eq!(typed("one two three", "gUww.w."), "ONE TWO THREE");
    }

    #[test]
    fn case_changes_handle_multibyte_text() {
        // Uppercasing is one-to-many in places, so the text can grow.
        assert_eq!(typed("straße", "gUiw"), "STRASSE");
        assert_eq!(typed("CAFÉ NIÑO", "guiw"), "café NIÑO");
        // And the caret must still land on a character boundary afterwards.
        let ed = editor("straße rest", "gUiw");
        assert_eq!(ed.cursor(), 0);
        assert_eq!(ed.buffer().to_string(), "STRASSE rest");
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
    fn one_shot_normal_runs_a_command_and_comes_back() {
        // `<C-o>` buys exactly one normal-mode command, then hands back the mode it
        // came from, caret where the command left it.
        let ed = editor("one two", "i<C-o>wX<Esc>");
        assert_eq!(ed.buffer().to_string(), "one Xtwo");
        assert_eq!(ed.mode(), Mode::Normal);

        // Only one: the second `w` is text again.
        assert_eq!(typed("one two", "i<C-o>ww<Esc>"), "one wtwo");

        // Counts belong to the command, not the `<C-o>`.
        assert_eq!(typed("a\nb\nc\nd", "i<C-o>2ddX<Esc>"), "Xc\nd");

        // The whole vocabulary is available, including operators over motions and
        // the doubled forms.
        assert_eq!(typed("one two", "A<C-o>dbthree<Esc>"), "one three");
        assert_eq!(typed("keep\nlose", "ji<C-o>ddX<Esc>"), "Xkeep");
    }

    #[test]
    fn one_shot_normal_is_visible_to_the_host() {
        // The grammar in force is normal mode's, and `mode()` says so — but a host
        // still has to render `-- (insert) --`, so the pending mode is queryable.
        let ed = editor("abc", "i<C-o>");
        assert_eq!(ed.mode(), Mode::Normal);
        assert_eq!(ed.resuming(), Some(Mode::Insert));

        // Replace mode uses the insert grammar, so it gets `<C-o>` too, and comes
        // back to overwriting rather than inserting.
        let ed = editor("abcdef", "R<C-o>2lXY<Esc>");
        assert_eq!(ed.resuming(), None);
        assert_eq!(ed.buffer().to_string(), "abXYef");

        // Spent, and back to insert.
        let ed = editor("abc", "i<C-o>l");
        assert_eq!(ed.mode(), Mode::Insert);
        assert_eq!(ed.resuming(), None);
    }

    #[test]
    fn one_shot_normal_keeps_the_caret_past_the_end_of_the_row() {
        // The caret is an insert caret throughout: normal mode would clamp it onto
        // the last character, and coming back would land a column short of where
        // the user was typing.
        let ed = editor("ab\ncd", "A<C-o>");
        assert_eq!(ed.cursor(), 2);
        assert_eq!(typed("ab\ncd", "A<C-o>zzX<Esc>"), "abX\ncd");

        // And `<C-o><Esc>` leaves insert for good, clamping as `<Esc>` always does.
        let ed = editor("ab\ncd", "A<C-o><Esc>");
        assert_eq!(ed.mode(), Mode::Normal);
        assert_eq!(ed.resuming(), None);
        assert_eq!(ed.cursor(), 1);
    }

    #[test]
    fn one_shot_normal_yields_to_a_command_that_picks_its_own_mode() {
        // `<C-o>cw` is already inserting when it hands back, so nothing to restore.
        let ed = editor("one two", "i<C-o>cwX<Esc>");
        assert_eq!(ed.buffer().to_string(), "X two");

        // `<C-o>v` means the user wants a selection, not another character typed.
        let ed = editor("abc", "i<C-o>v");
        assert_eq!(ed.mode(), Mode::Visual(VisualKind::Char));
        assert_eq!(ed.resuming(), None);
    }

    #[test]
    fn a_wasted_one_shot_hands_back_rather_than_stranding_you() {
        // `<C-o>` then a key that means nothing in normal mode: bell, and back to
        // insert. Being left in normal mode would silently reinterpret every
        // keystroke that followed.
        let mut ed = Editor::from_text("abc");
        ed.type_keys("i<C-o>").expect("valid keys");
        let effects = ed.type_keys("<C-y>").expect("valid keys");
        assert!(effects.contains(&Effect::Bell));
        assert_eq!(ed.mode(), Mode::Insert);
        assert_eq!(typed("abc", "i<C-o><C-y>X<Esc>"), "Xabc");

        // Abandoning a half-typed command spends the `<C-o>` too, for the same
        // reason — `d` alone leaves nothing to interpret the next key against.
        let ed = editor("abc", "i<C-o>d<Esc>");
        assert_eq!(ed.mode(), Mode::Insert);
        assert_eq!(ed.resuming(), None);
    }

    #[test]
    fn one_shot_normal_breaks_the_undo_sequence() {
        // vi splits the insert session at a `<C-o>`: what came before, the command
        // itself, and what came after are three separate steps.
        let mut ed = Editor::from_text("one two");
        ed.type_keys("ipre <C-o>dw post<Esc>").expect("valid keys");
        assert_eq!(ed.buffer().to_string(), "pre  posttwo");
        assert_eq!(ed.document().history().undo_depth(), 3);

        ed.type_keys("u").expect("valid keys");
        assert_eq!(ed.buffer().to_string(), "pre two");
        ed.type_keys("u").expect("valid keys");
        assert_eq!(ed.buffer().to_string(), "pre one two");
        ed.type_keys("u").expect("valid keys");
        assert_eq!(ed.buffer().to_string(), "one two");
    }

    #[test]
    fn dot_repeats_a_session_containing_a_one_shot() {
        // `.` replays raw keys, so the `<C-o>` and its command ride along with the
        // text that was typed around it.
        assert_eq!(typed("one two\nthree four", "i<C-o>Dx<Esc>j0."), "x\nx");

        // And the replayed command re-aims from wherever the caret is now rather
        // than reproducing the offset it hit the first time: the second `w` starts
        // from column 0 of the changed row, so both `X`s land on a word start.
        assert_eq!(typed("one two", "i<C-o>wX<Esc>0."), "one XXtwo");
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
    fn d_and_c_clear_to_the_end_of_the_row() {
        // The space before the deleted word stays — `D` cuts from the cursor, and
        // `w` landed on `i`, not on the space.
        assert_eq!(typed(SQL, "wD"), "select \nfrom users\nwhere id = 1");
        assert_eq!(
            typed(SQL, "wCname<Esc>"),
            "select name\nfrom users\nwhere id = 1"
        );

        // `$` is inclusive, so the last character goes too.
        assert_eq!(typed("abc", "D"), "");
        assert_eq!(typed("abc", "lD"), "a");

        // `C` leaves you in insert mode at the point of truncation.
        let ed = editor("abc", "lC");
        assert_eq!(ed.mode(), Mode::Insert);
        assert_eq!(ed.cursor(), 1);

        // The row itself survives, unlike `dd`.
        assert_eq!(typed("aa\nbb", "D"), "\nbb");

        // A count reaches into following rows: `2D` takes this row's tail and all
        // of the next, joining what is left.
        assert_eq!(typed("aa\nbb\ncc", "lD"), "a\nbb\ncc");
        assert_eq!(typed("aa\nbb\ncc", "2D"), "\ncc");
        assert_eq!(typed("aa\nbb\ncc", "3D"), "");

        // Yanked characterwise, so `p` pastes inline rather than onto a new row.
        let ed = editor("abc\nxyz", "D");
        assert!(!ed.register().linewise);
        assert_eq!(ed.register().text, "abc");

        // Undo restores in one step, caret included.
        let ed = editor(SQL, "wDu");
        assert_eq!(ed.buffer().to_string(), SQL);
        assert_eq!(ed.cursor_point(), Point { row: 0, col: 7 });

        // And `.` repeats it.
        assert_eq!(typed("aa bb\ncc dd", "wDj0w."), "aa \ncc ");
    }

    #[test]
    fn repeating_a_till_find_skips_where_it_already_is() {
        // `t`/`T` land one short of the target, so a naive repeat re-finds the same
        // target and resolves to the position it is already on. vi's default skips
        // it — `cpoptions` without `;`.
        // "a.b.c.d" — dots at 1, 3, 5.
        // `t.` from 0 is already a no-op, because the dot is adjacent. That part
        // matches vi. It is the repeat that has to move.
        let ed = editor("a.b.c.d", "t.");
        assert_eq!(ed.cursor(), 0);
        let ed = editor("a.b.c.d", "t.;");
        assert_eq!(ed.cursor(), 2);
        let ed = editor("a.b.c.d", "t.;;");
        assert_eq!(ed.cursor(), 4);

        // Backward, and with `,` reversing an earlier forward find.
        let ed = editor("a.b.c.d", "$T.;");
        assert_eq!(ed.cursor(), 4);
        let ed = editor("a.b.c.d", "t.;;,");
        assert_eq!(ed.cursor(), 2);

        // `f`/`F` were never affected: they land *on* the target, so the next
        // search already starts past it.
        let ed = editor("a.b.c.d", "f.;");
        assert_eq!(ed.cursor(), 3);

        // A count skips the adjacent target first, then counts from there. That is
        // the natural reading of the two rules together rather than a behaviour
        // checked against vi; pinned so it cannot drift silently.
        let ed = editor("a.b.c.d.e", "t.2;");
        assert_eq!(ed.cursor(), 4);

        // As an operator target, a forward `;` is inclusive, exactly as the `f`/`t`
        // it stands for. `d;` after `f,` takes the second comma with it.
        assert_eq!(typed("foo,bar,baz", "f,d;"), "foobaz");
        // And the till version stops before the target it found.
        assert_eq!(typed("a.b.c.d", "t.d;"), ".c.d");
        // Backward stays exclusive, leaving the character under the cursor.
        // `;` keeps `F`'s direction — it is `,` that would flip it to forward.
        assert_eq!(typed("foo,bar,baz", "$F,d;"), "foo,baz");
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
