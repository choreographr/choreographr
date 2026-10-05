//! The dispatcher loop: serves one server's commands and owns the in-flight
//! call registry.
//!
//! The loop drains completion notices, starts queued calls into any freed
//! slots, then blocks on the next command or completion. All mutable state
//! (the engine, the restart policy, the in-flight map, the queued deque, and
//! the admission [`CallGate`]) lives on this thread — no lock.

use crate::error::McpError;
use crate::session::cancel::CancelToken;
use crate::session::gate::{CallGate, QueuedCall};
use crate::session::restart::{RestartPolicy, is_transport_error_ref};
use crate::session::{EngineCall, EngineFactory, McpCommand, McpEngine};
use crossbeam_channel::{Receiver, Sender};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Completion notice a spawned task sends back to the dispatcher.
///
/// A tool call and a listing/resource read both report here so the dispatcher
/// can free a call's slot (when there is one) and reconnect a dead transport.
/// Listings consume no concurrency slot, so their `call_id` is `None`.
pub(super) struct Done {
    /// The in-flight call this notice retires, or `None` for a non-call
    /// operation (a listing or resource read) that holds no gate slot.
    pub(super) call_id: Option<u64>,
    pub(super) transport_failed: bool,
    /// The engine the operation ran against. Compared by identity so a failure
    /// that arrives after the engine has already been rebuilt (a straggler from
    /// an incident another notice already handled) frees its slot without
    /// triggering a redundant reconnect.
    pub(super) engine: Arc<dyn McpEngine>,
}

/// The dispatcher thread body: serve commands until shutdown or disconnect.
///
/// `max_concurrent_calls` bounds how many tool calls to this server run at once;
/// calls beyond the cap wait in a queue and are started as slots free. Listings
/// and resource reads are dispatched onto the runtime like calls but consume no
/// concurrency slot, so they are not subject to the cap and cannot stall the
/// dispatcher. `max_restarts` bounds how many times a dead transport is rebuilt
/// in a row before the server is left alone.
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
    let mut policy = RestartPolicy::new(max_restarts);
    let mut inflight: HashMap<u64, (CancelToken, u64)> = HashMap::new();
    let mut queued: VecDeque<QueuedCall> = VecDeque::new();
    let mut gate = CallGate::new(max_concurrent_calls);
    let mut next_call_id: u64 = 1;

    // Completion notices from spawned call tasks; drained between commands.
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<Done>();

    loop {
        // Drain completion notices before waiting: a finished call frees a slot
        // and, if it died on the transport, triggers a reconnect. The whole
        // available batch is coalesced into at most ONE reconnect, so N requests
        // lost on one dead transport cost one rebuild, not N.
        apply_completions(
            None,
            &done_rx,
            &mut inflight,
            &mut gate,
            &mut policy,
            &factory,
            &mut engine,
        );
        // Start queued calls into any slots the completions freed.
        pump_queued(
            &mut queued,
            &mut gate,
            &mut inflight,
            &rt,
            &engine,
            &done_tx,
        );

        // Wait for the next command or completion. Both are event sources, so no
        // polling: a finished call wakes the loop to free its slot even when no
        // command has arrived.
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
                        &mut policy,
                        &factory,
                        &mut engine,
                    );
                }
            }
        }
    }
    tracing::debug!("MCP dispatcher thread exiting");
}

/// Apply the completion notices currently available — `first`, when the caller
/// already received one from the `select!`, plus everything else the channel
/// holds — freeing each call's slot and rebuilding the engine AT MOST ONCE if
/// any notice died on the transport.
///
/// One dead transport fails every in-flight request at once, each sending its
/// own notice; rebuilding per notice would recreate the engine (with a backoff
/// sleep) once per request for a single incident. The batch is coalesced: the
/// reconnect happens once, after the whole batch is drained, so `pump_queued`
/// then starts queued calls against the rebuilt engine. A notice whose request
/// ran against an engine since replaced (compared by identity) only frees its
/// slot — it is a straggler from an already-handled incident, not a new one.
///
/// A notice that did NOT fail on the transport proves the connection is usable
/// again, so it resets the restart budget (`record_success`); the budget is
/// deliberately not reset by the reconnect itself (only a surviving request
/// clears it), which bounds a server that reconnects and immediately dies.
pub(super) fn apply_completions(
    first: Option<Done>,
    done_rx: &Receiver<Done>,
    inflight: &mut HashMap<u64, (CancelToken, u64)>,
    gate: &mut CallGate,
    policy: &mut RestartPolicy,
    factory: &EngineFactory,
    engine: &mut Arc<dyn McpEngine>,
) {
    let mut transport_failed = false;
    for done in first.into_iter().chain(done_rx.try_iter()) {
        // A call notice retires its in-flight slot; a listing notice (`None`)
        // holds no slot and only reports transport health.
        if let Some(call_id) = done.call_id {
            inflight.remove(&call_id);
            gate.complete();
        }
        if done.transport_failed {
            if Arc::ptr_eq(&done.engine, &*engine) {
                transport_failed = true;
            }
        } else {
            policy.record_success();
        }
    }
    if transport_failed {
        policy.on_transport_failure(factory, engine);
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

/// Spawn one call onto the sidecar runtime.
///
/// The task sends a [`Done`] notice (so the dispatcher frees the slot and can
/// reconnect a dead transport) followed by the reply; both are best-effort — a
/// dropped receiver means the caller already went away.
fn spawn_call(
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
        ..
    } = call;
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
        let transport_failed = is_transport_error_ref(&result);
        let _ = done_tx.send(Done {
            call_id: Some(call_id),
            transport_failed,
            engine: Arc::clone(&engine),
        });
        let _ = reply.send(result);
    });
}

/// Report a non-call operation's completion to the dispatcher.
///
/// A listing or resource read holds no call-gate slot, so its [`Done`] carries
/// `call_id: None`; the dispatcher still folds it into the same coalesced
/// transport-failure handling (and the success that resets the restart budget).
fn report_offloaded(done_tx: &Sender<Done>, engine: &Arc<dyn McpEngine>, transport_failed: bool) {
    let _ = done_tx.send(Done {
        call_id: None,
        transport_failed,
        engine: Arc::clone(engine),
    });
}

/// Dispatch one command, returning `false` when the dispatcher should stop
/// (i.e. the command was a shutdown). Split out of [`run_dispatcher`] so the
/// (long) match does not have to be nested inside the `select!` arm.
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
                report_offloaded(&done_tx, &engine, is_transport_error_ref(&result));
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
            let call = QueuedCall {
                call_id,
                session_id,
                request,
                reply,
                chunk_tx,
                cancel: cancel.clone(),
            };
            if gate.admit() {
                inflight.insert(call_id, (cancel, session_id));
                spawn_call(rt, engine, done_tx, call);
            } else {
                queued.push_back(call);
            }
        }
        McpCommand::ListResources(reply) => {
            // Off-thread for the same reason as `ListTools` above.
            let engine = Arc::clone(engine);
            let done_tx = done_tx.clone();
            rt.spawn(async move {
                let result = engine.list_resources().await;
                report_offloaded(&done_tx, &engine, is_transport_error_ref(&result));
                let _ = reply.send(result);
            });
        }
        McpCommand::ReadResource { uri, reply } => {
            // Off-thread for the same reason as `ListTools` above.
            let engine = Arc::clone(engine);
            let done_tx = done_tx.clone();
            rt.spawn(async move {
                let result = engine.read_resource(uri).await;
                report_offloaded(&done_tx, &engine, is_transport_error_ref(&result));
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
