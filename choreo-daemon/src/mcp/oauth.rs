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
//! and the command loop persists the rotation when it is next free. Before
//! enqueuing that durable command the backend also publishes the write into the
//! snapshot synchronously ([`McpOAuthRuntime::publish_credential`] /
//! `remove_credential`): a rotation (or a clear/rejection) lands in the read view
//! before the per-credential single-flight guard is released, so a concurrent
//! refresh of the same grant re-reads the rotated credential instead of spending
//! an already-rotated single-use refresh token. A crash before the durable
//! persist leaves a spent token → the next refresh is terminal → re-login, the
//! correct safe failure.
//!
//! The snapshot is the same single-writer pattern as the tool registry: the
//! command loop is its durable single writer and whole-map republisher (it
//! republishes after every credential mutation), while the connect-path backend
//! also advances its own key's read-view entry synchronously (see
//! [`McpOAuthRuntime::publish_credential`] / `remove_credential`), and the
//! connect-path backend reads it lock-protected. It carries only credential
//! values the command loop already owns; it is not a second source of truth,
//! just a read view that removes the deadlock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use choreo_mcp::{
    CredentialBackend, McpAuthWiring, McpServerConfig, McpTransport, RefreshGuard,
    credential_key_for,
};
use choreo_shared::ServiceCredential;
use crossbeam_channel::Sender;
use tracing::warn;
use zeroize::Zeroizing;

use crate::daemon::DaemonCommand;

/// The command-loop-published view of the daemon's MCP OAuth credentials, plus
/// the command sender the backend uses for its non-blocking writes.
pub struct McpOAuthRuntime {
    /// The current MCP-keyed credentials, keyed `mcp:<slug>|<hash>`. The
    /// command loop rewrites the whole map (via [`Self::publish`]); the
    /// connect-path backend and the command loop both advance a single key's
    /// entry (via [`Self::publish_credential`] / [`Self::remove_credential`]);
    /// read by the backend.
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

    /// Publish one rotated (or freshly obtained) MCP credential into the
    /// connect-path read view, synchronously.
    ///
    /// The connect-path backend calls this from inside a refresh while it holds
    /// the per-credential single-flight guard, BEFORE enqueuing the durable
    /// `OAuthSave`. That ordering is what closes the concurrent-refresh race: a
    /// second connection that acquires the guard next re-reads this rotation from
    /// the snapshot instead of the pre-rotation credential, so it never spends an
    /// already-rotated single-use refresh token. The command loop remains the
    /// durable single writer; this only advances the in-memory read view ahead of
    /// the enqueued persist.
    pub fn publish_credential(&self, key: &str, credential: ServiceCredential) {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_string(), credential);
    }

    /// Remove one MCP credential from the connect-path read view, synchronously
    /// (the counterpart to [`Self::publish_credential`] for a logout/clear or a
    /// terminally rejected grant), so a concurrent connect observes the removal
    /// without waiting for the command loop. The durable removal is still the
    /// command loop's.
    pub fn remove_credential(&self, key: &str) {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
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
                authorization_server: oauth.authorization_server.clone(),
                client_secret_env: oauth.client_secret_env.clone(),
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

    /// Persist a rotated (or freshly obtained) credential.
    ///
    /// The durable write is fire-and-forget — an unbounded send never blocks, so
    /// the connect-time refresh proceeds even while the command loop is busy;
    /// the loop persists it when next free. But the connect-path read view is
    /// advanced *synchronously first*: `rmcp` holds the per-credential
    /// single-flight guard across `load → exchange → save`, and a second
    /// connection can acquire that guard the moment this call returns. Publishing
    /// the rotation before the enqueue (and therefore before the guard release)
    /// means the loser re-reads the rotated credential rather than the
    /// pre-rotation one, so it never spends an already-rotated single-use refresh
    /// token. The command channel is FIFO, so the winner's publish happens-before
    /// its guard release happens-before the loser's enqueue, and the durable
    /// saves still apply in order.
    fn save(
        &self,
        credential: ServiceCredential,
        expected_refresh_token: Option<String>,
    ) -> Result<(), String> {
        self.runtime
            .publish_credential(&self.key, credential.clone());
        self.runtime
            .tx
            .send(DaemonCommand::OAuthSave {
                key: self.key.clone(),
                credential,
                // Wipe-on-drop across the command channel.
                expected_refresh_token: expected_refresh_token.map(Zeroizing::new),
            })
            .map_err(|e| e.to_string())
    }

    fn clear(&self) -> Result<(), String> {
        // Advance the read view synchronously so a concurrent connect observes
        // the removal without waiting for the command loop; the durable removal
        // still travels as the fire-and-forget command below.
        self.runtime.remove_credential(&self.key);
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
        let lock = crate::oauth_refresh_locks::acquire(&self.key);
        Ok(Some(RefreshGuard::new(lock.lock())))
    }

    fn mark_reauthentication(&self) -> Result<(), String> {
        // A terminally rejected grant is withheld from the read view at once (the
        // `dead_oauth` quarantine), so a concurrent connect never re-spends the
        // spent token; the durable quarantine still travels as the command below.
        self.runtime.remove_credential(&self.key);
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

    const KEY: &str = "mcp:docs|x";

    /// A minimal MCP OAuth credential whose refresh token is `refresh`, so a
    /// rotated publish is distinguishable from the one it replaces.
    fn sample_cred(refresh: &str) -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: None,
            resource: None,
            authorization_server: None,
            token_endpoint: None,
            refresh_encoding: None,
            refresh_token: Some(refresh.into()),
            account_id: None,
            client_id: None,
            client_secret: None,
            granted_scopes: vec![],
            token_type: None,
        }
    }

    /// A runtime with an unbounded command channel, plus the backend that serves
    /// `KEY` over it — the same shape the daemon builds for a remote server.
    fn runtime_and_backend() -> (
        Arc<McpOAuthRuntime>,
        McpOAuthBackend,
        crossbeam_channel::Receiver<DaemonCommand>,
    ) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let rt = Arc::new(McpOAuthRuntime {
            snapshot: Mutex::new(HashMap::new()),
            tx,
        });
        let backend = McpOAuthBackend {
            key: KEY.to_string(),
            runtime: Arc::clone(&rt),
            canonical_resource: "https://example.com/mcp".to_string(),
        };
        (rt, backend, rx)
    }

    #[test]
    fn runtime_loads_and_publishes() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let rt = Arc::new(McpOAuthRuntime {
            snapshot: Mutex::new(HashMap::new()),
            tx,
        });
        assert!(rt.load(KEY).is_none());
        let cred = sample_cred("r");
        rt.publish(HashMap::from([(KEY.to_string(), cred.clone())]));
        assert_eq!(rt.load(KEY), Some(cred));
    }

    #[test]
    fn publish_and_remove_credential_advance_one_entry() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let rt = McpOAuthRuntime {
            snapshot: Mutex::new(HashMap::new()),
            tx,
        };
        assert!(rt.load(KEY).is_none());

        // A first publish lands the credential; a second for the same key
        // replaces it in place (the rotation case).
        rt.publish_credential(KEY, sample_cred("r1"));
        assert_eq!(rt.load(KEY), Some(sample_cred("r1")));
        rt.publish_credential(KEY, sample_cred("r2"));
        assert_eq!(rt.load(KEY), Some(sample_cred("r2")));

        // Removing one key leaves any other entry untouched.
        rt.publish_credential("mcp:other|y", sample_cred("r3"));
        rt.remove_credential(KEY);
        assert!(rt.load(KEY).is_none());
        assert_eq!(rt.load("mcp:other|y"), Some(sample_cred("r3")));
    }

    #[test]
    fn save_publishes_the_rotation_then_enqueues_the_durable_write() {
        let (rt, backend, rx) = runtime_and_backend();
        let cred = sample_cred("rotated");
        backend.save(cred.clone(), None).expect("save");

        // The read view is advanced synchronously, so a concurrent refresh that
        // acquires the guard next re-reads the rotation without a loop round-trip.
        assert_eq!(rt.load(KEY), Some(cred.clone()));

        // The durable write is still enqueued for the command loop.
        let DaemonCommand::OAuthSave {
            key,
            credential,
            expected_refresh_token,
        } = rx.try_recv().expect("an enqueued OAuthSave")
        else {
            panic!("expected an OAuthSave command");
        };
        assert_eq!(key, KEY);
        assert_eq!(credential, cred);
        assert!(expected_refresh_token.is_none());
    }

    #[test]
    fn clear_and_reauthentication_remove_the_read_view_then_enqueue() {
        let (rt, backend, rx) = runtime_and_backend();

        rt.publish_credential(KEY, sample_cred("r1"));
        backend.clear().expect("clear");
        assert!(rt.load(KEY).is_none());
        assert!(matches!(
            rx.try_recv().expect("an enqueued OAuthClear"),
            DaemonCommand::OAuthClear { .. }
        ));

        // A terminally rejected grant is withheld from the read view at once.
        rt.publish_credential(KEY, sample_cred("r2"));
        backend.mark_reauthentication().expect("mark");
        assert!(rt.load(KEY).is_none());
        assert!(matches!(
            rx.try_recv().expect("an enqueued McpAuthRejected"),
            DaemonCommand::McpAuthRejected { .. }
        ));
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
