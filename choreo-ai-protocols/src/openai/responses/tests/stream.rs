//! The streaming tool-call accumulator: the per-`call_id` argument merge and the
//! call-count bound the SSE loop relies on.

// The suite repeats its parent's imports one level deeper: this module lives
// inside `tests`, so the adapter is two `super`s up.
use super::super::*;
use crate::shared::MAX_TOOL_CALLS;

#[test]
fn acc_call_new_sets_index() {
    let call = AccCall::new(5);
    assert_eq!(call.index, 5);
    assert!(call.name.is_none());
    assert_eq!(call.arguments, "");
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
