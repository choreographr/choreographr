use super::{
    MAX_TOOL_OUTPUT_BYTES, OutputBudget, TextStream, ToolExecError, confine_path, open_text_reader,
    render_streamed_line,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use tracing::debug;

/// Maximum number of lines read_file_range returns per call. Deliberately
/// small enough that a full page fits well under the shared byte budget
/// while keeping round-trips low on large files; the byte budget is the
/// real backstop for very long lines.
const MAX_READ_FILE_RANGE_LINES: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileRangeArgs {
    /// Relative or absolute path to a text file
    pub path: String,
    /// 1-based inclusive start line
    pub start_line: usize,
    /// Maximum number of lines to return (1-500)
    pub max_lines: usize,
}

pub(crate) fn execute_read_file_range_tool(
    args: &ReadFileRangeArgs,
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
    if args.max_lines > MAX_READ_FILE_RANGE_LINES {
        return Err(ToolExecError(format!(
            "max_lines must be <= {MAX_READ_FILE_RANGE_LINES}"
        )));
    }

    let resolved = confine_path(&args.path, working_dir)?;

    // Stream through a bounded reader (see read_file.rs for the rationale):
    // we hold at most one capped line plus the output budget in memory, and
    // binary files are rejected up front by `open_text_reader`.
    let mut stream = TextStream::new(open_text_reader(&resolved)?);

    let start = args.start_line as u64;
    let max = args.max_lines as u64;

    let mut out = String::new();
    let mut budget = OutputBudget::new(MAX_TOOL_OUTPUT_BYTES);
    let mut lines_shown: u64 = 0;

    for line in &mut stream {
        let line = line?;
        let in_window = line.line_number >= start && lines_shown < max;
        if !in_window || budget.is_truncated() {
            // Outside the requested range (or past the output budget): count
            // only. Encoding validation applies solely to lines we return.
            continue;
        }
        let display_line = render_streamed_line(&line, &resolved, true)?;
        if budget.push_line(&mut out, &display_line) {
            lines_shown += 1;
        }
    }

    let total_lines = stream.total_lines();
    let total_bytes = stream.total_bytes();

    if args.start_line as u64 > total_lines {
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
            "path: {}\nlines: none of {} (first line exceeds output budget)\n\n",
            resolved.display(),
            total_lines
        )
    } else {
        let shown_end = if budget.is_truncated() {
            start + lines_shown - 1
        } else {
            requested_end
        };
        format!(
            "path: {}\nlines: {}-{} of {}\n\n",
            resolved.display(),
            args.start_line,
            shown_end,
            total_lines
        )
    };

    if budget.is_truncated() {
        // Report the total bytes the caller actually receives before the
        // marker — body + prepended header + the marker's leading newline —
        // so "showing X of Y bytes" matches the returned content exactly
        // (the marker text itself is appended past the budget).
        let returned_bytes = budget.shown_bytes() + header.len() + 1;
        out.push_str(&format!(
            "\n...[truncated: showing {returned_bytes} of {total_bytes} bytes \
             ({lines_shown} of {total_lines} lines) — use a smaller range]"
        ));
    }

    debug!(
        path = %resolved.display(),
        start_line = args.start_line,
        max_lines = args.max_lines,
        total_lines,
        lines_shown,
        truncated = budget.is_truncated(),
        "read_file_range completed"
    );

    Ok(format!("{header}{out}"))
}

pub fn describe_read_file_range_invocation(args: &ReadFileRangeArgs) -> String {
    format!(
        "Reading file `{}` from line {} (max {} lines).",
        args.path, args.start_line, args.max_lines
    )
}

pub(crate) struct ReadFileRange;

define_tool!(
    ReadFileRange,
    "read_file_range",
    "Read a line range from a UTF-8 text file in the local workspace. Rejects binary files; max 500 lines per call.",
    ReadFileRangeArgs,
    execute_read_file_range_tool,
    "core",
    describe_read_file_range_invocation
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `content` to a temp file (kept alive for the duration of the
    /// call) and run the tool against it.
    fn run(content: &str, start_line: usize, max_lines: usize) -> Result<String, ToolExecError> {
        run_bytes(content.as_bytes(), start_line, max_lines)
    }

    fn run_bytes(
        content: &[u8],
        start_line: usize,
        max_lines: usize,
    ) -> Result<String, ToolExecError> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content).unwrap();
        execute_read_file_range_tool(
            &ReadFileRangeArgs {
                path: file.path().display().to_string(),
                start_line,
                max_lines,
            },
            None,
        )
    }

    #[test]
    fn reads_numbered_line_chunks() {
        let out = run("alpha\nbeta\ngamma\ndelta\n", 2, 2).unwrap();
        assert!(out.contains("lines: 2-3 of 4"), "{out}");
        assert!(out.contains("2 | beta"), "{out}");
        assert!(out.contains("3 | gamma"), "{out}");
    }

    #[test]
    fn clamps_to_eof() {
        let out = run("alpha\nbeta\ngamma\n", 2, 10).unwrap();
        assert!(out.contains("lines: 2-3 of 3"), "{out}");
        assert!(out.contains("2 | beta"), "{out}");
        assert!(out.contains("3 | gamma"), "{out}");
    }

    #[test]
    fn rejects_start_line_past_eof() {
        let err = run("alpha\nbeta\n", 5, 1).unwrap_err();
        assert!(err.to_string().contains("past end of file"), "{err}");
    }

    #[test]
    fn rejects_start_line_zero() {
        let err = run("alpha\n", 0, 1).unwrap_err();
        assert!(err.to_string().contains("start_line must be >= 1"), "{err}");
    }

    #[test]
    fn rejects_max_lines_zero() {
        let err = run("alpha\n", 1, 0).unwrap_err();
        assert!(err.to_string().contains("max_lines must be >= 1"), "{err}");
    }

    #[test]
    fn rejects_excessive_max_lines() {
        let err = run("alpha\n", 1, 501).unwrap_err();
        assert!(
            err.to_string().contains("max_lines must be <= 500"),
            "{err}"
        );
    }

    #[test]
    fn accepts_max_lines_at_cap() {
        let content = (1..=500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let out = run(&content, 1, 500).unwrap();
        assert!(out.contains("lines: 1-500 of 500"), "{out}");
    }

    #[test]
    fn rejects_binary_file() {
        let err = run_bytes(b"\x00\x01\x02binary", 1, 5).unwrap_err();
        assert!(err.to_string().contains("binary file"), "{err}");
    }

    #[test]
    fn rejects_nul_past_sniff_head_in_range() {
        // NUL beyond the 8 KiB head-sniff window, inside the requested
        // range: caught by the per-line check on the returned line.
        let mut content = vec![b'a'; 9 * 1024];
        content.extend_from_slice(b"\n\x00nul\n");
        let err = run_bytes(&content, 1, 5).unwrap_err();
        assert!(err.to_string().contains("binary file"), "{err}");
    }

    #[test]
    fn rejects_invalid_utf8_in_range() {
        let err = run_bytes(b"ok\n\xff\xfe\n", 1, 5).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn rejects_invalid_utf8_past_sniff_head_in_range() {
        // Invalid bytes beyond the 8 KiB head-sniff window, inside the
        // requested range: caught by per-line validation.
        let mut content = vec![b'a'; 9 * 1024];
        content.extend_from_slice(b"\n\xff\xfe\n");
        let err = run_bytes(&content, 2, 5).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn ignores_invalid_utf8_outside_requested_range() {
        // The documented contract: content outside the returned range is not
        // validated. Line 2 is invalid but excluded, so the call succeeds.
        let mut content = vec![b'a'; 9 * 1024];
        content.extend_from_slice(b"\n\xff\xfe\nok\n");
        let out = run_bytes(&content, 3, 1).unwrap();
        assert!(out.contains("3 | ok"), "{out}");
        assert!(out.contains("lines: 3-3 of 3"), "{out}");
    }

    #[test]
    fn marks_over_cap_line_truncated() {
        // A 100 KiB line (no newline) exceeds the 64 KiB per-line cap.
        let big = "x".repeat(100 * 1024);
        let out = run(&big, 1, 5).unwrap();
        assert!(out.contains("...[line truncated: exceeds 64 KiB]"), "{out}");
    }

    #[test]
    fn reports_truncation_when_lines_exceed_byte_budget() {
        // 300 lines × ~500 bytes = ~150 KiB > 128 KiB budget; each line is
        // well under the per-line cap so the byte budget binds.
        let content = (0..300)
            .map(|i| format!("{i:>3} {}", "y".repeat(496)))
            .collect::<Vec<_>>()
            .join("\n");
        let out = run(&content, 1, 500).unwrap();
        assert!(out.contains("...[truncated: showing"), "{out}");
        assert!(out.contains("of 300 lines)"), "{out}");
    }

    #[test]
    fn truncation_report_counts_header_bytes() {
        // The "showing X of Y bytes" figure must match the bytes actually
        // returned (header + body + separator newline), not just the body.
        let content = (0..300)
            .map(|i| format!("{i:>3} {}", "y".repeat(496)))
            .collect::<Vec<_>>()
            .join("\n");
        let out = run(&content, 1, 500).unwrap();
        let marker_prefix = "...[truncated: showing ";
        let marker_tail = out.rsplit(marker_prefix).next().expect("marker present");
        let shown: usize = marker_tail.split(" of ").next().unwrap().parse().unwrap();
        // Everything before the marker prefix is what the caller receives;
        // the reported figure must equal it exactly.
        let returned = out.len() - marker_tail.len() - marker_prefix.len();
        assert_eq!(shown, returned, "reported bytes != returned bytes: {out}");
    }

    #[test]
    fn describe_invocation() {
        let args = ReadFileRangeArgs {
            path: "src/lib.rs".into(),
            start_line: 10,
            max_lines: 50,
        };
        assert_eq!(
            super::describe_read_file_range_invocation(&args),
            "Reading file `src/lib.rs` from line 10 (max 50 lines)."
        );
    }
}
