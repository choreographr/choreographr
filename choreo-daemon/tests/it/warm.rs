//! Cache-warming integration tests (phase 4b runtime behaviour).
//!
//! These drive the REAL session agent loop (`session_main` +
//! `run_request_worker` + `run_agent_loop`) with a scripted OpenAI-compatible
//! provider, a long simulated tool call, and a SHORT prompt-cache TTL, so the
//! warmer's timer fires while the tool blocks — the exact window the feature
//! exists for. They assert:
//!
//! * a warm ping fires, with a 1-token output cap (`max_tokens == 1`) and the
//!   SAME message prefix the just-sent turn used;
//! * the ping never enters session context or turn state (its distinctive body
//!   text appears nowhere in the finalized transcript);
//! * a `requests`-metered account (like `flat`/`unknown`) never pings, even
//!   though its prefix is large enough for the `tokens` gate.
//!
//! The harness reuses the crate's existing session-level scaffolding (a real
//! `session_main` loop + `MockProvider`), NOT the daemon socket server: the
//! daemon's catalog-maintenance thread swaps the process-wide catalog at
//! startup, which would race the per-test short-TTL override below. The
//! session-level path leaves the catalog under the test's control, and warming
//! only needs the session loop (it is armed in `run_agent_loop`, not the
//! command loop).
//!
//! These bind a real local TCP socket (the mock provider), so per AGENTS.md
//! they live in `tests/` and are marked `#[ignore = "integration"]`.

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![expect(clippy::expect_used, clippy::panic)]
use choreo_ai_protocols::catalog::{ModelCost, ModelEntry, PromptCacheTtl, ProviderEntry};
use choreo_ai_protocols::openai::{MaxTokensField, OpenAiClient, ServiceConfig};
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{ProviderProtocol, catalog_snapshot, replace_catalog};
use choreo_daemon::broadcast::{ClientId, LagLimits, SubscriberSink};
use choreo_daemon::cache_warm::{CacheWarmingMode, MeterKind, WarmPolicy};
use choreo_daemon::providers::InferenceProvider;
use choreo_daemon::{RequestContext, SessionCommand, session_main};
use choreo_proto::{DaemonMessage, DaemonMessageType, OutputStream, SessionEvent};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use crate::common;

/// The model slug used by every request; installed into the short-TTL catalog
/// so `prompt_cache_ttl("openai", MODEL)` resolves.
const MODEL: &str = "warm-mock-4o";

/// The distinctive prefix on the ping's RESPONSE body. It must never surface in
/// the session transcript — that is the "did not enter session context or turn
/// state" assertion.
const PING_MARKER: &str = "WARM-PING-MUST-NOT-LEAK";

/// A bounded receive timeout for the session stream — the same generous budget
/// `stream_integrity.rs` uses, so a wedged session fails loudly instead of
/// hanging the suite.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Restores the process-wide catalog on drop so a failing swap test can never
/// leave the tiny catalog installed for later tests (relevant only under the
/// libtest fallback; nextest isolates each test in its own process).
struct CatalogGuard(Arc<Vec<ProviderEntry>>);

impl Drop for CatalogGuard {
    fn drop(&mut self) {
        replace_catalog((*self.0).clone());
    }
}

/// Install a one-provider/one-model catalog whose `openai` entry declares a
/// SHORT (13 s) prompt-cache TTL. 13 s yields a warm delay of
/// `min(13*0.9, 13-10) == 3 s` (see `cache_warm::warm_delay_secs`), so the ping
/// fires ~3 s into the tool run; the tool runs ~4 s, so exactly ONE ping lands
/// inside it (the plan arms the next ping 3 s after the first — past the tool's
/// end, and the request is stopped before it fires).
fn install_short_ttl_catalog() -> CatalogGuard {
    let saved = catalog_snapshot();
    let entry = ProviderEntry {
        slug: "openai".into(),
        display_name: "OpenAI".into(),
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.openai.com/v1".into(),
        default_model: MODEL.into(),
        // 13 s short tier -> a 3 s warm delay; no extended tier.
        prompt_cache: Some(PromptCacheTtl {
            short_secs: Some(13),
            long_secs: None,
        }),
        models: vec![ModelEntry {
            model: MODEL.into(),
            context_window: 128_000,
            // Non-reasoning, Chat Completions (NOT Responses) so the mock
            // speaks plain chat-completions SSE.
            reasoning_supported: false,
            openai_responses: false,
            max_output_tokens: 4096,
            cost: Some(ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: Some(0.3),
                cache_write: Some(3.75),
            }),
            ..Default::default()
        }],
    };
    replace_catalog(vec![entry]);
    CatalogGuard(saved)
}

/// An OpenAI-compatible provider (slug "openai") pointed at the mock base URL,
/// with streaming enabled — the turn loop streams, the ping does not (the
/// non-streaming `chat_completion_turn` path).
fn mock_openai_provider(base_url: String) -> InferenceProvider {
    let client = OpenAiClient::new(
        ServiceConfig {
            base_url,
            provider_slug: "openai".to_string(),
            streaming: true,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        },
        "test-key".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .expect("openai client");
    InferenceProvider::from_openai(client)
}

/// SSE body that emits a single `sh` tool call running `command`, then `[DONE]`.
fn sse_tool_use(command: &str) -> String {
    let args = serde_json::json!({ "command": command, "shell": "bash" });
    let payload = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "sh", "arguments": args.to_string() }
                }]
            }
        }]
    });
    format!("data: {payload}\n\ndata: [DONE]\n\n")
}

/// SSE body delivering one answer delta, then `[DONE]`.
fn sse_text_stream(text: &str) -> String {
    let payload = serde_json::json!({ "choices": [{ "delta": { "content": text } }] });
    format!("data: {payload}\n\ndata: [DONE]\n\n")
}

/// A plain (non-SSE) chat-completions body — the ping's non-streaming response.
/// Its content carries the distinctive [`PING_MARKER`].
fn ping_body() -> String {
    serde_json::json!({
        "choices": [{
            "message": { "role": "assistant", "content": PING_MARKER, "tool_calls": [] },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11 }
    })
    .to_string()
}

/// Spawn a real `session_main` loop with the given provider and warming policy.
fn spawn_session_with(
    provider: InferenceProvider,
    warm_policy: WarmPolicy,
) -> (
    crossbeam_channel::Sender<SessionCommand>,
    std::thread::JoinHandle<()>,
) {
    let db = Arc::new(common::test_db());
    // The daemon command channel has a DROPPED receiver (same as the other
    // session-level tests): session→daemon messages fail silently, and
    // `SetModel` validation falls back to "allow".
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let (session_tx, session_rx) = crossbeam_channel::unbounded();
    let tool_registry = choreo_daemon::tools::ToolRegistry::new()
        .build()
        .into_shared();
    let cmd_tx = session_tx.clone();
    let handle = std::thread::spawn(move || {
        session_main(
            &session_rx,
            Some(provider),
            choreo_ai_protocols::SocketRegistry::default(),
            None,
            None,
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
                warm_policy,
            },
        );
    });
    (session_tx, handle)
}

/// Collect every broadcast message until `Done`. Returns the concatenated
/// Answer chunks and a blob of every assistant/tool text that entered the
/// transcript (to assert the ping never leaked into it).
fn drain_until_done(rx: &crossbeam_channel::Receiver<DaemonMessage>) -> (Vec<u8>, String) {
    let mut answer = Vec::new();
    let mut transcript = String::new();
    loop {
        let msg = rx
            .recv_timeout(TIMEOUT)
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
                event: SessionEvent::TurnAppended { turn, .. },
                ..
            } => {
                if let Some(text) = &turn.assistant_text {
                    transcript.push_str(text);
                }
                for result in &turn.tool_results {
                    transcript.push_str(&result.content);
                }
            }
            DaemonMessageType::Session {
                event: SessionEvent::Done { .. },
                ..
            } => break,
            _ => {}
        }
    }
    (answer, transcript)
}

/// The output-token cap in a chat-completions request body, if present.
fn max_tokens_of(body: &serde_json::Value) -> Option<u64> {
    ["max_completion_tokens", "max_tokens", "max_output_tokens"]
        .iter()
        .find_map(|key| body.get(*key).and_then(serde_json::Value::as_u64))
}

/// A `tokens`-metered streaming policy with a trivially low prefix gate.
fn tokens_policy() -> WarmPolicy {
    WarmPolicy {
        mode: CacheWarmingMode::Streaming,
        meter: MeterKind::Tokens,
        prompt_cache_enabled: true,
        min_prefix_tokens: 1,
        min_expected_savings: 0.0,
    }
}

#[test]
#[ignore = "integration"]
fn warm_ping_fires_during_a_tool_run_and_stays_out_of_context() {
    let _catalog = install_short_ttl_catalog();

    // Turn 1: a tool call that blocks ~4 s. The ping (delay ~3 s) fires during
    // it. The ping response is the non-SSE JSON body. Turn 2: the final answer.
    let mock = MockProvider::start(vec![
        (200, "text/event-stream", sse_tool_use("sleep 4")),
        (200, "application/json", ping_body()),
        (200, "text/event-stream", sse_text_stream("final answer")),
    ]);
    let provider = mock_openai_provider(mock.base_url("v1"));
    let (session_tx, session_handle) = spawn_session_with(provider, tokens_policy());

    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    session_tx
        .send(SessionCommand::Attach {
            client_id: ClientId::from_raw(10),
            tx: SubscriberSink::new(tx),
        })
        .expect("attach");
    session_tx
        .send(SessionCommand::SetModel {
            model: MODEL.to_string(),
            reply: None,
        })
        .expect("set model");
    session_tx
        .send(SessionCommand::RunInput {
            input: b"run the tool".to_vec(),
            reply: None,
        })
        .expect("run input");

    let (answer, transcript) = drain_until_done(&rx);
    assert_eq!(answer, b"final answer", "the real turn's answer stream");

    // The ping never entered the transcript.
    assert!(
        !transcript.contains(PING_MARKER),
        "the warm ping must not enter session context or turn state: {transcript}"
    );

    let requests = mock.requests();
    // At least three provider calls: turn 1, one or more warm pings, turn 2.
    // The warmer can ping more than once if the tool window overruns the ping
    // interval; the mock repeats its last response, so turn 2 still receives the
    // final SSE even when an extra ping consumes the middle response.
    assert!(
        requests.len() >= 3,
        "expected turn1 + ping(s) + turn2, got {}",
        requests.len()
    );

    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(choreo_ai_protocols::test_utils::CapturedRequest::body_json)
        .collect();
    let ping_idx = bodies
        .iter()
        .position(|b| max_tokens_of(b) == Some(1))
        .expect("at least one warm ping must carry the 1-token cap");

    // The ping re-sends the SAME prefix the real turn sent (a cache hit needs
    // byte-identical messages).
    assert_eq!(
        bodies[ping_idx]["messages"], bodies[0]["messages"],
        "the ping must replay the just-sent prefix verbatim"
    );

    session_tx.send(SessionCommand::Shutdown).expect("shutdown");
    drop(session_tx);
    session_handle.join().expect("session thread panicked");
}

#[test]
#[ignore = "integration"]
fn requests_metered_account_never_pings() {
    let _catalog = install_short_ttl_catalog();

    let mock = MockProvider::start(vec![
        (200, "text/event-stream", sse_tool_use("sleep 4")),
        // Only two responses needed: no ping fires, so turn 2 takes the second.
        (200, "text/event-stream", sse_text_stream("final answer")),
    ]);
    let provider = mock_openai_provider(mock.base_url("v1"));
    // A request-metered account: a ping adds to the window and saves none, so
    // the gate declines even with a large prefix.
    let policy = WarmPolicy {
        meter: MeterKind::Requests,
        ..tokens_policy()
    };
    let (session_tx, session_handle) = spawn_session_with(provider, policy);

    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    session_tx
        .send(SessionCommand::Attach {
            client_id: ClientId::from_raw(10),
            tx: SubscriberSink::new(tx),
        })
        .expect("attach");
    session_tx
        .send(SessionCommand::SetModel {
            model: MODEL.to_string(),
            reply: None,
        })
        .expect("set model");
    session_tx
        .send(SessionCommand::RunInput {
            input: b"run the tool".to_vec(),
            reply: None,
        })
        .expect("run input");

    let (answer, _transcript) = drain_until_done(&rx);
    assert_eq!(answer, b"final answer");

    let requests = mock.requests();
    // Turn 1 + turn 2 only — a request-metered account never warms.
    assert_eq!(
        requests.len(),
        2,
        "a request-metered account must never ping, got {} requests",
        requests.len()
    );
    assert!(
        requests
            .iter()
            .all(|r| max_tokens_of(&r.body_json()) != Some(1)),
        "no request may carry a 1-token warm cap"
    );

    session_tx.send(SessionCommand::Shutdown).expect("shutdown");
    drop(session_tx);
    session_handle.join().expect("session thread panicked");
}
