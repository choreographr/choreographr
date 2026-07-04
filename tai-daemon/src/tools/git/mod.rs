use gix::{
    ObjectId,
    bstr::BString,
    prelude::ObjectIdExt,
    progress::Discard,
    status::{Item as StatusItem, UntrackedFiles},
    worktree::IndexPersistedOrInMemory,
};
use std::{fmt::Write as _, io, path::Path};

use super::ToolError;

mod commit;
mod diff;
mod log;
mod push;
mod stage;
mod status;

pub(crate) use commit::GitCommit;
pub use commit::execute_git_commit_tool;
pub(crate) use diff::GitDiff;
pub use diff::execute_git_diff_tool;
pub(crate) use log::GitLog;
pub use log::execute_git_log_tool;
pub(crate) use push::GitPush;
pub use push::execute_git_push_tool;
pub(crate) use stage::GitAdd;
pub use stage::execute_git_add_tool;
pub(crate) use status::GitStatus;
pub use status::execute_git_status_tool;

pub(crate) fn open_repo(
    repo_path: Option<&str>,
    cwd: Option<&std::path::Path>,
) -> Result<gix::Repository, ToolError> {
    let path = repo_path.unwrap_or(".").trim();
    let path = if path.is_empty() { "." } else { path };
    let resolved = super::resolve_path(path, cwd);
    gix::discover(&resolved).map_err(|error| {
        ToolError::Other(format!(
            "failed to open git repository from {}: {error}",
            resolved.display()
        ))
    })
}

pub(crate) fn repo_work_dir(repo: &gix::Repository) -> &Path {
    repo.workdir().unwrap_or_else(|| repo.git_dir())
}

pub(crate) fn repo_work_dir_display(repo: &gix::Repository) -> String {
    repo_work_dir(repo).display().to_string()
}

pub(crate) fn describe_head(repo: &gix::Repository) -> Result<String, ToolError> {
    if let Some(name) = repo.head_name().map_err(io::Error::other)? {
        return Ok(name.shorten().to_string());
    }
    match repo.head_id() {
        Ok(id) => Ok(format!("detached at {}", shorten_id(repo, id.detach())?)),
        Err(_) => Ok("unborn HEAD".to_string()),
    }
}

pub(crate) fn shorten_id(repo: &gix::Repository, id: ObjectId) -> Result<String, ToolError> {
    Ok(id
        .attach(repo)
        .shorten()
        .map_err(io::Error::other)?
        .to_string())
}

pub(crate) fn path_from_bytes(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

pub(crate) fn sort_and_dedup(lines: &mut Vec<String>) {
    lines.sort();
    lines.dedup();
}

pub(crate) fn write_section(out: &mut String, title: &str, lines: &[String]) {
    let _ = writeln!(out, "{title}:");
    if lines.is_empty() {
        let _ = writeln!(out, "  (none)");
        return;
    }
    for line in lines {
        let _ = writeln!(out, "  {line}");
    }
}

pub(crate) fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub(crate) fn append_command_output(out: &mut String, label: &str, content: &str) {
    if content.is_empty() {
        return;
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{label}:");
    let _ = writeln!(out, "{content}");
}

pub(crate) fn run_git_command(
    repo: &gix::Repository,
    args: &[String],
) -> Result<std::process::Output, ToolError> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(repo_work_dir(repo))
        .output()
        .map_err(|error| ToolError::Other(format!("failed to run git {}: {error}", args.join(" "))))
}

pub(crate) fn normalize_nonempty_argument<'a>(
    value: &'a str,
    name: &str,
) -> Result<&'a str, ToolError> {
    let value = value.trim();
    if value.is_empty() {
        Err(ToolError::Other(format!("{name} must not be empty")))
    } else {
        Ok(value)
    }
}

pub(crate) fn current_branch_name(repo: &gix::Repository) -> Result<String, ToolError> {
    repo.head_name()
        .map_err(io::Error::other)?
        .map(|name| name.shorten().to_string())
        .ok_or_else(|| {
            ToolError::Other("branch must be provided when HEAD is detached".to_string())
        })
}

pub(crate) fn load_mutable_index(repo: &gix::Repository) -> Result<gix::index::File, ToolError> {
    match repo
        .index_or_load_from_head_or_empty()
        .map_err(io::Error::other)?
    {
        IndexPersistedOrInMemory::Persisted(index) => Ok((**index).clone()),
        IndexPersistedOrInMemory::InMemory(index) => Ok(index),
    }
}

pub(crate) fn collect_cached_diff_lines(
    repo: &gix::Repository,
    pathspec: &[String],
) -> Result<Vec<String>, ToolError> {
    let iter = repo
        .status(Discard)
        .map_err(io::Error::other)?
        .untracked_files(UntrackedFiles::None)
        .into_iter(Vec::<BString>::new())
        .map_err(io::Error::other)?;

    let mut lines = Vec::new();
    for item in iter {
        let item = item.map_err(io::Error::other)?;
        if let StatusItem::TreeIndex(change) = item {
            let path = path_from_bytes(change.location().as_ref());
            if pathspec_matches(pathspec, &path) {
                lines.push(format_tree_index_change(&change));
            }
        }
    }
    Ok(lines)
}

pub(crate) fn pathspec_patterns(pathspec: &[String]) -> Vec<BString> {
    pathspec
        .iter()
        .map(|spec| BString::from(spec.as_str()))
        .collect()
}

pub(crate) fn pathspec_matches(pathspec: &[String], path: &str) -> bool {
    if pathspec.is_empty() {
        return true;
    }
    pathspec.iter().any(|spec| {
        let spec = spec.trim();
        !spec.is_empty()
            && (path == spec
                || path.starts_with(spec.strip_suffix('/').unwrap_or(spec))
                || simple_glob_matches(spec, path))
    })
}

pub(crate) fn simple_glob_matches(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return false;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    let mut rest = text;
    let mut first = true;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if first && !pattern.starts_with('*') {
            if let Some(stripped) = rest.strip_prefix(part) {
                rest = stripped;
            } else {
                return false;
            }
            first = false;
            continue;
        }
        if index == parts.len() - 1 && !pattern.ends_with('*') {
            return rest.ends_with(part);
        }
        if let Some(found) = rest.find(part) {
            rest = &rest[(found + part.len())..];
        } else {
            return false;
        }
        first = false;
    }
    true
}

pub(crate) fn format_tree_index_change(change: &gix::diff::index::Change) -> String {
    use gix::diff::index::ChangeRef;
    match change {
        ChangeRef::Addition { location, .. } => format!("A {}", path_from_bytes(location.as_ref())),
        ChangeRef::Deletion { location, .. } => format!("D {}", path_from_bytes(location.as_ref())),
        ChangeRef::Modification {
            location,
            previous_entry_mode,
            entry_mode,
            ..
        } => {
            let prefix = if previous_entry_mode != entry_mode {
                "T"
            } else {
                "M"
            };
            format!("{prefix} {}", path_from_bytes(location.as_ref()))
        }
        ChangeRef::Rewrite {
            source_location,
            location,
            copy,
            ..
        } => {
            let from = path_from_bytes(source_location.as_ref());
            let to = path_from_bytes(location.as_ref());
            if *copy {
                format!("C {from} -> {to}")
            } else {
                format!("R {from} -> {to}")
            }
        }
    }
}

pub(crate) fn format_index_worktree_change(change: &gix::status::index_worktree::Item) -> String {
    use gix::status::index_worktree::Item;
    match change {
        Item::Modification { .. } => match change.summary() {
            Some(summary) => format!(
                "{} {}",
                worktree_summary_code(summary),
                path_from_bytes(change.rela_path().as_ref())
            ),
            None => format!("M {}", path_from_bytes(change.rela_path().as_ref())),
        },
        Item::DirectoryContents { entry, .. } => {
            let path = path_from_bytes(entry.rela_path.as_ref());
            if matches!(entry.status, gix::dir::entry::Status::Untracked) {
                format!("?? {path}")
            } else {
                format!("DIR {path}")
            }
        }
        Item::Rewrite { source, copy, .. } => {
            let from = path_from_bytes(source.rela_path().as_ref());
            let to = path_from_bytes(change.rela_path().as_ref());
            if *copy {
                format!("C {from} -> {to}")
            } else {
                format!("R {from} -> {to}")
            }
        }
    }
}

pub(crate) fn worktree_summary_code(
    summary: gix::status::index_worktree::iter::Summary,
) -> &'static str {
    use gix::status::index_worktree::iter::Summary;
    match summary {
        Summary::Added => "A",
        Summary::Removed => "D",
        Summary::Modified => "M",
        Summary::Copied => "C",
        Summary::Renamed => "R",
        Summary::TypeChange => "T",
        Summary::Conflict => "U",
        Summary::IntentToAdd => "I",
    }
}
