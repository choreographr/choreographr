//! Thin binary entry point for the `choreographr` daemon.
//!
//! All of the daemon's logic lives in the `choreo-daemon` library crate; this
//! wrapper exists so that `cargo install choreographr` / `cargo binstall
//! choreographr` produce a `choreographr` executable. The real entry point is
//! [`choreo_daemon::main`].

fn main() -> anyhow::Result<()> {
    choreo_daemon::main()
}
