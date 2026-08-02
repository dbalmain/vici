//! The minimal composition of a buffer and a history.
//!
//! This type exists for one reason: the `stage → record → apply` ordering that
//! [`LinearHistory`] depends on is easy to get subtly wrong. Encoding it once
//! removes the hazard.

use core::fmt;
use core::ops::Range;

use crate::buffer::Buffer;
use crate::edit::{Change, Edit};
use crate::history::{LinearHistory, Step};

/// A buffer paired with a linear undo history.
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
pub struct Document {
    buffer: Buffer,
    history: LinearHistory,
}

impl Document {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self {
            buffer: Buffer::from_text(text),
            history: LinearHistory::new(),
        }
    }

    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    #[must_use]
    pub fn history(&self) -> &LinearHistory {
        &self.history
    }

    /// Mutable access, for configuration such as [`LinearHistory::set_limit`].
    pub fn history_mut(&mut self) -> &mut LinearHistory {
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
    /// [`LinearHistory::begin_group`].
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

    /// Undo the most recent step and return the changes that were applied.
    pub fn undo(&mut self) -> Step {
        let step = self.history.undo();
        self.apply_all(&step.changes);
        step
    }

    /// Redo the most recently undone step and return the changes that were applied.
    pub fn redo(&mut self) -> Step {
        let step = self.history.redo();
        self.apply_all(&step.changes);
        step
    }

    /// Apply changes handed back by the history. Deliberately does not `record`
    /// them — the history has already accounted for them.
    fn apply_all(&mut self, changes: &[Change]) {
        for change in changes {
            self.buffer.apply(change);
        }
    }
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.buffer.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_the_preimage_before_applying_a_change() {
        let mut document = Document::from_text("abc");
        document.replace(0..1, "X");
        assert_eq!(document.to_string(), "Xbc");
        document.undo();
        assert_eq!(document.to_string(), "abc");
        document.redo();
        assert_eq!(document.to_string(), "Xbc");
    }
}
