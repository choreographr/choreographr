//! The whole daemon must stop when its store latches a permanent I/O failure.
//!
//! `redb` latches the first failed file operation into a flag nothing clears:
//! every later read and write on that handle fails with `PreviousIo`, and only a
//! close-and-reopen clears it. The daemon clones one handle into every session
//! thread, so it cannot re-open in place — it has to exit, and the next launch
//! re-opens (and repairs) the file.
//!
//! That exit is the property under test. A latched store escalates through the
//! daemon's single shutdown trigger, the same one SIGINT uses: set the shared
//! flag and wake the blocking `accept()`. So the client is notified, the accept
//! loop leaves, the drain joins the sessions, and `run_server` RETURNS with the
//! socket unlinked. Stopping only the command loop (the shape this guards
//! against) leaves the accept loop serving a store that cannot be written: the
//! process keeps the socket bound — so `remove_stale_socket` refuses a restart
//! with "another daemon is already listening" — and keeps redb's exclusive file
//! lock, which leaves the store unreachable. This test fails by hanging in that
//! shape (the bounded waits below turn the hang into a named failure).
//!
//! Belongs to the `#[ignore = "integration"]` suite: it binds a real Unix
//! socket, runs the real `run_server` on its own thread, and drives it with a
//! real client over the wire.

// Every panic-family call below lives in the `#[test]` function's body, which
// clippy's allow-*-in-tests config covers directly — a plain helper fn here
// would need a file-level allowance instead.

use choreo_daemon::db::{BreakableBackend, Store};
use choreo_daemon::run_server;
use choreo_proto::{
    ClientMessage, ClientMessageType, DaemonMessage, DaemonMessageType, read_message, write_message,
};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crate::common;

/// Bound on every wait in this test. A healthy latch-to-exit takes well under a
/// second (the shutdown drain's graces sum to a few seconds at most); anything
/// past this is the wedged accept loop this test exists to catch.
const WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Bound on each client-side read, so a daemon that never answers fails the
/// test instead of blocking it.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A latched store stops the daemon end to end: the caller sees the real I/O
/// cause, the connected client observes `ShuttingDown` before the EOF,
/// `run_server` returns, and the socket file is gone.
#[test]
#[ignore = "integration"]
fn a_latched_store_stops_the_daemon_and_unlinks_its_socket() {
    let dir = tempfile::tempdir().expect("tempdir for the socket");
    let socket_path = dir.path().join("latch.sock");
    let socket_str = socket_path
        .to_str()
        .expect("socket path must be valid UTF-8")
        .to_string();

    // A store over a backend that starts healthy and fails every write once its
    // flag is set — the honest way to produce a REAL latched `redb` handle in a
    // test, without needing a full disk. `test_daemon_state` builds a normal
    // file-backed store; replace it before the core ever sees it.
    let (backend, failing) = BreakableBackend::new();
    let mut state = common::test_daemon_state();
    state.db = Arc::new(Store::with_backend(backend).expect("breakable store"));
    // The daemon takes `state`; this clone is the test's handle on the SAME
    // store, so the write below trips the very handle the daemon is serving.
    let store = Arc::clone(&state.db);

    let transport_sk = choreo_transport::key::TransportSecretKey::from([0u8; 32]);
    // No ACL file: this test never opens a TCP/Noise listener, so the ACL is
    // unused (the same shortcut `lifecycle_integration` takes).
    let acl = choreo_daemon::server::acl::SharedAcl::load(Path::new("/nonexistent"));
    let server = thread::spawn(move || {
        run_server(
            &socket_str,
            state,
            None,
            None,
            transport_sk,
            &acl,
            // Auto-exit off: this shutdown must come from the store latch, not
            // from the last client leaving.
            false,
            None,
        )
    });

    // Readiness, bounded: the bind creates the socket file.
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while !socket_path.exists() {
        assert!(
            Instant::now() < deadline,
            "the daemon never bound its socket at {}",
            socket_path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }

    // A real client on the real wire path.
    let stream = UnixStream::connect(&socket_path).expect("connect to the daemon");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set the read timeout");
    let mut reader = BufReader::new(stream.try_clone().expect("clone for the reader"));
    let mut writer = BufWriter::new(stream);

    // `ListSessions` rather than `Ping`: a Ping is answered by the connection
    // thread itself, while a command-loop reply proves this connection's
    // `RegisterClientWriter` has been processed. Commands are FIFO on the one
    // command channel, so a reply here means the client is registered and will
    // therefore receive the shutdown broadcast.
    write_message(
        &mut writer,
        &ClientMessage::request(0, ClientMessageType::ListSessions),
    )
    .expect("write ListSessions");
    writer.flush().expect("flush ListSessions");
    let reply: DaemonMessage = read_message(&mut reader).expect("read the ListSessions reply");
    assert!(
        matches!(reply.inner, DaemonMessageType::Sessions { .. }),
        "expected Sessions, got {:?}",
        reply.inner
    );

    // Trip the store. The write is a real one through the public `db` API, so
    // the failure path is the production one: the backend reports a full disk,
    // `redb` latches the handle, and `Store::err` escalates.
    //
    // Wait first for the ONE write a fresh daemon makes on its own: the
    // catalog-maintenance thread records its startup fetch attempt in
    // `catalog_state` right after the core comes up. Whichever write reaches
    // `redb` first is the one that latches the handle, and only the FIRST
    // failure carries the backend's own cause — every later one sees the latch —
    // so without this wait the assertion below would be racing the daemon's own
    // write and would intermittently check a masked `PreviousIo` instead.
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        let recorded = choreo_daemon::db::get_catalog_last_attempt_ms(&store)
            .expect("read the catalog attempt anchor");
        if recorded.is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never recorded its catalog fetch attempt"
        );
        thread::sleep(Duration::from_millis(10));
    }

    failing.store(true, Ordering::SeqCst);
    let err = choreo_daemon::db::kv_set(&store, 1, "k", b"v")
        .expect_err("a write to a broken backend must fail");
    // The caller must see the real cause. A retry-until-latched loop would
    // report a masked "previous I/O error occurred" here instead.
    assert!(
        err.to_string().contains("disk full (test)"),
        "the caller must see the cause, not a masked previous-IO error: {err}"
    );

    // The trigger runs the SIGINT drain, which broadcasts before closing, so the
    // client observes the notification rather than a bare EOF.
    let notification: DaemonMessage =
        read_message(&mut reader).expect("the client must be notified before the socket closes");
    assert!(
        matches!(notification.inner, DaemonMessageType::ShuttingDown),
        "expected ShuttingDown, got {:?}",
        notification.inner
    );

    // ... and then the EOF: the writer thread closed its socket after the
    // flush, so the reader's next frame header cannot arrive.
    let eof = read_message::<_, DaemonMessage>(&mut reader);
    assert!(
        eof.is_err(),
        "the connection must close after the notification, got {eof:?}"
    );

    // The whole point: `run_server` RETURNS. With the escalation stopping only
    // the command loop, this thread stayed blocked in `accept()` forever, and
    // the process kept the socket bound and the store locked.
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while !server.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit after its store latched: the accept loop is still serving a \
             store that cannot be written"
        );
        thread::sleep(Duration::from_millis(10));
    }
    server
        .join()
        .expect("server thread panicked")
        .expect("run_server must finish cleanly after a store latch");

    // The drain unlinked the socket, so the next launch binds a fresh one
    // instead of being refused as "another daemon is already listening".
    assert!(
        !socket_path.exists(),
        "the shutdown drain must unlink the socket at {}",
        socket_path.display()
    );
}
