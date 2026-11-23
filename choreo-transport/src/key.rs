//! On-disk Noise IK transport keypair management and key encoding.
//!
//! Provides the standard keypair paths under `{config}/choreographr`, the
//! [`ensure_transport_keypair`] loader/generator (race-safe via an advisory
//! file lock), its read-only counterpart [`load_transport_keypair`], and the
//! canonical base64 encoding of a 32-byte key ([`pubkey_base64`] /
//! [`parse_pubkey_base64`]) — the ONE textual form of a transport public key.
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

use std::path::{Path, PathBuf};

use base64::Engine as _;
use tracing::{debug, info};
use zeroize::Zeroize;

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
/// confused with other 32-byte values, and **zeroizes its bytes on drop**: it
/// is the instance's long-lived identity, so a stray copy left in freed heap or
/// stack memory is a copy an attacker who can read this process's memory later
/// can lift (or that a core dump preserves).
///
/// Deliberately **not `Copy`**. A `Copy` struct is duplicated byte-for-byte by
/// implicit assignment, and those duplicates escape [`Drop`] — they would be
/// zeroized nowhere, which defeats the point. A caller that genuinely needs a
/// second owner makes an explicit `clone` whose lifetime is visible.
#[derive(Clone)]
pub struct TransportSecretKey(pub(crate) [u8; 32]);

impl TransportSecretKey {
    /// Wrap a raw 32-byte secret key.
    ///
    /// # Errors
    ///
    /// Never fails; infallible by construction.
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        TransportSecretKey(bytes)
    }

    /// Expose the inner bytes for use in handshake APIs.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Zeroize for TransportSecretKey {
    /// Overwrite the secret bytes with zeros.
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for TransportSecretKey {
    /// Zeroize on drop, so ownership ending really does erase the key rather
    /// than merely returning its memory to the allocator.
    fn drop(&mut self) {
        self.zeroize();
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

/// Pair a secret with the public key **derived** from it.
///
/// This is the one place a loaded keypair's public half comes from, which is
/// what makes "the secret and the public half disagree" unrepresentable: there
/// is no second source to disagree with.
fn keypair_from_secret(sk: [u8; 32]) -> (TransportSecretKey, [u8; 32]) {
    let public = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(sk)).to_bytes();
    (TransportSecretKey::new(sk), public)
}

/// Encode a 32-byte key as standard-alphabet base64 (44 characters).
///
/// This is the canonical — and only — textual form of a transport public key:
/// the ACL stores pubkeys in exactly this form, and the value an operator reads
/// out of `choreographr pubkey` is byte-for-byte this string.
#[must_use]
pub fn pubkey_base64(pk: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(pk)
}

/// Parse a base64-encoded 32-byte key, trimming surrounding whitespace.
///
/// The inverse of [`pubkey_base64`], and tolerant of the trailing newline the
/// on-disk form carries.
///
/// Only the canonical form is accepted: the value must decode to exactly 32
/// bytes. A space-grouped key is not base64 and is refused with a message that
/// says so, so an operator who pasted a grouped value is told to send the plain
/// form rather than silently connecting to nothing.
///
/// # Errors
///
/// Returns [`TransportError::Io`] (with [`std::io::ErrorKind::InvalidData`])
/// when the text is not valid base64 or does not decode to exactly 32 bytes.
pub fn parse_pubkey_base64(text: &str) -> Result<[u8; 32], TransportError> {
    decode_key_base64(text).map_err(|detail| {
        TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid base64 key: {detail}"),
        ))
    })
}

/// Decode standard-alphabet base64 key text into exactly 32 bytes.
///
/// The single decode implementation behind both the public key parser and the
/// secret-file loader. The failure is a message rather than an error type so
/// each caller wraps it in the error its context calls for (a public key parse
/// is a plain I/O-shaped error; a secret file names its path and refuses
/// regeneration). Messages carry lengths and decoder positions only — never the
/// decoded bytes.
fn decode_key_base64(text: &str) -> Result<[u8; 32], String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|e| format!("not valid base64: {e}"))?;
    let len = decoded.len();
    let key: [u8; 32] = decoded
        .try_into()
        .map_err(|_| format!("decodes to {len} bytes, expected 32"))?;
    Ok(key)
}

/// Encode a secret key for on-disk storage: standard base64 plus a trailing
/// newline, so the file is a well-formed text file (the loader trims it).
fn encode_secret(sk: &[u8; 32]) -> String {
    let mut text = base64::engine::general_purpose::STANDARD.encode(sk);
    text.push('\n');
    text
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

/// Path to the Noise IK static secret key (~/.config/choreographr/transport.sec)
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
fn load_secret_key(path: &Path) -> Result<Option<[u8; 32]>, TransportError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
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
    decode_secret(&bytes)
        .map(Some)
        .map_err(|detail| TransportError::InvalidSecretKey {
            path: path.display().to_string(),
            detail,
        })
}

/// Decode secret key file contents as either the legacy raw 32-byte form or the
/// current base64 text form.
///
/// A file of exactly 32 bytes IS the legacy raw key. The base64 form is 44
/// characters plus a newline (45 bytes), and a 32-character base64 string would
/// decode to 24 bytes — so the raw length can never be mistaken for text.
fn decode_secret(bytes: &[u8]) -> Result<[u8; 32], String> {
    if bytes.len() == 32 {
        let mut sk = [0u8; 32];
        sk.copy_from_slice(bytes);
        return Ok(sk);
    }
    let len = bytes.len();
    let text = std::str::from_utf8(bytes).map_err(|_| {
        format!("{len} bytes of non-UTF-8 data, expected 32 raw bytes or base64 text")
    })?;
    decode_key_base64(text).map_err(|detail| format!("{len}-byte file: {detail}"))
}

/// Ensure that a Noise IK transport keypair exists at the standard path
/// (~/.config/choreographr/transport.sec).
///
/// If the secret file exists and decodes (base64, or a legacy raw 32-byte key),
/// loads it and returns it paired with the public key **derived** from it.
/// Otherwise generates a new X25519 keypair and writes the secret as base64
/// text — the only file it writes: there is no public-key file to write, since
/// the public half is derived from the secret on every load.
///
/// Generating is the daemon's startup behaviour and nothing else's; a query that
/// must not mint an identity uses [`load_transport_keypair`] instead.
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
    // Hoisted to the top of the function per clippy::items_after_statements:
    // the traits are needed in the write path below.
    use std::io::{Seek, SeekFrom, Write};

    let sec_path = transport_sec_path()?;

    // Fast path: a loadable secret means the keypair already exists, so this
    // takes no lock at all on the common start-up path.
    if let Some(sk) = load_secret_key(&sec_path)? {
        debug!("loaded existing transport secret key");
        return Ok(keypair_from_secret(sk));
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
    // key while we waited, in which case its key wins and ours is discarded —
    // two instances starting together must converge on ONE identity, or the
    // second one's ACL entry would never match the key it presents.
    let write_result = (|| -> Result<(TransportSecretKey, [u8; 32]), TransportError> {
        if let Some(sk) = load_secret_key(&sec_path)? {
            debug!("loaded existing transport secret key (written concurrently)");
            return Ok(keypair_from_secret(sk));
        }
        let encoded = encode_secret(&secret);
        sec_file.set_len(0)?;
        sec_file.seek(SeekFrom::Start(0))?;
        sec_file.write_all(encoded.as_bytes())?;
        sec_file.sync_all()?;
        // The secret is the whole on-disk identity: the public half is derived
        // from it on every load, so no second file is written to disagree with
        // it (and no crash can leave one half behind the other).
        info!(
            "generated and wrote new Noise IK transport keypair to {}",
            sec_path.display()
        );
        Ok((TransportSecretKey::new(secret), public))
    })();

    let unlock_result = sec_file.unlock();
    let pair = write_result?;
    unlock_result?;
    Ok(pair)
}

/// Load an existing transport keypair, deriving its public half from the
/// secret — and **never generating one**.
///
/// The read-only counterpart of [`ensure_transport_keypair`], and the accessor
/// every query uses (the daemon's `pubkey` subcommand). It exists so that
/// printing a key cannot create the identity it
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

    /// Install the test config root and return the keypair directory inside it,
    /// created so a test can plant a secret file before calling the loader.
    fn install_temp_root(temp: &tempfile::TempDir) -> PathBuf {
        set_test_config_root(Some(temp.path().to_path_buf()));
        let dir = temp.path().join("choreographr");
        std::fs::create_dir_all(&dir).expect("create keypair dir");
        dir
    }

    // These tests mutate the shared test-config-root override.
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
        // The public half is derived on every load, so generation writes ONE
        // file: a `transport.pub` on disk would be a second copy that could
        // drift from the secret.
        assert!(
            !temp
                .path()
                .join("choreographr")
                .join("transport.pub")
                .exists(),
            "generation must not write a public-key file"
        );
    }

    /// The read-only accessor loads an existing secret and derives the same
    /// public half the generator returned — and, with no secret present, ERRORS
    /// without creating one. That refusal is the whole point: a query must never
    /// mint an identity.
    #[test]
    #[serial]
    fn load_transport_keypair_derives_and_never_generates() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let dir = install_temp_root(&temp);
        let sec_path = dir.join("transport.sec");

        // No identity yet: an error, and the directory is left exactly as it was.
        let err = load_transport_keypair().expect_err("absent secret must not load");
        assert!(
            matches!(err, TransportError::MissingSecretKey { .. }),
            "expected MissingSecretKey, got {err:?}"
        );
        assert!(
            !sec_path.exists(),
            "a read-only load must not create the secret"
        );

        // Generate, then load: the public half matches the generator's.
        let (sk, pk) = ensure_transport_keypair().expect("generate keypair");
        let before = std::fs::read(&sec_path).expect("read secret file");

        let (sk2, pk2) = load_transport_keypair().expect("load existing keypair");
        assert_eq!(sk2.as_bytes(), sk.as_bytes());
        assert_eq!(pk2, pk);
        assert_eq!(
            std::fs::read(&sec_path).expect("re-read secret file"),
            before,
            "the read-only load must not rewrite the secret"
        );

        // An empty file is what a crash mid-creation leaves behind; it holds no
        // identity, so it is reported as missing rather than silently made up.
        std::fs::write(&sec_path, b"").expect("empty the secret file");
        assert!(matches!(
            load_transport_keypair().expect_err("empty secret must not load"),
            TransportError::MissingSecretKey { .. }
        ));

        set_test_config_root(None);
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

    /// The secret is written as standard base64 + newline (44 chars + `\n`),
    /// and loading that same file round-trips to the identical keypair.
    #[test]
    #[serial]
    fn secret_file_is_base64_and_round_trips() {
        use base64::Engine as _;

        let temp = tempfile::TempDir::new().expect("tempdir");
        let dir = install_temp_root(&temp);
        let sec_path = dir.join("transport.sec");

        let (sk, pk) = ensure_transport_keypair().expect("generate keypair");
        let raw = std::fs::read(&sec_path).expect("read secret file");

        assert_eq!(raw.len(), 45, "44 base64 chars plus one newline");
        assert_eq!(raw.last(), Some(&b'\n'), "text file ends with a newline");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&raw[..44])
            .expect("secret file body is valid base64");
        assert_eq!(decoded, sk.as_bytes(), "file holds the secret verbatim");

        // Round-trip: the loader reads back the very same keypair, and does not
        // rewrite the file it just read.
        let (sk2, pk2) = ensure_transport_keypair().expect("reload keypair");
        assert_eq!(sk2.as_bytes(), sk.as_bytes());
        assert_eq!(pk2, pk);
        assert_eq!(
            std::fs::read(&sec_path).expect("re-read secret file"),
            raw,
            "loading must not rewrite the secret file"
        );

        set_test_config_root(None);
    }

    /// The returned public half is the one derived from the returned secret, so
    /// a stale or corrupt public file can never be loaded.
    #[test]
    #[serial]
    fn public_key_is_derived_from_the_secret() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let dir = install_temp_root(&temp);

        let (sk, pk) = ensure_transport_keypair().expect("generate keypair");
        let derived =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*sk.as_bytes()))
                .to_bytes();
        assert_eq!(pk, derived, "public key must be derived from the secret");

        // A deliberately WRONG public file must change nothing: the loader
        // ignores it entirely.
        std::fs::write(dir.join("transport.pub"), [0xFFu8; 32]).expect("plant bad public file");
        let (sk2, pk2) = ensure_transport_keypair().expect("reload keypair");
        assert_eq!(sk2.as_bytes(), sk.as_bytes());
        assert_eq!(pk2, derived, "the stored public file is never read");

        set_test_config_root(None);
    }

    /// A legacy 32-raw-byte secret file still loads, unchanged on disk.
    #[test]
    #[serial]
    fn legacy_raw_secret_still_loads() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let dir = install_temp_root(&temp);
        let sec_path = dir.join("transport.sec");

        let raw_key = [0x42u8; 32];
        std::fs::write(&sec_path, raw_key).expect("write legacy raw secret");

        let (sk, pk) = ensure_transport_keypair().expect("load legacy raw secret");
        assert_eq!(sk.as_bytes(), &raw_key);
        let derived =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(raw_key)).to_bytes();
        assert_eq!(pk, derived);

        // The loader must not migrate the file underneath the operator.
        assert_eq!(
            std::fs::read(&sec_path).expect("re-read secret file"),
            raw_key,
            "loading a legacy key must not rewrite it"
        );

        set_test_config_root(None);
    }

    /// A base64 secret (with the trailing newline the writer emits) loads.
    #[test]
    #[serial]
    fn base64_secret_loads() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let dir = install_temp_root(&temp);
        let sec_path = dir.join("transport.sec");

        let key = [0x11u8; 32];
        std::fs::write(&sec_path, encode_secret(&key)).expect("write base64 secret");
        assert_eq!(
            std::fs::read(&sec_path).expect("read back").len(),
            45,
            "encoding is 44 base64 chars + newline"
        );

        let (sk, pk) = ensure_transport_keypair().expect("load base64 secret");
        assert_eq!(sk.as_bytes(), &key);
        let derived =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(key)).to_bytes();
        assert_eq!(pk, derived);

        set_test_config_root(None);
    }

    /// A present-but-garbage secret file is a hard error AND is left untouched:
    /// silently regenerating would replace the instance identity and orphan
    /// every ACL entry and pinned server key.
    #[test]
    #[serial]
    fn unparseable_secret_is_rejected_and_left_untouched() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let dir = install_temp_root(&temp);
        let sec_path = dir.join("transport.sec");

        let garbage = b"this is not a transport secret key\n".to_vec();
        std::fs::write(&sec_path, &garbage).expect("write garbage secret");

        let err = ensure_transport_keypair().expect_err("garbage must not load");
        let TransportError::InvalidSecretKey { path, .. } = &err else {
            panic!("expected InvalidSecretKey, got {err:?}");
        };
        assert!(
            path.ends_with("transport.sec"),
            "error names the offending file: {path}"
        );

        assert_eq!(
            std::fs::read(&sec_path).expect("re-read secret file"),
            garbage,
            "the unparseable file must be left byte-identical"
        );
        assert!(
            !dir.join("transport.pub").exists(),
            "a rejected secret must not produce a public file"
        );

        set_test_config_root(None);
    }

    /// The canonical base64 helpers round-trip and reject malformed input.
    #[test]
    fn pubkey_base64_round_trips_and_rejects_malformed_input() {
        let pk = [0xA5u8; 32];
        let text = pubkey_base64(&pk);
        assert_eq!(text.len(), 44);
        assert_eq!(parse_pubkey_base64(&text).expect("decode"), pk);

        // The on-disk form carries a trailing newline; whitespace is trimmed.
        assert_eq!(
            parse_pubkey_base64(&format!("{text}\n")).expect("decode"),
            pk
        );

        assert!(parse_pubkey_base64("not base64!!").is_err());
        // Valid base64 of the wrong length decodes but must not be accepted.
        // The error must say so (lengths, never bytes) so an operator knows the
        // value was a well-formed key of the wrong size, not a typo.
        let short_err = parse_pubkey_base64("AAAA").expect_err("16 bits is not a key");
        assert!(
            short_err.to_string().contains("expected 32"),
            "a wrong-length key must name the expected length: {short_err}"
        );
    }

    /// Only the canonical 44-character base64 form parses. A space-grouped
    /// value (the grouping some tools add for human comparison) is NOT base64
    /// and is refused — the project has exactly one textual key form, so the
    /// parser must not paper over a grouped paste.
    #[test]
    fn parse_pubkey_base64_rejects_grouped_and_wrong_length_values() {
        let pk = [0xA5u8; 32];
        let canonical = pubkey_base64(&pk);
        assert_eq!(canonical.len(), 44, "a 32-byte key is 44 base64 chars");

        // A group-of-four rendering of the SAME key must be refused, even
        // though it carries the right characters — the spaces make it invalid
        // base64, and the parser accepts only the canonical form.
        let grouped: String = canonical
            .as_bytes()
            .chunks(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let grouped_err = parse_pubkey_base64(&grouped).expect_err("grouped is not base64");
        assert!(
            grouped_err.to_string().contains("not valid base64"),
            "a grouped value must be reported as invalid base64: {grouped_err}"
        );

        // Too few / too many decoded bytes are both refused.
        assert!(parse_pubkey_base64("AAAA").is_err(), "4 chars is 3 bytes");
        assert!(
            parse_pubkey_base64(&format!("{canonical}AAAA")).is_err(),
            "48 chars is more than 32 bytes"
        );
    }
}
