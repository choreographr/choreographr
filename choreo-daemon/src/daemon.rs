use crate::accounts::{AccountConfig, AccountManager, accounts_config_path};
use crate::db::{self, SessionRecord};
use crate::mcp::McpManager;
use crate::providers::{InferenceProvider, lookup_context_window};
use crate::sessions::{
    ActiveSessionEntry, CANCEL_ALL, RequestContext, SessionCommand, SessionMetadata, session_main,
};
use choreo_keystore::ServiceCredential;
use choreo_proto::{
    AccountInfo, ContextConfig, DaemonMessage, SessionStatus, SessionSummary, TokenUsage,
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
        max_turns: Option<u32>,
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
}

impl DaemonState {
    pub fn handle_command(&mut self, cmd: DaemonCommand) {
        match cmd {
            DaemonCommand::CreateSession {
                title,
                parent_session_id,
                working_dir,
                max_turns,
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
                max_turns,
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
            DaemonCommand::TrackSessionSubscription { client_id, session_id } => {
                self.handle_track_session_subscription(client_id, session_id)
            }
            DaemonCommand::UntrackSessionSubscription { client_id, session_id } => {
                self.handle_untrack_session_subscription(client_id, session_id)
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
                reply,
            } => self.handle_add_account(
                name,
                provider,
                base_url,
                streaming,
                retry_max_attempts,
                connect_timeout_secs,
                request_timeout_secs,
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
        let max_turns_default = self.max_turns;

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
                    max_turns_default,
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

    #[allow(clippy::too_many_arguments)]
    /// Create a new session. Sessions are lightweight containers that can be
    /// created regardless of lock state.
    fn handle_create_session(
        &mut self,
        title: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<PathBuf>,
        max_turns: Option<u32>,
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

        let record = SessionRecord {
            title: title.clone(),
            selected_model,
            reasoning_effort,
            parent_session_id,
            working_dir: cwd_str.clone(),
            max_turns,
            turn_count: 0,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
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
            turn_count: 0,
            max_turns,
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
            max_turns,
        };
        let status_msg = DaemonMessage::SessionStatusChanged {
            session_id: sid,
            status: SessionStatus::Inactive,
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

    /// Return a list of all active session summaries.
    fn handle_list_sessions(&mut self, reply: std::sync::mpsc::Sender<Vec<SessionSummary>>) {
        let mut summaries: Vec<SessionSummary> = self
            .session_metadata
            .iter()
            .map(|(id, meta)| meta.to_summary(*id))
            .collect();

        summaries.sort_by_key(|s| s.session_id);
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
    fn handle_update_metadata(&mut self, session_id: u64, metadata: SessionMetadata) {
        debug!(
            "UpdateMetadata: id={}, model={:?}",
            session_id, metadata.selected_model
        );
        self.session_metadata.insert(session_id, metadata);
    }

    /// Mark a session as exited (sleeping) and broadcast the status change.
    /// If the session has any children, cancel and shut them down so they
    /// don't continue running as orphans.
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

        if let Some(meta) = self.session_metadata.get_mut(&session_id) {
            meta.status = SessionStatus::Sleeping;
        }
        let msg = DaemonMessage::SessionStatusChanged {
            session_id,
            status: SessionStatus::Sleeping,
        };
        self.broadcast(msg);
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

    /// Broadcast a session status change to all summary subscribers.
    fn handle_broadcast_session_status(&mut self, session_id: u64, status: SessionStatus) {
        let msg = DaemonMessage::SessionStatusChanged { session_id, status };
        self.broadcast(msg);
    }

    /// Register a client to receive all session activity broadcasts.
    fn handle_register_activity_subscriber(
        &mut self,
        client_id: u64,
        writer: std::sync::mpsc::SyncSender<DaemonMessage>,
    ) {
        self.activity_subscribers.insert(client_id, writer);
    }

    /// Unregister a client from all session activity broadcasts.
    fn handle_unregister_activity_subscriber(&mut self, client_id: u64) {
        self.activity_subscribers.remove(&client_id);
        // Also clean up the session subscription tracking for this client
        // so stale entries don't accumulate.
        self.client_subscribed_sessions.remove(&client_id);
    }

    /// Track that `client_id` is a direct subscriber of `session_id`.
    /// Idempotent — re-attach to the same session is a no-op.
    fn handle_track_session_subscription(&mut self, client_id: u64, session_id: u64) {
        self.client_subscribed_sessions
            .entry(client_id)
            .or_default()
            .insert(session_id);
    }

    /// Untrack that `client_id` is no longer a direct subscriber of `session_id`.
    fn handle_untrack_session_subscription(&mut self, client_id: u64, session_id: u64) {
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
        let origin_session_id = message_session_id(&msg);
        self.activity_subscribers
            .retain(|client_id, tx| {
                // Skip if this client is also a direct subscriber of the
                // session that originated this message — they'll receive it
                // through the per-session broadcast path.
                if let Some(ref sid) = origin_session_id {
                    if let Some(sessions) = self.client_subscribed_sessions.get(client_id) {
                        if sessions.contains(sid) {
                            return true;
                        }
                    }
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

    /// Shared session-teardown logic used by both `handle_delete_session`
    /// (with permission checks) and cascade-deletion of children.
    /// Returns an error if the database delete fails; callers decide whether
    /// to stop or continue (cascade-delete continues on error).
    fn delete_session_inner(&mut self, session_id: u64) -> io::Result<()> {
        info!("DeleteSession (inner): id={}", session_id);
        // Gracefully shut down the session thread so it can persist its
        // final state before we delete from the DB — otherwise the
        // session's persist_and_exit would re-write the session
        // back to the DB after we delete it.
        if let Some(entry) = self.active_sessions.remove(&session_id) {
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
            let _ = entry.handle.join();
        }
        // Remove from in-memory metadata
        self.session_metadata.remove(&session_id);
        // Remove from database
        db::delete_session(&self.db, session_id)?;
        // Broadcast deletion to subscribers
        self.broadcast(DaemonMessage::SessionDeleted { session_id });
        Ok(())
    }

    /// Add a new inference account.
    #[allow(clippy::too_many_arguments)]
    fn handle_add_account(
        &mut self,
        name: String,
        provider: String,
        base_url: Option<String>,
        streaming: Option<bool>,
        retry_max_attempts: Option<u32>,
        connect_timeout_secs: Option<u64>,
        request_timeout_secs: Option<u64>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let config = AccountConfig {
            base_url,
            streaming,
            retry_max_attempts,
            connect_timeout_secs,
            request_timeout_secs,
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

/// Extract the `session_id` field from a [`DaemonMessage`] variant, if it has one.
/// Used by `handle_broadcast_activity` to filter duplicates by origin session.
fn message_session_id(msg: &DaemonMessage) -> Option<u64> {
    match msg {
        DaemonMessage::SessionCreated { session_id, .. }
        | DaemonMessage::SessionAttached { session_id }
        | DaemonMessage::SessionState { session_id, .. }
        | DaemonMessage::SessionStatusChanged { session_id, .. }
        | DaemonMessage::SessionFailed { session_id, .. }
        | DaemonMessage::SessionDeleted { session_id }
        | DaemonMessage::SessionDeleteFailed { session_id, .. }
        | DaemonMessage::TurnAppended { session_id, .. }
        | DaemonMessage::TurnFinalized { session_id, .. }
        | DaemonMessage::TurnsUndone { session_id, .. }
        | DaemonMessage::TurnsRedone { session_id, .. }
        | DaemonMessage::Started { session_id, .. }
        | DaemonMessage::OutputChunk { session_id, .. }
        | DaemonMessage::ToolCallStarted { session_id, .. }
        | DaemonMessage::ToolCallFinished { session_id, .. }
        | DaemonMessage::ToolCallFailed { session_id, .. }
        | DaemonMessage::ToolResultChunk { session_id, .. }
        | DaemonMessage::Done { session_id, .. }
        | DaemonMessage::Failed { session_id, .. }
        | DaemonMessage::Cancelled { session_id, .. }
        | DaemonMessage::ModelSelected { session_id, .. }
        | DaemonMessage::ModelSelectionFailed { session_id, .. }
        | DaemonMessage::TokenUsageUpdate { session_id, .. }
        | DaemonMessage::LiveOutputTokenCount { session_id, .. }
        | DaemonMessage::SessionAccountSet { session_id, .. }
        | DaemonMessage::ContextWindowResolved { session_id, .. }
        | DaemonMessage::SessionWorkingDirSet { session_id, .. }
        | DaemonMessage::SessionTitleSet { session_id, .. }
        | DaemonMessage::ReasoningEffortSet { session_id, .. }
        | DaemonMessage::ReasoningEffortSetFailed { session_id, .. } => Some(*session_id),
        // The following variants do not carry a session_id.
        DaemonMessage::Sessions { .. }
        | DaemonMessage::Pong
        | DaemonMessage::Models { .. }
        | DaemonMessage::ModelsFailed { .. }
        | DaemonMessage::Unlocked
        | DaemonMessage::Locked
        | DaemonMessage::LockedError { .. }
        | DaemonMessage::CredentialAdded { .. }
        | DaemonMessage::CredentialAddFailed { .. }
        | DaemonMessage::CredentialRemoved { .. }
        | DaemonMessage::CredentialRemoveFailed { .. }
        | DaemonMessage::Credential { .. }
        | DaemonMessage::AccountAdded { .. }
        | DaemonMessage::AccountAddFailed { .. }
        | DaemonMessage::AccountRemoved { .. }
        | DaemonMessage::AccountRemoveFailed { .. }
        | DaemonMessage::Accounts { .. }
        | DaemonMessage::AccountListFailed { .. }
        | DaemonMessage::ShuttingDown => None,
        // Catch-all for any future variants added to this #[non_exhaustive] enum.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_util::make_test_provider;
    use crate::server::connection::SUBSCRIBER_CHANNEL_CAPACITY;
    use crate::sessions::SessionMetadata;
    use choreo_proto::{DaemonMessage, SessionStatus};
    use std::collections::HashMap;
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
                turn_count: 3,
                max_turns: None,
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
                turn_count: 0,
                max_turns: None,
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
            turn_count: 5,
            max_turns: None,
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
                status: SessionStatus::Inference
            }
        ));
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
            max_turns: None,
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
                turn_count: 0,
                max_turns: None,
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
                turn_count: 0,
                max_turns: None,
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
                turn_count: 0,
                max_turns: None,
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
                turn_count: 0,
                max_turns: None,
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
}
