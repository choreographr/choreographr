use crate::tools::{ToolExecError, resolve_path, truncate_tool_output};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesArgs {
    /// Relative or absolute path to a directory (defaults to working directory)
    pub path: Option<String>,
}

pub(crate) fn execute_list_files_tool(
    args: &ListFilesArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = args.path.as_deref().unwrap_or(".");
    let resolved = resolve_path(path, working_dir);
    let entries = std::fs::read_dir(&resolved)?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let mut name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    Ok(truncate_tool_output(&names.join("\n")))
}

pub fn describe_list_files_invocation(args: &ListFilesArgs) -> String {
    match &args.path {
        Some(p) => format!("Listing files in `{}`.", p),
        None => "Listing files in the working directory.".to_string(),
    }
}

pub(crate) struct ListFiles;

define_tool!(
    ListFiles,
    "list_files",
    "List files in a local directory.",
    ListFilesArgs,
    execute_list_files_tool,
    "core",
    describe_list_files_invocation
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_list_files_invocation_with_path() {
        let args = ListFilesArgs {
            path: Some("src".into()),
        };
        let desc = super::describe_list_files_invocation(&args);
        assert_eq!(desc, "Listing files in `src`.");
    }

    #[test]
    fn describe_list_files_invocation_without_path() {
        let args = ListFilesArgs { path: None };
        let desc = super::describe_list_files_invocation(&args);
        assert_eq!(desc, "Listing files in the working directory.");
    }
}
