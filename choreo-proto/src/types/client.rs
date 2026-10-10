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
    /// Create a session with the given initial metadata.
    CreateSession {
        /// Initial session title; `None` leaves it untitled for the daemon to
        /// name later.
        title: Option<String>,
        /// The session this one branches from, for a sub-session; `None` for a
        /// root session.
        parent_session_id: Option<u64>,
        /// The session's working directory; `None` inherits the daemon
        /// default.
        working_dir: Option<String>,
        /// Overrides for context-file discovery; `None` uses the default
        /// [`ContextConfig`].
        context_config: Option<ContextConfig>,
        /// The AI provider account to bind the session to; `None` uses the
        /// daemon default.
        account_name: Option<String>,
        /// The initial model id; `None` uses the account/provider default.
        selected_model: Option<String>,
        /// The initial reasoning-effort slug; `None` uses the model default.
        /// Must be one the model's capability set advertises.
        reasoning_effort: Option<String>,
    },
    /// List every session as a [`SessionSummary`](crate::SessionSummary), in the
    /// shared list order (pinned first, then newest).
    ListSessions,
    /// Subscribe this connection to session-summary broadcasts: session
    /// create/delete plus status, title, and flag changes.
    SubscribeSessionsSummary,
    /// Stop session-summary broadcasts for this connection.
    UnsubscribeSessionsSummary,
    /// Attach this connection to a session, loading its state and subscribing
    /// it to that session's events.
    AttachSession {
        /// The id of the session to attach to.
        session_id: u64,
    },
    /// Fetch a session's full state (`ClientMessageType::SessionState`
    /// payload) without attaching.
    GetSessionState {
        /// The id of the session whose state is requested.
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
        /// The raw user input bytes submitted for the attached session.
        input: Vec<u8>,
    },
    /// Cancel the run identified by `stream_id`, or whatever is active on the
    /// attached session when the `CANCEL_ALL` sentinel (`0`) is used.
    Cancel {
        /// The run's `stream_id`, or the `CANCEL_ALL` sentinel `0`.
        stream_id: u64,
    },
    /// Liveness probe; the daemon answers with
    /// [`DaemonMessageType::Pong`](crate::DaemonMessageType::Pong).
    Ping,
    /// Fetch the stored (encrypted) credential for one service.
    GetCredential {
        /// The service key whose stored credential is requested.
        service: String,
    },
    /// Request the available model ids and the currently selected model.
    ListModels,
    /// Ask the daemon to refresh the models.dev catalog from upstream: a
    /// conditional GET against the cached etag, then a catalog swap when the
    /// remote changed. `force` bypasses the etag (`Cache-Control: no-cache`).
    /// The daemon replies with `DaemonMessageType::ModelsRefreshed` (or
    /// `ModelsRefreshFailed`).
    RefreshModels {
        /// When `true`, bypass the cached etag and fetch the catalog
        /// unconditionally (`Cache-Control: no-cache`).
        force: bool,
    },
    /// Select a model for the attached session.
    SetModel {
        /// The model id to select.
        model: String,
    },
    /// Unlock the daemon's keystore by presenting the bound unlock key.
    Unlock {
        /// The 32-byte X25519 private unlock key, verified against the binding.
        private_key: Vec<u8>,
    },
    /// Wipe the daemon's in-memory credentials and re-latch the locked state.
    Lock,
    /// Establish (TOFU-bind) the daemon's keystore binding with the 32-byte
    /// X25519 private unlock key. This is the ONLY wire path that can create
    /// the binding: on an unbound keystore the daemon adopts the key (loud
    /// `KEYSTORE BOUND` log), runs the shared unlock tail (same code path as
    /// `AddCredential`'s implicit unlock), and replies
    /// [`DaemonMessageType::Bound`](crate::DaemonMessageType::Bound). On an ALREADY-bound keystore the key is
    /// verified against the binding — a mismatch is rejected with the usual
    /// wrong-key semantics (no unlock, no overwrite). Unlock and `AddCredential`
    /// are strictly VERIFY-ONLY and cannot create a binding.
    BindKeystore {
        /// The 32-byte X25519 private unlock key to adopt (or verify) as the
        /// daemon's keystore binding.
        key: Vec<u8>,
    },
    /// Store a client-side-encrypted credential, implicitly unlocking the
    /// daemon on receipt.
    AddCredential {
        /// The service key the credential is stored under.
        service: String,
        /// The client-side-encrypted credential blob to persist.
        encrypted_payload: Vec<u8>,
        // Required (not Option): the credential blob is encrypted with the
        // unlock key client-side, so the daemon must be able to
        // test-decrypt + implicitly unlock on receipt (TOFU per-daemon
        // keystore binding). An omitted key would leave an undecryptable
        // blob persisted, breaking the whole keystore.
        /// The 32-byte unlock key the blob was encrypted with, required so the
        /// daemon can test-decrypt and implicitly unlock on receipt.
        unlock_key: Vec<u8>,
    },
    /// Remove the stored credential for one service.
    RemoveCredential {
        /// The service key whose stored credential is removed.
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
        /// Base64 of the new client's 32-byte transport public key.
        pubkey: String,
    },
    /// Delete a session and its persisted turns.
    DeleteSession {
        /// The id of the session to delete.
        session_id: u64,
    },
    /// Set (or clear) the session's `pinned` flag. The daemon is the
    /// authority: it updates its metadata index, persists the flag via a
    /// read-modify-write that touches ONLY the two flag columns, and
    /// broadcasts [`SessionEvent::SessionFlagsChanged`](crate::SessionEvent::SessionFlagsChanged) to every subscriber
    /// (the requesting connection included) — that broadcast is the state
    /// update, not the acknowledgement. The requester ALSO gets exactly one
    /// targeted terminal reply: a [`DaemonMessageType::Accepted`](crate::DaemonMessageType::Accepted) on success,
    /// or a session-scoped [`SessionEvent::SessionFailed`](crate::SessionEvent::SessionFailed) on failure.
    SetSessionPinned {
        /// The id of the session whose `pinned` flag is set.
        session_id: u64,
        /// The new pinned state (`true` pins, `false` unpins).
        pinned: bool,
    },
    /// Set (or clear) the session's `archived` state. Archiving stamps the
    /// current time into the summary's `archived_at`; unarchiving clears it.
    /// Same daemon-authoritative update/broadcast/ack contract as
    /// [`ClientMessageType::SetSessionPinned`].
    SetSessionArchived {
        /// The id of the session whose archived state is set.
        session_id: u64,
        /// Whether to archive (`true`) or unarchive (`false`) the session.
        archived: bool,
    },
    /// Add a provider account (a named endpoint plus its transport tuning).
    AddAccount {
        /// The account's unique name.
        name: String,
        /// The provider the account targets (e.g. `anthropic`, `openai`).
        provider: String,
        /// Override for the provider API base URL; `None` uses the provider
        /// default.
        base_url: Option<String>,
        /// Whether to request streamed responses; `None` uses the provider
        /// default.
        streaming: Option<bool>,
        /// Maximum retry attempts on retryable errors; `None` uses the
        /// default.
        retry_max_attempts: Option<u32>,
        /// Connect timeout in seconds; `None` uses the default.
        connect_timeout_secs: Option<u64>,
        /// Per-request timeout in seconds; `None` uses the default.
        request_timeout_secs: Option<u64>,
        /// Total deadline across all retries, in seconds; `None` uses the
        /// default.
        total_timeout_secs: Option<u64>,
    },
    /// Remove a provider account by name.
    RemoveAccount {
        /// The name of the account to remove.
        name: String,
    },
    /// Request every configured account as [`AccountInfo`](crate::AccountInfo).
    ListAccounts,
    /// Bind the attached session to a provider account.
    SetSessionAccount {
        /// The account name to bind the session to.
        name: String,
    },
    /// Set the attached session's reasoning-effort slug.
    SetReasoningEffort {
        /// The reasoning-effort slug to set (must be in the model's available
        /// set).
        effort: String,
    },
    /// Request the attached session's current reasoning-effort setting.
    GetReasoningEffort,
    /// Undo the most recent turn.
    Undo,
    /// Redo the most recently undone turn.
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
    /// replies with a targeted [`DaemonMessageType::Image`](crate::DaemonMessageType::Image).
    ///
    /// The bytes are served from the daemon's durable `session_attachments`
    /// store, keyed exactly like the wire request: (`session_id`, `turn_id`,
    /// slot), where the [`ImageKey`] maps to the slot (`d{index}` for a displayed
    /// image, `r{call_id}` for a tool-result vision image). Both attachments are
    /// append-only within a turn (a new displayed image lands at the next index;
    /// a vision image is pinned to its tool call's id), so the key is a stable
    /// identifier and matches the DB slot the turn was persisted under.
    GetImage {
        /// The id of the session the attachment belongs to.
        session_id: u64,
        /// The turn the attachment belongs to.
        turn_id: u32,
        /// Which of the turn's attachments to fetch (see [`ImageKey`]).
        key: ImageKey,
    },
    /// Request the state of every configured MCP server. The daemon replies
    /// with [`DaemonMessageType::McpStatus`](crate::DaemonMessageType::McpStatus).
    McpStatusRequest,
    /// Reconnect one configured MCP server (rebuild its connection and refresh
    /// the tool catalogue), identified by its slug. On success the daemon
    /// replies with a refreshed [`DaemonMessageType::McpStatus`](crate::DaemonMessageType::McpStatus); on failure with
    /// [`DaemonMessageType::McpReconnectFailed`](crate::DaemonMessageType::McpReconnectFailed).
    McpReconnect {
        /// The slug of the MCP server to reconnect.
        slug: String,
    },
    /// Reload the MCP server configuration: reconcile the active session's
    /// project `.mcp.json` (its daemon-tier `mcp.json` and `trust.toml` are
    /// hot-reloaded by the daemon's config watcher), rebuilding the tool
    /// catalogue — without restarting the daemon. On success the daemon
    /// replies with [`DaemonMessageType::McpReloaded`](crate::DaemonMessageType::McpReloaded); when the config cannot be
    /// read or parsed, with [`DaemonMessageType::McpReloadFailed`](crate::DaemonMessageType::McpReloadFailed).
    McpReload,
    /// Trust the ACTIVE session's project MCP root: the directory containing
    /// the nearest `.mcp.json` found by walking up from the session's working
    /// directory to the git root. Trust is whole-project (one decision covers
    /// every server that root's `.mcp.json` declares) and content-agnostic (a
    /// root with no `.mcp.json` yet may be trusted). The daemon replies with
    /// [`DaemonMessageType::McpTrustUpdated`](crate::DaemonMessageType::McpTrustUpdated).
    McpTrust,
    /// Revoke trust for the ACTIVE session's project MCP root (the same root
    /// [`ClientMessageType::McpTrust`] would trust). Replies with
    /// [`DaemonMessageType::McpTrustUpdated`](crate::DaemonMessageType::McpTrustUpdated).
    McpUntrust,
    /// Request the list of currently trusted project MCP roots. Replies with
    /// [`DaemonMessageType::McpTrustList`](crate::DaemonMessageType::McpTrustList).
    McpTrustList,
    /// Subscribe this connection to the all-activity fan-out: every session's
    /// events, deduplicated across sessions.
    SubscribeAllActivity,
    /// Stop the all-activity fan-out for this connection.
    UnsubscribeAllActivity,
}

/// A coarse tag naming a request kind, one variant per [`ClientMessageType`].
///
/// The daemon surfaces it in [`DaemonMessageType::Accepted`](crate::DaemonMessageType::Accepted) /
/// [`DaemonMessageType::Failed`](crate::DaemonMessageType::Failed) and clients use it as the log/timeout key, so
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
    /// Create a new session.
    CreateSession,
    /// List all sessions.
    ListSessions,
    /// Subscribe to session-summary broadcasts.
    SubscribeSessionsSummary,
    /// Unsubscribe from session-summary broadcasts.
    UnsubscribeSessionsSummary,
    /// Attach to a session.
    AttachSession,
    /// Fetch a session's full state without attaching.
    GetSessionState,
    /// Submit user input to the attached session.
    RunInput,
    /// Cancel an in-flight run.
    Cancel,
    /// Liveness probe.
    Ping,
    /// Fetch a stored credential.
    GetCredential,
    /// List the available models.
    ListModels,
    /// Refresh the model catalog from upstream.
    RefreshModels,
    /// Select a model for the attached session.
    SetModel,
    /// Unlock the keystore.
    Unlock,
    /// Lock the keystore.
    Lock,
    /// Bind the keystore to a new key.
    BindKeystore,
    /// Store a client-encrypted credential.
    AddCredential,
    /// Remove a stored credential.
    RemoveCredential,
    /// Enroll a new client in the ACL.
    AclAdd,
    /// Delete a session.
    DeleteSession,
    /// Set a session's pinned flag.
    SetSessionPinned,
    /// Set a session's archived state.
    SetSessionArchived,
    /// Add a provider account.
    AddAccount,
    /// Remove a provider account.
    RemoveAccount,
    /// List the configured provider accounts.
    ListAccounts,
    /// Bind the attached session to a provider account.
    SetSessionAccount,
    /// Set the reasoning effort.
    SetReasoningEffort,
    /// Read the reasoning effort.
    GetReasoningEffort,
    /// Undo the last turn.
    Undo,
    /// Redo an undone turn.
    Redo,
    /// Continue generation with the daemon-supplied prompt.
    ContinueGeneration,
    /// Fetch a turn attachment's bytes.
    GetImage,
    /// Fetch MCP server status.
    McpStatusRequest,
    /// Reconnect an MCP server.
    McpReconnect,
    /// Reload the MCP configuration.
    McpReload,
    /// Trust the active session's project MCP root.
    McpTrust,
    /// Revoke trust for the active session's project MCP root.
    McpUntrust,
    /// List the trusted project MCP roots.
    McpTrustList,
    /// Subscribe to the all-activity fan-out.
    SubscribeAllActivity,
    /// Unsubscribe from the all-activity fan-out.
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
