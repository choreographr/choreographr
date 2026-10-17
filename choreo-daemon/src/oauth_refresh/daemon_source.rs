//! The [`OAuthTokenSource`] that drives the daemon command loop over a session's
//! `daemon_tx` channel.
//!
//! The daemon owns the token cache and is the sole blob writer, so every load /
//! save / terminal report is a command round-trip: the session runs only the
//! network exchange, and the daemon persists the rotation. This is the seam the
//! "session thread performs the exchange, daemon owns the cache" split rests on.
//! Each reply rides a one-shot `std::sync::mpsc` channel (AGENTS.md: a
//! per-request reply is the sanctioned use).

use super::{OAuthTokenSource, RefreshedTokens, TokenLoad};

/// An [`OAuthTokenSource`] that drives the daemon command loop over the
/// session's `daemon_tx` channel.
pub struct DaemonTokenSource {
    daemon_tx: crossbeam_channel::Sender<crate::daemon::DaemonCommand>,
}

impl DaemonTokenSource {
    /// Wrap the session's command-loop sender.
    #[must_use]
    pub fn new(daemon_tx: crossbeam_channel::Sender<crate::daemon::DaemonCommand>) -> Self {
        Self { daemon_tx }
    }
}

impl OAuthTokenSource for DaemonTokenSource {
    fn load(&self, account: &str) -> TokenLoad {
        let (reply, rx) = std::sync::mpsc::channel();
        // A closed command channel (daemon shutting down) yields an empty load:
        // the caller sees no material and reports not-applicable rather than
        // hanging.
        let empty = || TokenLoad {
            cached: None,
            material: None,
            dead: false,
        };
        if self
            .daemon_tx
            .send(crate::daemon::DaemonCommand::LoadOAuthToken {
                account: account.to_string(),
                reply,
            })
            .is_err()
        {
            return empty();
        }
        rx.recv().unwrap_or_else(|_| empty())
    }

    fn save(
        &self,
        account: &str,
        expected_refresh_token: Option<&str>,
        tokens: &RefreshedTokens,
    ) -> Result<(), String> {
        let (reply, rx) = std::sync::mpsc::channel();
        if self
            .daemon_tx
            .send(crate::daemon::DaemonCommand::SaveOAuthToken {
                account: account.to_string(),
                access_token: tokens.access_token.clone(),
                expires_at: tokens.expires_at,
                received_at: tokens.received_at,
                rotated_refresh_token: tokens.refresh_token.clone(),
                new_account_id: tokens.account_id.clone(),
                expected_refresh_token: expected_refresh_token.map(str::to_string),
                reply,
            })
            .is_err()
        {
            return Err("daemon command channel closed".to_string());
        }
        rx.recv()
            .unwrap_or_else(|_| Err("daemon did not reply to SaveOAuthToken".to_string()))
    }

    fn report_terminal(&self, account: &str) {
        // Best-effort: the dead-marking command carries no reply, so a closed
        // channel simply loses the notice (the daemon is gone).
        let _ = self
            .daemon_tx
            .send(crate::daemon::DaemonCommand::MarkOAuthDead {
                account: account.to_string(),
            });
    }
}
