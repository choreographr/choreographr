//! The cache-warming runtime: a per-request thread that owns a clone of the
//! session's [`InferenceProvider`] and, while a long tool call blocks, re-sends
//! the last request with a capped output to refresh the provider's prompt-cache
//! TTL. Everything decision-shaped lives in the pure state machine in
//! [`super::policy`]; this module only drives it, pings, and reports metrics.

use std::thread::JoinHandle;
use std::time::Instant;

use choreo_ai_protocols::openai::{ChatRequestMessage, ChatToolDefinition, RetryCallback};
use choreo_ai_protocols::{ChatTurnRequest, ChatTurnResult};
use crossbeam_channel::{Receiver, Sender};
use tracing::{debug, trace, warn};

use super::policy::{Action, WarmFacts, WarmPlan, WarmPolicy};
use crate::providers::InferenceProvider;

/// The exact request a warm ping replays: the just-sent turn's wire prefix,
/// the routing identity, and the facts the policy evaluated.
///
/// Owned (not borrowed) because the warmer runs on its own thread while the
/// agent loop rebuilds the next iteration's payload — see the O(conversation)
/// clone note at the arm site in `run_agent_loop`.
pub struct WarmRequest {
    /// The model slug of the just-sent request.
    pub model: String,
    /// The full message prefix sent to the provider on the just-sent turn.
    pub messages: Vec<ChatRequestMessage>,
    /// The tool definitions sent on the just-sent turn.
    pub tools: Vec<ChatToolDefinition>,
    /// The facts the (pure) policy is armed against.
    pub facts: WarmFacts,
    /// Gateway routing identity (opencode sticky routing) the real turn used.
    pub session_id: String,
    /// The per-turn request id the real turn used.
    pub stream_id: String,
    /// The per-call output cap the ping sends. Anthropic supports `max_tokens:
    /// 0` — the documented cache pre-warm, which writes the cache and bills no
    /// output tokens; every other protocol needs at least one output token, so
    /// the arm site resolves this per protocol.
    pub max_output_tokens: u32,
}

/// A message from the agent loop to the warmer thread.
enum Command {
    /// Arm (or re-arm) the plan from a fresh tool-loop iteration's request.
    Arm(Box<WarmRequest>),
    /// Stop the thread; the request is ending.
    Stop,
}

/// Handle to a running warmer thread.
///
/// This is an RAII guard: [`WarmHandle::stop`] (and therefore `Drop`) sends
/// [`Command::Stop`] and joins, so holding it in [`crate::requests::run_agent_loop`]
/// guarantees the thread is joined on **every** exit path — final text, cancel,
/// error, and panic (the drop runs during unwinding, before
/// `run_request_worker`'s `catch_unwind` catches).
pub struct WarmHandle {
    tx: Sender<Command>,
    join: Option<JoinHandle<()>>,
}

impl WarmHandle {
    /// Arm the warmer with the current iteration's request snapshot.
    ///
    /// A send failure means the thread already exited (only possible after a
    /// `Stop`), in which case there is nothing to warm and the arm is dropped.
    pub fn arm(&self, request: WarmRequest) {
        if self.tx.send(Command::Arm(Box::new(request))).is_err() {
            trace!("cache warmer thread already stopped; dropping arm");
        }
    }

    /// Stop and join the thread. Idempotent.
    pub fn stop(&mut self) {
        // A closed channel (the thread already exited) is fine.
        let _ = self.tx.send(Command::Stop);
        if let Some(join) = self.join.take() {
            // A panic in the driver is swallowed: the ping is best-effort and
            // must never affect the active run. Log it so it is not invisible.
            if join.join().is_err() {
                warn!("cache warmer thread panicked; ignoring (best-effort ping)");
            }
        }
    }
}

impl Drop for WarmHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawn the cache-warming thread for one request.
///
/// The caller owns the returned handle and must drop it before the request
/// ends (`Drop` stops and joins). The agent loop skips this spawn entirely when
/// `policy.mode == Off` (the default), so the off path has zero overhead.
///
/// `cancel_rx` is a channel the caller relays this request's cancellation
/// signal onto (NOT the loop's own `cancel_rx`: crossbeam channels deliver each
/// message to exactly one receiver, so sharing the loop's receiver would let
/// the warmer steal a cancel the loop needs to observe). When a cancel arrives
/// the warmer exits promptly; the request's end also stops it via `Drop`.
#[must_use]
pub fn spawn_warmer(
    client: InferenceProvider,
    policy: WarmPolicy,
    cancel_rx: Receiver<()>,
) -> WarmHandle {
    let (tx, rx) = crossbeam_channel::unbounded::<Command>();
    let join = std::thread::spawn(move || warmer_loop(&client, policy, &rx, &cancel_rx));
    WarmHandle {
        tx,
        join: Some(join),
    }
}

/// The state of an armed plan: the pure plan plus the request to replay.
struct Armed {
    plan: WarmPlan,
    request: WarmRequest,
}

/// What woke the driver out of its wait.
enum Wake {
    /// A fresh request snapshot to (re-)arm with.
    Arm(WarmRequest),
    /// The timer elapsed; re-evaluate the plan at the loop top.
    Timer,
}

/// The warmer thread body.
///
/// The clock is a single `Instant` captured at spawn; every decision is made
/// against `start.elapsed()`, matching the policy's injected-`now` contract.
/// The wait is fully event-driven (no sleep-poll): `select_biased!` over the
/// control receiver, the relayed cancel receiver, and — only while a plan is
/// armed with a future deadline — a `crossbeam_channel::after(wait)` timer.
/// Cancel/stop arms come first so a cancel is observed the instant it is sent.
fn warmer_loop(
    client: &InferenceProvider,
    policy: WarmPolicy,
    control_rx: &Receiver<Command>,
    cancel_rx: &Receiver<()>,
) {
    let start = Instant::now();
    let mut armed: Option<Armed> = None;

    loop {
        // (1) Evaluate the armed plan at the current clock. `decide` is Copy
        // and takes the clock by value, so the `map` ends the borrow of
        // `armed` before the arms below mutate it.
        let action = armed
            .as_ref()
            .map(|a| a.plan.decide(start.elapsed()).action);
        match action {
            Some(Action::Ping) => {
                // Take the request out (ending the borrow) so the same snapshot
                // can be re-armed after the ping, at the current clock.
                let Some(Armed { request, .. }) = armed.take() else {
                    continue;
                };
                do_ping(client, &request);
                let now = start.elapsed();
                let plan = policy.arm(request.facts, now);
                armed = Some(Armed { plan, request });
                continue;
            }
            Some(Action::Stop(reason)) => {
                // The request will not be warmed (or is no longer worth
                // warming): drop the plan and go idle. Do NOT exit the thread —
                // streaming-phase warming resumes on the next tool iteration.
                debug!(reason = %reason, "cache warming stopped for this request");
                crate::metrics::record_cache_warm_skip(reason.label());
                armed = None;
                continue;
            }
            // `None` (nothing armed) and `Some(Wait(_))` both fall through to
            // the wait below; only `Wait` schedules a timer.
            _ => {}
        }
        let wait = match action {
            Some(Action::Wait(d)) => Some(d),
            _ => None,
        };

        // (2) Wait, event-driven. Bias cancel/stop first so a stop is observed
        // the instant it is sent; the timer arm exists only while a plan is
        // armed with a future ping deadline.
        let wake = if let Some(wait) = wait {
            crossbeam_channel::select_biased! {
                recv(control_rx) -> msg => match msg {
                    Ok(Command::Arm(req)) => Wake::Arm(*req),
                    // `Stop`, or the control sender disconnecting, both end the
                    // driver: nothing can re-arm it once the owner is gone.
                    Ok(Command::Stop) | Err(_) => return,
                },
                recv(cancel_rx) -> _ => return,
                recv(crossbeam_channel::after(wait)) -> _ => Wake::Timer,
            }
        } else {
            crossbeam_channel::select_biased! {
                recv(control_rx) -> msg => match msg {
                    Ok(Command::Arm(req)) => Wake::Arm(*req),
                    Ok(Command::Stop) | Err(_) => return,
                },
                recv(cancel_rx) -> _ => return,
            }
        };
        match wake {
            // Re-evaluate at the loop top: at/after `next_warm_at` this yields
            // Ping, past the deadline Stop, otherwise a fresh Wait.
            Wake::Timer => {}
            Wake::Arm(request) => {
                let now = start.elapsed();
                let plan = policy.arm(request.facts, now);
                armed = Some(Armed { plan, request });
            }
        }
    }
}

/// Build the non-streaming ping request for `request`.
///
/// Factored out so the request shape is unit-testable without a socket. The
/// shape is deliberate: `thinking_effort: "off"` (a capped output would corrupt
/// an Anthropic `thinking.budget_tokens`, which derives from `max_tokens`),
/// `cancel_rx: None` (the ping must not consume a cancel the loop still needs —
/// the sockreg `shutdown_all` is the force-close path for a wedged ping),
/// `max_output_tokens_override: Some(request.max_output_tokens)` (touch the
/// prefix, emit at most that many tokens — 0 on Anthropic's pre-warm), and
/// `no_retry: true` (exactly one attempt).
pub(super) fn ping_request<'a>(
    request: &'a WarmRequest,
    on_retry: &'a mut Option<RetryCallback>,
) -> ChatTurnRequest<'a> {
    ChatTurnRequest {
        model: &request.model,
        messages: &request.messages,
        tools: &request.tools,
        thinking_effort: "off".to_string(),
        on_retry,
        cancel_rx: None,
        previous_response_id: None,
        tool_results: &[],
        programmatic_tool_calling: false,
        session_id: request.session_id.clone(),
        request_id: request.stream_id.clone(),
        max_output_tokens_override: Some(request.max_output_tokens),
        no_retry: true,
    }
}

/// Send one warm ping and record its outcome. Best-effort: every error is
/// swallowed (logged at `debug`/`trace`) and the result never reaches session
/// or turn state.
fn do_ping(client: &InferenceProvider, request: &WarmRequest) {
    let mut on_retry: Option<RetryCallback> = None;
    let result = client.chat_completion_turn(ping_request(request, &mut on_retry));
    // Count the attempt regardless of outcome: the attempt is what costs money.
    crate::metrics::record_cache_warm_attempt();
    match result {
        Ok(ChatTurnResult::FinalText(final_text)) => {
            // Surface the ping's usage to metrics/logs ONLY — it must never
            // enter the session's accumulated usage or a turn.
            if let Some(usage) = final_text.usage {
                trace!(
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    "cache warm ping usage"
                );
            }
        }
        Ok(_) => {
            trace!("cache warm ping returned a non-final-text result; ignoring");
        }
        Err(e) => {
            // A failed ping is expected sometimes (expired cache, transient
            // 5xx); it never affects the active run.
            debug!(error = %e, "cache warm ping failed (best-effort)");
        }
    }
}
