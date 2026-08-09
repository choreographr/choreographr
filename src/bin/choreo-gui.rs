//! Thin binary entry point for the `choreo-gui` desktop UI.
//!
//! All of the GUI's logic lives in the `choreo-gui` library crate; this
//! wrapper exists so the suite built with `--features gui` ships a
//! `choreo-gui` executable. The real entry point is [`choreo_gui::main`],
//! which returns `()` — the wrapper matches that exactly.

fn main() {
    choreo_gui::main()
}
