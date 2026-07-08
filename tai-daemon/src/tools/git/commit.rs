use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use gix::ObjectId;
use serde::Deserialize;
use std::io;

use super::{collect_cached_diff_lines, load_mutable_index, open_repo, path_from_bytes};

#[derive(Debug, Deserialize)]
struct GitCommitArgs {
    repo_path: Option<String>,
    message: String,
    allow_empty: Option<bool>,
}

pub fn execute_git_commit_tool(arguments_json: &str, cwd: Option<&std::path::Path>) -> ToolResult {
    match execute_git_commit_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_git_commit_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let args: GitCommitArgs = serde_json::from_str(arguments_json)?;
    let output = git_commit_impl(
        args.repo_path.as_deref(),
        &args.message,
        args.allow_empty.unwrap_or(false),
        cwd,
    )?;
    Ok(truncate_tool_output(&output))
}

fn git_commit_impl(
    repo_path: Option<&str>,
    message: &str,
    allow_empty: bool,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(repo_path, cwd)?;
    let message = message.trim();
    if message.is_empty() {
        return Err(ToolError::Other(
            "commit message must not be empty".to_string(),
        ));
    }

    let index = load_mutable_index(&repo)?;
    ensure_index_has_no_conflicts(&index)?;

    if !allow_empty && collect_cached_diff_lines(&repo, &[] as &[String])?.is_empty() {
        return Err(ToolError::Other("no staged changes to commit".to_string()));
    }
    let tree_id = write_tree_from_index(&repo, &index)?;
    let parents = current_head_parents(&repo)?;

    repo.commit("HEAD", message, tree_id, parents)
        .map_err(io::Error::other)?;

    super::log::git_log_impl(repo_path, 1, cwd)
}

fn ensure_index_has_no_conflicts(index: &gix::index::File) -> Result<(), ToolError> {
    if let Some(path) = index
        .entries()
        .iter()
        .find(|entry| entry.stage() != gix::index::entry::Stage::Unconflicted)
        .map(|entry| path_from_bytes(entry.path(index).as_ref()))
    {
        Err(ToolError::Other(format!(
            "cannot commit with unresolved index conflicts at {path}"
        )))
    } else {
        Ok(())
    }
}

fn write_tree_from_index(
    repo: &gix::Repository,
    index: &gix::index::File,
) -> Result<ObjectId, ToolError> {
    let mut editor = repo.empty_tree().edit().map_err(io::Error::other)?;
    for entry in index.entries() {
        if entry.stage() != gix::index::entry::Stage::Unconflicted {
            return Err(ToolError::Other(format!(
                "cannot write tree with conflicted index entry at {}",
                path_from_bytes(entry.path(index).as_ref())
            )));
        }
        let kind = entry.mode.to_tree_entry_mode().ok_or_else(|| {
            ToolError::Other(format!(
                "unsupported index entry mode {} at {}",
                entry.mode.bits(),
                path_from_bytes(entry.path(index).as_ref())
            ))
        })?;
        editor
            .upsert(entry.path(index).to_owned(), kind.into(), entry.id)
            .map_err(io::Error::other)?;
    }
    editor
        .write()
        .map(|id| id.detach())
        .map_err(io::Error::other)
        .map_err(ToolError::from)
}

fn current_head_parents(repo: &gix::Repository) -> Result<Vec<ObjectId>, ToolError> {
    match repo.head_id() {
        Ok(head) => Ok(vec![head.detach()]),
        Err(_) => Ok(Vec::new()),
    }
}

define_tool!(
    GitCommit,
    "git_commit",
    "Create a Git commit from the current index.",
    execute_git_commit_tool,
    serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"message":{"type":"string","description":"Commit message"}},"required":["message"],"additionalProperties":false}),
    "git"
);
