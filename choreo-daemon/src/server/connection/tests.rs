use super::*;
use crate::broadcast::test_sink;
use std::sync::LazyLock;

/// Shared in-memory database for the `ClientCtx` literals in this module.
/// Every literal now carries a `db: &'a redb::Database` field (required by
/// the struct whether or not the handler under test reads through it), and
/// a `redb::Database` is `Send + Sync`, so one lazily-initialized static —
/// backed by `InMemoryBackend`, which needs no filesystem path — serves
/// them all. Tests that DO exercise the store (the `GetImage` serve path)
/// write into this same handle with unique session/turn ids.
static TEST_DB: LazyLock<redb::Database> = LazyLock::new(|| {
    redb::Database::builder()
        .create_with_backend(redb::backends::InMemoryBackend::new())
        .expect("create the in-memory test database")
});

/// A `ConnectionWriter` test double that forwards every written message
/// to a channel and records shutdown calls on another. Message-passing
/// only (no shared state across threads): the test reads the record
/// after joining the writer thread.
struct MockConnectionWriter {
    sent: mpsc::Sender<DaemonMessage>,
    shutdown_tx: mpsc::Sender<()>,
    /// Fail `send_message` on the Nth call (1-based) to exercise the
    /// error path of `writer_thread`.
    fail_on: Option<usize>,
    calls: usize,
}

impl ConnectionWriter for MockConnectionWriter {
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
        self.calls += 1;
        if self.fail_on == Some(self.calls) {
            return Err("mock write failure".to_string());
        }
        let _ = self.sent.send(msg.clone());
        Ok(())
    }
    fn shutdown(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

fn mock_writer(
    fail_on: Option<usize>,
) -> (
    MockConnectionWriter,
    mpsc::Receiver<DaemonMessage>,
    mpsc::Receiver<()>,
) {
    let (sent_tx, sent_rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    (
        MockConnectionWriter {
            sent: sent_tx,
            shutdown_tx,
            fail_on,
            calls: 0,
        },
        sent_rx,
        shutdown_rx,
    )
}

/// The core `writer_thread` contract: `ShuttingDown` is flushed, the
/// socket is shut down HERE (by the writer thread itself), and draining
/// stops — a message enqueued after the notification is never written,
/// so the client observes the notification before the EOF.
#[test]
fn writer_thread_flushes_shutting_down_then_shuts_down_and_stops() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let (writer, sent_rx, shutdown_rx) = mock_writer(None);
    let bytes = Arc::new(AtomicUsize::new(0));
    let global = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn({
        let bytes = Arc::clone(&bytes);
        let global = Arc::clone(&global);
        move || writer_thread(writer, &rx, &bytes, &global)
    });

    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();
    tx.send(DaemonMessage::broadcast(DaemonMessageType::ShuttingDown))
        .unwrap();
    // Queued after the notification: must never be written.
    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();

    handle.join().expect("writer thread panicked");
    let written: Vec<_> = sent_rx.try_iter().collect();
    assert_eq!(
        written,
        vec![
            DaemonMessage::broadcast(DaemonMessageType::Pong),
            DaemonMessage::broadcast(DaemonMessageType::ShuttingDown)
        ],
        "ShuttingDown must be flushed in order, then draining must stop"
    );
    assert!(
        shutdown_rx.try_recv().is_ok(),
        "the writer thread must close the socket itself after ShuttingDown"
    );
}

/// `Evicted` is handled exactly like `ShuttingDown`: flushed, socket shut
/// down, draining stops — the lag-eviction advisory is also a
/// notify-before-EOF on the graceful path (the daemon additionally
/// force-closes the socket, but the ordering guarantee is preserved here).
#[test]
fn writer_thread_flushes_evicted_then_shuts_down_and_stops() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let (writer, sent_rx, shutdown_rx) = mock_writer(None);
    let bytes = Arc::new(AtomicUsize::new(0));
    let global = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn({
        let bytes = Arc::clone(&bytes);
        let global = Arc::clone(&global);
        move || writer_thread(writer, &rx, &bytes, &global)
    });

    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();
    tx.send(DaemonMessage::broadcast(DaemonMessageType::Evicted))
        .unwrap();
    // Queued after the advisory: must never be written.
    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();

    handle.join().expect("writer thread panicked");
    let written: Vec<_> = sent_rx.try_iter().collect();
    assert_eq!(
        written,
        vec![
            DaemonMessage::broadcast(DaemonMessageType::Pong),
            DaemonMessage::broadcast(DaemonMessageType::Evicted)
        ],
        "Evicted must be flushed in order, then draining must stop"
    );
    assert!(
        shutdown_rx.try_recv().is_ok(),
        "the writer thread must close the socket itself after Evicted"
    );
}

/// A send error is fatal for the connection: the loop stops AND shuts the
/// socket down. A send error is either a broken pipe (socket gone —
/// shutdown is a harmless no-op) or a [`WRITER_WRITE_TIMEOUT`] on a wedged
/// client whose receive window is zero (socket still open — shutdown is
/// what unblocks the reader's blocking read so the connection is reaped).
#[test]
fn writer_thread_stops_and_shuts_down_on_send_error() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    // Fail on the second write: the first Pong goes out, the loop breaks.
    let (writer, sent_rx, shutdown_rx) = mock_writer(Some(2));
    let bytes = Arc::new(AtomicUsize::new(0));
    let global = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn({
        let bytes = Arc::clone(&bytes);
        let global = Arc::clone(&global);
        move || writer_thread(writer, &rx, &bytes, &global)
    });

    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();
    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();
    tx.send(DaemonMessage::broadcast(DaemonMessageType::ShuttingDown))
        .unwrap();
    drop(tx); // disconnect so the thread cannot linger

    handle.join().expect("writer thread panicked");
    let written: Vec<_> = sent_rx.try_iter().collect();
    assert_eq!(written.len(), 1, "writer must stop at the failing message");
    assert!(
        shutdown_rx.try_recv().is_ok(),
        "writer must shut the socket down on a send error so the reader is unblocked"
    );
}

/// A disconnected channel ends the loop cleanly without shutdown — the
/// normal drain-to-exit path for a disconnected client.
#[test]
fn writer_thread_exits_cleanly_on_disconnect() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let (writer, sent_rx, shutdown_rx) = mock_writer(None);
    let bytes = Arc::new(AtomicUsize::new(0));
    let global = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn({
        let bytes = Arc::clone(&bytes);
        let global = Arc::clone(&global);
        move || writer_thread(writer, &rx, &bytes, &global)
    });

    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();
    drop(tx); // all senders gone: the for-loop drains and ends

    handle.join().expect("writer thread panicked");
    let written: Vec<_> = sent_rx.try_iter().collect();
    assert_eq!(
        written,
        vec![DaemonMessage::broadcast(DaemonMessageType::Pong)]
    );
    assert!(shutdown_rx.try_recv().is_err());
}

/// The writer thread decrements the per-client and daemon-wide byte
/// counters once per dequeued message, using each message's approximate
/// wire size — the exact counterpart of `enqueue`'s increment. A
/// two-message drain must zero both counters.
#[test]
fn writer_thread_decrements_byte_counters_per_message() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let (writer, _sent_rx, _shutdown_rx) = mock_writer(None);
    let bytes = Arc::new(AtomicUsize::new(0));
    let global = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn({
        let bytes = Arc::clone(&bytes);
        let global = Arc::clone(&global);
        move || writer_thread(writer, &rx, &bytes, &global)
    });

    let m1 = DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(1),
        event: SessionEvent::Failed {
            stream_id: 1,
            error: "a".repeat(100),
        },
    });
    let m2 = DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(2),
        event: SessionEvent::Failed {
            stream_id: 2,
            error: "b".repeat(50),
        },
    });
    let s1 = m1.approx_wire_size();
    let s2 = m2.approx_wire_size();

    // Pre-seed the counters exactly as `enqueue` would have (the two
    // messages are queued and counted before the writer starts).
    bytes.fetch_add(s1 + s2, Ordering::Relaxed);
    global.fetch_add(s1 + s2, Ordering::Relaxed);

    tx.send(m1).unwrap();
    tx.send(m2).unwrap();
    drop(tx);
    handle.join().expect("writer thread panicked");

    assert_eq!(
        bytes.load(Ordering::Relaxed),
        0,
        "every dequeued message must decrement the per-client counter"
    );
    assert_eq!(
        global.load(Ordering::Relaxed),
        0,
        "every dequeued message must decrement the daemon-wide counter"
    );
}

/// The abandoned-backlog drain: when the writer stops at `Evicted` (or a
/// send error), messages queued AFTER the stop point are never written —
/// but they were counted at enqueue. The post-loop drain must decrement
/// both counters for them, or an evicted client's backlog would stay
/// frozen in the daemon-wide total forever (the leak that could
/// permanently exhaust the global budget). The abandoned messages must
/// NOT appear on the wire.
#[test]
fn writer_thread_drains_and_decrements_abandoned_backlog() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let (writer, sent_rx, _shutdown_rx) = mock_writer(None);
    let bytes = Arc::new(AtomicUsize::new(0));
    let global = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn({
        let bytes = Arc::clone(&bytes);
        let global = Arc::clone(&global);
        move || writer_thread(writer, &rx, &bytes, &global)
    });

    let m1 = DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(1),
        event: SessionEvent::Failed {
            stream_id: 1,
            error: "a".repeat(100),
        },
    });
    let m2 = DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(2),
        event: SessionEvent::Failed {
            stream_id: 2,
            error: "b".repeat(50),
        },
    });
    let m3 = DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(3),
        event: SessionEvent::Failed {
            stream_id: 3,
            error: "c".repeat(25),
        },
    });
    let s1 = m1.approx_wire_size();
    let s2 = m2.approx_wire_size();
    let s3 = m3.approx_wire_size();

    // Pre-seed the counters exactly as `enqueue` would have for ALL four
    // messages (m1 + Evicted are written; m2/m3 are abandoned behind the
    // stop point).
    let evicted_size = DaemonMessage::broadcast(DaemonMessageType::Evicted).approx_wire_size();
    let total = s1 + s2 + s3 + evicted_size;
    bytes.fetch_add(total, Ordering::Relaxed);
    global.fetch_add(total, Ordering::Relaxed);

    tx.send(m1.clone()).unwrap();
    tx.send(DaemonMessage::broadcast(DaemonMessageType::Evicted))
        .unwrap();
    // Queued behind the advisory: never written, but must be decremented
    // by the exit drain.
    tx.send(m2).unwrap();
    tx.send(m3).unwrap();

    handle.join().expect("writer thread panicked");
    let written: Vec<_> = sent_rx.try_iter().collect();
    assert_eq!(
        written,
        vec![
            m1.clone(),
            DaemonMessage::broadcast(DaemonMessageType::Evicted)
        ],
        "messages behind the advisory must never be written"
    );
    assert_eq!(
        bytes.load(Ordering::Relaxed),
        0,
        "abandoned backlog must be decremented from the per-client counter"
    );
    assert_eq!(
        global.load(Ordering::Relaxed),
        0,
        "abandoned backlog must be decremented from the daemon-wide counter"
    );
}

/// Drive the drop guard directly: a `ReplyHandle` that is never sent must
/// trip the debug assertion on drop, so a handler that forgets its reply
/// cannot pass silently in a debug build.
#[test]
#[should_panic(expected = "left unacknowledged")]
fn reply_handle_dropped_unanswered_trips_guard() {
    let (sink, _rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let handle = ReplyHandle {
        sink: crate::broadcast::ReplySink::new(7, sink, global_lag),
        sent: false,
    };
    // Never sent: the guard must fire here.
    drop(handle);
}

/// The companion positive case: `send` stamps the request id onto the
/// reply and balances the daemon-wide lag counter (the counter holds the
/// enqueued bytes until the writer thread's dequeue decrement, which does
/// not run here).
#[test]
fn reply_handle_send_stamps_id_and_accounts_bytes() {
    let (sink, rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let handle = ReplyHandle {
        sink: crate::broadcast::ReplySink::new(99, sink, Arc::clone(&global_lag)),
        sent: false,
    };
    handle.send(DaemonMessageType::Pong);

    let msg = rx.recv().unwrap();
    assert_eq!(msg.id, Some(99), "the reply must carry the request id");
    assert!(matches!(msg.inner, DaemonMessageType::Pong));
    assert_eq!(
        global_lag.load(Ordering::Relaxed),
        DaemonMessage::reply(99, DaemonMessageType::Pong).approx_wire_size(),
        "send must increment the daemon-wide lag counter for the enqueued reply"
    );
}

/// The defensive wildcard must REPLY, not drop: `GetSessionState` is on the
/// wire but has no connection-thread handler, so it lands in the wildcard,
/// which answers a typed `Failed` naming the request's kind. This keeps the
/// client's pending slot resolvable instead of stranding it until timeout.
#[test]
fn dispatch_unhandled_request_replies_failed_with_kind() {
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(
        ClientMessage::request(11, ClientMessageType::GetSessionState { session_id: 1 }),
        &mut ctx,
    )
    .unwrap();

    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(11));
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::Failed {
            kind: choreo_proto::MessageKind::GetSessionState,
            error,
        } if error == "unsupported request"
    ));
}

#[test]
fn handle_acl_add_sync_refuses_remote_clients_without_dialing_daemon() {
    // A TCP client's AclAdd must be refused at the connection layer: the
    // daemon command loop is never even contacted (asserted by the
    // channel receiver staying empty), and the client gets a structured
    // refusal — the approver for a trust decision must be at the machine.
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 7,
        request_id: 0,
        is_unix: false, // a TCP/Noise client
    };

    let handle = ctx.reply_handle();
    handle_acl_add_sync(
        &mut ctx,
        handle,
        "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=",
    );

    let msg = writer_rx.recv().unwrap();
    match msg.inner {
        DaemonMessageType::AclAddResult { ok: false, message } => {
            assert!(
                message.contains("local connections"),
                "the refusal must explain the trust boundary, got: {message}"
            );
        }
        other => panic!("expected AclAddResult refusal, got {other:?}"),
    }
    // The command loop saw NOTHING (a refusal must not even route).
    assert!(
        daemon_rx.try_recv().is_err(),
        "a remote AclAdd must never reach the daemon command loop"
    );
}

#[test]
fn handle_unlock_sync_ok() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    // The daemon command loop now enqueues the targeted reply itself:
    // the stub simulates that by sending Unlocked through the reply target
    // BEFORE the ack (the ORDERING INVARIANT shape).
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::Unlock { reply, ack, .. }) = daemon_rx.recv() {
            if let Some(target) = &reply {
                target.send(DaemonMessageType::Unlocked);
            }
            let _ = ack.send(());
        }
    });
    handle_unlock_sync(&mut ctx, MessageKind::Unlock, vec![0u8; 32]);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg.inner, DaemonMessageType::Unlocked));
}

#[test]
fn handle_unlock_sync_err() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    // The daemon command loop enqueues the targeted LockedError itself;
    // the stub simulates that (see the ordering-invariant note above).
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::Unlock { reply, ack, .. }) = daemon_rx.recv() {
            if let Some(target) = &reply {
                target.send(DaemonMessageType::LockedError {
                    error: "wrong password".to_string(),
                });
            }
            let _ = ack.send(());
        }
    });
    handle_unlock_sync(&mut ctx, MessageKind::Unlock, vec![0u8; 32]);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg.inner, DaemonMessageType::LockedError { .. }));
    if let DaemonMessageType::LockedError { error } = &msg.inner {
        assert_eq!(error, "wrong password");
    }
}

#[test]
fn handle_unlock_sync_disconnected() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    drop(daemon_rx);
    handle_unlock_sync(&mut ctx, MessageKind::Unlock, vec![0u8; 32]);
    assert!(writer_rx.try_recv().is_err());
}

#[test]
fn handle_lock_sync_ok_replies_locked() {
    // `/lock` (ClientMessageType::Lock) routes a Lock command and, on success,
    // replies `Locked` to the acting client; the daemon separately
    // broadcasts `Locked` to every activity subscriber (the acting client
    // included, harmlessly idempotent).
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::Lock { reply }) = daemon_rx.recv() {
            let _ = reply.send(Ok(()));
        }
    });
    let handle = ctx.reply_handle();
    handle_lock_sync(&mut ctx, handle);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg.inner, DaemonMessageType::Locked));
}

#[test]
fn handle_lock_sync_err_replies_locked_error() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::Lock { reply }) = daemon_rx.recv() {
            // Lock's reply channel still carries a plain String error:
            // /lock is not a binding-verification operation.
            let _ = reply.send(Err("cannot lock".into()));
        }
    });
    let handle = ctx.reply_handle();
    handle_lock_sync(&mut ctx, handle);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg.inner, DaemonMessageType::LockedError { .. }));
}

#[test]
fn handle_list_models_sync_ok() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::ListModels { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Ok((
                vec!["gpt-4".into(), "gpt-3.5".into()],
                Some("gpt-4".into()),
            )));
        }
    });
    let handle = ctx.reply_handle();
    handle_list_models_sync(&mut ctx, handle, None);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg.inner, DaemonMessageType::Models { .. }));
}

#[test]
fn handle_refresh_models_sync_ok() {
    // The connection thread asks the daemon for a refresh; the daemon
    // (via the maintenance thread) replies with a report, which the
    // connection routes to the client as ModelsRefreshed.
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::RefreshModels { force, reply }) = daemon_rx.recv() {
            assert!(force);
            let _ = reply.send(Ok(crate::catalog::RefreshReport {
                providers: 208,
                models: 1234,
                status: choreo_proto::RefreshStatus::Updated,
            }));
        }
    });
    let handle = ctx.reply_handle();
    handle_refresh_models_sync(&mut ctx, handle, true);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::ModelsRefreshed {
            providers: 208,
            models: 1234,
            status: choreo_proto::RefreshStatus::Updated,
        }
    ));
}

#[test]
fn handle_refresh_models_sync_err() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::RefreshModels { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Err("daemon is locked".into()));
        }
    });
    let handle = ctx.reply_handle();
    handle_refresh_models_sync(&mut ctx, handle, false);
    let msg = writer_rx.recv().unwrap();
    assert!(
        matches!(&msg.inner, DaemonMessageType::ModelsRefreshFailed { error } if error == "daemon is locked")
    );
}

// ── MCP status + reconnect handlers ──────────────────────────────

/// Build a `ClientCtx` over the given daemon sender plus a fresh writer
/// sink, returning the sink's receiver too. Shared by the MCP handler
/// tests so each one only supplies the fake daemon thread.
fn mcp_ctx<'a>(
    daemon_tx: &'a crossbeam_channel::Sender<DaemonCommand>,
    sink: &'a crate::broadcast::SubscriberSink,
    global_lag: &'a Arc<AtomicUsize>,
    attached_session_id: &'a mut Option<u64>,
    attached_session_tx: &'a mut Option<crossbeam_channel::Sender<SessionCommand>>,
) -> ClientCtx<'a> {
    ClientCtx {
        writer: sink,
        db: &TEST_DB,
        global_lag,
        daemon_tx,
        attached_session_id,
        attached_session_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    }
}

fn sample_mcp_report() -> crate::mcp::McpStatusReport {
    crate::mcp::McpStatusReport {
        servers: vec![sample_mcp_status()],
        project_root: None,
        project_trusted: false,
        ignored_project_servers: Vec::new(),
    }
}

fn sample_mcp_status() -> crate::mcp::McpServerStatus {
    crate::mcp::McpServerStatus {
        slug: "docs".to_string(),
        tier: "daemon".to_string(),
        transport: "stdio".to_string(),
        target: "npx docs-server".to_string(),
        connected: true,
        tool_count: 3,
        server_name: Some("docs".to_string()),
        server_version: Some("1.0.0".to_string()),
        last_error: None,
    }
}

#[test]
fn wire_mcp_status_moves_every_field() {
    let wire = wire_mcp_status(sample_mcp_status());
    assert_eq!(wire.slug, "docs");
    assert_eq!(wire.tier, "daemon");
    assert_eq!(wire.transport, "stdio");
    assert_eq!(wire.target, "npx docs-server");
    assert!(wire.connected);
    assert_eq!(wire.tool_count, 3);
    assert_eq!(wire.server_name.as_deref(), Some("docs"));
    assert_eq!(wire.server_version.as_deref(), Some("1.0.0"));
    assert!(wire.last_error.is_none());
}

#[test]
fn handle_mcp_status_sync_ok() {
    // The connection asks the daemon for the status list; the daemon
    // replies with its records, which the connection converts to the wire
    // type and routes to the client as McpStatus.
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = mcp_ctx(&daemon_tx, &sink, &global_lag, &mut none_id, &mut none_tx);
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::McpStatus { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(sample_mcp_report());
        }
    });
    let handle = ctx.reply_handle();
    handle_mcp_status_sync(&mut ctx, handle);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::McpStatus { servers, .. }
            if servers.len() == 1 && servers[0].slug == "docs" && servers[0].connected
    ));
}

#[test]
fn handle_mcp_reconnect_sync_ok_replies_refreshed_status() {
    // A successful reconnect triggers a SECOND daemon round-trip (the
    // status read) and replies with the refreshed McpStatus list.
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = mcp_ctx(&daemon_tx, &sink, &global_lag, &mut none_id, &mut none_tx);
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::McpReconnect { slug, reply }) = daemon_rx.recv() {
            assert_eq!(slug, "docs");
            let _ = reply.send(Ok(()));
        }
        if let Ok(DaemonCommand::McpStatus { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(sample_mcp_report());
        }
    });
    let handle = ctx.reply_handle();
    handle_mcp_reconnect_sync(&mut ctx, handle, "docs".to_string());
    let msg = writer_rx.recv().unwrap();
    assert!(
        matches!(&msg.inner, DaemonMessageType::McpStatus { servers, .. } if servers.len() == 1)
    );
}

#[test]
fn handle_mcp_reconnect_sync_err_replies_failure() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = mcp_ctx(&daemon_tx, &sink, &global_lag, &mut none_id, &mut none_tx);
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::McpReconnect { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Err("connect timed out".into()));
        }
    });
    let handle = ctx.reply_handle();
    handle_mcp_reconnect_sync(&mut ctx, handle, "docs".to_string());
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::McpReconnectFailed { slug, error }
            if slug == "docs" && error == "connect timed out"
    ));
}

#[test]
fn handle_mcp_reload_sync_ok_replies_summary_and_status() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = mcp_ctx(&daemon_tx, &sink, &global_lag, &mut none_id, &mut none_tx);
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::McpReload { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Ok(crate::mcp::McpReloadOutcome {
                summary: "MCP reload: 1 added, 0 removed, 0 restarted, 0 unchanged, 0 failed"
                    .to_string(),
                servers: vec![sample_mcp_status()],
                affected_sessions: Vec::new(),
            }));
        }
    });
    let handle = ctx.reply_handle();
    handle_mcp_reload_sync(&mut ctx, handle);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::McpReloaded { summary, servers }
            if summary.contains("1 added") && servers.len() == 1 && servers[0].slug == "docs"
    ));
}

#[test]
fn handle_mcp_reload_sync_err_replies_failure() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = mcp_ctx(&daemon_tx, &sink, &global_lag, &mut none_id, &mut none_tx);
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::McpReload { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Err("failed to parse mcp.json".into()));
        }
    });
    let handle = ctx.reply_handle();
    handle_mcp_reload_sync(&mut ctx, handle);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::McpReloadFailed { error }
            if error == "failed to parse mcp.json"
    ));
}

#[test]
fn handle_list_models_sync_err() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::ListModels { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Err("daemon is locked".into()));
        }
    });
    let handle = ctx.reply_handle();
    handle_list_models_sync(&mut ctx, handle, None);
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg.inner, DaemonMessageType::ModelsFailed { .. }));
    if let DaemonMessageType::ModelsFailed { error } = &msg.inner {
        assert_eq!(error, "daemon is locked");
    }
}

#[test]
fn handle_client_get_image_serves_attached_session() {
    // A client attached to session 5 requests image 1 of turn 2; the
    // connection thread reads the bytes straight from its own DB handle
    // (`ctx.db`) and routes them back as Image — no command-loop hop. The
    // bytes are written the way persist-at-emit does, via `write_turn`.
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut attached = Some(5u64);
    let mut none_tx = None;
    let ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    // Persist a turn whose displayed-image index 1 is [1, 2, 3] (slot d1).
    // Unique session/turn ids keep this write isolated from any other test
    // that shares the static TEST_DB.
    let turn = choreo_proto::Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![displayed_image(b""), displayed_image(&[1, 2, 3])],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    crate::db::write_turn(&TEST_DB, 5, 2, &turn).unwrap();

    let handle = ctx.reply_handle();
    handle_client_get_image(
        5,
        2,
        choreo_proto::ImageKey::Displayed { index: 1 },
        &ctx,
        handle,
    );
    let msg = writer_rx.recv().unwrap();
    match msg.inner {
        DaemonMessageType::Image {
            session_id,
            turn_id,
            key,
            data,
        } => {
            assert_eq!((session_id, turn_id), (5, 2));
            assert_eq!(key, choreo_proto::ImageKey::Displayed { index: 1 });
            assert_eq!(data, Some(vec![1, 2, 3]));
        }
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn handle_client_get_image_serves_a_tool_result_vision_image() {
    // The same handler serves a tool-result vision image addressed by
    // `ImageKey::ToolResult { call_id }` — one fetch protocol for both
    // attachment kinds.
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut attached = Some(6u64);
    let mut none_tx = None;
    let ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    // Persist a turn whose tool result `call_v` carries [9, 8, 7] bytes
    // (slot `rcall_v`).
    let turn = choreo_proto::Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![choreo_proto::ToolResultRecord {
            call_id: "call_v".into(),
            name: "read_image".into(),
            content: "image".into(),
            is_error: false,
            invocation_description: "read_image".into(),
            image: Some(choreo_proto::ImageReference {
                path: "/tmp/a.png".into(),
                mime_type: "image/png".into(),
                width: 1,
                height: 1,
                data: vec![9, 8, 7],
            }),
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    crate::db::write_turn(&TEST_DB, 6, 1, &turn).unwrap();

    let handle = ctx.reply_handle();
    handle_client_get_image(
        6,
        1,
        choreo_proto::ImageKey::ToolResult {
            call_id: "call_v".into(),
        },
        &ctx,
        handle,
    );
    let msg = writer_rx.recv().unwrap();
    match msg.inner {
        DaemonMessageType::Image { data, .. } => {
            assert_eq!(data, Some(vec![9, 8, 7]));
        }
        other => panic!("expected Image, got {other:?}"),
    }
}

/// Build a `DisplayedImageRecord` carrying `data`, for the `GetImage` test.
fn displayed_image(data: &[u8]) -> choreo_proto::DisplayedImageRecord {
    choreo_proto::DisplayedImageRecord {
        metadata: choreo_proto::ImageMetadata {
            mime_type: "image/png".into(),
            width: 1,
            height: 1,
            byte_len: data.len() as u64,
            alt: None,
        },
        data: data.to_vec(),
        tool_call_id: None,
    }
}

#[test]
fn handle_client_get_image_refuses_unattached_session() {
    // The client is NOT attached to the requested session: the DB must not
    // be consulted for the request (no command is sent to the daemon) and
    // the reply is a not-found None.
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut attached = Some(5u64);
    let mut none_tx = None;
    let ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    let handle = ctx.reply_handle();
    handle_client_get_image(
        99,
        0,
        choreo_proto::ImageKey::Displayed { index: 0 },
        &ctx,
        handle,
    );
    let msg = writer_rx.recv().unwrap();
    match msg.inner {
        DaemonMessageType::Image {
            session_id, data, ..
        } => {
            assert_eq!(session_id, 99);
            assert_eq!(data, None);
        }
        other => panic!("expected Image, got {other:?}"),
    }
    assert!(
        daemon_rx.try_recv().is_err(),
        "an unattached request must not reach the daemon"
    );
}

#[test]
fn handle_get_credential_sync_some() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::GetCredential { service, reply }) = daemon_rx.recv() {
            assert_eq!(service, "openai");
            let _ = reply.send(Some("sk-123".into()));
        }
    });
    let handle = ctx.reply_handle();
    handle_get_credential_sync(&mut ctx, handle, "openai".into());
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg.inner, DaemonMessageType::Credential { .. }));
    if let DaemonMessageType::Credential { service, key } = &msg.inner {
        assert_eq!(service, "openai");
        assert_eq!(key.as_deref(), Some("sk-123"));
    }
}

#[test]
fn handle_get_credential_sync_none() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::GetCredential { service, reply }) = daemon_rx.recv() {
            assert_eq!(service, "openai");
            let _ = reply.send(None);
        }
    });
    let handle = ctx.reply_handle();
    handle_get_credential_sync(&mut ctx, handle, "openai".into());
    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg.inner, DaemonMessageType::Credential { .. }));
    if let DaemonMessageType::Credential { service, key } = &msg.inner {
        assert_eq!(service, "openai");
        assert!(key.is_none());
    }
}

#[test]
fn switch_session_to_different_sends_detach_to_old() {
    let (old_tx, old_rx) = crossbeam_channel::unbounded();
    let (new_tx, new_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let (sink, _writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let mut attached_id = Some(1u64);
    let mut attached_tx = Some(old_tx);
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached_id,
        attached_session_tx: &mut attached_tx,
        client_id: 42,
        request_id: 0,
        is_unix: true,
    };

    switch_attached_session(2, new_tx, &mut ctx);

    // Detach sent to old session
    assert!(matches!(
        old_rx.try_recv().ok(),
        Some(SessionCommand::Detach { client_id: 42 })
    ));
    // Attach sent to new session
    assert!(matches!(
        new_rx.try_recv().ok(),
        Some(SessionCommand::Attach { client_id: 42, .. })
    ));
    // State updated to new session
    assert_eq!(attached_id, Some(2));
}

#[test]
fn switch_session_same_skips_detach() {
    let (old_tx, old_rx) = crossbeam_channel::unbounded();
    let (new_tx, new_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let (sink, _writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let mut attached_id = Some(1u64);
    let mut attached_tx = Some(old_tx);
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached_id,
        attached_session_tx: &mut attached_tx,
        client_id: 42,
        request_id: 0,
        is_unix: true,
    };

    switch_attached_session(1, new_tx, &mut ctx);

    // No Detach sent — same session id
    assert!(old_rx.try_recv().is_err());
    // Attach still sent (caller expects the subscription)
    assert!(matches!(
        new_rx.try_recv().ok(),
        Some(SessionCommand::Attach { client_id: 42, .. })
    ));
    // State stays at session 1
    assert_eq!(attached_id, Some(1));
}

#[test]
fn handle_delete_session_sync_success_sends_accepted() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::DeleteSession { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Ok(()));
        }
    });
    let handle = ctx.reply_handle();
    handle_delete_session_sync(&mut ctx, handle, 42);
    // A targeted `Accepted` is the request's terminal reply; the
    // `SessionDeleted` fan-out is a separate (broadcast) message.
    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(0), "the ack must carry the request id");
    assert!(matches!(
        msg.inner,
        DaemonMessageType::Accepted {
            kind: MessageKind::DeleteSession
        }
    ));
}

#[test]
fn handle_delete_session_sync_error_sends_session_delete_failed() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::DeleteSession { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Err(io::Error::other("db error")));
        }
    });
    let handle = ctx.reply_handle();
    handle_delete_session_sync(&mut ctx, handle, 42);
    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(0));
    // The failure reply is the session-scoped `SessionDeleteFailed` (with
    // the origin session preserved), still carrying the correlation id.
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::SessionDeleteFailed { error },
        } if error == "db error"
    ));
}

#[test]
fn handle_delete_session_sync_disconnected() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    drop(daemon_rx);
    let handle = ctx.reply_handle();
    handle_delete_session_sync(&mut ctx, handle, 42);
    assert!(writer_rx.try_recv().is_err());
}

#[test]
fn switch_session_from_none_no_detach() {
    let (new_tx, new_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let (sink, _writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let mut attached_id: Option<u64> = None;
    let mut attached_tx: Option<crossbeam_channel::Sender<SessionCommand>> = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached_id,
        attached_session_tx: &mut attached_tx,
        client_id: 42,
        request_id: 0,
        is_unix: true,
    };

    switch_attached_session(1, new_tx, &mut ctx);

    assert_eq!(attached_id, Some(1));
    assert!(matches!(
        new_rx.try_recv().ok(),
        Some(SessionCommand::Attach { client_id: 42, .. })
    ));
}

// ── Undo dispatch ────────────────────────────────────────────────────

#[test]
fn dispatch_undo_when_attached_sends_undo_command() {
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let (sink, _writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let (session_tx, session_rx) = crossbeam_channel::unbounded();
    let mut attached_id = Some(1u64);
    let mut attached_tx = Some(session_tx);
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached_id,
        attached_session_tx: &mut attached_tx,
        client_id: 42,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(ClientMessage::request(0, ClientMessageType::Undo), &mut ctx).unwrap();

    assert!(matches!(
        session_rx.try_recv().ok(),
        Some(SessionCommand::Undo { .. })
    ));
}

#[test]
fn dispatch_undo_when_not_attached_sends_failed() {
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(ClientMessage::request(0, ClientMessageType::Undo), &mut ctx).unwrap();

    // No session attached: the requester still gets its terminal `Failed`.
    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(0));
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::Failed {
            kind: MessageKind::Undo,
            error,
        } if error == "no session attached"
    ));
}

// ── Redo dispatch ────────────────────────────────────────────────────

#[test]
fn dispatch_redo_when_attached_sends_redo_command() {
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let (sink, _writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let (session_tx, session_rx) = crossbeam_channel::unbounded();
    let mut attached_id = Some(1u64);
    let mut attached_tx = Some(session_tx);
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached_id,
        attached_session_tx: &mut attached_tx,
        client_id: 42,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(ClientMessage::request(0, ClientMessageType::Redo), &mut ctx).unwrap();

    assert!(matches!(
        session_rx.try_recv().ok(),
        Some(SessionCommand::Redo { .. })
    ));
}

#[test]
fn dispatch_redo_when_not_attached_sends_failed() {
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(ClientMessage::request(0, ClientMessageType::Redo), &mut ctx).unwrap();

    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(0));
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::Failed {
            kind: MessageKind::Redo,
            error,
        } if error == "no session attached"
    ));
}

// ── ContinueGeneration dispatch ──────────────────────────────────────

#[test]
fn dispatch_continue_generation_when_attached_sends_run_input() {
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let (sink, _writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let (session_tx, session_rx) = crossbeam_channel::unbounded();
    let mut attached_id = Some(1u64);
    let mut attached_tx = Some(session_tx);
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut attached_id,
        attached_session_tx: &mut attached_tx,
        client_id: 42,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(
        ClientMessage::request(0, ClientMessageType::ContinueGeneration),
        &mut ctx,
    )
    .unwrap();

    let cmd = session_rx.try_recv().expect("should receive RunInput");
    assert!(matches!(
        &cmd,
        SessionCommand::RunInput { input, .. } if input == b"Continue."
    ));
}

#[test]
fn dispatch_continue_generation_when_not_attached_sends_failed() {
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(
        ClientMessage::request(0, ClientMessageType::ContinueGeneration),
        &mut ctx,
    )
    .unwrap();

    let msg = writer_rx.recv().expect("should receive Failed");
    assert_eq!(msg.id, Some(0));
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::Failed {
            kind: MessageKind::ContinueGeneration,
            error,
        } if error == "no session attached"
    ));
}

// ── No-arg acks (Cancel / subscribe / unsubscribe) ───────────────

#[test]
fn dispatch_subscribe_all_activity_acks_accepted() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(
        ClientMessage::request(4, ClientMessageType::SubscribeAllActivity),
        &mut ctx,
    )
    .unwrap();

    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(4), "the ack must carry the request id");
    assert!(matches!(
        msg.inner,
        DaemonMessageType::Accepted {
            kind: MessageKind::SubscribeAllActivity
        }
    ));
    // The registration command still reaches the daemon command loop.
    assert!(matches!(
        daemon_rx.try_recv().ok(),
        Some(DaemonCommand::RegisterActivitySubscriber { .. })
    ));
}

#[test]
fn dispatch_unsubscribe_all_activity_acks_accepted() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(
        ClientMessage::request(5, ClientMessageType::UnsubscribeAllActivity),
        &mut ctx,
    )
    .unwrap();

    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(5));
    assert!(matches!(
        msg.inner,
        DaemonMessageType::Accepted {
            kind: MessageKind::UnsubscribeAllActivity
        }
    ));
    assert!(matches!(
        daemon_rx.try_recv().ok(),
        Some(DaemonCommand::UnregisterActivitySubscriber { .. })
    ));
}

#[test]
fn dispatch_cancel_acks_accepted() {
    // Cancelling with no session attached still acks: the request was
    // received, and the stream's own `Cancelled` broadcast is the outcome.
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };

    dispatch_client_message(
        ClientMessage::request(6, ClientMessageType::Cancel { stream_id: 1 }),
        &mut ctx,
    )
    .unwrap();

    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(6));
    assert!(matches!(
        msg.inner,
        DaemonMessageType::Accepted {
            kind: MessageKind::Cancel
        }
    ));
    // No session attached: no cancel command is routed.
    assert!(daemon_rx.try_recv().is_err());
}

#[test]
fn dispatch_set_session_pinned_success_acks_accepted() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::SetSessionFlags { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Ok(()));
        }
    });

    dispatch_client_message(
        ClientMessage::request(
            7,
            ClientMessageType::SetSessionPinned {
                session_id: 1,
                pinned: true,
            },
        ),
        &mut ctx,
    )
    .unwrap();

    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(7));
    assert!(matches!(
        msg.inner,
        DaemonMessageType::Accepted {
            kind: MessageKind::SetSessionPinned
        }
    ));
}

#[test]
fn dispatch_set_session_pinned_failure_sends_session_failed() {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (sink, writer_rx) = test_sink();
    let global_lag = Arc::new(AtomicUsize::new(0));
    let mut none_id = None;
    let mut none_tx = None;
    let mut ctx = ClientCtx {
        writer: &sink,
        db: &TEST_DB,
        global_lag: &global_lag,
        daemon_tx: &daemon_tx,
        attached_session_id: &mut none_id,
        attached_session_tx: &mut none_tx,
        client_id: 0,
        request_id: 0,
        is_unix: true,
    };
    std::thread::spawn(move || {
        if let Ok(DaemonCommand::SetSessionFlags { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(Err(io::Error::other("db error")));
        }
    });

    dispatch_client_message(
        ClientMessage::request(
            8,
            ClientMessageType::SetSessionPinned {
                session_id: 1,
                pinned: true,
            },
        ),
        &mut ctx,
    )
    .unwrap();

    // The failure reply is the session-scoped `SessionFailed` (which every
    // front-end renders) carrying the operation label, still correlated by
    // the request id.
    let msg = writer_rx.recv().unwrap();
    assert_eq!(msg.id, Some(8));
    assert!(matches!(
        &msg.inner,
        DaemonMessageType::Session {
            session_id: Some(1),
            event: SessionEvent::SessionFailed { operation, error },
        } if operation == "set_session_pinned" && error == "db error"
    ));
}

// ── ChannelConnectionWriter (embedded transport) ─────────────────────

/// A message sent while the writer is open arrives as a VALUE on the
/// receiver, and dropping the last sender (`writer_thread`'s post-loop
/// path) closes the receiver — the channel analogue of socket EOF.
#[test]
fn channel_writer_forwards_values_and_receiver_sees_close() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let mut writer = ChannelConnectionWriter::new(tx);
    writer
        .send_message(&DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();
    // Dropping the writer drops the sender: the receiver sees the value,
    // then a disconnect (Err) — without any shutdown call, mirroring the
    // writer_thread exit path.
    drop(writer);
    assert!(matches!(rx.recv(), Ok(m) if m.inner == DaemonMessageType::Pong));
    assert!(
        rx.recv().is_err(),
        "dropping the sender must close the receiver"
    );
}

/// After `shutdown()` the sender is gone, so any subsequent send is an
/// error and the receiver is closed immediately — the same
/// notify-before-close contract the socket writer provides via
/// `Shutdown::Both`.
#[test]
fn channel_writer_send_after_shutdown_errors() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let mut writer = ChannelConnectionWriter::new(tx);
    writer.shutdown();
    assert!(
        writer
            .send_message(&DaemonMessage::broadcast(DaemonMessageType::Pong))
            .is_err(),
        "sending after shutdown must error (the writer thread never does this on the \
             graceful path, but the contract must hold)"
    );
    assert!(rx.recv().is_err(), "shutdown must close the receiver");
}

/// Through the shared (generic) `writer_thread`: `ShuttingDown` is
/// delivered to the embedded receiver as a value FIRST, then the writer
/// shuts the channel down, so the GUI observes the notification before
/// the channel close — notify-before-close, no bytes involved.
#[test]
fn writer_thread_delivers_shutting_down_before_channel_close() {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let (out_tx, out_rx) = crossbeam_channel::unbounded();
    let bytes = Arc::new(AtomicUsize::new(0));
    let global = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn({
        let bytes = Arc::clone(&bytes);
        let global = Arc::clone(&global);
        move || writer_thread(ChannelConnectionWriter::new(out_tx), &rx, &bytes, &global)
    });

    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();
    tx.send(DaemonMessage::broadcast(DaemonMessageType::ShuttingDown))
        .unwrap();
    // Queued after the notification: must never reach the GUI.
    tx.send(DaemonMessage::broadcast(DaemonMessageType::Pong))
        .unwrap();

    handle.join().expect("writer thread panicked");
    assert!(matches!(out_rx.recv(), Ok(m) if m.inner == DaemonMessageType::Pong));
    assert!(
        matches!(out_rx.recv(), Ok(m) if m.inner == DaemonMessageType::ShuttingDown),
        "ShuttingDown must be delivered BEFORE the channel closes"
    );
    assert!(
        out_rx.recv().is_err(),
        "after the notification the channel must be closed (recv errors)"
    );
}
