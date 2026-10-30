//! The SSE subjects: the format-agnostic frame reader and `build_sse_event`, plus
//! the Responses API stream-event parser and the wire shapes it reads.
//!
//! These cases reach the adapter's items straight from the `openai` parent — the
//! reader and the event vocabulary are re-exported there, which is the same path
//! production code takes.

use super::super::*;

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
                matches!(&call, CallRef::Item(id) if id == "fc_1"),
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
        ResponsesStreamEvent::FunctionCallArgumentsDone {
            call,
            arguments,
            name,
        } => {
            assert!(
                matches!(&call, CallRef::Item(id) if id == "fc_1"),
                "{call:?}"
            );
            assert_eq!(arguments, r#"{"city":"London"}"#);
            assert!(
                name.is_none(),
                "the documented payload carries no name: {name:?}"
            );
        }
        _ => panic!("expected FunctionCallArgumentsDone"),
    }
}

#[test]
fn parse_responses_stream_event_reads_a_name_off_the_arguments_event() {
    // The name belongs to the call's output item, but a lenient gateway that
    // relays no items may put it on the arguments event — so it is read as a
    // fallback rather than ignored. An empty string is not a name.
    let event = parse_responses_stream_event(
        r#"{"type":"response.function_call_arguments.done","call_id":"call_1","name":"get_weather","arguments":"{}"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::FunctionCallArgumentsDone { name, .. } => {
            assert_eq!(name.as_deref(), Some("get_weather"));
        }
        other => panic!("expected FunctionCallArgumentsDone, got {other:?}"),
    }

    let event = parse_responses_stream_event(
        r#"{"type":"response.function_call_arguments.done","call_id":"call_1","name":"","arguments":"{}"}"#,
    )
    .expect("parse")
    .expect("event");
    match event {
        ResponsesStreamEvent::FunctionCallArgumentsDone { name, .. } => {
            assert!(name.is_none(), "an empty name is no name: {name:?}");
        }
        other => panic!("expected FunctionCallArgumentsDone, got {other:?}"),
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
                matches!(&call, CallRef::Call(id) if id == "call_1"),
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
