//! Client→daemon request types: the [`ClientMessage`] envelope, the
//! [`ClientMessageType`] request payloads, and the payload-free
//! [`MessageKind`] tag that names them.

use serde::{Deserialize, Serialize};

use super::common::ContextConfig;
use super::daemon::ImageKey;

/// A client→server wire frame: a per-connection correlation `id` plus the
/// request payload in [`ClientMessageType`].
///
/// `id` is allocated by the client as a monotonic per-connection counter
/// (starting at 0, never reused for the connection's life), and the daemon
/// MUST answer with exactly one `DaemonMessage { id: Some(self.id), .. }` —
/// the terminal reply, success or failure. There is no sentinel and no
/// uncorrelated send.
///
/// The reply `id` and the stream `stream_id` are two ORTHOGONAL axes and are
/// never merged: `id` is per-connection and one-shot (exactly one reply per
/// request), while `stream_id` is per-session and drives the many streaming
/// events a single `RunInput`/`ContinueGeneration` fans out to every session
/// subscriber. Two clients may each use their own request id 0, but a stream
/// fanned to all subscribers needs an id unique in a namespace they share —
/// which is why the two cannot be the same key. The daemon, not the client,
/// assigns `stream_id`; the client learns it from the run's acceptance reply
/// (`SessionEvent::Started`) or, for a pre-acceptance cancel, uses the
/// `CANCEL_ALL` sentinel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientMessage {
    /// The per-connection request id the daemon echoes onto its reply.
    pub id: u64,
    /// The request payload.
    pub inner: ClientMessageType,
}

impl ClientMessage {
    /// Build a request frame: `inner` tagged with correlation `id`.
    #[must_use]
    pub fn request(id: u64, inner: ClientMessageType) -> Self {
        Self { id, inner }
    }
}

/// The request payloads carried inside a [`ClientMessage`].
///
/// The variants are exactly the former `ClientMessage` variants, unchanged;
/// only the envelope around them moved to the [`ClientMessage`] struct's
/// `inner` field. Every variant is a correlated request that receives exactly
/// one terminal reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClientMessageType {
    CreateSession {
        title: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<String>,
        context_config: Option<ContextConfig>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
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
    /// Submit user input to the attached session. The daemon allocates the
    /// run's `stream_id` when it accepts the request and reports it in the
    /// targeted acceptance reply and the broadcast `SessionEvent::Started`; the
    /// client does not choose it. While the run is pending, `Cancel` with the
    /// `CANCEL_ALL` sentinel (`stream_id = 0`) stops whatever is active on the
    /// attached session; once `Started` arrives the client cancels by the
    /// learned `stream_id`.
    RunInput {
        input: Vec<u8>,
    },
    Cancel {
        stream_id: u64,
    },
    Ping,
    GetCredential {
        service: String,
    },
    ListModels,
    /// Ask the daemon to refresh the models.dev catalog from upstream: a
    /// conditional GET against the cached etag, then a catalog swap when the
    /// remote changed. `force` bypasses the etag (`Cache-Control: no-cache`).
    /// The daemon replies with `DaemonMessageType::ModelsRefreshed` (or
    /// `ModelsRefreshFailed`).
    RefreshModels {
        force: bool,
    },
    SetModel {
        model: String,
    },
    Unlock {
        private_key: Vec<u8>,
    },
    Lock,
    /// Establish (TOFU-bind) the daemon's keystore binding with the 32-byte
    /// X25519 private unlock key. This is the ONLY wire path that can create
    /// the binding: on an unbound keystore the daemon adopts the key (loud
    /// `KEYSTORE BOUND` log), runs the shared unlock tail (same code path as
    /// `AddCredential`'s implicit unlock), and replies
    /// [`DaemonMessageType::Bound`]. On an ALREADY-bound keystore the key is
    /// verified against the binding — a mismatch is rejected with the usual
    /// wrong-key semantics (no unlock, no overwrite). Unlock and `AddCredential`
    /// are strictly VERIFY-ONLY and cannot create a binding.
    BindKeystore {
        key: Vec<u8>,
    },
    AddCredential {
        service: String,
        encrypted_payload: Vec<u8>,
        // Required (not Option): the credential blob is encrypted with the
        // unlock key client-side, so the daemon must be able to
        // test-decrypt + implicitly unlock on receipt (TOFU per-daemon
        // keystore binding). An omitted key would leave an undecryptable
        // blob persisted, breaking the whole keystore.
        unlock_key: Vec<u8>,
    },
    RemoveCredential {
        service: String,
    },
    /// Enroll a NEW client in the daemon's ACL (base64 of the client's
    /// 32-byte transport public key). LOCAL connections only — the daemon
    /// rejects this from TCP clients, because the person physically at the
    /// machine (or an ssh session into it) is the right approver for a
    /// trust decision, and an already-remote client must not be able to
    /// mint new trust. The daemon appends the key to
    /// `authorized_clients.toml` under a file lock; the ACL hot-reload
    /// chain makes it authoritative immediately, and an `AclUpdated`
    /// broadcast informs every connected client.
    AclAdd {
        pubkey: String,
    },
    DeleteSession {
        session_id: u64,
    },
    /// Set (or clear) the session's `pinned` flag. The daemon is the
    /// authority: it updates its metadata index, persists the flag via a
    /// read-modify-write that touches ONLY the two flag columns, and
    /// broadcasts [`SessionEvent::SessionFlagsChanged`] to every subscriber
    /// (the requesting connection included) — that broadcast is the state
    /// update, not the acknowledgement. The requester ALSO gets exactly one
    /// targeted terminal reply: a [`DaemonMessageType::Accepted`] on success,
    /// or a session-scoped [`SessionEvent::SessionFailed`] on failure.
    SetSessionPinned {
        session_id: u64,
        pinned: bool,
    },
    /// Set (or clear) the session's `archived` state. Archiving stamps the
    /// current time into the summary's `archived_at`; unarchiving clears it.
    /// Same daemon-authoritative update/broadcast/ack contract as
    /// [`ClientMessageType::SetSessionPinned`].
    SetSessionArchived {
        session_id: u64,
        archived: bool,
    },
    AddAccount {
        name: String,
        provider: String,
        base_url: Option<String>,
        streaming: Option<bool>,
        retry_max_attempts: Option<u32>,
        connect_timeout_secs: Option<u64>,
        request_timeout_secs: Option<u64>,
        total_timeout_secs: Option<u64>,
    },
    RemoveAccount {
        name: String,
    },
    ListAccounts,
    SetSessionAccount {
        name: String,
    },
    SetReasoningEffort {
        effort: String,
    },
    GetReasoningEffort,
    Undo,
    Redo,
    /// Create a new turn with the text "Continue." and run the agent loop.
    /// Semantically distinct from `RunInput` — the daemon controls the prompt
    /// text. Like `RunInput`, the daemon assigns the run's `stream_id`.
    ContinueGeneration,
    /// Request the raw bytes of one turn attachment in `session_id` — either a
    /// displayed image or a tool-result vision image — selected by [`ImageKey`].
    ///
    /// Attachment bytes are deliberately kept OFF the session-scoped snapshots
    /// (`SessionState`, `TurnAppended`, `TurnsRedone`): displayed images carry
    /// only `ImageMetadata`, and a tool-result vision image carries only a
    /// byte-less `ImageReference` (path + mime + dimensions). A long session's
    /// history therefore ships no image bytes up front; the client fetches each
    /// image on demand — typically when it scrolls into view — and the daemon
    /// replies with a targeted [`DaemonMessageType::Image`].
    ///
    /// The bytes are served from the daemon's durable `session_attachments`
    /// store, keyed exactly like the wire request: (`session_id`, `turn_id`,
    /// slot), where the [`ImageKey`] maps to the slot (`d{index}` for a displayed
    /// image, `r{call_id}` for a tool-result vision image). Both attachments are
    /// append-only within a turn (a new displayed image lands at the next index;
    /// a vision image is pinned to its tool call's id), so the key is a stable
    /// identifier and matches the DB slot the turn was persisted under.
    GetImage {
        session_id: u64,
        turn_id: u32,
        key: ImageKey,
    },
    /// Request the state of every configured MCP server. The daemon replies
    /// with [`DaemonMessageType::McpStatus`].
    McpStatusRequest,
    /// Reconnect one configured MCP server (rebuild its connection and refresh
    /// the tool catalogue), identified by its slug. On success the daemon
    /// replies with a refreshed [`DaemonMessageType::McpStatus`]; on failure with
    /// [`DaemonMessageType::McpReconnectFailed`].
    McpReconnect {
        slug: String,
    },
    /// Reload the MCP server configuration: reconcile the active session's
    /// project `.mcp.json` (its daemon-tier `mcp.json` and `trust.toml` are
    /// hot-reloaded by the daemon's config watcher), rebuilding the tool
    /// catalogue — without restarting the daemon. On success the daemon
    /// replies with [`DaemonMessageType::McpReloaded`]; when the config cannot be
    /// read or parsed, with [`DaemonMessageType::McpReloadFailed`].
    McpReload,
    /// Trust the ACTIVE session's project MCP root: the directory containing
    /// the nearest `.mcp.json` found by walking up from the session's working
    /// directory to the git root. Trust is whole-project (one decision covers
    /// every server that root's `.mcp.json` declares) and content-agnostic (a
    /// root with no `.mcp.json` yet may be trusted). The daemon replies with
    /// [`DaemonMessageType::McpTrustUpdated`].
    McpTrust,
    /// Revoke trust for the ACTIVE session's project MCP root (the same root
    /// [`ClientMessageType::McpTrust`] would trust). Replies with
    /// [`DaemonMessageType::McpTrustUpdated`].
    McpUntrust,
    /// Request the list of currently trusted project MCP roots. Replies with
    /// [`DaemonMessageType::McpTrustList`].
    McpTrustList,
    SubscribeAllActivity,
    UnsubscribeAllActivity,
}

/// A coarse tag naming a request kind, one variant per [`ClientMessageType`].
///
/// The daemon surfaces it in [`DaemonMessageType::Accepted`] /
/// [`DaemonMessageType::Failed`] and clients use it as the log/timeout key, so
/// every request/reply exchange self-identifies without the receiver having to
/// reconstruct which request a bare success reply answered. It is `Copy`
/// because it is a pure tag carried by value.
///
/// # Why this is a separate enum, not [`ClientMessageType`] itself
///
/// It is deliberately NOT the payload enum reused as a tag. The ack rides the
/// wire back to the client (and into logs), and several request payloads carry
/// SECRET material — `Unlock { private_key }`, `BindKeystore { key }`,
/// `AddCredential { unlock_key, encrypted_payload }` — so echoing the request
/// back would put key material on the wire and in client logs. `MessageKind`
/// is the payload-free projection, `Copy` for cheap logging/timeout keys.
///
/// The two enums cannot silently drift: the [`From<&ClientMessageType>`](From)
/// impl below is exhaustive with no wildcard, so adding a [`ClientMessageType`]
/// variant fails to compile until the matching [`MessageKind`] variant and its
/// `From` arm are added. That impl is the single mapping point — edit it, not a
/// parallel list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    CreateSession,
    ListSessions,
    SubscribeSessionsSummary,
    UnsubscribeSessionsSummary,
    AttachSession,
    GetSessionState,
    RunInput,
    Cancel,
    Ping,
    GetCredential,
    ListModels,
    RefreshModels,
    SetModel,
    Unlock,
    Lock,
    BindKeystore,
    AddCredential,
    RemoveCredential,
    AclAdd,
    DeleteSession,
    SetSessionPinned,
    SetSessionArchived,
    AddAccount,
    RemoveAccount,
    ListAccounts,
    SetSessionAccount,
    SetReasoningEffort,
    GetReasoningEffort,
    Undo,
    Redo,
    ContinueGeneration,
    GetImage,
    McpStatusRequest,
    McpReconnect,
    McpReload,
    McpTrust,
    McpUntrust,
    McpTrustList,
    SubscribeAllActivity,
    UnsubscribeAllActivity,
}

impl ClientMessageType {
    /// The [`MessageKind`] tag naming this request's kind.
    #[must_use]
    pub fn kind(&self) -> MessageKind {
        MessageKind::from(self)
    }
}

impl From<&ClientMessageType> for MessageKind {
    fn from(message: &ClientMessageType) -> Self {
        match message {
            ClientMessageType::CreateSession { .. } => Self::CreateSession,
            ClientMessageType::ListSessions => Self::ListSessions,
            ClientMessageType::SubscribeSessionsSummary => Self::SubscribeSessionsSummary,
            ClientMessageType::UnsubscribeSessionsSummary => Self::UnsubscribeSessionsSummary,
            ClientMessageType::AttachSession { .. } => Self::AttachSession,
            ClientMessageType::GetSessionState { .. } => Self::GetSessionState,
            ClientMessageType::RunInput { .. } => Self::RunInput,
            ClientMessageType::Cancel { .. } => Self::Cancel,
            ClientMessageType::Ping => Self::Ping,
            ClientMessageType::GetCredential { .. } => Self::GetCredential,
            ClientMessageType::ListModels => Self::ListModels,
            ClientMessageType::RefreshModels { .. } => Self::RefreshModels,
            ClientMessageType::SetModel { .. } => Self::SetModel,
            ClientMessageType::Unlock { .. } => Self::Unlock,
            ClientMessageType::Lock => Self::Lock,
            ClientMessageType::BindKeystore { .. } => Self::BindKeystore,
            ClientMessageType::AddCredential { .. } => Self::AddCredential,
            ClientMessageType::RemoveCredential { .. } => Self::RemoveCredential,
            ClientMessageType::AclAdd { .. } => Self::AclAdd,
            ClientMessageType::DeleteSession { .. } => Self::DeleteSession,
            ClientMessageType::SetSessionPinned { .. } => Self::SetSessionPinned,
            ClientMessageType::SetSessionArchived { .. } => Self::SetSessionArchived,
            ClientMessageType::AddAccount { .. } => Self::AddAccount,
            ClientMessageType::RemoveAccount { .. } => Self::RemoveAccount,
            ClientMessageType::ListAccounts => Self::ListAccounts,
            ClientMessageType::SetSessionAccount { .. } => Self::SetSessionAccount,
            ClientMessageType::SetReasoningEffort { .. } => Self::SetReasoningEffort,
            ClientMessageType::GetReasoningEffort => Self::GetReasoningEffort,
            ClientMessageType::Undo => Self::Undo,
            ClientMessageType::Redo => Self::Redo,
            ClientMessageType::ContinueGeneration => Self::ContinueGeneration,
            ClientMessageType::GetImage { .. } => Self::GetImage,
            ClientMessageType::McpStatusRequest => Self::McpStatusRequest,
            ClientMessageType::McpReconnect { .. } => Self::McpReconnect,
            ClientMessageType::McpReload => Self::McpReload,
            ClientMessageType::McpTrust => Self::McpTrust,
            ClientMessageType::McpUntrust => Self::McpUntrust,
            ClientMessageType::McpTrustList => Self::McpTrustList,
            ClientMessageType::SubscribeAllActivity => Self::SubscribeAllActivity,
            ClientMessageType::UnsubscribeAllActivity => Self::UnsubscribeAllActivity,
        }
    }
}
