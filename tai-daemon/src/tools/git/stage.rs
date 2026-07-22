use crate::tools::{ToolError, truncate_tool_output};
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{collections::BTreeSet, fmt::Write as _, io, path::Path};

use super::{
    describe_head, load_mutable_index, open_repo, pathspec_patterns, repo_work_dir_display,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GitAddArgs {
    pub repo_path: Option<String>,
    pub pathspec: Vec<String>,
}

pub fn execute_git_add_tool(
    args: &GitAddArgs,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let pathspec = normalize_pathspecs(args.pathspec.clone())?;
    let output = git_add_impl(args.repo_path.as_deref(), pathspec, working_dir)?;
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
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(repo_path, working_dir)?;
    let effective_pathspec = prefix_pathspecs(&repo, repo_path, &pathspec, working_dir)?;
    let mut index = load_mutable_index(&repo)?;
    let paths = collect_paths_to_stage(&repo, &index, &effective_pathspec)?;
    if paths.is_empty() {
        return Err(ToolError::Other(format!(
            "pathspec did not match any tracked or untracked paths: {}",
            humfmt::list(&pathspec)
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
    let diff = super::diff::git_diff_impl(repo_path, true, effective_pathspec, working_dir)?;
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
    working_dir: Option<&Path>,
) -> Result<Vec<String>, ToolError> {
    // Delegate the prefix-computation logic to the shared helper so that
    // both git_add and git_diff handle pathspec-prefixing consistently.
    let prefix = match super::resolve_pathspec_prefix(repo, repo_path, working_dir)? {
        Some(p) => p,
        None => return Ok(pathspec.to_vec()),
    };

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
        .map_err(|e| ToolError::Io(e.to_string()))
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

pub fn describe_git_add_invocation(args: &GitAddArgs) -> String {
    let paths = args.pathspec.join("`, `");
    match &args.repo_path {
        Some(p) => format!("Staging `{}` in repository `{}`.", paths, p),
        None => format!("Staging `{}`.", paths),
    }
}

pub(crate) struct GitAdd;

define_tool!(
    GitAdd,
    "git_add",
    "Stage a file or pathspec in Git.",
    GitAddArgs,
    execute_git_add_tool,
    "git",
    describe_git_add_invocation
);
