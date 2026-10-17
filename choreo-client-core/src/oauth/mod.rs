//! Provider OAuth login library.
//!
//! This module turns a provider's OAuth sign-in parameters (an
//! [`OAuthProviderDescriptor`] the daemon serves) plus a few options into a
//! ready-to-store [`ServiceCredential`](choreo_keystore::ServiceCredential) and
//! the [`ClientMessageType::AddCredential`] frame that carries it. It runs the
//! frontend half of the flow — PKCE S256, a
//! loopback callback server, or an RFC 8628 device-code poll for headless hosts
//! — and hands the durable result (refresh token + client registration +
//! extracted identity claims) back to the caller. The access token is derived
//! and ephemeral; it is deliberately dropped here and never placed in the
//! credential (the daemon mints a fresh one when it needs it).
//!
//! The interaction runs on a **worker thread** and streams progress over a
//! crossbeam channel as [`LoginEvent`]s, so a UI can render a cancelable modal
//! (the auth URL, a device code, a prompt) without blocking its own loop.
//! Cancellation is a single lock-free [`AtomicBool`] the loopback accept loop
//! polls (a blocking accept cannot be interrupted by a channel message), plus a
//! stop channel for the device-code wait.
//!
//! The module is split by responsibility: this file owns the public types (the
//! [`OAuthLoginError`] taxonomy, [`LoginOptions`], the [`LoginEvent`] stream, and
//! the [`LoginSession`] handle) and the worker-thread driver ([`start_login`] /
//! `run`); the private `flow` module owns the two interactive flows (loopback
//! PKCE and RFC 8628 device-code); the private `credential` module builds the
//! durable [`ServiceCredential`](choreo_keystore::ServiceCredential) and its
//! `AddCredential` frame from the exchanged tokens; the private `mcp` module
//! (split into `mcp::{mod,discovery,register,authorize}`) owns the remote
//! MCP-server sign-in — RFC 9728/8414 discovery, dynamic client registration,
//! and the same loopback PKCE flow. The `pkce` and `http` modules are the small
//! primitives those shells compose.
//!
//! The render crates never build an OAuth credential themselves: this library
//! returns the `AddCredential` message so the credential's shape stays in one
//! place.

mod callback;
mod credential;
mod flow;
mod http;
mod mcp;
mod pkce;

pub use choreo_shared::oauth_claims::{chatgpt_account_id, decode_claims};
pub use mcp::{McpLoginInfo, McpLoginOptions, start_mcp_login};
pub use pkce::{challenge_s256, generate_state, generate_verifier, state_matches};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;

use choreo_proto::{ClientMessageType, OAuthProviderDescriptor};
use crossbeam_channel::{Receiver, Sender};
use tracing::warn;

use flow::{run_device, run_loopback};

/// A failure from the login flow. Each variant is a distinct recovery class
/// (bind failure vs. CSRF mismatch vs. provider error), so the UI can hint at
/// the right next step rather than surfacing one generic message.
#[derive(Debug, thiserror::Error)]
pub enum OAuthLoginError {
    /// The descriptor had no usable client id (a borrowed id withheld because
    /// the opt-in is off, or a provider that requires dynamic registration).
    #[error("no client id for this provider: {0}")]
    NoClientId(String),
    /// The provider does not offer the requested flow (device-code asked of a
    /// provider without one).
    #[error("provider does not support this login flow: {0}")]
    Unsupported(String),
    /// The loopback callback server could not bind any of the configured
    /// ports.
    #[error("could not bind the loopback callback server: {0}")]
    CallbackBind(String),
    /// The loopback accept loop failed.
    #[error("callback server error: {0}")]
    Callback(String),
    /// The `state` returned by the callback did not match the one sent — a
    /// possible CSRF attempt; the code is never exchanged.
    #[error("OAuth state mismatch (possible CSRF); login aborted")]
    StateMismatch,
    /// The flow was cancelled by the caller.
    #[error("login cancelled")]
    Cancelled,
    /// A URL was not `https` and not a loopback `http` address.
    #[error("refusing an insecure OAuth endpoint: {0}")]
    InsecureEndpoint(String),
    /// A network-level failure talking to the provider.
    #[error("network error: {0}")]
    Network(String),
    /// The provider's response could not be decoded.
    #[error("malformed provider response: {0}")]
    Decode(String),
    /// The provider returned an OAuth error (`error`/`error_description`).
    #[error("provider error: {error}: {description}")]
    TokenEndpoint {
        /// The OAuth `error` code.
        error: String,
        /// The optional `error_description`.
        description: String,
    },
    /// The device-code flow expired or was denied.
    #[error("device-code flow ended: {0}")]
    DeviceFlow(String),
    /// The `--callback-url` value could not be parsed into a code/state.
    #[error("could not parse the callback URL: {0}")]
    CallbackUrl(String),
}

/// Options controlling how the sign-in runs.
#[derive(Debug, Clone)]
pub struct LoginOptions {
    /// The daemon account name the credential is stored under (the
    /// `AddCredential` `service`). The account must already exist.
    pub account: String,
    /// Never open a browser; emit the authorize URL for the caller to surface.
    pub no_browser: bool,
    /// Emit the authorize URL (in addition to any browser open) so a remote/SSH
    /// user can open it elsewhere. The TUI always wants the URL, so this is a
    /// hint for a CLI that would otherwise stay silent.
    pub print_auth_url: bool,
    /// A full callback URL (or an authorize response URL) pasted by the user
    /// for the paste-URL path: when set, the flow parses the code/state from it
    /// instead of waiting on the loopback server.
    pub callback_url: Option<String>,
    /// Use the RFC 8628 device-code flow (headless) instead of loopback.
    pub device: bool,
}

/// A progress event streamed from the login worker thread.
#[derive(Debug)]
pub enum LoginEvent {
    /// The authorize URL to display (and possibly open). Never contains a
    /// `code` — only the request parameters.
    AuthUrl {
        /// The full authorization URL.
        url: String,
    },
    /// A device-code prompt to display: the short `code` the user enters at
    /// `uri`.
    DeviceCode {
        /// The short user code.
        code: String,
        /// The verification URL the user visits.
        uri: String,
    },
    /// A free-text prompt for the user (e.g. "paste the callback URL").
    Prompt {
        /// The prompt text.
        message: String,
    },
    /// A free-text progress note.
    Progress {
        /// The note.
        message: String,
    },
    /// The flow finished: `message` is the `AddCredential` request to send. The
    /// credential is boxed because it is large.
    Done {
        /// The account name the credential belongs to.
        service: String,
        /// The ready-to-send request.
        message: Box<ClientMessageType>,
    },
    /// The flow failed. `message` is human-readable and carries no secrets.
    Error {
        /// The failure reason.
        message: String,
    },
}

/// A running login flow: the event stream, a cancel handle, and the worker
/// join handle.
pub struct LoginSession {
    /// Events from the worker thread, oldest first. The stream ends when the
    /// worker finishes.
    pub events: Receiver<LoginEvent>,
    cancel: Arc<AtomicBool>,
    /// The device-code wait's stop channel; `None` for the MCP flow, which has
    /// no interval wait and cancels through the cancel flag alone. Optional so a
    /// flow without a stop channel carries none rather than a sender whose
    /// receiver was already dropped.
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl LoginSession {
    /// Request cancellation. Best-effort: the loopback accept loop observes it
    /// within one poll interval, the device-code wait immediately.
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // The device wait selects on this channel so a cancel is observed
        // without waiting out the poll interval. A flow with no interval wait
        // (MCP) carries no stop channel and cancels on the flag alone.
        if let Some(stop) = &self.stop {
            let _ = stop.send(());
        }
    }

    /// Block until the worker thread finishes.
    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Start the interactive sign-in for `descriptor` on a worker thread.
///
/// Returns immediately with a [`LoginSession`]; progress is streamed on
/// `session.events`. The caller drives the resulting `AddCredential` message
/// over its normal request path on `LoginEvent::Done`.
#[must_use]
pub fn start_login(descriptor: OAuthProviderDescriptor, options: LoginOptions) -> LoginSession {
    let (tx, rx) = crossbeam_channel::unbounded();
    let (stop, stop_rx) = crossbeam_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = Arc::clone(&cancel);
    // A clone goes to the worker so the original `tx` stays available for the
    // thread-spawn-failure path below; both drop when the login ends.
    let worker_tx = tx.clone();
    // The descriptor and options are owned by the worker for its whole life;
    // nothing is shared back except the event channel and the cancel flag.
    let handle = std::thread::Builder::new()
        .name("oauth-login".to_string())
        .spawn(move || {
            let result = run(&descriptor, &options, &worker_tx, &cancel_worker, &stop_rx);
            match result {
                Ok(()) => {}
                Err(error) => {
                    let message = error.to_string();
                    // Never log the raw error verbatim if it could carry a URL
                    // with a code; the error strings here are already
                    // code-free, but keep the log to a kind tag.
                    warn!("oauth login failed: {message}");
                    let _ = worker_tx.send(LoginEvent::Error { message });
                }
            }
        });
    let handle = match handle {
        Ok(handle) => Some(handle),
        Err(e) => {
            // Thread spawn failure is terminal: report it on the stream so the
            // caller's event handler surfaces it, and drop the join handle.
            let _ = tx.send(LoginEvent::Error {
                message: format!("could not start the login worker: {e}"),
            });
            None
        }
    };
    LoginSession {
        events: rx,
        cancel,
        stop: Some(stop),
        handle,
    }
}

/// The worker entry point: dispatch to the device or loopback flow and emit
/// `Done`.
fn run(
    descriptor: &OAuthProviderDescriptor,
    options: &LoginOptions,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
    stop: &Receiver<()>,
) -> Result<(), OAuthLoginError> {
    // Both flows return the tokens plus the client id to persist, so `finish`
    // stays one path: the loopback flow may adopt a client id the provider
    // returned on the callback, and the device flow uses the descriptor's.
    let (tokens, client_id) = if options.device {
        run_device(descriptor, tx, cancel, stop)?
    } else {
        run_loopback(descriptor, options, tx, cancel)?
    };
    credential::finish(descriptor, options, tx, &tokens, &client_id)
}
