use super::{
    Tool, ToolExecError, context::ToolContext, finish_tool_output, human_size, sanitize_name,
    symlink_target_label, truncation_marker,
};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::borrow::Cow;
use std::path::Path;
use std::sync::mpsc;
use tracing::debug;
use zlob::walk::{WalkBuilder, WalkEntryKind, WalkFlags, WalkMetadata, WalkState};
use zlob::{ZlobFlags, ZlobPattern};

/// Default result limit when the caller doesn't specify one.
const DEFAULT_MAX_RESULTS: u32 = 50;

/// Hard upper bound on results — prevents runaway searches from flooding the
/// LLM context window.
const MAX_RESULTS_CAP: u32 = 200;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindArgs {
    /// File name pattern to search for (supports glob like '*.rs')
    pub pattern: String,
    /// When true, treat pattern as a glob instead of substring match.
    /// When false (default), auto-detects glob metacharacters (`*`, `?`, `[`, `{`, `!`, `~`):
    /// if present, glob matching is used; otherwise, case-insensitive substring match.
    /// Set to false explicitly to force substring matching for patterns that
    /// happen to contain glob wildcards. Escape glob characters with `\` to
    /// match them literally in any mode.
    #[serde(default)]
    pub glob: bool,
    /// Directory to search in (defaults to working directory)
    pub path: Option<String>,
    /// Maximum number of matching files to return
    pub max_results: Option<u32>,
}

/// Stateless, zero-sized tool that finds files and directories by name.
///
/// Supports case-insensitive substring matching and glob-based pattern matching.
/// Glob mode is auto-detected when the pattern contains wildcard characters
/// (`*`, `?`, `[`, `{`, `!`, `~`). Use the `glob` parameter to override
/// auto-detection. Escape glob characters with `\` to match them literally.
/// Respects `.gitignore` and hidden files via zlob's gitignore-aware walker.
pub struct Find;

/// Determine whether the given search pattern should be treated as a glob.
/// When `glob` is explicitly true, always use glob matching. When false,
/// auto-detect: if the pattern contains wildcard characters (`*`, `?`, `[`,
/// `{`, `!`, `~`), glob is used; otherwise, case-insensitive substring
/// matching is used.
fn use_glob_pattern(pattern: &str, glob: bool) -> bool {
    glob || zlob::has_wildcards(pattern, ZlobFlags::RECOMMENDED)
}

/// Normalize a find pattern for matching against root-relative paths:
///
/// - A leading `./` is a path prefix, not part of any file name — strip it
///   (repeatedly) so `./src/*.rs` behaves like `src/*.rs` instead of silently
///   matching nothing.
/// - An absolute pattern must live under the search root to be expressible as
///   a root-relative pattern (zlob's walker `include()` matches against
///   root-relative paths). If it does, convert it to the equivalent relative
///   pattern; otherwise error rather than silently returning nothing.
fn normalize_find_pattern<'a>(
    pattern: &'a str,
    resolved: &Path,
) -> Result<Cow<'a, str>, ToolExecError> {
    let mut p = pattern;
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    if Path::new(p).is_absolute() {
        match Path::new(p).strip_prefix(resolved) {
            Ok(rel) => return Ok(Cow::Owned(rel.to_string_lossy().into_owned())),
            Err(_) => {
                return Err(ToolExecError(format!(
                    "pattern `{pattern}` is outside the search root `{}`",
                    resolved.display()
                )));
            }
        }
    }
    Ok(Cow::Borrowed(p))
}

/// Run the find walk with the given parameters, optionally streaming each
/// match to `output_tx` as it is found (for incremental client display).
fn run_find_walk(
    resolved: &Path,
    pattern: &str,
    glob: bool,
    max_results: u32,
    output_tx: Option<&mpsc::Sender<Vec<u8>>>,
) -> Result<String, ToolExecError> {
    let use_glob = use_glob_pattern(pattern, glob);
    // Normalize before deciding the matching mode: `./src/*.rs` must become
    // `src/*.rs` (a leading `./` is a path prefix) and an absolute pattern
    // under the root must become root-relative, or the include() matcher
    // (which compares root-relative paths) would silently match nothing.
    let pattern = normalize_find_pattern(pattern, resolved)?;
    debug!(pattern = %pattern, resolved = %resolved.display(), use_glob, max_results, "find: starting search");

    // Glob patterns containing a path separator are handed to the walker via
    // `include()`: zlob matches them against the entry's root-relative path
    // AND prunes directories outside the pattern's literal prefix, so
    // `src/**/*.rs` only ever descends into `src/`. Bare patterns (no `/`)
    // keep basename matching, preserving the "find by file name" contract —
    // the same split GlobFilter applies to grep's include argument.
    let native_include = use_glob && pattern.contains('/');
    let glob_matcher: Option<ZlobPattern> = if use_glob && !native_include {
        Some(
            ZlobPattern::compile(&pattern, ZlobFlags::RECOMMENDED)
                .map_err(|e| ToolExecError(format!("invalid glob pattern: {e}")))?,
        )
    } else {
        None
    };

    // Pre-lowercase the pattern once for case-insensitive substring matching
    // so we don't pay this cost on every entry.
    let pattern_lower = pattern.to_lowercase();

    // Clamp max_results to the configured bounds so the caller can't
    // request an unbounded or absurdly large result set.
    let max_results = max_results.clamp(1, MAX_RESULTS_CAP) as usize;
    let mut results: Vec<String> = Vec::new();
    // Set when the walk stops early at the max_results cap; drives the
    // `...[truncated at N results]` marker so the caller knows more exist.
    let mut truncated = false;

    let mut builder = WalkBuilder::new(resolved)
        .map_err(|e| ToolExecError(format!("failed to create walker: {e}")))?;
    builder.options(WalkFlags::RECOMMENDED);
    if native_include {
        // PERIOD so the glob matches whatever the walker yields — the walker,
        // not the glob, controls hidden-file visibility. Same choice as
        // GlobFilter in glob_util.rs.
        builder
            .include(&pattern)
            .map_err(|e| ToolExecError(format!("invalid glob pattern: {e}")))?
            .include_flags(ZlobFlags::RECOMMENDED | ZlobFlags::PERIOD);
    }

    // Walk the directory tree with gitignore-aware traversal.
    // WalkFlags::RECOMMENDED skips hidden files and respects .gitignore rules.
    // WalkMetadata::SIZE makes the walker fill in file sizes during its
    // existing lstat pass — no extra syscalls from Rust.
    builder
        .metadata(WalkMetadata::SIZE)
        .run_serial(|entry| {
            // Check whether the entry's name matches the search pattern. With
            // native include() the walker has already filtered by relative path,
            // so no further matching is needed here — and we skip the basename
            // extraction entirely (it would be dead work for every entry).
            let matched = if native_include {
                true
            } else if let Some(ref matcher) = glob_matcher {
                // Glob mode (bare pattern): delegate to zlob's compiled matcher
                // on the entry's basename. Use to_string_lossy so non-UTF-8
                // filenames are handled via replacement characters rather than
                // silently skipped.
                let name = entry
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                matcher.matches_default(&name)
            } else {
                // Substring mode: case-insensitive contains check using the
                // pre-lowercased pattern.
                let name = entry
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                name.to_lowercase().contains(&pattern_lower)
            };

            if !matched {
                return WalkState::Continue;
            }

            // Relative path from the search root via zlob's built-in
            // relative_path() method — avoids a strip_prefix roundtrip.
            // Sanitize so a pathological name (e.g. one containing a newline)
            // cannot corrupt the line-oriented output.
            let rel = sanitize_name(&entry.relative_path().to_string_lossy());
            // Entry kind comes from the walker's lstat at no extra cost; file
            // sizes are present because SIZE metadata was requested.
            let line = match entry.kind() {
                // Append a trailing slash for directories — a visual cue that
                // the entry is a directory, matching common ls/find conventions.
                WalkEntryKind::Dir => format!("{rel}/"),
                WalkEntryKind::File => match entry.size() {
                    Some(size) => format!("{rel}  {}", human_size(size)),
                    None => rel,
                },
                // Symlinks (not followed under RECOMMENDED flags) render their
                // target so dir-links are visually distinct from file-links.
                WalkEntryKind::Symlink => {
                    format!("{rel} -> {}", symlink_target_label(entry.path()))
                }
                WalkEntryKind::Unknown => rel,
            };

            // Stream the result if a sender is configured so the client can
            // display matches incrementally rather than waiting for the
            // entire walk to complete.
            if let Some(tx) = output_tx {
                let _ = tx.send(format!("{line}\n").into_bytes());
            }
            results.push(line);

            // Stop early once we've accumulated enough results.
            if results.len() >= max_results {
                truncated = true;
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
        .map_err(|e| {
            // zlob's walker skips per-entry I/O errors internally (permission
            // denied, broken symlinks, etc.) — only truly fatal errors surface
            // here (e.g. root-dir missing, OOM).
            tracing::warn!(error = %e, "find walk aborted due to fatal error");
            ToolExecError(format!("walk error: {e}"))
        })?;

    // When the cap cut the walk short, append an explicit marker (and stream
    // it as a final chunk) so the caller can tell "exactly N results" from
    // "N of many more".
    let marker = truncation_marker(truncated, max_results, "results");
    if let (Some(marker), Some(tx)) = (&marker, output_tx) {
        let _ = tx.send(format!("{marker}\n").into_bytes());
    }

    if results.is_empty() {
        return Ok(String::new());
    }

    // Cap the body at the shared byte budget, appending the truncation marker
    // *after* the cap so the "N of many more" signal always survives even when
    // the result body alone exceeds the budget.
    Ok(finish_tool_output(&results.join("\n"), marker))
}

pub fn execute_find_tool(
    args: &FindArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = args.path.as_deref().unwrap_or(".");
    let resolved = super::resolve_path(path, working_dir);
    run_find_walk(
        &resolved,
        &args.pattern,
        args.glob,
        args.max_results.unwrap_or(DEFAULT_MAX_RESULTS),
        None,
    )
}

impl Tool for Find {
    type Args = FindArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "find"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Find files and directories by name. Glob auto-detected when pattern contains wildcards — set glob:true to force glob mode or glob:false to force substring matching. Patterns containing '/' match relative paths (e.g. 'src/*.rs') and prune traversal; bare patterns match file names. A leading './' is stripped and absolute patterns are matched relative to the search root (erroring when outside it). Use path to scope the search directory and max_results to cap matches (a '...[truncated at N results]' line is appended when the cap is hit — it means *at least* N exist). Respects .gitignore and hidden files. Results show file sizes, a trailing '/' on directories, and symlink targets (control characters escaped)."
    }

    fn supports_streaming_output() -> bool {
        true
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Searching for files matching `{}`.", args.pattern)];
        let use_glob = use_glob_pattern(&args.pattern, args.glob);
        if args.glob {
            parts.push(" Using glob matching (explicit).".to_string());
        } else if use_glob {
            parts.push(" Using glob matching (auto-detected).".to_string());
        } else {
            parts.push(" Using substring matching.".to_string());
        }
        match &args.path {
            Some(p) => parts.push(format!(" In path: `{}`.", p)),
            None => parts.push(" In working directory.".to_string()),
        }
        if let Some(max) = args.max_results {
            parts.push(format!(" Max results: {}.", max));
        }
        parts.concat()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        execute_find_tool(&args, working_dir)
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let path = args.path.as_deref().unwrap_or(".");
        let resolved = super::resolve_path(path, working_dir);
        run_find_walk(
            &resolved,
            &args.pattern,
            args.glob,
            args.max_results.unwrap_or(DEFAULT_MAX_RESULTS),
            Some(&output_tx),
        )
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use tempfile::TempDir;

    /// Create a temporary directory with a known directory structure for testing:
    ///
    /// ```text
    /// tmp/
    ///   foo.rs
    ///   bar.rs
    ///   src/
    ///     main.rs
    ///     lib.rs
    ///   test/
    ///     test_foo.rs
    /// ```
    fn setup_test_dir() -> TempDir {
        let dir = TempDir::new().expect("failed to create temp dir for find tests");

        // Top-level files
        std::fs::write(dir.path().join("foo.rs"), "").expect("write foo.rs");
        std::fs::write(dir.path().join("bar.rs"), "").expect("write bar.rs");

        // src/ directory with two Rust source files
        let src_dir = dir.path().join("src");
        std::fs::create_dir(&src_dir).expect("create src/");
        std::fs::write(src_dir.join("main.rs"), "").expect("write src/main.rs");
        std::fs::write(src_dir.join("lib.rs"), "").expect("write src/lib.rs");

        // test/ directory with one test file
        let test_dir = dir.path().join("test");
        std::fs::create_dir(&test_dir).expect("create test/");
        std::fs::write(test_dir.join("test_foo.rs"), "").expect("write test/test_foo.rs");

        dir
    }

    #[test]
    fn test_substring_match() {
        let dir = setup_test_dir();
        let tool = Find;
        let args = FindArgs {
            pattern: "foo".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // "foo" is a substring of "foo.rs" and "test_foo.rs"
        assert!(
            result.contains("foo.rs"),
            "expected foo.rs in results:\n{result}"
        );
        assert!(
            result.contains("test_foo.rs"),
            "expected test_foo.rs in results:\n{result}"
        );
        // "bar" does NOT contain "foo"
        assert!(!result.contains("bar.rs"), "expected no bar.rs:\n{result}");
    }

    #[test]
    fn test_glob_match_explicit() {
        let dir = setup_test_dir();
        let tool = Find;
        let args = FindArgs {
            pattern: "*.rs".to_string(),
            glob: true,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        assert!(result.contains("foo.rs"), "expected foo.rs:\n{result}");
        assert!(result.contains("bar.rs"), "expected bar.rs:\n{result}");
        assert!(
            result.contains("src/main.rs"),
            "expected src/main.rs:\n{result}"
        );
        assert!(
            result.contains("src/lib.rs"),
            "expected src/lib.rs:\n{result}"
        );
        assert!(
            result.contains("test/test_foo.rs"),
            "expected test/test_foo.rs:\n{result}"
        );
    }

    #[test]
    fn test_glob_auto_detect() {
        // `*.rs` has wildcards → auto-detected as glob, no `glob: true` needed.
        let dir = setup_test_dir();
        let tool = Find;
        let args = FindArgs {
            pattern: "*.rs".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        assert!(result.contains("foo.rs"), "expected foo.rs:\n{result}");
        assert!(result.contains("bar.rs"), "expected bar.rs:\n{result}");
        assert!(
            result.contains("src/main.rs"),
            "expected src/main.rs:\n{result}"
        );
        assert!(
            result.contains("test/test_foo.rs"),
            "expected test/test_foo.rs:\n{result}"
        );
    }

    #[test]
    fn test_glob_auto_detect_with_question_mark() {
        let dir = setup_test_dir();
        let tool = Find;
        let args = FindArgs {
            // foo.rs matched by f?o.rs or foo.?s
            pattern: "foo.?s".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        assert!(result.contains("foo.rs"), "expected foo.rs:\n{result}");
        assert!(!result.contains("bar.rs"), "expected no bar.rs:\n{result}");
    }

    #[test]
    fn test_case_insensitive() {
        let dir = setup_test_dir();
        let tool = Find;
        let args = FindArgs {
            pattern: "FOO".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // The pattern "FOO" lowercased to "foo" should match "foo.rs" and "test_foo.rs"
        assert!(
            result.contains("foo.rs"),
            "expected foo.rs (case-insensitive):\n{result}"
        );
        assert!(
            result.contains("test_foo.rs"),
            "expected test_foo.rs (case-insensitive):\n{result}"
        );
    }

    #[test]
    fn test_max_results_cap() {
        let dir = setup_test_dir();
        let tool = Find;
        let args = FindArgs {
            pattern: ".rs".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: Some(1),
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // With max_results=1 we get one match line plus the explicit
        // truncation marker so the caller knows more results exist.
        assert_eq!(
            result.lines().count(),
            2,
            "expected 1 result + truncation marker:\n{result}"
        );
        assert!(
            result.contains("...[truncated at 1 results]"),
            "expected truncation marker:\n{result}"
        );
    }

    #[test]
    fn test_no_match() {
        let dir = setup_test_dir();
        let tool = Find;
        let args = FindArgs {
            pattern: "nonexistent".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // No file contains "nonexistent" — result should be empty
        assert!(result.is_empty(), "expected empty result, got:\n{result}");
    }

    #[test]
    fn test_directories_get_trailing_slash() {
        let dir = setup_test_dir();
        let tool = Find;
        let args = FindArgs {
            pattern: "src".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        assert!(
            result.contains("src/"),
            "expected 'src/' with trailing slash:\n{result}"
        );
    }

    #[test]
    fn normalize_strips_leading_dot_slash() {
        // `./` is a path prefix, not part of any file name — it must not
        // silently kill the match.
        let p = Path::new("/proj");
        assert_eq!(normalize_find_pattern("./src/*.rs", p).unwrap(), "src/*.rs");
        assert_eq!(normalize_find_pattern("././a", p).unwrap(), "a");
        assert_eq!(normalize_find_pattern("plain.rs", p).unwrap(), "plain.rs");
        // A bare `./` degenerates to the empty pattern (matches everything,
        // same as a bare empty pattern).
        assert_eq!(normalize_find_pattern("./", p).unwrap(), "");
    }

    #[test]
    fn normalize_absolute_pattern_under_root_becomes_relative() {
        let root = Path::new("/proj");
        let abs = "/proj/src/*.rs";
        assert_eq!(normalize_find_pattern(abs, root).unwrap(), "src/*.rs");
        // The search root itself degenerates to the empty pattern.
        assert_eq!(normalize_find_pattern("/proj", root).unwrap(), "");
    }

    #[test]
    fn normalize_absolute_pattern_outside_root_errors() {
        let root = Path::new("/proj");
        let err = normalize_find_pattern("/elsewhere/x.rs", root).unwrap_err();
        assert!(
            err.0.contains("outside the search root"),
            "unexpected error: {err}"
        );
    }
}
