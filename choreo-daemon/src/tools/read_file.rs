use super::{
    MAX_TOOL_OUTPUT_BYTES, OutputBudget, TextStream, ToolExecError, open_text_reader,
    render_streamed_line, resolve_path,
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
    let resolved = resolve_path(&args.path, working_dir);

    // Stream through a bounded reader (see `TextStream`): memory usage is
    // capped at one line plus the output budget, no matter how large the
    // file is. Binary files (NUL in the head) are rejected up front by
    // `open_text_reader`; per-line NUL/UTF-8 checks happen in
    // `render_streamed_line` on content we actually return.
    let mut stream = TextStream::new(open_text_reader(&resolved)?);

    let mut out = String::new();
    let mut budget = OutputBudget::new(MAX_TOOL_OUTPUT_BYTES);

    for line in &mut stream {
        let line = line?;
        // Once the output budget is exhausted we stop collecting (and stop
        // validating) but keep counting so the truncation report is exact.
        if budget.is_truncated() {
            continue;
        }
        let display_line = render_streamed_line(&line, &resolved, false)?;
        budget.push_line(&mut out, &display_line);
    }

    if budget.is_truncated() {
        // Report the total bytes the caller actually receives before the
        // marker — body + the marker's leading newline (which separates it
        // from the last line) — so "showing X of Y bytes" matches the
        // returned content exactly (the marker text itself is appended past
        // the budget).
        let returned_bytes = budget.shown_bytes() + 1;
        out.push_str(&format!(
            "\n...[truncated: showing {returned_bytes} of {} bytes; file has {} line(s) — \
             use read_file_range for the rest]",
            stream.total_bytes(),
            stream.total_lines()
        ));
    }

    debug!(
        path = %resolved.display(),
        total_lines = stream.total_lines(),
        total_bytes = stream.total_bytes(),
        shown_bytes = budget.shown_bytes(),
        truncated = budget.is_truncated(),
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
        // Invalid bytes beyond the 8 KiB head-sniff window: the line is
        // validated before it is returned.
        let mut content = vec![b'a'; 9 * 1024];
        content.extend_from_slice(b"\xff\xfe");
        let err = run_bytes(&content).unwrap_err();
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
    fn reads_lines_after_over_cap_line() {
        // A 100 KiB line followed by a normal line: the over-cap line is
        // drained (counted, never buffered) and reading resumes cleanly.
        let mut content = vec![b'x'; 100 * 1024];
        content.extend_from_slice(b"\nbeta\n");
        let out = run_bytes(&content).unwrap();
        assert!(out.contains("...[line truncated: exceeds 64 KiB]"), "{out}");
        assert!(out.ends_with("beta\n"), "missing trailing line: {out:?}");
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
