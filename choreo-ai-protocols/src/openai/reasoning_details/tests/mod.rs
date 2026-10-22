use super::*;
use choreo_proto::{ChatReasoningField, ReasoningArtifact};
use serde_json::json;

/// Fold one value into a fresh store, returning the store and the display text
/// the fold accumulated (the two are separate answers now: the store owns the
/// replay payload, the caller owns the text).
fn capture(raw: &Value) -> DetailsStore {
    let (store, _) = capture_with_text(raw);
    store
}

/// Fold one value, keeping both answers plus the "produced output" fact.
fn capture_with_text(raw: &Value) -> (DetailsStore, String) {
    let (store, text, _produced) = capture_all(raw);
    (store, text)
}

fn capture_all(raw: &Value) -> (DetailsStore, String, bool) {
    let mut store = DetailsStore::default();
    let mut text = String::new();
    let produced = store.push(raw, &mut text, &mut |_| {});
    (store, text, produced)
}

/// The display text a full turn would persist for one value.
fn display_of(raw: &Value) -> String {
    let (_, text, _) = capture_all(raw);
    text
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
    // Both spellings must fold to the same display text and the same payload.
    assert_eq!(
        display_of(&double_encoded),
        display_of(&entries),
        "the decoded value displays exactly like a real array"
    );
    let artifact = capture(&double_encoded).finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    assert_eq!(
        serde_json::from_slice::<Value>(bytes).expect("json"),
        json!([{"type":"reasoning.text","text":"Let me think","index":0}])
    );

    // A double-encoded single entry object is the same tolerance one level
    // down: it folds as that one entry.
    let single = Value::String(json!({"type":"summary","summary":"only","index":0}).to_string());
    assert_eq!(display_of(&single), "only");
    assert_eq!(
        display_of(&single),
        display_of(&json!([{"type":"summary","summary":"only","index":0}]))
    );
}

#[test]
fn non_structured_values_are_ignored_without_capture() {
    // A bare number and a string that is not JSON are neither entries nor
    // encoded payloads: they are ignored (logged by kind only) and the
    // capture stays empty rather than inventing an entry.
    let mut store = DetailsStore::default();
    let mut text = String::new();
    let mut fragments = Vec::new();
    for raw in [
        json!(42),
        json!("not json at all"),
        json!(null),
        json!(true),
    ] {
        let produced = store.push(&raw, &mut text, &mut |fragment: &str| {
            fragments.push(fragment.to_string());
        });
        assert!(!produced, "a non-entry is not model output");
    }
    assert!(fragments.is_empty(), "no display fragment from a non-entry");
    assert!(text.is_empty(), "no display text from a non-entry");
    assert!(store.finish().is_none());
}

#[test]
fn push_reports_fragments_through_the_callback() {
    // The callback is the display channel: it receives every recognised
    // textual fragment in arrival order and nothing else (the encrypted and
    // unrecognised entries yield no display text).
    let raw = json!([
        {"type":"reasoning.text","text":"alpha","index":0},
        {"type":"reasoning.encrypted","data":"opaque","index":1},
        {"type":"notes","text":"ignored","index":0},
        {"type":"reasoning.summary","summary":"beta","index":0}
    ]);
    let mut store = DetailsStore::default();
    let mut text = String::new();
    let mut fragments = Vec::new();
    let produced = store.push(&raw, &mut text, &mut |fragment: &str| {
        fragments.push(fragment.to_string());
    });
    assert!(produced);
    assert_eq!(fragments, vec!["alpha".to_string(), "beta".to_string()]);
    // The turn's display text is exactly the fragments, concatenated in order.
    assert_eq!(text, "alphabeta");
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
fn a_null_metadata_field_is_replaced_by_a_later_real_value() {
    // `OpenRouter`'s documented stream opens a block with `"signature": null`
    // and `"format": null` and delivers the real values on a later fragment of
    // the same slot. An explicit `null` is "not stated yet" (the same reading
    // `is_continuation` uses for an identity conflict), so the later value must
    // be grafted onto the one block rather than shadowed by the earlier null —
    // otherwise the replayed block would arrive unsigned.
    let mut store = DetailsStore::default();
    let mut text = String::new();
    store.push(
        &json!([{"type":"reasoning.text","text":"a","index":0,"signature":null,"format":null}]),
        &mut text,
        &mut |_| {},
    );
    store.push(
        &json!([{"type":"reasoning.text","text":"b","index":0,"signature":"sig","format":"f"}]),
        &mut text,
        &mut |_| {},
    );
    let artifact = store.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    assert_eq!(
        serde_json::from_slice::<Value>(bytes).expect("json"),
        json!([{"type":"reasoning.text","text":"ab","index":0,"signature":"sig","format":"f"}])
    );
    assert_eq!(capture_display(&artifact), "ab");
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
fn bare_entries_merge_and_keep_unknown_types_verbatim() {
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
    let (store, text, produced) = capture_all(&raw);
    // An unrecognised-only turn is still model output, not an empty response —
    // even though it has nothing to display.
    assert!(produced);
    assert_eq!(text, "");
    let artifact = store.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(entries, raw);
    assert_eq!(details_wire_value(bytes), raw);
}

#[test]
fn typeless_entry_is_not_an_entry() {
    let (store, text, produced) = capture_all(&json!([{"text":"no type tag"}, {"data":"x"}]));
    assert!(!produced, "a typeless object is not model output");
    assert_eq!(text, "");
    assert!(store.finish().is_none());
}

#[test]
fn index_less_entries_in_one_delta_stay_discrete() {
    // Two entries carried by ONE delta are two blocks by the array contract
    // (`index` is documented as optional), so fusing them would rewrite the
    // sequence the upstream requires to be replayed unmodified.
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
        json!([
            {"type":"reasoning.text","text":"first"},
            {"type":"reasoning.text","text":"second","signature":"sig"}
        ])
    );
}

#[test]
fn index_less_entries_across_deltas_merge_as_one_block() {
    // Neither entry names an index and they arrive in DIFFERENT deltas: the
    // provider does not distinguish slots, so the later fragment continues the
    // block being assembled — the stream rule the upstream states.
    let mut store = DetailsStore::default();
    let mut text = String::new();
    store.push(
        &json!([{"type":"reasoning.text","text":"first"}]),
        &mut text,
        &mut |_| {},
    );
    store.push(
        &json!([{"type":"reasoning.text","text":"second","signature":"sig"}]),
        &mut text,
        &mut |_| {},
    );
    let artifact = store.finish().expect("artifact");
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
    // The display text is the concatenation of the textual fragments in arrival
    // order; the encrypted entry contributes nothing to it.
    let raw = json!([
        {"type":"reasoning.text","text":"alpha","index":0},
        {"type":"reasoning.summary","summary":" beta","index":0},
        {"type":"reasoning.encrypted","data":"opaque","index":1}
    ]);
    assert_eq!(display_of(&raw), "alpha beta");
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
fn replay_preserves_bare_entries_and_strips_only_the_index() {
    // A persisted bare payload carries the stream `index`; replay removes that
    // one field and keeps everything else — including the payload under
    // whichever key the provider used, so a dialect this module does not model
    // cannot lose its block.
    let persisted = br#"[{"type":"summary","summary":"sum","index":0},{"type":"encrypted","data":"enc","index":1}]"#;
    let wire = details_wire_value(persisted);
    assert_eq!(
        wire,
        json!([{"type":"summary","summary":"sum"},{"type":"encrypted","data":"enc"}])
    );

    // The payload key is whatever arrived: an `encrypted` field survives too.
    let alternate = details_wire_value(br#"[{"type":"encrypted","encrypted":"opaque","index":1}]"#);
    assert_eq!(
        alternate,
        json!([{"type":"encrypted","encrypted":"opaque"}])
    );

    // Any other field an entry carried rides along untouched.
    let extra = details_wire_value(br#"[{"type":"summary","summary":"s","index":0,"id":"x"}]"#);
    assert_eq!(extra, json!([{"type":"summary","summary":"s","id":"x"}]));
}

#[test]
fn interleaved_slots_merge_into_their_own_blocks() {
    // Two stream slots whose fragments alternate across deltas: each index keeps
    // its own accumulating block, so the array ends up with one entry per slot
    // rather than a fragment entry per delta.
    let mut store = DetailsStore::default();
    let mut text = String::new();
    for delta in [
        json!([
            {"type":"reasoning.text","text":"a1 ","index":0},
            {"type":"reasoning.text","text":"b1 ","index":1}
        ]),
        json!([
            {"type":"reasoning.text","text":"a2","index":0},
            {"type":"reasoning.text","text":"b2","index":1}
        ]),
    ] {
        store.push(&delta, &mut text, &mut |_| {});
    }
    let artifact = store.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    assert_eq!(
        serde_json::from_slice::<Value>(bytes).expect("json"),
        json!([
            {"type":"reasoning.text","text":"a1 a2","index":0},
            {"type":"reasoning.text","text":"b1 b2","index":1}
        ])
    );
}

#[test]
fn a_conflicting_signature_keeps_two_blocks_discrete() {
    // Same slot, different signature: two distinct blocks whose deltas merely
    // share an index. Merging would edit a sequence the upstream requires to be
    // replayed unmodified, so both survive with their own signature.
    let capture = capture(&json!([
        {"type":"reasoning.text","text":"one","index":0,"signature":"sig_a"},
        {"type":"reasoning.text","text":"two","index":0,"signature":"sig_b"}
    ]));
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(
        entries,
        json!([
            {"type":"reasoning.text","text":"one","index":0,"signature":"sig_a"},
            {"type":"reasoning.text","text":"two","index":0,"signature":"sig_b"}
        ])
    );
    // Display still concatenates the textual payloads in order.
    assert_eq!(capture_display(&artifact), "onetwo");
}

#[test]
fn a_conflicting_identity_field_keeps_two_blocks_discrete() {
    // `id` and `format` identify a block just as `signature` does, so a stated
    // difference in either refuses the merge.
    for conflicting in [
        json!({"id":"reasoning-1"}),
        json!({"format":"google-gemini-v1"}),
    ] {
        let mut second = json!({"type":"reasoning.summary","summary":"two","index":0});
        let mut first = json!({"type":"reasoning.summary","summary":"one","index":0});
        for (name, value) in conflicting.as_object().expect("object") {
            first[name] = value.clone();
            second[name] = Value::String("something-else".to_string());
        }
        let capture = capture(&json!([first, second]));
        let artifact = capture.finish().expect("artifact");
        let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
            panic!("expected ChatReasoning");
        };
        let entries: Value = serde_json::from_slice(bytes).expect("json");
        assert_eq!(
            entries.as_array().map(Vec::len),
            Some(2),
            "a differing {conflicting} must keep both blocks: {entries}"
        );
    }

    // A value only ONE entry states is not a conflict: it merges and the
    // accumulated block keeps it (the later delta did not contradict anything).
    let capture = capture(&json!([
        {"type":"reasoning.summary","summary":"one","index":0},
        {"type":"reasoning.summary","summary":"two","index":0,"format":"anthropic-claude-v1"}
    ]));
    let artifact = capture.finish().expect("artifact");
    let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
        panic!("expected ChatReasoning");
    };
    let entries: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(
        entries,
        json!([{"type":"reasoning.summary","summary":"onetwo","index":0,"format":"anthropic-claude-v1"}])
    );
}

#[test]
fn the_array_is_the_only_reasoning_shape_a_details_route_emits() {
    // A lowered array is replayed as itself in both dialects: no scalar mirror
    // is invented beside it, because no upstream documents a message carrying
    // both and a strict gateway may reject the pair.
    let bare = details_wire_value(
        br#"[{"type":"summary","summary":"a","index":0},{"type":"summary","summary":"b","index":0}]"#,
    );
    assert_eq!(
        bare,
        json!([
            {"type":"summary","summary":"a"},
            {"type":"summary","summary":"b"}
        ])
    );
    let namespaced =
        details_wire_value(br#"[{"type":"reasoning.summary","summary":"a","index":0}]"#);
    assert_eq!(
        namespaced,
        json!([{"type":"reasoning.summary","summary":"a","index":0}])
    );
}

#[test]
fn oversized_payload_is_dropped_from_the_replay_payload() {
    let big = "x".repeat(MAX_REASONING_BYTES + 1);
    let (store, text, produced) =
        capture_all(&json!([{"type":"reasoning.text","text":big,"index":0}]));
    // The replay payload goes (the whole array — a truncated sequence is not
    // something the upstreams accept), but the turn still produced output and
    // still keeps what the model said.
    assert!(store.finish().is_none());
    assert!(produced, "an over-cap entry is still model output");
    assert_eq!(text.len(), MAX_REASONING_BYTES + 1);
}

#[test]
fn display_text_survives_the_replay_cap() {
    // The cap bounds what may be persisted and replayed, not what the session
    // shows: the display text keeps every fragment, including the ones the store
    // refused after the cap tripped.
    let head = "y".repeat(MAX_REASONING_BYTES);
    let (store, text, produced) = capture_all(&json!([
        {"type":"reasoning.text","text":head,"index":0},
        {"type":"reasoning.text","text":"tail","index":0}
    ]));
    assert!(store.finish().is_none(), "the payload is over the cap");
    assert_eq!(text.len(), MAX_REASONING_BYTES + 4);
    assert!(
        text.ends_with("tail"),
        "the post-cap fragment still displays"
    );
    assert!(produced);
}

#[test]
fn oversized_persisted_artifact_replays_empty() {
    let mut bytes = Vec::from(br#"[{"type":"reasoning.text","text":""#);
    bytes.extend(std::iter::repeat_n(b'x', MAX_REASONING_BYTES));
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
