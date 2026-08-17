use std::sync::Arc;

use choreo_client_core::{DiffHunk, DiffLine, DiffLineKind, FileDiff};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::parsing::SyntaxReference;
use tracing::debug;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cache::GlobalLruCache;
use crate::syntax::{highlight_theme, syntax_for_path, syntax_set, to_ratatui_color};

/// Minimum terminal width for side-by-side diff rendering.
/// Below this threshold, falls back to unified (inline) display.
const MIN_SIDEBYSIDE_WIDTH: u16 = 40;

/// Background colour for deleted lines (left pane).
const DEL_BG: Color = Color::Rgb(80, 0, 0);
/// Background colour for added lines (right pane).
const ADD_BG: Color = Color::Rgb(0, 80, 0);

// ── Detection ────────────────────────────────────────────────────────

/// Check if text looks like a unified diff.
///
/// Detects both raw unified diff headers and markdown-fenced diff blocks
/// (```` ```diff ```` / ```` ``` ````).  When looking for `diff --git` the match
/// must occur at a **line boundary** rather than anywhere in the string, so
/// that content inside other kinds of markdown code blocks (e.g. file contents
/// displayed by `write_file`) won't trigger a false positive.
///
/// This is now only ever consulted on the **interior** of a ` ```diff ` fence
/// already recognised by the markdown renderer (see
/// [`try_render_diff_content`]) — the raw-signal sniffs are opt-in gates for
/// fence interiors, never whole tool outputs.
pub fn is_diff_text(text: &str) -> bool {
    // Explicit fenced diff block
    if text.contains("\n```diff\n") || text.starts_with("```diff\n") {
        return true;
    }
    // Raw unified diff header at line start
    text.starts_with("diff --git ") || text.contains("\ndiff --git ")
    // `--- a/` / `--- /dev/` style path headers (part of unified diff)
    || text.starts_with("--- ") || text.contains("\n--- ")
}

// ── Parsing ──────────────────────────────────────────────────────────

#[derive(Default)]
struct DiffParserState {
    old_path: String,
    new_path: String,
    hunks: Vec<DiffHunk>,
    current_hunk_lines: Vec<DiffLine>,
    current_hunk_header: String,
    in_hunk: bool,
}

impl DiffParserState {
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
    let mut state = DiffParserState::default();

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
                // Skip this marker
            } else {
                state.flush_hunk();
            }
        }
    }
    state.flush_file(&mut files);
    files
}

// ── Pane rows ────────────────────────────────────────────────────────

/// Result of building aligned left/right panes for a single diff row.
pub struct DiffPaneRow {
    pub left_content: String,
    pub right_content: String,
    pub left_kind: DiffLineKind,
    pub right_kind: DiffLineKind,
    pub left_spans: Vec<Span<'static>>,
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

fn content_spans(content: &str) -> Vec<Span<'static>> {
    vec![Span::styled(content.to_string(), Style::default())]
}

fn is_meta_line(content: &str) -> bool {
    content.starts_with("--- ") || content.starts_with("+++ ") || content.starts_with("@@")
}

// ── Syntax highlighting ──────────────────────────────────────────────

fn highlight_lines_cached(
    syntax: &SyntaxReference,
    lines: &[&str],
) -> Arc<Vec<Vec<Span<'static>>>> {
    static CACHE: GlobalLruCache<(String, String), Arc<Vec<Vec<Span<'static>>>>, 200> =
        GlobalLruCache::new();

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

/// Highlight the content lines (non-meta, non-empty) in a single "bucket"
/// (left = deletions/context, right = additions/context).
fn highlight_bucket(
    rows: &mut [DiffPaneRow],
    syntax: &SyntaxReference,
    content: fn(&DiffPaneRow) -> &str,
    spans: fn(&mut DiffPaneRow) -> &mut Vec<Span<'static>>,
) {
    let mut indices: Vec<usize> = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let c = content(row);
        if !c.is_empty() && !is_meta_line(c) {
            indices.push(i);
            lines.push(c);
        }
    }
    if !lines.is_empty() {
        let highlighted = highlight_lines_cached(syntax, &lines);
        for (&idx, hl) in indices.iter().zip(highlighted.iter()) {
            *spans(&mut rows[idx]) = hl.clone();
        }
    }
}

pub fn highlight_diff_panes(rows: &mut [DiffPaneRow], file: &FileDiff) {
    let Some(syntax) = syntax_for_path(&file.new_path).or_else(|| syntax_for_path(&file.old_path))
    else {
        return;
    };

    highlight_bucket(rows, syntax, |r| &r.left_content, |r| &mut r.left_spans);
    highlight_bucket(rows, syntax, |r| &r.right_content, |r| &mut r.right_spans);
}

// ── Pane builder ─────────────────────────────────────────────────────

pub fn build_diff_panes(diffs: &[FileDiff]) -> Vec<DiffPaneRow> {
    let mut rows = Vec::new();
    for file in diffs {
        rows.push(DiffPaneRow::new(
            format!("--- a/{}", file.old_path),
            format!("+++ b/{}", file.new_path),
            DiffLineKind::Context,
            DiffLineKind::Context,
        ));
        for hunk in &file.hunks {
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
    }

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

// ── Rendering ────────────────────────────────────────────────────────

/// Ensure a collection of spans adds up to exactly `width` columns by
/// truncating content or padding with spaces.
fn spans_fixed_width(spans: &mut Vec<Span<'static>>, width: usize) {
    let total: usize = spans.iter().map(|s| s.width()).sum();
    if total == width {
        return;
    }
    if total > width {
        let mut remaining = width;
        let mut keep = 0usize;
        for i in 0..spans.len() {
            let w = spans[i].width();
            if w <= remaining {
                remaining -= w;
                keep = i + 1;
            } else {
                // This span is wider than the remaining space.  Truncate its
                // content to fit rather than dropping it entirely (which would
                // lose the text the user needs to see).
                spans.truncate(keep + 1);
                spans[keep] = Span::styled(
                    truncate_str(&spans[keep].content, remaining),
                    spans[keep].style,
                );
                remaining = 0;
                break;
            }
        }
        if remaining > 0 {
            spans.push(Span::styled(" ".repeat(remaining), Style::default()));
        }
        return;
    }
    spans.push(Span::styled(" ".repeat(width - total), Style::default()));
}

/// Render a parsed diff as side-by-side ratatui lines.
///
/// Each line is split into a left pane, a `│` gutter, and a right pane.
/// Panes are sized so the combined width equals the given `total_width`.
fn render_side_by_side(diffs: &[FileDiff], total_width: usize) -> Vec<Line<'static>> {
    let mut rows = build_diff_panes(diffs);

    // Syntax-highlight each file's rows (deferred here to avoid paying the
    // syntect cost when rendering in unified/narrow mode).
    let mut offset = 0;
    for file in diffs {
        let row_count = file.hunks.iter().map(|h| 1 + h.lines.len()).sum::<usize>() + 1;
        highlight_diff_panes(&mut rows[offset..offset + row_count], file);
        offset += row_count;
    }

    let gutter_width = 1usize;
    let left_w = (total_width.saturating_sub(gutter_width)) / 2;
    let right_w = total_width.saturating_sub(left_w + gutter_width);

    let mut out: Vec<Line<'static>> = Vec::with_capacity(rows.len());

    for row in &rows {
        let mut spans: Vec<Span<'static>> = Vec::new();

        // ── Left pane ─────────────────────────────────────────
        let left_is_meta = is_meta_line(&row.left_content);
        if row.left_content.is_empty() {
            spans.push(Span::styled(" ".repeat(left_w), Style::default()));
        } else if left_is_meta {
            let mut meta = vec![Span::styled(
                truncate_str(&row.left_content, left_w),
                Style::default().fg(Color::Yellow),
            )];
            spans_fixed_width(&mut meta, left_w);
            spans.extend(meta);
        } else {
            let mut left_spans = apply_bg(&row.left_spans, row.left_kind);
            spans_fixed_width(&mut left_spans, left_w);
            spans.extend(left_spans);
        }

        // ── Gutter ────────────────────────────────────────────
        spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));

        // ── Right pane ────────────────────────────────────────
        let right_is_meta = is_meta_line(&row.right_content);
        if row.right_content.is_empty() {
            spans.push(Span::styled(" ".repeat(right_w), Style::default()));
        } else if right_is_meta {
            let mut meta = vec![Span::styled(
                truncate_str(&row.right_content, right_w),
                Style::default().fg(Color::Yellow),
            )];
            spans_fixed_width(&mut meta, right_w);
            spans.extend(meta);
        } else {
            let mut right_spans = apply_bg(&row.right_spans, row.right_kind);
            spans_fixed_width(&mut right_spans, right_w);
            spans.extend(right_spans);
        }

        out.push(Line::from(spans));
    }

    out
}

/// Render a parsed diff as unified (inline) ratatui lines.
///
/// Each line has a 2-character prefix: `- ` for deletions, `+ ` for
/// additions, `  ` for context.
fn render_unified(diffs: &[FileDiff], total_width: usize) -> Vec<Line<'static>> {
    let prefix_w = 2usize;
    let content_w = total_width.saturating_sub(prefix_w);
    let rows = build_diff_panes(diffs);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(rows.len());

    for row in &rows {
        let left_is_meta = is_meta_line(&row.left_content);
        let right_is_meta = is_meta_line(&row.right_content);

        if left_is_meta && right_is_meta {
            // File header or hunk header — show once, full width
            let text = truncate_str(&row.left_content, total_width);
            let color = if row.left_content.starts_with("@@") {
                Color::Cyan
            } else {
                Color::Yellow
            };
            let mut spans = vec![Span::styled(text, Style::default().fg(color))];
            spans_fixed_width(&mut spans, total_width);
            out.push(Line::from(spans));
        } else if !row.left_content.is_empty() && row.right_content.is_empty() {
            // Deletion
            let text = truncate_str(&row.left_content, content_w);
            let mut spans = vec![
                Span::styled("- ", Style::default().fg(Color::Red)),
                Span::styled(text, Style::default().fg(Color::Red)),
            ];
            spans_fixed_width(&mut spans, total_width);
            out.push(Line::from(spans));
        } else if row.left_content.is_empty() && !row.right_content.is_empty() {
            // Addition
            let text = truncate_str(&row.right_content, content_w);
            let mut spans = vec![
                Span::styled("+ ", Style::default().fg(Color::Green)),
                Span::styled(text, Style::default().fg(Color::Green)),
            ];
            spans_fixed_width(&mut spans, total_width);
            out.push(Line::from(spans));
        } else if !row.left_content.is_empty() && !row.right_content.is_empty() {
            // Context
            let text = truncate_str(&row.left_content, content_w);
            let mut spans = vec![
                Span::styled("  ", Style::default()),
                Span::styled(text, Style::default()),
            ];
            spans_fixed_width(&mut spans, total_width);
            out.push(Line::from(spans));
        }
    }

    out
}

/// Parse a ` ```diff ` fence interior and render it as a diff.
///
/// The caller has already established opt-in: this is only ever invoked with
/// the *interior* of a markdown ` ```diff ` fence (see
/// `markdown_render::render_markdown_block`), which is in turn reachable only
/// through the markdown allowlist for tool results (`MARKDOWN_TOOLS`).
/// Returns `None` when the text is not recognised as a diff, allowing the
/// caller to fall through to its generic code-block rendering.
pub fn try_render_diff_content(diff_text: &str, width: u16) -> Option<Vec<Line<'static>>> {
    if !is_diff_text(diff_text) {
        return None;
    }
    let diffs = parse_diff(diff_text);
    if diffs.is_empty() {
        debug!("detected diff header but parse produced zero files — malformed?");
        return None;
    }
    let total = width as usize;
    let lines = if width >= MIN_SIDEBYSIDE_WIDTH {
        debug!(
            "rendering diff side-by-side ({} files, {} wide)",
            diffs.len(),
            total
        );
        render_side_by_side(&diffs, total)
    } else {
        debug!(
            "rendering diff unified ({} files, {} wide)",
            diffs.len(),
            total
        );
        render_unified(&diffs, total)
    };
    Some(lines)
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Truncate a string to at most `max_width` display columns, appending `…`
/// if truncated.  Uses `unicode-width` for proper CJK/emoji column widths.
pub(crate) fn truncate_str(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    let ellipsis_w = "…".width();
    let target = max_width.saturating_sub(ellipsis_w);
    let mut current = 0usize;
    let mut cutoff = s.len();
    for (i, c) in s.char_indices() {
        // Control characters have no fixed width — treat them as zero-width.
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if current + w > target {
            cutoff = i;
            break;
        }
        current += w;
    }
    let mut result = s[..cutoff].to_string();
    result.push('…');
    result
}

/// Apply a background colour to a list of spans based on the diff line
/// kind. Deletion → dark red bg, Addition → dark green bg, Context →
/// no added background.
fn apply_bg(spans: &[Span<'static>], kind: DiffLineKind) -> Vec<Span<'static>> {
    let bg = match kind {
        DiffLineKind::Deletion => Some(DEL_BG),
        DiffLineKind::Addition => Some(ADD_BG),
        DiffLineKind::Context => None,
    };
    let Some(bg) = bg else {
        return spans.to_vec();
    };
    spans
        .iter()
        .map(|s| Span::styled(s.content.clone(), s.style.bg(bg)))
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        let ctx = rows
            .iter()
            .find(|r| r.left_kind == DiffLineKind::Context && r.left_content == "ctx")
            .unwrap();
        assert_eq!(ctx.left_content, "ctx");
        assert_eq!(ctx.right_content, "ctx");
    }

    // ── try_render_diff_content ──

    #[test]
    fn render_non_diff_returns_none() {
        assert!(try_render_diff_content("hello world", 80).is_none());
    }

    #[test]
    fn render_diff_side_by_side_produces_lines() {
        let result = try_render_diff_content(simple_diff_text(), 80);
        assert!(result.is_some());
        let lines = result.unwrap();
        assert!(!lines.is_empty(), "should produce at least one line");
        // Each line should be exactly 80 columns wide
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| s.width()).sum();
            assert_eq!(w, 80, "side-by-side line should be exactly width=80");
        }
    }

    #[test]
    fn render_diff_unified_when_narrow() {
        let result = try_render_diff_content(simple_diff_text(), 30);
        assert!(result.is_some());
        let lines = result.unwrap();
        assert!(!lines.is_empty());
    }

    // ── truncate_str ──

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_fit() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result.chars().count(), 5, "should be 5 characters wide");
        assert!(result.ends_with('…'));
    }

    // ── spans_fixed_width ──

    #[test]
    fn spans_padded_to_width() {
        let mut spans = vec![Span::styled("hi", Style::default())];
        spans_fixed_width(&mut spans, 5);
        assert_eq!(spans.iter().map(|s| s.width()).sum::<usize>(), 5);
    }

    #[test]
    fn spans_truncated_to_width() {
        let mut spans = vec![Span::styled("hello world", Style::default())];
        spans_fixed_width(&mut spans, 5);
        assert_eq!(spans.iter().map(|s| s.width()).sum::<usize>(), 5);
        // Content should be truncated with … not dropped entirely.
        let text: String = spans.iter().flat_map(|s| s.content.chars()).collect();
        assert_eq!(text.chars().count(), 5, "should be 5 chars wide");
        assert!(
            text.ends_with('…'),
            "truncated content should end with ellipsis"
        );
    }

    #[test]
    fn spans_truncated_multi_span_with_overflow() {
        // Simulates a syntax-highlighted line where the first few small
        // spans (indent, punctuation) fit, but a large content span overflows.
        let mut spans = vec![
            Span::styled("        ".to_string(), Style::default().fg(Color::Blue)),
            Span::styled("\"".to_string(), Style::default().fg(Color::Green)),
            Span::styled(
                "very long string content that exceeds the available width by a lot".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled("\"".to_string(), Style::default().fg(Color::Green)),
        ];
        spans_fixed_width(&mut spans, 20);
        // Total width should be exactly 20.
        assert_eq!(spans.iter().map(|s| s.width()).sum::<usize>(), 20);
        // The first two spans should be preserved as-is.
        assert_eq!(spans[0].content, "        ", "indent span preserved");
        assert_eq!(spans[1].content, "\"", "opening quote preserved");
        // The third (overflowing) span should be truncated to fit the remaining width.
        // Remaining after 8 (indent) + 1 (quote) = 9 columns used; 11 remaining.
        // So the third span should occupy 11 columns and end with ….
        let third = &spans[2];
        assert_eq!(
            third.width(),
            11,
            "overflowing span should fill remaining width"
        );
        assert!(
            third.content.ends_with('…'),
            "truncated span should end with …"
        );
        // There should be no fourth span — it's beyond the overflow cutoff.
        assert_eq!(
            spans.len(),
            3,
            "should have exactly 3 spans after truncation"
        );
    }

    #[test]
    fn spans_truncated_single_span_too_wide() {
        // Single span wider than the target — should be truncated, not dropped.
        let mut spans = vec![Span::styled("aaabbbcccddd".to_string(), Style::default())];
        spans_fixed_width(&mut spans, 6);
        assert_eq!(spans.iter().map(|s| s.width()).sum::<usize>(), 6);
        let text: String = spans.iter().flat_map(|s| s.content.chars()).collect();
        assert_eq!(text.chars().count(), 6, "should be 6 chars wide");
        assert!(
            text.ends_with('…'),
            "truncated content should end with ellipsis"
        );
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
