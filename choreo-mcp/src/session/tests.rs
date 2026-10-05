use crate::session::cancel::CancelToken;
use crate::session::dispatch::{
    DispatcherStats, Done, DoneOutcome, apply_completions, classify_outcome, run_dispatcher,
    spawn_call,
};
use crate::session::gate::{CallGate, QueuedCall};
use crate::session::restart::{Reconnector, RestartPolicy};

use super::*;
use crate::protocol::CallToolResult;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// A scripted mock engine: `list_tools` returns a fixed vector; `call_tool`
/// waits for the cancellation token or returns a fixed result, whichever
/// comes first.
struct MockEngine {
    tools: Vec<McpTool>,
    result: CallToolResult,
    calls: Mutex<Vec<String>>,
    /// The `timeout` supplied to each `list_tools` call, in order (so a test
    /// can assert the deadline plumbed through the dispatcher).
    list_timeouts: Mutex<Vec<Option<Duration>>>,
    /// Signalled the instant a call reaches the mock, so a test can cancel
    /// deterministically after the call is registered (never by sleeping).
    started_tx: Sender<()>,
    /// When true, `call_tool` blocks until cancelled and never returns the
    /// scripted result (the analogue of a long-running server tool).
    blocks: bool,
    /// When true, only the FIRST `call_tool` blocks (until cancelled); every
    /// later call returns the scripted result at once. Lets a test hold a slot
    /// with one wedged call while a promoted call runs to completion.
    block_first_only: bool,
}

impl MockEngine {
    fn new(tools: Vec<McpTool>, result: CallToolResult) -> Self {
        let (started_tx, _) = crossbeam_channel::unbounded();
        Self {
            tools,
            result,
            calls: Mutex::new(Vec::new()),
            list_timeouts: Mutex::new(Vec::new()),
            started_tx,
            blocks: false,
            block_first_only: false,
        }
    }

    fn blocking(tools: Vec<McpTool>, result: CallToolResult, started_tx: Sender<()>) -> Self {
        Self {
            tools,
            result,
            calls: Mutex::new(Vec::new()),
            list_timeouts: Mutex::new(Vec::new()),
            started_tx,
            blocks: true,
            block_first_only: false,
        }
    }

    /// Like [`blocking`](Self::blocking) but only the first call parks; later
    /// calls return the scripted result immediately.
    fn blocking_first(tools: Vec<McpTool>, result: CallToolResult, started_tx: Sender<()>) -> Self {
        Self {
            tools,
            result,
            calls: Mutex::new(Vec::new()),
            list_timeouts: Mutex::new(Vec::new()),
            started_tx,
            blocks: false,
            block_first_only: true,
        }
    }
    fn call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
    fn list_timeouts(&self) -> Vec<Option<Duration>> {
        self.list_timeouts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl McpEngine for MockEngine {
    fn list_tools(
        &self,
        timeout: Option<Duration>,
    ) -> BoxFuture<'_, Result<Vec<McpTool>, McpError>> {
        self.list_timeouts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(timeout);
        let tools = self.tools.clone();
        Box::pin(async move { Ok(tools) })
    }

    fn call_tool(&self, call: EngineCall) -> BoxFuture<'_, Result<CallToolResult, McpError>> {
        let EngineCall {
            request, cancel, ..
        } = call;
        // Record the call and learn its 0-based index synchronously, so the
        // per-call block decision is deterministic (the future is polled later,
        // on the runtime).
        let index = {
            let mut calls = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            calls.push(request.name.clone());
            calls.len() - 1
        };
        let started_tx = self.started_tx.clone();
        let result = self.result.clone();
        let blocks = self.blocks || (self.block_first_only && index == 0);
        Box::pin(async move {
            // Signal that the call is in flight (and thus registered in the
            // dispatcher's in-flight map) before awaiting.
            let _ = started_tx.send(());
            if blocks {
                // Park until cancelled; a cancel is the only way out.
                cancel.cancelled().await;
                Err(McpError::Cancelled)
            } else {
                Ok(result)
            }
        })
    }

    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<McpResource>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn read_resource(&self, uri: String) -> BoxFuture<'_, Result<Vec<McpContent>, McpError>> {
        let _ = uri;
        Box::pin(async { Ok(Vec::new()) })
    }

    fn supports_resources(&self) -> bool {
        false
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn name(&self) -> &'static str {
        "mock"
    }

    fn version(&self) -> &'static str {
        "0.0.1"
    }
}

/// Spawn a dispatcher over `engine` with a factory that rebuilds it, using
/// the default concurrency cap.
fn spawn(engine: Arc<MockEngine>) -> (McpServerHandle, std::thread::JoinHandle<()>) {
    spawn_with_cap(engine, crate::DEFAULT_MAX_CONCURRENT_CALLS)
}

/// Spawn a dispatcher over `engine` with an explicit concurrency cap.
fn spawn_with_cap(
    engine: Arc<MockEngine>,
    cap: usize,
) -> (McpServerHandle, std::thread::JoinHandle<()>) {
    crate::runtime::init().expect("runtime init");
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let handle = McpServerHandle {
        cmd_tx,
        name: "mock".into(),
        version: "0.0.1".into(),
        default_timeout: Duration::from_secs(5),
        resources: false,
    };
    let factory_engine = Arc::clone(&engine);
    let factory: EngineFactory =
        Box::new(move || Ok(Arc::clone(&factory_engine) as Arc<dyn McpEngine>));
    let join = std::thread::spawn(move || {
        run_dispatcher(
            engine as Arc<dyn McpEngine>,
            factory,
            &cmd_rx,
            cap,
            crate::DEFAULT_MAX_RESTARTS,
        );
    });
    (handle, join)
}

/// Test-only accessor for the dispatcher's call accounting.
impl McpServerHandle {
    fn stats(&self) -> DispatcherStats {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = self.cmd_tx.send(McpCommand::InflightStats(tx));
        rx.recv().expect("dispatcher alive for a stats query")
    }
}

fn tool(name: &str) -> McpTool {
    McpTool {
        name: name.into(),
        description: Some(format!("the {name} tool")),
        input_schema: serde_json::json!({"type": "object"}),
        output_schema: None,
    }
}

fn ok_result() -> CallToolResult {
    CallToolResult {
        content: vec![crate::protocol::McpContent::Text { text: "ok".into() }],
        is_error: false,
        structured_content: None,
    }
}

#[test]
fn list_tools_round_trips() {
    let engine = Arc::new(MockEngine::new(vec![tool("echo")], ok_result()));
    let (handle, join) = spawn(Arc::clone(&engine));
    let tools = handle.list_tools().expect("list tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}

#[test]
fn list_tools_with_deadline_forwards_the_timeout() {
    // The catalogue-refresh path supplies a short deadline; the handle must
    // plumb it through to the engine's listing (a bare `list_tools` uses the
    // server's configured default, i.e. `None`).
    let engine = Arc::new(MockEngine::new(vec![], ok_result()));
    let (handle, join) = spawn(Arc::clone(&engine));
    handle
        .list_tools_with_deadline(Duration::from_secs(3))
        .expect("bounded list");
    handle.list_tools().expect("default list");
    assert_eq!(
        engine.list_timeouts(),
        vec![Some(Duration::from_secs(3)), None],
        "the deadline must reach the engine; a plain listing must not"
    );
    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}

#[test]
fn call_tool_round_trips() {
    let engine = Arc::new(MockEngine::new(vec![], ok_result()));
    let (handle, join) = spawn(engine);
    let result = handle
        .call_tool(7, "echo", serde_json::json!({}), None)
        .expect("call tool");
    assert!(!result.is_error);
    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}

#[test]
fn cancel_session_stops_inflight_call() {
    // The mock blocks on the cancellation token, so a `CancelSession` for
    // the call's session resolves it as cancelled — deterministically: the
    // mock signals over a channel when the call is in flight, and only then
    // does the test cancel, so the call is always registered.
    let (started_tx, started_rx) = crossbeam_channel::unbounded();
    let engine = Arc::new(MockEngine::blocking(vec![], ok_result(), started_tx));
    let (handle, join) = spawn(engine);
    let caller = handle.clone();
    let call =
        std::thread::spawn(move || caller.call_tool(42, "slow", serde_json::json!({}), None));
    // Wait until the call is in flight, then cancel its session.
    started_rx.recv().expect("call started");
    handle.cancel_session(42);
    let result = call.join().expect("call thread joins");
    assert!(matches!(result, Err(McpError::Cancelled)), "got {result:?}");
    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}

#[test]
fn cancel_session_leaves_other_sessions() {
    let engine = Arc::new(MockEngine::new(vec![], ok_result()));
    let (handle, join) = spawn(engine);
    // Cancel a session with no in-flight call: a later call for a different
    // session must still succeed.
    handle.cancel_session(1);
    let result = handle
        .call_tool(2, "echo", serde_json::json!({}), None)
        .expect("unrelated session call");
    assert!(!result.is_error);
    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}

#[test]
fn call_after_shutdown_reports_not_connected() {
    let engine = Arc::new(MockEngine::new(vec![], ok_result()));
    let (handle, join) = spawn(engine);
    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
    let err = handle
        .call_tool(0, "echo", serde_json::json!({}), None)
        .expect_err("dispatcher gone");
    assert!(matches!(err, McpError::NotConnected), "got {err:?}");
}

#[test]
fn restart_policy_backoff_is_capped() {
    let mut policy = RestartPolicy {
        max_attempts: 10,
        base_backoff: Duration::from_secs(1),
        failures: 0,
    };
    policy.failures = 1;
    assert_eq!(policy.backoff(), Duration::from_secs(1));
    policy.failures = 3;
    assert_eq!(policy.backoff(), Duration::from_secs(4));
    policy.failures = 20;
    assert_eq!(policy.backoff(), Duration::from_mins(1));
}

#[test]
fn restart_policy_zero_disables_reconnect() {
    // A `max_restarts` of 0 must leave the engine untouched: the very first
    // failure already exceeds the budget, so no worker is ever spawned and the
    // factory is never called.
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let factory = counting_factory(Arc::clone(&factory_calls));
    let mut reconnector = Reconnector::new(factory, RestartPolicy::new(0));
    reconnector.note_failure();
    assert!(
        !reconnector.is_in_flight(),
        "no reconnect worker is spawned when max_restarts is 0"
    );
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        0,
        "the factory is never called when max_restarts is 0"
    );
}

/// A restart policy whose backoff is zeroed, so a successful reconnect is
/// wait-free (the production 500 ms base would otherwise be slept).
fn zero_backoff_policy() -> RestartPolicy {
    RestartPolicy {
        max_attempts: 5,
        base_backoff: Duration::ZERO,
        failures: 0,
    }
}

/// A factory that rebuilds a fresh mock engine and counts its invocations.
fn counting_factory(counter: Arc<AtomicUsize>) -> EngineFactory {
    Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(MockEngine::new(vec![], ok_result())) as Arc<dyn McpEngine>)
    })
}

/// Drive a reconnect to completion deterministically: block on the worker's
/// result and swap in the fresh engine when it built one.
///
/// The worker always sends exactly one result, so a blocking `recv` is
/// deterministic — never a `recv_timeout`/sleep poll (forbidden in unit tests).
fn await_reconnect(reconnector: &mut Reconnector, current: &mut Arc<dyn McpEngine>) {
    let result = reconnector
        .receiver()
        .recv()
        .expect("reconnect worker always sends its result");
    if let Some(fresh) = reconnector.take_result(result) {
        *current = fresh;
    }
}

#[test]
fn batched_transport_failures_reconnect_once() {
    // Two calls lost on the same transport: the drain batch must spawn exactly
    // one reconnect worker, not one per notice.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let mut current = Arc::clone(&engine);
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let factory = counting_factory(Arc::clone(&factory_calls));
    let mut reconnector = Reconnector::new(factory, zero_backoff_policy());
    let mut gate = CallGate::new(4);
    assert!(gate.admit() && gate.admit());
    let mut inflight = HashMap::new();
    inflight.insert(1, (CancelToken::new(), 7));
    inflight.insert(2, (CancelToken::new(), 7));

    let (done_tx, done_rx) = crossbeam_channel::unbounded();
    for call_id in [1u64, 2] {
        done_tx
            .send(Done {
                call_id: Some(call_id),
                outcome: DoneOutcome::TransportFailed,
                engine: Arc::clone(&engine),
            })
            .expect("send done");
    }

    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );

    assert!(inflight.is_empty(), "every completed call is dropped");
    assert_eq!(gate.active, 0, "every completed call frees its slot");
    assert!(
        reconnector.is_in_flight(),
        "the batch starts exactly one reconnect worker"
    );
    await_reconnect(&mut reconnector, &mut current);
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        1,
        "one dead transport must cost exactly one reconnect, not one per call"
    );
}

#[test]
fn straggler_failure_after_reconnect_does_not_reconnect_again() {
    // A call that ran against an engine since replaced by an earlier
    // failure's reconnect is a straggler: it frees its slot but must not
    // trigger a second reconnect for the same incident.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let mut current = Arc::clone(&engine);
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let factory = counting_factory(Arc::clone(&factory_calls));
    let mut reconnector = Reconnector::new(factory, zero_backoff_policy());
    let mut gate = CallGate::new(4);
    let mut inflight = HashMap::new();
    let (done_tx, done_rx) = crossbeam_channel::unbounded();

    // First batch: one failure reconnects the engine (bumping its identity).
    assert!(gate.admit());
    inflight.insert(1, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(1),
            outcome: DoneOutcome::TransportFailed,
            engine: Arc::clone(&engine),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );
    await_reconnect(&mut reconnector, &mut current);
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1);

    // Second batch: a straggler carrying the OLD engine must not reconnect.
    assert!(gate.admit());
    inflight.insert(2, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(2),
            outcome: DoneOutcome::TransportFailed,
            engine: Arc::clone(&engine),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );

    assert!(inflight.is_empty());
    assert!(
        !reconnector.is_in_flight(),
        "a straggler does not start a reconnect"
    );
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        1,
        "a straggler from an already-handled incident must not reconnect again"
    );
}

#[test]
fn restart_budget_is_not_reset_by_a_reconnect_alone() {
    // A reconnect alone must not clear the budget: a server that reconnects and
    // then immediately dies again is left alone once the budget is spent, rather
    // than rebuilt forever.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let mut current = Arc::clone(&engine);
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let factory = counting_factory(Arc::clone(&factory_calls));
    // A budget of one attempt. After the first incident it is spent; because a
    // reconnect does NOT reset it, the second incident must not reconnect again.
    let mut reconnector = Reconnector::new(
        factory,
        RestartPolicy {
            max_attempts: 1,
            base_backoff: Duration::ZERO,
            failures: 0,
        },
    );
    let mut gate = CallGate::new(4);
    let mut inflight = HashMap::new();
    let (done_tx, done_rx) = crossbeam_channel::unbounded();

    // Incident 1 on the current engine: reconnect #1 (budget was 0).
    assert!(gate.admit());
    inflight.insert(1, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(1),
            outcome: DoneOutcome::TransportFailed,
            engine: Arc::clone(&current),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );
    await_reconnect(&mut reconnector, &mut current);
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1);

    // Incident 2 on the REBUILT engine: the budget is already spent, so no
    // second reconnect — a healthy reconnect did not clear the count.
    assert!(gate.admit());
    inflight.insert(2, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(2),
            outcome: DoneOutcome::TransportFailed,
            engine: Arc::clone(&current),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );
    assert!(
        !reconnector.is_in_flight(),
        "the spent budget spawns no worker"
    );
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        1,
        "a reconnect must not reset the budget; the second incident is left alone"
    );
}

#[test]
fn clean_completion_resets_the_restart_budget() {
    // A request that completes WITHOUT a transport error proves the connection
    // is healthy, so it resets the budget and a following incident may reconnect
    // again.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let mut current = Arc::clone(&engine);
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let factory = counting_factory(Arc::clone(&factory_calls));
    let mut reconnector = Reconnector::new(
        factory,
        RestartPolicy {
            max_attempts: 1,
            base_backoff: Duration::ZERO,
            failures: 0,
        },
    );
    let mut gate = CallGate::new(4);
    let mut inflight = HashMap::new();
    let (done_tx, done_rx) = crossbeam_channel::unbounded();

    // A clean completion (a genuinely completed exchange) resets the budget...
    assert!(gate.admit());
    inflight.insert(1, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(1),
            outcome: DoneOutcome::Completed,
            engine: Arc::clone(&current),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );
    assert_eq!(
        reconnector.failures(),
        0,
        "a clean completion resets the budget"
    );

    // ...so a following incident still reconnects within the budget.
    assert!(gate.admit());
    inflight.insert(2, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(2),
            outcome: DoneOutcome::TransportFailed,
            engine: Arc::clone(&current),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );
    assert!(reconnector.is_in_flight());
    await_reconnect(&mut reconnector, &mut current);
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn timeout_completion_does_not_reset_the_restart_budget() {
    // A deadline does NOT prove the connection is healthy: it can fire while the
    // transport is perfectly usable, so a timeout completion must leave the
    // consecutive-failure count untouched rather than resetting it.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let current = Arc::clone(&engine);
    let factory = counting_factory(Arc::new(AtomicUsize::new(0)));
    // Seed a non-zero failure count directly so a spurious reset is observable.
    let mut reconnector = Reconnector::new(
        factory,
        RestartPolicy {
            max_attempts: 5,
            base_backoff: Duration::ZERO,
            failures: 2,
        },
    );
    let mut gate = CallGate::new(4);
    let mut inflight = HashMap::new();
    let (done_tx, done_rx) = crossbeam_channel::unbounded();

    assert!(gate.admit());
    inflight.insert(1, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(1),
            outcome: classify_outcome(&Err::<(), _>(McpError::Timeout)),
            engine: Arc::clone(&current),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );

    assert!(
        inflight.is_empty(),
        "the timed-out call still frees its slot"
    );
    assert_eq!(
        reconnector.failures(),
        2,
        "a timeout does not prove health and must not reset the budget"
    );
}

#[test]
fn cancelled_completion_does_not_reset_the_restart_budget() {
    // A client-side cancel never reached a verdict about the connection, so it
    // must leave the consecutive-failure count untouched.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let current = Arc::clone(&engine);
    let factory = counting_factory(Arc::new(AtomicUsize::new(0)));
    let mut reconnector = Reconnector::new(
        factory,
        RestartPolicy {
            max_attempts: 5,
            base_backoff: Duration::ZERO,
            failures: 2,
        },
    );
    let mut gate = CallGate::new(4);
    let mut inflight = HashMap::new();
    let (done_tx, done_rx) = crossbeam_channel::unbounded();

    assert!(gate.admit());
    inflight.insert(1, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(1),
            outcome: classify_outcome(&Err::<(), _>(McpError::Cancelled)),
            engine: Arc::clone(&current),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );

    assert!(
        inflight.is_empty(),
        "the cancelled call still frees its slot"
    );
    assert_eq!(
        reconnector.failures(),
        2,
        "a cancel does not prove health and must not reset the budget"
    );
}

#[test]
fn completed_exchange_resets_the_restart_budget() {
    // An `Ok` result proves the connection works, so it must clear the
    // consecutive-failure count.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let current = Arc::clone(&engine);
    let factory = counting_factory(Arc::new(AtomicUsize::new(0)));
    let mut reconnector = Reconnector::new(
        factory,
        RestartPolicy {
            max_attempts: 5,
            base_backoff: Duration::ZERO,
            failures: 2,
        },
    );
    let mut gate = CallGate::new(4);
    let mut inflight = HashMap::new();
    let (done_tx, done_rx) = crossbeam_channel::unbounded();

    assert!(gate.admit());
    inflight.insert(1, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: Some(1),
            outcome: classify_outcome(&Ok::<(), McpError>(())),
            engine: Arc::clone(&current),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut reconnector,
        &current,
    );

    assert_eq!(
        reconnector.failures(),
        0,
        "a genuinely completed exchange resets the budget"
    );
}

#[test]
fn mock_engine_records_calls() {
    let engine = Arc::new(MockEngine::new(vec![], ok_result()));
    let (handle, join) = spawn(Arc::clone(&engine));
    handle
        .call_tool(0, "echo", serde_json::json!({}), None)
        .expect("call");
    assert_eq!(engine.call_count(), 1);
    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}

#[test]
fn call_gate_admits_up_to_cap_then_queues() {
    let mut gate = CallGate::new(2);
    assert!(gate.admit(), "first call takes a slot");
    assert!(gate.admit(), "second call takes the other slot");
    assert!(!gate.admit(), "third call is queued");
    assert!(!gate.admit(), "fourth call is queued too");
    assert_eq!((gate.active, gate.queued), (2, 2));

    // A completion frees a slot; exactly one queued call can then be
    // promoted, and its queue depth drops by one.
    gate.complete();
    assert!(gate.has_capacity());
    gate.promote();
    assert_eq!((gate.active, gate.queued), (2, 1));
    assert!(!gate.has_capacity());

    // Abandoning a queued call (a session cancel) drops it from the queue.
    gate.abandon();
    assert_eq!((gate.active, gate.queued), (2, 0));
}

#[test]
fn call_gate_clamps_zero_cap_to_one() {
    let mut gate = CallGate::new(0);
    assert!(gate.admit());
    assert!(!gate.admit());
    assert_eq!((gate.active, gate.queued), (1, 1));
}

#[test]
fn concurrency_cap_queues_excess_calls() {
    // A blocking mock: a call that is admitted parks until cancelled, so the
    // slot it holds stays busy. With a cap of one, the second call must wait
    // in the queue rather than reach the engine.
    let (started_tx, started_rx) = crossbeam_channel::unbounded();
    let engine = Arc::new(MockEngine::blocking(vec![], ok_result(), started_tx));
    let (handle, join) = spawn_with_cap(engine, 1);

    // First call: admitted immediately and reaches the engine.
    let (tx1, rx1) = crossbeam_channel::bounded(1);
    handle
        .cmd_tx
        .send(McpCommand::Call {
            session_id: 1,
            request: CallRequest {
                name: "slow".into(),
                arguments: serde_json::json!({}),
                timeout: Duration::from_secs(5),
            },
            reply: tx1,
            chunk_tx: None,
        })
        .expect("send call 1");
    started_rx.recv().expect("call 1 reached the engine");

    // Second call: the only slot is taken, so it queues. Sending both
    // commands and the stats query from this thread pins their order, so the
    // counts are deterministic (no sleep-poll).
    let (tx2, rx2) = crossbeam_channel::bounded(1);
    handle
        .cmd_tx
        .send(McpCommand::Call {
            session_id: 2,
            request: CallRequest {
                name: "slow".into(),
                arguments: serde_json::json!({}),
                timeout: Duration::from_secs(5),
            },
            reply: tx2,
            chunk_tx: None,
        })
        .expect("send call 2");
    let stats = handle.stats();
    assert_eq!(stats.active, 1, "one call in flight");
    assert_eq!(stats.queued, 1, "the second call waits in the queue");

    // Cancelling session 1 frees the slot; the queued call is promoted and
    // reaches the engine.
    handle.cancel_session(1);
    assert!(
        matches!(rx1.recv().expect("call 1 reply"), Err(McpError::Cancelled)),
        "call 1 should be cancelled"
    );
    started_rx
        .recv()
        .expect("call 2 promoted and reaches the engine after a slot frees");
    let stats = handle.stats();
    assert_eq!(stats.active, 1);
    assert_eq!(stats.queued, 0, "the queue drained once the slot freed");

    handle.cancel_session(2);
    assert!(
        matches!(rx2.recv().expect("call 2 reply"), Err(McpError::Cancelled)),
        "call 2 should be cancelled"
    );

    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}

/// Send one tool call through the dispatcher's command channel.
fn send_call(
    handle: &McpServerHandle,
    session_id: u64,
    name: &str,
    timeout: Duration,
) -> crossbeam_channel::Receiver<Result<CallToolResult, McpError>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    handle
        .cmd_tx
        .send(McpCommand::Call {
            session_id,
            request: CallRequest {
                name: name.into(),
                arguments: serde_json::json!({}),
                timeout,
            },
            reply: tx,
            chunk_tx: None,
        })
        .expect("send call");
    rx
}

#[test]
fn expired_queued_call_times_out_and_never_runs() {
    // A call's per-call timeout bounds its WHOLE lifetime. Here call 1 wedges
    // and holds the only slot; call 2 (a zero timeout) is parked with a deadline
    // already behind it, so the dispatcher must reap it with a timeout rather
    // than let it block forever — and it must never reach the engine.
    let (started_tx, started_rx) = crossbeam_channel::unbounded();
    let engine = Arc::new(MockEngine::blocking(vec![], ok_result(), started_tx));
    let (handle, join) = spawn_with_cap(Arc::clone(&engine), 1);

    let _rx1 = send_call(&handle, 1, "slow", Duration::from_secs(5));
    started_rx
        .recv()
        .expect("call 1 in flight and holding the slot");

    let rx2 = send_call(&handle, 2, "queued", Duration::ZERO);
    assert!(
        matches!(rx2.recv().expect("call 2 reply"), Err(McpError::Timeout)),
        "an expired parked call must reply with a timeout"
    );
    assert_eq!(
        engine.call_count(),
        1,
        "the expired parked call must never reach the engine"
    );

    handle.cancel_session(1);
    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}

#[test]
fn expired_call_at_spawn_is_not_run() {
    // The last-instant guard: a call promoted WITH no remaining time (its
    // deadline already passed) must be failed without ever touching the engine,
    // while still retiring the slot it took.
    crate::runtime::init().expect("runtime init");
    let rt = crate::runtime::handle().expect("runtime handle");
    let mock = Arc::new(MockEngine::new(vec![], ok_result()));
    let engine: Arc<dyn McpEngine> = Arc::clone(&mock) as Arc<dyn McpEngine>;
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<Done>();
    let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);

    let call = QueuedCall {
        call_id: 7,
        session_id: 1,
        request: CallRequest {
            name: "late".into(),
            arguments: serde_json::json!({}),
            timeout: Duration::from_secs(5),
        },
        reply: reply_tx,
        chunk_tx: None,
        cancel: CancelToken::new(),
        deadline: Instant::now(), // already reached: no time remains
    };
    spawn_call(&rt, &engine, &done_tx, call);

    assert!(
        matches!(reply_rx.recv().expect("reply"), Err(McpError::Timeout)),
        "a call past its deadline is failed, not run"
    );
    let done = done_rx.recv().expect("done notice retires the slot");
    assert_eq!(done.call_id, Some(7));
    assert!(
        matches!(done.outcome, DoneOutcome::Inconclusive),
        "a deadline proves nothing about the connection's health"
    );
    assert_eq!(
        mock.call_count(),
        0,
        "an expired call must never reach the engine"
    );
}

#[test]
fn queued_call_promoted_before_deadline_runs() {
    // A call that waits but is promoted BEFORE its deadline must still run and
    // return its result — the deadline bounds the wait, it does not cancel a
    // call that started in time.
    let (started_tx, started_rx) = crossbeam_channel::unbounded();
    let engine = Arc::new(MockEngine::blocking_first(vec![], ok_result(), started_tx));
    let (handle, join) = spawn_with_cap(Arc::clone(&engine), 1);

    let _rx1 = send_call(&handle, 1, "slow", Duration::from_secs(5));
    started_rx
        .recv()
        .expect("call 1 in flight and holding the slot");
    let rx2 = send_call(&handle, 2, "queued", Duration::from_secs(5));
    let stats = handle.stats();
    assert_eq!(
        (stats.active, stats.queued),
        (1, 1),
        "call 2 waits in the queue"
    );

    // Free the slot: call 2 is promoted (well before its deadline), reaches the
    // engine, and returns the scripted result.
    handle.cancel_session(1);
    started_rx
        .recv()
        .expect("call 2 promoted and reaches the engine");
    let result = rx2.recv().expect("call 2 reply").expect("call 2 succeeds");
    assert!(
        !result.is_error,
        "the promoted call returns its scripted result"
    );
    let stats = handle.stats();
    assert_eq!(stats.queued, 0, "the queue drained once the slot freed");

    let _ = handle.cmd_tx.send(McpCommand::Shutdown);
    join.join().expect("dispatcher joins");
}
