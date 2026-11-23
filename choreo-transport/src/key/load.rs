//! Reading an existing transport secret key — the never-regenerate direction.
//!
//! [`load_transport_keypair`] is the read-only accessor every query uses (the
//! daemon's `pubkey` subcommand), and [`load_secret_key`] is the single decoder
//! both it and the generator's fast path call, so the lock-free read and the
//! re-check under the advisory lock can never drift into accepting different
//! things.

use std::path::Path;

use tracing::debug;
use zeroize::Zeroizing;

use super::encoding::decode_secret;
use super::{TransportSecretKey, keypair_from_secret, transport_sec_path};
use crate::error::TransportError;

/// Read and decode the secret key file at `path`.
///
/// Returns `Ok(None)` when the file is absent — the caller then generates a
/// keypair — and `Ok(Some(secret))` when a key can be decoded from it.
///
/// A file that exists but does not decode is an error, **never** a regenerate:
/// see the module docs on why a lost identity is unrecoverable. The single
/// implementation behind both the lock-free fast path and the re-check under
/// the advisory lock, so the two can never drift into accepting different
/// things.
pub(super) fn load_secret_key(path: &Path) -> Result<Option<TransportSecretKey>, TransportError> {
    // `Zeroizing`: the file's bytes may be the secret itself (the legacy raw
    // form) or its base64 text, and either way the buffer must not be freed
    // un-erased after the key has been decoded out of it.
    let bytes = match std::fs::read(path) {
        Ok(bytes) => Zeroizing::new(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(TransportError::Io(e)),
    };
    if bytes.is_empty() {
        // A zero-length file holds no identity: it is what
        // `open_locked_for_write` creates before the key is written (and what
        // a crash between that create and the write leaves behind). Treating it
        // as "generate" is the only useful answer — there is nothing to
        // preserve, and erroring would wedge the instance permanently.
        return Ok(None);
    }
    let secret = decode_secret(&bytes).map_err(|detail| TransportError::InvalidSecretKey {
        path: path.display().to_string(),
        detail,
    })?;
    // The file holds a real identity, so re-assert its owner-only mode here —
    // the single point every load passes through (the fast path, the re-check
    // under the advisory lock, and the read-only accessor). A build that wrote
    // the secret before this rule existed, or a hand copy under a loose umask,
    // would otherwise stay group- or world-readable forever: nothing else ever
    // opens this file for write, so no later write would tighten it.
    tighten_secret_permissions(path);
    Ok(Some(secret))
}

/// Re-assert `0600` on an existing secret file, warning (never failing) if the
/// filesystem refuses: the key loaded, and refusing here would wedge the
/// instance over a permission bit it can do nothing about.
///
/// Path-based because this is a read path that holds no descriptor for the
/// file; the containing directory is owner-only (`ensure_private_dir` on the
/// generate path), which is what bounds a path-based chmod's swap exposure.
fn tighten_secret_permissions(path: &Path) {
    if let Err(e) = choreo_shared::perm::set_owner_only_path(path) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "could not set the transport secret to mode 0600; the directory's own mode is the \
             remaining control"
        );
    }
}

/// Load an existing transport keypair, deriving its public half from the
/// secret — and **never generating one**.
///
/// The read-only counterpart of [`super::ensure_transport_keypair`], and the
/// accessor every query uses (the daemon's `pubkey` subcommand). It exists so
/// that printing a key cannot create the identity it
/// prints: an absent secret is reported, not minted, because an identity minted
/// by a query would be one no daemon ever loaded — and, worse, would look to the
/// operator like the instance they were asking about.
///
/// No lock is taken: this only reads an existing file, and a concurrent
/// generation either wins (a complete secret is written and fsynced before this
/// read sees it) or the read reports the absence.
///
/// # Errors
///
/// Returns [`TransportError::MissingSecretKey`] when no secret file (or an empty
/// one) is present, [`TransportError::InvalidSecretKey`] when one exists but
/// does not decode, [`TransportError::ConfigDirNotFound`] when no config
/// directory resolves, or [`TransportError::Io`] on a read failure.
pub fn load_transport_keypair() -> Result<(TransportSecretKey, [u8; 32]), TransportError> {
    let sec_path = transport_sec_path()?;
    let Some(secret) = load_secret_key(&sec_path)? else {
        return Err(TransportError::MissingSecretKey {
            path: sec_path.display().to_string(),
        });
    };
    debug!("loaded existing transport secret key (read-only)");
    Ok(keypair_from_secret(secret))
}
