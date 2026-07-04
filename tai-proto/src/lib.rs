mod error;

pub use error::ProtoError;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/tai.sock";
pub const SOCKET_PATH_ENV: &str = "TAI_SOCKET_PATH";
pub const MAX_IMAGE_CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssistantToolCallRecord {
    pub call_id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionStatus {
    Sleeping,
    Inactive,
    Inference,
    ToolCall(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionMessage {
    SystemText {
        content: String,
    },
    UserText {
        content: String,
    },
    AssistantText {
        content: String,
    },
    AssistantToolUse {
        content: Option<String>,
        tool_calls: Vec<AssistantToolCallRecord>,
        reasoning_content: Option<String>,
        reasoning: Option<String>,
        reasoning_text: Option<String>,
    },
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

impl SessionMessage {
    pub fn render_line(&self) -> String {
        match self {
            Self::SystemText { content } => format!("[system] {content}"),
            Self::UserText { content } => format!("> {content}"),
            Self::AssistantText { content } => format!("< {content}"),
            Self::AssistantToolUse {
                content,
                tool_calls,
                ..
            } => {
                let calls = tool_calls
                    .iter()
                    .map(|call| format!("{}({})", call.name, call.arguments_json))
                    .collect::<Vec<_>>()
                    .join(", ");
                match content
                    .as_deref()
                    .map(str::trim)
                    .filter(|content| !content.is_empty())
                {
                    Some(content) => format!("[tool-call] {calls} — {content}"),
                    None => format!("[tool-call] {calls}"),
                }
            }
            Self::ToolResult {
                name,
                content,
                is_error,
                ..
            } => {
                let status = if *is_error { "error" } else { "ok" };
                format!("[tool-result:{status}] {name}: {content}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: u64,
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub parent_session_id: Option<u64>,
    pub cwd: Option<String>,
    pub created_at: i64,
    pub message_count: u32,
    pub max_turns: Option<u32>,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientMessage {
    CreateSession {
        title: Option<String>,
        parent_session_id: Option<u64>,
        cwd: Option<String>,
        max_turns: Option<u32>,
    },
    ListSessions,
    SubscribeSessionsSummary,
    UnsubscribeSessionsSummary,
    AttachSession {
        session_id: u64,
    },
    GetSessionState {
        session_id: u64,
    },
    RunInput {
        request_id: u32,
        input: Vec<u8>,
    },
    TestImage {
        request_id: u32,
    },
    Cancel {
        request_id: u32,
    },
    Ping,
    GetCredential {
        service: String,
    },
    ListModels,
    SetModel {
        model: String,
    },
    Unlock {
        passphrase: String,
    },
    Lock,
    AddApiKey {
        service: String,
        passphrase: String,
        key: String,
    },
    AddXCredential {
        service: String,
        passphrase: String,
        api_key: String,
        api_key_secret: String,
        access_token: String,
        access_token_secret: String,
        bearer_token: Option<String>,
    },
    RemoveCredential {
        service: String,
        passphrase: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutputStream {
    Answer,
    Reasoning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageMetadata {
    pub image_id: u32,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DaemonMessage {
    SessionCreated {
        session_id: u64,
        title: Option<String>,
        parent_session_id: Option<u64>,
        cwd: Option<String>,
        max_turns: Option<u32>,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    SessionAttached {
        session_id: u64,
    },
    SessionState {
        session_id: u64,
        title: Option<String>,
        selected_model: Option<String>,
        parent_session_id: Option<u64>,
        cwd: Option<String>,
        max_turns: Option<u32>,
        messages: Vec<SessionMessage>,
    },
    SessionMessageAppended {
        message: SessionMessage,
    },
    SessionStatusChanged {
        session_id: u64,
        status: SessionStatus,
    },
    SessionFailed {
        operation: String,
        error: String,
    },
    Started {
        request_id: u32,
    },
    ToolCallStarted {
        request_id: u32,
        call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    ToolCallFinished {
        request_id: u32,
        call_id: String,
        tool_name: String,
        output: String,
    },
    ToolCallFailed {
        request_id: u32,
        call_id: String,
        tool_name: String,
        error: String,
    },
    OutputChunk {
        request_id: u32,
        stream: OutputStream,
        data: Vec<u8>,
    },
    ImageStart {
        request_id: u32,
        metadata: ImageMetadata,
    },
    ImageChunk {
        request_id: u32,
        image_id: u32,
        data: Vec<u8>,
    },
    ImageEnd {
        request_id: u32,
        image_id: u32,
    },
    Done {
        request_id: u32,
    },
    Failed {
        request_id: u32,
        error: String,
    },
    Cancelled {
        request_id: u32,
    },
    Pong,
    Models {
        models: Vec<String>,
        selected_model: Option<String>,
    },
    ModelsFailed {
        error: String,
    },
    ModelSelected {
        model: String,
    },
    ModelSelectionFailed {
        model: String,
        error: String,
    },
    Unlocked,
    Locked,
    LockedError {
        error: String,
    },
    CredentialAdded {
        service: String,
    },
    CredentialAddFailed {
        service: String,
        error: String,
    },
    CredentialRemoved {
        service: String,
    },
    CredentialRemoveFailed {
        service: String,
        error: String,
    },
    Credential {
        service: String,
        key: Option<String>,
    },
    ShuttingDown,
}

pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload =
        bincode::serde::encode_to_vec((PROTOCOL_VERSION, message), bincode::config::standard())
            .map_err(|e| ProtoError::Bincode(e.to_string()))?;

    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }

    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame<T>(payload: &[u8]) -> Result<T, ProtoError>
where
    T: for<'de> Deserialize<'de>,
{
    let ((version, message), consumed): ((u8, T), usize) =
        bincode::serde::decode_from_slice(payload, bincode::config::standard())
            .map_err(|e| ProtoError::Bincode(e.to_string()))?;

    if consumed != payload.len() {
        return Err(ProtoError::TrailingBytes);
    }

    if version != PROTOCOL_VERSION {
        return Err(ProtoError::UnsupportedVersion { version });
    }

    Ok(message)
}

pub fn socket_path() -> String {
    socket_path_impl(|| std::env::var(SOCKET_PATH_ENV).ok())
}

fn socket_path_impl(get_env: impl Fn() -> Option<String>) -> String {
    get_env().unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_string())
}

pub fn write_message_sync<W, T>(writer: &mut W, message: &T) -> Result<(), ProtoError>
where
    W: std::io::Write,
    T: Serialize,
{
    let frame = encode_frame(message)?;
    writer.write_all(&frame)?;
    Ok(())
}

pub fn read_message_sync<R, T>(reader: &mut R) -> Result<T, ProtoError>
where
    R: std::io::Read,
    T: for<'de> Deserialize<'de>,
{
    let payload = read_payload_sync(reader)?;
    decode_frame(&payload)
}

pub fn read_payload_sync<R>(reader: &mut R) -> Result<Vec<u8>, ProtoError>
where
    R: std::io::Read,
{
    let mut len_buf = [0_u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }

    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

#[cfg(test)]
mod tests;
