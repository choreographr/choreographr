//! Wire test for the socket-registry integration in `build_agent`.
//!
//! Builds a real [`OpenAiClient`] whose HTTP agent dials through
//! choreo-sockreg's `RegisteringTcpConnector` (the connector chain wired in
//! `shared::build_agent`), points it at the scripted loopback HTTP provider,
//! and asserts that the connection actually shows up in the registry — i.e.
//! the registry-registered connector chain is live end-to-end, not just
//! type-correct.
//!
//! These tests bind a real local TCP socket, so per AGENTS.md they live in
//! `tests/` and are marked `#[ignore]` (run via `cargo test-integration`).

use choreo_ai_protocols::openai::{MaxTokensField, OpenAiClient, ServiceConfig};
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{ChatTurnRequest, ChatTurnResult, SocketRegistry};

#[test]
#[ignore]
fn provider_connection_is_registered_and_shutdown_all_does_not_panic() {
    let registry = SocketRegistry::new();

    let response = r#"{
        "choices":[{"message":{"content":"hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }"#;
    let mock = MockProvider::start(vec![(200, "application/json", response.to_string())]);

    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "deepseek".to_string(),
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        },
        "test-key".to_string(),
        &registry,
    )
    .expect("openai client");

    let user_message =
        choreo_ai_protocols::openai::ChatRequestMessage::simple("user", "hi".to_string());
    let result = client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-flash",
            messages: &[user_message],
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
        })
        .expect("turn succeeds over the registered connection");
    assert!(matches!(result, ChatTurnResult::FinalText(_)));

    // The connector registered a duplicate of the dialed socket. (Count is
    // >= 1 rather than == 1 because ureq may pool the connection for reuse;
    // pooled sockets keep their registry fd alive.)
    assert!(
        registry.registered_count() >= 1,
        "the dialed connection must appear in the socket registry"
    );

    // Expected semantics: shutdown_all force-closes every registered socket,
    // INCLUDING ones back in ureq's pool — that is the intended behavior
    // (pooled idle connections are exactly what a control thread wants to be
    // able to kill). The client still holds its twin fd; closing the registry
    // copy does not double-close anything (each fd is closed exactly once,
    // see choreo-sockreg's ownership contract). Must not panic.
    registry.shutdown_all();
    assert_eq!(
        registry.registered_count(),
        0,
        "shutdown_all clears the list"
    );
    // Idempotent: shutting down an empty registry is a no-op.
    registry.shutdown_all();
}
