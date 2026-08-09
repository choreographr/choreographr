use choreographr::tools::shell_util::{run_shell_streaming, spawn_with_streaming};
use choreographr::{ShArgs, execute_sh_tool};
use std::path::Path;

/// Helper: set up piped stdout/stderr on a Command for use with
/// spawn_with_streaming / run_shell_streaming.
fn cmd(program: &str, arg: &str, dir: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    c.args(["-c", arg])
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    c
}

#[test]
#[ignore]
fn spawn_with_streaming_produces_stdout() {
    let dir = Path::new("/tmp");
    let mut c = cmd("bash", "echo hello world", dir);
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    let (output, was_killed) = spawn_with_streaming(&mut c, 5000, tx).unwrap();
    drop(rx);
    assert!(!was_killed, "should not have been killed by timeout");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello world"), "stdout: {stdout}");
    assert!(output.status.success(), "exit should be 0");
}

#[test]
#[ignore]
fn spawn_with_streaming_stderr_is_accumulated() {
    let dir = Path::new("/tmp");
    let mut c = cmd("bash", "echo errmsg >&2", dir);
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    let (output, was_killed) = spawn_with_streaming(&mut c, 5000, tx).unwrap();
    drop(rx);
    assert!(!was_killed);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("errmsg"), "stderr: {stderr}");
}

#[test]
#[ignore]
fn spawn_with_streaming_timeout_kills() {
    let dir = Path::new("/tmp");
    let mut c = cmd("bash", "sleep 10", dir);
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    let (output, was_killed) = spawn_with_streaming(&mut c, 500, tx).unwrap();
    drop(rx);
    assert!(was_killed, "should have been killed by timeout");
    assert!(!output.status.success(), "should have non-zero exit");
}

#[test]
#[ignore]
fn run_shell_streaming_combines_output() {
    let dir = Path::new("/tmp");
    let mut c = cmd("bash", "echo hello", dir);
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    let result = run_shell_streaming(&mut c, "echo hello", 5000, tx).unwrap();
    drop(rx);

    assert!(result.contains("hello"), "result: {result}");
    assert!(result.contains("Exit code: 0"), "result: {result}");
}

#[test]
#[ignore]
fn run_shell_streaming_streams_lines_in_realtime() {
    let dir = Path::new("/tmp");
    let mut c = cmd("bash", "echo line1 && echo line2", dir);
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    let handle = std::thread::spawn(move || {
        // Collect streamed chunks until the sender is dropped
        let mut chunks = Vec::new();
        for chunk in rx {
            chunks.push(String::from_utf8_lossy(&chunk).to_string());
        }
        chunks
    });

    let result = run_shell_streaming(&mut c, "echo", 5000, tx).unwrap();
    let streamed = handle.join().unwrap();

    assert!(result.contains("Exit code: 0"), "result: {result}");
    // The stream should have captured each line as it arrived
    let combined: String = streamed.concat();
    assert!(combined.contains("line1"), "streamed: {combined}");
    assert!(combined.contains("line2"), "streamed: {combined}");
}

#[test]
#[ignore]
fn execute_sh_tool_non_streaming_still_works() {
    let result = execute_sh_tool(
        &ShArgs {
            command: "echo hello".into(),
            shell: choreographr::Shell::Bash,
            workdir: None,
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("hello"), "{content}");
    assert!(content.contains("Exit code: 0"), "{content}");
}
