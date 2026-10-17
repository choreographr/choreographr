//! OAuth credential persistence handlers for [`DaemonState`] — the
//! `DaemonCommand` arms that persist a refreshed provider or MCP OAuth grant,
//! quarantine a terminally rejected grant, and keep the MCP OAuth snapshot the
//! connect path reads.
//!
//! This is the OAuth-lifecycle sibling of `daemon/keystore.rs`, which owns the
//! unlock/bind/lock lifecycle. Both run on the command loop and share the single
//! blob-write path `encrypt_and_persist_credential` (defined in `keystore.rs`)
//! and the daemon's `credentials` / `access_tokens` / `dead_oauth` state;
//! splitting them keeps each module about one lifecycle.
//!
//! The daemon is the sole writer of every credential blob, so each handler here
//! re-encrypts the updated durable credential to the binding public key before
//! persisting it, and never needs the private key (the blob is a sealed box).

use super::DaemonState;
use crate::db;
use crate::oauth_refresh::SaveToken;
use choreo_keystore::ServiceCredential;
use std::sync::mpsc;
use tracing::{debug, info, warn};

impl DaemonState {
    /// Persist a refreshed OAuth token: update the in-memory access-token cache
    /// and, when the exchange rotated the refresh token or refreshed the
    /// identity claim, re-encrypt the updated durable credential to the binding
    /// and persist it (rotate-if-present — the stored value is kept when the
    /// exchange carried no new one).
    pub(super) fn handle_save_oauth_token(
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
        if !matches!(current, ServiceCredential::OAuth { .. }) {
            let _ = reply.send(Err(format!(
                "account '{}' is not an OAuth credential",
                save.account
            )));
            return;
        }
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
        self.access_tokens.insert(
            save.account.to_string(),
            super::AccessTokenCache {
                access_token: save.access_token.clone(),
                expires_at: save.expires_at,
                received_at: save.received_at,
            },
        );
        // A successful refresh means the grant is alive again.
        self.dead_oauth.remove(save.account);
        info!(
            account = %save.account,
            rotated = save.rotated_refresh_token.is_some(),
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
    pub(super) fn handle_mark_oauth_dead(&mut self, account: &str) {
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

    /// Persist a rotated MCP OAuth credential (a fire-and-forget
    /// [`DaemonCommand::OAuthSave`](crate::daemon::DaemonCommand::OAuthSave)).
    ///
    /// Re-encrypts the updated durable subset to the binding public key and
    /// persists it (the daemon is the sole blob writer; the access token was
    /// stripped by the adapter), then updates the in-memory map and the MCP
    /// OAuth snapshot, dropping any cached token or quarantine for the key.
    pub(super) fn handle_oauth_save(&mut self, key: &str, credential: ServiceCredential) {
        if self.locked {
            warn!(%key, "OAuthSave ignored: keystore is locked");
            return;
        }
        if let Err(e) = self.encrypt_and_persist_credential(key, &credential) {
            warn!(%key, error = %e, "failed to persist rotated MCP OAuth credential");
            return;
        }
        self.credentials.insert(key.to_string(), credential);
        self.access_tokens.remove(key);
        self.dead_oauth.remove(key);
        self.publish_mcp_oauth_snapshot();
        // The MCP refresh completed (this save is its terminal step), so prune
        // any now-idle single-flight locks the refresh registry is holding for
        // this or another credential.
        crate::oauth_refresh_locks::prune();
        info!(%key, "persisted rotated MCP OAuth credential");
    }

    /// Delete a stored MCP OAuth credential (a fire-and-forget
    /// [`DaemonCommand::OAuthClear`](crate::daemon::DaemonCommand::OAuthClear)).
    pub(super) fn handle_oauth_clear(&mut self, key: &str) {
        if let Err(e) = db::remove_credential_blob(&self.db, key) {
            warn!(%key, error = %e, "failed to remove MCP OAuth credential");
            return;
        }
        self.credentials.remove(key);
        self.access_tokens.remove(key);
        self.dead_oauth.remove(key);
        self.publish_mcp_oauth_snapshot();
        crate::oauth_refresh_locks::prune();
        info!(%key, "cleared MCP OAuth credential");
    }

    /// Quarantine an MCP credential after a terminal refresh rejection (a
    /// fire-and-forget
    /// [`DaemonCommand::McpAuthRejected`](crate::daemon::DaemonCommand::McpAuthRejected)),
    /// so the `/mcp` status shows a re-authentication request.
    pub(super) fn handle_mcp_auth_rejected(&mut self, key: &str) {
        if self.dead_oauth.insert(key.to_string()) {
            self.publish_mcp_oauth_snapshot();
            info!(%key, "MCP OAuth grant terminally rejected; re-authentication required");
        }
    }

    /// Republish the MCP OAuth snapshot — the read view the connect path uses
    /// so a credential load never round-trips the (possibly blocked) command
    /// loop. The command loop is the snapshot's sole writer; it calls this
    /// after every mutation of the MCP-keyed credentials.
    pub(super) fn publish_mcp_oauth_snapshot(&self) {
        #[cfg(feature = "mcp")]
        if let Some(runtime) = crate::mcp::oauth::runtime() {
            let creds: std::collections::HashMap<String, ServiceCredential> = self
                .credentials
                .iter()
                .filter(|(k, _)| k.starts_with("mcp:"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            runtime.publish(creds);
        }
        // Without the `mcp` feature there is no connect path to feed.
        #[cfg(not(feature = "mcp"))]
        let _ = self;
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::make_daemon_state;
    use super::*;
    use choreo_shared::OAuthKind;

    fn mcp_oauth_credential(refresh: &str) -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: Some("https://as.example".into()),
            resource: Some("https://example.com/mcp".into()),
            authorization_server: Some("https://as.example".into()),
            refresh_token: Some(refresh.to_string()),
            account_id: None,
            client_id: Some("client-1".into()),
            client_secret: None,
            granted_scopes: vec!["openid".into()],
            token_type: Some("Bearer".into()),
        }
    }

    #[test]
    fn oauth_save_is_refused_while_locked() {
        let (mut state, _rx) = make_daemon_state();
        assert!(state.locked);
        state.handle_oauth_save("mcp:docs|abc", mcp_oauth_credential("r"));
        // A locked daemon holds no cleartext: the save is dropped, not published.
        assert!(state.credentials.is_empty());
    }

    #[test]
    fn mcp_auth_rejected_quarantines_idempotently() {
        let (mut state, _rx) = make_daemon_state();
        assert!(!state.dead_oauth.contains("mcp:docs|abc"));
        state.handle_mcp_auth_rejected("mcp:docs|abc");
        assert!(state.dead_oauth.contains("mcp:docs|abc"));
        // A second rejection is a no-op.
        state.handle_mcp_auth_rejected("mcp:docs|abc");
        assert!(state.dead_oauth.contains("mcp:docs|abc"));
    }

    #[test]
    fn auth_state_mapping_refines_from_the_store() {
        let (mut state, _rx) = make_daemon_state();
        let key = "mcp:docs|abc".to_string();
        let status = crate::mcp::McpServerStatus {
            slug: "docs".into(),
            tier: "daemon".into(),
            transport: "http".into(),
            target: "https://example.com/mcp".into(),
            connected: false,
            tool_count: 0,
            server_name: None,
            server_version: None,
            last_error: None,
            credential_key: Some(key.clone()),
            oauth: Some(choreo_proto::McpOAuthConfig {
                client_id: None,
                callback_port: None,
                scopes: vec![],
            }),
            auth: choreo_proto::McpAuthState::NotLoggedIn,
        };
        // No stored credential: stays NotLoggedIn.
        assert_eq!(
            state.mcp_auth_state(&status),
            choreo_proto::McpAuthState::NotLoggedIn
        );
        // A stored grant: OAuth.
        state
            .credentials
            .insert(key.clone(), mcp_oauth_credential("r"));
        assert_eq!(
            state.mcp_auth_state(&status),
            choreo_proto::McpAuthState::OAuth
        );
        // A terminal rejection overrides to Reauthentication.
        state.dead_oauth.insert(key);
        assert_eq!(
            state.mcp_auth_state(&status),
            choreo_proto::McpAuthState::Reauthentication
        );
    }
}
