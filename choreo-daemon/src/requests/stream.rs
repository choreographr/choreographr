//! The forward-only streaming phase of one model call and the broadcasters that
//! report it: [`StreamPhase`] with `advance_stream_phase` (which keeps the
//! status label monotonic however a provider interleaves its deltas) and
//! `broadcast_stream_chunk` (one chunk plus the running output-token estimate).
//! Re-exported from `requests.rs` via `pub(crate) use stream::*;`.
//!
//! This is the thin "what a client sees while tokens arrive" layer the agent
//! loop calls from its streaming callback. It knows nothing about tool dispatch
//! or turn recording, so it lives apart from `run_agent_loop`, whose body stays
//! about the turn itself.
use crate::sessions::{RequestContext, SessionCommand};
use choreo_proto::{DaemonMessageType, OutputStream, SessionEvent, SessionStatus};

/// The forward-only streaming phase a single model call moves through, in
/// order: `Sending` (the request is dispatched and we are awaiting the first
/// token) → `Reasoning` (the model's reasoning tokens are streaming) →
/// `Responding` (the answer tokens are streaming).  Modeled as an ordered enum
/// so "only ever move forward" is the derived `Ord` comparison rather than a
/// pair of hand-written inverse guards that could drift apart.
///
/// The `Waiting` boundary — the request payload has been fully uploaded but
/// the first token has not arrived — is broadcast directly by the
/// request-uploaded callback rather than tracked here: it is a one-shot event
/// that fires before any delta, so it never needs to participate in the
/// forward-only comparison.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StreamPhase {
    Sending,
    Reasoning,
    Responding,
}

impl StreamPhase {
    /// The [`SessionStatus`] this phase broadcasts to clients.
    fn status(self) -> SessionStatus {
        match self {
            StreamPhase::Sending => SessionStatus::Sending,
            StreamPhase::Reasoning => SessionStatus::Reasoning,
            StreamPhase::Responding => SessionStatus::Responding,
        }
    }
}

/// Advance a turn's live streaming phase to `target`, broadcasting the new
/// status to subscribers — but only when it moves FORWARD along [`StreamPhase`].
/// A later chunk of an earlier kind therefore never demotes the phase back (a
/// trailing reasoning delta after the answer has begun does not flip
/// `Responding` back to `Reasoning`), so one model call emits at most two
/// *advances* — two `StatusChanged` broadcasts on top of the initial `Sending`
/// — no matter how the provider interleaves its deltas.  `Sending` is broadcast
/// once before the call, so it is the initial value here and this never
/// re-emits it.
pub(crate) fn advance_stream_phase(
    phase: &mut StreamPhase,
    target: StreamPhase,
    ctx: &RequestContext,
) {
    if target > *phase {
        *phase = target;
        // Best-effort: the `Sending` broadcast that began the turn already gated
        // the loop (its failure ends the turn), so a dropped mid-stream phase
        // update only means a client keeps the previous label until the next
        // one — not worth aborting the model call over.
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(target.status()));
    }
}

/// Forward one streamed model chunk to subscribers: the chunk itself on its
/// answer/reasoning stream, then the running output-token estimate so the UI's
/// live token display refreshes on every chunk.
pub(crate) fn broadcast_stream_chunk(
    ctx: &RequestContext,
    stream_id: u64,
    stream: OutputStream,
    text: String,
    output_tokens: u32,
) {
    let _ = ctx
        .cmd_tx
        .send(SessionCommand::Broadcast(DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::OutputChunk {
                stream_id,
                stream,
                data: text.into_bytes(),
            },
        }));
    let _ = ctx
        .cmd_tx
        .send(SessionCommand::Broadcast(DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::LiveOutputTokenCount {
                stream_id,
                output_tokens,
            },
        }));
}
