//! Resolving an account to its provider auth grant, and reading the durable
//! OAuth refresh material a session-side refresh runs against.
//!
//! The daemon is the sole holder of an account's credential, so these
//! `DaemonState` methods are the one place that turns a stored
//! [`ServiceCredential`] into (a) the [`ProviderAuth`] a provider client is
//! built from and (b) the [`RefreshMaterial`] the session thread runs a single
//! OAuth exchange against. Both read the in-memory maps the command loop owns,
//! so every method here runs on the command loop only.
//!
//! Split out of `daemon.rs` (which keeps the state and command types) so the
//! credential-to-auth projection sits beside its handler siblings
//! (`daemon/keystore.rs`, `daemon/oauth_store.rs`).

use choreo_keystore::{OAuthKind, ServiceCredential};
use zeroize::Zeroizing;

use super::DaemonState;
use crate::oauth_refresh::{CachedAccessToken, RefreshMaterial, TokenLoad};
use crate::providers::ProviderAuth;

impl DaemonState {
    /// Resolve the auth grant for an account, if a credential is stored. API
    /// keys project straight through; an OAuth credential carries its whole
    /// grant plus any cached access token, so the refresh path has the refresh
    /// token to work with.
    pub(super) fn auth_for(&self, name: &str) -> Option<ProviderAuth> {
        let credential = self.credentials.get(name)?;
        Some(match credential {
            ServiceCredential::ApiKey { key } => ProviderAuth::ApiKey {
                key: Zeroizing::new(key.clone()),
            },
            other => ProviderAuth::OAuth {
                credential: Box::new(other.clone()),
                access_token: self
                    .access_tokens
                    .get(name)
                    .map(|cache| cache.access_token.clone()),
            },
        })
    }

    /// Read an OAuth account's token state for a session-side refresh: the
    /// cached access token, the durable refresh material, and the quarantine
    /// flag. A locked keystore yields no material (no credential in memory).
    pub(crate) fn oauth_token_load(&self, account: &str) -> TokenLoad {
        let dead = self.dead_oauth.contains(account);
        let cached = self.access_tokens.get(account).map(|c| CachedAccessToken {
            access_token: c.access_token.clone(),
            expires_at: c.expires_at,
            received_at: c.received_at,
        });
        let material = self.oauth_refresh_material(account);
        TokenLoad {
            cached,
            material,
            dead,
        }
    }

    /// Build the durable refresh material for a **provider** OAuth account, or
    /// `None` when the account is not an OAuth provider grant (an API key, an
    /// MCP grant, or no credential).
    ///
    /// The token endpoint and the refresh body encoding are read straight off
    /// the credential, where they were recorded at login, so the runtime
    /// refresh reuses the exact endpoint and encoding the grant was issued
    /// against and never re-derives them from the current catalog profile — an
    /// overlay edit or a catalog change cannot redirect or re-encode the
    /// refresh. A grant that recorded neither cannot be refreshed here, so its
    /// requests fall back to the cached access token until a 401.
    pub(super) fn oauth_refresh_material(&self, account: &str) -> Option<RefreshMaterial> {
        let credential = self.credentials.get(account)?;
        let oauth = credential.as_oauth()?;
        // Only a provider grant refreshes here; an MCP grant is owned by the MCP
        // authorization manager.
        if oauth.kind != OAuthKind::Provider {
            return None;
        }
        let refresh_token = oauth.refresh_token?;
        // The token endpoint and refresh encoding were recorded on the
        // credential at login, so a runtime refresh reuses the exact endpoint
        // and body encoding it was issued against and does not depend on the
        // catalog profile still matching (an overlay edit or a catalog change
        // cannot redirect or re-encode the refresh).
        let token_endpoint = oauth.token_endpoint?;
        let encoding = oauth.refresh_encoding?;
        Some(RefreshMaterial {
            token_endpoint: token_endpoint.to_string(),
            encoding,
            client_id: oauth.client_id.map(str::to_string),
            // The durable grant root and the client secret are secrets the
            // session thread needs only for this one exchange, so they travel
            // wipe-on-drop.
            client_secret: oauth.client_secret.map(|s| Zeroizing::new(s.to_string())),
            refresh_token: Zeroizing::new(refresh_token.to_string()),
            granted_scopes: oauth.granted_scopes.to_vec(),
        })
    }
}
