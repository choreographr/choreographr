use super::super::*;

// ── Inline styling (bold, italic, strikethrough, code) ──────────────

#[test]
fn markdown_bold_applies_bold_modifier() {
    let result = markdown_lines("**bold text**", 80);
    let line = &result[0];
    let has_bold = line
        .spans
        .iter()
        .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
    assert!(has_bold, "bold markdown should apply BOLD modifier");
    let text = line.to_string();
    assert!(text.contains("bold text"), "bold content should appear");
    assert!(
        !text.contains("**"),
        "markdown syntax should not appear literally"
    );
}

#[test]
fn markdown_italic_applies_italic_modifier() {
    let result = markdown_lines("*italic text*", 80);
    let line = &result[0];
    let has_italic = line
        .spans
        .iter()
        .any(|s| s.style.add_modifier.contains(Modifier::ITALIC));
    assert!(has_italic, "italic markdown should apply ITALIC modifier");
    let text = line.to_string();
    assert!(text.contains("italic text"), "italic content should appear");
    assert!(
        !text.contains('*'),
        "markdown syntax should not appear literally"
    );
}

#[test]
fn markdown_strikethrough_applies_crossed_out_modifier() {
    let result = markdown_lines("~~strike~~", 80);
    let line = &result[0];
    let has_crossed = line
        .spans
        .iter()
        .any(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT));
    assert!(
        has_crossed,
        "strikethrough markdown should apply CROSSED_OUT modifier"
    );
    let text = line.to_string();
    assert!(
        text.contains("strike"),
        "strikethrough content should appear"
    );
    assert!(
        !text.contains("~~"),
        "markdown syntax should not appear literally"
    );
}

#[test]
fn markdown_inline_code_applies_cyan_color() {
    let result = markdown_lines("use `code` here", 80);
    let line = &result[0];
    let has_cyan = line.spans.iter().any(|s| s.style.fg == Some(Color::Cyan));
    assert!(has_cyan, "inline code should be rendered in Cyan");
    let text = line.to_string();
    assert!(text.contains("code"), "code content should appear");
    assert!(!text.contains('`'), "backticks should not appear literally");
}

#[test]
fn markdown_bold_and_italic_nested() {
    let result = markdown_lines("***nested***", 80);
    let line = &result[0];
    let has_bold = line
        .spans
        .iter()
        .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
    let has_italic = line
        .spans
        .iter()
        .any(|s| s.style.add_modifier.contains(Modifier::ITALIC));
    assert!(has_bold, "nested *** should apply BOLD");
    assert!(has_italic, "nested *** should apply ITALIC");
    let text = line.to_string();
    assert!(text.contains("nested"), "content should appear");
}

#[test]
fn markdown_styled_text_within_budget_wraps_correctly() {
    // Long styled content at a narrow width — should wrap without overflow.
    let words = (0..20).map(|_| "word").collect::<Vec<_>>().join(" ");
    let long_bold = format!("**{words}**");
    let result = markdown_lines(&long_bold, 20);
    assert!(result.len() > 1, "wide bold content should wrap");
    for line in &result {
        assert!(
            line.width() <= 20,
            "no wrapped bold line should exceed width, got {}",
            line.width()
        );
    }
    let has_bold = result
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
    assert!(has_bold, "wrapped content should still have BOLD modifier");
}

#[test]
fn markdown_styled_text_with_indent_does_not_overflow() {
    // Styled content inside a blockquote (which adds indent).
    let md = "> **bold content inside blockquote**";
    let result = markdown_lines(md, 20);
    for line in &result {
        assert!(
            line.width() <= 20,
            "indented styled line must not exceed width, got {}",
            line.width()
        );
    }
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("bold content"),
        "styled content should be present"
    );
}

#[test]
fn markdown_inline_code_in_blockquote_is_colored() {
    let md = "> `short_code`";
    let result = markdown_lines(md, 20);
    for line in &result {
        assert!(
            line.width() <= 20,
            "indented inline code must not exceed width, got {}",
            line.width()
        );
    }
    let has_cyan = result
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| s.style.fg == Some(Color::Cyan));
    assert!(has_cyan, "inline code in blockquote should be Cyan");
}

#[test]
fn markdown_inline_code_wider_than_width_splits() {
    // An inline code segment wider than the available width.
    let long_code = "abcdefghijklmnopqrstuvwxyz0123456789";
    let md = format!("`{long_code}`");
    let result = markdown_lines(&md, 10);
    // Should have wrapped onto multiple lines.
    assert!(result.len() > 1, "over-wide inline code should split");
    for line in &result {
        assert!(
            line.width() <= 10,
            "split code chunk must not exceed width, got {}",
            line.width()
        );
    }
    // All chunks should be cyan.
    for line in &result {
        for span in &line.spans {
            if !span.content.trim().is_empty() {
                assert_eq!(
                    span.style.fg,
                    Some(Color::Cyan),
                    "every code chunk should be Cyan"
                );
            }
        }
    }
    // Full content should appear across the lines.
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains(long_code),
        "all characters of the code must appear in the output"
    );
}

// ── Links ─────────────────────────────────────────────────

#[test]
fn markdown_link_renders_bold_content_with_underlined_url() {
    let result = markdown_lines("[click here](http://example.com)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("click"), "word 'click' should appear");
    assert!(whole.contains("here"), "word 'here' should appear");
    assert!(whole.contains("http://example.com"), "URL should appear");
    assert!(
        !whole.contains("[click here]"),
        "markdown syntax should not appear literally"
    );
    // The link content should have BOLD modifier
    let has_bold = result
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| s.content.contains("click") && s.style.add_modifier.contains(Modifier::BOLD));
    assert!(has_bold, "link content should be bold");
    // The URL should have UNDERLINED modifier
    let has_underlined = result.iter().flat_map(|l| l.spans.iter()).any(|s| {
        s.content.contains("http://") && s.style.add_modifier.contains(Modifier::UNDERLINED)
    });
    assert!(has_underlined, "URL should be underlined");
}

#[test]
fn markdown_link_empty_destination_no_url() {
    let result = markdown_lines("[text]()", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("text"), "link text should appear");
    assert!(
        !whole.contains("http"),
        "no URL should appear for empty destination"
    );
    // Without a destination, the content should have no BOLD modifier
    let has_bold = result
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
    assert!(!has_bold, "empty link should not apply bold");
}

#[test]
fn markdown_link_inside_bold_applies_both() {
    let result = markdown_lines("[**bold link**](http://example.com)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("bold"), "bold word should appear");
    assert!(whole.contains("link"), "link word should appear");
    assert!(
        !whole.contains("**"),
        "markdown syntax should not appear literally"
    );
    assert!(whole.contains("http://example.com"), "URL should appear");
    // The content inherits BOLD from markdown **plus** the link's BOLD
    let has_bold = result
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| s.content.contains("bold") && s.style.add_modifier.contains(Modifier::BOLD));
    assert!(has_bold, "link content should be bold");
}

#[test]
fn markdown_link_with_code_is_colored() {
    let result = markdown_lines("[`code`](http://example.com)", 80);
    let has_cyan = result
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| s.style.fg == Some(Color::Cyan));
    assert!(has_cyan, "inline code should be Cyan inside a link");
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("code"), "code content should appear");
    assert!(
        !whole.contains('`'),
        "backticks should not appear literally"
    );
}

#[test]
fn markdown_link_wrapping_does_not_overflow() {
    let long = "a".repeat(30);
    let md = format!("[{long}](http://example.com)");
    let result = markdown_lines(&md, 10);
    // Should wrap onto multiple lines: content wraps, then URL on its own line.
    assert!(
        result.len() >= 3,
        "long link text should wrap onto multiple lines, got {}",
        result.len()
    );
    // The first 3 lines are the bold content — each must be ≤ width.
    // The last line(s) contain the separator + URL, which may exceed width.
    for line in result.iter().take(3) {
        assert!(
            line.width() <= 10,
            "wrapped link content line width {} exceeds 10",
            line.width()
        );
    }
    // The URL should appear somewhere.
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("http://example.com"), "URL should appear");
}

// ── heading modifiers ────────────────────────────────────────────────

#[test]
fn heading_has_bold_and_underlined_modifier() {
    let result = markdown_lines("# heading text", 80);
    let has_modifiers = result.iter().flat_map(|l| l.spans.iter()).any(|s| {
        s.style.add_modifier.contains(Modifier::BOLD)
            && s.style.add_modifier.contains(Modifier::UNDERLINED)
    });
    assert!(
        has_modifiers,
        "heading spans should have BOLD | UNDERLINED modifiers"
    );
}

#[test]
fn heading_content_not_literal() {
    let result = markdown_lines("# **bold** heading", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        !whole.contains("**bold**"),
        "markdown syntax should not appear"
    );
    assert!(whole.contains("bold"), "bold content should appear");
}

#[test]
fn heading_has_two_blank_lines_before() {
    // Two blank lines should precede a heading when preceded by content.
    let result = markdown_lines("some text\n# heading\nmore text", 80);
    // Walk through lines and find the heading line.
    let heading_idx = result
        .iter()
        .position(|l| l.to_string().contains("heading"));
    assert!(heading_idx.is_some(), "heading text should appear");
    let idx = heading_idx.unwrap();
    // Verify two blank lines precede it.
    assert!(
        idx >= 2 && result[idx - 1].width() == 0 && result[idx - 2].width() == 0,
        "expected two blank lines before heading, got lines around index {idx}: \
             lines[{}]='{}' lines[{}]='{}' lines[{}]='{}'",
        idx.saturating_sub(2),
        result
            .get(idx - 2)
            .map(|l| format!("{l}"))
            .unwrap_or_default(),
        idx - 1,
        result[idx - 1],
        idx,
        result[idx]
    );
}

#[test]
fn first_heading_no_blank_lines_on_top() {
    // A heading at the very start of the document must not be preceded by
    // blank lines — the "two blank lines" rule only applies to headings
    // that follow other content.
    let result = markdown_lines("# first heading", 80);
    let heading_idx = result
        .iter()
        .position(|l| l.to_string().contains("first"))
        .expect("heading should appear");
    assert_eq!(
        heading_idx, 0,
        "first heading should be the very first rendered line, got {heading_idx} lines before it: \
             {result:?}"
    );
}

#[test]
fn first_heading_has_no_hash_prefix() {
    // A level-1 heading drops the `# ` marker entirely — the title
    // renders flush left.
    let result = markdown_lines("# Title", 80);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].to_string(), "Title");
    assert!(!result[0].to_string().contains('#'));
}

#[test]
fn heading_prefix_by_level() {
    // Level 1 has no prefix; level 2 gets a single wedge; deeper levels
    // stack one solid block per extra level before the wedge.
    assert_eq!(heading_prefix(1), None);
    assert_eq!(heading_prefix(2), Some("\u{e0b4} ".to_string()));
    assert_eq!(heading_prefix(3), Some("█\u{e0b4} ".to_string()));
    assert_eq!(heading_prefix(4), Some("██\u{e0b4} ".to_string()));
    assert_eq!(heading_prefix(6), Some("████\u{e0b4} ".to_string()));
}

#[test]
fn level_two_heading_renders_wedge_prefix() {
    let result = markdown_lines("# Title\n\n## Section", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("\u{e0b4} Section"), "got: {whole:?}");
    assert!(
        !whole.contains("## Section"),
        "raw markdown markers must not appear, got: {whole:?}"
    );
}

#[test]
fn level_three_heading_renders_block_before_wedge() {
    let result = markdown_lines("# Title\n\n## Section\n\n### Sub", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("█\u{e0b4} Sub"), "got: {whole:?}");
}

#[test]
fn first_heading_normalized_from_double_hash() {
    // A document whose first heading is `##` is normalized so the first
    // heading renders as level 1 (no prefix) and every later heading
    // shifts down by the same amount.
    let result = markdown_lines("## First\n\n### Sub", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("First"), "first heading text should render");
    assert!(
        whole.contains("\u{e0b4} Sub"),
        "the `###` heading should normalize to level 2 and get the wedge, got: {whole:?}"
    );
    assert!(
        !whole.contains("## First") && !whole.contains("### Sub"),
        "raw hash markers must not appear, got: {whole:?}"
    );
}

#[test]
fn first_heading_normalization_shifts_only_heading_levels() {
    // Paragraph text is untouched; only heading levels shift.  With the
    // first heading at `##` (shift 1), a `#####` heading normalizes to
    // level 4 → two solid blocks before the wedge.
    let result = markdown_lines("## First\n\nplain paragraph\n\n##### Deep", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("plain paragraph"), "paragraph should render");
    assert!(
        whole.contains("██\u{e0b4} Deep"),
        "`#####` normalizes to level 4 → two blocks + wedge, got: {whole:?}"
    );
}
