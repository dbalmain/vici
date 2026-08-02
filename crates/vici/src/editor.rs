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
            last_find: None,
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

        match self.pending.feed(key, self.mode, &self.keymap) {
            Resolution::Pending | Resolution::Cancelled { .. } => Vec::new(),
            Resolution::Rejected { .. } => vec![Effect::Bell],
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
            (Motion::RepeatFind { reverse }, Some(find)) => Motion::Find(Find {
                backward: find.backward != reverse,
                ..find
            }),
            _ => motion,
        }
    }

    fn remember_find(&mut self, motion: Motion) {
        if let Motion::Find(find) = motion {
            self.last_find = Some(find);
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
    /// row went.
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
        assert_eq!(ed.type_keys(":").unwrap(), vec![Effect::CommandPrompt]);
    }

    #[test]
    fn a_rebound_key_changes_behaviour_end_to_end() {
        use crate::keymap::{Binding, Layer};
        let mut keymap = Keymap::vim();
        keymap
            .bind_spec(Layer::Normal, "j", Binding::Motion(Motion::Up))
            .bind_spec(Layer::Normal, "k", Binding::Motion(Motion::Down));
        let mut ed = Editor::with(SQL, keymap);
        ed.type_keys("k").unwrap();
        assert_eq!(ed.cursor_point(), Point::new(1, 0));
        ed.type_keys("gg").unwrap();
        ed.type_keys("dk").unwrap();
        assert_eq!(ed.buffer().to_string(), "where id = 1");
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
