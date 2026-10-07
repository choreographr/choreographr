//! Daemon socket bridge: the read/write threads that speak the wire protocol on
//! one side and expose a platform-agnostic [`BridgeEvent`] stream on the other.
//!
//! [`DaemonBridge::spawn`] takes the connected socket halves, spawns a writer
//! thread (channel → socket) and a reader thread (socket → events, via
//! `choreo_client_core::run_daemon_reader`), and hands back
//! [`into_parts`](DaemonBridge::into_parts) so a platform layer (Telegram) can
//! drive both directions from its own threads. The module is deliberately
//! platform-agnostic: it folds daemon wire messages into [`BridgeEvent`]s and
//! knows nothing about how they are rendered.

use choreo_proto::{
    ClientMessage, ClientMessageType, DaemonMessage, DaemonMessageType, ImageKey, OutputStream,
    SessionEvent, SessionSummary, Turn, write_message,
};
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use tracing::{debug, error, info, warn};

use crossbeam_channel::{Receiver, Sender};
// Windows: std::os::windows::net::UnixStream is unstable (E0658, feature
// `windows_unix_domain_sockets`, rust-lang/rust#150487), so uds_windows provides
// the same connect/try_clone/shutdown API over named pipes.
#[cfg(windows)]
use uds_windows::UnixStream;

/// Local stand-in for the removed `StreamingText`. Accumulates reasoning and
/// answer chunks emitted during a request and flattens them into a single
/// text body on `Done`.
struct StreamBuffer {
    reasoning: String,
    answer: String,
}

impl StreamBuffer {
    fn new() -> Self {
        Self {
            reasoning: String::new(),
            answer: String::new(),
        }
    }

    fn append(&mut self, stream: &OutputStream, data: &str) {
        match stream {
            OutputStream::Reasoning => self.reasoning.push_str(data),
            OutputStream::Answer => self.answer.push_str(data),
            _ => {}
        }
    }

    /// Flatten reasoning + answer into a single trimmed string.
    fn flatten(&self) -> String {
        let mut text = String::new();
        if !self.reasoning.is_empty() {
            text.push_str("[reasoning]\n");
            text.push_str(&self.reasoning);
            text.push_str("\n\n");
        }
        text.push_str(&self.answer);
        text.trim().to_string()
    }
}

/// Handle onto the spawned daemon bridge: a channel for sending
/// [`ClientMessageType`]s to the daemon and one for receiving [`BridgeEvent`]s from
/// it.
///
/// Created by [`DaemonBridge::spawn`] and split into its two halves with
/// [`DaemonBridge::into_parts`], so a platform layer can hand the send and
/// receive halves to different threads.
pub struct DaemonBridge {
    client_tx: Sender<ClientMessageType>,
    event_rx: Receiver<BridgeEvent>,
}

/// Platform-agnostic event extracted from the daemon's message stream, ready
/// for a platform layer to render.
///
/// The bridge reader thread folds the raw wire messages into this small
/// vocabulary so a platform implementation never sees the wire protocol.
/// Streamed content is emitted only once the relevant stream completes.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// A completed assistant response, with any reasoning prepended.
    Text(String),
    /// A tool invocation has begun.
    ToolCallStarted {
        /// Name of the tool being invoked.
        name: String,
        /// The tool's arguments, as JSON.
        arguments_json: String,
    },
    /// A tool invocation finished successfully.
    ToolCallFinished {
        /// Name of the tool that finished.
        name: String,
        /// The tool's captured output.
        output: String,
    },
    /// A tool invocation failed.
    ToolCallFailed {
        /// Name of the tool that failed.
        name: String,
        /// The failure description.
        error: String,
    },
    /// Image bytes fetched for a displayed turn image.
    Image {
        /// MIME type; currently always empty — the daemon's on-demand image
        /// reply carries no metadata, so the platform infers the type from the
        /// bytes.
        _mime: String,
        /// The image bytes.
        data: Vec<u8>,
    },
    /// An error surfaced by the daemon: a failed request, a cancellation, or a
    /// transport error.
    Error(String),
    /// The available models and, if one is set, the selected model.
    Models {
        /// All selectable model names.
        models: Vec<String>,
        /// The selected model, or `None` if none is set.
        selected: Option<String>,
    },
    /// The daemon reported a change of selected model.
    ModelSelected(String),
    /// The daemon keystore was unlocked.
    Unlocked,
    /// The daemon keystore was locked.
    Locked,
    /// A liveness reply.
    Pong,
}

impl DaemonBridge {
    /// Spawn the writer and reader threads and return a handle onto them.
    ///
    /// The writer thread drains the returned send half onto the socket; the
    /// reader thread turns daemon messages into [`BridgeEvent`]s and drives the
    /// attach handshake and on-demand image fetch. Both channels are unbounded
    /// crossbeam channels, so a send never blocks on a busy peer.
    #[must_use]
    pub fn spawn(reader: BufReader<UnixStream>, writer: BufWriter<UnixStream>) -> Self {
        let (client_tx, client_rx) = crossbeam_channel::unbounded::<ClientMessageType>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded::<BridgeEvent>();
        let writer_event_tx = event_tx.clone();

        info!("spawning daemon bridge tasks");

        // Writer thread: reads ClientMessages from the channel and writes them
        // to the daemon socket. On write failure, sends an error event and shuts down.
        std::thread::spawn(move || {
            let mut writer = writer;
            let client_rx = client_rx;
            // The bridge writer thread is its single send site: it stamps a
            // per-connection request id onto every outbound frame (the IM
            // bridge ignores replies).
            let mut next_id: u64 = 0;
            while let Ok(msg) = client_rx.recv() {
                let framed = ClientMessage::request(next_id, msg);
                next_id = next_id.wrapping_add(1);
                debug!(?framed, "sending message to daemon");
                if let Err(e) = write_message(&mut writer, &framed) {
                    error!(%e, "write error, bridge writer shutting down");
                    if let Err(send_err) =
                        writer_event_tx.send(BridgeEvent::Error(format!("write error: {e}")))
                    {
                        warn!("failed to send write error event: {send_err}");
                    }
                    break;
                }
                let _ = writer.flush();
            }
            info!("bridge writer task finished");
        });

        // Reader thread: uses the shared run_daemon_reader loop from choreo-client-core.
        // It handles EOF, connection reset, and protocol errors uniformly.
        //
        // The reader also drives on-demand image fetching: displayed-image
        // bytes are stripped from `TurnAppended` (protocol v6), so it issues a
        // `GetImage` per image that has bytes and forwards the fetched bytes to
        // the bridge as an `Image` event (the path Telegram uploads).
        let reader_client_tx = client_tx.clone();
        let image_event_tx = event_tx.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffers: HashMap<u64, StreamBuffer> = HashMap::new();
            let mut tool_buffers: HashMap<u64, String> = HashMap::new();
            // Attach-once latch. The daemon only serves `GetImage` for — and
            // only delivers session-scoped events (`TurnAppended`, …) to
            // subscribers of — the session a connection is ATTACHED to. So the
            // bridge must attach before its image path can work. The latch
            // lives on the reader thread because the decision is driven by the
            // `ListSessions` reply arriving on this socket.
            let mut attached = false;

            let result = choreo_client_core::run_daemon_reader(&mut reader, |msg| {
                debug!(?msg, "received daemon message");
                // Attach handshake. The daemon only serves `GetImage` for — and
                // only delivers session-scoped events (`TurnAppended`, …) to
                // subscribers of — the session a connection is ATTACHED to, so
                // the bridge must attach before its image path can work. Two
                // triggers, both gated by the one-shot latch so a later summary
                // refresh cannot re-attach:
                //   1. the startup `ListSessions` reply (an existing session), and
                //   2. a later `SessionCreated` push — the bridge may start
                //      before ANY session exists, in which case it attaches to
                //      the first top-level session created afterwards. The
                //      daemon pushes `SessionCreated` to summary subscribers,
                //      which the bridge becomes at startup (the
                //      `SubscribeSessionsSummary` below) precisely so this
                //      retry path exists instead of the bridge staying
                //      unattached forever.
                if !attached && let Some(session_id) = attach_target(&msg) {
                    attached = true;
                    info!(session_id, "bridge attaching to session");
                    let _ = reader_client_tx.send(ClientMessageType::AttachSession { session_id });
                }
                // Live turns carry displayed images, but only their metadata:
                // request each image's bytes and emit them when the matching
                // `Image` reply arrives.
                if let DaemonMessageType::Session {
                    session_id,
                    event: SessionEvent::TurnAppended { turn_id, turn, .. },
                    ..
                } = &msg.inner
                {
                    let (events, requests) =
                        collect_turn_images(session_id.unwrap_or_default(), *turn_id, turn);
                    for event in events {
                        let _ = image_event_tx.send(event);
                    }
                    for request in requests {
                        let _ = reader_client_tx.send(request);
                    }
                }
                // The on-demand reply: forward the fetched bytes.
                if let DaemonMessageType::Image {
                    data: Some(bytes), ..
                } = &msg.inner
                {
                    let _ = image_event_tx.send(BridgeEvent::Image {
                        _mime: String::new(),
                        data: bytes.clone(),
                    });
                }
                if let Some(event) = daemon_to_bridge_events(msg, &mut buffers, &mut tool_buffers) {
                    let _ = event_tx.send(event);
                }
            });

            if let Err(e) = result {
                error!(%e, "daemon read loop ended with error");
                let _ = event_tx.send(BridgeEvent::Error(format!("daemon error: {e}")));
            } else {
                info!("daemon disconnected cleanly");
            }
        });

        // Kick off the session-selection handshake: subscribe to session
        // lifecycle pushes (so a session created AFTER startup still triggers
        // an attach) and ask for the current session list; the reader thread's
        // `attach_target` picks one and attaches. Without this the bridge
        // receives no live turns and its on-demand image fetch would be
        // refused (the daemon serves `GetImage` per attached session only).
        // Best-effort: `client_tx` outlives the writer thread as long as the
        // bridge is alive, so a send can only fail if the writer already
        // exited (daemon gone), in which case no attach would help anyway.
        if let Err(e) = client_tx.send(ClientMessageType::SubscribeSessionsSummary) {
            warn!("failed to subscribe to session summaries at bridge startup: {e}");
        }
        if let Err(e) = client_tx.send(ClientMessageType::ListSessions) {
            warn!("failed to request session list at bridge startup: {e}");
        }

        Self {
            client_tx,
            event_rx,
        }
    }

    /// Split the handle into its send and receive halves, for handing to
    /// separate threads.
    #[must_use]
    pub fn into_parts(self) -> (Sender<ClientMessageType>, Receiver<BridgeEvent>) {
        (self.client_tx, self.event_rx)
    }
}

/// Split a live turn's displayed images into the ones to forward directly and
/// the ones to fetch on demand.
///
/// Protocol v6 strips displayed-image bytes from `TurnAppended`, leaving only
/// metadata — so an image with `byte_len > 0` but empty `data` must be
/// requested with `ClientMessageType::GetImage` (keyed by session, turn, and the
/// image's index within the turn). An image that still carries bytes inline
/// (an older daemon) is forwarded immediately, and a genuinely zero-byte image
/// has nothing to send. Pure and side-effect free so the policy is unit-tested
/// without a live socket.
fn collect_turn_images(
    session_id: u64,
    turn_id: u32,
    turn: &Turn,
) -> (Vec<BridgeEvent>, Vec<ClientMessageType>) {
    let mut events = Vec::new();
    let mut requests = Vec::new();
    for (idx, record) in turn.displayed_images.iter().enumerate() {
        if !record.data.is_empty() {
            events.push(BridgeEvent::Image {
                _mime: record.metadata.mime_type.clone(),
                data: record.data.clone(),
            });
        } else if record.metadata.byte_len > 0 {
            requests.push(ClientMessageType::GetImage {
                session_id,
                turn_id,
                key: ImageKey::Displayed {
                    index: u32::try_from(idx).unwrap_or(u32::MAX),
                },
            });
        }
    }
    (events, requests)
}

/// Pick the session the bridge should attach to from a single daemon message,
/// or `None` if the message carries nothing attachable.
///
/// Two sources: the `ListSessions` reply (delegated to [`session_to_attach`])
/// and a `SessionCreated` push for a TOP-LEVEL session
/// (`parent_session_id.is_none()`). The latter is the retry path for a bridge
/// that started before any session existed — sub-sessions are never a root
/// attach target. Pure so the policy is unit-tested without a live socket.
fn attach_target(msg: &DaemonMessage) -> Option<u64> {
    match &msg.inner {
        DaemonMessageType::Sessions { sessions } => session_to_attach(sessions),
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::SessionCreated {
                    parent_session_id: None,
                    ..
                },
        } => Some(*session_id),
        _ => None,
    }
}

/// Choose which session the bridge attaches to from a `ListSessions` reply.
///
/// Prefer the first top-level session (`parent_session_id.is_none()`) — the
/// daemon's notion of a root conversation, matching the GUI/TUI's default —
/// and otherwise fall back to the first session of any kind. `None` when the
/// list is empty (nothing to attach to yet). Pure so the attach policy is
/// unit-tested without a live socket.
fn session_to_attach(sessions: &[SessionSummary]) -> Option<u64> {
    sessions
        .iter()
        .find(|s| s.parent_session_id.is_none())
        .or_else(|| sessions.first())
        .map(|s| s.session_id)
}

fn daemon_to_bridge_events(
    msg: DaemonMessage,
    buffers: &mut HashMap<u64, StreamBuffer>,
    tool_buffers: &mut HashMap<u64, String>,
) -> Option<BridgeEvent> {
    match msg.inner {
        DaemonMessageType::Session {
            event:
                SessionEvent::OutputChunk {
                    stream_id,
                    stream,
                    data,
                    ..
                },
            ..
        } => {
            let text = String::from_utf8_lossy(&data);
            let entry = buffers.entry(stream_id).or_insert_with(StreamBuffer::new);
            entry.append(&stream, &text);
            None
        }
        DaemonMessageType::Session {
            event: SessionEvent::Done { stream_id, .. },
            ..
        } => {
            if let Some(entry) = buffers.remove(&stream_id) {
                let text = entry.flatten();
                if !text.is_empty() {
                    return Some(BridgeEvent::Text(text));
                }
            }
            None
        }
        DaemonMessageType::Session {
            event: SessionEvent::Failed {
                stream_id, error, ..
            },
            ..
        } => {
            buffers.remove(&stream_id);
            Some(BridgeEvent::Error(error))
        }
        DaemonMessageType::Session {
            event: SessionEvent::Cancelled { stream_id, .. },
            ..
        } => {
            buffers.remove(&stream_id);
            tool_buffers.remove(&stream_id);
            Some(BridgeEvent::Error("cancelled".into()))
        }
        DaemonMessageType::Session {
            event:
                SessionEvent::ToolCallStarted {
                    tool_name: name,
                    arguments_json,
                    ..
                },
            ..
        } => Some(BridgeEvent::ToolCallStarted {
            name,
            arguments_json,
        }),
        DaemonMessageType::Session {
            event:
                SessionEvent::ToolCallFinished {
                    stream_id,
                    tool_name: name,
                    ..
                },
            ..
        } => {
            let output = tool_buffers.remove(&stream_id).unwrap_or_default();
            Some(BridgeEvent::ToolCallFinished { name, output })
        }
        DaemonMessageType::Session {
            event:
                SessionEvent::ToolCallFailed {
                    stream_id,
                    tool_name: name,
                    error,
                    ..
                },
            ..
        } => {
            tool_buffers.remove(&stream_id);
            Some(BridgeEvent::ToolCallFailed { name, error })
        }
        DaemonMessageType::Session {
            event:
                SessionEvent::TurnAppended { .. }
                | SessionEvent::TurnsUndone { .. }
                | SessionEvent::TurnsRedone { .. },
            ..
        } => {
            // Images are extracted from turns in the reader thread callback.
            // TurnsUndone/TurnsRedone don't carry image data.
            None
        }
        DaemonMessageType::Models {
            models,
            selected_model: selected,
            ..
        } => Some(BridgeEvent::Models { models, selected }),
        DaemonMessageType::Session {
            event: SessionEvent::ModelSelected { model, .. },
            ..
        } => Some(BridgeEvent::ModelSelected(model)),
        DaemonMessageType::Unlocked => Some(BridgeEvent::Unlocked),
        DaemonMessageType::Locked => Some(BridgeEvent::Locked),
        DaemonMessageType::Pong => Some(BridgeEvent::Pong),
        DaemonMessageType::Session {
            event: SessionEvent::SessionFailed { error, .. },
            ..
        }
        | DaemonMessageType::LockedError { error }
        | DaemonMessageType::ModelsFailed { error }
        | DaemonMessageType::Session {
            event: SessionEvent::ModelSelectionFailed { error, .. },
            ..
        } => Some(BridgeEvent::Error(error)),
        DaemonMessageType::Session {
            event: SessionEvent::Started { .. },
            ..
        } => {
            debug!("bridge ignoring Started event");
            None
        }
        DaemonMessageType::ShuttingDown => {
            info!("daemon shutting down");
            None
        }
        DaemonMessageType::Session {
            event:
                SessionEvent::SessionCreated { .. }
                | SessionEvent::SessionCreatedForRequester { .. }
                | SessionEvent::SessionAttached
                | SessionEvent::SessionState { .. }
                | SessionEvent::SessionStatusChanged { .. }
                | SessionEvent::SessionDeleted
                | SessionEvent::SessionDeleteFailed { .. }
                | SessionEvent::ContextWindowResolved { .. }
                | SessionEvent::LiveOutputTokenCount { .. }
                | SessionEvent::TokenUsageUpdate { .. },
            ..
        } => {
            // Expected traffic once the bridge is ATTACHED to a session (it
            // now sends `AttachSession` at startup): session metadata, the
            // attach acknowledgement, status changes, and the live token
            // counters. None of these map to a rendered bridge event, so drop
            // them at debug rather than warning on every ordinary turn.
            debug!(
                ?msg,
                "bridge ignoring attached-session metadata/status event"
            );
            None
        }
        DaemonMessageType::Sessions { .. } => {
            // The `ListSessions` reply is consumed by the reader callback (to
            // choose the session to attach to); the event channel has nothing
            // to render. Expected startup traffic, so no warning.
            None
        }
        DaemonMessageType::CredentialAdded { .. }
        | DaemonMessageType::CredentialAddFailed { .. }
        | DaemonMessageType::CredentialRemoved { .. }
        | DaemonMessageType::CredentialRemoveFailed { .. }
        | DaemonMessageType::Credential { .. } => {
            warn!(?msg, "unhandled daemon message variant in bridge");
            None
        }
        DaemonMessageType::Session {
            event: SessionEvent::ToolResultChunk {
                stream_id, data, ..
            },
            ..
        } => {
            if let Ok(text) = String::from_utf8(data) {
                tool_buffers.entry(stream_id).or_default().push_str(&text);
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::{DaemonMessage, OutputStream, SessionEvent, SessionStatus, SessionSummary};
    use std::collections::HashMap;

    /// Minimal `SessionSummary` for the attach-decision tests: only the two
    /// fields `session_to_attach` consults (`session_id`,
    /// `parent_session_id`) matter; the rest are filler.
    fn summary(session_id: u64, parent_session_id: Option<u64>) -> SessionSummary {
        SessionSummary {
            session_id,
            title: None,
            selected_model: None,
            reasoning_effort: None,
            parent_session_id,
            working_dir: None,
            created_at: 0,
            last_modified: 0,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            pinned: false,
            archived_at: None,
        }
    }

    #[test]
    fn session_to_attach_prefers_a_top_level_session() {
        // A sub-session first in the list must be skipped in favour of the
        // first top-level session.
        let sessions = vec![summary(5, Some(9)), summary(7, None), summary(8, None)];
        assert_eq!(session_to_attach(&sessions), Some(7));
    }

    #[test]
    fn session_to_attach_falls_back_to_first_and_handles_empty() {
        // No top-level session: fall back to whatever is first.
        assert_eq!(session_to_attach(&[summary(5, Some(9))]), Some(5));
        // Nothing to attach to yet.
        assert_eq!(session_to_attach(&[]), None);
    }

    #[test]
    fn reader_sessions_reply_attaches_to_first_session() {
        // Mirrors the reader callback's `Sessions` handling: the chosen id is
        // turned into a single `AttachSession`. Driving the pure helper here
        // (choreo-im has no in-crate socket test harness) proves the reader's
        // decision without a live daemon.
        let sessions = vec![summary(5, Some(9)), summary(7, None)];
        let msg = session_to_attach(&sessions)
            .map(|session_id| ClientMessageType::AttachSession { session_id });
        assert_eq!(
            msg,
            Some(ClientMessageType::AttachSession { session_id: 7 })
        );

        // Empty list attaches to nothing.
        assert_eq!(
            session_to_attach(&[])
                .map(|session_id| ClientMessageType::AttachSession { session_id }),
            None
        );
    }

    /// A `SessionCreated` envelope for `id` (only the fields `attach_target`
    /// consults matter; the rest are filler).
    fn session_created(id: u64, parent_session_id: Option<u64>) -> DaemonMessage {
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(id),
            event: SessionEvent::SessionCreated {
                title: None,
                parent_session_id,
                working_dir: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            },
        })
    }

    #[test]
    fn attach_target_from_list_reply_and_top_level_creation() {
        // A `ListSessions` reply is delegated to `session_to_attach`.
        let sessions = vec![summary(5, Some(9)), summary(7, None)];
        assert_eq!(
            attach_target(&DaemonMessage::broadcast(DaemonMessageType::Sessions {
                sessions
            })),
            Some(7)
        );
        // An empty list has nothing to attach to.
        assert_eq!(
            attach_target(&DaemonMessage::broadcast(DaemonMessageType::Sessions {
                sessions: vec![]
            })),
            None
        );

        // A later TOP-LEVEL `SessionCreated` is the retry path (the bridge
        // started before any session existed): attachable by its own id.
        assert_eq!(attach_target(&session_created(11, None)), Some(11));
        // A sub-session creation is never a root attach target.
        assert_eq!(attach_target(&session_created(11, Some(7))), None);

        // Unrelated messages carry nothing attachable.
        assert_eq!(
            attach_target(&DaemonMessage::broadcast(DaemonMessageType::Pong)),
            None
        );
    }

    #[test]
    fn test_output_chunk_buffering() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events1 = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::OutputChunk {
                    stream_id: 1,
                    stream: OutputStream::Answer,
                    data: b"hello ".to_vec(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events1.is_none());

        let events2 = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::OutputChunk {
                    stream_id: 1,
                    stream: OutputStream::Answer,
                    data: b"world".to_vec(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events2.is_none());

        let events3 = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::Done {
                    stream_id: 1,
                    token_usage: None,
                    last_prompt_tokens: None,
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events3.is_some());
        match &events3.unwrap() {
            BridgeEvent::Text(text) => assert_eq!(text, "hello world"),
            other => panic!("expected Text event, got {other:?}"),
        }
    }

    #[test]
    fn test_done_no_chunks() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::Done {
                    stream_id: 999,
                    token_usage: None,
                    last_prompt_tokens: None,
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_none());
    }

    #[test]
    fn test_failed_clears_buffer() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::OutputChunk {
                    stream_id: 1,
                    stream: OutputStream::Answer,
                    data: b"data".to_vec(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::Failed {
                    stream_id: 1,
                    error: "oops".into(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        assert!(matches!(events.as_ref().unwrap(), BridgeEvent::Error(msg) if msg == "oops"));

        let events2 = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::Done {
                    stream_id: 1,
                    token_usage: None,
                    last_prompt_tokens: None,
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events2.is_none());
    }

    #[test]
    fn test_cancelled_clears_buffer() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::OutputChunk {
                    stream_id: 2,
                    stream: OutputStream::Answer,
                    data: b"data".to_vec(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::Cancelled { stream_id: 2 },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        assert!(matches!(events.as_ref().unwrap(), BridgeEvent::Error(msg) if msg == "cancelled"));

        let events2 = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::Done {
                    stream_id: 2,
                    token_usage: None,
                    last_prompt_tokens: None,
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events2.is_none());
    }

    #[test]
    fn test_tool_call_events() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::ToolCallStarted {
                    stream_id: 1,
                    call_id: "call_1".into(),
                    tool_name: "read".into(),
                    arguments_json: r#"{"path":"/tmp"}"#.into(),
                    invocation_description: String::new(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        match events.unwrap() {
            BridgeEvent::ToolCallStarted {
                name,
                arguments_json,
            } => {
                assert_eq!(name, "read");
                assert_eq!(arguments_json, r#"{"path":"/tmp"}"#);
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn test_tool_call_finished() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        // First send a chunk so the buffer has content
        daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::ToolResultChunk {
                    stream_id: 1,
                    call_id: "call_1".into(),
                    data: b"file contents".to_vec(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::ToolCallFinished {
                    stream_id: 1,
                    call_id: "call_1".into(),
                    tool_name: "read".into(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        match events.unwrap() {
            BridgeEvent::ToolCallFinished { name, output } => {
                assert_eq!(name, "read");
                assert_eq!(output, "file contents");
            }
            other => panic!("expected ToolCallFinished, got {other:?}"),
        }
    }

    #[test]
    fn test_tool_call_failed() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::ToolCallFailed {
                    stream_id: 1,
                    call_id: "call_1".into(),
                    tool_name: "read".into(),
                    error: "permission denied".into(),
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        match events.unwrap() {
            BridgeEvent::ToolCallFailed { name, error } => {
                assert_eq!(name, "read");
                assert_eq!(error, "permission denied");
            }
            other => panic!("expected ToolCallFailed, got {other:?}"),
        }
    }

    #[test]
    fn collect_turn_images_forwards_inline_and_requests_stripped() {
        // Protocol v6: a live turn's images carry only metadata. An image that
        // still has inline bytes is forwarded; one with byte_len>0 but no bytes
        // is requested via GetImage; a zero-byte image is dropped.
        let meta = |byte_len| choreo_proto::ImageMetadata {
            mime_type: "image/png".into(),
            width: 1,
            height: 1,
            byte_len,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                choreo_proto::DisplayedImageRecord {
                    metadata: meta(4),
                    data: b"AAAA".to_vec(),
                    tool_call_id: None,
                },
                choreo_proto::DisplayedImageRecord {
                    metadata: meta(9),
                    data: Vec::new(),
                    tool_call_id: None,
                },
                choreo_proto::DisplayedImageRecord {
                    metadata: meta(0),
                    data: Vec::new(),
                    tool_call_id: None,
                },
            ],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let (events, requests) = collect_turn_images(7, 3, &turn);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], BridgeEvent::Image { data, .. } if data == b"AAAA"));
        assert_eq!(
            requests,
            vec![ClientMessageType::GetImage {
                session_id: 7,
                turn_id: 3,
                key: ImageKey::Displayed { index: 1 },
            }]
        );
    }

    #[test]
    fn test_turn_appended_images() {
        // TurnAppended with displayed images should produce Image events
        // when processed via the reader thread. The unit test checks that
        // daemon_to_bridge_events returns None for turn messages (images
        // are extracted in the reader callback instead).
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::TurnAppended {
                    turn_id: 1,
                    turn: choreo_proto::Turn {
                        created_at: choreo_proto::TimestampMs::now(),
                        undone: false,
                        error: None,
                        user_text: Some("hello".into()),
                        assistant_text: None,
                        assistant_reasoning: None,
                        tool_calls: vec![],
                        token_usage: None,
                        tool_results: vec![],
                        displayed_images: vec![choreo_proto::DisplayedImageRecord {
                            metadata: choreo_proto::ImageMetadata {
                                mime_type: "image/png".into(),
                                width: 100,
                                height: 100,
                                byte_len: 5,
                                alt: None,
                            },
                            data: b"hello".to_vec(),
                            tool_call_id: None,
                        }],
                        reasoning_artifact: None,
                        reasoning_producer: None,
                    },
                },
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        // daemon_to_bridge_events returns None for TurnAppended;
        // images are emitted via the reader thread callback.
        assert!(events.is_none());
    }

    #[test]
    fn test_models_event() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Models {
                models: vec!["gpt-4".into(), "claude".into()],
                selected_model: Some("claude".into()),
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        match events.unwrap() {
            BridgeEvent::Models { models, selected } => {
                assert_eq!(models, vec!["gpt-4".to_string(), "claude".to_string()]);
                assert_eq!(selected, Some("claude".to_string()));
            }
            other => panic!("expected Models, got {other:?}"),
        }
    }

    #[test]
    fn test_models_failed_event() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::ModelsFailed {
                error: "network error".into(),
            }),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        match events.unwrap() {
            BridgeEvent::Error(msg) => assert_eq!(msg, "network error"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_pong_event() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::broadcast(DaemonMessageType::Pong),
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        assert!(matches!(events.as_ref().unwrap(), BridgeEvent::Pong));
    }

    #[test]
    fn test_error_variants() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let cases = vec![
            DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::SessionFailed {
                    operation: "attach".into(),
                    error: "session error".into(),
                },
            },
            DaemonMessageType::LockedError {
                error: "already locked".into(),
            },
            DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::ModelSelectionFailed {
                    model: "gpt-4".into(),
                    error: "not available".into(),
                },
            },
        ];

        for msg in cases {
            let events = daemon_to_bridge_events(
                DaemonMessage::broadcast(msg),
                &mut buffers,
                &mut tool_buffers,
            );
            assert!(events.is_some());
            assert!(matches!(events.as_ref().unwrap(), BridgeEvent::Error(_)));
        }
    }
}
