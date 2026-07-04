use super::*;

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
fn chat_completions_stream_delta_keeps_reasoning_separate() {
    let payload: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{"choices":[{"delta":{"content":"answer","reasoning_text":"think"}}]}"#,
    )
    .expect("parse");

    let delta = payload
        .choices
        .into_iter()
        .next()
        .expect("choice")
        .delta
        .expect("delta");
    assert_eq!(delta.content.as_deref(), Some("answer"));
    assert_eq!(delta.reasoning_text.as_deref(), Some("think"));
}

#[test]
fn retryable_statuses() {
    use reqwest::StatusCode;
    assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
    assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
    assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
    assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
    assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
    assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
    assert!(!is_retryable_status(StatusCode::FORBIDDEN));
    assert!(!is_retryable_status(StatusCode::NOT_FOUND));
    assert!(!is_retryable_status(StatusCode::OK));
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
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "42".parse().unwrap());
    assert_eq!(parse_retry_after_secs(&headers), Some(42));
}

#[test]
fn parse_retry_after_missing_header() {
    let headers = reqwest::header::HeaderMap::new();
    assert_eq!(parse_retry_after_secs(&headers), None);
}

#[test]
fn parse_retry_after_non_integer() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "abc".parse().unwrap());
    assert_eq!(parse_retry_after_secs(&headers), None);
}
