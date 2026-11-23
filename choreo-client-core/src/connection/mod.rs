//! Daemon-connection entry points: dial, handshake, and serve.
//!
//! Every `run_daemon_*` function takes the UI's outgoing-message channel plus
//! an optional shutdown channel, and blocks on a reader loop until the daemon
//! closes the stream — pairing a writer thread that drains the UI channel
//! (biased, so queued messages flush before a stop) with the reader thread
//! that forwards each decoded message to the caller's handler. The variants
//! split by transport ([`ConnectionMode`]): Unix socket, TCP/Noise IK (pinned
//! or explicit key), TCP/Noise XX first contact, and an in-process channel
//! pair to an embedded daemon. The preflight helpers ([`probe_server_key`],
//! [`verify_daemon_authorization`], [`own_transport_pubkey`]) let a front-end
//! establish trust and enrollment before committing to a session.
//!
//! The implementation splits by concern: `unix` is the local-domain socket
//! path, `tcp` the Noise-over-TCP variants and the preflight helpers,
//! `handshake` the shared Noise handshake plumbing and the established-stream
//! service loop both TCP modes use, and `inproc` the embedded in-process
//! channel transport. This module owns the transport-selecting type and the
//! [`run_daemon_connection_with_mode`] dispatch that ties them together.

mod handshake;
mod inproc;
mod tcp;
mod unix;

pub use tcp::{
    PreflightError, own_transport_pubkey, probe_server_key, run_daemon_tcp_connection,
    run_daemon_tcp_connection_pinned, run_daemon_tcp_connection_xx_first_contact,
    verify_daemon_authorization,
};
pub use unix::{run_daemon_connection, run_daemon_connection_with_autostart, run_daemon_reader};

use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage};
// In-process transport: raw channel ends from an `choreo_daemon::embedded::EmbeddedLink`.
// Pure values — client-core never depends on choreo-daemon; the GUI creates
// the link and stuffs the ends into `ConnectionMode::InProcess`.
use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use std::fmt;

/// Selects the transport for connecting to a daemon.
///
// Clone stays derived: crossbeam channel ends are `Clone`, so a cloned mode
// shares the same underlying channel pair (fine — cloning a mode is only used
// to stash it for `connection_addr`-style display lookups).
#[derive(Clone)]
pub enum ConnectionMode {
    /// Connect via Unix domain socket at the given path.
    UnixSocket(String),
    /// Connect via TCP/Noise IK at the given address with the server's
    /// 32-byte X25519 public key (resolved before constructing this variant).
    Tcp {
        /// The `host:port` to dial.
        addr: String,
        /// The server's 32-byte X25519 static public key, which the Noise IK
        /// handshake authenticates.
        server_pk: [u8; 32],
    },
    /// Connect via TCP/Noise IK against the key PINNED in
    /// `known_servers.toml` for the address. The pin is loaded at connect
    /// time; a handshake failure is reported WITH the pinned public key and
    /// the re-pair guidance, so a server key change is loud instead of an
    /// opaque connection error (the `known_hosts` behavior).
    TcpPinned(String),
    /// Connect to an EMBEDDED daemon in the same process: `daemon_tx` is the
    /// client→daemon channel end, `daemon_rx` the daemon→client end (the raw
    /// pair from `choreo_daemon::embedded::EmbeddedLink`, moved here as
    /// values). Messages travel as VALUES — no codec, no socket. Channel
    /// close IS the EOF in both directions, exactly like the daemon-side
    /// embedded path. client-core never links choreo-daemon: the GUI (the
    /// owner of the `EmbeddedDaemon`) creates the link and hands over the ends.
    InProcess {
        /// The client→daemon channel end: the writer forwards UI messages
        /// here.
        daemon_tx: CrossbeamSender<ClientMessage>,
        /// The daemon→client channel end the reader loop drains until the
        /// daemon drops its sender.
        daemon_rx: CrossbeamReceiver<DaemonMessage>,
    },
}

// Manual Debug instead of a derive: crossbeam channel ends are not `Debug`,
// and the embedded link carries no printable identity — it is rendered as a
// placeholder. Every other variant formats EXACTLY as the derive output would
// (`UnixSocket("path")`, `Tcp { addr: "…", server_pk: [..] }`,
// `TcpPinned("addr")`) so log/error messages keep their previous shape.
impl fmt::Debug for ConnectionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionMode::UnixSocket(path) => f.debug_tuple("UnixSocket").field(path).finish(),
            ConnectionMode::Tcp { addr, server_pk } => f
                .debug_struct("Tcp")
                .field("addr", addr)
                .field("server_pk", server_pk)
                .finish(),
            ConnectionMode::TcpPinned(addr) => f.debug_tuple("TcpPinned").field(addr).finish(),
            // The ends are opaque by design: printing channel internals would
            // leak nothing useful and might suggest a stable identity that
            // does not exist.
            ConnectionMode::InProcess { .. } => f.write_str("InProcess(<embedded link>)"),
        }
    }
}

impl Default for ConnectionMode {
    fn default() -> Self {
        ConnectionMode::UnixSocket(choreo_proto::socket_path())
    }
}

/// Connect to a daemon using the given connection mode.
/// Dispatches to the appropriate connection function.
///
/// # Errors
///
/// Returns [`ClientError`] as raised by the connection mode actually used
/// (unix, TCP variants, first-contact preflight, or in-process).
pub fn run_daemon_connection_with_mode(
    mode: ConnectionMode,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    match mode {
        ConnectionMode::UnixSocket(path) => {
            run_daemon_connection(&path, handle_daemon_message, from_ui, shutdown_rx)
        }
        ConnectionMode::Tcp { addr, server_pk } => run_daemon_tcp_connection(
            &addr,
            &server_pk,
            handle_daemon_message,
            from_ui,
            shutdown_rx,
        ),
        ConnectionMode::TcpPinned(addr) => {
            run_daemon_tcp_connection_pinned(&addr, handle_daemon_message, from_ui, shutdown_rx)
        }
        ConnectionMode::InProcess {
            daemon_tx,
            daemon_rx,
        } => {
            inproc::run_daemon_connection_in_process(
                daemon_tx,
                daemon_rx,
                handle_daemon_message,
                from_ui,
                shutdown_rx,
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::ClientMessageType;
    use choreo_proto::DaemonMessageType;
    use choreo_proto::SessionSummary;
    use std::thread;

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
                    ClientMessageType::Ping => {
                        DaemonMessage::reply(msg.id, DaemonMessageType::Pong)
                    }
                    ClientMessageType::ListModels { .. } => DaemonMessage::reply(
                        msg.id,
                        DaemonMessageType::Models {
                            models: vec![choreo_proto::ModelInfo::plain("m1")],
                            selected_model: None,
                            suggested_model: Some("m1".to_string()),
                            provisional: false,
                        },
                    ),
                    ClientMessageType::Lock => {
                        DaemonMessage::reply(msg.id, DaemonMessageType::Locked)
                    }
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

    /// The autostart hook must fire ONLY when the dial finds nothing
    /// listening (`NotFound` here: no socket file at all). The hook deliberately
    /// fails, so a successful run would be impossible — the pinned outcome is
    /// exactly one hook invocation and the hook's error surfacing as the
    /// connection result. (The live-listener half of this contract binds real
    /// sockets, so it lives in `tests/it/connection_autostart.rs` — no filesystem
    /// or IPC boundary in unit tests.)
    #[test]
    fn autostart_hook_invoked_when_nothing_listens() {
        let path = std::env::temp_dir().join(format!(
            "choreo-core-autostart-absent-{}-{}",
            std::process::id(),
            format!("{:?}", std::thread::current().id()).as_str()
        ));
        let _ = std::fs::remove_file(&path); // absent by construction
        let path = path.to_string_lossy().into_owned();

        let mut hook_calls = 0;
        let mut ensure_daemon = || {
            hook_calls += 1;
            Err(ClientError::DaemonStart("test: no daemon".to_string()))
        };
        let (from_ui_tx, from_ui_rx) = crossbeam_channel::unbounded::<ClientMessage>();
        drop(from_ui_tx); // the pump's writer thread ends immediately

        let error = run_daemon_connection_with_autostart(
            &path,
            &mut ensure_daemon,
            |_| {},
            from_ui_rx,
            None,
        )
        .expect_err("the failing hook must fail the connection");

        assert!(matches!(error, ClientError::DaemonStart(_)));
        assert_eq!(hook_calls, 1, "the hook must run exactly once");
        let _ = std::fs::remove_file(&path);
    }
}
