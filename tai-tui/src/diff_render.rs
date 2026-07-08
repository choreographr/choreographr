use std::sync::Arc;

use ratatui::style::Style;
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::parsing::SyntaxReference;
use tai_client_core::{DiffHunk, DiffLine, DiffLineKind, FileDiff};

use crate::cache::GlobalLruCache;
use crate::syntax::{highlight_theme, syntax_for_path, syntax_set, to_ratatui_color};

/// Check if text looks like a unified diff.
/// Scans the full text (tool output may have a metadata prefix on the first line).
pub fn is_diff_text(text: &str) -> bool {
    text.contains("diff --git ") || text.contains("\n--- ") || text.starts_with("--- ")
}

/// Parser state that accumulates file and hunk boundaries as diff lines are
/// fed in. Wrapping the mutable fields in a struct avoids passing 7 separate
/// `&mut` references to the flush helpers.
struct DiffParserState {
    old_path: String,
    new_path: String,
    hunks: Vec<DiffHunk>,
    current_hunk_lines: Vec<DiffLine>,
    current_hunk_header: String,
    in_hunk: bool,
}

impl DiffParserState {
    fn new() -> Self {
        Self {
            old_path: String::new(),
            new_path: String::new(),
            hunks: Vec::new(),
            current_hunk_lines: Vec::new(),
            current_hunk_header: String::new(),
            in_hunk: false,
        }
    }

    fn flush_hunk(&mut self) {
        if self.in_hunk {
            self.hunks.push(DiffHunk {
                header: std::mem::take(&mut self.current_hunk_header),
                lines: std::mem::take(&mut self.current_hunk_lines),
            });
            self.in_hunk = false;
        }
    }

    fn flush_file(&mut self, files: &mut Vec<FileDiff>) {
        self.flush_hunk();
        if !self.old_path.is_empty() || !self.new_path.is_empty() || !self.hunks.is_empty() {
            files.push(FileDiff {
                old_path: std::mem::take(&mut self.old_path),
                new_path: std::mem::take(&mut self.new_path),
                hunks: std::mem::take(&mut self.hunks),
            });
        }
    }
}

/// Parse unified diff text into structured `FileDiff`s.
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut state = DiffParserState::new();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            state.flush_file(&mut files);
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            state.old_path = parts
                .first()
                .and_then(|p| p.strip_prefix("a/"))
                .unwrap_or("")
                .to_string();
            state.new_path = parts
                .get(1)
                .and_then(|p| p.strip_prefix("b/"))
                .unwrap_or("")
                .to_string();
        } else if let Some(rest) = line.strip_prefix("--- ") {
            if state.old_path.is_empty() {
                state.old_path = rest.strip_prefix("a/").unwrap_or(rest).to_string();
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if state.new_path.is_empty() {
                state.new_path = rest.strip_prefix("b/").unwrap_or(rest).to_string();
            }
        } else if line.starts_with("@@") {
            state.flush_hunk();
            state.current_hunk_header = line.to_string();
            state.in_hunk = true;
        } else if state.in_hunk {
            if let Some(content) = line.strip_prefix('+') {
                state.current_hunk_lines.push(DiffLine {
                    kind: DiffLineKind::Addition,
                    content: content.to_string(),
                });
            } else if let Some(content) = line.strip_prefix('-') {
                state.current_hunk_lines.push(DiffLine {
                    kind: DiffLineKind::Deletion,
                    content: content.to_string(),
                });
            } else if let Some(content) = line.strip_prefix(' ') {
                state.current_hunk_lines.push(DiffLine {
                    kind: DiffLineKind::Context,
                    content: content.to_string(),
                });
            } else if line == "\\ No newline at end of file" {
                // Skip this marker for simplicity
            } else {
                // Not a valid hunk line, stop hunk
                state.flush_hunk();
            }
        }
    }
    state.flush_file(&mut files);
    files
}

/// Number of display rows a diff takes up in side-by-side mode.
/// Computes the height directly from the diff structure without building
/// the full pane rows (which would trigger expensive syntax highlighting).
pub fn diff_display_height(diffs: &[FileDiff]) -> usize {
    if diffs.is_empty() {
        return 1;
    }
    diffs
        .iter()
        .map(|file| {
            // 1 for the file header row, 1 per hunk header, plus all hunk lines
            1usize + file.hunks.iter().map(|h| 1 + h.lines.len()).sum::<usize>()
        })
        .sum::<usize>()
        .max(1)
}

/// Result of building aligned left/right panes for a single hunk.
pub struct DiffPaneRow {
    pub left_content: String,
    pub right_content: String,
    pub left_kind: DiffLineKind,
    pub right_kind: DiffLineKind,
    /// Syntax-highlighted spans for the left pane, populated by `highlight_diff_panes`.
    pub left_spans: Vec<Span<'static>>,
    /// Syntax-highlighted spans for the right pane, populated by `highlight_diff_panes`.
    pub right_spans: Vec<Span<'static>>,
}

impl DiffPaneRow {
    fn new(
        left_content: String,
        right_content: String,
        left_kind: DiffLineKind,
        right_kind: DiffLineKind,
    ) -> Self {
        Self {
            left_content,
            right_content,
            left_kind,
            right_kind,
            left_spans: Vec::new(),
            right_spans: Vec::new(),
        }
    }
}

/// Create a single default-styled span from plain text content.
fn content_spans(content: &str) -> Vec<Span<'static>> {
    vec![Span::styled(content.to_string(), Style::default())]
}

/// True if the row content looks like diff metadata (file header or hunk header)
/// rather than actual source code. Syntax highlighting is skipped for these rows.
fn is_meta_line(content: &str) -> bool {
    content.starts_with("--- ") || content.starts_with("+++ ") || content.starts_with("@@")
}

/// Highlight lines through syntect, returning per-line vectors of styled spans.
///
/// Results are memoized in an LRU-cached global map so re-rendering the same
/// diff on the next frame does not re-run syntect.  The result is wrapped in
/// `Arc` so that cache hits are an O(1) refcount bump rather than a full
/// clone of all highlighted spans.
///
/// Takes a pre-resolved `SyntaxReference` (callers use `syntax_for_path`)
/// and a slice of individual lines, avoiding the join-then-split cycle of
/// passing a single joined string.
fn highlight_lines_cached(
    syntax: &SyntaxReference,
    lines: &[&str],
) -> Arc<Vec<Vec<Span<'static>>>> {
    // Memoize highlighted results so re-rendering the same diff on the
    // next frame does not re-run syntect.
    static CACHE: GlobalLruCache<(String, String), Arc<Vec<Vec<Span<'static>>>>, 200> =
        GlobalLruCache::new();

    // Build a cache key from the syntax name and the joined lines.
    let code = lines.join("\n");
    let key = (syntax.name.to_string(), code);

    CACHE.get_or_insert_with(&key, || {
        let ss = syntax_set();
        let theme = highlight_theme();

        let mut highlighter = HighlightLines::new(syntax, theme);
        let mut result: Vec<Vec<Span<'static>>> = Vec::with_capacity(lines.len());

        for &line_str in lines {
            if let Ok(ranges) = highlighter.highlight_line(line_str, ss) {
                result.push(
                    ranges
                        .into_iter()
                        .map(|(style, text)| {
                            Span::styled(
                                text.to_string(),
                                Style::default().fg(to_ratatui_color(style.foreground)),
                            )
                        })
                        .collect(),
                );
            } else {
                result.push(vec![Span::styled(line_str.to_string(), Style::default())]);
            }
        }

        Arc::new(result)
    })
}

/// Apply syntax highlighting to all code rows in a diff file's pane rows.
///
/// Implements the "two-bucket" approach used by opencode's @pierre/diffs:
/// all lines on the left side (deletions + context) are processed in one
/// pseudo-file and highlighted as a whole; similarly for lines on the right
/// side (additions + context). This gives syntect the sequential context it
/// needs for accurate tokenization across adjacent lines. The highlighted
/// per-line spans are then mapped back onto the corresponding `DiffPaneRow`.
///
/// Rows that are diff metadata (file headers, hunk headers) are left as plain
/// text.
pub fn highlight_diff_panes(rows: &mut [DiffPaneRow], file: &FileDiff) {
    let Some(syntax) = syntax_for_path(&file.new_path).or_else(|| syntax_for_path(&file.old_path))
    else {
        return;
    };

    // --- Left bucket: deletions + context (non-meta lines with left content) ---
    let mut left_rows: Vec<usize> = Vec::new();
    let mut left_lines: Vec<&str> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if !row.left_content.is_empty() && !is_meta_line(&row.left_content) {
            left_rows.push(i);
            left_lines.push(&row.left_content);
        }
    }
    if !left_lines.is_empty() {
        let highlighted = highlight_lines_cached(syntax, &left_lines);
        for (idx, line_spans) in left_rows.iter().zip(highlighted.iter()) {
            rows[*idx].left_spans = line_spans.clone();
        }
    }

    // --- Right bucket: additions + context (non-meta lines with right content) ---
    let mut right_rows: Vec<usize> = Vec::new();
    let mut right_lines: Vec<&str> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if !row.right_content.is_empty() && !is_meta_line(&row.right_content) {
            right_rows.push(i);
            right_lines.push(&row.right_content);
        }
    }
    if !right_lines.is_empty() {
        let highlighted = highlight_lines_cached(syntax, &right_lines);
        for (idx, line_spans) in right_rows.iter().zip(highlighted.iter()) {
            rows[*idx].right_spans = line_spans.clone();
        }
    }
}

/// Build aligned left/right pane rows from parsed diffs.
/// Each entry in the returned vec is one row in the side-by-side display.
/// After building, syntax highlighting is applied to code rows via the
/// two-bucket algorithm (see `highlight_diff_panes`).
pub fn build_diff_panes(diffs: &[FileDiff]) -> Vec<DiffPaneRow> {
    let mut rows = Vec::new();
    for file in diffs {
        // Record the starting index within this file's rows so we can pass
        // just the right slice to highlight_diff_panes.
        let file_start = rows.len();

        // File header rows (rendered full-width, not in panes, but we include empty rows for spacing)
        rows.push(DiffPaneRow::new(
            format!("--- a/{}", file.old_path),
            format!("+++ b/{}", file.new_path),
            DiffLineKind::Context,
            DiffLineKind::Context,
        ));
        for hunk in &file.hunks {
            // Hunk header row
            rows.push(DiffPaneRow::new(
                hunk.header.clone(),
                hunk.header.clone(),
                DiffLineKind::Context,
                DiffLineKind::Context,
            ));
            for line in &hunk.lines {
                match line.kind {
                    DiffLineKind::Context => {
                        rows.push(DiffPaneRow::new(
                            line.content.clone(),
                            line.content.clone(),
                            DiffLineKind::Context,
                            DiffLineKind::Context,
                        ));
                    }
                    DiffLineKind::Deletion => {
                        rows.push(DiffPaneRow::new(
                            line.content.clone(),
                            String::new(),
                            DiffLineKind::Deletion,
                            DiffLineKind::Context,
                        ));
                    }
                    DiffLineKind::Addition => {
                        rows.push(DiffPaneRow::new(
                            String::new(),
                            line.content.clone(),
                            DiffLineKind::Context,
                            DiffLineKind::Addition,
                        ));
                    }
                }
            }
        }

        // Apply syntax highlighting to the rows that belong to this file.
        highlight_diff_panes(&mut rows[file_start..], file);
    }

    // Fill any empty left/right spans with default plain-text spans so that
    // consumers always have at least one span per side.
    for row in &mut rows {
        if row.left_spans.is_empty() {
            row.left_spans = content_spans(&row.left_content);
        }
        if row.right_spans.is_empty() {
            row.right_spans = content_spans(&row.right_content);
        }
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    // ── is_diff_text ──

    #[test]
    fn detects_diff_git_header() {
        assert!(is_diff_text("diff --git a/x b/x"));
    }

    #[test]
    fn detects_diff_git_header_embedded() {
        assert!(is_diff_text("prefix\ndiff --git a/x b/x"));
    }

    #[test]
    fn detects_three_dash_line() {
        assert!(is_diff_text("--- a/file.txt"));
        assert!(is_diff_text("\n--- a/file.txt"));
    }

    #[test]
    fn rejects_plain_text() {
        assert!(!is_diff_text("hello world"));
        assert!(!is_diff_text("just some text"));
    }

    #[test]
    fn rejects_empty_input() {
        assert!(!is_diff_text(""));
    }

    // ── parse_diff ──

    fn simple_diff_text() -> &'static str {
        "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n"
    }

    #[test]
    fn parse_single_file() {
        let files = parse_diff(simple_diff_text());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path, "file.txt");
        assert_eq!(files[0].new_path, "file.txt");
    }

    #[test]
    fn parse_hunk_content() {
        let files = parse_diff(simple_diff_text());
        assert_eq!(files[0].hunks.len(), 1);
        let hunk = &files[0].hunks[0];
        assert_eq!(hunk.header, "@@ -1 +1 @@");
        assert_eq!(hunk.lines.len(), 2);
        assert_eq!(hunk.lines[0].kind, DiffLineKind::Deletion);
        assert_eq!(hunk.lines[0].content, "old");
        assert_eq!(hunk.lines[1].kind, DiffLineKind::Addition);
        assert_eq!(hunk.lines[1].content, "new");
    }

    #[test]
    fn parse_multiple_files() {
        let text = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n\
                     diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse_diff(text);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].old_path, "a.txt");
        assert_eq!(files[1].old_path, "b.txt");
    }

    #[test]
    fn parse_skips_no_newline_marker() {
        let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n";
        let files = parse_diff(text);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!(files[0].hunks[0].lines[0].content, "old");
        assert_eq!(files[0].hunks[0].lines[1].content, "new");
    }

    #[test]
    fn parse_empty_diff_returns_empty_vec() {
        let files = parse_diff("");
        assert!(files.is_empty());
    }

    // ── diff_display_height ──

    #[test]
    fn display_height_matches_pane_count() {
        let diffs = parse_diff(simple_diff_text());
        assert_eq!(
            diff_display_height(&diffs),
            build_diff_panes(&diffs).len().max(1)
        );
    }

    #[test]
    fn display_height_at_least_one() {
        assert_eq!(diff_display_height(&[]), 1);
    }

    // ── build_diff_panes ──

    #[test]
    fn pane_file_header_row() {
        let diffs = parse_diff(simple_diff_text());
        let rows = build_diff_panes(&diffs);
        assert!(rows[0].left_content.contains("--- a/file.txt"));
        assert!(rows[0].right_content.contains("+++ b/file.txt"));
    }

    #[test]
    fn pane_deletion_appears_on_left() {
        let diffs = parse_diff(simple_diff_text());
        let rows = build_diff_panes(&diffs);
        let deletion = rows
            .iter()
            .find(|r| r.left_kind == DiffLineKind::Deletion)
            .unwrap();
        assert_eq!(deletion.left_content, "old");
        assert!(deletion.right_content.is_empty());
    }

    #[test]
    fn pane_addition_appears_on_right() {
        let diffs = parse_diff(simple_diff_text());
        let rows = build_diff_panes(&diffs);
        let addition = rows
            .iter()
            .find(|r| r.right_kind == DiffLineKind::Addition)
            .unwrap();
        assert_eq!(addition.right_content, "new");
        assert!(addition.left_content.is_empty());
    }

    #[test]
    fn pane_context_appears_on_both_sides() {
        let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n ctx\n";
        let diffs = parse_diff(text);
        let rows = build_diff_panes(&diffs);
        // The leading space is stripped by parse_diff; content is "ctx" without the prefix
        let ctx = rows
            .iter()
            .find(|r| r.left_kind == DiffLineKind::Context && r.left_content == "ctx")
            .unwrap();
        assert_eq!(ctx.left_content, "ctx");
        assert_eq!(ctx.right_content, "ctx");
    }

    // ── content_spans ──

    #[test]
    fn content_spans_creates_single_default_span() {
        let spans = content_spans("hello");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "hello");
        assert_eq!(spans[0].style, Style::default());
    }

    #[test]
    fn content_spans_empty_string() {
        let spans = content_spans("");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "");
    }

    // ── highlight_diff_panes ──

    #[test]
    fn highlighting_applies_syntax_colors_to_rust_diffs() {
        // Diff of a .rs file with a recognizable keyword
        let text = "diff --git a/main.rs b/main.rs\n--- a/main.rs\n+++ b/main.rs\n@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n";
        let diffs = parse_diff(text);
        let rows = build_diff_panes(&diffs);

        // Build expects at least 4 rows: file header, hunk header, deletion, addition
        assert!(rows.len() >= 4);

        // Deletion line (index 2) should have syntax-colored spans
        let del = rows
            .iter()
            .find(|r| r.left_kind == DiffLineKind::Deletion)
            .unwrap();
        assert!(
            del.left_spans.len() > 1 || del.left_spans[0].style != Style::default(),
            "rust deletion should have multiple colored spans"
        );
        let has_colour = del
            .left_spans
            .iter()
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(has_colour, "rust deletion should have coloured spans");

        // Addition line (index 3) should also have syntax-colored spans
        let add = rows
            .iter()
            .find(|r| r.right_kind == DiffLineKind::Addition)
            .unwrap();
        let has_colour = add
            .right_spans
            .iter()
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(has_colour, "rust addition should have coloured spans");
    }

    #[test]
    fn highlighting_skips_unknown_extension() {
        let text = "diff --git a/file.xyz b/file.xyz\n--- a/file.xyz\n+++ b/file.xyz\n@@ -1 +1 @@\n-old\n+new\n";
        let diffs = parse_diff(text);
        let rows = build_diff_panes(&diffs);
        // highlight_diff_panes is called inside build_diff_panes; for unknown
        // extensions the spans should remain as single default-styled spans.
        let del = rows
            .iter()
            .find(|r| r.left_kind == DiffLineKind::Deletion)
            .unwrap();
        assert_eq!(del.left_spans.len(), 1);
        assert_eq!(del.left_spans[0].style, Style::default());
    }

    #[test]
    fn highlighting_handles_empty_hunks() {
        let text =
            "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -0,0 +1,1 @@\n+fn main() {}\n";
        let diffs = parse_diff(text);
        let rows = build_diff_panes(&diffs);
        let add = rows
            .iter()
            .find(|r| r.right_kind == DiffLineKind::Addition)
            .unwrap();
        let has_colour = add
            .right_spans
            .iter()
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(
            has_colour,
            "rust addition in new file should have coloured spans"
        );
    }

    #[test]
    fn highlighting_context_lines_get_coloured() {
        let text = "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n fn old() {}\n-foo\n+bar\n fn other() {}\n";
        let diffs = parse_diff(text);
        let rows = build_diff_panes(&diffs);
        // Context lines (fn old() {} and fn other() {}) should have syntax colours
        let ctx_lines: Vec<&DiffPaneRow> = rows
            .iter()
            .filter(|r| {
                r.left_kind == DiffLineKind::Context
                    && !r.left_content.is_empty()
                    && !r.left_content.starts_with("---")
                    && !r.left_content.starts_with("@@")
            })
            .collect();
        assert!(
            ctx_lines.len() >= 2,
            "should have at least 2 context code lines"
        );
        for ctx in &ctx_lines {
            let has_colour = ctx
                .left_spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
            assert!(
                has_colour,
                "context line '{}' should have coloured spans",
                ctx.left_content
            );
        }
    }

    // ── is_meta_line ──

    #[test]
    fn meta_line_detection() {
        assert!(is_meta_line("--- a/file.rs"));
        assert!(is_meta_line("+++ b/file.rs"));
        assert!(is_meta_line("@@ -1 +1 @@"));
        assert!(!is_meta_line("fn main() {}"));
        assert!(!is_meta_line(""));
    }
}
