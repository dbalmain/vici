//! Change geometry: what moved, where, in both byte and (row, column) space.

use core::ops::RangeInclusive;

/// A position in the buffer.
///
/// `col` is a **byte** offset from the start of `row` — not a character index and
/// not a display column. This matches `tree_sitter::Point`, which is the whole
/// point: `-- café` puts the position after `é` at column 8, not 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Point {
    pub row: usize,
    pub col: usize,
}

impl Point {
    #[must_use]
    pub const fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// The geometry of a single change, field-for-field compatible with
/// `tree_sitter::InputEdit`.
///
/// Read it as: *the region `start..old_end` was replaced by the region
/// `start..new_end`.* A pure insertion has `old_end == start`; a pure deletion
/// has `new_end == start`.
///
/// Consumers convert rather than depend — this crate knows nothing about
/// tree-sitter or LSP:
///
/// ```ignore
/// impl From<vici::Edit> for tree_sitter::InputEdit { /* field for field */ }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: Point,
    pub old_end_point: Point,
    pub new_end_point: Point,
}

impl Edit {
    /// The geometry of the change that would put things back.
    ///
    /// The struct is symmetric, so this is just a swap of the two end fields —
    /// no buffer access needed.
    #[must_use]
    pub const fn inverted(&self) -> Self {
        Self {
            start_byte: self.start_byte,
            old_end_byte: self.new_end_byte,
            new_end_byte: self.old_end_byte,
            start_point: self.start_point,
            old_end_point: self.new_end_point,
            new_end_point: self.old_end_point,
        }
    }

    #[must_use]
    pub const fn is_insertion(&self) -> bool {
        self.old_end_byte == self.start_byte
    }

    #[must_use]
    pub const fn is_deletion(&self) -> bool {
        self.new_end_byte == self.start_byte
    }

    /// Rows the change occupied *before* it was applied.
    #[must_use]
    pub fn old_rows(&self) -> RangeInclusive<usize> {
        self.start_point.row..=self.old_end_point.row
    }

    /// Rows the change occupies *after* being applied.
    #[must_use]
    pub fn new_rows(&self) -> RangeInclusive<usize> {
        self.start_point.row..=self.new_end_point.row
    }

    /// True when the change begins and ends on one row, both before and after —
    /// i.e. it neither spans nor alters the row structure.
    ///
    /// This is the test a line-scoped history (vi's `U`) keys off.
    #[must_use]
    pub const fn is_single_row(&self) -> bool {
        self.start_point.row == self.old_end_point.row
            && self.start_point.row == self.new_end_point.row
    }
}

/// A change, complete enough to apply *or* reverse without consulting a buffer.
///
/// This is the unit every history implementation stores. Because it carries the
/// text it displaced, undo is [`Change::inverted`] — histories never need to
/// snapshot the document, and never need to know what a rope is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub edit: Edit,
    /// Text that occupied `edit.start_byte..edit.old_end_byte` beforehand.
    pub removed: String,
    /// Text that occupies `edit.start_byte..edit.new_end_byte` afterwards.
    pub inserted: String,
}

impl Change {
    /// The change that undoes this one.
    #[must_use]
    pub fn inverted(&self) -> Self {
        Self {
            edit: self.edit.inverted(),
            removed: self.inserted.clone(),
            inserted: self.removed.clone(),
        }
    }

    /// True when applying this change would leave the buffer unchanged.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.removed == self.inserted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit() -> Edit {
        Edit {
            start_byte: 21,
            old_end_byte: 26,
            new_end_byte: 29,
            start_point: Point::new(1, 5),
            old_end_point: Point::new(1, 10),
            new_end_point: Point::new(1, 13),
        }
    }

    #[test]
    fn inversion_is_an_involution() {
        assert_eq!(edit().inverted().inverted(), edit());
    }

    #[test]
    fn inverted_swaps_the_ends() {
        let inv = edit().inverted();
        assert_eq!(inv.start_byte, 21);
        assert_eq!(inv.old_end_byte, 29);
        assert_eq!(inv.new_end_byte, 26);
        assert_eq!(inv.old_end_point, Point::new(1, 13));
        assert_eq!(inv.new_end_point, Point::new(1, 10));
    }

    #[test]
    fn change_inversion_swaps_text() {
        let change = Change {
            edit: edit(),
            removed: "users".into(),
            inserted: "accounts".into(),
        };
        let inv = change.inverted();
        assert_eq!(inv.removed, "accounts");
        assert_eq!(inv.inserted, "users");
        assert_eq!(inv.inverted(), change);
    }

    #[test]
    fn row_spans() {
        let dd = Edit {
            start_byte: 16,
            old_end_byte: 27,
            new_end_byte: 16,
            start_point: Point::new(1, 0),
            old_end_point: Point::new(2, 0),
            new_end_point: Point::new(1, 0),
        };
        assert!(!dd.is_single_row());
        assert!(dd.is_deletion());
        assert_eq!(dd.old_rows(), 1..=2);
        assert_eq!(dd.new_rows(), 1..=1);
    }
}
