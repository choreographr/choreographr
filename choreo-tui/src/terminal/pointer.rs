//! OSC 22 mouse-pointer shape.
//!
//! The TUI tells the terminal what the pointer over a hoverable target means by
//! setting the mouse cursor's shape: `ESC ] 22 ; <css-cursor-name> ST`. CSS
//! cursor names are what Ghostty, kitty and foot accept, so one sequence names
//! the shape on every terminal that implements OSC 22; a terminal that does not
//! ignores the sequence outright and keeps its own pointer.
//!
//! This is **output only**: it is a set command, never a query. A query would
//! make the terminal write a reply onto the input stream, and crossterm would
//! parse that reply body as individual keystrokes and type them into the user's
//! prompt.
//!
//! The shape communicates affordance — [`PointerShape::Pointer`] over a control
//! the user can click, [`PointerShape::Text`] over text they can select, and
//! [`PointerShape::Default`] everywhere else (and as the reset written on
//! suspend and exit, so the TUI never leaves a stale hand behind).

use super::{osc, write};

/// The pointer shapes the TUI emits, named by their CSS cursor keyword.
///
/// Closed on purpose: an unknown shape name is ignored by other terminals, but
/// keeping the vocabulary to the three affordances the UI actually has means a
/// typo or a speculative fourth shape cannot reach the terminal at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerShape {
    /// The terminal's ordinary arrow — nothing under the pointer is
    /// interactive.
    Default,
    /// An I-beam: the pointer is over selectable text.
    Text,
    /// A pointing hand: the pointer is over something clickable.
    Pointer,
}

impl PointerShape {
    /// The CSS cursor keyword this shape is emitted as.
    pub(crate) fn css_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Text => "text",
            Self::Pointer => "pointer",
        }
    }
}

/// Build the OSC 22 sequence that sets the pointer shape.
pub(crate) fn build(shape: PointerShape) -> String {
    osc(22, shape.css_name())
}

/// Emit the OSC 22 sequence that sets the pointer shape.
pub(crate) fn set(shape: PointerShape) {
    write(&build(shape));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_frames_the_css_cursor_name() {
        assert_eq!(build(PointerShape::Default), "\x1b]22;default\x1b\\");
        assert_eq!(build(PointerShape::Text), "\x1b]22;text\x1b\\");
        assert_eq!(build(PointerShape::Pointer), "\x1b]22;pointer\x1b\\");
    }

    #[test]
    fn default_shape_is_the_reset_sequence() {
        // OSC 22 has no separate reset command: the `default` keyword IS the
        // reset, and it is exactly what the suspend and exit paths write to
        // hand the pointer back to the terminal.
        assert_eq!(build(PointerShape::Default), osc(22, "default"));
    }

    #[test]
    fn css_names_are_the_three_supported_keywords() {
        assert_eq!(PointerShape::Default.css_name(), "default");
        assert_eq!(PointerShape::Text.css_name(), "text");
        assert_eq!(PointerShape::Pointer.css_name(), "pointer");
    }
}
