use std::path::PathBuf;
use tracing::{debug, info};

use crate::error::TransportError;

#[cfg(test)]
thread_local! {
    /// Test-only override for the keypair directory. When set, `keypair_dir()`
    /// returns `<root>/choreographr` instead of the user's real config dir.
    static TEST_CONFIG_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only override for the keypair directory (see `TEST_CONFIG_ROOT`).
#[cfg(test)]
pub(crate) fn set_test_config_root(root: Option<PathBuf>) {
    TEST_CONFIG_ROOT.with(|cell| cell.replace(root));
}

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

/// Directory that holds the transport keypair files (`<config>/choreographr`).
///
/// Under test, `set_test_config_root` redirects this to a temp dir. This
/// cannot be done via `XDG_CONFIG_HOME`: `dirs::config_dir()` honors it only
/// on Linux — on macOS it always returns `$HOME/Library/Application Support`,
/// so a test relying on it would write the keypair into the user's real
/// config directory.
fn keypair_dir() -> Result<PathBuf, TransportError> {
    #[cfg(test)]
    if let Some(root) = TEST_CONFIG_ROOT.with(|cell| cell.borrow().clone()) {
        return Ok(root.join("choreographr"));
    }
    let config = dirs::config_dir().ok_or(TransportError::ConfigDirNotFound)?;
    Ok(config.join("choreographr"))
}

/// Path to the Noise IK static secret key (~/.config/choreographr/transport.sec)
pub fn transport_sec_path() -> Result<PathBuf, TransportError> {
    Ok(keypair_dir()?.join("transport.sec"))
}

/// Path to the Noise IK static public key (~/.config/choreographr/transport.pub)
pub fn transport_pub_path() -> Result<PathBuf, TransportError> {
    Ok(keypair_dir()?.join("transport.pub"))
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

    // Advisory exclusive lock (std::fs::File::lock, stable since 1.89) so
    // concurrently-starting processes serialize keypair generation; on Unix
    // new secret files are created owner-only (0o600).
    let mut sec_file = open_locked_for_write(&sec_path)?;

    // Re-check under the lock: a concurrent process may have written a valid
    // pair while we waited.
    let write_result = (|| -> Result<(TransportSecretKey, [u8; 32]), TransportError> {
        if sec_path.exists()
            && pub_path.exists()
            && let Ok(secret) = std::fs::read(&sec_path)
            && let Ok(public) = std::fs::read(&pub_path)
            && secret.len() == 32
            && public.len() == 32
        {
            let mut sk = [0u8; 32];
            let mut pk = [0u8; 32];
            sk.copy_from_slice(&secret);
            pk.copy_from_slice(&public);
            debug!("loaded existing transport keypair (written concurrently)");
            return Ok((TransportSecretKey(sk), pk));
        }
        use std::io::{Seek, SeekFrom, Write};
        sec_file.set_len(0)?;
        sec_file.seek(SeekFrom::Start(0))?;
        sec_file.write_all(&secret)?;
        sec_file.sync_all()?;
        std::fs::write(&pub_path, public)?;
        info!(
            "generated and wrote new Noise IK transport keypair to {}",
            sec_path.display()
        );
        Ok((TransportSecretKey(secret), public))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // These two tests mutate the shared test-config-root override.
    // Running them in parallel would clobber each other's temp dir
    // mid-flight (one test's restore racing the other's setup), so they are
    // serialized with `#[serial]` to keep them deterministic.
    #[test]
    #[serial]
    fn ensure_transport_keypair_generates_and_loads() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        // Redirect the keypair dir to the temp dir. XDG_CONFIG_HOME cannot be
        // used for this: on macOS `dirs::config_dir()` ignores it.
        set_test_config_root(Some(temp.path().to_path_buf()));

        let result = ensure_transport_keypair();

        set_test_config_root(None);

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

    #[test]
    #[serial]
    fn ensure_transport_keypair_is_race_safe() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        // The config-root override is thread-local, so each spawned thread
        // must install it itself before racing; both target the same temp dir.
        let root_a = temp.path().to_path_buf();
        let root_b = root_a.clone();

        // Both threads race to generate/write the keypair against the same
        // paths; the advisory file lock must serialize them so they converge
        // on one pair instead of interleaving torn writes.
        let handle_a = std::thread::spawn(move || {
            set_test_config_root(Some(root_a));
            ensure_transport_keypair()
        });
        let handle_b = std::thread::spawn(move || {
            set_test_config_root(Some(root_b));
            ensure_transport_keypair()
        });
        let result_a = handle_a.join();
        let result_b = handle_b.join();

        let (sk_a, pk_a) = result_a
            .expect("thread A panicked")
            .expect("thread A ensure_transport_keypair failed");
        let (sk_b, pk_b) = result_b
            .expect("thread B panicked")
            .expect("thread B ensure_transport_keypair failed");

        // Both threads must have converged on the same keypair: no torn
        // write, and the secret/public halves must not be mismatched.
        assert_eq!(sk_a.as_bytes(), sk_b.as_bytes());
        assert_eq!(pk_a, pk_b);
    }
}
