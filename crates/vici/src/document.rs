//! The minimal composition of a buffer and a history.
//!
//! This type exists for one reason: the `stage → record → apply` ordering that
//! [`History`] depends on is easy to get subtly wrong, and getting it wrong means
//! `U` silently snapshots post-change text. Encoding it once removes the hazard.

use core::fmt;
use core::ops::Range;

use crate::buffer::Buffer;
use crate::edit::{Change, Edit};
use crate::history::{History, LinearHistory, Step};

/// What an undo, redo or `U` did: the edits applied, and the caret to restore.
///
/// `cursor` is `None` when the history does not track it — see [`Step::cursor`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Revert {
    pub edits: Vec<Edit>,
    pub cursor: Option<usize>,
}

impl Revert {
    /// True when there was nothing to undo or redo.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
}

/// A buffer paired with an undo policy.
///
/// Every mutating method returns the [`Edit`]s that were applied, in order, ready
/// to hand to an incremental consumer:
///
/// ```ignore
/// for edit in doc.insert(offset, "select ") {
///     tree.edit(&edit.into());
/// }
/// let tree = parser.parse(doc.buffer().rope(), Some(&tree));
/// ```
#[derive(Debug, Clone, Default)]
pub struct Document<H: History = LinearHistory> {
    buffer: Buffer,
    history: H,
}

impl Document<LinearHistory> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::with_history(text, LinearHistory::new())
    }
}

impl<H: History> Document<H> {
    pub fn with_history(text: &str, history: H) -> Self {
        Self {
            buffer: Buffer::from_text(text),
            history,
        }
    }

    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    #[must_use]
    pub fn history(&self) -> &H {
        &self.history
    }

    pub fn history_mut(&mut self) -> &mut H {
        &mut self.history
    }

    /// Replace `range` with `text`, recording it first so the history sees the
    /// pre-image.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Edit {
        let change = self.buffer.stage_replace(range, text);
        self.history.record(&change, &self.buffer);
        self.buffer.apply(&change);
        change.edit
    }

    pub fn insert(&mut self, at: usize, text: &str) -> Edit {
        self.replace(at..at, text)
    }

    pub fn delete(&mut self, range: Range<usize>) -> Edit {
        self.replace(range, "")
    }

    /// Group the changes made by `edits` into one undo step.
    ///
    /// A [`Document`] has no caret, so nothing is recorded for `undo` to restore.
    /// Callers that do have one bracket the group themselves — see
    /// [`History::begin_group`].
    ///
    /// Not a drop guard: an unwinding panic inside `edits` leaves the group open.
    /// That is deliberate — a panic here means buffer and history have already
    /// diverged, and silently closing the group would hide it.
    pub fn grouped<T>(&mut self, edits: impl FnOnce(&mut Self) -> T) -> T {
        self.history.begin_group(None);
        let out = edits(self);
        self.history.end_group(None);
        out
    }

    pub fn undo(&mut self) -> Revert {
        let step = self.history.undo();
        self.revert(&step)
    }

    pub fn redo(&mut self) -> Revert {
        let step = self.history.redo();
        self.revert(&step)
    }

    /// vi's `U`. Returns empty if the history does not support it, or if there is
    /// no row to restore.
    pub fn undo_row(&mut self) -> Revert {
        let step = self.history.undo_row(&self.buffer);
        self.revert(&step)
    }

    fn revert(&mut self, step: &Step) -> Revert {
        Revert {
            edits: self.apply_all(&step.changes),
            cursor: step.cursor,
        }
    }

    /// Apply changes handed back by the history. Deliberately does not `record`
    /// them — the history has already accounted for them.
    fn apply_all(&mut self, changes: &[Change]) -> Vec<Edit> {
        changes
            .iter()
            .map(|change| {
                self.buffer.apply(change);
                change.edit
            })
            .collect()
    }
}

impl<H: History> fmt::Display for Document<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.buffer.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Point;
    use crate::history::NoHistory;

    #[test]
    fn insert_mode_session_is_one_undo_step() {
        let mut doc = Document::from_text("");
        let edits = doc.grouped(|doc| {
            "select"
                .char_indices()
                .map(|(i, ch)| doc.insert(i, &ch.to_string()))
                .collect::<Vec<_>>()
        });
        // One edit per keystroke for the parser...
        assert_eq!(edits.len(), 6);
        // ...but one step for the user.
        assert_eq!(doc.history().undo_depth(), 1);
        doc.undo();
        assert_eq!(doc.to_string(), "");
    }

    #[test]
    fn edits_come_back_in_application_order() {
        let mut doc = Document::from_text("abc");
        doc.grouped(|doc| {
            doc.replace(0..1, "X");
            doc.replace(2..3, "Y");
        });
        let undone = doc.undo();
        assert_eq!(undone.edits.len(), 2);
        // Reversed relative to how they were applied.
        assert_eq!(undone.edits[0].start_byte, 2);
        assert_eq!(undone.edits[1].start_byte, 0);
        // A `Document` has no caret, so `grouped` records none.
        assert_eq!(undone.cursor, None);
        assert_eq!(doc.to_string(), "abc");
    }

    #[test]
    fn row_undo_through_the_document() {
        let mut doc = Document::from_text("select id\nfrom users");
        doc.replace(7..9, "name");
        doc.replace(0..6, "SELECT");
        let reverted = doc.undo_row();
        assert_eq!(reverted.edits.len(), 1);
        assert_eq!(reverted.edits[0].start_point, Point::new(0, 0));
        // `U` knows the row it restored.
        assert_eq!(reverted.cursor, Some(0));
        assert_eq!(doc.to_string(), "select id\nfrom users");
    }

    #[test]
    fn a_document_without_history_still_edits() {
        let mut doc = Document::with_history("select 1", NoHistory);
        doc.replace(7..8, "2");
        assert_eq!(doc.to_string(), "select 2");
        assert!(doc.undo().is_empty());
        assert_eq!(doc.to_string(), "select 2");
    }
}
