//! Thin binary entry point for the `choreo-im` messaging bridge.
//!
//! All of the bridge's logic lives in the `choreo-im` library crate; this
//! wrapper exists so the suite installed by `cargo install choreographr`
//! ships a `choreo-im` executable. The real entry point is
//! [`choreo_im::main`].

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

/// Process-wide allocator (musl tarball builds only): mimalloc's per-thread
/// heaps replace musl's weaker default malloc. Enabled via the `mimalloc`
/// feature (set by scripts/release.sh for the static musl tarball); default
/// builds keep the system allocator. Declared in the root binaries, not the
/// library crates, so crates.io consumers of the libraries stay
/// allocator-agnostic.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOC: MiMalloc = MiMalloc;

fn main() -> anyhow::Result<()> {
    choreo_im::main()
}
