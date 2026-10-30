//! The chat adapter's reasoning re-emission suite: how one projected wire
//! message carries a round-trip artifact under a route's echo policy, and which
//! assistant turns the projection drops.
//!
//! The helpers below build the two inputs every case needs — a message carrying
//! one artifact, and the [`ChatReasoningEcho`] policy of the route it is sent on
//! — so each test states only the policy and the payload it is about.

// The suite repeats its parent's imports at one level more depth: the module
// lives inside `tests`, so `openai` is three `super`s up.
use super::super::super::*;
use super::super::super::{
    AssistantToolCall, AssistantToolFunction, ChatReasoningEcho, chat_wire_message,
    chat_wire_messages,
};
use choreo_proto::{ChatReasoningField, ReasoningArtifact};

// -- reasoning artifact re-emission (phase 4a) -------------------------

#[test]
fn chat_request_message_reemits_chat_reasoning_artifact() {
    // An assistant message carrying the opaque ChatReasoning artifact must
    // re-emit it as `reasoning_content` on the wire (DeepSeek/Kimi reject
    // a tool-loop turn that drops it).
    let msg = ChatRequestMessage {
        role: "assistant",
        content: Some("answer".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: b"Let me think step-by-step".to_vec(),
        }),
    };
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert_eq!(body["reasoning_content"], "Let me think step-by-step");
    // The artifact field itself never appears on the wire.
    assert!(body.get("reasoning_artifact").is_none());
}

#[test]
fn chat_request_message_reemits_to_captured_field() {
    // The artifact records WHICH chat field the provider used; re-emission
    // must target that same field — a `reasoning_text` artifact goes back
    // as `reasoning_text`, a `reasoning` artifact as `reasoning`, and
    // neither leaks into `reasoning_content` (the historical default).
    for (field, wire_key) in [
        (ChatReasoningField::ReasoningText, "reasoning_text"),
        (ChatReasoningField::Reasoning, "reasoning"),
    ] {
        let msg = ChatRequestMessage {
            role: "assistant",
            content: Some("answer".into()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field,
                bytes: b"provider reasoning".to_vec(),
            }),
        };
        let body =
            serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
        assert_eq!(body[wire_key], "provider reasoning");
        for other in ["reasoning_content", "reasoning", "reasoning_text"] {
            if other != wire_key {
                assert!(
                    body.get(other).is_none(),
                    "{other} must be absent when re-emitting {wire_key}"
                );
            }
        }
        assert!(body.get("reasoning_artifact").is_none());
    }
}

#[test]
fn chat_request_message_without_artifact_omits_reasoning_content() {
    // Control: no artifact → no `reasoning_content` on the wire.
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn chat_request_message_corrupt_artifact_is_dropped_not_fatal() {
    // A persisted artifact whose bytes are not valid UTF-8 must NOT fail
    // the whole request serialization (the pre-fix behavior surfaced a
    // corrupted DB blob as a hard error on every subsequent turn). It is
    // logged and dropped: the message serializes as a plain assistant
    // message with no reasoning echo.
    let msg = ChatRequestMessage {
        role: "assistant",
        content: Some("answer".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            // 0xFF is never valid UTF-8.
            bytes: vec![0xFF, 0xFE, 0x00, 0x41],
        }),
    };
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert_eq!(body["content"], "answer");
    assert!(body.get("reasoning_content").is_none());
    assert!(body.get("reasoning_artifact").is_none());
}

#[test]
fn chat_request_message_non_assistant_role_drops_artifact() {
    // The artifact is assistant-only: a user message must never carry
    // `reasoning_content`, even if an artifact were attached.
    let msg = ChatRequestMessage {
        role: "user",
        content: Some("hi".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: b"should not leak".to_vec(),
        }),
    };
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert!(body.get("reasoning_content").is_none());
    assert!(body.get("reasoning_artifact").is_none());
}

#[test]
fn chat_request_message_wrong_artifact_variant_does_not_leak() {
    // A non-ChatReasoning artifact is foreign to this adapter — it must
    // not be misinterpreted as chat reasoning.
    let msg = ChatRequestMessage {
        role: "assistant",
        content: Some("answer".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::GoogleSignatures(
            b"encrypted-sig".to_vec(),
        )),
    };
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert!(body.get("reasoning_content").is_none());
    assert!(body.get("reasoning_artifact").is_none());
}

// -- route-declared reasoning echo policy ----------------------------

/// A chat artifact whose payload was observed under `observed`.
fn chat_artifact(observed: ChatReasoningField, text: &[u8]) -> ReasoningArtifact {
    ReasoningArtifact::ChatReasoning {
        field: observed,
        bytes: text.to_vec(),
    }
}

fn assistant_with(artifact: ReasoningArtifact) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "assistant",
        content: Some("answer".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(artifact),
    }
}

/// The echo policy a route that declares `field` resolves to.
fn declared(field: ChatReasoningField) -> ChatReasoningEcho {
    ChatReasoningEcho {
        field: Some(field),
        require_present: true,
    }
}

fn wire(msg: &ChatRequestMessage, echo: ChatReasoningEcho) -> serde_json::Value {
    serde_json::to_value(chat_wire_message(msg, echo)).unwrap()
}

/// A structured details artifact from a JSON entry array.
fn details_artifact(entries: &serde_json::Value) -> ReasoningArtifact {
    ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningDetails,
        bytes: serde_json::to_vec(entries).unwrap(),
    }
}

#[test]
fn declared_details_replays_openrouter_entries_verbatim() {
    // Verbatim replay: index, format and signature stay intact, and no
    // scalar field is invented beside the array.
    let entries = serde_json::json!([{
        "type":"reasoning.text","text":"think","signature":"sig","format":"f","index":0
    }]);
    let msg = assistant_with(details_artifact(&entries));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(body["reasoning_details"], entries);
    for scalar in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(scalar).is_none(), "{scalar} must stay absent");
    }
}

#[test]
fn declared_details_strips_the_stream_index_and_emits_no_scalar_mirror() {
    // A persisted bare payload keeps a stream `index`; replay strips it. The
    // array is the only reasoning field the message carries — a scalar mirror
    // beside it is a shape no upstream documents.
    let msg = assistant_with(details_artifact(&serde_json::json!([
        {"type":"summary","summary":"step one","index":0}
    ])));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(
        body["reasoning_details"],
        serde_json::json!([{"type":"summary","summary":"step one"}])
    );
    for scalar in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(scalar).is_none(), "{scalar} must stay absent");
    }
}

#[test]
fn declaring_details_route_emits_an_empty_array_when_nothing_to_echo() {
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(body["reasoning_details"], serde_json::json!([]));
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn injected_empty_details_does_not_keep_an_empty_message_alive() {
    // `reasoning_details: []` is a presence marker, not a payload.
    let messages = vec![
        ChatRequestMessage::simple("user", "q".into()),
        ChatRequestMessage::simple("assistant", String::new()),
    ];
    assert_eq!(
        project(&messages, declared(ChatReasoningField::ReasoningDetails)),
        (vec!["user".to_string()], 1)
    );
}

#[test]
fn non_empty_details_keeps_an_empty_assistant_message_alive() {
    // The array IS the payload: dropping the message would starve the echo.
    let mut msg = assistant_with(details_artifact(&serde_json::json!([
        {"type":"reasoning.encrypted","data":"opaque","index":0}
    ])));
    msg.content = Some(String::new());
    let messages = vec![ChatRequestMessage::simple("user", "q".into()), msg];
    let (roles, dropped) = project(&messages, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(roles, vec!["user".to_string(), "assistant".to_string()]);
    assert_eq!(dropped, 0);
}

#[test]
fn observed_details_replays_details_without_a_declaration() {
    // With no declared field the observation routes the payload: a details
    // artifact goes back as `reasoning_details`.
    let msg = assistant_with(details_artifact(&serde_json::json!([
        {"type":"reasoning.text","text":"t","index":0}
    ])));
    let body = wire(&msg, ChatReasoningEcho::default());
    assert!(body.get("reasoning_details").is_some());
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn declared_scalar_route_uses_details_display_text() {
    // Cross-route mismatch: a structured artifact on a route that declares a
    // scalar field replays its derived text, never raw JSON.
    let msg = assistant_with(details_artifact(&serde_json::json!([
        {"type":"reasoning.text","text":"think","index":0},
        {"type":"reasoning.summary","summary":" more","index":0}
    ])));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "think more");
}

#[test]
fn scalar_artifact_on_a_details_route_round_trips_as_a_text_entry() {
    // The mirror image of `declared_scalar_route_uses_details_display_text`:
    // a scalar payload observed under `reasoning_content` reaching a route
    // that declares the structured array is replayed as one
    // `reasoning.text` entry rather than discarded (a catalog/overlay flip
    // can put a scalar artifact on a details route).
    let msg = assistant_with(chat_artifact(
        ChatReasoningField::ReasoningContent,
        b"recoverable thought",
    ));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(
        body["reasoning_details"],
        serde_json::json!([{"type":"reasoning.text","text":"recoverable thought"}])
    );
    // Exactly one destination: no scalar reasoning key beside the array.
    for scalar in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(scalar).is_none(), "{scalar} must stay absent");
    }
}

#[test]
fn undeclared_route_injects_no_empty_details_array_for_an_unreplayable_artifact() {
    // Nothing is declared, so the destination came only from the
    // observation: an artifact that lowers to nothing must not materialize a
    // brand-new `reasoning_details: []` key the route never asked for.
    let msg = assistant_with(ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningDetails,
        bytes: b"not json".to_vec(),
    });
    let body = wire(&msg, ChatReasoningEcho::default());
    assert!(
        body.get("reasoning_details").is_none(),
        "an undeclared route must not receive an injected empty array: {body}"
    );
    for scalar in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(scalar).is_none(), "{scalar} must stay absent");
    }
}

#[test]
fn declaring_details_route_still_emits_an_empty_array_for_a_corrupt_artifact() {
    // A declared structured route keeps its presence marker even when the
    // persisted payload is unreplayable: the array is `[]`, never absent.
    let msg = assistant_with(ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningDetails,
        bytes: b"not json".to_vec(),
    });
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(body["reasoning_details"], serde_json::json!([]));
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn declared_field_overrides_the_observed_field() {
    // The live-failure shape: the gateway streamed the reasoning under
    // `reasoning` yet rejects any history without `reasoning_content`.
    // The catalog's declared field wins over the observation, and exactly
    // one reasoning key reaches the wire.
    let msg = assistant_with(chat_artifact(ChatReasoningField::Reasoning, b"thought"));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "thought");
    assert!(
        body.get("reasoning").is_none(),
        "the observed field must not survive beside the declared one"
    );
}

#[test]
fn declared_non_content_field_also_wins() {
    // The policy is not hard-wired to `reasoning_content`: any declared
    // field wins over the observation.
    let msg = assistant_with(chat_artifact(
        ChatReasoningField::ReasoningContent,
        b"thought",
    ));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningText));
    assert_eq!(body["reasoning_text"], "thought");
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn observed_field_is_used_when_the_route_declares_none() {
    // With no declared field the observation is the destination — a
    // provider that answers under `reasoning_text` gets `reasoning_text`
    // back rather than a key it never used.
    for observed in [
        ChatReasoningField::ReasoningContent,
        ChatReasoningField::Reasoning,
        ChatReasoningField::ReasoningText,
    ] {
        let msg = assistant_with(chat_artifact(observed, b"payload"));
        let body = wire(&msg, ChatReasoningEcho::default());
        let key = match observed {
            ChatReasoningField::ReasoningContent => "reasoning_content",
            ChatReasoningField::Reasoning => "reasoning",
            ChatReasoningField::ReasoningText => "reasoning_text",
            ChatReasoningField::ReasoningDetails => "reasoning_details",
        };
        assert_eq!(body[key], "payload", "{key}");
        for other in ["reasoning_content", "reasoning", "reasoning_text"] {
            if other != key {
                assert!(body.get(other).is_none(), "{other} must stay absent");
            }
        }
    }
}

#[test]
fn required_field_is_present_but_empty_when_there_is_nothing_to_echo() {
    // An assistant message with no artifact on a route that requires the
    // field: the key exists, empty. This is what keeps a DeepSeek/GLM
    // history valid on a turn that produced no reasoning at all.
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "");
}

#[test]
fn no_key_when_the_route_declares_nothing_and_there_is_nothing_to_echo() {
    // Without a declared field and without an artifact, no reasoning key is
    // invented — the message is serialized exactly as recorded.
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let body = wire(&msg, ChatReasoningEcho::default());
    for key in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(key).is_none(), "{key} must not be invented");
    }
}

#[test]
fn required_field_is_present_for_a_foreign_artifact_variant() {
    // An artifact this adapter cannot interpret must never satisfy the
    // required field with garbage — but the field itself still has to be
    // there, so it goes out empty.
    let msg = assistant_with(ReasoningArtifact::GoogleSignatures(b"sig".to_vec()));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "");
    assert!(body.get("reasoning_artifact").is_none());
}

#[test]
fn corrupt_artifact_still_satisfies_a_required_field() {
    // A corrupted persisted blob drops the payload but must not remove the
    // required key: the message stays valid instead of 400-ing forever.
    let msg = assistant_with(chat_artifact(
        ChatReasoningField::ReasoningContent,
        &[0xFF, 0xFE],
    ));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "");
    assert_eq!(body["content"], "answer");
}

// -- empty-message projection ----------------------------------------

/// Lower a slice, returning the wire roles and how many were dropped.
fn project(messages: &[ChatRequestMessage], echo: ChatReasoningEcho) -> (Vec<String>, usize) {
    let lowered = chat_wire_messages(messages, echo);
    (
        lowered
            .messages
            .iter()
            .map(|m| m.role().to_string())
            .collect(),
        lowered.dropped,
    )
}

#[test]
fn empty_assistant_message_is_dropped() {
    // An assistant turn that produced no visible content and carries no
    // reasoning payload is dropped from the request rather than sent empty
    // (a message with neither content nor tool calls is rejected outright).
    let messages = vec![
        ChatRequestMessage::simple("user", "q".into()),
        ChatRequestMessage::simple("assistant", String::new()),
    ];
    assert_eq!(
        project(&messages, ChatReasoningEcho::default()),
        (vec!["user".to_string()], 1)
    );
}

#[test]
fn whitespace_only_assistant_message_is_dropped() {
    // A blank "answer" is not a payload: strict servers treat it as an empty
    // message, so the projection trims before deciding.
    let messages = vec![
        ChatRequestMessage::simple("user", "q".into()),
        ChatRequestMessage::simple("assistant", " \n\t ".into()),
    ];
    assert_eq!(
        project(&messages, ChatReasoningEcho::default()),
        (vec!["user".to_string()], 1)
    );
}

#[test]
fn empty_assistant_message_with_a_reasoning_artifact_survives() {
    // The echo IS the payload: dropping this message would starve the
    // reasoning round-trip the route requires.
    let mut msg = assistant_with(chat_artifact(ChatReasoningField::Reasoning, b"thinking"));
    msg.content = Some(String::new());
    let messages = vec![ChatRequestMessage::simple("user", "q".into()), msg];
    let (roles, dropped) = project(&messages, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(roles, vec!["user".to_string(), "assistant".to_string()]);
    assert_eq!(dropped, 0);
}

#[test]
fn empty_assistant_message_with_tool_calls_survives() {
    let mut msg = ChatRequestMessage::simple("assistant", String::new());
    msg.tool_calls = Some(vec![AssistantToolCall {
        id: "call_1".into(),
        kind: "function".into(),
        function: AssistantToolFunction {
            name: "sh".into(),
            arguments: "{}".into(),
        },
        caller: None,
    }]);
    let messages = vec![ChatRequestMessage::simple("user", "q".into()), msg];
    let (roles, dropped) = project(&messages, ChatReasoningEcho::default());
    assert_eq!(roles, vec!["user".to_string(), "assistant".to_string()]);
    assert_eq!(dropped, 0);
}

#[test]
fn a_tool_call_caller_never_reaches_the_chat_wire() {
    // `caller` is a Responses-API attribution for resuming a generated program;
    // a chat-completions `tool_calls` entry has no such field. A history that
    // captured one under a Responses model and then switched to a chat route
    // must not hand the chat provider a field its wire does not define — and a
    // call with no attribution must not gain a `caller: null` either.
    let mut attributed = ChatRequestMessage::simple("assistant", String::new());
    attributed.tool_calls = Some(vec![AssistantToolCall {
        id: "call_1".into(),
        kind: "function".into(),
        function: AssistantToolFunction {
            name: "sh".into(),
            arguments: "{}".into(),
        },
        caller: Some(crate::CallerInfo {
            kind: "program".into(),
            caller_id: "prog_123".into(),
        }),
    }]);
    let body =
        serde_json::to_value(chat_wire_message(&attributed, ChatReasoningEcho::default())).unwrap();
    assert_eq!(body["tool_calls"][0]["id"], "call_1");
    assert_eq!(body["tool_calls"][0]["function"]["name"], "sh");
    assert!(body["tool_calls"][0].get("caller").is_none());
}

#[test]
fn required_empty_field_does_not_keep_an_empty_message_alive() {
    // The required-field empty string is a presence marker, not a payload: a
    // still-empty assistant message is dropped even on a route that declares
    // the field.
    let messages = vec![
        ChatRequestMessage::simple("user", "q".into()),
        ChatRequestMessage::simple("assistant", String::new()),
    ];
    assert_eq!(
        project(&messages, declared(ChatReasoningField::ReasoningContent)),
        (vec!["user".to_string()], 1)
    );
}

#[test]
fn non_assistant_messages_are_never_dropped() {
    // The projection only removes assistant messages: every other role is
    // part of the history the request was built from, so a request can never
    // end up with an empty messages array.
    let messages = vec![
        ChatRequestMessage::simple("system", String::new()),
        ChatRequestMessage::simple("user", String::new()),
    ];
    let (roles, dropped) = project(&messages, ChatReasoningEcho::default());
    assert_eq!(roles, vec!["system".to_string(), "user".to_string()]);
    assert_eq!(dropped, 0);
}

// -- per-call output cap (cache-warming ping) --------------------------
