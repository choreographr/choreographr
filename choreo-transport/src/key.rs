use std::path::PathBuf;
use tracing::{debug, info};

use crate::error::TransportError;

/// A Noise IK transport secret key (32-byte X25519 secret).
///
/// Provides type safety over a raw `[u8; 32]` so the key cannot be
/// confused with other 32-byte values.
#[derive(Clone, Copy)]
pub struct TransportSecretKey(pub(crate) [u8; 32]);

impl TransportSecretKey {
    /// Wrap a raw 32-byte secret key.
    pub fn new(bytes: [u8; 32]) -> Self {
        TransportSecretKey(bytes)
    }

    /// Expose the inner bytes for use in handshake APIs.
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
    fn from(bytes: [u8; 32]) -> Self {
        TransportSecretKey(bytes)
    }
}

impl AsRef<[u8; 32]> for TransportSecretKey {
    fn as_ref(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Generate a new X25519 keypair for Noise IK transport.
fn generate_keypair() -> ([u8; 32], [u8; 32]) {
    let secret = x25519_dalek::StaticSecret::random_from_rng(&mut rand::rng());
    let public = x25519_dalek::PublicKey::from(&secret);
    (secret.to_bytes(), public.to_bytes())
}

/// Path to the Noise IK static secret key (~/.config/choreographr/transport.sec)
pub fn transport_sec_path() -> Result<PathBuf, TransportError> {
    let config = dirs::config_dir().ok_or(TransportError::ConfigDirNotFound)?;
    Ok(config.join("choreographr").join("transport.sec"))
}

/// Path to the Noise IK static public key (~/.config/choreographr/transport.pub)
pub fn transport_pub_path() -> Result<PathBuf, TransportError> {
    let config = dirs::config_dir().ok_or(TransportError::ConfigDirNotFound)?;
    Ok(config.join("choreographr").join("transport.pub"))
}

/// Resolve a server public key value from an optional file path.
///
/// If `path` is `Some`, reads 32 bytes from that file.
/// If `path` is `None`, defaults to [`transport_pub_path()`].
///
/// Returns the 32-byte key on success, or a `TransportError` on I/O
/// failure, invalid length, or missing config directory.
pub fn read_server_pk(path: Option<&str>) -> Result<[u8; 32], TransportError> {
    let pk_path = path
        .map(PathBuf::from)
        .unwrap_or_else(|| transport_pub_path().unwrap_or_else(|_| PathBuf::from("transport.pub")));
    let bytes = std::fs::read(&pk_path)?;
    if bytes.len() != 32 {
        return Err(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server public key must be 32 bytes",
        )));
    }
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&bytes);
    Ok(pk)
}

/// Ensure that a Noise IK transport keypair exists at the standard paths
/// (~/.config/choreographr/transport.sec and ~/.config/choreographr/transport.pub).
///
/// If both files exist and are valid (32 bytes each), loads and returns them.
/// Otherwise generates a new X25519 keypair and writes both files.
///
/// Returns (secret_key, public_key).
pub fn ensure_transport_keypair() -> Result<(TransportSecretKey, [u8; 32]), TransportError> {
    let sec_path = transport_sec_path()?;
    let pub_path = transport_pub_path()?;

    if sec_path.exists() && pub_path.exists() {
        let secret = std::fs::read(&sec_path)?;
        let public = std::fs::read(&pub_path)?;
        if secret.len() == 32 && public.len() == 32 {
            let mut sk = [0u8; 32];
            let mut pk = [0u8; 32];
            sk.copy_from_slice(&secret);
            pk.copy_from_slice(&public);
            debug!("loaded existing transport keypair");
            return Ok((TransportSecretKey(sk), pk));
        }
    }

    let (secret, public) = generate_keypair();
    if let Some(parent) = sec_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&sec_path, secret)?;
    std::fs::write(&pub_path, public)?;
    info!(
        "generated and wrote new Noise IK transport keypair to {}",
        sec_path.display()
    );
    Ok((TransportSecretKey(secret), public))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_transport_keypair_generates_and_loads() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: we immediately restore the old value and run single-threaded.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path());
        }

        let result = ensure_transport_keypair();

        match prev {
            Some(val) => unsafe { std::env::set_var("XDG_CONFIG_HOME", val) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }

        let (sk, pk) = result.expect("ensure_transport_keypair should succeed in temp dir");
        assert_eq!(sk.as_bytes().len(), 32);
        assert_eq!(pk.len(), 32);

        assert!(
            temp.path()
                .join("choreographr")
                .join("transport.sec")
                .exists()
        );
        assert!(
            temp.path()
                .join("choreographr")
                .join("transport.pub")
                .exists()
        );
    }
}
