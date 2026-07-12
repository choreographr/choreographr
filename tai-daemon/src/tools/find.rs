use super::{ToolError, context::ToolContext, truncate_tool_output};
use globset::Glob;
use ignore::Walk;
use serde::Deserialize;
use std::path::Path;
use tai_keystore::ServiceCredential;

/// Default result limit when the caller doesn't specify one.
const DEFAULT_MAX_RESULTS: u32 = 50;

/// Hard upper bound on results — prevents runaway searches from flooding the
/// LLM context window.
const MAX_RESULTS_CAP: u32 = 200;

#[derive(Debug, Deserialize)]
pub struct FindArgs {
    pub pattern: String,
    #[serde(default)]
    pub glob: bool,
    pub path: Option<String>,
    pub max_results: Option<u32>,
}

/// Stateless, zero-sized tool that finds files and directories by name.
///
/// Supports case-insensitive substring matching (default) and glob-based
/// pattern matching. Respects `.gitignore` and hidden files via the `ignore`
/// crate's `Walk` traversal.
pub struct Find;

pub fn execute_find_tool(args: &FindArgs, working_dir: Option<&Path>) -> Result<String, ToolError> {
    // Resolve the search root — use the provided path or default to "."
    let path = args.path.as_deref().unwrap_or(".");
    let resolved = super::confine_path(path, working_dir)?;

    // Build an optional glob matcher when the caller wants glob matching.
    // For substring mode we don't need a matcher — we do simple contains().
    let glob_matcher: Option<globset::GlobMatcher> = if args.glob {
        Some(
            Glob::new(&args.pattern)
                .map_err(|e| ToolError::Other(format!("invalid glob pattern: {e}")))?
                .compile_matcher(),
        )
    } else {
        None
    };

    // Pre-lowercase the pattern so we only pay the cost once for
    // case-insensitive substring matching on every entry's name.
    let pattern_lower = args.pattern.to_lowercase();

    // Clamp max_results to the configured bounds so the caller can't
    // request an unbounded or absurdly large result set.
    let max_results = args
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CAP) as usize;

    // Walk the directory tree with .gitignore-aware traversal.
    // ignore::Walk skips hidden files and respects ignore rules by default.
    let walk = Walk::new(&resolved);

    let mut results: Vec<String> = Vec::new();

    for entry in walk {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // Per-entry walk errors (permissions, broken symlinks, etc.)
                // are logged but do not abort the entire search.
                tracing::warn!(error = %e, "find walk error, skipping entry");
                continue;
            }
        };

        // Get the file or directory name for matching against the pattern
        let name = entry.file_name().to_string_lossy();

        // Check whether the entry's name matches the search pattern
        let matched = if let Some(ref matcher) = glob_matcher {
            // Glob mode: delegate to globset's compiled matcher
            matcher.is_match(name.as_ref())
        } else {
            // Substring mode: case-insensitive contains check
            name.to_lowercase().contains(&pattern_lower)
        };

        if !matched {
            continue;
        }

        // Compute the relative path from the search root so results are
        // easy to use (e.g. "src/main.rs" instead of "/abs/path/src/main.rs").
        let rel = entry
            .path()
            .strip_prefix(&resolved)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();

        // Append a trailing slash for directories — a visual cue that the
        // entry is a directory, matching common ls/find conventions.
        let rel = if entry.file_type().is_some_and(|t| t.is_dir()) {
            format!("{rel}/")
        } else {
            rel
        };

        results.push(rel);

        // Stop early once we've accumulated enough results
        if results.len() >= max_results {
            break;
        }
    }

    if results.is_empty() {
        return Ok(String::new());
    }

    Ok(truncate_tool_output(&results.join("\n")))
}

impl super::Tool for Find {
    type Args = FindArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "find"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Find files and directories by name. Respects .gitignore and hidden files."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "File/directory name to search for (case-insensitive substring match by default)"
                },
                "glob": {
                    "type": "boolean",
                    "description": "If true, pattern is treated as a glob (e.g. '*test*', '*.rs', 'src/**')",
                    "default": false
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: session working directory)"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return",
                    "default": 50,
                    "minimum": 1,
                    "maximum": 200
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        execute_find_tool(&args, working_dir)
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
    fn test_glob_match() {
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

        // With max_results=1 we should get exactly one line of output
        assert_eq!(
            result.lines().count(),
            1,
            "expected exactly 1 result:\n{result}"
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
}
