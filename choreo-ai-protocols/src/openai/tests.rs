use super::*;
use std::time::Duration;

#[test]
fn chat_message_with_images_serializes_content_as_parts_array() {
    // A user message carrying an image serializes `content` as an array of
    // `text` + `image_url` parts (the OpenAI chat vision shape), with the
    // data URL nested under `image_url.url` and `detail` fixed to "auto".
    let msg = ChatRequestMessage {
        role: "user",
        content: Some("what is this?".to_string()),
        images: vec![ChatImagePart {
            data: b"\x89PNG-fake".to_vec(),
            mime_type: "image/png".to_string(),
        }],
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: None,
    };
    let value =
        serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    let content = value["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is this?");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/png;base64,iVBORy1mYWtl"
    );
    assert_eq!(content[1]["image_url"]["detail"], "auto");
    // The images field itself must never appear on the wire as its own field.
    assert!(value.get("images").is_none());
}

#[test]
fn chat_message_without_images_serializes_content_as_plain_string() {
    // A text-only message serializes to the plain role/content shape
    // (content is a plain string; no images field).
    let msg = ChatRequestMessage::simple("user", "hi".to_string());
    let value =
        serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert_eq!(value["content"], "hi");
    assert!(value.get("images").is_none());
}

#[test]
fn build_sse_event_joins_multiple_data_lines() {
    let mut lines = vec![
        "event: message".to_string(),
        "data: hello".to_string(),
        "data: world".to_string(),
    ];
    let event = build_sse_event(&mut lines).expect("event");
    assert_eq!(event, "hello\nworld");
    assert_eq!(lines, [] as [String; 0]);
}

#[test]
fn build_sse_event_returns_done_marker() {
    let mut lines = vec!["data: [DONE]".to_string()];
    let event = build_sse_event(&mut lines).expect("event");
    assert_eq!(event, "[DONE]");
    assert_eq!(lines, [] as [String; 0]);
}

#[test]
fn sse_reader_yields_successive_events() {
    // The per-event byte budget is reset between events, so a long stream of
    // ordinary events is unaffected by it.
    let mut reader =
        SseReader::from_reader(std::io::Cursor::new(b"data: one\n\ndata: two\n\n".to_vec()));
    assert_eq!(reader.next_event().unwrap().as_deref(), Some("one"));
    assert_eq!(reader.next_event().unwrap().as_deref(), Some("two"));
    assert_eq!(reader.next_event().unwrap(), None);
}

#[test]
fn sse_reader_rejects_an_unterminated_line_past_the_ceiling() {
    // A peer that never emits a newline must not be able to grow the pending
    // buffer without bound: the read fails once the ceiling is crossed.
    struct EndlessLine;
    impl std::io::Read for EndlessLine {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf.fill(b'x');
            Ok(buf.len())
        }
    }
    let mut reader = SseReader::from_reader(EndlessLine);
    let err = reader
        .next_event()
        .expect_err("an endless line must fail the read");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn sse_reader_rejects_a_single_line_past_the_ceiling() {
    // Same ceiling one level up: a line that is itself too long to be a real
    // provider event.
    let mut payload = vec![b'd'; 9 * 1024 * 1024];
    payload.push(b'\n');
    let mut reader = SseReader::from_reader(std::io::Cursor::new(payload));
    let err = reader
        .next_event()
        .expect_err("an oversized line must fail the read");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
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
    // The documented argument events name only the output item they belong to:
    // neither the function's name nor its `call_id` is part of the payload.
    let event = parse_responses_stream_event(
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"sequence_number":7,"delta":"{\"city\":"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::FunctionCallArgumentsDelta { call, delta } => {
            assert!(
                matches!(&call, super::CallRef::Item(id) if id == "fc_1"),
                "{call:?}"
            );
            assert_eq!(delta, r#"{"city":"#);
        }
        _ => panic!("expected FunctionCallArgumentsDelta"),
    }

    // Done
    let event = parse_responses_stream_event(
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":1,"sequence_number":9,"arguments":"{\"city\":\"London\"}"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::FunctionCallArgumentsDone { call, arguments } => {
            assert!(
                matches!(&call, super::CallRef::Item(id) if id == "fc_1"),
                "{call:?}"
            );
            assert_eq!(arguments, r#"{"city":"London"}"#);
        }
        _ => panic!("expected FunctionCallArgumentsDone"),
    }
}

#[test]
fn parse_responses_stream_event_reads_a_function_call_item() {
    // The item is the only event that names the function, so the parser lifts
    // the call's identity off it rather than leaving the call unnameable.
    for event_type in ["response.output_item.added", "response.output_item.done"] {
        let event = parse_responses_stream_event(&format!(
            r#"{{"type":"{event_type}","output_index":1,"item":{{"id":"fc_1","type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{{\"city\":\"London\"}}","status":"completed"}}}}"#,
        ))
        .expect("parse")
        .expect("event");
        match event {
            ResponsesStreamEvent::FunctionCallItem {
                item_id,
                call_id,
                name,
                arguments,
                caller,
            } => {
                assert_eq!(item_id.as_deref(), Some("fc_1"));
                assert_eq!(call_id.as_deref(), Some("call_1"));
                assert_eq!(name.as_deref(), Some("get_weather"));
                assert_eq!(arguments, r#"{"city":"London"}"#);
                assert!(caller.is_none());
            }
            _ => panic!("expected FunctionCallItem"),
        }
    }
}

#[test]
fn parse_responses_stream_event_captures_a_function_call_caller() {
    // The `caller` the API puts on a `function_call` output item is what the
    // replay copies back onto the call and its output, so the parser lifts it
    // off the item rather than leaving it in the opaque raw JSON.
    for event_type in ["response.output_item.added", "response.output_item.done"] {
        let event = parse_responses_stream_event(&format!(
            r#"{{"type":"{event_type}","item":{{"id":"fc_1","type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{{}}","caller":{{"type":"program","caller_id":"prog_123"}}}}}}"#,
        ))
        .expect("parse")
        .expect("event");
        match event {
            ResponsesStreamEvent::FunctionCallItem { caller, .. } => {
                let caller = caller.expect("a program caller is captured");
                assert_eq!(caller.kind, "program");
                assert_eq!(caller.caller_id, "prog_123");
            }
            _ => panic!("expected FunctionCallItem"),
        }
    }
}

#[test]
fn parse_responses_stream_event_tolerates_a_missing_call_caller() {
    // A `direct` caller names no program, and an un-attributed function call
    // carries no `caller` at all. Both are legal, so the attribution rides the
    // event as `None` rather than failing the parse — a call is still a call.
    let direct = parse_responses_stream_event(
        r#"{"type":"response.output_item.done","item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{}","caller":{"type":"direct"}}}"#,
    )
    .expect("parse")
    .expect("event");
    match direct {
        ResponsesStreamEvent::FunctionCallItem { caller, .. } => {
            let caller = caller.expect("a direct caller is still an attribution");
            assert_eq!(caller.kind, "direct");
            assert_eq!(caller.caller_id, "");
        }
        _ => panic!("expected FunctionCallItem"),
    }

    let absent = parse_responses_stream_event(
        r#"{"type":"response.output_item.done","item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{}"}}"#,
    )
    .expect("parse")
    .expect("event");
    match absent {
        ResponsesStreamEvent::FunctionCallItem {
            call_id, caller, ..
        } => {
            assert_eq!(call_id.as_deref(), Some("call_1"));
            assert!(caller.is_none());
        }
        _ => panic!("expected FunctionCallItem"),
    }
}

#[test]
fn a_function_call_with_a_nonconforming_item_is_still_read() {
    // A provider that leaves the item's identity or name out must not fail the
    // parse of a stream it is already billing for; the accumulator decides what
    // an unidentifiable item means.
    let event = parse_responses_stream_event(
        r#"{"type":"response.output_item.added","item":{"type":"function_call"}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::FunctionCallItem {
            item_id,
            call_id,
            name,
            arguments,
            ..
        } => {
            assert!(item_id.is_none());
            assert!(call_id.is_none());
            assert!(name.is_none());
            assert_eq!(arguments, "");
        }
        _ => panic!("expected FunctionCallItem"),
    }
}

#[test]
fn an_argument_event_that_names_no_call_is_not_an_event() {
    // Neither id present: the chunk cannot be attributed to any call, so it is
    // ignored the way an unknown event type is rather than keying a call that
    // nothing can ever name.
    for payload in [
        r#"{"type":"response.function_call_arguments.delta","delta":"{}"}"#,
        r#"{"type":"response.function_call_arguments.done","arguments":"{}"}"#,
    ] {
        assert!(
            parse_responses_stream_event(payload)
                .expect("parse")
                .is_none()
        );
    }
}

#[test]
fn an_argument_event_naming_only_a_call_id_still_resolves() {
    // A gateway that relays no output items has only the call id to name a call
    // by, and that has to keep identifying the call it belongs to.
    let event = parse_responses_stream_event(
        r#"{"type":"response.function_call_arguments.done","call_id":"call_1","arguments":"{}"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::FunctionCallArgumentsDone { call, .. } => {
            assert!(
                matches!(&call, super::CallRef::Call(id) if id == "call_1"),
                "{call:?}"
            );
        }
        _ => panic!("expected FunctionCallArgumentsDone"),
    }
}

#[test]
fn parse_responses_stream_event_completed_with_usage() {
    let event = parse_responses_stream_event(
        r#"{"type":"response.completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}"#,
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

#[test]
fn parse_responses_stream_event_completed_reads_the_wrapped_response_object() {
    // The documented terminal event wraps the whole `Response` object under
    // `response` — id and usage live there, not on the event root.
    let event = parse_responses_stream_event(
        r#"{"type":"response.completed","sequence_number":42,"response":{"id":"resp_9","status":"completed","usage":{"input_tokens":3,"output_tokens":4,"total_tokens":7}}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ResponseCompleted { id, usage } => {
            assert_eq!(id.as_deref(), Some("resp_9"));
            let usage = usage.expect("usage");
            assert_eq!(usage.total_tokens, 7);
        }
        other => panic!("expected ResponseCompleted, got {other:?}"),
    }
}

#[test]
fn parse_responses_stream_event_incomplete_carries_the_provider_reason() {
    // `incomplete` is a terminal success whose output may be partial; the
    // provider states why under `incomplete_details.reason`.
    let event = parse_responses_stream_event(
        r#"{"type":"response.incomplete","response":{"id":"resp_5","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":9,"output_tokens":2,"total_tokens":11}}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ResponseIncomplete { id, usage, reason } => {
            assert_eq!(id.as_deref(), Some("resp_5"));
            assert_eq!(reason.as_deref(), Some("max_output_tokens"));
            assert_eq!(usage.expect("usage").total_tokens, 11);
        }
        other => panic!("expected ResponseIncomplete, got {other:?}"),
    }
}

#[test]
fn parse_responses_stream_event_incomplete_without_a_reason() {
    let event = parse_responses_stream_event(
        r#"{"type":"response.incomplete","response":{"id":"resp_6","status":"incomplete"}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ResponseIncomplete { reason, .. } => assert!(reason.is_none()),
        other => panic!("expected ResponseIncomplete, got {other:?}"),
    }
}

#[test]
fn parse_responses_stream_event_failed_reads_the_wrapped_error() {
    // `response.failed` carries the same wrapping: the error object sits on
    // `response.error`, not on the event root.
    let event = parse_responses_stream_event(
        r#"{"type":"response.failed","response":{"id":"resp_7","status":"failed","error":{"code":"server_error","message":"upstream exploded"}}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ResponseFailed(error) => assert_eq!(error, "upstream exploded"),
        other => panic!("expected ResponseFailed, got {other:?}"),
    }
}

#[test]
fn parse_responses_stream_event_reasoning_summary_text_delta() {
    // The summary is the only reasoning text a Responses model streams; the
    // event carries one text fragment per chunk.
    let event = parse_responses_stream_event(
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"step one"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ReasoningSummaryText(text) => assert_eq!(text, "step one"),
        other => panic!("expected ReasoningSummaryText, got {other:?}"),
    }
}

#[test]
fn parse_responses_stream_event_reasoning_summary_part_boundaries_are_ignored() {
    // Part boundaries carry no text of their own (the parts concatenate in
    // arrival order), so they are not modelled as events.
    for event_type in [
        "response.reasoning_summary_part.added",
        "response.reasoning_summary_part.done",
    ] {
        let data = format!(r#"{{"type":"{event_type}","item_id":"rs_1"}}"#);
        assert!(
            parse_responses_stream_event(&data)
                .expect("parse")
                .is_none(),
            "{event_type} must not produce an event",
        );
    }
}

#[test]
fn parse_responses_stream_event_response_failed_decodes_error_object() {
    // `response.failed.error` is the OpenAI error object `{code, message,
    // param, type}`; the surfaced text must be `message`, not a blank string
    // (`as_str()` on the object yields nothing).
    let event = parse_responses_stream_event(
        r#"{"type":"response.failed","error":{"code":"server_error","message":"Request failed due to internal server error.","param":null,"type":"server_error"}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ResponseFailed(error) => {
            assert_eq!(error, "Request failed due to internal server error.");
        }
        _ => panic!("expected ResponseFailed"),
    }
}

#[test]
fn parse_responses_stream_event_response_failed_accepts_plain_string() {
    // Some providers/emulators put a plain string in `error` — that shape
    // must keep working.
    let event = parse_responses_stream_event(r#"{"type":"response.failed","error":"overloaded"}"#)
        .expect("parse")
        .expect("event");
    match event {
        ResponsesStreamEvent::ResponseFailed(error) => assert_eq!(error, "overloaded"),
        _ => panic!("expected ResponseFailed"),
    }
}

#[test]
fn parse_responses_stream_event_response_failed_object_without_message_keeps_serialized() {
    // An error object with no `message` field still surfaces something — the
    // serialized object — rather than an empty string.
    let event = parse_responses_stream_event(
        r#"{"type":"response.failed","error":{"code":"internal_error"}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ResponseFailed(error) => {
            assert!(error.contains("internal_error"), "got: {error}");
        }
        _ => panic!("expected ResponseFailed"),
    }
}

// ── Reasoning item SSE event tests (phase 2c) ───────────────────────

#[test]
fn parse_responses_stream_event_reasoning_item_done() {
    // `response.output_item.done` carrying a `type: "reasoning"` item is
    // captured as an opaque ReasoningItem, verbatim (including
    // encrypted_content in stateless mode).
    let event = parse_responses_stream_event(
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"re_1","type":"reasoning","summary":[{"text":"thinking"}],"encrypted_content":"eJxT"}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ReasoningItem(item) => {
            assert_eq!(item["id"], "re_1");
            assert_eq!(item["type"], "reasoning");
            assert_eq!(item["encrypted_content"], "eJxT");
        }
        other => panic!("expected ReasoningItem, got {other:?}"),
    }
}

#[test]
fn parse_responses_stream_event_reasoning_item_added() {
    // The item-open event also maps to ReasoningItem (item summary may
    // still be empty; merge_reasoning_item later dedupes by id).
    let event = parse_responses_stream_event(
        r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"re_2","type":"reasoning","summary":[]}}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::ReasoningItem(item) => {
            assert_eq!(item["id"], "re_2");
        }
        other => panic!("expected ReasoningItem, got {other:?}"),
    }
}

#[test]
fn parse_responses_stream_event_non_reasoning_item_is_ignored() {
    // Message / function_call items flow through dedicated events — only
    // reasoning items are captured here.
    let result = parse_responses_stream_event(
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}"#,
    )
    .expect("parse");
    assert!(result.is_none());
}

#[test]
fn parse_responses_stream_event_output_item_without_item_is_ignored() {
    // Malformed event (no `item` field) is silently ignored, not an error.
    let result =
        parse_responses_stream_event(r#"{"type":"response.output_item.done"}"#).expect("parse");
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
    let (tx, rx) = crossbeam_channel::unbounded::<()>();
    tx.send(()).unwrap();
    let result = crate::retry::sleep_or_cancel(Duration::from_secs(10), Some(&rx));
    assert!(result.is_err());
}

#[test]
fn sleep_or_cancel_disconnected_returns_ok() {
    let (tx, rx) = crossbeam_channel::unbounded::<()>();
    drop(tx);
    let result = crate::retry::sleep_or_cancel(Duration::from_millis(1), Some(&rx));
    assert!(result.is_ok());
}

// -- model-usage error annotation -------------------------------------

#[test]
fn annotates_not_a_chat_model_rejection_with_the_model() {
    // The exact OpenAI body the live model list can produce when the user
    // selects a legacy completions model. The user-facing message must be
    // short and plain, and must NOT leak the provider's technical wording.
    let err = annotate_model_usage_error(
        "gpt-3.5-turbo-instruct",
        OpenAiError::ClientError {
            status: 404,
            detail: "This is not a chat model and thus not supported in the \
                     v1/chat/completions endpoint. Did you mean to use v1/completions?"
                .to_string(),
        },
    );
    let OpenAiError::ClientError { status, detail } = err else {
        panic!("expected ClientError, got {err:?}");
    };
    assert_eq!(status, 404);
    assert_eq!(
        detail,
        "'gpt-3.5-turbo-instruct' is not a chat model — please try a different one."
    );
    assert!(!detail.contains("v1/chat/completions"), "{detail}");
}

#[test]
fn annotates_model_not_found_with_a_plain_message() {
    // The generic "model doesn't exist / no access" body reads the same to a
    // user: the model can't be used here.
    let err = annotate_model_usage_error(
        "gpt-6-astra",
        OpenAiError::ClientError {
            status: 404,
            detail: "The model 'gpt-6-astra' does not exist or you do not have access to it."
                .to_string(),
        },
    );
    let OpenAiError::ClientError { detail, .. } = err else {
        panic!("expected ClientError, got {err:?}");
    };
    assert_eq!(
        detail,
        "'gpt-6-astra' can't be used here — please try a different one."
    );
}

#[test]
fn leaves_unrelated_client_errors_untouched() {
    // A 400 that has nothing to do with model usability must pass through
    // verbatim so the real cause is not masked.
    let err = annotate_model_usage_error(
        "gpt-4o",
        OpenAiError::ClientError {
            status: 400,
            detail: "Invalid value: 'reasoning.summary'.".to_string(),
        },
    );
    let OpenAiError::ClientError { detail, .. } = err else {
        panic!("expected ClientError, got {err:?}");
    };
    assert_eq!(detail, "Invalid value: 'reasoning.summary'.");
}

#[test]
fn leaves_non_client_errors_untouched() {
    let err = annotate_model_usage_error("gpt-4o", OpenAiError::Cancelled);
    assert!(matches!(err, OpenAiError::Cancelled));
}

#[test]
fn leaves_unsupported_parameter_errors_untouched() {
    // Change #2: OpenAI reuses the SAME "is not supported in the v1/responses"
    // wording for genuine parameter errors. Matching it would rewrite a real
    // parameter bug as a model-usability rejection and hide the cause, so the
    // body must pass through verbatim (character-for-character).
    let body =
        "Unsupported parameter: 'reasoning_effort' is not supported in the v1/responses API.";
    let err = annotate_model_usage_error(
        "gpt-4o",
        OpenAiError::ClientError {
            status: 400,
            detail: body.to_string(),
        },
    );
    let OpenAiError::ClientError { status, detail } = err else {
        panic!("expected ClientError, got {err:?}");
    };
    assert_eq!(status, 400);
    assert_eq!(detail, body);
}

#[test]
fn model_usage_rejection_reason_classifies_bodies() {
    // A realistic OpenAI "not a chat model" body — the exact wording the live
    // model list can produce for a legacy completions model.
    assert_eq!(
        model_usage_rejection_reason(
            "This is not a chat model and thus not supported in the \
             v1/chat/completions endpoint. Did you mean to use v1/completions?"
        ),
        Some("is not a chat model")
    );
    // The parsed-envelope model-not-found signal: `extract_error_message`
    // keeps only `error.message`, so this human prose is what reaches us.
    assert_eq!(
        model_usage_rejection_reason(
            "The model 'x' does not exist or you do not have access to it."
        ),
        Some("can't be used here")
    );
    // The literal code only survives when the body was NOT a parseable
    // envelope (kept verbatim) — still recognized.
    assert_eq!(
        model_usage_rejection_reason("model_not_found"),
        Some("can't be used here")
    );
    // An unrelated body is not a model-usability rejection.
    assert_eq!(
        model_usage_rejection_reason("Invalid value: 'reasoning.summary'."),
        None
    );
}

#[test]
#[serial_test::serial(catalog)]
fn extended_cache_retention_matches_what_a_request_would_send() {
    // The warmer schedules its pings against the cache lifetime this answers, so
    // it must agree with the body builder's selector on every gate: the model's
    // dialect, the curated long tier, the API the body targets, and a field the
    // provider already rejected.
    use crate::{CacheRetentionDialect, PromptCacheTtl};
    let provider = "long-retention-extended-test";
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;

    // Pin one model on a tiny catalog. `openai_responses` decides which API the
    // body targets, which matters for the newer (Responses-only) spelling.
    let catalog_with = |dialect: CacheRetentionDialect, long_secs: Option<u32>, responses: bool| {
        let mut catalog = crate::catalog::test_util::tiny_catalog();
        catalog[0].slug = provider.to_string();
        let entry = &mut catalog[0].models[0];
        entry.model = "ret-model".to_string();
        entry.cache_retention = dialect;
        entry.openai_responses = responses;
        entry.prompt_cache = Some(PromptCacheTtl {
            short_secs: Some(300),
            long_secs,
        });
        crate::catalog::replace_catalog(catalog);
    };
    let client = || {
        let registry = crate::SocketRegistry::new();
        OpenAiClient::new(
            ServiceConfig {
                provider_slug: provider.to_string(),
                ..Default::default()
            },
            RequestAuth::api_key("test-key"),
            &registry,
        )
        .expect("client builds")
    };

    // Dialect + long tier, older spelling: emitted on both APIs.
    catalog_with(
        CacheRetentionDialect::PromptCacheRetention,
        Some(86_400),
        false,
    );
    let chat = client();
    assert!(chat.extended_cache_retention("ret-model"));
    // A model the catalog did not pin sends no selector.
    assert!(!chat.extended_cache_retention("unpinned-model"));
    catalog_with(
        CacheRetentionDialect::PromptCacheRetention,
        Some(86_400),
        true,
    );
    assert!(client().extended_cache_retention("ret-model"));

    // The same dialect with no curated long tier: nothing is emitted, so the
    // cache really does live the short tier.
    catalog_with(CacheRetentionDialect::PromptCacheRetention, None, false);
    assert!(!client().extended_cache_retention("ret-model"));

    // The newer spelling's extended `"12h"` value is Responses-only: Chat
    // Completions gets no selector, so it must not claim the long tier.
    catalog_with(
        CacheRetentionDialect::PromptCacheOptions,
        Some(43_200),
        false,
    );
    assert!(!client().extended_cache_retention("ret-model"));
    catalog_with(
        CacheRetentionDialect::PromptCacheOptions,
        Some(43_200),
        true,
    );
    assert!(client().extended_cache_retention("ret-model"));

    // And a field the provider already rejected for this pair is withheld, so
    // the request will not ask for the long tier.
    catalog_with(
        CacheRetentionDialect::PromptCacheRetention,
        Some(86_400),
        false,
    );
    crate::cache_field_rejection::note_cache_field_rejected(
        provider,
        "ret-model",
        crate::cache_field_rejection::PROMPT_CACHE_RETENTION_FIELD,
    );
    assert!(!client().extended_cache_retention("ret-model"));
}

#[test]
// `responses_stateless` reads the process-wide catalog, so serialize under the
// same key as the catalog-swap tests for libtest determinism.
#[serial_test::serial(catalog)]
fn client_cache_affinity_reports_what_the_request_sends() {
    // The daemon's cache-warming gate reads `ProviderClient::cache_affinity` and
    // documents it as the dialect the request bodies actually send, so the
    // stateless Responses backend must report `Off` even when the configured
    // field is set: its bodies withhold the hint outright.
    let client = |slug: &str| {
        let registry = crate::SocketRegistry::new();
        OpenAiClient::new(
            ServiceConfig {
                provider_slug: slug.to_string(),
                cache_affinity: crate::CacheAffinity::PromptCacheKey,
                ..Default::default()
            },
            RequestAuth::api_key("test-key"),
            &registry,
        )
        .expect("client builds")
    };
    assert_eq!(
        client("chatgpt").cache_affinity(),
        crate::CacheAffinity::Off
    );
    // The standard Responses provider keeps the dialect it was configured with.
    assert_eq!(
        client("openai").cache_affinity(),
        crate::CacheAffinity::PromptCacheKey
    );
}
