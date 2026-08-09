//! Thin binary entry point for the `choreo-tui` terminal UI.
//!
//! All of the TUI's logic lives in the `choreo-tui` library crate; this
//! wrapper exists so the suite installed by `cargo install choreographr`
//! ships a `choreo-tui` executable. The real entry point is
//! [`choreo_tui::main`].

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
    choreo_tui::main()
}
