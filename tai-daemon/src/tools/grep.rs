use super::{ToolError, context::ToolContext, truncate_tool_output};
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::Walk;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tai_keystore::ServiceCredential;

/// Default result limit when the caller doesn't specify one.
const DEFAULT_MAX_RESULTS: u32 = 50;

/// Hard upper bound on results — prevents runaway searches from flooding the
/// LLM context window.
const MAX_RESULTS_CAP: u32 = 200;

#[derive(Debug, Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    pub include: Option<String>,
    pub path: Option<String>,
    pub max_results: Option<u32>,
}

/// Stateless, zero-sized tool that searches file contents using the
/// ripgrep ecosystem (grep-regex + grep-searcher + ignore).
///
/// Respects `.gitignore`, hidden files, and binary files by default.
pub struct Grep;

pub fn execute_grep_tool(args: &GrepArgs, working_dir: Option<&Path>) -> Result<String, ToolError> {
    // Resolve the search root — use the provided path or default to working_dir
    let path = args.path.as_deref().unwrap_or(".");
    let resolved = super::confine_path(path, working_dir)?;

    // Build the pattern matcher: literal text or regex depending on flags
    let matcher: RegexMatcher = if args.regex {
        RegexMatcher::new(&args.pattern)
            .map_err(|e| ToolError::Other(format!("invalid regex pattern: {e}")))?
    } else {
        // `fixed_string(true)` escapes all regex metacharacters so the
        // pattern is matched as a literal substring.
        RegexMatcherBuilder::new()
            .fixed_strings(true)
            .build(&args.pattern)
            .map_err(|e| ToolError::Other(format!("invalid pattern: {e}")))?
    };

    // Optionally build a GlobSet from the `include` filter so we can
    // skip files that don't match the caller's path constraint.
    let glob_set: Option<GlobSet> = if let Some(ref include) = args.include {
        let mut builder = GlobSetBuilder::new();
        builder.add(
            Glob::new(include)
                .map_err(|e| ToolError::Other(format!("invalid include glob: {e}")))?,
        );
        Some(
            builder
                .build()
                .map_err(|e| ToolError::Other(format!("invalid glob set: {e}")))?,
        )
    } else {
        None
    };

    // Clamp max_results to the configured bounds so the caller can't
    // request an unbounded or absurdly large result set.
    let max_results = args
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CAP) as usize;

    // Walk the directory tree with .gitignore-aware traversal. By default
    // ignore::Walk also skips hidden files and respects ignore rules.
    let walk = Walk::new(&resolved);

    // Shared sink collects matches across all visited files. The
    // `current_path` field is updated before each file so the sink
    // knows which file a match belongs to (SinkMatch does not carry
    // path information in grep-searcher 0.1.x).
    let mut sink = GrepSink {
        max_results,
        results: Vec::new(),
        done: false,
        current_path: PathBuf::new(),
    };

    let mut searcher = SearcherBuilder::new().build();

    for entry in walk {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // Per-entry walk errors (permissions, broken symlinks, etc.)
                // are logged but do not abort the entire search.
                tracing::warn!(error = %e, "grep walk error, skipping entry");
                continue;
            }
        };

        // Skip non-file entries (directories, symlinks, etc.)
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }

        // Apply the include glob filter if one was configured
        if let Some(ref gs) = glob_set
            && !gs.is_match(entry.path())
        {
            continue;
        }

        // Tell the sink which file we're about to search so it can
        // attach the path to any matches it collects.
        sink.current_path = entry.path().to_path_buf();

        // Search the file. Individual read errors are non-fatal — we log
        // and continue to the next file.
        if let Err(e) = searcher.search_path(&matcher, entry.path(), &mut sink) {
            tracing::debug!(
                path = %entry.path().display(),
                error = %e,
                "grep search error on file, skipping"
            );
        }

        // Stop early once we've accumulated enough results
        if sink.done {
            break;
        }
    }

    if sink.results.is_empty() {
        return Ok(String::new());
    }

    // Format each match as "relative_path:line_number:content" — the same
    // format used by traditional grep, which LLMs parse reliably.
    let lines: Vec<String> = sink
        .results
        .iter()
        .map(|(path, line_num, content)| {
            let rel = path.strip_prefix(&resolved).unwrap_or(path);
            format!("{}:{}:{}", rel.display(), line_num, content)
        })
        .collect();

    Ok(truncate_tool_output(&lines.join("\n")))
}

impl super::Tool for Grep {
    type Args = GrepArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "grep"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Search file contents for a pattern. Respects .gitignore, hidden, and binary files. Results in file:line:content format."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Text pattern to search for in file contents"
                },
                "regex": {
                    "type": "boolean",
                    "description": "If true, pattern is treated as a regular expression (Rust regex syntax)",
                    "default": false
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files by path (e.g. '*.rs', 'src/**/*.rs', '*.{ts,js}')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: session working directory)"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of matching lines to return",
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
        execute_grep_tool(&args, working_dir)
    }
}

/// Custom Sink that collects matching lines up to a configured limit.
///
/// This avoids buffering the entire result set in memory and lets us
/// short-circuit the search as soon as the limit is hit.
///
/// Note: `SinkMatch` in grep-searcher 0.1.x does not expose the file path,
/// so we set `current_path` on the sink from the outer walk loop before
/// each `search_path` call.
struct GrepSink {
    /// Maximum number of results to collect.
    max_results: usize,
    /// Accumulated (path, line_number, line_content) triples.
    results: Vec<(PathBuf, u64, String)>,
    /// Once true, the searcher should stop for the current file and the
    /// outer walk loop should break.
    done: bool,
    /// Path of the file currently being searched (set by the outer loop).
    current_path: PathBuf,
}

impl Sink for GrepSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        // Already reached the global limit — tell the searcher to stop
        // searching the current file immediately.
        if self.done {
            return Ok(false);
        }

        // Extract match metadata. The path comes from `self.current_path`
        // (set by the outer loop) since `SinkMatch` in grep-searcher 0.1.x
        // doesn't carry path information. Line number defaults to 0 when
        // line counting is disabled (though we enable it by default).
        let path = self.current_path.clone();
        let line_number = mat.line_number().unwrap_or(0);
        let content = String::from_utf8_lossy(mat.bytes()).to_string();

        self.results.push((path, line_number, content));

        // If we've collected enough results, signal termination globally.
        if self.results.len() >= self.max_results {
            self.done = true;
            return Ok(false);
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use std::io::Write;
    use tempfile::TempDir;

    /// Create a temporary directory with a known set of files for testing.
    fn setup_test_dir() -> TempDir {
        let dir = TempDir::new().expect("failed to create temp dir for grep tests");

        // A Rust source file with two function definitions and a comment
        {
            let mut f = std::fs::File::create(dir.path().join("test1.rs"))
                .expect("failed to create test1.rs");
            writeln!(f, "fn hello() {{}}").expect("write error");
            writeln!(f, "fn world() {{}}").expect("write error");
            writeln!(f, "// this is a comment").expect("write error");
        }

        // A Python source file
        {
            let mut f = std::fs::File::create(dir.path().join("test2.py"))
                .expect("failed to create test2.py");
            writeln!(f, "def hello(): pass").expect("write error");
            writeln!(f, "def world(): pass").expect("write error");
        }

        // A plain text data file
        {
            let mut f = std::fs::File::create(dir.path().join("data.txt"))
                .expect("failed to create data.txt");
            writeln!(f, "hello world").expect("write error");
            writeln!(f, "goodbye world").expect("write error");
            writeln!(f, "foo bar").expect("write error");
        }

        dir
    }

    #[test]
    fn test_plain_text_match() {
        let dir = setup_test_dir();
        let tool = Grep;
        let args = GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // "hello" appears in test1.rs (function name), test2.py (function name),
        // and data.txt (first line).
        assert!(
            result.contains("test1.rs:1:fn hello()"),
            "expected match in test1.rs:\n{result}"
        );
        assert!(
            result.contains("test2.py:1:def hello(): pass"),
            "expected match in test2.py:\n{result}"
        );
        assert!(
            result.contains("data.txt:1:hello world"),
            "expected match in data.txt:\n{result}"
        );
    }

    #[test]
    fn test_regex_match() {
        let dir = setup_test_dir();
        let tool = Grep;
        let args = GrepArgs {
            pattern: r"fn \w+".to_string(),
            regex: true,
            include: Some("*.rs".to_string()),
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // Both `fn hello` and `fn world` should match in test1.rs
        assert!(
            result.contains("test1.rs:1:fn hello()"),
            "expected fn hello():\n{result}"
        );
        assert!(
            result.contains("test1.rs:2:fn world()"),
            "expected fn world():\n{result}"
        );

        // The include filter *.rs should exclude test2.py and data.txt
        assert!(
            !result.contains("test2.py"),
            "include=*.rs should exclude .py files"
        );
        assert!(
            !result.contains("data.txt"),
            "include=*.rs should exclude .txt files"
        );
    }

    #[test]
    fn test_include_filter() {
        let dir = setup_test_dir();
        let tool = Grep;
        let args = GrepArgs {
            pattern: "world".to_string(),
            regex: false,
            include: Some("*.rs".to_string()),
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // Only test1.rs should be searched
        assert!(
            result.contains("test1.rs:2:fn world()"),
            "expected match in test1.rs:\n{result}"
        );
        assert!(
            !result.contains("test2.py"),
            "include=*.rs should exclude .py files"
        );
        assert!(
            !result.contains("data.txt"),
            "include=*.rs should exclude .txt files"
        );
    }

    #[test]
    fn test_max_results_cap() {
        let dir = setup_test_dir();
        let tool = Grep;
        let args = GrepArgs {
            pattern: "world".to_string(),
            regex: false,
            include: None,
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
        let tool = Grep;
        let args = GrepArgs {
            pattern: "nonexistent".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // No file contains "nonexistent" — result should be empty
        assert!(result.is_empty(), "expected empty result, got:\n{result}");
    }

    #[test]
    fn test_case_sensitivity() {
        let dir = setup_test_dir();
        let tool = Grep;
        let args = GrepArgs {
            pattern: "HELLO".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();

        // fixed_string(true) performs an exact case-sensitive match by default.
        // "HELLO" (uppercase) should not match "hello" (lowercase).
        assert!(
            result.is_empty(),
            "expected case-sensitive no match, got:\n{result}"
        );
    }
}
