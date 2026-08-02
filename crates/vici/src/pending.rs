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
use crate::motion::Find;

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
    /// Start of the partial path through the current layer's bindings.
    path_start: usize,
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
        self.path_start == self.keys.len()
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
        let path_is_empty = self.path_start == self.keys.len();
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
        if path_is_empty && let Some(digit) = key.as_digit() {
            let digit = digit as usize;
            let slot = if self.operator.is_some() {
                &mut self.count_after
            } else {
                &mut self.count_before
            };
            if !(digit == 0 && slot.is_none()) {
                *slot = Some(slot.unwrap_or(0).saturating_mul(10).saturating_add(digit));
                self.path_start = self.keys.len();
                return Resolution::Pending;
            }
        }

        let layer = if self.operator.is_some() {
            Layer::Operator
        } else {
            Layer::of(mode)
        };
        match keymap.walk(layer, &self.keys[self.path_start..]) {
            Walk::Prefix => Resolution::Pending,
            Walk::Unbound => self.reject(),
            Walk::Bound(binding) => self.apply(binding, mode),
        }
    }

    fn feed_insert(&mut self, key: Key, keymap: &Keymap) -> Resolution {
        match keymap.walk(Layer::Insert, &self.keys[self.path_start..]) {
            Walk::Prefix => Resolution::Pending,
            Walk::Bound(Binding::Command(command)) => self.finish(command),
            // Operators and object scopes are meaningless while inserting.
            Walk::Bound(_) => self.reject(),
            Walk::Unbound => {
                // An unbound printable key is text.
                if self.keys.len() - self.path_start == 1
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
        self.path_start = self.keys.len();
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
                let motion = Motion::Find(Find {
                    target: ch,
                    backward,
                    till,
                });
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
            AwaitChar::SetMark if self.operator.is_none() && ch.is_ascii_lowercase() => {
                self.finish(Command::SetMark(ch))
            }
            // `'` and `` ` `` name the position before the latest jump. They go
            // through the motion path like any other mark, so `d''` is linewise
            // and ``` d`` ``` characterwise, as `d'a` and `` d`a `` are.
            AwaitChar::GotoMark { exact }
                if ch.is_ascii_lowercase() || matches!(ch, '\'' | '`') =>
            {
                self.finish(self.with_motion(Motion::Mark { name: ch, exact }))
            }
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
    use crate::command::{InsertAt, VisualKind};
    use crate::key::keys;

    fn resolve_in(mode: Mode, spec: &str) -> Resolution {
        let keymap = Keymap::vim();
        let mut pending = Pending::new();
        let parsed = keys(spec).expect("valid test notation");
        let (last, rest) = parsed.split_last().expect("non-empty test sequence");
        for key in rest {
            assert_eq!(pending.feed(*key, mode, &keymap), Resolution::Pending);
        }
        pending.feed(*last, mode, &keymap)
    }

    fn command(spec: &str) -> (Command, Option<usize>) {
        match resolve_in(Mode::Normal, spec) {
            Resolution::Command { command, count, .. } => (command, count),
            other => panic!("{spec:?} should resolve, got {other:?}"),
        }
    }

    #[test]
    fn counts_and_operator_composition() {
        let word = Target::Motion(Motion::WordForward { big: false });
        for (spec, expected, count) in [
            ("3j", Command::Move(Motion::Down), Some(3)),
            (
                "2d3w",
                Command::Operate {
                    operator: Operator::Delete,
                    target: word,
                },
                Some(6),
            ),
            (
                "diw",
                Command::Operate {
                    operator: Operator::Delete,
                    target: Target::Object {
                        scope: ObjectScope::Inner,
                        object: TextObject::Word { big: false },
                    },
                },
                None,
            ),
            (
                "dt;",
                Command::Operate {
                    operator: Operator::Delete,
                    target: Target::Motion(Motion::Find(Find {
                        target: ';',
                        backward: false,
                        till: true,
                    })),
                },
                None,
            ),
            (
                "d'a",
                Command::Operate {
                    operator: Operator::Delete,
                    target: Target::Motion(Motion::Mark {
                        name: 'a',
                        exact: false,
                    }),
                },
                None,
            ),
        ] {
            assert_eq!(command(spec), (expected, count), "{spec}");
        }
    }

    #[test]
    fn doubled_operators_are_linewise() {
        for (spec, operator) in [
            ("dd", Operator::Delete),
            ("cc", Operator::Change),
            ("yy", Operator::Yank),
            (">>", Operator::ShiftRight),
            ("<lt><lt>", Operator::ShiftLeft),
        ] {
            assert_eq!(
                command(spec),
                (
                    Command::Operate {
                        operator,
                        target: Target::CurrentRow,
                    },
                    None,
                )
            );
        }
    }

    #[test]
    fn cancellation_and_rejection_reset_the_parser() {
        for spec in ["d!", "dy", "dx", "diz", "f<Left>", "mA", "`A"] {
            assert!(matches!(
                resolve_in(Mode::Normal, spec),
                Resolution::Rejected { .. }
            ));
        }
        for spec in ["d<Esc>", "2d<Esc>", "f<Esc>"] {
            assert!(matches!(
                resolve_in(Mode::Normal, spec),
                Resolution::Cancelled { .. }
            ));
        }
    }

    #[test]
    fn marks_are_commands_or_motions_as_their_grammar_requires() {
        assert_eq!(command("ma").0, Command::SetMark('a'));
        assert_eq!(
            command("`a").0,
            Command::Move(Motion::Mark {
                name: 'a',
                exact: true,
            })
        );
        // `'` and `` ` `` name the position before the latest jump, and go
        // through the motion path like any other mark — so they carry the same
        // linewise/characterwise distinction and take an operator.
        for (spec, name, exact) in [("''", '\'', false), ("``", '`', true)] {
            assert_eq!(
                command(spec).0,
                Command::Move(Motion::Mark { name, exact }),
                "{spec}"
            );
        }
        assert_eq!(
            command("d''").0,
            Command::Operate {
                operator: Operator::Delete,
                target: Target::Motion(Motion::Mark {
                    name: '\'',
                    exact: false,
                }),
            }
        );
    }

    #[test]
    fn insert_mode_falls_through_to_text() {
        for (mode, spec, expected) in [
            (Mode::Insert, "a", Command::InsertText('a')),
            (Mode::Insert, "3", Command::InsertText('3')),
            (Mode::Replace, "z", Command::InsertText('z')),
            (Mode::Insert, "<Esc>", Command::EnterNormal),
            (Mode::Insert, "<CR>", Command::InsertNewline),
        ] {
            match resolve_in(mode, spec) {
                Resolution::Command { command, count, .. } => {
                    assert_eq!(command, expected, "{spec}");
                    assert_eq!(count, None, "{spec}");
                }
                other => panic!("{spec:?} should resolve, got {other:?}"),
            }
        }
        assert!(matches!(
            resolve_in(Mode::Insert, "<C-q>"),
            Resolution::Rejected { .. }
        ));
        assert_eq!(command("i").0, Command::EnterInsert(InsertAt::Cursor));
        assert!(matches!(
            resolve_in(Mode::Visual(VisualKind::Char), "d"),
            Resolution::Command {
                command: Command::Operate {
                    target: Target::Selection,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn custom_keymaps_flow_through_the_grammar() {
        let mut keymap = Keymap::vim();
        keymap.bind_spec(Layer::Normal, "<C-a>", Binding::Motion(Motion::LastColumn));
        let mut pending = Pending::new();
        assert_eq!(
            pending.feed(Key::char('d'), Mode::Normal, &keymap),
            Resolution::Pending
        );
        assert!(matches!(
            pending.feed(Key::ctrl('a'), Mode::Normal, &keymap),
            Resolution::Command {
                command: Command::Operate {
                    operator: Operator::Delete,
                    target: Target::Motion(Motion::LastColumn),
                },
                ..
            }
        ));
    }
}
