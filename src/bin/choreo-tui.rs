//! Thin binary entry point for the `choreo-tui` terminal UI.
//!
//! All of the TUI's logic lives in the `choreo-tui` library crate; this
//! wrapper exists so the suite installed by `cargo install choreographr`
//! ships a `choreo-tui` executable. The real entry point is
//! [`choreo_tui::main`].

fn main() -> anyhow::Result<()> {
    choreo_tui::main()
}
