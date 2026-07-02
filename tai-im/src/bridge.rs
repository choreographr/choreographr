use std::collections::HashMap;
use tai_client_core::{ImageAssembler, StreamingText};
use tai_proto::{
    ClientMessage, DaemonMessage, ImageMetadata, ProtoError, read_message, write_message,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
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
    pub fn spawn<R, W>(reader: R, writer: W) -> Self
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let (client_tx, mut client_rx) = mpsc::channel::<DaemonBridgeCommand>(128);
        let (event_tx, event_rx) = mpsc::channel::<BridgeEvent>(128);
        let writer_event_tx = event_tx.clone();

        info!("spawning daemon bridge tasks");

        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(cmd) = client_rx.recv().await {
                match cmd {
                    DaemonBridgeCommand::SendMessage(msg) => {
                        debug!(?msg, "sending message to daemon");
                        if let Err(e) = write_message(&mut writer, &msg).await {
                            error!(%e, "write error, bridge writer shutting down");
                            let _ = writer_event_tx
                                .send(BridgeEvent::Error(format!("write error: {e}")))
                                .await;
                            break;
                        }
                    }
                }
            }
            info!("bridge writer task finished");
            let _ = writer.shutdown().await;
        });

        tokio::spawn(async move {
            let mut reader = reader;
            let mut assembler = ImageAssembler::new();
            let mut buffers: HashMap<u32, StreamingText> = HashMap::new();

            loop {
                match read_message::<_, DaemonMessage>(&mut reader).await {
                    Ok(msg) => {
                        debug!(?msg, "received daemon message");
                        for event in
                            daemon_to_bridge_events(msg, &mut assembler, &mut buffers)
                        {
                            if event_tx.send(event).await.is_err() {
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
                        let _ = event_tx
                            .send(BridgeEvent::Error("daemon disconnected".into()))
                            .await;
                        return;
                    }
                    Err(e) => {
                        error!(%e, "daemon read error, reader task exiting");
                        let _ = event_tx
                            .send(BridgeEvent::Error(format!("daemon error: {e}")))
                            .await;
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

    pub fn into_parts(self) -> (mpsc::Sender<DaemonBridgeCommand>, mpsc::Receiver<BridgeEvent>) {
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
            let entry = buffers.entry(request_id).or_insert_with(|| StreamingText::new(request_id));
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
        DaemonMessage::Failed {
            request_id,
            error,
        } => {
            buffers.remove(&request_id);
            events.push(BridgeEvent::Error(error));
        }
        DaemonMessage::Cancelled {
            request_id,
        } => {
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
            let _ = assembler.start(request_id, metadata);
        }
        DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data,
        } => {
            let _ = assembler.push_chunk(request_id, image_id, &data);
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
            Err(e) => events.push(BridgeEvent::Error(format!(
                "image assembly failed: {e}"
            ))),
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
        DaemonMessage::SessionCreated { .. }
        | DaemonMessage::Sessions { .. }
        | DaemonMessage::SessionAttached { .. }
        | DaemonMessage::SessionState { .. }
        | DaemonMessage::SessionMessageAppended { .. }
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
