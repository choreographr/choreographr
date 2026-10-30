//! The streaming tool-call accumulator: how a call is assembled from its
//! `function_call` output item and the argument events that name it, the
//! call-count bound the SSE loop relies on, and the attribution a
//! `function_call` output item carries.

// Each subject module imports the items it names directly: it lives inside
// `tests`, so the adapter's own modules are two `super`s up.
use super::super::stream::{AccCall, AccCalls};
use crate::CallerInfo;
use crate::openai::{ResponsesStreamEvent, parse_responses_stream_event};
use crate::shared::MAX_TOOL_CALLS;

/// Parse one SSE data payload into its typed event, as the stream loop does.
fn event(data: &str) -> ResponsesStreamEvent {
    parse_responses_stream_event(data)
        .expect("the payload parses")
        .expect("a typed event")
}

/// The `response.output_item.added` a call arrives as: the item opens with its
/// identity, its name and no arguments yet.
const ITEM_ADDED: &str = r#"{"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"get_weather","arguments":""}}"#;

/// The `response.output_item.done` for the same call, attributed to a
/// model-generated program and carrying the item's complete arguments.
const ATTRIBUTED_ITEM_DONE: &str = r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"London\"}","status":"completed","caller":{"type":"program","caller_id":"prog_123"}}}"#;

/// A streamed chunk of the same call's arguments.
const ARGS_DELTA: &str = r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"sequence_number":8,"delta":"{\"city\":\"London\"}"}"#;

/// The argument completion for the same call.
const ARGS_DONE: &str = r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":1,"sequence_number":9,"arguments":"{\"city\":\"London\"}"}"#;

#[test]
fn acc_call_new_sets_index() {
    let call = AccCall::new(5);
    assert_eq!(call.index, 5);
    assert!(call.call_id.is_none());
    assert!(call.name.is_none());
    assert!(call.caller.is_none());
    assert_eq!(call.arguments, "");
}

#[test]
fn a_documented_stream_yields_one_named_call() {
    // The whole point of the accumulator: the argument events name only the
    // output item, so a call is runnable only because its item was read too.
    // The events arrive in the documented order — item opens, arguments stream,
    // arguments complete, item completes.
    let mut calls = AccCalls::default();
    for payload in [ITEM_ADDED, ARGS_DELTA, ARGS_DONE, ATTRIBUTED_ITEM_DONE] {
        calls.absorb(&event(payload)).expect("the call accumulates");
    }

    let finished = calls.finish().calls;
    assert_eq!(finished.len(), 1, "one item, one call: {finished:?}");
    assert_eq!(finished[0].id, "call_1");
    assert_eq!(finished[0].name, "get_weather");
    assert_eq!(finished[0].arguments_json, r#"{"city":"London"}"#);
    assert_eq!(
        finished[0].caller,
        Some(CallerInfo {
            kind: "program".to_string(),
            caller_id: "prog_123".to_string(),
        })
    );
}

#[test]
fn an_item_that_carries_no_arguments_leaves_accumulated_deltas_alone() {
    // `output_item.added` always carries an empty arguments string. It opens the
    // call; it does not reset arguments a previous delta already delivered.
    let mut calls = AccCalls::default();
    calls
        .absorb(&event(ARGS_DELTA))
        .expect("the delta accumulates");
    calls
        .absorb(&event(ITEM_ADDED))
        .expect("the item names the call");

    let finished = calls.finish().calls;
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].name, "get_weather");
    assert_eq!(finished[0].arguments_json, r#"{"city":"London"}"#);
}

#[test]
fn a_completed_item_replaces_the_arguments_with_its_own_copy() {
    // `output_item.done` carries the item's complete arguments, so it is as
    // authoritative as `arguments.done` — a stream that never sent the latter
    // still produces a runnable call.
    let mut calls = AccCalls::default();
    let partial = r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"city\":"}"#;
    calls
        .absorb(&event(partial))
        .expect("the delta accumulates");
    calls
        .absorb(&event(ATTRIBUTED_ITEM_DONE))
        .expect("the item completes the call");

    let finished = calls.finish().calls;
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].arguments_json, r#"{"city":"London"}"#);
}

#[test]
fn argument_events_naming_only_a_call_id_join_the_item_they_belong_to() {
    // A gateway that streams the call's item but names calls by id on its
    // argument events must not mint a second call: that would run the tool
    // twice, once with the item's name and once with the streamed arguments.
    let mut calls = AccCalls::default();
    calls
        .absorb(&event(ITEM_ADDED))
        .expect("the item names the call");
    calls
        .absorb(&event(
            r#"{"type":"response.function_call_arguments.done","call_id":"call_1","arguments":"{\"city\":\"London\"}"}"#,
        ))
        .expect("the call id resolves to the item's slot");

    let finished = calls.finish().calls;
    assert_eq!(finished.len(), 1, "one call, not two: {finished:?}");
    assert_eq!(finished[0].name, "get_weather");
    assert_eq!(finished[0].arguments_json, r#"{"city":"London"}"#);
}

#[test]
fn a_stream_without_output_items_leaves_the_call_unnameable() {
    // Nothing names the function, so the call cannot be run — and the count of
    // such calls is reported, because the turn's verdict depends on it: an
    // answer that silently omitted the call is exactly what must not happen.
    let mut calls = AccCalls::default();
    calls
        .absorb(&event(ARGS_DONE))
        .expect("arguments accumulate");

    let finished = calls.finish();
    assert_eq!(
        finished.calls.len(),
        0,
        "no call can be named: {finished:?}"
    );
    assert_eq!(finished.unnamed, 1);
}

#[test]
fn a_function_call_item_attributes_the_finished_call() {
    // The attribution arrives on the call's own output item and rides the
    // finished call, which is what the rebuilt request copies back onto both the
    // call and the output answering it.
    let mut calls = AccCalls::default();
    calls
        .absorb(&event(ARGS_DONE))
        .expect("arguments accumulate");
    calls
        .absorb(&event(ATTRIBUTED_ITEM_DONE))
        .expect("attribution attaches");

    let finished = calls.finish().calls;
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].caller,
        Some(CallerInfo {
            kind: "program".to_string(),
            caller_id: "prog_123".to_string(),
        })
    );
}

#[test]
fn an_item_with_no_identity_is_ignored() {
    // Nothing can answer a call that names neither an item nor a call id, so the
    // item is not accumulated — and, crucially, it does not fail a turn the
    // provider is already billing for.
    let mut calls = AccCalls::default();
    calls
        .absorb(&event(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","name":"get_weather","arguments":""}}"#,
        ))
        .expect("an unidentifiable item is ignored");

    assert_eq!(calls.finish().calls.len(), 0);
}

#[test]
fn an_item_past_the_call_cap_is_refused() {
    // The cap bounds the NUMBER of calls in one turn, whether the stream opens
    // them with an item or with an argument event.
    let mut calls = AccCalls::default();
    for i in 0..MAX_TOOL_CALLS {
        calls
            .entry(format!("call_{i}"))
            .expect("the first MAX_TOOL_CALLS ids fit")
            .name = Some("get_weather".to_string());
    }
    let over_cap = r#"{"type":"response.output_item.added","item":{"id":"fc_one_too_many","type":"function_call","call_id":"call_one_too_many","name":"get_weather","arguments":""}}"#;
    assert!(
        calls.absorb(&event(over_cap)).is_err(),
        "a new call past the cap is rejected",
    );
    assert_eq!(calls.finish().calls.len(), MAX_TOOL_CALLS);
}

#[test]
fn a_full_tool_call_map_still_accepts_a_known_call_id() {
    // The cap bounds the NUMBER of distinct calls; a later delta for a call the
    // stream already named is a legal continuation and must never be turned
    // into an error just because the map happens to be full.
    let mut calls = AccCalls::default();
    for i in 0..MAX_TOOL_CALLS {
        calls
            .entry(format!("call_{i}"))
            .expect("the first MAX_TOOL_CALLS ids fit");
    }
    calls
        .entry("call_0".to_string())
        .expect("a known id keeps accumulating")
        .arguments
        .push('{');
    assert!(
        calls.entry("call_one_too_many".to_string()).is_err(),
        "a new id past the cap is rejected"
    );
    let ordered = calls.into_ordered();
    assert_eq!(ordered.len(), MAX_TOOL_CALLS);
    assert_eq!(ordered[0].1.arguments, "{");
}
