//! Shared support for the suite's CLI binaries.
//!
//! This micro-crate hosts the small, binary-facing helpers that every CLI crate
//! in the workspace would otherwise duplicate:
//!
//! - [`release_name`] — the dance-style release-name metadata compiled into
//!   every binary (`--version`, startup banners) and read by CI for the GitHub
//!   release title.
//! - [`clap_styles`] — the one shared clap [`Styles`](clap::builder::Styles)
//!   used by every CLI (previously copy-pasted into each crate).
//! - [`logging`] — the shared `-v`/`-q` verbosity flags and the log-level
//!   resolution that every binary applies identically.
//!
//! It is deliberately a *leaf* (deps: `clap` and `tracing-subscriber` only) and
//! holds no protocol or transport logic — `choreo-proto` stays the wire
//! protocol.
//!
//! Logging precedence follows the Unix convention: **explicit CLI flags win
//! over the ambient environment** (see [`logging`]).

pub mod clap_styles;
pub mod logging;
pub mod release_name;

pub use clap_styles::clap_styles;
