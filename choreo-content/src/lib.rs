//! Choreographr Coordination Platform tools.
//!
//! This crate implements the read/write tooling for the Choreographr
//! Coordination Platform: a Substrate runtime (`choreo-runtime`) running
//! `pallet-content` and its companion pallets, with content payloads stored on
//! a local IPFS node and indexed by `acuity-index`.
//!
//! ## Threading model
//!
//! The daemon stays thread-only. This crate owns a [tokio sidecar runtime]
//! (`runtime`) used **only** to drive `subxt` (node RPC + tx submission). IPFS
//! (`ipfs`, via `ureq`) and the indexer (`indexer`, via `tungstenite` in
//! synchronous mode) make no use of the sidecar, so only signed chain writes
//! and on-chain state reads go through it.
//!
//! ## Public surface
//!
//! All four modules expose [`ContentError`]-returning, blocking `execute_*`
//! functions. The daemon registers thin `Tool` wrappers over them under the
//! `coord` tool group.
//!
//! [tokio sidecar runtime]: runtime

// `acuity_runtime` is codegen output (subxt's runtime types, ~10k lines): its
// many `#[allow(...)]`s (rustc lints + `clippy::all`) cannot be `#[expect]`ed —
// which lints fire varies by feature/consumer — and the file must not be
// hand-edited (regenerated on every subxt upgrade). Exempt the whole module
// from `clippy::allow_attributes` here, in the non-generated parent.
#[allow(clippy::allow_attributes)]
pub mod acuity_runtime;
pub mod chain;
pub mod config;
pub mod encode;
pub mod error;
pub mod image;
pub mod indexer;
pub mod ipfs;
pub mod orchestrate;
pub mod runtime;

pub use error::ContentError;

/// Convenience: every module re-exports its central public types under a
/// single `choreo_content::` path for the daemon's thin tool wrappers.
pub mod prelude {
    pub use crate::config::*;
    pub use crate::encode::{
        AccountType, ContentInput, ContentType, DecodedItem, ImageSpec, MipmapLevel, ProfileSpec,
        derive_item_id, short_hex,
    };
    pub use crate::error::ContentError;
}

/// Uniffi-style init hook: build the sidecar runtime. The daemon calls this
/// from `main()`. Idempotent.
///
/// # Errors
///
/// Fails with an opaque boxed error if the OS refuses to spawn the tokio
/// worker threads while building the sidecar runtime (see
/// [`runtime::init`]); subsequent calls after a successful init never fail.
pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    runtime::init()
}
