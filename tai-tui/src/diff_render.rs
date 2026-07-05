use tai_client_core::{DiffHunk, DiffLine, DiffLineKind, FileDiff};

/// Check if text looks like a unified diff.
/// Scans the full text (tool output may have a metadata prefix on the first line).
pub fn is_diff_text(text: &str) -> bool {
    text.contains("diff --git ") || text.contains("\n--- ") || text.starts_with("--- ")
}

/// Parse unified diff text into structured `FileDiff`s.
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut old_path = String::new();
    let mut new_path = String::new();
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut current_hunk_lines: Vec<DiffLine> = Vec::new();
    let mut current_hunk_header = String::new();
    let mut in_hunk = false;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush_file(&mut files, &mut old_path, &mut new_path, &mut hunks, &mut current_hunk_lines, &mut current_hunk_header, &mut in_hunk);
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            old_path = parts.first().and_then(|p| p.strip_prefix("a/")).unwrap_or("").to_string();
            new_path = parts.get(1).and_then(|p| p.strip_prefix("b/")).unwrap_or("").to_string();
        } else if let Some(rest) = line.strip_prefix("--- ") {
            if old_path.is_empty() {
                old_path = rest.strip_prefix("a/").unwrap_or(rest).to_string();
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if new_path.is_empty() {
                new_path = rest.strip_prefix("b/").unwrap_or(rest).to_string();
            }
        } else if line.starts_with("@@") {
            flush_hunk(&mut hunks, &mut current_hunk_lines, &mut current_hunk_header, &mut in_hunk);
            current_hunk_header = line.to_string();
            in_hunk = true;
        } else if in_hunk {
            if let Some(content) = line.strip_prefix('+') {
                current_hunk_lines.push(DiffLine { kind: DiffLineKind::Addition, content: content.to_string() });
            } else if let Some(content) = line.strip_prefix('-') {
                current_hunk_lines.push(DiffLine { kind: DiffLineKind::Deletion, content: content.to_string() });
            } else if let Some(content) = line.strip_prefix(' ') {
                current_hunk_lines.push(DiffLine { kind: DiffLineKind::Context, content: content.to_string() });
            } else if line == "\\ No newline at end of file" {
                // Skip this marker for simplicity
            } else {
                // Not a valid hunk line, stop hunk
                flush_hunk(&mut hunks, &mut current_hunk_lines, &mut current_hunk_header, &mut in_hunk);
            }
        }
    }
    flush_file(&mut files, &mut old_path, &mut new_path, &mut hunks, &mut current_hunk_lines, &mut current_hunk_header, &mut in_hunk);
    files
}

fn flush_file(
    files: &mut Vec<FileDiff>,
    old_path: &mut String,
    new_path: &mut String,
    hunks: &mut Vec<DiffHunk>,
    current_hunk_lines: &mut Vec<DiffLine>,
    current_hunk_header: &mut String,
    in_hunk: &mut bool,
) {
    flush_hunk(hunks, current_hunk_lines, current_hunk_header, in_hunk);
    if !old_path.is_empty() || !new_path.is_empty() || !hunks.is_empty() {
        files.push(FileDiff {
            old_path: std::mem::take(old_path),
            new_path: std::mem::take(new_path),
            hunks: std::mem::take(hunks),
        });
    }
}

fn flush_hunk(
    hunks: &mut Vec<DiffHunk>,
    lines: &mut Vec<DiffLine>,
    header: &mut String,
    in_hunk: &mut bool,
) {
    if *in_hunk {
        hunks.push(DiffHunk {
            header: std::mem::take(header),
            lines: std::mem::take(lines),
        });
        *in_hunk = false;
    }
}

/// Number of display rows a diff takes up in side-by-side mode.
/// Matches what `build_diff_panes` actually emits:
///   1 banner row per file (---/+++ combined), 1 per hunk header, N per hunk line.
pub fn diff_display_height(diffs: &[FileDiff]) -> usize {
    let mut rows = 0usize;
    for file in diffs {
        if !file.old_path.is_empty() || !file.new_path.is_empty() {
            rows += 1;
        }
        for hunk in &file.hunks {
            rows += 1;
            rows += hunk.lines.len();
        }
    }
    rows.max(1)
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
