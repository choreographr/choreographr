//! The MCP OAuth credential backend: the daemon half of remote-server OAuth.
//!
//! The interactive sign-in runs in the frontend, which stores a
//! [`ServiceCredential::OAuth`] under a target-scoped key. At connect time the
//! [`choreo_mcp`] engine drives `rmcp`'s `AuthorizationManager`, which refreshes
//! the access token through a [`CredentialBackend`].
//! This module supplies that backend.
//!
//! The backend must not round-trip the command loop for a **read**: an OAuth
//! server can connect while the command loop is itself blocked resolving a
//! session overlay (the connect runs on a worker thread, but the loop waits for
//! it), so a load that sent a `DaemonCommand` and waited for the loop's reply
//! would deadlock. Instead the command loop keeps a lock-protected
//! [`McpOAuthRuntime`] snapshot of the MCP-keyed credentials current, and the
//! backend's `load` reads it directly. Writes (`save`, `clear`,
//! `mark_reauthentication`) are **fire-and-forget** `DaemonCommand`s: an
//! unbounded crossbeam send never blocks, so the connect-time refresh proceeds
//! and the command loop persists the rotation when it is next free. A crash
//! before that persist leaves a spent token → the next refresh is terminal →
//! re-login, the correct safe failure.
//!
//! The snapshot is the same single-writer pattern as the tool registry: the
//! command loop is its sole writer (it republishes after every credential
//! mutation), and the connect-path backend reads it lock-protected. It carries
//! only credential values the command loop already owns; it is not a second
//! source of truth, just a read view that removes the deadlock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use choreo_mcp::{
    CredentialBackend, McpAuthWiring, McpServerConfig, McpTransport, RefreshGuard,
    credential_key_for,
};
use choreo_shared::ServiceCredential;
use crossbeam_channel::Sender;
use tracing::warn;

use crate::daemon::DaemonCommand;

/// The command-loop-published view of the daemon's MCP OAuth credentials, plus
/// the command sender the backend uses for its non-blocking writes.
pub struct McpOAuthRuntime {
    /// The current MCP-keyed credentials, keyed `mcp:<slug>|<hash>`. Written
    /// only by the command loop (via [`Self::publish`]); read by the backend.
    snapshot: Mutex<HashMap<String, ServiceCredential>>,
    /// The command-loop sender, used for fire-and-forget writes.
    tx: Sender<DaemonCommand>,
}

impl McpOAuthRuntime {
    /// Read one MCP credential (a lock-protected clone), or `None` when absent.
    #[must_use]
    pub fn load(&self, key: &str) -> Option<ServiceCredential> {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    /// Replace the whole MCP-keyed credential set (command-loop single writer).
    pub fn publish(&self, creds: HashMap<String, ServiceCredential>) {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = creds;
    }
}

/// The process-global MCP OAuth runtime. Installed once by the daemon core
/// (see [`install`]) so the connect path — which has no `DaemonState` — can
/// reach it. Absent outside a running daemon (e.g. unit tests), in which case
/// no OAuth wiring is produced and a remote server connects unauthenticated.
static RUNTIME: OnceLock<Arc<McpOAuthRuntime>> = OnceLock::new();

/// Install the runtime with the command-loop sender, returning it. Called once
/// the command loop's channel exists; a second call returns the existing one.
pub fn install(tx: Sender<DaemonCommand>) -> Arc<McpOAuthRuntime> {
    Arc::clone(RUNTIME.get_or_init(|| {
        Arc::new(McpOAuthRuntime {
            snapshot: Mutex::new(HashMap::new()),
            tx,
        })
    }))
}

/// The installed runtime, if the daemon core has been assembled.
#[must_use]
pub fn runtime() -> Option<Arc<McpOAuthRuntime>> {
    RUNTIME.get().cloned()
}

/// The target-scoped credential key, the wire `oauth` block, and the baseline
/// auth state for a server config, for the `/mcp` status surface.
///
/// The key is `Some` for any remote (HTTP) server; the `oauth` block is `Some`
/// only when the server configures one. This is the single place the daemon
/// derives the key it hands the frontend, so provisioning and connect-time load
/// cannot drift. The auth state is a baseline the daemon's status handler then
/// refines against its credential store (presence of a stored grant, or a
/// terminal rejection).
#[must_use]
pub fn status_auth(
    config: &McpServerConfig,
) -> (
    Option<String>,
    Option<choreo_proto::McpOAuthConfig>,
    choreo_proto::McpAuthState,
) {
    match &config.transport {
        McpTransport::Http { headers, oauth, .. } => {
            let key = credential_key_for(config);
            let oauth_config = oauth.as_ref().map(|oauth| choreo_proto::McpOAuthConfig {
                client_id: oauth.client_id.clone(),
                callback_port: oauth.callback_port,
                scopes: oauth.scopes.clone(),
            });
            let state = if oauth_config.is_some() {
                choreo_proto::McpAuthState::NotLoggedIn
            } else if headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"))
            {
                choreo_proto::McpAuthState::BearerToken
            } else {
                choreo_proto::McpAuthState::NotApplicable
            };
            (key, oauth_config, state)
        }
        McpTransport::Stdio { .. } => (None, None, choreo_proto::McpAuthState::NotApplicable),
    }
}

/// The OAuth wiring for a remote server with an `oauth` block, or `None`.
/// A server with no `oauth` block (or a stdio server) gets no wiring — it
/// authenticates with its static `headers` only. A running daemon that has
/// installed the runtime supplies the backend; before that the wiring is
/// absent and the server connects unauthenticated (then surfaces an
/// authorization-required error the user resolves with `mcp login`).
#[must_use]
pub fn wiring_for(config: &McpServerConfig) -> Option<McpAuthWiring> {
    match &config.transport {
        McpTransport::Http { oauth: Some(_), .. } => {}
        McpTransport::Http { oauth: None, .. } | McpTransport::Stdio { .. } => return None,
    }
    let runtime = runtime()?;
    let key = credential_key_for(config)?;
    let canonical_resource = match &config.transport {
        McpTransport::Http { url, .. } => choreo_mcp::canonical_target(url),
        McpTransport::Stdio { .. } => return None,
    };
    Some(McpAuthWiring {
        key: key.clone(),
        backend: Arc::new(McpOAuthBackend {
            key,
            runtime,
            canonical_resource,
        }),
    })
}

/// The [`CredentialBackend`] over the daemon's MCP OAuth runtime.
struct McpOAuthBackend {
    /// The target-scoped credential key this backend serves.
    key: String,
    /// The shared runtime (snapshot + command sender).
    runtime: Arc<McpOAuthRuntime>,
    /// The server's canonical resource URL, for fail-closed load validation.
    canonical_resource: String,
}

impl CredentialBackend for McpOAuthBackend {
    fn load(&self) -> Result<Option<ServiceCredential>, String> {
        match self.runtime.load(&self.key) {
            // Fail-closed: a stored credential whose `resource` no longer matches
            // the configured server URL (a re-pointed repo server) is treated as
            // absent, so it is never sent. The issuer is enforced separately at
            // connect: the `oauth` block carries no issuer, so the connect-path
            // adapter compares the stored issuer against the one re-discovered
            // during discovery and withholds a mismatch (see `choreo-mcp`'s
            // `CredentialStoreAdapter::with_expected_issuer`).
            Some(credential)
                if !choreo_mcp::credential_matches_target(
                    &credential,
                    &self.canonical_resource,
                    None,
                ) =>
            {
                warn!(
                    key = %self.key,
                    "stored MCP credential resource mismatch; treating as not logged in"
                );
                Ok(None)
            }
            other => Ok(other),
        }
    }

    fn save(&self, credential: ServiceCredential) -> Result<(), String> {
        // Fire-and-forget: an unbounded send never blocks, so the connect-time
        // refresh proceeds even while the command loop is busy. Persistence
        // happens on the loop.
        self.runtime
            .tx
            .send(DaemonCommand::OAuthSave {
                key: self.key.clone(),
                credential,
            })
            .map_err(|e| e.to_string())
    }

    fn clear(&self) -> Result<(), String> {
        self.runtime
            .tx
            .send(DaemonCommand::OAuthClear {
                key: self.key.clone(),
            })
            .map_err(|e| e.to_string())
    }

    fn acquire_refresh_guard(&self) -> Result<Option<RefreshGuard>, String> {
        // The adapter calls this from a `spawn_blocking` task (never an async
        // context), so the blocking acquisition is safe; the owned guard is
        // `Send + 'static` and can be held across the refresh `await`.
        let lock = crate::oauth_refresh_locks::acquire_owned(&self.key);
        Ok(Some(RefreshGuard::new(lock.lock())))
    }

    fn mark_reauthentication(&self) -> Result<(), String> {
        self.runtime
            .tx
            .send(DaemonCommand::McpAuthRejected {
                key: self.key.clone(),
            })
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_shared::OAuthKind;

    #[test]
    fn runtime_loads_and_publishes() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let rt = Arc::new(McpOAuthRuntime {
            snapshot: Mutex::new(HashMap::new()),
            tx,
        });
        assert!(rt.load("mcp:docs|x").is_none());
        let cred = ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: None,
            resource: None,
            authorization_server: None,
            refresh_token: Some("r".into()),
            account_id: None,
            client_id: None,
            client_secret: None,
            granted_scopes: vec![],
            token_type: None,
        };
        rt.publish(HashMap::from([("mcp:docs|x".to_string(), cred.clone())]));
        assert_eq!(rt.load("mcp:docs|x"), Some(cred));
    }

    #[test]
    fn wiring_is_absent_without_an_oauth_block() {
        let http_no_oauth = McpServerConfig {
            slug: "docs".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: HashMap::new(),
                oauth: None,
            },
            enabled: true,
            timeout: None,
            protocol: choreo_mcp::McpProtocolMode::Auto,
            max_concurrent_calls: None,
            max_restarts: None,
            disabled_tools: Vec::new(),
        };
        assert!(wiring_for(&http_no_oauth).is_none());
    }
}
