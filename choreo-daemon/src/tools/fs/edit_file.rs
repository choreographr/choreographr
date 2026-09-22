use super::{validate_nonempty_path, write_text_file};
use crate::tools::{
    ToolExecError, display_path_label, resolve_path, sanitize_content, sanitize_name, sha256_hex,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use tracing::{info, warn};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Relative or absolute path to the file to edit
    pub path: String,
    /// One or more exact text replacements to apply
    pub edits: Vec<TextEditArgs>,
    /// If set, the tool will verify the file matches this SHA-256 before editing (safety check)
    pub expected_sha256: Option<String>,
    /// When true, preview changes without applying them
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TextEditArgs {
    /// Exact text to replace (must match at least once)
    pub old_text: String,
    /// Replacement text
    pub new_text: String,
    /// When true, replace all exact matches instead of requiring exactly one match
    pub replace_all: Option<bool>,
    /// When `old_text` matches more than one location, replace the occurrence
    /// nearest this 1-based line number (as shown by `read_file`) instead of
    /// failing as ambiguous. Ignored when the match is already unique or when
    /// `replace_all` is set.
    pub near_line: Option<usize>,
}

/// Apply exact text replacements to a UTF-8 file, with optional SHA-256
/// pre-check and dry-run preview.
///
/// # Errors
///
/// Returns Err if the path is invalid, the file cannot be read/written,
/// the SHA-256 pre-check fails, or no edit matches.
pub fn execute_edit_file_tool(
    args: &EditFileArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = validate_nonempty_path(&args.path)?;

    if args.edits.is_empty() {
        return Err(ToolExecError(
            "missing required array argument: edits".to_string(),
        ));
    }

    let resolved = resolve_path(&path, working_dir);
    let original_content = std::fs::read_to_string(&resolved)?;

    if let Some(expected_sha256) = args.expected_sha256.as_deref() {
        let actual_sha256 = sha256_hex(&original_content);
        if actual_sha256 != expected_sha256.trim().to_ascii_lowercase() {
            return Err(ToolExecError(format!(
                "expected_sha256 mismatch for {}: expected {}, got {}",
                resolved.display(),
                expected_sha256.trim(),
                actual_sha256
            )));
        }
    }

    // Match on LF-normalized text so a CRLF file — whose `read_file` view is
    // already LF-normalized — still matches the `old_text` the model copied,
    // then restore the file's original line ending on write. Without this,
    // exact matching fails on *every* line of a Windows-style file. (opencode
    // and pi do the same normalize-match / restore-on-write dance.)
    let line_ending = detect_line_ending(&original_content);
    let normalized = to_lf(&original_content);
    let edit_summary = apply_text_edits(&normalized, &args.edits).map_err(ToolExecError)?;
    let final_content = restore_line_endings(&edit_summary.content, line_ending);

    // Sanitize the display label: a hostile file name must not corrupt the
    // line-oriented result (the same policy `grep`/`read_file` apply to paths).
    let display = sanitize_name(&display_path_label(&resolved, working_dir));

    if args.dry_run.unwrap_or(false) {
        return Ok(format_edit_result("would edit", &display, &edit_summary));
    }

    match write_text_file(&resolved, &final_content, true) {
        Ok(()) => {
            info!(path = %resolved.display(), replacement_count = edit_summary.replacement_count, "edit_file: applied edits");
            Ok(format_edit_result("edited", &display, &edit_summary))
        }
        Err(error) => {
            warn!(path = %resolved.display(), error = %error, "edit_file: failed to write edited content");
            Err(ToolExecError(format!("{error}")))
        }
    }
}

#[derive(Debug)]
struct AppliedEditSummary {
    content: String,
    original: Option<String>,
    replacement_count: usize,
    char_delta: isize,
}

/// The dominant line ending of `text`: CRLF if it appears anywhere, else LF.
/// A mixed-ending file is treated as CRLF (the majority case that motivates
/// the check), which keeps the round-trip lossless for the common all-CRLF
/// file and merely re-terminates stray LF lines in the rare mixed file.
fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Normalize CRLF to LF (a no-op copy when there is no `\r`).
fn to_lf(text: &str) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    }
}

/// Re-apply `ending` to every `\n` (a no-op copy for LF).
fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\n" {
        text.to_string()
    } else {
        text.replace('\n', ending)
    }
}

/// 1-based line number containing byte `offset` (which must be a char boundary).
fn line_of_offset(content: &str, offset: usize) -> usize {
    // `offset` comes from `match_indices`, so `content[..offset]` is always a
    // valid char boundary; `unwrap_or("")` keeps this total without panicking.
    content
        .get(..offset)
        .unwrap_or("")
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

/// Index of the occurrence of `old_text` whose start line is nearest `near_line`.
fn nearest_match(content: &str, matches: &[usize], near_line: usize) -> usize {
    let mut best = matches.first().copied().unwrap_or_default();
    let mut best_dist = usize::MAX;
    for &offset in matches {
        let dist = line_of_offset(content, offset).abs_diff(near_line);
        if dist < best_dist {
            best_dist = dist;
            best = offset;
        }
    }
    best
}

/// A short "closest match" hint for a not-found `old_text`: the line numbers
/// of up to five lines that contain the first non-empty line of `old_text`.
/// Best-effort — `None` when nothing plausible matches.
fn closest_line_hint(content: &str, old_text: &str) -> Option<String> {
    let needle = old_text.lines().next()?.trim();
    if needle.is_empty() {
        return None;
    }
    let mut hits: Vec<usize> = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.contains(needle) {
            hits.push(index + 1);
            if hits.len() >= 5 {
                break;
            }
        }
    }
    if hits.is_empty() {
        None
    } else {
        Some(
            hits.iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

fn apply_text_edits(
    original_content: &str,
    edits: &[TextEditArgs],
) -> Result<AppliedEditSummary, String> {
    let original = original_content.to_string();
    let mut content = original.clone();
    let mut replacement_count = 0usize;
    let mut char_delta = 0isize;

    for (index, edit) in edits.iter().enumerate() {
        // Normalize the edit's own text too, so a model that copied CRLF bytes
        // still matches (and `new_text` is LF before the file ending is
        // restored on write).
        let old_text = to_lf(&edit.old_text);
        let new_text = to_lf(&edit.new_text);

        if old_text.is_empty() {
            return Err(format!("edit {}: old_text must not be empty", index + 1));
        }
        if old_text == new_text {
            return Err(format!(
                "edit {}: old_text and new_text are identical (no change)",
                index + 1
            ));
        }

        let matches = content
            .match_indices(&old_text)
            .map(|(offset, _)| offset)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Err(match closest_line_hint(&content, &old_text) {
                Some(lines) => format!(
                    "edit {}: old_text not found (closest match at line {lines})",
                    index + 1
                ),
                None => format!("edit {}: old_text not found", index + 1),
            });
        }

        let replace_all = edit.replace_all.unwrap_or(false);
        let replacements_for_edit = if replace_all {
            content = content.replace(&old_text, &new_text);
            matches.len()
        } else if matches.len() == 1 {
            let offset = matches.first().copied().unwrap_or_default();
            content.replace_range(offset..offset + old_text.len(), &new_text);
            1
        } else if let Some(near_line) = edit.near_line {
            // Ambiguous, but the caller pinned a line: replace the occurrence
            // nearest it (the `near_line`-as-anchor path that pairs with
            // `read_file`'s line-numbered view).
            let offset = nearest_match(&content, &matches, near_line);
            content.replace_range(offset..offset + old_text.len(), &new_text);
            1
        } else {
            let lines = matches
                .iter()
                .map(|&offset| line_of_offset(&content, offset).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "edit {}: old_text matched {} locations (lines {lines}); \
                 pass near_line to disambiguate or replace_all to replace all",
                index + 1,
                matches.len()
            ));
        };

        replacement_count += replacements_for_edit;
        // usize→isize char-count delta: the delta may legitimately be negative
        // (shorter replacement), so the cast is the intended wrap, not a bug.
        #[allow(clippy::cast_possible_wrap)]
        {
            char_delta += (new_text.chars().count() as isize - old_text.chars().count() as isize)
                * replacements_for_edit as isize;
        }
    }

    Ok(AppliedEditSummary {
        content,
        original: Some(original),
        replacement_count,
        char_delta,
    })
}

fn format_edit_result(action: &str, path: &str, summary: &AppliedEditSummary) -> String {
    let mut out = format!(
        "{action} file: {path} ({} replacement{}, {:+} chars)",
        summary.replacement_count,
        if summary.replacement_count == 1 {
            ""
        } else {
            "s"
        },
        summary.char_delta,
    );

    // Append diff if we have original content. Fenced via the shared helper so
    // a diff whose content contains a backtick run (e.g. editing a Markdown
    // file that holds a bare ``` line) cannot close the fence early in the
    // TUI's markdown parser — the same hardening `fence_content` applies to
    // blob/commit-message bodies. A backtick-free diff still gets the
    // canonical 3-backtick ```diff fence.
    if let Some(ref original) = summary.original {
        let diff = crate::diff_util::generate_diff(original, &summary.content, path, path);
        if !diff.is_empty() {
            out.push_str("\n\n");
            out.push_str(&super::fence_content(&diff, "diff"));
        }
    }

    out
}

pub fn describe_edit_file_invocation(args: &EditFileArgs) -> String {
    // Line-oriented (logs, TUI): sanitize the raw path so a control character
    // cannot split the line or inject terminal escapes.
    let mut parts = vec![format!(
        "Editing file `{}` with {} edit(s).",
        sanitize_content(&args.path),
        args.edits.len()
    )];
    if let Some(ref sha) = args.expected_sha256 {
        parts.push(format!(" Expecting SHA-256: {sha}."));
    }
    if args.dry_run.unwrap_or(false) {
        parts.push(" Dry run (no changes will be applied).".to_string());
    }
    parts.concat()
}

pub(crate) struct EditFile;

define_tool!(
    EditFile,
    "edit_file",
    "Edit a UTF-8 text file by applying one or more exact text replacements. Each edit must match at least once; non-replace_all edits must match exactly once. When old_text matches multiple locations, pass near_line (the 1-based line number from read_file) to replace the nearest, or replace_all to replace every occurrence. old_text is the raw file text — do not include read_file's `N | ` line-number gutter.",
    EditFileArgs,
    execute_edit_file_tool,
    "core",
    describe_edit_file_invocation
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file
    }

    fn edit(path: &Path, edits: Vec<TextEditArgs>) -> Result<String, ToolExecError> {
        execute_edit_file_tool(
            &EditFileArgs {
                path: path.display().to_string(),
                edits,
                expected_sha256: None,
                dry_run: None,
            },
            None,
        )
    }

    fn edit_args(old: &str, new: &str) -> TextEditArgs {
        TextEditArgs {
            old_text: old.into(),
            new_text: new.into(),
            replace_all: None,
            near_line: None,
        }
    }

    #[test]
    fn apply_text_edits_sets_original() {
        let summary = apply_text_edits("hello world", &[edit_args("world", "there")]).unwrap();
        assert_eq!(summary.original, Some("hello world".into()));
        assert_eq!(summary.content, "hello there");
    }

    #[test]
    fn apply_text_edits_replaces_single_occurrence() {
        let summary = apply_text_edits("a b c", &[edit_args("b", "B")]).unwrap();
        assert_eq!(summary.content, "a B c");
        assert_eq!(summary.replacement_count, 1);
    }

    #[test]
    fn apply_text_edits_rejects_identical_text() {
        let err = apply_text_edits("abc", &[edit_args("b", "b")]).unwrap_err();
        assert!(err.contains("identical"), "{err}");
    }

    #[test]
    fn ambiguous_match_lists_candidate_lines() {
        let err = apply_text_edits("x\nfoo\nfoo\n", &[edit_args("foo", "bar")]).unwrap_err();
        assert!(err.contains("matched 2 locations"), "{err}");
        assert!(err.contains("lines 2, 3"), "{err}");
        assert!(err.contains("near_line"), "{err}");
    }

    #[test]
    fn near_line_picks_nearest_occurrence() {
        // Two identical lines; near_line 4 must edit the later one.
        let summary = apply_text_edits(
            "foo\nkeep\nfoo\nkeep\n",
            &[TextEditArgs {
                old_text: "foo".into(),
                new_text: "bar".into(),
                replace_all: None,
                near_line: Some(4),
            }],
        )
        .unwrap();
        assert_eq!(summary.content, "foo\nkeep\nbar\nkeep\n");
        assert_eq!(summary.replacement_count, 1);
    }

    #[test]
    fn not_found_reports_closest_line() {
        // "beta " (with a trailing space) is absent, but line 2 contains the
        // first line's trimmed text "beta", so the hint points at line 2.
        let err = apply_text_edits("alpha\nbeta\ngamma\n", &[edit_args("beta ", "x")]).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("closest match at line 2"), "{err}");
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let summary = apply_text_edits(
            "a\na\na\n",
            &[TextEditArgs {
                old_text: "a".into(),
                new_text: "b".into(),
                replace_all: Some(true),
                near_line: None,
            }],
        )
        .unwrap();
        assert_eq!(summary.content, "b\nb\nb\n");
        assert_eq!(summary.replacement_count, 3);
    }

    #[test]
    fn crlf_file_matches_and_restores_ending() {
        let file = write_temp("alpha\r\nbeta\r\ngamma\r\n");
        // old_text uses LF (as read_file shows); the tool normalizes.
        let out = edit(file.path(), vec![edit_args("beta\n", "BETA\n")]).unwrap();
        assert!(out.contains("edited"), "{out}");
        let written = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(written, "alpha\r\nBETA\r\ngamma\r\n");
    }

    #[test]
    fn lf_file_stays_lf() {
        let file = write_temp("alpha\nbeta\n");
        edit(file.path(), vec![edit_args("beta", "BETA")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(file.path()).unwrap(),
            "alpha\nBETA\n"
        );
    }

    #[test]
    fn dry_run_does_not_write() {
        let file = write_temp("alpha\n");
        let out = execute_edit_file_tool(
            &EditFileArgs {
                path: file.path().display().to_string(),
                edits: vec![edit_args("alpha", "beta")],
                expected_sha256: None,
                dry_run: Some(true),
            },
            None,
        )
        .unwrap();
        assert!(out.contains("would edit"), "{out}");
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "alpha\n");
    }

    #[test]
    fn describe_edit_file_invocation_with_sha_dry_run() {
        let args = EditFileArgs {
            path: "src/main.rs".into(),
            edits: vec![TextEditArgs {
                old_text: "foo".into(),
                new_text: "bar".into(),
                replace_all: Some(false),
                near_line: None,
            }],
            expected_sha256: Some("abc123".into()),
            dry_run: Some(true),
        };
        let desc = super::describe_edit_file_invocation(&args);
        assert!(desc.contains("Editing file `src/main.rs`"));
        assert!(desc.contains("1 edit(s)"));
        assert!(desc.contains("Expecting SHA-256: abc123"));
        assert!(desc.contains("Dry run"));
    }
}
