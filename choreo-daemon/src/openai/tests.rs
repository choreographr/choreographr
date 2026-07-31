use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn build_sse_event_joins_multiple_data_lines() {
    let mut lines = vec![
        "event: message".to_string(),
        "data: hello".to_string(),
        "data: world".to_string(),
    ];
    let event = build_sse_event(&mut lines).expect("event");
    assert_eq!(event, "hello\nworld");
    assert!(lines.is_empty());
}

#[test]
fn build_sse_event_returns_done_marker() {
    let mut lines = vec!["data: [DONE]".to_string()];
    let event = build_sse_event(&mut lines).expect("event");
    assert_eq!(event, "[DONE]");
    assert!(lines.is_empty());
}

#[test]
fn extracts_responses_text_delta() {
    let delta =
        extract_responses_text_delta(r#"{"type":"response.output_text.delta","delta":"hello"}"#)
            .expect("extract")
            .expect("delta");
    assert_eq!(delta, "hello");

    assert!(
        extract_responses_text_delta(r#"{"type":"response.output_text.done"}"#)
            .expect("extract done")
            .is_none()
    );
}

#[test]
fn retryable_statuses() {
    assert!(is_retryable_status(429)); // TOO_MANY_REQUESTS
    assert!(is_retryable_status(500)); // INTERNAL_SERVER_ERROR
    assert!(is_retryable_status(502)); // BAD_GATEWAY
    assert!(is_retryable_status(503)); // SERVICE_UNAVAILABLE
    assert!(is_retryable_status(504)); // GATEWAY_TIMEOUT
    assert!(!is_retryable_status(400)); // BAD_REQUEST
    assert!(!is_retryable_status(401)); // UNAUTHORIZED
    assert!(!is_retryable_status(403)); // FORBIDDEN
    assert!(!is_retryable_status(404)); // NOT_FOUND
    assert!(!is_retryable_status(200)); // OK
}

#[test]
fn backoff_grows_exponentially_within_jitter_bounds() {
    let config = RetryConfig {
        max_attempts: 5,
        initial_backoff_ms: 1000,
        max_backoff_ms: 30000,
    };

    let d1 = backoff_duration(1, &config);
    assert!(d1.as_millis() >= 750 && d1.as_millis() <= 1250);

    let d2 = backoff_duration(2, &config);
    assert!(d2.as_millis() >= 1500 && d2.as_millis() <= 2500);

    let d3 = backoff_duration(3, &config);
    assert!(d3.as_millis() >= 3000 && d3.as_millis() <= 5000);

    let d4 = backoff_duration(4, &config);
    assert!(d4.as_millis() >= 6000 && d4.as_millis() <= 10000);
}

#[test]
fn backoff_respects_max_cap() {
    let config = RetryConfig {
        max_attempts: 5,
        initial_backoff_ms: 1000,
        max_backoff_ms: 5000,
    };

    let d = backoff_duration(10, &config);
    assert!(d.as_millis() >= 3750 && d.as_millis() <= 6250);
}

#[test]
fn parse_retry_after_seconds() {
    assert_eq!(parse_retry_after_secs(Some("42")), Some(42));
}

#[test]
fn parse_retry_after_missing_header() {
    assert_eq!(parse_retry_after_secs(None), None);
}

#[test]
fn parse_retry_after_non_integer() {
    assert_eq!(parse_retry_after_secs(Some("abc")), None);
}

#[test]
fn daemon_config_deserializes_max_turns() {
    let raw = "max_turns = 42\n";
    let config: DaemonConfig = toml::from_str(raw).unwrap();
    assert_eq!(config.max_turns, Some(42));
}

#[test]
fn daemon_config_deserializes_context() {
    let raw = r#"
[context]
context_file_names = ["AGENTS.md"]
context_file_max_bytes = 16384
disable_claude_code_prompt = true
"#;
    let config: DaemonConfig = toml::from_str(raw).unwrap();
    assert_eq!(config.context.context_file_names, vec!["AGENTS.md"]);
    assert_eq!(config.context.context_file_max_bytes, 16384);
    assert!(config.context.disable_claude_code_prompt);
}

#[test]
fn daemon_config_ignores_unknown_fields() {
    let raw = r#"
max_turns = 10
base_url = "https://example.com"
streaming = false
"#;
    let config: DaemonConfig = toml::from_str(raw).unwrap();
    assert_eq!(config.max_turns, Some(10));
}

#[test]
fn daemon_config_defaults_when_empty() {
    let config: DaemonConfig = toml::from_str("").unwrap();
    assert_eq!(config.max_turns, None);
}

#[test]
fn daemon_config_errors_on_invalid_toml() {
    let result: Result<DaemonConfig, _> = toml::from_str("[[[");
    assert!(result.is_err());
}

// ── Responses API SSE stream event parsing tests ────────────────────

#[test]
fn parse_responses_stream_event_text_delta() {
    let event =
        parse_responses_stream_event(r#"{"type":"response.output_text.delta","delta":"Hello"}"#)
            .expect("parse")
            .expect("event");
    match event {
        ResponsesStreamEvent::TextDelta(text) => assert_eq!(text, "Hello"),
        _ => panic!("expected TextDelta"),
    }
}

#[test]
fn parse_responses_stream_event_text_done() {
    let event = parse_responses_stream_event(r#"{"type":"response.output_text.done"}"#)
        .expect("parse")
        .expect("event");
    match event {
        ResponsesStreamEvent::TextDone => {}
        _ => panic!("expected TextDone"),
    }
}

#[test]
fn parse_responses_stream_event_function_call_args() {
    // Delta — use r##"..."## so the \" inside doesn't collide with the delimiter.
    let event = parse_responses_stream_event(
        r##"{"type":"response.function_call_arguments.delta","call_id":"call_1","delta":"{\"city\":"}"##,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::FunctionCallArgumentsDelta { call_id, delta } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(delta, r#"{"city":"#);
        }
        _ => panic!("expected FunctionCallArgumentsDelta"),
    }

    // Done
    let event = parse_responses_stream_event(
        r#"{"type":"response.function_call_arguments.done","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"London\"}"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::FunctionCallArgumentsDone {
            call_id,
            name,
            arguments,
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(name, "get_weather");
            assert_eq!(arguments, r#"{"city":"London"}"#);
        }
        _ => panic!("expected FunctionCallArgumentsDone"),
    }
}

#[test]
fn parse_responses_stream_event_completed_with_usage() {
    let event = parse_responses_stream_event(
        r#"{"type":"response.completed","usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ResponseCompleted { usage, .. } => {
            let u = usage.expect("usage should be present");
            assert_eq!(u.prompt_tokens, 10);
            assert_eq!(u.completion_tokens, 5);
            assert_eq!(u.total_tokens, 15);
        }
        _ => panic!("expected ResponseCompleted"),
    }
}

#[test]
fn parse_responses_stream_event_unknown_type_returns_none() {
    let result =
        parse_responses_stream_event(r#"{"type":"response.unknown_event","data":"ignored"}"#)
            .expect("parse");
    assert!(result.is_none());
}

// ── Programmatic tool calling SSE event tests ──────────────────────

#[test]
fn parse_responses_stream_event_program_code_delta() {
    let event = parse_responses_stream_event(
        r#"{"type":"response.program.code.delta","delta":"console.log"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ProgramCodeDelta(delta) => {
            assert_eq!(delta, "console.log");
        }
        _ => panic!("expected ProgramCodeDelta"),
    }
}

#[test]
fn parse_responses_stream_event_program_code_delta_missing_delta() {
    // When delta is missing, the parser returns None for this event type.
    let result =
        parse_responses_stream_event(r#"{"type":"response.program.code.delta"}"#).expect("parse");
    assert!(result.is_none(), "expected None when delta is missing");
}

#[test]
fn parse_responses_stream_event_program_code_done() {
    let event = parse_responses_stream_event(
        r#"{"type":"response.program.code.done","call_id":"prog_1","fingerprint":"fp_abc"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ProgramCodeDone {
            call_id,
            fingerprint,
        } => {
            assert_eq!(call_id, "prog_1");
            assert_eq!(fingerprint.as_deref(), Some("fp_abc"));
        }
        _ => panic!("expected ProgramCodeDone"),
    }
}

#[test]
fn parse_responses_stream_event_program_code_done_no_fingerprint() {
    let event =
        parse_responses_stream_event(r#"{"type":"response.program.code.done","call_id":"prog_1"}"#)
            .expect("parse")
            .expect("event");
    match event {
        ResponsesStreamEvent::ProgramCodeDone {
            call_id,
            fingerprint,
        } => {
            assert_eq!(call_id, "prog_1");
            assert!(fingerprint.is_none());
        }
        _ => panic!("expected ProgramCodeDone"),
    }
}

#[test]
fn parse_responses_stream_event_program_code_done_missing_call_id() {
    let result = parse_responses_stream_event(r#"{"type":"response.program.code.done"}"#);
    assert!(result.is_err(), "missing call_id should be an error");
}

#[test]
fn parse_responses_stream_event_program_output_done() {
    let event = parse_responses_stream_event(
        r#"{"type":"response.program_output.done","call_id":"prog_1","result":"{\"status\":\"ok\"}","status":"completed"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ProgramOutputDone {
            call_id,
            result,
            status,
        } => {
            assert_eq!(call_id, "prog_1");
            assert_eq!(result, r#"{"status":"ok"}"#);
            assert_eq!(status, "completed");
        }
        _ => panic!("expected ProgramOutputDone"),
    }
}

#[test]
fn parse_responses_stream_event_program_output_done_defaults() {
    let event = parse_responses_stream_event(
        r#"{"type":"response.program_output.done","call_id":"prog_1"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ProgramOutputDone {
            call_id,
            result,
            status,
        } => {
            assert_eq!(call_id, "prog_1");
            assert_eq!(result, "", "result defaults to empty string");
            assert_eq!(status, "", "status defaults to empty string");
        }
        _ => panic!("expected ProgramOutputDone"),
    }
}

#[test]
fn parse_responses_stream_event_program_output_done_missing_call_id() {
    let result = parse_responses_stream_event(r#"{"type":"response.program_output.done"}"#);
    assert!(result.is_err(), "missing call_id should be an error");
}

// -- sleep_or_cancel tests -------------------------------------------

#[test]
fn sleep_or_cancel_signal_returns_cancelled() {
    let (tx, rx) = mpsc::channel::<()>();
    tx.send(()).unwrap();
    let result = crate::retry::sleep_or_cancel(Duration::from_secs(10), Some(&rx));
    assert!(result.is_err());
}

#[test]
fn sleep_or_cancel_disconnected_returns_ok() {
    let (tx, rx) = mpsc::channel::<()>();
    drop(tx);
    let result = crate::retry::sleep_or_cancel(Duration::from_millis(1), Some(&rx));
    assert!(result.is_ok());
}
