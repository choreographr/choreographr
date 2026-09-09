//! Thin binary entry point for the `choreo-acp` agent-client-protocol bridge.
//!
//! All of the bridge's logic lives in the `choreo-acp` library crate; this
//! wrapper exists so that `cargo install choreo-acp` produces a `choreo-acp`
//! executable. The real entry point is [`choreo_acp::main`], which returns
//! `Result<(), anyhow::Error>` — the same type as `anyhow::Result<()>`.

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

/// Process-wide allocator (musl tarball builds only): mimalloc's per-thread
/// heaps replace musl's weaker default malloc. Enabled via the `mimalloc`
/// feature (set by the release pipeline for the static musl tarball); default
/// builds keep the system allocator. Declared in the binary, not the library
/// crate, so crates.io consumers of the library stay allocator-agnostic.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOC: MiMalloc = MiMalloc;

fn main() -> anyhow::Result<()> {
    choreo_acp::main()
}
