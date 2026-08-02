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
    /// Keys that have shaped the current visual selection, for `.` to replay.
    visual_keys: Vec<Key>,
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
            visual_keys: Vec::new(),
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
                let was_visual = self.mode.is_visual();
                let effects = self.run(command, count);
                // Everything typed since the selection opened, so that `.` can
                // replay the shape and not just the operator. The operator's own
                // key is not among them: by the time it runs, visual mode is over.
                if self.mode.is_visual() {
                    if !was_visual {
                        self.visual_keys.clear();
                    }
                    self.visual_keys.extend_from_slice(&consumed);
                }
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
                    let (first, last) = motion::span_rows(self.buffer(), &range);
                    self.buffer().row_range(first).start..self.buffer().row_content_range(last).end
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
        // A visual operator is only half of what happened: replaying a bare `>`
        // would leave an operator pending for whatever gets typed next. The keys
        // that opened and shaped the selection go in front of it, so `Vj>` repeats
        // as `Vj>` — the same two rows, from wherever the caret now is.
        let mut script = match command {
            Command::Operate {
                target: Target::Selection,
                ..
            } => core::mem::take(&mut self.visual_keys),
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

    const SQL: &str = "select id, name\nfrom users\nwhere id = 1";

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

    // These one-shot-normal tests remain inline until `<C-o>` is removed.
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
    fn a_rebound_key_changes_behaviour_end_to_end() {
        use crate::keymap::{Binding, Layer};
        let mut keymap = Keymap::vim();
        keymap
            .bind_spec(Layer::Normal, "j", Binding::Motion(Motion::Up))
            .bind_spec(Layer::Normal, "k", Binding::Motion(Motion::Down));
        let mut ed = Editor::with(SQL, keymap, LinearHistory::new());
        ed.type_keys("k").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(1, 0));
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
