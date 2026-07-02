use crate::{Tool, ToolExecutionOutput, ToolResult, truncate_tool_output};
use async_trait::async_trait;
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    prelude::ObjectIdExt,
    progress::Discard,
    status::{Item as StatusItem, UntrackedFiles},
    worktree::IndexPersistedOrInMemory,
};
use serde::Deserialize;
use std::{collections::BTreeSet, fmt::Write as _, io, path::Path};

#[derive(Debug, Deserialize)]
struct GitRepoArgs {
    repo_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitDiffArgs {
    repo_path: Option<String>,
    cached: Option<bool>,
    pathspec: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct GitLogArgs {
    repo_path: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct GitAddArgs {
    repo_path: Option<String>,
    pathspec: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GitCommitArgs {
    repo_path: Option<String>,
    message: String,
    allow_empty: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GitPushArgs {
    repo_path: Option<String>,
    remote: String,
    branch: Option<String>,
    set_upstream: Option<bool>,
    force_with_lease: Option<bool>,
    dry_run: Option<bool>,
}

pub async fn execute_git_status_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitRepoArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    map_io_result(git_status_impl(args.repo_path.as_deref()))
}

pub async fn execute_git_diff_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitDiffArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    map_io_result(git_diff_impl(
        args.repo_path.as_deref(),
        args.cached.unwrap_or(false),
        args.pathspec.unwrap_or_default(),
    ))
}

pub async fn execute_git_log_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitLogArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    map_io_result(git_log_impl(
        args.repo_path.as_deref(),
        args.limit.unwrap_or(10).clamp(1, 100),
    ))
}

pub async fn execute_git_add_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitAddArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    let pathspec = match normalize_pathspecs(args.pathspec) {
        Ok(pathspec) => pathspec,
        Err(error) => {
            return ToolResult {
                content: error.to_string(),
                is_error: true,
            };
        }
    };

    map_io_result(git_add_impl(args.repo_path.as_deref(), pathspec))
}

pub async fn execute_git_commit_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitCommitArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    map_io_result(git_commit_impl(
        args.repo_path.as_deref(),
        &args.message,
        args.allow_empty.unwrap_or(false),
    ))
}

pub async fn execute_git_push_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitPushArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    map_io_result(git_push_impl(
        args.repo_path.as_deref(),
        &args.remote,
        args.branch.as_deref(),
        args.set_upstream.unwrap_or(false),
        args.force_with_lease.unwrap_or(false),
        args.dry_run.unwrap_or(false),
    ))
}

fn invalid_arguments(error: serde_json::Error) -> ToolResult {
    ToolResult {
        content: format!("invalid arguments: {error}"),
        is_error: true,
    }
}

fn map_io_result(result: io::Result<String>) -> ToolResult {
    match result {
        Ok(content) => ToolResult {
            content: truncate_tool_output(&content),
            is_error: false,
        },
        Err(error) => ToolResult {
            content: error.to_string(),
            is_error: true,
        },
    }
}

fn normalize_pathspecs(pathspec: Vec<String>) -> io::Result<Vec<String>> {
    let normalized = pathspec
        .into_iter()
        .map(|spec| spec.trim().to_string())
        .filter(|spec| !spec.is_empty())
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pathspec must contain at least one non-empty entry",
        ))
    } else {
        Ok(normalized)
    }
}

fn git_status_impl(repo_path: Option<&str>) -> io::Result<String> {
    let repo = open_repo(repo_path)?;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();

    let iter = repo
        .status(Discard)
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

fn git_diff_impl(
    repo_path: Option<&str>,
    cached: bool,
    pathspec: Vec<String>,
) -> io::Result<String> {
    let repo = open_repo(repo_path)?;
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

fn git_log_impl(repo_path: Option<&str>, limit: usize) -> io::Result<String> {
    let repo = open_repo(repo_path)?;
    let head = match repo.head_id() {
        Ok(head) => head,
        Err(_) => return Ok("repository has no commits yet".to_string()),
    };

    let mut walk = repo
        .rev_walk([head.detach()])
        .all()
        .map_err(io::Error::other)?;

    let mut out = String::new();
    writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
    writeln!(&mut out, "head: {}", describe_head(&repo)?).ok();

    let mut count = 0usize;
    for info in (&mut walk).take(limit) {
        let info = info.map_err(io::Error::other)?;
        let commit = info.object().map_err(io::Error::other)?;
        let short_id = commit.short_id().map_err(io::Error::other)?;
        let decoded = commit.decode().map_err(io::Error::other)?;
        let author = commit.author().map_err(io::Error::other)?;
        let title = decoded
            .message
            .lines()
            .next()
            .map(|line| String::from_utf8_lossy(line.trim()).into_owned())
            .unwrap_or_default();
        writeln!(
            &mut out,
            "{} {} <{}> {}",
            short_id, author.name, author.email, title
        )
        .ok();
        count += 1;
    }

    if count == 0 {
        writeln!(&mut out, "repository has no commits yet").ok();
    }

    Ok(out.trim_end().to_string())
}

fn git_add_impl(repo_path: Option<&str>, pathspec: Vec<String>) -> io::Result<String> {
    let repo = open_repo(repo_path)?;
    let effective_pathspec = prefix_pathspecs(&repo, repo_path, &pathspec)?;
    let mut index = load_mutable_index(&repo)?;
    let paths = collect_paths_to_stage(&repo, &index, &effective_pathspec)?;
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "pathspec did not match any tracked or untracked paths: {}",
                pathspec.join(", ")
            ),
        ));
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
    let diff = git_diff_impl(repo_path, true, effective_pathspec)?;
    writeln!(&mut out).ok();
    writeln!(&mut out, "{diff}").ok();
    Ok(out.trim_end().to_string())
}

fn git_commit_impl(
    repo_path: Option<&str>,
    message: &str,
    allow_empty: bool,
) -> io::Result<String> {
    let repo = open_repo(repo_path)?;
    let message = message.trim();
    if message.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "commit message must not be empty",
        ));
    }

    let index = load_mutable_index(&repo)?;
    ensure_index_has_no_conflicts(&index)?;

    if !allow_empty && collect_cached_diff_lines(&repo, &[] as &[String])?.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no staged changes to commit",
        ));
    }
    let tree_id = write_tree_from_index(&repo, &index)?;
    let parents = current_head_parents(&repo)?;

    repo.commit("HEAD", message, tree_id, parents)
        .map_err(io::Error::other)?;

    git_log_impl(repo_path, 1)
}

fn git_push_impl(
    repo_path: Option<&str>,
    remote: &str,
    branch: Option<&str>,
    set_upstream: bool,
    force_with_lease: bool,
    dry_run: bool,
) -> io::Result<String> {
    let repo = open_repo(repo_path)?;
    let remote = normalize_nonempty_argument(remote, "remote")?;
    let branch = match branch {
        Some(branch) => normalize_nonempty_argument(branch, "branch")?.to_string(),
        None => current_branch_name(&repo)?,
    };

    let mut args = vec!["push".to_string()];
    if dry_run {
        args.push("--dry-run".to_string());
    }
    if set_upstream {
        args.push("--set-upstream".to_string());
    }
    if force_with_lease {
        args.push("--force-with-lease".to_string());
    }
    args.push(remote.to_string());
    args.push(branch.clone());

    let output = run_git_command(&repo, &args)?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let mut out = String::new();
        writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
        writeln!(&mut out, "head: {}", describe_head(&repo)?).ok();
        writeln!(&mut out, "remote: {remote}").ok();
        writeln!(&mut out, "branch: {branch}").ok();
        writeln!(&mut out, "dry_run: {}", yes_no(dry_run)).ok();
        writeln!(&mut out, "set_upstream: {}", yes_no(set_upstream)).ok();
        writeln!(&mut out, "force_with_lease: {}", yes_no(force_with_lease)).ok();
        writeln!(&mut out, "result: push failed").ok();
        append_command_output(&mut out, "stdout", &stdout);
        append_command_output(&mut out, "stderr", &stderr);
        return Err(io::Error::other(out.trim_end().to_string()));
    }

    let mut out = String::new();
    writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
    writeln!(&mut out, "head: {}", describe_head(&repo)?).ok();
    writeln!(&mut out, "remote: {remote}").ok();
    writeln!(&mut out, "branch: {branch}").ok();
    writeln!(&mut out, "dry_run: {}", yes_no(dry_run)).ok();
    writeln!(&mut out, "set_upstream: {}", yes_no(set_upstream)).ok();
    writeln!(&mut out, "force_with_lease: {}", yes_no(force_with_lease)).ok();
    writeln!(
        &mut out,
        "result: {}",
        if dry_run {
            "dry run complete"
        } else {
            "pushed"
        }
    )
    .ok();
    append_command_output(&mut out, "stdout", &stdout);
    append_command_output(&mut out, "stderr", &stderr);
    Ok(out.trim_end().to_string())
}

fn open_repo(repo_path: Option<&str>) -> io::Result<gix::Repository> {
    let path = repo_path.unwrap_or(".").trim();
    let path = if path.is_empty() { "." } else { path };
    gix::discover(path).map_err(|error| {
        io::Error::other(format!(
            "failed to open git repository from {}: {error}",
            Path::new(path).display()
        ))
    })
}

fn normalize_nonempty_argument<'a>(value: &'a str, name: &str) -> io::Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must not be empty"),
        ))
    } else {
        Ok(value)
    }
}

fn current_branch_name(repo: &gix::Repository) -> io::Result<String> {
    repo.head_name()
        .map_err(io::Error::other)?
        .map(|name| name.shorten().to_string())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "branch must be provided when HEAD is detached",
            )
        })
}

fn describe_head(repo: &gix::Repository) -> io::Result<String> {
    if let Some(name) = repo.head_name().map_err(io::Error::other)? {
        return Ok(name.shorten().to_string());
    }
    match repo.head_id() {
        Ok(id) => Ok(format!("detached at {}", shorten_id(repo, id.detach())?)),
        Err(_) => Ok("unborn HEAD".to_string()),
    }
}

fn shorten_id(repo: &gix::Repository, id: ObjectId) -> io::Result<String> {
    Ok(id
        .attach(repo)
        .shorten()
        .map_err(io::Error::other)?
        .to_string())
}

fn repo_work_dir(repo: &gix::Repository) -> &Path {
    repo.workdir().unwrap_or_else(|| repo.git_dir())
}

fn repo_work_dir_display(repo: &gix::Repository) -> String {
    repo_work_dir(repo).display().to_string()
}

fn run_git_command(repo: &gix::Repository, args: &[String]) -> io::Result<std::process::Output> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(repo_work_dir(repo))
        .output()
        .map_err(|error| io::Error::other(format!("failed to run git {}: {error}", args.join(" "))))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn append_command_output(out: &mut String, label: &str, content: &str) {
    if content.is_empty() {
        return;
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{label}:");
    let _ = writeln!(out, "{content}");
}

fn collect_worktree_diff_lines(
    repo: &gix::Repository,
    pathspec: &[String],
) -> io::Result<Vec<String>> {
    let patterns = pathspec_patterns(pathspec);
    let iter = repo
        .status(Discard)
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

fn collect_cached_diff_lines(
    repo: &gix::Repository,
    pathspec: &[String],
) -> io::Result<Vec<String>> {
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

fn load_mutable_index(repo: &gix::Repository) -> io::Result<gix::index::File> {
    match repo
        .index_or_load_from_head_or_empty()
        .map_err(io::Error::other)?
    {
        IndexPersistedOrInMemory::Persisted(index) => Ok((**index).clone()),
        IndexPersistedOrInMemory::InMemory(index) => Ok(index),
    }
}

fn pathspec_patterns(pathspec: &[String]) -> Vec<BString> {
    pathspec
        .iter()
        .map(|spec| BString::from(spec.as_str()))
        .collect()
}

fn collect_paths_to_stage(
    repo: &gix::Repository,
    index: &gix::index::File,
    pathspec: &[String],
) -> io::Result<Vec<BString>> {
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
        .status(Discard)
        .map_err(io::Error::other)?
        .untracked_files(UntrackedFiles::Files)
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
) -> io::Result<bool> {
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
            let current = current_entry_snapshot(index, path)
                .expect("entry was just inserted for staged path");
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
) -> io::Result<Vec<String>> {
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
        std::env::current_dir()?.join(candidate)
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

fn worktree_metadata(repo: &gix::Repository, path: &BStr) -> io::Result<gix::index::fs::Metadata> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| io::Error::other("repository has no worktree"))?;
    gix::index::fs::Metadata::from_path_no_follow(&workdir.join(gix::path::from_bstr(path)))
        .map_err(io::Error::other)
}

fn finalize_index(index: &mut gix::index::File) -> io::Result<()> {
    index.sort_entries();
    let _ = index.remove_tree();
    index.write(Default::default()).map_err(io::Error::other)
}

fn ensure_index_has_no_conflicts(index: &gix::index::File) -> io::Result<()> {
    if let Some(path) = index
        .entries()
        .iter()
        .find(|entry| entry.stage() != gix::index::entry::Stage::Unconflicted)
        .map(|entry| path_from_bytes(entry.path(index).as_ref()))
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot commit with unresolved index conflicts at {path}"),
        ))
    } else {
        Ok(())
    }
}

fn write_tree_from_index(repo: &gix::Repository, index: &gix::index::File) -> io::Result<ObjectId> {
    let mut editor = repo.empty_tree().edit().map_err(io::Error::other)?;
    for entry in index.entries() {
        if entry.stage() != gix::index::entry::Stage::Unconflicted {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot write tree with conflicted index entry at {}",
                    path_from_bytes(entry.path(index).as_ref())
                ),
            ));
        }
        let kind = entry.mode.to_tree_entry_mode().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported index entry mode {} at {}",
                    entry.mode.bits(),
                    path_from_bytes(entry.path(index).as_ref())
                ),
            )
        })?;
        editor
            .upsert(entry.path(index).to_owned(), kind.into(), entry.id)
            .map_err(io::Error::other)?;
    }
    editor
        .write()
        .map(|id| id.detach())
        .map_err(io::Error::other)
}

fn current_head_parents(repo: &gix::Repository) -> io::Result<Vec<ObjectId>> {
    match repo.head_id() {
        Ok(head) => Ok(vec![head.detach()]),
        Err(_) => Ok(Vec::new()),
    }
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

fn format_index_worktree_change(change: &gix::status::index_worktree::Item) -> String {
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

fn format_tree_index_change(change: &gix::diff::index::Change) -> String {
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

fn worktree_summary_code(summary: gix::status::index_worktree::iter::Summary) -> &'static str {
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

fn pathspec_matches(pathspec: &[String], path: &str) -> bool {
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

fn simple_glob_matches(pattern: &str, text: &str) -> bool {
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

fn path_from_bytes(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn sort_and_dedup(lines: &mut Vec<String>) {
    lines.sort();
    lines.dedup();
}

fn write_section(out: &mut String, title: &str, lines: &[String]) {
    let _ = writeln!(out, "{title}:");
    if lines.is_empty() {
        let _ = writeln!(out, "  (none)");
        return;
    }
    for line in lines {
        let _ = writeln!(out, "  {line}");
    }
}

pub(crate) struct GitStatus;

#[async_trait]
impl Tool for GitStatus {
    fn name(&self) -> &'static str { "git_status" }
    fn description(&self) -> &'static str { "Show the status of the Git repository containing the given path." }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."}},"additionalProperties":false})
    }
    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        ToolExecutionOutput { result: execute_git_status_tool(arguments_json).await, image: None }
    }
}

pub(crate) struct GitDiff;

#[async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &'static str { "git_diff" }
    fn description(&self) -> &'static str { "Show the diff for a file or repository." }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"cached":{"type":"boolean","description":"Show staged (cached) changes instead of worktree changes","default":false},"pathspec":{"type":"array","items":{"type":"string"},"description":"Optional pathspecs to filter"},"additionalProperties":false}})
    }
    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        ToolExecutionOutput { result: execute_git_diff_tool(arguments_json).await, image: None }
    }
}

pub(crate) struct GitLog;

#[async_trait]
impl Tool for GitLog {
    fn name(&self) -> &'static str { "git_log" }
    fn description(&self) -> &'static str { "Show recent Git commits for the repository containing the given path." }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"max_count":{"type":"integer","minimum":1,"maximum":100,"default":10}},"additionalProperties":false})
    }
    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        ToolExecutionOutput { result: execute_git_log_tool(arguments_json).await, image: None }
    }
}

pub(crate) struct GitAdd;

#[async_trait]
impl Tool for GitAdd {
    fn name(&self) -> &'static str { "git_add" }
    fn description(&self) -> &'static str { "Stage a file or pathspec in Git." }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"pathspec":{"type":"array","items":{"type":"string"},"description":"Files or pathspecs to stage"}},"required":["pathspec"],"additionalProperties":false})
    }
    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        ToolExecutionOutput { result: execute_git_add_tool(arguments_json).await, image: None }
    }
}

pub(crate) struct GitCommit;

#[async_trait]
impl Tool for GitCommit {
    fn name(&self) -> &'static str { "git_commit" }
    fn description(&self) -> &'static str { "Create a Git commit from the current index." }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"message":{"type":"string","description":"Commit message"}},"required":["message"],"additionalProperties":false})
    }
    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        ToolExecutionOutput { result: execute_git_commit_tool(arguments_json).await, image: None }
    }
}

pub(crate) struct GitPush;

#[async_trait]
impl Tool for GitPush {
    fn name(&self) -> &'static str { "git_push" }
    fn description(&self) -> &'static str { "Push to a Git remote branch." }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"remote":{"type":"string","description":"Remote name","default":"origin"},"branch":{"type":"string","description":"Remote branch name"},"set_upstream":{"type":"boolean","description":"Set upstream tracking reference","default":false},"force_with_lease":{"type":"boolean","description":"Force push with lease (safe force push)","default":false},"dry_run":{"type":"boolean","description":"Simulate push without sending data","default":false}},"required":[],"additionalProperties":false})
    }
    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        ToolExecutionOutput { result: execute_git_push_tool(arguments_json).await, image: None }
    }
}

