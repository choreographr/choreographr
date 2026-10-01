use super::super::*;

// ── BlockQuote ──────────────────────────────────────────────────────

#[test]
#[expect(clippy::assert_is_empty)] // this clippy version wants assert_ne!(v, [] as [...]) here — worse
fn markdown_lines_blockquote_simple() {
    let md = "> hello world";
    let result = markdown_lines(md, 80);
    assert!(!result.is_empty());
    assert_eq!(result[0].to_string(), "│ hello world");
}

#[test]
fn markdown_lines_blockquote_within_budget() {
    let md = "> hello world";
    let result = markdown_lines(md, 20);
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for line in &result {
        assert!(
            line.width() <= 20,
            "blockquote line width {} exceeds 20",
            line.width()
        );
    }
    assert!(text.contains("│ hello world"), "text should be present");
}

#[test]
fn markdown_blockquote_bar_replaces_angle_marker() {
    // The literal `"> "` marker is gone; a two-column bar takes its place so
    // the wrap budget (width - 2) is unchanged but the block reads as a quote
    // rather than as body text with ASCII markers.
    let result = markdown_lines("> quoted", 80);
    assert_eq!(result[0].to_string(), "│ quoted");
    assert!(!result[0].to_string().contains('>'));
}

#[test]
fn markdown_blockquote_text_is_italic_but_inline_code_is_not() {
    let result = markdown_lines("> plain and `code`", 80);
    let spans: Vec<&Span<'static>> = result.iter().flat_map(|l| l.spans.iter()).collect();
    // Plain prose (no foreground colour) is italicised.
    assert!(
        spans.iter().any(|s| s.style.fg.is_none()
            && s.content.contains("plain")
            && s.style.add_modifier.contains(Modifier::ITALIC)),
        "plain quote text should be italic: {spans:#?}"
    );
    // Inline code keeps its Cyan foreground and stays upright so syntax
    // highlighting is not smeared.
    assert!(
        spans.iter().any(|s| s.style.fg == Some(Color::Cyan)
            && s.content.contains("code")
            && !s.style.add_modifier.contains(Modifier::ITALIC)),
        "inline code should stay upright: {spans:#?}"
    );
    // The bar itself is drawn in the reserved gutter colour, not italic.
    assert!(
        spans.iter().any(|s| s.content.as_ref() == QUOTE_BAR
            && s.style.fg == Some(QUOTE_BAR_COLOR)
            && !s.style.add_modifier.contains(Modifier::ITALIC)),
        "bar span should be the reserved gutter span: {spans:#?}"
    );
}

#[test]
fn blockquote_chrome_counts_leading_bars() {
    let (single, _joins, chrome) = markdown_lines_joined("> one", 80);
    assert_eq!(single[0].width(), 5, "│ one");
    assert_eq!(chrome[0].intervals(), &[(0, 2)]);
    // `>>` nests: the outer bar is prepended in front of the inner bar, and the
    // inner chrome shifts right, so both bars are recorded.
    let (nested, _joins, nchrome) = markdown_lines_joined(">> two", 80);
    assert_eq!(nested[0].width(), 7, "│ │ two");
    assert_eq!(nchrome[0].intervals(), &[(0, 2), (2, 4)]);
    // A non-quote line has no chrome.
    let (_plain, _joins, pchrome) = markdown_lines_joined("just text", 80);
    assert!(pchrome[0].is_empty());
}

#[test]
fn blockquote_in_list_chrome_hides_bar_keeps_marker() {
    // The fix this task exists for: a block quote nested directly inside a list
    // item records the bar as chrome (shifted past the marker) so a copy keeps
    // the list marker and drops only the bar.
    let (lines, _joins, chrome) = markdown_lines_joined("- > hello", 80);
    assert_eq!(lines[0].to_string(), "• │ hello");
    // Marker columns 0..2 are content; the bar (2..4) is chrome.
    assert_eq!(chrome[0].intervals(), &[(2, 4)]);
    // Deeper `- > > hi`: both nested bars are chrome, shifted past the marker.
    let (deep, _joins, dchrome) = markdown_lines_joined("- > > hi", 80);
    assert_eq!(deep[0].to_string(), "• │ │ hi");
    assert_eq!(dchrome[0].intervals(), &[(2, 4), (4, 6)]);
}
