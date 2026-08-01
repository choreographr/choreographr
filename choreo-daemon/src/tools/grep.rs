use super::glob_util::GlobFilter;
use super::{
    Tool, ToolExecError, context::ToolContext, finish_tool_output, sanitize_name, truncation_marker,
};
use choreo_keystore::ServiceCredential;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use zlob::walk::{WalkBuilder, WalkFlags, WalkState};

/// Default result limit when the caller doesn't specify one.
const DEFAULT_MAX_RESULTS: u32 = 50;

/// Hard upper bound on results — prevents runaway searches from flooding the
/// LLM context window.
const MAX_RESULTS_CAP: u32 = 200;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Search pattern (plain text or regex)
    pub pattern: String,
    /// When true, treat pattern as a regular expression
    #[serde(default)]
    pub regex: bool,
    /// File glob pattern to filter which files are searched (e.g. '*.rs')
    pub include: Option<String>,
    /// Directory or file to search in (defaults to working directory)
    pub path: Option<String>,
    /// Maximum number of matching lines to return
    pub max_results: Option<u32>,
}

/// Stateless, zero-sized tool that searches file contents using the
/// ripgrep ecosystem (grep-regex + grep-searcher).
///
/// Respects `.gitignore`, hidden files, and binary files by default.
pub struct Grep;

/// Produce a hint string when the pattern looks like a regex but `regex` is
/// not enabled and no results were found.  Returns `None` when there is no
/// hint to give (results found, regex enabled, or pattern is plain text).
fn regex_mode_hint(pattern: &str, regex: bool, has_results: bool) -> Option<String> {
    if regex || has_results {
        return None;
    }
    // Check for common regex metacharacters that indicate the caller
    // likely intended regex semantics.
    let has_regex_chars = pattern.contains('|')
        || pattern.contains('(')
        || pattern.contains(')')
        || pattern.contains('^')
        || pattern.contains('$')
        || pattern.contains('+')
        || pattern.contains('*')
        || pattern.contains('?')
        || pattern.contains('[')
        || pattern.contains(']')
        || pattern.contains('\\');
    if !has_regex_chars {
        return None;
    }
    Some(
        "Note: pattern contains regex metacharacters but regex:false (default). \
         These characters were matched literally: `|`, `(`, `)`, `^`, `$`, `+`, `*`, `?`, `[`, `]`, `\\`. \
         If you intended regex, set regex:true."
            .to_string(),
    )
}

/// Run the grep walk with the given parameters, optionally streaming each
/// match to `output_tx` in real time for incremental client display.
fn run_grep_walk(
    resolved: &Path,
    pattern: &str,
    regex: bool,
    include: Option<&str>,
    max_results: u32,
    output_tx: Option<mpsc::Sender<Vec<u8>>>,
) -> Result<String, ToolExecError> {
    // Build the pattern matcher: literal text or regex depending on flags.
    let matcher: RegexMatcher = if regex {
        RegexMatcher::new(pattern)
            .map_err(|e| ToolExecError(format!("invalid regex pattern: {e}")))?
    } else {
        // `fixed_string(true)` escapes all regex metacharacters so the
        // pattern is matched as a literal substring.
        RegexMatcherBuilder::new()
            .fixed_strings(true)
            .build(pattern)
            .map_err(|e| ToolExecError(format!("invalid pattern: {e}")))?
    };

    // Compile the include glob. Patterns without `/` are matched against
    // the file's basename (gitignore convention) — a bare `Cargo.toml`
    // matches at any directory depth without needing an explicit `*` prefix.
    let include_filter: Option<GlobFilter> = if let Some(include) = include {
        Some(
            GlobFilter::compile(include)
                .map_err(|e| ToolExecError(format!("invalid include glob: {e}")))?,
        )
    } else {
        None
    };

    // Clamp max_results to the configured bounds so the caller can't
    // request an unbounded or absurdly large result set.
    let max_results = max_results.clamp(1, MAX_RESULTS_CAP) as usize;

    // When streaming is active, capture the search root for computing
    // relative paths in the streaming output.
    let search_root = output_tx.is_some().then(|| resolved.to_path_buf());
    let mut sink = GrepSink {
        max_results,
        results: Vec::new(),
        done: false,
        current_path: PathBuf::new(),
        output_tx,
        search_root,
    };

    let mut searcher = SearcherBuilder::new().build();

    // When the path points directly to a file (not a directory), search it
    // directly rather than going through the directory walker. zlob's
    // WalkBuilder does not yield the root entry when it is a file, so the
    // walk loop skips it silently. This also avoids .gitignore filtering
    // for explicitly-requested files.
    if resolved.is_file() {
        // The directly-named file has no directory context, so the include
        // glob is matched against the file name — the same path string the
        // output displays. Bare globs (`*.rs`) match by basename as before;
        // a path-anchored glob (`src/*.rs`) requires the pattern to match
        // the bare file name, consistent with the root-relative contract.
        let raw_name = resolved
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if let Some(ref filter) = include_filter
            && !filter.matches(Path::new(raw_name.as_ref()))
        {
            // File doesn't match the glob — return empty.
            return Ok(String::new());
        }
        // Sanitize so a pathological file name (e.g. one containing a
        // newline) cannot corrupt the line-oriented `path:line:content` output.
        let file_name = sanitize_name(&raw_name);

        // Search the file. `GrepSink` handles both streaming (when
        // `output_tx` is set) and collecting modes transparently.
        sink.current_path = resolved.to_path_buf();
        if let Err(e) = searcher.search_path(&matcher, resolved, &mut sink) {
            tracing::debug!(
                path = %resolved.display(),
                error = %e,
                "grep search error on file, skipping"
            );
        }

        let has_results = !sink.results.is_empty();
        let hint = regex_mode_hint(pattern, regex, has_results);
        if !has_results {
            return Ok(hint.unwrap_or_default());
        }
        let lines: Vec<String> = sink
            .results
            .iter()
            .map(|(_, line_num, content)| format!("{file_name}:{line_num}:{content}"))
            .collect();
        return Ok(assemble_grep_output(
            hint,
            lines.join("\n"),
            sink.done,
            max_results,
        ));
    }

    // Walk the directory tree with gitignore-aware traversal.
    // WalkFlags::RECOMMENDED skips hidden files and respects .gitignore rules.
    WalkBuilder::new(resolved)
        .map_err(|e| ToolExecError(format!("failed to create walker: {e}")))?
        .options(WalkFlags::RECOMMENDED)
        .run_serial(|entry| {
            // Skip non-file entries (directories, symlinks, etc.).
            if !entry.is_file() {
                return WalkState::Continue;
            }

            // Apply the include glob filter if one was configured. The glob
            // is matched against the entry's **root-relative** path (gitignore
            // convention, matching find's native include), so `src/*.rs`
            // matches `src/main.rs` regardless of where the search root
            // happens to live. Matching the absolute path instead would
            // silently return nothing for every anchored include.
            if let Some(ref filter) = include_filter
                && !filter.matches(entry.relative_path())
            {
                return WalkState::Continue;
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

            // Stop early once we've accumulated enough results.
            if sink.done {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
        .map_err(|e| {
            // zlob's walker skips per-entry I/O errors internally (permission
            // denied, broken symlinks, etc.) — only truly fatal errors surface
            // here (e.g. root-dir missing, OOM).
            tracing::warn!(error = %e, "grep walk aborted due to fatal error");
            ToolExecError(format!("walk error: {e}"))
        })?;

    let has_results = !sink.results.is_empty();
    let hint = regex_mode_hint(pattern, regex, has_results);
    if !has_results {
        return Ok(hint.unwrap_or_default());
    }

    let lines: Vec<String> = sink
        .results
        .iter()
        .map(|(path, line_num, content)| {
            let rel = path.strip_prefix(resolved).unwrap_or(path);
            // Sanitize the path so a pathological file name (e.g. one
            // containing a newline) cannot corrupt the line-oriented output.
            format!(
                "{}:{}:{}",
                sanitize_name(&rel.to_string_lossy()),
                line_num,
                content
            )
        })
        .collect();

    Ok(assemble_grep_output(
        hint,
        lines.join("\n"),
        sink.done,
        max_results,
    ))
}

pub fn execute_grep_tool(
    args: &GrepArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = args.path.as_deref().unwrap_or(".");
    let resolved = super::resolve_path(path, working_dir);
    run_grep_walk(
        &resolved,
        &args.pattern,
        args.regex,
        args.include.as_deref(),
        args.max_results.unwrap_or(DEFAULT_MAX_RESULTS),
        None,
    )
}

impl Tool for Grep {
    type Args = GrepArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "grep"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Search file contents for a pattern. Pattern is treated as a literal substring by default — set regex:true to use regular expressions (be sure to set regex:true if your pattern contains regex metacharacters like |, (, ), ^, $, +, etc. — without it they are matched literally). Use include to filter files by glob (e.g. \"*.rs\"); globs with '/' match root-relative paths (e.g. 'src/*.rs') and bare globs match file names. path scopes the search (a file or directory), and max_results caps matches (a '...[truncated at N matches]' line is appended when the cap is hit — it means *at least* N exist). Results in file:line:content format. Respects .gitignore, hidden, and binary files."
    }

    fn supports_streaming_output() -> bool {
        true
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Searching for `{}`.", args.pattern)];
        if args.regex {
            parts.push(" Using regex.".to_string());
        }
        if let Some(ref incl) = args.include {
            parts.push(format!(" Include pattern: `{}`.", incl));
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
        execute_grep_tool(&args, working_dir)
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
        run_grep_walk(
            &resolved,
            &args.pattern,
            args.regex,
            args.include.as_deref(),
            args.max_results.unwrap_or(DEFAULT_MAX_RESULTS),
            Some(output_tx),
        )
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

/// Assemble the final grep output: an optional regex-mode hint line, then the
/// match lines capped at the shared byte budget with the truncation marker
/// appended **past** the cap so the "N of many more" count signal always
/// survives even when the body alone exceeds the budget. Shared by the
/// single-file and directory paths.
fn assemble_grep_output(
    hint: Option<String>,
    body: String,
    truncated: bool,
    max_results: usize,
) -> String {
    let marker = truncation_marker(truncated, max_results, "matches");
    let capped = finish_tool_output(&body, marker);
    match hint {
        Some(h) => format!("{h}\n{capped}"),
        None => capped,
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
///
/// When `output_tx` is `Some`, each match is also streamed to the channel
/// in real time for incremental display.
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
    /// Optional streaming channel — when set, each match is sent here as
    /// a `path:line:content` line in real time.
    output_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Search root for computing relative paths in streaming output.
    search_root: Option<PathBuf>,
}

impl Sink for GrepSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.done {
            return Ok(false);
        }

        let path = self.current_path.clone();
        let line_number = mat.line_number().unwrap_or(0);
        // SinkMatch bytes include the line terminator (\n).  Strip it so
        // that joining results with "\n" does not produce blank lines.
        let content = String::from_utf8_lossy(mat.bytes())
            .trim_end_matches('\n')
            .to_string();

        // Stream the match line if a sender is configured, so the client
        // can display results incrementally rather than waiting for the
        // entire walk to finish.
        if let Some(ref tx) = self.output_tx {
            let rel = self
                .search_root
                .as_ref()
                .and_then(|root| path.strip_prefix(root).ok())
                .unwrap_or(&path);
            let line = format!(
                "{}:{}:{}\n",
                sanitize_name(&rel.to_string_lossy()),
                line_number,
                content
            );
            let _ = tx.send(line.into_bytes());
        }

        self.results.push((path, line_number, content));

        // If we've collected enough results, signal termination globally
        // and stream the truncation marker as the final chunk so the
        // client's incremental display ends with the same signal as the
        // fully-assembled output.
        if self.results.len() >= self.max_results {
            self.done = true;
            if let Some(ref tx) = self.output_tx
                && let Some(marker) = truncation_marker(true, self.max_results, "matches")
            {
                let _ = tx.send(format!("{marker}\n").into_bytes());
            }
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

        // With max_results=1 we get one match line plus the explicit
        // truncation marker so the caller knows more matches exist.
        assert_eq!(
            result.lines().count(),
            2,
            "expected 1 match + truncation marker:\n{result}"
        );
        assert!(
            result.contains("...[truncated at 1 matches]"),
            "expected truncation marker:\n{result}"
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

    #[test]
    fn describe_invocation_includes_pattern_and_path() {
        let tool = Grep;
        let args = GrepArgs {
            pattern: "fn main".into(),
            regex: false,
            include: None,
            path: Some("src".into()),
            max_results: None,
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Searching for `fn main`."));
        assert!(desc.contains("In path: `src`."));
    }

    #[test]
    fn describe_invocation_includes_regex_and_include() {
        let tool = Grep;
        let args = GrepArgs {
            pattern: "fn \\w+".into(),
            regex: true,
            include: Some("*.rs".into()),
            path: None,
            max_results: Some(50),
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Searching for `fn \\w+`."));
        assert!(desc.contains("Using regex."));
        assert!(desc.contains("Include pattern: `*.rs`."));
        assert!(desc.contains("Max results: 50."));
    }

    #[test]
    fn test_bare_filename_include_matches_at_any_depth() {
        let dir = TempDir::new().expect("temp dir");
        // Create a file at root level
        {
            let mut f =
                std::fs::File::create(dir.path().join("root.txt")).expect("create root.txt");
            writeln!(f, "content").expect("write");
        }
        // Create a file in a subdirectory
        {
            let sub = dir.path().join("sub");
            std::fs::create_dir(&sub).expect("create subdir");
            let mut f = std::fs::File::create(sub.join("root.txt")).expect("create sub/root.txt");
            writeln!(f, "content").expect("write");
        }

        let tool = Grep;
        let args = GrepArgs {
            pattern: "content".to_string(),
            regex: false,
            // Bare filename with no path separator — matches by basename
            // at any directory depth.
            include: Some("root.txt".to_string()),
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result.lines().count(), 2, "expected 2 matches:\n{result}");
        assert!(
            result.lines().any(|l| l.contains("root.txt:1:content")),
            "expected root.txt:\n{result}"
        );
        assert!(
            result.lines().any(|l| l.contains("sub/root.txt:1:content")),
            "expected sub/root.txt:\n{result}"
        );
    }

    #[test]
    fn test_bare_filename_include_no_match() {
        let dir = setup_test_dir();
        let tool = Grep;
        let args = GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: Some("nonexistent.rs".to_string()),
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(result.is_empty(), "expected empty, got:\n{result}");
    }

    #[test]
    fn test_path_anchored_include_matches_relative_paths() {
        let dir = TempDir::new().expect("temp dir");
        // Create a file at root level (should NOT match `*/data.txt` — it is
        // not inside a subdirectory relative to the search root).
        {
            let mut f =
                std::fs::File::create(dir.path().join("data.txt")).expect("create data.txt");
            writeln!(f, "hello").expect("write");
        }
        // Create a file in subdir (SHOULD match `*/data.txt`).
        {
            let sub = dir.path().join("sub");
            std::fs::create_dir(&sub).expect("create subdir");
            let mut f = std::fs::File::create(sub.join("data.txt")).expect("create sub/data.txt");
            writeln!(f, "hello").expect("write");
        }

        let tool = Grep;
        // Pattern has a `/` so it's matched against the root-relative path.
        // `*/data.txt` requires the file to sit exactly one directory below
        // the search root — a root-level `data.txt` has no leading directory.
        let args = GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: Some("*/data.txt".to_string()),
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result.lines().count(), 1, "expected 1 match:\n{result}");
        assert!(
            result.contains("sub/data.txt:1:hello"),
            "expected sub/data.txt:\n{result}"
        );
    }

    #[test]
    fn test_src_anchored_include_matches_relative_to_root() {
        // The regression this guards: `src/*.rs` used to be matched against
        // the absolute path and silently returned nothing. It must match
        // `src/main.rs` relative to the search root wherever that root lives.
        let dir = TempDir::new().expect("temp dir");
        std::fs::create_dir(dir.path().join("src")).expect("create src");
        {
            let mut f =
                std::fs::File::create(dir.path().join("src/main.rs")).expect("create main.rs");
            writeln!(f, "hello").expect("write");
        }

        let tool = Grep;
        let args = GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: Some("src/*.rs".to_string()),
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("src/main.rs:1:hello"),
            "expected src/main.rs match:\n{result}"
        );
    }

    #[test]
    fn test_single_file_include_matches_basename() {
        let dir = setup_test_dir();
        let tool = Grep;
        // A directly-named file has no directory context: the include glob is
        // matched against the file name. A bare glob matches the basename.
        let file_path = dir.path().join("test1.rs");
        let args = GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: Some("*.rs".to_string()),
            path: Some(file_path.to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("test1.rs:1:fn hello()"),
            "expected match in test1.rs:\n{result}"
        );

        // A non-matching glob filters the file out entirely.
        let args = GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: Some("*.py".to_string()),
            path: Some(file_path.to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.is_empty(),
            "expected no match for *.py on test1.rs:\n{result}"
        );
    }

    #[test]
    fn test_file_path_direct() {
        let dir = setup_test_dir();
        let tool = Grep;
        // Point path directly at a single file, not a directory.
        let file_path = dir.path().join("test1.rs");
        let args = GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: None,
            path: Some(file_path.to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            !result.is_empty(),
            "expected match when path points directly to a file, got empty"
        );
        assert!(
            result.contains("test1.rs:1:fn hello()"),
            "expected match in test1.rs:\n{result}"
        );
    }

    #[test]
    fn test_regex_hint_on_empty_result() {
        let dir = setup_test_dir();
        let tool = Grep;
        // Pattern contains | but regex:false — no file contains literal "foo|bar".
        let args = GrepArgs {
            pattern: "foo|bar".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        // Should get the hint, not empty.
        assert!(
            !result.is_empty(),
            "expected a hint about regex metacharacters, got empty"
        );
        assert!(
            result.contains("regex metacharacters"),
            "expected hint containing 'regex metacharacters', got:\n{result}"
        );
    }

    #[test]
    fn test_no_hint_when_regex_enabled() {
        let dir = setup_test_dir();
        let tool = Grep;
        // regex:true, so no hint should be given even if pattern has metacharacters.
        // Pattern doesn't match anything as a regex either.
        let args = GrepArgs {
            pattern: "zxyz|quux".to_string(),
            regex: true,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        // Empty because foo|bar as regex matches nothing in the test dir.
        assert!(
            result.is_empty(),
            "expected empty result with regex:true (no match), got:\n{result}"
        );
    }

    #[test]
    fn test_no_hint_on_successful_match() {
        let dir = setup_test_dir();
        let tool = Grep;
        // Pattern has no regex chars and returns results — no hint.
        let args = GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        };
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(!result.is_empty(), "expected results, got empty");
        // Should not contain a hint about regex.
        assert!(
            !result.contains("regex"),
            "expected no hint about regex, got:\n{result}"
        );
    }
}
