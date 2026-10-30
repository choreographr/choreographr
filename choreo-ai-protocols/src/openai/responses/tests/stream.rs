//! The streaming tool-call accumulator: the per-`call_id` argument merge, the
//! call-count bound the SSE loop relies on, and the attribution a
//! `function_call` output item carries.

// The suite repeats its parent's imports one level deeper: this module lives
// inside `tests`, so the adapter is two `super`s up.
use super::super::*;
use crate::CallerInfo;
use crate::openai::{ResponsesStreamEvent, parse_responses_stream_event};
use crate::shared::MAX_TOOL_CALLS;

/// Parse one SSE data payload into its typed event, as the stream loop does.
fn event(data: &str) -> ResponsesStreamEvent {
    parse_responses_stream_event(data)
        .expect("the payload parses")
        .expect("a typed event")
}

/// A `response.output_item.done` for a `function_call` item, attributed to a
/// model-generated program.
const ATTRIBUTED_ITEM: &str = r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"get_weather","arguments":"{}","caller":{"type":"program","caller_id":"prog_123"}}}"#;

/// The argument completion for the same call.
const ARGS_DONE: &str = r#"{"type":"response.function_call_arguments.done","call_id":"call_1","name":"get_weather","arguments":"{}"}"#;

#[test]
fn acc_call_new_sets_index() {
    let call = AccCall::new(5);
    assert_eq!(call.index, 5);
    assert!(call.name.is_none());
    assert!(call.caller.is_none());
    assert_eq!(call.arguments, "");
}

#[test]
fn a_function_call_item_attributes_the_finished_call() {
    // The attribution arrives on the call's own output item, separately from its
    // arguments, and the two events have to land on the same accumulator slot:
    // the finished call then carries the caller a rebuilt request copies back
    // onto the call and the output answering it.
    let mut calls = AccCalls::default();
    calls
        .absorb(&event(ARGS_DONE))
        .expect("arguments accumulate");
    calls
        .absorb(&event(ATTRIBUTED_ITEM))
        .expect("attribution attaches");

    let finished = calls.into_tool_calls();
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
fn an_attribution_event_leaves_the_accumulated_call_alone() {
    // The item event carries the whole call, but only the attribution is taken
    // from it: a name or an arguments string must never be overwritten by a late
    // item, because `function_call_arguments.done` is the authority on both.
    let mut calls = AccCalls::default();
    let partial =
        r#"{"type":"response.function_call_arguments.delta","call_id":"call_1","delta":"{}"}"#;
    let done = r#"{"type":"response.function_call_arguments.done","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"London\"}"}"#;
    calls
        .absorb(&event(partial))
        .expect("the delta accumulates");
    calls.absorb(&event(done)).expect("the done replaces it");
    calls
        .absorb(&event(ATTRIBUTED_ITEM))
        .expect("attribution attaches");

    let finished = calls.into_tool_calls();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].name, "get_weather");
    assert_eq!(finished[0].arguments_json, r#"{"city":"London"}"#);
    assert!(finished[0].caller.is_some());
}

#[test]
fn an_attribution_past_the_call_cap_is_dropped_without_failing() {
    // An attribution is metadata about a call that is already legal, so a refused
    // slot (a new id past the cap, which the accumulator drops anyway) must never
    // turn a finished turn into an error — and it must not squeeze an extra call
    // into a map that is already full.
    let mut calls = AccCalls::default();
    for i in 0..MAX_TOOL_CALLS {
        calls
            .entry(format!("call_{i}"))
            .expect("the first MAX_TOOL_CALLS ids fit")
            .name = Some("get_weather".to_string());
    }
    let over_cap = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_one_too_many","caller":{"type":"program","caller_id":"prog_123"}}}"#;
    calls
        .absorb(&event(over_cap))
        .expect("an attribution never fails the turn");
    assert_eq!(calls.into_tool_calls().len(), MAX_TOOL_CALLS);
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
