//! A swappable undo policy over a stream of self-inverting changes.
//!
//! Nothing here knows what a rope is, and [`crate::Buffer`] knows nothing about
//! any of it. The only currency is [`Change`], which carries the text it
//! displaced and is therefore self-inverting.

use crate::buffer::Buffer;
use crate::edit::Change;

/// Changes that reverse a step, and where the caret was when it began.
///
/// Text and caret travel together because restoring one without the other is
/// jarring: undoing an `o` should put you back where you pressed it, not at the
/// end of the row the newline was appended to. A history that does not track the
/// caret leaves [`cursor`](Step::cursor) as `None` and the caller falls back to
/// whatever it can infer from the changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Step {
    pub changes: Vec<Change>,
    /// Byte offset to restore the caret to, if this history remembers one.
    pub cursor: Option<usize>,
}

impl Step {
    /// True when there was nothing to undo or redo.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// A policy for remembering changes and handing back the changes that reverse
/// them.
///
/// # Contract
///
/// - [`record`](History::record) is called **before** the change is applied, so
///   `buf` is the pre-image.
/// - [`undo`](History::undo) and [`redo`](History::redo) return changes for the
///   caller to apply, in the order given. The caller must apply all of them and
///   must **not** feed them back through `record`. [`crate::Document`] enforces
///   this.
/// - An empty return means "nothing to do", not an error.
pub trait History {
    /// Observe a change about to be applied to `buf`.
    fn record(&mut self, change: &Change, buf: &Buffer);

    /// Open a group. Changes recorded until the matching
    /// [`end_group`](History::end_group) undo as one step.
    ///
    /// An insert-mode session is one group. Note that this is a *coarser*
    /// granularity than the [`crate::Edit`] stream a host feeds to tree-sitter,
    /// which wants one edit per keystroke — the two must not be conflated.
    ///
    /// `cursor` is the caret position the group starts from, for
    /// [`Step::cursor`] to hand back on undo. Callers with no caret pass `None`.
    /// Only the outermost group's value is kept, since nesting exists to let an
    /// inner bracket be a no-op.
    fn begin_group(&mut self, cursor: Option<usize>);

    /// Close the innermost open group. Calls nest.
    ///
    /// `cursor` is the caret position the group ends at, which is what a *redo*
    /// restores.
    fn end_group(&mut self, cursor: Option<usize>);

    /// Changes that undo the most recent step.
    fn undo(&mut self) -> Step;

    /// Changes that reapply the most recently undone step.
    fn redo(&mut self) -> Step;
}

/// Discards everything. Undo is a no-op.
///
/// Useful for throwaway single-line inputs, and as proof that the buffer really
/// does not depend on history.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHistory;

impl History for NoHistory {
    fn record(&mut self, _change: &Change, _buf: &Buffer) {}
    fn begin_group(&mut self, _cursor: Option<usize>) {}
    fn end_group(&mut self, _cursor: Option<usize>) {}
    fn undo(&mut self) -> Step {
        Step::default()
    }
    fn redo(&mut self) -> Step {
        Step::default()
    }
}

/// One undo step: the changes it applied, bracketed by the caret on either side.
#[derive(Debug, Clone)]
struct Group {
    changes: Vec<Change>,
    /// Caret before the changes, restored by `undo`.
    before: Option<usize>,
    /// Caret after them, restored by `redo`.
    after: Option<usize>,
}

/// A linear undo stack with grouping.
///
/// `u` walks back through groups, `C-r` walks forward, and a new change
/// truncates the redo tail.
///
/// An undo *tree* is a different implementation of the same trait — it keeps
/// branches instead of truncating them. Nothing else has to change.
#[derive(Debug, Clone, Default)]
pub struct LinearHistory {
    /// `groups[..cursor]` are applied to the buffer; `groups[cursor..]` are the
    /// redo tail.
    groups: Vec<Group>,
    cursor: usize,
    depth: usize,
    open: Vec<Change>,
    /// Caret at the moment the outermost open group began.
    open_from: Option<usize>,
    limit: Option<usize>,
}

impl LinearHistory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep at most `groups` undo steps, discarding the oldest.
    #[must_use]
    pub fn with_limit(groups: usize) -> Self {
        Self {
            limit: Some(groups),
            ..Self::default()
        }
    }

    /// Number of steps `undo` can take.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.cursor
    }

    /// Number of steps `redo` can take.
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        self.groups.len() - self.cursor
    }

    fn push_group(&mut self, group: Group) {
        self.groups.truncate(self.cursor);
        self.groups.push(group);
        self.cursor = self.groups.len();
        self.trim();
    }

    fn trim(&mut self) {
        if let Some(limit) = self.limit
            && self.groups.len() > limit
        {
            let excess = self.groups.len() - limit;
            self.groups.drain(..excess);
            self.cursor = self.cursor.saturating_sub(excess);
        }
    }
}

impl History for LinearHistory {
    fn record(&mut self, change: &Change, _buf: &Buffer) {
        if change.is_noop() {
            return;
        }
        if self.depth > 0 {
            self.groups.truncate(self.cursor);
            self.open.push(change.clone());
        } else {
            // Ungrouped, so there is no caret to bracket it with.
            self.push_group(Group {
                changes: vec![change.clone()],
                before: None,
                after: None,
            });
        }
    }

    fn begin_group(&mut self, cursor: Option<usize>) {
        if self.depth == 0 {
            self.open_from = cursor;
        }
        self.depth += 1;
    }

    fn end_group(&mut self, cursor: Option<usize>) {
        self.depth = self.depth.saturating_sub(1);
        if self.depth > 0 {
            return;
        }
        let before = self.open_from.take();
        if !self.open.is_empty() {
            let changes = core::mem::take(&mut self.open);
            self.push_group(Group {
                changes,
                before,
                after: cursor,
            });
        }
    }

    fn undo(&mut self) -> Step {
        if self.cursor == 0 {
            return Step::default();
        }
        self.cursor -= 1;
        let group = &self.groups[self.cursor];
        Step {
            // Reverse order: a group applied c1, c2, c3 is undone by inv(c3),
            // inv(c2), inv(c1).
            changes: group.changes.iter().rev().map(Change::inverted).collect(),
            cursor: group.before,
        }
    }

    fn redo(&mut self) -> Step {
        if self.cursor >= self.groups.len() {
            return Step::default();
        }
        let group = &self.groups[self.cursor];
        let step = Step {
            changes: group.changes.clone(),
            cursor: group.after,
        };
        self.cursor += 1;
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a history the way `Document` does, so these tests exercise the real
    /// call ordering.
    struct Harness {
        buf: Buffer,
        hist: LinearHistory,
    }

    impl Harness {
        fn new(text: &str) -> Self {
            Self {
                buf: Buffer::from_text(text),
                hist: LinearHistory::new(),
            }
        }

        fn replace(&mut self, range: core::ops::Range<usize>, text: &str) {
            let change = self.buf.stage_replace(range, text);
            self.hist.record(&change, &self.buf);
            self.buf.apply(&change);
        }

        fn apply_all(&mut self, changes: &[Change]) {
            for change in changes {
                self.buf.apply(change);
            }
        }

        fn undo(&mut self) -> Option<usize> {
            let step = self.hist.undo();
            self.apply_all(&step.changes);
            step.cursor
        }

        fn redo(&mut self) -> Option<usize> {
            let step = self.hist.redo();
            self.apply_all(&step.changes);
            step.cursor
        }

        fn text(&self) -> String {
            self.buf.to_string()
        }
    }

    #[test]
    fn undo_and_redo_a_single_change() {
        let mut h = Harness::new("select 1");
        h.replace(7..8, "2");
        assert_eq!(h.text(), "select 2");
        h.undo();
        assert_eq!(h.text(), "select 1");
        h.redo();
        assert_eq!(h.text(), "select 2");
    }

    #[test]
    fn a_group_undoes_as_one_step() {
        let mut h = Harness::new("");
        h.hist.begin_group(None);
        for (i, ch) in "select".chars().enumerate() {
            h.replace(i..i, &ch.to_string());
        }
        h.hist.end_group(None);
        assert_eq!(h.text(), "select");
        assert_eq!(h.hist.undo_depth(), 1);
        h.undo();
        assert_eq!(h.text(), "");
    }

    #[test]
    fn groups_nest() {
        let mut h = Harness::new("");
        h.hist.begin_group(None);
        h.hist.begin_group(None);
        h.replace(0..0, "a");
        h.hist.end_group(None);
        h.replace(1..1, "b");
        h.hist.end_group(None);
        assert_eq!(h.hist.undo_depth(), 1);
        h.undo();
        assert_eq!(h.text(), "");
    }

    #[test]
    fn a_new_change_truncates_the_redo_tail() {
        let mut h = Harness::new("a");
        h.replace(1..1, "b");
        h.undo();
        assert_eq!(h.hist.redo_depth(), 1);
        h.replace(1..1, "c");
        assert_eq!(h.hist.redo_depth(), 0);
        h.redo();
        assert_eq!(h.text(), "ac");
    }

    #[test]
    fn undo_reverses_a_group_in_reverse_order() {
        let mut h = Harness::new("abc");
        h.hist.begin_group(None);
        h.replace(0..1, "X"); // Xbc
        h.replace(2..3, "Y"); // XbY
        h.hist.end_group(None);
        assert_eq!(h.text(), "XbY");
        h.undo();
        assert_eq!(h.text(), "abc");
    }

    #[test]
    fn limit_discards_the_oldest_groups() {
        let mut h = Harness::new("");
        h.hist = LinearHistory::with_limit(2);
        h.replace(0..0, "a");
        h.replace(1..1, "b");
        h.replace(2..2, "c");
        assert_eq!(h.hist.undo_depth(), 2);
        h.undo();
        h.undo();
        assert_eq!(h.text(), "a");
        h.undo();
        assert_eq!(h.text(), "a");
    }

    #[test]
    fn no_history_records_nothing() {
        let mut hist = NoHistory;
        let mut buf = Buffer::from_text("select 1");
        let change = buf.stage_replace(7..8, "2");
        hist.record(&change, &buf);
        buf.apply(&change);
        assert!(hist.undo().is_empty());
        assert!(hist.redo().is_empty());
        assert_eq!(buf.to_string(), "select 2");
    }

    #[test]
    fn noop_changes_are_not_recorded() {
        let mut h = Harness::new("select 1");
        h.replace(7..8, "1");
        assert_eq!(h.hist.undo_depth(), 0);
    }
}
