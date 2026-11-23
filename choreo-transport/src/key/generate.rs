//! Minting and publishing a new transport secret key.
//!
//! [`ensure_transport_keypair`] is the daemon's start-up behaviour and nothing
//! else's: it loads an existing identity when there is one and otherwise
//! generates one, serializing concurrent starts behind an advisory file lock so
//! two instances converge on ONE identity rather than each enrolling a key the
//! other will never present.

use tracing::{debug, info};

use super::encoding::encode_secret;
use super::load::load_secret_key;
use super::{TransportSecretKey, keypair_from_secret, transport_sec_path};
use crate::error::TransportError;

/// Generate a new X25519 keypair for the Noise transport.
///
/// The secret is returned already wrapped, so no bare `[u8; 32]` copy of it
/// exists outside the wiping type: `StaticSecret` itself zeroizes on drop, and
/// its byte form is moved straight into the wrapper.
fn generate_keypair() -> (TransportSecretKey, [u8; 32]) {
    let secret = x25519_dalek::StaticSecret::random_from_rng(&mut rand::rng());
    let public = x25519_dalek::PublicKey::from(&secret);
    (
        TransportSecretKey::from(secret.to_bytes()),
        public.to_bytes(),
    )
}

/// Ensure that a Noise transport keypair exists at the standard path
/// (~/.config/choreographr/transport.sec).
///
/// If the secret file exists and decodes (base64, or a legacy raw 32-byte key),
/// loads it and returns it paired with the public key **derived** from it.
/// Otherwise generates a new X25519 keypair and writes the secret as base64
/// text — the only file it writes: there is no public-key file to write, since
/// the public half is derived from the secret on every load.
///
/// Generating is the daemon's startup behaviour and nothing else's; a query that
/// must not mint an identity uses [`super::load_transport_keypair`] instead.
///
/// A secret file that exists but does not decode is reported as
/// [`TransportError::InvalidSecretKey`] and left untouched; it is never
/// overwritten with a freshly generated key.
///
/// Returns (`secret_key`, `public_key`).
///
/// # Errors
///
/// Returns [`TransportError`] on config-directory resolution, file I/O,
/// lock/unlock failures, or an unparseable secret key file.
pub fn ensure_transport_keypair() -> Result<(TransportSecretKey, [u8; 32]), TransportError> {
    let sec_path = transport_sec_path()?;

    // Fast path: a loadable secret means the keypair already exists, so this
    // takes no lock at all on the common start-up path.
    if let Some(sk) = load_secret_key(&sec_path)? {
        debug!("loaded existing transport secret key");
        return Ok(keypair_from_secret(sk));
    }

    let (secret, public) = generate_keypair();
    if let Some(parent) = sec_path.parent() {
        // Narrow the directory BEFORE anything is written into it: the secret
        // is the instance's identity, and `ensure_private_dir` both creates it
        // owner-only (0700) and refuses one owned by another user — never write
        // a secret into a directory somebody else controls.
        choreo_shared::perm::ensure_private_dir(parent)?;
    }

    // Advisory exclusive lock (std::fs::File::lock, stable since 1.89) so
    // concurrently-starting processes serialize keypair generation; on Unix
    // new secret files are created owner-only (0o600). The lock is held on the
    // target path for the whole generate-and-publish step.
    let sec_file = open_locked_for_write(&sec_path)?;

    // Re-check under the lock: a concurrent process may have written a valid
    // key while we waited, in which case its key wins and ours is discarded —
    // two instances starting together must converge on ONE identity, or the
    // second one's ACL entry would never match the key it presents. The
    // re-check reads the PATH (not the locked file handle), which is what makes
    // the lock sufficient even though publishing renames a new inode over it.
    let write_result = (|| -> Result<(TransportSecretKey, [u8; 32]), TransportError> {
        if let Some(sk) = load_secret_key(&sec_path)? {
            debug!("loaded existing transport secret key (written concurrently)");
            return Ok(keypair_from_secret(sk));
        }
        // Publish the secret atomically (sibling temp file, fsync, rename): a
        // crash mid-write leaves either no secret (which regenerates) or a
        // complete one, never a truncated file whose bytes are not a key.
        // The secret is the whole on-disk identity: the public half is derived
        // from it on every load, so no second file is written to disagree with
        // it (and no crash can leave one half behind the other).
        //
        // `encode_secret` returns a wiping buffer: base64 of a secret is the
        // secret, so the text must not outlive the write un-erased.
        let encoded = encode_secret(secret.as_bytes());
        choreo_shared::atomic_write::write_private_atomic(&sec_path, encoded.as_bytes())?;
        info!(
            "generated and wrote new Noise transport keypair to {}",
            sec_path.display()
        );
        Ok((secret, public))
    })();

    let unlock_result = sec_file.unlock();
    let pair = write_result?;
    unlock_result?;
    Ok(pair)
}

/// Open `path` for writing (creating it if needed) and take an advisory
/// exclusive lock. On Unix, new files get owner-only 0o600 permissions
/// because they contain a secret key.
fn open_locked_for_write(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        file.lock()?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        file.lock()?;
        Ok(file)
    }
}
