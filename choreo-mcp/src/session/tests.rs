use crate::session::cancel::CancelToken;
use crate::session::dispatch::{DispatcherStats, Done, apply_completions, run_dispatcher};
use crate::session::gate::CallGate;
use crate::session::restart::RestartPolicy;

use super::*;
use crate::protocol::CallToolResult;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.name.clone());
        let started_tx = self.started_tx.clone();
        let result = self.result.clone();
        let blocks = self.blocks;
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
    // failure already exceeds the budget, so the factory is never called.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let mut current = Arc::clone(&engine);
    let mut policy = RestartPolicy::new(0);
    let factory: EngineFactory =
        Box::new(|| Ok(Arc::new(MockEngine::new(vec![], ok_result())) as Arc<dyn McpEngine>));
    policy.on_transport_failure(&factory, &mut current);
    assert!(
        Arc::ptr_eq(&current, &engine),
        "no rebuild happens when max_restarts is 0"
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

#[test]
fn batched_transport_failures_reconnect_once() {
    // Two calls lost on the same transport: the drain batch must rebuild the
    // engine exactly once, not once per notice.
    let engine: Arc<dyn McpEngine> = Arc::new(MockEngine::new(vec![], ok_result()));
    let mut current = Arc::clone(&engine);
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let factory = counting_factory(Arc::clone(&factory_calls));
    let mut policy = zero_backoff_policy();
    let mut gate = CallGate::new(4);
    assert!(gate.admit() && gate.admit());
    let mut inflight = HashMap::new();
    inflight.insert(1, (CancelToken::new(), 7));
    inflight.insert(2, (CancelToken::new(), 7));

    let (done_tx, done_rx) = crossbeam_channel::unbounded();
    for call_id in [1u64, 2] {
        done_tx
            .send(Done {
                call_id,
                transport_failed: true,
                engine: Arc::clone(&engine),
            })
            .expect("send done");
    }

    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut policy,
        &factory,
        &mut current,
    );

    assert!(inflight.is_empty(), "every completed call is dropped");
    assert_eq!(gate.active, 0, "every completed call frees its slot");
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
    let mut policy = zero_backoff_policy();
    let mut gate = CallGate::new(4);
    let mut inflight = HashMap::new();
    let (done_tx, done_rx) = crossbeam_channel::unbounded();

    // First batch: one failure reconnects the engine (bumping its identity).
    assert!(gate.admit());
    inflight.insert(1, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: 1,
            transport_failed: true,
            engine: Arc::clone(&engine),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut policy,
        &factory,
        &mut current,
    );
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1);

    // Second batch: a straggler carrying the OLD engine must not reconnect.
    assert!(gate.admit());
    inflight.insert(2, (CancelToken::new(), 7));
    done_tx
        .send(Done {
            call_id: 2,
            transport_failed: true,
            engine: Arc::clone(&engine),
        })
        .expect("send done");
    apply_completions(
        None,
        &done_rx,
        &mut inflight,
        &mut gate,
        &mut policy,
        &factory,
        &mut current,
    );

    assert!(inflight.is_empty());
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        1,
        "a straggler from an already-handled incident must not reconnect again"
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
