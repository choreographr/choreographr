use crate::{ToolResult, truncate_tool_output};
use gix::{
    ObjectId,
    bstr::{BString, ByteSlice},
    prelude::ObjectIdExt,
    progress::Discard,
    status::{Item as StatusItem, UntrackedFiles},
};
use serde::Deserialize;
use std::{fmt::Write as _, io, path::Path};

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

pub(crate) async fn execute_git_status_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitRepoArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    match git_status_impl(args.repo_path.as_deref()) {
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

pub(crate) async fn execute_git_diff_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitDiffArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    match git_diff_impl(
        args.repo_path.as_deref(),
        args.cached.unwrap_or(false),
        args.pathspec.unwrap_or_default(),
    ) {
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

pub(crate) async fn execute_git_log_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<GitLogArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => return invalid_arguments(error),
    };

    match git_log_impl(
        args.repo_path.as_deref(),
        args.limit.unwrap_or(10).clamp(1, 100),
    ) {
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

fn invalid_arguments(error: serde_json::Error) -> ToolResult {
    ToolResult {
        content: format!("invalid arguments: {error}"),
        is_error: true,
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

fn repo_work_dir_display(repo: &gix::Repository) -> String {
    repo.workdir()
        .unwrap_or_else(|| repo.git_dir())
        .display()
        .to_string()
}

fn collect_worktree_diff_lines(
    repo: &gix::Repository,
    pathspec: &[String],
) -> io::Result<Vec<String>> {
    let patterns = pathspec
        .iter()
        .map(|p| BString::from(p.as_str()))
        .collect::<Vec<_>>();
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

    fn init_repo() -> std::path::PathBuf {
        let dir = unique_repo_dir("repo");
        std::fs::create_dir_all(&dir).expect("create repo dir");
        git(&dir, &["init", "-b", "main"]);
        git(&dir, &["config", "user.name", "Tai Test"]);
        git(&dir, &["config", "user.email", "tai@example.com"]);
        dir
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
}
