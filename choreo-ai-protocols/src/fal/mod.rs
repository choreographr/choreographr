//! Shared fal.ai wire-contract helpers.
//!
//! fal's HTTP error bodies come in **two shapes** that are keyed by their JSON
//! structure and switched on the machine-readable `type`/`error_type` value —
//! never the free-text `msg`. Both the synchronous image adapter
//! ([`crate::images::FalImageClient`]) and the queue-based video adapter
//! ([`crate::videos::FalVideoClient`]) speak the same fal platform, so the
//! parser lives here once instead of being duplicated (and left to drift)
//! between them.

pub(crate) mod error;
