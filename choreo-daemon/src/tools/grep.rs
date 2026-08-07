use super::glob_util::GlobFilter;
use super::{
    MAX_LINE_DISPLAY_BYTES, MAX_TOOL_OUTPUT_BYTES, Tool, ToolExecError, context::ToolContext,
    finish_tool_output, sanitize_content, sanitize_name, truncation_marker,
};
use choreo_keystore::ServiceCredential;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::borrow::Cow;
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

/// Marker appended to an over-cap matched/context line, matching the
/// file-read tools' convention (`...[line truncated: exceeds 64 KiB]` in
/// `render_streamed_line`) so a shortened one-liner reads consistently across
/// tools.
const LINE_TRUNCATED_MARKER: &str = "...[line truncated: exceeds 64 KiB]";

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

impl GrepOutputMode {
    /// The noun used in the truncation marker for this mode: matches are
    /// capped in Content mode, files in the other two.
    fn cap_noun(self) -> &'static str {
        match self {
            GrepOutputMode::Content => "matches",
            GrepOutputMode::FilesWithMatches | GrepOutputMode::Count => "files",
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
    /// and do NOT count against `max_results`. Only applied in `content`
    /// output mode — `files_with_matches` and `count` ignore it.
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

/// Normalize one line from grep-searcher for display: strip exactly the
/// trailing line terminator, escape control characters (via the shared
/// [`sanitize_content`], which keeps tabs literal), and cap over-long lines
/// so a giant one-liner cannot balloon the result into memory.
///
/// grep-searcher hands back each line *including* exactly one terminator —
/// `\n` for LF files, `\r\n` for CRLF. Strip precisely that one sequence
/// (not *all* trailing CR/LF bytes) so a line whose data legitimately ends in
/// `\r` before a CRLF terminator keeps that `\r` (escaped) instead of losing
/// it.
///
/// Memory stays bounded per line even for pathological input: the byte window
/// is cut *before* `from_utf8_lossy`, because lossy conversion eagerly copies
/// a line containing any invalid UTF-8 into an owned String (replacement
/// chars). For a multi-MiB binary-ish one-liner that copy would dwarf every
/// allocation the output byte budget ever sees.
///
/// Returns the display string and whether the line was truncated by the line
/// cap — callers use the flag to detect pathological over-cap lines (e.g. to
/// bound the after-context drain).
fn sanitized_line(bytes: &[u8]) -> (String, bool) {
    // The window is the display cap plus the CRLF terminator and a couple of
    // bytes of UTF-8 slop; `cap_line` below re-cuts it to the exact cap on a
    // char boundary, so the slop only bounds the lossy-conversion cost.
    let window = &bytes[..bytes.len().min(MAX_LINE_DISPLAY_BYTES + 4)];
    let lossy = String::from_utf8_lossy(window);
    let line = lossy
        .strip_suffix("\r\n")
        .or_else(|| lossy.strip_suffix('\n'))
        .unwrap_or(&lossy);
    let (line, truncated) = cap_line(line);
    (sanitize_content(&line), truncated)
}

/// Cut `line` at [`MAX_LINE_DISPLAY_BYTES`] on a char boundary, appending the
/// shared `...[line truncated: exceeds 64 KiB]` marker so a silently
/// shortened one-liner is explicit. Borrows for under-cap lines (no
/// allocation); over-cap lines allocate exactly the capped prefix + marker.
/// Returns whether the line was cut, so callers can detect pathological
/// over-cap input.
fn cap_line(line: &str) -> (Cow<'_, str>, bool) {
    if line.len() <= MAX_LINE_DISPLAY_BYTES {
        return (Cow::Borrowed(line), false);
    }
    let split = line.floor_char_boundary(MAX_LINE_DISPLAY_BYTES);
    let mut capped = String::with_capacity(split + LINE_TRUNCATED_MARKER.len());
    capped.push_str(&line[..split]);
    capped.push_str(LINE_TRUNCATED_MARKER);
    (Cow::Owned(capped), true)
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

impl GrepItem {
    /// Render one item under its file label: matches use `:` separators,
    /// context lines `-` (ripgrep's -C convention), breaks render `--`.
    fn render(&self, label: &str) -> String {
        match self {
            GrepItem::Match {
                line_number,
                content,
            } => format!("{label}:{line_number}:{content}"),
            GrepItem::Context {
                line_number,
                content,
            } => format!("{label}-{line_number}-{content}"),
            GrepItem::Break => "--".to_string(),
        }
    }

    /// Number of bytes this item occupies in the rendered output under
    /// `label`: label + separator + line number + content, or `--` for a
    /// break. (The joining newline is charged by the caller.) This is the
    /// exact rendered size, so the byte-budget guard in [`GrepSink::push_item`]
    /// stops collection at the same threshold the renderer caps at.
    fn render_len(&self, label: &str) -> usize {
        match self {
            GrepItem::Match {
                line_number,
                content,
            }
            | GrepItem::Context {
                line_number,
                content,
            } => label.len() + 2 + decimal_len(*line_number) + content.len(),
            GrepItem::Break => 2,
        }
    }
}

/// Decimal digit count of `n` (0 → 1, 9 → 1, 10 → 2, 100 → 3) — sizes the
/// byte-budget accounting without allocating a `String` (which the previous
/// `line_number.to_string().len()` did per buffered item).
fn decimal_len(n: u64) -> usize {
    // ilog10(0) is None (one digit); otherwise digits = floor(log10(n)) + 1.
    n.checked_ilog10().map_or(1, |d| d as usize + 1)
}

/// Why the walk stopped searching early. Drives the `...[truncated at N …]`
/// marker and the walk loop's quit decision. `Cap` stops *matching* but the
/// capped match's after-context drain keeps running; `ByteBudget` stops
/// everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    /// The max_results cap was hit — at least `max_results` results exist.
    Cap,
    /// The buffered output passed [`MAX_TOOL_OUTPUT_BYTES`] — collection
    /// stopped before the cap; the marker reports the count collected.
    ByteBudget,
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
    /// Why the walk stopped (`None` = still collecting). `Cap` stops matching
    /// but the capped match's after-context drain continues; `ByteBudget`
    /// stops everything.
    stop: Option<StopReason>,
    /// Search root used to compute display labels: root-relative paths in
    /// directory mode, the bare file name for a directly-named file.
    resolved: PathBuf,
    /// Whether the search targets a single directly-named file (drives the
    /// label shape and the truncation-marker suppression for file-capped
    /// modes).
    single_file: bool,
    /// Path of the file currently being searched (set by `begin_file`).
    current_path: PathBuf,
    /// Sanitized display label of `current_path`, precomputed by `begin_file`
    /// so `push_item` can charge the exact rendered bytes to the budget and
    /// `render_content` can render without recomputing it.
    current_label: String,

    // Content-mode state: per-file (label, ordered items, charged byte
    // total), capped by match count. Buckets are opened lazily by
    // `push_item`; the charged total lets `drop_current_bucket` refund the
    // exact bytes so the budget stays precise across aborted files.
    content_files: Vec<(String, Vec<GrepItem>, usize)>,
    content_match_count: usize,
    /// Running byte total of the *rendered* output buffered in `content_files`
    /// — each item charges `label + separator + line number + content +
    /// newline`, the exact bytes `render_content` will produce. The renderer
    /// caps the final output at [`MAX_TOOL_OUTPUT_BYTES`], so buffering more
    /// than that is pure waste — and on a pathological tree (200 matches ×
    /// 201 context lines × 64 KiB lines) it could otherwise be gigabytes
    /// before `finish_grep` ever trims it.
    content_bytes: usize,

    // FilesWithMatches-mode state: one path per file (first hit only).
    matched_files: Vec<PathBuf>,

    // Count-mode state: tally for the current file, flushed to entries at
    // `end_file` so a per-file count reflects the whole file.
    count_file_lines: u64,
    count_entries: Vec<(PathBuf, u64)>,

    /// After-context lines configured for Content mode (0 otherwise). When
    /// `max_results` caps a match, the searcher is told to keep going so the
    /// capped match's trailing context is still delivered — ripgrep `-m` +
    /// `-C` semantics. [`GrepSink::after_context_remaining`] runs the drain.
    after_context: usize,
    /// Lines of the capped match's after-context window still to drain. The
    /// sink's counter is authoritative over the searcher's own, which resets
    /// whenever a line *matches*; lines inside the window are delivered as
    /// context (even matches, as rg `-m` shows them), and once this reaches
    /// zero the file stops.
    after_context_remaining: usize,
    /// Number of files the walk has *attempted* to search (set by
    /// `begin_file`). Used to decide whether the regex-mode hint is honest:
    /// when zero files were searched (include glob filtered everything, empty
    /// directory), the empty result cannot be blamed on the pattern.
    files_searched: usize,
    /// Set when the current file's search stopped early — a read error (from
    /// the walk loop) or binary-data truncation (from the searcher's
    /// `binary_data` callback). Partial output observed before the stop is
    /// not the file's true result, so `end_file` discards it: the count-mode
    /// tally is dropped and the content-mode bucket is removed. Callers set
    /// this via [`GrepSink::abort_file`]; `end_file` clears it.
    file_aborted: bool,
}

impl GrepSink {
    fn new(
        output_mode: GrepOutputMode,
        max_results: usize,
        after_context: usize,
        resolved: &Path,
        single_file: bool,
    ) -> Self {
        GrepSink {
            output_mode,
            max_results,
            stop: None,
            resolved: resolved.to_path_buf(),
            single_file,
            current_path: PathBuf::new(),
            current_label: String::new(),
            content_files: Vec::new(),
            content_match_count: 0,
            content_bytes: 0,
            matched_files: Vec::new(),
            count_file_lines: 0,
            count_entries: Vec::new(),
            after_context,
            after_context_remaining: after_context,
            files_searched: 0,
            file_aborted: false,
        }
    }

    /// Called by the walk loop before searching each file. Records the file
    /// being searched (grep-searcher's `SinkMatch` has no path) and its
    /// display label. Per-file output buckets are opened lazily by
    /// [`GrepSink::push_item`], so a file that yields no output never
    /// allocates one.
    fn begin_file(&mut self, path: &Path) {
        self.current_path = path.to_path_buf();
        self.current_label = sanitize_name(&path_label(path, &self.resolved, self.single_file));
        self.files_searched += 1;
        // Per-file abort/drain state is consumed by `end_file` and the capped
        // match's after-context drain; reset defensively so a stale flag can
        // never poison a later file.
        self.file_aborted = false;
        self.after_context_remaining = self.after_context;
    }

    /// Called by the walk loop after searching each file. Count mode flushes
    /// the completed tally here — a per-file count must reflect the whole
    /// file, not just the lines seen before some other cap applied. When the
    /// file's search was aborted (see [`GrepSink::abort_file`]), the partial
    /// output is discarded instead: the searcher may have stopped mid-file,
    /// so neither the count-mode tally nor the content-mode bucket collected
    /// before the stop is the file's true result. (This is what makes a file
    /// with a NUL byte past its head render as "skipped" in *every* output
    /// mode, matching ripgrep's `quit` semantics — not just count mode.)
    fn end_file(&mut self) {
        if self.file_aborted {
            if self.output_mode == GrepOutputMode::Content {
                // Drop the current file's bucket (and refund its charged
                // bytes) so matches observed before a NUL byte or read error
                // never render.
                self.drop_current_bucket();
            }
        } else if self.output_mode == GrepOutputMode::Count && self.count_file_lines > 0 {
            self.count_entries
                .push((self.current_path.clone(), self.count_file_lines));
            if self.count_entries.len() >= self.max_results {
                self.stop = Some(StopReason::Cap);
            }
        }
        if self.output_mode == GrepOutputMode::Count {
            // Always reset, so a partial tally never leaks into the next file.
            self.count_file_lines = 0;
        }
        self.file_aborted = false;
    }

    /// Mark the current file's search as not running to completion — a read
    /// error (from the walk loop) or binary-data truncation (from the
    /// searcher). At [`GrepSink::end_file`] the file's partial output is
    /// discarded rather than reported as its true result.
    fn abort_file(&mut self) {
        self.file_aborted = true;
    }

    /// Drop the current file's content bucket (if any) and refund its charged
    /// bytes, so a file whose search was aborted contributes nothing to the
    /// output and the byte budget stays exact. Match counts are refunded too,
    /// keeping `result_count` in line with what will actually render.
    fn drop_current_bucket(&mut self) {
        if let Some((label, items, charged)) = self.content_files.last()
            && label == &self.current_label
        {
            self.content_bytes -= *charged;
            // Refund the match tally so a file whose matches were all dropped
            // does not count as "has results" in `finish_grep`.
            let matches = items
                .iter()
                .filter(|i| matches!(i, GrepItem::Match { .. }))
                .count();
            self.content_match_count -= matches;
            self.content_files.pop();
        }
    }

    /// Cheap byte-budget pre-check for a *raw* line (not yet sanitized).
    /// Estimates the rendered size from the raw byte count capped at the
    /// per-line display cap: an over-cap line renders as at most
    /// [`MAX_LINE_DISPLAY_BYTES`] of content plus the truncation marker, so
    /// a multi-MiB line does not actually consume its raw length of the
    /// budget. Sanitization only ever *expands* an under-cap line (trimming
    /// the ≤2 terminator bytes is the sole shrink), so the raw count is a
    /// tight approximation there. If the estimate exceeds the budget, stop
    /// without allocating a sanitized copy that `push_item` would reject a
    /// moment later. Returns `false` to stop the searcher.
    fn budget_allows_raw(&mut self, line_number: u64, raw_len: usize) -> bool {
        // Content estimate = raw minus the terminator, capped at the line
        // cap (the marker appended by `cap_line` for over-cap lines is
        // omitted — a conservative under-count that never false-rejects).
        let content_est = raw_len.saturating_sub(2).min(MAX_LINE_DISPLAY_BYTES);
        // Rendered size ≈ label + separator + line digits + content +
        // newline (matches `render_len` + the joining newline).
        let estimate = self.content_bytes
            + self.current_label.len()
            + 2
            + decimal_len(line_number)
            + content_est
            + 1;
        if estimate > MAX_TOOL_OUTPUT_BYTES {
            self.stop = Some(StopReason::ByteBudget);
            return false;
        }
        true
    }

    /// Deliver one line of the capped match's after-context window (Content
    /// mode) as a context item. Returns `Ok(false)` when the searcher must
    /// stop: the window is exhausted, the byte budget was hit, or the line
    /// was truncated by the line cap (pathological input — filling the
    /// remaining window would make the searcher scan the rest of the file
    /// one giant line at a time, so deliver this one and stop).
    fn drain_after_context(
        &mut self,
        line_number: u64,
        bytes: &[u8],
    ) -> Result<bool, std::io::Error> {
        let (content, truncated) = sanitized_line(bytes);
        if !self.push_item(GrepItem::Context {
            line_number,
            content,
        }) {
            return Ok(false);
        }
        self.after_context_remaining -= 1;
        if truncated {
            return Ok(false);
        }
        Ok(true)
    }

    /// Append an item for the currently-searched file, opening a per-file
    /// bucket lazily on the first item. Files are searched strictly one at a
    /// time, so a bucket left open by a previous file is closed as soon as an
    /// item for the current file arrives (compared by label). A `Break` with
    /// no open bucket is a separator with nothing to separate — dropped
    /// rather than rendered as a stray `--`.
    ///
    /// Returns `false` when the byte budget stopped collection — the caller
    /// (the `matched`/`context` handlers) must then stop the searcher.
    fn push_item(&mut self, item: GrepItem) -> bool {
        // Defensive: once the byte budget is exhausted, nothing more may be
        // buffered even if a callback slips through before the searcher
        // stops. (The max_results cap does NOT stop collection — the
        // after-context drain keeps pushing context lines.)
        if self.stop == Some(StopReason::ByteBudget) {
            return false;
        }
        let matches_current = self
            .content_files
            .last()
            .map(|(label, _, _)| label == &self.current_label)
            .unwrap_or(false);
        if !matches_current {
            if matches!(item, GrepItem::Break) {
                return true;
            }
            self.content_files
                .push((self.current_label.clone(), Vec::new(), 0));
        }
        // Collapse consecutive Breaks: a separator directly after another
        // separator (grep-searcher never does this, but defend against it)
        // would render doubled `--` lines.
        if matches!(item, GrepItem::Break)
            && self.content_files.last().is_some_and(|(_, items, _)| {
                items.last().is_some_and(|i| matches!(i, GrepItem::Break))
            })
        {
            return true;
        }
        // Exact byte-budget guard (Content mode only): charge the rendered
        // bytes this item will occupy — label + separator + line number +
        // content, plus the joining newline for every item but the very
        // first (`render_content` joins with "\n", so the first item has no
        // preceding newline). This matches the bytes the renderer produces
        // exactly, so collection stops at the same threshold the renderer
        // caps at.
        let first_item = self.content_bytes == 0;
        let rendered = item.render_len(&self.current_label) + usize::from(!first_item);
        if self.content_bytes + rendered > MAX_TOOL_OUTPUT_BYTES {
            self.stop = Some(StopReason::ByteBudget);
            return false;
        }
        if let Some((_, items, charged)) = self.content_files.last_mut() {
            *charged += rendered;
            self.content_bytes += rendered;
            items.push(item);
        }
        true
    }

    /// Number of results collected under the active cap unit — match lines in
    /// Content mode, files in the other two. Doubles as the "has results"
    /// signal and the completion-event counter.
    fn result_count(&self) -> usize {
        match self.output_mode {
            GrepOutputMode::Content => self.content_match_count,
            GrepOutputMode::FilesWithMatches => self.matched_files.len(),
            GrepOutputMode::Count => self.count_entries.len(),
        }
    }

    /// Whether the output should carry the `...[truncated at N …]` marker:
    /// the walk stopped early, either at the max_results cap or at the byte
    /// budget. For a directly-named single file, the file-capped modes are
    /// provably complete once that one file is searched — the cap (≥ 1) was
    /// necessarily met, so claiming truncation would be misleading. Content
    /// mode keeps the marker because the searcher may genuinely have stopped
    /// mid-file at the cap or byte budget.
    fn truncated(&self) -> bool {
        self.stop.is_some() && !(self.single_file && self.output_mode != GrepOutputMode::Content)
    }

    /// Whether the walk should stop searching further files: the max_results
    /// cap was hit or the byte budget was exhausted.
    fn should_stop(&self) -> bool {
        self.stop.is_some()
    }

    /// The count reported in the truncation marker. When the byte budget
    /// stopped collection before the requested cap, the honest "at least N
    /// exist" figure is the number actually collected; otherwise the
    /// max_results cap itself.
    fn marker_count(&self) -> usize {
        if self.stop == Some(StopReason::ByteBudget) {
            self.result_count()
        } else {
            self.max_results
        }
    }
}

impl Sink for GrepSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let line_number = mat.line_number().unwrap_or(0);
        if self.stop == Some(StopReason::Cap) {
            // Draining the capped match's after-context window (Content mode):
            // a line inside the window that *also* matches is shown as a
            // context line, not as another match (rg -m + -C semantics). The
            // drain continues; the counter stops the file once the window is
            // exhausted.
            if self.output_mode == GrepOutputMode::Content && self.after_context_remaining > 0 {
                return self.drain_after_context(line_number, mat.bytes());
            }
            return Ok(false);
        }
        // A byte-budget stop should never reach here — the searcher stops as
        // soon as `push_item` reports the budget exhausted. Guard anyway so a
        // stray callback cannot buffer anything more.
        if self.stop.is_some() {
            return Ok(false);
        }

        match self.output_mode {
            GrepOutputMode::Content => {
                // Reject a line that would blow the byte budget before
                // sanitizing it — `sanitized_line` can allocate up to the
                // line cap, and `push_item` would discard the result anyway.
                if !self.budget_allows_raw(line_number, mat.bytes().len()) {
                    return Ok(false);
                }
                // SinkMatch bytes include the line terminator — strip it (and
                // a CR for CRLF files) so joining results with "\n" does not
                // produce blank lines, then escape any remaining control
                // characters so a hostile line cannot inject terminal escapes
                // into the tool result.
                let (content, _) = sanitized_line(mat.bytes());
                if !self.push_item(GrepItem::Match {
                    line_number,
                    content,
                }) {
                    // Byte budget exhausted — stop this file; the walk loop
                    // sees the stop and quits.
                    return Ok(false);
                }
                self.content_match_count += 1;
                if self.content_match_count >= self.max_results {
                    self.stop = Some(StopReason::Cap);
                    // Without after-context there is nothing left to drain, so
                    // stop the file here — otherwise the searcher would keep
                    // scanning the remainder of the file until the next match
                    // or EOF (a huge trailing tail after an early cap would be
                    // read in full for nothing). The walk loop breaks on the
                    // stop so remaining files are never opened.
                    if self.after_context == 0 {
                        return Ok(false);
                    }
                    // With after-context configured, keep the searcher running
                    // so this match's trailing context lines are still
                    // delivered (ripgrep -m + -C semantics); the `context`
                    // handler drains those. `builder.max_matches` below stops
                    // the searcher natively once the window is exhausted, so
                    // the tail after the window is never scanned.
                    return Ok(true);
                }
                Ok(true)
            }
            GrepOutputMode::FilesWithMatches => {
                // First hit per file is enough (rg -l semantics).
                self.matched_files.push(self.current_path.clone());
                if self.matched_files.len() >= self.max_results {
                    self.stop = Some(StopReason::Cap);
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
        if self.stop == Some(StopReason::Cap) {
            // Draining the capped match's after-context window: accept only
            // `After` lines while the window still has room. Anything else —
            // before-context of a later match, or the window exhausted —
            // means the group is over, so stop the searcher.
            if *ctx.kind() == SinkContextKind::After && self.after_context_remaining > 0 {
                return self.drain_after_context(ctx.line_number().unwrap_or(0), ctx.bytes());
            }
            return Ok(false);
        }
        if self.stop.is_some() {
            return Ok(false);
        }
        // Same pre-check as `matched`: a line that would exceed the byte
        // budget is rejected before the sanitizing allocation.
        if !self.budget_allows_raw(ctx.line_number().unwrap_or(0), ctx.bytes().len()) {
            return Ok(false);
        }
        let (content, _) = sanitized_line(ctx.bytes());
        if !self.push_item(GrepItem::Context {
            line_number: ctx.line_number().unwrap_or(0),
            content,
        }) {
            return Ok(false);
        }
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        if self.output_mode != GrepOutputMode::Content {
            return Ok(true);
        }
        if self.stop.is_some() {
            // A break only fires once the capped match's after-context window
            // is exhausted (there is a gap to the next match) — nothing left
            // to collect, so stop the searcher before it delivers the next
            // group's before-context.
            return Ok(false);
        }
        if !self.push_item(GrepItem::Break) {
            return Ok(false);
        }
        Ok(true)
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, Self::Error> {
        // With `BinaryDetection::quit` the searcher stops the file here
        // regardless of our response. Mark the file aborted so *any* output
        // observed before the NUL — a count-mode tally or a content-mode
        // bucket of matches — is discarded at `end_file`: the lines before a
        // binary truncation are not the file's true content, and the tool
        // documents binary files as skipped. Returning false is the explicit
        // "stop" signal for the searcher.
        self.abort_file();
        Ok(false)
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
fn render_content(sink: &GrepSink) -> String {
    let mut lines: Vec<String> = Vec::new();
    // Buckets are never empty (push_item opens one only to fill it), so no
    // empty-skip is needed here. The label was precomputed at push time so
    // the byte budget could charge exact rendered bytes; reuse it verbatim.
    for (label, items, _) in &sink.content_files {
        lines.extend(items.iter().map(|item| item.render(label)));
    }
    lines.join("\n")
}

/// FilesWithMatches mode: one deduplicated, sorted path per hit file.
fn render_files(sink: &GrepSink) -> String {
    let mut files: Vec<String> = sink
        .matched_files
        .iter()
        .map(|p| sanitize_name(&path_label(p, &sink.resolved, sink.single_file)))
        .collect();
    // Deterministic ordering — the walk order is stable, but sorting removes
    // any dependence on traversal internals.
    files.sort();
    files.join("\n")
}

/// Count mode: `path: N` per file, sorted by path, zero-match files omitted.
fn render_count(sink: &GrepSink) -> String {
    let mut entries: Vec<(String, u64)> = sink
        .count_entries
        .iter()
        .map(|(p, n)| {
            (
                sanitize_name(&path_label(p, &sink.resolved, sink.single_file)),
                *n,
            )
        })
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
fn finish_grep(sink: GrepSink, pattern: &str, regex: bool) -> String {
    let result_count = sink.result_count();
    // Log the completion for every search — including empty ones — so the
    // walk-start event in run_grep_walk always has a matching finish event.
    tracing::debug!(
        path = %sink.resolved.display(),
        output_mode = %sink.output_mode,
        result_count,
        // Report the marker state actually rendered (single-file file-capped
        // modes suppress it even when stopped), not the raw stop flag.
        truncated = sink.truncated(),
        "grep search finished"
    );
    if result_count == 0 {
        // If the walk never actually searched a file (an include glob filtered
        // everything out, an empty directory, …), the regex hint would
        // misattribute the empty result to the pattern — the plain message is
        // accurate, matching the directly-named-file include-filter path.
        if sink.files_searched == 0 {
            return "No matches found.".to_string();
        }
        return empty_result(pattern, regex);
    }

    let body = match sink.output_mode {
        GrepOutputMode::Content => render_content(&sink),
        GrepOutputMode::FilesWithMatches => render_files(&sink),
        GrepOutputMode::Count => render_count(&sink),
    };
    assemble_grep_output(
        body,
        sink.truncated(),
        // Report the honest count: the max_results cap when the cap stopped the
        // walk, or the actually-collected count when the byte budget did.
        sink.marker_count(),
        sink.output_mode.cap_noun(),
    )
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

    // Clamp max_results to the configured bounds so the caller can't request
    // an unbounded or absurdly large result set — before the walk-start log
    // so the event records the value actually applied.
    let max_results = max_results.clamp(1, MAX_RESULTS_CAP) as usize;
    // Same for context: clamp before the log so the event records what the
    // searcher (and the sink's after-context drain) will actually apply.
    let context = context.min(MAX_CONTEXT_LINES) as usize;

    tracing::debug!(
        path = %resolved.display(),
        pattern = %pattern,
        regex,
        ignore_case,
        context,
        output_mode = %output_mode,
        max_results,
        "grep walk starting"
    );

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

    // The sink tracks the capped match's after-context window with its own
    // counter (the searcher's resets whenever a line matches), so both the
    // builder and the sink need the same clamped count. The sink also needs
    // the search root and single-file flag up front: it precomputes each
    // file's display label at `begin_file` so the byte budget can charge
    // exact rendered bytes.
    let single_file = resolved.is_file();
    let after_context = if output_mode == GrepOutputMode::Content && context > 0 {
        context
    } else {
        0
    };
    let mut sink = GrepSink::new(
        output_mode,
        max_results,
        after_context,
        resolved,
        single_file,
    );

    // Context is only meaningful in Content mode; the other modes ignore it
    // (and never enable it on the searcher, so no extra work is done).
    let mut builder = SearcherBuilder::new();
    if after_context > 0 {
        builder
            .before_context(after_context)
            .after_context(after_context);
    }
    // Cap matches at the searcher level too (Content mode only): without
    // max_matches the searcher would keep scanning the current file after the
    // sink's own cap (it only stops on a callback returning false, which never
    // fires for a long tail of non-matching lines). With max_matches set, the
    // searcher stops natively as soon as the capped match's after-context
    // window is exhausted (rg -m + -C semantics), bounding per-file work after
    // the cap. Count mode must NOT set it — it counts every matching line in a
    // file, and the searcher's own cap would silently truncate that count.
    if output_mode == GrepOutputMode::Content {
        builder.max_matches(Some(max_results as u64));
    }
    // Treat files with a NUL byte in the head as binary and skip them
    // (ripgrep's default), honouring the documented contract and keeping a
    // binary blob from flooding the result with garbage lines. The per-line
    // cap above would bound the damage, but not searching binary files at all
    // is the semantically correct behaviour.
    builder.binary_detection(BinaryDetection::quit(b'\0'));
    let mut searcher = builder.build();

    // When the path points directly to a file (not a directory), search it
    // directly rather than going through the directory walker. zlob's
    // WalkBuilder does not yield the root entry when it is a file, so the
    // walk loop skips it silently. This also avoids .gitignore filtering
    // for explicitly-requested files.
    if single_file {
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
            // The glob excluded the file before any search ran, so the regex
            // hint would misattribute the empty result to the pattern. The
            // plain no-match message is accurate here.
            return Ok("No matches found.".to_string());
        }

        // Search the file. `GrepSink` collects per its output mode.
        sink.begin_file(resolved);
        if let Err(e) = searcher.search_path(&matcher, resolved, &mut sink) {
            // A directly-named file that cannot be searched is a real error:
            // reporting "No matches found." would mislead the caller into
            // thinking the file exists and simply has no hits. Directory
            // sweeps keep the skip-and-continue behavior (one unreadable file
            // among thousands must not fail the whole walk), but a single
            // explicitly-addressed file has nothing to hide behind.
            tracing::warn!(
                path = %resolved.display(),
                error = %e,
                "grep failed to search directly-named file"
            );
            return Err(ToolExecError(format!(
                "failed to search '{}': {e}",
                resolved.display()
            )));
        }
        // The search completed, so the file's output is authoritative
        // (abort_file would have been set by binary_data if the file was cut
        // short, and the error path above returns before this point).
        sink.end_file();
        return Ok(finish_grep(sink, pattern, regex));
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
            let search = searcher.search_path(&matcher, entry.path(), &mut sink);
            if let Err(e) = &search {
                tracing::debug!(
                    path = %entry.path().display(),
                    error = %e,
                    "grep search error on file, skipping"
                );
                // A failed read must not flush partial output for this file —
                // the searcher may have stopped mid-file, so neither the
                // count-mode tally nor the content-mode bucket collected so
                // far is the file's true result. (`binary_data` sets the same
                // abort flag internally when a NUL byte cuts the file short.)
                sink.abort_file();
            }
            sink.end_file();

            // Stop early once we've accumulated enough results (max_results
            // cap or byte budget).
            if sink.should_stop() {
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

    Ok(finish_grep(sink, pattern, regex))
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
        // Context is only ever applied in Content mode — the other modes ignore
        // it entirely — so advertising it there would mislead the model.
        if args.context > 0 && args.output_mode == GrepOutputMode::Content {
            // Report the value the searcher will actually apply — out-of-range
            // requests are clamped to MAX_CONTEXT_LINES, so advertising e.g.
            // "1000 context line(s)" while 100 are shown would mislead the
            // model.
            let shown = args.context.min(MAX_CONTEXT_LINES);
            parts.push(format!(" Showing {shown} context line(s)."));
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
            // Report the value the tool will actually apply — out-of-range
            // requests are clamped to MAX_RESULTS_CAP at execution, so
            // advertising e.g. "5000" while 200 are returned would mislead
            // the model.
            let shown = max.clamp(1, MAX_RESULTS_CAP);
            parts.push(format!(" Max results: {shown}."));
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
    fn test_crlf_content_strips_carriage_return() {
        // grep-searcher hands back the line *including* its terminator; for a
        // CRLF file that is `\r\n`. The `\r` must be stripped (and any
        // mid-line CR escaped) so the line-oriented output stays clean.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("crlf.txt"), b"hello world\r\nfoo\r\n").expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("crlf.txt:1:hello world"),
            "expected match without CR:\n{result:?}"
        );
        assert!(
            !result.contains('\r'),
            "carriage return must not leak:\n{result:?}"
        );
    }

    #[test]
    fn test_content_control_chars_escaped() {
        // An embedded ESC simulates a terminal-escape injection attempt; the
        // matched content must render it as inert ASCII instead of passing the
        // raw byte through to the TUI.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("esc.txt"), b"hello \x1b[31mred\x1b[0m\n").expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        // escape_default renders ESC as the literal ASCII `\u{1b}`.
        assert!(
            result.contains("\\u{1b}"),
            "ESC must be escaped:\n{result:?}"
        );
        assert!(
            !result.contains('\x1b'),
            "raw ESC must not pass through:\n{result:?}"
        );
    }

    #[test]
    fn test_content_unicode_line_separator_escaped() {
        // U+2028 (LINE SEPARATOR) is not `is_control` (category Zl), but
        // terminals render it as an actual line break — a hostile file could
        // use it to split the line-oriented output. It must render as inert
        // ASCII instead of passing through raw.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("ls.txt"), "hello\u{2028}world\n").expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("\\u{2028}"),
            "U+2028 must be escaped:\n{result:?}"
        );
        assert!(
            !result.contains('\u{2028}'),
            "raw U+2028 must not pass through:\n{result:?}"
        );
    }

    #[test]
    fn test_line_terminator_strip_preserves_embedded_cr() {
        // A line whose data legitimately ends in `\r` before a CRLF terminator
        // keeps that `\r` (escaped) instead of having it trimmed together with
        // the terminator — only the single terminator sequence is stripped.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("cr.txt"), b"hello world\r\r\nfoo\n").expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("cr.txt:1:hello world\\r"),
            "embedded CR must be preserved (escaped), got:\n{result:?}"
        );
    }

    #[test]
    fn test_content_tabs_preserved() {
        // Tabs are legitimate code content and harmless in terminal output;
        // escaping them would mangle every tab-indented source line.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("tabbed.txt"), "hello\tworld\n").expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(
            result.contains("hello\tworld"),
            "tabs are legitimate content:\n{result:?}"
        );
    }

    #[test]
    fn binary_file_is_not_searched() {
        // The tool documents that it respects binary files — a file whose
        // head contains a NUL byte must not produce garbage matches. With the
        // NUL as the very first byte the searcher treats the file as binary
        // before any line is delivered.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("bin.dat"), b"\0hello\n").expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "No matches found.", "{result}");
    }

    #[test]
    fn count_mode_discards_partial_tally_on_binary_file() {
        // A file with a NUL byte mid-way is truncated by binary detection
        // before the search completes. The count of matching lines observed
        // before the NUL is not the file's true count, so it must be discarded
        // — the file is "skipped", never counted.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("bin.txt"), b"hello\nhello\n\0hello\n").expect("write");

        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.output_mode = GrepOutputMode::Count;
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "No matches found.", "{result}");
    }

    #[test]
    fn content_mode_discards_binary_partial_matches() {
        // A file with a NUL byte mid-way is truncated by binary detection
        // before the search completes. Matches observed before the NUL are
        // not the file's true content — a binary file must render as skipped
        // in *every* output mode (the tool documents that it respects binary
        // files), not leak pre-NUL text matches.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("bin.txt"), b"hello\nhello\n\0hello\n").expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "No matches found.", "{result}");
    }

    #[test]
    fn binary_file_skipped_among_text_files() {
        // A binary file between two text files must be skipped entirely in
        // content mode — its pre-NUL matches must not render, and the text
        // files' matches must survive.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("a.txt"), "hello one\n").expect("write");
        std::fs::write(dir.path().join("bin.txt"), b"hello\n\0\n").expect("write");
        std::fs::write(dir.path().join("c.txt"), "hello three\n").expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        assert!(result.contains("a.txt:1:hello one"), "{result}");
        assert!(result.contains("c.txt:1:hello three"), "{result}");
        assert!(
            !result.contains("bin.txt"),
            "binary file must not appear:\n{result}"
        );
    }

    #[test]
    fn drop_current_bucket_refunds_exact_charged_bytes() {
        // An aborted file's bucket is removed wholesale: the charged bytes
        // and the match tally must be refunded so `result_count` and the
        // byte budget stay consistent with what will actually render.
        let mut sink = GrepSink::new(GrepOutputMode::Content, 10, 0, Path::new("root"), false);
        sink.begin_file(Path::new("a.txt"));
        sink.push_item(GrepItem::Match {
            line_number: 1,
            content: "x".into(),
        });
        // push_item does not tally matches — the `matched` handler does.
        sink.content_match_count += 1;
        sink.push_item(GrepItem::Context {
            line_number: 2,
            content: "y".into(),
        });
        assert!(sink.content_bytes > 0, "items must charge bytes");
        assert_eq!(sink.result_count(), 1);

        sink.abort_file();
        sink.end_file();
        assert!(sink.content_files.is_empty(), "bucket must be dropped");
        assert_eq!(sink.content_bytes, 0, "charged bytes must be refunded");
        assert_eq!(sink.result_count(), 0, "match tally must be refunded");
    }

    #[test]
    fn byte_budget_charges_exact_rendered_size() {
        // `push_item` charges `render_len` + a joining newline for every item
        // except the first (which `render_content`'s join emits with no
        // preceding newline). The charged total must equal the bytes
        // `render_content` actually produces, so the guard and the renderer
        // agree on the threshold.
        let mut sink = GrepSink::new(GrepOutputMode::Content, 10, 0, Path::new("root"), false);
        sink.begin_file(Path::new("a.txt"));
        sink.push_item(GrepItem::Match {
            line_number: 1,
            content: "x".into(),
        });
        sink.push_item(GrepItem::Context {
            line_number: 2,
            content: "y".into(),
        });
        sink.push_item(GrepItem::Break);
        sink.push_item(GrepItem::Match {
            line_number: 4,
            content: "z".into(),
        });

        let rendered = render_content(&sink);
        assert_eq!(
            sink.content_bytes,
            rendered.len(),
            "charged bytes must match rendered output"
        );
    }

    #[test]
    fn decimal_len_counts_digits_without_allocation() {
        assert_eq!(decimal_len(0), 1);
        assert_eq!(decimal_len(1), 1);
        assert_eq!(decimal_len(9), 1);
        assert_eq!(decimal_len(10), 2);
        assert_eq!(decimal_len(99), 2);
        assert_eq!(decimal_len(100), 3);
        assert_eq!(decimal_len(u64::MAX), 20);
    }

    #[test]
    fn test_over_cap_line_truncated_with_marker() {
        // A 256 KiB one-liner is far past the 64 KiB line display cap. The
        // match must render capped with the shared marker instead of being
        // fully buffered into the result (and, before the cap existed,
        // duplicated by from_utf8_lossy + sanitize_content).
        let dir = TempDir::new().expect("temp dir");
        let mut big = String::with_capacity(300_000);
        big.push_str("hello");
        big.push_str(&"a".repeat(256 * 1024));
        big.push('\n');
        std::fs::write(dir.path().join("big.txt"), big).expect("write");

        let tool = Grep;
        let args = test_args("hello", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        let line = result.lines().next().expect("one match line");
        assert!(line.starts_with("big.txt:1:hello"), "{result}");
        assert!(
            line.contains("...[line truncated: exceeds 64 KiB]"),
            "over-cap line must carry the truncation marker:\n{result}"
        );
        // Label + capped content + marker stay well under the byte budget;
        // the raw line would have been ~256 KiB.
        assert!(
            line.len() < 100 * 1024,
            "capped line unexpectedly large: {} bytes",
            line.len()
        );
        assert_eq!(result.lines().count(), 1, "{result}");
    }

    #[test]
    fn content_mode_stops_collecting_past_byte_budget() {
        // Eight 20 KiB matching lines total ~160 KiB of buffered content —
        // far past the 128 KiB output budget the renderer keeps anyway. The
        // sink must stop collecting once the budget is exceeded (instead of
        // buffering the whole tree), and the truncation marker must report
        // the count actually collected, not the requested cap.
        let dir = TempDir::new().expect("temp dir");
        for i in 0..8 {
            let mut content = String::with_capacity(20 * 1024 + 16);
            content.push_str(&format!("file{i} "));
            content.push_str(&"a".repeat(20 * 1024));
            content.push('\n');
            std::fs::write(dir.path().join(format!("f{i}.txt")), content).expect("write");
        }

        let tool = Grep;
        let args = test_args("file", Some(dir.path()));
        let result = tool.execute(args, None, None, None).unwrap();
        // Six matches ≈ 123 KiB fit under the budget; a seventh would push
        // it past. The raw tree is ~160 KiB, so a bounded result proves
        // collection stopped early.
        assert!(
            result.len() < 128 * 1024,
            "result exceeded the byte budget: {} bytes",
            result.len()
        );
        assert!(
            result.contains("...[truncated at 6 matches]"),
            "expected honest byte-budget marker:\n{result}"
        );
        assert!(
            !result.contains("...[truncated at 200 matches]"),
            "the byte budget, not max_results, stopped the walk:\n{result}"
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
        // two before-context lines + the match render, then the marker. (The
        // file ends at the match, so there is no after-context to drain.)
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
    fn capped_match_includes_after_context() {
        // The max_results cap stops *matching*, but the capped match's trailing
        // context lines must still be delivered (ripgrep -m + -C semantics).
        // Without the after-context drain, lines 4-5 would be silently dropped.
        let dir = TempDir::new().expect("temp dir");
        {
            let mut f = std::fs::File::create(dir.path().join("test.txt")).expect("create");
            writeln!(f, "a").expect("write");
            writeln!(f, "b").expect("write");
            writeln!(f, "hello").expect("write");
            writeln!(f, "c").expect("write");
            writeln!(f, "d").expect("write");
            writeln!(f, "e").expect("write");
        }

        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.context = 2;
        args.max_results = Some(1);
        let result = tool.execute(args, None, None, None).unwrap();
        // 2 before-context + the capped match + its 2 after-context lines +
        // the marker. The third trailing line ("e") is past the window and
        // must not appear.
        assert_eq!(
            result.lines().count(),
            6,
            "2 before + match + 2 after + marker:\n{result}"
        );
        assert!(result.contains("test.txt:3:hello"), "{result}");
        assert!(result.contains("test.txt-4-c"), "{result}");
        assert!(result.contains("test.txt-5-d"), "{result}");
        assert!(
            !result.contains("test.txt-6-e"),
            "drain must stop at the after-context window:\n{result}"
        );
        assert!(
            result.contains("...[truncated at 1 matches]"),
            "expected truncation marker:\n{result}"
        );
    }

    #[test]
    fn capped_match_context_window_includes_within_window_match() {
        // A second match inside the capped match's after-context window (line 5
        // is 2 lines after line 3) must still be shown — as a context line,
        // not a second match (rg -m + -C semantics). Without the drain's own
        // counter, the searcher would deliver line 5 via `matched`, which the
        // done guard would reject, silently dropping it.
        let dir = TempDir::new().expect("temp dir");
        {
            let mut f = std::fs::File::create(dir.path().join("test.txt")).expect("create");
            writeln!(f, "a").expect("write");
            writeln!(f, "b").expect("write");
            writeln!(f, "hello").expect("write");
            writeln!(f, "c").expect("write");
            writeln!(f, "hello").expect("write");
            writeln!(f, "d").expect("write");
        }

        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.context = 2;
        args.max_results = Some(1);
        let result = tool.execute(args, None, None, None).unwrap();
        // 2 before-context + capped match + 2 within-window lines (4 as
        // context, 5 as the second "hello" rendered as context) + marker.
        // Line 6 is past the window and must not appear.
        assert_eq!(
            result.lines().count(),
            6,
            "2 before + match + 2 after (incl. within-window match) + marker:\n{result}"
        );
        assert!(result.contains("test.txt:3:hello"), "{result}");
        assert!(result.contains("test.txt-4-c"), "{result}");
        assert!(
            result.contains("test.txt-5-hello"),
            "within-window match must render as a context line:\n{result}"
        );
        assert!(
            !result.contains("test.txt:5:hello"),
            "within-window match must not render as a second match:\n{result}"
        );
        assert!(
            !result.contains("test.txt-6-d"),
            "drain must stop at the after-context window:\n{result}"
        );
        assert!(
            result.contains("...[truncated at 1 matches]"),
            "expected truncation marker:\n{result}"
        );
    }

    #[test]
    fn capped_match_drain_stops_at_truncated_context_line() {
        // A line over the 64 KiB display cap directly after the capped match
        // means filling the after-context window would force the searcher to
        // scan the rest of the file one giant line at a time (the line buffer
        // grows to hold each line whole). The drain must deliver the over-cap
        // line (capped + marker) and then stop, rather than scanning on for
        // the remaining window lines.
        let dir = TempDir::new().expect("temp dir");
        {
            let mut f = std::fs::File::create(dir.path().join("test.txt")).expect("create");
            writeln!(f, "hello").expect("write");
            writeln!(f, "{}", "x".repeat(300 * 1024)).expect("write");
            writeln!(f, "c").expect("write");
            writeln!(f, "d").expect("write");
        }

        let tool = Grep;
        let mut args = test_args("hello", Some(dir.path()));
        args.context = 2;
        args.max_results = Some(1);
        let result = tool.execute(args, None, None, None).unwrap();
        // Match + the over-cap context line (truncated) + marker. The lines
        // after the pathological line must NOT appear — the drain stops at it.
        assert!(result.contains("test.txt:1:hello"), "{result}");
        assert!(
            result.contains("test.txt-2-xxxx"),
            "over-cap context line should be delivered (capped):\n{result}"
        );
        assert!(
            result.contains("...[line truncated: exceeds 64 KiB]"),
            "over-cap line must carry the truncation marker:\n{result}"
        );
        assert!(
            !result.contains("test.txt-3-c"),
            "drain must stop at the pathological line:\n{result}"
        );
        assert!(
            !result.contains("test.txt-4-d"),
            "drain must stop at the pathological line:\n{result}"
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
    fn test_single_file_files_mode_no_truncation_marker() {
        // A directly-named single file in the file-capped modes is provably
        // complete once that one file is searched — the cap (≥ 1) was
        // necessarily met, so claiming truncation would be misleading.
        let dir = setup_test_dir();
        let file_path = dir.path().join("data.txt");
        let tool = Grep;
        let mut args = test_args("world", Some(&file_path));
        args.output_mode = GrepOutputMode::FilesWithMatches;
        args.max_results = Some(1);
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "data.txt", "got:\n{result}");
    }

    #[test]
    fn test_single_file_count_mode_no_truncation_marker() {
        let dir = setup_test_dir();
        let file_path = dir.path().join("data.txt");
        let tool = Grep;
        let mut args = test_args("world", Some(&file_path));
        args.output_mode = GrepOutputMode::Count;
        args.max_results = Some(1);
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "data.txt: 2", "got:\n{result}");
    }

    #[test]
    fn count_mode_discards_partial_tally_on_failed_file() {
        // A search that errored mid-file must not report the partial tally as
        // the file's true count — the searcher may have stopped before seeing
        // every matching line.
        let mut sink = GrepSink::new(GrepOutputMode::Count, 10, 0, Path::new("root"), false);
        sink.begin_file(Path::new("a.txt"));
        sink.count_file_lines = 3; // matches observed before the read failed
        sink.abort_file();
        sink.end_file();
        assert!(
            sink.count_entries.is_empty(),
            "partial tally must be discarded"
        );
        assert!(sink.stop.is_none(), "no entry means no cap met");

        // A completed search flushes normally.
        sink.count_file_lines = 2;
        sink.end_file();
        assert_eq!(sink.count_entries.len(), 1);
        assert_eq!(sink.count_entries[0].0, PathBuf::from("a.txt"));
        assert_eq!(sink.count_entries[0].1, 2);
        // The tally is reset after each file, so a partial leak can't happen.
        assert_eq!(sink.count_file_lines, 0);
    }

    #[test]
    fn push_item_drops_stray_break_and_collapses_consecutive() {
        let mut sink = GrepSink::new(GrepOutputMode::Content, 10, 0, Path::new("root"), false);
        sink.begin_file(Path::new("a.txt"));
        // A Break with no open bucket has nothing to separate — dropped.
        sink.push_item(GrepItem::Break);
        sink.push_item(GrepItem::Match {
            line_number: 1,
            content: "m".into(),
        });
        sink.push_item(GrepItem::Break);
        // A second consecutive Break would render doubled `--` — collapsed.
        sink.push_item(GrepItem::Break);
        sink.push_item(GrepItem::Context {
            line_number: 2,
            content: "c".into(),
        });
        let items = &sink.content_files[0].1;
        assert_eq!(items.len(), 3, "stray + duplicated breaks must not render");
        assert!(matches!(items[0], GrepItem::Match { .. }));
        assert!(matches!(items[1], GrepItem::Break));
        assert!(matches!(items[2], GrepItem::Context { .. }));
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
        args.output_mode = GrepOutputMode::Count;
        args.include = Some("*.rs".into());
        args.max_results = Some(25);
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Searching for `fn \\w+`."));
        assert!(desc.contains("Using regex."));
        assert!(desc.contains("Ignoring case."));
        assert!(desc.contains("Output mode: count."));
        assert!(desc.contains("Include pattern: `*.rs`."));
        assert!(desc.contains("Max results: 25."));
    }

    #[test]
    fn describe_invocation_context_reported_only_in_content_mode() {
        // Context is only ever applied in Content mode — the non-content modes
        // ignore it entirely, so the invocation description must not claim it
        // is shown there (the model reads this text to predict the output).
        let tool = Grep;
        let mut args = test_args("foo", None);
        args.context = 3;
        // Content mode (default): the searcher applies context, so report it.
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Showing 3 context line(s)."), "{desc}");
        // Count / files modes: context is silently dropped — no "Showing" claim.
        args.output_mode = GrepOutputMode::Count;
        let desc = tool.describe_invocation(&args);
        assert!(!desc.contains("Showing"), "{desc}");
        args.output_mode = GrepOutputMode::FilesWithMatches;
        let desc = tool.describe_invocation(&args);
        assert!(!desc.contains("Showing"), "{desc}");
    }

    #[test]
    fn describe_invocation_clamps_context() {
        // Out-of-range context requests are clamped to MAX_CONTEXT_LINES at
        // execution; the invocation description must report the value the
        // searcher will actually apply, not the raw request.
        let tool = Grep;
        let mut args = test_args("foo", None);
        args.context = 500;
        let desc = tool.describe_invocation(&args);
        assert!(
            desc.contains("Showing 100 context line(s)."),
            "clamped context should be reported: {desc}"
        );
        assert!(
            !desc.contains("500"),
            "unclamped context must not be advertised: {desc}"
        );
    }

    #[test]
    fn describe_invocation_clamps_max_results() {
        // Out-of-range max_results requests are clamped to MAX_RESULTS_CAP at
        // execution; the invocation description must report the value the
        // tool will actually apply, not the raw request.
        let tool = Grep;
        let mut args = test_args("foo", None);
        args.max_results = Some(5000);
        let desc = tool.describe_invocation(&args);
        assert!(
            desc.contains("Max results: 200."),
            "clamped max_results should be reported: {desc}"
        );
        assert!(
            !desc.contains("5000"),
            "unclamped max_results must not be advertised: {desc}"
        );
        // In-range requests pass through unchanged.
        args.max_results = Some(42);
        assert!(tool.describe_invocation(&args).contains("Max results: 42."));
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
    fn include_filtered_single_file_reports_plain_no_match() {
        // When the include glob excludes the explicitly-named file, no search
        // runs at all — the regex hint would misattribute the empty result to
        // the pattern, so only the plain no-match message may appear.
        let dir = setup_test_dir();
        let file_path = dir.path().join("test1.rs");
        let tool = Grep;
        let mut args = test_args("foo|bar", Some(&file_path));
        args.include = Some("*.py".to_string());
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "No matches found.", "{result}");
    }

    #[test]
    fn include_filtered_all_files_suppresses_regex_hint() {
        // A directory sweep where the include glob filters out *every* file
        // searches nothing — the regex hint would misattribute the empty
        // result to the pattern (same rationale as the single-file path
        // above), so the plain no-match message must appear.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("a.txt"), "hello\n").expect("write");

        let tool = Grep;
        let mut args = test_args("foo|bar", Some(dir.path()));
        args.include = Some("*.rs".to_string()); // excludes the only file
        let result = tool.execute(args, None, None, None).unwrap();
        assert_eq!(result, "No matches found.", "{result}");
    }

    #[cfg(unix)]
    #[test]
    fn single_file_read_error_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        // A directly-named file the searcher cannot read must surface as an
        // error, not a misleading "No matches found." — the caller asked
        // about this exact file, so the failure is meaningful.
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("locked.txt");
        std::fs::write(&file, "hello\n").expect("write");
        let mut perms = std::fs::metadata(&file).expect("metadata").permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&file, perms).expect("chmod");

        // Running as root bypasses permission bits — nothing to test there.
        if std::fs::File::open(&file).is_ok() {
            return;
        }

        let tool = Grep;
        let args = test_args("hello", Some(&file));
        let err = tool.execute(args, None, None, None).unwrap_err();
        assert!(
            err.to_string().contains("failed to search"),
            "direct-file read failure must surface as an error: {err}"
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
