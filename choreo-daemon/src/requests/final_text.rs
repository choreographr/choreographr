//! The agent loop's final-answer arm: the `FinalText` turn a request normally
//! ends on, and the mixed reply it is recovered from.
//!
//! The helper records the answer — its finish-reason truncation marker included
//! — keeps the model's prose when tool calls were dropped alongside it, and
//! routes that mixed case into the shared turn-recovery unit instead of letting
//! a half-finished reply read as a completed task. It owns the turn counter for
//! its arm: every exit that lets the loop continue has already advanced the
//! counter, so the agent loop's tail increment serves only the error-path
//! recovery.

use std::io;

use choreo_ai_protocols::FinalTextResult;
use choreo_proto::ReasoningProducer;
use tracing::{debug, warn};

use crate::reasoning::chains_via_response_id;
use crate::sessions::{AssistantResponse, RequestContext, SessionState};

use super::recovery::{
    RecoveryBudget, RecoveryOutcome, TRUNCATION_RECOVERY, TurnRecoveryParams, discarded_names,
    recover_unrunnable_turn,
};
use super::{accumulate_token_usage, broadcast_token_usage, finalize_and_broadcast_turn};

/// Everything [`handle_final_text_turn`] needs, bundled so the routine takes one
/// value rather than eleven positional arguments.
pub(crate) struct FinalTextTurnParams<'a> {
    /// The provider's completed answer, moved in whole: each path below takes
    /// the parts it owns (`usage` is recorded before the recovery branch, the
    /// prose and reasoning go to whichever record is written, and the dropped
    /// calls only steer the branch).
    pub final_text: FinalTextResult,
    /// The session whose transcript the answer is recorded on.
    pub session: &'a mut SessionState,
    /// Request context (command channel + session id) for the broadcasts.
    pub ctx: &'a RequestContext,
    /// The turn the answer belongs to.
    pub current_turn_id: u32,
    /// The request's stream id, carried onto the recovery log line.
    pub stream_id: u64,
    /// This request's turn counter. The helper advances it on a continuing exit
    /// (see [`FinalTextTurnOutcome::Continue`]).
    pub turn_iter: &'a mut u32,
    /// The account's provider slug: reasoning provenance on the record, the
    /// discard metric's provider dimension, and the passback-policy lookup.
    pub provider_slug: &'a str,
    /// The model that produced the answer, for the same three purposes.
    pub model: &'a str,
    /// The consecutive-recovery budget a recovered mixed reply draws down.
    pub recovery_budget: &'a mut RecoveryBudget,
    /// The in-request response-id chain a recovery drops.
    pub prev_resp_id: &'a mut Option<String>,
    /// The next turn's user text, set by a recovery.
    pub pending_user_text: &'a mut Option<String>,
}

/// What the agent loop does after a final-text turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalTextTurnOutcome {
    /// The turn lost its tool calls to truncation and was recovered: the loop
    /// continues, and the turn counter is already advanced for it.
    Continue,
    /// The request ends here — this turn is its answer.
    End,
}

/// Record a final-text turn — the answer, its truncation marker, and its
/// reasoning provenance — and report whether the request ends on it.
///
/// The one non-obvious exit: a final answer that still lost tool calls to
/// truncation is recovered rather than accepted as the end of the request, so
/// the helper reports [`FinalTextTurnOutcome::Continue`] and has already
/// advanced the turn counter for the next iteration.
pub(crate) fn handle_final_text_turn(
    params: FinalTextTurnParams<'_>,
) -> io::Result<FinalTextTurnOutcome> {
    let FinalTextTurnParams {
        final_text,
        session,
        ctx,
        current_turn_id,
        stream_id,
        turn_iter,
        provider_slug,
        model,
        recovery_budget,
        prev_resp_id,
        pending_user_text,
    } = params;

    debug!(
        session_id = ctx.session_id,
        turn = *turn_iter,
        response_len = final_text.content.len(),
        // Length only: the reasoning (and the answer it may carry)
        // is model content and must never be logged.
        reasoning_len = final_text.reasoning.as_deref().map_or(0, str::len),
        "model returned final text",
    );
    // Taken apart up front so every path below moves its own part
    // (`usage` is recorded before the recovery branch, the prose and
    // reasoning go to whichever record is written, and the dropped
    // calls only steer the branch).
    let choreo_ai_protocols::FinalTextResult {
        content,
        finish,
        discarded_tool_calls,
        reasoning,
        usage,
        response_id,
        reasoning_artifact,
    } = final_text;
    let token_usage = usage;
    accumulate_token_usage(
        session,
        token_usage.as_ref(),
        *turn_iter,
        provider_slug,
        model,
        ctx,
    );
    broadcast_token_usage(ctx, session);
    // A final answer can still be carrying a casualty. A reply whose
    // tool calls were all cut off mid-arguments but that also said
    // something is surfaced as text — the answer is kept, not thrown
    // away — yet the calls it asked for never ran. Reading that as a
    // finished answer ends the request mid-task with no notice, so
    // such a turn is recovered exactly like a text-less truncated
    // one: the prose is recorded with a note naming what was lost,
    // the model is told how to retry smaller, and the loop continues
    // under the same consecutive-recovery budget.
    if !discarded_tool_calls.is_empty() {
        let producer = ReasoningProducer {
            provider_slug: provider_slug.to_string(),
            model: model.to_string(),
        };
        // The provider slug and model are named here because the
        // metric needs them and the recovery unit does not carry
        // them; only bounded labels are recorded, never a tool name
        // or any message content.
        crate::metrics::record_tool_call_discarded(
            provider_slug,
            model,
            TRUNCATION_RECOVERY.label,
            finish,
            discarded_tool_calls.len(),
        );
        let outcome = recover_unrunnable_turn(TurnRecoveryParams {
            session,
            ctx,
            turn_id: current_turn_id,
            stream_id,
            cause: TRUNCATION_RECOVERY,
            detail: &discarded_names(&discarded_tool_calls),
            finish,
            budget: recovery_budget,
            // The model's own words are kept: the casualty is the
            // tool call, not the answer.
            prose: Some(content),
            reasoning,
            reasoning_artifact,
            producer: Some(producer),
            prev_resp_id,
            pending_user_text,
        })?;
        if outcome == RecoveryOutcome::Stop {
            return Ok(FinalTextTurnOutcome::End);
        }
        // The helper owns this arm's turn counter: a `continue` in the agent
        // loop skips its tail increment, so the counter advances here for the
        // iteration the recovery just seeded.
        *turn_iter += 1;
        return Ok(FinalTextTurnOutcome::Continue);
    }
    // Cheap display hook for the finish-reason truncation flag:
    // `length` on a final-text turn means the answer was cut off
    // by the output limit. Append a visible notice so the user is
    // not left reading a half-answer that looks complete. It goes
    // into the persisted assistant text, so the next request also
    // "sees" the truncation marker — acceptable, since resuming
    // mid-answer would otherwise be indistinguishable from a
    // finished reply.
    let mut assistant_text = content;
    if finish == choreo_ai_protocols::FinishOutcome::Length {
        warn!(
            session_id = ctx.session_id,
            turn = *turn_iter,
            "provider cut the final answer at the output length limit"
        );
        assistant_text.push_str("\n\n⚠ response truncated (length limit)");
    } else if finish == choreo_ai_protocols::FinishOutcome::Other {
        // The provider said the reply ended early without a reason
        // this client can name. Mark it rather than letting a
        // half-answer read as complete — but do not claim a length
        // limit the provider never reported.
        warn!(
            session_id = ctx.session_id,
            turn = *turn_iter,
            "provider ended the final answer early without a reason"
        );
        assistant_text.push_str("\n\n⚠ response ended early (provider stopped)");
    }
    // Write the reasoning artifact + producing model through to the
    // turn (phase 4c): the builder re-emits it on the next request
    // when the same model is still active and the passback policy
    // asks for it.
    // Record the producing provider+model once: it feeds both the
    // turn's provenance and the persisted response-id provenance.
    let producer = ReasoningProducer {
        provider_slug: provider_slug.to_string(),
        model: model.to_string(),
    };
    session.set_assistant_response(
        current_turn_id,
        AssistantResponse {
            text: Some(assistant_text),
            reasoning,
            token_usage,
            reasoning_artifact,
            reasoning_producer: Some(producer.clone()),
            ..Default::default()
        },
    );
    // Persist the response id + its producing model so a
    // ResponseId-policy provider can chain the next user turn via
    // previous_response_id (restored at the top of the next loop
    // invocation only when the same provider+model is still
    // active — the id is service-bound and must not be replayed
    // into a different provider).
    //
    // Only a route that actually chains needs this. A provider
    // whose reasoning passback is not ResponseId has no
    // `previous_response_id` continuity at all (the stateless
    // Codex provider replays captured reasoning items instead), so
    // an id it hands back is a value the next request must ignore:
    // storing it would only leave a stale chain on disk for a
    // later provenance match to replay into a service that does
    // not understand it.
    if chains_via_response_id(provider_slug, model) {
        session.config.last_response_id.clone_from(&response_id);
        session.config.last_response_id_producer = Some(producer);
    }
    finalize_and_broadcast_turn(session, ctx, current_turn_id)?;
    Ok(FinalTextTurnOutcome::End)
}
