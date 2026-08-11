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
fn spawn_with_streaming_stderr_is_streamed_into_the_body() {
    let dir = Path::new("/tmp");
    let mut c = cmd("bash", "echo errmsg >&2", dir);
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    let (output, was_killed) = spawn_with_streaming(&mut c, 5000, tx).unwrap();
    // The channel is unbounded, so every forwarded chunk is buffered by the
    // time the tool returns — collect them all.
    let streamed: String = rx
        .try_iter()
        .map(|c| String::from_utf8_lossy(&c).into_owned())
        .collect();
    assert!(!was_killed);

    // stderr content lands in the interleaved body (returned as stdout)…
    let body = String::from_utf8_lossy(&output.stdout);
    assert!(body.contains("errmsg"), "body: {body}");
    // …and is forwarded live: stderr is streamed, not just accumulated, so a
    // tool that writes progress to stderr shows a live view matching the
    // final result.
    assert!(streamed.contains("errmsg"), "streamed: {streamed}");
}

#[test]
#[ignore]
fn spawn_with_streaming_interleaves_stdout_and_stderr() {
    let dir = Path::new("/tmp");
    // The exact interleave of the three lines is scheduling-dependent; what
    // must hold is that ALL of them appear, and each stream keeps its order.
    let mut c = cmd("bash", "echo out1; echo err1 >&2; echo out2", dir);
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    let (output, _was_killed) = spawn_with_streaming(&mut c, 5000, tx).unwrap();
    let streamed: String = rx
        .try_iter()
        .map(|c| String::from_utf8_lossy(&c).into_owned())
        .collect();

    let body = String::from_utf8_lossy(&output.stdout);
    let pos1 = body.find("out1").expect("out1 in body");
    let pos2 = body.find("out2").expect("out2 in body");
    assert!(pos1 < pos2, "stdout order preserved: {body}");
    assert!(body.contains("err1"), "stderr present in body: {body}");
    for needle in ["out1", "err1", "out2"] {
        assert!(
            streamed.contains(needle),
            "stream missing {needle}: {streamed}"
        );
    }
}

#[test]
#[ignore]
fn run_shell_streaming_final_body_matches_streamed_body() {
    let dir = Path::new("/tmp");
    let mut c = cmd("bash", "echo line1; echo err1 >&2; echo line2", dir);
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let collector = std::thread::spawn(move || {
        let mut s = String::new();
        for chunk in rx {
            s.push_str(&String::from_utf8_lossy(&chunk));
        }
        s
    });

    let result = run_shell_streaming(&mut c, "echo", 5000, tx).unwrap();
    let streamed = collector.join().unwrap();

    // The final body is exactly `$ echo\n` + the streamed body (stdout and
    // stderr interleaved, byte-identical to the live view) + the exit-code
    // footer. `format_shell_output` appends the footer as `\n` + `\nExit code: 0`
    // on top of the body's own trailing newline, hence the two `\n` before the
    // footer in the expected string. For ASCII output sanitize_transcript is a
    // no-op, so the equality is exact — this is the contract "streaming matches
    // the final output".
    assert_eq!(
        result,
        format!("$ echo\n{streamed}\n\nExit code: 0"),
        "final body must equal the streamed body"
    );
    for needle in ["line1", "err1", "line2"] {
        assert!(
            streamed.contains(needle),
            "stream missing {needle}: {streamed}"
        );
    }
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
