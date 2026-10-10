//! The daemon command loop: the [`DaemonCommand`] enum, the [`DaemonState`]
//! it mutates, and the single-threaded handler that dispatches between them.
//!
//! [`DaemonState`] is owned exclusively by the command-loop thread. Every
//! other thread — connection threads, session threads, config/power watchers,
//! and the detached model-prefetch and catalog-maintenance workers —
//! communicates with it only by sending a [`DaemonCommand`] over the single
//! `daemon_tx` crossbeam channel, so all daemon state is mutated through
//! [`DaemonState::handle_command`] with no shared-mutable-state lock. This is
//! the workspace's actor model (AGENTS.md "Thread Communication"); see
//! ARCHITECTURE.md's `daemon.rs` row for the module's role in the system.
//!
//! Replies travel back per-request over a channel the sender placed in the
//! command (a one-shot `std::sync::mpsc` or a crossbeam sender), never through
//! the broadcast fan-outs unless the message is a genuine client-visible event.
//! The subscriber broadcast fan-outs, the keystore handlers, the MCP command
//! handlers, and per-session overlay resolution live in child modules
//! (`daemon/subscriber_handlers.rs`, `daemon/keystore.rs`,
//! `daemon/mcp_commands.rs`, `daemon/image_provider.rs`); this module keeps the
//! state and command types plus the core command handling. `DaemonState` is
//! constructed by `daemon/open.rs`.

use crate::accounts::{AccountConfig, AccountManager, AccountOverrides};
use crate::broadcast::{ClientId, FanoutTarget, LagLimits, ReplyTarget, SubscriberSink};
use crate::cache_warm::{CacheWarmingConfig, WarmPolicy};
use crate::catalog::{CatalogPaths, MaintenanceEvent, RefreshReport, RefreshRequester};
use crate::db::{self, SessionRecord};
use crate::mcp::trust::McpTrustStore;
use crate::mcp::{McpManager, McpReloadOutcome, McpStatusReport, McpTrustOutcome};
use crate::providers::{ImageProviderHandle, InferenceProvider};
use arc_swap::ArcSwap;

// Re-export the image-resolution error type next to the DaemonCommand that
// carries it, so senders of `GetImageGenerationProvider` can name the reply
// error without reaching into the private child module.
pub use self::image_provider::ImageProviderError;
use crate::sessions::{
    ActiveSessionEntry, CANCEL_ALL, RequestContext, SessionCommand, SessionMetadata, session_main,
};
use choreo_ai_protocols::{
    SocketRegistry, bundled_overlay_src, catalog_snapshot, lookup_context_window, merge_overlay,
    replace_catalog,
};
use choreo_keystore::ServiceCredential;
use choreo_power_events::SuspendEvent;
use choreo_proto::{
    AccountInfo, CatalogProvider, ContextConfig, DaemonMessage, DaemonMessageType, RefreshStatus,
    SessionEvent, SessionStatus, SessionSummary, TimestampMs, TokenUsage,
};
pub use keystore::KeystoreOpError;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tracing::{debug, error, info, trace, warn};
use zeroize::Zeroizing;

mod image_provider;
/// Keystore command handlers (Unlock / BindKeystore / Lock / AddCredential)
/// and the TOFU binding helpers — see `daemon/keystore.rs`.
mod keystore;
mod mcp_commands;
mod open;
mod subscriber_handlers;

// Re-export the state constructor's options type alongside DaemonState so
// embedders (and the CLI) can name it without reaching into the private
// `open` module.
pub use open::OpenOptions;

/// TTL for cached provider model lists. Shared by the freshness checks in
/// `handle_list_models_inner` and the background-prefetch guard
/// (`should_prefetch_models`) so both paths agree on what "fresh" means.
const MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

/// Reply type for the `ListModels` command.
pub(super) type ListModelsReply =
    std::sync::mpsc::Sender<Result<(Vec<String>, Option<String>), String>>;

/// Reply channel for `ResolveAccountCmd`: the resolved account's ingredients
/// (see [`ResolvedAccount`]). Aliased because the reference-taking handler
/// signature otherwise trips `type_complexity`.
type ResolveAccountReply = crossbeam_channel::Sender<Option<ResolvedAccount>>;

/// The daemon's reply to `ResolveAccountCmd`: the resolved account config, its
/// API key wrapped in `Zeroizing` (wipe-on-drop — the single hop where cleartext
/// leaves the daemon), and the cache-warming policy resolved for that account
/// (the global `[cache_warming]` merged with the account overrides). Grouping
/// them means the session learns its warm policy exactly when it learns its
/// account config: the lazy first resolve, an account switch, and an accounts
/// reload all flow through this reply.
pub struct ResolvedAccount {
    /// The account's configuration.
    pub config: crate::accounts::AccountConfig,
    /// The account's API key, wipe-on-drop. `None` when no credential is stored
    /// (the config is still valid for slug/warm-policy resolution).
    pub api_key: Option<Zeroizing<String>>,
    /// The cache-warming policy resolved for this account.
    pub warm_policy: WarmPolicy,
}

/// A session's last-resolved MCP project: the project root its overlay was
/// resolved for (`None` for no project tier) and whether that root was
/// trusted at resolve time. Kept on the command loop so a working-directory
/// change OR a trust flip can release exactly the connections the session is
/// leaving, and so a trust change is never mistaken for an unchanged project.
#[derive(Debug, Clone, Default)]
pub struct SessionMcpProject {
    /// The project root the overlay was last resolved for, or `None` when the
    /// session has no project tier.
    pub root: Option<PathBuf>,
    /// Whether `root` was trusted at resolve time. Part of the identity so a
    /// trust flip on the same root counts as a project CHANGE, not an
    /// unchanged project.
    pub trusted: bool,
}

/// Everything the daemon tracks about one connected client, keyed by its
/// [`ClientId`] in [`DaemonState::clients`].
///
/// One entry per client holds everything the daemon tracks about that
/// connection: its writer sink and its subscription/membership state together,
/// so register/unregister/disconnect/evict are each a single map operation
/// rather than edits that must be kept in lockstep across sibling maps. The
/// subscription flags and the session-membership set are the "is this client
/// in class X" facts the fan-out policies key on, so the dedup checks read
/// them straight off the entry.
pub struct ClientState {
    /// The client's delivery sink: its unbounded writer channel plus the
    /// per-client in-flight byte counter (shared with the writer thread).
    pub writer: SubscriberSink,
    /// Whether this client receives session-summary broadcasts.
    pub wants_summary: bool,
    /// Whether this client receives all-activity broadcasts.
    pub wants_activity: bool,
    /// The sessions this client is a direct subscriber of. Used to suppress
    /// duplicate delivery of a session's events through the activity bus.
    pub sessions: HashSet<u64>,
}

impl ClientState {
    /// A freshly-connected client's entry: holds its writer, subscribed to
    /// nothing yet.
    fn new(writer: SubscriberSink) -> Self {
        Self {
            writer,
            wants_summary: false,
            wants_activity: false,
            sessions: HashSet::new(),
        }
    }
}

impl FanoutTarget for ClientState {
    fn sink(&self) -> &SubscriberSink {
        &self.writer
    }
}

/// All daemon state, owned exclusively by the command-loop thread.
///
/// Because exactly one thread ever touches it, every field is plain data with
/// no synchronization; mutating handlers take `&mut self` and run to completion
/// on the loop. Background threads never hold a reference — they send a
/// [`DaemonCommand`] and let the loop apply the change. A field's doc either
/// states a single-writer invariant or names the handler that maintains it.
pub struct DaemonState {
    /// The next session id to hand out; monotonic, incremented per creation.
    pub next_session_id: u64,
    /// Per-session turn ceiling applied to every spawned session thread (from
    /// the daemon config).
    pub max_turns: u32,
    /// Live session threads, keyed by session id: the command sender handed to
    /// clients/tools plus the join handle used to detect an already-finished
    /// thread at delete time.
    pub active_sessions: HashMap<u64, ActiveSessionEntry>,
    /// The daemon's in-memory session index (the summaries served to clients,
    /// plus status and flags). The authoritative view every list/get reads.
    pub session_metadata: HashMap<u64, SessionMetadata>,
    /// Sessions that have been deleted but whose session thread may still be
    /// alive (shutting down after `Cancel`/`Shutdown`).  Guards the in-memory
    /// index against straggler `UpdateMetadata` / status messages from that
    /// thread re-creating a session the user deleted, and makes
    /// `AttachSession` refuse to resurrect it while its record is still in
    /// the DB.  The marker is dropped by `handle_session_exited` once the
    /// thread's `SessionExited` arrives and the record has been deleted;
    /// on a delete failure the marker is kept (with the deletion tombstone)
    /// so the session cannot resurface until the startup purge retries.
    pub deleted_sessions: HashSet<u64>,
    /// Tracks parent→children session relationships so that cancelling or
    /// deleting a parent session also stops its child sub-sessions.
    pub children: HashMap<u64, Vec<u64>>,
    /// Named inference accounts with their config overrides. The command loop
    /// is its single writer (loaded at unlock, on `AccountsReload`, and on
    /// add/remove).
    pub accounts: AccountManager,
    /// The daemon-owned provider-socket registry, used only for provider
    /// clients that are NOT session-scoped: model prefetch and catalog
    /// maintenance fetches. Those are never individually cancelled, so they
    /// don't need a session's cancellable scope; this registry lives on the
    /// command-loop thread (the sole writer/closer) and is force-closed on
    /// suspend.
    pub daemon_registry: choreo_ai_protocols::SocketRegistry,
    /// Per-session provider-socket registries, one clone per live session
    /// (the session thread owns the sibling clone inside its
    /// `SessionState`). The daemon's clone exists so `handle_cancel_request`
    /// can force-close a wedged session's sockets from the command loop: a
    /// reader blocked in a provider `read()` cannot observe a channel
    /// message, so the close must happen here, on the thread that DECIDED
    /// the cancel. Cancellation granularity is exactly the session — other
    /// sessions' connections are untouched. Entries are created in
    /// `spawn_session` and dropped in `handle_session_exited`.
    pub session_registries: HashMap<u64, choreo_ai_protocols::SocketRegistry>,
    /// Each live session's last-resolved MCP project (root + trust state).
    /// Kept on the command loop so a working-directory change OR a trust flip
    /// that LEAVES a project can cancel that session's in-flight calls to
    /// exactly THAT project's servers, leaving its daemon-tier calls running,
    /// and so a trust change is never mistaken for an unchanged project.
    /// Entries are set in `resolve_and_push_session_overlay` /
    /// `refresh_session_overlay` and dropped in `handle_session_exited`.
    pub session_mcp_projects: HashMap<u64, SessionMcpProject>,
    /// Decrypted credentials held in memory while the keystore is unlocked,
    /// keyed by service name. Empty while locked; cleared by `/lock`.
    pub credentials: HashMap<String, ServiceCredential>,
    /// The X/Twitter credential, mirrored out of `credentials` for the `x_*`
    /// tools' single `x_credentials` slot.
    pub x_credentials: Option<ServiceCredential>,
    /// Whether the credential keystore is currently locked (no decrypted
    /// credentials in memory). Starts `true` at daemon startup — the keystore
    /// is only decrypted into memory once a valid unlock key is presented —
    /// flips to `false` on a successful Unlock / `AddCredential` implicit
    /// unlock, and back to `true` on `/lock`. This is the authoritative
    /// daemon-side lock state: it is broadcast to all activity subscribers on
    /// every transition and pushed to each fresh activity subscriber at
    /// subscribe time, so client UIs latch the real state instead of guessing.
    pub locked: bool,
    /// Whether the daemon's keystore has a binding at all (TOFU-adopted once
    /// via `BindKeystore`). Loaded from the DB at `DaemonState::open` and
    /// flipped to `true` exactly once, inside `bind_keystore` (the sole
    /// adoption path, single-writer on the command loop), so it can be read
    /// cheaply to derive the authoritative [`choreo_proto::KeystoreState`]
    /// pushed to clients — `Unbound` when there is no binding yet, else
    /// `Locked`/`Unlocked` per `self.locked`. Without this, a fresh daemon
    /// could only report "locked", never "unbound", so a first-run client
    /// with no key had no signal that it should auto-bind.
    pub keystore_bound: bool,
    /// The shared redb database handle. Every connection thread holds its own
    /// clone for concurrent reads; the command loop is the writer for
    /// daemon-owned records (sessions, credentials, the catalog etag).
    pub db: Arc<redb::Database>,
    /// The daemon's live tool catalogue, shared with every session and request
    /// worker as one `Arc<ArcSwap<…>>`. Readers load the current registry
    /// lock-free; the command loop is the SOLE writer, replacing the whole
    /// registry when an MCP server's tool list changes (`DaemonCommand::McpListChanged`)
    /// so a live list change reaches in-flight sessions without a restart.
    pub tool_registry: Arc<ArcSwap<crate::tools::ToolRegistry>>,
    /// The tool policy the registry was built under. Kept so a list-changed
    /// rebuild re-applies exactly the same registration filter.
    pub tool_policy: crate::tools::ToolPolicy,
    /// The platform-tool bridge the registry was built with (if any). Kept so a
    /// rebuild re-registers the protected `ios` group identically.
    pub platform_tool_bridge: Option<Arc<dyn crate::tools::ios_bridge::IosToolBridge>>,
    /// The command loop's own sender, cloned to every thread that needs to
    /// enqueue a [`DaemonCommand`] (session threads, connection threads,
    /// background workers, watchers).
    pub daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    /// Every connected client (both transports), keyed by its [`ClientId`],
    /// holding its delivery sink and its subscription/correlation state in ONE
    /// entry. Registered on connect and removed on disconnect/eviction; the
    /// shutdown path routes `ShuttingDown` through every entry's writer thread.
    /// Register/disconnect/evict are each a single map operation here, and the
    /// fan-out dedup checks read flags on the entry instead of cross-referencing
    /// sibling maps.
    pub clients: HashMap<ClientId, ClientState>,
    /// Daemon-wide bytes in flight to every connected client's queue, shared
    /// by ALL subscriber sinks (see `broadcast::SubscriberSink::enqueue`).
    /// The 6th sanctioned shared-state exception (see AGENTS.md); writers
    /// decrement it on every dequeue, eviction releases a client's remainder.
    pub global_lag: Arc<AtomicUsize>,
    /// Lag thresholds (per-client cap + daemon-wide budget). Injectable so
    /// tests can use tiny caps; defaults are 64 MiB / 512 MiB.
    pub lag_limits: LagLimits,
    /// Socket write timeout applied to every connection's writer thread
    /// (bounds a single blocking `write` syscall so a wedged client — receive
    /// window permanently zero — is reaped by lag eviction instead of stalling
    /// its writer forever). Injectable so a wedged-writer test can use a tiny
    /// value; production uses `crate::server::connection::WRITER_WRITE_TIMEOUT`
    /// (5 s). Read once by the transport adapters when a connection is
    /// accepted.
    pub writer_write_timeout: Duration,
    /// Cached provider model lists, keyed by account name, with the instant
    /// they were fetched. The command loop is its single writer (the
    /// [`DaemonCommand::ModelPrefetchResult`] insert); `MODEL_CACHE_TTL` is
    /// the freshness bound.
    pub model_cache: HashMap<String, (Vec<String>, Instant)>,
    /// Accounts with a model-list prefetch currently running on a background
    /// thread. The command loop sets a name when it spawns the fetch thread
    /// and clears it when the thread's `ModelPrefetchResult` arrives — the
    /// guard that keeps several session joins on the same account from
    /// spawning duplicate HTTP fetches. Managed exclusively by the command
    /// loop (single writer); the fetch threads themselves never touch it —
    /// they report back through the `daemon_tx` channel.
    pub model_prefetch_in_flight: HashSet<String>,
    /// The MCP connection manager: the daemon-tier shared servers plus every
    /// session's project/per-session pools and the tool catalogue they feed.
    /// The command loop drives its reconciliation.
    pub mcp_manager: McpManager,
    /// The MCP project-trust store (the set of project roots whose `.mcp.json`
    /// the daemon honours). Owned by the command loop (its single writer); the
    /// backing `trust.toml` is hot-reloaded via the config watcher.
    pub mcp_trust: McpTrustStore,
    /// Sender to the ONE background catalog-maintenance thread (see
    /// `crate::catalog`). `None` until `run_server` spawns the thread — a
    /// unit-test `DaemonState` has no maintenance thread, and `/refresh-models`
    /// then replies with an error instead of hanging.
    pub maintenance_tx: Option<crossbeam_channel::Sender<MaintenanceEvent>>,
    /// The hot-reloadable client ACL (see `crate::server::acl::SharedAcl`).
    /// `None` until `run_server` installs it — a unit-test `DaemonState` has
    /// no ACL file to reload, and `AclReload` then logs and no-ops instead
    /// of touching state it does not own.
    pub acl: Option<std::sync::Arc<crate::server::acl::SharedAcl>>,
    /// Filesystem locations of the runtime catalog cache + user overlay
    /// (resolved from the standard XDG dirs; see `crate::catalog`).
    pub catalog_paths: CatalogPaths,
    /// The daemon's loaded `[cache_warming]` config.toml table. Loaded ONCE at
    /// startup (see `OpenOptions::cache_warming` / `cli.rs`), then merged with
    /// each account's `meter`/`cache_warming`/`prompt_cache` in `spawn_session`
    /// to resolve that session's [`WarmPolicy`] — no request re-reads or
    /// re-parses the file.
    pub cache_warming: CacheWarmingConfig,
}

/// Every message the daemon command loop can receive.
///
/// Senders enqueue a command on the single `daemon_tx` channel and the loop
/// dispatches it in FIFO order from [`DaemonState::handle_command`]. A command
/// that expects an answer carries its own reply channel, used exactly once; a
/// client-visible event is delivered through the broadcast fan-outs instead.
/// Ordering on this one channel is load-bearing: a registration that must
/// precede a broadcast is queued ahead of it, and a targeted keystore reply is
/// enqueued before the lock-state transition broadcast it must precede.
pub enum DaemonCommand {
    /// Stop the daemon. Handled at the command-loop level (not in
    /// `handle_command`) so it can also wake the accept loop.
    Shutdown,
    /// Create a new session — a conversation container creatable while the
    /// keystore is locked — and reply with its id and command sender.
    CreateSession {
        /// Optional display title for the new session.
        title: Option<String>,
        /// Parent session to register the new session under as a child, if any.
        parent_session_id: Option<u64>,
        /// Optional initial working directory.
        working_dir: Option<PathBuf>,
        /// Optional initial reasoning-effort setting.
        reasoning_effort: Option<String>,
        /// Optional initial model selection.
        selected_model: Option<String>,
        /// Optional context-management configuration.
        context_config: Option<ContextConfig>,
        /// Optional inference account to bind the session to.
        account_name: Option<String>,
        /// Tool groups to activate at creation; the default set is used when
        /// empty.
        active_tool_groups: Vec<String>,
        /// Replies with the new session id and its command sender, or an
        /// IO error.
        reply:
            std::sync::mpsc::Sender<io::Result<(u64, crossbeam_channel::Sender<SessionCommand>)>>,
    },
    /// Ensure a stored session's thread is live and reply with its command
    /// sender (loading it from the DB if it had slept).
    AttachSession {
        /// The session to attach to.
        session_id: u64,
        /// Replies with the session's command sender, or `NotFound` when the
        /// session is unknown or deleted.
        reply: std::sync::mpsc::Sender<io::Result<crossbeam_channel::Sender<SessionCommand>>>,
    },
    /// List every known session as a summary, in the shared list order.
    ListSessions {
        /// Replies with the session summaries.
        reply: std::sync::mpsc::Sender<Vec<SessionSummary>>,
    },
    /// Fetch one session's summary by id.
    GetSession {
        /// The session to look up.
        session_id: u64,
        /// Replies with the summary, or `None` when unknown.
        reply: std::sync::mpsc::Sender<Option<SessionSummary>>,
    },
    /// Reply with the session's current full-state snapshot for a
    /// `GetSessionState` request. Ensures the session thread is live — loading
    /// it from the DB if it had slept — and forwards the reply channel straight
    /// to it, so the snapshot is built off the command loop (the connection
    /// thread is the one that blocks). A missing or deleted session answers
    /// `NotFound`.
    GetSessionState {
        /// The session whose snapshot is requested.
        session_id: u64,
        /// Replies with the session thread's full-state snapshot, or
        /// `NotFound`.
        reply: std::sync::mpsc::Sender<io::Result<DaemonMessageType>>,
    },
    /// Replace a session's in-memory metadata from its thread's snapshot,
    /// guarded against resurrecting a deleted session and against a stale
    /// timestamp or status.
    UpdateMetadata {
        /// The session the metadata belongs to.
        session_id: u64,
        /// The latest metadata snapshot from the session thread.
        metadata: SessionMetadata,
    },
    /// The session thread has terminated and persisted its final state; the
    /// loop marks the session sleeping, releases its registries and MCP refs,
    /// and finalizes any pending delete.
    SessionExited {
        /// The exited session.
        session_id: u64,
    },
    /// Sent by the background delete-finalize thread after it has removed the
    /// record of a deleted session (and cleared its tombstone).  Distinct from
    /// `SessionExited`: the record is only gone once this re-delete commits, so
    /// only this message drops the `deleted_sessions` marker.
    SessionDeleteFinalized {
        /// The session whose record the background finalize removed.
        session_id: u64,
    },
    /// Present a keystore unlock key. Verify-only against the stored binding,
    /// then run the shared unlock tail (bulk-decrypt, load accounts, clear
    /// `locked`).
    Unlock {
        /// The raw X25519 private key presented for unlock.
        private_key: Vec<u8>,
        /// The acting client's reply target. The TARGETED reply is enqueued
        /// through it DIRECTLY by the daemon command loop — before any
        /// lock-state broadcast — because routing the reply through the
        /// connection thread's mpsc handoff does not order against a broadcast
        /// enqueued by THIS thread into the same sink. See ORDERING INVARIANT
        /// in `handle_unlock`.
        reply: Option<ReplyTarget>,
        /// One-shot ACK back to the blocked connection thread; carries no data
        /// (the reply rode `reply`), only "the command loop is done".
        ack: std::sync::mpsc::Sender<()>,
    },
    /// Establish (TOFU-bind) the keystore binding. The ONLY path that can
    /// create the binding: on an unbound keystore the key is adopted (loud
    /// `KEYSTORE BOUND` log) and the shared unlock tail runs; on an
    /// already-bound keystore the key is verified against the binding and a
    /// mismatch is rejected without unlocking or overwriting.
    BindKeystore {
        /// The raw X25519 private key to adopt as the keystore binding.
        key: Vec<u8>,
        /// See `Unlock.reply` for why the targeted reply rides here.
        reply: Option<ReplyTarget>,
        /// One-shot ACK back to the blocked connection thread; see `Unlock.ack`.
        ack: mpsc::Sender<()>,
    },
    /// Lock the daemon's keystore: clear all decrypted in-memory credentials
    /// (and their cached providers) and flip `locked` back to `true`. The
    /// cleartext is wiped from memory; the encrypted blobs stay in the DB.
    /// Broadcasts the `Locked` state to all activity subscribers so every
    /// connected client's lock banner reappears. Sessions themselves are
    /// untouched — they remain browsable, only inference is disabled until the
    /// next unlock.
    Lock {
        /// Replies `Ok` once the keystore is locked and the state broadcast
        /// sent, or `Err` on failure.
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Persist an encrypted credential blob and run the implicit unlock tail.
    /// Verify-only against the binding — rebinding happens only via
    /// [`DaemonCommand::BindKeystore`].
    SaveCredential {
        /// The service name the credential is stored under.
        service: String,
        /// The credential blob, already encrypted client-side.
        encrypted_blob: Vec<u8>,
        /// REQUIRED (per-daemon keystore design): the raw X25519 private
        /// key the credential blob was encrypted with. The daemon VERIFY-ONLY
        /// checks it against the stored binding (an unbound keystore is
        /// rejected — binding happens exclusively via `BindKeystore`), uses
        /// it to test-decrypt + persist the blob, and then performs the
        /// implicit unlock (same tail as `Unlock`).
        unlock_key: Vec<u8>,
        /// See `Unlock.reply` for why the targeted replies ride here.
        reply: Option<ReplyTarget>,
        /// One-shot ACK back to the blocked connection thread; see `Unlock.ack`.
        ack: mpsc::Sender<()>,
    },
    /// Remove a stored credential by service name and invalidate the sessions
    /// bound to it.
    RemoveCredentialCmd {
        /// The service name to remove.
        service: String,
        /// Replies `Ok`, or an error string on failure.
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Enroll a client key in the ACL (from a LOCAL connection only — the
    /// transport check lives in the connection dispatch). The handler
    /// validates, appends to `authorized_clients.toml` under the advisory
    /// file lock, hot-reloads the `SharedAcl` (single writer), broadcasts
    /// `AclUpdated`, and replies with the new total.
    AclAddCmd {
        /// The base64-encoded 32-byte client public key to enroll.
        pubkey: String,
        /// Replies with the new trusted-client count, or an error string.
        reply: mpsc::Sender<Result<usize, String>>,
    },
    /// List the models available to a session's account (or, with no session,
    /// the default account context).
    ListModels {
        /// The session whose account scope applies; `None` uses the default
        /// account context.
        session_id: Option<u64>,
        /// Replies with the model names and the selected model, or an error
        /// string.
        reply: ListModelsReply,
    },
    /// A client requested `/refresh-models`. The daemon does NOT do the HTTP
    /// fetch here — it hands the request to the maintenance thread over its
    /// channel (the fetch can block for the whole 30s timeout), and the reply
    /// is routed back through [`DaemonCommand::CatalogBaseChanged`] (fetched)
    /// or [`DaemonCommand::CatalogNotModified`] (304) once the maintenance
    /// thread has a result.
    RefreshModels {
        /// Whether to force a refetch even if the cache is fresh.
        force: bool,
        /// Replies with the refresh report, or an error string.
        reply: mpsc::Sender<Result<RefreshReport, String>>,
    },
    /// The maintenance thread delivered a (possibly refreshed) models.dev
    /// base + the current user overlay. The daemon command loop — the single
    /// writer of the catalog `ArcSwap` — merges overlays, swaps the catalog,
    /// optionally persists the cache, broadcasts `CatalogUpdated`, and
    /// replies to the `/refresh-models` requester(s).
    CatalogBaseChanged {
        /// The (possibly refreshed) models.dev base provider entries.
        base: Vec<choreo_ai_protocols::ProviderEntry>,
        /// The HTTP etag for the fetched base, persisted for the next
        /// conditional GET.
        etag: Option<String>,
        /// The user overlay contents, or `None` for bundled-only. `Some`
        /// with a fresh value means the file was edited; `None` after `Some`
        /// means it was deleted.
        user_overlay: Option<String>,
        /// Persist the cache bin (file) + etag (DB) after swapping (live
        /// fetches only — a startup cache load is already on disk).
        persist: bool,
        /// Reply channel(s) for a `/refresh-models` request (empty for
        /// background events; one entry per coalesced requester, each
        /// carrying its own force flag so the reply status is individualized).
        reply: Vec<RefreshRequester>,
    },
    /// A models.dev conditional GET returned 304 — nothing changed. Routed
    /// through the command loop (rather than replied to directly by the
    /// maintenance thread) so any user-overlay reload queued just before it
    /// is applied first and the `UpToDate` counts reflect the current
    /// catalog. Carries no base: no swap happens.
    CatalogNotModified {
        /// The coalesced `/refresh-models` requesters to answer `UpToDate`.
        reply: Vec<RefreshRequester>,
    },
    /// Fetch a stored credential's API key by service name.
    GetCredential {
        /// The service name to look up.
        service: String,
        /// Replies with the API key, or `None` when locked or absent.
        reply: std::sync::mpsc::Sender<Option<String>>,
    },
    /// A background model-prefetch thread (spawned by
    /// `DaemonState::maybe_spawn_model_prefetch`) finished fetching an
    /// account's model list. Routed through the command loop — the single
    /// writer of `model_cache` — so the insert is serialized with all other
    /// cache mutations. `result` carries the fetch outcome so the loop can
    /// release the per-account in-flight guard even on failure (otherwise a
    /// failed fetch would permanently block re-prefetching that account).
    ModelPrefetchResult {
        /// The account whose model list was fetched.
        account: String,
        /// The fetch outcome: model names, or an error string. The loop
        /// releases the in-flight guard either way.
        result: Result<Vec<String>, String>,
    },
    /// Register a client to receive session-summary broadcasts.
    RegisterSummarySubscriber {
        /// The registering client.
        client_id: ClientId,
        /// The client's delivery sink.
        writer: SubscriberSink,
    },
    /// Stop a client's session-summary broadcasts.
    UnregisterSummarySubscriber {
        /// The client to unregister.
        client_id: ClientId,
    },
    /// Register a client to receive all-activity broadcasts.
    RegisterActivitySubscriber {
        /// The registering client.
        client_id: ClientId,
        /// The client's delivery sink.
        writer: SubscriberSink,
    },
    /// Stop a client's all-activity broadcasts.
    UnregisterActivitySubscriber {
        /// The client to unregister.
        client_id: ClientId,
    },
    /// Track that a client is now a direct subscriber of a session.
    /// The daemon uses this to avoid duplicate delivery through the
    /// activity subscriber path (see `handle_broadcast_activity`).
    TrackSessionSubscription {
        /// The subscribing client.
        client_id: ClientId,
        /// The session the client now subscribes to directly.
        session_id: u64,
    },
    /// Untrack that a client is no longer a direct subscriber of a session.
    UntrackSessionSubscription {
        /// The client dropping its subscription.
        client_id: ClientId,
        /// The session the client no longer subscribes to.
        session_id: u64,
    },
    /// Clean up all per-client tracking when a client disconnects: drop the
    /// client's entry from `DaemonState::clients` (its writer, subscription
    /// flags, and session memberships together) and tell each of its sessions
    /// to drop it, in a single atomic command.
    ClientDisconnected {
        /// The client that disconnected.
        client_id: ClientId,
    },
    /// Auto-exit mode (`--auto-exit`): sent by a connection thread AFTER its
    /// connection has fully ended and its RAII `ConnectionSlot` has been
    /// released (the live-connection counter decremented). Deliberately
    /// carries no data — the shutdown DECISION reads the shared counter on
    /// the command loop, keeping that decision on a single thread (connection
    /// threads only report the disconnect event; see `start_daemon_core`).
    LastClientDisconnected,
    /// Register a connection's writer channel so the shutdown path can route
    /// `ShuttingDown` through that connection's single writer thread.
    RegisterClientWriter {
        /// The client whose writer is being registered.
        client_id: ClientId,
        /// The client's writer sink, used to route `ShuttingDown`.
        writer: SubscriberSink,
    },
    /// Disconnect a client that fell too far behind its delivery queue (see
    /// `broadcast::EnqueueOutcome::ClientOverLag`). Idempotent.
    EvictClient {
        /// The client to evict.
        client_id: ClientId,
    },
    /// Disconnect the currently most-lagging client (see
    /// `broadcast::EnqueueOutcome::GlobalOverBudget`).
    EvictLargestLagging,
    /// Deliver `DaemonMessageType::ShuttingDown` to every connected client via its
    /// writer channel; each connection's writer thread then closes its own
    /// socket, so clients observe the notification before EOF.
    BroadcastShuttingDown,
    /// Fan a session-scoped or global `DaemonMessage` out to all activity
    /// subscribers with lossless + lag-eviction.
    ///
    /// `session_id` is the ORIGIN session for duplicate suppression: `Some`
    /// for session-originated broadcasts (the session thread that produced
    /// the message knows its own id), `None` for global/control broadcasts
    /// (catalog updates, models refresh, ...). The daemon consumes this
    /// field directly to skip clients that are also direct subscribers of
    /// the origin session — it no longer reverse-engineers the origin from
    /// the message shape.
    BroadcastActivity {
        /// The origin session for duplicate suppression, or `None` for a
        /// global/control broadcast. Must agree with the message's own origin.
        session_id: Option<u64>,
        /// The daemon message to fan out.
        msg: DaemonMessage,
    },
    /// Broadcast one session's status change to the summary subscribers
    /// (deduplicated against the activity path).
    BroadcastSessionStatus {
        /// The session whose status changed.
        session_id: u64,
        /// The new status.
        status: SessionStatus,
    },
    /// Delete a session and its children, shutting down the session thread.
    DeleteSession {
        /// The session to delete.
        session_id: u64,
        /// Replies `Ok`, or an IO error on failure.
        reply: std::sync::mpsc::Sender<io::Result<()>>,
    },
    /// Add a new inference account.
    AddAccountCmd {
        /// The account name.
        name: String,
        /// The provider slug/protocol key.
        provider: String,
        /// Optional base URL override.
        base_url: Option<String>,
        /// Optional streaming override.
        streaming: Option<bool>,
        /// Optional retry-attempt ceiling override.
        retry_max_attempts: Option<u32>,
        /// Optional connect-timeout override, in seconds.
        connect_timeout_secs: Option<u64>,
        /// Optional request-timeout override, in seconds.
        request_timeout_secs: Option<u64>,
        /// Optional total-timeout override, in seconds.
        total_timeout_secs: Option<u64>,
        /// Replies `Ok`, or an error string on failure.
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// Remove an inference account.
    RemoveAccountCmd {
        /// The account name to remove.
        name: String,
        /// Replies `Ok`, or an error string on failure.
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// List all inference accounts with credential status.
    ListAccountsCmd {
        /// Replies with the account info list, or an error string.
        reply: std::sync::mpsc::Sender<Result<Vec<AccountInfo>, String>>,
    },
    /// The config watcher detected an `accounts.toml` edit (or the daemon's
    /// own `add`/`remove` rewrote the file). The command loop — the single
    /// writer of `state.accounts` — re-reads, parse-compares against the
    /// in-memory manager, and applies only a real change. No reply: this is a
    /// fire-and-forget reload signal, and the sender may be absent entirely
    /// (an un-unlocked daemon has no loaded accounts to reload).
    AccountsReload,
    /// The config watcher detected an `authorized_clients.toml` edit. The
    /// command loop is the SINGLE WRITER of the client ACL (`SharedAcl`, the
    /// sanctioned `ArcSwap` exception #4): it is the one that calls
    /// `SharedAcl::reload` (re-read, parse-compare, atomic swap). The TCP
    /// accept path only ever READS lock-free snapshots. No reply:
    /// fire-and-forget, like `AccountsReload`.
    AclReload,
    /// Hand the session the raw resolution ingredients (account config +
    /// decrypted API key) so the SESSION thread can build its own client
    /// against its own socket registry. The cleartext key crosses only this
    /// per-request reply channel and never enters a cache; it is carried in a
    /// `Zeroizing<String>` so a reply that is never consumed (session dies
    /// mid-request) is wiped from the channel queue on drop instead of
    /// lingering as an ordinary `String`. `None` covers unknown account,
    /// keystore locked, and no credential stored.
    ResolveAccountCmd {
        /// The account to resolve.
        account: String,
        /// Replies with the resolved account, or `None` when the account is
        /// unknown, the keystore is locked, or no credential is stored.
        reply: crossbeam_channel::Sender<Option<ResolvedAccount>>,
    },
    /// Fetch an opaque image-generation client (plus the provider slug) for
    /// an account. The reply goes back to the TOOL thread directly over the
    /// crossbeam channel — not through the broadcast machinery — because the
    /// handle is a per-request credential-shaped value, not a client-visible
    /// event. The client is built against the REQUESTING SESSION's socket
    /// registry (looked up by `session_id`), so image sockets are
    /// cancellable with the session; when the session is already gone the
    /// daemon-owned registry is used as a fallback.
    GetImageGenerationProvider {
        /// The session requesting the image generation (socket-registry scope).
        session_id: u64,
        /// Explicit account to use; `None` selects deterministically among
        /// the image-capable credentialed accounts (sorted by account name).
        account_name: Option<String>,
        /// Replies with the image-provider handle, or a structured
        /// [`ImageProviderError`].
        reply: crossbeam_channel::Sender<Result<ImageProviderHandle, ImageProviderError>>,
    },
    /// Check whether an account with the given name exists.
    AccountExists {
        /// The account name to test.
        name: String,
        /// Replies `true` when the account exists.
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// Validate that a model is available for a session's account. Best-effort:
    /// a model is allowed through when nothing is cached to check against.
    ValidateModel {
        /// The session whose account scope applies.
        session_id: u64,
        /// The model name to validate.
        model: String,
        /// Replies `Ok`, or a guidance error string.
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Cancel the active request in a session and propagate cancellation
    /// to any child sub-sessions.  The daemon handles child propagation
    /// directly so that leaf sessions never generate unnecessary messages.
    CancelRequest {
        /// The session whose active request is cancelled.
        session_id: u64,
        /// The stream id of the request to cancel (or `CANCEL_ALL` for the
        /// whole session).
        stream_id: u64,
    },
    /// An MCP server reported a tool- or resource-list change on its
    /// `subscriptions/listen` stream. The command loop (the sole writer of the
    /// tool catalogue) rebuilds the registry from `McpManager` and swaps it in,
    /// so the server's `mcp/<slug>` group is refreshed in place. The slug is
    /// carried for logging; `tools_changed` distinguishes a TOOLS list change
    /// (which needs the catalogue rebuild) from a RESOURCES list change (which
    /// does not — the resource catalogue is read on demand, never snapshotted).
    McpListChanged {
        /// The MCP server slug that changed (carried for logging).
        slug: String,
        /// Whether the TOOLS list changed (needs a catalogue rebuild) as
        /// opposed to RESOURCES only.
        tools_changed: bool,
    },
    /// Report the state of every MCP server visible to `session_id` (daemon
    /// tier plus, when attached, that session's own project servers), tagged by
    /// tier, plus the session's project-root trust context. Read-only; replies
    /// on a plain channel so a blocked tool execution (or a connection
    /// handler) can wait for it. `session_id: None` reports only the daemon
    /// tier (the `session_inspect` diagnostic path).
    McpStatus {
        /// The session whose MCP servers to report; `None` reports only the
        /// daemon tier.
        session_id: Option<u64>,
        /// Replies with the status report.
        reply: std::sync::mpsc::Sender<McpStatusReport>,
    },
    /// Reconnect one MCP server (rebuild its connection), then rebuild the tool
    /// catalogue. Replies with the outcome, targeted to the requesting
    /// connection (or the tool caller).
    McpReconnect {
        /// The server slug to reconnect.
        slug: String,
        /// Replies `Ok`, or an error string.
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// Reconcile the MCP configuration: re-read the daemon-tier `mcp.json`
    /// (reconcile the shared servers) and, when `session_id` is `Some`, the
    /// active session's project `.mcp.json` — then rebuild the tool catalogue
    /// and push the refreshed overlay to the session. The daemon-tier `mcp.json`
    /// and `trust.toml` are also hot-reloaded by the config watcher; this
    /// command is the on-demand path (and the only one that reconciles a
    /// session's project file). Replies with the reload outcome, targeted to
    /// the requesting connection.
    McpReload {
        /// The session whose project `.mcp.json` to reconcile, or `None` for
        /// the daemon tier only.
        session_id: Option<u64>,
        /// Replies with the reload outcome, or an error string.
        reply: std::sync::mpsc::Sender<Result<McpReloadOutcome, String>>,
    },
    /// Resolve (or re-resolve) a session's MCP overlay: compute its project
    /// root from its working directory and the trust store, then ensure/release
    /// the project-shared and per-session servers. `cancel_inflight` requests
    /// that the session's in-flight calls to its previously-resolved project be
    /// stopped (a trust revocation passes `true`); the command loop ALSO
    /// cancels when it detects the change actually left that project, so a
    /// same-project working-directory change cancels nothing. A cancel is
    /// scoped to the old project's servers only — the session's daemon-tier
    /// calls keep running. The command loop resolves the overlay and PUSHES it
    /// to the session via [`SessionCommand::SetMcpOverlay`]; there is no reply
    /// (fire-and-forget).
    McpEnsureSession {
        /// The session whose overlay to resolve.
        session_id: u64,
        /// Whether to stop the session's in-flight calls to its previous
        /// project; the loop also cancels on a detected project change.
        cancel_inflight: bool,
    },
    /// Set (`trusted = true`) or revoke (`trusted = false`) trust for the
    /// active session's project root, then re-resolve the session's overlay.
    /// Replies with the resulting trust state.
    McpTrustSet {
        /// The session whose project root trust is set.
        session_id: u64,
        /// `true` to trust, `false` to revoke.
        trusted: bool,
        /// Replies with the resulting trust state.
        reply: std::sync::mpsc::Sender<McpTrustOutcome>,
    },
    /// List the trusted project roots. Read-only.
    McpTrustList {
        /// Replies with the trusted project roots.
        reply: std::sync::mpsc::Sender<Vec<PathBuf>>,
    },
    /// The config watcher detected an `mcp.json` edit. The command loop (the
    /// sole writer of the daemon-tier catalogue) re-reads and reconciles the
    /// shared servers, then rebuilds the catalogue. Fire-and-forget.
    McpTierReload,
    /// The config watcher detected a `trust.toml` edit. The command loop is the
    /// trust store's single writer: it re-reads the file and re-resolves every
    /// active session's overlay (a trust flip changes which project servers are
    /// spawned). Fire-and-forget.
    McpTrustReload,
    /// Set the display title for a session, forwarded to the session's
    /// main loop for in-memory update, broadcast, and persistence.
    SetSessionTitle {
        /// The session whose title is set.
        session_id: u64,
        /// The new title.
        title: String,
    },
    /// Set the daemon-owned `pinned`/`archived` flags of a session. `Some`
    /// requests a change to that field; `None` leaves it untouched — so the
    /// two client messages (`SetSessionPinned`/`SetSessionArchived`) share
    /// this one command. Replies `Ok(())` on success, `Err` (targeted to the
    /// requesting connection) on failure; the SUCCESS signal to other clients
    /// is the broadcast `SessionFlagsChanged`.
    SetSessionFlags {
        /// The session whose flags are set.
        session_id: u64,
        /// `Some` sets pinned, `None` leaves it untouched.
        pinned: Option<bool>,
        /// `Some` sets archived, `None` leaves it untouched.
        archived: Option<bool>,
        /// Replies `Ok`, or an IO error on failure.
        reply: std::sync::mpsc::Sender<io::Result<()>>,
    },
    /// Set the session working directory, forwarded to the session's main
    /// loop for in-memory update, broadcast, and persistence.  The session
    /// replies once the change has been applied; the daemon replies with an
    /// error immediately if the session is inactive so the caller (a blocked
    /// tool execution) never hangs.
    SetWorkingDir {
        /// The session whose working directory is set.
        session_id: u64,
        /// The new working directory.
        path: PathBuf,
        /// Replies with the applied path, or an error string.
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// A platform power transition (suspend/wake) detected by the
    /// `choreo-power-events` monitor. Delivered to the command loop by the
    /// dedicated forwarder thread (spawned in `start_daemon_core`) rather
    /// than a `select!` arm: the codebase's established pattern for external
    /// event sources (config watchers, ACL watcher) is exactly this
    /// forwarder-into-`DaemonCommand` shape, so the power monitor reuses it
    /// for one uniform delivery path. Same delivery semantics, zero special
    /// casing across the ~40 existing `DaemonCommand` senders.
    PowerEvent(SuspendEvent),
    /// Activate tool groups.  Forwarded to the session's main loop, which
    /// applies the change to the authoritative active-group set and replies
    /// with a summary of what changed.
    LoadTools {
        /// The session to activate groups on.
        session_id: u64,
        /// The tool-group names to activate.
        groups: Vec<String>,
        /// Replies with a summary of what changed, or an error string.
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Deactivate tool groups ("core" is protected).  Forwarded to the
    /// session's main loop, which applies the change and replies with a
    /// summary of what changed.
    UnloadTools {
        /// The session to deactivate groups on.
        session_id: u64,
        /// The tool-group names to deactivate.
        groups: Vec<String>,
        /// Replies with a summary of what changed, or an error string.
        reply: mpsc::Sender<Result<String, String>>,
    },
}

/// Background finalize for a deleted session whose thread has exited: remove
/// the record the thread's final `persist_and_exit` left behind, clear the
/// deletion tombstone, then confirm via `DaemonCommand::SessionDeleteFinalized`
/// so the daemon drops the `deleted_sessions` marker.  Runs on a detached
/// thread because `db::delete_session` walks every turn and kv entry — a
/// pathologically large session must not block the command loop.  On failure
/// the marker (and tombstone) stay in place so the session cannot be attached
/// or resurrected; `purge_tombstoned_sessions` at the next startup retries.
fn finalize_session_delete(
    db: &Arc<redb::Database>,
    session_id: u64,
    daemon_tx: &crossbeam_channel::Sender<DaemonCommand>,
) {
    match db::delete_session(db, session_id) {
        Ok(()) => {
            // The deletion tombstone (written by `delete_session_inner`) is no
            // longer needed now that the record is gone for good.
            if let Err(e) = db::clear_session_tombstone(db, session_id) {
                warn!(session_id, error = %e, "failed to clear session-deletion tombstone");
            }
            let _ = daemon_tx.send(DaemonCommand::SessionDeleteFinalized { session_id });
        }
        Err(e) => {
            // Keep the marker (and tombstone) so the deleted session cannot be
            // attached or resurrected; `purge_tombstoned_sessions` at the next
            // startup retries the delete.
            error!(
                session_id,
                error = %e,
                "failed to delete session record during exit finalize; keeping tombstone"
            );
        }
    }
}

/// The inputs for creating a session, grouped so
/// [`DaemonState::handle_create_session`] takes one value instead of nine
/// positional arguments (and clippy's `too_many_arguments` lint needs no
/// suppression). Mirrors the fields of [`DaemonCommand::CreateSession`] minus
/// the reply channel; every field is optional because a session can be created
/// with pure defaults.
struct CreateSessionParams {
    title: Option<String>,
    parent_session_id: Option<u64>,
    working_dir: Option<PathBuf>,
    reasoning_effort: Option<String>,
    selected_model: Option<String>,
    context_config: Option<ContextConfig>,
    account_name: Option<String>,
    active_tool_groups: Vec<String>,
}

impl DaemonState {
    /// Dispatch one [`DaemonCommand`] on the command-loop thread.
    ///
    /// The loop's sole entry point for daemon state mutation: it routes the
    /// command to the matching `handle_*` method (or the child-module handler).
    /// `Shutdown` and `LastClientDisconnected` are intentionally handled by the
    /// loop caller, not here, because they need the accept loop / connection
    /// counter that [`DaemonState`] does not own.
    pub fn handle_command(&mut self, cmd: DaemonCommand) {
        match cmd {
            DaemonCommand::CreateSession {
                title,
                parent_session_id,
                working_dir,
                reasoning_effort,
                selected_model,
                context_config,
                account_name,
                active_tool_groups,
                reply,
            } => self.handle_create_session(
                CreateSessionParams {
                    title,
                    parent_session_id,
                    working_dir,
                    reasoning_effort,
                    selected_model,
                    context_config,
                    account_name,
                    active_tool_groups,
                },
                &reply,
            ),
            DaemonCommand::AttachSession { session_id, reply } => {
                self.handle_attach_session(session_id, &reply);
            }
            DaemonCommand::ListSessions { reply } => self.handle_list_sessions(&reply),
            DaemonCommand::GetSession { session_id, reply } => {
                self.handle_get_session(session_id, &reply);
            }
            DaemonCommand::GetSessionState { session_id, reply } => {
                self.handle_get_session_state(session_id, reply);
            }
            DaemonCommand::UpdateMetadata {
                session_id,
                metadata,
            } => self.handle_update_metadata(session_id, metadata),
            DaemonCommand::SessionExited { session_id } => self.handle_session_exited(session_id),
            DaemonCommand::SessionDeleteFinalized { session_id } => {
                self.handle_session_delete_finalized(session_id);
            }
            DaemonCommand::Unlock {
                private_key,
                reply,
                ack,
            } => self.handle_unlock(private_key, reply, &ack),
            DaemonCommand::BindKeystore { key, reply, ack } => {
                self.handle_bind_keystore(key, reply, &ack);
            }
            DaemonCommand::Lock { reply } => self.handle_lock(&reply),
            DaemonCommand::SaveCredential {
                service,
                encrypted_blob,
                unlock_key,
                reply,
                ack,
            } => self.handle_save_credential(service, &encrypted_blob, unlock_key, reply, &ack),
            DaemonCommand::RemoveCredentialCmd { service, reply } => {
                self.handle_remove_credential(&service, &reply);
            }
            DaemonCommand::AclAddCmd { pubkey, reply } => self.handle_acl_add(&pubkey, &reply),
            DaemonCommand::ListModels { session_id, reply } => {
                self.handle_list_models(session_id, &reply);
            }
            DaemonCommand::RefreshModels { force, reply } => {
                self.handle_refresh_models(force, &reply);
            }
            DaemonCommand::CatalogBaseChanged {
                base,
                etag,
                user_overlay,
                persist,
                reply,
            } => self.handle_catalog_base_changed(
                &base,
                etag.as_deref(),
                user_overlay.as_deref(),
                persist,
                reply,
            ),
            DaemonCommand::CatalogNotModified { reply } => {
                Self::handle_catalog_not_modified(reply);
            }
            DaemonCommand::GetCredential { service, reply } => {
                self.handle_get_credential(&service, &reply);
            }
            DaemonCommand::ModelPrefetchResult { account, result } => {
                self.handle_model_prefetch_result(account, result);
            }
            DaemonCommand::RegisterSummarySubscriber { client_id, writer } => {
                self.handle_register_summary_subscriber(client_id, &writer);
            }
            DaemonCommand::UnregisterSummarySubscriber { client_id } => {
                self.handle_unregister_summary_subscriber(client_id);
            }
            DaemonCommand::RegisterActivitySubscriber { client_id, writer } => {
                self.handle_register_activity_subscriber(client_id, &writer);
            }
            DaemonCommand::UnregisterActivitySubscriber { client_id } => {
                self.handle_unregister_activity_subscriber(client_id);
            }
            DaemonCommand::TrackSessionSubscription {
                client_id,
                session_id,
            } => self.handle_track_session_subscription(client_id, session_id),
            DaemonCommand::UntrackSessionSubscription {
                client_id,
                session_id,
            } => self.handle_untrack_session_subscription(client_id, session_id),
            DaemonCommand::ClientDisconnected { client_id } => {
                self.handle_client_disconnected(client_id);
            }
            DaemonCommand::RegisterClientWriter { client_id, writer } => {
                self.handle_register_client_writer(client_id, writer);
            }
            DaemonCommand::EvictClient { client_id } => self.handle_evict_client(client_id),
            DaemonCommand::EvictLargestLagging => self.handle_evict_largest_lagging(),
            DaemonCommand::BroadcastShuttingDown => self.handle_broadcast_shutting_down(),
            DaemonCommand::BroadcastActivity { session_id, msg } => {
                self.handle_broadcast_activity(session_id, &msg);
            }
            DaemonCommand::BroadcastSessionStatus { session_id, status } => {
                self.handle_broadcast_session_status(session_id, status);
            }
            DaemonCommand::DeleteSession { session_id, reply } => {
                self.handle_delete_session(session_id, &reply);
            }
            DaemonCommand::AddAccountCmd {
                name,
                provider,
                base_url,
                streaming,
                retry_max_attempts,
                connect_timeout_secs,
                request_timeout_secs,
                total_timeout_secs,
                reply,
            } => self.handle_add_account(
                &name,
                &provider,
                AccountOverrides {
                    base_url,
                    streaming,
                    retry_max_attempts,
                    connect_timeout_secs,
                    request_timeout_secs,
                    total_timeout_secs,
                },
                &reply,
            ),
            DaemonCommand::RemoveAccountCmd { name, reply } => {
                self.handle_remove_account(&name, &reply);
            }
            DaemonCommand::ListAccountsCmd { reply } => self.handle_list_accounts(&reply),
            DaemonCommand::AccountsReload => self.handle_accounts_reload(),
            DaemonCommand::AclReload => self.handle_acl_reload(),
            DaemonCommand::ResolveAccountCmd { account, reply } => {
                self.handle_resolve_account(&account, &reply);
            }
            DaemonCommand::GetImageGenerationProvider {
                session_id,
                account_name,
                reply,
            } => self.handle_get_image_generation_provider(
                session_id,
                account_name.as_deref(),
                &reply,
            ),
            DaemonCommand::AccountExists { name, reply } => {
                self.handle_account_exists(&name, &reply);
            }
            DaemonCommand::ValidateModel {
                session_id,
                model,
                reply,
            } => self.handle_validate_model(session_id, &model, &reply),
            DaemonCommand::CancelRequest {
                session_id,
                stream_id,
            } => self.handle_cancel_request(session_id, stream_id),
            DaemonCommand::McpListChanged {
                slug,
                tools_changed,
            } => {
                self.handle_mcp_list_changed(&slug, tools_changed);
            }
            DaemonCommand::McpStatus { session_id, reply } => {
                let _ = reply.send(self.handle_mcp_status(session_id));
            }
            DaemonCommand::McpReconnect { slug, reply } => {
                self.handle_mcp_reconnect(&slug, &reply);
            }
            DaemonCommand::McpReload { session_id, reply } => {
                self.handle_mcp_reload(session_id, &reply);
            }
            DaemonCommand::McpEnsureSession {
                session_id,
                cancel_inflight,
            } => {
                let _ = self.resolve_and_push_session_overlay(session_id, cancel_inflight);
            }
            DaemonCommand::McpTrustSet {
                session_id,
                trusted,
                reply,
            } => {
                let outcome = self.handle_mcp_trust_set(session_id, trusted);
                let _ = reply.send(outcome);
            }
            DaemonCommand::McpTrustList { reply } => {
                let _ = reply.send(self.mcp_trust.list());
            }
            DaemonCommand::McpTierReload => self.handle_mcp_tier_reload(),
            DaemonCommand::McpTrustReload => self.handle_mcp_trust_reload(),
            DaemonCommand::SetSessionTitle { session_id, title } => {
                self.handle_set_session_title(session_id, title);
            }
            DaemonCommand::SetSessionFlags {
                session_id,
                pinned,
                archived,
                reply,
            } => self.handle_set_session_flags(session_id, pinned, archived, &reply),
            DaemonCommand::SetWorkingDir {
                session_id,
                path,
                reply,
            } => self.handle_set_working_dir(session_id, path, reply),
            DaemonCommand::LoadTools {
                session_id,
                groups,
                reply,
            } => self.handle_load_tools(session_id, groups, reply),
            DaemonCommand::UnloadTools {
                session_id,
                groups,
                reply,
            } => self.handle_unload_tools(session_id, groups, reply),
            DaemonCommand::PowerEvent(event) => {
                handle_suspend_event(event, &self.daemon_registry, &self.session_registries);
            }
            DaemonCommand::LastClientDisconnected => {
                // Never handled here: the auto-exit decision needs the shared
                // connection counter and the accept-loop wake probe, neither
                // of which belongs in DaemonState (the embedded daemon has no
                // socket to wake). Handled at the command-loop level in
                // start_daemon_core.
                debug!(
                    "unexpected LastClientDisconnected in handle_command; handled at loop level"
                );
            }
            DaemonCommand::Shutdown => {
                warn!("unexpected Shutdown command in handle_command; handled at loop level");
            }
        }
    }

    fn spawn_session(
        &mut self,
        session_id: u64,
        record: SessionRecord,
        metadata: SessionMetadata,
    ) -> crossbeam_channel::Sender<SessionCommand> {
        let db = Arc::clone(&self.db);
        let tool_registry = Arc::clone(&self.tool_registry);
        let daemon_tx = self.daemon_tx.clone();
        let max_turns = self.max_turns;
        // The session thread is a producer in the lossless fan-out: it
        // enforces the same lag caps as the command loop and shares the one
        // daemon-wide backlog counter. Copied/cloned BEFORE the `move`
        // closure so the closure never borrows `self` (which the method
        // still uses after spawning).
        let lag_limits = self.lag_limits;
        let global_lag = Arc::clone(&self.global_lag);
        // TEMPORARY: reserve the Tool trait's single `x_credentials` slot for
        // the content (Coordination Platform) signing credential. Only done
        // when the `content` feature is compiled in — without it there are no
        // content write tools to feed, so the slot stays empty. See
        // `RequestContext::substrate_credential` for the stopgap rationale
        // until a proper tool→keystore credential-access system replaces it.
        #[cfg(feature = "content")]
        let substrate_credential = self.pick_substrate_credential();
        #[cfg(not(feature = "content"))]
        let substrate_credential = None;

        // Each session gets its OWN socket registry: the owned instance goes
        // into the session's `SessionState` (its provider client registers
        // every dialed socket there), and a clone stays in
        // `session_registries` so the command loop can force-close exactly
        // this session's connections on cancel/suspend. Closing a session's
        // registry never disturbs another session's connections.
        let session_registry = choreo_ai_protocols::SocketRegistry::default();
        self.session_registries
            .insert(session_id, session_registry.clone());

        // Resolve provider from the session's account name
        let account_name = metadata.account_name.clone();
        // The provider is NEVER built here: sessions can be created while the
        // keystore is locked, so the session thread builds its client lazily
        // on the first request (see `SessionState::resolve_provider`).
        let provider = None;
        // The provider slug, though, is a NON-SECRET catalog fact the command
        // loop already knows from the account config: hand it to the session
        // thread so slug-keyed catalog lookups (context window, reasoning
        // capability) are exact even while the keystore is locked.
        let provider_slug = account_name
            .as_ref()
            .and_then(|name| self.account_provider_slug(name));

        // Resolve this session's cache-warming policy from the daemon's loaded
        // `[cache_warming]` config merged with the account's `meter`/
        // `cache_warming`/`prompt_cache` overrides. This is the INITIAL seed —
        // the session refreshes it from the same resolution whenever it
        // (re-)resolves its account (see `handle_resolve_account`), so a session
        // created before the keystore unlocks still picks up the right policy.
        // Resolving per account rather than per request keeps the request path
        // free of config parsing and file reads.
        let warm_policy = self.warm_policy_for(account_name.as_deref());

        // Crossbeam (unbounded) for the session transport channel: the daemon
        // hands this sender to clients/tools and the session control loop
        // blocks on the receiver, so it must share the workspace's channel
        // type (AGENTS.md "Channel selection"). Unbounded matches the old
        // `mpsc::channel` unbounded semantics exactly.
        let (session_tx, session_rx) = crossbeam_channel::unbounded::<SessionCommand>();
        let cmd_tx = session_tx.clone();

        let handle = thread::spawn(move || {
            session_main(
                &session_rx,
                provider,
                session_registry,
                account_name,
                provider_slug,
                Some(&record),
                &RequestContext {
                    cmd_tx,
                    session_id,
                    db,
                    tool_registry,
                    daemon_tx,
                    max_turns,
                    lag_limits,
                    global_lag,
                    substrate_credential,
                    warm_policy,
                    // No socket registry here: the session owns its own (see
                    // `SessionState::registry`); cancel/suspend closes it via
                    // the daemon's `session_registries` clone, not via the
                    // request context.
                },
            );
        });

        self.active_sessions.insert(
            session_id,
            ActiveSessionEntry {
                cmd_tx: session_tx.clone(),
                handle,
            },
        );
        self.session_metadata.insert(session_id, metadata);
        // Resolve the session's MCP overlay now that its working directory is
        // indexed, and push it to the session thread. This runs before
        // `session_tx` reaches the client, so the overlay is queued ahead of
        // any first request and is in place before the session can run tools.
        self.resolve_and_push_session_overlay(session_id, false);
        session_tx
    }

    /// Pick the single Substrate credential from the daemon's credential map.
    ///
    /// TEMPORARY: this reserves the Tool trait's single `x_credentials` slot
    /// for the content (Coordination Platform) signing credential (see
    /// `RequestContext::substrate_credential` for the stopgap rationale). When
    /// exactly one Substrate credential exists it is returned; when several,
    /// one named `"main"`/`"default"` is preferred (then the first in map
    /// order); when none, `None`.
    ///
    /// Only compiled with the `content` feature: without it no content write
    /// tools exist, so nothing consumes the credential and the slot stays
    /// empty (see the `spawn_session` call site).
    #[cfg(feature = "content")]
    fn pick_substrate_credential(&self) -> Option<ServiceCredential> {
        // First pass prefers a credential explicitly named "main"/"default";
        // otherwise keep the first Substrate credential encountered.
        let mut first_substrate: Option<&ServiceCredential> = None;
        for cred in self.credentials.values() {
            if matches!(
                cred,
                ServiceCredential::Substrate { name, .. } if name == "main" || name == "default"
            ) {
                return Some(cred.clone());
            }
            if first_substrate.is_none() && matches!(cred, ServiceCredential::Substrate { .. }) {
                first_substrate = Some(cred);
            }
        }
        first_substrate.cloned()
    }

    /// Extract the decrypted API key for an account, if one is held in
    /// memory. `None` covers both "keystore locked" and "no credential
    /// stored" — callers that must distinguish them check `self.locked`.
    fn api_key_for(&self, name: &str) -> Option<String> {
        self.credentials.get(name).and_then(|c| match c {
            ServiceCredential::ApiKey { key } => Some(key.clone()),
            _ => None,
        })
    }

    /// Build a provider client for an account against the DAEMON-owned
    /// registry. Used only for non-session-scoped clients (model prefetch,
    /// image-gen fallback when the session is already gone) — those are never
    /// individually cancelled. Session request clients are built by the
    /// session thread against the session's own registry instead
    /// (see `SessionState::resolve_provider`).
    fn build_daemon_provider(&self, name: &str) -> Option<InferenceProvider> {
        let config = self.accounts.get(name)?;
        InferenceProvider::from_account_config(
            config,
            self.api_key_for(name),
            &self.daemon_registry,
        )
        .ok()
    }

    /// The account's provider slug (catalog key, e.g. "opencode-go"), if the
    /// account is known. A NON-SECRET catalog fact — readable with no
    /// credential — used to seed a session's slug-keyed catalog lookups before
    /// its provider client exists. Single source for the `AccountManager`
    /// account→slug mapping.
    fn account_provider_slug(&self, name: &str) -> Option<String> {
        self.accounts
            .get(name)
            .map(|config| config.provider.clone())
    }

    /// Send a freshly-built `SessionCommand` to every active session bound to
    /// `account` (or ALL sessions when `account` is `None`), returning how many
    /// sends succeeded. The single fan-out point shared by provider-client
    /// invalidation and the account-reload slug refresh, so both target exactly
    /// the same session set. `make` is called once per target because
    /// `SessionCommand` (which carries reply channels) is not `Clone`.
    fn for_each_session_bound_to<F>(&self, account: Option<&str>, mut make: F) -> usize
    where
        F: FnMut() -> SessionCommand,
    {
        let targets: Vec<u64> = self
            .session_metadata
            .iter()
            .filter(|(_, meta)| match account {
                // Some(account): only sessions bound to that account.
                Some(a) => meta.account_name.as_deref() == Some(a),
                // None (e.g. /lock): every session.
                None => true,
            })
            .map(|(id, _)| *id)
            .collect();
        let mut sent = 0;
        for id in targets {
            if let Some(entry) = self.active_sessions.get(&id)
                && entry.cmd_tx.send(make()).is_ok()
            {
                sent += 1;
            }
        }
        sent
    }

    /// Drop the cached provider client of every active session bound to
    /// `account` (or ALL sessions when `account` is `None`, e.g. `/lock`) by
    /// sending `SessionCommand::DropProvider`. The session thread then
    /// rebuilds its client lazily on the next request — against fresh
    /// credentials and its own registry. This replaces the old per-account
    /// provider cache: there is no daemon-side cache to clear, only live
    /// session clients to invalidate.
    fn drop_session_clients(&self, account: Option<&str>) {
        let dropped = self.for_each_session_bound_to(account, || SessionCommand::DropProvider);
        if dropped > 0 {
            info!(
                account = ?account,
                dropped,
                "invalidated cached session provider clients; they rebuild on next use"
            );
        }
    }

    /// Push the account's (non-secret) provider slug to every active session
    /// bound to it, so slug-keyed static catalog facts stay exact immediately
    /// after an external account edit — instead of going stale until the next
    /// request rebuilds the client. `None` clears the recorded slug (the
    /// account was removed).
    fn set_session_provider_slug(&self, account: &str, slug: Option<&str>) {
        let sent =
            self.for_each_session_bound_to(Some(account), || SessionCommand::SetProviderSlug {
                slug: slug.map(str::to_owned),
            });
        if sent > 0 {
            debug!(
                account,
                ?slug,
                sessions = sent,
                "pushed provider slug to account's sessions"
            );
        }
    }

    /// Decide whether the model list for `account` needs a background
    /// prefetch: only when the account is configured AND holds a decrypted
    /// credential (otherwise the client build would fail anyway), no fetch is
    /// already running for the account, and the cached list is missing or
    /// past [`MODEL_CACHE_TTL`].  Pure — no side effects — so tests can
    /// assert the gate independently of thread spawning.
    fn should_prefetch_models(&self, account: &str) -> bool {
        if !self.accounts.contains(account)
            || self.api_key_for(account).is_none()
            || self.model_prefetch_in_flight.contains(account)
        {
            return false;
        }
        match self.model_cache.get(account) {
            Some((_, cached_at)) => Instant::now().duration_since(*cached_at) >= MODEL_CACHE_TTL,
            None => true,
        }
    }

    /// Spawn a detached background thread that fetches the model list for
    /// `account` and reports the outcome back to the command loop via
    /// [`DaemonCommand::ModelPrefetchResult`] — the loop, not the fetch
    /// thread, owns `model_cache` and the in-flight guard.  A no-op unless
    /// [`Self::should_prefetch_models`] says a fetch is needed, which is what
    /// keeps a burst of session joins (or an account switch per request) from
    /// stacking duplicate HTTP fetches.  A failed spawn releases the guard so
    /// the account stays re-prefetchable.
    fn maybe_spawn_model_prefetch(&mut self, account: &str) {
        if !self.should_prefetch_models(account) {
            return;
        }
        self.model_prefetch_in_flight.insert(account.to_string());
        // Build the client fresh against the daemon registry: there is no
        // provider cache anymore. `should_prefetch_models` guarantees config
        // + credential exist; the None arm is belt-and-braces so the
        // in-flight guard can never leak.
        let Some(provider) = self.build_daemon_provider(account) else {
            self.model_prefetch_in_flight.remove(account);
            return;
        };
        let daemon_tx = self.daemon_tx.clone();
        let account_name = account.to_string();
        let spawned = thread::Builder::new()
            .name(format!("model-prefetch-{account_name}"))
            .spawn(move || {
                // The fetch is deliberately detached from the command loop:
                // a slow provider endpoint (up to the full request timeout,
                // retried) must never stall daemon commands the way the old
                // unlock-time synchronous prefetch did.
                //
                // The whole fetch is wrapped in `catch_unwind` (the provider
                // is an owned value, so `AssertUnwindSafe` is sound here —
                // the thread never touches shared state): a panic inside the
                // provider's HTTP/serde code is not covered by the
                // workspace's no-panic discipline, and an uncaught unwind
                // would skip the `ModelPrefetchResult` send below — the ONLY
                // message that releases the in-flight guard — permanently
                // wedging the account against re-prefetching until daemon
                // restart. A caught panic is reported as a plain fetch
                // error, and the next join retries.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    provider.list_models().map_err(|e| e.to_string())
                }))
                .unwrap_or_else(|panic| {
                    // Panic payloads are `String`/`&str` in practice, but
                    // the payload type is `dyn Any` — fall back to a generic
                    // message rather than assuming the shape.
                    let detail = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
                        .unwrap_or_else(|| "unknown panic payload".to_string());
                    Err(format!("model list fetch panicked: {detail}"))
                });
                let _ = daemon_tx.send(DaemonCommand::ModelPrefetchResult {
                    account: account_name,
                    result,
                });
            });
        if let Err(e) = spawned {
            self.model_prefetch_in_flight.remove(account);
            warn!(
                account = %account,
                error = %e,
                "failed to spawn model prefetch thread; account stays re-prefetchable"
            );
        }
    }

    /// Receive a background model-prefetch outcome: release the account's
    /// in-flight guard and, on success, populate `model_cache` (the command
    /// loop is its single writer).  Failures are logged only — the next
    /// session join re-prefetches, and the on-demand path in
    /// `handle_list_models_inner` remains the fallback while nothing is
    /// cached.
    fn handle_model_prefetch_result(
        &mut self,
        account: String,
        result: Result<Vec<String>, String>,
    ) {
        self.model_prefetch_in_flight.remove(&account);
        match result {
            Ok(models) => {
                // Only cache while the account still exists with a credential:
                // the account may have been removed or reconfigured
                // (AccountsReload invalidates sessions) while the fetch was
                // in flight, and inserting then would serve a dead
                // provider's model list for a full TTL.
                if !self.accounts.contains(&account) || self.api_key_for(&account).is_none() {
                    debug!(
                        account = %account,
                        "discarding background model prefetch result; \
                         account was removed or reconfigured while the fetch ran"
                    );
                    return;
                }
                debug!(
                    account = %account,
                    models = models.len(),
                    "background model prefetch complete"
                );
                self.model_cache.insert(account, (models, Instant::now()));
            }
            Err(e) => {
                warn!(
                    account = %account,
                    error = %e,
                    "background model prefetch failed; will retry on the next join"
                );
            }
        }
    }

    /// Create a new session. Sessions are lightweight containers that can be
    /// created regardless of lock state.
    fn handle_create_session(
        &mut self,
        params: CreateSessionParams,
        reply: &std::sync::mpsc::Sender<
            io::Result<(u64, crossbeam_channel::Sender<SessionCommand>)>,
        >,
    ) {
        let CreateSessionParams {
            title,
            parent_session_id,
            working_dir,
            reasoning_effort,
            selected_model,
            context_config,
            account_name,
            active_tool_groups,
        } = params;
        // A session is just a conversation container — it can be
        // created, browsed, and deleted regardless of whether the
        // daemon is locked.  Credentials are only needed when running
        // models (RunInput).
        let sid = self.next_session_id;
        self.next_session_id += 1;
        info!("CreateSession: id={}, title={:?}", sid, title);

        let cwd_str = working_dir.map(|p| p.display().to_string());
        // The default active groups mirror the always-on groups; the
        // Coordination Platform group is included only when the `content`
        // feature is compiled in. Stale persisted names (e.g. `coord` from
        // before the group rename, or groups whose feature is off) are
        // silently ignored downstream: a group with no registered tools
        // contributes nothing to `available_definitions`, and load/unload
        // validation rejects unknown names on new requests only.
        // `mut` is only needed when the `content` feature pushes its group.
        #[cfg_attr(not(feature = "content"), expect(unused_mut))]
        let mut default_groups = vec!["core".to_string(), "git".to_string(), "shell".to_string()];
        #[cfg(feature = "content")]
        default_groups.push("content".to_string());
        // The iOS tools' group is PROTECTED (register_platform_tools) and
        // always unioned into the active set at definition time — listing it
        // here is belt-and-suspenders so the session's persisted/displayed
        // active set is honest about what the model can actually call.
        #[cfg(target_os = "ios")]
        default_groups.push("ios".to_string());
        let active_cats = if active_tool_groups.is_empty() {
            default_groups
        } else {
            active_tool_groups
        };

        // Resolve context window from the provider catalog at creation time
        // when both account and model are known — no provider instance needed.
        let context_window = account_name.as_ref().and_then(|name| {
            selected_model.as_ref().and_then(|model| {
                self.account_provider_slug(name)
                    .and_then(|slug| lookup_context_window(&slug, model))
            })
        });

        // Clone before moving into record — needed for created_msg below.
        let selected_model_clone = selected_model.clone();
        let reasoning_effort_clone = reasoning_effort.clone();

        // A freshly created session's modification time is its creation time,
        // so a new session sorts to the top of the list immediately.
        let created_at = TimestampMs::now().as_millis();
        let record = SessionRecord {
            title: title.clone(),
            selected_model,
            reasoning_effort,
            parent_session_id,
            working_dir: cwd_str.clone(),
            turn_count: 0,
            created_at,
            last_modified: created_at,
            active_tool_groups: active_cats.clone(),
            context_config: context_config.unwrap_or_default(),
            account_name: account_name.clone(),
            last_response_id: None,
            last_response_id_producer: None,
            // New sessions start unpinned and unarchived.
            pinned: false,
            archived_at: None,
        };

        if let Err(e) = db::write_session(&self.db, sid, &record) {
            error!("CreateSession: failed to persist session {}: {e}", sid);
        }

        let metadata = SessionMetadata {
            title: title.clone(),
            selected_model: record.selected_model.clone(),
            reasoning_effort: record.reasoning_effort.clone(),
            parent_session_id,
            working_dir: cwd_str.clone(),
            created_at: record.created_at,
            last_modified: record.last_modified,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: active_cats.clone(),
            account_name: account_name.clone(),
            accumulated_usage: TokenUsage::default(),
            context_window,
            last_prompt_tokens: None,
            // A brand-new session starts unpinned and unarchived; these are
            // daemon-owned and afterwards preserved across UpdateMetadata.
            pinned: false,
            archived_at: None,
        };
        let session_tx = self.spawn_session(sid, record, metadata);

        // Warm the model list for the session's account in the background so
        // the model picker is populated by the time the user opens it — a
        // no-op when the cache is already fresh or a fetch is in flight.
        if let Some(name) = &account_name {
            self.maybe_spawn_model_prefetch(name);
        }

        // Track parent→child relationship so cancellation/deletion
        // of the parent propagates to sub-sessions.
        if let Some(parent_id) = parent_session_id {
            self.children.entry(parent_id).or_default().push(sid);
        }

        let _ = reply.send(Ok((sid, session_tx)));
        crate::metrics::record_session_created();
        // BROADCAST notification only: every subscriber learns a session now
        // exists, but this must NOT move any client's view. The direct reply
        // that lets the CREATING client attach is `SessionCreatedForRequester`,
        // built and sent in the connection thread (`handle_client_create_session`)
        // — see the split documented on `SessionEvent`.
        let created_msg = DaemonMessageType::Session {
            session_id: Some(sid),
            event: SessionEvent::SessionCreated {
                title,
                parent_session_id,
                working_dir: cwd_str,
                account_name,
                selected_model: selected_model_clone,
                reasoning_effort: reasoning_effort_clone,
            },
        };
        let status_msg = DaemonMessageType::Session {
            session_id: Some(sid),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                // Copy the creation timestamp before `record` is moved into
                // spawn_session above.
                last_modified: created_at,
            },
        };
        self.broadcast(&created_msg);
        self.broadcast(&status_msg);
    }

    /// Ensure a session's thread is live, returning its command sender: the
    /// already-active thread when one exists, otherwise loading the session from
    /// the DB and spawning a fresh thread. A session marked deleted, or one with
    /// no stored record, is `NotFound`.
    ///
    /// Shared by every entry point that needs a session's authoritative thread
    /// rather than just its metadata index ([`handle_attach_session`],
    /// [`handle_get_session_state`]) so the load-or-spawn policy (and its
    /// deleted-session guard) lives in exactly one place.
    fn ensure_active_session(
        &mut self,
        session_id: u64,
    ) -> io::Result<crossbeam_channel::Sender<SessionCommand>> {
        // A deleted session's still-shutting-down thread can leave the DB record
        // in place until `handle_session_exited` finalizes the delete (and drops
        // the deleted marker). Without this guard, reviving it would resurrect a
        // session the user deleted — the record would be gone moments later,
        // stranding the new thread.
        if self.deleted_sessions.contains(&session_id) {
            debug!(session_id, "ensure_active_session: session is deleted");
            return Err(io::Error::new(io::ErrorKind::NotFound, "session not found"));
        }
        if let Some(entry) = self.active_sessions.get(&session_id) {
            return Ok(entry.cmd_tx.clone());
        }
        match db::read_session(&self.db, session_id) {
            Ok(Some(record)) => {
                let mut metadata: SessionMetadata = record.clone().into();
                metadata.status = SessionStatus::Inactive;
                let session_tx = self.spawn_session(session_id, record, metadata);
                info!(session_id, "loaded session from db");
                Ok(session_tx)
            }
            Ok(None) => Err(io::Error::new(io::ErrorKind::NotFound, "session not found")),
            Err(e) => Err(e),
        }
    }

    /// Attach to an existing session by ID. Loads from the database if the
    /// session is not currently active.
    fn handle_attach_session(
        &mut self,
        session_id: u64,
        reply: &std::sync::mpsc::Sender<io::Result<crossbeam_channel::Sender<SessionCommand>>>,
    ) {
        debug!("AttachSession: id={}", session_id);
        // Attaching to a session is allowed regardless of lock state.
        // Credentials are only needed to run models (RunInput), not
        // to browse or attach to existing sessions.
        match self.ensure_active_session(session_id) {
            Ok(session_tx) => {
                // Attaching also warms the session's model list in the
                // background — the session may have been joined on a different
                // client (or before this account's cache went stale), and the
                // in-flight guard keeps this idempotent.
                if let Some(name) = self
                    .session_metadata
                    .get(&session_id)
                    .and_then(|m| m.account_name.clone())
                {
                    self.maybe_spawn_model_prefetch(&name);
                }
                let _ = reply.send(Ok(session_tx));
            }
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }

    /// Return a list of all active session summaries in the shared list
    /// order (pinned first, then newest, then id-desc —
    /// [`SessionSummary::cmp_for_list`]). This is the same ordering every
    /// client re-applies when it renders the list, so the two cannot drift.
    fn handle_list_sessions(&mut self, reply: &std::sync::mpsc::Sender<Vec<SessionSummary>>) {
        let mut summaries: Vec<SessionSummary> = self
            .session_metadata
            .iter()
            .map(|(id, meta)| meta.to_summary(*id))
            .collect();

        // One definition of list order (`cmp_for_list`), used here and by the
        // clients, so equal timestamps stay deterministic and pinned rows
        // float to the top identically everywhere.
        summaries.sort_by(SessionSummary::cmp_for_list);
        let _ = reply.send(summaries);
    }

    /// Get a single session summary by ID.
    fn handle_get_session(
        &mut self,
        session_id: u64,
        reply: &std::sync::mpsc::Sender<Option<SessionSummary>>,
    ) {
        let summary = self
            .session_metadata
            .get(&session_id)
            .map(|meta| meta.to_summary(session_id));
        let _ = reply.send(summary);
    }

    /// Answer a `GetSessionState` request: ensure the session thread is live
    /// (loading it from the DB if it had slept), then forward the client's reply
    /// channel straight to it so the snapshot is built off the command loop —
    /// the connection thread is the one that blocks. A missing or deleted
    /// session answers `NotFound`.
    fn handle_get_session_state(
        &mut self,
        session_id: u64,
        reply: mpsc::Sender<io::Result<DaemonMessageType>>,
    ) {
        debug!(session_id, "GetSessionState");
        match self.ensure_active_session(session_id) {
            Ok(session_tx) => {
                // Hand the reply channel to the session thread. If the send
                // fails the thread is gone; the command (and its reply sender)
                // is dropped, which unblocks the connection thread with a
                // `RecvError`, and it answers its own failure.
                if session_tx.send(SessionCommand::GetState { reply }).is_err() {
                    debug!(session_id, "GetSessionState: session thread gone");
                }
            }
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }

    /// Set the daemon-owned `pinned`/`archived` flags of a session. The daemon
    /// is the authority: it updates its in-memory index, persists ONLY the two
    /// flag fields (a read-modify-write that cannot clobber the rest of the
    /// record the session thread owns), and broadcasts
    /// `SessionFlagsChanged` to every subscriber. There is no targeted SUCCESS
    /// reply (the broadcast is the signal); a failure is reported to the
    /// requesting connection only.
    fn handle_set_session_flags(
        &mut self,
        session_id: u64,
        pinned: Option<bool>,
        archived: Option<bool>,
        reply: &std::sync::mpsc::Sender<io::Result<()>>,
    ) {
        debug!(session_id, ?pinned, ?archived, "SetSessionFlags");
        // A deleted session must not be resurrected by a flag update — the
        // same guard every other index mutation uses.
        if self.deleted_sessions.contains(&session_id) {
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session not found",
            )));
            return;
        }
        // Resolve the POST-change flag state WITHOUT mutating the index yet.
        // Persisting first — and only touching the in-memory index once the DB
        // write has succeeded — keeps memory, the DB, and every client in
        // agreement: a persist failure must NOT leave the daemon reporting a
        // flag that no client was ever told about (and that a restart would
        // lose). A `None` field leaves the existing value untouched; archiving
        // stamps the current time, unarchiving clears it.
        let Some(existing) = self.session_metadata.get(&session_id) else {
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session not found",
            )));
            return;
        };
        let pinned = pinned.unwrap_or(existing.pinned);
        let archived_at = match archived {
            Some(true) => Some(TimestampMs::now().as_millis()),
            Some(false) => None,
            None => existing.archived_at,
        };
        // Persist ONLY the two flag fields via the read-modify-write helper, so
        // a concurrent full-record write from the session thread can neither
        // clobber these flags nor have its own fields clobbered here.
        if let Err(e) = db::update_session_flags(&self.db, session_id, pinned, archived_at) {
            warn!(session_id, error = %e, "failed to persist session flags");
            let _ = reply.send(Err(e));
            return;
        }
        // The DB write succeeded — now (and only now) apply the change to the
        // in-memory index and broadcast it as the success signal for EVERY
        // subscriber (including the requester; there is no targeted reply).
        if let Some(meta) = self.session_metadata.get_mut(&session_id) {
            meta.pinned = pinned;
            meta.archived_at = archived_at;
        }
        self.broadcast(&DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionFlagsChanged {
                pinned,
                archived_at,
            },
        });
        let _ = reply.send(Ok(()));
    }

    /// Update the in-memory metadata for a session.
    fn handle_update_metadata(&mut self, session_id: u64, mut metadata: SessionMetadata) {
        debug!(
            "UpdateMetadata: id={}, model={:?}",
            session_id, metadata.selected_model
        );
        // A deleted session's still-shutting-down thread may still emit
        // metadata updates before it exits (e.g. a straggler
        // RequestFinished).  Never re-insert a deleted session into the index.
        if self.deleted_sessions.contains(&session_id) {
            debug!(session_id, "ignoring UpdateMetadata for deleted session");
            return;
        }
        if let Some(existing) = self.session_metadata.get(&session_id) {
            // last_modified is monotonic: never let a stale (older) update
            // regress the timestamp the session thread or a status broadcast
            // just set.
            metadata.last_modified = metadata.last_modified.max(existing.last_modified);

            // Sleeping is the exit marker: the daemon sets it in
            // handle_session_exited once the session thread has terminated,
            // and only AttachSession (which bypasses this path) brings a
            // session back to Inactive.  Any UpdateMetadata that still
            // arrives was generated by the thread before it exited, so its
            // status snapshot is stale — e.g. a straggler RequestFinished
            // would claim Inactive and make a dead session look idle.
            // Preserve the exit status rather than letting the snapshot
            // regress it.
            if existing.status == SessionStatus::Sleeping {
                metadata.status = SessionStatus::Sleeping;
            }

            // The daemon OWNS the pinned/archived flags: the session thread's
            // snapshot (`SessionMetadata::from(&SessionState)`) never carries
            // them, so preserve the daemon's current values rather than letting
            // a straggler snapshot reset them to the defaults.
            metadata.pinned = existing.pinned;
            metadata.archived_at = existing.archived_at;
        }
        // Detect a real account CHANGE before the metadata is moved into the
        // index: switching (or attaching) an account on a live session is the
        // third trigger for a background model-list prefetch, alongside
        // create and attach.  UpdateMetadata fires per request, so the
        // in-flight + freshness guards inside `maybe_spawn_model_prefetch`
        // are what keep this from spawning repeated fetches.
        let old_account = self
            .session_metadata
            .get(&session_id)
            .and_then(|m| m.account_name.clone());
        let new_account = metadata.account_name.clone();
        self.session_metadata.insert(session_id, metadata);
        if new_account.is_some()
            && new_account != old_account
            && let Some(name) = new_account.as_deref()
        {
            self.maybe_spawn_model_prefetch(name);
        }
    }

    /// Mark a session as exited (sleeping) and broadcast the status change.
    /// If the session has any children, cancel and shut them down so they
    /// don't continue running as orphans.
    ///
    /// If the session was deleted while its thread was alive, this is also
    /// where the delete is finalized: the thread's `persist_and_exit` runs
    /// *before* it sends `SessionExited`, so by the time this handler runs the
    /// record on disk is the thread's final state — safe to delete without a
    /// re-create race.  The delete runs on a background thread (see
    /// [`DaemonCommand::SessionDeleteFinalized`]) so a pathologically large
    /// session — `db::delete_session` walks every turn and kv entry — cannot
    /// block the command loop.
    fn handle_session_exited(&mut self, session_id: u64) {
        info!("SessionExited: id={}", session_id);
        crate::metrics::record_session_exited();

        // Drop the session's registry clone: the thread is gone, nothing can
        // register or be cancelled through it anymore.
        self.session_registries.remove(&session_id);
        self.session_mcp_projects.remove(&session_id);

        // Release the session's MCP pool references: decrement project-shared
        // ref-counts (dropping connections no session uses anymore) and drop
        // its per-session connections.
        self.mcp_manager.release_session(session_id);

        // Remove the session entry so it is no longer treated as active.
        self.active_sessions.remove(&session_id);

        // Cancel and shut down children so they don't run as orphans.
        if let Some(children) = self.children.remove(&session_id) {
            for child_id in children {
                self.remove_child_from_all_parents(child_id);
                self.cancel_and_shutdown_child(child_id);
            }
        }

        // Exiting (the last subscriber detached) is lifecycle noise, not a
        // modification — the session produced no new content by shutting
        // down, so the sessions list must NOT re-sort here.  Update the
        // index *status* and reuse its current `last_modified` for the
        // broadcast so clients' monotonic `max()` guards keep both sides in
        // sync.  (The daemon-side timestamp for a request that just finished
        // was already set by `UpdateMetadata` in `handle_request_finished`.)
        let last_modified = match self.session_metadata.get_mut(&session_id) {
            Some(meta) => {
                meta.status = SessionStatus::Sleeping;
                meta.last_modified
            }
            None => 0,
        };
        // Only broadcast for sessions that still exist: a deleted session's
        // shutting-down thread must not emit a ghost "sleeping" status for a
        // session the user removed.
        if self.session_metadata.contains_key(&session_id) {
            let msg = DaemonMessageType::Session {
                session_id: Some(session_id),
                event: SessionEvent::SessionStatusChanged {
                    status: SessionStatus::Sleeping,
                    last_modified,
                },
            };
            self.broadcast(&msg);
        }

        // Finalize a pending delete: the thread has fully exited and
        // persisted, so the record can now be removed without a re-create
        // race.  The actual DB work is handed to a detached thread so a large
        // session cannot block the command loop; the `deleted_sessions` marker
        // (which blocks `AttachSession` resurrection) stays in place until
        // that thread reports back with `SessionDeleteFinalized`.
        if self.deleted_sessions.contains(&session_id) {
            // Finalize on a background thread (see `finalize_session_delete`)
            // so a pathologically large session cannot block the command loop;
            // the `deleted_sessions` marker stays in place until that thread
            // reports back with `SessionDeleteFinalized`.
            let db = Arc::clone(&self.db);
            let daemon_tx = self.daemon_tx.clone();
            std::thread::spawn(move || finalize_session_delete(&db, session_id, &daemon_tx));
        }
    }

    /// The background finalize has deleted the record the still-shutting-down
    /// thread left behind (and cleared its tombstone).  Only now is it safe to
    /// drop the `deleted_sessions` marker — the record is gone for good, so no
    /// attach or straggler message can resurrect the session.
    fn handle_session_delete_finalized(&mut self, session_id: u64) {
        debug!("SessionDeleteFinalized: id={}", session_id);
        self.deleted_sessions.remove(&session_id);
    }

    /// Remove a stored credential for a service.
    fn handle_remove_credential(
        &mut self,
        service: &str,
        reply: &mpsc::Sender<Result<(), String>>,
    ) {
        // Remove from DB
        if let Err(e) = db::remove_credential_blob(&self.db, service) {
            let _ = reply.send(Err(format!("failed to remove credential: {e}")));
            return;
        }
        // Remove from in-memory state. No provider cache to drop — instead
        // invalidate the cached client of every session bound to this
        // account so it rebuilds (and fails with clean guidance) on next use.
        self.credentials.remove(service);
        self.drop_session_clients(Some(service));
        if service == "twitter" {
            self.x_credentials = None;
        }
        let _ = reply.send(Ok(()));
    }

    /// List available models, optionally scoped to a session's account.
    fn handle_list_models(&mut self, session_id: Option<u64>, reply: &ListModelsReply) {
        debug!("ListModels: session_id={:?}", session_id);
        let result = handle_list_models_inner(self, session_id);
        let _ = reply.send(result);
    }

    /// Handle a `/refresh-models` request. The fetch must NEVER run here (it
    /// can block for the whole 30s timeout, stalling the command loop), so the
    /// request is handed to the maintenance thread over its channel; the reply
    /// comes back through [`DaemonCommand::CatalogBaseChanged`] after the
    /// thread has a result (or is sent directly by the thread on 304/error).
    fn handle_refresh_models(
        &mut self,
        force: bool,
        reply: &mpsc::Sender<Result<RefreshReport, String>>,
    ) {
        if let Some(tx) = &self.maintenance_tx {
            info!(
                force,
                "RefreshModels: handing fetch to the maintenance thread"
            );
            // Clone the reply: on a dead thread the request must still
            // get a structured error instead of silently vanishing (the
            // clone rides the maintenance channel; the original replies
            // on send failure).
            if tx
                .send(MaintenanceEvent::RefreshNow {
                    force,
                    reply: reply.clone(),
                })
                .is_err()
            {
                warn!("RefreshModels: maintenance thread is gone; replying with an error");
                let _ = reply.send(Err("catalog maintenance thread is not running".to_string()));
            }
        } else {
            warn!("RefreshModels: no maintenance thread (unit-test state); replying with an error");
            let _ = reply.send(Err("catalog maintenance thread is not running".to_string()));
        }
    }

    /// Apply a new catalog base + user overlay delivered by the maintenance
    /// thread. This is the ONLY place the daemon calls `replace_catalog` for
    /// runtime refreshes (single-writer invariant: the daemon command loop).
    ///
    /// Merge order, lowest → highest wins: normalized models.dev base →
    /// bundled overlay → user overlay. The merged catalog is validated
    /// non-empty before the swap (a hostile/typo'd overlay must never leave
    /// the daemon with an empty catalog). On a live fetch the cache bin is
    /// persisted atomically and the etag to the DB. Every swap broadcasts
    /// `CatalogUpdated` so clients can refresh their provider pickers. The
    /// work is split into small steps (merge → validate → swap → persist →
    /// broadcast → reply) so each stage stays readable and unit-testable.
    fn handle_catalog_base_changed(
        &mut self,
        base: &[choreo_ai_protocols::ProviderEntry],
        etag: Option<&str>,
        user_overlay: Option<&str>,
        persist: bool,
        reply: Vec<RefreshRequester>,
    ) {
        debug!(
            base_providers = base.len(),
            user_overlay_present = user_overlay.is_some(),
            persist,
            "CatalogBaseChanged: merging overlays",
        );

        // Lowest → highest: base → bundled overlay → user overlay.
        let effective = merge_catalog_layers(base, user_overlay);

        if effective.is_empty() {
            // Never swap in an empty catalog: keep the current one and tell
            // the requester(s) (merge_overlay is infallible, so an empty
            // result means the base itself was empty — a broken fetch).
            error!("refusing to swap in an empty catalog; keeping the current one");
            for r in reply {
                let _ = r.tx.send(Err(
                    "merged catalog is empty; keeping the current catalog".to_string()
                ));
            }
            return;
        }

        // Single-writer point: the atomic swap. Readers are lock-free.
        replace_catalog(effective.clone());
        self.persist_catalog_cache(base, etag, persist);

        // Broadcast the new provider list to all activity subscribers so the
        // TUI's provider picker tracks the live catalog.
        let providers = catalog_provider_pairs();
        self.handle_broadcast_activity(
            None,
            &DaemonMessage::broadcast(DaemonMessageType::CatalogUpdated { providers }),
        );

        let models: usize = effective.iter().map(|e| e.models.len()).sum();
        info!(providers = effective.len(), models, "catalog updated",);
        send_catalog_reply(reply, effective.len(), models);
    }

    /// A models.dev conditional GET returned 304 — the cached base is
    /// current. The reply is routed through the command loop (not sent
    /// directly by the maintenance thread) so any user-overlay reload queued
    /// just before the request is applied first: FIFO on the command channel
    /// orders the swap ahead of this reply, so the `UpToDate` counts reflect
    /// the post-reload catalog rather than stale pre-reload numbers. Carries
    /// no base — nothing is swapped, nothing persisted, nothing broadcast.
    fn handle_catalog_not_modified(reply: Vec<RefreshRequester>) {
        if reply.is_empty() {
            return;
        }
        let snapshot = catalog_snapshot();
        let providers = snapshot.len();
        let models: usize = snapshot.iter().map(|e| e.models.len()).sum();
        info!(providers, models, "models.dev catalog unchanged (304)");
        for r in reply {
            let _ = r.tx.send(Ok(RefreshReport {
                providers,
                models,
                // A 304 means nothing changed, even for a requester that
                // asked for --force (the server said the cache is current).
                status: RefreshStatus::UpToDate,
            }));
        }
    }

    /// Persist the cache bin + etag after a live fetch. Startup loads
    /// (`persist: false`) are already on disk — a cache-sourced base needs no
    /// rewrite, and a cache-miss will be persisted on the first fetch — so
    /// only live fetches write. The **bin file is written first, the etag to
    /// the DB second**: a crash between the two leaves the OLD etag paired
    /// with the OLD bin (self-healing — the next conditional GET 200s and
    /// stores a fresh etag), never a NEW etag over OLD content (which would
    /// 304 forever against a stale cache). If the bin write fails, the etag
    /// is deliberately NOT updated — it must never describe content that is
    /// not on disk. Failures are logged, never fatal: the next refresh
    /// re-fetches and tries again.
    fn persist_catalog_cache(
        &self,
        base: &[choreo_ai_protocols::ProviderEntry],
        etag: Option<&str>,
        persist: bool,
    ) {
        if !persist {
            return;
        }
        // Bin first: the etag write below must only happen once the content
        // it validates is durably on disk.
        if let Err(e) = crate::catalog::write_catalog_cache(base, &self.catalog_paths.bin) {
            warn!(
                error = %e,
                "failed to persist the catalog cache; the next refresh will re-fetch",
            );
            return;
        }
        if let Err(e) = crate::db::set_catalog_etag(&self.db, etag) {
            warn!(
                error = %e,
                "failed to persist the catalog etag; the next refresh will do a plain GET",
            );
        }
    }

    /// Validate that a model exists in the provider's model list for this
    /// session's account.  The model list is warmed by the background
    /// prefetch spawned at session join/attach/account-switch time
    /// (`maybe_spawn_model_prefetch`).  If no cached data exists (fetch
    /// failed or the prefetch hasn't landed yet) the model is allowed
    /// through — we'd rather fail at inference time than reject a potentially
    /// valid model we couldn't verify.
    fn handle_validate_model(
        &mut self,
        session_id: u64,
        model: &str,
        reply: &mpsc::Sender<Result<(), String>>,
    ) {
        debug!("ValidateModel: session_id={}, model={}", session_id, model);

        let Some(account_name) = self
            .session_metadata
            .get(&session_id)
            .and_then(|m| m.account_name.clone())
        else {
            debug!(
                "ValidateModel: no session or no account attached, \
                 allowing model '{model}' through"
            );
            let _ = reply.send(Ok(()));
            return;
        };

        // No resolvable client for this account (locked, credential missing,
        // or account unknown) → reject so the user knows they must unlock
        // first (or configure a credential) rather than silently accepting an
        // unvalidated model.
        if !self.accounts.contains(&account_name) || self.api_key_for(&account_name).is_none() {
            debug!(
                "ValidateModel: no provider for account '{account_name}', \
                 rejecting model '{model}'"
            );
            let _ = reply.send(Err(format!(
                "daemon is locked or no credential configured for account \
                 '{account_name}'"
            )));
            return;
        }

        // Check the cache.  If missing (fetch failed earlier) or empty,
        // allow through rather than reject a potentially valid model.
        match self.model_cache.get(&account_name) {
            Some((cached_models, _cached_at)) if !cached_models.is_empty() => {
                if cached_models.iter().any(|m| m == model) {
                    let _ = reply.send(Ok(()));
                } else {
                    let available = humfmt::list(cached_models);
                    let _ = reply.send(Err(format!(
                        "model '{model}' not found. Available: {available}"
                    )));
                }
            }
            _ => {
                debug!(
                    "ValidateModel: no cached models for account '{account_name}', \
                     allowing model '{model}' through"
                );
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Remove `child_id` from any parent's children list (safety net).
    /// This handles the case where a child appears in multiple tracking
    /// entries (shouldn't happen, but we guard against it).
    fn remove_child_from_all_parents(&mut self, child_id: u64) {
        self.children.retain(|_, v| {
            v.retain(|c| *c != child_id);
            !v.is_empty()
        });
    }

    /// Get the API key for a stored credential (returns None if not found).
    fn handle_get_credential(
        &mut self,
        service: &str,
        reply: &std::sync::mpsc::Sender<Option<String>>,
    ) {
        let key = self.credentials.get(service).and_then(|c| match c {
            ServiceCredential::ApiKey { key } => Some(key.clone()),
            _ => None,
        });
        let _ = reply.send(key);
    }

    /// Handle a cancel request from a client.  Sends `SessionCommand::Cancel`
    /// to the target session and then propagates cancellation to any child
    /// sub-sessions directly — avoiding a round-trip message from the session
    /// thread back to the daemon.
    fn handle_cancel_request(&mut self, session_id: u64, stream_id: u64) {
        debug!("CancelRequest: session={session_id} request={stream_id}");

        // Forward the cancel to the session thread.
        if let Some(entry) = self.active_sessions.get(&session_id) {
            let _ = entry.cmd_tx.send(SessionCommand::Cancel { stream_id });
        }

        // Propagate to children — this runs here in the daemon so that
        // leaf sessions never generate an unnecessary message.
        self.cancel_children_of(session_id);

        // The cancel is DECIDED here (the session worker only observes it),
        // so this is where the force-close belongs: a streaming inference
        // read wedged on a half-dead provider connection would otherwise
        // keep the worker blocked until the request timeout even after the
        // cooperative cancel flag fired. Closing the TARGET SESSION's
        // registry makes its blocked reads return immediately — and touches
        // NOTHING belonging to other concurrent sessions (each session owns
        // its own registry). Only provider sockets are affected — client
        // connections and tools are untouched. The count is logged by
        // `shutdown_all` itself; this line records the WHY (a user cancel,
        // distinct from suspend or organic IO errors) so the daemon log
        // stays greppable.
        self.force_close_session_sockets(session_id, "request cancelled");

        // Stop any in-flight MCP tool call this session started: the MCP
        // dispatchers cancel their matching calls and tell the servers to stop
        // cooperatively. Best-effort and non-blocking.
        self.mcp_manager.cancel_session(session_id);
    }

    /// Force-close one session's provider sockets by shutting down its
    /// registry clone (the session thread holds the sibling that its client
    /// registers sockets into). No-op when the session is already gone.
    fn force_close_session_sockets(&self, session_id: u64, why: &str) {
        if let Some(registry) = self.session_registries.get(&session_id) {
            info!(
                session_id,
                why, "force-closing provider sockets to unblock any wedged reader"
            );
            registry.shutdown_all();
        }
    }

    /// Send `Cancel` to every active child session of `parent_id`.
    /// If the parent no longer exists (e.g. cascade-deleted while a
    /// child's cancel fired), this is a no-op.
    fn cancel_children_of(&mut self, parent_id: u64) {
        // Guard: if the parent has already been torn down (e.g. during
        // cascade delete), don't try to cancel its children.
        if !self.active_sessions.contains_key(&parent_id)
            && !self.session_metadata.contains_key(&parent_id)
        {
            return;
        }
        let Some(children) = self.children.get(&parent_id).cloned() else {
            return;
        };
        for child_id in &children {
            if let Some(entry) = self.active_sessions.get(child_id) {
                debug!(
                    "propagating cancel from session {} to child {}",
                    parent_id, child_id
                );
                if entry
                    .cmd_tx
                    .send(SessionCommand::Cancel {
                        stream_id: CANCEL_ALL,
                    })
                    .is_err()
                {
                    warn!("cancel_children_of: failed to send Cancel to child {child_id}");
                }
                // A parent cancel kills the whole subtree's connections:
                // each child's registry is closed too, while every OTHER
                // session (siblings elsewhere, unrelated sessions) keeps its
                // sockets.
                self.force_close_session_sockets(*child_id, "parent request cancelled");
                // And stop each child's in-flight MCP call too.
                self.mcp_manager.cancel_session(*child_id);
            }
        }
    }

    /// Cancel the active request in a child session and send Shutdown so it
    /// persists its state and exits. Used when the parent session exits.
    fn cancel_and_shutdown_child(&mut self, child_id: u64) {
        let Some(entry) = self.active_sessions.get(&child_id) else {
            return;
        };
        if entry
            .cmd_tx
            .send(SessionCommand::Cancel {
                stream_id: CANCEL_ALL,
            })
            .is_err()
        {
            warn!("cancel_and_shutdown_child: failed to send Cancel to child {child_id}");
        }
        if entry.cmd_tx.send(SessionCommand::Shutdown).is_err() {
            warn!("cancel_and_shutdown_child: failed to send Shutdown to child {child_id}");
        }
    }

    /// Forward a title change to the session thread for in-memory update,
    /// subscriber broadcast, and persistence.
    fn handle_set_session_title(&mut self, session_id: u64, title: String) {
        debug!(session_id, title = %title, "forwarding title change to session");
        match self.active_sessions.get(&session_id) {
            Some(entry) => {
                let _ = entry.cmd_tx.send(SessionCommand::SetTitle {
                    title,
                    // Tool-driven: the agent's `set_session_title` tool has no
                    // client request id to ack.
                    reply: None,
                });
            }
            None => {
                warn!(session_id, "cannot set title: session is not active");
            }
        }
    }

    /// Forward a working-directory change to the session thread for
    /// in-memory update, subscriber broadcast, and persistence.
    fn handle_set_working_dir(
        &mut self,
        session_id: u64,
        path: PathBuf,
        reply: mpsc::Sender<Result<String, String>>,
    ) {
        debug!(session_id, path = %path.display(), "forwarding working dir change to session");
        if let Some(entry) = self.active_sessions.get(&session_id) {
            let _ = entry.cmd_tx.send(SessionCommand::SetWorkingDir {
                path,
                tool_reply: reply,
                // Tool-driven (see handle_set_session_title's note).
                reply: None,
            });
        } else {
            warn!(session_id, "cannot set working dir: session is not active");
            // Reply immediately so the caller (a blocked tool execution)
            // doesn't hang waiting on a session that doesn't exist.
            let _ = reply.send(Err("session is not active".into()));
        }
    }

    /// Forward a tool-group activation to the session thread, which applies
    /// it to the authoritative active-group set and replies with a summary.
    fn handle_load_tools(
        &mut self,
        session_id: u64,
        groups: Vec<String>,
        reply: mpsc::Sender<Result<String, String>>,
    ) {
        debug!(session_id, groups = ?groups, "forwarding load_tools to session");
        if let Some(entry) = self.active_sessions.get(&session_id) {
            let _ = entry
                .cmd_tx
                .send(SessionCommand::LoadTools { groups, reply });
        } else {
            warn!(session_id, "cannot load tools: session is not active");
            // Reply immediately so the caller (a blocked tool execution)
            // doesn't hang waiting on a session that doesn't exist.
            let _ = reply.send(Err("session is not active".into()));
        }
    }

    /// Forward a tool-group deactivation to the session thread, which
    /// applies it to the authoritative active-group set and replies with
    /// a summary.
    fn handle_unload_tools(
        &mut self,
        session_id: u64,
        groups: Vec<String>,
        reply: mpsc::Sender<Result<String, String>>,
    ) {
        debug!(session_id, groups = ?groups, "forwarding unload_tools to session");
        if let Some(entry) = self.active_sessions.get(&session_id) {
            let _ = entry
                .cmd_tx
                .send(SessionCommand::UnloadTools { groups, reply });
        } else {
            warn!(session_id, "cannot unload tools: session is not active");
            // Reply immediately so the caller (a blocked tool execution)
            // doesn't hang waiting on a session that doesn't exist.
            let _ = reply.send(Err("session is not active".into()));
        }
    }

    /// Delete a session, shutting down its thread and removing it from the DB.
    /// If the session has children, they are cascade-deleted first.
    ///
    /// Sessions are just conversation containers — they can be deleted
    /// regardless of whether the daemon is locked, just like they can
    /// be created and browsed freely.  Credentials are only needed to
    /// run models (`RunInput`).
    fn handle_delete_session(
        &mut self,
        session_id: u64,
        reply: &std::sync::mpsc::Sender<io::Result<()>>,
    ) {
        info!("DeleteSession: id={}", session_id);

        // Cascade-delete children before the parent.
        if let Some(children) = self.children.remove(&session_id) {
            for child_id in children {
                self.remove_child_from_all_parents(child_id);
                if let Err(e) = self.delete_session_inner(child_id) {
                    warn!("failed to cascade-delete child {child_id}: {e}");
                }
            }
        }

        // Remove from any parent's children list
        self.remove_child_from_all_parents(session_id);

        match self.delete_session_inner(session_id) {
            Ok(()) => {
                let _ = reply.send(Ok(()));
            }
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }

    /// Fast path for deleting a session whose thread has already terminated
    /// (`JoinHandle::is_finished()` — its final `persist_and_exit` ran and its
    /// `SessionExited` is queued behind this command).  Nothing can re-create
    /// the record now, so delete it immediately — no tombstone write, no
    /// deferred finalize.
    ///
    /// The `deleted_sessions` marker IS set, even though the record is gone:
    /// the thread's straggler `UpdateMetadata` / status messages are queued
    /// *ahead of* its `SessionExited`, and without the marker
    /// `handle_update_metadata` would re-insert the session into the index
    /// (a ghost with no record and no thread).  The queued `SessionExited`
    /// then runs the standard finalize — an idempotent no-op delete here (the
    /// record is already gone), a tombstone clear — and drops the marker.
    fn delete_finished_session(&mut self, session_id: u64) -> io::Result<()> {
        self.deleted_sessions.insert(session_id);
        db::delete_session(&self.db, session_id)?;
        // No pending delete can own a stale tombstone here (the marker was
        // set only now), so sweeping it cannot race a finalize; a leftover
        // tombstone would only trigger a redundant startup purge.
        if let Err(e) = db::clear_session_tombstone(&self.db, session_id) {
            warn!(session_id, error = %e, "failed to clear stale session-deletion tombstone");
        }
        self.session_metadata.remove(&session_id);
        self.broadcast(&DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleted,
        });
        Ok(())
    }

    /// Remove any stale deletion tombstone for `session_id` left by an earlier
    /// interrupted delete.  Callers must only invoke this when no delete is
    /// pending for the id: while a deferred delete's thread is still shutting
    /// down, the tombstone is owned by (and cleared by) its finalize.
    fn clear_stale_session_tombstone(&self, session_id: u64) {
        if let Err(e) = db::clear_session_tombstone(&self.db, session_id) {
            warn!(session_id, error = %e, "failed to clear stale session-deletion tombstone");
        }
    }

    /// Shared session-teardown logic used by both `handle_delete_session`
    /// (with permission checks) and cascade-deletion of children.
    ///
    /// Returns an error only when there is no live thread to defer to and the
    /// immediate DB delete fails; callers decide whether to stop or continue
    /// (cascade-delete continues on error).
    ///
    /// Never blocks the command loop: when the session thread is alive we mark
    /// it deleted and write a deletion tombstone (crash-window safety) BEFORE
    /// sending `Cancel` + `Shutdown`, and let `handle_session_exited` delete
    /// the record once the thread's final `persist_and_exit` lands — no
    /// bounded join here.
    fn delete_session_inner(&mut self, session_id: u64) -> io::Result<()> {
        info!("DeleteSession (inner): id={}", session_id);
        if let Some(entry) = self.active_sessions.remove(&session_id) {
            // Fast path: the session thread has ALREADY terminated (its final
            // `persist_and_exit` ran and its `SessionExited` is queued behind
            // this command).  Delete immediately, but set the deleted marker
            // so the thread's queued straggler messages cannot resurrect the
            // session in the index (see `delete_finished_session`).
            if entry.handle.is_finished() {
                return self.delete_finished_session(session_id);
            }
            // Mark it deleted and write the deletion tombstone FIRST so a
            // crash in the window after `Shutdown` but before the tombstone
            // commits cannot leave a re-created record unmarked for the
            // startup purge; then shut the thread down gracefully.  The
            // record is deleted later in `handle_session_exited` (after
            // `persist_and_exit` has run), so the thread cannot re-create the
            // record after we remove it.
            self.deleted_sessions.insert(session_id);
            if let Err(e) = db::mark_session_deleted(&self.db, session_id) {
                warn!(session_id, error = %e, "failed to write session-deletion tombstone");
            }
            if entry
                .cmd_tx
                .send(SessionCommand::Cancel {
                    stream_id: CANCEL_ALL,
                })
                .is_err()
            {
                warn!("delete_session_inner: failed to send Cancel to session {session_id}");
            }
            if entry.cmd_tx.send(SessionCommand::Shutdown).is_err() {
                warn!("delete_session_inner: failed to send Shutdown to session {session_id}");
            }
        } else {
            // No live thread: nothing can re-create the record, so delete it
            // now.  This is the only path that can fail here.
            db::delete_session(&self.db, session_id)?;
            // Sweep any stale tombstone from an earlier interrupted delete of
            // this id — but only when no delete is still pending.  A pending
            // deferred delete (from an earlier DeleteSession while the thread
            // was alive) owns the tombstone: its thread is still shutting down
            // and can re-create the record via `persist_and_exit` before the
            // finalize clears it, so sweeping here would reopen the crash
            // window (a restart could resurrect the deleted session).
            if !self.deleted_sessions.contains(&session_id) {
                self.clear_stale_session_tombstone(session_id);
            }
        }
        // Remove from in-memory metadata and broadcast deletion immediately:
        // from here on the session is invisible (index removed) and
        // unattachable (deleted marker), even while its record is still being
        // cleaned up in the background.
        self.session_metadata.remove(&session_id);
        self.broadcast(&DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleted,
        });
        Ok(())
    }

    /// Add a new inference account.
    fn handle_add_account(
        &mut self,
        name: &str,
        provider: &str,
        overrides: AccountOverrides,
        reply: &std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let config = overrides.into_config(name, provider);
        let result = self.accounts.add(config);
        match &result {
            Ok(()) => info!(
                account = %name,
                provider = %provider,
                "added inference account"
            ),
            Err(e) => error!(
                account = %name,
                provider = %provider,
                error = %e,
                "failed to add inference account"
            ),
        }
        // If account was added and there's a matching credential, sessions
        // bound to it drop their cached client so the next request rebuilds
        // against the NEW config (the account may have existed before with a
        // different provider/base_url). The model list warms in the
        // background on session join.
        if result.is_ok() {
            self.drop_session_clients(Some(name));
        }
        let _ = reply.send(result);
    }

    /// Remove an inference account.
    fn handle_remove_account(
        &mut self,
        name: &str,
        reply: &std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let result = self.accounts.remove(name);
        match &result {
            Ok(()) => info!(account = %name, "removed inference account"),
            Err(e) => {
                error!(account = %name, error = %e, "failed to remove inference account");
            }
        }
        if result.is_ok() {
            // Invalidate sessions bound to the removed account so their next
            // request surfaces the clean "account not configured" guidance
            // instead of silently dialing the deleted provider.
            self.drop_session_clients(Some(name));
        }
        let _ = reply.send(result);
    }

    /// List all inference accounts (with credential status).
    fn handle_list_accounts(
        &mut self,
        reply: &std::sync::mpsc::Sender<Result<Vec<AccountInfo>, String>>,
    ) {
        let _ = reply.send(Ok(self.account_infos()));
    }

    /// Build the credential-aware `AccountInfo` list. Shared by
    /// [`DaemonCommand::ListAccountsCmd`] (pull) and the external-edit reload
    /// broadcast (push) so both carry the identical payload shape.
    fn account_infos(&self) -> Vec<AccountInfo> {
        // Credential status: decrypted in-memory credentials plus encrypted
        // blobs stored in the DB, so the TUI shows whether each account has
        // had a credential supplied regardless of unlock state.
        let mut credentialed: std::collections::HashSet<String> =
            self.credentials.keys().cloned().collect();
        if let Ok(blobs) = db::get_all_credential_blobs(&self.db) {
            credentialed.extend(blobs.into_keys());
        }
        self.accounts.list(&credentialed)
    }

    /// Enroll a client key: validate the base64/32-byte key, append a
    /// `[[client]]` entry via [`acl::append_key_locked`] (the shared
    /// lock-discipline write used by the CLI too), hot-reload the `SharedAcl`
    /// (this loop is its single writer), broadcast `AclUpdated` so connected
    /// clients see the new trust total, and reply with the count.
    ///
    /// Re-authorizing an ALREADY-present key is a success reply with no
    /// write — idempotent for a client that retries a slow request.
    fn handle_acl_add(&mut self, pubkey: &str, reply: &mpsc::Sender<Result<usize, String>>) {
        use base64::Engine as _;
        let result = (|| -> Result<usize, String> {
            let Some(acl) = &self.acl else {
                return Err("no ACL is loaded (unit-test state)".to_string());
            };
            let key: [u8; 32] = base64::engine::general_purpose::STANDARD
                .decode(pubkey.trim())
                .map_err(|e| format!("invalid pubkey: not valid base64: {e}"))?
                .try_into()
                .map_err(|_| "invalid pubkey: must decode to exactly 32 bytes".to_string())?;

            // Idempotency: an already-trusted key is a successful no-op.
            if acl.contains(&key) {
                return Ok(acl.len());
            }

            crate::server::acl::append_key_locked(acl.path(), &key)?;

            // Single-writer reload: the parse-compare inside reload makes
            // this the authoritative snapshot update.
            //
            // Note: append_key_locked does fsync-able file I/O ON THE
            // COMMAND LOOP — the one thread all daemon state serializes
            // through. This is a deliberate, accepted trade: the write is
            // rare (only on actual enrollment), small (one ~70-byte
            // append), and the command loop already performs comparable
            // blocking I/O in its other handler paths; moving it to a
            // worker thread would add cross-thread coordination for a
            // once-per-enrollment millisecond-scale stall.
            acl.reload();

            Ok(acl.len())
        })();

        if let Ok(count) = &result {
            info!(clients = count, "ACL: client key enrolled (hot-reload)");
            // Connection-level control broadcast (no session origin): every
            // connected client learns the new trust total.
            self.handle_broadcast_activity(
                None,
                &DaemonMessage::broadcast(DaemonMessageType::AclUpdated {
                    clients: *count as u64,
                }),
            );
        }
        let _ = reply.send(result);
    }

    /// Handle an `authorized_clients.toml` watcher event: hand the reload to
    /// the `SharedAcl` (the command loop is its single writer — re-read,
    /// parse-compare, atomic swap all live inside `reload`). A unit-test
    /// `DaemonState` has no ACL (`None`) and the event is a logged no-op.
    /// No reply: fire-and-forget, mirroring `handle_accounts_reload`.
    fn handle_acl_reload(&mut self) {
        match &self.acl {
            Some(acl) => {
                debug!(
                    path = %acl.path().display(),
                    "AclReload: re-reading authorized_clients.toml"
                );
                acl.reload();
            }
            None => {
                debug!("AclReload ignored: no ACL installed (unit-test state)");
            }
        }
    }

    /// Re-read `accounts.toml` after a watcher event and apply a real change.
    ///
    /// This is the single writer of `state.accounts`, so all reload policy
    /// lives here: re-read, **parse-compare** against the in-memory manager,
    /// and apply only a logical difference (a byte compare would false-positive
    /// on the daemon's own rewrites, whose serialization order the deterministic
    /// [`AccountManager::save`] now keeps stable). Removed accounts drop their
    /// cached provider (a stale provider for a gone account is dead weight);
    /// accounts whose config *changed* drop and **rebuild** their provider so
    /// the cache reflects the new config instead of serving a stale one.
    /// credentials are left intact — a credential with no account is inert, and
    /// pruning it automatically could surprise a user who is mid-migration.
    /// A successful apply broadcasts the fresh account list so connected
    /// clients can refresh their pickers live. A read/parse failure keeps the
    /// current accounts rather than churn on a transient error.
    fn handle_accounts_reload(&mut self) {
        // Only meaningful after unlock, when the manager holds a real path.
        // Before that the in-memory manager is empty and there is nothing to
        // reload (the watcher runs regardless of unlock state). The path is
        // copied to an owned value so no borrow of `self.accounts` outlives
        // the reassignment below.
        let path = self.accounts.path().to_path_buf();
        if path.as_os_str().is_empty() {
            debug!("accounts reload requested before unlock; ignoring");
            return;
        }
        let fresh = match AccountManager::load(&path) {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to reload accounts from disk; keeping the current accounts",
                );
                return;
            }
        };
        if fresh.all_configs() == self.accounts.all_configs() {
            // A no-op edit (the daemon's own save, or a rewrite with identical
            // logical content) must not broadcast or churn.
            debug!(path = %path.display(), "accounts.toml changed but accounts are unchanged");
            return;
        }
        // Snapshot the OLD configs by name BEFORE `self.accounts` is reassigned
        // below, so removed accounts can be told apart from merely modified ones.
        // Owned values (not references) so the snapshot survives the reload.
        let old_by_name: HashMap<String, AccountConfig> = self
            .accounts
            .all_configs()
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect();
        let fresh_by_name: HashMap<String, AccountConfig> = fresh
            .all_configs()
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect();

        // Split the diff into removed vs changed accounts.
        let mut removed: Vec<String> = Vec::new();
        let mut changed: Vec<String> = Vec::new();
        for (name, old_cfg) in &old_by_name {
            match fresh_by_name.get(name) {
                None => removed.push(name.clone()),
                Some(new_cfg) if new_cfg != old_cfg => changed.push(name.clone()),
                Some(_) => {}
            }
        }

        // Accounts that vanished: invalidate sessions bound to them (their
        // cached client would keep dialing a dead provider) and leave
        // credentials intact (a credential with no account is inert).
        // (At this point `self.accounts` still holds the OLD configs, so each
        // `removed` name is genuinely present in it.)
        for name in &removed {
            warn!(
                account = name,
                "account removed from accounts.toml externally; invalidating its session clients",
            );
        }
        // A non-empty → empty transition (the file was deleted or emptied
        // externally) drops every account; warn loudly, since this is
        // destructive and likely accidental.
        if !old_by_name.is_empty() && fresh.is_empty() {
            warn!(
                path = %path.display(),
                "accounts.toml became empty/missing externally; all accounts were removed",
            );
        }
        self.accounts = fresh;
        info!(path = %path.display(), "accounts reloaded from disk");
        // Accounts present in BOTH files but with a different config (e.g. the
        // provider protocol or an override changed): invalidate the cached
        // client of every session bound to them so the next request rebuilds
        // against the NEW config + still-held credential. This mirrors the
        // /lock invalidation; without it the session would keep dialing the
        // old file's endpoint forever.
        for name in &changed {
            warn!(
                account = name,
                "account config changed externally; invalidating its session clients",
            );
        }
        // Invalidate ONLY the sessions bound to a removed or changed
        // account — per-account targeting, matching how /lock (all) and
        // RemoveCredential (one account) already scope their invalidation.
        // The previous blanket `drop_session_clients(None)` over-invalidated
        // sessions bound to UNTOUCHED accounts: they tore down cached
        // clients and their HTTP connection pools and rebuilt on the next
        // request for no reason. The diff computed above says exactly which
        // accounts changed — use it. Added names cannot have existing sessions
        // bound to them (the session was bound BEFORE the reload), so additions
        // need no invalidation.
        //
        // Each touched account also gets its (non-secret) provider slug
        // refreshed — a removed account clears it (`None`), a changed one
        // pushes the new catalog key — so slug-keyed static facts stay exact
        // without waiting for the next request to rebuild the client.
        // Order matters: invalidate the client first, then refresh the slug.
        for name in &removed {
            self.drop_session_clients(Some(name));
            self.set_session_provider_slug(name, None);
        }
        for name in &changed {
            let slug = self.account_provider_slug(name);
            self.drop_session_clients(Some(name));
            self.set_session_provider_slug(name, slug.as_deref());
        }
        // Push the fresh list to activity subscribers (global/control
        // provenance — a flat, non-session message — so no origin-contract
        // dedup runs). Clients can refresh their account pickers live.
        let accounts = self.account_infos();
        self.handle_broadcast_activity(
            None,
            &DaemonMessage::broadcast(DaemonMessageType::Accounts { accounts }),
        );
    }

    /// Resolve the cache-warming policy for a session's account: the daemon's
    /// loaded `[cache_warming]` table merged with the account's `meter`/
    /// `cache_warming`/`prompt_cache` overrides. An unknown/`None` account gets
    /// the conservative default (never warm). Shared by `spawn_session` (the
    /// initial policy) and `handle_resolve_account` (the refresh the session
    /// applies on every (re-)resolve).
    fn warm_policy_for(&self, account: Option<&str>) -> WarmPolicy {
        let account = account.and_then(|name| self.accounts.get(name));
        WarmPolicy::resolve(
            &self.cache_warming,
            account.and_then(|a| a.meter),
            account.and_then(|a| a.cache_warming),
            account.and_then(|a| a.prompt_cache).unwrap_or(true),
        )
    }

    /// Reply to a session's lazy provider-resolution request with the raw
    /// ingredients (config + API key) plus the resolved warm policy. The session
    /// thread builds the client itself, against its own socket registry. The key
    /// is wrapped in `Zeroizing` here — the single hop where the daemon hands
    /// cleartext across a thread boundary — so unconsumed replies are wiped on
    /// drop.
    fn handle_resolve_account(&mut self, account: &str, reply: &ResolveAccountReply) {
        let resolved = self.accounts.get(account).map(|config| ResolvedAccount {
            config: config.clone(),
            // api_key_for returns an inert String for internal gates
            // (prefetch/validate checks); this reply is the credential
            // EXIT point, so the wipe-on-drop wrapper goes on here.
            api_key: self.api_key_for(account).map(Zeroizing::new),
            warm_policy: self.warm_policy_for(Some(account)),
        });
        let _ = reply.send(resolved);
    }

    /// Check whether an account with the given name exists.
    fn handle_account_exists(&mut self, name: &str, reply: &std::sync::mpsc::Sender<bool>) {
        let _ = reply.send(self.accounts.contains(name));
    }
}

/// Build the slug + display-name pair list for a `CatalogUpdated` broadcast
/// from the currently swapped catalog. Shared by the broadcast and the
/// send-on-subscribe path so both carry the identical payload shape.
fn catalog_provider_pairs() -> Vec<CatalogProvider> {
    catalog_snapshot()
        .iter()
        .map(|e| CatalogProvider {
            slug: e.slug.clone(),
            display_name: e.display_name.clone(),
        })
        .collect()
}

/// Merge the layered catalog: normalized models.dev base → bundled overlay →
/// user overlay (lowest → highest wins, matching `merge_overlay` semantics).
/// Extracted so `handle_catalog_base_changed` reads as a straight-line
/// pipeline and the layer order is pinned in one place.
fn merge_catalog_layers(
    base: &[choreo_ai_protocols::ProviderEntry],
    user_overlay: Option<&str>,
) -> Vec<choreo_ai_protocols::ProviderEntry> {
    let mut effective = merge_overlay(base, bundled_overlay_src());
    if let Some(overlay) = user_overlay {
        effective = merge_overlay(&effective, overlay);
    }
    effective
}

/// Fan a `/refresh-models` reply out to every requester in a coalesced batch
/// once the swap has happened. Each requester's status reflects its OWN force
/// flag: the batch's shared fetch is forced if ANY requester asked
/// (`fold_refresh_nows` ORs the flags), but a plain request folded into a
/// forced burst is reported `Updated`, not `Forced` — matching what it
/// actually asked for. An empty `reply` (background events) is a no-op.
fn send_catalog_reply(reply: Vec<RefreshRequester>, providers: usize, models: usize) {
    for r in reply {
        let status = if r.force {
            RefreshStatus::Forced
        } else {
            RefreshStatus::Updated
        };
        let _ = r.tx.send(Ok(RefreshReport {
            providers,
            models,
            status,
        }));
    }
}

/// Handle a platform suspend/wake event on the daemon command loop.
/// Factored out of `handle_command` so the policy is unit-testable without a
/// full `DaemonState`.
///
/// * `Sleep`: force-close every registered provider socket BEFORE the machine
///   suspends — the logind event arrives before suspension, so this is the
///   one window where the closure is proactive rather than reactive. Both
///   the daemon-owned registry (prefetch/maintenance clients) and EVERY
///   session's registry are closed — this runs on the command loop, which
///   owns [`DaemonState`], so the per-session clones are right here. Any
///   worker blocked in a provider `read()` wakes immediately with an error;
///   after resume the sockets would be dead anyway (the OS's TCP state is
///   gone), so nothing is lost.
/// * `Wake`: log only. Sockets that survived the sleep are dead on resume;
///   the kernel keepalive tuning from `choreo-sockreg` notices them on the
///   next use, and clients re-establish connections lazily. No shutdown here:
///   `shutdown_all` on wake would add nothing (the sleep path already
///   cleared the registry) and could only disturb fresh connections.
fn handle_suspend_event(
    // Copy enum: taken by value (the lint flags the needless `&`).
    event: SuspendEvent,
    daemon_registry: &SocketRegistry,
    session_registries: &HashMap<u64, SocketRegistry>,
) {
    match event {
        SuspendEvent::Sleep => {
            // Read the count BEFORE the shutdown consumes the lists, so the
            // log reports what was actually closed.
            let daemon_sockets = daemon_registry.registered_count();
            let session_sockets: usize = session_registries
                .values()
                .map(choreo_ai_protocols::SocketRegistry::registered_count)
                .sum();
            info!(
                daemon_sockets,
                session_sockets,
                sessions = session_registries.len(),
                "machine sleeping: force-closing provider sockets \
                 ({} daemon-owned, {} across {} session registries)",
                daemon_sockets,
                session_sockets,
                session_registries.len()
            );
            daemon_registry.shutdown_all();
            for (session_id, registry) in session_registries {
                registry.shutdown_all();
                trace!(
                    session_id,
                    "session provider sockets force-closed for sleep"
                );
            }
        }
        SuspendEvent::Wake => {
            // Sockets that survived an UNANNOUNCED suspend (a missed Sleep
            // event — the power monitor is explicitly best-effort) are dead
            // but still registered: prune them now instead of waiting for
            // the opportunistic 256-entry prune. This is defense-in-depth,
            // not a correctness dependency: with a well-delivered Sleep the
            // registries are already empty, so this is normally a no-op —
            // and when there ARE entries to probe, the probe is non-blocking
            // (MSG_PEEK with an O_NONBLOCK flag flip, EOF/EAGAIN verdicts)
            // and bounded by registry size, so it is safe on the command
            // loop. Live connections are never disturbed on Wake.
            let daemon_pruned = daemon_registry.prune_dead();
            let mut session_pruned = 0;
            for registry in session_registries.values() {
                session_pruned += registry.prune_dead();
            }
            if daemon_pruned > 0 || session_pruned > 0 {
                info!(
                    daemon_pruned,
                    session_pruned, "pruned dead provider sockets after wake"
                );
            }
            info!(
                "machine woke from suspend; stale provider sockets pruned, \
                 any survivors re-established lazily"
            );
        }
    }
}

fn handle_list_models_inner(
    state: &mut DaemonState,
    session_id: Option<u64>,
) -> Result<(Vec<String>, Option<String>), String> {
    let account_name = session_id
        .and_then(|sid| state.session_metadata.get(&sid))
        .and_then(|m| m.account_name.clone())
        .unwrap_or_default();

    debug!(
        "ListModels: session_id={:?}, account_name='{}', accounts={:?}",
        session_id,
        account_name,
        state
            .accounts
            .all_configs()
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
    );

    // Existence + credential check only — no provider instance is needed
    // below, because the actual fetch (if any) runs on the detached
    // background thread.
    if !state.accounts.contains(&account_name) || state.api_key_for(&account_name).is_none() {
        return Err(if state.accounts.is_empty() {
            "no accounts configured".to_string()
        } else {
            format!("no credential stored for account '{account_name}'")
        });
    }

    // A fresh cache answers immediately. Otherwise the fetch NEVER runs here
    // synchronously: a blocking HTTP round-trip (up to the full request
    // timeout, retried) would stall the whole daemon command loop — the
    // exact stall the background-prefetch design removed from unlock. The
    // request instead TRIGGERS a background prefetch (dedup-guarded via
    // `maybe_spawn_model_prefetch` — no-op when one is already running, so
    // an open picker while a join-time prefetch is in flight does not
    // double-fetch) and serves what it can:
    //   - a stale-but-present list beats nothing, so it is served;
    //   - with nothing cached at all, a retryable "warming" error is
    //     returned and the client refetches once the prefetch lands.
    let now = Instant::now();
    let models = match state.model_cache.get(&account_name) {
        Some((cached_models, cached_at)) if now.duration_since(*cached_at) < MODEL_CACHE_TTL => {
            cached_models.clone()
        }
        _ => {
            // Clone the stale list (if any) BEFORE the mutable spawn call,
            // so no borrow of `state` is live across it.
            let stale = state
                .model_cache
                .get(&account_name)
                .map(|(models, _)| models.clone());
            state.maybe_spawn_model_prefetch(&account_name);
            stale.ok_or_else(|| {
                format!(
                    "model list for account '{account_name}' is warming in the \
                     background; retry in a moment"
                )
            })?
        }
    };

    let selected_model = session_id
        .and_then(|sid| state.session_metadata.get(&sid))
        .and_then(|m| m.selected_model.clone());
    Ok((models, selected_model))
}

#[cfg(test)]
mod tests;
