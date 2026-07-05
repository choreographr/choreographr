use tai_client_core::{DiffHunk, DiffLine, DiffLineKind, FileDiff};

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
            state.old_path = parts.first().and_then(|p| p.strip_prefix("a/")).unwrap_or("").to_string();
            state.new_path = parts.get(1).and_then(|p| p.strip_prefix("b/")).unwrap_or("").to_string();
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
                state.current_hunk_lines.push(DiffLine { kind: DiffLineKind::Addition, content: content.to_string() });
            } else if let Some(content) = line.strip_prefix('-') {
                state.current_hunk_lines.push(DiffLine { kind: DiffLineKind::Deletion, content: content.to_string() });
            } else if let Some(content) = line.strip_prefix(' ') {
                state.current_hunk_lines.push(DiffLine { kind: DiffLineKind::Context, content: content.to_string() });
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
/// Delegates to `build_diff_panes` to avoid duplicating the layout logic.
pub fn diff_display_height(diffs: &[FileDiff]) -> usize {
    build_diff_panes(diffs).len().max(1)
}

/// Result of building aligned left/right panes for a single hunk.
pub struct DiffPaneRow {
    pub left_content: String,
    pub right_content: String,
    pub left_kind: DiffLineKind,
    pub right_kind: DiffLineKind,
}

/// Build aligned left/right pane rows from parsed diffs.
/// Each entry in the returned vec is one row in the side-by-side display.
/// Returns (left_rows, right_rows) where both have the same length.
pub fn build_diff_panes(diffs: &[FileDiff]) -> Vec<DiffPaneRow> {
    let mut rows = Vec::new();
    for file in diffs {
        // File header rows (rendered full-width, not in panes, but we include empty rows for spacing)
        rows.push(DiffPaneRow {
            left_content: format!("--- a/{}", file.old_path),
            right_content: format!("+++ b/{}", file.new_path),
            left_kind: DiffLineKind::Context,
            right_kind: DiffLineKind::Context,
        });
        for hunk in &file.hunks {
            // Hunk header row
            rows.push(DiffPaneRow {
                left_content: hunk.header.clone(),
                right_content: hunk.header.clone(),
                left_kind: DiffLineKind::Context,
                right_kind: DiffLineKind::Context,
            });
            for line in &hunk.lines {
                match line.kind {
                    DiffLineKind::Context => {
                        rows.push(DiffPaneRow {
                            left_content: line.content.clone(),
                            right_content: line.content.clone(),
                            left_kind: DiffLineKind::Context,
                            right_kind: DiffLineKind::Context,
                        });
                    }
                    DiffLineKind::Deletion => {
                        rows.push(DiffPaneRow {
                            left_content: line.content.clone(),
                            right_content: String::new(),
                            left_kind: DiffLineKind::Deletion,
                            right_kind: DiffLineKind::Context,
                        });
                    }
                    DiffLineKind::Addition => {
                        rows.push(DiffPaneRow {
                            left_content: String::new(),
                            right_content: line.content.clone(),
                            left_kind: DiffLineKind::Context,
                            right_kind: DiffLineKind::Addition,
                        });
                    }
                }
            }
        }
    }
    rows
}

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

    // ── diff_display_height ──

    #[test]
    fn display_height_matches_pane_count() {
        let diffs = parse_diff(simple_diff_text());
        assert_eq!(diff_display_height(&diffs), build_diff_panes(&diffs).len().max(1));
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
        let deletion = rows.iter().find(|r| r.left_kind == DiffLineKind::Deletion).unwrap();
        assert_eq!(deletion.left_content, "old");
        assert!(deletion.right_content.is_empty());
    }

    #[test]
    fn pane_addition_appears_on_right() {
        let diffs = parse_diff(simple_diff_text());
        let rows = build_diff_panes(&diffs);
        let addition = rows.iter().find(|r| r.right_kind == DiffLineKind::Addition).unwrap();
        assert_eq!(addition.right_content, "new");
        assert!(addition.left_content.is_empty());
    }

    #[test]
    fn pane_context_appears_on_both_sides() {
        let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n ctx\n";
        let diffs = parse_diff(text);
        let rows = build_diff_panes(&diffs);
        // The leading space is stripped by parse_diff; content is "ctx" without the prefix
        let ctx = rows.iter().find(|r| r.left_kind == DiffLineKind::Context && r.left_content == "ctx").unwrap();
        assert_eq!(ctx.left_content, "ctx");
        assert_eq!(ctx.right_content, "ctx");
    }
}
