//! Concrete iOS bridge (choreo-gui side): implements
//! [`choreo_daemon::tools::ios_bridge::IosToolBridge`] against the Swift host
//! (`ios/IosToolHost.swift`) over the C ABI.
//!
//! Only compiled under `#[cfg(target_os = "ios")]`: choreo-gui depends on
//! choreo-daemon only for that target (see Cargo.toml), so this cfg is also
//! the dependency gate — desktop/Android builds never compile any of this.
//! `scripts/build-ios.sh`'s zig path validates this code for
//! `aarch64-apple-ios`; the Swift file itself is compiled only on a Mac.
//!
//! Threading contract: `choreo_ios_tool_request` only ENQUEUES onto the app's
//! main queue and returns immediately; the blocking wait happens on the
//! caller's thread via [`IosToolPending::wait`]. The reply callback
//! ([`choreo_ios_tool_reply`]) runs on the main queue (Swift calls it from
//! there), reconstructs the boxed reply sender, sends exactly once, and drops
//! the slot — see the reply-slot ownership contract in the daemon module's
//! header (BINDING): Rust never frees the box before Swift replies; a late
//! reply into a disconnected channel sends `Err`-ignored and drops the slot.

use choreo_daemon::tools::ios_bridge::{
    IosToolBridge, IosToolPending, IosToolRequest, ToolBridgeError, ToolBridgeReply,
};
use crossbeam_channel::Sender;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};

// The C ABI surface the Swift host exports (same staticlib/app binary, so
// plain symbol linkage — no dlopen). Declared unsafe per edition-2024 rules.
// Plain comments (not `///`): rustdoc does not document extern blocks, so doc
// comments here warn as unused.
//
// SAFETY (callers): both string args must be valid NUL-terminated C strings
// for the duration of the call; the call returns after enqueueing (never
// blocks on the main queue). `reply_ctx` transfers ownership of the boxed
// reply sender to Swift per the reply-slot contract — it is freed ONLY by
// `choreo_ios_tool_reply`. Plain comments: rustdoc does not document extern
// blocks, so `///` here would warn as unused doc comments.
unsafe extern "C" {
    // NOTE: `request_id` is a documented addition to the subsession
    // contract's four-argument signature — without it
    // `choreo_ios_tool_cancel(request_id)` has no way to identify the
    // request, making the best-effort cancel path unimplementable (see the
    // report / IosToolHost.swift's matching NOTE).
    fn choreo_ios_tool_request(
        request_id: u64,
        name: *const c_char,
        args_json: *const c_char,
        reply_ctx: *mut c_void,
        reply_cb: extern "C" fn(*mut c_void, i32, *const c_char),
    );
    fn choreo_ios_tool_cancel(request_id: u64);
}

/// The reply callback handed to Swift. Exported so Swift could call it
/// directly by symbol too, but the normal path is the `reply_cb` function
/// pointer captured at dispatch (same function — one source of truth).
///
/// SAFETY: called by Swift exactly once per request, on the main queue, with
/// `ctx` being the pointer `dispatch` produced via `Box::into_raw`, and
/// `payload` a valid NUL-terminated C string (or null on failure) valid only
/// for the duration of this call. Status convention: 0 = success (payload is
/// JSON), nonzero = failure (payload, when non-null, is a message string).
#[unsafe(no_mangle)]
pub extern "C" fn choreo_ios_tool_reply(ctx: *mut c_void, status: i32, payload: *const c_char) {
    // A null ctx means the reply-slot contract was violated upstream; log
    // loudly and bail — there is nothing to reconstruct (and reconstructing a
    // null box would be UB).
    if ctx.is_null() {
        tracing::error!("ios bridge reply: null reply_ctx (contract violation)");
        return;
    }
    // Reconstruct the boxed one-shot sender (ownership returns to Rust here —
    // the single free of the slot, which drops after the send below).
    let sender = unsafe { Box::from_raw(ctx.cast::<Sender<ToolBridgeReply>>()) };
    let reply: ToolBridgeReply = if status == 0 {
        // Empty/null success payload = `null` JSON (the convention for tools
        // with nothing to return, e.g. clipboard_write).
        if payload.is_null() {
            Ok(serde_json::Value::Null)
        } else {
            let c = unsafe { CStr::from_ptr(payload) };
            let s = c.to_string_lossy();
            match serde_json::from_str(&s) {
                Ok(v) => Ok(v),
                // A success payload that is not JSON is a host-side bug, not
                // a tool failure — surface it as a Platform error so the
                // model sees the corruption instead of garbage. (Assigned,
                // not early-returned: the sender box must still be sent-then-
                // dropped, otherwise the receiver would see a disconnect and
                // misreport BridgeUnavailable.)
                Err(e) => Err(ToolBridgeError::Platform(format!(
                    "bridge reply payload was not valid JSON: {e}"
                ))),
            }
        }
    } else {
        let msg = if payload.is_null() {
            String::from("unknown platform error")
        } else {
            unsafe { CStr::from_ptr(payload) }
                .to_string_lossy()
                .into_owned()
        };
        Err(ToolBridgeError::Platform(msg))
    };
    // Err on send = the receiver was dropped (timeout/cancel abandonment).
    // Per the reply-slot contract this is the DESIGNED path: send is ignored
    // and the slot is freed by the drop below. No UAF (the receiver side
    // never touches the sender), no leak.
    let _ = sender.send(reply);
    tracing::trace!(status, "ios bridge reply delivered");
}

/// Swift-host-backed bridge. One instance per embedded-daemon process is
/// enough (it is stateless apart from the id counter); tools will hold it as
/// `Arc<dyn IosToolBridge>` next subsession.
#[derive(Debug, Default)]
pub struct SwiftIosToolBridge {
    /// Monotonic request-id source. Ids only need to be unique within the
    /// process (Swift keys per-request exactly-once state on them); Relaxed
    /// ordering is enough — the id never synchronizes memory.
    next_id: AtomicU64,
}

impl SwiftIosToolBridge {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IosToolBridge for SwiftIosToolBridge {
    fn dispatch(&self, request: IosToolRequest) -> Result<IosToolPending, ToolBridgeError> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // One-shot bounded(1) reply channel: exactly one value ever arrives
        // (Swift's exactly-once guarantee), so capacity 1 can never block a
        // late send either — and a dropped receiver is the abandonment path.
        let (tx, rx) = crossbeam_channel::bounded::<ToolBridgeReply>(1);
        // Box the sender as the opaque reply_ctx per the reply-slot contract.
        let reply_ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();
        // NUL-terminated C strings for the FFI. An interior NUL is impossible
        // for a tool name and a malformed-JSON edge for args — either way,
        // fail the dispatch BEFORE handing Swift a partial request, and free
        // the slot (we still own it; the contract's "Rust never frees"
        // applies only AFTER ownership transferred at the call).
        let name = match CString::new(request.name.as_str()) {
            Ok(s) => s,
            Err(e) => {
                drop(unsafe { Box::from_raw(reply_ctx.cast::<Sender<ToolBridgeReply>>()) });
                tracing::warn!(error = %e, "ios bridge dispatch: NUL in tool name");
                return Err(ToolBridgeError::Platform("tool name contained NUL".into()));
            }
        };
        let args = match CString::new(request.args_json.as_str()) {
            Ok(s) => s,
            Err(e) => {
                drop(unsafe { Box::from_raw(reply_ctx.cast::<Sender<ToolBridgeReply>>()) });
                tracing::warn!(error = %e, "ios bridge dispatch: NUL in args");
                return Err(ToolBridgeError::Platform("args contained NUL".into()));
            }
        };
        tracing::debug!(request_id, name = %request.name, "ios bridge: dispatching to Swift host");
        // SAFETY: both CStrings outlive the call; the call only enqueues and
        // returns (Swift-side contract); reply_ctx ownership transfers to
        // Swift exactly here — never freed by Rust again.
        unsafe {
            choreo_ios_tool_request(
                request_id,
                name.as_ptr(),
                args.as_ptr(),
                reply_ctx,
                choreo_ios_tool_reply,
            );
        }
        Ok(IosToolPending::new(
            request_id,
            rx,
            // Best-effort Swift-side cancel: echoes the id so the host can
            // drop a still-queued request. The caller's flag stays
            // authoritative for `wait`.
            Box::new(move || {
                unsafe { choreo_ios_tool_cancel(request_id) };
            }),
        ))
    }
}
