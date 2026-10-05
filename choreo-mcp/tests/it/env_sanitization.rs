// Only meaningful where a POSIX shell exists to probe the child environment.
#![cfg(unix)]

//! Verifies the stdio child environment is sanitized: the daemon's
//! code-injection environment variables must not reach a freshly spawned MCP
//! server, even though the child would otherwise inherit the daemon's whole
//! environment.
//!
//! Marked `#[ignore = "integration"]` per AGENTS.md — it spawns an external
//! process, so it belongs in a crate-level `tests/` directory.

use crate::common;
use std::io;
use std::process::Command;

/// A code-injection variable from the stripped set (`choreo_mcp::stdio`'s
/// `INJECTION_ENV_VARS`). `PYTHONPATH` is chosen as the probe because it has no
/// side effect on a shell, unlike `LD_PRELOAD`'s dynamic-loader involvement.
const PROBE_VAR: &str = "PYTHONPATH";

/// Run `sh` with `cmd`'s settings and report whether the probe variable is set
/// in the child's environment (`unset` when it is absent).
fn read_probe_var(cmd: &mut Command) -> io::Result<String> {
    cmd.arg("-c")
        .arg(format!("printf '%s' \"${{{PROBE_VAR}:-unset}}\""));
    let out = cmd.output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
#[ignore = "integration"]
fn injection_env_var_is_stripped_from_the_child() {
    common::watchdog();

    // Control: without sanitization the variable IS visible to the child.
    let mut inherited = Command::new("sh");
    inherited.env(PROBE_VAR, "injected");
    assert_eq!(
        read_probe_var(&mut inherited).expect("spawn the probe child"),
        "injected",
        "the probe variable should be visible when it is merely inherited"
    );

    // With sanitization applied, the same variable is not visible.
    let mut sanitized = Command::new("sh");
    sanitized.env(PROBE_VAR, "injected");
    choreo_mcp::sanitize_child_env(&mut sanitized);
    assert_eq!(
        read_probe_var(&mut sanitized).expect("spawn the probe child"),
        "unset",
        "the injection variable must be stripped from the child environment"
    );
}
