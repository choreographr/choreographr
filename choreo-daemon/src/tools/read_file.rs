use super::{
    MAX_TOOL_OUTPUT_BYTES, OutputBudget, TextStream, ToolExecError, display_path_label,
    open_text_reader, render_streamed_line, resolve_path, sanitize_content, sanitize_name,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::fmt::Write as _;
use std::path::Path;
use tracing::debug;

/// Default *and* maximum number of lines one `read_file` call returns. Kept
/// well under the shared 128 KiB byte budget for ordinary source (a 2000-line
/// page of ~60-byte lines is ~120 KiB) so the byte budget stays the real
/// backstop for very long lines, while matching the ~2000-line default every
/// peer read tool ships.
const MAX_READ_FILE_LINES: usize = 2000;

/// Serde default for an omitted `start_line`: 1-based, so "no `start_line`"
/// means "from the top".
fn default_start_line() -> usize {
    1
}

/// Serde default for an omitted `max_lines`: the cap, so a caller wanting a
/// narrow window names it and omitting it means "don't make me paginate".
fn default_max_lines() -> usize {
    MAX_READ_FILE_LINES
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Relative or absolute path to a text file
    pub path: String,
    /// 1-based inclusive start line (defaults to 1)
    #[serde(default = "default_start_line")]
    pub start_line: usize,
    /// Maximum number of lines to return (1-2000; defaults to 2000)
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
}

/// Read a numbered, optionally windowed view of a UTF-8 text file.
///
/// Output is a `path:` / `lines: a-b of N` header followed by the selected
/// lines, each prefixed with its 1-based number and a ` | ` gutter. The
/// gutter is a display aid, not file content: `edit_file`'s `old_text` must
/// be the raw line text with the gutter removed.
pub(crate) fn execute_read_file_tool(
    args: &ReadFileArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }
    if args.start_line == 0 {
        return Err(ToolExecError("start_line must be >= 1".to_string()));
    }
    if args.max_lines == 0 {
        return Err(ToolExecError("max_lines must be >= 1".to_string()));
    }
    if args.max_lines > MAX_READ_FILE_LINES {
        return Err(ToolExecError(format!(
            "max_lines must be <= {MAX_READ_FILE_LINES}"
        )));
    }

    let resolved = resolve_path(&args.path, working_dir);

    // Stream through a bounded reader (see `TextStream`): memory usage is
    // capped at one line plus the output budget, no matter how large the
    // file is. Binary files (NUL in the head) are rejected up front by
    // `open_text_reader`; per-line NUL/UTF-8 checks happen in
    // `render_streamed_line` on content we actually return.
    let mut stream = TextStream::new(open_text_reader(&resolved)?);

    let start = args.start_line as u64;
    let max = args.max_lines as u64;

    let mut out = String::new();
    let mut budget = OutputBudget::new(MAX_TOOL_OUTPUT_BYTES);
    let mut lines_shown: u64 = 0;

    for line in &mut stream {
        let line = line?;
        // Only lines inside the requested window are rendered (and validated);
        // everything else — lines before the window, lines past `max_lines`,
        // and any line once the byte budget is spent — is counted only so the
        // totals stay exact.
        let in_window = line.line_number >= start && lines_shown < max;
        if !in_window || budget.is_truncated() {
            continue;
        }
        let display_line = render_streamed_line(&line, &resolved)?;
        if budget.push_line(&mut out, &display_line) {
            lines_shown += 1;
        }
    }

    let total_lines = stream.total_lines();
    let total_bytes = stream.total_bytes();
    // Sanitize the label: a hostile file name (a legal newline, a bidi
    // override) must not split the header or spoof the numbered view — the
    // same policy `grep`/`find` apply to their path labels.
    let label = sanitize_name(&display_path_label(&resolved, working_dir));

    if total_lines == 0 {
        // An empty file has no lines to number; report it rather than
        // tripping the past-EOF check below.
        return Ok(format!("path: {label}\nlines: none (empty file)\n\n"));
    }
    if start > total_lines {
        return Err(ToolExecError(format!(
            "start_line {} is past end of file; file has {} lines",
            args.start_line, total_lines
        )));
    }

    // Build the header with honest totals. When the byte budget cut us off,
    // report the number of lines actually shown; otherwise the clamped
    // requested end. (saturating_add guards against start_line near usize::MAX
    // in debug builds — the past-EOF check above already errors for those.)
    let requested_end = total_lines.min(start.saturating_add(max - 1));
    let header = if budget.is_truncated() && lines_shown == 0 {
        format!(
            "path: {label}\nlines: none of {total_lines} (first line exceeds output budget)\n\n"
        )
    } else {
        let shown_end = if budget.is_truncated() {
            start + lines_shown - 1
        } else {
            requested_end
        };
        format!("path: {label}\nlines: {start}-{shown_end} of {total_lines}\n\n")
    };

    if budget.is_truncated() {
        // Report the total bytes the caller actually receives before the
        // marker — body + prepended header + the marker's leading newline —
        // so "showing X of Y bytes" matches the returned content exactly
        // (the marker text itself is appended past the budget). The resume
        // value names the next unread line so a follow-up call is mechanical.
        let returned_bytes = budget.shown_bytes() + header.len() + 1;
        let resume = start + lines_shown;
        let _ = write!(
            out,
            "\n...[truncated: showing {returned_bytes} of {total_bytes} bytes \
             ({lines_shown} of {total_lines} lines) — continue with start_line={resume}]"
        );
    } else if requested_end < total_lines {
        // The line window (not the byte budget) cut the output: more lines
        // follow below. Name the resume line so the follow-up call is
        // mechanical — the same "continue with start_line=" contract the
        // byte-budget marker above carries.
        let resume = requested_end + 1;
        let _ = write!(
            out,
            "\n...[more lines follow: showing {lines_shown} of {total_lines} lines \
             — continue with start_line={resume}]"
        );
    }

    debug!(
        path = %resolved.display(),
        start_line = args.start_line,
        max_lines = args.max_lines,
        total_lines,
        total_bytes,
        lines_shown,
        truncated = budget.is_truncated(),
        "read_file completed"
    );

    Ok(format!("{header}{out}"))
}

pub fn describe_read_file_invocation(args: &ReadFileArgs) -> String {
    // The description is line-oriented (logs, TUI): sanitize the raw path so a
    // control character cannot split the line or inject terminal escapes.
    let path = sanitize_content(&args.path);
    if args.start_line <= 1 && args.max_lines >= MAX_READ_FILE_LINES {
        format!("Reading file `{path}`.")
    } else {
        format!(
            "Reading file `{path}` from line {} (max {} lines).",
            args.start_line, args.max_lines
        )
    }
}

pub(crate) struct ReadFile;

define_tool!(
    ReadFile,
    "read_file",
    "Read a UTF-8 text file from the local workspace as a numbered, optionally windowed view. Each line is prefixed with its 1-based number and a ` | ` gutter — the gutter is a display aid, not file content, so never include it in edit_file's old_text. Returns at most 2000 lines starting at `start_line` (default 1); rejects binary files. When the line window or the 128 KiB byte budget cuts the output, the result reports totals and the `start_line` to continue from.",
    ReadFileArgs,
    execute_read_file_tool,
    "core",
    describe_read_file_invocation
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `content` to a temp file (kept alive for the duration of the
    /// call) and run the tool against it with default args.
    fn run(content: &str) -> Result<String, ToolExecError> {
        run_bytes(content.as_bytes())
    }

    fn run_bytes(content: &[u8]) -> Result<String, ToolExecError> {
        run_args(content, 1, MAX_READ_FILE_LINES)
    }

    fn run_args(
        content: &[u8],
        start_line: usize,
        max_lines: usize,
    ) -> Result<String, ToolExecError> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content).unwrap();
        execute_read_file_tool(
            &ReadFileArgs {
                path: file.path().display().to_string(),
                start_line,
                max_lines,
            },
            None,
        )
    }

    #[test]
    fn reads_whole_file_numbered_with_header() {
        let out = run("alpha\nbeta\ngamma\n").unwrap();
        assert!(out.contains("lines: 1-3 of 3"), "{out}");
        assert!(out.contains("1 | alpha"), "{out}");
        assert!(out.contains("3 | gamma"), "{out}");
    }

    #[test]
    fn reads_file_without_trailing_newline() {
        let out = run("alpha\nbeta").unwrap();
        assert!(out.contains("lines: 1-2 of 2"), "{out}");
        assert!(out.ends_with("2 | beta\n"), "{out:?}");
    }

    #[test]
    fn normalizes_crlf_line_endings() {
        let out = run("alpha\r\nbeta\r\n").unwrap();
        assert!(out.contains("1 | alpha\n"), "{out:?}");
        assert!(!out.contains('\r'), "CRLF must be normalized: {out:?}");
    }

    #[test]
    fn empty_file_reports_empty() {
        let out = run("").unwrap();
        assert!(out.contains("lines: none (empty file)"), "{out}");
    }

    #[test]
    fn reads_numbered_line_chunk() {
        let out = run_args(b"alpha\nbeta\ngamma\ndelta\n", 2, 2).unwrap();
        assert!(out.contains("lines: 2-3 of 4"), "{out}");
        assert!(out.contains("2 | beta"), "{out}");
        assert!(out.contains("3 | gamma"), "{out}");
        assert!(!out.contains("1 | alpha"), "{out}");
        assert!(!out.contains("4 | delta"), "{out}");
    }

    #[test]
    fn clamps_to_eof() {
        let out = run_args(b"alpha\nbeta\ngamma\n", 2, 10).unwrap();
        assert!(out.contains("lines: 2-3 of 3"), "{out}");
    }

    #[test]
    fn omitted_ranges_default_to_whole_file() {
        let args: ReadFileArgs = serde_json::from_str(r#"{"path": "README.md"}"#)
            .expect("omitted range fields must default");
        assert_eq!(args.start_line, 1);
        assert_eq!(args.max_lines, MAX_READ_FILE_LINES);
    }

    #[test]
    fn only_start_line_supplied_max_lines_defaulted() {
        let args: ReadFileArgs = serde_json::from_str(r#"{"path": "x", "start_line": 5}"#).unwrap();
        assert_eq!(args.start_line, 5);
        assert_eq!(args.max_lines, MAX_READ_FILE_LINES);
    }

    #[test]
    fn rejects_missing_path() {
        let err = execute_read_file_tool(
            &ReadFileArgs {
                path: "  ".into(),
                start_line: 1,
                max_lines: MAX_READ_FILE_LINES,
            },
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing required"), "{err}");
    }

    #[test]
    fn rejects_start_line_zero() {
        let err = run_args(b"alpha\n", 0, 1).unwrap_err();
        assert!(err.to_string().contains("start_line must be >= 1"), "{err}");
    }

    #[test]
    fn rejects_max_lines_zero() {
        let err = run_args(b"alpha\n", 1, 0).unwrap_err();
        assert!(err.to_string().contains("max_lines must be >= 1"), "{err}");
    }

    #[test]
    fn rejects_excessive_max_lines() {
        let err = run_args(b"alpha\n", 1, MAX_READ_FILE_LINES + 1).unwrap_err();
        assert!(err.to_string().contains("max_lines must be <="), "{err}");
    }

    #[test]
    fn rejects_start_line_past_eof() {
        let err = run_args(b"alpha\nbeta\n", 5, 1).unwrap_err();
        assert!(err.to_string().contains("past end of file"), "{err}");
    }

    #[test]
    fn rejects_binary_file() {
        let err = run_bytes(b"\x89PNG\r\n\x1a\n\x00\x00").unwrap_err();
        assert!(err.to_string().contains("binary file"), "{err}");
    }

    #[test]
    fn rejects_nul_past_sniff_head() {
        // NUL beyond the 8 KiB head-sniff window: caught by the per-line
        // check on the returned line, not the up-front sniff.
        let mut content = vec![b'a'; 9 * 1024];
        content.push(0);
        let err = run_bytes(&content).unwrap_err();
        assert!(err.to_string().contains("binary file"), "{err}");
    }

    #[test]
    fn rejects_invalid_utf8() {
        let err = run_bytes(b"ok\n\xff\xfe").unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn rejects_invalid_utf8_past_sniff_head() {
        // Invalid bytes beyond the 8 KiB head-sniff window: the up-front sniff
        // only sees the head, so these are caught by per-line validation when
        // the line is returned.
        let mut content = vec![b'a'; 9 * 1024];
        content.extend_from_slice(b"\xff\xfe");
        let err = run_bytes(&content).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn ignores_invalid_utf8_outside_requested_window() {
        // Content outside the returned window is not validated: line 2 is
        // invalid UTF-8 but excluded, so a read starting at line 3 succeeds
        // (the documented contract).
        let mut content = vec![b'a'; 9 * 1024];
        content.extend_from_slice(b"\n\xff\xfe\nok\n");
        let out = run_args(&content, 3, 1).unwrap();
        assert!(out.contains("3 | ok"), "{out}");
        assert!(out.contains("lines: 3-3 of 3"), "{out}");
    }

    #[test]
    fn reports_totals_and_resume_line_when_truncated() {
        // 3000 lines × 100 bytes, but the final line has no trailing newline,
        // so the file is 299,999 bytes > 128 KiB budget.
        let content = (0..3000)
            .map(|i| format!("{i:>8} {}", "x".repeat(90)))
            .collect::<Vec<_>>()
            .join("\n");
        let out = run(&content).unwrap();
        assert!(out.contains("...[truncated: showing"), "{out}");
        assert!(out.contains("of 299999 bytes"), "{out}");
        assert!(out.contains("of 3000 lines)"), "{out}");
        assert!(out.contains("continue with start_line="), "{out}");
    }

    #[test]
    fn truncation_report_counts_header_bytes() {
        // The "showing X of Y bytes" figure must match the bytes actually
        // returned — body + prepended header + the marker's separator
        // newline — not just the body.
        let content = (0..3000)
            .map(|i| format!("{i:>8} {}", "y".repeat(96)))
            .collect::<Vec<_>>()
            .join("\n");
        let out = run(&content).unwrap();
        let marker_prefix = "...[truncated: showing ";
        let marker_tail = out.rsplit(marker_prefix).next().expect("marker present");
        let shown: usize = marker_tail.split(" of ").next().unwrap().parse().unwrap();
        let returned = out.len() - marker_tail.len() - marker_prefix.len();
        assert_eq!(shown, returned, "reported bytes != returned bytes: {out}");
    }

    #[test]
    fn window_cap_reports_resume_line() {
        // 3000 short lines fit under the byte budget, so the 2000-line window
        // is what cuts the output — the result must still name the resume line
        // so the follow-up call is mechanical.
        let content = (1..=3000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let out = run(&content).unwrap();
        assert!(out.contains("lines: 1-2000 of 3000"), "{out}");
        assert!(out.contains("more lines follow"), "{out}");
        assert!(out.contains("continue with start_line=2001"), "{out}");
    }

    #[test]
    fn marks_over_cap_line_truncated() {
        // A single 100 KiB line (no newline) exceeds the 64 KiB per-line cap.
        let big = "x".repeat(100 * 1024);
        let out = run(&big).unwrap();
        assert!(out.contains("...[line truncated: exceeds 64 KiB]"), "{out}");
    }

    #[test]
    fn reads_lines_after_over_cap_line() {
        // A 100 KiB line followed by a normal line: the over-cap line is
        // drained (counted, never buffered) and reading resumes cleanly.
        let mut content = vec![b'x'; 100 * 1024];
        content.extend_from_slice(b"\nbeta\n");
        let out = run_bytes(&content).unwrap();
        assert!(out.contains("...[line truncated: exceeds 64 KiB]"), "{out}");
        assert!(out.contains("2 | beta"), "{out}");
    }

    #[test]
    fn describe_invocation_default_and_ranged() {
        let whole = ReadFileArgs {
            path: "src/main.rs".into(),
            start_line: 1,
            max_lines: MAX_READ_FILE_LINES,
        };
        assert_eq!(
            super::describe_read_file_invocation(&whole),
            "Reading file `src/main.rs`."
        );
        let ranged = ReadFileArgs {
            path: "src/lib.rs".into(),
            start_line: 10,
            max_lines: 50,
        };
        assert_eq!(
            super::describe_read_file_invocation(&ranged),
            "Reading file `src/lib.rs` from line 10 (max 50 lines)."
        );
    }
}
