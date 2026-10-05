//! The dispatcher loop: serves one server's commands and owns the in-flight
//! call registry.
//!
//! The loop drains completion notices, reaps parked calls whose whole-call
//! deadline has passed, starts queued calls into any freed slots, then blocks on
//! the next command, completion, reconnect result, or queued-call deadline. All
//! mutable state (the engine, the reconnector, the in-flight map, the queued
//! deque, and the admission [`CallGate`]) lives on this thread — no lock. A
//! transport failure starts a reconnect on a detached worker thread (see
//! [`Reconnector`]) so this loop keeps serving commands — cancels and shutdown
//! included — throughout the backoff and the rebuild; new calls are queued, not
//! spawned against the dead engine, until the rebuild completes. A call's
//! per-call timeout bounds its WHOLE lifetime (queue wait plus execution), so a
//! call parked behind a busy server is reaped once its deadline passes and the
//! queue cannot grow without bound.

use crate::error::McpError;
use crate::session::cancel::CancelToken;
use crate::session::gate::{CallGate, QueuedCall};
use crate::session::restart::{Reconnector, RestartPolicy, is_transport_error};
use crate::session::{CallRequest, EngineCall, EngineFactory, McpCommand, McpEngine};
use crossbeam_channel::{Receiver, Sender};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

/// How a completed operation's result bears on the connection's health.
///
/// The dispatcher uses this to decide whether a notice should reset the restart
/// budget. Only an exchange that actually reached the server — or whose failure
/// is a settled answer a rebuild cannot change — proves the connection works;
/// a deadline or a client-side cancel says nothing either way.
pub(super) enum DoneOutcome {
    /// The exchange reached the server or is a settled answer (an `Ok`, or a
    /// JSON-RPC / protocol / auth-required error), so it proves the connection
    /// is alive — it resets the restart budget.
    Completed,
    /// A transport-level failure: the reconnect trigger.
    TransportFailed,
    /// A non-transport failure that does NOT prove health (a deadline or a
    /// client-side cancel).
    Inconclusive,
}

/// Classify a completed operation's result into a [`DoneOutcome`].
///
/// A transport error is checked first, so the transport variants can never be
/// mistaken for a settled `Err(_)`. [`McpError::Timeout`] and
/// [`McpError::Cancelled`] are the only non-transport failures that leave the
/// connection's health unproven: a deadline can fire while the transport is
/// perfectly usable, and a client-side cancel never reached a verdict. Every
/// other `Err` (a JSON-RPC error, a protocol error, a rejected auth) is a
/// settled answer the live connection delivered, so it counts as `Completed`.
pub(super) fn classify_outcome<T>(result: &Result<T, McpError>) -> DoneOutcome {
    match result {
        Ok(_) => DoneOutcome::Completed,
        Err(e) if is_transport_error(e) => DoneOutcome::TransportFailed,
        Err(McpError::Timeout | McpError::Cancelled) => DoneOutcome::Inconclusive,
        Err(_) => DoneOutcome::Completed,
    }
}

/// Completion notice a spawned task sends back to the dispatcher.
///
/// A tool call and a listing/resource read both report here so the dispatcher
/// can free a call's slot (when there is one) and reconnect a dead transport.
/// Listings consume no concurrency slot, so their `call_id` is `None`.
pub(super) struct Done {
    /// The in-flight call this notice retires, or `None` for a non-call
    /// operation (a listing or resource read) that holds no gate slot.
    pub(super) call_id: Option<u64>,
    /// How the operation's result bears on the connection's health; decides
    /// whether this notice resets the restart budget (see [`apply_completions`]).
    pub(super) outcome: DoneOutcome,
    /// The engine the operation ran against. Compared by identity so a notice
    /// that arrives after the engine has already been rebuilt (a straggler from
    /// an incident another notice already handled) frees its slot without
    /// affecting the current engine's budget.
    pub(super) engine: Arc<dyn McpEngine>,
}

/// The dispatcher thread body: serve commands until shutdown or disconnect.
///
/// `max_concurrent_calls` bounds how many tool calls to this server run at once;
/// calls beyond the cap wait in a queue and are started as slots free. A queued
/// call is bounded by its whole-call deadline — its per-call timeout measured
/// from when the command arrived, not from when it starts — so a call that
/// waits too long is reaped with a timeout reply rather than blocking forever
/// behind a busy server. Listings and resource reads are dispatched onto the
/// runtime like calls but consume no concurrency slot, so they are not subject
/// to the cap and cannot stall the dispatcher. `max_restarts` bounds how many
/// times a dead transport is rebuilt in a row before the server is left alone.
pub(super) fn run_dispatcher(
    initial: Arc<dyn McpEngine>,
    factory: EngineFactory,
    cmd_rx: &Receiver<McpCommand>,
    max_concurrent_calls: usize,
    max_restarts: u32,
) {
    let rt = match crate::runtime::handle() {
        Ok(handle) => handle,
        Err(e) => {
            tracing::error!(error = %e, "MCP dispatcher cannot start without a runtime");
            return;
        }
    };

    let mut engine = initial;
    let mut reconnector = Reconnector::new(factory, RestartPolicy::new(max_restarts));
    let mut inflight: HashMap<u64, (CancelToken, u64)> = HashMap::new();
    let mut queued: VecDeque<QueuedCall> = VecDeque::new();
    let mut gate = CallGate::new(max_concurrent_calls);
    let mut next_call_id: u64 = 1;

    // Completion notices from spawned call tasks; drained between commands.
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<Done>();

    // The reconnect worker's result channel, cloned out of the reconnector so
    // the `select!` can observe a finished reconnect without borrowing the
    // reconnector across an arm that mutates it (`Receiver` is `Clone`).
    let reconnect_rx = reconnector.receiver().clone();

    loop {
        // Drain completion notices before waiting: a finished call frees a slot
        // and, if it died on the transport, starts a reconnect. The whole
        // available batch is coalesced into at most ONE reconnect, so N requests
        // lost on one dead transport cost one rebuild, not N.
        apply_completions(
            None,
            &done_rx,
            &mut inflight,
            &mut gate,
            &mut reconnector,
            &engine,
        );
        // Reap parked calls whose whole-call deadline has passed BEFORE any
        // promotion: an expired call must be dropped, never started into a slot
        // that just freed. This is what bounds the queue's growth behind a
        // wedged server, whose in-flight calls can hold every slot indefinitely.
        expire_queued(&mut queued, &mut gate, Instant::now());
        // Start queued calls into any slots the completions freed — but only
        // when no reconnect is in flight, since promoting calls against a dead
        // engine would just fail them; they wait for the rebuilt engine.
        if !reconnector.is_in_flight() {
            pump_queued(
                &mut queued,
                &mut gate,
                &mut inflight,
                &rt,
                &engine,
                &done_tx,
            );
        }
        // Arm a fresh deadline timer from whatever is still queued, so the loop
        // wakes exactly when the next call expires (or never, if none is). The
        // timer is a per-iteration binding, so the `select!` arm always reads
        // the timer that matches the queue as it stands now.
        let queue_timer = arm_queue_timer(&queued);

        // Wait for the next command, completion, reconnect result, or queued-
        // call deadline. All are event sources, so no polling: a finished call
        // wakes the loop to free its slot even when no command has arrived, and
        // the timer wakes it the instant a parked call's deadline passes.
        crossbeam_channel::select! {
            recv(cmd_rx) -> msg => {
                if let Ok(cmd) = msg {
                    let keep_running = handle_command(
                        cmd,
                        &rt,
                        &engine,
                        &mut inflight,
                        &mut queued,
                        &mut gate,
                        &done_tx,
                        &mut next_call_id,
                        reconnector.is_in_flight(),
                    );
                    if !keep_running {
                        break;
                    }
                } else {
                    tracing::debug!("MCP command channel closed; dispatcher exiting");
                    break;
                }
            }
            recv(done_rx) -> msg => {
                if let Ok(done) = msg {
                    // Fold this notice in with any siblings that completed in
                    // the same incident, so the batch reconnects at most once.
                    apply_completions(
                        Some(done),
                        &done_rx,
                        &mut inflight,
                        &mut gate,
                        &mut reconnector,
                        &engine,
                    );
                }
            }
            recv(reconnect_rx) -> msg => {
                // The reconnect worker finished: adopt the rebuilt engine when
                // it succeeded, leaving the old one in place on failure so a
                // later incident retries within the budget.
                if let Ok(result) = msg
                    && let Some(fresh) = reconnector.take_result(result)
                {
                    engine = fresh;
                }
            }
            // The earliest queued call's deadline timer fired; the next loop-
            // top `expire_queued` does the reaping. This arm exists only to
            // wake the loop — no work belongs here.
            recv(queue_timer) -> _ => {}
        }
    }
    tracing::debug!("MCP dispatcher thread exiting");
}

/// Apply the completion notices currently available — `first`, when the caller
/// already received one from the `select!`, plus everything else the channel
/// holds — freeing each call's slot and reporting AT MOST ONE transport
/// failure to the reconnector.
///
/// One dead transport fails every in-flight request at once, each sending its
/// own notice; reporting each separately would charge the restart budget once
/// per request for a single incident. The batch is coalesced: the single
/// [`Reconnector::note_failure`] happens once, after the whole batch is drained
/// (and, being idempotent while a reconnect is in flight, spawns only one
/// worker), so `pump_queued` then starts queued calls against the rebuilt
/// engine once it arrives. A notice whose request ran against an engine since
/// replaced (compared by identity) only frees its slot — it is a straggler from
/// an already-handled incident, not a new one, and says nothing about the
/// current engine's health.
///
/// A notice from the CURRENT engine that genuinely COMPLETED its exchange (or
/// whose failure is a settled answer) proves the connection is usable again, so
/// it resets the restart budget (`record_success`); a notice that timed out or
/// was cancelled client-side proves nothing, so it is neither a failure nor a
/// success. The budget is deliberately not reset by the reconnect itself (only
/// a surviving request clears it), which bounds a server that reconnects and
/// immediately dies.
pub(super) fn apply_completions(
    first: Option<Done>,
    done_rx: &Receiver<Done>,
    inflight: &mut HashMap<u64, (CancelToken, u64)>,
    gate: &mut CallGate,
    reconnector: &mut Reconnector,
    engine: &Arc<dyn McpEngine>,
) {
    let mut transport_failed = false;
    for done in first.into_iter().chain(done_rx.try_iter()) {
        // A call notice retires its in-flight slot; a listing notice (`None`)
        // holds no slot and only reports transport health. This runs for EVERY
        // notice, stragglers included, so their slots are always freed.
        if let Some(call_id) = done.call_id {
            inflight.remove(&call_id);
            gate.complete();
        }
        // Only a notice from the CURRENT engine can speak to its health; a
        // straggler from an already-replaced engine is otherwise ignored.
        if !Arc::ptr_eq(&done.engine, engine) {
            continue;
        }
        match done.outcome {
            DoneOutcome::TransportFailed => transport_failed = true,
            DoneOutcome::Completed => reconnector.record_success(),
            DoneOutcome::Inconclusive => {}
        }
    }
    if transport_failed {
        reconnector.note_failure();
    }
}

/// Start queued calls while the gate has free slots.
///
/// A promoted call is registered in `inflight` exactly like an immediately
/// admitted one, so a later session cancel can still reach it.
fn pump_queued(
    queued: &mut VecDeque<QueuedCall>,
    gate: &mut CallGate,
    inflight: &mut HashMap<u64, (CancelToken, u64)>,
    rt: &tokio::runtime::Handle,
    engine: &Arc<dyn McpEngine>,
    done_tx: &Sender<Done>,
) {
    while gate.has_capacity() {
        let Some(call) = queued.pop_front() else {
            break;
        };
        gate.promote();
        inflight.insert(call.call_id, (call.cancel.clone(), call.session_id));
        spawn_call(rt, engine, done_tx, call);
    }
}

/// Reap every parked call whose whole-call deadline has already passed.
///
/// A queued call's per-call timeout bounds its ENTIRE lifetime, so once its
/// deadline is behind `now` it can never be run (a promotion would spend the
/// caller's remaining budget on a call that must fail anyway). Dropping it here
/// — with the same accounting as the `CancelSession` removal loop — is what
/// keeps the queue bounded behind a busy or wedged server: the caller gets a
/// timeout reply immediately instead of blocking forever.
fn expire_queued(queued: &mut VecDeque<QueuedCall>, gate: &mut CallGate, now: Instant) {
    let mut index = 0;
    while index < queued.len() {
        if queued[index].deadline <= now {
            if let Some(call) = queued.remove(index) {
                gate.abandon();
                call.cancel.cancel();
                let _ = call.reply.send(Err(McpError::Timeout));
            }
        } else {
            index += 1;
        }
    }
}

/// Build the timer the dispatcher `select!`s on to reap queued calls.
///
/// The earliest deadline among the parked calls decides how long the timer
/// waits — its `after(remaining)` fires exactly then, so there is no polling. An
/// empty (or already-past) queue arms a `never()` channel that never fires, so
/// the loop blocks only on real events.
fn arm_queue_timer(queued: &VecDeque<QueuedCall>) -> crossbeam_channel::Receiver<Instant> {
    match queued.iter().map(|c| c.deadline).min() {
        Some(deadline) => {
            crossbeam_channel::after(deadline.saturating_duration_since(Instant::now()))
        }
        None => crossbeam_channel::never(),
    }
}

/// Spawn one call onto the sidecar runtime.
///
/// The task sends a [`Done`] notice (so the dispatcher frees the slot and can
/// reconnect a dead transport) followed by the reply; both are best-effort — a
/// dropped receiver means the caller already went away.
///
/// The call's deadline bounds its WHOLE lifetime, so a call that spent time in
/// the queue runs with only what remains. A call promoted at the last instant —
/// or run while the queue timer and a completion race — may already be past its
/// deadline: it must not touch the engine at all, so it is retired here with a
/// timeout reply (`Inconclusive`, since a deadline proves nothing about the
/// connection's health).
pub(super) fn spawn_call(
    rt: &tokio::runtime::Handle,
    engine: &Arc<dyn McpEngine>,
    done_tx: &Sender<Done>,
    call: QueuedCall,
) {
    let QueuedCall {
        call_id,
        request,
        reply,
        chunk_tx,
        cancel,
        deadline,
        ..
    } = call;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        // Too late to run: fail without spawning, but still free the slot the
        // caller's promotion took, exactly as a completed call would.
        let _ = done_tx.send(Done {
            call_id: Some(call_id),
            outcome: DoneOutcome::Inconclusive,
            engine: Arc::clone(engine),
        });
        let _ = reply.send(Err(McpError::Timeout));
        return;
    }
    // The engine arms its own per-call deadline from this timeout, so passing
    // the REMAINING time is what makes the deadline bound the whole call rather
    // than restart at spawn.
    let request = CallRequest {
        timeout: remaining,
        ..request
    };
    let engine = Arc::clone(engine);
    let done_tx = done_tx.clone();
    rt.spawn(async move {
        let result = engine
            .call_tool(EngineCall {
                request,
                cancel,
                chunk_tx,
            })
            .await;
        let outcome = classify_outcome(&result);
        let _ = done_tx.send(Done {
            call_id: Some(call_id),
            outcome,
            engine: Arc::clone(&engine),
        });
        let _ = reply.send(result);
    });
}

/// Report a non-call operation's completion to the dispatcher.
///
/// A listing or resource read holds no call-gate slot, so its [`Done`] carries
/// `call_id: None`; the dispatcher still folds it into the same coalesced
/// transport-failure handling (and the completed exchange that resets the
/// restart budget).
fn report_offloaded(done_tx: &Sender<Done>, engine: &Arc<dyn McpEngine>, outcome: DoneOutcome) {
    let _ = done_tx.send(Done {
        call_id: None,
        outcome,
        engine: Arc::clone(engine),
    });
}

/// Dispatch one command, returning `false` when the dispatcher should stop
/// (i.e. the command was a shutdown). Split out of [`run_dispatcher`] so the
/// (long) match does not have to be nested inside the `select!` arm.
///
/// `reconnecting` is true while a transport rebuild is in flight; a new call is
/// then parked (and accounted via [`CallGate::queue`]) rather than spawned
/// against the dead engine, and is promoted once the rebuilt engine arrives.
#[expect(
    clippy::too_many_arguments,
    reason = "the dispatcher's mutable state is all owned on the thread; bundling it into a struct would add indirection without clarifying anything"
)]
fn handle_command(
    cmd: McpCommand,
    rt: &tokio::runtime::Handle,
    engine: &Arc<dyn McpEngine>,
    inflight: &mut HashMap<u64, (CancelToken, u64)>,
    queued: &mut VecDeque<QueuedCall>,
    gate: &mut CallGate,
    done_tx: &Sender<Done>,
    next_call_id: &mut u64,
    reconnecting: bool,
) -> bool {
    match cmd {
        McpCommand::ListTools { timeout, reply } => {
            // Dispatch the listing onto the runtime rather than blocking the
            // dispatcher on it (as calls already do), so a slow `tools/list`
            // cannot stall this server's command processing. A transport
            // failure still reaches the reconnect logic through the slot-less
            // `Done` the offloaded listing task reports; because the caller
            // keeps its previous catalogue on a failure, the next listing
            // simply runs against the rebuilt engine, so no inline retry
            // belongs here.
            let engine = Arc::clone(engine);
            let done_tx = done_tx.clone();
            rt.spawn(async move {
                let result = engine.list_tools(timeout).await;
                report_offloaded(&done_tx, &engine, classify_outcome(&result));
                let _ = reply.send(result);
            });
        }
        McpCommand::Call {
            session_id,
            request,
            reply,
            chunk_tx,
        } => {
            let call_id = *next_call_id;
            *next_call_id = next_call_id.wrapping_add(1);
            let cancel = CancelToken::new();
            // The per-call timeout bounds the WHOLE call — queue wait plus
            // execution — so the deadline is fixed the instant the command
            // arrives, before the call can park. Both the immediate-admit and
            // the parked path carry the same instant: a call that waits behind
            // a busy server gets only the time still remaining when it runs.
            let deadline = Instant::now() + request.timeout;
            let call = QueuedCall {
                call_id,
                session_id,
                request,
                reply,
                chunk_tx,
                cancel: cancel.clone(),
                deadline,
            };
            if !reconnecting && gate.admit() {
                inflight.insert(call_id, (cancel, session_id));
                spawn_call(rt, engine, done_tx, call);
            } else {
                // While a reconnect is in flight `admit` is skipped entirely, so
                // the parked call must be accounted here; when not reconnecting
                // `admit` already incremented `queued` by refusing, so counting
                // it again would be wrong.
                if reconnecting {
                    gate.queue();
                }
                queued.push_back(call);
            }
        }
        McpCommand::ListResources(reply) => {
            // Off-thread for the same reason as `ListTools` above.
            let engine = Arc::clone(engine);
            let done_tx = done_tx.clone();
            rt.spawn(async move {
                let result = engine.list_resources().await;
                report_offloaded(&done_tx, &engine, classify_outcome(&result));
                let _ = reply.send(result);
            });
        }
        McpCommand::ReadResource { uri, reply } => {
            // Off-thread for the same reason as `ListTools` above.
            let engine = Arc::clone(engine);
            let done_tx = done_tx.clone();
            rt.spawn(async move {
                let result = engine.read_resource(uri).await;
                report_offloaded(&done_tx, &engine, classify_outcome(&result));
                let _ = reply.send(result);
            });
        }
        McpCommand::CancelSession { session_id } => {
            for (token, call_session) in inflight.values() {
                if *call_session == session_id {
                    token.cancel();
                }
            }
            // Cancel queued calls too, replying immediately and freeing their
            // slots — a cancelled call must not linger behind the cap.
            let mut index = 0;
            while index < queued.len() {
                if queued[index].session_id == session_id {
                    if let Some(call) = queued.remove(index) {
                        gate.abandon();
                        call.cancel.cancel();
                        let _ = call.reply.send(Err(McpError::Cancelled));
                    }
                } else {
                    index += 1;
                }
            }
        }
        #[cfg(test)]
        McpCommand::InflightStats(reply) => {
            let _ = reply.send(DispatcherStats {
                active: gate.active,
                queued: gate.queued,
            });
        }
        McpCommand::Shutdown => {
            rt.block_on(engine.shutdown());
            // Leaving returns `false`; `run_dispatcher` then drops `queued`,
            // whose reply senders close, so a waiting caller observes
            // `NotConnected` rather than blocking.
            return false;
        }
    }
    true
}

/// Test-only snapshot of the dispatcher's call accounting.
#[cfg(test)]
pub(crate) struct DispatcherStats {
    pub(super) active: usize,
    pub(super) queued: usize,
}
