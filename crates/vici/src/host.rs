//! Facts and policy the host supplies.
//!
//! What the core cannot work out for itself, because it is a property of a
//! display the core deliberately does not own. It arrives as a plain parameter —
//! passing one in is not the same as handing the editor a viewport.
//!
//! This module sits below [`crate::motion`] and [`crate::Editor`], so that what
//! the host supplies can be read from either without inverting the layers.

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
