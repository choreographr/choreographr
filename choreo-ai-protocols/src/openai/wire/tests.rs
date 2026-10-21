use super::*;

/// An artifact whose serialized text starts with `label` and is exactly
/// `payload_len` bytes long (the label pads with `x`), so a test can both
/// identify an artifact on the wire and size it against the budget.
fn artifact(label: &str, payload_len: usize) -> ReasoningArtifact {
    let mut bytes = label.as_bytes().to_vec();
    bytes.resize(payload_len.max(label.len()), b'x');
    ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningContent,
        bytes,
    }
}

/// An assistant message carrying one artifact, with content so the projection
/// never drops it (a dropped message would shift the positions under test).
fn assistant(label: &str, payload_len: usize) -> ChatRequestMessage {
    ChatRequestMessage {
        reasoning_artifact: Some(artifact(label, payload_len)),
        ..ChatRequestMessage::simple("assistant", "answer".to_string())
    }
}

fn user() -> ChatRequestMessage {
    ChatRequestMessage::simple("user", "prompt".to_string())
}

/// A route that names no field and requires none: a skipped artifact leaves
/// the message with no reasoning key at all.
fn plain_echo() -> ChatReasoningEcho {
    ChatReasoningEcho {
        field: None,
        require_present: false,
    }
}

/// A route that requires the reasoning key's presence: a skipped artifact
/// must still emit the empty presence marker.
fn pinned_echo() -> ChatReasoningEcho {
    ChatReasoningEcho {
        field: None,
        require_present: true,
    }
}

/// The `reasoning_content` payload each kept message carries, in order.
fn replayed(wire: &ChatWireMessages<'_>) -> Vec<Option<String>> {
    wire.messages
        .iter()
        .map(|message| {
            let json = serde_json::to_value(message).expect("serialize");
            json.get("reasoning_content")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn a_request_within_budget_replays_every_artifact() {
    let messages = vec![
        user(),
        assistant("alpha:", 1_000),
        user(),
        assistant("beta:", 2_000),
    ];
    let payloads = replayed(&chat_wire_messages(&messages, plain_echo()));
    assert_eq!(payloads.len(), 4);
    assert!(
        payloads[1]
            .as_deref()
            .is_some_and(|text| text.starts_with("alpha:")),
        "every artifact under the budget replays"
    );
    assert!(
        payloads[3]
            .as_deref()
            .is_some_and(|text| text.starts_with("beta:")),
        "every artifact under the budget replays"
    );
    assert!(
        payloads[1]
            .as_deref()
            .is_some_and(|text| text.len() == 1_000)
            && payloads[3]
                .as_deref()
                .is_some_and(|text| text.len() == 2_000),
        "the replayed payload is the artifact itself"
    );
}

#[test]
fn a_request_over_budget_replays_the_newest_artifacts() {
    // Four artifacts of just over a third of the budget each: the newest three
    // are accepted before the running total passes the budget, so the oldest
    // is the one that loses its payload.
    let each = MAX_REQUEST_REASONING_BYTES / 3 + 1;
    let messages = vec![
        assistant("oldest:", each),
        assistant("second:", each),
        assistant("third:", each),
        assistant("newest:", each),
    ];
    let payloads = replayed(&chat_wire_messages(&messages, pinned_echo()));
    // The dropped turn keeps its place on a require-present route, carrying the
    // empty presence marker rather than omitting the key.
    assert_eq!(payloads[0], Some(String::new()));
    for (index, label) in [(1, "second:"), (2, "third:"), (3, "newest:")] {
        assert!(
            payloads[index]
                .as_deref()
                .is_some_and(|text| text.starts_with(label)),
            "message {index} should still replay its artifact"
        );
    }
}

#[test]
fn the_newest_artifact_replays_even_when_it_alone_exceeds_the_budget() {
    let messages = vec![
        assistant("old:", 10),
        assistant("huge:", MAX_REQUEST_REASONING_BYTES + 1),
    ];
    let payloads = replayed(&chat_wire_messages(&messages, plain_echo()));
    assert_eq!(
        payloads[0], None,
        "the older artifact is dropped once the newest has spent the budget"
    );
    assert!(
        payloads[1]
            .as_deref()
            .is_some_and(|text| text.starts_with("huge:")),
        "the newest artifact replays even alone over budget"
    );
}

#[test]
fn artifact_free_messages_consume_no_budget() {
    // The same four over-budget artifacts as above, but with artifact-free user
    // turns and one artifact-free assistant turn interleaved: neither consumes
    // budget, so exactly the same artifacts replay.
    let each = MAX_REQUEST_REASONING_BYTES / 3 + 1;
    let bare_assistant = ChatRequestMessage::simple("assistant", "no artifact".to_string());
    let messages = vec![
        user(),
        user(),
        assistant("oldest:", each),
        user(),
        bare_assistant,
        assistant("second:", each),
        user(),
        assistant("third:", each),
        user(),
        user(),
        assistant("newest:", each),
        user(),
    ];
    let carrying: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.reasoning_artifact.is_some())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(carrying.len(), 4, "four turns carry an artifact");
    let payloads = replayed(&chat_wire_messages(&messages, plain_echo()));
    // Nothing is dropped (every message has content), so positions line up.
    assert_eq!(payloads.len(), messages.len());
    assert_eq!(
        payloads[carrying[0]], None,
        "the oldest artifact is still the one dropped"
    );
    for (label, index) in ["second:", "third:", "newest:"]
        .into_iter()
        .zip(&carrying[1..])
    {
        assert!(
            payloads[*index]
                .as_deref()
                .is_some_and(|text| text.starts_with(label)),
            "message {index} should still replay its artifact"
        );
    }
    assert_eq!(
        payloads[4], None,
        "an artifact-free assistant turn carries no reasoning key"
    );
}

#[test]
fn presence_pin_without_a_destination_emits_an_empty_key() {
    // A route whose catalog fact demands the field without naming one: the
    // lowering falls back to the chat default `reasoning_content` and writes
    // it EMPTY as a presence marker, so the message never omits the key.
    let messages = vec![ChatRequestMessage::simple("assistant", String::new())];
    let echo = ChatReasoningEcho {
        field: None,
        require_present: true,
    };
    let wire = chat_wire_message(&messages[0], echo);
    let json = serde_json::to_value(&wire).expect("serialize");
    assert_eq!(
        json.get("reasoning_content"),
        Some(&serde_json::Value::String(String::new())),
        "the presence pin must put the key on the wire, empty"
    );
    // The empty scalar is a presence marker, not a payload, so it does not
    // keep an otherwise-empty turn alive: the request projection still drops
    // it. The pin only matters on a message that survives for another reason.
    assert!(wire.is_empty());
    assert_eq!(chat_wire_messages(&messages, echo).dropped, 1);
}

#[test]
fn scalar_text_on_a_details_route_becomes_one_text_entry() {
    let artifact = ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningContent,
        bytes: b"recoverable thought".to_vec(),
    };
    let details = lower_reasoning(
        "assistant",
        Some(&artifact),
        ChatReasoningEcho {
            field: Some(ChatReasoningField::ReasoningDetails),
            require_present: true,
        },
    );
    let Some(WireReasoning::Details(details)) = details else {
        panic!("expected a details emission");
    };
    assert_eq!(
        details,
        serde_json::json!([{"type":"reasoning.text","text":"recoverable thought"}])
    );
}

#[test]
fn blank_scalar_text_is_not_a_details_entry() {
    assert_eq!(
        lower_scalar_text_as_details(b"  \n"),
        serde_json::json!([]),
        "blank text must not invent an entry"
    );
    assert_eq!(
        lower_scalar_text_as_details(&[0xFF, 0xFE]),
        serde_json::json!([]),
        "non-UTF-8 bytes must not invent an entry"
    );
}

#[test]
fn wire_keys_cover_every_field_variant() {
    for field in [
        ChatReasoningField::ReasoningContent,
        ChatReasoningField::Reasoning,
        ChatReasoningField::ReasoningText,
        ChatReasoningField::ReasoningDetails,
    ] {
        assert!(
            CHAT_REASONING_WIRE_KEYS.contains(&reasoning_wire_key(field)),
            "every field variant's key must be listed"
        );
    }
}
