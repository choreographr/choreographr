//! Windows backend: suspend/resume notifications via
//! `RegisterSuspendResumeNotification` with a `DEVICE_NOTIFY_CALLBACK`.
//!
//! # Mechanism
//!
//! `RegisterSuspendResumeNotification` (user32, Windows 8+) with the
//! `DEVICE_NOTIFY_CALLBACK` flag takes a `DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS`
//! whose `Callback` Windows invokes **on a system thread** (not ours) with
//! the `WM_POWERBROADCAST` code. `PBT_APMSUSPEND` arrives just before the
//! machine sleeps; the `PBT_APMRESUMEAUTOMATIC` / `PBT_APMRESUMESUSPEND` /
//! `PBT_APMRESUMECRITICAL` family arrives after resume. We forward those to
//! the crossbeam channel exactly like the other backends.
//!
//! # Threading
//!
//! Unlike Linux/macOS this backend needs NO dedicated monitor thread: the
//! producer is the system thread Windows invokes the callback on. The
//! registration's context and its `DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS` are
//! therefore LEAKED so they outlive the process's registration (the same
//! deliberate leak the macOS backend uses for its run-loop context). The
//! callback must return promptly, so it only does a non-blocking channel
//! `send` — all real work happens on the consumer's thread.
//!
//! # One registration per process
//!
//! The leak has a consequence: every successful `PowerMonitor::new()` on
//! Windows permanently leaks one context + channel and one OS registration
//! (`UnregisterSuspendResumeNotification` is deliberately never called —
//! reclaiming them would need the handle to outlive a monitor `Drop`, which
//! buys nothing). Create the monitor ONCE per process (as the daemon does,
//! via `PowerMonitor::best_effort` at startup) and never re-create it; a
//! second monitor's callback would only send into a dropped receiver anyway
//! (harmless, one debug log per power event).

use std::ffi::c_void;

use windows_sys::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PDEVICE_NOTIFY_CALLBACK_ROUTINE,
    RegisterSuspendResumeNotification,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMECRITICAL, PBT_APMRESUMESUSPEND,
    PBT_APMSUSPEND,
};

use crate::{PowerMonitor, PowerMonitorError, SuspendEvent};

/// `ERROR_SUCCESS` — the value a power callback returns "handled".
const ERROR_SUCCESS: u32 = 0;

/// State shared with the system-thread callback. Leaked by `spawn_monitor`
/// (see the module docs): the callback dereferences it for the lifetime of
/// the process's registration.
struct PowerContext {
    /// Crossbeam producer; `send` fails once the consumer drops the
    /// receiver, which is the monitor's (and process's) exit signal.
    sender: crossbeam_channel::Sender<SuspendEvent>,
}

/// The power-broadcast callback. Runs on a system thread.
unsafe extern "system" fn power_callback(
    context: *const c_void,
    r#type: u32,
    _setting: *const c_void,
) -> u32 {
    // SAFETY: `context` is the leaked `PowerContext` we registered; it
    // outlives the registration (leaked by design).
    let context = unsafe { &*context.cast::<PowerContext>() };
    match r#type {
        PBT_APMSUSPEND => {
            tracing::info!("system is about to suspend (WM_POWERBROADCAST PBT_APMSUSPEND)");
            // Send BEFORE the machine sleeps (the callback is delivered
            // pre-suspend). Non-blocking; a dropped receiver just logs.
            if context.sender.send(SuspendEvent::Sleep).is_err() {
                tracing::debug!("power-event receiver dropped; Windows monitor winding down");
            }
        }
        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL => {
            tracing::info!("system has resumed (WM_POWERBROADCAST resume)");
            if context.sender.send(SuspendEvent::Wake).is_err() {
                tracing::debug!("power-event receiver dropped; Windows monitor winding down");
            }
        }
        // Other power broadcasts are not ones we registered interest in;
        // ignoring them is the correct no-op.
        _ => {}
    }
    ERROR_SUCCESS
}

/// Register the suspend/resume callback and return an active monitor.
///
/// Registration happens HERE (on the caller's thread) so a failure can be
/// logged before degrading to the inert monitor, exactly like the macOS
/// backend's registration step.
///
/// Call at most ONCE per process: a successful registration is never
/// unregistered and its context is deliberately leaked so the system-thread
/// callback stays valid (see the module docs' "One registration per
/// process"); repeated calls simply multiply that leak.
// `Result` is intentional even though this backend never returns `Err`: it
// keeps the platform modules' contract uniform (`crate::platform::spawn_monitor`
// dispatches on `windows::spawn_monitor()` exactly like the Linux/macOS arms),
// and the registration-failure path mirrors macOS by logging + degrading to
// the inert monitor rather than surfacing an error. Silencing the wrap lint
// here is narrower than dropping the `Result` and special-casing the Windows
// arm at the dispatch site.
#[allow(clippy::unnecessary_wraps)]
pub fn spawn_monitor() -> Result<PowerMonitor, PowerMonitorError> {
    // Unbounded, like the other backends: events are human-rate.
    let (sender, receiver) = crossbeam_channel::unbounded();

    // Both structures must outlive the registration: the callback
    // dereferences the context on a system thread for the process's life,
    // and Windows keeps the params pointer. Leak both (the macOS backend
    // leaks its context the same way).
    let context = Box::into_raw(Box::new(PowerContext { sender }));
    let params = Box::into_raw(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(power_callback),
        Context: context.cast(),
    }));

    // SAFETY: `params` points to a live DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS
    // that stays alive for the process (leaked); DEVICE_NOTIFY_CALLBACK
    // tells Windows to treat the recipient as that struct and invoke its
    // Callback. The returned handle is NULL on failure.
    let handle =
        unsafe { RegisterSuspendResumeNotification(params.cast(), DEVICE_NOTIFY_CALLBACK) };
    if handle == 0 {
        let error = std::io::Error::last_os_error();
        // Registration failed, so Windows never received `params` and will
        // never dereference either pointer — reclaim both boxes instead of
        // leaking them.
        // SAFETY: both pointers came from Box::into_raw just above and were
        // never shared with Windows (registration failed).
        unsafe {
            drop(Box::from_raw(params));
            drop(Box::from_raw(context));
        }
        tracing::warn!(
            %error,
            "RegisterSuspendResumeNotification failed; falling back to inert monitor"
        );
        return Ok(PowerMonitor::inert());
    }

    tracing::info!("subscribed to Windows suspend/resume notifications");
    Ok(PowerMonitor {
        events: receiver,
        // The system-thread callback owns the only Sender, via the leaked
        // PowerContext; the monitor holds no producing half itself.
        sender: None,
        // Registered with the power manager: the real thing.
        active: true,
    })
}

// Compile-check the callback against the windows-sys callback type it must
// interoperate with (mirrors the macOS backend's `const _` assertion).
// NOTE: windows-sys already declares `PDEVICE_NOTIFY_CALLBACK_ROUTINE` as
// `Option<unsafe extern "system" fn(..)>`, so the alias IS the optional fn
// type — wrapping it in another `Option` (as the macOS analogue does for the
// io-kit-sys `IOServiceInterestCallback` fn pointer) would not compile.
const _: PDEVICE_NOTIFY_CALLBACK_ROUTINE = Some(power_callback);
