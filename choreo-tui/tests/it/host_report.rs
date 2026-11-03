//! End-to-end check of the hosting workspace manager reporter.
//!
//! The reporter's command-line shape is the whole contract with the host, and
//! the only honest way to check it is to let it run: this suite points the
//! reporting at a stand-in host binary (a shell script that records its argv)
//! and then asserts what it was called with. That means real processes and real
//! files, so it is part of the `#[ignore]` integration suite per AGENTS.md, not
//! a `src/` unit test.

#![cfg(unix)]

use choreo_tui::host_report::{HostReport, HostState, Reporter, ReporterConfig};
use std::fs;
use std::os::unix::fs::PermissionsExt;

/// The `--source` this program claims, and the `--agent` name it reports.
const SOURCE: &str = "choreo-tui";
const AGENT: &str = "Choreographr";

/// One recorded invocation: the arguments the stand-in host was started with.
struct Invocation<'a> {
    /// The recorded arguments, in order.
    args: Vec<&'a str>,
}

impl<'a> Invocation<'a> {
    /// Take the fixed leading words, returning the sequence number they carried
    /// and the remaining words.
    ///
    /// `expected` is the exact prefix, except that a `"<seq>"` placeholder
    /// stands for a numeric sequence number, which this returns so the test can
    /// compare two invocations.
    fn split_prefix(&mut self, expected: &[&str]) -> (u64, Vec<&'a str>) {
        let mut seq = 0;
        for want in expected {
            assert!(
                !self.args.is_empty(),
                "recorded argv is shorter than {expected:?}"
            );
            let Some(got) = self.args.first().copied() else {
                return (seq, Vec::new());
            };
            self.args.remove(0);
            if *want == "<seq>" {
                seq = got.parse::<u64>().unwrap_or(0);
            } else {
                assert_eq!(got, *want, "recorded argv mismatch: {:?}", self.args);
            }
        }
        // The remaining words, drained off so the caller can assert the tail.
        (seq, self.args.clone())
    }
}

#[test]
#[ignore = "integration: spawns a stand-in host binary and writes a real file"]
fn reporter_reports_one_state_change_and_releases_on_exit() {
    let dir = std::env::temp_dir().join(format!(
        "choreo-tui-host-report-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("temp dir create");
    let log = dir.join("argv.log");
    let script = dir.join("fake-host.sh");

    // The stand-in host: one `INVOCATION` marker line per call, then one line
    // per argument — so the assertions can tell where an argv starts, see a
    // bare `--` as its own argument, and see the resume words after it.
    let script_body = format!(
        "#!/bin/sh\n{{ printf 'INVOCATION\\n'; for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done; }} >> '{}'\n",
        log.display()
    );
    fs::write(&script, script_body).expect("script write");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script is executable");

    let config = ReporterConfig::new(script.to_string_lossy(), "pane-7", "choreo-tui");
    let reporter = Reporter::start(config).expect("a configured reporter starts");
    reporter.sink().publish(HostReport {
        state: HostState::Working,
        message: Some("read_file".to_string()),
        session_id: Some(42),
    });
    // Teardown is the release: it blocks until the serving thread has run the
    // release command and exited, so the recording is complete on return.
    reporter.shutdown();

    let recorded = fs::read_to_string(&log).expect("the stand-in host recorded its argv");
    let mut invocations: Vec<Invocation<'_>> = split_invocations(&recorded)
        .into_iter()
        .map(|args| Invocation { args })
        .collect();
    assert_eq!(
        invocations.len(),
        2,
        "one report then one release: {recorded:?}"
    );

    let (report_seq, tail) = invocations
        .get_mut(0)
        .expect("a report was recorded")
        .split_prefix(&[
            "pane",
            "report-agent",
            "pane-7",
            "--source",
            SOURCE,
            "--agent",
            AGENT,
            "--state",
            "working",
            "--seq",
            "<seq>",
        ]);
    assert!(report_seq > 0, "the sequence is a millisecond timestamp");
    assert_eq!(
        tail,
        vec![
            "--message",
            "read_file",
            "--agent-session-id",
            "42",
            "--",
            "choreo-tui",
            "--attach",
            "42",
        ],
        "the message followed by the resume command after a bare `--`"
    );

    let (release_seq, tail) = invocations
        .get_mut(1)
        .expect("a release was recorded")
        .split_prefix(&[
            "pane",
            "release-agent",
            "pane-7",
            "--source",
            SOURCE,
            "--agent",
            AGENT,
            "--seq",
            "<seq>",
        ]);
    assert!(
        release_seq > report_seq,
        "the release sequence must increase past the report's ({report_seq} -> {release_seq})"
    );
    assert!(
        tail.is_empty(),
        "a release names no session and no message: {tail:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Split the recorded file into one argument list per invocation.
fn split_invocations(recorded: &str) -> Vec<Vec<&str>> {
    let mut invocations: Vec<Vec<&str>> = Vec::new();
    for line in recorded.lines() {
        if line == "INVOCATION" {
            invocations.push(Vec::new());
        } else if let Some(current) = invocations.last_mut() {
            current.push(line);
        }
    }
    invocations
}
