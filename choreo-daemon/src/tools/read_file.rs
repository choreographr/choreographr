use super::{
    MAX_LINE_DISPLAY_BYTES, MAX_TOOL_OUTPUT_BYTES, ToolExecError, confine_path, drain_rest_of_line,
    open_text_reader, read_line_capped,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use tracing::debug;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Relative or absolute path to a text file
    pub path: String,
}

pub(crate) fn execute_read_file_tool(
    args: &ReadFileArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }
    let resolved = confine_path(&args.path, working_dir)?;

    // Stream through a bounded reader: memory usage is capped at one line
    // plus the output budget, no matter how large the file is. Binary files
    // (NUL in the head) are rejected up front by `open_text_reader`.
    let mut reader = open_text_reader(&resolved)?;

    let mut out = String::new();
    let mut line_buf: Vec<u8> = Vec::new();
    let mut total_lines: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut shown_bytes: usize = 0;
    let mut output_truncated = false;

    loop {
        let complete = read_line_capped(&mut reader, &mut line_buf, MAX_LINE_DISPLAY_BYTES)?;
        if line_buf.is_empty() {
            // EOF (empty file, or the trailing newline was already consumed):
            // there is no extra final line to report.
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

        // Once the output budget is exhausted we stop collecting (and stop
        // validating) but keep counting so the truncation report is exact.
        if output_truncated {
            continue;
        }

        // Binary / encoding checks apply to everything we actually return.
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

        // Match `str::lines()` display semantics: strip one trailing '\n'
        // and then one trailing '\r', then re-append a single '\n'.
        let mut display = line_str;
        if let Some(stripped) = display.strip_suffix('\n') {
            display = stripped;
        }
        if let Some(stripped) = display.strip_suffix('\r') {
            display = stripped;
        }
        let mut display_line = display.to_string();
        if !complete {
            display_line.push_str("\n...[line truncated: exceeds 64 KiB]");
        }
        // +1 accounts for the newline re-appended below.
        let display_len = display_line.len() + 1;
        if shown_bytes + display_len <= MAX_TOOL_OUTPUT_BYTES {
            out.push_str(&display_line);
            out.push('\n');
            shown_bytes += display_len;
        } else {
            output_truncated = true;
        }
    }

    if output_truncated {
        out.push_str(&format!(
            "\n...[truncated: showing {} of {} bytes; file has {} line(s) — \
             use read_file_range for the rest]",
            shown_bytes, total_bytes, total_lines
        ));
    }

    debug!(
        path = %resolved.display(),
        total_lines,
        total_bytes,
        shown_bytes,
        truncated = output_truncated,
        "read_file completed"
    );

    Ok(out)
}

pub fn describe_read_file_invocation(args: &ReadFileArgs) -> String {
    format!("Reading file `{}`.", args.path)
}

pub(crate) struct ReadFile;

define_tool!(
    ReadFile,
    "read_file",
    "Read a UTF-8 text file from the local workspace. Rejects binary files; output is capped and truncation reports totals.",
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
    /// call) and run the tool against it.
    fn run(content: &str) -> Result<String, ToolExecError> {
        run_bytes(content.as_bytes())
    }

    fn run_bytes(content: &[u8]) -> Result<String, ToolExecError> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content).unwrap();
        execute_read_file_tool(
            &ReadFileArgs {
                path: file.path().display().to_string(),
            },
            None,
        )
    }

    #[test]
    fn reads_plain_text_file() {
        let out = run("alpha\nbeta\ngamma\n").unwrap();
        assert_eq!(out, "alpha\nbeta\ngamma\n");
    }

    #[test]
    fn reads_file_without_trailing_newline() {
        let out = run("alpha\nbeta").unwrap();
        assert_eq!(out, "alpha\nbeta\n");
    }

    #[test]
    fn normalizes_crlf_line_endings() {
        let out = run("alpha\r\nbeta\r\n").unwrap();
        assert_eq!(out, "alpha\nbeta\n");
    }

    #[test]
    fn empty_file_returns_empty_output() {
        let out = run("").unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn rejects_missing_path() {
        let err = execute_read_file_tool(&ReadFileArgs { path: "  ".into() }, None).unwrap_err();
        assert!(err.to_string().contains("missing required"));
    }

    #[test]
    fn rejects_binary_file() {
        let err = run_bytes(b"\x89PNG\r\n\x1a\n\x00\x00").unwrap_err();
        assert!(err.to_string().contains("binary file"), "{err}");
    }

    #[test]
    fn rejects_invalid_utf8() {
        let err = run_bytes(b"ok\n\xff\xfe").unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn reports_totals_when_output_truncated() {
        // 3000 lines × 100 bytes, but the final line has no trailing newline,
        // so the file is 299,999 bytes > 128 KiB budget.
        let content = (0..3000)
            .map(|i| format!("{i:>8} {}", "x".repeat(90)))
            .collect::<Vec<_>>()
            .join("\n");
        let out = run(&content).unwrap();
        assert!(out.contains("...[truncated: showing"), "{out}");
        assert!(out.contains("of 299999 bytes"), "{out}");
        assert!(out.contains("file has 3000 line(s)"), "{out}");
    }

    #[test]
    fn marks_over_cap_line_truncated() {
        // A single 100 KiB line (no newline) exceeds the 64 KiB per-line cap.
        let big = "x".repeat(100 * 1024);
        let out = run(&big).unwrap();
        assert!(out.contains("...[line truncated: exceeds 64 KiB]"), "{out}");
    }

    #[test]
    fn describe_invocation() {
        let args = ReadFileArgs {
            path: "src/main.rs".into(),
        };
        assert_eq!(
            super::describe_read_file_invocation(&args),
            "Reading file `src/main.rs`."
        );
    }
}
