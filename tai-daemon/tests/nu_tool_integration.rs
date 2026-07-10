use std::path::Path;
use tai_daemon::{NuArgs, execute_nu_tool};

#[test]
#[ignore]
fn echo_hello() {
    let result = execute_nu_tool(
        &NuArgs {
            command: "print 'hello world'".into(),
            workdir: None,
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("hello world"), "{}", content);
    assert!(content.contains("Exit code: 0"), "{}", content);
}

#[test]
#[ignore]
fn exit_nonzero() {
    let result = execute_nu_tool(
        &NuArgs {
            command: "exit 42".into(),
            workdir: None,
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("Exit code: 42"), "{}", content);
}

#[test]
#[ignore]
fn working_directory() {
    let dir = std::env::temp_dir();
    let result = execute_nu_tool(
        &NuArgs {
            command: "pwd".into(),
            workdir: Some(dir.display().to_string()),
            timeout: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains(&dir.display().to_string()), "{}", content);
}

#[test]
#[ignore]
fn timeout_kills_command() {
    let result = execute_nu_tool(
        &NuArgs {
            command: "sleep 10sec".into(),
            workdir: None,
            timeout: Some(500),
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("timed out"), "{}", content);
    assert!(content.contains("Exit code: -1"), "{}", content);
}

#[test]
#[ignore]
fn path_confinement_rejects_escape() {
    let result = execute_nu_tool(
        &NuArgs {
            command: "print 'escape'".into(),
            workdir: Some("/etc".into()),
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("outside the session working directory"),
        "{}",
        err
    );
}

#[test]
#[ignore]
fn path_confinement_allows_subdirectory() {
    let result = execute_nu_tool(
        &NuArgs {
            command: "print 'ok'".into(),
            workdir: None,
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    assert!(result.is_ok(), "expected success: {:?}", result);
}

#[test]
#[ignore]
fn no_cwd_skips_confinement() {
    let result = execute_nu_tool(
        &NuArgs {
            command: "print 'ok'".into(),
            workdir: None,
            timeout: None,
        },
        None,
    );
    assert!(result.is_ok(), "expected success: {:?}", result);
}

#[test]
#[ignore]
fn output_truncation() {
    let result = execute_nu_tool(
        &NuArgs {
            command: "1..99999 | each { 'x' } | str join".into(),
            workdir: None,
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("[truncated]"), "{}", content);
}

#[test]
#[ignore]
fn stderr_output_included() {
    let result = execute_nu_tool(
        &NuArgs {
            command: "print 'out'; print -e 'err'".into(),
            workdir: None,
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("out"), "{}", content);
    assert!(content.contains("err"), "{}", content);
}
