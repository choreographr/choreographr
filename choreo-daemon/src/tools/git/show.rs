use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use gix::bstr::ByteSlice;
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::tools::{ToolError, truncate_tool_output};

use super::open_repo;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GitShowArgs {
    /// Path to the git repository. Defaults to the session working directory.
    pub repo_path: Option<String>,
    /// Revision, ref, or object SHA to show. Accepts branch names, tag names,
    /// commit SHAs, relative refs like HEAD~3, or raw object hashes. Defaults to HEAD.
    pub revision: Option<String>,
    /// Optional file path within the revision. When set, shows only this file's
    /// content at the given revision instead of the full object.
    pub path: Option<String>,
    /// If true and the object resolves to a commit, include a unified diff
    /// showing the changes introduced by that commit.
    pub diff: Option<bool>,
}

pub fn execute_git_show_tool(
    args: &GitShowArgs,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    debug!(?args.repo_path, revision = ?args.revision, "executing git_show");
    let output = git_show_impl(args, working_dir)?;
    debug!(output_len = output.len(), "git_show completed");
    Ok(truncate_tool_output(&output))
}

/// Resolve a revision string to a parsed git object, returning both the id
/// and the resolved object.
fn parse_revision<'a>(
    repo: &'a gix::Repository,
    revision: &str,
) -> Result<(gix::Id<'a>, gix::Object<'a>), ToolError> {
    let id = repo
        .rev_parse_single(revision)
        .map_err(|e| ToolError::Other(format!("failed to parse revision '{revision}': {e}")))?;
    let object = id
        .object()
        .map_err(|e| ToolError::Other(format!("failed to find object at '{revision}': {e}")))?;
    Ok((id, object))
}

fn git_show_impl(
    args: &GitShowArgs,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(args.repo_path.as_deref(), working_dir)?;
    let revision = args.revision.as_deref().unwrap_or("HEAD");

    debug!(revision, "resolved revision for git_show");

    if let Some(path) = &args.path {
        return show_path(&repo, revision, path);
    }

    let (_id, object) = parse_revision(&repo, revision)?;

    match object.kind {
        gix::object::Kind::Commit => show_commit(&repo, &object, args.diff.unwrap_or(false)),
        gix::object::Kind::Tree => show_tree(&object),
        gix::object::Kind::Blob => show_blob(&object, None),
        gix::object::Kind::Tag => show_tag(&repo, &object),
    }
}

/// Show a specific file (or tree entry) at a given revision.
fn show_path(repo: &gix::Repository, revision: &str, path: &str) -> Result<String, ToolError> {
    let (_id, object) = parse_revision(repo, revision)?;

    let tree = match object.kind {
        // `peel_to_tree` safely unwraps commits (returning their tree) and
        // passes trees through unmodified — unlike `into_commit().tree()`,
        // it cannot panic on kind mismatch.
        gix::object::Kind::Commit | gix::object::Kind::Tree => object
            .peel_to_tree()
            .map_err(|e| ToolError::Other(format!("failed to get tree for path lookup: {e}")))?,
        kind => {
            return Err(ToolError::Other(format!(
                "cannot look up path at a {kind} object"
            )));
        }
    };

    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(|e| ToolError::Other(format!("failed to look up path '{path}': {e}")))?
        .ok_or_else(|| {
            ToolError::Other(format!("path '{path}' not found at revision '{revision}'"))
        })?;

    let entry_object = entry
        .object()
        .map_err(|e| ToolError::Other(format!("failed to get object for '{path}': {e}")))?;

    match entry_object.kind {
        gix::object::Kind::Blob => show_blob(&entry_object, Some(path)),
        gix::object::Kind::Tree => show_tree(&entry_object),
        kind => Err(ToolError::Other(format!(
            "path '{path}' resolves to a {kind} object at revision '{revision}'"
        ))),
    }
}

/// Format a `gix::date::Time` in the style of `git log`.
///
/// chrono handles all calendar arithmetic (leap years, negative timestamps
/// before 1970, weekday calculation) so we don't need to reinvent it.
fn format_date(time: &gix::date::Time) -> String {
    let dt: DateTime<Utc> = DateTime::from_timestamp(time.seconds, 0).unwrap_or_default();
    let offset_secs = time.offset;
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let offset_mins_total = offset_secs.unsigned_abs() / 60;
    let offset_hours = offset_mins_total / 60;
    let offset_mins_rem = offset_mins_total % 60;
    format!(
        "{} {}{:02}{:02}",
        dt.format("%a %b %d %H:%M:%S %Y"),
        sign,
        offset_hours,
        offset_mins_rem,
    )
}

/// Show a commit: metadata, full message, and optionally a unified diff.
///
/// Safety: `object` must have been confirmed as `Kind::Commit` by the caller.
/// `into_commit()` panics on kind mismatch.
fn show_commit(
    repo: &gix::Repository,
    object: &gix::Object<'_>,
    include_diff: bool,
) -> Result<String, ToolError> {
    let commit = object.clone().into_commit();
    let decoded = commit
        .decode()
        .map_err(|e| ToolError::Other(format!("failed to decode commit: {e}")))?;
    let author = commit
        .author()
        .map_err(|e| ToolError::Other(format!("failed to get author: {e}")))?;
    let committer = commit
        .committer()
        .map_err(|e| ToolError::Other(format!("failed to get committer: {e}")))?;

    let mut out = String::new();
    writeln!(out, "commit {}", commit.id).ok();

    let head_desc = super::describe_head(repo).unwrap_or_default();
    let parent_ids: Vec<_> = commit.parent_ids().collect();

    writeln!(out, "Author:   {} <{}>", author.name, author.email).ok();
    if author.name != committer.name || author.email != committer.email {
        writeln!(out, "Committer: {} <{}>", committer.name, committer.email).ok();
    }
    if let Ok(time) = author.time() {
        writeln!(out, "Date:     {}", format_date(&time)).ok();
    }
    if let Ok(tree_id) = commit.tree_id() {
        writeln!(out, "Tree:     {}", tree_id).ok();
    }
    if parent_ids.is_empty() {
        writeln!(out, "Parent:   (root commit)").ok();
    } else {
        for pid in &parent_ids {
            writeln!(out, "Parent:   {}", pid).ok();
        }
    }
    writeln!(out, "Head:     {head_desc}").ok();
    writeln!(out).ok();

    // Full commit message. Emitted unindented: git_show results are parsed
    // as markdown by the TUI (see MARKDOWN_TOOLS), and a 4-space indent
    // would be read as a CommonMark indented code block — the TUI would
    // wrap the whole message in a literal ``` box. Plain lines render as
    // paragraph rows whose soft breaks stay separate lines on copy.
    let message = decoded.message;
    for line in message.lines() {
        let s = String::from_utf8_lossy(line);
        writeln!(out, "{s}").ok();
    }

    // Optionally generate and append the diff
    if include_diff {
        let diff_text = generate_commit_diff(repo, &commit, &parent_ids)?;
        if !diff_text.is_empty() {
            writeln!(out).ok();
            out.push_str(&diff_text);
        }
    }

    Ok(out.trim_end().to_string())
}

/// Generate a unified diff between this commit and its first parent.
fn generate_commit_diff(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    parent_ids: &[gix::Id<'_>],
) -> Result<String, ToolError> {
    let commit_tree = commit
        .tree()
        .map_err(|e| ToolError::Other(format!("failed to get commit tree: {e}")))?;

    let parent_tree = if let Some(pid) = parent_ids.first() {
        let parent_obj = pid
            .object()
            .map_err(|e| ToolError::Other(format!("failed to get parent object: {e}")))?;
        let parent_commit = parent_obj.into_commit();
        Some(
            parent_commit
                .tree()
                .map_err(|e| ToolError::Other(format!("failed to get parent tree: {e}")))?,
        )
    } else {
        None
    };

    use gix::object::tree::diff::ChangeDetached;

    let changes: Vec<ChangeDetached> = repo
        .diff_tree_to_tree(
            parent_tree.as_ref(),
            Some(&commit_tree),
            None::<gix::diff::Options>,
        )
        .map_err(|e| ToolError::Other(format!("failed to diff trees: {e}")))?;

    let mut out = String::new();

    for change in &changes {
        // gix's tree diff reports a change for every modified *directory* (and
        // gitlink) entry in addition to the files inside it. Directories have
        // no diffable content — reading the tree object as a blob yields raw
        // tree bytes (NUL separators), which we would otherwise report as a
        // bogus `Binary file: <dir>` entry that makes agents think the repo
        // contains binaries or symlinks at those paths. Skip structural
        // entries; the leaf file changes are emitted separately.
        if !is_blob_change(change) {
            continue;
        }

        let path = String::from_utf8_lossy(change.location().as_ref()).into_owned();
        let source_path = String::from_utf8_lossy(change.source_location().as_ref()).into_owned();

        let (old_oid, new_oid) = oids_for_change(change);

        let old_content = if let Some(oid) = old_oid {
            fetch_blob_text_or_empty(repo, &oid)
        } else {
            String::new()
        };
        let new_content = if let Some(oid) = new_oid {
            fetch_blob_text_or_empty(repo, &oid)
        } else {
            String::new()
        };

        // Skip binary / large files
        if is_binary_content(&old_content) || is_binary_content(&new_content) {
            writeln!(out, "diff --git a/{source_path} b/{path}").ok();
            writeln!(out, "Binary file: {path}").ok();
            continue;
        }

        let diff = crate::diff_util::generate_diff(&old_content, &new_content, &source_path, &path);
        if !diff.is_empty() {
            super::append_fenced_diff(&mut out, &diff);
        }
    }

    Ok(out)
}

/// Returns `true` when the change describes file content (blobs, executable
/// blobs, or symlinks) worth diffing, and `false` for structural entries
/// (directories and gitlinks/submodules).
///
/// gix's tree diff emits a change for every changed *tree* entry in addition
/// to the files inside it — so a commit touching `crate-a/src/lib.rs` also
/// yields changes at `crate-a` and `crate-a/src`. Only blob-like entries have
/// diffable content; treating a tree object as a blob surfaces raw tree bytes
/// (which contain NUL separators) as a misleading `Binary file: <dir>` entry.
fn is_blob_change(change: &gix::object::tree::diff::ChangeDetached) -> bool {
    use gix::object::tree::EntryKind;
    use gix::object::tree::diff::ChangeDetached;

    let is_file_mode = |mode: gix::object::tree::EntryMode| {
        matches!(
            mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
        )
    };
    match change {
        ChangeDetached::Addition { entry_mode, .. } => is_file_mode(*entry_mode),
        ChangeDetached::Deletion { entry_mode, .. } => is_file_mode(*entry_mode),
        ChangeDetached::Modification {
            previous_entry_mode,
            entry_mode,
            ..
        } => is_file_mode(*previous_entry_mode) && is_file_mode(*entry_mode),
        ChangeDetached::Rewrite {
            source_entry_mode,
            entry_mode,
            ..
        } => is_file_mode(*source_entry_mode) && is_file_mode(*entry_mode),
    }
}

/// Extract old/new OIDs from a tree change, returning `(old, new)` where
/// `None` means the side is absent (addition or deletion).
fn oids_for_change(
    change: &gix::object::tree::diff::ChangeDetached,
) -> (Option<gix::hash::ObjectId>, Option<gix::hash::ObjectId>) {
    use gix::object::tree::diff::ChangeDetached;
    match change {
        ChangeDetached::Addition { id, .. } => (None, Some(*id)),
        ChangeDetached::Deletion { id, .. } => (Some(*id), None),
        ChangeDetached::Modification {
            previous_id, id, ..
        } => (Some(*previous_id), Some(*id)),
        ChangeDetached::Rewrite { source_id, id, .. } => (Some(*source_id), Some(*id)),
    }
}

/// Fetch the textual content of a blob by OID, falling back to an empty string
/// on error and logging a warning.
fn fetch_blob_text_or_empty(repo: &gix::Repository, oid: &gix::hash::ObjectId) -> String {
    let obj = match repo.find_object(*oid) {
        Ok(o) => o,
        Err(e) => {
            warn!(%oid, error = %e, "failed to find blob object for diff");
            return String::new();
        }
    };
    let (text, had_lossy) = utf8_lossy_detected(&obj.data);
    if had_lossy {
        warn!(%oid, "blob content is not valid UTF-8 — lossy conversion applied in diff");
    }
    text
}

/// Decode bytes as UTF-8, falling back to lossy replacement, and report
/// whether any replacement occurred.
fn utf8_lossy_detected(data: &[u8]) -> (String, bool) {
    match std::str::from_utf8(data) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (String::from_utf8_lossy(data).into_owned(), true),
    }
}

/// Heuristic: content with null bytes or exceeding 1 MB is treated as binary.
///
/// Scanning `content.as_bytes()` for `0u8` catches null bytes directly, which
/// is slightly more explicit than `str::contains('\0')` and avoids any
/// potential Unicode overhead for a simple byte check.
fn is_binary_content(content: &str) -> bool {
    content.len() > 1_000_000 || content.as_bytes().contains(&0u8)
}

/// Show a tree: list entries like `git ls-tree`.
///
/// Safety: `object` must have been confirmed as `Kind::Tree` by the caller.
/// `into_tree()` panics on kind mismatch.
fn show_tree(object: &gix::Object<'_>) -> Result<String, ToolError> {
    let tree = object.clone().into_tree();
    let decoded = tree
        .decode()
        .map_err(|e| ToolError::Other(format!("failed to decode tree: {e}")))?;

    let mut out = String::new();
    writeln!(out, "tree {}", tree.id).ok();
    writeln!(out).ok();

    for entry in decoded.entries {
        let kind_str = match entry.mode.kind() {
            gix::object::tree::EntryKind::Blob => "blob",
            gix::object::tree::EntryKind::BlobExecutable => "blob",
            gix::object::tree::EntryKind::Link => "link",
            gix::object::tree::EntryKind::Tree => "tree",
            gix::object::tree::EntryKind::Commit => "commit",
        };
        writeln!(
            out,
            "{:06o} {} {}\t{}",
            entry.mode, kind_str, entry.oid, entry.filename,
        )
        .ok();
    }

    Ok(out.trim_end().to_string())
}

/// Show a blob: file contents in a fenced code block.
///
/// Safety: `object` must have been confirmed as `Kind::Blob` by the caller.
/// `into_blob()` panics on kind mismatch.
fn show_blob(object: &gix::Object<'_>, path_hint: Option<&str>) -> Result<String, ToolError> {
    let blob = object.clone().into_blob();
    let content = String::from_utf8_lossy(&blob.data);

    let mut out = String::new();
    writeln!(out, "blob {}", blob.id).ok();
    writeln!(out).ok();

    let lang = path_hint.and_then(|p| p.rsplit('.').next()).unwrap_or("");

    if is_binary_content(&content) {
        writeln!(out, "<binary blob: {} bytes>", blob.data.len()).ok();
    } else {
        writeln!(out, "```{lang}").ok();
        out.push_str(&content);
        if !content.ends_with('\n') {
            writeln!(out).ok();
        }
        writeln!(out, "```").ok();
    }

    Ok(out.trim_end().to_string())
}

/// Show an annotated tag: metadata then the tagged object.
///
/// Safety: `object` must have been confirmed as `Kind::Tag` by the caller.
/// `into_tag()` panics on kind mismatch.
fn show_tag(repo: &gix::Repository, object: &gix::Object<'_>) -> Result<String, ToolError> {
    let tag = object.clone().into_tag();
    let decoded = tag
        .decode()
        .map_err(|e| ToolError::Other(format!("failed to decode tag: {e}")))?;

    let mut out = String::new();
    writeln!(out, "tag {}", tag.id).ok();

    let target_id = tag.target_id().ok();
    if let Ok(name_str) = decoded.name.to_str() {
        writeln!(out, "Name:     {name_str}").ok();
    }
    if let Some(id) = target_id {
        let kind_str = String::from_utf8_lossy(decoded.target_kind.as_bytes());
        writeln!(out, "Object:   {id} ({kind_str})").ok();
    }
    if let Ok(Some(tagger)) = tag.tagger() {
        writeln!(out, "Tagger:   {} <{}>", tagger.name, tagger.email).ok();
        if let Ok(time) = tagger.time() {
            writeln!(out, "Date:     {}", format_date(&time)).ok();
        }
    }
    writeln!(out).ok();

    // Tag message, unindented for the same reason as the commit message
    // above: git_show output is markdown-parsed in the TUI, and a 4-space
    // indent would render as a literal ```-boxed CommonMark code block.
    let message = decoded.message;
    for line in message.lines() {
        let s = String::from_utf8_lossy(line);
        writeln!(out, "{s}").ok();
    }

    // Recurse into the tagged object
    if let Some(id) = target_id {
        let target_obj = repo
            .find_object(id)
            .map_err(|e| ToolError::Other(format!("failed to find tagged object: {e}")))?;
        writeln!(out).ok();
        writeln!(out, "--- tagged object ---").ok();
        let nested = match target_obj.kind {
            gix::object::Kind::Commit => show_commit(repo, &target_obj, false),
            gix::object::Kind::Tree => show_tree(&target_obj),
            gix::object::Kind::Blob => show_blob(&target_obj, None),
            gix::object::Kind::Tag => show_tag(repo, &target_obj),
        }?;
        writeln!(out, "{nested}").ok();
    }

    Ok(out.trim_end().to_string())
}

pub fn describe_git_show_invocation(args: &GitShowArgs) -> String {
    use std::fmt::Write as _;
    let revision = args.revision.as_deref().unwrap_or("HEAD");
    let mut s = format!("Showing git object at `{revision}`.");
    if let Some(ref path) = args.path {
        write!(s, " File: `{path}`.").ok();
    }
    if args.diff.unwrap_or(false) {
        s.push_str(" Including diff.");
    }
    if let Some(ref p) = args.repo_path {
        write!(s, " Repository: `{p}`.").ok();
    }
    s
}

pub(crate) struct GitShow;

define_tool!(
    GitShow,
    "git_show",
    "Show the details of a Git object (commit, tree, blob, tag) or a file at a given revision.",
    GitShowArgs,
    execute_git_show_tool,
    "git",
    describe_git_show_invocation
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_date_epoch() {
        let time = gix::date::Time {
            seconds: 0,
            offset: 0,
        };
        let result = format_date(&time);
        assert!(result.contains("Jan 01 00:00:00 1970"), "got: {result}");
    }

    #[test]
    fn test_format_date_positive_offset() {
        let time = gix::date::Time {
            seconds: 1700000000,
            offset: 28800,
        };
        let result = format_date(&time);
        assert!(
            result.contains("+0800"),
            "expected +0800 offset, got: {result}"
        );
    }

    #[test]
    fn test_format_date_negative_offset() {
        let time = gix::date::Time {
            seconds: 1700000000,
            offset: -18000,
        };
        let result = format_date(&time);
        assert!(
            result.contains("-0500"),
            "expected -0500 offset, got: {result}"
        );
    }

    #[test]
    fn test_format_date_pre_epoch() {
        let time = gix::date::Time {
            seconds: -12614400,
            offset: 0,
        };
        let result = format_date(&time);
        assert!(result.contains("1969"), "got: {result}");
    }

    #[test]
    fn test_utf8_lossy_detected_ascii() {
        let (text, had_lossy) = utf8_lossy_detected(b"hello");
        assert!(!had_lossy);
        assert_eq!(text, "hello");
    }

    #[test]
    fn test_utf8_lossy_detected_invalid() {
        let (text, had_lossy) = utf8_lossy_detected(&[0xFF, 0xFE]);
        assert!(had_lossy);
        assert!(text.contains('\u{FFFD}'));
    }

    #[test]
    fn test_utf8_lossy_detected_empty() {
        let (text, had_lossy) = utf8_lossy_detected(b"");
        assert!(!had_lossy);
        assert_eq!(text, "");
    }

    #[test]
    fn test_is_binary_null() {
        assert!(is_binary_content("hello\0world"));
    }

    #[test]
    fn test_is_binary_plain_text() {
        assert!(!is_binary_content("hello world"));
    }

    #[test]
    fn test_is_binary_large() {
        let large = "x".repeat(1_000_001);
        assert!(is_binary_content(&large));
    }

    #[test]
    fn test_is_binary_under_limit() {
        let small = "x".repeat(999_999);
        assert!(!is_binary_content(&small));
    }

    // ── is_blob_change ──

    fn mode(octal: u32) -> gix::object::tree::EntryMode {
        gix::object::tree::EntryMode::try_from(octal).expect("valid entry mode")
    }

    fn null_id() -> gix::hash::ObjectId {
        gix::hash::ObjectId::null(gix::hash::Kind::Sha1)
    }

    fn addition(mode_octal: u32) -> gix::object::tree::diff::ChangeDetached {
        gix::object::tree::diff::ChangeDetached::Addition {
            location: gix::bstr::BString::from("f"),
            relation: None,
            entry_mode: mode(mode_octal),
            id: null_id(),
        }
    }

    fn deletion(mode_octal: u32) -> gix::object::tree::diff::ChangeDetached {
        gix::object::tree::diff::ChangeDetached::Deletion {
            location: gix::bstr::BString::from("f"),
            relation: None,
            entry_mode: mode(mode_octal),
            id: null_id(),
        }
    }

    fn modification(
        old_mode_octal: u32,
        new_mode_octal: u32,
    ) -> gix::object::tree::diff::ChangeDetached {
        gix::object::tree::diff::ChangeDetached::Modification {
            location: gix::bstr::BString::from("f"),
            previous_entry_mode: mode(old_mode_octal),
            previous_id: null_id(),
            entry_mode: mode(new_mode_octal),
            id: null_id(),
        }
    }

    #[test]
    fn blob_changes_are_diffed() {
        // Regular file, executable file, and symlink additions.
        assert!(is_blob_change(&addition(0o100644)));
        assert!(is_blob_change(&addition(0o100755)));
        assert!(is_blob_change(&addition(0o120000)));
        // Deletions and modifications of files.
        assert!(is_blob_change(&deletion(0o100644)));
        assert!(is_blob_change(&modification(0o100644, 0o100644)));
        assert!(is_blob_change(&modification(0o100644, 0o100755)));
    }

    #[test]
    fn directory_and_gitlink_changes_are_skipped() {
        // Directory additions/deletions are structural, not content.
        assert!(!is_blob_change(&addition(0o040000)));
        assert!(!is_blob_change(&deletion(0o040000)));
        // Submodule (gitlink) entries carry no diffable content either.
        assert!(!is_blob_change(&addition(0o160000)));
        // Directory → directory and file ↔ directory transitions.
        assert!(!is_blob_change(&modification(0o040000, 0o040000)));
        assert!(!is_blob_change(&modification(0o100644, 0o040000)));
        assert!(!is_blob_change(&modification(0o040000, 0o100644)));
    }

    #[test]
    fn test_describe_invocation_defaults() {
        let args = GitShowArgs {
            repo_path: None,
            revision: None,
            path: None,
            diff: None,
        };
        let desc = describe_git_show_invocation(&args);
        assert!(desc.contains("HEAD"), "got: {desc}");
        assert!(!desc.contains("diff"), "got: {desc}");
        assert!(!desc.contains("File:"), "got: {desc}");
    }

    #[test]
    fn test_describe_invocation_with_all_fields() {
        let args = GitShowArgs {
            repo_path: Some("/repo".into()),
            revision: Some("v1.0".into()),
            path: Some("README.md".into()),
            diff: Some(true),
        };
        let desc = describe_git_show_invocation(&args);
        assert!(desc.contains("v1.0"), "got: {desc}");
        assert!(desc.contains("README.md"), "got: {desc}");
        assert!(desc.contains("diff"), "got: {desc}");
        assert!(desc.contains("/repo"), "got: {desc}");
    }

    #[test]
    fn test_describe_invocation_path_only() {
        let args = GitShowArgs {
            repo_path: None,
            revision: None,
            path: Some("src/main.rs".into()),
            diff: None,
        };
        let desc = describe_git_show_invocation(&args);
        assert!(desc.contains("src/main.rs"), "got: {desc}");
    }
}
