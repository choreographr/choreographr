use crate::tools::{ToolError, truncate_tool_output};
use gix::status::UntrackedFiles;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{fmt::Write as _, io, ops::Deref};

use super::{
    collect_cached_diff_lines, open_repo, path_from_bytes, pathspec_patterns, repo_work_dir_display,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GitDiffArgs {
    pub repo_path: Option<String>,
    pub cached: Option<bool>,
    pub pathspec: Option<Vec<String>>,
}

/// Append a unified diff wrapped in a ````diff` fenced code block.
///
/// This ensures the diff content is clearly delimited from surrounding
/// tool output when rendered in markdown or the TUI.
pub fn append_fenced_diff(out: &mut String, diff: &str) {
    if !diff.is_empty() {
        out.push_str("```diff\n");
        out.push_str(diff);
        out.push_str("\n```");
    }
}

pub fn execute_git_diff_tool(
    args: &GitDiffArgs,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let pathspec = args.pathspec.clone().unwrap_or_default();
    let output = git_diff_impl(
        args.repo_path.as_deref(),
        args.cached.unwrap_or(false),
        pathspec,
        working_dir,
    )?;
    Ok(truncate_tool_output(&output))
}

/// Produce unified diffs for changed files using gix.
///
/// When `cached` is true, compares HEAD↔index (staged changes).
/// When `cached` is false, compares index↔worktree (unstaged changes).
/// Added files diff against an empty string; deleted files diff against HEAD content.
pub(crate) fn git_diff_impl(
    repo_path: Option<&str>,
    cached: bool,
    pathspec: Vec<String>,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    use gix::status::index_worktree::Item as WtItem;

    let repo = open_repo(repo_path, working_dir)?;
    let workdir = repo_work_dir_display(&repo);

    let mut out = String::new();
    writeln!(&mut out, "repository: {workdir}").ok();
    writeln!(
        &mut out,
        "mode: {}",
        if cached { "staged" } else { "working tree" }
    )
    .ok();

    let mut has_changes = false;

    // Staged changes: iterate over the HEAD↔index diff entries produced by
    // collect_cached_diff_lines, and for each status code produce the
    // corresponding unified diff.
    if cached {
        let changes = collect_cached_diff_lines(&repo, &pathspec)?;
        let index = repo.index().map_err(io::Error::other)?;

        for line in &changes {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() < 2 {
                continue;
            }
            let status = parts[0];
            let path = parts[1];

            if !pathspec.is_empty() && !super::pathspec_matches(&pathspec, path) {
                continue;
            }

            match status {
                // Added or copied files have no old content — diff against empty string.
                "A" | "C" => {
                    has_changes = true;
                    writeln!(out).ok();
                    if let Some(new_content) = entry_content_by_path(&repo, &index, path) {
                        let diff = crate::diff_util::generate_diff("", &new_content, path, path);
                        append_fenced_diff(&mut out, &diff);
                    }
                }
                // Deleted files have no new content — diff against HEAD.
                "D" => {
                    has_changes = true;
                    writeln!(out).ok();
                    if let Some(old_content) = head_content_by_path(&repo, path) {
                        let diff = crate::diff_util::generate_diff(&old_content, "", path, path);
                        append_fenced_diff(&mut out, &diff);
                    }
                }
                // Modified or renamed files: diff HEAD content against index content.
                _ => {
                    has_changes = true;
                    writeln!(out).ok();
                    let old_content = head_content_by_path(&repo, path).unwrap_or_default();
                    let new_content =
                        entry_content_by_path(&repo, &index, path).unwrap_or_default();
                    if old_content != new_content {
                        let diff =
                            crate::diff_util::generate_diff(&old_content, &new_content, path, path);
                        append_fenced_diff(&mut out, &diff);
                    }
                }
            }
        }
    } else {
        // Unstaged changes: iterate over the index↔worktree status and produce
        // unified diffs for modifications and untracked files.
        let index = repo.index().map_err(io::Error::other)?;

        let status_iter = repo
            .status(gix::progress::Discard)
            .map_err(io::Error::other)?
            .untracked_files(UntrackedFiles::Files)
            .into_index_worktree_iter(pathspec_patterns(&pathspec))
            .map_err(io::Error::other)?;

        for item in status_iter {
            let item = item.map_err(io::Error::other)?;
            let path = path_from_bytes(item.rela_path().as_ref());

            match &item {
                // Differing file content: diff index entry against file on disk.
                WtItem::Modification { .. } => {
                    let old_content =
                        entry_content_by_path(&repo, &index, &path).unwrap_or_default();
                    let full_path = repo.workdir().unwrap_or(repo.git_dir()).join(&path);
                    let new_content = std::fs::read_to_string(&full_path).unwrap_or_default();

                    if old_content != new_content {
                        has_changes = true;
                        writeln!(out).ok();
                        let diff = crate::diff_util::generate_diff(
                            &old_content,
                            &new_content,
                            &path,
                            &path,
                        );
                        append_fenced_diff(&mut out, &diff);
                    }
                }
                // Untracked files have no index counterpart — diff against empty string.
                WtItem::DirectoryContents { entry, .. }
                    if matches!(entry.status, gix::dir::entry::Status::Untracked) =>
                {
                    has_changes = true;
                    writeln!(out).ok();
                    let full_path = repo.workdir().unwrap_or(repo.git_dir()).join(&path);
                    if let Ok(new_content) = std::fs::read_to_string(&full_path) {
                        let diff = crate::diff_util::generate_diff("", &new_content, &path, &path);
                        append_fenced_diff(&mut out, &diff);
                    }
                }
                _ => {}
            }
        }
    }

    if !has_changes {
        writeln!(out, "no changes").ok();
    }

    Ok(out.trim_end().to_string())
}

/// Walk from HEAD commit → tree → entry by path, then return the file content.
///
/// The `??` on the `peel_to_entry_by_path` line unwraps the outer `Result` (I/O error)
/// and then the inner `Option` (path not found in tree).
fn head_content_by_path(repo: &gix::Repository, path: &str) -> Option<String> {
    let head_id = repo.head().ok()?.id()?;
    let object = head_id.object().ok()?;
    let mut tree = object.peel_to_tree().ok()?;
    let entry = tree.peel_to_entry_by_path(path).ok()?;
    let entry = entry?;
    let obj = entry.object().ok()?;
    String::from_utf8(obj.data.to_vec()).ok()
}

/// Search the index for a matching entry by path, then return its blob content.
///
/// Deref is used to access the `State` from `gix::index::File` so we can call
/// `path_backing()` and `path_in()` — these live on `State`, not on the `File` wrapper.
fn entry_content_by_path(
    repo: &gix::Repository,
    index: &gix::index::File,
    path: &str,
) -> Option<String> {
    let state: &gix::index::State = index.deref();
    let backing = state.path_backing();
    let entry = index.entries().iter().find(|e| {
        let p = e.path_in(backing);
        path_from_bytes(p.as_ref()) == path
    })?;
    let obj = repo.find_object(entry.id).ok()?;
    String::from_utf8(obj.data.to_vec()).ok()
}

pub fn describe_git_diff_invocation(args: &GitDiffArgs) -> String {
    let mut parts = vec!["Showing git diff.".to_string()];
    if args.cached.unwrap_or(false) {
        parts.push(" Cached (staged) changes.".to_string());
    }
    if let Some(ref paths) = args.pathspec
        && !paths.is_empty()
    {
        parts.push(format!(" Pathspec: `{}`.", paths.join("`, `")));
    }
    parts.concat()
}

pub(crate) struct GitDiff;

define_tool!(
    GitDiff,
    "git_diff",
    "Show the line-by-line unified diff for a file or repository.",
    GitDiffArgs,
    execute_git_diff_tool,
    "git",
    describe_git_diff_invocation
);
