use crate::tools::{
    ToolExecError, human_size, resolve_path, sanitize_name, symlink_target_label,
    truncate_tool_output,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::fs;
use std::path::Path;
use tracing::warn;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesArgs {
    /// Relative or absolute path to a directory (defaults to working directory)
    pub path: Option<String>,
}

/// What kind of filesystem object an entry is — drives both the summary
/// counts and which metadata columns are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Link,
    Other,
}

/// The trailing column of a row: a human-readable size (files and "other"
/// entries) or a text annotation (dirs, symlink targets, unreadable entries).
///
/// A sum type instead of two `Option`s so the "exactly one of size-or-note"
/// invariant is enforced by the compiler rather than by hoping every
/// construction site picks a valid combination.
#[derive(Debug)]
enum EntryDetail {
    Size(u64),
    Note(String),
}

/// One rendered row of the listing.
#[derive(Debug)]
struct EntryRecord {
    /// Sanitized display name (control chars escaped; trailing `/` for dirs).
    name: String,
    kind: EntryKind,
    detail: EntryDetail,
}

/// Count the entries in a subdirectory so the listing can show `(N entries)`
/// at a glance. One extra `read_dir` per subdir — cheap, and lets the LLM
/// decide whether drilling into the subdir is worth a call.
fn subdir_summary(path: &Path) -> String {
    let mut count = 0u64;
    let read_dir = match fs::read_dir(path) {
        Ok(iter) => iter,
        Err(err) => {
            warn!(
                error = %err,
                path = %path.display(),
                "list_files: failed to read subdirectory"
            );
            return "(unreadable)".to_string();
        }
    };
    for entry in read_dir {
        match entry {
            Ok(_) => count += 1,
            Err(err) => {
                warn!(
                    error = %err,
                    path = %path.display(),
                    "list_files: failed to read subdir entry while counting"
                );
                return "(unreadable)".to_string();
            }
        }
    }
    if count == 0 {
        "(empty)".to_string()
    } else {
        format!("({count} entries)")
    }
}

/// Render a symlink as `name -> target`, appending `/` to the target when it
/// resolves to a directory so dir-links are visually distinct from file-links.
/// Target rendering is shared: `crate::tools::symlink_target_label`.
fn describe_symlink(raw_name: &str, path: &Path) -> EntryRecord {
    EntryRecord {
        name: sanitize_name(raw_name),
        kind: EntryKind::Link,
        detail: EntryDetail::Note(format!("-> {}", symlink_target_label(path))),
    }
}

/// Build one `EntryRecord` from a `read_dir` entry.
fn describe_entry(entry: &fs::DirEntry) -> EntryRecord {
    let raw_name = entry.file_name().to_string_lossy().into_owned();
    // DirEntry::metadata has lstat semantics — it does NOT follow symlinks,
    // so a symlink-to-dir is correctly classified as a link here.
    let meta = entry.metadata();

    if meta
        .as_ref()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return describe_symlink(&raw_name, &entry.path());
    }

    match meta {
        Ok(meta) if meta.is_dir() => EntryRecord {
            name: format!("{}/", sanitize_name(&raw_name)),
            kind: EntryKind::Dir,
            detail: EntryDetail::Note(subdir_summary(&entry.path())),
        },
        Ok(meta) if meta.is_file() => EntryRecord {
            name: sanitize_name(&raw_name),
            kind: EntryKind::File,
            detail: EntryDetail::Size(meta.len()),
        },
        // Sockets, fifos, devices, and stat failures: show what we can. A
        // per-entry failure degrades to an `(unreadable)` annotation rather
        // than aborting the entire listing — one bad entry shouldn't hide
        // the rest of the directory.
        Ok(meta) => EntryRecord {
            name: sanitize_name(&raw_name),
            kind: EntryKind::Other,
            detail: EntryDetail::Size(meta.len()),
        },
        Err(err) => {
            warn!(
                error = %err,
                path = %entry.path().display(),
                "list_files: stat failed, marking entry unreadable"
            );
            EntryRecord {
                name: sanitize_name(&raw_name),
                kind: EntryKind::Other,
                detail: EntryDetail::Note("(unreadable)".to_string()),
            }
        }
    }
}

/// `"1 file"` / `"2 files"` — singular for exactly one, plural otherwise.
/// Takes the full plural form explicitly so irregular nouns
/// ("entry" -> "entries") render correctly without string heuristics.
fn count_label(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

pub fn execute_list_files_tool(
    args: &ListFilesArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = args.path.as_deref().unwrap_or(".");
    let resolved = resolve_path(path, working_dir);

    let mut records = Vec::new();
    for entry in fs::read_dir(&resolved)? {
        // Iteration errors on the directory itself are fatal (the listing is
        // meaningless if we cannot read the entries); per-entry stat errors
        // are handled gracefully inside describe_entry.
        let entry = entry?;
        records.push(describe_entry(&entry));
    }
    // read_dir order is filesystem-defined; sort for deterministic output.
    records.sort_by(|a, b| a.name.cmp(&b.name));

    // Summary line: resolved path plus entry counts, with zero categories
    // omitted so the common cases stay compact (e.g. "3 entries (2 files, 1 dir)").
    let files = records.iter().filter(|r| r.kind == EntryKind::File).count();
    let dirs = records.iter().filter(|r| r.kind == EntryKind::Dir).count();
    let links = records.iter().filter(|r| r.kind == EntryKind::Link).count();
    let others = records
        .iter()
        .filter(|r| r.kind == EntryKind::Other)
        .count();

    let mut out = format!(
        "{}: {}",
        resolved.display(),
        count_label(records.len(), "entry", "entries")
    );
    let mut parts = Vec::new();
    if files > 0 {
        parts.push(count_label(files, "file", "files"));
    }
    if dirs > 0 {
        parts.push(count_label(dirs, "dir", "dirs"));
    }
    if links > 0 {
        parts.push(count_label(links, "link", "links"));
    }
    if others > 0 {
        parts.push(count_label(others, "other", "others"));
    }
    if !parts.is_empty() {
        out.push_str(&format!(" ({})", parts.join(", ")));
    }
    out.push('\n');

    // Align the name column to the widest display name so the size and
    // annotation columns read as a table.
    let width = records
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    for record in &records {
        let name_col = format!("{:<width$}", record.name);
        match &record.detail {
            EntryDetail::Size(bytes) => {
                out.push_str(&format!("{name_col} {}\n", human_size(*bytes)));
            }
            EntryDetail::Note(note) => out.push_str(&format!("{name_col} {note}\n")),
        }
    }
    Ok(truncate_tool_output(&out))
}

pub fn describe_list_files_invocation(args: &ListFilesArgs) -> String {
    match &args.path {
        Some(p) => format!("Listing files in `{}`.", p),
        None => "Listing files in the working directory.".to_string(),
    }
}

pub(crate) struct ListFiles;

define_tool!(
    ListFiles,
    "list_files",
    "List files in a local directory with sizes, symlink targets, and subdirectory entry counts.",
    ListFilesArgs,
    execute_list_files_tool,
    "core",
    describe_list_files_invocation
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_list_files_invocation_with_path() {
        let args = ListFilesArgs {
            path: Some("src".into()),
        };
        let desc = super::describe_list_files_invocation(&args);
        assert_eq!(desc, "Listing files in `src`.");
    }

    #[test]
    fn describe_list_files_invocation_without_path() {
        let args = ListFilesArgs { path: None };
        let desc = super::describe_list_files_invocation(&args);
        assert_eq!(desc, "Listing files in the working directory.");
    }

    #[test]
    fn count_label_singular_and_plural() {
        assert_eq!(count_label(1, "file", "files"), "1 file");
        assert_eq!(count_label(2, "file", "files"), "2 files");
        assert_eq!(count_label(1, "dir", "dirs"), "1 dir");
        assert_eq!(count_label(3, "link", "links"), "3 links");
        assert_eq!(count_label(1, "entry", "entries"), "1 entry");
        assert_eq!(count_label(4, "entry", "entries"), "4 entries");
    }
}
