//! Tests for the connection dispatch and the in-process transport: reply
//! ordering, writer drain semantics, shutdown on a dropped signal sender, the
//! manual `Debug` shapes, the handshake failure classification, and the writer's
//! message budget.
//!
//! Everything here is channel- and join-driven — no sleeps, no sockets, no
//! filesystem. The autostart hook's filesystem half and the live-socket contracts
//! live in `tests/it/`.

use super::handshake::{handshake_error, is_handshake_refusal};
use super::serve::{
    WRITER_MAX_BUDGET, WRITER_MIN_RATE_BYTES_PER_SEC, WRITER_STALL, message_budget,
};
use super::*;
use choreo_proto::ClientMessageType;
use choreo_proto::DaemonMessageType;
use choreo_proto::SessionSummary;
use choreo_transport::error::TransportError;
use std::io;
use std::thread;
use std::time::Duration;

/// Bare in-process link — two crossbeam channels, NO real daemon. The
/// fake-daemon threads below play the daemon side of an `EmbeddedLink`:
/// they receive forwarded `ClientMessage`s from `client_rx`, reply
/// through their `daemon_tx` end (the daemon→client channel), and
/// DROPPING that end is the EOF the reader observes — exactly how the
/// real embedded daemon signals disconnect.
fn make_link() -> (
    ConnectionMode,
    CrossbeamSender<ClientMessage>,
    CrossbeamReceiver<ClientMessage>,
    crossbeam_channel::Receiver<ClientMessage>,
    crossbeam_channel::Sender<DaemonMessage>,
) {
    let (client_tx, client_rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let (from_ui_tx, from_ui_rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let mode = ConnectionMode::InProcess {
        daemon_tx: client_tx,
        daemon_rx,
    };
    (mode, from_ui_tx, from_ui_rx, client_rx, daemon_tx)
}

/// Run the connection on a dedicated thread (mirroring production: the
/// pump's READER is its calling thread) and return its result plus a
/// receiver of every `DaemonMessage` it handled, in order.
fn spawn_connection(
    mode: ConnectionMode,
    from_ui: CrossbeamReceiver<ClientMessage>,
    handle: impl FnMut(DaemonMessage) + Send + 'static,
) -> thread::JoinHandle<(
    Result<(), ClientError>,
    crossbeam_channel::Receiver<DaemonMessage>,
)> {
    // Bounded crossbeam channel between the handler closure and the test:
    // join-driven, no sleeps anywhere. Capacity is generous enough that
    // the reader never blocks on it before the daemon closes its end.
    let (seen_tx, seen_rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    thread::spawn(move || {
        let mut handle = handle;
        let result = run_daemon_connection_with_mode(
            mode,
            |message| {
                let _ = seen_tx.send(message.clone());
                handle(message);
            },
            from_ui,
            None,
        );
        (result, seen_rx)
    })
}

/// One reply per request, in order: the fake daemon echoes a distinct
/// `DaemonMessage` per `ClientMessage` variant, then closes its channel
/// (EOF). The reader must receive every reply, in order, and end cleanly.
#[test]
fn in_process_replies_arrive_in_order_and_close_cleanly() {
    let (mode, from_ui_tx, from_ui_rx, client_rx, daemon_tx) = make_link();
    thread::spawn(move || {
        for msg in client_rx {
            let reply = match &msg.inner {
                ClientMessageType::Ping => DaemonMessage::reply(msg.id, DaemonMessageType::Pong),
                ClientMessageType::ListModels { .. } => DaemonMessage::reply(
                    msg.id,
                    DaemonMessageType::Models {
                        models: vec![choreo_proto::ModelInfo::plain("m1")],
                        selected_model: None,
                        suggested_model: Some("m1".to_string()),
                        provisional: false,
                    },
                ),
                ClientMessageType::Lock => DaemonMessage::reply(msg.id, DaemonMessageType::Locked),
                _ => continue,
            };
            if daemon_tx.send(reply).is_err() {
                break;
            }
        }
        // Dropping `daemon_tx` (and the writer's client-side end) is the
        // daemon-side EOF.
    });

    let requests = [
        ClientMessage::request(0, ClientMessageType::Ping),
        ClientMessage::request(1, ClientMessageType::ListModels { account: None }),
        ClientMessage::request(2, ClientMessageType::Lock),
    ];
    for request in requests {
        from_ui_tx.send(request).expect("from_ui open");
    }
    // Close `from_ui`: the writer drains everything, then exits, which
    // closes the client→daemon channel and lets the daemon-side thread
    // end and close the daemon→client channel.
    drop(from_ui_tx);

    let (result, seen_rx) = spawn_connection(mode, from_ui_rx, |_| {})
        .join()
        .expect("join");
    result.expect("in-process connection must end cleanly on channel close");

    // Every reply arrived, in order, before the clean EOF. Each outbound
    // frame carried its front-end-allocated id (0, 1, 2), and the fake
    // daemon echoed it back.
    let replies: Vec<DaemonMessage> = seen_rx.into_iter().collect();
    assert_eq!(
        replies,
        vec![
            DaemonMessage::reply(0, DaemonMessageType::Pong),
            DaemonMessage::reply(
                1,
                DaemonMessageType::Models {
                    models: vec![choreo_proto::ModelInfo::plain("m1")],
                    selected_model: None,
                    suggested_model: Some("m1".to_string()),
                    provisional: false,
                },
            ),
            DaemonMessage::reply(2, DaemonMessageType::Locked),
        ]
    );
}

/// The writer forwards EVERY `from_ui` message — verified from the
/// daemon side: the fake daemon counts what it receives and reports the
/// sequence back as `Sessions` replies before closing.
#[test]
fn in_process_writer_forwards_every_message() {
    let (mode, from_ui_tx, from_ui_rx, client_rx, daemon_tx) = make_link();
    thread::spawn(move || {
        let mut pings = 0usize;
        for msg in client_rx {
            if matches!(msg.inner, ClientMessageType::Ping) {
                pings += 1;
                let _ = daemon_tx.send(DaemonMessage::reply(
                    msg.id,
                    DaemonMessageType::Sessions {
                        sessions: vec![SessionSummary {
                            session_id: pings as u64,
                            title: None,
                            selected_model: None,
                            parent_session_id: None,
                            working_dir: None,
                            created_at: 0,
                            last_modified: 0,
                            turn_count: 0,
                            status: choreo_proto::SessionStatus::Inactive,
                            active_tool_groups: vec![],
                            account_name: None,
                            reasoning_effort: None,
                            token_usage: None,
                            context_window: None,
                            last_prompt_tokens: None,
                            pinned: false,
                            archived_at: None,
                        }],
                    },
                ));
            }
        }
    });

    // Multiple messages through the writer while it is alive.
    for id in 0u64..5 {
        from_ui_tx
            .send(ClientMessage::request(id, ClientMessageType::Ping))
            .expect("from_ui open");
    }
    drop(from_ui_tx);

    let (result, seen_rx) = spawn_connection(mode, from_ui_rx, |_| {})
        .join()
        .expect("join");
    result.expect("clean end after from_ui close");
    let replies: Vec<u64> = seen_rx
        .into_iter()
        .map(|m| match m.inner {
            DaemonMessageType::Sessions { sessions } => sessions[0].session_id,
            other => panic!("unexpected message: {other:?}"),
        })
        .collect();
    assert_eq!(
        replies,
        vec![1, 2, 3, 4, 5],
        "every forwarded Ping must be answered, in order"
    );
}

/// Daemon-side close ends the connection cleanly even while the UI is
/// still open: the reader ends on `daemon_rx` exhaustion (the EOF), the
/// writer is stopped through the internal shutdown channel and joined.
#[test]
fn in_process_daemon_close_ends_connection_while_ui_open() {
    let (mode, from_ui_tx, from_ui_rx, client_rx, daemon_tx) = make_link();
    thread::spawn(move || {
        // One advisory message, then the daemon drops its end: the
        // channel-close EOF. `client_rx` is dropped un-received — the
        // daemon may vanish while client messages are still in flight.
        let _ = client_rx;
        let _ = daemon_tx.send(DaemonMessage::broadcast(DaemonMessageType::ShuttingDown));
        drop(daemon_tx);
    });

    // The UI sender stays OPEN for the whole test: the connection must
    // still end because the daemon side is gone.
    from_ui_tx
        .send(ClientMessage::request(0, ClientMessageType::Ping))
        .expect("from_ui open");

    let (result, seen_rx) = spawn_connection(mode, from_ui_rx, |_| {})
        .join()
        .expect("join");
    result.expect("daemon-side close must be a clean EOF");
    let replies: Vec<DaemonMessage> = seen_rx.into_iter().collect();
    assert_eq!(
        replies,
        vec![DaemonMessage::broadcast(DaemonMessageType::ShuttingDown)]
    );
    // The pump must NOT have closed `from_ui` itself (it only ever
    // drains it): a late send is delivered into the channel — it simply
    // has no consumer. It must not panic or error.
    let _ = from_ui_tx.send(ClientMessage::request(0, ClientMessageType::Ping));
}

/// The optional external shutdown channel stops the writer even when its
/// sender is DROPPED without sending — the same "a disconnect means shut
/// down" contract the socket modes honor (a dropped sender there still
/// force-closes the stream). The writer owns the client→daemon sender, so
/// its exit closes the daemon-side receiver: that disconnect is the
/// deterministic signal (no timing) that the writer stopped, and it can
/// only have come from the external shutdown because `from_ui` stays open
/// for the whole test.
#[test]
fn in_process_external_shutdown_drop_stops_writer() {
    let (mode, from_ui_tx, from_ui_rx, client_rx, daemon_tx) = make_link();
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);

    let handle = thread::spawn(move || {
        let _ = run_daemon_connection_with_mode(mode, |_| {}, from_ui_rx, Some(shutdown_rx));
    });

    // Drop WITHOUT sending: the external-shutdown thread must still fire
    // and stop the writer.
    drop(shutdown_tx);

    // The writer's exit drops the client→daemon sender, so the fake
    // daemon's receiver disconnects deterministically.
    assert!(
        client_rx.recv().is_err(),
        "a dropped external-shutdown sender must still stop the writer"
    );

    // Close the daemon→client channel to end the reader, then join. Keep
    // `from_ui_tx` alive until now to prove the writer stopped on the
    // external shutdown, not on a `from_ui` close.
    drop(daemon_tx);
    handle.join().expect("in-process pump thread joins");
    drop(from_ui_tx);
}

/// The manual `Debug` impl must keep the derived shapes for the socket
/// variants (log/error messages rely on them) and render the embedded
/// link as an opaque placeholder.
#[test]
fn debug_output_matches_previous_derive_shapes() {
    assert_eq!(
        format!("{:?}", ConnectionMode::UnixSocket("/tmp/sock".to_string())),
        r#"UnixSocket("/tmp/sock")"#
    );
    let mut pk = [0u8; 32];
    pk[0] = 1;
    pk[31] = 255;
    assert_eq!(
        format!(
            "{:?}",
            ConnectionMode::Tcp {
                addr: "127.0.0.1:9443".to_string(),
                server_pk: pk
            }
        ),
        format!("Tcp {{ addr: \"127.0.0.1:9443\", server_pk: {:?} }}", pk)
    );
    assert_eq!(
        format!("{:?}", ConnectionMode::TcpPinned("a:1".to_string())),
        r#"TcpPinned("a:1")"#
    );
    let (mode, _from_ui_tx, _from_ui_rx, _client_rx, daemon_tx) = make_link();
    assert_eq!(format!("{mode:?}"), "InProcess(<embedded link>)");
    // The ends themselves are opaque and dropped with the mode.
    drop(daemon_tx);
}

/// Every shape a handshake REFUSAL takes — including the EOF an initiator sees
/// when the responder aborts the exchange — maps to `ConnectionRefused` and
/// classifies as a refusal.
#[test]
fn handshake_refusals_classify_as_refused() {
    let refusals = [
        TransportError::AuthFailed,
        TransportError::Noise(snow::Error::Decrypt),
        TransportError::HandshakeModeMismatch {
            expected: 1,
            got: 2,
        },
        TransportError::HandshakeModeUnknown { byte: 0 },
        TransportError::InvalidFragment("oversized fragment".to_string()),
        // The initiator's refusal signal: a clean close and an abortive one.
        TransportError::Io(io::Error::from(io::ErrorKind::UnexpectedEof)),
        TransportError::Io(io::Error::from(io::ErrorKind::ConnectionReset)),
    ];

    for error in refusals {
        assert!(
            is_handshake_refusal(&error),
            "{error:?} must classify as a handshake refusal"
        );
        match handshake_error(error) {
            ClientError::Io(io) => assert_eq!(
                io.kind(),
                io::ErrorKind::ConnectionRefused,
                "a refusal must keep the ConnectionRefused shape"
            ),
            other => panic!("a refusal must map to Io(ConnectionRefused), got {other:?}"),
        }
    }
}

/// A handshake that failed WITHOUT a refusal keeps its own meaning, so the
/// callers' remediation advice never blames enrollment or the pin for a network
/// problem.
#[test]
fn handshake_non_refusals_keep_their_own_meaning() {
    let timeout = TransportError::HandshakeTimeout;
    assert!(!is_handshake_refusal(&timeout));
    match handshake_error(timeout) {
        ClientError::Io(io) => assert_eq!(io.kind(), io::ErrorKind::TimedOut),
        other => panic!("a handshake timeout must map to Io(TimedOut), got {other:?}"),
    }

    let broken = TransportError::Io(io::Error::from(io::ErrorKind::BrokenPipe));
    assert!(!is_handshake_refusal(&broken));
    match handshake_error(broken) {
        ClientError::Io(io) => assert_eq!(io.kind(), io::ErrorKind::BrokenPipe),
        other => panic!("a socket failure must pass through, got {other:?}"),
    }

    let closed = TransportError::ConnectionClosed;
    assert!(!is_handshake_refusal(&closed));
    match handshake_error(closed) {
        ClientError::Io(io) => assert_eq!(io.kind(), io::ErrorKind::ConnectionAborted),
        other => panic!("a peer close must map to Io(ConnectionAborted), got {other:?}"),
    }

    let protocol = TransportError::Protocol(choreo_proto::ProtoError::FrameTooLarge);
    assert!(!is_handshake_refusal(&protocol));
    assert!(
        matches!(handshake_error(protocol), ClientError::Proto(_)),
        "a codec error must keep ClientError::Proto"
    );

    // A missing XX static key is a protocol-contract violation, not a refusal:
    // it must never be reported as enrollment remediation.
    let missing_static = TransportError::HandshakeStaticMissing;
    assert!(!is_handshake_refusal(&missing_static));
    match handshake_error(missing_static) {
        ClientError::Io(io) => assert_eq!(io.kind(), io::ErrorKind::Other),
        other => panic!("a protocol violation must keep Io(..), got {other:?}"),
    }

    // A concurrent-access refusal is a caller bug, not a wire refusal either.
    let concurrent = TransportError::ConcurrentAccess {
        what: "send_message",
    };
    assert!(!is_handshake_refusal(&concurrent));

    // A local key-file failure is neither a refusal nor a network problem.
    let local = TransportError::ConfigDirNotFound;
    assert!(!is_handshake_refusal(&local));
    match handshake_error(local) {
        ClientError::Io(io) => {
            assert_eq!(io.kind(), io::ErrorKind::Other);
            assert!(
                io.to_string().contains("config directory"),
                "the cause must survive the wrap: {io}"
            );
        }
        other => panic!("a local failure must keep Io(..), got {other:?}"),
    }
}

/// A small frame cannot finish in less than the stall, so its budget is exactly
/// [`WRITER_STALL`].
#[test]
fn a_small_frame_budgets_exactly_the_writer_stall() {
    assert_eq!(message_budget(64), WRITER_STALL);
    assert_eq!(message_budget(0), WRITER_STALL);
    // 20 KiB at the floor rate is 2.5 s — still below the stall.
    assert_eq!(message_budget(20 * 1024), WRITER_STALL);
}

/// Above the boundary where the rate-derived duration overtakes the stall, the
/// budget is the rate-derived one: a big frame on a slow link is allowed the
/// time its size implies.
#[test]
fn a_frame_past_the_rate_boundary_gets_the_rate_derived_budget() {
    // 48 KiB at 8 KiB/s is 6 s, past the 5 s stall.
    let wire_len = 48 * 1024;
    let expected = Duration::from_millis(
        u64::try_from(wire_len).expect("fits u64") * 1000 / WRITER_MIN_RATE_BYTES_PER_SEC,
    );
    assert!(expected > WRITER_STALL, "the boundary case must exceed it");
    assert_eq!(message_budget(wire_len), expected);
}

/// A 64 MiB frame — the codec's maximum — must not overflow the millisecond
/// computation, and its rate-derived budget (~8192 s) is clamped to the
/// [`WRITER_MAX_BUDGET`] ceiling so it cannot stall the connection's teardown.
#[test]
fn a_64_mib_frame_is_clamped_to_the_max_budget() {
    let wire_len = 64 * 1024 * 1024;
    assert_eq!(message_budget(wire_len), WRITER_MAX_BUDGET);
    assert!(message_budget(wire_len) > WRITER_STALL);
}
