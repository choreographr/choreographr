use crate::tools::{ToolError, truncate_tool_output};
use gix::{
    bstr::BString,
    status::{Item as StatusItem, UntrackedFiles},
};
use serde::Deserialize;
use std::{fmt::Write as _, io};

use super::{
    describe_head, format_index_worktree_change, format_tree_index_change, open_repo,
    path_from_bytes, repo_work_dir_display, sort_and_dedup, write_section,
};

#[derive(Debug, Deserialize)]
pub struct GitRepoArgs {
    pub repo_path: Option<String>,
}

pub fn execute_git_status_tool(
    args: &GitRepoArgs,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let output = git_status_impl(args.repo_path.as_deref(), working_dir)?;
    Ok(truncate_tool_output(&output))
}

fn git_status_impl(
    repo_path: Option<&str>,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(repo_path, working_dir)?;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();

    let iter = repo
        .status(gix::progress::Discard)
        .map_err(io::Error::other)?
        .untracked_files(UntrackedFiles::Files)
        .into_iter(Vec::<BString>::new())
        .map_err(io::Error::other)?;

    for item in iter {
        let item = item.map_err(io::Error::other)?;
        match item {
            StatusItem::TreeIndex(change) => staged.push(format_tree_index_change(&change)),
            StatusItem::IndexWorktree(change) => match &change {
                gix::status::index_worktree::Item::DirectoryContents { entry, .. }
                    if matches!(entry.status, gix::dir::entry::Status::Untracked) =>
                {
                    untracked.push(format!(
                        "?? {}",
                        path_from_bytes(change.rela_path().as_ref())
                    ));
                }
                _ => unstaged.push(format_index_worktree_change(&change)),
            },
        }
    }

    sort_and_dedup(&mut staged);
    sort_and_dedup(&mut unstaged);
    sort_and_dedup(&mut untracked);

    let mut out = String::new();
    writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
    writeln!(&mut out, "head: {}", describe_head(&repo)?).ok();
    write_section(&mut out, "staged", &staged);
    write_section(&mut out, "unstaged", &unstaged);
    write_section(&mut out, "untracked", &untracked);
    Ok(out.trim_end().to_string())
}

pub(crate) struct GitStatus;

define_tool!(
    GitStatus,
    "git_status",
    "Show the status of the Git repository containing the given path.",
    GitRepoArgs,
    String,
    execute_git_status_tool,
    serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."}},"additionalProperties":false}),
    "git"
);
