use choreo_daemon::tools::shell_util::{run_shell_streaming, spawn_with_streaming};
use choreo_daemon::{ShArgs, execute_sh_tool};
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
            shell: choreo_daemon::Shell::Bash,
            workdir: None,
            timeout: None,
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("hello"), "{content}");
    assert!(content.contains("Exit code: 0"), "{content}");
}

#[test]
#[ignore]
fn spawn_with_streaming_caps_total_forwarded_bytes() {
    // A chatty command must not push an unbounded live view: the streamed
    // total is capped at MAX_TOOL_OUTPUT_BYTES with one `...[truncated]`
    // marker, and the accumulated stdout copy is capped the same way.
    // Integration test (spawns a real `sh` subprocess) — see AGENTS.md test
    // discipline; moved here from the in-crate unit suite.
    let mut cmd = std::process::Command::new("sh");
    // ~400 KiB of stdout across 5000 lines.
    cmd.args([
        "-c",
        "i=0; while [ $i -lt 5000 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n'; i=$((i+1)); done",
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());

    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (output, _was_killed) = spawn_with_streaming(&mut cmd, 30_000, output_tx).expect("spawn");

    let mut forwarded = 0usize;
    let mut marker_count = 0usize;
    for chunk in output_rx {
        if chunk.as_slice() == b"\n...[truncated]" {
            marker_count += 1;
        }
        forwarded += chunk.len();
    }

    assert_eq!(
        marker_count, 1,
        "truncation marker must be sent exactly once"
    );
    assert!(
        forwarded <= choreo_sanitize::MAX_TOOL_OUTPUT_BYTES + b"\n...[truncated]".len(),
        "streamed total must not exceed cap + one marker: {forwarded}"
    );
    assert!(
        output.stdout.len() <= choreo_sanitize::MAX_TOOL_OUTPUT_BYTES,
        "accumulated stdout must be capped: {}",
        output.stdout.len()
    );
    assert!(
        forwarded > 0,
        "the command's output must actually be streamed"
    );
}
