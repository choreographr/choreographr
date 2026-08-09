//! Thin binary entry point for the `choreo-gui` desktop UI.
//!
//! All of the GUI's logic lives in this crate's library; this wrapper exists
//! so `cargo install choreo-gui` / `cargo run -p choreo-gui` produce a
//! `choreo-gui` executable directly (the GUI is not part of the root
//! `choreographr` suite package, which ships the daemon, TUI, and bridges).
//! The real entry point is [`choreo_gui::main`], which returns `()` — the
//! wrapper matches that exactly.

fn main() {
    choreo_gui::main()
}
