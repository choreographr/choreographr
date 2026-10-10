//! Daemon→client message types: the [`DaemonMessage`] envelope, the
//! [`DaemonMessageType`] payloads (the session envelope plus the flat
//! connection/reply/global messages), and the reply/global payload types they
//! carry.

use serde::{Deserialize, Serialize};

use super::client::MessageKind;
use super::common::AccountInfo;
use super::session::{SessionEvent, SessionSummary};

/// Which stream an [`SessionEvent::OutputChunk`](crate::SessionEvent::OutputChunk) belongs to: the model's visible
/// answer vs. its reasoning trace, which a client renders in separate channels.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputStream {
    /// The user-visible answer text.
    Answer,
    /// The model's reasoning/thinking trace.
    Reasoning,
}

/// Which turn attachment a [`ClientMessageType::GetImage`](crate::ClientMessageType::GetImage) / [`DaemonMessageType::Image`]
/// addresses. The two attachments are TURN-SCOPED and share one durable byte
/// store (`session_attachments`), so the fetch protocol addresses them with one
/// message and a tagged key rather than two parallel request/reply pairs. The
/// variant names the slot kind; the `session_attachments` slot name itself
/// (`d{index}` / `r{call_id}`) is an internal storage detail the wire never sees.
///
/// # Exhaustive by design
///
/// Deliberately NOT `#[non_exhaustive]`, matching [`DaemonMessage`]: the variant
/// set IS the wire contract, and each variant maps to a DISTINCT storage slot
/// (`d{index}` / `r{call_id}`) that every match site must know. Without the
/// attribute, adding a variant points the compiler at each site that needs it —
/// the daemon's key→slot mapping (`slot_for`), the client's key↔slot conversions,
/// and the size gauge — instead of letting it silently fall through a wildcard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ImageKey {
    /// A **displayed** image — the `index`-th entry of the turn's
    /// `displayed_images` (produced by `display_image`, `generate_image`, or a
    /// `retrieve_webpage` screenshot). `displayed_images` is append-only within
    /// a turn, so the positional index is a stable identifier.
    Displayed {
        /// The zero-based position in the turn's append-only
        /// `displayed_images` list.
        index: u32,
    },
    /// A **tool-result vision image** — the normalized image a tool such as
    /// `read_image` fed back to a vision model, attached to the tool result
    /// whose `call_id` this is. Keyed by call id (not a position) because the
    /// image belongs to a specific tool call and the call id is the only stable
    /// handle across turn rewrites. A turn may carry several (one per
    /// image-bearing tool result).
    ToolResult {
        /// The `call_id` of the tool call whose result produced this vision
        /// image.
        call_id: String,
    },
}

/// Metadata describing one turn attachment: its MIME type, pixel dimensions,
/// encoded byte length, and optional alt text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageMetadata {
    /// The image's MIME type (e.g. `image/png`).
    pub mime_type: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Encoded byte length of the image.
    pub byte_len: u64,
    /// Optional alt text for accessibility/display; `None` when the producer
    /// supplied none.
    pub alt: Option<String>,
}

/// Outcome of a `ClientMessageType::RefreshModels` request, reported in
/// `DaemonMessageType::ModelsRefreshed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshStatus {
    /// The conditional GET returned 304 — the cached catalog is current.
    UpToDate,
    /// The remote changed and the catalog was swapped in.
    Updated,
    /// A `--force` refresh fetched and swapped in a new catalog.
    Forced,
}

/// One provider in a `DaemonMessageType::CatalogUpdated` broadcast: the slug the
/// daemon's catalog is keyed by, plus the human-readable display name. A
/// plain wire pair — the TUI maps it into its own `ProviderInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogProvider {
    /// The provider slug the daemon's catalog is keyed by (e.g. `anthropic`).
    pub slug: String,
    /// The human-readable provider name for the picker.
    pub display_name: String,
}

/// The state of one configured MCP server, as carried in
/// [`DaemonMessageType::McpStatus`].
///
/// The same fields as the daemon's own status record
/// (`choreo-daemon`'s `mcp::McpServerStatus`), so the daemon's connection
/// handler converts one to the other field-for-field. Reports both connected
/// servers (with their advertised name/version and tool count) and servers
/// that failed or were skipped at startup (with the recorded `last_error`),
/// so a client can show the whole configured set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    /// The server's config key (its tool-name namespace, `mcp__<slug>__*`).
    pub slug: String,
    /// The configuration tier this server came from: `"daemon"` for the
    /// daemon-wide `mcp.json`, `"project"` for a session's own `.mcp.json`.
    pub tier: String,
    /// The resolved transport label (`"stdio"` / `"http"`).
    pub transport: String,
    /// The command (stdio) or URL (http) the transport targets.
    pub target: String,
    /// Whether the server is connected and its tools are registered.
    pub connected: bool,
    /// How many tools (excluding the resource catalogue tools) are registered.
    pub tool_count: usize,
    /// The server's self-reported name, once connected.
    pub server_name: Option<String>,
    /// The server's self-reported version, once connected.
    pub server_version: Option<String>,
    /// The last connect/refresh error, when the server is not connected (or a
    /// refresh failed).
    pub last_error: Option<String>,
}

impl McpServerStatus {
    /// A one-line human-readable summary of this server's state, shared by
    /// every front-end so the rendering cannot drift between them.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.connected {
            format!(
                "{} [{} → {}] connected: {} tool(s)",
                self.slug, self.transport, self.target, self.tool_count
            )
        } else {
            format!(
                "{} [{} → {}] not connected: {}",
                self.slug,
                self.transport,
                self.target,
                self.last_error.as_deref().unwrap_or("not connected")
            )
        }
    }
}

/// Authoritative daemon keystore STATUS (see [`DaemonMessageType::Keystore`]).
///
/// Three states, because "unbound" is a distinct fact from "locked": a fresh
/// daemon has no binding at all and a client must BIND it (auto-bind with a
/// freshly minted key), whereas a bound-but-locked daemon needs only an
/// `Unlock`. Collapsing the two into a single "locked" boolean is what left a
/// first-run client unable to bind the daemon (it had no key, and the wire
/// carried no signal that the keystore was unbound).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum KeystoreState {
    /// No binding exists yet — a fresh daemon. A client mints a bind key and
    /// sends [`ClientMessageType::BindKeystore`](crate::ClientMessageType::BindKeystore); the frontends do this
    /// automatically, once per connection, when they observe this state.
    Unbound,
    /// A binding exists, but no cleartext credentials are in memory. A client
    /// unlocks by presenting the bound key via [`ClientMessageType::Unlock`](crate::ClientMessageType::Unlock).
    Locked,
    /// A binding exists and the credentials are decrypted in memory.
    Unlocked,
}

/// Messages sent from the daemon to a client.
///
/// Split into two families:
/// - [`DaemonMessageType::Session`] carries a session-scoped [`SessionEvent`]
///   wrapped in an envelope that supplies the origin session — always
///   `session_id: Some(id)` for session-scoped events, present on the wire
///   as `Some(id)` — by construction. These events are delivered to
///   per-session subscribers and the all-activity fan, and the all-activity
///   dedup is keyed on the envelope's origin session.
/// - The remaining flat variants are connection/reply/global messages
///   (`Sessions`, `Pong`, `Models`, keystore and account replies,
///   `ShuttingDown`, …) that have no session scope and stay flat. A
///   connection-level reply with NO origin session (e.g. a daemon "no
///   session attached" failure) carries `session_id: None`, absent on the
///   wire as `null`.
//
// `Session` holds a `SessionEvent` by value per the agreed split design (the
// envelope hoists `session_id`, and the event must arrive flat on the wire, not
// behind indirection). Boxing it would shrink `DaemonMessage` past the
// `large_enum_variant` threshold but would also change the documented public
// shape every producer/consumer migrated to. The lint is the accepted cost of
// the honest envelope, so it stays allowed. (No `#[non_exhaustive]`: the
// variant set IS the wire contract — a new variant is a protocol bump, v5, the
// same as removing one — and without the attribute every consumer match must
// enumerate the whole set, so the compiler points at every site that needs
// revisiting when that happens.)
///
/// A daemon→client wire frame: an optional correlation `id` plus the payload
/// in [`DaemonMessageType`].
///
/// `id: Some(n)` is the targeted terminal reply to the client request `n`
/// (exactly one such reply per request, success or failure); `id: None` is a
/// broadcast, which never resolves a request. Reply-ness is a property of the
/// SEND, not the payload type: the same `inner` (e.g. `SessionState`,
/// `ReasoningEffortSet`) may be emitted with `id: Some` (a reply) or `id: None`
/// (a broadcast). A client always applies the payload's state effect, and
/// ADDITIONALLY resolves a pending request slot iff `id` is `Some`. A variant
/// is split into a separate type only when the reply carries requester-
/// relative intent that no broadcast may carry (the
/// `SessionCreatedForRequester` precedent).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonMessage {
    /// The request id this frame answers, or `None` for a broadcast. Absent
    /// from the wire when `None` (`skip_serializing_if`), so broadcasts stay
    /// compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// The payload.
    pub inner: DaemonMessageType,
}

impl DaemonMessage {
    /// Build a targeted reply to request `id`.
    #[must_use]
    pub fn reply(id: u64, inner: DaemonMessageType) -> Self {
        Self {
            id: Some(id),
            inner,
        }
    }

    /// Build a broadcast (carries no correlation id and resolves no request).
    #[must_use]
    pub fn broadcast(inner: DaemonMessageType) -> Self {
        Self { id: None, inner }
    }
}

/// The payloads carried inside a [`DaemonMessage`].
///
/// The variants are exactly the former `DaemonMessage` variants, unchanged;
/// only the envelope around them moved to the [`DaemonMessage`] struct.
#[expect(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DaemonMessageType {
    /// Single home for all session-scoped events. The envelope supplies the
    /// origin `session_id` that used to ride on each of the 31 moved
    /// variants, so every inner [`SessionEvent`] has an origin session **by
    /// construction** when `session_id` is `Some(id)` (present on the wire
    /// as `Some(id)`). Connection-level replies with no origin session (e.g.
    /// daemon "no session attached" failures) carry `session_id: None`,
    /// absent on the wire as `null`.
    Session {
        /// The origin session's id, or `None` for a connection-level reply
        /// with no session scope.
        session_id: Option<u64>,
        /// The session-scoped event this envelope carries.
        event: SessionEvent,
    },
    /// The full session list as summaries, in the shared list order (pinned
    /// first, then newest).
    Sessions {
        /// The sessions, already ordered as the daemon will render them.
        sessions: Vec<SessionSummary>,
    },
    /// Liveness reply to a [`ClientMessageType::Ping`](crate::ClientMessageType::Ping).
    Pong,
    /// The available model ids plus the currently selected one.
    Models {
        /// The available model ids.
        models: Vec<String>,
        /// The currently selected model id, if any.
        selected_model: Option<String>,
    },
    /// The model list could not be produced.
    ModelsFailed {
        /// Why the model list failed.
        error: String,
    },
    /// Targeted reply to [`ClientMessageType::Unlock`](crate::ClientMessageType::Unlock): the presented key matched
    /// the binding and the daemon decrypted its credentials. This is an
    /// OPERATION OUTCOME; the current keystore *status* is pushed separately
    /// as [`DaemonMessageType::Keystore`].
    Unlocked,
    /// Targeted reply to [`ClientMessageType::Lock`](crate::ClientMessageType::Lock): the daemon wiped its
    /// in-memory credentials and re-latched the locked state. An OPERATION
    /// OUTCOME; the current keystore *status* is pushed separately as
    /// [`DaemonMessageType::Keystore`].
    Locked,
    /// The daemon's authoritative keystore STATUS. Pushed to a client the
    /// moment it registers as an ACTIVITY subscriber and broadcast to every
    /// activity subscriber on each transition, so a client latches the real
    /// state instead of inferring it from operation replies. (A client that
    /// subscribes ONLY to session summaries does NOT receive it — a frontend
    /// that only needs the session list drives its keystore itself, e.g. an
    /// unlock-at-connect probe.) The `Unbound` push is what lets a first-run
    /// client with no key
    /// bind the daemon automatically.
    Keystore {
        /// The daemon's current keystore binding/unlock state.
        state: KeystoreState,
    },
    /// Error reply for an unlock/credential operation against a bound keystore
    /// whose presented key did not match the binding.
    LockedError {
        /// The failure reason (e.g. the presented key was wrong).
        error: String,
    },
    /// Targeted reply to [`ClientMessageType::BindKeystore`](crate::ClientMessageType::BindKeystore) when an unbound
    /// keystore adopted the presented key and the implicit unlock succeeded.
    /// Distinct from [`DaemonMessageType::Unlocked`] so the client can tell "I just
    /// created this binding" from "I verified an existing one". Ordering
    /// contract: the daemon serializes this targeted reply to the acting
    /// client's socket BEFORE the lock-state transition broadcast, so the
    /// client can key-record on this message knowing the broadcast cannot
    /// overtake it.
    Bound,
    /// Error reply for `Unlock` / `AddCredential` / `BindKeystore` against a daemon
    /// whose keystore has NO binding yet. Deliberately distinct from
    /// [`DaemonMessageType::LockedError`] (which means "bound but wrong key") so
    /// the client knows this daemon has never been bound and can AUTO-BIND it
    /// with a freshly generated key instead of replaying a stored key that
    /// can never match a nonexistent binding.
    KeystoreUnbound {
        /// The failure reason (the keystore has no binding to verify against).
        error: String,
    },
    /// Reply confirming a credential was stored for one service.
    CredentialAdded {
        /// The service key the credential was stored under.
        service: String,
    },
    /// The credential could not be stored.
    CredentialAddFailed {
        /// The service key whose credential failed to store.
        service: String,
        /// Why the credential could not be stored.
        error: String,
    },
    /// Reply confirming a stored credential was removed.
    CredentialRemoved {
        /// The service key whose credential was removed.
        service: String,
    },
    /// The stored credential could not be removed.
    CredentialRemoveFailed {
        /// The service key whose credential failed to remove.
        service: String,
        /// Why the credential could not be removed.
        error: String,
    },
    /// Reply to [`ClientMessageType::AclAdd`](crate::ClientMessageType::AclAdd): `ok` false carries the failure
    /// reason in `message` (rejected transport, bad key, I/O error); `ok`
    /// true carries the new total of authorized clients.
    AclAddResult {
        /// Whether the enrollment succeeded.
        ok: bool,
        /// On failure, the reason (rejected transport, bad key, I/O error); on
        /// success, a human-readable note.
        message: String,
    },
    /// Global broadcast (connection-level, no session) after a successful
    /// ACL change: the new total of authorized client keys. Clients that
    /// surface ACL information can refresh; it carries no key material.
    AclUpdated {
        /// The new total of authorized client keys.
        clients: u64,
    },
    /// Reply to [`ClientMessageType::GetCredential`](crate::ClientMessageType::GetCredential): the stored credential for one
    /// service, or `None` when none is stored.
    Credential {
        /// The service key the credential belongs to.
        service: String,
        /// The stored credential value, or `None` when none is stored.
        key: Option<String>,
    },
    /// Reply confirming a provider account was added.
    AccountAdded {
        /// The name of the account that was added.
        name: String,
    },
    /// The provider account could not be added.
    AccountAddFailed {
        /// The name of the account that failed to add.
        name: String,
        /// Why the account could not be added.
        error: String,
    },
    /// Reply confirming a provider account was removed.
    AccountRemoved {
        /// The name of the account that was removed.
        name: String,
    },
    /// The provider account could not be removed.
    AccountRemoveFailed {
        /// The name of the account that failed to remove.
        name: String,
        /// Why the account could not be removed.
        error: String,
    },
    /// The full list of configured provider accounts.
    Accounts {
        /// Every configured account.
        accounts: Vec<AccountInfo>,
    },
    /// The account list could not be produced.
    AccountListFailed {
        /// Why the account list could not be produced.
        error: String,
    },
    /// Reply to `ClientMessageType::RefreshModels`. `status` distinguishes
    /// "nothing changed" (304) from a real swap (200), and a forced swap.
    ModelsRefreshed {
        /// The number of providers in the refreshed catalog.
        providers: usize,
        /// The number of models in the refreshed catalog.
        models: usize,
        /// Whether the catalog was already current, was swapped, or was
        /// force-swapped.
        status: RefreshStatus,
    },
    /// Reply to `ClientMessageType::RefreshModels` when the fetch/merge failed.
    ModelsRefreshFailed {
        /// Why the refresh failed.
        error: String,
    },
    /// Broadcast whenever the daemon swaps the provider catalog (startup
    /// refresh, user-overlay reload, `/refresh-models`). Carries the full
    /// provider list so clients can replace their static default picker.
    CatalogUpdated {
        /// The full provider list replacing the client's static default.
        providers: Vec<CatalogProvider>,
    },
    /// Targeted reply to [`ClientMessageType::GetImage`](crate::ClientMessageType::GetImage): the raw bytes of the
    /// requested attachment (displayed image or tool-result vision image), or
    /// `None` when it is not found (the session or turn was deleted, the
    /// attachment was evicted, or the key is stale). `Some(vec![])` is a
    /// genuinely zero-byte image — distinct from `None` (unknown), so the client
    /// can tell "empty image" from "fetch failed" and avoid retrying a missing
    /// image forever. The `session_id`, `turn_id`, and `key` echo the request so
    /// a client with several fetches in flight can route the reply.
    Image {
        /// The session the attachment belongs to (echoes the request).
        session_id: u64,
        /// The turn the attachment belongs to (echoes the request).
        turn_id: u32,
        /// Which attachment this reply answers (echoes the request).
        key: ImageKey,
        /// The raw attachment bytes, or `None` when not found; `Some(vec![])`
        /// is a genuinely zero-byte image.
        data: Option<Vec<u8>>,
    },
    /// Reply to [`ClientMessageType::McpStatusRequest`](crate::ClientMessageType::McpStatusRequest), and the success reply to
    /// [`ClientMessageType::McpReconnect`](crate::ClientMessageType::McpReconnect): the current state of every visible
    /// MCP server (daemon-tier servers plus, for an attached session, that
    /// session's own project servers), each tagged with its `tier`. Carries
    /// the active session's project root and its trust state, plus the slugs
    /// of any project servers that were read but ignored because the root is
    /// untrusted.
    McpStatus {
        /// The current state of each visible MCP server.
        servers: Vec<McpServerStatus>,
        /// The active session's project MCP root, when it has a working
        /// directory that resolves to one.
        project_root: Option<String>,
        /// Whether `project_root` (when present) is trusted.
        project_trusted: bool,
        /// Slugs of project servers declared by an UNTRUSTED `.mcp.json` —
        /// read so the operator can see what is being ignored, never spawned.
        ignored_project_servers: Vec<String>,
    },
    /// Reply to [`ClientMessageType::McpReconnect`](crate::ClientMessageType::McpReconnect) when the reconnect failed: the
    /// slug it targeted and the failure reason.
    McpReconnectFailed {
        /// The slug of the MCP server that failed to reconnect.
        slug: String,
        /// Why the reconnect failed.
        error: String,
    },
    /// Reply to [`ClientMessageType::McpReload`](crate::ClientMessageType::McpReload): a one-line human-readable summary
    /// of what the reload changed (added/removed/restarted/unchanged/failed
    /// counts) plus the refreshed state of every configured server.
    McpReloaded {
        /// A one-line summary of what the reload changed.
        summary: String,
        /// The refreshed state of every configured server.
        servers: Vec<McpServerStatus>,
    },
    /// Reply to [`ClientMessageType::McpReload`](crate::ClientMessageType::McpReload) when the reload could not run at
    /// all (the config file could not be read or parsed): the failure reason.
    McpReloadFailed {
        /// Why the reload could not run.
        error: String,
    },
    /// Reply to [`ClientMessageType::McpTrust`](crate::ClientMessageType::McpTrust) / [`ClientMessageType::McpUntrust`](crate::ClientMessageType::McpUntrust): the
    /// resulting trust state of the target root (or `None` when the active
    /// session has no resolvable project root) plus a one-line human-readable
    /// summary of what happened.
    McpTrustUpdated {
        /// The resulting trust target root, or `None` when the session has no
        /// resolvable project root.
        root: Option<String>,
        /// Whether the root is now trusted.
        trusted: bool,
        /// A one-line human-readable summary of what happened.
        message: String,
    },
    /// Reply to [`ClientMessageType::McpTrustList`](crate::ClientMessageType::McpTrustList): the trusted project MCP
    /// roots, in stable (sorted) order.
    McpTrustList {
        /// The trusted project MCP roots, in stable (sorted) order.
        roots: Vec<String>,
    },
    /// Broadcast immediately before the daemon exits, so clients can shut down
    /// cleanly instead of treating the dropped connection as a crash.
    ShuttingDown,
    /// Best-effort advisory, sent by the daemon immediately before it
    /// disconnects a client that has fallen too far behind the streaming
    /// frontier (lag eviction). Clients use it to distinguish an
    /// "evicted for lag" disconnect from a daemon crash. Best-effort: the
    /// daemon may drop the connection before this message is flushed, so
    /// clients must not treat its absence as meaningful.
    Evicted,
    /// Terminal success reply to a request whose outcome is otherwise silent
    /// (a fire-and-confirm mutation: pinned/archived flags, title/working-dir,
    /// undo/redo, subscriptions, …): the request was accepted and applied. The
    /// request's own outcome broadcast (if any) still rides `id: None`; this
    /// acknowledgement exists so every request has exactly one targeted reply.
    Accepted {
        /// The kind of the request that was accepted.
        kind: MessageKind,
    },
    /// Terminal failure reply to any request that could not be honoured, tagged
    /// with the request's [`MessageKind`] and a human-readable reason. This is
    /// the uniform failure channel: a request that used to fall through a
    /// dispatch wildcard now has somewhere concrete to report why.
    Failed {
        /// The kind of the request that failed.
        kind: MessageKind,
        /// The human-readable failure reason.
        error: String,
    },
}

impl DaemonMessageType {
    /// A stable, payload-free name for the variant, for logging and metrics.
    ///
    /// A session payload (turn text, tool arguments/results) rides
    /// [`Self::Session`]'s inner [`SessionEvent`], so this enum's `Debug`
    /// output must never be formatted into a log line or a bail string. Log
    /// this tag (with the envelope's correlation `id`) instead.
    ///
    /// Returns a `&'static str` rather than a `MessageKind`-style enum:
    /// this exists only as a log/metric tag, never a value anyone matches on,
    /// so the string is the whole contract. ([`ClientMessageType::kind`](crate::ClientMessageType::kind)
    /// returns an enum because the daemon branches on it.)
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Session { .. } => "Session",
            Self::Sessions { .. } => "Sessions",
            Self::Pong => "Pong",
            Self::Models { .. } => "Models",
            Self::ModelsFailed { .. } => "ModelsFailed",
            Self::Unlocked => "Unlocked",
            Self::Locked => "Locked",
            Self::Keystore { .. } => "Keystore",
            Self::LockedError { .. } => "LockedError",
            Self::Bound => "Bound",
            Self::KeystoreUnbound { .. } => "KeystoreUnbound",
            Self::CredentialAdded { .. } => "CredentialAdded",
            Self::CredentialAddFailed { .. } => "CredentialAddFailed",
            Self::CredentialRemoved { .. } => "CredentialRemoved",
            Self::CredentialRemoveFailed { .. } => "CredentialRemoveFailed",
            Self::AclAddResult { .. } => "AclAddResult",
            Self::AclUpdated { .. } => "AclUpdated",
            Self::Credential { .. } => "Credential",
            Self::AccountAdded { .. } => "AccountAdded",
            Self::AccountAddFailed { .. } => "AccountAddFailed",
            Self::AccountRemoved { .. } => "AccountRemoved",
            Self::AccountRemoveFailed { .. } => "AccountRemoveFailed",
            Self::Accounts { .. } => "Accounts",
            Self::AccountListFailed { .. } => "AccountListFailed",
            Self::ModelsRefreshed { .. } => "ModelsRefreshed",
            Self::ModelsRefreshFailed { .. } => "ModelsRefreshFailed",
            Self::CatalogUpdated { .. } => "CatalogUpdated",
            Self::Image { .. } => "Image",
            Self::McpStatus { .. } => "McpStatus",
            Self::McpReconnectFailed { .. } => "McpReconnectFailed",
            Self::McpReloaded { .. } => "McpReloaded",
            Self::McpReloadFailed { .. } => "McpReloadFailed",
            Self::McpTrustUpdated { .. } => "McpTrustUpdated",
            Self::McpTrustList { .. } => "McpTrustList",
            Self::ShuttingDown => "ShuttingDown",
            Self::Evicted => "Evicted",
            Self::Accepted { .. } => "Accepted",
            Self::Failed { .. } => "Failed",
        }
    }
}
