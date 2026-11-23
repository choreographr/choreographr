//! Tests for the on-disk keypair: generation, the read-only accessor, the file
//! format and its dual read, permissions, and the race the advisory lock
//! closes.
//!
//! These mutate the thread-local `TEST_CONFIG_ROOT` seam, so they are
//! `#[serial]`-marked for libtest (which runs them as threads in one process);
//! each also restores the seam before returning so a sibling test cannot inherit
//! a temp root.

use super::encoding::encode_secret;
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

/// The derived public half of `sk`, computed independently of the loader.
fn derived_public(sk: &[u8; 32]) -> [u8; 32] {
    x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*sk)).to_bytes()
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
    assert_eq!(pk, derived_public(sk.as_bytes()));

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
    let derived = derived_public(sk.as_bytes());
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
    assert_eq!(pk, derived_public(&raw_key));

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
    assert_eq!(pk, derived_public(&key));

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
