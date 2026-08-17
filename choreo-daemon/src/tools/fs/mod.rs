mod delete_files;
mod edit_file;
mod line_count;
mod list_files;
mod write_file;

pub(crate) use delete_files::DeleteFiles;
pub(crate) use edit_file::EditFile;
pub(crate) use line_count::LineCount;
pub(crate) use list_files::ListFiles;
// Public so crate-level integration tests (tests/*_integration.rs) can drive
// the tool through the same API the registry uses, mirroring how find.rs
// exposes FindArgs/execute_find_tool.
pub use edit_file::{EditFileArgs, TextEditArgs, execute_edit_file_tool};
pub use list_files::{ListFilesArgs, execute_list_files_tool};
pub(crate) use write_file::WriteFile;
pub use write_file::{WriteFileArgs, execute_write_file_tool};

use crate::tools::ToolExecError;
use std::{fs::OpenOptions, io::Write};
use std::{io, path::Path};
use tracing::debug;

fn validate_nonempty_path(path: &str) -> Result<String, ToolExecError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ))
    } else {
        Ok(trimmed.to_string())
    }
}

fn ensure_parent_directories(path: &Path, create_parents: bool) -> Result<(), ToolExecError> {
    if !create_parents {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn write_text_file(path: &Path, content: &str, overwrite: bool) -> io::Result<()> {
    if !overwrite {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(path)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        return Ok(());
    }

    atomic_write_text_file(path, content)
}

/// Wrap content in a fenced code block, choosing a fence wide enough that
/// content containing backticks cannot close it early.
///
/// Shared by write_file, git_show's commit/tag messages, and show_blob:
/// all of them emit untrusted data (file contents, commit/tag messages)
/// verbatim inside a fence so a markdown-parsing client (the TUI'S
/// MARKDOWN_TOOLS renderer) cannot re-interpret the enclosed bytes.
pub(crate) fn fence_content(content: &str, lang: &str) -> String {
    // Strip trailing newlines so the closing fence sits directly after the
    // last line of content, avoiding a blank line before the fence.
    let trimmed = content.trim_end_matches('\n');
    let max_run = trimmed
        .chars()
        .fold((0usize, 0usize), |(max_run, current), c| {
            if c == '`' {
                (max_run.max(current + 1), current + 1)
            } else {
                (max_run, 0)
            }
        })
        .0;
    // A fence one backtick longer than the longest run inside the content can
    // never be matched by an interior run, so the block cannot close early.
    let fence_len = (max_run + 1).max(3);
    let fence = "`".repeat(fence_len);
    format!("{fence}{lang}\n{trimmed}\n{fence}")
}

fn atomic_write_text_file(path: &Path, content: &str) -> io::Result<()> {
    // Resolve symlinks before the swap: persisting the temp file over a
    // symlink would replace the link itself with a regular file, while
    // editing a symlinked file should update the real target in place and
    // keep the link. A missing target has no link to preserve, so fall back
    // to the literal path (canonicalize requires the file to exist).
    let target = match std::fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(_) => path.to_path_buf(),
    };
    let dir = target.parent().unwrap_or(Path::new("."));
    // Capture the target's permissions BEFORE the atomic swap: NamedTempFile
    // is created 0600 on Unix, so persisting it over an existing file would
    // silently strip the original mode (e.g. the +x bit on a script) and
    // leave a 0600 copy behind. A missing target (new file) keeps the
    // tempfile default — there are no pre-existing permissions to honor.
    let original_permissions = match std::fs::metadata(&target) {
        Ok(m) => Some(m.permissions()),
        // NotFound is the normal new-file case (keep the tempfile default);
        // any other metadata error (e.g. EACCES on a parent directory) means
        // the write cannot succeed either, so surface it now with a clear
        // message rather than later at persist with a confusing one.
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    // Snapshot the flag before the move into the if-let below; it also feeds
    // the debug event so operators can tell preserved-mode swaps from the
    // new-file (tempfile default) case.
    let preserved_mode = original_permissions.is_some();
    if let Some(perms) = original_permissions {
        // Apply before persist so the rename lands with the right mode — no
        // window where the destination has stripped permissions. This is a
        // best-effort snapshot: if the file's mode changes concurrently, the
        // swap applies the stale mode (an accepted TOCTOU for a local tool).
        tmp.as_file().set_permissions(perms)?;
    }
    debug!(path = %target.display(), preserved_mode, "atomic write: replacing file");
    tmp.persist(&target).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    // ── fence_content tests ──────────────────────────────────────────

    #[test]
    fn fence_content_basic() {
        let result = super::fence_content("hello", "rust");
        assert_eq!(result, "```rust\nhello\n```");
    }

    #[test]
    fn fence_content_no_lang() {
        let result = super::fence_content("plain text", "");
        assert_eq!(result, "```\nplain text\n```");
    }

    #[test]
    fn fence_content_with_backticks() {
        let result = super::fence_content("`code`", "text");
        // Content contains a single backtick, so fence must be at least 2 wide.
        assert!(result.starts_with("``"));
        assert!(result.ends_with("``"));
        assert!(result.contains("`code`"));
    }

    #[test]
    fn fence_content_triple_backticks() {
        let result = super::fence_content("```\ncode\n```", "text");
        // Content contains 3 consecutive backticks, so fence must be at least 4 wide.
        assert!(result.starts_with("````"));
        assert!(result.ends_with("````"));
    }

    #[test]
    fn fence_content_diff_with_backtick_context_line_never_closes_early() {
        // A git diff for a Markdown file can carry a bare ``` line as a
        // *context* line (space-prefixed, so it parses as a valid CommonMark
        // closing fence). The sized fence must exceed that run so the ```\n```
        // block cannot be closed by the diff's own content — this is the
        // hardening `append_fenced_diff`/`edit_file` rely on.
        //
        // Built with `concat!` rather than `\`-continued string literals: Rust
        // line continuations strip the leading whitespace of the next source
        // line, which would silently drop the all-important space before ```.
        let diff = concat!(
            "diff --git a/README.md b/README.md\n",
            "--- a/README.md\n",
            "+++ b/README.md\n",
            "@@ -1,4 +1,4 @@\n",
            "plain\n",
            " ```\n",
            "-old\n",
            "+new\n",
            "code\n",
        );
        let result = super::fence_content(diff, "diff");
        // The longest interior run is 3 backticks (the context line) -> fence is
        // 4 wide, so no interior line can ever match the closing fence.
        assert!(
            result.starts_with("````diff\n"),
            "start: {}...",
            &result[..result.len().min(40)]
        );
        assert!(
            result.ends_with("\n````"),
            "end: {}...",
            &result[..result.len().min(40)]
        );
        // The diff's own context line (space + 3 backticks) must survive as
        // interior content, not be mistaken for the closing fence.
        assert!(
            result.contains("\n ```\n"),
            "context fence must stay inside: {result}"
        );
    }

    #[test]
    fn fence_content_diff_without_backticks_keeps_three_backtick_fence() {
        // Backtick-free diffs (the common case) keep the canonical 3-backtick
        // ```diff fence, so existing callers/tests observing that shape pass.
        let result = super::fence_content("diff --git a/f b/f\n-old\n+new", "diff");
        assert!(result.starts_with("```diff\n"), "{}", result);
        assert!(result.ends_with("\n```"), "{}", result);
    }

    #[test]
    fn fence_content_empty_content() {
        let result = super::fence_content("", "json");
        assert_eq!(result, "```json\n\n```");
    }

    #[test]
    fn fence_content_trailing_newline_stripped() {
        let result = super::fence_content("hello\n", "text");
        // Should not have a blank line before the closing fence.
        assert_eq!(result, "```text\nhello\n```");
    }

    #[test]
    fn fence_content_multiple_trailing_newlines_stripped() {
        let result = super::fence_content("a\nb\n\n", "text");
        assert_eq!(result, "```text\na\nb\n```");
    }
}
