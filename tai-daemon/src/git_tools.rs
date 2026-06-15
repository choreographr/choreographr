use crate::{ToolResult, truncate_tool_output};
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

pub(crate) async fn execute_git_status_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitRepoArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    map_io_result(git_status_impl(args.repo_path.as_deref()))
}

pub(crate) async fn execute_git_diff_tool(arguments_json: &str) -> ToolResult {
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

pub(crate) async fn execute_git_log_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitLogArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    map_io_result(git_log_impl(
        args.repo_path.as_deref(),
        args.limit.unwrap_or(10).clamp(1, 100),
    ))
}

pub(crate) async fn execute_git_add_tool(arguments_json: &str) -> ToolResult {
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

pub(crate) async fn execute_git_commit_tool(arguments_json: &str) -> ToolResult {
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

pub(crate) async fn execute_git_push_tool(arguments_json: &str) -> ToolResult {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn unique_repo_dir(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("tai-git-tool-{name}-{unique}"))
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .expect("run git");
        assert!(status.success(), "git {:?} failed with {status}", args);
    }

    fn git_output(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn init_repo() -> std::path::PathBuf {
        let dir = unique_repo_dir("repo");
        std::fs::create_dir_all(&dir).expect("create repo dir");
        git(&dir, &["init", "-b", "main"]);
        git(&dir, &["config", "user.name", "Tai Test"]);
        git(&dir, &["config", "user.email", "tai@example.com"]);
        dir
    }

    fn init_bare_remote() -> std::path::PathBuf {
        let dir = unique_repo_dir("remote");
        std::fs::create_dir_all(&dir).expect("create remote dir");
        git(&dir, &["init", "--bare", "--initial-branch=main"]);
        dir
    }

    fn git_output_result(repo: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git")
    }

    #[tokio::test]
    async fn git_status_reports_staged_unstaged_and_untracked_changes() {
        let repo = init_repo();
        std::fs::write(repo.join("tracked.txt"), "one\n").expect("write tracked");
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "-m", "initial commit"]);

        std::fs::write(repo.join("tracked.txt"), "two\n").expect("modify tracked");
        std::fs::write(repo.join("staged.txt"), "stage me\n").expect("write staged");
        std::fs::write(repo.join("untracked.txt"), "new\n").expect("write untracked");
        git(&repo, &["add", "staged.txt"]);

        let result =
            execute_git_status_tool(&serde_json::json!({ "repo_path": repo }).to_string()).await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("head: main"));
        assert!(result.content.contains("staged:"));
        assert!(result.content.contains("A staged.txt"));
        assert!(result.content.contains("unstaged:"));
        assert!(result.content.contains("M tracked.txt"));
        assert!(result.content.contains("untracked:"));
        assert!(result.content.contains("?? untracked.txt"));

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_diff_reports_worktree_and_cached_changes() {
        let repo = init_repo();
        std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-m", "initial commit"]);

        std::fs::write(repo.join("file.txt"), "two\n").expect("modify file");
        std::fs::write(repo.join("added.txt"), "new\n").expect("write added");
        git(&repo, &["add", "added.txt"]);

        let worktree = execute_git_diff_tool(
            &serde_json::json!({ "repo_path": repo, "cached": false }).to_string(),
        )
        .await;
        assert!(!worktree.is_error, "{}", worktree.content);
        assert!(worktree.content.contains("mode: working tree"));
        assert!(worktree.content.contains("M file.txt"));

        let cached = execute_git_diff_tool(
            &serde_json::json!({ "repo_path": repo, "cached": true }).to_string(),
        )
        .await;
        assert!(!cached.is_error, "{}", cached.content);
        assert!(cached.content.contains("mode: staged"));
        assert!(cached.content.contains("A added.txt"));

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_log_reports_recent_commits() {
        let repo = init_repo();
        std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-m", "first commit"]);
        std::fs::write(repo.join("file.txt"), "two\n").expect("rewrite file");
        git(&repo, &["commit", "-am", "second commit"]);

        let result =
            execute_git_log_tool(&serde_json::json!({ "repo_path": repo, "limit": 2 }).to_string())
                .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("head: main"));
        assert!(
            result
                .content
                .contains("Tai Test <tai@example.com> second commit")
        );
        assert!(
            result
                .content
                .contains("Tai Test <tai@example.com> first commit")
        );

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_add_stages_modified_untracked_and_deleted_paths() {
        let repo = init_repo();
        std::fs::write(repo.join("tracked.txt"), "one\n").expect("write tracked");
        std::fs::write(repo.join("delete-me.txt"), "gone\n").expect("write delete me");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial commit"]);

        std::fs::write(repo.join("tracked.txt"), "two\n").expect("modify tracked");
        std::fs::write(repo.join("new.txt"), "brand new\n").expect("write new");
        std::fs::remove_file(repo.join("delete-me.txt")).expect("remove file");

        let result = execute_git_add_tool(
            &serde_json::json!({
                "repo_path": repo,
                "pathspec": ["tracked.txt", "new.txt", "delete-me.txt"]
            })
            .to_string(),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        let cached = git_output(&repo, &["diff", "--cached", "--name-status"]);
        assert!(cached.contains("M\ttracked.txt"), "{cached}");
        assert!(cached.contains("A\tnew.txt"), "{cached}");
        assert!(cached.contains("D\tdelete-me.txt"), "{cached}");

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_add_accepts_clean_tracked_paths_as_noop() {
        let repo = init_repo();
        std::fs::write(repo.join("tracked.txt"), "one\n").expect("write tracked");
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "-m", "initial commit"]);

        let result = execute_git_add_tool(
            &serde_json::json!({ "repo_path": repo, "pathspec": ["tracked.txt"] }).to_string(),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("index_changed: no"));
        assert!(result.content.contains("no changes"));

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_add_works_from_subdirectory_repo_path() {
        let repo = init_repo();
        std::fs::create_dir_all(repo.join("src")).expect("create src");
        std::fs::write(repo.join("src/lib.rs"), "pub fn one() {}\n").expect("write file");

        let subdir = repo.join("src");
        let result = execute_git_add_tool(
            &serde_json::json!({ "repo_path": subdir, "pathspec": ["lib.rs"] }).to_string(),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        let cached = git_output(&repo, &["diff", "--cached", "--name-status"]);
        assert!(cached.contains("A\tsrc/lib.rs"), "{cached}");

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_add_rejects_empty_and_unmatched_pathspecs() {
        let repo = init_repo();

        let empty = execute_git_add_tool(
            &serde_json::json!({ "repo_path": repo, "pathspec": ["", "  "] }).to_string(),
        )
        .await;
        assert!(empty.is_error);
        assert!(
            empty
                .content
                .contains("pathspec must contain at least one non-empty entry")
        );

        let unmatched = execute_git_add_tool(
            &serde_json::json!({ "repo_path": repo, "pathspec": ["missing.txt"] }).to_string(),
        )
        .await;
        assert!(unmatched.is_error);
        assert!(
            unmatched
                .content
                .contains("pathspec did not match any tracked or untracked paths")
        );

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_commit_creates_commit_from_staged_index() {
        let repo = init_repo();
        std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
        execute_git_add_tool(
            &serde_json::json!({ "repo_path": repo, "pathspec": ["file.txt"] }).to_string(),
        )
        .await;

        let result = execute_git_commit_tool(
            &serde_json::json!({ "repo_path": repo, "message": "Add file" }).to_string(),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("head: main"));
        assert!(
            result
                .content
                .contains("Tai Test <tai@example.com> Add file")
        );
        let log = git_output(&repo, &["log", "--format=%s", "-1"]);
        assert_eq!(log.trim(), "Add file");

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_commit_supports_multiline_messages_and_allow_empty() {
        let repo = init_repo();

        let empty_commit = execute_git_commit_tool(
            &serde_json::json!({
                "repo_path": repo,
                "message": "Initial empty\n\nBody",
                "allow_empty": true
            })
            .to_string(),
        )
        .await;
        assert!(!empty_commit.is_error, "{}", empty_commit.content);
        assert!(empty_commit.content.contains("Initial empty"));

        let body = git_output(&repo, &["log", "--format=%B", "-1"]);
        assert!(body.starts_with("Initial empty\n\nBody"), "{body}");

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_commit_rejects_blank_message_and_missing_staged_changes() {
        let repo = init_repo();

        let blank = execute_git_commit_tool(
            &serde_json::json!({ "repo_path": repo, "message": "   " }).to_string(),
        )
        .await;
        assert!(blank.is_error);
        assert!(blank.content.contains("commit message must not be empty"));

        let no_changes = execute_git_commit_tool(
            &serde_json::json!({ "repo_path": repo, "message": "Nothing" }).to_string(),
        )
        .await;
        assert!(no_changes.is_error);
        assert!(no_changes.content.contains("no staged changes to commit"));

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_commit_rejects_conflicted_index() {
        let repo = init_repo();
        std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-m", "base"]);

        git(&repo, &["checkout", "-b", "feature"]);
        std::fs::write(repo.join("file.txt"), "feature\n").expect("write feature");
        git(&repo, &["commit", "-am", "feature change"]);

        git(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("file.txt"), "main\n").expect("write main");
        git(&repo, &["commit", "-am", "main change"]);

        let output = Command::new("git")
            .args(["merge", "feature"])
            .current_dir(&repo)
            .output()
            .expect("run git merge");
        assert!(!output.status.success(), "merge unexpectedly succeeded");

        let result = execute_git_commit_tool(
            &serde_json::json!({ "repo_path": repo, "message": "should fail" }).to_string(),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("unresolved index conflicts"));

        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn git_push_pushes_branch_to_remote_and_sets_upstream() {
        let repo = init_repo();
        let remote = init_bare_remote();
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("utf8 remote"),
            ],
        );
        std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-m", "initial commit"]);

        let result = execute_git_push_tool(
            &serde_json::json!({
                "repo_path": repo,
                "remote": "origin",
                "set_upstream": true
            })
            .to_string(),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("remote: origin"));
        assert!(result.content.contains("branch: main"));
        assert!(result.content.contains("set_upstream: yes"));
        assert!(result.content.contains("result: pushed"));
        let remote_head = git_output(&remote, &["rev-parse", "main"]);
        let local_head = git_output(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(remote_head.trim(), local_head.trim());
        let upstream = git_output(
            &repo,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        );
        assert_eq!(upstream.trim(), "origin/main");

        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn git_push_supports_dry_run() {
        let repo = init_repo();
        let remote = init_bare_remote();
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("utf8 remote"),
            ],
        );
        std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-m", "initial commit"]);

        let result = execute_git_push_tool(
            &serde_json::json!({
                "repo_path": repo,
                "remote": "origin",
                "branch": "main",
                "dry_run": true
            })
            .to_string(),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("dry_run: yes"));
        assert!(result.content.contains("result: dry run complete"));
        let remote_lookup = git_output_result(&remote, &["rev-parse", "main"]);
        assert!(
            !remote_lookup.status.success(),
            "dry run should not update remote"
        );

        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn git_push_rejects_detached_head_without_branch() {
        let repo = init_repo();
        let remote = init_bare_remote();
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("utf8 remote"),
            ],
        );
        std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-m", "initial commit"]);
        let head = git_output(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", head.trim()]);

        let result = execute_git_push_tool(
            &serde_json::json!({ "repo_path": repo, "remote": "origin" }).to_string(),
        )
        .await;

        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("branch must be provided when HEAD is detached")
        );

        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn git_push_reports_push_failure() {
        let repo = init_repo();
        std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-m", "initial commit"]);

        let result = execute_git_push_tool(
            &serde_json::json!({
                "repo_path": repo,
                "remote": "origin",
                "branch": "main"
            })
            .to_string(),
        )
        .await;

        assert!(result.is_error);
        assert!(result.content.contains("result: push failed"));
        assert!(result.content.contains("remote: origin"));

        let _ = std::fs::remove_dir_all(repo);
    }
}
