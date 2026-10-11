//! The session control-plane command enum and the request-pipeline context
//! types.
//!
//! [`SessionCommand`] is the message a session control thread consumes to
//! mutate its authoritative state; [`RequestContext`] bundles the handles a
//! request handler threads through the pipeline; [`ChildResult`] carries a
//! child run's outcome back to its parent thread.

use super::{
    Arc, AtomicUsize, ClientId, DaemonCommand, DaemonMessageType, LagLimits, PathBuf, ReplyTarget,
    ServiceCredential, SessionMcpOverlay, SessionSnapshot, SessionStatus, SessionSummary,
    SubscriberSink, TokenUsage, ToolRegistry, WarmPolicy, io, mpsc,
};

// Named only by the [`SessionState`] intra-doc link on `SessionCommand`; rustc
// cannot observe doc-link usage, so the import reads as unused to the lint.
#[expect(
    unused_imports,
    reason = "referenced only by the [`SessionState`] intra-doc link in `SessionCommand`'s docs"
)]
use super::SessionState;

#[expect(
    clippy::large_enum_variant,
    reason = "a control-plane channel message enum whose variants carry large payloads (`DaemonMessage` broadcasts, per-request snapshots) by nature; boxing every payload variant would churn every construction and match site for a human-rate channel where throughput is not the bottleneck"
)]
/// Commands sent to a session's control thread over its crossbeam command
/// channel.
///
/// The session control thread owns the authoritative [`SessionState`]; every
/// mutation of a session's model, account, title, working directory, tool
/// groups, or turn history flows through one of these messages rather than a
/// shared-lock write, so all cross-thread state changes serialize on the one
/// thread that owns the state. Request workers hold only a snapshot of that
/// state and report results back through here as well.
///
/// Several variants carry an optional `ReplyTarget` — the daemon's one-shot
/// reply channel back to the originating client. The reply axis is strictly one
/// request → exactly one terminal reply, so a `ReplyTarget` is consumed by the
/// single reply it sends (and a dropped one answers with a best-effort
/// `Failed`); the variant's unchanged `id: None` broadcast stream rides
/// alongside that targeted reply.
pub enum SessionCommand {
    /// Start a run on this session from raw user input.
    RunInput {
        /// The user input bytes for the run; the handler decodes them as UTF-8,
        /// trims, and rejects an empty prompt.
        input: Vec<u8>,
        /// The requester's reply target, when the run originated from a client
        /// request (`RunInput`/`ContinueGeneration`). The session thread sends
        /// the acceptance reply — a TARGETED `Started` on accept, a targeted
        /// `Failed` on reject — in addition to the unchanged broadcast stream.
        /// `None` for internally-requested runs (e.g. a child session's
        /// implicit continue) that have no client awaiting a reply.
        ///
        /// The run's `stream_id` is NOT carried here: the daemon assigns it on
        /// the session thread when it accepts the run, so a client never
        /// chooses one (the cross-client collision fix).
        reply: Option<ReplyTarget>,
    },
    /// Run the agent loop on a child session and deliver the result back to the
    /// parent through a one-shot channel.
    ///
    /// The caller is responsible for injecting any prompt into the child before
    /// sending this; the run only exercises whatever turns are already queued.
    /// The reply is a one-shot `std::sync::mpsc` channel (single producer,
    /// single consumer, consumed once by the blocked parent) rather than a
    /// `ReplyTarget`, because the result never reaches a wire client directly.
    RunChildInput {
        /// Optional user text to append to the child session before running.
        user_text: Option<String>,
        /// One-shot reply to the blocked parent thread with the child's
        /// [`ChildResult`], or an error if the run could not start.
        reply: std::sync::mpsc::Sender<io::Result<ChildResult>>,
    },
    /// Cancel an in-flight run, or every run when the `stream_id` is the
    /// `CANCEL_ALL` sentinel (`0`).
    Cancel {
        /// The `stream_id` to cancel; `0` cancels all active requests.
        stream_id: u64,
    },
    /// Select the model for this session.
    SetModel {
        /// The model name to select.
        model: String,
        /// The requester's reply target: `Accepted` on success,
        /// `Failed { kind: SetModel }` on rejection. The `ModelSelected` /
        /// `ModelSelectionFailed` broadcasts still fire unchanged.
        reply: Option<ReplyTarget>,
    },
    /// Update the session's status and broadcast the change to subscribers.
    StatusChanged(SessionStatus),
    /// Register a client as a subscriber so it receives this session's
    /// broadcasts.
    Attach {
        /// The attaching client's id.
        client_id: ClientId,
        /// The client's outbound writer sink that broadcasts fan out into.
        tx: SubscriberSink,
    },
    /// Remove a client's subscription when it detaches from the session.
    Detach {
        /// The detaching client's id.
        client_id: ClientId,
    },
    /// Remove a subscriber without detaching the session (used by the daemon
    /// when a client is evicted for lag or fully disconnects — the daemon
    /// knows the client's session memberships and cleans them up promptly
    /// instead of waiting for the next broadcast to notice the dead sink).
    RemoveSubscriber {
        /// The evicted or disconnected client's id whose sink should be
        /// dropped.
        client_id: ClientId,
    },
    /// Build this session's summary for the daemon's session list.
    GetSummary {
        /// One-shot reply carrying the freshly built summary.
        reply: std::sync::mpsc::Sender<SessionSummary>,
    },
    /// Signal that a request worker has finished and hand back its state
    /// snapshot for the main thread to merge and persist.
    RequestFinished {
        /// The `stream_id` of the request that finished.
        stream_id: u64,
        /// The worker's final session snapshot, merged into the authoritative
        /// state with any mid-request mutations the worker never saw preserved.
        snapshot: SessionSnapshot,
    },
    /// Route a daemon message through the main session thread's subscriber
    /// map so that workers always broadcast to the live subscriber set
    /// rather than a stale clone of it.
    Broadcast(DaemonMessageType),
    /// Mid-turn token-usage sync from the request worker.  The worker owns
    /// the live accumulation (its private session clone), so the main
    /// thread's `config.accumulated_usage` would otherwise stay at the
    /// pre-request value until `RequestFinished` — leaking stale totals into
    /// attach snapshots and session summaries for the whole turn.  Applying
    /// the worker's cumulative total here (and re-broadcasting the update
    /// from the authoritative state) keeps every consumer fresh mid-turn.
    SyncAccumulatedUsage {
        /// The worker's cumulative token total for the session so far.
        token_usage: TokenUsage,
        /// `input_tokens` from the worker's most recent provider response, for
        /// the context-window progress display.
        last_prompt_tokens: Option<u32>,
    },
    /// Rename the session.
    SetTitle {
        /// The new title (its grapheme-cluster length is validated upstream).
        title: String,
        /// The requester's reply target. Title changes are driven by the agent's
        /// `set_session_title` tool, which has no client request id, so this is
        /// `None` there; the slot exists so a client-originated title request
        /// can ack the same way as the other session mutations without another
        /// cross-crate change.
        reply: Option<ReplyTarget>,
    },
    /// Set the session working directory (authoritative state lives in the
    /// main loop, so this must be routed here rather than mutated on the
    /// request worker's throwaway copy).  Replies with the applied path once
    /// the change has been broadcast and persisted.
    SetWorkingDir {
        /// The new working directory to adopt.
        path: PathBuf,
        /// One-shot reply to the blocked `set_working_dir` tool caller (the
        /// tool's synchronous round-trip), carrying the applied path or the
        /// rejection reason.
        tool_reply: mpsc::Sender<Result<String, String>>,
        /// The client requester's reply target (see [`SessionCommand::SetTitle`]
        /// for why this is `None` on the current tool-driven path).
        reply: Option<ReplyTarget>,
    },
    /// Replace the session's MCP overlay: its private project/per-session tool
    /// wrappers plus the daemon-tier groups those project servers shadow. Pushed
    /// by the daemon command loop after it (re)resolves the session's project.
    /// Crucially NOT persisted into `active_tool_groups` — the overlay is
    /// recomputed on every working-directory change, so a moved directory can
    /// never strand a stale project group.
    SetMcpOverlay(Box<SessionMcpOverlay>),
    /// Activate tool groups on the authoritative active-group set, then
    /// reply to the caller with a summary of what changed.
    LoadTools {
        /// The tool group names to activate.
        groups: Vec<String>,
        /// One-shot reply with a human-readable summary of what changed, or a
        /// rejection reason if no group matched.
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Deactivate tool groups on the authoritative active-group set, then
    /// reply to the caller with a summary of what changed.
    UnloadTools {
        /// The tool group names to deactivate.
        groups: Vec<String>,
        /// One-shot reply with a human-readable summary of what changed, or a
        /// rejection reason if no group matched.
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Bind an inference account to this session.
    SetAccount {
        /// The account name to bind.
        name: String,
        /// The requester's reply target: `Accepted` on success. The account's
        /// existence is verified on the connection thread BEFORE this command
        /// is sent, so the session thread only ever sees the success path.
        reply: Option<ReplyTarget>,
    },
    /// Drop the session's cached provider client so it is rebuilt lazily on
    /// the next request. Sent by the daemon command loop when the keystore
    /// locks, or a credential/account is removed or reconfigured — the
    /// cached client would otherwise keep dialing with a stale (possibly
    /// revoked) key. The session registry itself stays alive: the session
    /// keeps its own cancellable scope; only the client is discarded.
    DropProvider,
    /// Push the (non-secret) provider slug the daemon now knows for this
    /// session's account, so slug-keyed static catalog facts (context window,
    /// reasoning capability) stay exact after an external account edit without
    /// waiting for the next request to rebuild the client. `None` means the
    /// account was removed — clear the recorded slug. Sent alongside
    /// [`SessionCommand::DropProvider`] by the daemon's accounts-reload path.
    SetProviderSlug {
        /// The non-secret provider slug to record, or `None` to clear it after
        /// the account was removed.
        slug: Option<String>,
    },
    /// Set this session's reasoning effort slug.
    SetReasoningEffort {
        /// The reasoning effort slug to apply (e.g. `"off"`, `"low"`,
        /// `"medium"`, `"high"`).
        effort: String,
        /// The requester's reply target: `Accepted` on success,
        /// `Failed { kind: SetReasoningEffort }` on rejection. The
        /// `ReasoningEffortSet` / `ReasoningEffortSetFailed` broadcasts still
        /// fire unchanged.
        reply: Option<ReplyTarget>,
    },
    /// Report the session's current reasoning effort slug.
    GetReasoningEffort {
        /// One-shot reply carrying the effort slug.
        reply: mpsc::Sender<String>,
    },
    /// Reply with the session's current full-state snapshot (`SessionState`) to
    /// answer a `GetSessionState` request. The requesting client does NOT attach
    /// (it just wants to look), so this is a read-only snapshot built by the
    /// authoritative session thread — the same `session_state_message` the
    /// attach push uses, so the wire shape cannot drift. The reply carries the
    /// daemon's own `io::Result` wrapper so the session thread can answer an
    /// active session directly, off the daemon command loop.
    GetState {
        /// One-shot reply carrying the `SessionState` snapshot message, or the
        /// session's `io::Error` if it is currently inactive.
        reply: mpsc::Sender<io::Result<DaemonMessageType>>,
    },
    /// Undo the most recent user-initiated turn(s). Only honoured while the
    /// session is idle; a request in flight rejects it.
    Undo {
        /// The requester's reply target: `Accepted` when turns were undone,
        /// `Failed { kind: Undo, error: "nothing to undo" }` when there was
        /// nothing to undo (closing the silent no-op gap), or
        /// `Failed { kind: Undo, error: "session is not idle" }` while a
        /// request is in flight.
        reply: Option<ReplyTarget>,
    },
    /// Redo the most recent undo, restoring exactly the turns it removed. Only
    /// honoured while the session is idle; a request in flight rejects it.
    Redo {
        /// The requester's reply target: `Accepted` when turns were restored,
        /// `Failed { kind: Redo, error: "nothing to redo" }` otherwise, or
        /// `Failed { kind: Redo, error: "session is not idle" }` while a
        /// request is in flight.
        reply: Option<ReplyTarget>,
    },
    /// Ask the session thread to persist its final state and exit. Sent by the
    /// daemon's lifecycle shutdown.
    Shutdown,
}

/// Bundles parameters that are threaded through the session/request pipeline,
/// reducing argument count and making the dependency flow explicit.
#[derive(Clone)]
pub struct RequestContext {
    /// Channel to send `SessionCommands` back to the session main loop.
    pub cmd_tx: crossbeam_channel::Sender<SessionCommand>,
    /// The session ID scoping all operations.
    pub session_id: u64,
    /// Database handle for persisting state.
    pub db: Arc<redb::Database>,
    /// The daemon's live tool catalogue. Shared as the SAME
    /// `Arc<ArcSwap<…>>` across every session and request worker, so a
    /// list-changed rebuild on the command loop (the sole writer) is visible to
    /// a live session on its next load — no restart, no session respawn.
    pub tool_registry: Arc<arc_swap::ArcSwap<ToolRegistry>>,
    /// Channel to the daemon command loop.
    pub daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    /// Daemon-wide cap on agent tool-loop iterations per request (0 = unlimited).
    pub max_turns: u32,
    /// Lag thresholds for the lossless broadcast fan-out (see `crate::broadcast`).
    /// Session threads are producers in that fan-out, so they enforce the same
    /// per-client cap and global budget as the daemon command loop.
    pub lag_limits: LagLimits,
    /// Daemon-wide backlog counter, shared with every session thread and the
    /// daemon command loop (the 6th sanctioned shared-state exception).
    pub global_lag: Arc<AtomicUsize>,
    /// The daemon's Substrate credential, plumbed to the request worker so the
    /// `content` write tools can build a signing `ChainAccount`.
    ///
    /// // TEMPORARY: this rides the Tool trait's single `x_credentials` slot
    /// (the same slot the X tools use), so only ONE credential can be active
    /// per session at a time. This is a stopgap until a proper tool→keystore
    /// credential-access system replaces it. Only populated when the
    /// `content` feature is compiled in (see daemon.rs `spawn_session`).
    pub substrate_credential: Option<ServiceCredential>,
    /// The resolved cache-warming policy for this session's account (see
    /// [`crate::cache_warm::WarmPolicy`]). Resolved once in `spawn_session`
    /// from the daemon's loaded `[cache_warming]` config plus the account's
    /// `meter`/`cache_warming`/`prompt_cache`, so no request re-reads or
    /// re-parses config. The agent loop spawns a warmer only when `mode ==
    /// Streaming`, so the default (off) policy costs nothing.
    pub warm_policy: WarmPolicy,
}

/// The outcome of a child session's run, returned to the parent thread.
pub struct ChildResult {
    /// The child's final assistant output text.
    pub output: String,
    /// Whether the run ended in an error.
    pub is_error: bool,
}
