use super::{ensure_parent_directories, validate_nonempty_path, write_text_file};
use crate::tools::{ToolExecError, resolve_path};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{io, path::Path};
use tracing::{info, warn};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Relative or absolute path to the file to write
    pub path: String,
    /// Full UTF-8 file contents to write
    pub content: String,
    /// Whether to overwrite an existing file
    pub overwrite: Option<bool>,
    /// Whether to create missing parent directories
    pub create_parents: Option<bool>,
}

pub fn execute_write_file_tool(
    args: &WriteFileArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = validate_nonempty_path(&args.path)?;
    let resolved = resolve_path(&path, working_dir);
    ensure_parent_directories(&resolved, args.create_parents.unwrap_or(true))?;

    match write_text_file(&resolved, &args.content, args.overwrite.unwrap_or(true)) {
        Ok(()) => {
            info!(path = %resolved.display(), bytes = args.content.len(), "write_file: wrote file");
            let lang = ext_to_lang(&resolved.display().to_string());
            let fenced = fence_content(&args.content, lang);
            Ok(format!("wrote file: {}\n\n{}", resolved.display(), fenced))
        }
        Err(error) => {
            let overwrite = args.overwrite.unwrap_or(true);
            if !overwrite && error.kind() == io::ErrorKind::AlreadyExists {
                warn!(path = %resolved.display(), "write_file: refusing to overwrite existing file");
                Err(ToolExecError(format!(
                    "refusing to overwrite existing file: {}",
                    resolved.display()
                )))
            } else {
                warn!(path = %resolved.display(), error = %error, "write_file: failed to write file");
                Err(ToolExecError(format!("{error}")))
            }
        }
    }
}

fn ext_to_lang(path: &str) -> &'static str {
    let p = std::path::Path::new(path);
    // Filename-based detection for files without extensions (e.g., Dockerfile).
    if let Some(fname) = p.file_name().and_then(|n| n.to_str())
        && fname.eq_ignore_ascii_case("dockerfile")
    {
        return "dockerfile";
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext.to_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" | "tsx" | "mts" | "cts" => "javascript",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "mdown" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" | "sass" => "scss",
        "sh" | "bash" | "zsh" => "bash",
        "c" => "c",
        "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "sql" => "sql",
        "xml" => "xml",
        "dockerfile" => "dockerfile",
        "makefile" | "mk" => "makefile",
        "lua" => "lua",
        "zig" => "zig",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "pl" | "pm" => "perl",
        "tex" => "latex",
        "proto" => "protobuf",
        _ => "",
    }
}

fn fence_content(content: &str, lang: &str) -> String {
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
    let fence_len = (max_run + 1).max(3);
    let fence = "`".repeat(fence_len);
    format!("{fence}{lang}\n{trimmed}\n{fence}")
}

pub fn describe_write_file_invocation(args: &WriteFileArgs) -> String {
    format!(
        "Writing {} bytes to file `{}` (overwrite: {}, create_parents: {}).",
        args.content.len(),
        args.path,
        args.overwrite.unwrap_or(false),
        args.create_parents.unwrap_or(false)
    )
}

pub(crate) struct WriteFile;

define_tool!(
    WriteFile,
    "write_file",
    "Write a UTF-8 text file to the local workspace.",
    WriteFileArgs,
    execute_write_file_tool,
    "core",
    describe_write_file_invocation
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_write_file_invocation() {
        let args = WriteFileArgs {
            path: "output.txt".into(),
            content: "hello world".into(),
            overwrite: Some(true),
            create_parents: Some(false),
        };
        let desc = super::describe_write_file_invocation(&args);
        assert_eq!(
            desc,
            "Writing 11 bytes to file `output.txt` (overwrite: true, create_parents: false)."
        );
    }

    // ── ext_to_lang tests ────────────────────────────────────────────

    #[test]
    fn ext_to_lang_rust() {
        assert_eq!(super::ext_to_lang("src/main.rs"), "rust");
    }

    #[test]
    fn ext_to_lang_python() {
        assert_eq!(super::ext_to_lang("script.py"), "python");
    }

    #[test]
    fn ext_to_lang_javascript() {
        assert_eq!(super::ext_to_lang("app.js"), "javascript");
    }

    #[test]
    fn ext_to_lang_typescript_mapped_to_javascript() {
        assert_eq!(super::ext_to_lang("app.ts"), "javascript");
        assert_eq!(super::ext_to_lang("app.tsx"), "javascript");
        assert_eq!(super::ext_to_lang("app.mts"), "javascript");
        assert_eq!(super::ext_to_lang("app.cts"), "javascript");
    }

    #[test]
    fn ext_to_lang_dockerfile_detected_by_filename() {
        assert_eq!(super::ext_to_lang("Dockerfile"), "dockerfile");
        assert_eq!(super::ext_to_lang("path/to/Dockerfile"), "dockerfile");
    }

    #[test]
    fn ext_to_lang_dockerfile_extension() {
        assert_eq!(super::ext_to_lang("config.dockerfile"), "dockerfile");
    }

    #[test]
    fn ext_to_lang_unknown_extension() {
        assert_eq!(super::ext_to_lang("file.xyzzy"), "");
    }

    #[test]
    fn ext_to_lang_no_extension() {
        assert_eq!(super::ext_to_lang("Makefile"), "");
    }

    #[test]
    fn ext_to_lang_markdown() {
        assert_eq!(super::ext_to_lang("README.md"), "markdown");
    }

    #[test]
    fn ext_to_lang_shell() {
        assert_eq!(super::ext_to_lang("script.sh"), "bash");
        assert_eq!(super::ext_to_lang("script.bash"), "bash");
    }

    #[test]
    fn ext_to_lang_protobuf() {
        assert_eq!(super::ext_to_lang("message.proto"), "protobuf");
    }

    #[test]
    fn ext_to_lang_toml() {
        assert_eq!(super::ext_to_lang("Cargo.toml"), "toml");
    }

    #[test]
    fn ext_to_lang_yaml() {
        assert_eq!(super::ext_to_lang("config.yaml"), "yaml");
        assert_eq!(super::ext_to_lang("config.yml"), "yaml");
    }

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
