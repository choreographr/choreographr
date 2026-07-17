use crate::accounts::{AccountConfig, AccountManager, accounts_config_path};
use crate::db::{self, SessionRecord};
use crate::mcp::McpManager;
use crate::providers::InferenceProvider;
use crate::sessions::{
    ActiveSessionEntry, RequestContext, SessionCommand, SessionMetadata, session_main,
};
use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;
use tai_keystore::ServiceCredential;
use tai_proto::{
    AccountInfo, ContextConfig, DaemonMessage, SessionStatus, SessionSummary, ThinkingEffort,
    TokenUsage,
};
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
    pub accounts: AccountManager,
    pub providers: HashMap<String, InferenceProvider>,
    pub credentials: HashMap<String, ServiceCredential>,
    pub x_credentials: Option<ServiceCredential>,
    pub db: Arc<redb::Database>,
    pub tool_registry: Arc<crate::tools::ToolRegistry>,
    pub daemon_tx: mpsc::Sender<DaemonCommand>,
    pub client_streams: Vec<UnixStream>,
    pub summary_subscribers: HashMap<u64, mpsc::SyncSender<DaemonMessage>>,
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
        reasoning_effort: Option<ThinkingEffort>,
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
            DaemonCommand::Shutdown => {
                warn!("unexpected Shutdown command in handle_command; handled at loop level");
            }
        }
    }

    /// Returns an error if the daemon hasn't been unlocked yet.
    fn ensure_unlocked(&self) -> io::Result<()> {
        if self.credentials.is_empty() {
            Err(io::Error::other("daemon is locked"))
        } else {
            Ok(())
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
        reasoning_effort: Option<ThinkingEffort>,
        selected_model: Option<String>,
        context_config: Option<ContextConfig>,
        account_name: Option<String>,
        active_tool_groups: Vec<String>,
        reply: std::sync::mpsc::Sender<io::Result<(u64, std::sync::mpsc::Sender<SessionCommand>)>>,
    ) {
        // A session is just a conversation container — it can be
        // created regardless of whether the daemon is locked.
        // Credentials are only needed when running models (RunInput).
        // The daemon may be locked, but the user can still browse,
        // create, and delete sessions freely.
        let sid = self.next_session_id;
        self.next_session_id += 1;
        info!("CreateSession: id={}, title={:?}", sid, title);

        let cwd_str = working_dir.as_ref().map(|p| p.display().to_string());
        let active_cats = if active_tool_groups.is_empty() {
            vec!["core".into(), "git".into(), "shell".into()]
        } else {
            active_tool_groups.clone()
        };
        let record = SessionRecord {
            title: title.clone(),
            selected_model,
            reasoning_effort,
            parent_session_id,
            working_dir: cwd_str.clone(),
            max_turns,
            message_count: 0,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            active_tool_groups: active_cats.clone(),
            context_config: context_config.clone().unwrap_or_default(),
            account_name: account_name.clone(),
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        };

        if let Err(e) = db::write_session(&self.db, sid, &record) {
            error!("CreateSession: failed to persist session {}: {e}", sid);
        }

        let metadata = SessionMetadata {
            title: title.clone(),
            selected_model: record.selected_model.clone(),
            reasoning_effort,
            parent_session_id,
            working_dir: cwd_str.clone(),
            created_at: record.created_at,
            message_count: 0,
            max_turns,
            status: SessionStatus::Inactive,
            active_tool_groups: active_cats.clone(),
            account_name: account_name.clone(),
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        };
        let session_tx = self.spawn_session(sid, record, metadata);
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
            .map(|(id, meta)| SessionSummary {
                session_id: *id,
                title: meta.title.clone(),
                selected_model: meta.selected_model.clone(),
                reasoning_effort: meta.reasoning_effort,
                parent_session_id: meta.parent_session_id,
                working_dir: meta.working_dir.clone(),
                created_at: meta.created_at,
                message_count: meta.message_count,
                max_turns: meta.max_turns,
                status: meta.status.clone(),
                active_tool_groups: meta.active_tool_groups.clone(),
                account_name: meta.account_name.clone(),
                token_usage: Some(meta.accumulated_usage),
                context_window: meta.context_window,
                last_prompt_tokens: meta.last_prompt_tokens,
            })
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
            .map(|meta| SessionSummary {
                session_id,
                title: meta.title.clone(),
                selected_model: meta.selected_model.clone(),
                reasoning_effort: meta.reasoning_effort,
                parent_session_id: meta.parent_session_id,
                working_dir: meta.working_dir.clone(),
                created_at: meta.created_at,
                message_count: meta.message_count,
                max_turns: meta.max_turns,
                status: meta.status.clone(),
                active_tool_groups: meta.active_tool_groups.clone(),
                account_name: meta.account_name.clone(),
                token_usage: Some(meta.accumulated_usage),
                context_window: meta.context_window,
                last_prompt_tokens: meta.last_prompt_tokens,
            });
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
    fn handle_session_exited(&mut self, session_id: u64) {
        info!("SessionExited: id={}", session_id);
        crate::metrics::record_session_exited();
        self.active_sessions.remove(&session_id);
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
                tai_keystore::crypto::decrypt_with_private_key(&key, &encrypted_blob)
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

        // No provider for this account → cannot validate, allow through.
        if !self.providers.contains_key(&account_name) {
            debug!(
                "ValidateModel: no provider for account '{account_name}', \
                 allowing model '{model}' through"
            );
            let _ = reply.send(Ok(()));
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

    /// Delete a session, shutting down its thread and removing it from the DB.
    fn handle_delete_session(
        &mut self,
        session_id: u64,
        reply: std::sync::mpsc::Sender<io::Result<()>>,
    ) {
        info!("DeleteSession: id={}", session_id);
        if let Err(e) = self.ensure_unlocked() {
            let _ = reply.send(Err(e));
            return;
        }
        // Gracefully shut down the session thread so it can persist its
        // final state before we delete from the DB — otherwise the
        // session's persist_and_exit would re-write the session
        // back to the DB after we delete it.
        if let Some(entry) = self.active_sessions.remove(&session_id) {
            let _ = entry.cmd_tx.send(SessionCommand::Shutdown);
            let _ = entry.handle.join();
        }
        // Remove from in-memory metadata
        self.session_metadata.remove(&session_id);
        // Remove from database
        if let Err(e) = db::delete_session(&self.db, session_id) {
            error!(
                "DeleteSession: failed to delete session {} from db: {e}",
                session_id
            );
            let _ = reply.send(Err(e));
            return;
        }
        // Broadcast deletion to subscribers
        self.broadcast(DaemonMessage::SessionDeleted { session_id });
        let _ = reply.send(Ok(()));
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
        match tai_keystore::crypto::decrypt_with_private_key(&key, blob) {
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
    use crate::server::connection::SUBSCRIBER_CHANNEL_CAPACITY;
    use crate::sessions::SessionMetadata;
    use std::collections::HashMap;
    use std::sync::mpsc;
    use tai_proto::{DaemonMessage, SessionStatus};

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
            accounts: AccountManager::load(&accounts_path).unwrap(),
            providers: HashMap::new(),
            credentials: HashMap::new(),
            x_credentials: None,
            db,
            tool_registry,
            daemon_tx,
            client_streams: Vec::new(),
            summary_subscribers: HashMap::new(),
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
                message_count: 3,
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
                message_count: 0,
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
            message_count: 5,
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
        assert_eq!(stored.message_count, 5);
        assert_eq!(stored.status, SessionStatus::Inference);
    }

    #[test]
    fn handle_session_exited_nonexistent() {
        let (mut state, _rx) = make_daemon_state();
        state.handle_command(DaemonCommand::SessionExited { session_id: 999 });
        assert!(state.session_metadata.get(&999).is_none());
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
    fn ensure_unlocked_returns_err_when_locked() {
        let (state, _rx) = make_daemon_state();
        let result = state.ensure_unlocked();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "daemon is locked");
    }

    #[test]
    fn handle_delete_session_locked() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::DeleteSession {
            session_id: 1,
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "daemon is locked");
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
    fn handle_validate_model_allows_through_when_no_provider() {
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
                message_count: 0,
                max_turns: None,
                status: SessionStatus::Sleeping,
                active_tool_groups: vec![],
                account_name: Some("nonexistent".into()),
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
        assert_eq!(result, Ok(()));
    }
}
