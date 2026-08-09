//! Thin binary entry point for the `choreo-gui` desktop UI.
//!
//! All of the GUI's logic lives in this crate's library; this wrapper exists
//! so `cargo run -p choreo-gui` in the workspace produces a `choreo-gui`
//! executable directly (the GUI is not part of the root `choreographr` suite
//! package, which ships the daemon, TUI, and bridges — and it is not
//! published to crates.io, so there is no `cargo install choreo-gui`).
//! The real entry point is [`choreo_gui::main`], which returns `()` — the
//! wrapper matches that exactly.

fn main() {
    choreo_gui::main()
}
