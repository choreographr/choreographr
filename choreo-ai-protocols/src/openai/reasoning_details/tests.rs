use super::*;
use serde_json::json;

fn capture(raw: &Value) -> DetailsCapture {
    let mut capture = DetailsCapture::default();
    capture.push(raw, &mut |_| {});
    capture
}

#[test]
fn a_double_encoded_array_string_folds_like_a_real_array() {
    // Some proxies serialize the array a second time, so the delta arrives as
    // a JSON STRING. Decoding it once must yield exactly the capture a real
    // array produces — same entries, same display, same replay.
    let entries = json!([
        {"type":"reasoning.text","text":"Let me ","index":0},
        {"type":"reasoning.text","text":"think","index":0}
    ]);
    let double_encoded = Value::String(entries.to_string());
    let folded = capture(&double_encoded);
    assert_eq!(folded.display(), "Let me think");
    let plain = capture(&entries);
    assert_eq!(folded.display(), plain.display());
    let artifact = folded.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    assert_eq!(
        serde_json::from_slice::<Value>(bytes).expect("json"),
        json!([{"type":"reasoning.text","text":"Let me think","index":0}])
    );

    // A double-encoded single entry object is the same tolerance one level
    // down: it folds as that one entry.
    let single = capture(&Value::String(
        json!({"type":"summary","summary":"only","index":0}).to_string(),
    ));
    assert_eq!(single.display(), "only");
}

#[test]
fn non_structured_values_are_ignored_without_capture() {
    // A bare number and a string that is not JSON are neither entries nor
    // encoded payloads: they are ignored (logged by kind only) and the
    // capture stays empty rather than inventing an entry.
    let mut capture = DetailsCapture::default();
    let mut fragments = Vec::new();
    for raw in [
        json!(42),
        json!("not json at all"),
        json!(null),
        json!(true),
    ] {
        capture.push(&raw, &mut |fragment| fragments.push(fragment.to_string()));
    }
    assert!(fragments.is_empty(), "no display fragment from a non-entry");
    assert!(!capture.has_entries());
    assert!(capture.finish().is_none());
}

#[test]
fn push_reports_fragments_through_the_callback() {
    // The callback is the display channel: it receives every recognised
    // textual fragment in arrival order and nothing else (the encrypted and
    // unrecognised entries yield no display text).
    let mut capture = DetailsCapture::default();
    let mut fragments = Vec::new();
    capture.push(
        &json!([
            {"type":"reasoning.text","text":"alpha","index":0},
            {"type":"reasoning.encrypted","data":"opaque","index":1},
            {"type":"notes","text":"ignored","index":0},
            {"type":"reasoning.summary","summary":"beta","index":0}
        ]),
        &mut |fragment| fragments.push(fragment.to_string()),
    );
    assert_eq!(fragments, vec!["alpha".to_string(), "beta".to_string()]);
    // The same fragments are what the derived display text concatenates.
    assert_eq!(capture.display(), "alphabeta");
}

#[test]
fn openrouter_text_entries_are_kept_verbatim_and_merged() {
    // Two streamed fragments of the same text block, same index, with the
    // signature arriving on the second delta: merge into one entry that
    // keeps every field (index, format, signature).
    let capture = capture(&json!([
        {"type":"reasoning.text","text":"Let me ","index":0,"format":"openai-responses-v1"},
        {"type":"reasoning.text","text":"think","index":0,"signature":"sig_abc"}
    ]));
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { field, bytes } = &artifact else {
        panic!("expected ChatReasoning");
    };
    assert_eq!(*field, ChatReasoningField::ReasoningDetails);
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(
        entries,
        json!([{
            "type":"reasoning.text",
            "text":"Let me think",
            "index":0,
            "format":"openai-responses-v1",
            "signature":"sig_abc"
        }])
    );
    assert_eq!(capture_display(&artifact), "Let me think");
}

#[test]
fn encrypted_entries_stay_discrete() {
    let capture = capture(&json!([
        {"type":"reasoning.encrypted","data":"AAA","index":0},
        {"type":"reasoning.encrypted","data":"BBB","index":1}
    ]));
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(entries.as_array().map(Vec::len), Some(2));
    assert_eq!(capture_display(&artifact), "");
}

#[test]
fn kimi_bare_entries_merge_and_keep_unknown_types_verbatim() {
    let raw = json!([
        {"type":"summary","summary":"step ","index":0},
        {"type":"summary","summary":"one","index":0},
        {"type":"notes","text":"kept","index":0},
        {"type":"encrypted","data":"ZZZ","index":1}
    ]);
    let capture = capture(&raw);
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    // The same-index summaries merge; the unknown bare entry is kept exactly
    // as it arrived so the sequence the upstream sent can be replayed.
    assert_eq!(
        entries,
        json!([
            {"type":"summary","summary":"step one","index":0},
            {"type":"notes","text":"kept","index":0},
            {"type":"encrypted","data":"ZZZ","index":1}
        ])
    );
    // An unknown tag contributes no display text.
    assert_eq!(capture_display(&artifact), "step one");
    // ... and replays in place: the recognised bare entries are rebuilt
    // without the stream-only `index`, while the unknown one is replayed
    // exactly as it was captured.
    assert_eq!(
        details_wire_value(bytes),
        json!([
            {"type":"summary","summary":"step one"},
            {"type":"notes","text":"kept","index":0},
            {"type":"encrypted","data":"ZZZ"}
        ])
    );
}

#[test]
fn unrecognised_only_round_trips_verbatim_without_display() {
    let raw = json!([{"type":"reasoning.image","data":"AA","index":0}]);
    let capture = capture(&raw);
    // An unrecognised-only turn is still model output, not an empty response.
    assert!(capture.has_entries());
    assert_eq!(capture.display(), "");
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(entries, raw);
    assert_eq!(details_wire_value(bytes), raw);
}

#[test]
fn typeless_entry_is_not_an_entry() {
    let capture = capture(&json!([{"text":"no type tag"}, {"data":"x"}]));
    assert!(!capture.has_entries());
    assert!(capture.finish().is_none());
}

#[test]
fn entries_without_an_index_merge_as_one_block() {
    // Neither entry names an index, so the provider does not distinguish
    // stream slots: consecutive same-kind fragments are one logical block
    // and merge, with the later fragment's metadata carried over.
    let capture = capture(&json!([
        {"type":"reasoning.text","text":"first"},
        {"type":"reasoning.text","text":"second","signature":"sig"}
    ]));
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(
        entries,
        json!([{"type":"reasoning.text","text":"firstsecond","signature":"sig"}])
    );
    assert_eq!(capture_display(&artifact), "firstsecond");
}

#[test]
fn mixed_index_presence_is_not_merged() {
    // One entry names a slot and the next does not: absent evidence must not
    // fuse two blocks, so they stay discrete.
    let capture = capture(&json!([
        {"type":"reasoning.text","text":"one","index":0},
        {"type":"reasoning.text","text":"two"}
    ]));
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(
        entries,
        json!([
            {"type":"reasoning.text","text":"one","index":0},
            {"type":"reasoning.text","text":"two"}
        ])
    );
}

#[test]
fn entries_with_different_indices_are_not_merged() {
    // Distinct slots: no text concatenation and no metadata grafted onto the
    // earlier block.
    let capture = capture(&json!([
        {"type":"reasoning.text","text":"one","index":0},
        {"type":"reasoning.text","text":"two","index":1,"signature":"sig"}
    ]));
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(
        entries,
        json!([
            {"type":"reasoning.text","text":"one","index":0},
            {"type":"reasoning.text","text":"two","index":1,"signature":"sig"}
        ])
    );
}

#[test]
fn display_derives_from_text_and_summary_entries() {
    let capture = capture(&json!([
        {"type":"reasoning.text","text":"alpha","index":0},
        {"type":"reasoning.summary","summary":" beta","index":0},
        {"type":"reasoning.encrypted","data":"opaque","index":1}
    ]));
    assert_eq!(capture.display(), "alpha beta");
}

#[test]
fn replay_keeps_namespaced_entries_verbatim() {
    let persisted =
        br#"[{"type":"reasoning.text","text":"hi","signature":"s","format":"f","index":3}]"#;
    let wire = details_wire_value(persisted);
    assert_eq!(
        wire,
        json!([{"type":"reasoning.text","text":"hi","signature":"s","format":"f","index":3}])
    );
}

#[test]
fn replay_rebuilds_bare_entries_without_index() {
    // A persisted Kimi payload carries the stream `index`; replay must
    // rebuild it minimally, stripping the index.
    let persisted = br#"[{"type":"summary","summary":"sum","index":0},{"type":"encrypted","data":"enc","index":1}]"#;
    let wire = details_wire_value(persisted);
    assert_eq!(
        wire,
        json!([{"type":"summary","summary":"sum"},{"type":"encrypted","data":"enc"}])
    );
}

#[test]
fn companion_mirrors_only_kimi_summary_text() {
    let kimi = details_wire_value(br#"[{"type":"summary","summary":"a","index":0},{"type":"summary","summary":"b","index":0}]"#);
    assert_eq!(details_companion(&kimi).as_deref(), Some("ab"));
    // OpenRouter dialect: the array is read alone.
    let openrouter =
        details_wire_value(br#"[{"type":"reasoning.summary","summary":"a","index":0}]"#);
    assert_eq!(details_companion(&openrouter), None);
    // Encrypted-only: nothing textual to mirror.
    let encrypted = details_wire_value(br#"[{"type":"encrypted","data":"x","index":0}]"#);
    assert_eq!(details_companion(&encrypted), None);
    // Empty array injects the key without a companion.
    assert_eq!(details_companion(&json!([])), None);
}

#[test]
fn oversized_payload_is_dropped_at_capture() {
    let big = "x".repeat(MAX_REASONING_DETAILS_BYTES + 1);
    let capture = capture(&json!([{"type":"reasoning.text","text":big,"index":0}]));
    assert!(!capture.has_entries());
    assert!(capture.finish().is_none());
}

#[test]
fn overflowed_capture_reports_no_display_text() {
    // The payload is dropped, so the session must not display reasoning it can
    // never replay — display() agrees with has_entries()/finish().
    let big = "y".repeat(MAX_REASONING_DETAILS_BYTES + 1);
    let capture = capture(&json!([{"type":"reasoning.text","text":big,"index":0}]));
    assert_eq!(capture.display(), "");
}

#[test]
fn oversized_persisted_artifact_replays_empty() {
    let mut bytes = Vec::from(br#"[{"type":"reasoning.text","text":""#);
    bytes.extend(std::iter::repeat_n(b'x', MAX_REASONING_DETAILS_BYTES));
    bytes.extend_from_slice(br#""}]"#);
    assert_eq!(details_wire_value(&bytes), json!([]));
}

#[test]
fn corrupt_artifact_replays_empty() {
    assert_eq!(details_wire_value(b"not json"), json!([]));
    assert_eq!(
        details_wire_value(br#"{"type":"reasoning.text"}"#),
        json!([])
    );
}

fn capture_display(artifact: &ReasoningArtifact) -> String {
    let ReasoningArtifact::ChatReasoning { bytes, .. } = artifact else {
        return String::new();
    };
    details_display_text(bytes).unwrap_or_default()
}
