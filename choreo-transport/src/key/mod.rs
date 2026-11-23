//! On-disk Noise transport keypair management.
//!
//! Provides the standard keypair path under `{config}/choreographr`, the
//! [`ensure_transport_keypair`] loader/generator (race-safe via an advisory
//! file lock), and its read-only counterpart [`load_transport_keypair`]. The
//! keypair is one identity per instance and serves both handshake modes — the
//! daemon presents it as the responder and the client as the initiator. The
//! canonical base64 encoding of a 32-byte key ([`pubkey_base64`] /
//! [`parse_pubkey_base64`]) lives in the `encoding` submodule and is the ONE
//! textual form of a transport key.
//!
//! This module owns the identity type, the path resolution and the test seam;
//! `load` owns reading an existing secret and `generate` owns minting and
//! publishing a new one. The split follows the two directions' different
//! failure contracts: a load may never regenerate (see below), while a generate
//! must never publish a partial file.
//!
//! # In memory
//!
//! The decoded secret lives only inside [`TransportSecretKey`], whose bytes are
//! a [`zeroize::Zeroizing`] array, and every intermediate on the way there or
//! back is a `Zeroizing` buffer too: the file's bytes, the base64 text of the
//! secret, and the decoder's output are all wiped on drop. A key that is only
//! erased in its final resting place is still readable from the freed buffers it
//! passed through, so the wipe covers the whole path rather than the last copy.
//!
//! # On-disk secret key format
//!
//! The secret key is stored as **standard-alphabet base64**: exactly 44
//! characters followed by one `\n`. The file is therefore text, which keeps it
//! legible, diffable, and pasteable — a binary blob invites copy/paste damage
//! that looks like a key but is not one.
//!
//! Reading accepts **either** form: a file of exactly 32 bytes is a legacy raw
//! key, anything else is decoded as base64 (surrounding whitespace trimmed, so
//! the normal trailing newline is fine) and must yield exactly 32 bytes. The
//! two lengths cannot be confused — 32 bytes of raw key is never valid base64
//! text of key length, and the base64 form is 44 characters — so the dual read
//! is unambiguous.
//!
//! A secret file that exists with content that decodes to neither form is a
//! **hard error** ([`TransportError::InvalidSecretKey`]), never a regeneration.
//! The secret is this instance's long-lived identity: silently minting a new
//! one on a parse failure would orphan every ACL entry and every pinned server
//! key, turning a recoverable typo into a fleet-wide re-enrollment. Only an
//! absent (or zero-length) file means "generate".
//!
//! Generation publishes the secret ATOMICALLY — a sibling temp file, fsync,
//! rename, via [`choreo_shared::atomic_write::write_private_atomic`] — under the
//! advisory lock that serializes concurrent starts. A crash therefore leaves
//! either no secret (which regenerates) or a complete one: it can never leave a
//! truncated file, whose bytes would be rejected as `InvalidSecretKey` and would
//! wedge the instance until an operator removed it by hand.
//!
//! # Permissions
//!
//! The identity is the most sensitive file this instance owns, so its
//! reachability is narrowed on both ends. Generating it first narrows the
//! containing directory to owner-only (`0700`) through
//! [`choreo_shared::perm::ensure_private_dir`] — the same helper the MCP trust
//! store uses, which also refuses a directory owned by another user rather than
//! writing a secret into it. Every load then re-asserts the file's own `0600`
//! mode ([`choreo_shared::perm::set_owner_only_path`]), because the file may
//! predate that rule: a secret written by an older build, or copied in by hand
//! under a loose umask, is group- or world-readable until something tightens it,
//! and nothing else ever opens it for write. A chmod that fails only warns — the
//! key still loaded, so refusing would wedge the instance over a permission bit.
//!
//! # Public key derivation
//!
//! The public half is always **derived** from the secret
//! (`PublicKey::from(&StaticSecret::from(sk))`), never read back from disk, and
//! never persisted: only `transport.sec` is written. A stored copy could be
//! stale or truncated — a crash between the secret's fsync and the public write
//! leaves exactly that state — and reading it back would silently load the
//! mismatch, surfacing later as an inexplicable handshake failure where the peer
//! sees a key the operator's own file does not show. With no second copy there
//! is nothing to go stale, and no key file an operator could edit into a
//! disagreement. A public key is exported as standard base64 through a query
//! instead ([`load_transport_keypair`] plus [`pubkey_base64`], the daemon's
//! `pubkey` subcommand).

mod encoding;
mod generate;
mod load;

use std::path::PathBuf;

use zeroize::Zeroizing;

use crate::error::TransportError;

pub use encoding::{parse_pubkey_base64, pubkey_base64};
pub use generate::ensure_transport_keypair;
pub use load::load_transport_keypair;

thread_local! {
    /// Test-only override for the keypair directory. When set, `keypair_dir()`
    /// returns `<root>/choreographr` instead of the user's real config dir.
    static TEST_CONFIG_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only override for the keypair directory (see `TEST_CONFIG_ROOT`).
///
/// Public so integration tests (which compile the crate without #[cfg(test)])
/// can redirect keypair generation away from the user's real config directory.
///
/// Hidden from the public API docs on purpose: this is a test seam, not part
/// of the transport's contract. It is thread-local and resets on drop, so a
/// production caller could only ever redirect key generation in its own
/// thread — but it should not be treated as a supported API.
///
/// Thread-local, not process-global, because the callers that need it most are
/// the per-connection threads in the daemon's integration tests: each installs
/// the override for the keypair its OWN `ensure_transport_keypair` call loads
/// and clears it when done. A process-global would make one thread's clear
/// visible to a sibling thread mid-connection, which is exactly the leak
/// (a keypair written into the real config dir) this seam exists to prevent.
#[doc(hidden)]
pub fn set_test_config_root(root: Option<PathBuf>) {
    TEST_CONFIG_ROOT.with(|cell| cell.replace(root));
}

/// A Noise transport secret key (32-byte X25519 secret).
///
/// Provides type safety over a raw `[u8; 32]` so the key cannot be
/// confused with other 32-byte values, and **zeroizes its bytes on drop**: it
/// is the instance's long-lived identity, so a stray copy left in freed heap or
/// stack memory is a copy an attacker who can read this process's memory later
/// can lift (or that a core dump preserves).
///
/// The bytes live in a [`Zeroizing`] array rather than a plain one, so the wipe
/// is the wrapper's drop rather than a hand-written one that a refactor could
/// silently lose. The wrappers on the load and generate paths extend the same
/// guarantee to the buffers the key passes through (see the module docs).
///
/// Deliberately **not `Copy`**. A `Copy` struct is duplicated byte-for-byte by
/// implicit assignment, and those duplicates would escape the wipe entirely —
/// they would be zeroized nowhere, which defeats the point. A caller that
/// genuinely needs a second owner makes an explicit `clone` whose lifetime is
/// visible.
#[derive(Clone)]
pub struct TransportSecretKey(Zeroizing<[u8; 32]>);

impl TransportSecretKey {
    /// Expose the inner bytes for use in handshake APIs.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for TransportSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TransportSecretKey").field(&"[***]").finish()
    }
}

impl From<[u8; 32]> for TransportSecretKey {
    /// Take ownership of the bytes as-is: the array is MOVED into the
    /// `Zeroizing` wrapper, so accepting it here leaves no second copy of the
    /// secret on the caller's stack.
    fn from(bytes: [u8; 32]) -> Self {
        TransportSecretKey(Zeroizing::new(bytes))
    }
}

impl AsRef<[u8; 32]> for TransportSecretKey {
    fn as_ref(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Pair a secret with the public key **derived** from it.
///
/// This is the one place a loaded keypair's public half comes from, which is
/// what makes "the secret and the public half disagree" unrepresentable: there
/// is no second source to disagree with. The secret is taken by value so the
/// only copy that outlives this call is the one inside the returned
/// [`TransportSecretKey`].
fn keypair_from_secret(sk: TransportSecretKey) -> (TransportSecretKey, [u8; 32]) {
    // The deref below is the one transient copy of the secret this API forces:
    // `StaticSecret::from` takes a `[u8; 32]` by value and offers no slice
    // constructor. It is still wiped — the copied array is moved into the
    // `StaticSecret`, whose `Drop` zeroizes it, and `zeroize` is an explicit
    // feature of this workspace's `x25519-dalek` dependency, so a future
    // `default-features = false` cannot silently drop that wipe.
    let public =
        x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*sk.as_bytes())).to_bytes();
    (sk, public)
}

/// Directory that holds the transport keypair files (`<config>/choreographr`).
///
/// Resolved through the shared [`choreo_shared::paths::config_dir`], so the
/// `--base-dir` / `CHOREOGRAPHR_BASE_DIR` override relocates the keypair with
/// the rest of the instance. Under test, `set_test_config_root` redirects this
/// to a temp dir (and wins over the shared resolver). This env override is the
/// portable seam: `XDG_CONFIG_HOME` cannot be used for it because
/// `dirs::config_dir()` honors it only on Linux — on macOS it always returns
/// `$HOME/Library/Application Support`, so a test relying on it would write the
/// keypair into the user's real config directory.
fn keypair_dir() -> Result<PathBuf, TransportError> {
    if let Some(root) = TEST_CONFIG_ROOT.with(|cell| cell.borrow().clone()) {
        return Ok(root.join("choreographr"));
    }
    choreo_shared::paths::config_dir().map_err(|_| TransportError::ConfigDirNotFound)
}

/// Path to the transport static secret key (~/.config/choreographr/transport.sec)
///
/// The file holds the base64 secret (see the module docs for the format and its
/// legacy raw-byte acceptance).
///
/// # Errors
///
/// Returns [`TransportError::ConfigDirNotFound`] when no config directory
/// exists.
pub fn transport_sec_path() -> Result<PathBuf, TransportError> {
    Ok(keypair_dir()?.join("transport.sec"))
}

#[cfg(test)]
mod tests;
