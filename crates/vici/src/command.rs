//! The command vocabulary: what a resolved key sequence means.
//!
//! Everything here is plain data — no closures, no function pointers — so a
//! keymap can be deserialised from config later without redesign.

/// Editing mode.
///
/// Operator-pending is deliberately *not* a mode. It is transient parser state
/// ([`crate::Pending`]), which keeps the mode enum small and stops "am I in
/// operator-pending?" from leaking into rendering and keymap lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Replace,
    Visual(VisualKind),
}

impl Mode {
    /// True where the count/operator/motion grammar applies.
    #[must_use]
    pub const fn is_command(self) -> bool {
        matches!(self, Self::Normal | Self::Visual(_))
    }

    #[must_use]
    pub const fn is_visual(self) -> bool {
        matches!(self, Self::Visual(_))
    }
}

/// Visual-mode granularity. Block mode is not implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VisualKind {
    Char,
    Line,
}

/// A cursor movement.
///
/// Each motion carries its own operator semantics via [`Motion::is_linewise`] and
/// [`Motion::is_inclusive`]. Getting these right is what makes `dw` and `de`
/// differ by one character, and `dj` delete two whole rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Down,
    Up,
    /// `0`
    FirstColumn,
    /// `^`
    FirstNonBlank,
    /// `$`
    LastColumn,
    /// `w` / `W`
    WordForward {
        big: bool,
    },
    /// `b` / `B`
    WordBackward {
        big: bool,
    },
    /// `e` / `E`
    WordEnd {
        big: bool,
    },
    /// `f` / `F` / `t` / `T`
    Find {
        target: char,
        backward: bool,
        /// `t`/`T` stop short of the target.
        till: bool,
    },
    /// `;` / `,`
    RepeatFind {
        reverse: bool,
    },
    /// `G` — count as an absolute row, else the last row.
    GotoRow,
    /// `gg` — count as an absolute row, else the first row.
    GotoFirstRow,
}

impl Motion {
    /// Whether an operator over this motion acts on whole rows.
    #[must_use]
    pub const fn is_linewise(self) -> bool {
        matches!(
            self,
            Self::Down | Self::Up | Self::GotoRow | Self::GotoFirstRow
        )
    }

    /// Whether the character under the motion's destination is included.
    #[must_use]
    pub const fn is_inclusive(self) -> bool {
        match self {
            Self::WordEnd { .. } | Self::LastColumn => true,
            // Forward `f` and `t` are both inclusive; `t` simply lands a character
            // earlier. So `dt,` deletes up to but not including the comma, which
            // requires including the character `t` landed on. Backward `F`/`T` are
            // exclusive, leaving the character under the cursor alone.
            Self::Find { backward, .. } => !backward,
            _ => false,
        }
    }
}

/// A text object's extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectScope {
    /// `i` — contents only.
    Inner,
    /// `a` — contents plus delimiters or trailing whitespace.
    Around,
}

/// A structural region, selected by `i`/`a` plus a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObject {
    Word {
        big: bool,
    },
    /// A paired delimiter, e.g. `(`/`)` for `ib`.
    Delimited {
        open: char,
        close: char,
    },
    /// A symmetric delimiter, e.g. `"` or `` ` ``.
    Quoted(char),
    Paragraph,
}

/// What an operator acts upon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Motion(Motion),
    Object {
        scope: ObjectScope,
        object: TextObject,
    },
    /// A doubled operator: `dd`, `cc`, `yy`.
    CurrentRow,
    /// The active visual selection.
    Selection,
}

/// An operator, awaiting a [`Target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operator {
    Delete,
    Change,
    Yank,
}

/// Where insert mode begins relative to the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InsertAt {
    /// `i`
    Cursor,
    /// `a`
    After,
    /// `I`
    FirstNonBlank,
    /// `A`
    EndOfRow,
    /// `o`
    RowBelow,
    /// `O`
    RowAbove,
}

/// Viewport movements. The core does not own the viewport — these resolve to
/// effects the host fulfils.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scroll {
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    /// `zz`
    Center,
    /// `zt`
    Top,
    /// `zb`
    Bottom,
}

/// A fully resolved command.
///
/// Counts are *not* carried here — see [`crate::Resolution::Command`]. Keeping
/// them separate means the same `Command` value can sit in a keymap unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Move(Motion),
    Operate {
        operator: Operator,
        target: Target,
    },
    /// Extend the visual selection to a text object.
    SelectObject {
        scope: ObjectScope,
        object: TextObject,
    },

    EnterInsert(InsertAt),
    EnterVisual(VisualKind),
    EnterReplace,
    /// `<Esc>` from any mode.
    EnterNormal,

    /// `x` / `X`
    DeleteChar {
        before: bool,
    },
    /// `r{char}`
    ReplaceChar(char),
    /// `J`
    JoinRows,
    /// `p` / `P`
    Put {
        before: bool,
    },
    /// `~`
    SwapCase,

    Undo,
    Redo,
    /// `U`
    UndoRow,
    /// `.`
    Repeat,

    /// `q{register}` — a second `q` stops recording.
    RecordMacro(char),
    StopRecording,
    /// `@{register}`
    PlayMacro(char),

    Scroll(Scroll),

    /// `/` and `?` — the host opens a prompt and calls back.
    SearchPrompt {
        backward: bool,
    },
    /// `n` / `N`
    SearchRepeat {
        reverse: bool,
    },
    /// `:` — likewise a host prompt.
    CommandPrompt,

    /// Literal text typed in insert mode.
    InsertText(char),
    /// `<CR>` in insert mode.
    InsertNewline,
    /// `<BS>` in insert mode.
    DeleteBack,
    /// `<C-w>` in insert mode.
    DeleteWordBack,
}

/// A binding that needs one further key as a literal character argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AwaitChar {
    /// `f` / `F` / `t` / `T`
    Find { backward: bool, till: bool },
    /// `r`
    ReplaceChar,
    /// `q`
    RecordMacro,
    /// `@`
    PlayMacro,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_semantics_of_motions() {
        // `dw` stops before the next word; `de` eats the last character.
        assert!(!Motion::WordForward { big: false }.is_inclusive());
        assert!(Motion::WordEnd { big: false }.is_inclusive());

        // `dj` takes whole rows.
        assert!(Motion::Down.is_linewise());
        assert!(!Motion::Right.is_linewise());

        // Both forward finds are inclusive of where they land: `t` lands one
        // character earlier, which is how `dt,` stops before the comma.
        let forward = |till| Motion::Find {
            target: ',',
            backward: false,
            till,
        };
        assert!(forward(false).is_inclusive());
        assert!(forward(true).is_inclusive());
        // Backward finds leave the character under the cursor alone.
        assert!(
            !Motion::Find {
                target: ',',
                backward: true,
                till: false
            }
            .is_inclusive()
        );
    }

    #[test]
    fn operator_pending_is_not_a_mode() {
        assert!(Mode::Normal.is_command());
        assert!(Mode::Visual(VisualKind::Line).is_command());
        assert!(!Mode::Insert.is_command());
        assert!(Mode::Visual(VisualKind::Char).is_visual());
    }
}
