use crate::tools::{TextStream, ToolExecError, display_path_label, open_text_reader, resolve_path};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

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

    // Drain the stream without materializing per-line values — only the
    // running line total (and the exact byte count) is needed.
    let mut stream = TextStream::new(open_text_reader(&resolved)?);
    for line in &mut stream {
        line?;
    }

    Ok(format!(
        "{}: {} lines",
        display_path_label(&resolved, working_dir),
        stream.total_lines()
    ))
}

pub fn describe_line_count_invocation(args: &LineCountArgs) -> String {
    format!("Counting lines in `{}`.", args.path)
}

pub(crate) struct LineCount;

define_tool!(
    LineCount,
    "line_count",
    "Count the number of lines in a UTF-8 text file.",
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
