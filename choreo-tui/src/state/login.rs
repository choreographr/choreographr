//! The OAuth login modal: the state that renders a `/login` flow and drives
//! the shared login library.
//!
//! A `/login <account>` command resolves the account to its provider, starts
//! the `choreo_client_core` login worker for that provider's descriptor, and
//! opens this modal. The modal renders the streamed progress (the authorize
//! URL, a device code, prompts) and, on completion, the caller sends the
//! `AddCredential` message the library built. Cancelling the modal (Esc) trips
//! the login worker's cancel flag.

use choreo_client_core::{LoginEvent, LoginOptions, LoginSession, start_login};
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
            print_auth_url: false,
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

/// Drop cancels the worker so a modal closed mid-flow (a quit, or a replaced
/// modal) never leaves the login thread, its browser, or its loopback server
/// running.
impl Drop for LoginModal {
    fn drop(&mut self) {
        self.cancel();
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
                // OAuth `AddCredential` emits exactly like an API-key one.
                self.arm_setup_follow(&service);
                self.status = Some(format!("signed in {service}; storing credential…"));
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
