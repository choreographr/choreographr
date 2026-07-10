use serde::{Deserialize, Serialize};

pub const MAX_IMAGE_CHUNK_SIZE: usize = 64 * 1024;

/// ContextConfig — controls file discovery for session context.
/// Moved here from tai-daemon so proto messages can carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_context_file_names")]
    pub context_file_names: Vec<String>,
    #[serde(default = "default_context_file_max_bytes")]
    pub context_file_max_bytes: usize,
    #[serde(default)]
    pub disable_claude_code_prompt: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            context_file_names: default_context_file_names(),
            context_file_max_bytes: default_context_file_max_bytes(),
            disable_claude_code_prompt: false,
        }
    }
}

fn default_context_file_names() -> Vec<String> {
    vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()]
}

fn default_context_file_max_bytes() -> usize {
    32 * 1024
}

/// Unified error type for all inference providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, thiserror::Error)]
pub enum InferenceError {
    #[error("unauthorized ({status}): {detail}")]
    Unauthorized { status: u16, detail: String },
    #[error("rate limited: {detail}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },
    #[error("server error ({status}): {detail}")]
    ServerError { status: u16, detail: String },
    #[error("client error ({status}): {detail}")]
    ClientError { status: u16, detail: String },
    #[error("provider returned an empty response")]
    EmptyResponse,
    #[error("request cancelled during retry backoff")]
    Cancelled,
    #[error("{0}")]
    Io(String),
}

impl From<InferenceError> for std::io::Error {
    fn from(e: InferenceError) -> Self {
        std::io::Error::other(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountInfo {
    pub name: String,
    pub provider: String,
    pub has_credential: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayedImageRecord {
    pub metadata: ImageMetadata,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssistantToolCallRecord {
    pub call_id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionStatus {
    Sleeping,
    Inactive,
    Inference,
    ToolCall(String),
    /// The daemon received a retryable HTTP error (429/5xx/connection) and is
    /// waiting before the next attempt.  Displayed in the TUI so the user
    /// knows the model call hasn't stalled and can choose to cancel.
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionMessage {
    SystemText {
        content: String,
    },
    UserText {
        content: String,
    },
    AssistantText {
        content: String,
        reasoning: Option<String>,
    },
    AssistantToolUse {
        content: Option<String>,
        tool_calls: Vec<AssistantToolCallRecord>,
        reasoning: Option<String>,
    },
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    DisplayedImage(DisplayedImageRecord),
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
    pub active_tool_groups: Vec<String>,
    /// The AI provider account name associated with this session, if any.
    pub account_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClientMessage {
    CreateSession {
        title: Option<String>,
        parent_session_id: Option<u64>,
        cwd: Option<String>,
        max_turns: Option<u32>,
        context_config: Option<ContextConfig>,
        account_name: Option<String>,
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
        private_key: Vec<u8>,
    },
    Lock,
    AddCredential {
        service: String,
        encrypted_payload: Vec<u8>,
        unlock_key: Option<Vec<u8>>,
    },
    RemoveCredential {
        service: String,
    },
    DeleteSession {
        session_id: u64,
    },
    AddAccount {
        name: String,
        provider: String,
        base_url: Option<String>,
        streaming: Option<bool>,
        retry_max_attempts: Option<u32>,
        connect_timeout_secs: Option<u64>,
        request_timeout_secs: Option<u64>,
    },
    RemoveAccount {
        name: String,
    },
    ListAccounts,
    SetSessionAccount {
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputStream {
    Answer,
    Reasoning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageMetadata {
    /// Image identifier for the live streaming protocol (`ImageStart`/`Chunk`/`End`).
    /// Meaningless for persisted `DisplayedImage` records (always set to `0`).
    pub image_id: u32,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
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
        active_tool_groups: Vec<String>,
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
    ToolCallOutput {
        request_id: u32,
        call_id: String,
        data: Vec<u8>,
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
    SessionDeleted {
        session_id: u64,
    },
    SessionDeleteFailed {
        session_id: u64,
        error: String,
    },
    Credential {
        service: String,
        key: Option<String>,
    },
    AccountAdded {
        name: String,
    },
    AccountAddFailed {
        name: String,
        error: String,
    },
    AccountRemoved {
        name: String,
    },
    AccountRemoveFailed {
        name: String,
        error: String,
    },
    Accounts {
        accounts: Vec<AccountInfo>,
    },
    AccountListFailed {
        error: String,
    },
    SessionAccountSet {
        account: String,
    },
    ShuttingDown,
}

impl DaemonMessage {
    pub fn session_created(
        session_id: u64,
        title: Option<String>,
        parent_session_id: Option<u64>,
        cwd: Option<String>,
        max_turns: Option<u32>,
    ) -> Self {
        Self::SessionCreated {
            session_id,
            title,
            parent_session_id,
            cwd,
            max_turns,
        }
    }

    pub fn sessions(sessions: Vec<SessionSummary>) -> Self {
        Self::Sessions { sessions }
    }

    pub fn session_attached(session_id: u64) -> Self {
        Self::SessionAttached { session_id }
    }

    pub fn session_message_appended(message: SessionMessage) -> Self {
        Self::SessionMessageAppended { message }
    }

    pub fn session_status_changed(session_id: u64, status: SessionStatus) -> Self {
        Self::SessionStatusChanged { session_id, status }
    }

    pub fn session_failed(operation: impl Into<String>, error: impl Into<String>) -> Self {
        Self::SessionFailed {
            operation: operation.into(),
            error: error.into(),
        }
    }

    pub fn started(request_id: u32) -> Self {
        Self::Started { request_id }
    }

    pub fn tool_call_started(
        request_id: u32,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments_json: impl Into<String>,
    ) -> Self {
        Self::ToolCallStarted {
            request_id,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            arguments_json: arguments_json.into(),
        }
    }

    pub fn tool_call_finished(
        request_id: u32,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self::ToolCallFinished {
            request_id,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            output: output.into(),
        }
    }

    pub fn tool_call_failed(
        request_id: u32,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self::ToolCallFailed {
            request_id,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            error: error.into(),
        }
    }

    pub fn tool_call_output(request_id: u32, call_id: impl Into<String>, data: Vec<u8>) -> Self {
        Self::ToolCallOutput {
            request_id,
            call_id: call_id.into(),
            data,
        }
    }

    pub fn output_chunk(request_id: u32, stream: OutputStream, data: Vec<u8>) -> Self {
        Self::OutputChunk {
            request_id,
            stream,
            data,
        }
    }

    pub fn image_start(request_id: u32, metadata: ImageMetadata) -> Self {
        Self::ImageStart {
            request_id,
            metadata,
        }
    }

    pub fn image_chunk(request_id: u32, image_id: u32, data: Vec<u8>) -> Self {
        Self::ImageChunk {
            request_id,
            image_id,
            data,
        }
    }

    pub fn image_end(request_id: u32, image_id: u32) -> Self {
        Self::ImageEnd {
            request_id,
            image_id,
        }
    }

    pub fn done(request_id: u32) -> Self {
        Self::Done { request_id }
    }

    pub fn failed(request_id: u32, error: impl Into<String>) -> Self {
        Self::Failed {
            request_id,
            error: error.into(),
        }
    }

    pub fn cancelled(request_id: u32) -> Self {
        Self::Cancelled { request_id }
    }

    pub fn pong() -> Self {
        Self::Pong
    }

    pub fn models(models: Vec<String>, selected_model: Option<String>) -> Self {
        Self::Models {
            models,
            selected_model,
        }
    }

    pub fn models_failed(error: impl Into<String>) -> Self {
        Self::ModelsFailed {
            error: error.into(),
        }
    }

    pub fn model_selected(model: impl Into<String>) -> Self {
        Self::ModelSelected {
            model: model.into(),
        }
    }

    pub fn model_selection_failed(model: impl Into<String>, error: impl Into<String>) -> Self {
        Self::ModelSelectionFailed {
            model: model.into(),
            error: error.into(),
        }
    }

    pub fn unlocked() -> Self {
        Self::Unlocked
    }

    pub fn locked() -> Self {
        Self::Locked
    }

    pub fn locked_error(error: impl Into<String>) -> Self {
        Self::LockedError {
            error: error.into(),
        }
    }

    pub fn credential_added(service: impl Into<String>) -> Self {
        Self::CredentialAdded {
            service: service.into(),
        }
    }

    pub fn credential_add_failed(service: impl Into<String>, error: impl Into<String>) -> Self {
        Self::CredentialAddFailed {
            service: service.into(),
            error: error.into(),
        }
    }

    pub fn credential_removed(service: impl Into<String>) -> Self {
        Self::CredentialRemoved {
            service: service.into(),
        }
    }

    pub fn credential_remove_failed(service: impl Into<String>, error: impl Into<String>) -> Self {
        Self::CredentialRemoveFailed {
            service: service.into(),
            error: error.into(),
        }
    }

    pub fn session_deleted(session_id: u64) -> Self {
        Self::SessionDeleted { session_id }
    }

    pub fn session_delete_failed(session_id: u64, error: impl Into<String>) -> Self {
        Self::SessionDeleteFailed {
            session_id,
            error: error.into(),
        }
    }

    pub fn credential(service: impl Into<String>, key: Option<String>) -> Self {
        Self::Credential {
            service: service.into(),
            key,
        }
    }

    pub fn shutting_down() -> Self {
        Self::ShuttingDown
    }

    pub fn account_added(name: impl Into<String>) -> Self {
        Self::AccountAdded { name: name.into() }
    }

    pub fn account_add_failed(name: impl Into<String>, error: impl Into<String>) -> Self {
        Self::AccountAddFailed {
            name: name.into(),
            error: error.into(),
        }
    }

    pub fn account_removed(name: impl Into<String>) -> Self {
        Self::AccountRemoved { name: name.into() }
    }

    pub fn account_remove_failed(name: impl Into<String>, error: impl Into<String>) -> Self {
        Self::AccountRemoveFailed {
            name: name.into(),
            error: error.into(),
        }
    }

    pub fn accounts(accounts: Vec<AccountInfo>) -> Self {
        Self::Accounts { accounts }
    }

    pub fn account_list_failed(error: impl Into<String>) -> Self {
        Self::AccountListFailed {
            error: error.into(),
        }
    }

    pub fn session_account_set(account: impl Into<String>) -> Self {
        Self::SessionAccountSet {
            account: account.into(),
        }
    }
}

impl ClientMessage {
    pub fn create_session(
        title: Option<String>,
        parent_session_id: Option<u64>,
        cwd: Option<String>,
        max_turns: Option<u32>,
        context_config: Option<ContextConfig>,
        account_name: Option<String>,
    ) -> Self {
        Self::CreateSession {
            title,
            parent_session_id,
            cwd,
            max_turns,
            context_config,
            account_name,
        }
    }

    pub fn list_sessions() -> Self {
        Self::ListSessions
    }

    pub fn subscribe_sessions_summary() -> Self {
        Self::SubscribeSessionsSummary
    }

    pub fn unsubscribe_sessions_summary() -> Self {
        Self::UnsubscribeSessionsSummary
    }

    pub fn attach_session(session_id: u64) -> Self {
        Self::AttachSession { session_id }
    }

    pub fn get_session_state(session_id: u64) -> Self {
        Self::GetSessionState { session_id }
    }

    pub fn run_input(request_id: u32, input: Vec<u8>) -> Self {
        Self::RunInput { request_id, input }
    }

    pub fn test_image(request_id: u32) -> Self {
        Self::TestImage { request_id }
    }

    pub fn cancel(request_id: u32) -> Self {
        Self::Cancel { request_id }
    }

    pub fn ping() -> Self {
        Self::Ping
    }

    pub fn get_credential(service: impl Into<String>) -> Self {
        Self::GetCredential {
            service: service.into(),
        }
    }

    pub fn list_models() -> Self {
        Self::ListModels
    }

    pub fn set_model(model: impl Into<String>) -> Self {
        Self::SetModel {
            model: model.into(),
        }
    }

    pub fn unlock(private_key: Vec<u8>) -> Self {
        Self::Unlock { private_key }
    }

    pub fn lock() -> Self {
        Self::Lock
    }

    pub fn delete_session(session_id: u64) -> Self {
        Self::DeleteSession { session_id }
    }

    pub fn add_credential(
        service: impl Into<String>,
        encrypted_payload: Vec<u8>,
        unlock_key: Option<Vec<u8>>,
    ) -> Self {
        Self::AddCredential {
            service: service.into(),
            encrypted_payload,
            unlock_key,
        }
    }

    pub fn remove_credential(service: impl Into<String>) -> Self {
        Self::RemoveCredential {
            service: service.into(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_account(
        name: impl Into<String>,
        provider: impl Into<String>,
        base_url: Option<String>,
        streaming: Option<bool>,
        retry_max_attempts: Option<u32>,
        connect_timeout_secs: Option<u64>,
        request_timeout_secs: Option<u64>,
    ) -> Self {
        Self::AddAccount {
            name: name.into(),
            provider: provider.into(),
            base_url,
            streaming,
            retry_max_attempts,
            connect_timeout_secs,
            request_timeout_secs,
        }
    }

    pub fn remove_account(name: impl Into<String>) -> Self {
        Self::RemoveAccount { name: name.into() }
    }

    pub fn list_accounts() -> Self {
        Self::ListAccounts
    }

    pub fn set_session_account(name: impl Into<String>) -> Self {
        Self::SetSessionAccount { name: name.into() }
    }
}
