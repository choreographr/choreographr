//! The per-request worker that drives the agent loop for one run, plus the
//! session-thread exit persistence helper.

use super::{
    ChildResult, DaemonCommand, DaemonMessageType, InferenceProvider, RequestContext,
    SessionCommand, SessionEvent, SessionRecord, SessionState, debug, error, info, io, mpsc,
    run_agent_loop, write_session_retry,
};

/// Inputs for [`run_request_worker`], grouped so the worker entry point takes
/// one value instead of eight positional arguments (and the
/// `too_many_arguments` lint needs no suppression).
pub(super) struct RequestWorkerArgs<'a> {
    pub(super) stream_id: u64,
    // Borrowed only: `run_agent_loop` also takes the client by reference;
    // the worker thread outlives the call via its own clones of `ctx` and
    // `model` at the spawn site.
    pub(super) client: &'a InferenceProvider,
    pub(super) session: &'a mut SessionState,
    pub(super) model: &'a str,
    pub(super) cancel_rx: &'a crossbeam_channel::Receiver<()>,
    pub(super) ctx: &'a RequestContext,
    pub(super) child_reply: Option<&'a mpsc::Sender<io::Result<ChildResult>>>,
    pub(super) user_text: Option<&'a str>,
}

pub(super) fn run_request_worker(args: RequestWorkerArgs<'_>) {
    let RequestWorkerArgs {
        stream_id,
        client,
        session,
        model,
        cancel_rx,
        ctx,
        child_reply,
        user_text,
    } = args;
    // No error path: every failure mode is folded into a RequestOutcome and
    // routed back through the command channel, so the function returns ()
    // instead of a transparent Ok wrapper.
    let request_start = std::time::Instant::now();
    let initial_snapshot = session.snapshot();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_agent_loop(client, session, model, stream_id, cancel_rx, ctx, user_text)
    }));

    let (outcome, snapshot) = match result {
        Ok(Ok(true)) => (RequestOutcome::Cancelled, session.snapshot()),
        Ok(Ok(false)) => (RequestOutcome::Done, session.snapshot()),
        Ok(Err(e)) => (RequestOutcome::Failed(e), session.snapshot()),
        Err(_) => (
            RequestOutcome::Failed(io::Error::other("request worker panicked")),
            initial_snapshot,
        ),
    };

    let req_status = match &outcome {
        RequestOutcome::Done => "done",
        RequestOutcome::Failed(_) => "failed",
        RequestOutcome::Cancelled => "cancelled",
    };
    crate::metrics::record_request_total(req_status);
    crate::metrics::record_request_duration(req_status, request_start.elapsed().as_secs_f64());

    match &outcome {
        RequestOutcome::Done => {
            info!(session_id = ctx.session_id, stream_id, "request completed");
            // Route through the main session thread so detach is respected.
            // Include the worker's accumulated token usage so subscribers
            // (e.g. the TUI) can show per-request token counts.
            let usage = &session.config.accumulated_usage;
            debug!(
                session_id = ctx.session_id,
                stream_id,
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                total_tokens = usage.total_tokens,
                "broadcasting Done with accumulated token usage"
            );
            let _ = ctx
                .cmd_tx
                .send(SessionCommand::Broadcast(DaemonMessageType::Session {
                    session_id: Some(ctx.session_id),
                    event: SessionEvent::Done {
                        stream_id,
                        token_usage: Some(*usage),
                        last_prompt_tokens: session.config.last_prompt_tokens,
                    },
                }));
        }
        RequestOutcome::Failed(error) => {
            info!(session_id = ctx.session_id, stream_id, error = %error, "request failed");
            // Route through the main session thread so detach is respected.
            let _ = ctx
                .cmd_tx
                .send(SessionCommand::Broadcast(DaemonMessageType::Session {
                    session_id: Some(ctx.session_id),
                    event: SessionEvent::Failed {
                        stream_id,
                        error: error.to_string(),
                    },
                }));
        }
        RequestOutcome::Cancelled => {
            info!(session_id = ctx.session_id, stream_id, "request cancelled");
        }
    }

    if let Some(reply) = child_reply {
        let child_result = match &outcome {
            RequestOutcome::Done => {
                let output = session
                    .turns
                    .values()
                    .filter_map(|t| t.assistant_text.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ChildResult {
                    output,
                    is_error: false,
                })
            }
            RequestOutcome::Failed(error) => Ok(ChildResult {
                output: error.to_string(),
                is_error: true,
            }),
            RequestOutcome::Cancelled => Ok(ChildResult {
                output: "request cancelled".to_string(),
                is_error: true,
            }),
        };
        let _ = reply.send(child_result);
    }

    let _ = ctx.cmd_tx.send(SessionCommand::RequestFinished {
        stream_id,
        snapshot,
    });
}

pub(super) enum RequestOutcome {
    Done,
    Failed(io::Error),
    Cancelled,
}

pub(super) fn persist_and_exit(
    state: &SessionState,
    db: &redb::Database,
    session_id: u64,
    daemon_tx: &crossbeam_channel::Sender<DaemonCommand>,
) {
    let record: SessionRecord = SessionRecord::from(state);
    if let Err(e) = write_session_retry(db, session_id, &record) {
        error!(
            "persist_and_exit: failed to persist session {}: {e}",
            session_id
        );
    }
    let _ = daemon_tx.send(DaemonCommand::SessionExited { session_id });
}
