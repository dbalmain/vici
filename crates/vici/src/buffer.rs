//! The text buffer. Knows nothing about modes, keys, history or rendering.

use core::fmt;
use core::ops::Range;

use ropey::{LineType, Rope, RopeSlice};

use crate::edit::{Change, Edit, Point};

/// Rows are counted by LF only, to match `tree_sitter::Point.row`.
///
/// A `\r\n` therefore counts as one row break with the `\r` left as an ordinary
/// content byte at the end of the row — which is exactly how the parser sees it.
/// Line endings are never rewritten.
const LINES: LineType = LineType::LF;

/// A UTF-8 text buffer addressed entirely in **byte** offsets.
///
/// Every index in this type's public API is a byte offset, and every [`Point`]
/// column is a byte offset within its row. Character and grapheme space belong
/// to the motion layer; display-width space belongs to the view layer. Keeping
/// this type single-valued is what stops the three from being confused.
#[derive(Debug, Clone, Default)]
pub struct Buffer {
    rope: Rope,
}

impl Buffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
        }
    }

    #[must_use]
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> RopeSlice<'_> {
        self.rope.slice(range)
    }

    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.rope.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rope.len() == 0
    }

    /// Number of rows. A buffer with no trailing newline still counts its final
    /// partial row, and an empty buffer has one row.
    #[must_use]
    pub fn len_rows(&self) -> usize {
        self.rope.len_lines(LINES)
    }

    /// The byte at `idx`, without materialising anything.
    ///
    /// # Panics
    /// If `idx >= len_bytes()`.
    #[must_use]
    pub fn byte(&self, idx: usize) -> u8 {
        let (chunk, chunk_start) = self.rope.chunk(idx);
        chunk.as_bytes()[idx - chunk_start]
    }

    #[must_use]
    pub fn byte_to_point(&self, byte: usize) -> Point {
        let row = self.rope.byte_to_line_idx(byte, LINES);
        let row_start = self.rope.line_to_byte_idx(row, LINES);
        Point::new(row, byte - row_start)
    }

    /// Clamps into the buffer rather than panicking, so callers converting a
    /// stale point don't have to pre-validate.
    #[must_use]
    pub fn point_to_byte(&self, point: Point) -> usize {
        let row = point.row.min(self.len_rows().saturating_sub(1));
        let content = self.row_content_range(row);
        (content.start + point.col).min(content.end)
    }

    /// Byte range of `row` **including** its line terminator.
    #[must_use]
    pub fn row_range(&self, row: usize) -> Range<usize> {
        let start = self.rope.line_to_byte_idx(row, LINES);
        let end = if row + 1 < self.len_rows() {
            self.rope.line_to_byte_idx(row + 1, LINES)
        } else {
            self.rope.len()
        };
        start..end
    }

    /// Byte range of `row` **excluding** its line terminator.
    ///
    /// This is the range a line-scoped operation should target, so that
    /// restoring a row's content never disturbs the row structure.
    #[must_use]
    pub fn row_content_range(&self, row: usize) -> Range<usize> {
        let full = self.row_range(row);
        let mut end = full.end;
        if end > full.start && self.byte(end - 1) == b'\n' {
            end -= 1;
        }
        if end > full.start && self.byte(end - 1) == b'\r' {
            end -= 1;
        }
        full.start..end
    }

    #[must_use]
    pub fn row_text(&self, row: usize) -> String {
        self.text_in(self.row_content_range(row))
    }

    #[must_use]
    pub fn text_in(&self, range: Range<usize>) -> String {
        let mut out = String::with_capacity(range.end - range.start);
        for chunk in self.rope.slice(range).chunks() {
            out.push_str(chunk);
        }
        out
    }

    /// Compute the [`Change`] that replacing `range` with `text` would produce,
    /// **without** mutating anything.
    ///
    /// Separating this from [`Buffer::apply`] is what lets a history see the
    /// pre-image of a change. `U` needs it: to restore a row later, something has
    /// to capture that row's content before the first edit lands on it.
    ///
    /// # Panics
    /// If `range` is out of bounds or not on `char` boundaries.
    #[must_use]
    pub fn stage_replace(&self, range: Range<usize>, text: &str) -> Change {
        let start_point = self.byte_to_point(range.start);
        Change {
            edit: Edit {
                start_byte: range.start,
                old_end_byte: range.end,
                new_end_byte: range.start + text.len(),
                start_point,
                old_end_point: self.byte_to_point(range.end),
                new_end_point: advance(start_point, text),
            },
            removed: self.text_in(range),
            inserted: text.to_owned(),
        }
    }

    /// Apply a previously staged change.
    ///
    /// # Panics
    /// In debug builds, if the buffer does not currently hold `change.removed`
    /// at the change's old extent — which means buffer and history have drifted.
    pub fn apply(&mut self, change: &Change) {
        let edit = &change.edit;
        debug_assert_eq!(
            self.text_in(edit.start_byte..edit.old_end_byte),
            change.removed,
            "buffer does not match the change being applied"
        );
        if edit.old_end_byte > edit.start_byte {
            self.rope.remove(edit.start_byte..edit.old_end_byte);
        }
        if !change.inserted.is_empty() {
            self.rope.insert(edit.start_byte, &change.inserted);
        }
    }

    /// Stage and apply in one step. Convenient for tests and for callers that
    /// keep no history.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Change {
        let change = self.stage_replace(range, text);
        self.apply(&change);
        change
    }

    pub fn insert(&mut self, at: usize, text: &str) -> Change {
        self.replace(at..at, text)
    }

    pub fn delete(&mut self, range: Range<usize>) -> Change {
        self.replace(range, "")
    }
}

/// Where `start` ends up after `text` is inserted there.
///
/// The subtlety: when `text` contains a newline, the resulting column is measured
/// from the start of the *final* row of `text` — it is not `start.col + len`.
fn advance(start: Point, text: &str) -> Point {
    match text.rfind('\n') {
        None => Point::new(start.row, start.col + text.len()),
        Some(last) => Point::new(
            start.row + text.bytes().filter(|&b| b == b'\n').count(),
            text.len() - last - 1,
        ),
    }
}

impl fmt::Display for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.rope.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

impl From<&str> for Buffer {
    fn from(text: &str) -> Self {
        Self::from_text(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The running example. Byte offsets:
    ///   row 0 `select id, name`  0..15, LF at 15
    ///   row 1 `from users`      16..26, LF at 26
    ///   row 2 `where id = 1`    27..39
    const SQL: &str = "select id, name\nfrom users\nwhere id = 1";

    fn buf() -> Buffer {
        Buffer::from_text(SQL)
    }

    #[test]
    fn offsets_are_where_we_think() {
        let b = buf();
        assert_eq!(b.len_bytes(), 39);
        assert_eq!(b.len_rows(), 3);
        assert_eq!(b.row_range(1), 16..27);
        assert_eq!(b.row_content_range(1), 16..26);
        assert_eq!(b.row_text(1), "from users");
        assert_eq!(b.byte_to_point(38), Point::new(2, 11));
        assert_eq!(b.point_to_byte(Point::new(2, 11)), 38);
    }

    #[test]
    fn x_on_a_single_byte() {
        let mut b = buf();
        let c = b.delete(38..39);
        assert_eq!(c.edit.start_byte, 38);
        assert_eq!(c.edit.old_end_byte, 39);
        assert_eq!(c.edit.new_end_byte, 38);
        assert_eq!(c.edit.start_point, Point::new(2, 11));
        assert_eq!(c.edit.old_end_point, Point::new(2, 12));
        assert_eq!(c.edit.new_end_point, Point::new(2, 11));
        assert_eq!(c.removed, "1");
        assert!(c.edit.is_deletion());
        assert!(c.edit.is_single_row());
        assert_eq!(b.row_text(2), "where id = ");
    }

    #[test]
    fn pure_insertion() {
        let mut b = buf();
        let c = b.insert(15, ", email");
        assert_eq!(c.edit.start_byte, 15);
        assert_eq!(c.edit.old_end_byte, 15);
        assert_eq!(c.edit.new_end_byte, 22);
        assert_eq!(c.edit.start_point, Point::new(0, 15));
        assert_eq!(c.edit.old_end_point, Point::new(0, 15));
        assert_eq!(c.edit.new_end_point, Point::new(0, 22));
        assert!(c.edit.is_insertion());
        assert_eq!(b.row_text(0), "select id, name, email");
    }

    #[test]
    fn cw_is_one_change_not_two() {
        let mut b = buf();
        let c = b.replace(21..26, "accounts");
        assert_eq!(c.edit.start_byte, 21);
        assert_eq!(c.edit.old_end_byte, 26);
        assert_eq!(c.edit.new_end_byte, 29);
        assert_eq!(c.edit.start_point, Point::new(1, 5));
        assert_eq!(c.edit.old_end_point, Point::new(1, 10));
        assert_eq!(c.edit.new_end_point, Point::new(1, 13));
        assert_eq!(c.removed, "users");
        assert_eq!(b.row_text(1), "from accounts");
    }

    #[test]
    fn dd_consumes_the_newline_and_changes_the_row_count() {
        let mut b = buf();
        let c = b.delete(16..27);
        assert_eq!(c.edit.start_point, Point::new(1, 0));
        // Row 2, not (1, 10): swallowing the LF extends the region to the start
        // of the next row. No byte offset can express that.
        assert_eq!(c.edit.old_end_point, Point::new(2, 0));
        assert_eq!(c.edit.new_end_point, Point::new(1, 0));
        assert!(!c.edit.is_single_row());
        assert_eq!(b.len_rows(), 2);
        assert_eq!(b.to_string(), "select id, name\nwhere id = 1");
    }

    #[test]
    fn multi_row_insertion_measures_the_column_on_the_last_row() {
        let mut b = buf();
        let c = b.insert(39, "\n  and name is not null");
        assert_eq!(c.edit.new_end_byte, 62);
        assert_eq!(c.edit.start_point, Point::new(2, 12));
        // (3, 22), not (2, 35).
        assert_eq!(c.edit.new_end_point, Point::new(3, 22));
        assert_eq!(b.len_rows(), 4);
    }

    #[test]
    fn columns_are_bytes_not_characters() {
        let b = Buffer::from_text("-- café\nselect 1");
        // `é` is two bytes, so the row is 8 bytes for 7 characters.
        assert_eq!(b.row_content_range(0), 0..8);
        assert_eq!(b.byte_to_point(8), Point::new(0, 8));
        assert_eq!(b.byte_to_point(9), Point::new(1, 0));
    }

    #[test]
    fn crlf_leaves_the_cr_as_content() {
        let b = Buffer::from_text("a\r\nb");
        assert_eq!(b.len_rows(), 2);
        assert_eq!(b.row_range(0), 0..3);
        assert_eq!(b.row_content_range(0), 0..1);
        assert_eq!(b.row_text(0), "a");
    }

    #[test]
    fn applying_an_inverted_change_restores_the_buffer() {
        let mut b = buf();
        let c = b.replace(21..26, "accounts\nfrom logs");
        b.apply(&c.inverted());
        assert_eq!(b.to_string(), SQL);
        assert_eq!(b.len_rows(), 3);
    }

    #[test]
    fn point_to_byte_clamps_stale_points() {
        let b = buf();
        assert_eq!(b.point_to_byte(Point::new(99, 0)), 27);
        assert_eq!(b.point_to_byte(Point::new(1, 99)), 26);
    }

    #[test]
    fn empty_buffer_has_one_row() {
        let b = Buffer::new();
        assert!(b.is_empty());
        assert_eq!(b.len_rows(), 1);
        assert_eq!(b.row_content_range(0), 0..0);
        assert_eq!(b.byte_to_point(0), Point::new(0, 0));
    }
}
