//! OAuth credential persistence handlers for [`DaemonState`] — the
//! `DaemonCommand` arms that persist a refreshed provider or MCP OAuth grant,
//! quarantine a terminally rejected grant, and keep the MCP OAuth snapshot the
//! connect path reads.
//!
//! This is the OAuth-lifecycle sibling of `daemon/keystore.rs`, which owns the
//! unlock/bind/lock lifecycle. Both run on the command loop and share the single
//! blob-write path `encrypt_and_persist_credential` (defined in `keystore.rs`)
//! and the daemon's `credentials` / `access_tokens` / `dead_oauth` state.
//!
//! The handlers are split by lifecycle: [`provider`] owns the provider refresh
//! save and terminal quarantine, [`mcp`] owns the MCP credential save/clear and
//! terminal rejection. This file keeps the shared stale-rotation guard
//! ([`refresh_token_unchanged`]) and the MCP read-view publication
//! ([`DaemonState::publish_mcp_oauth_key`] /
//! [`DaemonState::publish_mcp_oauth_snapshot`]). The daemon is the sole writer of
//! every credential blob, so each handler re-encrypts the updated durable
//! credential to the binding public key before persisting it, and never needs
//! the private key (the blob is a sealed box).

use super::DaemonState;
use choreo_keystore::ServiceCredential;

mod mcp;
mod provider;

/// Whether the stored grant's refresh token still equals the one a refresh
/// presented (or no expectation was recorded). Shared by the provider and MCP
/// save handlers so their stale-rotation guards cannot drift.
fn refresh_token_unchanged(stored: &ServiceCredential, expected: Option<&str>) -> bool {
    expected.is_none_or(|expected| {
        stored.as_oauth().and_then(|view| view.refresh_token) == Some(expected)
    })
}

impl DaemonState {
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

    /// Publish one key's current durable value into the MCP OAuth read view,
    /// leaving every other key's entry untouched.
    ///
    /// A single-key mutation must NOT republish the whole map: the connect path
    /// advances its own key's entry synchronously, ahead of the fire-and-forget
    /// `OAuthSave` that makes it durable (`OAuthSave` is processed later by the
    /// command loop). A whole-map republish built from `self.credentials` would
    /// therefore revert another key whose rotation is visible but not yet
    /// durable back to its pre-rotation (already-spent) refresh token. Updating
    /// only this key keeps a rotation visible for its own key without
    /// clobbering a pending rotation for any other.
    ///
    /// A non-MCP key, a quarantined key, or an absent credential removes the
    /// entry — it must not be served (a non-MCP key never belongs in this
    /// snapshot anyway, but gating on the key shape keeps the intent explicit).
    pub(super) fn publish_mcp_oauth_key(&self, key: &str) {
        #[cfg(feature = "mcp")]
        {
            // Only `mcp:`-keyed credentials belong in this read view; leave the
            // snapshot untouched for any other key.
            if !choreo_mcp::is_mcp_credential_key(key) {
                return;
            }
            let Some(runtime) = crate::mcp::oauth::runtime() else {
                return;
            };
            // A live durable grant is published; a quarantined or absent one is
            // removed so a connect never re-sends a terminally rejected token
            // (it surfaces a re-login instead).
            match self.credentials.get(key) {
                Some(credential) if !self.dead_oauth.contains(key) => {
                    runtime.publish_credential(key, credential.clone());
                }
                _ => runtime.remove_credential(key),
            }
        }
        // Without the `mcp` feature there is no connect path to feed.
        #[cfg(not(feature = "mcp"))]
        let _ = key;
    }

    /// Republish the whole MCP OAuth snapshot — the read view the connect path
    /// uses so a credential load never round-trips the (possibly blocked)
    /// command loop. Reserved for the two wholesale changes of the credential
    /// set (lock clears it, unlock decrypts it afresh); every per-key mutation
    /// uses [`Self::publish_mcp_oauth_key`] instead so it cannot clobber a
    /// rotation another key published ahead of its not-yet-durable `OAuthSave`.
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
    use crate::db;
    use crate::oauth_refresh::SaveToken;
    use choreo_keystore::OAuthKind;
    use std::sync::mpsc;
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
            access_token: None,
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
            access_token: None,
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

    #[cfg(feature = "mcp")]
    #[test]
    fn an_applied_save_for_one_key_does_not_clobber_another_keys_pending_rotation() {
        // Install the process-global runtime so the per-key publishes have a
        // read view to write (see the sibling snapshot test).
        let (tx, _tx_rx) = crossbeam_channel::unbounded();
        let runtime = crate::mcp::oauth::install(tx);

        let (mut state, _rx) = make_daemon_state();
        bind_for_test(&mut state);
        state.locked = false;

        let key_a = "mcp:docs|a";
        let key_b = "mcp:docs|b";

        // B's DURABLE grant still holds its old (already-spent) refresh token,
        // while the connect path has published B's NEW rotation into the read
        // view ahead of the OAuthSave that would make it durable.
        state
            .credentials
            .insert(key_b.into(), mcp_oauth_credential("b-old"));
        runtime.publish_credential(key_b, mcp_oauth_credential("b-new"));

        // A normal applied save for an UNRELATED key A: its durable grant
        // matches the presented expectation, so the rotation is persisted.
        state
            .credentials
            .insert(key_a.into(), mcp_oauth_credential("a-old"));
        state.handle_oauth_save(key_a, mcp_oauth_credential("a-new"), Some("a-old"));

        // A's rotation took in the read view.
        assert_eq!(
            runtime.load(key_a),
            Some(mcp_oauth_credential("a-new")),
            "the applied save must publish its own key's rotation"
        );
        // B's pending rotation is untouched: an applied save for one key must
        // not republish the whole map and revert another key's visible-but-not-yet-
        // durable rotation to its spent token.
        assert_eq!(
            runtime.load(key_b),
            Some(mcp_oauth_credential("b-new")),
            "an applied save for one key must not clobber another key's pending rotation"
        );
    }
}
