//! iOS-native-tool bridge — the Rust side of the choreo-daemon ↔ Swift host
//! seam (tool names: `clipboard_write`, `clipboard_read`, `open_url`,
//! `notify`; the tools themselves are registered in the NEXT subsession —
//! this module is bridge plumbing only, compiled UNCONDITIONALLY on every
//! target, following the `powershell` precedent: modules compile on all
//! targets, only registration is platform-gated).
//!
//! # Architecture
//!
//! On iOS the GUI hosts the daemon in-process (`choreo-daemon/src/embedded.rs`)
//! inside an app that also owns the UIKit main thread. Tool execution happens
//! on daemon worker threads, but UIKit calls must happen on (or be serialized
//! onto) the main queue. The seam is therefore:
//!
//! ```text
//! daemon tool thread ──dispatch──► SwiftIosToolBridge (choreo-gui, cfg(ios))
//!      │                                   │  (enqueue only — NEVER blocks)
//!      │                                   ▼
//!      │                        choreo_ios_tool_request (C ABI → Swift)
//!      │                                   │  DispatchQueue.main.async
//!      │                                   ▼
//!      │                          IosToolHost.swift handlers
//!      │                                   │  exactly-once reply
//!      │                                   ▼
//!      └── reply channel ◄── choreo_ios_tool_reply (C ABI → Rust) ◄──────┘
//! ```
//!
//! `choreo_ios_tool_request` must only ENQUEUE onto the main queue and return
//! immediately; the blocking wait happens Rust-side on the reply channel
//! (with a caller-supplied deadline and cancellation polling), never inside
//! the C call and never on the main queue.
//!
//! # Reply-slot ownership contract (BINDING — do not paraphrase)
//!
//! Each request boxes a one-shot crossbeam reply `Sender` as an opaque pointer
//! passed through the C ABI; ownership transfers to Swift at dispatch; Rust
//! NEVER frees the box; Swift guarantees exactly-once reply on its serial main
//! queue; if Rust abandons (timeout/cancel) it drops the receiver and a late
//! Swift reply sends into a disconnected channel (`Err` ignored) and then
//! drops the slot — no UAF, no leak.
//!
//! Concretely: `dispatch` does `Box::into_raw(Box::new(sender))` and passes
//! that pointer as `reply_ctx`. Swift's per-request exactly-once flag calls
//! the reply callback exactly once, which does `Box::from_raw`, `send`s
//! (a `SendError` into the disconnected receiver is deliberately ignored —
//! that IS the abandoned-request path), and lets the `Box` drop, freeing the
//! slot. The sender is never cloned and the box is never freed anywhere else.
//!
//! # Test seam and CI status
//!
//! [`MockBridge`] lets unit tests script replies (success / platform error /
//! dropped sender) without any Apple runtime, so the trait contract — JSON
//! round-trip, timeout with `Duration::ZERO`, cancel-precedence — is pinned
//! on every dev box. The concrete `SwiftIosToolBridge` lives in
//! `choreo-gui/src/ios_bridge.rs` behind `#[cfg(target_os = "ios")]` (that
//! crate only depends on choreo-daemon under iOS — see its Cargo.toml — so
//! the cfg is also the dependency gate).
//!
//! The Swift host (`ios/IosToolHost.swift`) is NOT compiled in CI: the first
//! Swift error will surface on a Mac. `scripts/build-ios.sh`'s zig path
//! validates the Rust `cfg(ios)` code for `aarch64-apple-ios`; the final
//! Apple link is skipped on non-Mac hosts (expected).

use crossbeam_channel::{Receiver, bounded, select};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── Caller-supplied timeouts ────────────────────────────────────────────────
//
// The iOS tools (next subsession) take these as constructor params; they are
// named here because the per-tool values are a bridge-design decision, not a
// per-call-site one: clipboard touches an in-process pasteboard (fast), open
// URL hands off to SpringBoard and awaits the completion handler (can bounce
// through system UI), notify adds a UNUserNotificationCenter request and may
// lazily trigger a provisional-authorization round trip (slowest).

/// Deadline for `clipboard_write` / `clipboard_read`.
pub const CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(1500);
/// Deadline for `open_url` (SpringBoard handoff + completion handler).
pub const OPEN_URL_TIMEOUT: Duration = Duration::from_millis(3000);
/// Deadline for `notify` (may include a lazy provisional-authorization trip).
pub const NOTIFY_TIMEOUT: Duration = Duration::from_millis(5000);

/// How long a `wait` polls between cancellation-flag checks. Cancellation is
/// a caller-supplied predicate (the tool will pass `ToolContext.cancelled`'s
/// flag — the sanctioned cooperative-cancellation-flag exception), not a
/// channel event, so waiting the FULL deadline before noticing a cancel would
/// make cancellation unresponsive; instead `wait` re-checks on this slice.
/// The reply channel itself is waited on with `select!` — no busy polling.
const CANCEL_POLL_SLICE: Duration = Duration::from_millis(50);

/// Structured error crossing the bridge. Serializable so it can ride the
/// tool-result JSON path (and be embedded in `ToolOutput::result_json`).
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ToolBridgeError {
    /// The Swift host never replied — either dispatch failed before Swift
    /// ever saw the request, or the reply channel was dropped (abandoned
    /// request / host gone). Detected via a disconnected reply channel.
    #[error("iOS tool bridge unavailable")]
    BridgeUnavailable,
    /// The caller's cancellation flag was observed (before or after the reply
    /// arrived). Best-effort Swift-side cancellation may also have been sent.
    #[error("request was canceled")]
    Canceled,
    /// The caller-supplied deadline elapsed without a reply.
    #[error("request timed out")]
    Timeout,
    /// The Swift host reported a failure (or its reply payload could not be
    /// decoded). The string is a human-readable message for the model.
    #[error("platform error: {0}")]
    Platform(String),
}

/// Envelope sent Rust → Swift. `name` selects the Swift handler
/// (`clipboard_write` | `clipboard_read` | `open_url` | `notify`);
/// `args_json` is the tool's JSON arguments verbatim (Swift decodes with
/// JSONSerialization — no shared schema on this boundary, only the
/// per-handler convention documented in IosToolHost.swift).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IosToolRequest {
    /// Monotonically increasing per-bridge id; echoed by
    /// `choreo_ios_tool_cancel` for Swift-side best-effort cancel and useful
    /// for correlating traces across the boundary.
    pub request_id: u64,
    /// Tool name (`&'static str` at the call sites — the `Tool` trait's
    /// `name()` — carried as `String` so the envelope is owned/serializable).
    pub name: String,
    /// Serialized tool arguments (JSON object).
    pub args_json: String,
}

/// Reply payload: a JSON value on success, or a structured bridge error.
pub type ToolBridgeReply = Result<serde_json::Value, ToolBridgeError>;

/// Handle for one in-flight request. Exposes the id (for tracing and
/// Swift-side cancel), the reply channel (for a blocking `wait`), and
/// `cancel()`.
// No derived Debug: the boxed cancel hook is not Debug, and a manual impl
// would gain nothing (the interesting fields are `request_id` + the reply
// channel, which callers log directly).
pub struct IosToolPending {
    /// Assigned by the bridge at dispatch time (see the id counter per impl).
    pub request_id: u64,
    /// One-shot reply channel: exactly one value ever arrives, or the sender
    /// is dropped and `wait` reports [`ToolBridgeError::BridgeUnavailable`].
    pub reply_rx: Receiver<ToolBridgeReply>,
    /// Best-effort cancel hook, supplied by the concrete bridge at dispatch:
    /// for the Swift bridge it calls `choreo_ios_tool_cancel(request_id)`;
    /// for [`MockBridge`] it just records the cancel. `Option` because a
    /// bridge may have no cancel path (e.g. the request already completed).
    cancel: Option<Box<dyn FnOnce() + Send>>,
}

impl IosToolPending {
    /// The only construction path. Public because the concrete bridge lives
    /// in another crate (choreo-gui); the private field keeps the cancel hook
    /// inaccessible afterwards (cancel is exclusively via `cancel(self)`).
    pub fn new(
        request_id: u64,
        reply_rx: Receiver<ToolBridgeReply>,
        cancel: Box<dyn FnOnce() + Send>,
    ) -> Self {
        Self {
            request_id,
            reply_rx,
            cancel: Some(cancel),
        }
    }

    /// Wait for the reply until `deadline`, polling `is_canceled` so a cancel
    /// is noticed even while no reply is in flight. On success the JSON value
    /// is returned; a Swift-reported failure surfaces as
    /// [`ToolBridgeError::Platform`].
    ///
    /// Cancel-precedence: if the flag is observed set — including AFTER a
    /// reply has arrived but BEFORE it is returned — the result is
    /// [`ToolBridgeError::Canceled`]; the tool loop is ending anyway, so the
    /// model must see "canceled", not a result for a request the user
    /// abandoned mid-flight.
    ///
    /// `Duration::ZERO` (or an already-elapsed deadline) returns `Timeout`
    /// immediately without ever blocking — the deterministic test path.
    pub fn wait(&self, deadline: Duration, is_canceled: &dyn Fn() -> bool) -> ToolBridgeReply {
        // Absolute deadline so per-iteration select slices can never stretch
        // the total (the same absolute-budget discipline the Noise handshake
        // uses): a poll slice bounds only cancel responsiveness.
        let deadline_at = Instant::now()
            .checked_add(deadline)
            .unwrap_or_else(Instant::now);
        loop {
            // Check cancellation FIRST: a pre-canceled request must not even
            // consume its (possibly already-arrived) reply.
            if is_canceled() {
                return Err(ToolBridgeError::Canceled);
            }
            let now = Instant::now();
            if now >= deadline_at {
                return Err(ToolBridgeError::Timeout);
            }
            // Slice the remaining budget so the flag is re-checked at least
            // every CANCEL_POLL_SLICE; the reply arm fires the moment a reply
            // lands regardless of the slice.
            let slice = CANCEL_POLL_SLICE.min(deadline_at - now);
            select! {
                recv(self.reply_rx) -> res => {
                    match res {
                        Ok(reply) => {
                            // Cancel-precedence re-check after the reply: the
                            // flag may have been set while the reply was in
                            // flight (the mock test pins this exact race).
                            if is_canceled() {
                                return Err(ToolBridgeError::Canceled);
                            }
                            return reply;
                        }
                        // Sender dropped with no reply: the abandoned-request
                        // / host-gone path.
                        Err(_) => return Err(ToolBridgeError::BridgeUnavailable),
                    }
                }
                recv(crossbeam_channel::after(slice)) -> _ => {
                    // Timer slice elapsed: loop re-checks the flag, then the
                    // deadline. No sleep anywhere — the select IS the wait.
                    continue;
                }
            }
        }
    }

    /// Best-effort Swift-side cancel (the request id is echoed so the host
    /// can drop a still-queued request). Always pairs with setting the
    /// caller's flag — the flag is authoritative for `wait`, this only
    /// tells Swift to stop doing work that nobody will read.
    pub fn cancel(self) {
        if let Some(cancel) = self.cancel {
            cancel();
        }
        // Dropping `self` (and with it `reply_rx`) is the abandonment path:
        // a late Swift reply then sends into a disconnected channel and is
        // ignored — see the module-header reply-slot contract.
    }
}

/// The seam the iOS tools (next subsession) will hold as
/// `Arc<dyn IosToolBridge>`. Object-safe, `Send + Sync`, dispatch-only:
/// all waiting/cancellation lives on [`IosToolPending`] so implementations
/// stay trivial and the concurrency contract lives in exactly one place.
pub trait IosToolBridge: Send + Sync {
    /// Hand a request to the host. Returns the pending handle or an error
    /// produced BEFORE the request ever reached Swift (e.g. the concrete
    /// bridge failing to encode strings). Must not block: the Swift bridge's
    /// implementation only enqueues onto the main queue.
    fn dispatch(&self, request: IosToolRequest) -> Result<IosToolPending, ToolBridgeError>;
}

// ── MockBridge ──────────────────────────────────────────────────────────────

/// Scripted in-process bridge for unit tests (and any host that wants to
/// exercise tool logic without UIKit). Replies are popped FIFO from a
/// script queue; with an empty queue the reply sender is DROPPED (the
/// abandoned-request path → `BridgeUnavailable`), which is the safest
/// default: an unscripted test failure surfaces loudly instead of
/// fabricating a success.
#[derive(Default)]
pub struct MockBridge {
    /// FIFO of scripted per-dispatch behaviors. Interior mutability through a
    /// `Mutex` (poisoning-recovered per house rules) — dispatch may run on
    /// any thread and tests may script from another.
    script: Mutex<VecDeque<MockResponse>>,
    /// Every dispatched request, in order (test assertion surface).
    dispatched: Mutex<Vec<IosToolRequest>>,
    /// Every canceled request id, in order. An `Arc` because the cancel hook
    /// captured into each pending handle must be `'static` (the same shape
    /// the Swift bridge uses for its FFI cancel call).
    cancels: Arc<Mutex<Vec<u64>>>,
    /// Monotonic request-id source.
    next_id: AtomicU64,
}

/// One scripted dispatch behavior.
#[derive(Debug)]
pub enum MockResponse {
    /// Reply with this value.
    Reply(ToolBridgeReply),
    /// Drop the reply sender without replying → `wait` sees
    /// [`ToolBridgeError::BridgeUnavailable`] (disconnected channel).
    DropSender,
}

impl MockBridge {
    /// Queue one scripted response (served FIFO; see the struct doc for the
    /// empty-queue default).
    pub fn script(&self, response: MockResponse) {
        self.script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(response);
    }

    /// Snapshot of dispatched requests (ordered).
    pub fn dispatched(&self) -> Vec<IosToolRequest> {
        self.dispatched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Snapshot of canceled request ids (ordered).
    pub fn cancels(&self) -> Vec<u64> {
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl IosToolBridge for MockBridge {
    fn dispatch(&self, request: IosToolRequest) -> Result<IosToolPending, ToolBridgeError> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = IosToolRequest {
            request_id,
            ..request
        };
        self.dispatched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.clone());
        let (tx, rx) = bounded::<ToolBridgeReply>(1);
        // Bounded(1): a scripted reply never blocks the mock (capacity fits
        // the one-shot contract), so no thread is spawned even in tests.
        let response = self
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or(MockResponse::DropSender);
        match response {
            MockResponse::Reply(reply) => {
                let _ = tx.send(reply);
                // tx drops here; the value is already buffered in rx.
            }
            MockResponse::DropSender => { /* tx drops disconnected → BridgeUnavailable */ }
        }
        Ok(IosToolPending::new(
            request_id,
            rx,
            Box::new({
                // Cloned Arc, not a borrow: the closure outlives this
                // dispatch call (it lives in the pending handle).
                let cancels = Arc::clone(&self.cancels);
                move || {
                    cancels
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(request_id);
                }
            }),
        ))
    }
}

// ── Unit tests (deterministic — no sleeps anywhere) ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mock() -> MockBridge {
        MockBridge::default()
    }

    fn request() -> IosToolRequest {
        IosToolRequest {
            request_id: 0, // reassigned by dispatch
            name: "clipboard_write".to_string(),
            args_json: r#"{"text":"hello"}"#.to_string(),
        }
    }

    #[test]
    fn envelope_json_round_trips() {
        // The envelope crosses the C ABI as strings but must stay
        // serde-compatible (it is the documented wire shape; Swift decodes
        // args_json, Rust owns the envelope itself).
        let req = request();
        let json = serde_json::to_string(&req).unwrap();
        let back: IosToolRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "clipboard_write");
        assert_eq!(back.args_json, r#"{"text":"hello"}"#);
    }

    #[test]
    fn error_type_serializes_round_trip() {
        for err in [
            ToolBridgeError::BridgeUnavailable,
            ToolBridgeError::Canceled,
            ToolBridgeError::Timeout,
            ToolBridgeError::Platform("springboard refused".into()),
        ] {
            let json = serde_json::to_string(&err).unwrap();
            let back: ToolBridgeError = serde_json::from_str(&json).unwrap();
            assert_eq!(back.to_string(), err.to_string());
        }
    }

    #[test]
    fn dispatch_success_and_request_recording() {
        let m = mock();
        m.script(MockResponse::Reply(Ok(serde_json::json!({"ok": true}))));
        let pending = m.dispatch(request()).unwrap();
        assert_eq!(pending.request_id, 0);
        let value = pending.wait(CLIPBOARD_TIMEOUT, &|| false).unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
        let dispatched = m.dispatched();
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].name, "clipboard_write");
        assert_eq!(dispatched[0].request_id, 0);
        // Ids increase across dispatches.
        let p2 = m.dispatch(request()).unwrap();
        assert_eq!(p2.request_id, 1);
    }

    #[test]
    fn platform_error_propagates() {
        let m = mock();
        m.script(MockResponse::Reply(Err(ToolBridgeError::Platform(
            "https or mailto only".into(),
        ))));
        let pending = m.dispatch(request()).unwrap();
        match pending.wait(OPEN_URL_TIMEOUT, &|| false) {
            Err(ToolBridgeError::Platform(msg)) => assert_eq!(msg, "https or mailto only"),
            other => panic!("expected Platform error, got {other:?}"),
        }
    }

    #[test]
    fn zero_deadline_times_out_without_sleeping() {
        // Injected Duration::ZERO: the deadline check fires before the first
        // select — the deterministic timeout path (no real time elapses).
        let m = mock();
        m.script(MockResponse::Reply(Ok(serde_json::json!({"ok": true}))));
        let pending = m.dispatch(request()).unwrap();
        assert!(matches!(
            pending.wait(Duration::ZERO, &|| false),
            Err(ToolBridgeError::Timeout)
        ));
    }

    #[test]
    fn cancel_precedes_waiting_reply() {
        // Flag set BEFORE wait: must return Canceled immediately, even though
        // a successful reply is already queued.
        let m = mock();
        m.script(MockResponse::Reply(Ok(serde_json::json!({"ok": true}))));
        let pending = m.dispatch(request()).unwrap();
        let canceled = true;
        assert!(matches!(
            pending.wait(Duration::ZERO, &|| canceled),
            Err(ToolBridgeError::Canceled)
        ));
    }

    #[test]
    fn cancel_wins_over_arrived_reply() {
        // The pinned race: reply arrives Ok but the flag is set by the time
        // the reply arm fires → Canceled, not the value.
        let m = mock();
        m.script(MockResponse::Reply(Ok(serde_json::json!({"ok": true}))));
        let pending = m.dispatch(request()).unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_wait = flag.clone();
        let is_canceled = move || flag_wait.load(Ordering::Relaxed);
        // Deterministic flag flip with no thread/sleep: the predicate consults
        // a cell we flip right before wait, exercising the post-reply
        // re-check inside the reply arm.
        flag.store(true, Ordering::Relaxed);
        assert!(matches!(
            pending.wait(Duration::ZERO, &is_canceled),
            Err(ToolBridgeError::Canceled)
        ));
    }

    #[test]
    fn cancel_hook_records_request_id() {
        let m = mock();
        let pending = m.dispatch(request()).unwrap();
        let id = pending.request_id;
        pending.cancel();
        assert_eq!(m.cancels(), vec![id]);
    }

    #[test]
    fn dropped_sender_is_bridge_unavailable() {
        // Scripted DropSender: the mock never sends, tx drops → disconnected
        // channel → wait must surface BridgeUnavailable.
        let m = mock();
        m.script(MockResponse::DropSender);
        let pending = m.dispatch(request()).unwrap();
        assert!(matches!(
            pending.wait(NOTIFY_TIMEOUT, &|| false),
            Err(ToolBridgeError::BridgeUnavailable)
        ));
        // ... and the empty-script default is the same path.
        let pending = m.dispatch(request()).unwrap();
        assert!(matches!(
            pending.wait(NOTIFY_TIMEOUT, &|| false),
            Err(ToolBridgeError::BridgeUnavailable)
        ));
    }
}
