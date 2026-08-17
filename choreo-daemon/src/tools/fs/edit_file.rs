use super::{validate_nonempty_path, write_text_file};
use crate::tools::{ToolExecError, resolve_path, sha256_hex};
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
}

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

    let edit_summary = apply_text_edits(&original_content, &args.edits).map_err(ToolExecError)?;

    if args.dry_run.unwrap_or(false) {
        return Ok(format_edit_result(
            "would edit",
            &resolved.display().to_string(),
            &edit_summary,
        ));
    }

    match write_text_file(&resolved, &edit_summary.content, true) {
        Ok(()) => {
            info!(path = %resolved.display(), replacement_count = edit_summary.replacement_count, "edit_file: applied edits");
            Ok(format_edit_result(
                "edited",
                &resolved.display().to_string(),
                &edit_summary,
            ))
        }
        Err(error) => {
            warn!(path = %resolved.display(), error = %error, "edit_file: failed to write edited content");
            Err(ToolExecError(format!("{error}")))
        }
    }
}

struct AppliedEditSummary {
    content: String,
    original: Option<String>,
    replacement_count: usize,
    char_delta: isize,
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
        if edit.old_text.is_empty() {
            return Err(format!("edit {}: old_text must not be empty", index + 1));
        }

        let matches = content
            .match_indices(&edit.old_text)
            .map(|(offset, _)| offset)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Err(format!("edit {}: old_text not found", index + 1));
        }

        let replace_all = edit.replace_all.unwrap_or(false);
        let replacements_for_edit = if replace_all {
            matches.len()
        } else {
            if matches.len() != 1 {
                return Err(format!(
                    "edit {}: old_text matched {} locations; edit is ambiguous",
                    index + 1,
                    matches.len()
                ));
            }
            1
        };

        content = if replace_all {
            content.replace(&edit.old_text, &edit.new_text)
        } else {
            content.replacen(&edit.old_text, &edit.new_text, 1)
        };
        replacement_count += replacements_for_edit;
        char_delta += (edit.new_text.chars().count() as isize
            - edit.old_text.chars().count() as isize)
            * replacements_for_edit as isize;
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
    let mut parts = vec![format!(
        "Editing file `{}` with {} edit(s).",
        args.path,
        args.edits.len()
    )];
    if let Some(ref sha) = args.expected_sha256 {
        parts.push(format!(" Expecting SHA-256: {}.", sha));
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
    "Edit a UTF-8 text file by applying one or more exact text replacements. Each edit must match at least once; non-replace_all edits must match exactly once.",
    EditFileArgs,
    execute_edit_file_tool,
    "core",
    describe_edit_file_invocation
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_text_edits_sets_original() {
        let summary = apply_text_edits(
            "hello world",
            &[TextEditArgs {
                old_text: "world".into(),
                new_text: "there".into(),
                replace_all: None,
            }],
        )
        .unwrap();
        assert_eq!(summary.original, Some("hello world".into()));
    }

    #[test]
    fn apply_text_edits_replaces_single_occurrence() {
        let summary = apply_text_edits(
            "a b c",
            &[TextEditArgs {
                old_text: "b".into(),
                new_text: "B".into(),
                replace_all: None,
            }],
        )
        .unwrap();
        assert_eq!(summary.content, "a B c");
        assert_eq!(summary.replacement_count, 1);
    }

    #[test]
    fn describe_edit_file_invocation_with_sha_dry_run() {
        let args = EditFileArgs {
            path: "src/main.rs".into(),
            edits: vec![TextEditArgs {
                old_text: "foo".into(),
                new_text: "bar".into(),
                replace_all: Some(false),
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
