//! The OAuth login modal: the state that renders a `/login` flow and drives
//! the shared login library.
//!
//! A `/login <account>` command resolves the account to its provider, starts
//! the `choreo_client_core` login worker for that provider's descriptor, and
//! opens this modal. The modal renders the streamed progress (the authorize
//! URL, a device code, prompts) and, on completion, the caller sends the
//! `AddCredential` message the library built. Cancelling the modal (Esc) trips
//! the login worker's cancel flag.

use choreo_client_core::{
    LoginEvent, LoginOptions, LoginSession, McpLoginInfo, McpLoginOptions, start_login,
    start_mcp_login,
};
use choreo_proto::{ClientMessageType, OAuthProviderDescriptor};

use super::App;

/// The live state of an in-progress OAuth login.
pub(crate) struct LoginModal {
    /// The provider slug the flow targets (for the title).
    pub(crate) provider: String,
    /// The account being signed in (the `AddCredential` service).
    pub(crate) account: String,
    /// The latest human-readable status line.
    pub(crate) status: String,
    /// The authorize URL to display, once emitted.
    pub(crate) auth_url: Option<String>,
    /// A device code and its verification URI, once emitted.
    pub(crate) device_code: Option<(String, String)>,
    /// The worker session. `None` once the flow has ended (the modal closes).
    /// Dropping the modal drops this session, which requests cancellation — so a
    /// modal closed mid-flow (a quit, or a replaced modal) never leaves the login
    /// thread, its browser, or its loopback server running.
    session: Option<LoginSession>,
}

impl LoginModal {
    /// Start a login flow for `descriptor`, storing under `account`.
    pub(crate) fn start(
        descriptor: OAuthProviderDescriptor,
        account: String,
        device: bool,
    ) -> Self {
        let provider = descriptor.provider.clone();
        let options = LoginOptions {
            account: account.clone(),
            // Best-effort browser open: a headless host fails and falls back to
            // the URL shown in this modal.
            no_browser: false,
            callback_url: None,
            device,
        };
        let session = start_login(descriptor, options);
        Self {
            provider,
            account,
            status: "Starting sign-in…".to_string(),
            auth_url: None,
            device_code: None,
            session: Some(session),
        }
    }

    /// Start an MCP server sign-in flow (discovery + DCR + PKCE) for `info`.
    pub(crate) fn start_mcp(info: McpLoginInfo, options: McpLoginOptions) -> Self {
        let provider = info.slug.clone();
        let account = info.credential_key.clone();
        let session = start_mcp_login(info, options);
        Self {
            provider,
            account,
            status: "Starting sign-in…".to_string(),
            auth_url: None,
            device_code: None,
            session: Some(session),
        }
    }

    /// A clone of the worker's event receiver, for the UI loop to poll, or
    /// `None` once the session has ended.
    pub(crate) fn events(&self) -> Option<crossbeam_channel::Receiver<LoginEvent>> {
        self.session.as_ref().map(|s| s.events.clone())
    }

    /// Request cancellation of the flow.
    pub(crate) fn cancel(&self) {
        if let Some(session) = &self.session {
            session.cancel();
        }
    }
}

#[cfg(test)]
impl LoginModal {
    /// A modal with no live worker, for unit tests of the terminal
    /// `apply_login_event` transitions. No worker is started, so `events`
    /// returns `None` and `cancel` is a no-op; only the terminal path under test
    /// (which never touches the worker) is exercised.
    pub(crate) fn test_modal(provider: &str, account: &str) -> Self {
        Self {
            provider: provider.to_string(),
            account: account.to_string(),
            status: String::new(),
            auth_url: None,
            device_code: None,
            session: None,
        }
    }
}

/// Classify a login event: `None` for an in-progress event, or the terminal
/// outcome once the flow finished or failed.
fn classify_event(event: LoginEvent) -> Option<LoginOutcome> {
    match event {
        LoginEvent::AuthUrl { .. }
        | LoginEvent::DeviceCode { .. }
        | LoginEvent::Prompt { .. }
        | LoginEvent::Progress { .. } => None,
        LoginEvent::Done { service, message } => Some(LoginOutcome::Done { service, message }),
        LoginEvent::Error { message } => Some(LoginOutcome::Error(message)),
    }
}

/// The terminal result of a login flow.
pub(crate) enum LoginOutcome {
    /// The flow produced the `AddCredential` request to send.
    Done {
        /// The account the credential belongs to.
        service: String,
        /// The ready-to-send request.
        message: Box<ClientMessageType>,
    },
    /// The flow failed; carries a secret-free reason.
    Error(String),
}

impl App {
    /// Begin a `/login <account>` flow: resolve the account's provider,
    /// request the OAuth descriptors if not yet cached, and (once they are
    /// known) start the flow.
    pub(crate) fn begin_login(
        &mut self,
        account: String,
        device: bool,
        client_tx: &crossbeam_channel::Sender<choreo_proto::ClientMessage>,
    ) {
        let Some(info) = self
            .ai_providers
            .accounts
            .iter()
            .find(|a| a.name == account)
        else {
            self.status = Some(format!(
                "no account '{account}'; create it first with /add-account"
            ));
            return;
        };
        let provider = info.provider.clone();
        if !self.oauth_providers_fetched {
            // Fetch once, then start the pending flow when the reply lands.
            self.pending_login = Some((account, provider, device));
            self.oauth_providers_fetched = true;
            self.pending
                .send(client_tx, ClientMessageType::ListOAuthProviders);
            self.status = Some("fetching OAuth sign-in options…".to_string());
            return;
        }
        self.start_login_flow(&provider, account, device);
    }

    /// Start the login worker for `provider`, or surface that it has no OAuth
    /// flow.
    fn start_login_flow(&mut self, provider: &str, account: String, device: bool) {
        let Some(descriptor) = self
            .oauth_providers
            .iter()
            .find(|d| d.provider == provider)
            .cloned()
        else {
            self.status = Some(format!(
                "provider '{provider}' does not support OAuth sign-in; use /add-key or a supported provider"
            ));
            return;
        };
        self.status = Some(format!("signing in {account} ({provider})…"));
        self.login = Some(LoginModal::start(descriptor, account, device));
    }

    /// Handle a `ListOAuthProviders` reply: cache the descriptors and, if a
    /// `/login` was waiting on them, start that flow now.
    pub(crate) fn handle_oauth_providers_reply(&mut self, providers: Vec<OAuthProviderDescriptor>) {
        self.oauth_providers = providers;
        self.oauth_providers_fetched = true;
        if let Some((account, provider, device)) = self.pending_login.take() {
            self.start_login_flow(&provider, account, device);
        }
    }

    /// Begin a `/mcp login <slug>` flow.
    ///
    /// The server's resolved `oauth` block, resource URL, and credential key
    /// are daemon-owned and change with the config (adding a scope or a client
    /// id, say), so a FRESH status is always requested and the flow starts when
    /// it arrives — a cached copy may predate a config edit (e.g. adding
    /// `offline_access` to the scopes). The reply is handled by
    /// [`Self::on_mcp_status`].
    pub(crate) fn begin_mcp_login(
        &mut self,
        slug: String,
        client_tx: &crossbeam_channel::Sender<choreo_proto::ClientMessage>,
    ) {
        // A newer command supersedes any flow still awaiting a status reply.
        self.pending_mcp_logout = None;
        self.pending_mcp_login = Some(slug);
        self.pending
            .send(client_tx, ClientMessageType::McpStatusRequest);
        self.status = Some("fetching MCP server status…".to_string());
    }

    /// Begin a `/mcp logout <slug>` flow.
    ///
    /// As with login, the credential key is read from a FRESH status: a stale
    /// cached key could remove the wrong credential (or report a false "no
    /// credential"). The reply is handled by [`Self::on_mcp_status`].
    pub(crate) fn begin_mcp_logout(
        &mut self,
        slug: String,
        client_tx: &crossbeam_channel::Sender<choreo_proto::ClientMessage>,
    ) {
        // A newer command supersedes any flow still awaiting a status reply.
        self.pending_mcp_login = None;
        self.pending_mcp_logout = Some(slug);
        self.pending
            .send(client_tx, ClientMessageType::McpStatusRequest);
        self.status = Some("fetching MCP server status…".to_string());
    }

    /// Start the login/logout a fresh `McpStatus` reply was awaited for.
    ///
    /// Called from the `McpStatus` reply handler with that reply's `servers`, so
    /// the flow resolves the daemon's LIVE oauth block and credential key
    /// straight from the reply — there is no cached status that could go stale.
    pub(crate) fn on_mcp_status(
        &mut self,
        servers: &[choreo_proto::McpServerStatus],
        client_tx: &crossbeam_channel::Sender<choreo_proto::ClientMessage>,
    ) {
        if let Some(slug) = self.pending_mcp_login.take() {
            self.start_mcp_login_for(servers, &slug);
        }
        if let Some(slug) = self.pending_mcp_logout.take() {
            self.logout_mcp(servers, &slug, client_tx);
        }
    }

    /// Start the MCP sign-in worker for `slug`, or surface why it cannot be
    /// signed into (no `oauth` block, or not a remote server).
    fn start_mcp_login_for(&mut self, servers: &[choreo_proto::McpServerStatus], slug: &str) {
        let Some(server) = servers.iter().find(|s| s.slug == slug) else {
            self.status = Some(format!("no MCP server '{slug}'"));
            return;
        };
        let Some(oauth) = server.oauth.clone() else {
            self.status = Some(format!(
                "MCP server '{slug}' has no oauth block; it cannot be signed in"
            ));
            return;
        };
        let Some(credential_key) = server.credential_key.clone() else {
            self.status = Some(format!("MCP server '{slug}' is not a remote server"));
            return;
        };
        let info = McpLoginInfo {
            slug: slug.to_string(),
            resource: server.target.clone(),
            credential_key,
            oauth,
        };
        self.status = Some(format!("signing in MCP server {slug}…"));
        self.login = Some(LoginModal::start_mcp(info, McpLoginOptions::default()));
    }

    // Private: the only caller is `on_mcp_status`, which reads the freshly
    // received statuses rather than a cache.
    /// Sign out of an MCP server: remove its stored credential (keyed by the
    /// daemon-supplied credential key) and reconnect it so it drops the cached
    /// token.
    fn logout_mcp(
        &mut self,
        servers: &[choreo_proto::McpServerStatus],
        slug: &str,
        client_tx: &crossbeam_channel::Sender<choreo_proto::ClientMessage>,
    ) {
        let key = servers
            .iter()
            .find(|s| s.slug == slug)
            .and_then(|s| s.credential_key.clone());
        let Some(key) = key else {
            self.status = Some(format!("no MCP credential for server '{slug}'"));
            return;
        };
        self.pending.send(
            client_tx,
            ClientMessageType::RemoveCredential { service: key },
        );
        self.pending.send(
            client_tx,
            ClientMessageType::McpReconnect {
                slug: slug.to_string(),
            },
        );
        self.status = Some(format!("signing out MCP server {slug}…"));
    }

    /// Apply one streamed login event. Returns whether the UI changed. On
    /// completion, sends the `AddCredential` and closes the modal; the
    /// daemon's `CredentialAdded` confirmation then drives the usual
    /// first-run follow-through.
    pub(crate) fn apply_login_event(
        &mut self,
        event: LoginEvent,
        client_tx: &crossbeam_channel::Sender<choreo_proto::ClientMessage>,
    ) -> bool {
        // Project the event's display fields first (owned), so the immutable
        // read of `self.login` ends before the mutable update.
        let (auth_url, device_code, status) = match &event {
            LoginEvent::AuthUrl { url } => {
                // `url` is the authorize request — never a code.
                (Some(url.clone()), None, None)
            }
            LoginEvent::DeviceCode { code, uri } => (None, Some((code.clone(), uri.clone())), None),
            LoginEvent::Prompt { message } | LoginEvent::Progress { message } => {
                (None, None, Some(message.clone()))
            }
            _ => (None, None, None),
        };
        self.update_login_fields(auth_url, device_code, status);
        // The flow's target slug, captured before the modal is cleared on
        // completion: an MCP sign-in's terminal `service` is a credential key
        // (`mcp:<slug>|…`), so the human-facing server name comes from here.
        let target = self.login.as_ref().map(|modal| modal.provider.clone());
        let terminal = if self.login.is_some() {
            classify_event(event)
        } else {
            None
        };
        match terminal {
            Some(LoginOutcome::Done { service, message }) => {
                self.pending.send(client_tx, *message);
                // Mirror the API-key add: a first-run session with no account
                // follows through to select this account, then open the model
                // picker, once the daemon confirms `CredentialAdded` — which an
                // OAuth `AddCredential` emits exactly like an API-key one. An
                // MCP sign-in's service is a credential key (`mcp:<slug>|…`),
                // not an account, so it takes no follow-through: the daemon
                // brings the server up itself once the grant is stored, and its
                // tools land under the `mcp/<slug>` group.
                if service.starts_with("mcp:") {
                    let slug = target.unwrap_or(service);
                    self.status = Some(format!(
                        "signed in MCP server {slug}; connecting — its tools appear under the mcp/{slug} group"
                    ));
                } else {
                    self.arm_setup_follow(&service);
                    self.status = Some(format!("signed in {service}; storing credential…"));
                }
                self.login = None;
            }
            Some(LoginOutcome::Error(message)) => {
                self.status = Some(format!("[error] {message}"));
                self.login = None;
            }
            None => {}
        }
        true
    }

    /// Update the modal's displayed fields without re-creating it.
    fn update_login_fields(
        &mut self,
        auth_url: Option<String>,
        device_code: Option<(String, String)>,
        status: Option<String>,
    ) {
        if let Some(modal) = self.login.as_mut() {
            if let Some(url) = auth_url {
                modal.auth_url = Some(url);
            }
            if let Some(device) = device_code {
                modal.device_code = Some(device);
            }
            if let Some(status) = status {
                modal.status = status;
            }
        }
    }

    /// Cancel the active login flow (Esc). Returns whether a flow was open.
    pub(crate) fn cancel_login(&mut self) -> bool {
        if let Some(modal) = self.login.take() {
            modal.cancel();
            self.status = Some("sign-in cancelled".to_string());
            true
        } else {
            false
        }
    }
}
