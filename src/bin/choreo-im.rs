//! Thin binary entry point for the `choreo-im` messaging bridge.
//!
//! All of the bridge's logic lives in the `choreo-im` library crate; this
//! wrapper exists so the suite installed by `cargo install choreographr`
//! ships a `choreo-im` executable. The real entry point is
//! [`choreo_im::main`].

fn main() -> anyhow::Result<()> {
    choreo_im::main()
}
