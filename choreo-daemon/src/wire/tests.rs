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

fn tool_result(id: &str) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "tool",
        content: Some("ok".to_string()),
        images: Vec::new(),
        tool_call_id: Some(id.to_string()),
        tool_calls: None,
        reasoning_artifact: None,
    }
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
    assert!(is_provider_safe_function_name(&out));
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
    assert!(is_provider_safe_function_name(&out));
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
    // Two calls, but the result ids name neither — positional pairing still
    // consumes each call once, so the extra (third) result is an orphan.
    let mut messages = vec![
        assistant(vec![call("call_1", "sh", "{}")]),
        tool_result("call_1"),
        tool_result("call_1"),
    ];
    repair_replayed_calls(&mut messages, true);
    assert_eq!(messages.len(), 2);
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
    repair_replayed_calls(&mut first, true);
    repair_replayed_calls(&mut second, true);
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
}
