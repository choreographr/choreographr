//! Anthropic Messages-API streaming tool-call wire tests: how a streamed
//! `tool_use` block's arguments are assembled from `input_json_delta`
//! fragments, and what the turn reports when they do not form a document.
//!
//! The wire puts a tool call's arguments in two different places: the
//! `content_block_start` event carries an `input` object (an EMPTY one in
//! practice) and the block's `input_json_delta` events carry the argument text
//! itself. These tests pin that the delta text — and only the delta text — is
//! the call's arguments, that the start object is a fallback used solely when
//! no delta arrives, and that a truncated document is discarded whole rather
//! than dispatched half-written.
//!
//! Every test binds a real local TCP socket through `MockProvider`, so per
//! AGENTS.md they live in `tests/` and are marked `#[ignore]` (run via
//! `cargo test-integration`).

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![expect(clippy::expect_used)]

use choreo_ai_protocols::anthropic::{AnthropicClient, AnthropicConfig, AnthropicError};
use choreo_ai_protocols::openai::{ChatRequestMessage, ChatToolDefinition};
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{ChatTurnRequest, ChatTurnResult, FinishOutcome, SocketRegistry};

/// Frame one SSE event the way the Messages API does: a named `event:` line
/// followed by its single `data:` line, terminated by a blank line.
fn sse(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// The `get_weather` tool the canned responses call.
fn tool_def() -> ChatToolDefinition {
    ChatToolDefinition::function(
        "get_weather",
        "Get the current weather for a city",
        serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        }),
    )
}

/// The opening half of every streamed turn: usage from `message_start`, then a
/// `tool_use` block whose start event carries the empty `input` object the API
/// sends, followed by the block's own argument delta events.
fn tool_use_stream(deltas: &[&str], stop_reason: &str) -> String {
    let mut body = sse(
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}"#,
    );
    body.push_str(&sse(
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
    ));
    for delta in deltas {
        let event = format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
            serde_json::Value::String((*delta).to_string()),
        );
        body.push_str(&sse("content_block_delta", &event));
    }
    body.push_str(&sse(
        "content_block_stop",
        r#"{"type":"content_block_stop","index":0}"#,
    ));
    body.push_str(&sse(
        "message_delta",
        &format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}"}},"usage":{{"output_tokens":24}}}}"#
        ),
    ));
    body.push_str(&sse("message_stop", r#"{"type":"message_stop"}"#));
    body
}

/// Point a streaming `AnthropicClient` at `body` and drive one turn against it.
fn stream_turn(body: String) -> Result<ChatTurnResult, AnthropicError> {
    let mock = MockProvider::start(vec![(200, "text/event-stream", body)]);
    let client = AnthropicClient::new(
        AnthropicConfig {
            base_url: mock.base_url(""),
            streaming: true,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            ..Default::default()
        },
        "test-key".to_string(),
        &SocketRegistry::new(),
    )
    .expect("anthropic client");

    let messages = vec![ChatRequestMessage::simple(
        "user",
        "What's the weather in London?".to_string(),
    )];
    let tools = vec![tool_def()];
    client.chat_completion_turn_streaming(
        ChatTurnRequest {
            model: "claude-haiku-4-5",
            messages: &messages,
            tools: &tools,
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            on_uploaded: None,
            cancel_rx: None,
            previous_response_id: None,
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        },
        &mut |_| Ok(()),
    )
}

#[test]
#[ignore = "integration"]
fn streamed_tool_call_arguments_are_the_delta_text() {
    // The regression this pins: the wire sends `{"input": {}}` on the block
    // start and the whole argument document as `input_json_delta` fragments.
    // Seeding the argument buffer with the start object concatenates the two
    // (`{}{"city": "London"}`), which is not JSON — so the accumulation must
    // be the delta text alone, and the call must survive validation with
    // exactly the arguments the provider streamed.
    let body = tool_use_stream(&[r#"{"city": "#, r#""London"}"#], "tool_use");
    let result = stream_turn(body).expect("a well-formed streamed tool call is a turn");
    let ChatTurnResult::ToolUse(turn) = result else {
        panic!("expected ToolUse, got {result:?}");
    };
    assert_eq!(turn.tool_calls.len(), 1, "one call: {:?}", turn.tool_calls);
    let call = &turn.tool_calls[0];
    assert_eq!(call.id, "toolu_1", "the block's own id names the call");
    assert_eq!(call.name, "get_weather");
    assert_eq!(
        call.arguments_json, r#"{"city": "London"}"#,
        "the arguments are the delta text, with no start-object prefix",
    );
    assert!(
        turn.discarded_tool_calls.is_empty(),
        "a valid call must not be discarded: {:?}",
        turn.discarded_tool_calls,
    );
    assert_eq!(
        turn.finish,
        FinishOutcome::Stop,
        "`tool_use` is a normal stop"
    );
}

#[test]
#[ignore = "integration"]
fn truncated_streamed_arguments_are_discarded_whole() {
    // The same whole-or-nothing rule the OpenAI paths apply runs here: a
    // document cut off by the provider is never dispatched half-written. The
    // daemon keys its recovery on `TruncatedToolCall`, and the provider's own
    // output cap must be reported as a length stop so that recovery knows why
    // the call arrived incomplete.
    let body = tool_use_stream(&[r#"{"city": "#], "max_tokens");
    let error = stream_turn(body).expect_err("a cut-off argument document is not a turn");
    let AnthropicError::TruncatedToolCall { discarded, finish } = error else {
        panic!("expected TruncatedToolCall, got {error:?}");
    };
    assert_eq!(
        discarded.len(),
        1,
        "the one call was discarded: {discarded:?}"
    );
    assert_eq!(
        finish,
        FinishOutcome::Length,
        "`max_tokens` is the provider's output cap, not a normal stop",
    );
}

#[test]
#[ignore = "integration"]
fn tool_call_without_deltas_falls_back_to_the_start_input() {
    // A provider that sends the complete input object on `content_block_start`
    // and no `input_json_delta` at all still yields its real arguments, and a
    // genuinely no-argument call yields `{}` rather than an empty string —
    // which would be discarded as unparseable and read as a truncation.
    let body = tool_use_stream(&[], "tool_use");
    let result = stream_turn(body).expect("a delta-less tool call is still a turn");
    let ChatTurnResult::ToolUse(turn) = result else {
        panic!("expected ToolUse, got {result:?}");
    };
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(
        turn.tool_calls[0].arguments_json, "{}",
        "the start-block input object is the fallback when no delta arrives",
    );
    assert!(
        turn.discarded_tool_calls.is_empty(),
        "a no-argument call is not a truncation: {:?}",
        turn.discarded_tool_calls,
    );
}
