use crate::accounts::{AccountConfig, AccountManager, accounts_config_path};
use crate::db::{self, SessionRecord};
use crate::mcp::McpManager;
use crate::providers::InferenceProvider;
use crate::sessions::{
    ActiveSessionEntry, CANCEL_ALL, RequestContext, SessionCommand, SessionMetadata, session_main,
};
use choreo_ai_protocols::lookup_context_window;
use choreo_keystore::ServiceCredential;
use choreo_proto::{
    AccountInfo, ContextConfig, DaemonMessage, SessionStatus, SessionSummary, TimestampMs,
    TokenUsage,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;
use tracing::{debug, error, info, warn};
use zeroize::Zeroize;

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
    pub db: Arc<redb::Database>,
    pub tool_registry: Arc<crate::tools::ToolRegistry>,
    pub daemon_tx: mpsc::Sender<DaemonCommand>,
    pub client_streams: Vec<UnixStream>,
    pub summary_subscribers: HashMap<u64, mpsc::SyncSender<DaemonMessage>>,
    pub activity_subscribers: HashMap<u64, mpsc::SyncSender<DaemonMessage>>,
    /// Tracks which clients are direct session subscribers of which sessions.
    /// Used by `handle_broadcast_activity` to skip duplicate delivery to
    /// clients that are both activity subscribers AND session subscribers
    /// — the message reaches them through the per-session subscriber path.
    pub client_subscribed_sessions: HashMap<u64, HashSet<u64>>,
    pub model_cache: HashMap<String, (Vec<String>, Instant)>,
    pub mcp_manager: McpManager,
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
    SaveCredential {
        service: String,
        encrypted_blob: Vec<u8>,
        unlock_key: Option<Vec<u8>>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    RemoveCredentialCmd {
        service: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    ListModels {
        session_id: Option<u64>,
        reply: ListModelsReply,
    },
    GetCredential {
        service: String,
        reply: std::sync::mpsc::Sender<Option<String>>,
    },
    RegisterSummarySubscriber {
        client_id: u64,
        writer: std::sync::mpsc::SyncSender<DaemonMessage>,
    },
    UnregisterSummarySubscriber {
        client_id: u64,
    },
    RegisterActivitySubscriber {
        client_id: u64,
        writer: std::sync::mpsc::SyncSender<DaemonMessage>,
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
    BroadcastActivity(DaemonMessage),
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
            DaemonCommand::SaveCredential {
                service,
                encrypted_blob,
                unlock_key,
                reply,
            } => self.handle_save_credential(service, encrypted_blob, unlock_key, reply),
            DaemonCommand::RemoveCredentialCmd { service, reply } => {
                self.handle_remove_credential(service, reply)
            }
            DaemonCommand::ListModels { session_id, reply } => {
                self.handle_list_models(session_id, reply)
            }
            DaemonCommand::GetCredential { service, reply } => {
                self.handle_get_credential(service, reply)
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
            DaemonCommand::BroadcastActivity(msg) => self.handle_broadcast_activity(msg),
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

    /// Try to resolve an `InferenceProvider` for the given account name using
    /// the stored credential.  Silently ignores missing credentials or config.
    /// Resolve the provider for `name` and pre-fetch the model list so
    /// SetModel doesn't wait on an HTTP round-trip through the event loop.
    /// Returns `true` if a provider was successfully created and cached.
    fn resolve_account_provider(&mut self, name: &str, api_key: Option<String>) -> bool {
        if let Some(config) = self.accounts.get(name)
            && let Ok(provider) = InferenceProvider::from_account_config(config, api_key)
        {
            fetch_and_cache_models(self, name, &provider);
            self.providers.insert(name.to_string(), provider);
            true
        } else {
            false
        }
    }

    /// Send a message to all summary subscribers, removing dead ones.
    fn broadcast(&mut self, msg: DaemonMessage) {
        self.summary_subscribers
            .retain(|_id, tx| tx.send(msg.clone()).is_ok());
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
        let active_cats = if active_tool_groups.is_empty() {
            vec!["core".into(), "git".into(), "shell".into()]
        } else {
            active_tool_groups.clone()
        };

        // Resolve context window from the static catalog at creation time
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

        // Track parent→child relationship so cancellation/deletion
        // of the parent propagates to sub-sessions.
        if let Some(parent_id) = parent_session_id {
            self.children.entry(parent_id).or_default().push(sid);
        }

        let _ = reply.send(Ok((sid, session_tx)));
        crate::metrics::record_session_created();
        let created_msg = DaemonMessage::SessionCreated {
            session_id: sid,
            title,
            parent_session_id,
            working_dir: cwd_str,
            account_name,
            selected_model: selected_model_clone,
            reasoning_effort: reasoning_effort_clone,
        };
        let status_msg = DaemonMessage::SessionStatusChanged {
            session_id: sid,
            status: SessionStatus::Inactive,
            // Copy the creation timestamp before `record` is moved into
            // spawn_session above.
            last_modified: created_at,
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
        match self.active_sessions.get(&session_id) {
            Some(entry) => {
                let _ = reply.send(Ok(entry.cmd_tx.clone()));
            }
            None => match db::read_session(&self.db, session_id) {
                Ok(Some(record)) => {
                    let mut metadata: SessionMetadata = record.clone().into();
                    metadata.status = SessionStatus::Inactive;
                    let session_tx = self.spawn_session(session_id, record, metadata);
                    info!("AttachSession: loaded session {} from db", session_id);
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
        self.session_metadata.insert(session_id, metadata);
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

        // Capture a single timestamp for both the index bump and the
        // broadcast message so the two can never disagree by a millisecond
        // (the max() guard on the index makes a skew harmless, but a single
        // read is simpler to reason about).
        let last_modified = TimestampMs::now().as_millis();
        if let Some(meta) = self.session_metadata.get_mut(&session_id) {
            meta.status = SessionStatus::Sleeping;
            meta.last_modified = meta.last_modified.max(last_modified);
        }
        // Only broadcast for sessions that still exist: a deleted session's
        // shutting-down thread must not emit a ghost "sleeping" status for a
        // session the user removed.
        if self.session_metadata.contains_key(&session_id) {
            let msg = DaemonMessage::SessionStatusChanged {
                session_id,
                status: SessionStatus::Sleeping,
                last_modified,
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
        let result = handle_unlock_inner(self, private_key).map_err(|e| e.to_string());
        info!("Unlock result: success={}", result.is_ok());
        let _ = reply.send(result);
    }

    /// Save an encrypted credential blob for a service.
    fn handle_save_credential(
        &mut self,
        service: String,
        encrypted_blob: Vec<u8>,
        unlock_key: Option<Vec<u8>>,
        reply: mpsc::Sender<Result<(), String>>,
    ) {
        // Write to DB
        if let Err(e) = db::set_credential_blob(&self.db, &service, &encrypted_blob) {
            let _ = reply.send(Err(format!("failed to save credential: {e}")));
            return;
        }
        // Optionally decrypt into memory
        if let Some(mut uk) = unlock_key {
            let key: [u8; 32] = match uk.as_slice().try_into() {
                Ok(k) => k,
                Err(_) => {
                    uk.zeroize();
                    let _ = reply.send(Ok(()));
                    return;
                }
            };
            uk.zeroize();
            if let Ok(plaintext) =
                choreo_keystore::crypto::decrypt_with_private_key(&key, &encrypted_blob)
                && let Ok(cred) = postcard::from_bytes::<ServiceCredential>(&plaintext)
            {
                // Update in-memory state
                if matches!(&cred, ServiceCredential::X { .. }) && service == "twitter" {
                    self.x_credentials = Some(cred.clone());
                }
                self.credentials.insert(service.clone(), cred.clone());
                // Resolve provider (and pre-fetch models) for any account
                // matching this service name.
                if let ServiceCredential::ApiKey { key: api_key } = &cred {
                    self.resolve_account_provider(&service, Some(api_key.clone()));
                }
            }
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

    /// Validate that a model exists in the provider's model list for this
    /// session's account.  The model list is pre-populated by
    /// `fetch_and_cache_models` at provider-resolution time (unlock, credential
    /// save, account add).  If no cached data exists (fetch failed or provider
    /// was just resolved without a successful fetch) the model is allowed
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

    /// Register a client to receive session summary broadcasts.
    fn handle_register_summary_subscriber(
        &mut self,
        client_id: u64,
        writer: std::sync::mpsc::SyncSender<DaemonMessage>,
    ) {
        self.summary_subscribers.insert(client_id, writer);
    }

    /// Unregister a client from session summary broadcasts.
    fn handle_unregister_summary_subscriber(&mut self, client_id: u64) {
        self.summary_subscribers.remove(&client_id);
    }

    /// Broadcast a session status change to all summary subscribers and keep
    /// the metadata index in sync.
    ///
    /// This is the choke point that fixes stale statuses on the sessions page:
    /// the session thread broadcasts status changes (see `handle_status_changed`
    /// in sessions.rs) but never updates the daemon's `session_metadata` index,
    /// so a subsequent ListSessions would serve an outdated status.  Updating
    /// the index here — and bumping `last_modified` so the list reorders —
    /// covers every status-transition path.
    fn handle_broadcast_session_status(&mut self, session_id: u64, status: SessionStatus) {
        let last_modified = TimestampMs::now().as_millis();
        if let Some(meta) = self.session_metadata.get_mut(&session_id) {
            meta.status = status.clone();
            meta.last_modified = meta.last_modified.max(last_modified);
        }
        let msg = DaemonMessage::SessionStatusChanged {
            session_id,
            status,
            last_modified,
        };
        // A deleted session's still-shutting-down thread must not emit ghost
        // status events for a session the user removed; the index is empty
        // for deleted sessions, so use its presence as the "session exists"
        // signal.
        if self.session_metadata.contains_key(&session_id) {
            self.broadcast(msg);
        }
    }

    /// Register a client to receive all session activity broadcasts.
    fn handle_register_activity_subscriber(
        &mut self,
        client_id: u64,
        writer: std::sync::mpsc::SyncSender<DaemonMessage>,
    ) {
        info!("registering activity subscriber: client_id={}", client_id);
        self.activity_subscribers.insert(client_id, writer);
    }

    /// Unregister a client from all session activity broadcasts.
    ///
    /// Only removes from the activity subscriber map — does NOT clear
    /// `client_subscribed_sessions`.  Session subscription tracking is
    /// cleaned up by explicit `UntrackSessionSubscription` messages sent
    /// from session threads on client detach, and by `handle_client_disconnected`
    /// when the client fully disconnects.
    ///
    /// This preserves the invariant that a client that explicitly unsubscribes
    /// from all activity but remains attached to sessions can re-subscribe
    /// without causing duplicate delivery (the dedup filter in
    /// `handle_broadcast_activity` still knows about their session subscriptions).
    fn handle_unregister_activity_subscriber(&mut self, client_id: u64) {
        debug!("unregistering activity subscriber: client_id={}", client_id);
        self.activity_subscribers.remove(&client_id);
    }

    /// Clean up all per-client tracking when a client disconnects.
    /// Removes from summary subscribers, activity subscribers, and session
    /// subscription tracking in a single atomic operation so stale entries
    /// don't accumulate.
    fn handle_client_disconnected(&mut self, client_id: u64) {
        info!("client disconnected cleanup: client_id={}", client_id);
        self.summary_subscribers.remove(&client_id);
        self.activity_subscribers.remove(&client_id);
        self.client_subscribed_sessions.remove(&client_id);
    }

    /// Track that `client_id` is a direct subscriber of `session_id`.
    /// Idempotent — re-attach to the same session is a no-op.
    fn handle_track_session_subscription(&mut self, client_id: u64, session_id: u64) {
        debug!(
            "track session subscription: client_id={}, session_id={}",
            client_id, session_id
        );
        self.client_subscribed_sessions
            .entry(client_id)
            .or_default()
            .insert(session_id);
    }

    /// Untrack that `client_id` is no longer a direct subscriber of `session_id`.
    fn handle_untrack_session_subscription(&mut self, client_id: u64, session_id: u64) {
        debug!(
            "untrack session subscription: client_id={}, session_id={}",
            client_id, session_id
        );
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.client_subscribed_sessions.entry(client_id)
        {
            entry.get_mut().remove(&session_id);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    /// Broadcast a message to all activity subscribers, removing dead ones.
    ///
    /// Uses `try_send` so a slow subscriber does not block the daemon's
    /// single-threaded command loop (mirroring the behaviour in the per-session
    /// `broadcast()` function in sessions.rs).
    ///
    /// Skips delivery to clients that are also direct session subscribers of
    /// the originating session — those clients receive the message through
    /// the per-session subscriber path, avoiding duplicate delivery.
    fn handle_broadcast_activity(&mut self, msg: DaemonMessage) {
        let origin_session_id = msg.session_id();
        self.activity_subscribers.retain(|client_id, tx| {
            // Skip if this client is also a direct subscriber of the
            // session that originated this message — they'll receive it
            // through the per-session broadcast path.
            if let Some(ref sid) = origin_session_id
                && let Some(sessions) = self.client_subscribed_sessions.get(client_id)
                && sessions.contains(sid)
            {
                return true;
            }
            tx.try_send(msg.clone()).is_ok()
        });
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
        self.broadcast(DaemonMessage::SessionDeleted { session_id });
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
        self.broadcast(DaemonMessage::SessionDeleted { session_id });
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
        // resolve the provider immediately (which also pre-fetches models).
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
        // Collect the set of account names that have credentials
        // (either decrypted in memory or stored as encrypted blobs
        // in the DB) so the TUI can show whether each account has
        // had a credential supplied, regardless of unlock state.
        let mut credentialed: std::collections::HashSet<String> =
            self.credentials.keys().cloned().collect();
        // Also check the DB for stored-but-not-yet-decrypted blobs.
        if let Ok(blobs) = db::get_all_credential_blobs(&self.db) {
            credentialed.extend(blobs.into_keys());
        }
        let _ = reply.send(Ok(self.accounts.list(&credentialed)));
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

fn handle_unlock_inner(state: &mut DaemonState, private_key: Vec<u8>) -> io::Result<()> {
    let mut key: [u8; 32] = private_key
        .as_slice()
        .try_into()
        .map_err(|_| io::Error::other("invalid private key: expected 32 bytes"))?;
    drop(private_key);

    let blobs = db::get_all_credential_blobs(&state.db)
        .map_err(|e| io::Error::other(format!("failed to read credentials from database: {e}")))?;

    info!("Unlock: {} credential blobs in DB", blobs.len());

    let mut credentials = HashMap::new();
    let mut decrypt_failures = 0usize;
    for (service, blob) in &blobs {
        match choreo_keystore::crypto::decrypt_with_private_key(&key, blob) {
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
    info!(
        "Unlock: decrypted {}/{} credentials; {} failures",
        credentials.len(),
        blobs.len(),
        decrypt_failures
    );

    // Set up X credentials
    if let Some(c) = credentials.get("twitter")
        && matches!(c, ServiceCredential::X { .. })
    {
        state.x_credentials = Some(c.clone());
    }

    info!(
        "Unlock: decrypted {} credentials from DB: {:?}",
        state.credentials.len(),
        state.credentials.keys().collect::<Vec<_>>()
    );

    state.credentials = credentials;

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

    key.zeroize();
    Ok(())
}

/// Eagerly fetch the model list for `provider` and cache it under
/// `account_name`.  If the fetch fails (network, API error, etc.) we
/// simply log a warning — callers will fall through to their allow-on-error
/// path or attempt an on-demand fetch (see `handle_list_models_inner`).
fn fetch_and_cache_models(
    state: &mut DaemonState,
    account_name: &str,
    provider: &InferenceProvider,
) {
    match provider.list_models() {
        Ok(models) => {
            debug!(
                "cached {} models for account '{}'",
                models.len(),
                account_name
            );
            state
                .model_cache
                .insert(account_name.to_string(), (models, Instant::now()));
        }
        Err(e) => {
            warn!(
                "failed to fetch model list for account '{}': {e}",
                account_name
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
        "ListModels: session_id={:?}, account_name='{}', providers_keys={:?}",
        session_id,
        account_name,
        state.providers.keys().collect::<Vec<_>>()
    );

    let provider = state.providers.get(&account_name).ok_or_else(|| {
        if state.accounts.is_empty() {
            "no accounts configured".to_string()
        } else {
            format!("no credential stored for account '{account_name}'")
        }
    })?;

    let now = Instant::now();
    let five_minutes = std::time::Duration::from_secs(300);
    let models = match state.model_cache.get(&account_name) {
        Some((cached_models, cached_at)) if now.duration_since(*cached_at) < five_minutes => {
            cached_models.clone()
        }
        _ => {
            let models = provider
                .list_models()
                .map_err(|e| format!("failed to list models: {e}"))?;
            state
                .model_cache
                .insert(account_name, (models.clone(), now));
            models
        }
    };

    let selected_model = session_id
        .and_then(|sid| state.session_metadata.get(&sid))
        .and_then(|m| m.selected_model.clone());
    Ok((models, selected_model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_util::make_test_provider;
    use crate::server::connection::SUBSCRIBER_CHANNEL_CAPACITY;
    use crate::sessions::SessionMetadata;
    use choreo_proto::{DaemonMessage, SessionStatus, TimestampMs, Turn};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::mpsc;
    use std::time::Instant;

    fn make_daemon_state() -> (DaemonState, mpsc::Receiver<DaemonCommand>) {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let tool_registry = crate::tools::ToolRegistry::new().build();
        let config_dir = tempfile::tempdir().unwrap();
        let accounts_path = config_dir.path().join("accounts.toml");
        let state = DaemonState {
            next_session_id: 1,
            max_turns: 10,
            active_sessions: HashMap::new(),
            session_metadata: HashMap::new(),
            deleted_sessions: HashSet::new(),
            children: HashMap::new(),
            accounts: AccountManager::load(&accounts_path).unwrap(),
            providers: HashMap::new(),
            credentials: HashMap::new(),
            x_credentials: None,
            db,
            tool_registry,
            daemon_tx,
            client_streams: Vec::new(),
            summary_subscribers: HashMap::new(),
            activity_subscribers: HashMap::new(),
            client_subscribed_sessions: HashMap::new(),
            model_cache: HashMap::new(),
            mcp_manager: crate::mcp::McpManager::empty(),
        };
        (state, daemon_rx)
    }

    #[test]
    fn handle_list_sessions_empty() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ListSessions { reply });
        let sessions = rx.recv().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn handle_list_sessions_with_metadata() {
        let (mut state, _rx) = make_daemon_state();
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: Some("test".into()),
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 1000,
                turn_count: 3,
                status: SessionStatus::Inactive,
                active_tool_groups: vec!["core".into()],
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ListSessions { reply });
        let sessions: Vec<SessionSummary> = rx.recv().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, 1);
        assert_eq!(sessions[0].title.as_deref(), Some("test"));
    }

    #[test]
    fn handle_list_sessions_orders_by_last_modified_desc() {
        let (mut state, _rx) = make_daemon_state();
        // Insert three sessions with distinct modification times, deliberately
        // out of order in the map.
        for (id, created, modified) in [(1, 1000, 1000), (2, 2000, 9000), (3, 3000, 5000)] {
            state.session_metadata.insert(
                id,
                SessionMetadata {
                    title: Some(format!("s{id}")),
                    selected_model: None,
                    reasoning_effort: None,
                    parent_session_id: None,
                    working_dir: None,
                    created_at: created,
                    last_modified: modified,
                    turn_count: 0,
                    status: SessionStatus::Inactive,
                    active_tool_groups: vec![],
                    account_name: None,
                    accumulated_usage: TokenUsage::default(),
                    context_window: None,
                    last_prompt_tokens: None,
                },
            );
        }
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ListSessions { reply });
        let sessions: Vec<SessionSummary> = rx.recv().unwrap();
        let ids: Vec<u64> = sessions.iter().map(|s| s.session_id).collect();
        // Most recently modified first: 2 (9000), 3 (5000), 1 (1000).
        assert_eq!(ids, vec![2, 3, 1]);
    }

    #[test]
    fn handle_list_sessions_tiebreaks_by_session_id_desc() {
        let (mut state, _rx) = make_daemon_state();
        // Equal modification times must order deterministically by id desc.
        for id in [1u64, 2, 3] {
            state.session_metadata.insert(
                id,
                SessionMetadata {
                    title: None,
                    selected_model: None,
                    reasoning_effort: None,
                    parent_session_id: None,
                    working_dir: None,
                    created_at: id as i64 * 1000,
                    last_modified: 5000,
                    turn_count: 0,
                    status: SessionStatus::Inactive,
                    active_tool_groups: vec![],
                    account_name: None,
                    accumulated_usage: TokenUsage::default(),
                    context_window: None,
                    last_prompt_tokens: None,
                },
            );
        }
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ListSessions { reply });
        let sessions: Vec<SessionSummary> = rx.recv().unwrap();
        let ids: Vec<u64> = sessions.iter().map(|s| s.session_id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    #[test]
    fn handle_get_session_missing() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::GetSession {
            session_id: 1,
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn handle_update_metadata() {
        let (mut state, _rx) = make_daemon_state();
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: Some("original".into()),
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 1000,
                turn_count: 0,
                status: SessionStatus::Inactive,
                active_tool_groups: vec!["core".into()],
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
        let new_meta = SessionMetadata {
            title: Some("updated".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 2000,
            last_modified: 2000,
            turn_count: 5,
            status: SessionStatus::Inference,
            active_tool_groups: vec!["core".into(), "git".into()],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        };
        state.handle_command(DaemonCommand::UpdateMetadata {
            session_id: 1,
            metadata: new_meta.clone(),
        });
        let stored = state.session_metadata.get(&1).unwrap();
        assert_eq!(stored.title.as_deref(), Some("updated"));
        assert_eq!(stored.selected_model.as_deref(), Some("gpt-4"));
        assert_eq!(stored.turn_count, 5);
        assert_eq!(stored.status, SessionStatus::Inference);
    }

    #[test]
    fn handle_update_metadata_preserves_sleeping_status_after_exit() {
        let (mut state, _rx) = make_daemon_state();
        // A session that has exited: present in the metadata index with
        // Sleeping status, and no active session thread (the daemon removed
        // it from active_sessions in handle_session_exited).
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: Some("exited".into()),
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 5000,
                turn_count: 3,
                status: SessionStatus::Sleeping,
                active_tool_groups: vec![],
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
        // Straggler snapshot from the (now dead) session thread — e.g. a
        // RequestFinished handler that raced with exit — claims Inactive.
        let stale = SessionMetadata {
            title: Some("exited".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 5000,
            turn_count: 4,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        };
        state.handle_command(DaemonCommand::UpdateMetadata {
            session_id: 1,
            metadata: stale,
        });
        let stored = state.session_metadata.get(&1).unwrap();
        // The exit status must win over the stale snapshot…
        assert_eq!(
            stored.status,
            SessionStatus::Sleeping,
            "exited session must not regress to a stale status"
        );
        // …while non-status fields from the snapshot still apply.
        assert_eq!(stored.turn_count, 4);
    }

    #[test]
    fn handle_session_exited_nonexistent() {
        let (mut state, _rx) = make_daemon_state();
        state.handle_command(DaemonCommand::SessionExited { session_id: 999 });
        assert!(!state.session_metadata.contains_key(&999));
    }

    #[test]
    fn handle_get_credential_locked() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::GetCredential {
            service: "openai".into(),
            reply,
        });
        let key = rx.recv().unwrap();
        assert!(key.is_none());
    }

    #[test]
    fn handle_register_unregister_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, _rx_sub) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        assert!(!state.summary_subscribers.contains_key(&42));
        state.handle_command(DaemonCommand::RegisterSummarySubscriber {
            client_id: 42,
            writer: tx,
        });
        assert!(state.summary_subscribers.contains_key(&42));
        state.handle_command(DaemonCommand::UnregisterSummarySubscriber { client_id: 42 });
        assert!(!state.summary_subscribers.contains_key(&42));
    }

    #[test]
    fn handle_broadcast_session_status() {
        let (mut state, _rx) = make_daemon_state();
        // Seed the metadata index so the broadcast has something to update.
        state.session_metadata.insert(
            42,
            SessionMetadata {
                title: None,
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 1000,
                turn_count: 0,
                status: SessionStatus::Inactive,
                active_tool_groups: vec![],
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        state.handle_command(DaemonCommand::RegisterSummarySubscriber {
            client_id: 1,
            writer: tx,
        });
        state.handle_command(DaemonCommand::BroadcastSessionStatus {
            session_id: 42,
            status: SessionStatus::Inference,
        });
        let msg = rx.recv().unwrap();
        assert!(matches!(
            msg,
            DaemonMessage::SessionStatusChanged {
                session_id: 42,
                status: SessionStatus::Inference,
                ..
            }
        ));
        // The metadata index must stay in sync so a later ListSessions serves
        // the fresh status (this is the stale-status bug fix).
        let meta = state.session_metadata.get(&42).expect("index updated");
        assert_eq!(meta.status, SessionStatus::Inference);
        assert!(
            meta.last_modified > 0,
            "last_modified bumped on status change"
        );
    }

    #[test]
    fn handle_create_session_succeeds_when_locked() {
        // CreateSession should succeed even when the daemon is locked,
        // because a session is just a container — credentials are only
        // needed to run models, not to create or browse sessions.
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::CreateSession {
            title: None,
            parent_session_id: None,
            working_dir: None,
            reasoning_effort: None,
            selected_model: None,
            context_config: None,
            account_name: None,
            active_tool_groups: Vec::new(),
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(
            result.is_ok(),
            "CreateSession should succeed even when locked: {:?}",
            result.err()
        );
    }

    #[test]
    fn handle_delete_session_succeeds_when_locked() {
        // DeleteSession should succeed even when the daemon is locked,
        // because a session is just a container — credentials are only
        // needed to run models, not to create, browse, or delete sessions.
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::DeleteSession {
            session_id: 1,
            reply,
        });
        // Should succeed (session 1 doesn't exist, so it's a no-op)
        let result = rx.recv().unwrap();
        assert!(
            result.is_ok(),
            "DeleteSession should succeed even when locked: {:?}",
            result.err()
        );
    }

    #[test]
    fn handle_attach_session_rejects_deleted_session() {
        // A deleted session's still-shutting-down thread can leave the DB
        // record in place until `handle_session_exited` finalizes the delete.
        // The attach guard must refuse to resurrect it even though a record
        // exists on disk.
        let (mut state, _daemon_rx) = make_daemon_state();
        let record = SessionRecord {
            title: Some("ghost".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
        };
        db::write_session(&state.db, 1, &record).unwrap();
        // The deleted marker is set (the session thread has not yet exited,
        // so the record has not been finalized/deleted yet).
        state.deleted_sessions.insert(1);

        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::AttachSession {
            session_id: 1,
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(
            result.is_err(),
            "deleted session must not be resurrected via attach"
        );
        // And it must not have been re-inserted into the in-memory index.
        assert!(!state.session_metadata.contains_key(&1));
        assert!(!state.active_sessions.contains_key(&1));
    }

    #[test]
    fn session_exited_finalizes_pending_delete() {
        // A deleted session's thread exits: `handle_session_exited` must
        // delete the record, clear the deletion tombstone, and drop the
        // deleted marker, so the session cannot resurface and no stale
        // tombstone is left for the startup purge.  The delete now runs on a
        // background thread; wait for its `SessionDeleteFinalized`
        // confirmation — deterministic, because the thread sends it only
        // after the delete and tombstone clear have committed, so `recv`
        // unblocks with the DB already in its final state.
        let (mut state, daemon_rx) = make_daemon_state();
        let record = SessionRecord {
            title: Some("doomed".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
        };
        db::write_session(&state.db, 7, &record).unwrap();
        db::mark_session_deleted(&state.db, 7).unwrap();
        state.deleted_sessions.insert(7);

        state.handle_command(DaemonCommand::SessionExited { session_id: 7 });

        // The background finalize reports back once the record is gone; route
        // it through the command handler exactly as the daemon loop would so
        // the `deleted_sessions` marker is dropped.
        match daemon_rx.recv() {
            Ok(DaemonCommand::SessionDeleteFinalized { session_id: 7 }) => {
                state.handle_command(DaemonCommand::SessionDeleteFinalized { session_id: 7 });
            }
            other => panic!(
                "expected SessionDeleteFinalized for session 7, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // Marker dropped, record gone, tombstone gone (purge is a no-op).
        assert!(!state.deleted_sessions.contains(&7));
        assert!(db::read_session(&state.db, 7).unwrap().is_none());
        assert_eq!(
            db::purge_tombstoned_sessions(&state.db).unwrap(),
            0,
            "tombstone must be cleared once the record is deleted"
        );
    }

    #[test]
    fn delete_finished_session_guards_against_straggler_resurrection() {
        // The fast path (`delete_finished_session`) must set the deleted
        // marker even though the record is gone: the finished thread's
        // `UpdateMetadata` straggler is queued ahead of its `SessionExited`
        // and would otherwise re-insert the session into the index.  The
        // marker blocks that, and the queued `SessionExited` finalizes the
        // delete (no-op here) and drops the marker.
        let (mut state, daemon_rx) = make_daemon_state();
        let record = SessionRecord {
            title: Some("doomed".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
        };
        db::write_session(&state.db, 12, &record).unwrap();
        // A stale tombstone from an earlier interrupted delete of the same id.
        db::mark_session_deleted(&state.db, 12).unwrap();

        // Drive the extracted fast-path method directly: `is_finished()` is
        // gated by the caller in production and cannot be observed
        // deterministically in a unit test, so we exercise the fast-path body
        // itself.
        state.delete_finished_session(12).unwrap();

        // Record gone, marker set, metadata not present, stale tombstone swept.
        assert!(db::read_session(&state.db, 12).unwrap().is_none());
        assert!(
            state.deleted_sessions.contains(&12),
            "fast path must set the deleted marker so stragglers cannot resurrect the session"
        );
        assert!(!state.session_metadata.contains_key(&12));
        assert_eq!(
            db::purge_tombstoned_sessions(&state.db).unwrap(),
            0,
            "stale tombstone must be cleared on the fast-path delete"
        );

        // The straggler UpdateMetadata queued ahead of SessionExited must be
        // ignored (the marker blocks re-insertion into the index).
        state.handle_command(DaemonCommand::UpdateMetadata {
            session_id: 12,
            metadata: SessionMetadata {
                title: Some("doomed".into()),
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 2000,
                turn_count: 0,
                status: SessionStatus::Inactive,
                active_tool_groups: vec![],
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        });
        assert!(
            !state.session_metadata.contains_key(&12),
            "straggler UpdateMetadata must not resurrect a deleted session"
        );

        // The queued SessionExited runs the standard finalize on a background
        // thread; wait for its SessionDeleteFinalized confirmation and route
        // it through the command handler exactly as the daemon loop would.
        state.handle_command(DaemonCommand::SessionExited { session_id: 12 });
        match daemon_rx.recv() {
            Ok(DaemonCommand::SessionDeleteFinalized { session_id: 12 }) => {
                state.handle_command(DaemonCommand::SessionDeleteFinalized { session_id: 12 });
            }
            other => panic!(
                "expected SessionDeleteFinalized for session 12, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert!(
            !state.deleted_sessions.contains(&12),
            "marker must be dropped once the finalize confirms the record is gone"
        );
    }

    #[test]
    fn delete_session_clears_stale_tombstone_when_no_live_thread() {
        // Deleting a session that has no live thread deletes the record
        // immediately AND sweeps any stale deletion tombstone left by an
        // earlier interrupted delete of the same id, so the tombstone cannot
        // accumulate or trigger a redundant startup purge.
        let (mut state, _daemon_rx) = make_daemon_state();
        let record = SessionRecord {
            title: Some("stale".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
        };
        db::write_session(&state.db, 3, &record).unwrap();
        db::mark_session_deleted(&state.db, 3).unwrap();

        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::DeleteSession {
            session_id: 3,
            reply,
        });
        assert!(rx.recv().unwrap().is_ok());

        // Record gone, tombstone swept, and no deleted marker leaked (there
        // was no live thread to defer to).
        assert!(db::read_session(&state.db, 3).unwrap().is_none());
        assert_eq!(
            db::purge_tombstoned_sessions(&state.db).unwrap(),
            0,
            "stale tombstone must be cleared on immediate delete"
        );
        assert!(!state.deleted_sessions.contains(&3));
    }

    #[test]
    fn delete_session_defers_when_thread_alive() {
        // Deleting a session whose thread is still running must NOT delete
        // the record synchronously — the thread can re-create it via
        // `persist_and_exit` during shutdown.  Instead it marks the session
        // deleted and writes a tombstone (crash-window safety); the record is
        // removed later by `handle_session_exited`'s background finalize.
        let (mut state, _daemon_rx) = make_daemon_state();
        let record = SessionRecord {
            title: Some("deferred".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
        };
        db::write_session(&state.db, 4, &record).unwrap();
        // A blocking stand-in session thread: not finished, so the delete
        // must take the deferred path.
        let (_cmd_rx, release_tx) = insert_active_session(&mut state, 4);

        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::DeleteSession {
            session_id: 4,
            reply,
        });
        assert!(rx.recv().unwrap().is_ok());

        // Deferred: record still present, marker set, tombstone written
        // (a startup purge would clean it up if the daemon died now).
        assert!(db::read_session(&state.db, 4).unwrap().is_some());
        assert!(state.deleted_sessions.contains(&4));
        assert_eq!(
            db::purge_tombstoned_sessions(&state.db).unwrap(),
            1,
            "deferred delete must write a tombstone"
        );

        // Release the stand-in thread so it exits (no leaked threads).
        drop(release_tx);
    }

    #[test]
    fn delete_session_keeps_tombstone_while_a_delete_is_pending() {
        // A second DeleteSession arriving while an earlier deferred delete is
        // still shutting its thread down must NOT sweep the pending delete's
        // tombstone: the thread can re-create the record via
        // `persist_and_exit` before its finalize runs, and a swept tombstone
        // would let a crash in that window resurrect the deleted session at
        // the next startup.
        let (mut state, _daemon_rx) = make_daemon_state();
        let record = SessionRecord {
            title: Some("double-deleted".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
        };
        db::write_session(&state.db, 5, &record).unwrap();
        // First delete defers: live thread → marker set, tombstone written,
        // entry removed (the record stays until the thread exits).
        let (_cmd_rx, release_tx) = insert_active_session(&mut state, 5);
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::DeleteSession {
            session_id: 5,
            reply,
        });
        assert!(rx.recv().unwrap().is_ok());
        assert!(state.deleted_sessions.contains(&5));

        // Second delete arrives before the thread has exited: there is no live
        // entry now, so it takes the immediate-delete branch — but the pending
        // delete still owns the tombstone, which must survive.  (The tombstone
        // is probed with `purge_tombstoned_sessions` only once, at the end,
        // because the purge both deletes the record and clears tombstones.)
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::DeleteSession {
            session_id: 5,
            reply,
        });
        assert!(rx.recv().unwrap().is_ok());
        assert!(
            state.deleted_sessions.contains(&5),
            "marker must stay while the deferred delete is pending"
        );
        assert_eq!(
            db::purge_tombstoned_sessions(&state.db).unwrap(),
            1,
            "the pending delete's tombstone must not be swept by a second delete"
        );

        // Release the stand-in thread so it exits (no leaked threads).
        drop(release_tx);
    }

    #[test]
    fn session_delete_finalized_drops_marker() {
        // The background finalize's confirmation must be the thing that drops
        // the `deleted_sessions` marker — clearing it earlier (e.g. on the
        // zombie's `SessionExited`) would reopen an attach-resurrection
        // window while the record is still on disk.
        let (mut state, _daemon_rx) = make_daemon_state();
        state.deleted_sessions.insert(9);
        state.handle_command(DaemonCommand::SessionDeleteFinalized { session_id: 9 });
        assert!(!state.deleted_sessions.contains(&9));
    }

    #[test]
    fn session_exited_does_not_delete_non_deleted_session() {
        // A normal (non-deleted) session exit must leave the record alone —
        // finalize only runs for sessions whose delete is still pending.
        let (mut state, _daemon_rx) = make_daemon_state();
        let record = SessionRecord {
            title: Some("alive".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
        };
        db::write_session(&state.db, 8, &record).unwrap();

        state.handle_command(DaemonCommand::SessionExited { session_id: 8 });

        assert!(db::read_session(&state.db, 8).unwrap().is_some());
    }

    #[test]
    fn broadcast_sends_to_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        state.summary_subscribers.insert(1, tx);
        let msg = DaemonMessage::SessionDeleted { session_id: 42 };
        state.broadcast(msg.clone());
        let received = rx.recv().unwrap();
        assert_eq!(received, msg);
        // Subscriber should still be registered
        assert!(state.summary_subscribers.contains_key(&1));
    }

    #[test]
    fn broadcast_removes_disconnected_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        state.summary_subscribers.insert(1, tx);
        drop(rx); // Disconnect the receiver
        state.broadcast(DaemonMessage::SessionDeleted { session_id: 42 });
        // Dead subscriber should be removed
        assert!(!state.summary_subscribers.contains_key(&1));
    }

    #[test]
    fn handle_validate_model_allows_through_when_no_session() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ValidateModel {
            session_id: 999,
            model: "gpt-4".into(),
            reply,
        });
        let result = rx.recv().unwrap();
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn handle_validate_model_rejects_when_no_provider() {
        let (mut state, _rx) = make_daemon_state();
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: None,
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 1000,
                turn_count: 0,
                status: SessionStatus::Sleeping,
                active_tool_groups: vec![],
                account_name: Some("locked-account".into()),
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ValidateModel {
            session_id: 1,
            model: "gpt-4".into(),
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("locked"), "error should mention locked daemon");
        assert!(
            err.contains("locked-account"),
            "error should mention the account"
        );
    }

    #[test]
    fn handle_validate_model_rejects_unknown_model() {
        let (mut state, _rx) = make_daemon_state();
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: None,
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 1000,
                turn_count: 0,
                status: SessionStatus::Inactive,
                active_tool_groups: vec![],
                account_name: Some("test-account".into()),
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
        state
            .providers
            .insert("test-account".into(), make_test_provider());
        state.model_cache.insert(
            "test-account".into(),
            (vec!["gpt-4".into(), "gpt-3.5".into()], Instant::now()),
        );
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ValidateModel {
            session_id: 1,
            model: "nonexistent-model".into(),
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("nonexistent-model"),
            "error should mention the model name"
        );
        assert!(err.contains("gpt-4"), "error should list available models");
    }

    #[test]
    fn handle_validate_model_allows_known_model() {
        let (mut state, _rx) = make_daemon_state();
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: None,
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 1000,
                turn_count: 0,
                status: SessionStatus::Inactive,
                active_tool_groups: vec![],
                account_name: Some("test-account".into()),
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
        state
            .providers
            .insert("test-account".into(), make_test_provider());
        state.model_cache.insert(
            "test-account".into(),
            (vec!["gpt-4".into(), "gpt-3.5".into()], Instant::now()),
        );
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ValidateModel {
            session_id: 1,
            model: "gpt-4".into(),
            reply,
        });
        let result = rx.recv().unwrap();
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn handle_set_session_title_forwards_to_session() {
        let (mut state, _daemon_rx) = make_daemon_state();

        // Create an active session entry with a cmd_tx so the daemon
        // can forward the title change.
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (handle_tx, handle_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            // Block until told to stop — we just need the entry to exist.
            let _ = handle_rx.recv();
        });
        state
            .active_sessions
            .insert(1, ActiveSessionEntry { cmd_tx, handle });
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: Some("old title".into()),
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: 1000,
                last_modified: 1000,
                turn_count: 0,
                status: SessionStatus::Inactive,
                active_tool_groups: vec!["core".into()],
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );

        state.handle_command(DaemonCommand::SetSessionTitle {
            session_id: 1,
            title: "new title".into(),
        });

        // Verify the session thread received a SetTitle command.
        // The send is synchronous (handle_command sends on cmd_tx), so
        // try_recv is deterministic — no time-based wait needed.
        match cmd_rx.try_recv() {
            Ok(SessionCommand::SetTitle { title }) => {
                assert_eq!(title, "new title");
            }
            Ok(_) => {
                panic!("expected SetTitle, got a different SessionCommand variant");
            }
            Err(e) => {
                panic!("expected SetTitle, got error: {e}");
            }
        }

        // Clean up the session thread.
        let _ = handle_tx.send(());
    }

    #[test]
    fn handle_set_session_title_nonexistent_session_logs_warning() {
        let (mut state, _rx) = make_daemon_state();

        // Sending SetSessionTitle for a session that doesn't exist should
        // log a warning and not panic.
        state.handle_command(DaemonCommand::SetSessionTitle {
            session_id: 999,
            title: "ghost title".into(),
        });
        // No active session = no message to verify; just checking no panic.
    }

    // ── Session-config tool forwarding tests ────────────────────────────

    /// Insert a minimal active-session entry whose command channel is
    /// returned for verification.  The session thread blocks until the
    /// returned release sender fires, then exits.
    fn insert_active_session(
        state: &mut DaemonState,
        session_id: u64,
    ) -> (mpsc::Receiver<SessionCommand>, mpsc::Sender<()>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            // Block until told to stop — we just need the entry to exist.
            let _ = release_rx.recv();
        });
        state
            .active_sessions
            .insert(session_id, ActiveSessionEntry { cmd_tx, handle });
        (cmd_rx, release_tx)
    }

    #[test]
    fn handle_set_working_dir_forwards_to_session() {
        let (mut state, _daemon_rx) = make_daemon_state();
        let (cmd_rx, release_tx) = insert_active_session(&mut state, 1);

        state.handle_command(DaemonCommand::SetWorkingDir {
            session_id: 1,
            path: PathBuf::from("/tmp"),
            reply: mpsc::channel().0,
        });

        // The send is synchronous (handle_command sends on cmd_tx), so
        // try_recv is deterministic — no time-based wait needed.
        match cmd_rx.try_recv() {
            Ok(SessionCommand::SetWorkingDir { path, .. }) => {
                assert_eq!(path, PathBuf::from("/tmp"));
            }
            Ok(_) => panic!("expected SetWorkingDir, got a different SessionCommand variant"),
            Err(e) => panic!("expected SetWorkingDir, got error: {e}"),
        }

        // Clean up the session thread.
        let _ = release_tx.send(());
    }

    #[test]
    fn handle_set_working_dir_nonexistent_session_replies_error() {
        let (mut state, _daemon_rx) = make_daemon_state();
        let (reply_tx, reply_rx) = mpsc::channel();

        state.handle_command(DaemonCommand::SetWorkingDir {
            session_id: 999,
            path: PathBuf::from("/tmp"),
            reply: reply_tx,
        });

        // The daemon replies synchronously for inactive sessions so a
        // blocked tool execution never hangs.
        match reply_rx.recv() {
            Ok(Err(msg)) => assert!(msg.contains("not active"), "unexpected msg: {msg}"),
            Ok(Ok(_)) => panic!("expected an error reply for an inactive session"),
            Err(e) => panic!("expected error reply, got {e:?}"),
        }
    }

    #[test]
    fn handle_load_tools_forwards_to_session() {
        let (mut state, _daemon_rx) = make_daemon_state();
        let (cmd_rx, release_tx) = insert_active_session(&mut state, 1);

        state.handle_command(DaemonCommand::LoadTools {
            session_id: 1,
            groups: vec!["x".into()],
            reply: mpsc::channel().0,
        });

        match cmd_rx.try_recv() {
            Ok(SessionCommand::LoadTools { groups, .. }) => {
                assert_eq!(groups, vec!["x"]);
            }
            Ok(_) => panic!("expected LoadTools, got a different SessionCommand variant"),
            Err(e) => panic!("expected LoadTools, got error: {e}"),
        }

        let _ = release_tx.send(());
    }

    #[test]
    fn handle_load_tools_nonexistent_session_replies_error() {
        let (mut state, _daemon_rx) = make_daemon_state();
        let (reply_tx, reply_rx) = mpsc::channel();

        state.handle_command(DaemonCommand::LoadTools {
            session_id: 999,
            groups: vec!["x".into()],
            reply: reply_tx,
        });

        // The daemon replies synchronously for inactive sessions so a
        // blocked tool execution never hangs.
        match reply_rx.recv() {
            Ok(Err(msg)) => assert!(msg.contains("not active"), "unexpected msg: {msg}"),
            Ok(Ok(_)) => panic!("expected an error reply for an inactive session"),
            Err(e) => panic!("expected error reply, got {e:?}"),
        }
    }

    #[test]
    fn handle_unload_tools_forwards_to_session() {
        let (mut state, _daemon_rx) = make_daemon_state();
        let (cmd_rx, release_tx) = insert_active_session(&mut state, 1);

        state.handle_command(DaemonCommand::UnloadTools {
            session_id: 1,
            groups: vec!["x".into()],
            reply: mpsc::channel().0,
        });

        match cmd_rx.try_recv() {
            Ok(SessionCommand::UnloadTools { groups, .. }) => {
                assert_eq!(groups, vec!["x"]);
            }
            Ok(_) => panic!("expected UnloadTools, got a different SessionCommand variant"),
            Err(e) => panic!("expected UnloadTools, got error: {e}"),
        }

        let _ = release_tx.send(());
    }

    #[test]
    fn handle_unload_tools_nonexistent_session_replies_error() {
        let (mut state, _daemon_rx) = make_daemon_state();
        let (reply_tx, reply_rx) = mpsc::channel();

        state.handle_command(DaemonCommand::UnloadTools {
            session_id: 999,
            groups: vec!["x".into()],
            reply: reply_tx,
        });

        match reply_rx.recv() {
            Ok(Err(msg)) => assert!(msg.contains("not active"), "unexpected msg: {msg}"),
            Ok(Ok(_)) => panic!("expected an error reply for an inactive session"),
            Err(e) => panic!("expected error reply, got {e:?}"),
        }
    }

    // ── Activity subscriber tests ───────────────────────────────────────

    #[test]
    fn handle_register_activity_subscriber_adds_to_map() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, _) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx,
        });

        assert!(state.activity_subscribers.contains_key(&10));
    }

    #[test]
    fn handle_register_activity_subscriber_replaces_existing() {
        let (mut state, _rx) = make_daemon_state();
        let (tx1, _) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let (tx2, _) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx1,
        });
        // Re-register with a different writer — should replace without error
        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx2,
        });

        assert!(state.activity_subscribers.contains_key(&10));
    }

    #[test]
    fn handle_unregister_activity_subscriber_preserves_session_tracking() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, _) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        // Set up: client 10 is subscribed to activity AND subscribed to session 42
        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });

        // Unsubscribe from all activity — this should NOT clear session tracking
        state.handle_command(DaemonCommand::UnregisterActivitySubscriber { client_id: 10 });

        // Verify: activity subscriber is gone
        assert!(!state.activity_subscribers.contains_key(&10));
        // Verify: session tracking is PRESERVED
        assert!(state.client_subscribed_sessions.contains_key(&10));
        let sessions = state.client_subscribed_sessions.get(&10).unwrap();
        assert!(sessions.contains(&42));
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn handle_client_disconnected_clears_all_tracking() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, _) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        // Set up: client 10 is registered in all three maps
        state.handle_command(DaemonCommand::RegisterSummarySubscriber {
            client_id: 10,
            writer: tx.clone(),
        });
        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 1,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 2,
        });

        assert!(state.summary_subscribers.contains_key(&10));
        assert!(state.activity_subscribers.contains_key(&10));
        assert!(state.client_subscribed_sessions.contains_key(&10));

        // Disconnect: clears everything
        state.handle_command(DaemonCommand::ClientDisconnected { client_id: 10 });

        assert!(!state.summary_subscribers.contains_key(&10));
        assert!(!state.activity_subscribers.contains_key(&10));
        assert!(!state.client_subscribed_sessions.contains_key(&10));
    }

    #[test]
    fn handle_client_disconnected_noop_for_unknown_client() {
        let (mut state, _rx) = make_daemon_state();
        state.handle_command(DaemonCommand::ClientDisconnected { client_id: 999 });
        // Just checking no panic
    }

    #[test]
    fn handle_track_session_subscription_adds_entry() {
        let (mut state, _rx) = make_daemon_state();

        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });

        let sessions = state
            .client_subscribed_sessions
            .get(&10)
            .expect("client should have entry");
        assert!(sessions.contains(&42));
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn handle_track_session_subscription_idempotent_re_attach() {
        let (mut state, _rx) = make_daemon_state();

        // Attach to same session twice — should be idempotent
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });

        let sessions = state
            .client_subscribed_sessions
            .get(&10)
            .expect("client should have entry");
        assert!(sessions.contains(&42));
        assert_eq!(sessions.len(), 1, "should not duplicate session_id");
    }

    #[test]
    fn handle_track_session_subscription_tracks_multiple_sessions() {
        let (mut state, _rx) = make_daemon_state();

        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 99,
        });

        let sessions = state
            .client_subscribed_sessions
            .get(&10)
            .expect("client should have entry");
        assert!(sessions.contains(&42));
        assert!(sessions.contains(&99));
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn handle_untrack_session_subscription_removes_session() {
        let (mut state, _rx) = make_daemon_state();

        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 99,
        });

        // Untrack one session
        state.handle_command(DaemonCommand::UntrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });

        let sessions = state
            .client_subscribed_sessions
            .get(&10)
            .expect("client should still have entry");
        assert!(!sessions.contains(&42));
        assert!(sessions.contains(&99));
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn handle_untrack_session_subscription_removes_client_when_empty() {
        let (mut state, _rx) = make_daemon_state();

        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });

        // Untrack the only session
        state.handle_command(DaemonCommand::UntrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });

        // Client entry should be removed entirely when empty
        assert!(!state.client_subscribed_sessions.contains_key(&10));
    }

    #[test]
    fn handle_untrack_session_subscription_noop_for_unknown_session() {
        let (mut state, _rx) = make_daemon_state();

        // Untrack a session that was never tracked — should be a no-op
        state.handle_command(DaemonCommand::UntrackSessionSubscription {
            client_id: 10,
            session_id: 42,
        });

        assert!(!state.client_subscribed_sessions.contains_key(&10));
    }

    #[test]
    fn handle_untrack_session_subscription_noop_for_unknown_client() {
        let (mut state, _rx) = make_daemon_state();
        state.handle_command(DaemonCommand::UntrackSessionSubscription {
            client_id: 999,
            session_id: 42,
        });
        // Just checking no panic
    }

    #[test]
    fn handle_broadcast_activity_sends_to_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx,
        });

        let msg = DaemonMessage::OutputChunk {
            session_id: 1,
            request_id: 5,
            stream: choreo_proto::OutputStream::Answer,
            data: b"hello".to_vec(),
        };
        state.handle_command(DaemonCommand::BroadcastActivity(msg.clone()));

        let received = rx.recv().unwrap();
        assert_eq!(received, msg);
        // Subscriber should still be registered
        assert!(state.activity_subscribers.contains_key(&10));
    }

    #[test]
    fn handle_broadcast_activity_skips_dedup_for_session_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        // Use a sync_channel with capacity 1 so we can detect if a message
        // was sent vs skipped.
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        // Client 10 is both an activity subscriber AND a subscriber of session 1
        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 1,
        });

        // Broadcast a message FROM session 1 — should be SKIPPED for client 10
        // because they're already a direct subscriber of session 1.
        let msg = DaemonMessage::OutputChunk {
            session_id: 1,
            request_id: 5,
            stream: choreo_proto::OutputStream::Answer,
            data: b"hello".to_vec(),
        };
        state.handle_command(DaemonCommand::BroadcastActivity(msg));

        // The client should NOT have received the message (it was suppressed
        // by the dedup filter).  The retain closure returned true, so the
        // subscriber remains registered.
        assert!(
            rx.try_recv().is_err(),
            "message should have been suppressed for session subscriber"
        );
        assert!(state.activity_subscribers.contains_key(&10));
    }

    #[test]
    fn handle_broadcast_activity_no_dedup_for_different_session() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        // Client 10 subscribes to session 1, but the broadcast is about session 2
        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 1,
        });

        // Broadcast a message FROM session 2 — client 10 is NOT a subscriber
        // of session 2, so the message should be delivered.
        let msg = DaemonMessage::OutputChunk {
            session_id: 2,
            request_id: 5,
            stream: choreo_proto::OutputStream::Answer,
            data: b"hello".to_vec(),
        };
        state.handle_command(DaemonCommand::BroadcastActivity(msg.clone()));

        let received = rx.recv().unwrap();
        assert_eq!(received, msg);
    }

    #[test]
    fn handle_broadcast_activity_sends_when_no_session_id() {
        // Messages without a session_id (Models, Pong, etc.) should always
        // be delivered to all activity subscribers.
        let (mut state, _rx) = make_daemon_state();
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx,
        });

        let msg = DaemonMessage::Models {
            models: vec!["gpt-4".into()],
            selected_model: Some("gpt-4".into()),
        };
        state.handle_command(DaemonCommand::BroadcastActivity(msg.clone()));

        let received = rx.recv().unwrap();
        assert_eq!(received, msg);
    }

    #[test]
    fn handle_broadcast_activity_removes_disconnected_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        // Use a sync_channel so we can drop the receiver
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);

        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx,
        });

        // Drop the receiver to simulate a disconnected client
        drop(rx);

        // Broadcast should detect the dead subscriber and remove it
        let msg = DaemonMessage::SessionStatusChanged {
            session_id: 1,
            status: SessionStatus::Inactive,
            last_modified: 0,
        };
        state.handle_command(DaemonCommand::BroadcastActivity(msg));

        // Dead subscriber should be removed
        assert!(!state.activity_subscribers.contains_key(&10));
    }

    #[test]
    fn handle_broadcast_activity_handles_multiple_clients() {
        let (mut state, _rx) = make_daemon_state();
        let (tx1, rx1) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let (tx2, rx2) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);

        // Client 10: activity subscriber + session 1 subscriber
        // Client 20: activity subscriber only
        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 10,
            writer: tx1,
        });
        state.handle_command(DaemonCommand::RegisterActivitySubscriber {
            client_id: 20,
            writer: tx2,
        });
        state.handle_command(DaemonCommand::TrackSessionSubscription {
            client_id: 10,
            session_id: 1,
        });

        let msg = DaemonMessage::OutputChunk {
            session_id: 1,
            request_id: 5,
            stream: choreo_proto::OutputStream::Answer,
            data: b"data".to_vec(),
        };
        state.handle_command(DaemonCommand::BroadcastActivity(msg.clone()));

        // Client 10 (session subscriber) should be skipped
        assert!(
            rx1.try_recv().is_err(),
            "client 10 is a session subscriber, should be suppressed"
        );
        // Client 20 (activity only) should receive the message
        let received = rx2.recv().unwrap();
        assert_eq!(received, msg);
    }

    // ── DaemonMessage::session_id() tests ───────────────────────────────

    #[test]
    fn session_id_returns_some_for_session_scoped_variants() {
        let cases: Vec<DaemonMessage> = vec![
            DaemonMessage::SessionCreated {
                session_id: 42,
                title: None,
                parent_session_id: None,
                working_dir: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            },
            DaemonMessage::SessionAttached { session_id: 42 },
            DaemonMessage::SessionState {
                session_id: 42,
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: BTreeMap::new(),
                active_tool_groups: vec![],
                token_usage: None,
                context_window: None,
                last_prompt_tokens: None,
                status: SessionStatus::Inactive,
                reasoning_effort: None,
                reasoning_capability: None,
            },
            DaemonMessage::SessionStatusChanged {
                session_id: 42,
                status: SessionStatus::Inactive,
                last_modified: 0,
            },
            DaemonMessage::SessionFailed {
                session_id: 42,
                operation: "test".into(),
                error: "err".into(),
            },
            DaemonMessage::SessionDeleted { session_id: 42 },
            DaemonMessage::SessionDeleteFailed {
                session_id: 42,
                error: "err".into(),
            },
            DaemonMessage::TurnAppended {
                session_id: 42,
                turn_id: 1,
                turn: Turn {
                    created_at: TimestampMs::now(),
                    undone: false,
                    error: None,
                    user_text: None,
                    assistant_text: None,
                    assistant_reasoning: None,
                    tool_calls: vec![],
                    token_usage: None,
                    tool_results: vec![],
                    displayed_images: vec![],
                },
            },
            DaemonMessage::TurnFinalized {
                session_id: 42,
                turn_id: 1,
                turn: Turn {
                    created_at: TimestampMs::now(),
                    undone: false,
                    error: None,
                    user_text: None,
                    assistant_text: None,
                    assistant_reasoning: None,
                    tool_calls: vec![],
                    token_usage: None,
                    tool_results: vec![],
                    displayed_images: vec![],
                },
            },
            DaemonMessage::Started {
                session_id: 42,
                request_id: 1,
                turn_id: 0,
                estimated_prompt_tokens: 0,
            },
            DaemonMessage::OutputChunk {
                session_id: 42,
                request_id: 1,
                stream: choreo_proto::OutputStream::Answer,
                data: vec![],
            },
            DaemonMessage::Done {
                session_id: 42,
                request_id: 1,
                token_usage: None,
                last_prompt_tokens: None,
            },
            DaemonMessage::Failed {
                session_id: 42,
                request_id: 1,
                error: "e".into(),
            },
            DaemonMessage::Cancelled {
                session_id: 42,
                request_id: 1,
            },
            DaemonMessage::ModelSelected {
                session_id: 42,
                model: "gpt-4".into(),
                reasoning_capability: None,
            },
            DaemonMessage::ModelSelectionFailed {
                session_id: 42,
                model: "gpt-4".into(),
                error: "e".into(),
            },
            DaemonMessage::ReasoningEffortSet {
                session_id: 42,
                effort: "off".into(),
            },
            DaemonMessage::SessionAccountSet {
                session_id: 42,
                account: "test".into(),
            },
            DaemonMessage::ContextWindowResolved {
                session_id: 42,
                context_window: 128000,
            },
            DaemonMessage::SessionTitleSet {
                session_id: 42,
                title: "test".into(),
            },
        ];

        for msg in cases {
            assert_eq!(
                msg.session_id(),
                Some(42),
                "expected session_id=42 for {:?}",
                std::mem::discriminant(&msg)
            );
        }
    }

    #[test]
    fn session_id_returns_none_for_non_session_variants() {
        let cases: Vec<DaemonMessage> = vec![
            DaemonMessage::Sessions { sessions: vec![] },
            DaemonMessage::Pong,
            DaemonMessage::Models {
                models: vec![],
                selected_model: None,
            },
            DaemonMessage::ModelsFailed { error: "e".into() },
            DaemonMessage::Unlocked,
            DaemonMessage::Locked,
            DaemonMessage::LockedError { error: "e".into() },
            DaemonMessage::CredentialAdded {
                service: "s".into(),
            },
            DaemonMessage::CredentialAddFailed {
                service: "s".into(),
                error: "e".into(),
            },
            DaemonMessage::Credential {
                service: "s".into(),
                key: None,
            },
            DaemonMessage::AccountAdded { name: "a".into() },
            DaemonMessage::Accounts { accounts: vec![] },
            DaemonMessage::ShuttingDown,
        ];

        for msg in cases {
            assert!(
                msg.session_id().is_none(),
                "expected no session_id for {:?}",
                std::mem::discriminant(&msg)
            );
        }
    }

    #[test]
    fn session_id_extracts_correct_value() {
        let msg = DaemonMessage::OutputChunk {
            session_id: 12345,
            request_id: 1,
            stream: choreo_proto::OutputStream::Answer,
            data: vec![],
        };
        assert_eq!(msg.session_id(), Some(12345));
    }
}
