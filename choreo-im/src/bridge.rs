use choreo_proto::{ClientMessage, DaemonMessage, OutputStream, write_message};
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use tracing::{debug, error, info, warn};

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

    fn append(&mut self, stream: OutputStream, data: &str) {
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

pub struct DaemonBridge {
    client_tx: mpsc::Sender<ClientMessage>,
    event_rx: mpsc::Receiver<BridgeEvent>,
}

#[derive(Debug, Clone)]
pub enum BridgeEvent {
    Text(String),
    ToolCallStarted {
        name: String,
        arguments_json: String,
    },
    ToolCallFinished {
        name: String,
        output: String,
    },
    ToolCallFailed {
        name: String,
        error: String,
    },
    Image {
        _mime: String,
        data: Vec<u8>,
    },
    Error(String),
    Models {
        models: Vec<String>,
        selected: Option<String>,
    },
    ModelSelected(String),
    Unlocked,
    Locked,
    Pong,
}

impl DaemonBridge {
    pub fn spawn(reader: BufReader<UnixStream>, writer: BufWriter<UnixStream>) -> Self {
        let (client_tx, client_rx) = mpsc::channel::<ClientMessage>();
        let (event_tx, event_rx) = mpsc::channel::<BridgeEvent>();
        let writer_event_tx = event_tx.clone();

        info!("spawning daemon bridge tasks");

        // Writer thread: reads ClientMessages from the channel and writes them
        // to the daemon socket. On write failure, sends an error event and shuts down.
        std::thread::spawn(move || {
            let mut writer = writer;
            let client_rx = client_rx;
            while let Ok(msg) = client_rx.recv() {
                debug!(?msg, "sending message to daemon");
                if let Err(e) = write_message(&mut writer, &msg) {
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
        let image_event_tx = event_tx.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffers: HashMap<u32, StreamBuffer> = HashMap::new();
            let mut tool_buffers: HashMap<u32, String> = HashMap::new();

            let result = choreo_client_core::run_daemon_reader(&mut reader, |msg| {
                debug!(?msg, "received daemon message");
                // Extract images from TurnAppended/TurnFinalized before passing
                // to the standard handler.
                match &msg {
                    DaemonMessage::TurnAppended { turn, .. }
                    | DaemonMessage::TurnFinalized { turn, .. } => {
                        for record in &turn.displayed_images {
                            let _ = image_event_tx.send(BridgeEvent::Image {
                                _mime: record.metadata.mime_type.clone(),
                                data: record.data.clone(),
                            });
                        }
                    }
                    _ => {}
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

        Self {
            client_tx,
            event_rx,
        }
    }

    pub fn into_parts(self) -> (mpsc::Sender<ClientMessage>, mpsc::Receiver<BridgeEvent>) {
        (self.client_tx, self.event_rx)
    }
}

fn daemon_to_bridge_events(
    msg: DaemonMessage,
    buffers: &mut HashMap<u32, StreamBuffer>,
    tool_buffers: &mut HashMap<u32, String>,
) -> Option<BridgeEvent> {
    match msg {
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
            ..
        } => {
            let text = String::from_utf8_lossy(&data);
            let entry = buffers.entry(request_id).or_insert_with(StreamBuffer::new);
            entry.append(stream, &text);
            None
        }
        DaemonMessage::Done { request_id, .. } => {
            if let Some(entry) = buffers.remove(&request_id) {
                let text = entry.flatten();
                if !text.is_empty() {
                    return Some(BridgeEvent::Text(text));
                }
            }
            None
        }
        DaemonMessage::Failed {
            request_id, error, ..
        } => {
            buffers.remove(&request_id);
            Some(BridgeEvent::Error(error))
        }
        DaemonMessage::Cancelled { request_id, .. } => {
            buffers.remove(&request_id);
            tool_buffers.remove(&request_id);
            Some(BridgeEvent::Error("cancelled".into()))
        }
        DaemonMessage::ToolCallStarted {
            tool_name: name,
            arguments_json,
            ..
        } => Some(BridgeEvent::ToolCallStarted {
            name,
            arguments_json,
        }),
        DaemonMessage::ToolCallFinished {
            request_id,
            tool_name: name,
            ..
        } => {
            let output = tool_buffers.remove(&request_id).unwrap_or_default();
            Some(BridgeEvent::ToolCallFinished { name, output })
        }
        DaemonMessage::ToolCallFailed {
            request_id,
            tool_name: name,
            error,
            ..
        } => {
            tool_buffers.remove(&request_id);
            Some(BridgeEvent::ToolCallFailed { name, error })
        }
        DaemonMessage::TurnAppended { .. }
        | DaemonMessage::TurnFinalized { .. }
        | DaemonMessage::TurnsUndone { .. }
        | DaemonMessage::TurnsRedone { .. } => {
            // Images are extracted from turns in the reader thread callback.
            // TurnsUndone/TurnsRedone don't carry image data.
            None
        }
        DaemonMessage::Models {
            models,
            selected_model: selected,
            ..
        } => Some(BridgeEvent::Models { models, selected }),
        DaemonMessage::ModelSelected { model, .. } => Some(BridgeEvent::ModelSelected(model)),
        DaemonMessage::Unlocked => Some(BridgeEvent::Unlocked),
        DaemonMessage::Locked => Some(BridgeEvent::Locked),
        DaemonMessage::Pong => Some(BridgeEvent::Pong),
        DaemonMessage::SessionFailed { error, .. }
        | DaemonMessage::LockedError { error }
        | DaemonMessage::ModelsFailed { error }
        | DaemonMessage::ModelSelectionFailed { error, .. } => Some(BridgeEvent::Error(error)),
        DaemonMessage::Started { .. } => {
            debug!("bridge ignoring Started event");
            None
        }
        DaemonMessage::ShuttingDown => {
            info!("daemon shutting down");
            None
        }
        DaemonMessage::SessionCreated { .. }
        | DaemonMessage::Sessions { .. }
        | DaemonMessage::SessionAttached { .. }
        | DaemonMessage::SessionState { .. }
        | DaemonMessage::SessionStatusChanged { .. }
        | DaemonMessage::SessionDeleted { .. }
        | DaemonMessage::SessionDeleteFailed { .. }
        | DaemonMessage::CredentialAdded { .. }
        | DaemonMessage::CredentialAddFailed { .. }
        | DaemonMessage::CredentialRemoved { .. }
        | DaemonMessage::CredentialRemoveFailed { .. }
        | DaemonMessage::Credential { .. } => {
            warn!(?msg, "unhandled daemon message variant in bridge");
            None
        }
        DaemonMessage::ToolResultChunk {
            request_id, data, ..
        } => {
            if let Ok(text) = String::from_utf8(data) {
                tool_buffers.entry(request_id).or_default().push_str(&text);
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::{DaemonMessage, OutputStream};
    use std::collections::HashMap;

    #[test]
    fn test_output_chunk_buffering() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events1 = daemon_to_bridge_events(
            DaemonMessage::OutputChunk {
                session_id: 0,
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"hello ".to_vec(),
            },
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events1.is_none());

        let events2 = daemon_to_bridge_events(
            DaemonMessage::OutputChunk {
                session_id: 0,
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"world".to_vec(),
            },
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events2.is_none());

        let events3 = daemon_to_bridge_events(
            DaemonMessage::Done {
                session_id: 0,
                request_id: 1,
                token_usage: None,
                last_prompt_tokens: None,
            },
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
            DaemonMessage::Done {
                session_id: 0,
                request_id: 999,
                token_usage: None,
                last_prompt_tokens: None,
            },
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
            DaemonMessage::OutputChunk {
                session_id: 0,
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"data".to_vec(),
            },
            &mut buffers,
            &mut tool_buffers,
        );

        let events = daemon_to_bridge_events(
            DaemonMessage::Failed {
                session_id: 0,
                request_id: 1,
                error: "oops".into(),
            },
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        assert!(matches!(events.as_ref().unwrap(), BridgeEvent::Error(msg) if msg == "oops"));

        let events2 = daemon_to_bridge_events(
            DaemonMessage::Done {
                session_id: 0,
                request_id: 1,
                token_usage: None,
                last_prompt_tokens: None,
            },
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
            DaemonMessage::OutputChunk {
                session_id: 0,
                request_id: 2,
                stream: OutputStream::Answer,
                data: b"data".to_vec(),
            },
            &mut buffers,
            &mut tool_buffers,
        );

        let events = daemon_to_bridge_events(
            DaemonMessage::Cancelled {
                session_id: 0,
                request_id: 2,
            },
            &mut buffers,
            &mut tool_buffers,
        );
        assert!(events.is_some());
        assert!(matches!(events.as_ref().unwrap(), BridgeEvent::Error(msg) if msg == "cancelled"));

        let events2 = daemon_to_bridge_events(
            DaemonMessage::Done {
                session_id: 0,
                request_id: 2,
                token_usage: None,
                last_prompt_tokens: None,
            },
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
            DaemonMessage::ToolCallStarted {
                session_id: 0,
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read".into(),
                arguments_json: r#"{"path":"/tmp"}"#.into(),
            },
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
            DaemonMessage::ToolResultChunk {
                session_id: 0,
                request_id: 1,
                call_id: "call_1".into(),
                data: b"file contents".to_vec(),
            },
            &mut buffers,
            &mut tool_buffers,
        );

        let events = daemon_to_bridge_events(
            DaemonMessage::ToolCallFinished {
                session_id: 0,
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read".into(),
            },
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
            DaemonMessage::ToolCallFailed {
                session_id: 0,
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read".into(),
                error: "permission denied".into(),
            },
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
    fn test_turn_appended_images() {
        // TurnAppended with displayed images should produce Image events
        // when processed via the reader thread. The unit test checks that
        // daemon_to_bridge_events returns None for turn messages (images
        // are extracted in the reader callback instead).
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::TurnAppended {
                session_id: 0,
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
                },
            },
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
            DaemonMessage::Models {
                models: vec!["gpt-4".into(), "claude".into()],
                selected_model: Some("claude".into()),
            },
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
            DaemonMessage::ModelsFailed {
                error: "network error".into(),
            },
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

        let events = daemon_to_bridge_events(DaemonMessage::Pong, &mut buffers, &mut tool_buffers);
        assert!(events.is_some());
        assert!(matches!(events.as_ref().unwrap(), BridgeEvent::Pong));
    }

    #[test]
    fn test_error_variants() {
        let mut buffers = HashMap::new();
        let mut tool_buffers = HashMap::new();

        let cases = vec![
            DaemonMessage::SessionFailed {
                session_id: 0,
                operation: "attach".into(),
                error: "session error".into(),
            },
            DaemonMessage::LockedError {
                error: "already locked".into(),
            },
            DaemonMessage::ModelSelectionFailed {
                session_id: 0,
                model: "gpt-4".into(),
                error: "not available".into(),
            },
        ];

        for msg in cases {
            let events = daemon_to_bridge_events(msg, &mut buffers, &mut tool_buffers);
            assert!(events.is_some());
            assert!(matches!(events.as_ref().unwrap(), BridgeEvent::Error(_)));
        }
    }
}
