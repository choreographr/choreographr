//! Connect-time keystore establishment tests (`establish_keystore`).
//!
//! These drive the real flow over `UnixStream::pair()` sockets (socket-
//! exercising tests belong in crate-level `tests/`, marked `#[ignore]` per
//! the workspace test discipline). The test acts as the "daemon" side and
//! feeds scripted `DaemonMessage` responses; `establish_keystore` is the
//! client side. Both `try_auto_unlock_key` and `bind_fresh_daemon` persist
//! keys into known_servers.toml, so every test installs its own
//! `TestConfigGuard` over a fresh tempdir to isolate config writes.

use choreo_client_core::KnownServers;
use choreo_im::establish_keystore;
use choreo_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;

/// Fixed addr string: `establish_keystore` keys known_servers on it.
const ADDR: &str = "test-addr:1";

/// Fresh isolated config root (tempdir + the `choreographr` subdir the
/// paths module appends), mirroring the credentials.rs test setup.
///
/// NOTE: `TestConfigGuard` is THREAD-LOCAL (see `choreo-keystore::paths`), so
/// it must be installed on every thread that resolves config paths — the
/// test thread AND the spawned client thread (see `run_client`).
fn isolated_config() -> (tempfile::TempDir, choreo_keystore::paths::TestConfigGuard) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();
    let guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
    (dir, guard)
}

/// Run `establish_keystore` on its own thread, re-installing the config
/// isolation guard there (thread_local does not cross thread boundaries).
/// Returns the join handle so the test can await the result.
fn run_client(
    root: std::path::PathBuf,
    mut reader: BufReader<UnixStream>,
    mut writer: BufWriter<UnixStream>,
) -> std::thread::JoinHandle<anyhow::Result<()>> {
    std::thread::spawn(move || {
        let _guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(root));
        establish_keystore(ADDR, &mut reader, &mut writer)
    })
}

/// Wire pair for one test: returns the client-side halves to hand to
/// `establish_keystore` and the "daemon"-side halves for scripted replies.
/// Buffered halves of a `UnixStream::pair()`: client side + daemon side.
type SocketHalves = (BufReader<UnixStream>, BufWriter<UnixStream>);

fn socket_pair() -> (SocketHalves, SocketHalves) {
    let (a, b) = UnixStream::pair().unwrap();
    let c_reader = BufReader::new(a.try_clone().unwrap());
    let c_writer = BufWriter::new(a);
    let d_reader = BufReader::new(b.try_clone().unwrap());
    let d_writer = BufWriter::new(b);
    ((c_reader, c_writer), (d_reader, d_writer))
}

/// Read one client message, asserting it is a `BindKeystore`, and return the
/// presented key.
fn expect_bind(client: &mut BufReader<UnixStream>) -> Vec<u8> {
    match read_message::<_, ClientMessage>(client).unwrap() {
        ClientMessage::BindKeystore { key } => key,
        other => panic!("expected BindKeystore, got {other:?}"),
    }
}

fn flush(d_writer: &mut BufWriter<UnixStream>) {
    d_writer.flush().unwrap();
}

/// No stored key on a fresh config root: the bridge probes with a fresh
/// `BindKeystore` (minted + recorded PRE-SEND), and an unbound daemon adopts
/// it and replies `Bound`.
#[ignore]
#[test]
fn unbound_daemon_auto_binds() {
    let (_dir, _guard) = isolated_config();
    let ((reader, writer), (mut d_reader, mut d_writer)) = socket_pair();

    let handle = run_client(_dir.path().to_path_buf(), reader, writer);

    let key = expect_bind(&mut d_reader);
    // Pre-send recording: the key must already be in known_servers for this
    // addr BEFORE the daemon acknowledges anything.
    let store = KnownServers::load().unwrap();
    assert_eq!(
        store.unlock_key(ADDR).unwrap(),
        Some(key.try_into().unwrap())
    );

    write_message(&mut d_writer, &DaemonMessage::Bound).unwrap();
    flush(&mut d_writer);

    handle.join().unwrap().unwrap();
}

/// A stored key triggers an `Unlock` first; when the daemon answers
/// `KeystoreUnbound`, the bridge auto-binds with a FRESH key (never the
/// stored one) that replaces the stored key in known_servers.
#[ignore]
#[test]
fn stored_key_unlock_then_auto_bind_mints_fresh_key() {
    let (_dir, _guard) = isolated_config();
    let stored_key: [u8; 32] = [7u8; 32];
    KnownServers::load()
        .unwrap()
        .set_unlock_key(ADDR, &stored_key)
        .unwrap();
    let ((reader, writer), (mut d_reader, mut d_writer)) = socket_pair();

    let handle = run_client(_dir.path().to_path_buf(), reader, writer);

    // Stored key path: an Unlock arrives first.
    match read_message::<_, ClientMessage>(&mut d_reader).unwrap() {
        ClientMessage::Unlock { private_key } => assert_eq!(private_key, stored_key.to_vec()),
        other => panic!("expected Unlock, got {other:?}"),
    }
    // Stored verify key can never create a binding → daemon answers unbound.
    write_message(
        &mut d_writer,
        &DaemonMessage::KeystoreUnbound {
            error: "no binding".to_string(),
        },
    )
    .unwrap();
    flush(&mut d_writer);

    // Auto-bind: a FRESH key, distinct from the stored verify key.
    let bind_key = expect_bind(&mut d_reader);
    assert_ne!(bind_key, stored_key.to_vec());
    // The fresh key REPLACES the stored key for this addr (pre-send record).
    let store = KnownServers::load().unwrap();
    assert_eq!(
        store.unlock_key(ADDR).unwrap(),
        Some(bind_key.try_into().unwrap())
    );

    write_message(&mut d_writer, &DaemonMessage::Bound).unwrap();
    flush(&mut d_writer);

    handle.join().unwrap().unwrap();
}

/// No stored key against an ALREADY-bound daemon: the probe bind is
/// rejected with `LockedError`, which is a benign fall-through (the
/// GetCredential tail carries the user-facing unlock guidance).
#[ignore]
#[test]
fn probe_against_bound_daemon_falls_through() {
    let (_dir, _guard) = isolated_config();
    let ((reader, writer), (mut d_reader, mut d_writer)) = socket_pair();

    let handle = run_client(_dir.path().to_path_buf(), reader, writer);

    expect_bind(&mut d_reader);
    write_message(
        &mut d_writer,
        &DaemonMessage::LockedError {
            error: "already bound".to_string(),
        },
    )
    .unwrap();
    flush(&mut d_writer);

    handle.join().unwrap().unwrap(); // Ok(())
}

/// Unlock with a stored key rejected by a bound daemon (wrong key) → Err
/// carrying the re-pair guidance.
#[ignore]
#[test]
fn unlock_rejected_yields_repair_guidance() {
    let (_dir, _guard) = isolated_config();
    let stored_key: [u8; 32] = [7u8; 32];
    KnownServers::load()
        .unwrap()
        .set_unlock_key(ADDR, &stored_key)
        .unwrap();
    let ((reader, writer), (mut d_reader, mut d_writer)) = socket_pair();

    let handle = run_client(_dir.path().to_path_buf(), reader, writer);

    assert!(matches!(
        read_message::<_, ClientMessage>(&mut d_reader).unwrap(),
        ClientMessage::Unlock { .. }
    ));
    write_message(
        &mut d_writer,
        &DaemonMessage::LockedError {
            error: "wrong key".to_string(),
        },
    )
    .unwrap();
    flush(&mut d_writer);

    let err = handle.join().unwrap().unwrap_err().to_string();
    assert!(err.contains("re-pair"), "missing re-pair guidance: {err}");
}

/// Weird case: the probe bind is answered with `KeystoreUnbound` (i.e. the
// daemon claims it is still unbound but rejected the binding) → Err.
#[ignore]
#[test]
fn bind_rejected_against_unbound_daemon_is_err() {
    let (_dir, _guard) = isolated_config();
    let ((reader, writer), (mut d_reader, mut d_writer)) = socket_pair();

    let handle = run_client(_dir.path().to_path_buf(), reader, writer);

    expect_bind(&mut d_reader);
    write_message(
        &mut d_writer,
        &DaemonMessage::KeystoreUnbound {
            error: "still unbound".to_string(),
        },
    )
    .unwrap();
    flush(&mut d_writer);

    let err = handle.join().unwrap().unwrap_err().to_string();
    assert!(err.contains("bind failed"), "unexpected error: {err}");
}
