use super::{
    MAX_LINE_DISPLAY_BYTES, MAX_TOOL_OUTPUT_BYTES, ToolExecError, confine_path, drain_rest_of_line,
    open_text_reader, read_line_capped,
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
    let mut reader = open_text_reader(&resolved)?;

    let start = args.start_line as u64;
    let max = args.max_lines as u64;

    let mut out = String::new();
    let mut line_buf: Vec<u8> = Vec::new();
    let mut total_lines: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut shown_bytes: usize = 0;
    let mut lines_shown: u64 = 0;
    let mut output_truncated = false;

    loop {
        let complete = read_line_capped(&mut reader, &mut line_buf, MAX_LINE_DISPLAY_BYTES)?;
        if line_buf.is_empty() {
            // EOF (empty file, or trailing newline consumed): no extra line.
            break;
        }
        total_lines += 1;
        let bytes_before_line = total_bytes;
        let line_total: u64 = if complete {
            line_buf.len() as u64
        } else {
            // Over-cap line: count its full length (draining keeps memory
            // bounded) but display only the capped prefix below.
            line_buf.len() as u64 + drain_rest_of_line(&mut reader)?
        };
        total_bytes += line_total;

        let in_window = total_lines >= start && lines_shown < max;
        if !in_window || output_truncated {
            // Outside the requested range (or past the output budget): count
            // only. Encoding validation applies solely to lines we return.
            continue;
        }

        // Binary / encoding checks — only for lines we actually return.
        if let Some(pos) = line_buf.iter().position(|&b| b == 0) {
            return Err(ToolExecError(format!(
                "'{}' appears to be a binary file (NUL byte at offset {})",
                resolved.display(),
                bytes_before_line + pos as u64
            )));
        }
        let line_str = match std::str::from_utf8(&line_buf) {
            Ok(s) => s,
            Err(e) if !complete && e.error_len().is_none() => {
                // The display cap split a multi-byte char mid-sequence; the
                // prefix before the split is valid and that is all we show.
                std::str::from_utf8(&line_buf[..e.valid_up_to()]).unwrap_or_default()
            }
            Err(e) => {
                return Err(ToolExecError(format!(
                    "'{}' is not valid UTF-8 text (invalid byte sequence at offset {})",
                    resolved.display(),
                    bytes_before_line + e.valid_up_to() as u64
                )));
            }
        };

        // Match `str::lines()` display semantics (strip one '\n', then one
        // '\r'), then render with the current 1-based line number.
        let mut display = line_str;
        if let Some(stripped) = display.strip_suffix('\n') {
            display = stripped;
        }
        if let Some(stripped) = display.strip_suffix('\r') {
            display = stripped;
        }
        let mut display_line = format!("{total_lines} | {display}");
        if !complete {
            display_line.push_str("\n...[line truncated: exceeds 64 KiB]");
        }
        // +1 accounts for the newline re-appended below.
        let display_len = display_line.len() + 1;
        if shown_bytes + display_len <= MAX_TOOL_OUTPUT_BYTES {
            out.push_str(&display_line);
            out.push('\n');
            shown_bytes += display_len;
            lines_shown += 1;
        } else {
            output_truncated = true;
        }
    }

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
    let header = if output_truncated && lines_shown == 0 {
        format!(
            "path: {}\nlines: none of {} (first line exceeds output budget)\n\n",
            resolved.display(),
            total_lines
        )
    } else {
        let shown_end = if output_truncated {
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

    if output_truncated {
        out.push_str(&format!(
            "\n...[truncated: showing {} of {} bytes ({} of {} lines) — \
             use a smaller range]",
            shown_bytes, total_bytes, lines_shown, total_lines
        ));
    }

    debug!(
        path = %resolved.display(),
        start_line = args.start_line,
        max_lines = args.max_lines,
        total_lines,
        lines_shown,
        truncated = output_truncated,
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
    fn rejects_invalid_utf8_in_range() {
        let err = run_bytes(b"ok\n\xff\xfe\n", 1, 5).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
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
