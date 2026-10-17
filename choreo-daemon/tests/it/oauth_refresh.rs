//! Provider OAuth refresh integration tests.
//!
//! Three behaviours, each against a real local TCP stub (never a real provider):
//!
//! 1. **Single-flight spend-once**: many concurrent refreshes of one account
//!    must spend the single-use refresh token exactly once — the losers reuse
//!    the winner's persisted token instead of racing the stub.
//! 2. **Rotation persistence survives a lock/unlock cycle**: a refresh that
//!    rotates the refresh token re-encrypts the credential to the daemon's
//!    binding, so a `/lock` → `/unlock` re-reads the ROTATED token.
//! 3. **401 → refresh → rebuild → retry**: an OAuth account whose request is
//!    rejected with 401 is transparently refreshed, its client rebuilt with the
//!    new access token, and the request retried once against a stub provider.
//!
//! These bind real TCP sockets (the stub token endpoint + provider), so per
//! AGENTS.md they live in `tests/` and are marked `#[ignore = "integration"]`.

#![expect(clippy::unwrap_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use choreo_ai_protocols::catalog::GrantEncoding;
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_daemon::accounts::AccountConfig;
use choreo_daemon::broadcast::{ClientId, LagLimits, SubscriberSink};
use choreo_daemon::cache_warm::WarmPolicy;
use choreo_daemon::daemon::{DaemonCommand, ResolvedAccount};
use choreo_daemon::oauth_refresh::{
    CachedAccessToken, OAuthTokenSource, RefreshMaterial, RefreshTokenOutcome, RefreshedTokens,
    TokenLoad, refresh_single_flight,
};
use choreo_daemon::providers::ProviderAuth;
use choreo_daemon::{DaemonState, RequestContext, SessionCommand, session_main};
use choreo_keystore::{OAuthGrantEncoding, OAuthKind, ServiceCredential};
use choreo_proto::{DaemonMessage, DaemonMessageType, OutputStream, SessionEvent};
use zeroize::Zeroizing;

use crate::common;

/// A JSON token-endpoint success body with a fixed access token and a rotated
/// refresh token.
fn token_body(access_token: &str, refresh_token: Option<&str>) -> String {
    match refresh_token {
        Some(rt) => format!(
            r#"{{"access_token":"{access_token}","refresh_token":"{rt}","expires_in":3600,"token_type":"Bearer"}}"#
        ),
        None => format!(
            r#"{{"access_token":"{access_token}","expires_in":3600,"token_type":"Bearer"}}"#
        ),
    }
}

/// A provider OAuth credential for the refresh-material tests.
fn oauth_credential(account_id: Option<&str>, refresh_token: &str) -> ServiceCredential {
    ServiceCredential::OAuth {
        kind: OAuthKind::Provider,
        issuer: Some("http://127.0.0.1:1".into()),
        resource: None,
        authorization_server: None,
        // The refresh path reads these recorded fields directly; a bogus token
        // endpoint is fine because these tests drive the exchange through a stub
        // `OAuthTokenSource`, not this credential.
        token_endpoint: Some("http://127.0.0.1:1/token".into()),
        refresh_encoding: Some(OAuthGrantEncoding::Json),
        refresh_token: Some(refresh_token.into()),
        account_id: account_id.map(str::to_string),
        client_id: Some("client-abc".into()),
        client_secret: None,
        granted_scopes: vec!["openid".into()],
        token_type: Some("Bearer".into()),
    }
}

// ── Test 1: single-flight spend-once ────────────────────────────────────

/// A source that hits a stub token endpoint and records the exchange as a fresh
/// cached token on save (so a loser of the race reuses it). `grants` counts the
/// exchanges the stub actually served.
struct CountingSource {
    material: RefreshMaterial,
    cached: Mutex<Option<CachedAccessToken>>,
    grants: Arc<AtomicUsize>,
}

impl OAuthTokenSource for CountingSource {
    fn load(&self, _account: &str) -> TokenLoad {
        let cached = self
            .cached
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| CachedAccessToken {
                access_token: c.access_token.clone(),
                expires_at: c.expires_at,
                received_at: c.received_at,
            });
        TokenLoad {
            cached,
            material: Some(RefreshMaterial {
                token_endpoint: self.material.token_endpoint.clone(),
                encoding: self.material.encoding,
                client_id: self.material.client_id.clone(),
                client_secret: self.material.client_secret.clone(),
                refresh_token: self.material.refresh_token.clone(),
                granted_scopes: self.material.granted_scopes.clone(),
            }),
            dead: false,
        }
    }

    fn save(
        &self,
        _account: &str,
        _expected_refresh_token: Option<&str>,
        tokens: &RefreshedTokens,
    ) -> Result<(), String> {
        self.grants.fetch_add(1, Ordering::SeqCst);
        *self.cached.lock().unwrap() = Some(CachedAccessToken {
            access_token: tokens.access_token.clone(),
            expires_at: Some(tokens.received_at + 3_600),
            received_at: tokens.received_at,
        });
        Ok(())
    }

    fn report_terminal(&self, _account: &str) {}
}

#[test]
#[ignore = "integration"]
fn concurrent_refreshes_spend_the_refresh_token_once() {
    // The stub serves the token endpoint; its request count is the number of
    // grants actually spent. A rotating provider would invalidate every grant
    // after the first, so more than one request here means a real spend-twice
    // bug.
    let stub = MockProvider::start(vec![(
        200,
        "application/json",
        token_body("at-new", Some("rt-rotated")),
    )]);
    let grants = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(CountingSource {
        material: RefreshMaterial {
            token_endpoint: stub.base_url("token"),
            encoding: GrantEncoding::Json,
            client_id: Some("client-abc".into()),
            client_secret: None,
            refresh_token: Zeroizing::new("rt-original".into()),
            granted_scopes: vec!["openid".into()],
        },
        cached: Mutex::new(None),
        grants: Arc::clone(&grants),
    });

    // Eight threads race one refresh; the single-flight lock must serialize
    // them so only the first exchanges and the rest reuse its token.
    let now = 1_700_000_000i64;
    let mut handles = Vec::new();
    for _ in 0..8 {
        let source = Arc::clone(&source);
        handles.push(std::thread::spawn(move || {
            refresh_single_flight(source.as_ref(), "acct", false, now)
        }));
    }
    let outcomes: Vec<RefreshTokenOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("refresh thread").expect("refresh succeeds"))
        .collect();

    // Exactly one thread performed the exchange; the rest reused the result.
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == RefreshTokenOutcome::Refreshed)
            .count(),
        1,
        "exactly one refresh must occur: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .filter(|o| **o == RefreshTokenOutcome::NotNeeded)
            .count()
            == 7,
        "the seven losers must reuse the winner's token: {outcomes:?}"
    );
    // `save` is our own counter of exchanges (the stub repeats its last
    // response, so assert on the source's count).
    assert_eq!(
        grants.load(Ordering::SeqCst),
        1,
        "the refresh token must be spent exactly once"
    );
}

// ── Test 2: rotation persistence survives lock/unlock ───────────────────

#[test]
#[ignore = "integration"]
fn rotation_persists_through_a_lock_unlock_cycle() {
    let mut state: DaemonState = common::test_daemon_state();
    // Bind (TOFU) with a fixed 32-byte key: bind is the sole adopt path and
    // leaves the daemon unlocked.
    let key = vec![7u8; 32];
    let (bind_ack, _bind_rx) = std::sync::mpsc::channel();
    state.handle_command(DaemonCommand::BindKeystore {
        key: key.clone(),
        reply: None,
        ack: bind_ack,
    });
    assert!(!state.locked, "bind must leave the keystore unlocked");

    // Store an OAuth credential whose refresh token is the ORIGINAL.
    let (save_ack, _save_rx) = std::sync::mpsc::channel();
    state.handle_command(DaemonCommand::SaveCredential {
        service: "acct".into(),
        credential: oauth_credential(Some("acct-old"), "rt-original"),
        reply: None,
        ack: save_ack,
    });

    // A refresh rotated the refresh token and the identity claim, and produced
    // a new access token: persist it.
    let (save_reply, save_rx) = std::sync::mpsc::channel();
    state.handle_command(DaemonCommand::SaveOAuthToken {
        account: "acct".into(),
        access_token: Zeroizing::new("at-new".into()),
        expires_at: Some(1_700_003_600),
        received_at: 1_700_000_000,
        rotated_refresh_token: Some(Zeroizing::new("rt-rotated".into())),
        new_account_id: Some("acct-new".into()),
        // The refresh presented the stored credential's token, so the rotation
        // applies rather than being dropped as a superseded grant.
        expected_refresh_token: Some(Zeroizing::new("rt-original".into())),
        reply: save_reply,
    });
    save_rx.recv().expect("save reply").expect("save ok");

    // The in-memory credential now carries the rotation.
    let rotated = state
        .credentials
        .get("acct")
        .and_then(ServiceCredential::as_oauth)
        .expect("oauth credential present");
    assert_eq!(rotated.refresh_token, Some("rt-rotated"));
    assert_eq!(rotated.account_id, Some("acct-new"));

    // A `/lock` wipes the cleartext (credential + token cache).
    let (lock_reply, lock_rx) = std::sync::mpsc::channel();
    state.handle_command(DaemonCommand::Lock { reply: lock_reply });
    lock_rx.recv().expect("lock reply").expect("lock ok");
    assert!(state.locked);
    assert!(state.credentials.is_empty(), "lock must wipe credentials");
    assert!(state.access_tokens.is_empty(), "lock must wipe tokens");

    // A `/unlock` re-decrypts the persisted blob — which must be the ROTATED
    // one, proving the rotation was re-encrypted to the binding.
    let (unlock_ack, _unlock_rx) = std::sync::mpsc::channel();
    state.handle_command(DaemonCommand::Unlock {
        private_key: key,
        reply: None,
        ack: unlock_ack,
    });
    assert!(!state.locked, "unlock must succeed with the bound key");
    let reread = state
        .credentials
        .get("acct")
        .and_then(ServiceCredential::as_oauth)
        .expect("credential re-read after unlock");
    assert_eq!(
        reread.refresh_token,
        Some("rt-rotated"),
        "the rotated refresh token must survive lock/unlock"
    );
    assert_eq!(reread.account_id, Some("acct-new"));
}

// ── Test 3: 401 → refresh → rebuild → retry ─────────────────────────────

/// SSE body delivering one answer delta, then `[DONE]`.
fn sse_text_stream(text: &str) -> String {
    let payload = serde_json::json!({ "choices": [{ "delta": { "content": text } }] });
    format!("data: {payload}\n\ndata: [DONE]\n\n")
}

/// The model slug used by the 401-loop test; installed into a tiny catalog
/// (Chat Completions, not Responses) so the mock speaks plain chat SSE.
const OAUTH_MODEL: &str = "oauth-mock-4o";

/// Restores the process-wide catalog on drop so a failing swap test cannot
/// leave the tiny catalog installed for later tests (relevant only under the
/// libtest fallback; nextest isolates each test in its own process).
struct CatalogGuard(Arc<Vec<choreo_ai_protocols::ProviderEntry>>);

impl Drop for CatalogGuard {
    fn drop(&mut self) {
        choreo_ai_protocols::replace_catalog((*self.0).clone());
    }
}

/// Install a one-provider/one-model catalog whose `openai` entry is a plain
/// Chat Completions model, so the provider client speaks the mock's chat SSE.
fn install_catalog() -> CatalogGuard {
    use choreo_ai_protocols::catalog::ModelEntry;
    use choreo_ai_protocols::openai::MaxTokensField;
    use choreo_ai_protocols::{ProviderEntry, ProviderProtocol, RequestCompression};

    let saved = choreo_ai_protocols::catalog_snapshot();
    let entry = ProviderEntry {
        slug: "openai".into(),
        display_name: "OpenAI".into(),
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.openai.com/v1".into(),
        default_model: OAUTH_MODEL.into(),
        prompt_cache: None,
        request_compression: RequestCompression::Identity,
        oauth: None,
        models: vec![ModelEntry {
            model: OAUTH_MODEL.into(),
            context_window: 128_000,
            reasoning_supported: false,
            openai_responses: false,
            max_output_tokens: 4096,
            ..Default::default()
        }],
    };
    choreo_ai_protocols::replace_catalog(vec![entry]);
    CatalogGuard(saved)
}

/// Shared state the fake daemon command loop serves: the account base URL, the
/// token exchange endpoint, and the current access token.
struct FakeDaemon {
    provider_base: String,
    token_endpoint: String,
    access_token: Mutex<Option<String>>,
    expires_at: Mutex<Option<i64>>,
}

/// Spawn a fake daemon command loop that answers the session's resolve /
/// OAuth-token commands from `state`. The returned sender is the session's
/// `daemon_tx`.
fn spawn_fake_daemon(
    state: Arc<FakeDaemon>,
) -> (
    crossbeam_channel::Sender<DaemonCommand>,
    std::thread::JoinHandle<()>,
) {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let handle = std::thread::spawn(move || {
        while let Ok(cmd) = daemon_rx.recv() {
            match cmd {
                DaemonCommand::ResolveAccountCmd { account, reply } => {
                    let mut config = AccountConfig::simple(&account, "openai");
                    config.base_url = Some(state.provider_base.clone());
                    config.streaming = Some(true);
                    config.retry_max_attempts = Some(1);
                    config.connect_timeout_secs = Some(5);
                    config.request_timeout_secs = Some(30);
                    config.total_timeout_secs = Some(60);
                    let access_token = state
                        .access_token
                        .lock()
                        .unwrap()
                        .clone()
                        .map(Zeroizing::new);
                    let auth = ProviderAuth::OAuth {
                        credential: Box::new(oauth_credential(Some("acct-1"), "rt-original")),
                        access_token,
                    };
                    let _ = reply.send(Some(ResolvedAccount {
                        config,
                        auth: Some(auth),
                        warm_policy: WarmPolicy::default(),
                    }));
                }
                DaemonCommand::LoadOAuthToken { reply, .. } => {
                    let access_token = state.access_token.lock().unwrap().clone();
                    let cached = access_token.map(|t| CachedAccessToken {
                        access_token: Zeroizing::new(t),
                        expires_at: *state.expires_at.lock().unwrap(),
                        received_at: 1_700_000_000,
                    });
                    let _ = reply.send(TokenLoad {
                        cached,
                        material: Some(RefreshMaterial {
                            token_endpoint: state.token_endpoint.clone(),
                            encoding: GrantEncoding::Json,
                            client_id: Some("client-abc".into()),
                            client_secret: None,
                            refresh_token: Zeroizing::new("rt-original".into()),
                            granted_scopes: vec!["openid".into()],
                        }),
                        dead: false,
                    });
                }
                DaemonCommand::SaveOAuthToken {
                    access_token,
                    expires_at,
                    reply,
                    ..
                } => {
                    *state.access_token.lock().unwrap() = Some(access_token.to_string());
                    *state.expires_at.lock().unwrap() = expires_at;
                    let _ = reply.send(Ok(()));
                }
                DaemonCommand::MarkOAuthDead { .. } => {}
                _ => {}
            }
        }
    });
    (daemon_tx, handle)
}

/// Drive a session's broadcast stream until `Done`, returning the concatenated
/// answer text.
fn drain_answer(rx: &crossbeam_channel::Receiver<DaemonMessage>) -> Vec<u8> {
    let mut answer = Vec::new();
    loop {
        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("timed out waiting for daemon message: {e:?}"));
        match msg.inner {
            DaemonMessageType::Session {
                event:
                    SessionEvent::OutputChunk {
                        stream: OutputStream::Answer,
                        data,
                        ..
                    },
                ..
            } => answer.extend_from_slice(&data),
            DaemonMessageType::Session {
                event: SessionEvent::Done { .. },
                ..
            } => break,
            DaemonMessageType::Session {
                event: SessionEvent::Failed { .. },
                ..
            } => break,
            _ => {}
        }
    }
    answer
}

#[test]
#[ignore = "integration"]
fn unauthorized_refreshes_rebuilds_and_retries_once() {
    // The provider stub 401s the first request, then streams a final answer for
    // the retried request. A 401 is a non-retryable status, so the client does
    // not retry it internally — the retry must come from the OAuth path.
    let _catalog = install_catalog();
    let provider = MockProvider::start(vec![
        (
            401,
            "application/json",
            r#"{"error":"expired"}"#.to_string(),
        ),
        (200, "text/event-stream", sse_text_stream("final answer")),
    ]);
    let token_stub = MockProvider::start(vec![(
        200,
        "application/json",
        token_body("new-token", Some("rt-rotated")),
    )]);

    // The account base URL points at the provider stub; the token exchange
    // points at the token stub. A cached token that is NOT near expiry means
    // the proactive path does not refresh — the 401 must drive the refresh.
    let provider_base = provider.base_url("v1");
    let state = Arc::new(FakeDaemon {
        provider_base: provider_base.clone(),
        token_endpoint: token_stub.base_url("token"),
        access_token: Mutex::new(Some("old-token".into())),
        expires_at: Mutex::new(Some(i64::MAX)),
    });
    let (daemon_tx, daemon_handle) = spawn_fake_daemon(Arc::clone(&state));

    let (session_tx, session_rx) = crossbeam_channel::unbounded();
    let cmd_tx = session_tx.clone();
    let db = Arc::new(common::test_db());
    let tool_registry = choreo_daemon::tools::ToolRegistry::new()
        .build()
        .into_shared();
    let session_handle = std::thread::spawn(move || {
        session_main(
            &session_rx,
            None,
            choreo_ai_protocols::SocketRegistry::default(),
            Some("oauth-acct".to_string()),
            Some("openai".to_string()),
            None,
            &RequestContext {
                cmd_tx,
                session_id: 1,
                db,
                tool_registry,
                daemon_tx,
                max_turns: 0,
                lag_limits: LagLimits::default(),
                global_lag: Arc::new(AtomicUsize::new(0)),
                substrate_credential: None,
                warm_policy: WarmPolicy::default(),
            },
        );
    });

    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    session_tx
        .send(SessionCommand::Attach {
            client_id: ClientId::from_raw(10),
            tx: SubscriberSink::new(tx),
        })
        .expect("attach");
    session_tx
        .send(SessionCommand::SetModel {
            model: OAUTH_MODEL.to_string(),
            reply: None,
        })
        .expect("set model");
    session_tx
        .send(SessionCommand::RunInput {
            input: b"hello".to_vec(),
            reply: None,
        })
        .expect("run input");

    let answer = drain_answer(&rx);
    assert_eq!(answer, b"final answer", "the retried request must succeed");

    // The provider saw the 401 then the retried request; the retry carried the
    // REFRESHED access token, proving the client was rebuilt.
    let provider_requests = provider.requests();
    assert_eq!(
        provider_requests.len(),
        2,
        "expected the 401 then exactly one retry, got {}",
        provider_requests.len()
    );
    assert_eq!(
        provider_requests[1].header("authorization").unwrap_or(""),
        "Bearer new-token",
        "the retry must carry the refreshed access token"
    );
    // The token stub was hit exactly once (the forced 401 refresh).
    assert_eq!(
        token_stub.requests().len(),
        1,
        "the 401 must trigger exactly one forced refresh"
    );

    session_tx.send(SessionCommand::Shutdown).expect("shutdown");
    drop(session_tx);
    session_handle.join().expect("session thread panicked");
    daemon_handle.join().expect("fake daemon thread");
}

// ── Test 4: a transient proactive refresh falls back to the cached token ──

#[test]
#[ignore = "integration"]
fn transient_proactive_refresh_falls_back_to_the_cached_token() {
    // A cached access token that is inside the expiry buffer drives the
    // proactive, pre-request refresh. The token endpoint answers 500 — a
    // transient failure — so the session must fall through with the cached
    // bearer rather than failing the whole request. The provider accepts it,
    // proving the request proceeded on the cached token.
    let _catalog = install_catalog();
    let provider = MockProvider::start(vec![(
        200,
        "text/event-stream",
        sse_text_stream("final answer"),
    )]);
    // Every token-endpoint call transiently fails (a 5xx is retryable, so it
    // classifies as a `Transient` refresh failure, never a dead grant).
    let token_stub = MockProvider::start(vec![(
        500,
        "application/json",
        r#"{"error":"server_error"}"#.to_string(),
    )]);

    let provider_base = provider.base_url("v1");
    let state = Arc::new(FakeDaemon {
        provider_base: provider_base.clone(),
        token_endpoint: token_stub.base_url("token"),
        // A cached token whose reported expiry is already past: the proactive
        // refresh fires, yet a bearer is still present to fall back on.
        access_token: Mutex::new(Some("old-token".into())),
        expires_at: Mutex::new(Some(0)),
    });
    let (daemon_tx, daemon_handle) = spawn_fake_daemon(Arc::clone(&state));

    let (session_tx, session_rx) = crossbeam_channel::unbounded();
    let cmd_tx = session_tx.clone();
    let db = Arc::new(common::test_db());
    let tool_registry = choreo_daemon::tools::ToolRegistry::new()
        .build()
        .into_shared();
    let session_handle = std::thread::spawn(move || {
        session_main(
            &session_rx,
            None,
            choreo_ai_protocols::SocketRegistry::default(),
            Some("oauth-acct".to_string()),
            Some("openai".to_string()),
            None,
            &RequestContext {
                cmd_tx,
                session_id: 1,
                db,
                tool_registry,
                daemon_tx,
                max_turns: 0,
                lag_limits: LagLimits::default(),
                global_lag: Arc::new(AtomicUsize::new(0)),
                substrate_credential: None,
                warm_policy: WarmPolicy::default(),
            },
        );
    });

    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    session_tx
        .send(SessionCommand::Attach {
            client_id: ClientId::from_raw(11),
            tx: SubscriberSink::new(tx),
        })
        .expect("attach");
    session_tx
        .send(SessionCommand::SetModel {
            model: OAUTH_MODEL.to_string(),
            reply: None,
        })
        .expect("set model");
    session_tx
        .send(SessionCommand::RunInput {
            input: b"hello".to_vec(),
            reply: None,
        })
        .expect("run input");

    let answer = drain_answer(&rx);
    assert_eq!(
        answer, b"final answer",
        "the request must proceed on the cached token despite the refresh blip"
    );

    // The provider saw exactly one request, carrying the CACHED bearer: the
    // failed proactive refresh did not fail the request and did not rebuild the
    // client with a fresh token.
    let provider_requests = provider.requests();
    assert_eq!(
        provider_requests.len(),
        1,
        "the request must not be retried: the cached token was accepted"
    );
    assert_eq!(
        provider_requests[0].header("authorization").unwrap_or(""),
        "Bearer old-token",
        "the request must carry the cached access token"
    );
    // The token endpoint was hit exactly once (the failed proactive refresh).
    assert_eq!(
        token_stub.requests().len(),
        1,
        "the proactive refresh must have been attempted exactly once"
    );

    session_tx.send(SessionCommand::Shutdown).expect("shutdown");
    drop(session_tx);
    session_handle.join().expect("session thread panicked");
    daemon_handle.join().expect("fake daemon thread");
}
