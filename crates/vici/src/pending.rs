//! The pending-input parser: vi's command grammar.
//!
//! A trie alone cannot express vi's normal-mode syntax, because counts and
//! operators *compose* rather than sequence:
//!
//! ```text
//! [count] operator [count] motion | textobject
//! [count] command
//! ```
//!
//! So this type holds the compositional state — counts, a pending operator, a
//! pending object scope, a pending character argument — and defers the rest to
//! [`Keymap`]. It touches no buffer, which makes the whole grammar testable as
//! string in, [`Command`] out.

use crate::command::{AwaitChar, Command, Mode, Motion, ObjectScope, Operator, Target, TextObject};
use crate::key::{Key, KeyCode};
use crate::keymap::{Binding, Keymap, Layer, Walk};

/// What feeding one key produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A valid partial sequence. Show the keys as a `showcmd` hint if you like.
    Pending,
    /// A complete command.
    Command {
        command: Command,
        /// The effective count, `None` when the user gave none.
        ///
        /// Distinguishing `None` from `Some(1)` matters: `G` with no count goes to
        /// the last row, `1G` to the first.
        count: Option<usize>,
        /// Every key consumed, in order. This is what dot-repeat and macro
        /// recording store — replaying keys rather than re-executing commands is
        /// what makes both fall out of one mechanism.
        keys: Vec<Key>,
    },
    /// Not a valid sequence. State has been reset; the host may beep.
    Rejected { keys: Vec<Key> },
    /// `<Esc>` abandoned a partial sequence.
    Cancelled { keys: Vec<Key> },
}

/// Accumulated state between keys.
#[derive(Debug, Clone, Default)]
pub struct Pending {
    keys: Vec<Key>,
    /// Partial path through the current layer's trie.
    path: Vec<Key>,
    count_before: Option<usize>,
    count_after: Option<usize>,
    operator: Option<Operator>,
    scope: Option<ObjectScope>,
    awaiting: Option<AwaitChar>,
}

impl Pending {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Nothing accumulated — the next key starts a fresh command.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.path.is_empty()
            && self.count_before.is_none()
            && self.count_after.is_none()
            && self.operator.is_none()
            && self.scope.is_none()
            && self.awaiting.is_none()
    }

    /// Keys consumed so far, for a `showcmd` indicator.
    #[must_use]
    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// The operator awaiting a target, if any.
    #[must_use]
    pub fn operator(&self) -> Option<Operator> {
        self.operator
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Feed one key.
    ///
    /// `mode` selects the grammar: [`Mode::is_command`] modes get counts and
    /// operators, insert and replace get a straight keymap lookup with unbound
    /// printable keys falling through to text.
    pub fn feed(&mut self, key: Key, mode: Mode, keymap: &Keymap) -> Resolution {
        if !mode.is_command() {
            self.keys.push(key);
            return self.feed_insert(key, keymap);
        }

        // `<Esc>` abandons a partial sequence. When nothing is pending it falls
        // through to the keymap, where it means "return to normal mode".
        let idle = self.is_idle();
        self.keys.push(key);
        if key.code == KeyCode::Esc && !idle {
            return self.cancel();
        }

        if let Some(awaiting) = self.awaiting {
            return match key.as_text() {
                Some(ch) => self.resolve_await(awaiting, ch),
                None => self.reject(),
            };
        }

        if let Some(scope) = self.scope {
            return match keymap.object(key) {
                Some(object) => self.resolve_object(scope, object),
                None => self.reject(),
            };
        }

        // Counts accumulate only at the start of a key path, and a leading `0` is
        // the first-column motion rather than a digit — the classic ambiguity.
        if self.path.is_empty()
            && let Some(digit) = key.as_digit()
        {
            let digit = digit as usize;
            let slot = if self.operator.is_some() {
                &mut self.count_after
            } else {
                &mut self.count_before
            };
            if !(digit == 0 && slot.is_none()) {
                *slot = Some(slot.unwrap_or(0).saturating_mul(10).saturating_add(digit));
                return Resolution::Pending;
            }
        }

        self.path.push(key);
        let layer = if self.operator.is_some() {
            Layer::Operator
        } else {
            Layer::of(mode)
        };
        match keymap.walk(layer, &self.path) {
            Walk::Prefix => Resolution::Pending,
            Walk::Unbound => self.reject(),
            Walk::Bound(binding) => self.apply(binding, mode),
        }
    }

    fn feed_insert(&mut self, key: Key, keymap: &Keymap) -> Resolution {
        self.path.push(key);
        match keymap.walk(Layer::Insert, &self.path) {
            Walk::Prefix => Resolution::Pending,
            Walk::Bound(Binding::Command(command)) => self.finish(command),
            // Operators and object scopes are meaningless while inserting.
            Walk::Bound(_) => self.reject(),
            Walk::Unbound => {
                // An unbound printable key is text.
                if self.path.len() == 1
                    && let Some(ch) = key.as_text()
                {
                    self.finish(Command::InsertText(ch))
                } else {
                    self.reject()
                }
            }
        }
    }

    fn apply(&mut self, binding: Binding, mode: Mode) -> Resolution {
        self.path.clear();
        match binding {
            Binding::Command(command) => {
                // An operator needs a target; a plain command is not one.
                if self.operator.is_some() {
                    return self.reject();
                }
                self.finish(command)
            }
            Binding::Motion(motion) => self.finish(self.with_motion(motion)),
            Binding::Operator(operator) => {
                // In visual mode an operator applies to the selection at once.
                if mode.is_visual() {
                    return self.finish(Command::Operate {
                        operator,
                        target: Target::Selection,
                    });
                }
                match self.operator {
                    None => {
                        self.operator = Some(operator);
                        Resolution::Pending
                    }
                    // A doubled operator is linewise: `dd`, `cc`, `yy`.
                    Some(active) if active == operator => self.finish(Command::Operate {
                        operator,
                        target: Target::CurrentRow,
                    }),
                    Some(_) => self.reject(),
                }
            }
            Binding::ObjectScope(scope) => {
                if self.operator.is_none() && !mode.is_visual() {
                    return self.reject();
                }
                self.scope = Some(scope);
                Resolution::Pending
            }
            Binding::Await(awaiting) => {
                self.awaiting = Some(awaiting);
                Resolution::Pending
            }
        }
    }

    fn resolve_await(&mut self, awaiting: AwaitChar, ch: char) -> Resolution {
        self.awaiting = None;
        match awaiting {
            AwaitChar::Find { backward, till } => {
                let motion = Motion::Find {
                    target: ch,
                    backward,
                    till,
                };
                self.finish(self.with_motion(motion))
            }
            // These take no target, so a pending operator is a syntax error.
            AwaitChar::ReplaceChar if self.operator.is_none() => {
                self.finish(Command::ReplaceChar(ch))
            }
            AwaitChar::RecordMacro if self.operator.is_none() => {
                self.finish(Command::RecordMacro(ch))
            }
            AwaitChar::PlayMacro if self.operator.is_none() => self.finish(Command::PlayMacro(ch)),
            _ => self.reject(),
        }
    }

    fn resolve_object(&mut self, scope: ObjectScope, object: TextObject) -> Resolution {
        self.scope = None;
        let command = match self.operator {
            Some(operator) => Command::Operate {
                operator,
                target: Target::Object { scope, object },
            },
            None => Command::SelectObject { scope, object },
        };
        self.finish(command)
    }

    /// A motion is a movement on its own, or an operator's target.
    fn with_motion(&self, motion: Motion) -> Command {
        match self.operator {
            Some(operator) => Command::Operate {
                operator,
                target: Target::Motion(motion),
            },
            None => Command::Move(motion),
        }
    }

    /// vi multiplies the two counts: `2d3w` deletes six words.
    fn count(&self) -> Option<usize> {
        match (self.count_before, self.count_after) {
            (None, None) => None,
            (before, after) => Some(before.unwrap_or(1).saturating_mul(after.unwrap_or(1))),
        }
    }

    fn finish(&mut self, command: Command) -> Resolution {
        let count = self.count();
        let keys = core::mem::take(&mut self.keys);
        self.reset();
        Resolution::Command {
            command,
            count,
            keys,
        }
    }

    fn reject(&mut self) -> Resolution {
        let keys = core::mem::take(&mut self.keys);
        self.reset();
        Resolution::Rejected { keys }
    }

    fn cancel(&mut self) -> Resolution {
        let keys = core::mem::take(&mut self.keys);
        self.reset();
        Resolution::Cancelled { keys }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{InsertAt, Scroll, VisualKind};
    use crate::key::keys;

    /// Feed a whole sequence, asserting every key but the last leaves the parser
    /// `Pending`. That makes each test a statement about the grammar's shape, not
    /// just its final answer.
    fn resolve_in(mode: Mode, spec: &str) -> Resolution {
        let keymap = Keymap::vim();
        let mut pending = Pending::new();
        let parsed = keys(spec).unwrap();
        let (last, rest) = parsed.split_last().expect("non-empty sequence");
        for key in rest {
            let step = pending.feed(*key, mode, &keymap);
            assert_eq!(step, Resolution::Pending, "`{key}` in `{spec}` should pend");
        }
        pending.feed(*last, mode, &keymap)
    }

    fn resolve(spec: &str) -> Resolution {
        resolve_in(Mode::Normal, spec)
    }

    #[track_caller]
    fn cmd(spec: &str) -> (Command, Option<usize>) {
        match resolve(spec) {
            Resolution::Command { command, count, .. } => (command, count),
            other => panic!("`{spec}` should resolve, got {other:?}"),
        }
    }

    const DW: Target = Target::Motion(Motion::WordForward { big: false });

    #[test]
    fn a_bare_motion() {
        assert_eq!(cmd("h"), (Command::Move(Motion::Left), None));
    }

    #[test]
    fn counts_prefix_commands() {
        assert_eq!(cmd("3j"), (Command::Move(Motion::Down), Some(3)));
        assert_eq!(cmd("12k"), (Command::Move(Motion::Up), Some(12)));
    }

    #[test]
    fn operator_plus_motion() {
        assert_eq!(
            cmd("dw"),
            (
                Command::Operate {
                    operator: Operator::Delete,
                    target: DW
                },
                None
            )
        );
    }

    #[test]
    fn the_two_counts_multiply() {
        // `2d3w` is `d6w`, not `d3w` twice and not count 2.
        let (command, count) = cmd("2d3w");
        assert_eq!(count, Some(6));
        assert_eq!(
            command,
            Command::Operate {
                operator: Operator::Delete,
                target: DW
            }
        );
    }

    #[test]
    fn a_doubled_operator_is_linewise() {
        for (spec, operator) in [
            ("dd", Operator::Delete),
            ("cc", Operator::Change),
            ("yy", Operator::Yank),
            (">>", Operator::ShiftRight),
            ("<lt><lt>", Operator::ShiftLeft),
        ] {
            assert_eq!(
                cmd(spec),
                (
                    Command::Operate {
                        operator,
                        target: Target::CurrentRow
                    },
                    None
                )
            );
        }
        assert_eq!(cmd("3dd").1, Some(3));
    }

    #[test]
    fn mismatched_operators_are_rejected() {
        assert!(matches!(resolve("dy"), Resolution::Rejected { .. }));
    }

    #[test]
    fn an_operator_will_not_take_a_command_as_a_target() {
        // `dx` means nothing.
        assert!(matches!(resolve("dx"), Resolution::Rejected { .. }));
    }

    #[test]
    fn text_objects() {
        assert_eq!(
            cmd("diw"),
            (
                Command::Operate {
                    operator: Operator::Delete,
                    target: Target::Object {
                        scope: ObjectScope::Inner,
                        object: TextObject::Word { big: false }
                    }
                },
                None
            )
        );
        assert_eq!(
            cmd("ca(").0,
            Command::Operate {
                operator: Operator::Change,
                target: Target::Object {
                    scope: ObjectScope::Around,
                    object: TextObject::Delimited {
                        open: '(',
                        close: ')'
                    }
                }
            }
        );
        assert_eq!(
            cmd("yi\"").0,
            Command::Operate {
                operator: Operator::Yank,
                target: Target::Object {
                    scope: ObjectScope::Inner,
                    object: TextObject::Quoted('"')
                }
            }
        );
    }

    #[test]
    fn an_unknown_object_key_is_rejected() {
        assert!(matches!(resolve("diz"), Resolution::Rejected { .. }));
    }

    #[test]
    fn object_scopes_need_an_operator_or_a_selection() {
        // `i` in normal mode is insert, not "inner".
        assert_eq!(cmd("i").0, Command::EnterInsert(InsertAt::Cursor));
    }

    #[test]
    fn character_arguments() {
        assert_eq!(
            cmd("f,").0,
            Command::Move(Motion::Find {
                target: ',',
                backward: false,
                till: false
            })
        );
        assert_eq!(
            cmd("dt;").0,
            Command::Operate {
                operator: Operator::Delete,
                target: Target::Motion(Motion::Find {
                    target: ';',
                    backward: false,
                    till: true
                })
            }
        );
        assert_eq!(cmd("rx").0, Command::ReplaceChar('x'));
        assert_eq!(cmd("2rx").1, Some(2));
    }

    #[test]
    fn a_character_argument_rejects_a_non_character() {
        assert!(matches!(resolve("f<Left>"), Resolution::Rejected { .. }));
    }

    #[test]
    fn zero_is_a_motion_until_it_is_a_digit() {
        assert_eq!(cmd("0"), (Command::Move(Motion::FirstColumn), None));
        // Once a count is underway, `0` joins it.
        assert_eq!(cmd("10j"), (Command::Move(Motion::Down), Some(10)));
        assert_eq!(
            cmd("d0").0,
            Command::Operate {
                operator: Operator::Delete,
                target: Target::Motion(Motion::FirstColumn),
            }
        );
    }

    #[test]
    fn multi_key_motions() {
        assert_eq!(cmd("gg"), (Command::Move(Motion::GotoFirstRow), None));
        assert_eq!(cmd("5gg").1, Some(5));
        assert_eq!(cmd("G"), (Command::Move(Motion::GotoRow), None));
        assert_eq!(
            cmd("dgg").0,
            Command::Operate {
                operator: Operator::Delete,
                target: Target::Motion(Motion::GotoFirstRow)
            }
        );
    }

    #[test]
    fn no_count_is_distinct_from_count_one() {
        // `G` goes to the last row, `1G` to the first. The parser must not
        // collapse these.
        assert_eq!(cmd("G").1, None);
        assert_eq!(cmd("1G").1, Some(1));
    }

    #[test]
    fn partial_sequences_pend() {
        let keymap = Keymap::vim();
        let mut pending = Pending::new();
        for spec in ["d", "2", "g", "f", "di", "z"] {
            pending.reset();
            for key in keys(spec).unwrap() {
                assert_eq!(
                    pending.feed(key, Mode::Normal, &keymap),
                    Resolution::Pending,
                    "`{spec}` should still be pending"
                );
            }
            assert!(!pending.is_idle());
        }
    }

    #[test]
    fn escape_cancels_a_partial_sequence() {
        assert!(matches!(resolve("d<Esc>"), Resolution::Cancelled { .. }));
        assert!(matches!(resolve("2d<Esc>"), Resolution::Cancelled { .. }));
        assert!(matches!(resolve("f<Esc>"), Resolution::Cancelled { .. }));
        // But with nothing pending it is an ordinary command.
        assert_eq!(cmd("<Esc>").0, Command::EnterNormal);
    }

    #[test]
    fn rejection_reports_the_keys() {
        match resolve("d!") {
            Resolution::Rejected { keys: reported } => {
                assert_eq!(reported, keys("d!").unwrap());
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn an_operator_pends_over_an_inherited_prefix() {
        // `z` is a prefix in the normal layer, so `dz` is not yet wrong — the
        // parser cannot know until the sequence completes. `dzz` then fails,
        // because `zz` is a command and commands are not operator targets.
        assert_eq!(resolve("dz"), Resolution::Pending);
        assert!(matches!(resolve("dzz"), Resolution::Rejected { .. }));
    }

    #[test]
    fn resolution_carries_the_keys_for_replay() {
        // This is the whole basis of dot-repeat and macros: the keys, not the
        // command, are what gets stored.
        match resolve("2d3w") {
            Resolution::Command { keys: consumed, .. } => {
                assert_eq!(consumed, keys("2d3w").unwrap());
            }
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn state_resets_after_every_resolution() {
        let keymap = Keymap::vim();
        let mut pending = Pending::new();
        for key in keys("2dw").unwrap() {
            pending.feed(key, Mode::Normal, &keymap);
        }
        assert!(pending.is_idle());
        assert!(pending.keys().is_empty());
        assert_eq!(pending.operator(), None);
    }

    #[test]
    fn operator_is_visible_while_pending() {
        let keymap = Keymap::vim();
        let mut pending = Pending::new();
        pending.feed(Key::char('d'), Mode::Normal, &keymap);
        assert_eq!(pending.operator(), Some(Operator::Delete));
        assert_eq!(pending.keys(), keys("d").unwrap());
    }

    #[test]
    fn visual_mode_operators_act_on_the_selection() {
        let visual = Mode::Visual(VisualKind::Char);
        for spec in ["d", "c", "y"] {
            match resolve_in(visual, spec) {
                Resolution::Command {
                    command: Command::Operate { target, .. },
                    ..
                } => assert_eq!(target, Target::Selection),
                other => panic!("`{spec}` in visual gave {other:?}"),
            }
        }
        // `x` is shadowed in the visual layer.
        match resolve_in(visual, "x") {
            Resolution::Command {
                command: Command::Operate { operator, target },
                ..
            } => {
                assert_eq!(operator, Operator::Delete);
                assert_eq!(target, Target::Selection);
            }
            other => panic!("expected a selection delete, got {other:?}"),
        }
    }

    #[test]
    fn visual_mode_objects_extend_the_selection() {
        let visual = Mode::Visual(VisualKind::Char);
        match resolve_in(visual, "iw") {
            Resolution::Command {
                command: Command::SelectObject { scope, object },
                ..
            } => {
                assert_eq!(scope, ObjectScope::Inner);
                assert_eq!(object, TextObject::Word { big: false });
            }
            other => panic!("expected an object selection, got {other:?}"),
        }
    }

    #[test]
    fn visual_mode_inherits_normal_motions_and_counts() {
        let visual = Mode::Visual(VisualKind::Line);
        assert_eq!(
            resolve_in(visual, "3w"),
            Resolution::Command {
                command: Command::Move(Motion::WordForward { big: false }),
                count: Some(3),
                keys: keys("3w").unwrap(),
            }
        );
    }

    #[test]
    fn insert_mode_has_no_grammar() {
        let insert = Mode::Insert;
        assert_eq!(
            resolve_in(insert, "a"),
            Resolution::Command {
                command: Command::InsertText('a'),
                count: None,
                keys: keys("a").unwrap(),
            }
        );
        // Digits are text, not counts.
        match resolve_in(insert, "3") {
            Resolution::Command { command, count, .. } => {
                assert_eq!(command, Command::InsertText('3'));
                assert_eq!(count, None);
            }
            other => panic!("expected text, got {other:?}"),
        }
        // Bound keys still win.
        match resolve_in(insert, "<Esc>") {
            Resolution::Command { command, .. } => assert_eq!(command, Command::EnterNormal),
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn insert_mode_bindings() {
        for (spec, expected) in [
            ("<CR>", Command::InsertNewline),
            ("<BS>", Command::DeleteBack),
            ("<C-w>", Command::DeleteWordBack),
            ("<Tab>", Command::InsertText('\t')),
        ] {
            match resolve_in(Mode::Insert, spec) {
                Resolution::Command { command, .. } => assert_eq!(command, expected),
                other => panic!("`{spec}` gave {other:?}"),
            }
        }
    }

    #[test]
    fn unbound_control_keys_in_insert_mode_are_rejected_not_inserted() {
        assert!(matches!(
            resolve_in(Mode::Insert, "<C-q>"),
            Resolution::Rejected { .. }
        ));
    }

    #[test]
    fn replace_mode_uses_the_insert_grammar() {
        match resolve_in(Mode::Replace, "z") {
            Resolution::Command { command, .. } => assert_eq!(command, Command::InsertText('z')),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn macros_and_history() {
        assert_eq!(cmd("qa").0, Command::RecordMacro('a'));
        assert_eq!(cmd("@a").0, Command::PlayMacro('a'));
        assert_eq!(cmd("u").0, Command::Undo);
        assert_eq!(cmd("<C-r>").0, Command::Redo);
        assert_eq!(cmd("U").0, Command::UndoRow);
        assert_eq!(cmd(".").0, Command::Repeat);
        assert_eq!(cmd("3.").1, Some(3));
    }

    #[test]
    fn scrolling_and_prompts() {
        assert_eq!(cmd("zz").0, Command::Scroll(Scroll::Center));
        assert_eq!(cmd("<C-d>").0, Command::Scroll(Scroll::HalfPageDown));
        assert_eq!(cmd("/").0, Command::SearchPrompt { backward: false });
        assert_eq!(cmd(":").0, Command::CommandPrompt);
        assert_eq!(cmd("n").0, Command::SearchRepeat { reverse: false });
    }

    #[test]
    fn absurd_counts_saturate_rather_than_panic() {
        let (command, count) = cmd("99999999999999999999999999j");
        assert_eq!(command, Command::Move(Motion::Down));
        assert_eq!(count, Some(usize::MAX));
    }

    #[test]
    fn a_custom_binding_flows_through_the_grammar() {
        // Bind `<C-a>` to a motion and confirm an operator will take it.
        let mut keymap = Keymap::vim();
        keymap.bind_spec(Layer::Normal, "<C-a>", Binding::Motion(Motion::LastColumn));
        let mut pending = Pending::new();
        assert_eq!(
            pending.feed(Key::char('d'), Mode::Normal, &keymap),
            Resolution::Pending
        );
        match pending.feed(Key::ctrl('a'), Mode::Normal, &keymap) {
            Resolution::Command { command, .. } => assert_eq!(
                command,
                Command::Operate {
                    operator: Operator::Delete,
                    target: Target::Motion(Motion::LastColumn)
                }
            ),
            other => panic!("expected an operate, got {other:?}"),
        }
    }
}
