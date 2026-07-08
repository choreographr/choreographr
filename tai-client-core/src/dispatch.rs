use crate::error::ClientError;
use tai_proto::{ClientMessage, DaemonMessage, ImageMetadata, OutputStream, SessionMessage};

pub trait DaemonMessageHandler {
    fn push_text(&mut self, text: String);
    fn push_tool_text(&mut self, _request_id: u32, text: String) {
        self.push_text(text);
    }
    fn push_session_message(&mut self, message: SessionMessage);

    /// Like `push_session_message` but inserts the message *before* the active
    /// stream for `request_id` so that tool results appear before the model's
    /// answer text that follows them.
    fn insert_session_message_before_stream(&mut self, request_id: u32, message: SessionMessage);

    fn begin_stream(&mut self, request_id: u32);
    fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str);
    fn finalize_stream(&mut self, request_id: u32);
    fn drop_request(&mut self, request_id: u32) {
        self.finalize_stream(request_id);
    }
    fn handle_image_start(
        &mut self,
        request_id: u32,
        metadata: ImageMetadata,
    ) -> Result<(), ClientError>;
    fn handle_image_chunk(
        &mut self,
        request_id: u32,
        image_id: u32,
        data: &[u8],
    ) -> Result<(), ClientError>;
    fn handle_image_end(&mut self, request_id: u32, image_id: u32) -> Result<(), ClientError>;
}

pub fn dispatch_daemon_message<H: DaemonMessageHandler>(
    handler: &mut H,
    message: DaemonMessage,
) -> Result<Option<ClientMessage>, ClientError> {
    match message {
        DaemonMessage::SessionCreated {
            session_id, title, ..
        } => {
            let label = title.unwrap_or_else(|| "untitled".to_string());
            handler.push_text(format!("[daemon] created session {session_id}: {label}"));
            Ok(None)
        }
        DaemonMessage::Sessions { sessions } => {
            if sessions.is_empty() {
                handler.push_text("[daemon] no sessions".to_string());
            } else {
                handler.push_text(format!("[daemon] sessions ({})", sessions.len()));
                for session in &sessions {
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    handler.push_text(format!(
                        "  {}: \"{title}\" ({model}) — {} messages",
                        session.session_id, session.message_count
                    ));
                }
            }
            Ok(None)
        }
        DaemonMessage::SessionAttached { session_id } => {
            handler.push_text(format!("[daemon] attached session: {session_id}"));
            Ok(None)
        }
        DaemonMessage::SessionStatusChanged { .. } => {
            Ok(None)
        }
        DaemonMessage::SessionState {
            session_id,
            title,
            selected_model,
            parent_session_id,
            cwd,
            max_turns,
            active_tool_groups: _,
            messages,
        } => {
            let title = title.unwrap_or_else(|| "untitled".to_string());
            handler.push_text(format!("[daemon] session {session_id}: {title}"));
            if let Some(model) = &selected_model {
                handler.push_text(format!("[daemon]   model: {model}"));
            }
            if let Some(parent) = parent_session_id {
                handler.push_text(format!("[daemon]   parent: {parent}"));
            }
            if let Some(cwd) = &cwd {
                handler.push_text(format!("[daemon]   cwd: {cwd}"));
            }
            if let Some(mt) = max_turns {
                handler.push_text(format!("[daemon]   max-turns: {mt}"));
            }
            handler.push_text(format!("[daemon]   {} messages", messages.len()));
            for message in messages {
                handler.push_session_message(message);
            }
            Ok(None)
        }
        DaemonMessage::SessionFailed { operation, error } => {
            handler.push_text(format!("[daemon] {operation} failed: {error}"));
            Ok(None)
        }
        DaemonMessage::SessionMessageAppended { message } => {
            handler.push_session_message(message);
            Ok(None)
        }
        DaemonMessage::Started { request_id } => {
            handler.begin_stream(request_id);
            Ok(None)
        }
        DaemonMessage::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
        } => {
            handler.push_tool_text(
                request_id,
                format!("[{request_id}] tool {tool_name}#{call_id} start {arguments_json}"),
            );
            Ok(None)
        }
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
            output,
        } => {
            handler.insert_session_message_before_stream(request_id, SessionMessage::ToolResult {
                call_id,
                name: tool_name,
                content: output,
                is_error: false,
            });
            Ok(None)
        }
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => {
            handler.push_tool_text(
                request_id,
                format!("[{request_id}] tool {tool_name}#{call_id} failed: {error}"),
            );
            Ok(None)
        }
        DaemonMessage::ToolCallOutput {
            request_id,
            call_id,
            data,
        } => {
            let text = String::from_utf8(data)?;
            handler.push_tool_text(
                request_id,
                format!("[{request_id}] tool #{call_id} output: {text}"),
            );
            Ok(None)
        }
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        } => {
            let text = String::from_utf8(data)?;
            handler.append_stream(request_id, stream, &text);
            Ok(None)
        }
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        } => {
            handler.handle_image_start(request_id, metadata)?;
            Ok(None)
        }
        DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data,
        } => {
            handler.handle_image_chunk(request_id, image_id, &data)?;
            Ok(None)
        }
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        } => {
            handler.handle_image_end(request_id, image_id)?;
            Ok(None)
        }
        DaemonMessage::Done { request_id } => {
            handler.push_text(format!("[{request_id}] done"));
            handler.drop_request(request_id);
            Ok(None)
        }
        DaemonMessage::Failed { request_id, error } => {
            handler.push_text(format!("[{request_id}] failed: {error}"));
            handler.drop_request(request_id);
            Ok(None)
        }
        DaemonMessage::Cancelled { request_id } => {
            handler.push_text(format!("[{request_id}] cancelled"));
            handler.drop_request(request_id);
            Ok(None)
        }
        DaemonMessage::Pong => {
            handler.push_text("[daemon] pong".to_string());
            Ok(None)
        }
        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if models.is_empty() {
                handler.push_text("[daemon] no models available".to_string());
            } else {
                handler.push_text(format!("[daemon] supported models ({})", models.len()));
                for model in models {
                    let prefix = if selected_model.as_deref() == Some(model.as_str()) {
                        "*"
                    } else {
                        "-"
                    };
                    handler.push_text(format!("{prefix} {model}"));
                }
            }
            Ok(None)
        }
        DaemonMessage::ModelsFailed { error } => {
            handler.push_text(format!("[daemon] models failed: {error}"));
            Ok(None)
        }
        DaemonMessage::ModelSelected { model } => {
            handler.push_text(format!("[daemon] selected model: {model}"));
            Ok(None)
        }
        DaemonMessage::ModelSelectionFailed { model, error } => {
            handler.push_text(format!("[daemon] failed to select model {model}: {error}"));
            Ok(None)
        }
        DaemonMessage::Unlocked => {
            handler.push_text("[daemon] keystore unlocked, credentials available".to_string());
            Ok(None)
        }
        DaemonMessage::Locked => {
            handler.push_text("[daemon] keystore locked, credentials cleared".to_string());
            Ok(None)
        }
        DaemonMessage::LockedError { error } => {
            handler.push_text(format!("[daemon] locked: {error}"));
            Ok(None)
        }
        DaemonMessage::CredentialAdded { service } => {
            handler.push_text(format!("[daemon] credential added: {service}"));
            Ok(None)
        }
        DaemonMessage::CredentialAddFailed { service, error } => {
            handler.push_text(format!(
                "[daemon] credential add failed ({service}): {error}"
            ));
            Ok(None)
        }
        DaemonMessage::CredentialRemoved { service } => {
            handler.push_text(format!("[daemon] credential removed: {service}"));
            Ok(None)
        }
        DaemonMessage::CredentialRemoveFailed { service, error } => {
            handler.push_text(format!(
                "[daemon] credential remove failed ({service}): {error}"
            ));
            Ok(None)
        }
        DaemonMessage::Credential { .. } => Ok(None),
        DaemonMessage::SessionDeleted { .. } => {
            // Handled upstream by the TUI before dispatch; this crate-level
            // handler just acknowledges it so the match is exhaustive.
            Ok(None)
        }
        DaemonMessage::SessionDeleteFailed { .. } => {
            // Handled upstream by the TUI before dispatch; this crate-level
            // handler just acknowledges it so the match is exhaustive.
            Ok(None)
        }
        DaemonMessage::ShuttingDown => {
            handler.push_text("[daemon] shutting down".to_string());
            Ok(None)
        }
        _ => Ok(None),
    }
}
