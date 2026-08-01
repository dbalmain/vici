//! Undo, as a swappable policy over the change stream.
//!
//! Nothing here knows what a rope is, and [`crate::Buffer`] knows nothing about
//! any of it. The only currency is [`Change`], which carries the text it
//! displaced and is therefore self-inverting.
//!
//! That seam is what makes vi's `U` expressible. Editors whose history is a flat
//! list of opaque transactions cannot offer it, because nothing in the log says
//! which row a change belonged to. Here, [`Edit::is_single_row`] and
//! `start_point.row` say exactly that, so a line-scoped snapshot is cheap
//! bookkeeping — see [`LinearHistory::undo_row`].
//!
//! [`Edit::is_single_row`]: crate::Edit::is_single_row

use crate::buffer::Buffer;
use crate::edit::Change;

/// A policy for remembering changes and handing back the changes that reverse
/// them.
///
/// # Contract
///
/// - [`record`](History::record) is called **before** the change is applied, so
///   `buf` is the pre-image. Implementations that need to snapshot prior state
///   (line-scoped undo, checkpointing) depend on this.
/// - [`undo`](History::undo), [`redo`](History::redo) and
///   [`undo_row`](History::undo_row) return changes for the caller to apply, in
///   the order given. The caller must apply all of them and must **not** feed
///   them back through `record`. [`crate::Document`] enforces this.
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
    fn begin_group(&mut self);

    /// Close the innermost open group. Calls nest.
    fn end_group(&mut self);

    /// Changes that undo the most recent step.
    fn undo(&mut self) -> Vec<Change>;

    /// Changes that reapply the most recently undone step.
    fn redo(&mut self) -> Vec<Change>;

    /// vi's `U`: restore the most recently changed row to its content from
    /// before the current run of changes on it.
    ///
    /// Defaults to unsupported, so simple histories opt out by saying nothing.
    fn undo_row(&mut self, _buf: &Buffer) -> Vec<Change> {
        Vec::new()
    }
}

/// Discards everything. Undo is a no-op.
///
/// Useful for throwaway single-line inputs, and as proof that the buffer really
/// does not depend on history.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHistory;

impl History for NoHistory {
    fn record(&mut self, _change: &Change, _buf: &Buffer) {}
    fn begin_group(&mut self) {}
    fn end_group(&mut self) {}
    fn undo(&mut self) -> Vec<Change> {
        Vec::new()
    }
    fn redo(&mut self) -> Vec<Change> {
        Vec::new()
    }
}

#[derive(Debug, Clone)]
struct RowSnapshot {
    row: usize,
    content: String,
}

/// A linear undo stack with grouping and row-scoped undo.
///
/// This is the ordinary vi model: `u` walks back through groups, `C-r` walks
/// forward, a new change truncates the redo tail, and `U` toggles one row.
///
/// An undo *tree* is a different implementation of the same trait — it keeps
/// branches instead of truncating them. Nothing else has to change.
#[derive(Debug, Clone, Default)]
pub struct LinearHistory {
    /// `groups[..cursor]` are applied to the buffer; `groups[cursor..]` are the
    /// redo tail.
    groups: Vec<Vec<Change>>,
    cursor: usize,
    depth: usize,
    open: Vec<Change>,
    row: Option<RowSnapshot>,
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

    /// The row `undo_row` would act on, if any.
    #[must_use]
    pub fn pending_row(&self) -> Option<usize> {
        self.row.as_ref().map(|snapshot| snapshot.row)
    }

    fn push_group(&mut self, changes: Vec<Change>) {
        self.groups.truncate(self.cursor);
        self.groups.push(changes);
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

    /// Maintain the row-scoped snapshot.
    ///
    /// The first change to land on a row captures that row's prior content;
    /// subsequent changes to the same row accumulate against it, so `U` reverses
    /// the whole run rather than one edit. Any change that spans or alters row
    /// structure abandons the snapshot — vi's `U` is deliberately not a
    /// multi-line operation.
    fn note_row(&mut self, change: &Change, buf: &Buffer) {
        if !change.edit.is_single_row() {
            self.row = None;
            return;
        }
        let row = change.edit.start_point.row;
        if self.row.as_ref().is_none_or(|snapshot| snapshot.row != row) {
            self.row = Some(RowSnapshot {
                row,
                content: buf.row_text(row),
            });
        }
    }
}

impl History for LinearHistory {
    fn record(&mut self, change: &Change, buf: &Buffer) {
        if change.is_noop() {
            return;
        }
        self.note_row(change, buf);
        if self.depth > 0 {
            self.groups.truncate(self.cursor);
            self.open.push(change.clone());
        } else {
            self.push_group(vec![change.clone()]);
        }
    }

    fn begin_group(&mut self) {
        self.depth += 1;
    }

    fn end_group(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        if self.depth == 0 && !self.open.is_empty() {
            let changes = core::mem::take(&mut self.open);
            self.push_group(changes);
        }
    }

    fn undo(&mut self) -> Vec<Change> {
        if self.cursor == 0 {
            return Vec::new();
        }
        self.cursor -= 1;
        self.row = None;
        // Reverse order: a group applied c1, c2, c3 is undone by inv(c3),
        // inv(c2), inv(c1).
        self.groups[self.cursor]
            .iter()
            .rev()
            .map(Change::inverted)
            .collect()
    }

    fn redo(&mut self) -> Vec<Change> {
        if self.cursor >= self.groups.len() {
            return Vec::new();
        }
        let changes = self.groups[self.cursor].clone();
        self.cursor += 1;
        self.row = None;
        changes
    }

    fn undo_row(&mut self, buf: &Buffer) -> Vec<Change> {
        let Some(snapshot) = self.row.take() else {
            return Vec::new();
        };
        if snapshot.row >= buf.len_rows() {
            return Vec::new();
        }
        let range = buf.row_content_range(snapshot.row);
        let current = buf.text_in(range.clone());
        if current == snapshot.content {
            self.row = Some(snapshot);
            return Vec::new();
        }
        let change = buf.stage_replace(range, &snapshot.content);
        // `U` is an ordinary change as far as `u` is concerned.
        self.push_group(vec![change.clone()]);
        // Swapping the snapshot for what we just displaced is what makes a second
        // `U` toggle back.
        self.row = Some(RowSnapshot {
            row: snapshot.row,
            content: current,
        });
        vec![change]
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

        fn undo(&mut self) {
            let changes = self.hist.undo();
            self.apply_all(&changes);
        }

        fn redo(&mut self) {
            let changes = self.hist.redo();
            self.apply_all(&changes);
        }

        fn undo_row(&mut self) {
            let changes = self.hist.undo_row(&self.buf);
            self.apply_all(&changes);
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
        h.hist.begin_group();
        for (i, ch) in "select".chars().enumerate() {
            h.replace(i..i, &ch.to_string());
        }
        h.hist.end_group();
        assert_eq!(h.text(), "select");
        assert_eq!(h.hist.undo_depth(), 1);
        h.undo();
        assert_eq!(h.text(), "");
    }

    #[test]
    fn groups_nest() {
        let mut h = Harness::new("");
        h.hist.begin_group();
        h.hist.begin_group();
        h.replace(0..0, "a");
        h.hist.end_group();
        h.replace(1..1, "b");
        h.hist.end_group();
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
        h.hist.begin_group();
        h.replace(0..1, "X"); // Xbc
        h.replace(2..3, "Y"); // XbY
        h.hist.end_group();
        assert_eq!(h.text(), "XbY");
        h.undo();
        assert_eq!(h.text(), "abc");
    }

    #[test]
    fn u_restores_a_whole_run_of_changes_on_one_row() {
        let mut h = Harness::new("select id\nfrom users");
        h.replace(7..9, "name"); // select name
        h.replace(0..6, "SELECT"); // SELECT name
        assert_eq!(h.buf.row_text(0), "SELECT name");
        assert_eq!(h.hist.pending_row(), Some(0));

        h.undo_row();
        assert_eq!(h.buf.row_text(0), "select id");
        // Row 1 was never touched.
        assert_eq!(h.buf.row_text(1), "from users");
    }

    #[test]
    fn u_toggles() {
        let mut h = Harness::new("select id");
        h.replace(7..9, "name");
        h.undo_row();
        assert_eq!(h.text(), "select id");
        h.undo_row();
        assert_eq!(h.text(), "select name");
        h.undo_row();
        assert_eq!(h.text(), "select id");
    }

    #[test]
    fn u_is_itself_undoable_with_u_lowercase() {
        let mut h = Harness::new("select id");
        h.replace(7..9, "name");
        h.undo_row();
        assert_eq!(h.text(), "select id");
        h.undo();
        assert_eq!(h.text(), "select name");
    }

    #[test]
    fn moving_to_another_row_resnapshots() {
        let mut h = Harness::new("aaa\nbbb");
        h.replace(0..1, "X"); // Xaa
        h.replace(4..5, "Y"); // Ybb
        assert_eq!(h.hist.pending_row(), Some(1));
        h.undo_row();
        assert_eq!(h.text(), "Xaa\nbbb");
    }

    #[test]
    fn a_multi_row_change_abandons_the_row_snapshot() {
        let mut h = Harness::new("aaa\nbbb");
        h.replace(0..1, "X");
        assert_eq!(h.hist.pending_row(), Some(0));
        h.replace(3..4, ""); // joins the rows
        assert_eq!(h.hist.pending_row(), None);
        h.undo_row();
        assert_eq!(h.text(), "Xaabbb");
    }

    #[test]
    fn u_does_nothing_before_any_change() {
        let mut h = Harness::new("select 1");
        h.undo_row();
        assert_eq!(h.text(), "select 1");
        assert_eq!(h.hist.undo_depth(), 0);
    }

    #[test]
    fn a_plain_undo_abandons_the_row_snapshot() {
        let mut h = Harness::new("select id");
        h.replace(7..9, "name");
        h.undo();
        assert_eq!(h.hist.pending_row(), None);
        h.undo_row();
        assert_eq!(h.text(), "select id");
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
        assert!(hist.undo_row(&buf).is_empty());
        assert_eq!(buf.to_string(), "select 2");
    }

    #[test]
    fn noop_changes_are_not_recorded() {
        let mut h = Harness::new("select 1");
        h.replace(7..8, "1");
        assert_eq!(h.hist.undo_depth(), 0);
    }
}
