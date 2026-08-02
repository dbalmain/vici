//! Facts and policy the host supplies.
//!
//! Two things the core cannot work out for itself, because both are properties
//! of a display it deliberately does not own. They arrive as plain parameters —
//! passing one in is not the same as handing the editor a viewport.
//!
//! This module sits below [`crate::motion`] and [`crate::Editor`] so that a
//! screen-relative motion can read a [`Viewport`] without the motion layer
//! depending on the editor above it.

/// Indentation policy supplied by the host.
///
/// This is the one display-width parameter the core needs: removing a shift from
/// a tab-indented row requires knowing how far a tab advances. The default
/// deliberately differs from vim's `sw=8 ts=8 noexpandtab` — a library default
/// should suit the hosts that embed it rather than preserve history's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Indent {
    /// Columns per shift. vim's `shiftwidth`.
    pub shift_width: usize,
    /// Columns a tab advances to. vim's `tabstop`.
    ///
    /// A host that expands tabs on screen must report the same width it renders
    /// with, or `<<` removes something other than what the user can see.
    ///
    /// Zero is treated as one column, so a host-provided value cannot make an
    /// indentation operation divide by zero.
    pub tab_width: usize,
    /// Render the new indent with tabs where possible. Inverse of vim's
    /// `expandtab`.
    pub use_tabs: bool,
}

impl Default for Indent {
    fn default() -> Self {
        Self {
            shift_width: 4,
            tab_width: 8,
            use_tabs: false,
        }
    }
}

/// Viewport facts supplied by the host.
///
/// The host still decides where its window sits; the core only needs `height` to
/// decide where a page move lands, and `top_row` to answer what is at the top of
/// the screen. A host must call [`crate::Editor::set_viewport`] whenever it
/// renders or resizes. Otherwise page moves use stale dimensions and `H`/`M`/`L`
/// answer for the wrong screen — or bell, while `height` remains zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Viewport {
    /// First buffer row currently displayed.
    pub top_row: usize,
    /// Rows the window can show. Zero means no host has reported a viewport.
    pub height: usize,
}
