//! MCP OAuth credential persistence handlers for [`DaemonState`].
//!
//! The MCP connect path persists a rotated grant
//! ([`DaemonState::handle_oauth_save`]), deletes one on logout
//! ([`DaemonState::handle_oauth_clear`]), and records a terminal refresh
//! rejection ([`DaemonState::handle_mcp_auth_rejected`]); each also advances the
//! connect-path read view for its own key so a concurrent refresh re-reads a
//! just-obtained rotation. This is the MCP half of `daemon/oauth_store`; the
//! provider half lives in the sibling [`super::provider`] module, and both share
//! the stale-rotation guard [`super::refresh_token_unchanged`].

use crate::daemon::DaemonState;
use crate::db;
use choreo_keystore::ServiceCredential;
use tracing::{info, warn};

use super::refresh_token_unchanged;

impl DaemonState {
    /// Persist a rotated MCP OAuth credential (a fire-and-forget
    /// [`DaemonCommand::OAuthSave`](crate::daemon::DaemonCommand::OAuthSave)).
    ///
    /// Re-encrypts the updated durable subset to the binding public key and
    /// persists it (the daemon is the sole blob writer; the adapter keeps the
    /// access token out of the durable grant unless the server issued no
    /// refresh token, where it is the only credential), then updates the
    /// in-memory map and the MCP OAuth snapshot, dropping any cached token or
    /// quarantine for the key.
    pub(in crate::daemon) fn handle_oauth_save(
        &mut self,
        key: &str,
        credential: ServiceCredential,
        expected_refresh_token: Option<&str>,
    ) {
        if self.locked {
            warn!(%key, "OAuthSave ignored: keystore is locked");
            // The connect path published this rotation into the read-view
            // snapshot before enqueuing (that ordering is what lets a concurrent
            // refresh re-read it), so the read view is holding a live token even
            // though the save is dropped. A locked daemon must not keep serving
            // it: with `credentials` cleared by the lock, this resolves to a
            // `remove_credential` for the key, withdrawing exactly what the
            // connect path advanced.
            self.publish_mcp_oauth_key(key);
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
            // refresh re-read it). Restore THIS key's entry from the durable
            // store — whose credential was just removed — so the read view does
            // not keep serving a grant the user deleted, without disturbing any
            // other key's pending rotation.
            self.publish_mcp_oauth_key(key);
            return;
        }
        // Guard against a credential replaced under the refresh (a re-login
        // landing between the connect-path load and this save): the stored grant
        // is newer, so persisting the rotated refresh token would clobber it.
        // Only apply the save when the stored refresh token is still the one the
        // refresh presented.
        //
        // This guard keys on the refresh token, so it does NOT cover an
        // access-token-only grant (a server that issued no refresh token): with
        // no refresh token to compare, `expected_refresh_token` is `None` and
        // the guard passes unconditionally. That is acceptable because `rmcp`
        // never reaches `save` for a grant with no refresh token — its refresh
        // path returns `AuthorizationRequired` before calling `save` — so the
        // access-token persistence here is defense-in-depth, not a live path.
        // Should that ever change, fold the durable access token into the
        // expectation this guard compares.
        let changed = match self.credentials.get(key) {
            Some(current) => !refresh_token_unchanged(current, expected_refresh_token),
            None => false,
        };
        if changed {
            warn!(%key, "OAuthSave dropped: credential changed under the refresh");
            // As above: the connect path already advanced this key's read-view
            // entry to the superseded rotation, so republish this key from the
            // durable store to put the newer grant back.
            self.publish_mcp_oauth_key(key);
            return;
        }
        if let Err(e) = self.encrypt_and_persist_credential(key, &credential) {
            warn!(%key, error = %e, "failed to persist rotated MCP OAuth credential");
            return;
        }
        self.credentials.insert(key.to_string(), credential);
        self.access_tokens.remove(key);
        self.dead_oauth.remove(key);
        // Publish only this key: the rotation is now durable, so its entry is
        // refreshed, while other keys' entries (including any rotation another
        // key published ahead of its own save) are left untouched.
        self.publish_mcp_oauth_key(key);
        // The MCP refresh completed (this save is its terminal step), so prune
        // any now-idle single-flight locks the refresh registry is holding for
        // this or another credential.
        crate::oauth_refresh_locks::prune();
        info!(%key, "persisted rotated MCP OAuth credential");
    }

    /// Delete a stored MCP OAuth credential (a fire-and-forget
    /// [`DaemonCommand::OAuthClear`](crate::daemon::DaemonCommand::OAuthClear)).
    pub(in crate::daemon) fn handle_oauth_clear(&mut self, key: &str) {
        if let Err(e) = db::remove_credential_blob(&self.db, key) {
            warn!(%key, error = %e, "failed to remove MCP OAuth credential");
            return;
        }
        self.credentials.remove(key);
        self.access_tokens.remove(key);
        self.dead_oauth.remove(key);
        self.publish_mcp_oauth_key(key);
        crate::oauth_refresh_locks::prune();
        info!(%key, "cleared MCP OAuth credential");
    }

    /// Quarantine an MCP credential after a terminal refresh rejection (a
    /// fire-and-forget
    /// [`DaemonCommand::McpAuthRejected`](crate::daemon::DaemonCommand::McpAuthRejected)),
    /// so the `/mcp` status shows a re-authentication request.
    pub(in crate::daemon) fn handle_mcp_auth_rejected(&mut self, key: &str) {
        if self.dead_oauth.insert(key.to_string()) {
            // Quarantining withdraws this key from the read view only; other
            // keys' entries (and their pending rotations) stay intact.
            self.publish_mcp_oauth_key(key);
            info!(%key, "MCP OAuth grant terminally rejected; re-authentication required");
        }
    }
}
