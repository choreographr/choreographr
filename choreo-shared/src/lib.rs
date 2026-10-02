//! Shared support for the suite's CLI binaries.
//!
//! This micro-crate hosts the small, binary-facing helpers that every CLI crate
//! in the workspace would otherwise duplicate:
//!
//! - [`release_name`] — the dance-style release-name metadata compiled into
//!   every binary (`--version`, startup banners) and read by CI for the GitHub
//!   release title.
//! - [`clap_styles()`] — the one shared clap [`Styles`](clap::builder::Styles)
//!   used by every CLI (previously copy-pasted into each crate).
//! - [`logging`] — the shared `-v`/`-q` verbosity flags, the log-level
//!   resolution every binary applies identically, and the one hardened
//!   pid-keyed log-file opener the file-only binaries share.
//! - [`paths`] — the one filesystem-layout resolver: the XDG config/data/
//!   runtime/state locations and the `--base-dir` override that relocates an
//!   entire instance under a single root.
//!
//! It is deliberately a *leaf* (deps: `clap`, `dirs`, `tracing`, and
//! `tracing-subscriber` only) and holds no protocol or transport logic —
//! `choreo-proto` stays the wire protocol.
//!
//! Logging precedence follows the Unix convention: **explicit CLI flags win
//! over the ambient environment** (see [`logging`]).

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod clap_styles;
pub mod logging;
pub mod paths;
pub mod release_name;

pub use clap_styles::clap_styles;
