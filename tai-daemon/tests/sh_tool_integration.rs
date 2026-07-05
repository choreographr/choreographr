use std::path::Path;
use tai_daemon::execute_sh_tool;

const SHELL: &str = "bash";

#[test]
#[ignore]
fn echo_hello() {
    let result = execute_sh_tool(
        &format!(r#"{{"command": "echo hello world", "shell": "{SHELL}"}}"#),
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("hello world"), "{}", result.content);
    assert!(result.content.contains("Exit code: 0"), "{}", result.content);
}

#[test]
#[ignore]
fn exit_nonzero() {
    let result = execute_sh_tool(
        &format!(r#"{{"command": "exit 42", "shell": "{SHELL}"}}"#),
        Some(Path::new("/tmp")),
    );
    assert!(result.content.contains("Exit code: 42"), "{}", result.content);
}

#[test]
#[ignore]
fn working_directory() {
    let dir = std::env::temp_dir();
    let result = execute_sh_tool(
        &format!(r#"{{"command": "pwd", "shell": "{SHELL}", "workdir": "{}"}}"#, dir.display()),
        None,
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains(&dir.display().to_string()), "{}", result.content);
}

#[test]
#[ignore]
fn timeout_kills_command() {
    let result = execute_sh_tool(
        &format!(r#"{{"command": "sleep 10", "shell": "{SHELL}", "timeout": 500}}"#),
        Some(Path::new("/tmp")),
    );
    assert!(result.content.contains("timed out"), "{}", result.content);
    assert!(result.content.contains("Exit code: -1"), "{}", result.content);
}

#[test]
#[ignore]
fn path_confinement_rejects_escape() {
    let result = execute_sh_tool(
        &format!(r#"{{"command": "echo escape", "shell": "{SHELL}", "workdir": "/etc"}}"#),
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
    let result = execute_sh_tool(
        &format!(r#"{{"command": "echo ok", "shell": "{SHELL}"}}"#),
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
}

#[test]
#[ignore]
fn no_cwd_skips_confinement() {
    let result = execute_sh_tool(
        &format!(r#"{{"command": "echo ok", "shell": "{SHELL}"}}"#),
        None,
    );
    assert!(!result.is_error, "expected success: {}", result.content);
}

#[test]
#[ignore]
fn output_truncation() {
    let result = execute_sh_tool(
        &format!(r#"{{"command": "head -c 100000 /dev/zero | tr '\\0' 'x'", "shell": "{SHELL}"}}"#),
        Some(Path::new("/tmp")),
    );
    assert!(result.content.contains("[truncated]"), "{}", result.content);
}

#[test]
#[ignore]
fn stderr_output_included() {
    let result = execute_sh_tool(
        &format!(r#"{{"command": "echo out && echo err >&2", "shell": "{SHELL}"}}"#),
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("out"), "{}", result.content);
    assert!(result.content.contains("err"), "{}", result.content);
}

#[test]
#[ignore]
fn invalid_json_returns_error() {
    let result = execute_sh_tool(
        r#"not json"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.is_error, "expected error: {}", result.content);
}

#[test]
#[ignore]
fn missing_shell_returns_error() {
    let result = execute_sh_tool(
        r#"{"command": "echo test"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.is_error, "expected error for missing shell: {}", result.content);
}
