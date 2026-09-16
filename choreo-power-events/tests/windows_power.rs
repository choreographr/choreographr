//! Windows integration stub — COMPILE-CHECK ONLY.
//!
//! Registering the real suspend/resume callback requires a Windows host and
//! would touch a real system resource; this workspace validates on Linux, so
//! this test keeps the Windows cfg surface honest at the type level only. It
//! deliberately does nothing at runtime and is `#[ignore]`d like the rest of
//! the integration suite. The real symbol checks are compile time (see the
//! `const _: PDEVICE_NOTIFY_CALLBACK_ROUTINE` assertion in
//! `platform/windows.rs`).

#![cfg(target_os = "windows")]

#[test]
// A reason is required by clippy's pedantic `ignore_without_reason`; the
// macOS stub's bare `#[ignore]` never trips it because that file is cfg'd
// out of every non-Apple clippy run, but the Windows stub IS compiled when
// clippy targets x86_64-pc-windows-msvc.
#[ignore = "compile-check only; not runnable without a Windows host"]
fn windows_power_surface_compiles() {
    // The Windows backend is exercised purely by compilation on a Windows
    // target; running anything here would register a real power listener on
    // a developer machine, which we must not do from CI.
    let monitor = choreo_power_events::PowerMonitor::inert();
    assert!(!monitor.is_active());
}
