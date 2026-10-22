use super::*;

/// Parse a dialect-shaped delta from JSON, so a test states only the wire
/// fields it cares about.
fn delta(json: &str) -> StreamDelta {
    serde_json::from_str(json).expect("delta")
}

/// Fold one delta, returning the display events it emitted.
fn fold(state: &mut ReasoningStreamState, delta: &StreamDelta) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    state
        .apply(delta, &mut |event| {
            events.push(event);
            Ok(())
        })
        .expect("fold");
    events
}

fn reasoning_events(events: &[StreamEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Reasoning(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn details_claim_the_display_then_scalar_fragments_stay_quiet() {
    let mut state = ReasoningStreamState::default();
    let events = fold(
        &mut state,
        &delta(
            r#"{"reasoning":"think ","reasoning_details":[{"type":"reasoning.text","text":"think ","index":0}]}"#,
        ),
    );
    // The structured entry and the scalar field carry the same fragment; only
    // the channel that claimed the display emits it.
    assert_eq!(reasoning_events(&events), vec!["think "]);

    // A later chunk carrying both channels, and one carrying only the scalar
    // field: the details channel keeps the display, so the scalar fragments
    // emit nothing while still being accumulated for the fallback.
    let events = fold(
        &mut state,
        &delta(
            r#"{"reasoning":"more","reasoning_details":[{"type":"reasoning.text","text":"more","index":0}]}"#,
        ),
    );
    assert_eq!(reasoning_events(&events), vec!["more"]);
    let events = fold(&mut state, &delta(r#"{"reasoning":" again"}"#));
    assert_eq!(reasoning_events(&events), Vec::<&str>::new());

    let TurnReasoning {
        display, artifact, ..
    } = state.finish();
    // The scalar text is the persisted display text (it outranks the
    // details-derived text) and the details array is the artifact.
    assert_eq!(display.as_deref(), Some("think more again"));
    let Some(ReasoningArtifact::ChatReasoning { field, bytes }) = artifact else {
        panic!("expected a chat reasoning artifact");
    };
    assert_eq!(field, ChatReasoningField::ReasoningDetails);
    let entries: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        entries,
        serde_json::json!([{"type":"reasoning.text","text":"think more","index":0}])
    );
}

#[test]
fn scalar_claiming_early_suppresses_later_details_fragments() {
    // A route may stream the scalar field first and the structured array
    // afterwards. The scalar fragments are the display, the later details
    // fragments emit nothing, and the artifact is still the details payload.
    let mut state = ReasoningStreamState::default();
    let events = fold(&mut state, &delta(r#"{"reasoning_content":"early "}"#));
    assert_eq!(reasoning_events(&events), vec!["early "]);

    let events = fold(
        &mut state,
        &delta(r#"{"reasoning_details":[{"type":"reasoning.text","text":"late","index":0}]}"#),
    );
    assert_eq!(
        reasoning_events(&events),
        Vec::<&str>::new(),
        "the scalar channel already owns the display"
    );

    let TurnReasoning {
        display, artifact, ..
    } = state.finish();
    assert_eq!(display.as_deref(), Some("early "));
    let Some(ReasoningArtifact::ChatReasoning { field, .. }) = artifact else {
        panic!("expected a chat reasoning artifact");
    };
    assert_eq!(field, ChatReasoningField::ReasoningDetails);
}

#[test]
fn duplicate_scalar_fields_emit_once_from_the_first_field() {
    // chutes.ai and llama.cpp populate two scalar fields with the same text in
    // one delta; precedence order picks `reasoning_content` and the other
    // field contributes nothing.
    let mut state = ReasoningStreamState::default();
    let events = fold(
        &mut state,
        &delta(r#"{"reasoning":"dup","reasoning_content":"dup"}"#),
    );
    assert_eq!(reasoning_events(&events), vec!["dup"]);

    let TurnReasoning {
        display, artifact, ..
    } = state.finish();
    assert_eq!(display.as_deref(), Some("dup"));
    assert_eq!(
        artifact,
        Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: b"dup".to_vec(),
        })
    );
}

#[test]
fn a_stray_non_locked_scalar_field_is_neither_displayed_nor_captured() {
    let mut state = ReasoningStreamState::default();
    fold(&mut state, &delta(r#"{"reasoning_text":"locked"}"#));
    let events = fold(&mut state, &delta(r#"{"reasoning_content":"stray"}"#));
    assert_eq!(reasoning_events(&events), Vec::<&str>::new());

    let TurnReasoning {
        display, artifact, ..
    } = state.finish();
    assert_eq!(display.as_deref(), Some("locked"));
    assert_eq!(
        artifact,
        Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningText,
            bytes: b"locked".to_vec(),
        })
    );
}

#[test]
fn the_reported_output_flag_tracks_either_channel() {
    let mut state = ReasoningStreamState::default();
    let mut sink = |_: StreamEvent| Ok(());
    // An ordinary answer-only delta carries no reasoning.
    assert!(
        !state
            .apply(&delta(r#"{"content":"answer"}"#), &mut sink)
            .expect("fold")
    );
    // A scalar delta and an encrypted-only details delta both count as output.
    assert!(
        state
            .apply(&delta(r#"{"reasoning":"think"}"#), &mut sink)
            .expect("fold")
    );
    assert!(
        state
            .apply(
                &delta(r#"{"reasoning_details":[{"type":"reasoning.encrypted","data":"opaque"}]}"#),
                &mut sink,
            )
            .expect("fold")
    );
}

#[test]
fn finish_reports_scalar_only() {
    let mut state = ReasoningStreamState::default();
    fold(&mut state, &delta(r#"{"reasoning":"only scalar"}"#));
    let TurnReasoning {
        display, artifact, ..
    } = state.finish();
    assert_eq!(display.as_deref(), Some("only scalar"));
    assert_eq!(
        artifact,
        Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::Reasoning,
            bytes: b"only scalar".to_vec(),
        })
    );
}

#[test]
fn finish_reports_details_only() {
    let mut state = ReasoningStreamState::default();
    fold(
        &mut state,
        &delta(r#"{"reasoning_details":[{"type":"reasoning.text","text":"think","index":0}]}"#),
    );
    let TurnReasoning {
        display, artifact, ..
    } = state.finish();
    // No scalar field streamed, so the display is derived from the entries.
    assert_eq!(display.as_deref(), Some("think"));
    let Some(ReasoningArtifact::ChatReasoning { field, .. }) = artifact else {
        panic!("expected a chat reasoning artifact");
    };
    assert_eq!(field, ChatReasoningField::ReasoningDetails);
}

#[test]
fn finish_with_nothing_captured_is_empty() {
    let mut state = ReasoningStreamState::default();
    fold(&mut state, &delta(r#"{"content":"answer"}"#));
    let finished = state.finish();
    assert_eq!(finished.display, None);
    assert_eq!(finished.artifact, None);
}

#[test]
fn finish_keeps_the_display_of_an_overflowed_details_capture() {
    // The payload over the per-turn cap is dropped wholesale (no artifact, so
    // nothing to replay), but the turn keeps what the model said and still
    // counts as output — the cap governs replay, not the transcript or status.
    let big = "x".repeat(super::super::super::reasoning_details::MAX_REASONING_BYTES + 1);
    let mut state = ReasoningStreamState::default();
    let raw = format!(r#"{{"reasoning_details":[{{"type":"reasoning.text","text":"{big}"}}]}}"#);
    fold(&mut state, &delta(&raw));
    let finished = state.finish();
    assert_eq!(finished.artifact, None, "nothing to replay past the cap");
    assert_eq!(
        finished.display.as_deref().map(str::len),
        Some(big.len()),
        "the turn still displays the reasoning it received"
    );
    assert!(
        finished.produced,
        "an over-cap payload is still model output"
    );
}
