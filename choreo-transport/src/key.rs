use std::path::PathBuf;
use tracing::{debug, info};

use crate::error::TransportError;

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
#[doc(hidden)]
pub fn set_test_config_root(root: Option<PathBuf>) {
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

/// Render a 32-byte transport public key as a human-comparable fingerprint.
///
/// The fingerprint is the base64 (standard alphabet — the SAME encoding the
/// daemon's ACL uses for pubkeys) of the key itself, clustered into
/// 4-character groups: e.g. `3F2A 9C11 7B04 ... 8xE=`. Because the key is
/// only 32 bytes, the fingerprint is BIJECTIVE with the key — no hashing
/// (SSH hashes because host keys can be much longer), so comparing two
/// fingerprints compares the actual keys, and a fingerprint can be traced
/// back to its key by eye or by decoding.
///
/// This is the string both sides of an enrollment exchange render and
/// compare: the daemon operator reads out the server's fingerprint (or the
/// client's, for the ACL), and the client's first-contact flow displays it
/// for the human confirm step. The base64 cluster groups are small enough
/// to compare by eye without mistaking a single changed character.
pub fn fingerprint(pk: &[u8; 32]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(pk);
    // Cluster the 44-char base64 string into 4-char groups joined by
    // spaces. 44 % 4 == 0, so every group is exactly 4 chars — no ragged
    // tail to special-case. (from_utf8_lossy, not unwrap: the base64
    // alphabet is ASCII so lossy decoding is identity, but production code
    // has no unwrap escape hatches.)
    let groups: Vec<String> = b64
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    groups.join(" ")
}

/// [`fingerprint`] of the 32-byte key stored in a file.
///
/// Used by the `fingerprint` subcommand to render either side's key: with
/// no argument, the local `transport.pub`; with a path, a copied server key
/// (to verify a download) or any pinned known-servers entry. Enforces the
/// 32-byte length exactly like [`read_server_pk`] — a truncated or padded
/// file is an error, never a quietly wrong fingerprint.
pub fn fingerprint_of_file(path: &std::path::Path) -> Result<String, TransportError> {
    let bytes = std::fs::read(path)?;
    let len = bytes.len();
    let pk: [u8; 32] = bytes.try_into().map_err(|_| {
        TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("key file must be exactly 32 bytes, got {len}"),
        ))
    })?;
    Ok(fingerprint(&pk))
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

    /// A fixed known key: the fingerprint must be deterministic, purely a
    /// regrouping of the base64 encoding (so the ACL's base64 form and the
    /// fingerprint are trivially cross-checkable), 11 groups of exactly 4
    /// chars (44 base64 chars + 10 separators = 54), and decodable back to
    /// the original key.
    #[test]
    fn fingerprint_is_deterministic_grouped_and_bijective() {
        let pk = [
            0x3F, 0x2A, 0x9C, 0x11, 0x7B, 0x04, 0xE5, 0xD8, 0xA1, 0xC6, 0x42, 0xD9, 0x08, 0xF3,
            0xB7, 0xE2, 0x5D, 0x60, 0x19, 0xAB, 0xCC, 0x37, 0x84, 0x0E, 0x71, 0xFA, 0x92, 0x63,
            0xDD, 0x4B, 0x26, 0x50,
        ];
        let fp = fingerprint(&pk);
        assert_eq!(fp, fingerprint(&pk), "fingerprint must be deterministic");

        let groups: Vec<&str> = fp.split(' ').collect();
        assert_eq!(groups.len(), 11, "32 bytes -> 44 base64 chars -> 11 groups");
        for g in &groups {
            assert_eq!(g.len(), 4, "every group is exactly 4 chars: {g}");
        }

        // Stripping the separators yields the plain base64 — the same
        // string the ACL stores — and decoding THAT is the original key.
        use base64::Engine as _;
        let joined: String = groups.concat();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&joined)
            .expect("regrouped fingerprint must still be valid base64");
        assert_eq!(decoded, pk, "fingerprint is bijective with the key");
    }

    /// `fingerprint_of_file` round-trips a real key file and REJECTS a
    /// wrong-length file (a truncated download must never render as a
    /// plausible-looking fingerprint of something else).
    #[test]
    fn fingerprint_of_file_round_trips_and_rejects_bad_length() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let path = temp.path().join("key.bin");
        let pk = [7u8; 32];
        std::fs::write(&path, pk).expect("write key file");

        let fp = fingerprint_of_file(&path).expect("32-byte file renders");
        assert_eq!(fp, fingerprint(&pk));

        // 31 bytes (a classic truncation) must error, not render.
        std::fs::write(&path, [7u8; 31]).expect("write truncated file");
        assert!(fingerprint_of_file(&path).is_err());

        // 33 bytes (a trailing newline accident) must error too.
        let mut long = vec![7u8; 32];
        long.push(b'\n');
        std::fs::write(&path, long).expect("write overlong file");
        assert!(fingerprint_of_file(&path).is_err());
    }
}
