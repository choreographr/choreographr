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
