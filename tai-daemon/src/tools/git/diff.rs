use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use gix::status::UntrackedFiles;
use serde::Deserialize;
use std::{fmt::Write as _, io};

use super::{
    collect_cached_diff_lines, format_index_worktree_change, open_repo, pathspec_patterns,
    repo_work_dir_display, sort_and_dedup,
};

#[derive(Debug, Deserialize)]
struct GitDiffArgs {
    repo_path: Option<String>,
    cached: Option<bool>,
    pathspec: Option<Vec<String>>,
}

pub fn execute_git_diff_tool(arguments_json: &str, cwd: Option<&std::path::Path>) -> ToolResult {
    match execute_git_diff_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_git_diff_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let args: GitDiffArgs = serde_json::from_str(arguments_json)?;
    let output = git_diff_impl(
        args.repo_path.as_deref(),
        args.cached.unwrap_or(false),
        args.pathspec.unwrap_or_default(),
        cwd,
    )?;
    Ok(truncate_tool_output(&output))
}

pub(crate) fn git_diff_impl(
    repo_path: Option<&str>,
    cached: bool,
    pathspec: Vec<String>,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(repo_path, cwd)?;
    let mut lines = if cached {
        collect_cached_diff_lines(&repo, &pathspec)?
    } else {
        collect_worktree_diff_lines(&repo, &pathspec)?
    };
    sort_and_dedup(&mut lines);

    let mut out = String::new();
    writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
    writeln!(
        &mut out,
        "mode: {}",
        if cached { "staged" } else { "working tree" }
    )
    .ok();
    if !pathspec.is_empty() {
        writeln!(&mut out, "pathspec: {}", pathspec.join(", ")).ok();
    }
    if lines.is_empty() {
        writeln!(&mut out, "no changes").ok();
    } else {
        for line in lines {
            writeln!(&mut out, "{line}").ok();
        }
    }
    Ok(out.trim_end().to_string())
}

fn collect_worktree_diff_lines(
    repo: &gix::Repository,
    pathspec: &[String],
) -> Result<Vec<String>, ToolError> {
    let patterns = pathspec_patterns(pathspec);
    let iter = repo
        .status(gix::progress::Discard)
        .map_err(io::Error::other)?
        .untracked_files(UntrackedFiles::Files)
        .into_index_worktree_iter(patterns)
        .map_err(io::Error::other)?;

    let mut lines = Vec::new();
    for item in iter {
        let item = item.map_err(io::Error::other)?;
        lines.push(format_index_worktree_change(&item));
    }
    Ok(lines)
}

define_tool_with_cwd!(
    GitDiff,
    "git_diff",
    "Show the diff for a file or repository.",
    execute_git_diff_tool,
    serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"cached":{"type":"boolean","description":"Show staged (cached) changes instead of worktree changes","default":false},"pathspec":{"type":"array","items":{"type":"string"},"description":"Optional pathspecs to filter"},"additionalProperties":false}})
);
