use crate::markdown_render::*;
use crate::state::*;
use ratatui::text::Line;

#[test]
fn history_text_height_accounts_for_wrapping_and_blank_lines() {
    assert_eq!(history_text_height("hello", 10), 1);
    assert_eq!(history_text_height("hello world", 5), 3);
    assert_eq!(history_text_height("a\nb\n", 10), 3);
    assert_eq!(history_text_height("", 10), 1);
    assert_eq!(history_text_height("\n", 10), 2);
}

#[test]
fn display_width_treats_emoji_as_terminal_cells() {
    assert_eq!(display_width("😀"), 2);
    assert_eq!(display_width("A😀B"), 4);
    assert_eq!(display_width("👨‍👩‍👧‍👦"), 2);
}

#[test]
fn wrapped_line_height_uses_terminal_display_width() {
    assert_eq!(lines_height(&[Line::from("😀😀")], 2), 2);
    assert_eq!(lines_height(&[Line::from("👨‍👩‍👧‍👦")], 2), 1);
}

#[test]
fn markdown_lines_render_tables() {
    let lines = markdown_lines(
        "| Name | Role | Years |\n|:--|:--:|--:|\n| Ada Lovelace | Mathematician | 1842 |\n| Grace Hopper | Computer Scientist | 1943 |",
        60,
    );

    let rendered = lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("╭"));
    assert!(rendered.contains("Ada Lovelace"));
    assert!(rendered.contains("Grace Hopper"));
    assert!(rendered.contains("Mathematician"));
}

#[test]
fn markdown_lines_render_lists_with_item_text() {
    let lines = markdown_lines("- one\n- [x] done\n1. first\n2. second", 80);

    let rendered = lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("• one"));
    assert!(rendered.contains("• [x] done"));
    assert!(rendered.contains("1. first"));
    assert!(rendered.contains("2. second"));
}

#[test]
fn oversized_history_item_keeps_visible_tail() {
    let wrapped = history_text_height("123456789", 3);
    assert_eq!(wrapped, 3);

    let rows_remaining = 2;
    let rows_to_skip = 0;
    let bottom_line = wrapped.saturating_sub(rows_to_skip);
    let top_line = bottom_line.saturating_sub(rows_remaining);

    assert_eq!(top_line, 1);
}
