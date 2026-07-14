use super::render::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use tai_client_core::{DiffHunk, DiffLine, DiffLineKind, FileDiff};
use tai_proto::SessionStatus;

// ── render_history_text tests ──

#[test]
fn render_history_text_no_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 0;

    terminal
        .draw(|frame| {
            render_history_text(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                "line1\nline2",
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    assert_eq!(rows_remaining, 27, "consumed 2 visible rows");
    assert_eq!(y, 27, "y moved up by 2");
    assert_eq!(rows_to_skip, 0, "rows_to_skip consumed completely");
}

#[test]
fn render_history_text_partial_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 2;

    terminal
        .draw(|frame| {
            render_history_text(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                "line1\nline2\nline3\nline4\nline5",
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    // wrapped=6, skip=2 → visible = (6-2).min(30) = 4 → remaining = 30-4 = 26
    assert_eq!(rows_remaining, 26);
    assert_eq!(y, 26);
    assert_eq!(rows_to_skip, 0);
}

#[test]
fn render_history_text_full_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 10;

    terminal
        .draw(|frame| {
            render_history_text(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                "line1\nline2\nline3\nline4\nline5",
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    // wrapped=6 <= skip=10 → fully skipped, skip reduced by 6
    assert_eq!(rows_remaining, 30, "no rows consumed");
    assert_eq!(y, 30, "y unchanged");
    assert_eq!(rows_to_skip, 4, "rows_to_skip decremented by 6");
}

#[test]
fn render_history_text_exhausted_viewport() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 2;
    let mut y = 30;
    let mut rows_to_skip = 2;

    terminal
        .draw(|frame| {
            render_history_text(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                "line1\nline2\nline3\nline4\nline5",
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    // wrapped=6, skip=2 → visible = (6-2).min(2) = 2 → remaining = 0
    assert_eq!(rows_remaining, 0, "viewport exhausted");
    assert_eq!(y, 28);
    assert_eq!(rows_to_skip, 0);
}

#[test]
fn render_history_text_zero_remaining() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 0;
    let mut y = 0;
    let mut rows_to_skip = 0;

    terminal
        .draw(|frame| {
            render_history_text(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                "content",
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    // visible = (1-0).min(0) = 0 → returns early
    assert_eq!(rows_remaining, 0);
    assert_eq!(y, 0);
    assert_eq!(rows_to_skip, 0);
}

// ── render_history_lines tests ──

#[test]
fn render_history_lines_no_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 0;

    terminal
        .draw(|frame| {
            render_history_lines(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                vec![Line::from("a"), Line::from("b"), Line::from("c")],
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    assert_eq!(rows_remaining, 26, "3 rows consumed");
    assert_eq!(y, 26);
    assert_eq!(rows_to_skip, 0);
}

#[test]
fn render_history_lines_partial_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 1;

    terminal
        .draw(|frame| {
            render_history_lines(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                vec![Line::from("a"), Line::from("b"), Line::from("c")],
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    // wrapped=4, skip=1 → visible=3 → remaining=27
    assert_eq!(rows_remaining, 27);
    assert_eq!(y, 27);
    assert_eq!(rows_to_skip, 0);
}

#[test]
fn render_history_lines_full_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 10;

    terminal
        .draw(|frame| {
            render_history_lines(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                vec![Line::from("only")],
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    assert_eq!(rows_remaining, 30, "no rows consumed");
    assert_eq!(y, 30);
    assert_eq!(rows_to_skip, 8, "rows_to_skip decremented by 2");
}

#[test]
fn render_history_lines_zero_remaining() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 0;
    let mut y = 0;
    let mut rows_to_skip = 0;

    terminal
        .draw(|frame| {
            render_history_lines(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                vec![Line::from("content")],
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
                78,
            );
        })
        .unwrap();

    assert_eq!(rows_remaining, 0);
    assert_eq!(y, 0);
    assert_eq!(rows_to_skip, 0);
}

// ── render_history_diff tests ──

#[test]
fn render_history_diff_no_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 0;

    let diffs = vec![FileDiff {
        old_path: String::new(),
        new_path: String::new(),
        hunks: vec![DiffHunk {
            header: "header".to_string(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Context,
                content: "unchanged".to_string(),
            }],
        }],
    }];

    terminal
        .draw(|frame| {
            render_history_diff(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                &diffs,
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
            );
        })
        .unwrap();

    // build_diff_panes always emits a file header row, so height = 1 (file) + 1 (hunk) + 1 (line) = 3, +2 blanks = 5
    assert_eq!(rows_remaining, 25, "5 diff rows consumed");
    assert_eq!(y, 25, "y moved up by 5");
    assert_eq!(rows_to_skip, 0);
}

#[test]
fn render_history_diff_partial_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 1;

    let diffs = vec![FileDiff {
        old_path: String::new(),
        new_path: String::new(),
        hunks: vec![DiffHunk {
            header: "hdr".to_string(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Addition,
                content: "added".to_string(),
            }],
        }],
    }];

    terminal
        .draw(|frame| {
            render_history_diff(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                &diffs,
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
            );
        })
        .unwrap();

    // full_height=5, skip=1 → visible=4 → remaining=26
    assert_eq!(rows_remaining, 26);
    assert_eq!(y, 26);
    assert_eq!(rows_to_skip, 0);
}

#[test]
fn render_history_diff_full_skip() {
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut rows_remaining = 30;
    let mut y = 30;
    let mut rows_to_skip = 10;

    let diffs = vec![FileDiff {
        old_path: String::new(),
        new_path: String::new(),
        hunks: vec![DiffHunk {
            header: "h".to_string(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Context,
                content: "c".to_string(),
            }],
        }],
    }];

    terminal
        .draw(|frame| {
            render_history_diff(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 30,
                },
                &diffs,
                &mut rows_remaining,
                &mut y,
                &mut rows_to_skip,
            );
        })
        .unwrap();

    // full_height=5 <= skip=10 → fully skipped, skip reduced by 5
    assert_eq!(rows_remaining, 30);
    assert_eq!(y, 30);
    assert_eq!(rows_to_skip, 5);
}

// ── diff_cell_spans tests ──

fn span_from_text(text: &str) -> Vec<ratatui::text::Span<'static>> {
    vec![ratatui::text::Span::styled(
        text.to_string(),
        Style::default(),
    )]
}

#[test]
fn diff_cell_spans_pads_short_content() {
    let spans = diff_cell_spans(&span_from_text("hi"), DiffLineKind::Context, 10, true);
    let text = spans[0].content.trim_end();
    assert!(
        text.starts_with("hi"),
        "content='{text}' should start with 'hi'"
    );
}

#[test]
fn diff_cell_spans_truncates_long_content() {
    let long = "a".repeat(20);
    let spans = diff_cell_spans(&span_from_text(&long), DiffLineKind::Context, 5, true);
    // truncated to 4 chars in the first span + '…' as a separate span = 2 spans
    assert_eq!(spans[0].content.chars().count(), 4);
    assert_eq!(spans[1].content, "…");
}

#[test]
fn diff_cell_spans_left_deletion_has_red_style() {
    let spans = diff_cell_spans(&span_from_text("del"), DiffLineKind::Deletion, 10, true);
    let style = spans[0].style;
    assert_eq!(style.fg, Some(Color::Red));
    assert_eq!(style.bg, Some(Color::Rgb(80, 0, 0)));
}

#[test]
fn diff_cell_spans_right_deletion_has_default_style() {
    let spans = diff_cell_spans(&span_from_text("del"), DiffLineKind::Deletion, 10, false);
    assert_eq!(spans[0].style, Style::default());
}

#[test]
fn diff_cell_spans_right_addition_has_green_style() {
    let spans = diff_cell_spans(&span_from_text("add"), DiffLineKind::Addition, 10, false);
    let style = spans[0].style;
    assert_eq!(style.fg, Some(Color::Green));
    assert_eq!(style.bg, Some(Color::Rgb(0, 80, 0)));
}

#[test]
fn diff_cell_spans_left_addition_has_default_style() {
    let spans = diff_cell_spans(&span_from_text("add"), DiffLineKind::Addition, 10, true);
    assert_eq!(spans[0].style, Style::default());
}

#[test]
fn diff_cell_spans_context_has_default_style() {
    let spans = diff_cell_spans(&span_from_text("ctx"), DiffLineKind::Context, 10, true);
    assert_eq!(spans[0].style, Style::default());
    let spans = diff_cell_spans(&span_from_text("ctx"), DiffLineKind::Context, 10, false);
    assert_eq!(spans[0].style, Style::default());
}

#[test]
fn diff_cell_spans_preserves_syntax_fg_on_deletion() {
    // Simulate a syntax-highlighted span with a non-default fg colour
    let input = vec![ratatui::text::Span::styled(
        "fn".to_string(),
        Style::default().fg(Color::Rgb(200, 100, 0)),
    )];
    let spans = diff_cell_spans(&input, DiffLineKind::Deletion, 10, true);
    assert_eq!(
        spans[0].style.fg,
        Some(Color::Rgb(200, 100, 0)),
        "syntax foreground should be preserved on deletion"
    );
    assert_eq!(
        spans[0].style.bg,
        Some(Color::Rgb(80, 0, 0)),
        "diff background should be applied"
    );
}

#[test]
fn diff_cell_spans_preserves_syntax_fg_on_addition() {
    let input = vec![ratatui::text::Span::styled(
        "let".to_string(),
        Style::default().fg(Color::Rgb(0, 150, 200)),
    )];
    let spans = diff_cell_spans(&input, DiffLineKind::Addition, 10, false);
    assert_eq!(spans[0].style.fg, Some(Color::Rgb(0, 150, 200)));
    assert_eq!(spans[0].style.bg, Some(Color::Rgb(0, 80, 0)));
}

// ── format_status tests ──

#[test]
fn format_status_retrying() {
    let status = SessionStatus::Retrying {
        attempt: 2,
        max_attempts: 5,
        delay_ms: 3000,
    };
    assert_eq!(format_status(&status), "retrying (2/5, 3s)");
}

#[test]
fn format_status_retrying_first_attempt() {
    let status = SessionStatus::Retrying {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 1500,
    };
    assert_eq!(format_status(&status), "retrying (1/3, 1s 500ms)");
}

#[test]
fn diff_cell_spans_pads_with_diff_background() {
    let spans = diff_cell_spans(&span_from_text("hi"), DiffLineKind::Deletion, 10, true);
    // There should be a padding span at the end with the diff background
    assert!(spans.len() > 1, "should have padding span");
    assert_eq!(spans.last().unwrap().style.bg, Some(Color::Rgb(80, 0, 0)),);
    // The text span should have the red bg too
    assert_eq!(spans[0].style.bg, Some(Color::Rgb(80, 0, 0)),);
}
