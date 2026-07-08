use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
};
use serde::Deserialize;
use std::{collections::BTreeSet, fmt::Write as _, io, path::Path};

use super::{
    describe_head, load_mutable_index, open_repo, pathspec_patterns, repo_work_dir_display,
};

#[derive(Debug, Deserialize)]
struct GitAddArgs {
    repo_path: Option<String>,
    pathspec: Vec<String>,
}

pub fn execute_git_add_tool(arguments_json: &str, cwd: Option<&std::path::Path>) -> ToolResult {
    match execute_git_add_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

pub(super) fn execute_git_add_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let args: GitAddArgs = serde_json::from_str(arguments_json)?;
    let pathspec = normalize_pathspecs(args.pathspec)?;
    let output = git_add_impl(args.repo_path.as_deref(), pathspec, cwd)?;
    Ok(truncate_tool_output(&output))
}

fn normalize_pathspecs(pathspec: Vec<String>) -> Result<Vec<String>, ToolError> {
    let normalized = pathspec
        .into_iter()
        .map(|spec| spec.trim().to_string())
        .filter(|spec| !spec.is_empty())
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        Err(ToolError::Other(
            "pathspec must contain at least one non-empty entry".to_string(),
        ))
    } else {
        Ok(normalized)
    }
}

fn git_add_impl(
    repo_path: Option<&str>,
    pathspec: Vec<String>,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(repo_path, cwd)?;
    let effective_pathspec = prefix_pathspecs(&repo, repo_path, &pathspec)?;
    let mut index = load_mutable_index(&repo)?;
    let paths = collect_paths_to_stage(&repo, &index, &effective_pathspec)?;
    if paths.is_empty() {
        return Err(ToolError::Other(format!(
            "pathspec did not match any tracked or untracked paths: {}",
            pathspec.join(", ")
        )));
    }

    let (mut pipeline, _) = repo.filter_pipeline(None).map_err(io::Error::other)?;
    let mut changed = false;
    for path in &paths {
        changed |= stage_path(&repo, &mut pipeline, &mut index, path.as_bstr())?;
    }

    finalize_index(&mut index)?;

    let mut out = String::new();
    writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
    writeln!(&mut out, "head: {}", describe_head(&repo)?).ok();
    writeln!(&mut out, "staged_paths: {}", paths.len()).ok();
    writeln!(
        &mut out,
        "index_changed: {}",
        if changed { "yes" } else { "no" }
    )
    .ok();
    let diff = super::diff::git_diff_impl(repo_path, true, effective_pathspec, cwd)?;
    writeln!(&mut out).ok();
    writeln!(&mut out, "{diff}").ok();
    Ok(out.trim_end().to_string())
}

fn collect_paths_to_stage(
    repo: &gix::Repository,
    index: &gix::index::File,
    pathspec: &[String],
) -> Result<Vec<BString>, ToolError> {
    let mut paths = BTreeSet::<BString>::new();

    let patterns = pathspec_patterns(pathspec);
    let mut matcher = repo
        .pathspec(
            true,
            patterns.iter().map(|pattern| pattern.as_bstr()),
            true,
            index,
            gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
        )
        .map_err(io::Error::other)?;

    if let Some(entries) = matcher.index_entries_with_paths(index) {
        for (path, _) in entries {
            paths.insert(path.to_owned());
        }
    }

    let iter = repo
        .status(gix::progress::Discard)
        .map_err(io::Error::other)?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_iter(patterns)
        .map_err(io::Error::other)?;

    for item in iter {
        let item = item.map_err(io::Error::other)?;
        paths.insert(item.location().to_owned());
    }

    Ok(paths.into_iter().collect())
}

fn stage_path(
    repo: &gix::Repository,
    pipeline: &mut gix::filter::Pipeline<'_>,
    index: &mut gix::index::File,
    path: &BStr,
) -> Result<bool, ToolError> {
    let previous = current_entry_snapshot(index, path);
    remove_entries_for_path(index, path);

    let maybe_object = pipeline
        .worktree_file_to_object(path, index)
        .map_err(io::Error::other)?;

    match maybe_object {
        Some((id, kind, _)) => {
            let metadata = worktree_metadata(repo, path)?;
            let stat = gix::index::entry::Stat::from_fs(&metadata).map_err(io::Error::other)?;
            index.dangerously_push_entry(
                stat,
                id,
                gix::index::entry::Flags::from(gix::index::entry::Stage::Unconflicted),
                kind.into(),
                path,
            );
            let current = current_entry_snapshot(index, path).ok_or_else(|| {
                ToolError::Other("staged entry not found after insertion".to_string())
            })?;
            Ok(previous.as_ref() != Some(&current))
        }
        None => Ok(previous.is_some()),
    }
}

fn current_entry_snapshot(index: &gix::index::File, path: &BStr) -> Option<IndexEntrySnapshot> {
    index
        .entry_by_path(path)
        .map(|entry| IndexEntrySnapshot::from_entry(path, entry))
}

fn remove_entries_for_path(index: &mut gix::index::File, path: &BStr) {
    index.remove_entries(|_, entry_path, _| entry_path == path);
}

fn prefix_pathspecs(
    repo: &gix::Repository,
    repo_path: Option<&str>,
    pathspec: &[String],
) -> Result<Vec<String>, ToolError> {
    let Some(workdir) = repo.workdir() else {
        return Ok(pathspec.to_vec());
    };
    let Some(repo_path) = repo_path else {
        return Ok(pathspec.to_vec());
    };

    let trimmed = repo_path.trim();
    if trimmed.is_empty() || trimmed == "." {
        return Ok(pathspec.to_vec());
    }

    let candidate = Path::new(trimmed);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(ToolError::Io)?
            .join(candidate)
    };

    let Ok(prefix) = absolute.strip_prefix(workdir) else {
        return Ok(pathspec.to_vec());
    };
    if prefix.as_os_str().is_empty() {
        return Ok(pathspec.to_vec());
    }

    let prefix = prefix
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Ok(pathspec
        .iter()
        .map(|spec| {
            if spec == "." || spec == "./" {
                prefix.clone()
            } else {
                format!("{prefix}/{}", spec.trim_start_matches("./"))
            }
        })
        .collect())
}

fn worktree_metadata(
    repo: &gix::Repository,
    path: &BStr,
) -> Result<gix::index::fs::Metadata, ToolError> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| ToolError::Other("repository has no worktree".to_string()))?;
    gix::index::fs::Metadata::from_path_no_follow(&workdir.join(gix::path::from_bstr(path)))
        .map_err(ToolError::Io)
}

fn finalize_index(index: &mut gix::index::File) -> Result<(), ToolError> {
    index.sort_entries();
    let _ = index.remove_tree();
    index.write(Default::default()).map_err(io::Error::other)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntrySnapshot {
    id: ObjectId,
    mode: gix::index::entry::Mode,
    flags: gix::index::entry::Flags,
    stat: gix::index::entry::Stat,
    path: BString,
}

impl IndexEntrySnapshot {
    fn from_entry(path: &BStr, entry: &gix::index::Entry) -> Self {
        Self {
            id: entry.id,
            mode: entry.mode,
            flags: entry.flags,
            stat: entry.stat,
            path: path.to_owned(),
        }
    }
}

define_tool!(
    GitAdd,
    "git_add",
    "Stage a file or pathspec in Git.",
    execute_git_add_tool,
    serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"pathspec":{"type":"array","items":{"type":"string"},"description":"Files or pathspecs to stage"}},"required":["pathspec"],"additionalProperties":false}),
    "git"
);
