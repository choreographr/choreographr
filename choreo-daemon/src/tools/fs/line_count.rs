use crate::tools::{
    TextStream, ToolExecError, display_path_label, open_text_reader, resolve_path,
    sanitize_content, sanitize_name,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use tracing::debug;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LineCountArgs {
    /// Relative or absolute path to a text file
    pub path: String,
}

/// Count the lines in a UTF-8 text file.
///
/// Shares the read tools' streaming [`TextStream`] (and their binary/UTF-8
/// head sniff via `open_text_reader`) so the reported total matches the
/// `of N` figure `read_file` shows for the same file, and so a giant file is
/// never loaded whole into memory just to be counted.
pub(crate) fn execute_line_count_tool(
    args: &LineCountArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }
    let resolved = resolve_path(&args.path, working_dir);

    // Drain the stream for its line total only — `drain_counting` walks the
    // file without materializing (and cloning) each line, so counting a huge
    // file stays memory-bounded at one line. The total matches `read_file`'s
    // `of N` because both count through the same `TextStream`.
    let mut stream = TextStream::new(open_text_reader(&resolved)?);
    stream.drain_counting()?;
    let total_lines = stream.total_lines();

    debug!(path = %resolved.display(), total_lines, "line_count completed");

    Ok(format!(
        "{}: {} lines",
        // Sanitize the label: a hostile file name must not corrupt the
        // line-oriented result.
        sanitize_name(&display_path_label(&resolved, working_dir)),
        total_lines
    ))
}

pub fn describe_line_count_invocation(args: &LineCountArgs) -> String {
    format!("Counting lines in `{}`.", sanitize_content(&args.path))
}

pub(crate) struct LineCount;

define_tool!(
    LineCount,
    "line_count",
    "Count the number of lines in a UTF-8 text file. Shares read_file's binary/UTF-8 head sniff, so binary files are rejected the same way.",
    LineCountArgs,
    execute_line_count_tool,
    "core",
    describe_line_count_invocation
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn run(content: &str) -> Result<String, ToolExecError> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        execute_line_count_tool(
            &LineCountArgs {
                path: file.path().display().to_string(),
            },
            None,
        )
    }

    #[test]
    fn counts_lines() {
        let out = run("alpha\nbeta\ngamma\n").unwrap();
        assert!(out.ends_with(": 3 lines"), "{out}");
    }

    #[test]
    fn counts_file_without_trailing_newline() {
        let out = run("alpha\nbeta").unwrap();
        assert!(out.ends_with(": 2 lines"), "{out}");
    }

    #[test]
    fn empty_file_counts_zero() {
        let out = run("").unwrap();
        assert!(out.ends_with(": 0 lines"), "{out}");
    }

    #[test]
    fn rejects_binary_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"\x00\x01\x02binary").unwrap();
        let err = execute_line_count_tool(
            &LineCountArgs {
                path: file.path().display().to_string(),
            },
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("binary file"), "{err}");
    }

    #[test]
    fn describe_line_count_invocation() {
        let args = LineCountArgs {
            path: "Cargo.toml".into(),
        };
        let desc = super::describe_line_count_invocation(&args);
        assert_eq!(desc, "Counting lines in `Cargo.toml`.");
    }
}
