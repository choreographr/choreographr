use crate::tools::{ToolExecError, confine_path};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LineCountArgs {
    /// Relative or absolute path to a text file
    pub path: String,
}

pub(crate) fn execute_line_count_tool(
    args: &LineCountArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }
    let resolved = confine_path(&args.path, working_dir)?;
    let content = std::fs::read_to_string(&resolved)?;
    let line_count = content.lines().count();
    Ok(format!("{}: {} lines", resolved.display(), line_count))
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

    #[test]
    fn describe_line_count_invocation() {
        let args = LineCountArgs {
            path: "Cargo.toml".into(),
        };
        let desc = super::describe_line_count_invocation(&args);
        assert_eq!(desc, "Counting lines in `Cargo.toml`.");
    }
}
