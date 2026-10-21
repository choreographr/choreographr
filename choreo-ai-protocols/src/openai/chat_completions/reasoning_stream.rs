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

use super::super::reasoning_details::DetailsStore;
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

/// What one turn's reasoning amounts to, after the whole turn is folded.
///
/// Three independent answers, which is the point: `produced` describes the
/// provider's response, while `display` and `artifact` describe what this client
/// chose to keep from it. A turn over the replay cap has `produced: true` with no
/// artifact, and must not be mistaken for an empty response.
#[derive(Debug, Default)]
pub(crate) struct TurnReasoning {
    /// Whether the provider produced any reasoning output this turn, whether or
    /// not any of it could be displayed or replayed.
    pub(crate) produced: bool,
    /// The turn's display text, when it has any.
    pub(crate) display: Option<String>,
    /// The opaque round-trip payload, when one was captured and fits the caps.
    pub(crate) artifact: Option<ReasoningArtifact>,
}

/// One turn's reasoning state: the live display owner, both channels' display
/// text, the replay payload, and whether the model produced reasoning at all.
///
/// Those are four separate answers and stay separate here. The display text and
/// the "produced output" fact are accumulated from the deltas as they arrive;
/// only the replay payload
/// ([`DetailsStore`]) is bounded by the per-turn cap. A payload the cap refuses
/// therefore costs the turn its echo, never its transcript or its status.
///
/// Both streaming request paths own one of these for the whole turn; the plain
/// prompt path drops [`finish`](Self::finish) (it persists no artifact) while the
/// tool path turns it into the round-trip artifact and the persisted display.
#[derive(Debug, Default)]
pub(crate) struct ReasoningStreamState {
    /// The structured `reasoning_details` replay payload for this turn. Richest
    /// payload the chat wire can replay; when it holds entries it becomes the
    /// artifact.
    details: DetailsStore,
    /// Every displayable structured fragment, in arrival order — accumulated
    /// here rather than re-derived from the stored entries, so the replay cap
    /// can never blank the session's reasoning.
    details_text: String,
    /// Whether the provider produced any reasoning output this turn, whether or
    /// not it could be displayed or replayed (an encrypted-only entry, an entry
    /// the replay cap refused, or a stray scalar delta). The one place the fact
    /// is decided: [`apply`](Self::apply) sets it and [`finish`](Self::finish)
    /// reports it, so a storage decision can never be mistaken for an empty
    /// response.
    produced: bool,
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
        // considered. Any entry at all (even an encrypted-only one, or one the
        // replay cap refuses) means the model produced output, so the turn is
        // never reported as empty.
        if let Some(raw) = delta.reasoning_details.as_ref() {
            // `DetailsStore::push` reports fragments through an infallible
            // callback because the store has nothing to do with display, so an
            // `on_event` failure cannot propagate from inside it: the first error
            // is parked and re-raised below, after the delta is folded, so a
            // failed emit never loses what the turn accumulated.
            let mut emit_error: Option<io::Error> = None;
            // Split the borrows: the store appends the turn's display text, the
            // display-owner decides live emission, and neither needs the other
            // field of `self`.
            let ReasoningStreamState {
                details,
                details_text,
                display,
                produced: turn_produced,
                ..
            } = self;
            let saw_entry = details.push(raw, details_text, &mut |fragment| {
                // A fragment reaches the live display only while the details
                // channel owns (or is first to claim) it; once the scalar field
                // owns the display, the array carries the same text that is
                // already on screen.
                if !display.claim(DisplaySource::Details) || emit_error.is_some() {
                    return;
                }
                if let Err(error) = on_event(StreamEvent::Reasoning(fragment.to_owned())) {
                    emit_error = Some(error);
                }
            });
            // Both answers: the turn-wide fact, and this delta's contribution to
            // the caller's own "did anything happen" flag.
            *turn_produced |= saw_entry;
            produced |= saw_entry;
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
            self.produced = true;
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

    /// Finalize the turn into its display text, round-trip artifact, and the
    /// fact that it produced reasoning at all.
    ///
    /// The captured details outrank the scalar payload as the artifact (the array
    /// is the richer, replayable form) and the scalar text outranks the
    /// details-derived display text; either may be absent. `produced` is
    /// independent of both: it reports what the provider sent, so a turn whose
    /// payload was over the replay cap still counts as output.
    pub(crate) fn finish(self) -> TurnReasoning {
        let ReasoningStreamState {
            details,
            details_text,
            produced,
            scalar_field,
            scalar_text,
            ..
        } = self;
        let artifact = details.finish().or_else(|| {
            scalar_field.and_then(|field| chat_reasoning_artifact(field, &scalar_text))
        });
        // The scalar channel wins the display when it streamed anything (it is
        // what the client already rendered live); otherwise the structured
        // fragments stand in.
        let display = if scalar_text.is_empty() {
            details_text
        } else {
            scalar_text
        };
        TurnReasoning {
            produced,
            display: (!display.is_empty()).then_some(display),
            artifact,
        }
    }
}

#[cfg(test)]
mod tests;
