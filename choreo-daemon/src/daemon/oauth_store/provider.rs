//! Provider OAuth credential persistence handlers for [`DaemonState`].
//!
//! The provider refresh path ([`DaemonState::handle_save_oauth_token`]) persists
//! a rotated provider grant and refreshes its in-memory access token, and
//! [`DaemonState::handle_mark_oauth_dead`] quarantines a terminally rejected
//! provider grant. This is the provider half of `daemon/oauth_store`; the MCP
//! half lives in the sibling [`super::mcp`] module, and both share the
//! stale-rotation guard [`super::refresh_token_unchanged`]. The daemon is the
//! sole writer of every credential blob, so each handler re-encrypts to the
//! binding public key before persisting and never needs the private key.

use crate::daemon::{AccessTokenCache, DaemonState};
use crate::oauth_refresh::SaveToken;
use choreo_keystore::{OAuthKind, ServiceCredential};
use std::sync::mpsc;
use tracing::{debug, info, warn};

use super::refresh_token_unchanged;

impl DaemonState {
    /// Persist a refreshed OAuth token: update the in-memory access-token cache
    /// and, when the exchange rotated the refresh token or refreshed the
    /// identity claim, re-encrypt the updated durable credential to the binding
    /// and persist it (rotate-if-present — the stored value is kept when the
    /// exchange carried no new one).
    pub(in crate::daemon) fn handle_save_oauth_token(
        &mut self,
        save: SaveToken<'_>,
        reply: &mpsc::Sender<Result<(), String>>,
    ) {
        if self.locked {
            let _ = reply.send(Err("unlock the keystore first".to_string()));
            return;
        }
        // The account must hold an OAuth credential to rotate; an API key has
        // no refresh token and is refused rather than silently mis-updated.
        let Some(current) = self.credentials.get(save.account).cloned() else {
            let _ = reply.send(Err(format!(
                "no credential stored for account '{}'",
                save.account
            )));
            return;
        };
        // The provider refresh path rotates ONLY a provider grant: an MCP grant
        // stored under a same-named key refreshes through the MCP authorization
        // manager, so its refresh token must never be rewritten here.
        if !matches!(
            current,
            ServiceCredential::OAuth {
                kind: OAuthKind::Provider,
                ..
            }
        ) {
            let _ = reply.send(Err(format!(
                "account '{}' is not a provider OAuth credential",
                save.account
            )));
            return;
        }
        // Guard against a credential replaced under us between the load and
        // this save (a re-login or logout landing mid-exchange): the stored
        // grant is a newer one, so the access token obtained here — minted from
        // the superseded refresh token — must not overwrite the cache, and any
        // rotation must not clobber the newer grant. The guard is unconditional
        // (it must fire even for a non-rotating refresh), and
        // `refresh_token_unchanged` is true when no expectation was recorded, so
        // a save with no `expected_refresh_token` skips it.
        if !refresh_token_unchanged(&current, save.expected_refresh_token) {
            warn!(
                account = %save.account,
                "OAuth credential changed under the refresh; dropping the save"
            );
            // The newer grant owns the cache and the quarantine state: the
            // access token obtained here was minted from the superseded refresh
            // token, so do not overwrite the cache with it, and do not
            // un-quarantine the account.
            let _ = reply.send(Ok(()));
            return;
        }
        // A refresh changes the durable blob only when it carries a refresh
        // token or an identity claim that DIFFERS from the stored one — an OIDC
        // provider that re-issues an identical `id_token`, or echoes the same
        // refresh token, must not force a re-encrypt and disk write. Compare
        // VALUES, not presence; a plain expiry refresh then updates the
        // in-memory access-token cache alone.
        let stored = current.as_oauth();
        let durable_change = save
            .rotated_refresh_token
            .is_some_and(|rotated| stored.and_then(|v| v.refresh_token) != Some(rotated))
            || save
                .new_account_id
                .is_some_and(|id| stored.and_then(|v| v.account_id) != Some(id));
        if durable_change {
            let mut updated = current;
            if let ServiceCredential::OAuth {
                refresh_token,
                account_id,
                ..
            } = &mut updated
            {
                if let Some(rotated) = save.rotated_refresh_token {
                    *refresh_token = Some(rotated.to_string());
                }
                if let Some(new_id) = save.new_account_id {
                    *account_id = Some(new_id.to_string());
                }
            }
            if let Err(e) = self.encrypt_and_persist_credential(save.account, &updated) {
                let _ = reply.send(Err(e));
                return;
            }
            self.credentials.insert(save.account.to_string(), updated);
        }
        self.access_tokens.insert(
            save.account.to_string(),
            AccessTokenCache {
                access_token: save.access_token.clone(),
                expires_at: save.expires_at,
                received_at: save.received_at,
            },
        );
        // A successful refresh means the grant is alive again.
        self.dead_oauth.remove(save.account);
        info!(
            account = %save.account,
            rotated = durable_change,
            "OAuth token refreshed and persisted"
        );
        let _ = reply.send(Ok(()));
    }

    /// Quarantine an account after a terminal OAuth refresh failure and
    /// broadcast a single re-login-required notice.
    ///
    /// Idempotent by design: the broadcast fires only on the FIRST transition
    /// into the quarantined state, so concurrent sessions and retries cannot
    /// produce a duplicate re-login notice. The dead grant's cached token is
    /// dropped — it can only 401 again.
    pub(in crate::daemon) fn handle_mark_oauth_dead(&mut self, account: &str) {
        if !self.dead_oauth.insert(account.to_string()) {
            debug!(
                account,
                "OAuth account already quarantined; suppressing duplicate re-login notice"
            );
            return;
        }
        self.access_tokens.remove(account);
        info!(
            account,
            provider = %self.account_provider_slug(account).unwrap_or_default(),
            "OAuth grant terminally rejected; re-login required"
        );
        self.broadcast_oauth_relogin_required(account);
    }
}
