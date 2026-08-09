//! Thin binary entry point for the `choreo-acp` agent-client-protocol bridge.
//!
//! All of the bridge's logic lives in the `choreo-acp` library crate; this
//! wrapper exists so the suite installed by `cargo install choreographr`
//! ships a `choreo-acp` executable. The real entry point is
//! [`choreo_acp::main`], which returns `Result<(), anyhow::Error>` — the same
//! type as `anyhow::Result<()>`.

fn main() -> anyhow::Result<()> {
    choreo_acp::main()
}
