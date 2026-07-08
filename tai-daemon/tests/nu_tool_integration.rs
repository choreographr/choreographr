use std::path::Path;
use tai_daemon::execute_nu_tool;

#[test]
#[ignore]
fn echo_hello() {
    let result = execute_nu_tool(
        r#"{"command": "print 'hello world'"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("hello world"), "{}", result.content);
    assert!(
        result.content.contains("Exit code: 0"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn exit_nonzero() {
    let result = execute_nu_tool(r#"{"command": "exit 42"}"#, Some(Path::new("/tmp")));
    assert!(
        result.content.contains("Exit code: 42"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn working_directory() {
    let dir = std::env::temp_dir();
    let result = execute_nu_tool(
        &format!(r#"{{"command": "pwd", "workdir": "{}"}}"#, dir.display()),
        None,
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(
        result.content.contains(&dir.display().to_string()),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn timeout_kills_command() {
    let result = execute_nu_tool(
        r#"{"command": "sleep 10sec", "timeout": 500}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.content.contains("timed out"), "{}", result.content);
    assert!(
        result.content.contains("Exit code: -1"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn path_confinement_rejects_escape() {
    let result = execute_nu_tool(
        r#"{"command": "print 'escape'", "workdir": "/etc"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result
            .content
            .contains("outside the session working directory"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn path_confinement_allows_subdirectory() {
    let result = execute_nu_tool(r#"{"command": "print 'ok'"}"#, Some(Path::new("/tmp")));
    assert!(!result.is_error, "expected success: {}", result.content);
}

#[test]
#[ignore]
fn no_cwd_skips_confinement() {
    let result = execute_nu_tool(r#"{"command": "print 'ok'"}"#, None);
    assert!(!result.is_error, "expected success: {}", result.content);
}

#[test]
#[ignore]
fn output_truncation() {
    let result = execute_nu_tool(
        r#"{"command": "1..99999 | each { 'x' } | str join"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.content.contains("[truncated]"), "{}", result.content);
}

#[test]
#[ignore]
fn stderr_output_included() {
    let result = execute_nu_tool(
        r#"{"command": "print 'out'; print -e 'err'"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("out"), "{}", result.content);
    assert!(result.content.contains("err"), "{}", result.content);
}

#[test]
#[ignore]
fn invalid_json_returns_error() {
    let result = execute_nu_tool(r#"not json"#, Some(Path::new("/tmp")));
    assert!(result.is_error, "expected error: {}", result.content);
}
