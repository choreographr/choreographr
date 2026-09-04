use crate::accounts::{AccountConfig, AccountManager, accounts_config_path};
use crate::broadcast::{LagLimits, SubscriberSink};
use crate::catalog::{CatalogPaths, MaintenanceEvent, RefreshReport, RefreshRequester};
use crate::db::{self, SessionRecord};
use crate::mcp::McpManager;
use crate::providers::InferenceProvider;
use crate::sessions::{
    ActiveSessionEntry, CANCEL_ALL, RequestContext, SessionCommand, SessionMetadata, session_main,
};
use choreo_ai_protocols::{
    bundled_overlay_src, catalog_snapshot, lookup_context_window, merge_overlay, replace_catalog,
};
use choreo_keystore::ServiceCredential;
use choreo_proto::{
    AccountInfo, CatalogProvider, ContextConfig, DaemonMessage, RefreshStatus, SessionEvent,
    SessionStatus, SessionSummary, TimestampMs, TokenUsage,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tracing::{debug, error, info, warn};
use zeroize::Zeroize;

mod subscriber_handlers;

/// TTL for cached provider model lists. Shared by the freshness checks in
/// `handle_list_models_inner` and the background-prefetch guard
/// (`should_prefetch_models`) so both paths agree on what "fresh" means.
const MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

/// Reply type for the ListModels command.
pub(super) type ListModelsReply =
    std::sync::mpsc::Sender<Result<(Vec<String>, Option<String>), String>>;

pub struct DaemonState {
    pub next_session_id: u64,
    pub max_turns: u32,
    pub active_sessions: HashMap<u64, ActiveSessionEntry>,
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
    pub accounts: AccountManager,
    pub providers: HashMap<String, InferenceProvider>,
    pub credentials: HashMap<String, ServiceCredential>,
    pub x_credentials: Option<ServiceCredential>,
    /// Whether the credential keystore is currently locked (no decrypted
    /// credentials in memory). Starts `true` at daemon startup — the keystore
    /// is only decrypted into memory once a valid unlock key is presented —
    /// flips to `false` on a successful Unlock / AddCredential implicit
    /// unlock, and back to `true` on `/lock`. This is the authoritative
    /// daemon-side lock state: it is broadcast to all activity subscribers on
    /// every transition and pushed to each fresh activity subscriber at
    /// subscribe time, so client UIs latch the real state instead of guessing.
    pub locked: bool,
    pub db: Arc<redb::Database>,
    pub tool_registry: Arc<crate::tools::ToolRegistry>,
    pub daemon_tx: mpsc::Sender<DaemonCommand>,
    pub summary_subscribers: HashMap<u64, SubscriberSink>,
    /// Writer channel of EVERY connected client (both transports), registered
    /// on connect and removed on disconnect. The shutdown path uses it to
    /// route `ShuttingDown` through each connection's single writer thread —
    /// distinct from the opt-in summary/activity subscriber maps.
    pub client_writers: HashMap<u64, SubscriberSink>,
    pub activity_subscribers: HashMap<u64, SubscriberSink>,
    /// Tracks which clients are direct session subscribers of which sessions.
    /// Used by `handle_broadcast_activity` to skip duplicate delivery to
    /// clients that are both activity subscribers AND session subscribers
    /// — the message reaches them through the per-session subscriber path.
    pub client_subscribed_sessions: HashMap<u64, HashSet<u64>>,
    /// Daemon-wide bytes in flight to every connected client's queue, shared
    /// by ALL subscriber sinks (see `broadcast::SubscriberSink::enqueue`).
    /// The 6th sanctioned shared-state exception (see AGENTS.md); writers
    /// decrement it on every dequeue, eviction releases a client's remainder.
    pub global_lag: Arc<AtomicUsize>,
    /// Lag thresholds (per-client cap + daemon-wide budget). Injectable so
    /// tests can use tiny caps; defaults are 64 MiB / 512 MiB.
    pub lag_limits: LagLimits,
    pub model_cache: HashMap<String, (Vec<String>, Instant)>,
    /// Accounts with a model-list prefetch currently running on a background
    /// thread. The command loop sets a name when it spawns the fetch thread
    /// and clears it when the thread's `ModelPrefetchResult` arrives — the
    /// guard that keeps several session joins on the same account from
    /// spawning duplicate HTTP fetches. Managed exclusively by the command
    /// loop (single writer); the fetch threads themselves never touch it —
    /// they report back through the `daemon_tx` channel.
    pub model_prefetch_in_flight: HashSet<String>,
    pub mcp_manager: McpManager,
    /// Sender to the ONE background catalog-maintenance thread (see
    /// `crate::catalog`). `None` until `run_server` spawns the thread — a
    /// unit-test DaemonState has no maintenance thread, and `/refresh-models`
    /// then replies with an error instead of hanging.
    pub maintenance_tx: Option<crossbeam_channel::Sender<MaintenanceEvent>>,
    /// The hot-reloadable client ACL (see `crate::server::acl::SharedAcl`).
    /// `None` until `run_server` installs it — a unit-test DaemonState has
    /// no ACL file to reload, and `AclReload` then logs and no-ops instead
    /// of touching state it does not own.
    pub acl: Option<std::sync::Arc<crate::server::acl::SharedAcl>>,
    /// Filesystem locations of the runtime catalog cache + user overlay
    /// (resolved from the standard XDG dirs; see `crate::catalog`).
    pub catalog_paths: CatalogPaths,
}

pub enum DaemonCommand {
    Shutdown,
    CreateSession {
        title: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<PathBuf>,
        reasoning_effort: Option<String>,
        selected_model: Option<String>,
        context_config: Option<ContextConfig>,
        account_name: Option<String>,
        active_tool_groups: Vec<String>,
        reply: std::sync::mpsc::Sender<io::Result<(u64, std::sync::mpsc::Sender<SessionCommand>)>>,
    },
    AttachSession {
        session_id: u64,
        reply: std::sync::mpsc::Sender<io::Result<std::sync::mpsc::Sender<SessionCommand>>>,
    },
    ListSessions {
        reply: std::sync::mpsc::Sender<Vec<SessionSummary>>,
    },
    GetSession {
        session_id: u64,
        reply: std::sync::mpsc::Sender<Option<SessionSummary>>,
    },
    UpdateMetadata {
        session_id: u64,
        metadata: SessionMetadata,
    },
    SessionExited {
        session_id: u64,
    },
    /// Sent by the background delete-finalize thread after it has removed the
    /// record of a deleted session (and cleared its tombstone).  Distinct from
    /// `SessionExited`: the record is only gone once this re-delete commits, so
    /// only this message drops the `deleted_sessions` marker.
    SessionDeleteFinalized {
        session_id: u64,
    },
    Unlock {
        private_key: Vec<u8>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// Lock the daemon's keystore: clear all decrypted in-memory credentials
    /// (and their cached providers) and flip `locked` back to `true`. The
    /// cleartext is wiped from memory; the encrypted blobs stay in the DB.
    /// Broadcasts the `Locked` state to all activity subscribers so every
    /// connected client's lock banner reappears. Sessions themselves are
    /// untouched — they remain browsable, only inference is disabled until the
    /// next unlock.
    Lock {
        reply: mpsc::Sender<Result<(), String>>,
    },
    SaveCredential {
        service: String,
        encrypted_blob: Vec<u8>,
        /// REQUIRED (per-daemon keystore TOFU design): the raw X25519 private
        /// key the credential blob was encrypted with. The daemon adopts it
        /// (first contact) or verifies it against the stored binding, uses it
        /// to test-decrypt + persist the blob, and then performs the implicit
        /// unlock (same tail as `Unlock`).
        unlock_key: Vec<u8>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    RemoveCredentialCmd {
        service: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Enroll a client key in the ACL (from a LOCAL connection only — the
    /// transport check lives in the connection dispatch). The handler
    /// validates, appends to `authorized_clients.toml` under the advisory
    /// file lock, hot-reloads the SharedAcl (single writer), broadcasts
    /// `AclUpdated`, and replies with the new total.
    AclAddCmd {
        pubkey: String,
        reply: mpsc::Sender<Result<usize, String>>,
    },
    ListModels {
        session_id: Option<u64>,
        reply: ListModelsReply,
    },
    /// A client requested `/refresh-models`. The daemon does NOT do the HTTP
    /// fetch here — it hands the request to the maintenance thread over its
    /// channel (the fetch can block for the whole 30s timeout), and the reply
    /// is routed back through [`DaemonCommand::CatalogBaseChanged`] (fetched)
    /// or [`DaemonCommand::CatalogNotModified`] (304) once the maintenance
    /// thread has a result.
    RefreshModels {
        force: bool,
        reply: mpsc::Sender<Result<RefreshReport, String>>,
    },
    /// The maintenance thread delivered a (possibly refreshed) models.dev
    /// base + the current user overlay. The daemon command loop — the single
    /// writer of the catalog `ArcSwap` — merges overlays, swaps the catalog,
    /// optionally persists the cache, broadcasts `CatalogUpdated`, and
    /// replies to the `/refresh-models` requester(s).
    CatalogBaseChanged {
        base: Vec<choreo_ai_protocols::ProviderEntry>,
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
        reply: Vec<RefreshRequester>,
    },
    GetCredential {
        service: String,
        reply: std::sync::mpsc::Sender<Option<String>>,
    },
    /// A background model-prefetch thread (spawned by
    /// [`DaemonState::maybe_spawn_model_prefetch`]) finished fetching an
    /// account's model list. Routed through the command loop — the single
    /// writer of `model_cache` — so the insert is serialized with all other
    /// cache mutations. `result` carries the fetch outcome so the loop can
    /// release the per-account in-flight guard even on failure (otherwise a
    /// failed fetch would permanently block re-prefetching that account).
    ModelPrefetchResult {
        account: String,
        result: Result<Vec<String>, String>,
    },
    RegisterSummarySubscriber {
        client_id: u64,
        writer: SubscriberSink,
    },
    UnregisterSummarySubscriber {
        client_id: u64,
    },
    RegisterActivitySubscriber {
        client_id: u64,
        writer: SubscriberSink,
    },
    UnregisterActivitySubscriber {
        client_id: u64,
    },
    /// Track that a client is now a direct subscriber of a session.
    /// The daemon uses this to avoid duplicate delivery through the
    /// activity subscriber path (see `handle_broadcast_activity`).
    TrackSessionSubscription {
        client_id: u64,
        session_id: u64,
    },
    /// Untrack that a client is no longer a direct subscriber of a session.
    UntrackSessionSubscription {
        client_id: u64,
        session_id: u64,
    },
    /// Clean up all per-client tracking when a client disconnects.
    /// Removes from summary subscribers, activity subscribers, and session
    /// subscription tracking in a single atomic command.
    ClientDisconnected {
        client_id: u64,
    },
    /// Register a connection's writer channel so the shutdown path can route
    /// `ShuttingDown` through that connection's single writer thread.
    RegisterClientWriter {
        client_id: u64,
        writer: SubscriberSink,
    },
    /// Disconnect a client that fell too far behind its delivery queue (see
    /// `broadcast::EnqueueOutcome::ClientOverLag`). Idempotent.
    EvictClient {
        client_id: u64,
    },
    /// Disconnect the currently most-lagging client (see
    /// `broadcast::EnqueueOutcome::GlobalOverBudget`).
    EvictLargestLagging,
    /// Deliver `DaemonMessage::ShuttingDown` to every connected client via its
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
        session_id: Option<u64>,
        msg: DaemonMessage,
    },
    BroadcastSessionStatus {
        session_id: u64,
        status: SessionStatus,
    },
    DeleteSession {
        session_id: u64,
        reply: std::sync::mpsc::Sender<io::Result<()>>,
    },
    AddAccountCmd {
        name: String,
        provider: String,
        base_url: Option<String>,
        streaming: Option<bool>,
        retry_max_attempts: Option<u32>,
        connect_timeout_secs: Option<u64>,
        request_timeout_secs: Option<u64>,
        total_timeout_secs: Option<u64>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    RemoveAccountCmd {
        name: String,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    ListAccountsCmd {
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
    /// sanctioned ArcSwap exception #4): it is the one that calls
    /// `SharedAcl::reload` (re-read, parse-compare, atomic swap). The TCP
    /// accept path only ever READS lock-free snapshots. No reply:
    /// fire-and-forget, like `AccountsReload`.
    AclReload,
    ResolveProviderCmd {
        account: String,
        reply: std::sync::mpsc::Sender<Option<InferenceProvider>>,
    },
    AccountExists {
        name: String,
        reply: std::sync::mpsc::Sender<bool>,
    },
    ValidateModel {
        session_id: u64,
        model: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Cancel the active request in a session and propagate cancellation
    /// to any child sub-sessions.  The daemon handles child propagation
    /// directly so that leaf sessions never generate unnecessary messages.
    CancelRequest {
        session_id: u64,
        request_id: u32,
    },
    /// Set the display title for a session, forwarded to the session's
    /// main loop for in-memory update, broadcast, and persistence.
    SetSessionTitle {
        session_id: u64,
        title: String,
    },
    /// Set the session working directory, forwarded to the session's main
    /// loop for in-memory update, broadcast, and persistence.  The session
    /// replies once the change has been applied; the daemon replies with an
    /// error immediately if the session is inactive so the caller (a blocked
    /// tool execution) never hangs.
    SetWorkingDir {
        session_id: u64,
        path: PathBuf,
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Activate tool groups.  Forwarded to the session's main loop, which
    /// applies the change to the authoritative active-group set and replies
    /// with a summary of what changed.
    LoadTools {
        session_id: u64,
        groups: Vec<String>,
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Deactivate tool groups ("core" is protected).  Forwarded to the
    /// session's main loop, which applies the change and replies with a
    /// summary of what changed.
    UnloadTools {
        session_id: u64,
        groups: Vec<String>,
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
    db: Arc<redb::Database>,
    session_id: u64,
    daemon_tx: mpsc::Sender<DaemonCommand>,
) {
    match db::delete_session(&db, session_id) {
        Ok(()) => {
            // The deletion tombstone (written by `delete_session_inner`) is no
            // longer needed now that the record is gone for good.
            if let Err(e) = db::clear_session_tombstone(&db, session_id) {
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

impl DaemonState {
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
                title,
                parent_session_id,
                working_dir,
                reasoning_effort,
                selected_model,
                context_config,
                account_name,
                active_tool_groups,
                reply,
            ),
            DaemonCommand::AttachSession { session_id, reply } => {
                self.handle_attach_session(session_id, reply)
            }
            DaemonCommand::ListSessions { reply } => self.handle_list_sessions(reply),
            DaemonCommand::GetSession { session_id, reply } => {
                self.handle_get_session(session_id, reply)
            }
            DaemonCommand::UpdateMetadata {
                session_id,
                metadata,
            } => self.handle_update_metadata(session_id, metadata),
            DaemonCommand::SessionExited { session_id } => self.handle_session_exited(session_id),
            DaemonCommand::SessionDeleteFinalized { session_id } => {
                self.handle_session_delete_finalized(session_id)
            }
            DaemonCommand::Unlock { private_key, reply } => self.handle_unlock(private_key, reply),
            DaemonCommand::Lock { reply } => self.handle_lock(reply),
            DaemonCommand::SaveCredential {
                service,
                encrypted_blob,
                unlock_key,
                reply,
            } => self.handle_save_credential(service, encrypted_blob, unlock_key, reply),
            DaemonCommand::RemoveCredentialCmd { service, reply } => {
                self.handle_remove_credential(service, reply)
            }
            DaemonCommand::AclAddCmd { pubkey, reply } => self.handle_acl_add(pubkey, reply),
            DaemonCommand::ListModels { session_id, reply } => {
                self.handle_list_models(session_id, reply)
            }
            DaemonCommand::RefreshModels { force, reply } => {
                self.handle_refresh_models(force, reply)
            }
            DaemonCommand::CatalogBaseChanged {
                base,
                etag,
                user_overlay,
                persist,
                reply,
            } => self.handle_catalog_base_changed(base, etag, user_overlay, persist, reply),
            DaemonCommand::CatalogNotModified { reply } => self.handle_catalog_not_modified(reply),
            DaemonCommand::GetCredential { service, reply } => {
                self.handle_get_credential(service, reply)
            }
            DaemonCommand::ModelPrefetchResult { account, result } => {
                self.handle_model_prefetch_result(account, result)
            }
            DaemonCommand::RegisterSummarySubscriber { client_id, writer } => {
                self.handle_register_summary_subscriber(client_id, writer)
            }
            DaemonCommand::UnregisterSummarySubscriber { client_id } => {
                self.handle_unregister_summary_subscriber(client_id)
            }
            DaemonCommand::RegisterActivitySubscriber { client_id, writer } => {
                self.handle_register_activity_subscriber(client_id, writer)
            }
            DaemonCommand::UnregisterActivitySubscriber { client_id } => {
                self.handle_unregister_activity_subscriber(client_id)
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
                self.handle_client_disconnected(client_id)
            }
            DaemonCommand::RegisterClientWriter { client_id, writer } => {
                self.handle_register_client_writer(client_id, writer)
            }
            DaemonCommand::EvictClient { client_id } => self.handle_evict_client(client_id),
            DaemonCommand::EvictLargestLagging => self.handle_evict_largest_lagging(),
            DaemonCommand::BroadcastShuttingDown => self.handle_broadcast_shutting_down(),
            DaemonCommand::BroadcastActivity { session_id, msg } => {
                self.handle_broadcast_activity(session_id, msg)
            }
            DaemonCommand::BroadcastSessionStatus { session_id, status } => {
                self.handle_broadcast_session_status(session_id, status)
            }
            DaemonCommand::DeleteSession { session_id, reply } => {
                self.handle_delete_session(session_id, reply)
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
                name,
                provider,
                base_url,
                streaming,
                retry_max_attempts,
                connect_timeout_secs,
                request_timeout_secs,
                total_timeout_secs,
                reply,
            ),
            DaemonCommand::RemoveAccountCmd { name, reply } => {
                self.handle_remove_account(name, reply)
            }
            DaemonCommand::ListAccountsCmd { reply } => self.handle_list_accounts(reply),
            DaemonCommand::AccountsReload => self.handle_accounts_reload(),
            DaemonCommand::AclReload => self.handle_acl_reload(),
            DaemonCommand::ResolveProviderCmd { account, reply } => {
                self.handle_resolve_provider(account, reply)
            }
            DaemonCommand::AccountExists { name, reply } => self.handle_account_exists(name, reply),
            DaemonCommand::ValidateModel {
                session_id,
                model,
                reply,
            } => self.handle_validate_model(session_id, model, reply),
            DaemonCommand::CancelRequest {
                session_id,
                request_id,
            } => self.handle_cancel_request(session_id, request_id),
            DaemonCommand::SetSessionTitle { session_id, title } => {
                self.handle_set_session_title(session_id, title)
            }
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
    ) -> mpsc::Sender<SessionCommand> {
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

        // Resolve provider from the session's account name
        let account_name = metadata.account_name.clone();
        let provider = account_name
            .as_ref()
            .and_then(|name| self.providers.get(name))
            .cloned();

        let (session_tx, session_rx) = std::sync::mpsc::channel();
        let cmd_tx = session_tx.clone();

        let handle = thread::spawn(move || {
            session_main(
                session_rx,
                provider,
                account_name,
                Some(record),
                RequestContext {
                    cmd_tx,
                    session_id,
                    db,
                    tool_registry,
                    daemon_tx,
                    max_turns,
                    lag_limits,
                    global_lag,
                    substrate_credential,
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

    /// Try to resolve an `InferenceProvider` for the given account name using
    /// the stored credential.  Silently ignores missing credentials or config.
    /// This is a pure in-memory operation (client construction from config —
    /// no I/O); the model list is warmed separately, in the background, by
    /// [`Self::maybe_spawn_model_prefetch`], so unlocking and credential/
    /// account mutations never block the command loop on HTTP round-trips.
    /// Returns `true` if a provider was successfully created and cached.
    fn resolve_account_provider(&mut self, name: &str, api_key: Option<String>) -> bool {
        if let Some(config) = self.accounts.get(name)
            && let Ok(provider) = InferenceProvider::from_account_config(config, api_key)
        {
            self.providers.insert(name.to_string(), provider);
            true
        } else {
            false
        }
    }

    /// Decide whether the model list for `account` needs a background
    /// prefetch: only when a resolved provider exists, no fetch is already
    /// running for the account, and the cached list is missing or past
    /// [`MODEL_CACHE_TTL`].  Pure — no side effects — so tests can assert the
    /// gate independently of thread spawning.
    fn should_prefetch_models(&self, account: &str) -> bool {
        if !self.providers.contains_key(account) || self.model_prefetch_in_flight.contains(account)
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
        // `should_prefetch_models` guarantees the provider exists; the None
        // arm is belt-and-braces so the in-flight guard can never leak.
        let provider = match self.providers.get(account) {
            Some(p) => p.clone(),
            None => {
                self.model_prefetch_in_flight.remove(account);
                return;
            }
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
                // Only cache while the account still has a resolved provider:
                // the account may have been removed or reconfigured
                // (`AccountsReload` rebuilds the provider) while the fetch
                // was in flight, and inserting then would serve a dead
                // provider's model list for a full TTL.
                if !self.providers.contains_key(&account) {
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

    #[expect(clippy::too_many_arguments)]
    /// Create a new session. Sessions are lightweight containers that can be
    /// created regardless of lock state.
    fn handle_create_session(
        &mut self,
        title: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<PathBuf>,
        reasoning_effort: Option<String>,
        selected_model: Option<String>,
        context_config: Option<ContextConfig>,
        account_name: Option<String>,
        active_tool_groups: Vec<String>,
        reply: std::sync::mpsc::Sender<io::Result<(u64, std::sync::mpsc::Sender<SessionCommand>)>>,
    ) {
        // A session is just a conversation container — it can be
        // created, browsed, and deleted regardless of whether the
        // daemon is locked.  Credentials are only needed when running
        // models (RunInput).
        let sid = self.next_session_id;
        self.next_session_id += 1;
        info!("CreateSession: id={}, title={:?}", sid, title);

        let cwd_str = working_dir.as_ref().map(|p| p.display().to_string());
        // The default active groups mirror the always-on groups; the
        // Coordination Platform group is included only when the `content`
        // feature is compiled in. Stale persisted names (e.g. `coord` from
        // before the group rename, or groups whose feature is off) are
        // silently ignored downstream: a group with no registered tools
        // contributes nothing to `available_definitions`, and load/unload
        // validation rejects unknown names on new requests only.
        // `mut` is only needed when the `content` feature pushes its group.
        #[cfg_attr(not(feature = "content"), allow(unused_mut))]
        let mut default_groups = vec!["core".to_string(), "git".to_string(), "shell".to_string()];
        #[cfg(feature = "content")]
        default_groups.push("content".to_string());
        let active_cats = if active_tool_groups.is_empty() {
            default_groups
        } else {
            active_tool_groups.clone()
        };

        // Resolve context window from the provider catalog at creation time
        // when both account and model are known — no provider instance needed.
        let context_window = account_name.as_ref().and_then(|name| {
            self.accounts.get(name).and_then(|config| {
                selected_model
                    .as_ref()
                    .and_then(|model| lookup_context_window(&config.provider, model))
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
            context_config: context_config.clone().unwrap_or_default(),
            account_name: account_name.clone(),
            last_response_id: None,
            last_response_id_producer: None,
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
        let created_msg = DaemonMessage::Session {
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
        let status_msg = DaemonMessage::Session {
            session_id: Some(sid),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                // Copy the creation timestamp before `record` is moved into
                // spawn_session above.
                last_modified: created_at,
            },
        };
        self.broadcast(created_msg);
        self.broadcast(status_msg);
    }

    /// Attach to an existing session by ID. Loads from the database if the
    /// session is not currently active.
    fn handle_attach_session(
        &mut self,
        session_id: u64,
        reply: std::sync::mpsc::Sender<io::Result<std::sync::mpsc::Sender<SessionCommand>>>,
    ) {
        debug!("AttachSession: id={}", session_id);
        // Attaching to a session is allowed regardless of lock state.
        // Credentials are only needed to run models (RunInput), not
        // to browse or attach to existing sessions.
        //
        // A deleted session's still-shutting-down thread can leave the DB
        // record in place until `handle_session_exited` finalizes the delete
        // (and drops the deleted marker).  Without this guard, an attach in
        // that window would resurrect a session the user deleted — the
        // record would be gone moments later, stranding the new session.
        if self.deleted_sessions.contains(&session_id) {
            debug!(
                session_id,
                "AttachSession: session is deleted, refusing attach"
            );
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session not found",
            )));
            return;
        }
        match self
            .active_sessions
            .get(&session_id)
            .map(|entry| entry.cmd_tx.clone())
        {
            Some(cmd_tx) => {
                // Attach to an already-active session also warms its model
                // list in the background — the session may have been joined on
                // a different client (or before this account's cache went
                // stale), and the in-flight guard keeps this idempotent.
                // The sender is cloned out first (above) so no borrow of
                // `self` is live across the mutable spawn call.
                let account = self
                    .session_metadata
                    .get(&session_id)
                    .and_then(|m| m.account_name.clone());
                if let Some(name) = account {
                    self.maybe_spawn_model_prefetch(&name);
                }
                let _ = reply.send(Ok(cmd_tx));
            }
            None => match db::read_session(&self.db, session_id) {
                Ok(Some(record)) => {
                    let mut metadata: SessionMetadata = record.clone().into();
                    metadata.status = SessionStatus::Inactive;
                    let session_tx = self.spawn_session(session_id, record, metadata);
                    info!("AttachSession: loaded session {} from db", session_id);
                    // Warm the model list for the reattached session's
                    // account in the background (no-op when fresh).
                    if let Some(name) = self
                        .session_metadata
                        .get(&session_id)
                        .and_then(|m| m.account_name.clone())
                    {
                        self.maybe_spawn_model_prefetch(&name);
                    }
                    let _ = reply.send(Ok(session_tx));
                }
                Ok(None) => {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "session not found",
                    )));
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            },
        }
    }

    /// Return a list of all active session summaries, most recently
    /// modified first.
    fn handle_list_sessions(&mut self, reply: std::sync::mpsc::Sender<Vec<SessionSummary>>) {
        let mut summaries: Vec<SessionSummary> = self
            .session_metadata
            .iter()
            .map(|(id, meta)| meta.to_summary(*id))
            .collect();

        // Newest first; the session_id tiebreak keeps equal timestamps
        // deterministic (no ordering jitter between refreshes).
        summaries.sort_by(|a, b| {
            b.last_modified
                .cmp(&a.last_modified)
                .then_with(|| b.session_id.cmp(&a.session_id))
        });
        let _ = reply.send(summaries);
    }

    /// Get a single session summary by ID.
    fn handle_get_session(
        &mut self,
        session_id: u64,
        reply: std::sync::mpsc::Sender<Option<SessionSummary>>,
    ) {
        let summary = self
            .session_metadata
            .get(&session_id)
            .map(|meta| meta.to_summary(session_id));
        let _ = reply.send(summary);
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
            let msg = DaemonMessage::Session {
                session_id: Some(session_id),
                event: SessionEvent::SessionStatusChanged {
                    status: SessionStatus::Sleeping,
                    last_modified,
                },
            };
            self.broadcast(msg);
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
            std::thread::spawn(move || finalize_session_delete(db, session_id, daemon_tx));
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

    /// Attempt to unlock the daemon with the given private key.
    fn handle_unlock(
        &mut self,
        private_key: Vec<u8>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        info!("Unlock attempt");
        // Capture the pre-unlock lock state so the transition broadcast below
        // fires only on a REAL locked→unlocked change (a re-unlock of an
        // already-unlocked daemon is a no-op for the banner, not a spammy
        // repeat). `handle_unlock_inner` -> `unlock_tail` clears `locked` on
        // success.
        let was_locked = self.locked;
        let result = handle_unlock_inner(self, private_key).map_err(|e| e.to_string());
        // A successful unlock is a lock-state transition: fan it out to ALL
        // activity subscribers (not just the acting client, which also gets its
        // `send_to_writer` `Unlocked` reply) so every connected UI clears its
        // lock banner — e.g. client B unlocking updates client A's status bar.
        if result.is_ok() && was_locked {
            self.broadcast_lock_state();
        }
        info!("Unlock result: success={}", result.is_ok());
        let _ = reply.send(result);
    }

    /// Lock the daemon's keystore (`/lock`): clear every decrypted in-memory
    /// credential and its cached provider, flip `locked` back to `true`, and
    /// broadcast the `Locked` state to all activity subscribers.
    ///
    /// This is intentionally soft/cooperative: sessions are untouched (they
    /// stay browsable — only inference requires credentials), so locking just
    /// drops cleartext from memory and re-latches the banner. The encrypted
    /// blobs remain in the DB and re-decrypt on the next Unlock.
    fn handle_lock(&mut self, reply: mpsc::Sender<Result<(), String>>) {
        let was_locked = self.locked;
        // Wipe decrypted credentials (and their derived providers) from memory
        // now that the keystore is locked. `credentials` holds the plaintext
        // ServiceCredentials; dropping them is what "locked" means for this
        // daemon.
        self.credentials.clear();
        self.providers.clear();
        self.x_credentials = None;
        self.locked = true;
        info!(
            credentials_cleared = 0,
            "keystore locked: in-memory credentials cleared"
        );
        // The acting client gets its `send_to_writer` `Locked` reply from the
        // connection layer; this transition broadcast reaches every connected
        // client (the acting one included, harmlessly idempotent).
        if !was_locked {
            self.broadcast_lock_state();
        }
        let _ = reply.send(Ok(()));
    }

    /// Save an encrypted credential blob for a service.
    ///
    /// `unlock_key` is REQUIRED (per-daemon keystore TOFU design): the flow
    /// is adopt-or-verify the key against the keystore binding, TEST-DECRYPT
    /// the incoming blob with the key (a blob that does not decrypt is
    /// rejected and never persisted — this enforces "the credential was
    /// encrypted with the same key as the rest of the keystore"), persist,
    /// then run the IMPLICIT UNLOCK (shared tail) — so a valid AddCredential
    /// to a locked daemon unlocks it. The caller (connection layer) replies
    /// `CredentialAdded` and emits `Unlocked` exactly like a successful
    /// `Unlock`.
    fn handle_save_credential(
        &mut self,
        service: String,
        encrypted_blob: Vec<u8>,
        mut unlock_key: Vec<u8>,
        reply: mpsc::Sender<Result<(), String>>,
    ) {
        // Capture the pre-operations lock state so the implicit-unlock
        // transition broadcast below fires only on a REAL locked→unlocked
        // change (`unlock_tail` clears `locked` on a successful save tail).
        let was_locked = self.locked;
        // Reject anything that is not exactly 32 bytes up front: the X25519
        // key derivation (and every crypto helper below) needs [u8; 32], and
        // a shorter/longer key can never be a valid unlock key.
        let mut key: [u8; 32] = match unlock_key.as_slice().try_into() {
            Ok(k) => k,
            Err(_) => {
                // The rejected bytes are still secret material — wipe them so
                // a failed add does not leave the key in a freed allocation.
                unlock_key.zeroize();
                let _ = reply.send(Err(
                    "invalid unlock_key: expected exactly 32 bytes".to_string()
                ));
                return;
            }
        };
        // Wipe the heap `Vec` copy; only the stack `key` array is used below
        // and it is zeroized on every exit path.
        unlock_key.zeroize();

        // Adopt-or-verify BEFORE anything is written: a wrong key must not
        // persist a blob the daemon could later read with an attacker's key
        // silently adopted. Mirrors handle_unlock_inner (shared helper — the
        // two paths cannot drift).
        if let Err(e) = adopt_or_verify_keystore_binding(self, &key) {
            key.zeroize();
            let _ = reply.send(Err(e.to_string()));
            return;
        }

        // TEST-DECRYPT the incoming blob. A blob that fails to decrypt (or
        // whose plaintext does not decode as a ServiceCredential) is REJECTED
        // and never persisted: storing an unreadable blob would poison the
        // keystore — the next unlock's bulk decrypt would log a failure
        // forever, and the credential would look saved but be unusable.
        let plaintext =
            match choreo_keystore::crypto::decrypt_with_private_key(&key, &encrypted_blob) {
                Ok(pt) => pt,
                Err(e) => {
                    key.zeroize();
                    warn!(
                        service = %service,
                        error = %e,
                        "AddCredential: blob failed test-decrypt with the presented unlock key; \
                         rejecting without persisting"
                    );
                    let _ = reply.send(Err(format!(
                        "credential blob failed to decrypt with the provided unlock key: {e}"
                    )));
                    return;
                }
            };
        let cred: ServiceCredential = match postcard::from_bytes(&plaintext) {
            Ok(c) => c,
            Err(e) => {
                key.zeroize();
                let _ = reply.send(Err(format!(
                    "credential payload is not a valid ServiceCredential: {e}"
                )));
                return;
            }
        };

        // Persist to DB only after both checks passed.
        if let Err(e) = db::set_credential_blob(&self.db, &service, &encrypted_blob) {
            key.zeroize();
            let _ = reply.send(Err(format!("failed to save credential: {e}")));
            return;
        }

        // Update in-memory state (same bookkeeping the old optional-key path
        // did), then run the shared implicit-unlock tail: bulk-decrypt ALL
        // blobs, load accounts, resolve providers. The tail re-decrypts the
        // blob just persisted — slightly redundant but keeps one code path
        // for "daemon is unlocked with this key" semantics.
        if matches!(&cred, ServiceCredential::X { .. }) && service == "twitter" {
            self.x_credentials = Some(cred.clone());
        }
        if let ServiceCredential::ApiKey { key: api_key } = &cred {
            // Resolve immediately too (in-memory only): so an AddCredential
            // for a known account works before the tail re-resolves.
            self.resolve_account_provider(&service, Some(api_key.clone()));
        }
        let result = unlock_tail(self, &key);
        key.zeroize();
        if let Err(e) = result {
            // The blob IS persisted and the binding holds — only the tail
            // (accounts load / bulk decrypt) failed. Report it rather than
            // lying with a silent success: the caller surfaces the error to
            // the client even though the credential was stored.
            error!(
                service = %service,
                error = %e,
                "AddCredential: persisted credential but implicit unlock failed"
            );
            let _ = reply.send(Err(format!("credential saved but unlock failed: {e}")));
            return;
        }
        info!(
            service = %service,
            "AddCredential: persisted, tested, and implicitly unlocked the keystore"
        );
        // A valid AddCredential to a locked daemon IS a lock-state transition
        // (implicit unlock): fan out the newly-unlocked state to ALL activity
        // subscribers so every connected UI clears its lock banner.
        if was_locked && !self.locked {
            self.broadcast_lock_state();
        }
        let _ = reply.send(Ok(()));
    }

    /// Remove a stored credential for a service.
    fn handle_remove_credential(
        &mut self,
        service: String,
        reply: mpsc::Sender<Result<(), String>>,
    ) {
        // Remove from DB
        if let Err(e) = db::remove_credential_blob(&self.db, &service) {
            let _ = reply.send(Err(format!("failed to remove credential: {e}")));
            return;
        }
        // Remove from in-memory state
        self.credentials.remove(&service);
        self.providers.remove(&service);
        if service == "twitter" {
            self.x_credentials = None;
        }
        let _ = reply.send(Ok(()));
    }

    /// List available models, optionally scoped to a session's account.
    fn handle_list_models(&mut self, session_id: Option<u64>, reply: ListModelsReply) {
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
        reply: mpsc::Sender<Result<RefreshReport, String>>,
    ) {
        match &self.maintenance_tx {
            Some(tx) => {
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
                    let _ =
                        reply.send(Err("catalog maintenance thread is not running".to_string()));
                }
            }
            None => {
                warn!(
                    "RefreshModels: no maintenance thread (unit-test state); replying with an error"
                );
                let _ = reply.send(Err("catalog maintenance thread is not running".to_string()));
            }
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
        base: Vec<choreo_ai_protocols::ProviderEntry>,
        etag: Option<String>,
        user_overlay: Option<String>,
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
        let effective = merge_catalog_layers(&base, user_overlay.as_deref());

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
        self.persist_catalog_cache(&base, etag.as_deref(), persist);

        // Broadcast the new provider list to all activity subscribers so the
        // TUI's provider picker tracks the live catalog.
        let providers = catalog_provider_pairs();
        self.handle_broadcast_activity(None, DaemonMessage::CatalogUpdated { providers });

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
    fn handle_catalog_not_modified(&mut self, reply: Vec<RefreshRequester>) {
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
        model: String,
        reply: mpsc::Sender<Result<(), String>>,
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

        // No provider for this account → daemon is locked or the credential
        // hasn't been saved.  Reject so the user knows they must unlock first
        // (or configure a credential) rather than silently accepting an
        // unvalidated model.
        if !self.providers.contains_key(&account_name) {
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
                if cached_models.contains(&model) {
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
        service: String,
        reply: std::sync::mpsc::Sender<Option<String>>,
    ) {
        let key = self.credentials.get(&service).and_then(|c| match c {
            ServiceCredential::ApiKey { key } => Some(key.clone()),
            _ => None,
        });
        let _ = reply.send(key);
    }

    /// Handle a cancel request from a client.  Sends `SessionCommand::Cancel`
    /// to the target session and then propagates cancellation to any child
    /// sub-sessions directly — avoiding a round-trip message from the session
    /// thread back to the daemon.
    fn handle_cancel_request(&mut self, session_id: u64, request_id: u32) {
        debug!("CancelRequest: session={session_id} request={request_id}");

        // Forward the cancel to the session thread.
        if let Some(entry) = self.active_sessions.get(&session_id) {
            let _ = entry.cmd_tx.send(SessionCommand::Cancel { request_id });
        }

        // Propagate to children — this runs here in the daemon so that
        // leaf sessions never generate an unnecessary message.
        self.cancel_children_of(session_id);
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
                        request_id: CANCEL_ALL,
                    })
                    .is_err()
                {
                    warn!("cancel_children_of: failed to send Cancel to child {child_id}");
                }
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
                request_id: CANCEL_ALL,
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
                let _ = entry.cmd_tx.send(SessionCommand::SetTitle { title });
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
        match self.active_sessions.get(&session_id) {
            Some(entry) => {
                let _ = entry
                    .cmd_tx
                    .send(SessionCommand::SetWorkingDir { path, reply });
            }
            None => {
                warn!(session_id, "cannot set working dir: session is not active");
                // Reply immediately so the caller (a blocked tool execution)
                // doesn't hang waiting on a session that doesn't exist.
                let _ = reply.send(Err("session is not active".into()));
            }
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
        match self.active_sessions.get(&session_id) {
            Some(entry) => {
                let _ = entry
                    .cmd_tx
                    .send(SessionCommand::LoadTools { groups, reply });
            }
            None => {
                warn!(session_id, "cannot load tools: session is not active");
                // Reply immediately so the caller (a blocked tool execution)
                // doesn't hang waiting on a session that doesn't exist.
                let _ = reply.send(Err("session is not active".into()));
            }
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
        match self.active_sessions.get(&session_id) {
            Some(entry) => {
                let _ = entry
                    .cmd_tx
                    .send(SessionCommand::UnloadTools { groups, reply });
            }
            None => {
                warn!(session_id, "cannot unload tools: session is not active");
                // Reply immediately so the caller (a blocked tool execution)
                // doesn't hang waiting on a session that doesn't exist.
                let _ = reply.send(Err("session is not active".into()));
            }
        }
    }

    /// Delete a session, shutting down its thread and removing it from the DB.
    /// If the session has children, they are cascade-deleted first.
    ///
    /// Sessions are just conversation containers — they can be deleted
    /// regardless of whether the daemon is locked, just like they can
    /// be created and browsed freely.  Credentials are only needed to
    /// run models (RunInput).
    fn handle_delete_session(
        &mut self,
        session_id: u64,
        reply: std::sync::mpsc::Sender<io::Result<()>>,
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
        self.broadcast(DaemonMessage::Session {
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
                    request_id: CANCEL_ALL,
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
        self.broadcast(DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleted,
        });
        Ok(())
    }

    /// Add a new inference account.
    #[expect(clippy::too_many_arguments)]
    fn handle_add_account(
        &mut self,
        name: String,
        provider: String,
        base_url: Option<String>,
        streaming: Option<bool>,
        retry_max_attempts: Option<u32>,
        connect_timeout_secs: Option<u64>,
        request_timeout_secs: Option<u64>,
        total_timeout_secs: Option<u64>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let config = AccountConfig {
            base_url,
            streaming,
            retry_max_attempts,
            connect_timeout_secs,
            request_timeout_secs,
            total_timeout_secs,
            ..AccountConfig::simple(&name, &provider)
        };
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
        // If account was added and there's a matching credential,
        // resolve the provider immediately (in-memory only — the model
        // list warms in the background on session join).
        if result.is_ok()
            && let Some(ServiceCredential::ApiKey { key }) = self.credentials.get(&name)
        {
            self.resolve_account_provider(&name, Some(key.clone()));
        }
        let _ = reply.send(result);
    }

    /// Remove an inference account.
    fn handle_remove_account(
        &mut self,
        name: String,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let result = self.accounts.remove(&name);
        match &result {
            Ok(()) => info!(account = %name, "removed inference account"),
            Err(e) => {
                error!(account = %name, error = %e, "failed to remove inference account")
            }
        }
        if result.is_ok() {
            self.providers.remove(&name);
        }
        let _ = reply.send(result);
    }

    /// List all inference accounts (with credential status).
    fn handle_list_accounts(
        &mut self,
        reply: std::sync::mpsc::Sender<Result<Vec<AccountInfo>, String>>,
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
    /// lock-discipline write used by the CLI too), hot-reload the SharedAcl
    /// (this loop is its single writer), broadcast `AclUpdated` so connected
    /// clients see the new trust total, and reply with the count.
    ///
    /// Re-authorizing an ALREADY-present key is a success reply with no
    /// write — idempotent for a client that retries a slow request.
    fn handle_acl_add(&mut self, pubkey: String, reply: mpsc::Sender<Result<usize, String>>) {
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
                DaemonMessage::AclUpdated {
                    clients: *count as u64,
                },
            );
        }
        let _ = reply.send(result);
    }

    /// Handle an `authorized_clients.toml` watcher event: hand the reload to
    /// the `SharedAcl` (the command loop is its single writer — re-read,
    /// parse-compare, atomic swap all live inside `reload`). A unit-test
    /// DaemonState has no ACL (`None`) and the event is a logged no-op.
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

        // Accounts that vanished: drop the cached provider (dead weight) and
        // leave credentials intact (a credential with no account is inert).
        for name in &removed {
            if self.providers.remove(name).is_some() {
                warn!(
                    account = name,
                    "account removed from accounts.toml externally; dropped its cached provider",
                );
            }
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
        // provider protocol or an override changed): drop the stale cached
        // provider and rebuild it against the NEW config + still-held
        // credential. This mirrors the unlock-time bulk resolve and the
        // /add-key path; without it the next session would capture a provider
        // built from the old file forever. A failed resolve just leaves the
        // account uncached; the next explicit resolve retries it.
        for name in &changed {
            if self.providers.remove(name).is_some() {
                warn!(
                    account = name,
                    "account config changed externally; rebuilding its provider",
                );
            }
            let api_key = self.credentials.get(name).and_then(|c| match c {
                ServiceCredential::ApiKey { key } => Some(key.clone()),
                _ => None,
            });
            if self.resolve_account_provider(name, api_key) {
                info!(
                    account = name,
                    "provider rebuilt for externally-modified account",
                );
            }
        }
        // Push the fresh list to activity subscribers (global/control
        // provenance — a flat, non-session message — so no origin-contract
        // dedup runs). Clients can refresh their account pickers live.
        let accounts = self.account_infos();
        self.handle_broadcast_activity(None, DaemonMessage::Accounts { accounts });
    }

    /// Resolve a cached provider for the given account name.
    fn handle_resolve_provider(
        &mut self,
        account: String,
        reply: std::sync::mpsc::Sender<Option<InferenceProvider>>,
    ) {
        let _ = reply.send(self.providers.get(&account).cloned());
    }

    /// Check whether an account with the given name exists.
    fn handle_account_exists(&mut self, name: String, reply: std::sync::mpsc::Sender<bool>) {
        let _ = reply.send(self.accounts.contains(&name));
    }
}

fn handle_unlock_inner(state: &mut DaemonState, mut private_key: Vec<u8>) -> io::Result<()> {
    let mut key: [u8; 32] = match private_key.as_slice().try_into() {
        Ok(k) => k,
        Err(_) => {
            // The presented bytes are unusable, but still secret material —
            // wipe the heap copy before returning so a failed unlock does not
            // leave the key lying in a freed allocation.
            private_key.zeroize();
            return Err(io::Error::other("invalid private key: expected 32 bytes"));
        }
    };
    // Wipe the heap `Vec` copy as soon as the stack array exists; only `key`
    // is used below and it is zeroized on every exit path.
    private_key.zeroize();

    // TOFU adopt-or-verify against the persisted keystore binding. A locked
    // daemon presents its key on every Unlock; once a key is bound, any key
    // whose derived public key differs is REJECTED (surfaces as LockedError)
    // instead of unlocking with wrong credentials.
    if let Err(e) = adopt_or_verify_keystore_binding(state, &key) {
        // Guard the early return so `key` is wiped even when the binding
        // rejects this key.
        key.zeroize();
        return Err(e);
    }

    // Shared bulk-decrypt + accounts-load + provider-resolve tail — the same
    // code path `AddCredential` runs as its implicit unlock, so the two
    // paths cannot drift.
    let result = unlock_tail(state, &key);
    key.zeroize();
    result
}

/// TOFU keystore-binding enforcement, shared by `Unlock` and
/// `AddCredential` (factored into one helper so the two paths cannot drift).
///
/// * Unbound keystore (no stored binding): ADOPT the presented key — derive
///   its X25519 public key and persist it. This is a one-time,
///   security-relevant event, so the log is deliberately LOUD.
/// * Bound keystore: derive the presented key's public key and compare
///   against the binding; a mismatch is an error (LockedError for Unlock,
///   CredentialAddFailed for AddCredential).
pub(crate) fn adopt_or_verify_keystore_binding(
    state: &DaemonState,
    key: &[u8; 32],
) -> io::Result<()> {
    let binding = db::get_keystore_binding(&state.db)
        .map_err(|e| io::Error::other(format!("failed to read keystore binding: {e}")))?;
    // x25519_dalek: the public key is what the binding stores (and what the
    // CLIENT used to encrypt credential blobs), never the private key itself.
    let derived = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*key));
    match binding {
        None => {
            db::set_keystore_binding(&state.db, derived.as_bytes()).map_err(|e| {
                io::Error::other(format!("failed to persist keystore binding: {e}"))
            })?;
            // One-time, security-relevant event: LOUD on purpose. Operators
            // must be able to see when a daemon's keystore became bound to a
            // key (any later unlock requires exactly that key).
            info!(
                "KEYSTORE BOUND: adopted unlock key on first contact (TOFU); \
                 public key (hex) = {} — all future Unlock/AddCredential \
                 attempts must present this key, others are rejected",
                hex::encode(derived.as_bytes())
            );
            Ok(())
        }
        Some(stored) if stored == *derived.as_bytes() => Ok(()),
        Some(_) => Err(io::Error::other(
            "unlock key does not match the daemon's keystore binding",
        )),
    }
}

/// The unlock TAIL shared by `handle_unlock_inner` and the implicit unlock
/// in `handle_save_credential`: bulk-decrypt every stored credential blob
/// with `key` into `state.credentials`, load accounts from TOML, and resolve
/// providers (in-memory, no I/O beyond the account file). Factored out so
/// the Unlock path and the AddCredential-implicit-unlock path cannot drift.
pub(crate) fn unlock_tail(state: &mut DaemonState, key: &[u8; 32]) -> io::Result<()> {
    let blobs = db::get_all_credential_blobs(&state.db)
        .map_err(|e| io::Error::other(format!("failed to read credentials from database: {e}")))?;

    info!("Unlock: {} credential blobs in DB", blobs.len());

    let mut credentials = HashMap::new();
    let mut decrypt_failures = 0usize;
    for (service, blob) in &blobs {
        match choreo_keystore::crypto::decrypt_with_private_key(key, blob) {
            Ok(plaintext) => match postcard::from_bytes::<ServiceCredential>(&plaintext) {
                Ok(cred) => {
                    credentials.insert(service.clone(), cred);
                }
                Err(e) => {
                    warn!("Unlock: failed to decode credential '{}': {e}", service);
                    decrypt_failures += 1;
                }
            },
            Err(e) => {
                warn!("Unlock: failed to decrypt credential '{}': {e}", service);
                decrypt_failures += 1;
            }
        }
    }
    // The full decrypt summary (counts, failures, service names) is logged
    // once, below, AFTER `state.credentials` is assigned — so the log always
    // reports what this unlock actually decrypted, not a pre-assignment map.

    // Set up X credentials
    if let Some(c) = credentials.get("twitter")
        && matches!(c, ServiceCredential::X { .. })
    {
        state.x_credentials = Some(c.clone());
    }

    state.credentials = credentials;

    // Log AFTER the assignment: this used to run before `state.credentials`
    // was updated, so it always reported the pre-unlock (stale or empty) map
    // instead of what this unlock actually decrypted.
    info!(
        "Unlock: decrypted {}/{} credentials ({} failures): {:?}",
        state.credentials.len(),
        blobs.len(),
        decrypt_failures,
        state.credentials.keys().collect::<Vec<_>>()
    );

    // Load accounts from TOML
    let accounts_path = accounts_config_path()
        .map_err(|e| io::Error::other(format!("failed to get accounts config path: {e}")))?;
    state.accounts = AccountManager::load(&accounts_path)
        .map_err(|e| io::Error::other(format!("failed to load accounts: {e}")))?;

    // If no accounts configured but an "openai" credential exists, create a
    // default account automatically so the user doesn't have to set one up.
    if state.accounts.is_empty() && state.credentials.contains_key("openai") {
        let default_config = AccountConfig::simple("default", "openai");
        if let Err(e) = state.accounts.add(default_config) {
            tracing::warn!("failed to create default account: {e}");
        }
    }

    let account_names: Vec<String> = state
        .accounts
        .all_configs()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    info!("Unlock: accounts loaded: {:?}", account_names);

    // Resolve providers for all accounts
    state.providers.clear();
    for config in state.accounts.all_configs() {
        let api_key = state.credentials.get(&config.name).and_then(|c| match c {
            ServiceCredential::ApiKey { key } => Some(key.clone()),
            _ => None,
        });
        info!(
            "Unlock: account '{}': api_key_found={}, has_credential={}",
            config.name,
            api_key.is_some(),
            state.credentials.contains_key(&config.name)
        );
        if state.resolve_account_provider(&config.name, api_key) {
            info!("Unlock: provider resolved for account '{}'", config.name);
        } else {
            info!(
                "Unlock: failed to resolve provider for account '{}'",
                config.name
            );
        }
    }
    info!(
        "Unlock: providers resolved: {:?}",
        state.providers.keys().collect::<Vec<_>>()
    );

    // The keystore is now fully decrypted into memory (credentials, accounts,
    // providers): this is the single authoritative unlocked point shared by
    // the `Unlock` path and the `AddCredential` implicit-unlock path. The
    // caller methods broadcast `Unlocked` on the locked→unlocked transition.
    state.locked = false;

    Ok(())
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

fn handle_list_models_inner(
    state: &mut DaemonState,
    session_id: Option<u64>,
) -> Result<(Vec<String>, Option<String>), String> {
    let account_name = session_id
        .and_then(|sid| state.session_metadata.get(&sid))
        .and_then(|m| m.account_name.clone())
        .unwrap_or_default();

    debug!(
        "ListModels: session_id={:?}, account_name='{}', providers_keys={:?}",
        session_id,
        account_name,
        state.providers.keys().collect::<Vec<_>>()
    );

    // Existence check only — no provider instance is needed below, because
    // the actual fetch (if any) runs on the detached background thread.
    if !state.providers.contains_key(&account_name) {
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
