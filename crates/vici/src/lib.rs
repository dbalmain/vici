//! Headless editing primitives for a vi-like core.
//!
//! No rendering, no terminal, no filesystem, no async. Layers, each ignorant of
//! the ones above it:
//!
//! | Layer | Knows about | Does not know about |
//! |---|---|---|
//! | [`Buffer`] | bytes, rows, ropes | modes, keys, undo |
//! | [`LinearHistory`] | [`Change`]s | ropes, keys, rendering |
//! | [`Document`] | both of the above | modes, keys, rendering |
//! | [`Keymap`] | key sequences → [`Binding`]s | buffers, cursors |
//! | [`Pending`] | vi's command grammar | buffers, cursors |
//!
//! # Coordinates
//!
//! Everything in the buffer layer is a **byte** offset, and every [`Point`] column
//! is a byte offset within its row. That is not a convenience — it is what makes
//! an [`Edit`] convertible field-for-field into `tree_sitter::InputEdit`.
//!
//! Character space (grapheme-correct `l`, `x`, `~`) belongs to the motion layer;
//! display-width space (CJK, tabs) belongs to the view layer. Holding this crate
//! to one coordinate system is what keeps those two from leaking into each other.
//!
//! Rows are counted by LF only, matching the parser. Line endings are preserved
//! byte-for-byte; a `\r` is ordinary content at the end of a row.
//!
//! # Consumers convert, this crate does not depend
//!
//! There is no tree-sitter or LSP dependency here. Hosts write the conversion:
//!
//! ```ignore
//! impl From<vici::Edit> for tree_sitter::InputEdit {
//!     fn from(e: vici::Edit) -> Self { /* field for field */ }
//! }
//! ```
//!
//! LSP's `TextDocumentContentChangeEvent` is a second such impl — note it defaults
//! to UTF-16 columns, negotiable via the `positionEncoding` capability.

mod buffer;
mod command;
mod document;
mod edit;
mod editor;
pub mod fixtures;
mod history;
mod host;
mod key;
mod keymap;
mod motion;
mod pending;

pub use buffer::Buffer;
pub use command::{
    AwaitChar, Command, InsertAt, Mode, Motion, ObjectScope, Operator, Scroll, Target, TextObject,
    VisualKind,
};
pub use document::Document;
pub use edit::{Change, Edit, Point};
pub use editor::{Editor, Effect, Register};
pub use history::{LinearHistory, Step};
pub use host::{Indent, Viewport};
pub use key::{Key, KeyCode, Mods, ParseError, key, keys, render};
pub use keymap::{Binding, Keymap, Layer, Walk, is_count_digit};
pub use motion::{
    Bound, Find, Span, clamp, delimiters, grapheme_col, object_span, resolve as resolve_motion,
    row_span,
};
pub use pending::{Pending, Resolution};

pub use ropey;
