use choreo_daemon::{ShArgs, Shell, execute_sh_tool};
use std::path::Path;

const SHELL: Shell = Shell::Bash;

#[test]
#[ignore]
fn echo_hello() {
    let result = execute_sh_tool(
        &ShArgs {
            command: "echo hello world".into(),
            shell: SHELL,
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
    let result = execute_sh_tool(
        &ShArgs {
            command: "exit 42".into(),
            shell: SHELL,
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
    // `pwd` prints the physical (symlink-resolved) directory, so on macOS the
    // expected path must be canonicalized (/var → /private/var).
    let dir = std::env::temp_dir().canonicalize().unwrap();
    let result = execute_sh_tool(
        &ShArgs {
            command: "pwd".into(),
            shell: SHELL,
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
    let result = execute_sh_tool(
        &ShArgs {
            command: "sleep 10".into(),
            shell: SHELL,
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
fn output_truncation() {
    let result = execute_sh_tool(
        &ShArgs {
            command: "head -c 200000 /dev/zero | tr '\\0' 'x'".into(),
            shell: SHELL,
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
    let result = execute_sh_tool(
        &ShArgs {
            command: "echo out && echo err >&2".into(),
            shell: SHELL,
            workdir: None,
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("out"), "{}", content);
    assert!(content.contains("err"), "{}", content);
}
