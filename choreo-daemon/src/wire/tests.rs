//! Unit tests for the wire-repair module.

use super::*;
use choreo_ai_protocols::openai::{AssistantToolCall, AssistantToolFunction};

fn call(id: &str, name: &str, arguments: &str) -> AssistantToolCall {
    AssistantToolCall {
        id: id.to_string(),
        kind: "function".to_string(),
        function: AssistantToolFunction {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
        caller: None,
    }
}

fn assistant(calls: Vec<AssistantToolCall>) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "assistant",
        content: None,
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: Some(calls),
        reasoning_artifact: None,
    }
}

fn tool_result_with(id: &str, content: &str) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "tool",
        content: Some(content.to_string()),
        images: Vec::new(),
        tool_call_id: Some(id.to_string()),
        tool_calls: None,
        reasoning_artifact: None,
    }
}

fn tool_result(id: &str) -> ChatRequestMessage {
    tool_result_with(id, "ok")
}

fn ids_of(msg: &ChatRequestMessage) -> Vec<&str> {
    msg.tool_calls
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|c| c.id.as_str())
        .collect()
}

fn names_of(msg: &ChatRequestMessage) -> Vec<&str> {
    msg.tool_calls
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|c| c.function.name.as_str())
        .collect()
}

// ── name normalization ──

#[test]
fn valid_name_is_unchanged() {
    assert_eq!(normalize_name("sh"), "sh");
}

#[test]
fn empty_name_becomes_sentinel() {
    assert_eq!(normalize_name(""), INVALID_TOOL_CALL_NAME);
}

#[test]
fn whitespace_name_becomes_sentinel() {
    assert_eq!(normalize_name("  "), INVALID_TOOL_CALL_NAME);
}

#[test]
fn dotted_name_is_underscored() {
    assert_eq!(
        normalize_name("multi_tool_use.parallel"),
        "multi_tool_use_parallel"
    );
}

#[test]
fn max_length_name_is_unchanged() {
    let name = "x".repeat(MAX_FUNCTION_NAME_LEN);
    assert_eq!(normalize_name(&name), name);
}

#[test]
fn overlong_name_is_truncated() {
    let name = "x".repeat(MAX_FUNCTION_NAME_LEN + 1);
    let out = normalize_name(&name);
    assert_eq!(out.len(), MAX_FUNCTION_NAME_LEN);
    assert!(tool_name::is_provider_safe(&out));
}

#[test]
fn non_ascii_name_becomes_sentinel() {
    assert_eq!(normalize_name("日本語"), INVALID_TOOL_CALL_NAME);
}

#[test]
fn long_shell_command_name_is_sanitized() {
    let name = format!("rm -rf {} && echo done", "/tmp/some/path ".repeat(20));
    let out = normalize_name(&name);
    assert_ne!(out, "");
    assert!(out.len() <= MAX_FUNCTION_NAME_LEN);
    assert!(tool_name::is_provider_safe(&out));
}

// ── id normalization ──

#[test]
fn valid_id_is_identity() {
    let out = normalize_call("sh", "call_abc", "{}", 0, true);
    assert_eq!(out.id, "call_abc");
}

#[test]
fn blank_id_is_minted_and_stable() {
    let a = normalize_call("sh", "", r#"{"a":1}"#, 0, true);
    let b = normalize_call("sh", "", r#"{"a":1}"#, 0, true);
    assert!(a.id.starts_with("call_"));
    assert_eq!(a.id.len(), "call_".len() + 12);
    assert_eq!(a.id, b.id);
}

#[test]
fn two_blank_ids_in_one_turn_are_distinct() {
    let a = normalize_call("sh", "", "{}", 0, true);
    let b = normalize_call("sh", "", "{}", 1, true);
    assert_ne!(a.id, b.id);
}

#[test]
fn rewrite_ids_false_keeps_nonblank_id_byte_identical() {
    // A non-blank id with characters our rewrite would otherwise touch is left
    // exactly as the service minted it.
    let out = normalize_call("sh", "call/with:weird.id", "{}", 0, false);
    assert_eq!(out.id, "call/with:weird.id");
}

#[test]
fn rewrite_ids_false_still_mints_a_blank_id() {
    let out = normalize_call("sh", "", "{}", 0, false);
    assert!(out.id.starts_with("call_"));
}

#[test]
fn rewrite_ids_true_sanitizes_a_bad_id() {
    let out = normalize_call("sh", "call/with:weird.id", "{}", 0, true);
    assert_eq!(out.id, "call_with_weird_id");
}

#[test]
fn duplicate_ids_get_suffixes() {
    let mut messages = vec![assistant(vec![
        call("dup", "sh", "{}"),
        call("dup", "sh", "{}"),
        call("dup", "sh", "{}"),
    ])];
    repair_replayed_calls(&mut messages, true);
    assert_eq!(ids_of(&messages[0]), vec!["dup", "dup_d2", "dup_d3"]);
}

// ── argument normalization ──

#[test]
fn object_arguments_are_byte_identical() {
    let args = r#"{"a": 1,"b":[2, 3]}"#;
    let out = normalize_call("sh", "id", args, 0, true);
    assert_eq!(out.arguments_json, args);
}

#[test]
fn malformed_arguments_become_empty_object() {
    let out = normalize_call("sh", "id", "{", 0, true);
    assert_eq!(out.arguments_json, "{}");
}

#[test]
fn null_arguments_become_empty_object() {
    let out = normalize_call("sh", "id", "null", 0, true);
    assert_eq!(out.arguments_json, "{}");
}

#[test]
fn array_arguments_become_empty_object() {
    let out = normalize_call("sh", "id", "[1]", 0, true);
    assert_eq!(out.arguments_json, "{}");
}

#[test]
fn empty_arguments_become_empty_object() {
    let out = normalize_call("sh", "id", "", 0, true);
    assert_eq!(out.arguments_json, "{}");
}

#[test]
fn an_absent_argument_list_is_repaired_and_counted() {
    // The same rule the adapters apply at ingest, so the two layers agree: an
    // empty argument list is a legal no-argument call normalized to `{}` (never
    // discarded, never reported as a truncation), and the rewrite is counted
    // like any other repair so a replayed call is visible in the metric.
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "")]),
        tool_result("call_1"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.arguments, 1);
    assert_eq!(counts.names, 0);
    assert_eq!(counts.ids, 0);
    assert_eq!(
        messages[0]
            .tool_calls
            .as_ref()
            .map(|calls| calls[0].function.arguments.as_str()),
        Some("{}"),
    );
}

// ── pairing, stubbing, dropping ──

#[test]
fn unanswered_call_gets_a_stub() {
    let mut messages = vec![assistant(vec![call("call_1", "sh", "{}")])];
    repair_replayed_calls(&mut messages, true);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, "tool");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
    assert!(
        messages[1]
            .content
            .as_deref()
            .unwrap()
            .contains("Result unavailable")
    );
}

#[test]
fn orphan_result_is_dropped() {
    // An assistant message with no calls, then a stray tool message.
    let mut messages = vec![
        ChatRequestMessage::simple("assistant", "no calls here".to_string()),
        tool_result("call_ghost"),
    ];
    repair_replayed_calls(&mut messages, true);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "assistant");
}

#[test]
fn two_calls_one_result_gets_one_stub() {
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "{}"), call("call_2", "sh", "{}")]),
        tool_result("call_2"),
    ];
    repair_replayed_calls(&mut messages, true);
    // assistant + the real result (re-paired to call_2) + one stub for call_1.
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1].role, "tool");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_2"));
    assert_eq!(messages[2].role, "tool");
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn result_answering_nothing_in_its_turn_is_dropped() {
    // The first result matches call_1 by id; the second (same non-blank id, now
    // no longer matching an unanswered call) is a genuine orphan. A non-blank id
    // gets no positional fallback, so the extra result is dropped.
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "{}")]),
        tool_result("call_1"),
        tool_result("call_1"),
    ];
    repair_replayed_calls(&mut messages, true);
    assert_eq!(messages.len(), 2);
}

#[test]
fn a_nonblank_orphan_does_not_steal_a_legitimate_result() {
    // A result whose non-blank id matches no call must NOT be paired
    // positionally: doing so would let it take call_1's only answer, so the real
    // result for call_1 would be dropped as the orphan. The genuine orphan is
    // dropped and the real answer is kept and still pairs by its own id.
    let ghost = ChatRequestMessage {
        role: "tool",
        content: Some("ghost content".to_string()),
        images: Vec::new(),
        tool_call_id: Some("call_ghost".to_string()),
        tool_calls: None,
        reasoning_artifact: None,
    };
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "{}")]),
        ghost,
        tool_result("call_1"),
    ];
    repair_replayed_calls(&mut messages, true);
    assert_eq!(messages.len(), 2, "assistant + the real result only");
    assert_eq!(messages[0].role, "assistant");
    assert_eq!(messages[1].role, "tool");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(
        messages[1].content.as_deref(),
        Some("ok"),
        "the genuine answer must survive, not the orphan's content"
    );
}

#[test]
fn a_lone_unmatched_result_still_answers_the_only_call() {
    // The common single-call turn whose recorded id no longer matches — a
    // mangling proxy, or a transcript recorded before ids were minted. Exactly
    // one call is open, so the pairing is unambiguous: the tool's real output
    // must survive instead of being replaced by a stub.
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "{}")]),
        tool_result_with("call_rewritten", "real output"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.stubbed, 0, "the result answers the call, so no stub");
    assert_eq!(counts.dropped, 0, "the result is not an orphan");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, "tool");
    assert_eq!(
        messages[1].tool_call_id.as_deref(),
        Some("call_1"),
        "the result is relabelled with the call's final id"
    );
    assert_eq!(messages[1].content.as_deref(), Some("real output"));
}

#[test]
fn an_unmatched_result_answers_the_last_open_call() {
    // Two calls, the genuine answer for call_1, then a result whose id matches
    // nothing. By the time the fallback runs only call_2 is open, so the choice
    // is unambiguous and the second tool's output is preserved rather than
    // stubbed.
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "{}"), call("call_2", "sh", "{}")]),
        tool_result_with("call_1", "first output"),
        tool_result_with("call_mangled", "second output"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.stubbed, 0);
    assert_eq!(counts.dropped, 0);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[1].content.as_deref(), Some("first output"));
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_2"));
    assert_eq!(messages[2].content.as_deref(), Some("second output"));
}

#[test]
fn a_nonblank_orphan_with_two_calls_open_is_dropped() {
    // The anti-stealing rule as its own case: an orphan processed FIRST in a
    // two-call turn. Two candidates are open, so the fallback declines, and the
    // genuine answers still claim their calls by id.
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "{}"), call("call_2", "sh", "{}")]),
        tool_result_with("call_ghost", "ghost content"),
        tool_result_with("call_1", "first output"),
        tool_result_with("call_2", "second output"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.dropped, 1, "only the orphan is dropped");
    assert_eq!(counts.stubbed, 0);
    assert_eq!(messages.len(), 3, "assistant + the two genuine results");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[1].content.as_deref(), Some("first output"));
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_2"));
    assert_eq!(messages[2].content.as_deref(), Some("second output"));
}

#[test]
fn empty_assistant_message_is_dropped() {
    let mut messages = vec![ChatRequestMessage {
        role: "assistant",
        content: Some("   ".to_string()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: None,
    }];
    repair_replayed_calls(&mut messages, true);
    assert!(messages.is_empty());
}

#[test]
fn blank_named_call_is_repaired_and_keeps_its_result() {
    // The real incident: a blank name plus its `unknown tool: ` result.
    let mut messages = vec![assistant(vec![call("", "", "not json")]), tool_result("")];
    repair_replayed_calls(&mut messages, true);
    assert_eq!(names_of(&messages[0]), vec![INVALID_TOOL_CALL_NAME]);
    let id = ids_of(&messages[0])[0];
    assert!(!id.is_empty(), "a blank id is never replayed");
    assert_eq!(messages[1].tool_call_id.as_deref(), Some(id));
}

// ── cross-layer agreement ──

/// Every name shape the three rules meet in practice: the stream placeholders,
/// ordinary provider names, the exact byte cap and one past it, a dotted
/// multi-tool name, a whitespace-prefixed name, a non-ASCII name, and a long
/// shell command a weak model emitted as a "tool".
fn name_corpus() -> Vec<String> {
    vec![
        "sh".to_string(),
        String::new(),
        "  ".to_string(),
        "multi_tool_use.parallel".to_string(),
        " sh".to_string(),
        "x".repeat(MAX_FUNCTION_NAME_LEN),
        "x".repeat(MAX_FUNCTION_NAME_LEN + 1),
        "日本語".to_string(),
        format!("rm -rf {} && echo done", "/tmp/some/path ".repeat(20)),
    ]
}

/// The ingest gate, the provider-validity predicate, and the replay normalizer
/// must give the *same* verdict on every name — and the blank merge rule must
/// be strictly subsumed by them.
///
/// This is the invariant the shared `choreo_ai_protocols::tool_name` module
/// exists to hold: a name the ingest gate lets through but the normalizer
/// rewrites (or vice versa) is a call dispatched under one name and replayed
/// under another, which poisons the transcript and the prompt-cache prefix.
#[test]
fn intake_and_wire_agree_on_every_name() {
    for name in name_corpus() {
        // 1. The daemon's re-export and the crate-level rule are one rule.
        let safe = crate::tools::is_provider_safe_function_name(&name);
        assert_eq!(
            safe,
            choreo_ai_protocols::tool_name::is_provider_safe(&name),
            "daemon and crate disagree about {name:?}",
        );

        // 2. The ingest gate's runnability rule is its exact negation, so a
        //    call the loop prunes is precisely one this predicate rejects.
        assert_eq!(
            crate::tools::tool_call_is_unrunnable(&name),
            !safe,
            "ingest gate disagrees with the validity rule about {name:?}",
        );

        // 3. The replay normalizer passes a valid name through byte-identically
        //    (the cache prefix depends on it) and repairs anything else into a
        //    name a strict provider accepts.
        let normalized = normalize_call(&name, "call_1", "{}", 0, true);
        if safe {
            assert_eq!(normalized.name, name, "valid name was rewritten");
        } else {
            assert_ne!(normalized.name, name, "invalid name was left as-is");
            assert!(
                !normalized.name.is_empty() && normalized.name.len() <= MAX_FUNCTION_NAME_LEN,
                "repaired name {:?} violates the byte cap",
                normalized.name,
            );
            assert!(
                normalized
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "repaired name {:?} keeps an illegal byte",
                normalized.name,
            );
            assert!(
                choreo_ai_protocols::tool_name::is_provider_safe(&normalized.name),
                "repaired name {:?} is still not provider-safe",
                normalized.name,
            );
        }

        // 4. Blank is a strict subset of not-provider-safe: a caller that only
        //    rejects blank names accepts names a provider refuses.
        if choreo_ai_protocols::tool_name::is_blank(&name) {
            assert!(!safe, "blank name {name:?} must not be provider-safe");
            assert_ne!(normalized.name, name, "blank name was not rewritten");
        }
    }
}

// ── repair counts ──

#[test]
fn clean_messages_report_no_repairs() {
    let mut messages = vec![
        ChatRequestMessage::simple("user", "hi".to_string()),
        assistant(vec![call("call_1", "sh", r#"{"cmd":"ls"}"#)]),
        tool_result("call_1"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts, RepairCounts::default());
    assert!(counts.is_empty());
    // Nothing was rewritten, so nothing was removed or added either.
    assert_eq!(messages.len(), 3);
}

#[test]
fn a_coerced_name_is_counted() {
    let mut messages = vec![
        assistant(vec![call("call_1", "", "{}")]),
        tool_result("call_1"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.names, 1);
    assert_eq!(counts.ids, 0);
    assert_eq!(counts.arguments, 0);
    assert_eq!(counts.stubbed, 0);
    assert_eq!(counts.dropped, 0);
}

#[test]
fn a_minted_id_is_counted() {
    let mut messages = vec![assistant(vec![call("", "sh", "{}")]), tool_result("")];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.ids, 1);
    assert_eq!(counts.names, 0);
    assert_eq!(counts.arguments, 0);
    // The blank-id result still pairs positionally, so nothing is stubbed.
    assert_eq!(counts.stubbed, 0);
    assert_eq!(counts.dropped, 0);
}

#[test]
fn non_object_arguments_are_counted() {
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "{")]),
        tool_result("call_1"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.arguments, 1);
    assert_eq!(counts.names, 0);
    assert_eq!(counts.ids, 0);
    assert_eq!(counts.stubbed, 0);
    assert_eq!(counts.dropped, 0);
}

#[test]
fn an_inserted_stub_is_counted() {
    let mut messages = vec![assistant(vec![call("call_1", "sh", "{}")])];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.stubbed, 1);
    assert_eq!(counts.dropped, 0);
    assert_eq!(counts.names, 0);
    assert_eq!(counts.ids, 0);
    assert_eq!(counts.arguments, 0);
}

#[test]
fn an_orphan_result_is_counted_as_dropped() {
    let mut messages = vec![
        ChatRequestMessage::simple("assistant", "no calls here".to_string()),
        tool_result("call_ghost"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.dropped, 1);
    assert_eq!(counts.stubbed, 0);
    assert_eq!(counts.names, 0);
    assert_eq!(counts.ids, 0);
    assert_eq!(counts.arguments, 0);
}

#[test]
fn an_emptied_assistant_message_counts_as_dropped_too() {
    // One emptied assistant message plus one orphan result: both are drops.
    let mut messages = vec![
        ChatRequestMessage {
            role: "assistant",
            content: Some("   ".to_string()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_artifact: None,
        },
        ChatRequestMessage::simple("assistant", "no calls here".to_string()),
        tool_result("call_ghost"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.dropped, 2);
    assert_eq!(messages.len(), 1);
}

#[test]
fn counts_aggregate_every_site_of_a_kind() {
    // Two of each: blank names, blank ids, unusable arguments, unanswered calls
    // (two stubs) and two drops (one emptied assistant message, one orphan).
    let mut messages = vec![
        ChatRequestMessage::simple("user", "hi".to_string()),
        assistant(vec![call("", "", "{"), call("", "", "{")]),
        ChatRequestMessage {
            role: "assistant",
            content: Some("   ".to_string()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_artifact: None,
        },
        ChatRequestMessage::simple("assistant", "no calls here".to_string()),
        tool_result("call_ghost"),
    ];
    let counts = repair_replayed_calls(&mut messages, true);
    assert_eq!(counts.names, 2);
    assert_eq!(counts.ids, 2);
    assert_eq!(counts.arguments, 2);
    assert_eq!(counts.stubbed, 2);
    assert_eq!(counts.dropped, 2);
}

// ── summary rendering ──

#[test]
fn summary_renders_the_counters_in_a_fixed_order() {
    // The diagnostic line is greppable, so the order and the labels are part of
    // its contract: a `session_inspect` reader matches `names=1` positionally.
    let counts = RepairCounts {
        names: 1,
        ids: 2,
        arguments: 3,
        stubbed: 4,
        dropped: 5,
    };
    assert_eq!(
        counts.summary(),
        "names=1 ids=2 arguments=3 stubbed=4 dropped=5"
    );
}

#[test]
fn summary_of_a_clean_pass_still_names_every_counter() {
    // All-zero must render the five zeroes rather than an empty string: the
    // report prints `none` itself for a clean pass, so an empty summary would
    // silently hide a counter a future kind added.
    let counts = RepairCounts::default();
    assert!(counts.is_empty());
    assert_eq!(
        counts.summary(),
        "names=0 ids=0 arguments=0 stubbed=0 dropped=0"
    );
}

// ── determinism ──

#[test]
fn repair_is_deterministic() {
    let build = || {
        vec![
            ChatRequestMessage::simple("user", "hello".to_string()),
            assistant(vec![
                call("", "", "not json"),
                call("dup", "multi_tool_use.parallel", "{}"),
                call("dup", "", "{}"),
            ]),
            tool_result(""),
        ]
    };
    let mut first = build();
    // Repair a clone twice and compare the rendered forms — the second pass
    // must be byte-identical to the first, or the cache prefix would churn.
    let mut second = build();
    let first_counts = repair_replayed_calls(&mut first, true);
    let second_counts = repair_replayed_calls(&mut second, true);
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
    // The reported counts are part of that determinism: two identical inputs
    // must report identical repairs, or the metric would drift per request.
    assert_eq!(first_counts, second_counts);
}
