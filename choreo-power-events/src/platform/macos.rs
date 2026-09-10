//! macOS backend: IOKit system-power interest notifications on a CFRunLoop.
//!
//! # Symbol verification notes (checked against the vendored crates)
//!
//! `io-kit-sys` 0.5.0 links the IOKit framework (its build.rs emits
//! `cargo:rustc-link-lib=framework=IOKit`) and exports
//! `IONotificationPortCreate` / `IONotificationPortGetRunLoopSource` /
//! `IONotificationPortDestroy` / `IOObjectRelease` plus the types
//! `io_connect_t`, `io_object_t`, `IONotificationPortRef` and
//! `IOServiceInterestCallback`. It does NOT export the IOPMLib power
//! symbols `IORegisterForSystemPower` / `IOAllowPowerChange`, so those two
//! are declared here verbatim from
//! `IOKit/pwr_mgt/IOPMLib.h`:
//!
//! ```c
//! mach_port_t IORegisterForSystemPower(void *refCon,
//!     IONotificationPortRef *thePortRef,
//!     IOServiceInterestCallback callback,
//!     io_connect_t *notifier);
//! kern_return_t IOAllowPowerChange(io_connect_t rootPort,
//!     long notificationID);
//! ```
//!
//! Run-loop plumbing (`CFRunLoopGetCurrent`, `CFRunLoopAddSource`,
//! `CFRunLoopRun`, `CFRunLoopStop`, `kCFRunLoopCommonModes`) comes from
//! `core-foundation` 0.10 / `core-foundation-sys` 0.8, whose signatures
//! were checked in the vendored sources.
//!
//! # Sleep acknowledgement (REQUIRED)
//!
//! The interest callback runs ON the run-loop thread. For
//! `kIOMessageSystemWillSleep` the callback MUST call `IOAllowPowerChange`
//! with the notification id carried in `messageArgument` — returning
//! without acknowledging blocks the whole system from sleeping. We send
//! [`SuspendEvent::Sleep`] first, then acknowledge.
//!
//! # Threading
//!
//! Everything (registration, run loop, callback, channel send) happens on
//! the dedicated monitor thread; the crossbeam channel is the only thing
//! crossing to other threads. The context struct is leaked (the thread is
//! daemon-like — see `PowerMonitor::drop`) and its `root_port` field is
//! written once on the run-loop thread between registration and
//! `CFRunLoopRun`, before any callback can fire (callbacks are only
//! dispatched from the run loop itself).

use super::spawn_thread;
use crate::{PowerMonitor, PowerMonitorError, SuspendEvent};

use core_foundation::runloop::{CFRunLoop, CFRunLoopSource};
use core_foundation_sys::runloop::kCFRunLoopCommonModes;
use io_kit_sys::types::{io_connect_t, io_object_t};
use io_kit_sys::{
    IONotificationPortDestroy, IONotificationPortGetRunLoopSource, IONotificationPortRef,
    IOServiceInterestCallback,
};
use std::cell::Cell;
use std::ffi::{c_long, c_void};
use std::ptr;

// Note: core-foundation's `CFStringRef`/`CFRunLoopMode` is a direct
// re-export of `core_foundation_sys`'s, so the raw `kCFRunLoopCommonModes`
// token is passed to `add_source` without any bridging casts.

/// `kIOMessageSystemWillSleep` (IOKit/pwr_mgt/IOPMLib.h): the system WILL
/// sleep; the listener must acknowledge with `IOAllowPowerChange` or the
/// sleep is blocked system-wide.
const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xe000_0280;
/// `kIOMessageSystemHasPoweredOn` (same header): the system has just
/// resumed.
const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xe000_0300;

// IOPMLib symbols absent from io-kit-sys 0.5 (see module docs). `c_long`
// matches IOPMLib's `long notificationID` on both 64-bit Apple ABIs.
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        the_port_ref: *mut IONotificationPortRef,
        callback: IOServiceInterestCallback,
        notifier: *mut io_connect_t,
    ) -> io_object_t;

    fn IOAllowPowerChange(root_port: io_connect_t, notification_id: c_long);
}

/// State shared between the run-loop callback and the run-loop owner —
/// both of which are the SAME monitor thread, so a `Cell` (no locking, no
/// atomics) is sufficient and keeps us inside the crate's message-passing
/// rules (the only cross-thread object is the channel `Sender`).
struct PowerContext {
    /// Crossbeam producer; `send` fails once the consumer drops the
    /// receiver, which is the monitor's exit signal.
    sender: crossbeam_channel::Sender<SuspendEvent>,
    /// The root power port returned by `IORegisterForSystemPower` — an
    /// out-parameter, hence the one-time write through `Cell`.
    root_port: Cell<io_connect_t>,
}

/// The IOKit interest callback. Runs on the monitor (run-loop) thread.
// Signature must match io-kit-sys's `IOServiceInterestCallback` exactly
// (note: it returns `()`, not kern_return_t — the const check at the bottom
// of this file proves that at compile time).
unsafe extern "C" fn power_interest_callback(
    refcon: *mut c_void,
    _service: io_object_t,
    message_type: u32,
    message_argument: *mut c_void,
) {
    // SAFETY: `refcon` is the leaked `PowerContext` we registered with
    // `IORegisterForSystemPower`; it outlives the run loop (leaked by
    // design). `message_argument` for the two messages we handle carries
    // the notification id as a `long` by value (IOPMLib.h).
    let context = unsafe { &*(refcon.cast::<PowerContext>()) };
    let notification_id = message_argument as c_long;
    match message_type {
        K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
            tracing::info!("system is about to suspend (IOKit kIOMessageSystemWillSleep)");
            // Send the event BEFORE acknowledging: the consumer gets the
            // Sleep event while the machine is still awake.
            if context.sender.send(SuspendEvent::Sleep).is_err() {
                tracing::debug!("power-event receiver dropped; macOS monitor winding down");
            }
            // REQUIRED: without this the system sleep is blocked for
            // everyone. The root port is guaranteed set — callbacks cannot
            // fire before CFRunLoopRun, which is entered only after
            // registration populated it.
            unsafe {
                IOAllowPowerChange(context.root_port.get(), notification_id);
            }
        }
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => {
            tracing::info!("system has resumed (IOKit kIOMessageSystemHasPoweredOn)");
            if context.sender.send(SuspendEvent::Wake).is_err() {
                tracing::debug!("power-event receiver dropped; macOS monitor winding down");
            }
        }
        // Other power messages (canSystemSleep, willPowerOn, …) are not
        // subscribed to us for; ignoring them is the correct no-op.
        _ => {}
    }
}

/// Register an IOKit system-power listener and spin its CFRunLoop on the
/// dedicated monitor thread.
pub fn spawn_monitor() -> Result<PowerMonitor, PowerMonitorError> {
    // Unbounded, like the other backends: events are human-rate.
    let (sender, receiver) = crossbeam_channel::unbounded();
    // Context is deliberately leaked: the run loop dereferences it from
    // the callback for the lifetime of the (daemon-like) thread, which
    // outlives this constructor and has no synchronous teardown.
    let context = Box::into_raw(Box::new(PowerContext {
        sender,
        root_port: Cell::new(0),
    }));

    let register_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: all pointers are valid IOKit out-params allocated in this
        // scope; the callback is a `unsafe extern "C" fn` of the exact
        // `IOServiceInterestCallback` signature from io-kit-sys.
        unsafe {
            let mut notification_port: IONotificationPortRef = ptr::null_mut();
            let mut root_port: io_connect_t = 0;
            let port = IORegisterForSystemPower(
                context.cast(),
                &mut notification_port,
                power_interest_callback,
                &mut root_port,
            );
            (port, notification_port, root_port)
        }
    }));
    let (port, notification_port, root_port) = match register_result {
        Ok(triple) => triple,
        Err(_) => {
            // IOKit calls are plain C and should not unwind, but a panic
            // here must not take the daemon down — degrade to inert.
            tracing::error!("IORegisterForSystemPower panicked; falling back to inert monitor");
            drop(unsafe { Box::from_raw(context) });
            return Ok(PowerMonitor::inert());
        }
    };
    // MACH_PORT_NULL is 0: registration failed. Free the notification port
    // we may have been handed and fall back.
    if port == 0 {
        tracing::warn!("IORegisterForSystemPower failed; falling back to inert monitor");
        if !notification_port.is_null() {
            // SAFETY: a valid IONotificationPortRef (or null) from above.
            unsafe { IONotificationPortDestroy(notification_port) };
        }
        drop(unsafe { Box::from_raw(context) });
        return Ok(PowerMonitor::inert());
    }

    // Record the root port before the run loop can deliver any callback.
    unsafe { (*context).root_port.set(root_port) };

    spawn_thread("power-monitor-iokit", move || {
        // SAFETY: `notification_port` is the valid port from registration.
        let raw_source = unsafe { IONotificationPortGetRunLoopSource(notification_port) };
        // Wrap the (get-rule, not owned) source so the CFRunLoop API can
        // add it; `wrap_under_get_rule` does NOT take ownership, matching
        // IOPMLib's semantics (the source lives as long as the port).
        let source = unsafe { CFRunLoopSource::wrap_under_get_rule(raw_source) };
        let run_loop = CFRunLoop::get_current();
        run_loop.add_source(&source, kCFRunLoopCommonModes);
        tracing::info!("subscribed to IOKit system power notifications on the monitor run loop");
        // Runs "forever" (until the process exits or the run loop's sources
        // invalidate). There is no synchronous stop IOKit exposes for this
        // registration, which is why Drop is best-effort by design.
        CFRunLoop::run_current();
        tracing::debug!("IOKit power monitor run loop returned");
    })?;

    Ok(PowerMonitor {
        events: receiver,
        // The sender moves into the monitor thread (via the leaked
        // PowerContext); `None` here means PowerMonitor holds no producing
        // half itself.
        sender: None,
        // Registered with the power manager: the real thing.
        active: true,
    })
}

// Compile-check the IOPMLib extern declarations against the io-kit-sys
// types they must interoperate with (see the module-doc verification notes).
const _: Option<IOServiceInterestCallback> = Some(power_interest_callback);
