use std::path::Path;
use tai_daemon::execute_exec_tool;

#[test]
#[ignore]
fn echo_hello() {
    let result = execute_exec_tool(
        r#"{"command": "echo", "args": ["hello world"]}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("hello world"), "{}", result.content);
    assert!(result.content.contains("Exit code: 0"), "{}", result.content);
}

#[test]
#[ignore]
fn exit_nonzero() {
    let result = execute_exec_tool(
        r#"{"command": "sh", "args": ["-c", "exit 42"]}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.content.contains("Exit code: 42"), "{}", result.content);
}

#[test]
#[ignore]
fn working_directory() {
    let dir = std::env::temp_dir();
    let result = execute_exec_tool(
        &format!(r#"{{"command": "pwd", "workdir": "{}"}}"#, dir.display()),
        None,
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains(&dir.display().to_string()), "{}", result.content);
}

#[test]
#[ignore]
fn timeout_kills_command() {
    let result = execute_exec_tool(
        r#"{"command": "sleep", "args": ["10"], "timeout": 500}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.content.contains("timed out"), "{}", result.content);
    assert!(result.content.contains("Exit code: -1"), "{}", result.content);
}

#[test]
#[ignore]
fn path_confinement_rejects_escape() {
    let result = execute_exec_tool(
        r#"{"command": "echo", "args": ["escape"], "workdir": "/etc"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result.content.contains("outside the session working directory"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn path_confinement_allows_subdirectory() {
    let result = execute_exec_tool(
        r#"{"command": "echo", "args": ["ok"]}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
}

#[test]
#[ignore]
fn no_cwd_skips_confinement() {
    let result = execute_exec_tool(
        r#"{"command": "echo", "args": ["ok"]}"#,
        None,
    );
    assert!(!result.is_error, "expected success: {}", result.content);
}

#[test]
#[ignore]
fn stderr_output_included() {
    let result = execute_exec_tool(
        r#"{"command": "sh", "args": ["-c", "echo out && echo err >&2"]}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("out"), "{}", result.content);
    assert!(result.content.contains("err"), "{}", result.content);
}

#[test]
#[ignore]
fn invalid_json_returns_error() {
    let result = execute_exec_tool(
        r#"not json"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.is_error, "expected error: {}", result.content);
}

#[test]
#[ignore]
fn custom_args_are_passed() {
    let result = execute_exec_tool(
        r#"{"command": "sh", "args": ["-c", "echo $#; for a; do echo \"arg: $a\"; done", "--", "foo", "bar", "baz"]}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("arg: foo"), "{}", result.content);
    assert!(result.content.contains("arg: bar"), "{}", result.content);
    assert!(result.content.contains("arg: baz"), "{}", result.content);
}

#[test]
#[ignore]
fn fast_command_does_not_wait_full_timeout() {
    // The bug: even a fast command would block for the default 30s timeout
    // because the watchdog thread's sleep was non-interruptible.
    // This test verifies a 5ms command with a 10s timeout returns in < 1s.
    let start = std::time::Instant::now();
    let result = execute_exec_tool(
        r#"{"command": "echo", "args": ["fast"], "timeout": 10000}"#,
        Some(Path::new("/tmp")),
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 1000,
        "fast command took {}ms (should be << 10000ms timeout)",
        elapsed.as_millis()
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("fast"), "{}", result.content);
}
