use super::super::*;
use super::{first_content_column, leading_spaces};

// ── List ─────────────────────────────────────────────────────────────

#[test]
fn markdown_lines_unordered_list_simple() {
    let md = "- item one\n- item two";
    let result = markdown_lines(md, 80);
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("• item one"), "first item should render");
    assert!(text.contains("• item two"), "second item should render");
}

#[test]
fn markdown_lines_ordered_list_simple() {
    let md = "1. first\n2. second";
    let result = markdown_lines(md, 80);
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("1. first"), "first ordered item");
    assert!(text.contains("2. second"), "second ordered item");
}

#[test]
fn ordered_list_items_share_content_column() {
    // The reported repro: a list spanning the 9/10/11 digit boundary must
    // render every item's content at the same column (the widest marker's
    // 4 columns) — not just the wrapped lines, but the first lines too.
    let md = "9. Thread Communication\n10. Inline Comments\n11. Pre-Commit Workflow";
    let result = markdown_lines(md, 80);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    for line in &text {
        assert_eq!(
            first_content_column(line),
            4,
            "content should start at column 4 (widest marker \"10. \"), got {line:?}"
        );
    }
}

#[test]
fn ordered_list_numbers_are_right_aligned() {
    // The digit columns line up on the ones place: item 9's "9" must sit
    // directly above item 10's "0" (not its "1"), with the ". " suffix and
    // following content at a fixed column for every item.
    let md = "9. ninth\n10. tenth\n11. eleventh";
    let result = markdown_lines(md, 80);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    assert_eq!(text[0], " 9. ninth", "item 9 right-aligned under item 10");
    assert_eq!(text[1], "10. tenth");
    assert_eq!(text[2], "11. eleventh");
    // The ". " of every marker sits at the same column (index 2 here).
    for line in &text {
        assert_eq!(line.find(". "), Some(2), "period column fixed: {line:?}");
    }
    // The ones digits share a column: "9" (line 0) above "0" (line 1).
    assert_eq!(text[0].as_bytes()[1], b'9');
    assert_eq!(text[1].as_bytes()[1], b'0');
}

#[test]
fn ordered_list_wrapped_lines_share_widest_marker_indent() {
    // Item 1's marker is 3 columns wide but item 10's is 4; every item's
    // continuation lines must indent to the widest marker (4 columns) so
    // wrapped text lines up across the list.
    let long = "b".repeat(30);
    let md = format!("1. {long}\n2. x\n3. x\n4. x\n5. x\n6. x\n7. x\n8. x\n9. x\n10. {long}");
    let result = markdown_lines(&md, 20);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    // Both wrapped items must continue under the widest marker, and their
    // first-line content must start at the same column as well.  Item 1's
    // number is right-aligned in the two-digit column, so its marker is
    // " 1. " (leading pad) rather than "1. ".
    for marker in [" 1. ", "10. "] {
        let idx = text
            .iter()
            .position(|l| l.starts_with(marker))
            .unwrap_or_else(|| panic!("marker {marker:?} not found: {text:?}"));
        assert_eq!(
            first_content_column(&text[idx]),
            4,
            "first-line content of {marker:?} should start at col 4, got {:?}",
            text[idx]
        );
        let cont = text
            .get(idx + 1)
            .expect("wrapped item should have a continuation line");
        assert_eq!(
            leading_spaces(cont),
            4,
            "continuation of {marker:?} should indent 4 cols (widest marker), got {cont:?}"
        );
    }
}

#[test]
fn ordered_list_wrapped_lines_never_exceed_width() {
    // With a 1-digit and a 2-digit marker (list starting at 9), the uniform
    // content budget must keep every line (marker line and continuation
    // line) inside `width`.  Without the shared budget the narrower item's
    // continuation would overflow by one column.
    let long = "d".repeat(30);
    let md = format!("9. {long}\n10. {long}");
    let result = markdown_lines(&md, 20);
    assert!(
        result.len() >= 4,
        "expected marker lines plus wrapped continuations: {result:?}"
    );
    for line in &result {
        assert!(
            line.width() <= 20,
            "line exceeds width: {line:?} (width {})",
            line.width()
        );
    }
}

#[test]
fn ordered_list_three_digit_marker_indent() {
    // A list starting at 98 reaches a 3-digit marker ("100. " = 5 cols);
    // item 98's continuation lines must indent 5, not its own 4.
    let long = "c".repeat(30);
    let md = format!("98. {long}\n99. {long}\n100. {long}");
    let result = markdown_lines(&md, 20);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    // Item 98's number is right-aligned in the three-digit column, so its
    // marker is " 98. " (leading pad).
    let idx = text
        .iter()
        .position(|l| l.starts_with(" 98. "))
        .expect("item 98 should render");
    let cont = text
        .get(idx + 1)
        .expect("wrapped item 98 should have a continuation line");
    assert_eq!(
        leading_spaces(cont),
        5,
        "continuation of item 98 should indent 5 cols (widest marker \"100. \"), got {cont:?}"
    );
    // All lines stay within the terminal width.
    for line in &result {
        assert!(
            line.width() <= 20,
            "line exceeds width: {line:?} (width {})",
            line.width()
        );
    }
}

#[test]
fn unordered_list_continuation_indent_unchanged() {
    // Bullet markers are all the same width, so the shared-indent logic
    // must not change unordered-list wrapping (continuation stays 2 cols).
    let long = "e".repeat(30);
    let md = format!("- {long}\n- short");
    let result = markdown_lines(&md, 20);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    let idx = text
        .iter()
        .position(|l| l.starts_with("• "))
        .expect("bullet item should render");
    let cont = text
        .get(idx + 1)
        .expect("wrapped bullet should have a continuation line");
    assert_eq!(
        leading_spaces(cont),
        2,
        "bullet continuation indent should stay 2 cols, got {cont:?}"
    );
}

#[test]
fn markdown_lines_list_within_budget() {
    let md = "- hello world";
    let result = markdown_lines(md, 10);
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("•"), "bullet should be present");
    assert!(text.contains("hello"), "content should be present");
}

#[test]
#[expect(clippy::assert_is_empty)] // this clippy version wants assert_ne!(v, [] as [...]) here — worse
fn markdown_lines_list_continuation_preserves_spans() {
    let md = "- **bold** and `code`";
    let result = markdown_lines(md, 80);
    assert!(!result.is_empty());
    let first = &result[0];
    // At minimum the text should not have markdown syntax literals.
    let text = first.to_string();
    assert!(
        !text.contains("**bold**"),
        "bold syntax should not appear literally"
    );
    assert!(text.contains("bold"), "bold text should appear");
}

// ── ensure_blank_line ──────────────────────────────────────────────────

#[test]
fn ensure_blank_line_empty() {
    let mut lines = vec![];
    ensure_blank_line(&mut lines);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].width(), 0);
}

#[test]
fn ensure_blank_line_after_nonblank() {
    let mut lines = vec![Line::from("hello")];
    ensure_blank_line(&mut lines);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1].width(), 0);
}

#[test]
fn ensure_blank_line_collapses() {
    let mut lines = vec![
        Line::from("hello"),
        Line::from(Span::styled(String::new(), Style::default())),
    ];
    ensure_blank_line(&mut lines);
    assert_eq!(lines.len(), 2, "should not add another blank line");
}

#[test]
fn ensure_blank_line_twice_collapses() {
    let mut lines = vec![Line::from("hello")];
    ensure_blank_line(&mut lines); // adds blank
    ensure_blank_line(&mut lines); // should collapse
    assert_eq!(lines.len(), 2);
}

#[test]
fn ensure_blank_line_collapses_whitespace_only() {
    // A line of indent-only spaces is visually blank even though it has
    // nonzero width — e.g. a nested list's after-margin rendered as a
    // continuation line inside an outer item.  The margin must collapse
    // into it rather than stacking a second blank row.
    let mut lines = vec![
        Line::from("hello"),
        Line::from(Span::styled("     ".to_string(), Style::default())),
    ];
    ensure_blank_line(&mut lines);
    assert_eq!(lines.len(), 2, "indented blank should collapse, not stack");
}

// ── list blank-line collapsing ────────────────────────────────────────

#[test]
fn list_items_compact_when_single_line() {
    // Single-line list items should not have blank lines between them.
    let result = markdown_lines("- alpha\n- beta\n- gamma", 80);
    let whole: String = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(whole.contains("• alpha"), "first item should render");
    assert!(whole.contains("• beta"), "second item should render");
    assert!(whole.contains("• gamma"), "third item should render");
    // No blank lines between single-line items.
    let blank_lines: Vec<bool> = result
        .array_windows::<2>()
        .map(|w| w[0].width() == 0 && w[1].width() > 0)
        .collect();
    assert_eq!(
        blank_lines.iter().filter(|&&b| b).count(),
        0,
        "single-line list items should have no blank lines between them\n{whole}"
    );
}

#[test]
fn list_stays_tight_when_minority_wraps() {
    // A single wrapping item in a two-item list is not a majority, so the
    // list stays tight: no blank line between the items.
    let long = "a".repeat(60);
    let md = format!("- {long}\n- short");
    let result = markdown_lines(&md, 40);
    // The long item wraps to multiple visual lines, but it is only 1 of 2
    // items (1 * 2 is not > 2), so no blank line before "• short".
    let short_idx = result.iter().position(|l| l.to_string().contains("short"));
    assert!(short_idx.is_some(), "second item should appear");
    let idx = short_idx.unwrap();
    assert!(
        idx >= 1 && result[idx - 1].width() > 0,
        "expected no blank line before '• short' (minority wraps), got lines[{}]='{}'",
        idx - 1,
        result[idx - 1]
    );
}

#[test]
fn list_spaces_all_items_when_majority_wraps() {
    // Two of three items wrap at this width — a majority — so every item
    // pair is separated by a blank line, including before the short one.
    let long = "a".repeat(60);
    let md = format!("- {long}\n- short\n- {long}");
    let result = markdown_lines(&md, 40);
    let whole: String = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // Blank line before the short middle item.
    let short_idx = result.iter().position(|l| l.to_string().contains("short"));
    assert!(short_idx.is_some(), "short item should appear");
    let idx = short_idx.unwrap();
    assert!(
        idx >= 1 && result[idx - 1].width() == 0,
        "expected blank line before '• short' (majority wraps), got lines[{}]='{}'",
        idx - 1,
        result[idx - 1]
    );
    // No consecutive blank lines anywhere.
    let has_double_blank = result
        .array_windows::<2>()
        .any(|w| w[0].width() == 0 && w[1].width() == 0);
    assert!(
        !has_double_blank,
        "should not have two consecutive blank lines\n{whole}"
    );
}

#[test]
fn ordered_list_spaces_all_items_when_majority_wraps() {
    let long = "b".repeat(60);
    let md = format!("1. {long}\n2. short\n3. {long}");
    let result = markdown_lines(&md, 40);
    let idx = result.iter().position(|l| l.to_string().contains("short"));
    assert!(idx.is_some(), "short ordered item should appear");
    let idx = idx.unwrap();
    assert!(
        idx >= 1 && result[idx - 1].width() == 0,
        "expected blank line before '2. short' (majority wraps), got lines[{}]='{}'",
        idx - 1,
        result[idx - 1]
    );
}

#[test]
fn even_split_stays_tight() {
    // Four items, exactly two wrap: half is not a majority (> half), so
    // the list stays tight.
    let long = "c".repeat(60);
    let md = format!("- {long}\n- {long}\n- short1\n- short2");
    let result = markdown_lines(&md, 40);
    let blank_lines: Vec<bool> = result
        .array_windows::<2>()
        .map(|w| w[0].width() == 0 && w[1].width() > 0)
        .collect();
    assert_eq!(
        blank_lines.iter().filter(|&&b| b).count(),
        0,
        "an even 2:2 wrap split is not a majority, so no blank lines between items"
    );
}

#[test]
fn list_has_blank_line_before_and_after() {
    // Regardless of tight/spaced, the list is separated from surrounding
    // paragraphs by a blank line on each side.
    let md = "before\n- one\n- two\n- three\n\nafter";
    let result = markdown_lines(md, 80);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    let one = text.iter().position(|l| l.contains("• one")).unwrap();
    let after = text.iter().position(|l| l.contains("after")).unwrap();
    // One blank line before the list, directly after the preceding paragraph.
    assert_eq!(text[one - 1], "", "blank line before the list");
    assert_eq!(text[one - 2], "before", "preceding paragraph");
    // One blank line after the list, directly before the following paragraph.
    assert_eq!(text[after - 1], "", "blank line after the list");
    assert_eq!(text[after - 2], "• three", "last list item");
}

#[test]
fn nested_list_makes_own_spacing_decision() {
    // The outer list has 3 items, two of which contain a nested list
    // (multi-line) — a majority — so outer items are spaced apart.
    // The inner lists are all single-line items, so they stay compact.
    let md = "- outer a\n  - inner\n  - inner2\n- outer b\n  - inner\n  - inner2\n- outer c";
    let result = markdown_lines(md, 80);
    let whole: String = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(whole.contains("• outer a"), "first outer item");
    assert!(whole.contains("• outer c"), "third outer item");
    // Inner items compact: no blank between "• inner" and "• inner2".
    let inner2_idx = result.iter().position(|l| l.to_string().contains("inner2"));
    assert!(inner2_idx.is_some(), "inner2 should appear");
    let i = inner2_idx.unwrap();
    assert!(
        i >= 1 && result[i - 1].width() > 0,
        "inner items should be compact (no blank before inner2)"
    );
    // Outer items spaced: blank line before "• outer b".  The blank is
    // the nested list's after-margin rendered as an indented
    // whitespace-only line, so it is visually blank but has nonzero
    // width.
    let outer_b_idx = result
        .iter()
        .position(|l| l.to_string().contains("outer b"));
    assert!(outer_b_idx.is_some(), "outer b should appear");
    let b = outer_b_idx.unwrap();
    assert!(
        b >= 1 && result[b - 1].to_string().trim().is_empty(),
        "expected blank line before '• outer b', got lines[{}]='{}'",
        b - 1,
        result[b - 1]
    );
    // No consecutive blank lines anywhere (visually blank includes
    // indented whitespace-only lines).
    let has_double_blank = result
        .array_windows::<2>()
        .any(|w| w[0].to_string().trim().is_empty() && w[1].to_string().trim().is_empty());
    assert!(
        !has_double_blank,
        "should not have two consecutive blank lines\n{whole}"
    );
}

#[test]
fn nested_list_gets_blank_line_before_next_item() {
    // A tight ordered list where one item contains a nested bullet list:
    // the next sibling's marker must not run directly against the nested
    // list's last line (the originally reported bug).  Every list is
    // delimited by a collapsing margin after it, so the boundary after
    // the nested list is separated while single-line items stay tight.
    // Raw string so the bullet indentation survives into the parser.
    let md = r"1. What the model is (context for sizing)
2. The fundamental requirement: ~150-600 GB of memory depending on quantization
3. Options table:
      - Budget/self-host small: 2× DGX Spark (~$8K one-time)
      - Mid: 4× RTX PRO 6000 / 8× A100 80GB
      - Production cloud: 8× H200 node
      - Extreme: 8× H100/H200 for FP16
4. Cloud monthly costs (table with providers)
5. On-prem purchase costs
6. Throughput expectations
7. Business reality check
8. Software stack: vLLM, FP8";
    let result = markdown_lines(md, 100);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    let whole = text.join("\n");
    // Blank line before item 4 (the sibling after the nested-list item).
    let idx4 = text
        .iter()
        .position(|l| l.contains("4. Cloud"))
        .expect("item 4");
    assert!(
        idx4 >= 1 && text[idx4 - 1].trim().is_empty(),
        "expected a blank line before '4. Cloud monthly costs', got lines[{}]='{}'\n{whole}",
        idx4 - 1,
        text[idx4 - 1]
    );
    // Items 2 and 3 stay tight — the margin delimits the list, it does
    // not space out the items themselves.
    let idx3 = text
        .iter()
        .position(|l| l.contains("3. Options table:"))
        .unwrap();
    assert_eq!(
        text[idx3 - 1],
        "2. The fundamental requirement: ~150-600 GB of memory depending on quantization",
        "items 2 and 3 should stay tight\n{whole}"
    );
    // No trailing blank line: the list margin at the end of the document
    // is stripped by markdown_lines.
    assert!(
        !text.last().is_some_and(|l| l.trim().is_empty()),
        "no trailing blank line after the list\n{whole}"
    );
    // No consecutive blank lines anywhere (visually blank includes
    // indented whitespace-only lines).
    let has_double_blank = result
        .array_windows::<2>()
        .any(|w| w[0].to_string().trim().is_empty() && w[1].to_string().trim().is_empty());
    assert!(!has_double_blank, "no consecutive blank lines\n{whole}");
}

#[test]
fn ordered_list_items_compact_when_single_line() {
    let result = markdown_lines("1. first\n2. second\n3. third", 80);
    let blank: Vec<bool> = result
        .array_windows::<2>()
        .map(|w| w[0].width() == 0 && w[1].width() > 0)
        .collect();
    assert_eq!(
        blank.iter().filter(|&&b| b).count(),
        0,
        "single-line ordered items should have no blank lines between them"
    );
}

#[test]
fn spaced_list_nested_list_single_blank_between_items() {
    // A spaced outer list (majority of items wrap) where an item contains
    // a nested list: the nested list's after-margin is an indented
    // whitespace-only line, and the between-item margin must collapse
    // into it — exactly one blank row, not two.  This is the reported
    // regression: the old ensure_blank_line only collapsed zero-width
    // lines and stacked a second blank after the indented one.
    let long = "a".repeat(60);
    let md =
        format!("1. {long} Options:\n      - inner one\n      - inner two\n2. {long}\n3. short");
    let result = markdown_lines(&md, 40);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    let whole = text.join("\n");
    let idx2 = text
        .iter()
        .position(|l| l.trim_start().starts_with("2. "))
        .expect("item 2");
    // Exactly one blank row between the nested list and item 2.
    assert!(
        text[idx2 - 1].trim().is_empty(),
        "expected a blank line before '2. '\n{whole}"
    );
    assert!(
        !text[idx2 - 2].trim().is_empty(),
        "expected exactly one blank line before '2. ', got two\n{whole}"
    );
    // No two consecutive visually-blank rows anywhere.
    let has_double_blank = text
        .array_windows::<2>()
        .any(|w| w[0].trim().is_empty() && w[1].trim().is_empty());
    assert!(!has_double_blank, "no two consecutive blank lines\n{whole}");
}

#[test]
fn list_ending_with_nested_list_has_no_trailing_blank() {
    // The document's last item ends with a nested list.  The nested
    // list's after-margin is an indented whitespace-only line and the
    // outer list's own after-margin collapses into it; that trailing
    // whitespace line must be stripped by markdown_lines just like a
    // zero-width blank.
    let md = "1. first\n2. outer\n      - inner\n      - inner2";
    let result = markdown_lines(md, 80);
    let text: Vec<String> = result.iter().map(ToString::to_string).collect();
    assert!(
        !text.last().is_some_and(|l| l.trim().is_empty()),
        "no trailing blank line\n{}",
        text.join("\n")
    );
}

#[test]
fn mixed_list_and_paragraph_separated_by_one_blank() {
    let md = "paragraph\n- list";
    let result = markdown_lines(md, 80);
    let blank: Vec<bool> = result
        .array_windows::<2>()
        .map(|w| w[0].width() == 0 && w[1].width() > 0)
        .collect();
    assert_eq!(
        blank.iter().filter(|&&b| b).count(),
        1,
        "one blank line between para and list"
    );
}
