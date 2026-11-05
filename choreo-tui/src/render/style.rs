//! Text styles shared across the render layer's read-only reference pages.
//!
//! The Terminal page, the Help modal and the welcome panel each render a titled
//! block of headings and secondary prose, and had grown byte-identical copies of
//! the two styles below. They live here once so a change to the reference-page
//! palette is a single edit, not three that can drift apart.
//!
//! Only styles that are genuinely identical across their users belong here.
//! `key_style` deliberately does not: the Help modal paints chords yellow while
//! the welcome panel paints them cyan, so each keeps its own.

use ratatui::style::{Color, Modifier, Style};

/// A section heading inside a reference page (and a page or panel title).
pub(super) fn heading_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Secondary prose: explanatory lines, hints and any row that is not a
/// heading, a key chord or a verdict.
pub(super) fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}
