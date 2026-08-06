use super::glob_util::GlobFilter;
use super::{
    Tool, ToolExecError, context::ToolContext, finish_tool_output, sanitize_name, truncation_marker,
};
use choreo_keystore::ServiceCredential;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use schemars::JsonSchema;
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};
use zlob::walk::{WalkBuilder, WalkFlags, WalkState};

/// Default result limit when the caller doesn't specify one.
const DEFAULT_MAX_RESULTS: u32 = 50;

/// Hard upper bound on results — prevents runaway searches from flooding the
/// LLM context window.
const MAX_RESULTS_CAP: u32 = 200;

/// Upper bound on the `context` argument. Each match renders up to 2×N
/// surrounding lines, so the full 200-match cap with a context of 100 would
/// otherwise balloon to 40,000 lines before the shared byte budget cuts in.
/// The cap is advertised in the tool schema (`range(max = 100)`) and enforced
/// by clamping so an out-of-range request degrades gracefully instead of
/// erroring.
const MAX_CONTEXT_LINES: u32 = 100;

/// Output format for grep results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    /// `path:line:content` per match line, with optional context lines
    /// (`path-{line}-{content}`, `--` between non-contiguous groups).
    /// The default.
    #[default]
    Content,
    /// One deduplicated, sorted file path per line. Each file is searched
    /// only until its first hit (ripgrep `-l` semantics).
    FilesWithMatches,
    /// `path: N` per file — the number of *matching lines* per file
    /// (ripgrep `-c` semantics). Files with zero matches are omitted.
    Count,
}

impl fmt::Display for GrepOutputMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrepOutputMode::Content => write!(f, "content"),
            GrepOutputMode::FilesWithMatches => write!(f, "files_with_matches"),
            GrepOutputMode::Count => write!(f, "count"),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Search pattern (plain text or regex)
    pub pattern: String,
    /// When true, treat pattern as a regular expression
    #[serde(default)]
    pub regex: bool,
    /// When true, match case-insensitively. Applies to both literal and
    /// regex patterns (mirrors ripgrep's `--ignore-case`).
    #[serde(default)]
    pub ignore_case: bool,
    /// Number of context lines to show before and after each match
    /// (default: 0). Context lines are rendered as `path-{line}-{content}`
    /// and do NOT count against `max_results`.
    #[serde(default)]
    #[schemars(range(min = 0, max = 100))]
    pub context: u32,
    /// Output format: `content` (default), `files_with_matches`, or `count`
    #[serde(default)]
    pub output_mode: GrepOutputMode,
    /// File glob pattern to filter which files are searched (e.g. '*.rs')
    pub include: Option<String>,
    /// Directory or file to search in (defaults to working directory)
    pub path: Option<String>,
    /// Maximum number of results to return. In `content` mode this caps
    /// match lines; in the other two modes it caps files.
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
///
/// The message is deliberately short: the LLM reads this as an explanation
/// for a surprising empty result, so it states the one actionable fix and
/// stops there (no metacharacter enumeration).
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
        "Note: pattern matched literally. Set regex:true to interpret it as a regular expression."
            .to_string(),
    )
}

/// The result string for a search that found nothing: an explicit message the
/// model can distinguish from a failed/incomplete tool call. When the pattern
/// looks like an intended regex, the hint leads so it reads as an explanation
/// of *why* nothing matched.
fn empty_result(pattern: &str, regex: bool) -> String {
    match regex_mode_hint(pattern, regex, false) {
        Some(hint) => format!("{hint}\nNo matches found."),
        None => "No matches found.".to_string(),
    }
}

/// One renderable unit from a file's search, in stream order.
#[derive(Debug, Clone)]
enum GrepItem {
    /// A line that matched the pattern → rendered `path:line:content`.
    Match { line_number: u64, content: String },
    /// A surrounding context line → rendered `path-{line}-{content}`. Before
    /// and after context render identically, so `SinkContextKind` is not
    /// stored.
    Context { line_number: u64, content: String },
    /// Separator between non-contiguous context groups → rendered `--`.
    /// Emitted by the sink's `context_break` callback.
    Break,
}

/// Accumulates matches during the walk and renders them per `GrepOutputMode`.
///
/// grep-searcher drives this sink one file at a time. `SinkMatch`/`SinkContext`
/// expose the line but not the containing file, so the outer walk calls
/// `begin_file` before each `search_path` and `end_file` after it. The three
/// callbacks (`matched`, `context`, `context_break`) arrive in stream order,
/// so Content mode renders straight from the collected items — no post-hoc
/// file re-reading.
struct GrepSink {
    /// Active output mode — determines the cap unit and what is collected.
    output_mode: GrepOutputMode,
    /// Cap on match lines (Content) or files (FilesWithMatches, Count),
    /// already clamped to [1, MAX_RESULTS_CAP].
    max_results: usize,
    /// Path of the file currently being searched (set by `begin_file`).
    current_path: PathBuf,

    // Content-mode state: per-file ordered items, capped by match count.
    content_files: Vec<(PathBuf, Vec<GrepItem>)>,
    content_match_count: usize,

    // FilesWithMatches-mode state: one path per file (first hit only).
    matched_files: Vec<PathBuf>,

    // Count-mode state: tally for the current file, flushed to entries at
    // `end_file` so a per-file count reflects the whole file.
    count_file_lines: u64,
    count_entries: Vec<(PathBuf, u64)>,

    /// True once the cap is hit — the searcher must stop the current file
    /// and the outer walk must break. Doubles as the truncation flag
    /// (`truncation_marker(sink.done, …)`).
    done: bool,
}

impl GrepSink {
    fn new(output_mode: GrepOutputMode, max_results: usize) -> Self {
        GrepSink {
            output_mode,
            max_results,
            current_path: PathBuf::new(),
            content_files: Vec::new(),
            content_match_count: 0,
            matched_files: Vec::new(),
            count_file_lines: 0,
            count_entries: Vec::new(),
            done: false,
        }
    }

    /// Called by the walk loop before searching each file. Records the file
    /// being searched (grep-searcher's `SinkMatch` has no path) and opens the
    /// per-mode state for this file.
    fn begin_file(&mut self, path: &Path) {
        self.current_path = path.to_path_buf();
        if self.output_mode == GrepOutputMode::Content {
            self.content_files.push((path.to_path_buf(), Vec::new()));
        }
    }

    /// Called by the walk loop after searching each file. Count mode flushes
    /// the completed tally here — a per-file count must reflect the whole
    /// file, not just the lines seen before some other cap applied.
    fn end_file(&mut self) {
        if self.output_mode == GrepOutputMode::Count && self.count_file_lines > 0 {
            self.count_entries
                .push((self.current_path.clone(), self.count_file_lines));
            self.count_file_lines = 0;
            if self.count_entries.len() >= self.max_results {
                self.done = true;
            }
        }
    }
}

impl Sink for GrepSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.done {
            return Ok(false);
        }
        let line_number = mat.line_number().unwrap_or(0);
        // SinkMatch bytes include the line terminator (\n). Strip it so that
        // joining results with "\n" does not produce blank lines.
        let content = String::from_utf8_lossy(mat.bytes())
            .trim_end_matches('\n')
            .to_string();

        match self.output_mode {
            GrepOutputMode::Content => {
                if let Some((_, items)) = self.content_files.last_mut() {
                    items.push(GrepItem::Match {
                        line_number,
                        content,
                    });
                }
                self.content_match_count += 1;
                if self.content_match_count >= self.max_results {
                    self.done = true;
                    // Stop this file immediately; the walk loop breaks on
                    // `done` so remaining files are never opened.
                    return Ok(false);
                }
                Ok(true)
            }
            GrepOutputMode::FilesWithMatches => {
                // First hit per file is enough (rg -l semantics).
                self.matched_files.push(self.current_path.clone());
                if self.matched_files.len() >= self.max_results {
                    self.done = true;
                }
                // Stop this file after its first hit.
                Ok(false)
            }
            GrepOutputMode::Count => {
                // Count every matching line in the file; the cap applies to
                // the number of files reported, checked at `end_file`.
                self.count_file_lines += 1;
                Ok(true)
            }
        }
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        // Context is only ever configured for Content mode; the guard keeps
        // the sink correct even if that ever changes.
        if self.output_mode != GrepOutputMode::Content {
            return Ok(true);
        }
        if self.done {
            return Ok(false);
        }
        if let Some((_, items)) = self.content_files.last_mut() {
            items.push(GrepItem::Context {
                line_number: ctx.line_number().unwrap_or(0),
                content: String::from_utf8_lossy(ctx.bytes())
                    .trim_end_matches('\n')
                    .to_string(),
            });
        }
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        if !self.done
            && self.output_mode == GrepOutputMode::Content
            && let Some((_, items)) = self.content_files.last_mut()
        {
            items.push(GrepItem::Break);
        }
        Ok(true)
    }
}

/// Label for a matched file in output: root-relative in directory mode, the
/// file's own name for a directly-named file (matching the pre-existing
/// output shape).
fn path_label(path: &Path, resolved: &Path, single_file: bool) -> String {
    if single_file {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        path.strip_prefix(resolved)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

/// Content mode: per-file items in stream order. Match lines use `:`
/// separators, context lines `-` (ripgrep's -C convention), groups of
/// context are separated by `--`.
fn render_content(sink: &GrepSink, resolved: &Path, single_file: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (path, items) in &sink.content_files {
        if items.is_empty() {
            continue;
        }
        let label = sanitize_name(&path_label(path, resolved, single_file));
        for item in items {
            match item {
                GrepItem::Match {
                    line_number,
                    content,
                } => lines.push(format!("{label}:{line_number}:{content}")),
                GrepItem::Context {
                    line_number,
                    content,
                } => lines.push(format!("{label}-{line_number}-{content}")),
                GrepItem::Break => lines.push("--".to_string()),
            }
        }
    }
    lines.join("\n")
}

/// FilesWithMatches mode: one deduplicated, sorted path per hit file.
fn render_files(sink: &GrepSink, resolved: &Path, single_file: bool) -> String {
    let mut files: Vec<String> = sink
        .matched_files
        .iter()
        .map(|p| sanitize_name(&path_label(p, resolved, single_file)))
        .collect();
    // Deterministic ordering — the walk order is stable, but sorting removes
    // any dependence on traversal internals.
    files.sort();
    files.join("\n")
}

/// Count mode: `path: N` per file, sorted by path, zero-match files omitted.
fn render_count(sink: &GrepSink, resolved: &Path, single_file: bool) -> String {
    let mut entries: Vec<(String, u64)> = sink
        .count_entries
        .iter()
        .map(|(p, n)| (sanitize_name(&path_label(p, resolved, single_file)), *n))
        .collect();
    entries.sort();
    entries
        .iter()
        .map(|(p, n)| format!("{p}: {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render the collected sink per its output mode, appending the regex-mode
/// hint / "No matches found." message when nothing matched.
fn finish_grep(
    sink: GrepSink,
    resolved: &Path,
    single_file: bool,
    pattern: &str,
    regex: bool,
) -> String {
    let has_results = match sink.output_mode {
        GrepOutputMode::Content => sink.content_match_count > 0,
        GrepOutputMode::FilesWithMatches => !sink.matched_files.is_empty(),
        GrepOutputMode::Count => !sink.count_entries.is_empty(),
    };
    if !has_results {
        return empty_result(pattern, regex);
    }

    let body = match sink.output_mode {
        GrepOutputMode::Content => render_content(&sink, resolved, single_file),
        GrepOutputMode::FilesWithMatches => render_files(&sink, resolved, single_file),
        GrepOutputMode::Count => render_count(&sink, resolved, single_file),
    };
    let noun = if sink.output_mode == GrepOutputMode::Content {
        "matches"
    } else {
        "files"
    };
    assemble_grep_output(body, sink.done, sink.max_results, noun)
}

/// Parsed search configuration, shared by `run_grep_walk` and the `Tool`
/// impl so the walker doesn't take a long flat argument list.
struct GrepConfig<'a> {
    pattern: &'a str,
    regex: bool,
    ignore_case: bool,
    context: u32,
    output_mode: GrepOutputMode,
    include: Option<&'a str>,
    max_results: u32,
}

/// Run the grep walk with the given parameters.
fn run_grep_walk(resolved: &Path, config: GrepConfig<'_>) -> Result<String, ToolExecError> {
    let GrepConfig {
        pattern,
        regex,
        ignore_case,
        context,
        output_mode,
        include,
        max_results,
    } = config;

    // Build the pattern matcher: literal text or regex depending on flags.
    // `case_insensitive` applies to both paths (mirrors rg --ignore-case).
    let matcher: RegexMatcher = if regex {
        RegexMatcherBuilder::new()
            .case_insensitive(ignore_case)
            .build(pattern)
            .map_err(|e| ToolExecError(format!("invalid regex pattern: {e}")))?
    } else {
        // `fixed_string(true)` escapes all regex metacharacters so the
        // pattern is matched as a literal substring.
        RegexMatcherBuilder::new()
            .case_insensitive(ignore_case)
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
    let mut sink = GrepSink::new(output_mode, max_results);

    // Context is only meaningful in Content mode; the other modes ignore it
    // (and never enable it on the searcher, so no extra work is done).
    let mut builder = SearcherBuilder::new();
    if output_mode == GrepOutputMode::Content && context > 0 {
        let n = context.min(MAX_CONTEXT_LINES) as usize;
        builder.before_context(n).after_context(n);
    }
    let mut searcher = builder.build();

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
            // File doesn't match the glob — return the empty-result message.
            return Ok(empty_result(pattern, regex));
        }

        // Search the file. `GrepSink` collects per its output mode.
        sink.begin_file(resolved);
        if let Err(e) = searcher.search_path(&matcher, resolved, &mut sink) {
            tracing::debug!(
                path = %resolved.display(),
                error = %e,
                "grep search error on file, skipping"
            );
        }
        sink.end_file();
        return Ok(finish_grep(sink, resolved, true, pattern, regex));
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
            sink.begin_file(entry.path());

            // Search the file. Individual read errors are non-fatal — we log
            // and continue to the next file.
            if let Err(e) = searcher.search_path(&matcher, entry.path(), &mut sink) {
                tracing::debug!(
                    path = %entry.path().display(),
                    error = %e,
                    "grep search error on file, skipping"
                );
            }
            sink.end_file();

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

    Ok(finish_grep(sink, resolved, false, pattern, regex))
}

pub fn execute_grep_tool(
    args: &GrepArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = args.path.as_deref().unwrap_or(".");
    let resolved = super::resolve_path(path, working_dir);
    run_grep_walk(
        &resolved,
        GrepConfig {
            pattern: &args.pattern,
            regex: args.regex,
            ignore_case: args.ignore_case,
            context: args.context,
            output_mode: args.output_mode,
            include: args.include.as_deref(),
            max_results: args.max_results.unwrap_or(DEFAULT_MAX_RESULTS),
        },
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
        "Search file contents for a pattern. Patterns are matched literally by default — set regex:true to use a regular expression, ignore_case:true to ignore case, and context to show surrounding lines. Use output_mode (content, files_with_matches, or count) to change the result format. Use include to filter files by glob (e.g. \"*.rs\"); globs with '/' match root-relative paths (e.g. 'src/*.rs') and bare globs match file names. path scopes the search (a file or directory), and max_results caps matches — a '...[truncated at N matches]' line is appended when the cap is hit (it means *at least* N exist). Results in file:line:content format (context lines use file-line-content). Respects .gitignore, hidden, and binary files."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Searching for `{}`.", args.pattern)];
        if args.regex {
            parts.push(" Using regex.".to_string());
        }
        if args.ignore_case {
            parts.push(" Ignoring case.".to_string());
        }
        if args.context > 0 {
            parts.push(format!(" Showing {} context line(s).", args.context));
        }
        if args.output_mode != GrepOutputMode::Content {
            parts.push(format!(" Output mode: {}.", args.output_mode));
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

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

/// Assemble the final grep output, capped at the shared byte budget with the
/// truncation marker appended **past** the cap so the "N of many more" count
/// signal always survives even when the body alone exceeds the budget.
fn assemble_grep_output(body: String, truncated: bool, max_results: usize, noun: &str) -> String {
    let marker = truncation_marker(truncated, max_results, noun);
    finish_tool_output(&body, marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use std::io::Write;
    use tempfile::TempDir;

    /// A `GrepArgs` with sensible defaults so tests only override the fields
    /// they exercise.
    fn test_args(pattern: &str, path: Option<&Path>) -> GrepArgs {
        GrepArgs {
            pattern: pattern.to_string(),
            regex: false,
            ignore_case: false,
            context: 0,
            output_mode: GrepOutputMode::Content,
            include: None,
            path: path.map(|p| p.to_string_lossy().into_owned()),
            max_results: None,
        }
    }

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
        let args = test_args("hello", Some(dir.path()));
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
        let mut args = test_args(r"fn \w+", Some(dir.path()));
        args.regex = true;
        args.include = Some("*.rs".to_string());
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
        let mut args = test_args("world", Some(dir.path()));
        args.include = Some("*.rs".to_string());
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
        let mut args = test_args("world", Some(dir.path()));
        args.max_results = Some(1);
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
        let args = test_args("nonexistent", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();

        // No file contains "nonexistent" — the tool says so explicitly rather
        // than returning an ambiguous empty string.
        assert_eq!(result, "No matches found.");
    }

    #[test]
    fn test_case_sensitivity() {
        let dir = setup_test_dir();
        let tool = Grep;
        let args = test_args("HELLO", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();

        // fixed_string(true) performs an exact case-sensitive match by default.
        // "HELLO" (uppercase) should not match "hello" (lowercase).
        assert!(result.contains("No matches found."), "got:\n{result}");
    }

    #[test]
    fn test_ignore_case() {
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("test.txt"), "Hello World\nfoo\n").expect("write");

        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.ignore_case = true;
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("test.txt:1:Hello World"),
            "expected case-insensitive match:\n{result}"
        );
    }

    #[test]
    fn test_ignore_case_with_regex() {
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("test.txt"), "Hello World\n").expect("write");

        let tool = Grep;
        let mut args = test_args("^hello", Some(dir.path()));
        args.regex = true;
        args.ignore_case = true;
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("test.txt:1:Hello World"),
            "expected case-insensitive regex match:\n{result}"
        );
    }

    #[test]
    fn test_context_lines() {
        let dir = TempDir::new().expect("temp dir");
        {
            let mut f = std::fs::File::create(dir.path().join("test.txt")).expect("create");
            writeln!(f, "line1").expect("write");
            writeln!(f, "line2").expect("write");
            writeln!(f, "hello world").expect("write");
            writeln!(f, "line4").expect("write");
            writeln!(f, "line5").expect("write");
        }

        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.context = 2;
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(result.contains("test.txt-1-line1"), "{result}");
        assert!(result.contains("test.txt-2-line2"), "{result}");
        assert!(result.contains("test.txt:3:hello world"), "{result}");
        assert!(result.contains("test.txt-4-line4"), "{result}");
        assert!(result.contains("test.txt-5-line5"), "{result}");
        assert_eq!(result.lines().count(), 5, "{result}");
    }

    #[test]
    fn test_context_break_separator() {
        let dir = TempDir::new().expect("temp dir");
        {
            let mut f = std::fs::File::create(dir.path().join("test.txt")).expect("create");
            writeln!(f, "world").expect("write");
            writeln!(f, "a").expect("write");
            writeln!(f, "b").expect("write");
            writeln!(f, "c").expect("write");
            writeln!(f, "d").expect("write");
            writeln!(f, "e").expect("write");
            writeln!(f, "world").expect("write");
        }

        let tool = Grep;
        let mut args = test_args("world", Some(dir.path()));
        args.context = 1;
        let result = tool.execute(args, None, None, None).unwrap();
        // First match at line 1 (after-context line 2), second at line 7
        // (before-context line 6); the gap between lines 2 and 6 renders `--`.
        assert!(result.contains("test.txt:1:world"), "{result}");
        assert!(result.contains("test.txt-2-a"), "{result}");
        assert!(result.contains("test.txt-6-e"), "{result}");
        assert!(result.contains("test.txt:7:world"), "{result}");
        assert!(
            result.contains("--"),
            "expected context break separator:\n{result}"
        );
    }

    #[test]
    fn test_context_does_not_count_against_max_results() {
        let dir = TempDir::new().expect("temp dir");
        {
            let mut f = std::fs::File::create(dir.path().join("test.txt")).expect("create");
            writeln!(f, "a").expect("write");
            writeln!(f, "b").expect("write");
            writeln!(f, "hello").expect("write");
        }

        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.context = 2;
        args.max_results = Some(1);
        let result = tool.execute(args, None, None, None).unwrap();
        // The cap hits on the match (line 3); the searcher stops there, so the
        // two before-context lines + the match render, then the marker.
        assert_eq!(
            result.lines().count(),
            4,
            "expected 2 context + match + marker:\n{result}"
        );
        assert!(
            result.contains("test.txt:3:hello"),
            "expected match line:\n{result}"
        );
        assert!(
            result.contains("...[truncated at 1 matches]"),
            "expected truncation marker:\n{result}"
        );
    }

    #[test]
    fn test_files_with_matches() {
        let dir = setup_test_dir();
        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.output_mode = GrepOutputMode::FilesWithMatches;
        let result = tool.execute(args, None, None, None).unwrap();
        // Sorted, deduplicated paths — no line numbers or content.
        assert_eq!(result, "data.txt\ntest1.rs\ntest2.py", "{result}");
    }

    #[test]
    fn test_count_mode() {
        let dir = setup_test_dir();
        let tool = Grep;
        let mut args = test_args("world", Some(dir.path()));
        args.output_mode = GrepOutputMode::Count;
        let result = tool.execute(args, None, None, None).unwrap();
        // data.txt has "world" on lines 1-2; test1.rs and test2.py once each.
        assert!(result.contains("data.txt: 2"), "{result}");
        assert!(result.contains("test1.rs: 1"), "{result}");
        assert!(result.contains("test2.py: 1"), "{result}");
        assert_eq!(result.lines().count(), 3, "{result}");
    }

    #[test]
    fn test_files_mode_truncation() {
        let dir = setup_test_dir();
        let tool = Grep;
        let mut args = test_args("world", Some(dir.path()));
        args.output_mode = GrepOutputMode::FilesWithMatches;
        args.max_results = Some(1);
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result.lines().count(), 2, "1 file + marker:\n{result}");
        assert!(
            result.contains("...[truncated at 1 files]"),
            "expected files truncation marker:\n{result}"
        );
    }

    #[test]
    fn test_count_mode_ignores_context() {
        let dir = TempDir::new().expect("temp dir");
        {
            let mut f = std::fs::File::create(dir.path().join("test.txt")).expect("create");
            writeln!(f, "a").expect("write");
            writeln!(f, "hello").expect("write");
            writeln!(f, "c").expect("write");
        }

        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.output_mode = GrepOutputMode::Count;
        args.context = 5;
        let result = tool.execute(args, None, None, None).unwrap();
        // Context is meaningless in count mode — plain per-file counts.
        assert_eq!(result, "test.txt: 1", "{result}");
    }

    #[test]
    fn test_single_file_files_mode() {
        let dir = setup_test_dir();
        let file_path = dir.path().join("test1.rs");
        let tool = Grep;
        let mut args = test_args("hello", Some(&file_path));
        args.output_mode = GrepOutputMode::FilesWithMatches;
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "test1.rs", "{result}");
    }

    #[test]
    fn test_single_file_count_mode() {
        let dir = setup_test_dir();
        let file_path = dir.path().join("data.txt");
        let tool = Grep;
        let mut args = test_args("world", Some(&file_path));
        args.output_mode = GrepOutputMode::Count;
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "data.txt: 2", "{result}");
    }

    #[test]
    fn describe_invocation_includes_pattern_and_path() {
        let tool = Grep;
        let args = test_args("fn main", Some(Path::new("src")));
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Searching for `fn main`."));
        assert!(desc.contains("In path: `src`."));
    }

    #[test]
    fn describe_invocation_includes_regex_and_include() {
        let tool = Grep;
        let mut args = test_args("fn \\w+", None);
        args.regex = true;
        args.include = Some("*.rs".into());
        args.max_results = Some(50);
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Searching for `fn \\w+`."));
        assert!(desc.contains("Using regex."));
        assert!(desc.contains("Include pattern: `*.rs`."));
        assert!(desc.contains("Max results: 50."));
    }

    #[test]
    fn describe_invocation_includes_new_options() {
        let tool = Grep;
        let mut args = test_args("fn \\w+", None);
        args.regex = true;
        args.ignore_case = true;
        args.context = 3;
        args.output_mode = GrepOutputMode::Count;
        args.include = Some("*.rs".into());
        args.max_results = Some(25);
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Searching for `fn \\w+`."));
        assert!(desc.contains("Using regex."));
        assert!(desc.contains("Ignoring case."));
        assert!(desc.contains("Showing 3 context line(s)."));
        assert!(desc.contains("Output mode: count."));
        assert!(desc.contains("Include pattern: `*.rs`."));
        assert!(desc.contains("Max results: 25."));
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
        // Bare filename with no path separator — matches by basename
        // at any directory depth.
        let mut args = test_args("content", Some(dir.path()));
        args.include = Some("root.txt".to_string());
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
        let mut args = test_args("hello", Some(dir.path()));
        args.include = Some("nonexistent.rs".to_string());
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(result.contains("No matches found."), "got:\n{result}");
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
        let mut args = test_args("hello", Some(dir.path()));
        args.include = Some("*/data.txt".to_string());
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
        let mut args = test_args("hello", Some(dir.path()));
        args.include = Some("src/*.rs".to_string());
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
        let mut args = test_args("hello", Some(&file_path));
        args.include = Some("*.rs".to_string());
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("test1.rs:1:fn hello()"),
            "expected match in test1.rs:\n{result}"
        );

        // A non-matching glob filters the file out entirely.
        let mut args = test_args("hello", Some(&file_path));
        args.include = Some("*.py".to_string());
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("No matches found."),
            "expected no match for *.py on test1.rs:\n{result}"
        );
    }

    #[test]
    fn test_file_path_direct() {
        let dir = setup_test_dir();
        let tool = Grep;
        // Point path directly at a single file, not a directory.
        let file_path = dir.path().join("test1.rs");
        let args = test_args("hello", Some(&file_path));
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            !result.contains("No matches found."),
            "expected match when path points directly to a file, got:\n{result}"
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
        let args = test_args("foo|bar", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        // Should get the message plus a hint, not an empty string.
        assert!(
            result.contains("No matches found."),
            "expected explicit no-match message:\n{result}"
        );
        assert!(
            result.contains("regex:true"),
            "expected hint pointing at regex:true:\n{result}"
        );
    }

    #[test]
    fn test_no_hint_when_regex_enabled() {
        let dir = setup_test_dir();
        let tool = Grep;
        // regex:true, so no hint should be given even if pattern has
        // metacharacters. Pattern doesn't match anything as a regex either.
        let mut args = test_args("zxyz|quux", Some(dir.path()));
        args.regex = true;
        let result = tool.execute(args, None, None, None).unwrap();
        // Explicit no-match message, but no "matched literally" hint.
        assert!(
            result.contains("No matches found."),
            "expected no-match message:\n{result}"
        );
        assert!(
            !result.contains("Note: pattern matched literally"),
            "expected no hint with regex:true:\n{result}"
        );
    }

    #[test]
    fn test_no_hint_on_successful_match() {
        let dir = setup_test_dir();
        let tool = Grep;
        // Pattern has no regex chars and returns results — no hint.
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            !result.contains("No matches found."),
            "expected results, got:\n{result}"
        );
        // Should not contain a hint about regex.
        assert!(
            !result.contains("regex"),
            "expected no hint about regex, got:\n{result}"
        );
    }
}
