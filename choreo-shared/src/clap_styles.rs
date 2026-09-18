//! The suite's shared clap [`Styles`].
//!
//! Previously copy-pasted (identically) into every CLI crate's `lib.rs`; this
//! is that single copy. Each crate still *owns* its own `#[command(...)]`
//! declaration — only the styling is shared.

/// Shared clap [`Styles`] for the suite's CLI binaries.
///
/// Uses real ANSI hues (green headers/usage, cyan literals/placeholders) rather
/// than bold/underline only, so help output stays legible even in terminals whose
/// bold text isn't visually distinct (e.g. themes that don't remap the bold color).
/// `Styles::styled()` keeps clap's default error/invalid/valid coloring; the
/// overrides colorize the help elements.
#[must_use]
pub fn clap_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}
