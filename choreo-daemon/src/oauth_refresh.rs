//! Single-flight OAuth refresh orchestration shared by the session refresh path
//! and its tests.
//!
//! A provider OAuth grant is refreshed by presenting its refresh token to the
//! provider's token endpoint. The **session thread** runs the blocking exchange
//! (the provider crates stay synchronous and daemon-free), while the **daemon
//! command loop** owns the access-token cache and is the sole writer of the
//! credential blob. This module is the seam between them: it defines the token
//! material a source hands out, the refreshed value it takes back, and the
//! single-flight driver that serializes a whole `load → re-check → exchange →
//! persist` sequence under [`crate::oauth_refresh_locks`].
//!
//! **Single-flight.** Rotation makes a refresh token single-use: two concurrent
//! refreshes of one credential can each spend it, and the loser fails with a
//! terminal `invalid_grant` even though the grant is healthy. Acquiring the
//! per-credential lock for the whole sequence means a loser re-reads the
//! winner's persisted token and reuses it instead of spending a stale one.
//!
//! **Rotate-if-present.** The engine adopts a returned refresh token when the
//! response carries one and keeps the stored token when it does not — one code
//! path for rotating and non-rotating providers alike. The same rule applies to
//! the identity claim (`account_id`), replaced only when a new `id_token`
//! carries it.

use std::sync::PoisonError;

use choreo_ai_protocols::catalog::GrantEncoding;
use choreo_ai_protocols::oauth::{OAuthRefreshError, OAuthRefreshRequest, refresh_oauth_token};
use zeroize::Zeroizing;

use crate::oauth_refresh_locks;

/// The buffer before expiry at which a proactive refresh fires, in seconds.
///
/// Hardcoded for v1: an access token whose remaining lifetime is at or below
/// this is refreshed before the request is sent, so a request is not sent with a
/// token about to expire. A token with no reported expiry is treated as usable
/// until the provider rejects it (a 401 triggers a forced refresh).
pub const REFRESH_BUFFER_SECS: i64 = 60;

/// A cached access token's state, as the daemon holds it.
pub struct CachedAccessToken {
    /// The bearer access token (memory-only).
    pub access_token: Zeroizing<String>,
    /// The provider-reported expiry (Unix seconds), when reported.
    pub expires_at: Option<i64>,
    /// When the token was obtained (Unix seconds).
    pub received_at: i64,
}

/// The durable refresh material a source hands out for one exchange.
pub struct RefreshMaterial {
    /// The absolute token endpoint URL (issuer + token path).
    pub token_endpoint: String,
    /// The request-body encoding the endpoint expects.
    pub encoding: GrantEncoding,
    /// The client id recorded at login.
    pub client_id: Option<String>,
    /// The client secret, for a confidential client.
    pub client_secret: Option<String>,
    /// The refresh token to present.
    pub refresh_token: String,
    /// The scopes to request.
    pub granted_scopes: Vec<String>,
}

/// A source's answer to a load: the cached token, the durable material, and
/// whether the account is quarantined.
pub struct TokenLoad {
    /// The cached access token, when one is held.
    pub cached: Option<CachedAccessToken>,
    /// The durable refresh material; `None` when the account is not an OAuth
    /// provider grant (an API key, or no credential).
    pub material: Option<RefreshMaterial>,
    /// Whether the account is quarantined after a terminal failure.
    pub dead: bool,
}

/// A successfully refreshed token, ready to persist. Rotation is
/// rotate-if-present: `refresh_token` and `account_id` are `None` when the
/// exchange carried no new value, so the stored ones are kept.
pub struct RefreshedTokens {
    /// The new access token.
    pub access_token: Zeroizing<String>,
    /// The new expiry (Unix seconds), when the provider reported one.
    pub expires_at: Option<i64>,
    /// When the token was obtained (Unix seconds).
    pub received_at: i64,
    /// The rotated refresh token, when the exchange issued one.
    pub refresh_token: Option<Zeroizing<String>>,
    /// The refreshed identity claim, when a new `id_token` carried one.
    pub account_id: Option<String>,
}

/// A borrowed view of the fields a save needs, so the daemon command handler
/// takes one grouped value instead of six positional arguments.
#[derive(Clone, Copy)]
pub struct SaveToken<'a> {
    /// The account whose token was refreshed.
    pub account: &'a str,
    /// The new access token.
    pub access_token: &'a Zeroizing<String>,
    /// The new expiry (Unix seconds), when reported.
    pub expires_at: Option<i64>,
    /// When the token was obtained (Unix seconds).
    pub received_at: i64,
    /// The rotated refresh token, when the exchange issued one.
    pub rotated_refresh_token: Option<&'a str>,
    /// The refreshed identity claim, when a new `id_token` carried one.
    pub new_account_id: Option<&'a str>,
}

/// The destination a refresh drives: it loads the durable material, persists a
/// refreshed token, and reports a terminal failure. The session's implementation
/// forwards each call to the daemon over its command channel; a test
/// implementation serves them from memory.
pub trait OAuthTokenSource: Send + Sync {
    /// Load the current token state for `account`.
    fn load(&self, account: &str) -> TokenLoad;
    /// Persist a refreshed token for `account`.
    ///
    /// # Errors
    ///
    /// Returns an error string when the persist failed (e.g. a locked
    /// keystore), which the caller treats as transient.
    fn save(&self, account: &str, tokens: &RefreshedTokens) -> Result<(), String>;
    /// Report that `account`'s grant is terminally dead (quarantine + one
    /// re-login notice).
    fn report_terminal(&self, account: &str);
}

/// The outcome of a refresh attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// A refresh ran and the new token was persisted.
    Refreshed,
    /// No refresh was needed: the cached token is still usable.
    NotNeeded,
    /// The account is not an OAuth provider grant, so there is nothing to do.
    NotApplicable,
}

/// A refresh failure.
#[derive(Debug, thiserror::Error)]
pub enum RefreshFailure {
    /// The grant is terminally dead; the source has already reported it. The
    /// caller surfaces a re-login-required error.
    #[error("oauth grant is dead; re-login required")]
    Terminal,
    /// A transient failure; the caller may back off and retry.
    #[error("oauth refresh transiently failed: {detail}")]
    Transient {
        /// A short reason for the transient failure.
        detail: String,
    },
}

/// An [`OAuthTokenSource`] that drives the daemon command loop over the
/// session's `daemon_tx` channel.
///
/// The daemon owns the token cache and is the sole blob writer, so every load /
/// save / terminal report is a command round-trip: the session runs only the
/// network exchange, and the daemon persists the rotation. This is the seam the
/// "session thread performs the exchange, daemon owns the cache" split rests
/// on. Each reply rides a one-shot `std::sync::mpsc` channel (AGENTS.md: a
/// per-request reply is the sanctioned use).
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

    fn save(&self, account: &str, tokens: &RefreshedTokens) -> Result<(), String> {
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

/// Whether a cached token is usable at `now`: an absent expiry is usable (until
/// a 401), and a reported expiry is usable only while `now` is more than the
/// buffer away from it.
#[must_use]
pub fn token_is_usable(cached: &CachedAccessToken, now: i64) -> bool {
    match cached.expires_at {
        Some(expires_at) => now.saturating_add(REFRESH_BUFFER_SECS) < expires_at,
        None => true,
    }
}

/// Refresh `account`'s OAuth token if needed (or unconditionally when `force`),
/// serialized against concurrent refreshes of the same credential.
///
/// The whole `load → re-check → exchange → persist` runs under the account's
/// single-flight lock, so the loser of a race observes the winner's persisted
/// token and reuses it (returning [`RefreshOutcome::NotNeeded`]) rather than
/// spending a single-use refresh token twice.
///
/// # Errors
///
/// Returns [`RefreshFailure::Terminal`] when the source reports the account
/// dead (before or as a result of this call) and [`RefreshFailure::Transient`]
/// on a transient exchange or persist failure.
pub fn refresh_single_flight(
    source: &dyn OAuthTokenSource,
    account: &str,
    force: bool,
    now: i64,
) -> Result<RefreshOutcome, RefreshFailure> {
    // The lock is held for the entire sequence. It is a plain `Mutex<()>`; a
    // poisoned lock (a panic while holding it) is recovered so a prior panic
    // cannot wedge every later refresh.
    let lock = oauth_refresh_locks::acquire(account);
    let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);

    // The load happens UNDER the lock: a loser re-reads the winner's rotation
    // rather than using a stale in-memory copy.
    let load = source.load(account);
    if load.dead {
        return Err(RefreshFailure::Terminal);
    }
    let Some(material) = load.material else {
        return Ok(RefreshOutcome::NotApplicable);
    };
    if !force
        && let Some(cached) = &load.cached
        && token_is_usable(cached, now)
    {
        return Ok(RefreshOutcome::NotNeeded);
    }

    let request = OAuthRefreshRequest {
        token_endpoint: &material.token_endpoint,
        encoding: material.encoding,
        client_id: material.client_id.as_deref(),
        client_secret: material.client_secret.as_deref(),
        refresh_token: &material.refresh_token,
        scopes: &material.granted_scopes,
    };
    match refresh_oauth_token(&request) {
        Ok(tokens) => {
            let refreshed = RefreshedTokens {
                access_token: tokens.access_token,
                // `expires_in` is a small seconds count; the cast cannot
                // realistically overflow, and saturating keeps a hostile value
                // from wrapping into the past.
                expires_at: tokens
                    .expires_in
                    .map(|secs| now.saturating_add(i64::try_from(secs).unwrap_or(i64::MAX))),
                received_at: now,
                refresh_token: tokens.refresh_token,
                account_id: tokens.account_id,
            };
            match source.save(account, &refreshed) {
                Ok(()) => Ok(RefreshOutcome::Refreshed),
                Err(detail) => Err(RefreshFailure::Transient {
                    detail: format!("failed to persist refreshed token: {detail}"),
                }),
            }
        }
        Err(OAuthRefreshError::Terminal { .. }) => {
            source.report_terminal(account);
            Err(RefreshFailure::Terminal)
        }
        Err(OAuthRefreshError::Transient { detail }) => Err(RefreshFailure::Transient { detail }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn cached(expires_at: Option<i64>, now: i64) -> CachedAccessToken {
        CachedAccessToken {
            access_token: Zeroizing::new("at".to_string()),
            expires_at,
            received_at: now,
        }
    }

    #[test]
    fn a_token_with_no_expiry_is_usable() {
        assert!(token_is_usable(&cached(None, 1_000), 1_000));
    }

    #[test]
    fn a_token_far_from_expiry_is_usable() {
        // 1000s away is well beyond the 60s buffer.
        assert!(token_is_usable(&cached(Some(2_000), 1_000), 1_000));
    }

    #[test]
    fn a_token_within_the_buffer_is_not_usable() {
        // 30s away is within the 60s buffer.
        assert!(!token_is_usable(&cached(Some(1_030), 1_000), 1_000));
        // Exactly at the buffer edge is not usable.
        assert!(!token_is_usable(&cached(Some(1_060), 1_000), 1_000));
        // One second past the edge is usable.
        assert!(token_is_usable(&cached(Some(1_061), 1_000), 1_000));
    }

    #[test]
    fn an_already_expired_token_is_not_usable() {
        assert!(!token_is_usable(&cached(Some(900), 1_000), 1_000));
    }

    /// A source that hands out fixed material and records how many exchanges it
    /// served, plus the last saved value.
    struct StubSource {
        material: Option<RefreshMaterial>,
        cached: Mutex<Option<CachedAccessToken>>,
        dead: Mutex<bool>,
        saves: Mutex<Vec<(Option<String>, i64)>>,
        terminal_reports: Mutex<usize>,
    }

    impl StubSource {
        fn new(material: Option<RefreshMaterial>, cached: Option<CachedAccessToken>) -> Self {
            Self {
                material,
                cached: Mutex::new(cached),
                dead: Mutex::new(false),
                saves: Mutex::new(Vec::new()),
                terminal_reports: Mutex::new(0),
            }
        }
    }

    impl OAuthTokenSource for StubSource {
        fn load(&self, _account: &str) -> TokenLoad {
            TokenLoad {
                cached: self
                    .cached
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .map(|c| CachedAccessToken {
                        access_token: c.access_token.clone(),
                        expires_at: c.expires_at,
                        received_at: c.received_at,
                    }),
                material: self.material.as_ref().map(|m| RefreshMaterial {
                    token_endpoint: m.token_endpoint.clone(),
                    encoding: m.encoding,
                    client_id: m.client_id.clone(),
                    client_secret: m.client_secret.clone(),
                    refresh_token: m.refresh_token.clone(),
                    granted_scopes: m.granted_scopes.clone(),
                }),
                dead: *self.dead.lock().unwrap_or_else(PoisonError::into_inner),
            }
        }

        fn save(&self, _account: &str, tokens: &RefreshedTokens) -> Result<(), String> {
            self.saves
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((
                    tokens
                        .refresh_token
                        .as_ref()
                        .map(|t| t.as_str().to_string()),
                    tokens.received_at,
                ));
            *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = Some(CachedAccessToken {
                access_token: tokens.access_token.clone(),
                expires_at: tokens.expires_at,
                received_at: tokens.received_at,
            });
            Ok(())
        }

        fn report_terminal(&self, _account: &str) {
            *self
                .terminal_reports
                .lock()
                .unwrap_or_else(PoisonError::into_inner) += 1;
        }
    }

    #[test]
    fn a_fresh_token_needs_no_refresh() {
        // No material means "not OAuth", so nothing to do.
        let none = StubSource::new(None, None);
        assert_eq!(
            refresh_single_flight(&none, "acct", false, 1_000).expect("ok"),
            RefreshOutcome::NotApplicable
        );

        // Material present but a fresh cached token: no refresh.
        let fresh = StubSource::new(
            Some(RefreshMaterial {
                token_endpoint: "http://127.0.0.1:1/token".into(),
                encoding: GrantEncoding::Json,
                client_id: None,
                client_secret: None,
                refresh_token: "rt".into(),
                granted_scopes: Vec::new(),
            }),
            Some(cached(Some(9_999), 1_000)),
        );
        assert_eq!(
            refresh_single_flight(&fresh, "acct", false, 1_000).expect("ok"),
            RefreshOutcome::NotNeeded
        );
    }

    #[test]
    fn a_dead_account_is_terminal_without_an_exchange() {
        let source = StubSource::new(
            Some(RefreshMaterial {
                token_endpoint: "http://127.0.0.1:1/token".into(),
                encoding: GrantEncoding::Json,
                client_id: None,
                client_secret: None,
                refresh_token: "rt".into(),
                granted_scopes: Vec::new(),
            }),
            None,
        );
        *source.dead.lock().unwrap_or_else(PoisonError::into_inner) = true;
        assert!(matches!(
            refresh_single_flight(&source, "acct", false, 1_000),
            Err(RefreshFailure::Terminal)
        ));
    }
}
