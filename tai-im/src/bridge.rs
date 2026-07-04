use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use tai_client_core::{ImageAssembler, StreamingText};
use tai_proto::{
    ClientMessage, DaemonMessage, ImageMetadata, ProtoError, read_message_sync, write_message_sync,
};
use std::sync::mpsc;
use tracing::{debug, error, info, warn};

pub struct DaemonBridge {
    client_tx: mpsc::Sender<DaemonBridgeCommand>,
    event_rx: mpsc::Receiver<BridgeEvent>,
}

pub enum DaemonBridgeCommand {
    SendMessage(ClientMessage),
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
        let (client_tx, client_rx) = mpsc::channel::<DaemonBridgeCommand>();
        let (event_tx, event_rx) = mpsc::channel::<BridgeEvent>();
        let writer_event_tx = event_tx.clone();

        info!("spawning daemon bridge tasks");

        std::thread::spawn(move || {
            let mut writer = writer;
            let client_rx = client_rx;
            while let Ok(cmd) = client_rx.recv() {
                match cmd {
                    DaemonBridgeCommand::SendMessage(msg) => {
                        debug!(?msg, "sending message to daemon");
                        if let Err(e) = write_message_sync(&mut writer, &msg) {
                            error!(%e, "write error, bridge writer shutting down");
                            if let Err(send_err) = writer_event_tx
                                .send(BridgeEvent::Error(format!("write error: {e}")))
                            {
                                warn!("failed to send write error event: {send_err}");
                            }
                            break;
                        }
                        let _ = writer.flush();
                    }
                }
            }
            info!("bridge writer task finished");
        });

        std::thread::spawn(move || {
            let mut reader = reader;
            let mut assembler = ImageAssembler::new();
            let mut buffers: HashMap<u32, StreamingText> = HashMap::new();

            loop {
                match read_message_sync::<_, DaemonMessage>(&mut reader) {
                    Ok(msg) => {
                        debug!(?msg, "received daemon message");
                        for event in daemon_to_bridge_events(msg, &mut assembler, &mut buffers) {
                            if event_tx.send(event).is_err() {
                                warn!("bridge event receiver dropped, reader task exiting");
                                return;
                            }
                        }
                    }
                    Err(ProtoError::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        error!(%e, "daemon disconnected");
                        if let Err(send_err) =
                            event_tx.send(BridgeEvent::Error("daemon disconnected".into()))
                        {
                            warn!("failed to send disconnect error event: {send_err}");
                        }
                        return;
                    }
                    Err(e) => {
                        error!(%e, "daemon read error, reader task exiting");
                        if let Err(send_err) =
                            event_tx.send(BridgeEvent::Error(format!("daemon error: {e}")))
                        {
                            warn!("failed to send daemon error event: {send_err}");
                        }
                        return;
                    }
                }
            }
        });

        Self {
            client_tx,
            event_rx,
        }
    }

    pub fn into_parts(
        self,
    ) -> (
        mpsc::Sender<DaemonBridgeCommand>,
        mpsc::Receiver<BridgeEvent>,
    ) {
        (self.client_tx, self.event_rx)
    }
}

fn daemon_to_bridge_events(
    msg: DaemonMessage,
    assembler: &mut ImageAssembler,
    buffers: &mut HashMap<u32, StreamingText>,
) -> Vec<BridgeEvent> {
    let mut events = Vec::new();

    match msg {
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        } => {
            let text = String::from_utf8_lossy(&data);
            let entry = buffers
                .entry(request_id)
                .or_insert_with(|| StreamingText::new(request_id));
            entry.append(stream, &text);
        }
        DaemonMessage::Done { request_id } => {
            if let Some(entry) = buffers.remove(&request_id) {
                let mut text = String::new();
                if !entry.reasoning.is_empty() {
                    text.push_str("[reasoning]\n");
                    text.push_str(&entry.reasoning);
                    text.push_str("\n\n");
                }
                text.push_str(&entry.answer);
                let trimmed = text.trim().to_string();
                if !trimmed.is_empty() {
                    events.push(BridgeEvent::Text(trimmed));
                }
            }
        }
        DaemonMessage::Failed { request_id, error } => {
            buffers.remove(&request_id);
            events.push(BridgeEvent::Error(error));
        }
        DaemonMessage::Cancelled { request_id } => {
            buffers.remove(&request_id);
            events.push(BridgeEvent::Error("cancelled".into()));
        }
        DaemonMessage::ToolCallStarted {
            tool_name: name,
            arguments_json,
            ..
        } => events.push(BridgeEvent::ToolCallStarted {
            name,
            arguments_json,
        }),
        DaemonMessage::ToolCallFinished {
            tool_name: name,
            output,
            ..
        } => events.push(BridgeEvent::ToolCallFinished { name, output }),
        DaemonMessage::ToolCallFailed {
            tool_name: name,
            error,
            ..
        } => events.push(BridgeEvent::ToolCallFailed { name, error }),
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        } => {
            if let Err(e) = assembler.start(request_id, metadata) {
                warn!("failed to start image assembly: {e}");
            }
        }
        DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data,
        } => {
            if let Err(e) = assembler.push_chunk(request_id, image_id, &data) {
                warn!("failed to push image chunk: {e}");
            }
        }
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        } => match assembler.finish(request_id, image_id) {
            Ok((ImageMetadata { mime_type, .. }, data)) => {
                events.push(BridgeEvent::Image {
                    _mime: mime_type,
                    data,
                });
            }
            Err(e) => events.push(BridgeEvent::Error(format!("image assembly failed: {e}"))),
        },
        DaemonMessage::Models {
            models,
            selected_model: selected,
        } => events.push(BridgeEvent::Models { models, selected }),
        DaemonMessage::ModelSelected { model } => events.push(BridgeEvent::ModelSelected(model)),
        DaemonMessage::Unlocked => events.push(BridgeEvent::Unlocked),
        DaemonMessage::Locked => events.push(BridgeEvent::Locked),
        DaemonMessage::Pong => events.push(BridgeEvent::Pong),
        DaemonMessage::SessionFailed { error, .. }
        | DaemonMessage::LockedError { error }
        | DaemonMessage::ModelsFailed { error }
        | DaemonMessage::ModelSelectionFailed { error, .. } => {
            events.push(BridgeEvent::Error(error));
        }
        DaemonMessage::Started { .. } => {
            debug!("bridge ignoring Started event");
        }
        DaemonMessage::ShuttingDown => {
            info!("daemon shutting down");
        }
        DaemonMessage::SessionCreated { .. }
        | DaemonMessage::Sessions { .. }
        | DaemonMessage::SessionAttached { .. }
        | DaemonMessage::SessionState { .. }
        | DaemonMessage::SessionMessageAppended { .. }
        | DaemonMessage::SessionStatusChanged { .. }
        | DaemonMessage::CredentialAdded { .. }
        | DaemonMessage::CredentialAddFailed { .. }
        | DaemonMessage::CredentialRemoved { .. }
        | DaemonMessage::CredentialRemoveFailed { .. }
        | DaemonMessage::Credential { .. } => {
            warn!(?msg, "unhandled daemon message variant in bridge");
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tai_client_core::ImageAssembler;
    use tai_proto::{DaemonMessage, ImageMetadata, OutputStream};

    #[test]
    fn test_output_chunk_buffering() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let events1 = daemon_to_bridge_events(
            DaemonMessage::OutputChunk {
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"hello ".to_vec(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert!(events1.is_empty());

        let events2 = daemon_to_bridge_events(
            DaemonMessage::OutputChunk {
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"world".to_vec(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert!(events2.is_empty());

        let events3 = daemon_to_bridge_events(
            DaemonMessage::Done { request_id: 1 },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events3.len(), 1);
        match &events3[0] {
            BridgeEvent::Text(text) => assert_eq!(text, "hello world"),
            other => panic!("expected Text event, got {other:?}"),
        }
    }

    #[test]
    fn test_done_no_chunks() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::Done { request_id: 999 },
            &mut assembler,
            &mut buffers,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn test_failed_clears_buffer() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        daemon_to_bridge_events(
            DaemonMessage::OutputChunk {
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"data".to_vec(),
            },
            &mut assembler,
            &mut buffers,
        );

        let events = daemon_to_bridge_events(
            DaemonMessage::Failed {
                request_id: 1,
                error: "oops".into(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], BridgeEvent::Error(msg) if msg == "oops"));

        let events2 = daemon_to_bridge_events(
            DaemonMessage::Done { request_id: 1 },
            &mut assembler,
            &mut buffers,
        );
        assert!(events2.is_empty());
    }

    #[test]
    fn test_cancelled_clears_buffer() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        daemon_to_bridge_events(
            DaemonMessage::OutputChunk {
                request_id: 2,
                stream: OutputStream::Answer,
                data: b"data".to_vec(),
            },
            &mut assembler,
            &mut buffers,
        );

        let events = daemon_to_bridge_events(
            DaemonMessage::Cancelled { request_id: 2 },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], BridgeEvent::Error(msg) if msg == "cancelled"));

        let events2 = daemon_to_bridge_events(
            DaemonMessage::Done { request_id: 2 },
            &mut assembler,
            &mut buffers,
        );
        assert!(events2.is_empty());
    }

    #[test]
    fn test_tool_call_events() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::ToolCallStarted {
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read".into(),
                arguments_json: r#"{"path":"/tmp"}"#.into(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
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
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::ToolCallFinished {
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read".into(),
                output: "file contents".into(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            BridgeEvent::ToolCallFinished { name, output } => {
                assert_eq!(name, "read");
                assert_eq!(output, "file contents");
            }
            other => panic!("expected ToolCallFinished, got {other:?}"),
        }
    }

    #[test]
    fn test_tool_call_failed() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::ToolCallFailed {
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read".into(),
                error: "permission denied".into(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            BridgeEvent::ToolCallFailed { name, error } => {
                assert_eq!(name, "read");
                assert_eq!(error, "permission denied");
            }
            other => panic!("expected ToolCallFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_image_stream() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let metadata = ImageMetadata {
            image_id: 1,
            mime_type: "image/png".into(),
            width: 100,
            height: 100,
            byte_len: 5,
            alt: None,
        };

        let events1 = daemon_to_bridge_events(
            DaemonMessage::ImageStart {
                request_id: 1,
                metadata: metadata.clone(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert!(events1.is_empty());

        let events2 = daemon_to_bridge_events(
            DaemonMessage::ImageChunk {
                request_id: 1,
                image_id: 1,
                data: b"hello".to_vec(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert!(events2.is_empty());

        let events3 = daemon_to_bridge_events(
            DaemonMessage::ImageEnd {
                request_id: 1,
                image_id: 1,
            },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events3.len(), 1);
        match &events3[0] {
            BridgeEvent::Image { _mime, data } => {
                assert_eq!(_mime, "image/png");
                assert_eq!(data, b"hello");
            }
            other => panic!("expected Image event, got {other:?}"),
        }
    }

    #[test]
    fn test_models_event() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::Models {
                models: vec!["gpt-4".into(), "claude".into()],
                selected_model: Some("claude".into()),
            },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            BridgeEvent::Models { models, selected } => {
                assert_eq!(models, &vec!["gpt-4".to_string(), "claude".to_string()]);
                assert_eq!(selected, &Some("claude".to_string()));
            }
            other => panic!("expected Models, got {other:?}"),
        }
    }

    #[test]
    fn test_models_failed_event() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let events = daemon_to_bridge_events(
            DaemonMessage::ModelsFailed {
                error: "network error".into(),
            },
            &mut assembler,
            &mut buffers,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            BridgeEvent::Error(msg) => assert_eq!(msg, "network error"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_pong_event() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let events = daemon_to_bridge_events(DaemonMessage::Pong, &mut assembler, &mut buffers);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], BridgeEvent::Pong));
    }

    #[test]
    fn test_error_variants() {
        let mut assembler = ImageAssembler::new();
        let mut buffers = HashMap::new();

        let cases = vec![
            DaemonMessage::SessionFailed {
                operation: "attach".into(),
                error: "session error".into(),
            },
            DaemonMessage::LockedError {
                error: "already locked".into(),
            },
            DaemonMessage::ModelSelectionFailed {
                model: "gpt-4".into(),
                error: "not available".into(),
            },
        ];

        for msg in cases {
            let events = daemon_to_bridge_events(msg, &mut assembler, &mut buffers);
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0], BridgeEvent::Error(_)));
        }
    }
}
