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
    /// Separating this from [`Buffer::apply`] lets a history observe the pre-image
    /// of a change.
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

    #[test]
    fn rows_are_counted_by_lf_only() {
        let buffer = Buffer::from_text("a\rb\nc");
        assert_eq!(buffer.len_rows(), 2);
        assert_eq!(buffer.row_text(0), "a\rb");
        assert_eq!(buffer.byte_to_point(4), Point::new(1, 0));
    }

    #[test]
    fn crlf_is_preserved_as_a_single_terminator() {
        let buffer = Buffer::from_text("a\r\nb");
        assert_eq!(buffer.row_range(0), 0..3);
        assert_eq!(buffer.row_content_range(0), 0..1);
        assert_eq!(buffer.row_text(0), "a");
        assert_eq!(buffer.to_string(), "a\r\nb");
    }

    #[test]
    fn point_to_byte_clamps_stale_coordinates() {
        let buffer = Buffer::from_text("one\ntwo");
        assert_eq!(buffer.point_to_byte(Point::new(99, 0)), 4);
        assert_eq!(buffer.point_to_byte(Point::new(1, 99)), 7);
    }

    #[test]
    fn an_empty_buffer_has_one_empty_row() {
        let buffer = Buffer::new();
        assert!(buffer.is_empty());
        assert_eq!(buffer.len_rows(), 1);
        assert_eq!(buffer.row_content_range(0), 0..0);
        assert_eq!(buffer.byte_to_point(0), Point::new(0, 0));
    }
}
