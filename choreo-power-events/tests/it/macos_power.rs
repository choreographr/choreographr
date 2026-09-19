//! macOS integration stub — COMPILE-CHECK ONLY.
//!
//! Actually exercising `IORegisterForSystemPower` requires a macOS host
//! (an Apple runner); this workspace validates on Linux, so this test
//! exists to keep the macOS-specific cfg surface honest at the type level
//! only. It deliberately does nothing at runtime and is `#[ignore]`d like
//! the rest of the integration suite. The real symbol checks are compile
//! time (see the `const _: Option<IOServiceInterestCallback>` assertion
//! in `platform/macos.rs` and the module-doc verification notes).

#![cfg(target_os = "macos")]

#[test]
#[ignore]
fn macos_power_surface_compiles() {
    // The macOS backend is exercised purely by compilation on an Apple
    // target; running anything here would register real power listeners
    // on a developer machine, which we must not do from CI.
    let monitor = choreo_power_events::PowerMonitor::inert();
    assert!(!monitor.is_active());
}
