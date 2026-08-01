use crate::tools::{ToolExecError, resolve_path, truncate_tool_output};
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

/// One rendered row of the listing.
#[derive(Debug)]
struct EntryRecord {
    /// Sanitized display name (control chars escaped; trailing `/` for dirs).
    name: String,
    kind: EntryKind,
    /// File size in bytes (files and "other" entries only; `None` for dirs,
    /// links, and unreadable entries).
    size: Option<u64>,
    /// Annotation column: `"-> target"`, `"(24 entries)"`, `"(empty)"`,
    /// `"(unreadable)"`.
    annotation: Option<String>,
}

/// Escape control characters in a file name so a pathological name (e.g. one
/// containing a newline) cannot corrupt the line-oriented tool output — every
/// entry must stay on exactly one line for the LLM to parse the listing.
fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_control() {
            // escape_default renders e.g. `\n` for a literal newline — the
            // two-character sequence keeps the output one line per entry.
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Human-readable byte size: `"512 B"`, `"2.6 KiB"`, `"1.4 MiB"`. Values
/// ≥ 100 in a unit drop the decimal so columns stay compact (`"100 MiB"`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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
fn describe_symlink(raw_name: &str, path: &Path) -> EntryRecord {
    let target = match fs::read_link(path) {
        Ok(target) => target.to_string_lossy().into_owned(),
        Err(err) => {
            warn!(
                error = %err,
                path = %path.display(),
                "list_files: failed to resolve symlink target"
            );
            "<unreadable target>".to_string()
        }
    };
    // fs::metadata follows the link; on failure (e.g. dangling link) we keep
    // the bare target rather than failing the whole listing.
    let target = match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => format!("{target}/"),
        _ => target,
    };
    EntryRecord {
        name: sanitize_name(raw_name),
        kind: EntryKind::Link,
        size: None,
        annotation: Some(format!("-> {target}")),
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
            size: None,
            annotation: Some(subdir_summary(&entry.path())),
        },
        Ok(meta) if meta.is_file() => EntryRecord {
            name: sanitize_name(&raw_name),
            kind: EntryKind::File,
            size: Some(meta.len()),
            annotation: None,
        },
        // Sockets, fifos, devices, and stat failures: show what we can. A
        // per-entry failure degrades to an `(unreadable)` annotation rather
        // than aborting the entire listing — one bad entry shouldn't hide
        // the rest of the directory.
        Ok(meta) => EntryRecord {
            name: sanitize_name(&raw_name),
            kind: EntryKind::Other,
            size: Some(meta.len()),
            annotation: None,
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
                size: None,
                annotation: Some("(unreadable)".to_string()),
            }
        }
    }
}

/// `"1 file"` / `"2 files"` — singular for exactly one, plural otherwise.
/// Keeps the summary line readable ("3 entries (2 files, 1 dir)") without
/// hard-coding per-category grammar at each call site.
fn count_label(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

pub(crate) fn execute_list_files_tool(
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

    let mut out = format!("{}: {} entries", resolved.display(), records.len());
    let mut parts = Vec::new();
    if files > 0 {
        parts.push(count_label(files, "file"));
    }
    if dirs > 0 {
        parts.push(count_label(dirs, "dir"));
    }
    if links > 0 {
        parts.push(count_label(links, "link"));
    }
    if others > 0 {
        parts.push(count_label(others, "other"));
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
        match (record.size.map(human_size), &record.annotation) {
            (Some(size), Some(annotation)) => {
                out.push_str(&format!("{name_col} {size:<10} {annotation}\n"));
            }
            (Some(size), None) => out.push_str(&format!("{name_col} {size}\n")),
            (None, Some(annotation)) => out.push_str(&format!("{name_col} {annotation}\n")),
            (None, None) => out.push_str(&format!("{}\n", name_col.trim_end())),
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
    use tempfile::TempDir;

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
    fn human_size_formats() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1500), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_size(100 * 1024 * 1024), "100 MiB");
    }

    #[test]
    fn sanitize_name_escapes_control_chars() {
        assert_eq!(sanitize_name("plain.txt"), "plain.txt");
        assert_eq!(sanitize_name("a\nb"), "a\\nb");
        assert_eq!(sanitize_name("a\tb"), "a\\tb");
    }

    #[test]
    fn count_label_singular_and_plural() {
        assert_eq!(count_label(1, "file"), "1 file");
        assert_eq!(count_label(2, "file"), "2 files");
        assert_eq!(count_label(1, "dir"), "1 dir");
        assert_eq!(count_label(3, "link"), "3 links");
    }

    #[test]
    fn lists_files_with_rich_metadata() {
        let dir = TempDir::new().unwrap();
        // A small text file with a known size.
        fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        // A 4 KiB file.
        fs::write(dir.path().join("blob.bin"), vec![0u8; 4096]).unwrap();
        // Subdirectory with a couple of entries.
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let args = ListFilesArgs {
            path: Some(dir.path().to_str().unwrap().into()),
        };
        let out = execute_list_files_tool(&args, None).unwrap();

        assert!(out.contains("3 entries (2 files, 1 dir)"), "{out}");
        assert!(out.contains("main.rs"), "{out}");
        assert!(out.contains("4.0 KiB"), "{out}");
        assert!(out.contains("src/"), "{out}");
        assert!(out.contains("(2 entries)"), "{out}");
        // Pure metadata: sizes only, never content-derived annotations.
        assert!(!out.contains("lines"), "{out}");
        assert!(!out.contains("binary"), "{out}");
    }

    #[test]
    fn empty_directory_reports_zero_entries() {
        let dir = TempDir::new().unwrap();
        let args = ListFilesArgs {
            path: Some(dir.path().to_str().unwrap().into()),
        };
        let out = execute_list_files_tool(&args, None).unwrap();
        assert!(out.contains("0 entries"), "{out}");
        assert_eq!(out.lines().count(), 1, "only the summary line: {out}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_shows_target() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("real.txt"), "hi\n").unwrap();
        std::os::unix::fs::symlink("real.txt", dir.path().join("link.txt")).unwrap();

        let args = ListFilesArgs {
            path: Some(dir.path().to_str().unwrap().into()),
        };
        let out = execute_list_files_tool(&args, None).unwrap();

        assert!(out.contains("2 entries (1 file, 1 link)"), "{out}");
        assert!(out.contains("link.txt -> real.txt"), "{out}");
    }

    #[test]
    fn default_path_lists_working_directory() {
        // Execute with a temp working dir so the result is deterministic.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let args = ListFilesArgs { path: None };
        let out = execute_list_files_tool(&args, Some(dir.path())).unwrap();
        assert!(out.contains("1 entries (1 file)"), "{out}");
        assert!(out.contains("a.txt"), "{out}");
        assert!(
            out.starts_with(&format!("{}:", dir.path().display())),
            "{out}"
        );
    }
}
