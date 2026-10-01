//! Noise-based authenticated, encrypted transport for the daemon↔client TCP
//! protocol.
//!
//! Wraps a `TcpStream` in a Noise IK (or first-contact XX) handshake and an
//! AES-256-GCM stream that length-prefixes and fragments every message. The
//! [`error`] module defines the shared error type, [`handshake`] the handshake
//! state machines and their absolute-deadline budgeting, [`key`] the on-disk
//! transport keypair and its fingerprint, and [`noise`] the encrypted stream
//! itself.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod error;
pub mod handshake;
pub mod key;
pub mod noise;
