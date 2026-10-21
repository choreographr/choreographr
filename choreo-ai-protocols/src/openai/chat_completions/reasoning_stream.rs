//! Streaming reasoning state for the chat-completions adapter.
//!
//! A chat route can carry reasoning on two channels: the scalar fields
//! (`reasoning_content` / `reasoning` / `reasoning_text`) and the structured
//! `reasoning_details` array. Several routes populate both with the same text
//! (chutes.ai and llama.cpp among them), so a turn must render exactly one
//! channel's fragments and silently fold the other — emitting both renders the
//! reasoning twice in the client.
//!
//! The rules this state machine enforces:
//!
//! - **First claim wins the live display.** [`DisplaySource`] starts
//!   [`Unclaimed`](DisplaySource::Unclaimed) and the first channel to produce a
//!   displayable fragment claims it for the rest of the turn; from then on the
//!   other channel contributes nothing to the live display. The rule is
//!   symmetric — neither channel is privileged — because a route may stream the
//!   scalar field first and the structured array later just as easily as the
//!   reverse, and a one-way latch would then display the reasoning twice.
//! - **Details fold first.** [`apply`](ReasoningStreamState::apply) processes a
//!   delta's structured entries before its scalar fields, so a delta carrying
//!   both lets the structured entries claim the display first.
//! - **Both channels are always captured.** The scalar text accumulates even
//!   when it does not own the display (it is the display fallback for a
//!   details-only turn that yields no text, and the artifact fallback when no
//!   details were captured), and the details are captured even when the scalar
//!   field owns the display — only emission is suppressed.
//! - **The scalar field locks on the first non-empty delta**, in the
//!   non-streaming precedence `reasoning_content` > `reasoning` >
//!   `reasoning_text`, so the artifact is re-emitted to the SAME wire field the
//!   provider used: a stream that sends `reasoning_text` must not have its
//!   payload replayed as `reasoning_content` on the next tool-loop turn. A stray
//!   delta from a non-locked field is neither displayed nor captured.

use std::io;

use choreo_proto::{ChatReasoningField, ReasoningArtifact};

use super::super::reasoning_details::DetailsCapture;
use super::{StreamDelta, chat_reasoning_artifact};
use crate::types::StreamEvent;

/// Which channel owns the turn's LIVE display.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum DisplaySource {
    /// No channel has produced a displayable fragment yet.
    #[default]
    Unclaimed,
    /// The scalar reasoning fields own the display.
    Scalar,
    /// The structured `reasoning_details` entries own the display.
    Details,
}

impl DisplaySource {
    /// Claim the display for `source`, reporting whether `source` owns it.
    ///
    /// The first caller wins; every later claim from the losing channel reports
    /// `false` for the rest of the turn.
    fn claim(&mut self, source: DisplaySource) -> bool {
        if *self == DisplaySource::Unclaimed {
            *self = source;
        }
        *self == source
    }
}

/// One turn's streaming reasoning state: the live display owner plus both
/// channels' captured payloads.
///
/// Both streaming request paths own one of these for the whole turn; the plain
/// prompt path drops [`finish`](Self::finish) (it persists no artifact) while the
/// tool path turns it into the round-trip artifact and the persisted display.
#[derive(Debug, Default)]
pub(crate) struct ReasoningStreamState {
    /// The structured `reasoning_details` capture for this turn. Richest payload
    /// the chat wire can replay; when it holds entries it becomes the artifact.
    details: DetailsCapture,
    /// Which channel owns the LIVE display this turn.
    display: DisplaySource,
    /// The scalar field the stream is using, locked in on the first non-empty
    /// delta (`None` until then) — see [`ChatReasoningField`]. Locked so the
    /// artifact is re-emitted to the same wire field the provider used.
    scalar_field: Option<ChatReasoningField>,
    /// Accumulated scalar reasoning text: the display and artifact fallback for a
    /// turn that streams (or also streams) the scalar fields.
    scalar_text: String,
}

impl ReasoningStreamState {
    /// Fold one delta's reasoning fields, forwarding displayable fragments to
    /// `on_event`.
    ///
    /// Returns whether the delta carried any reasoning output, so the caller can
    /// fold it into its own "did this turn produce anything" flag.
    pub(crate) fn apply(
        &mut self,
        delta: &StreamDelta,
        on_event: &mut impl FnMut(StreamEvent) -> io::Result<()>,
    ) -> io::Result<bool> {
        let mut produced = false;

        // Structured entries fold first, so a delta carrying both channels lets
        // the structured entries claim the display before the scalar fields are
        // considered. Any recognised entry (even an encrypted-only one) means the
        // model produced output, so the turn is never reported as empty.
        if let Some(raw) = delta.reasoning_details.as_ref() {
            // `DetailsCapture::push` reports fragments through an infallible
            // callback because the capture has nothing to do with display, so an
            // `on_event` failure cannot propagate from inside it: the first error
            // is parked and re-raised below, after the delta is folded, so a
            // failed emit never loses the capture.
            let mut emit_error: Option<io::Error> = None;
            let display = &mut self.display;
            self.details.push(raw, &mut |fragment| {
                // A fragment reaches the display only while the details channel
                // owns (or is first to claim) it; once the scalar field owns the
                // display, the array carries the same text that is already on
                // screen.
                if !display.claim(DisplaySource::Details) || emit_error.is_some() {
                    return;
                }
                if let Err(error) = on_event(StreamEvent::Reasoning(fragment.to_owned())) {
                    emit_error = Some(error);
                }
            });
            if self.details.has_entries() {
                produced = true;
            }
            if let Some(error) = emit_error {
                return Err(error);
            }
        }

        for (field, text) in [
            (
                ChatReasoningField::ReasoningContent,
                &delta.reasoning_content,
            ),
            (ChatReasoningField::Reasoning, &delta.reasoning),
            (ChatReasoningField::ReasoningText, &delta.reasoning_text),
        ] {
            let Some(text) = text.as_deref().filter(|text| !text.is_empty()) else {
                continue;
            };
            produced = true;
            let chosen = *self.scalar_field.get_or_insert(field);
            if chosen != field {
                // Stray delta from a non-primary field — display and capture
                // neither, so the artifact stays field-pure.
                continue;
            }
            // The scalar text is always accumulated: it is the display fallback
            // for a details turn that yields no text and the artifact fallback
            // when no details were captured. Only the live event is withheld
            // once the details channel owns the display.
            self.scalar_text.push_str(text);
            if self.display.claim(DisplaySource::Scalar) {
                on_event(StreamEvent::Reasoning(text.to_owned()))?;
            }
        }

        Ok(produced)
    }

    /// Finalize the turn into its display text and round-trip artifact.
    ///
    /// The captured details outrank the scalar payload as the artifact (the array
    /// is the richer, replayable form) and the scalar text outranks the
    /// details-derived display text; either may be absent.
    pub(crate) fn finish(self) -> (Option<String>, Option<ReasoningArtifact>) {
        let details_display = self.details.display();
        let artifact = self.details.finish().or_else(|| {
            self.scalar_field
                .and_then(|field| chat_reasoning_artifact(field, &self.scalar_text))
        });
        let display = if self.scalar_text.is_empty() {
            details_display
        } else {
            self.scalar_text
        };
        ((!display.is_empty()).then_some(display), artifact)
    }
}

#[cfg(test)]
mod tests {
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

        let (display, artifact) = state.finish();
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
        // Regression: a route may stream the scalar field first and the structured
        // array afterwards. The scalar fragments are the display, the later
        // details fragments emit nothing, and the artifact is still the details
        // payload.
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

        let (display, artifact) = state.finish();
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

        let (display, artifact) = state.finish();
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

        let (display, artifact) = state.finish();
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
                    &delta(
                        r#"{"reasoning_details":[{"type":"reasoning.encrypted","data":"opaque"}]}"#
                    ),
                    &mut sink,
                )
                .expect("fold")
        );
    }

    #[test]
    fn finish_reports_scalar_only() {
        let mut state = ReasoningStreamState::default();
        fold(&mut state, &delta(r#"{"reasoning":"only scalar"}"#));
        let (display, artifact) = state.finish();
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
        let (display, artifact) = state.finish();
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
        assert_eq!(state.finish(), (None, None));
    }

    #[test]
    fn finish_drops_an_overflowed_details_capture() {
        // The payload over the per-turn cap is dropped wholesale, so the turn has
        // no display text and no artifact to replay.
        let big =
            "x".repeat(super::super::super::reasoning_details::MAX_REASONING_DETAILS_BYTES + 1);
        let mut state = ReasoningStreamState::default();
        let raw =
            format!(r#"{{"reasoning_details":[{{"type":"reasoning.text","text":"{big}"}}]}}"#);
        fold(&mut state, &delta(&raw));
        assert_eq!(state.finish(), (None, None));
    }
}
