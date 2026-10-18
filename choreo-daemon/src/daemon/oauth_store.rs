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
use choreo_keystore::{OAuthKind, ServiceCredential};
use std::sync::mpsc;
use tracing::{debug, info, warn};

/// Whether the stored grant's refresh token still equals the one a refresh
/// presented (or no expectation was recorded). Shared by the provider and MCP
/// save handlers so their stale-rotation guards cannot drift.
fn refresh_token_unchanged(stored: &ServiceCredential, expected: Option<&str>) -> bool {
    expected.is_none_or(|expected| {
        stored.as_oauth().and_then(|view| view.refresh_token) == Some(expected)
    })
}

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
    pub(super) fn handle_oauth_save(
        &mut self,
        key: &str,
        credential: ServiceCredential,
        expected_refresh_token: Option<&str>,
    ) {
        if self.locked {
            warn!(%key, "OAuthSave ignored: keystore is locked");
            return;
        }
        // A save is always the terminal step of a refresh of a credential the
        // connect path just loaded, so the key must still be present. The save
        // is fire-and-forget (an in-flight refresh cannot round-trip the command
        // loop), so a save enqueued just before a logout/clear can be processed
        // AFTER the credential was removed; persisting it then would resurrect a
        // credential the user just deleted. Drop it instead.
        if !self.credentials.contains_key(key) {
            warn!(%key, "OAuthSave dropped: credential already removed (logout/clear)");
            // The connect path published this rotation into the read-view
            // snapshot before enqueuing (that ordering is what lets a concurrent
            // refresh re-read it). Restore the snapshot from the durable store —
            // whose credential was just removed — so the read view does not keep
            // serving a grant the user deleted.
            self.publish_mcp_oauth_snapshot();
            return;
        }
        // Guard against a credential replaced under the refresh (a re-login
        // landing between the connect-path load and this save): the stored grant
        // is newer, so persisting the rotated refresh token would clobber it.
        // Only apply the save when the stored refresh token is still the one the
        // refresh presented.
        let changed = match self.credentials.get(key) {
            Some(current) => !refresh_token_unchanged(current, expected_refresh_token),
            None => false,
        };
        if changed {
            warn!(%key, "OAuthSave dropped: credential changed under the refresh");
            // As above: the connect path already advanced the read-view snapshot
            // to this superseded rotation, so republish from the durable store to
            // put the newer grant back.
            self.publish_mcp_oauth_snapshot();
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

    /// The MCP-keyed credentials the connect path may see: every `mcp:`-keyed
    /// credential that is not quarantined. A terminally rejected grant
    /// (`dead_oauth`) is withheld — the server already refused that refresh
    /// token, so serving it again would only re-spend a single-use token and
    /// fail again. Kept separate from [`Self::publish_mcp_oauth_snapshot`] so
    /// the filter is unit-testable without the process-global runtime.
    #[cfg(feature = "mcp")]
    fn mcp_oauth_snapshot(&self) -> std::collections::HashMap<String, ServiceCredential> {
        self.credentials
            .iter()
            .filter(|(k, _)| {
                choreo_mcp::is_mcp_credential_key(k) && !self.dead_oauth.contains(k.as_str())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Republish the MCP OAuth snapshot — the read view the connect path uses
    /// so a credential load never round-trips the (possibly blocked) command
    /// loop. The command loop is the snapshot's sole writer; it calls this
    /// after every mutation of the MCP-keyed credentials.
    ///
    /// A quarantined grant is withheld (see [`Self::mcp_oauth_snapshot`]): the
    /// connect then proceeds unauthenticated and surfaces a re-login. The `/mcp`
    /// status reads `dead_oauth` directly, so the quarantine is still reported
    /// there even though the snapshot omits the credential.
    pub(super) fn publish_mcp_oauth_snapshot(&self) {
        #[cfg(feature = "mcp")]
        if let Some(runtime) = crate::mcp::oauth::runtime() {
            runtime.publish(self.mcp_oauth_snapshot());
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
    use crate::daemon::DaemonCommand;
    use zeroize::Zeroizing;

    fn mcp_oauth_credential(refresh: &str) -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: Some("https://as.example".into()),
            resource: Some("https://example.com/mcp".into()),
            authorization_server: Some("https://as.example".into()),
            token_endpoint: None,
            refresh_encoding: None,
            refresh_token: Some(refresh.to_string()),
            account_id: None,
            client_id: Some("client-1".into()),
            client_secret: None,
            granted_scopes: vec!["openid".into()],
            token_type: Some("Bearer".into()),
        }
    }

    fn provider_oauth_credential(refresh: &str) -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: OAuthKind::Provider,
            issuer: Some("https://auth.example".into()),
            resource: None,
            authorization_server: None,
            token_endpoint: Some("https://auth.example/token".into()),
            refresh_encoding: Some(choreo_keystore::OAuthGrantEncoding::Json),
            refresh_token: Some(refresh.to_string()),
            account_id: None,
            client_id: None,
            client_secret: None,
            granted_scopes: Vec::new(),
            token_type: Some("Bearer".into()),
        }
    }

    /// Bind the keystore so a durable write can actually persist.
    fn bind_for_test(state: &mut DaemonState) {
        let key: [u8; 32] = [9u8; 32];
        let (ack, ack_rx) = mpsc::channel();
        state.handle_command(DaemonCommand::BindKeystore {
            key: key.to_vec(),
            reply: None,
            ack,
        });
        ack_rx.recv().unwrap();
    }

    #[test]
    fn oauth_save_is_refused_while_locked() {
        let (mut state, _rx) = make_daemon_state();
        assert!(state.locked);
        state.handle_oauth_save("mcp:docs|abc", mcp_oauth_credential("r"), None);
        // A locked daemon holds no cleartext: the save is dropped, not published.
        assert!(state.credentials.is_empty());
    }

    #[test]
    fn oauth_save_is_dropped_when_the_credential_was_removed() {
        let (mut state, _rx) = make_daemon_state();
        state.locked = false;
        // No credential stored for the key: the save is a late write landing
        // after a logout/clear, so it must not resurrect the credential.
        state.handle_oauth_save("mcp:docs|abc", mcp_oauth_credential("r"), Some("r"));
        assert!(state.credentials.is_empty());
        assert!(
            db::get_all_credential_blobs(&state.db)
                .expect("read blobs")
                .is_empty(),
            "nothing durable must be written for a credential that was removed"
        );
    }

    #[test]
    fn oauth_save_guard_applies_only_onto_the_presented_credential() {
        let (mut state, _rx) = make_daemon_state();
        bind_for_test(&mut state);
        let key = "mcp:docs|abc";
        state
            .credentials
            .insert(key.into(), mcp_oauth_credential("old"));

        // A stale expected token (a credential replaced by a re-login mid-
        // refresh) drops the save rather than clobbering the newer grant.
        state.handle_oauth_save(key, mcp_oauth_credential("new-A"), Some("stale"));
        assert_eq!(
            state
                .credentials
                .get(key)
                .and_then(ServiceCredential::as_oauth)
                .and_then(|v| v.refresh_token),
            Some("old"),
            "the newer grant must be preserved"
        );

        // The presented token matches the stored one → the rotation is applied.
        state.handle_oauth_save(key, mcp_oauth_credential("new-B"), Some("old"));
        assert_eq!(
            state
                .credentials
                .get(key)
                .and_then(ServiceCredential::as_oauth)
                .and_then(|v| v.refresh_token),
            Some("new-B"),
            "a matching save rotates the durable grant"
        );
    }

    #[test]
    fn save_oauth_token_guard_rotates_only_onto_the_loaded_credential() {
        let (mut state, _rx) = make_daemon_state();
        bind_for_test(&mut state);
        state
            .credentials
            .insert("acct".into(), provider_oauth_credential("old"));
        let access = Zeroizing::new("at".to_string());

        // A mismatching expected token means the credential was replaced by a
        // re-login mid-refresh. The durable rotation is dropped AND the cache is
        // left alone — the fresh token was minted from the superseded grant.
        let (reply, rx) = mpsc::channel();
        state.handle_save_oauth_token(
            SaveToken {
                account: "acct",
                access_token: &access,
                expires_at: None,
                received_at: 1,
                rotated_refresh_token: Some("rotated-A"),
                new_account_id: None,
                expected_refresh_token: Some("stale"),
            },
            &reply,
        );
        assert!(rx.recv().expect("a reply").is_ok());
        assert_eq!(
            state
                .credentials
                .get("acct")
                .and_then(ServiceCredential::as_oauth)
                .and_then(|v| v.refresh_token),
            Some("old"),
            "the superseded grant must not be clobbered"
        );
        assert!(
            !state.access_tokens.contains_key("acct"),
            "the cache must not adopt a token from the superseded grant"
        );

        // The matching expected token rotates the durable grant.
        let (reply, rx) = mpsc::channel();
        state.handle_save_oauth_token(
            SaveToken {
                account: "acct",
                access_token: &access,
                expires_at: None,
                received_at: 2,
                rotated_refresh_token: Some("rotated-B"),
                new_account_id: None,
                expected_refresh_token: Some("old"),
            },
            &reply,
        );
        assert!(rx.recv().expect("a reply").is_ok());
        assert_eq!(
            state
                .credentials
                .get("acct")
                .and_then(ServiceCredential::as_oauth)
                .and_then(|v| v.refresh_token),
            Some("rotated-B")
        );
    }

    #[test]
    fn save_oauth_token_drops_a_stale_non_rotating_refresh() {
        // A credential replaced mid-exchange (stored "old" != the presented
        // "stale") whose refresh carried NO rotation must still be dropped: the
        // access token was minted from the superseded refresh token, so it must
        // not reach the cache even though nothing durable would have changed.
        let (mut state, _rx) = make_daemon_state();
        bind_for_test(&mut state);
        state
            .credentials
            .insert("acct".into(), provider_oauth_credential("old"));
        let access = Zeroizing::new("at".to_string());
        let (reply, rx) = mpsc::channel();
        state.handle_save_oauth_token(
            SaveToken {
                account: "acct",
                access_token: &access,
                expires_at: Some(9_999),
                received_at: 1,
                rotated_refresh_token: None,
                new_account_id: None,
                expected_refresh_token: Some("stale"),
            },
            &reply,
        );
        assert!(rx.recv().expect("a reply").is_ok());
        assert!(
            !state.access_tokens.contains_key("acct"),
            "a superseded grant's access token must not reach the cache"
        );
        assert!(
            db::get_all_credential_blobs(&state.db)
                .expect("read blobs")
                .is_empty()
        );
    }

    #[test]
    fn save_oauth_token_skips_the_reencrypt_when_nothing_durable_changed() {
        // An OIDC provider that echoes the SAME refresh token must not force a
        // re-encrypt: the fresh access token is cached, but the durable grant is
        // left untouched (no blob write).
        let (mut state, _rx) = make_daemon_state();
        bind_for_test(&mut state);
        state
            .credentials
            .insert("acct".into(), provider_oauth_credential("same"));
        let access = Zeroizing::new("at".to_string());
        let (reply, rx) = mpsc::channel();
        state.handle_save_oauth_token(
            SaveToken {
                account: "acct",
                access_token: &access,
                expires_at: Some(9_999),
                received_at: 1,
                rotated_refresh_token: Some("same"),
                new_account_id: None,
                expected_refresh_token: Some("same"),
            },
            &reply,
        );
        assert!(rx.recv().expect("a reply").is_ok());
        assert_eq!(
            state
                .access_tokens
                .get("acct")
                .map(|c| c.access_token.as_str()),
            Some("at"),
            "the fresh access token is still cached"
        );
        assert!(
            db::get_all_credential_blobs(&state.db)
                .expect("read blobs")
                .is_empty(),
            "an unchanged refresh token must not re-encrypt the durable grant"
        );
    }

    #[test]
    fn save_oauth_token_rejects_a_non_provider_kind() {
        let (mut state, _rx) = make_daemon_state();
        state.locked = false;
        // An MCP-kind grant under a same-named key must not be rotated by the
        // provider refresh path.
        state
            .credentials
            .insert("acct".into(), mcp_oauth_credential("r"));
        let access = Zeroizing::new("at".to_string());
        let (reply, rx) = mpsc::channel();
        state.handle_save_oauth_token(
            SaveToken {
                account: "acct",
                access_token: &access,
                expires_at: None,
                received_at: 0,
                rotated_refresh_token: None,
                new_account_id: None,
                expected_refresh_token: None,
            },
            &reply,
        );
        assert!(rx.recv().expect("a reply").is_err());
        // The MCP grant is left untouched.
        assert!(matches!(
            state.credentials.get("acct"),
            Some(ServiceCredential::OAuth {
                kind: OAuthKind::Mcp,
                ..
            })
        ));
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

    #[cfg(feature = "mcp")]
    #[test]
    fn snapshot_withholds_a_quarantined_grant_but_keeps_live_ones() {
        let (mut state, _rx) = make_daemon_state();
        let live = "mcp:docs|live".to_string();
        let dead = "mcp:docs|dead".to_string();
        state
            .credentials
            .insert(live.clone(), mcp_oauth_credential("r-live"));
        state
            .credentials
            .insert(dead.clone(), mcp_oauth_credential("r-dead"));
        // A provider-keyed credential is never part of the MCP snapshot.
        state.credentials.insert(
            "acct-provider".to_string(),
            mcp_oauth_credential("r-provider"),
        );
        state.dead_oauth.insert(dead.clone());

        let snapshot = state.mcp_oauth_snapshot();
        assert!(snapshot.contains_key(&live), "a live MCP grant is exposed");
        assert!(
            !snapshot.contains_key(&dead),
            "a quarantined grant is withheld so the connect surfaces a re-login"
        );
        assert!(
            !snapshot.contains_key("acct-provider"),
            "only `mcp:`-keyed credentials belong in the MCP snapshot"
        );
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
                authorization_server: None,
                client_secret_env: None,
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

    #[cfg(feature = "mcp")]
    #[test]
    fn a_dropped_oauth_save_restores_the_read_view_snapshot() {
        // Install the process-global runtime so `publish_mcp_oauth_snapshot` has
        // a snapshot to write; the command sender is unused by these handlers.
        // `install` is a `OnceLock`, and under nextest each test is its own
        // process, so this test owns the runtime.
        let (tx, _tx_rx) = crossbeam_channel::unbounded();
        let runtime = crate::mcp::oauth::install(tx);

        let (mut state, _rx) = make_daemon_state();
        state.locked = false;
        let key = "mcp:docs|drop";

        // The connect path published a rotation into the read view, but the
        // durable credential was already removed, so the save is dropped — the
        // snapshot must be restored to the durable truth (key absent) rather
        // than keep serving the rotation.
        runtime.publish_credential(key, mcp_oauth_credential("rotated"));
        state.handle_oauth_save(key, mcp_oauth_credential("rotated"), Some("rotated"));
        assert!(
            runtime.load(key).is_none(),
            "a save dropped after removal must restore the read view"
        );

        // A save dropped because a re-login replaced the credential likewise
        // republishes the newer stored grant, not the stale rotation.
        state
            .credentials
            .insert(key.into(), mcp_oauth_credential("newer"));
        runtime.publish_credential(key, mcp_oauth_credential("stale"));
        state.handle_oauth_save(key, mcp_oauth_credential("stale"), Some("old"));
        assert_eq!(
            runtime.load(key),
            Some(mcp_oauth_credential("newer")),
            "the read view must show the newer stored grant, not the stale rotation"
        );
    }
}
