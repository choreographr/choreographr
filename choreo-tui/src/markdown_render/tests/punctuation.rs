use super::super::*;

// ── source adjacency between inline nodes ────────────────────────────
//
// Whether a space separates two inline nodes is decided by the whitespace the
// *source* had between them, never by what the neighbouring characters look
// like. A gap lives either in the following text node's leading whitespace or
// in the preceding node's trailing whitespace, and only those make a space.
// (Punctuation therefore needs no special case: `(**hi**)` and `( **hi** )` are
// both answered by the source's own spacing.)
//
// The assertions below compare the *whole* rendered row, not a substring of the
// rows concatenated together: a `contains` check cannot tell a space that is
// genuinely there from one that vanished across a row boundary, nor a correct
// render from one with a stray leading or trailing space.

/// Render one markdown paragraph and return its single rendered row's text.
fn row(markdown: &str) -> String {
    let lines = markdown_lines(markdown, 80);
    assert_eq!(
        lines.len(),
        1,
        "expected one row for {markdown:?}: {lines:?}"
    );
    lines[0].to_string()
}

#[test]
fn styled_text_glued_to_words_stays_glued() {
    // `a**b**c` has no whitespace anywhere, so nothing gets a space.
    assert_eq!(row("a**b**c"), "abc");
}

#[test]
fn code_span_glued_to_a_plus_sign_stays_glued() {
    // The case that reads wrong in a table: `(+`num-traits`)` must not become
    // `(+ num-traits`.
    assert_eq!(
        row("rmp (+`num-traits`, already present)"),
        "rmp (+num-traits, already present)"
    );
}

#[test]
fn code_span_glued_after_a_word_stays_glued() {
    assert_eq!(row("foo`bar`"), "foobar");
}

#[test]
fn code_span_keeps_the_source_spaces_around_it() {
    assert_eq!(row("a `b` c"), "a b c");
}

#[test]
fn adjacent_styled_spans_separated_by_source_whitespace_keep_one_space() {
    // `**a** **b**` puts a whitespace-only text node between two styled
    // containers: that node *is* the gap, and it must survive as one space.
    assert_eq!(row("**a** **b**"), "a b");
}

#[test]
fn multiple_spaces_in_the_source_collapse_to_one() {
    // Markdown collapses runs of whitespace; the renderer must not turn a
    // three-space gap into three columns.
    assert_eq!(row("a   **b**    c"), "a b c");
}

#[test]
fn unicode_whitespace_on_a_node_boundary_is_a_gap() {
    // `split_whitespace` already collapses any Unicode whitespace *inside* a
    // text node, so the leading/trailing tests must use the same class: a
    // non-breaking space after a styled span must separate, not glue.
    assert_eq!(row("**a**\u{a0}b"), "a b");
    assert_eq!(row("a**b**\u{2009}c"), "ab c");
}

#[test]
fn image_after_a_word_keeps_the_source_space() {
    // The rendered `[image: …]` marker sits on a word boundary like any other
    // inline, so the source's space before the image is preserved.
    assert_eq!(
        row("see ![alt](u.png) here"),
        "see [image: alt] (u.png) here"
    );
}

#[test]
fn image_alt_starting_with_whitespace_does_not_double_the_prefix_space() {
    // The `[image: ` prefix already ends in a space, so leading whitespace in
    // the alt text must not add a second one — mid-line or at a row's start.
    assert_eq!(row("x ![ alt](u.png) y"), "x [image: alt] (u.png) y");
    assert_eq!(row("![ alt](u.png)"), "[image: alt] (u.png)");
}

#[test]
fn image_glued_to_a_word_stays_glued() {
    assert_eq!(row("x![](u.png)"), "x[image: ] (u.png)");
}

#[test]
fn link_destination_and_following_text_follow_the_source_spacing() {
    assert_eq!(row("see [a](u) b"), "see a - u b");
}

#[test]
fn link_glued_to_the_following_word_stays_glued() {
    // No whitespace in the source after the link, so none in the render.
    assert_eq!(row("[a](u)b"), "a - ub");
}

#[test]
fn wrapped_rows_never_open_with_an_inserted_space() {
    // A gap belongs *between* two nodes on a row, never at the start of the row
    // a wrap just opened.
    let rows: Vec<String> = markdown_lines("alpha **beta** gamma `delta`", 10)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(rows.len() > 1, "expected a wrapped paragraph: {rows:?}");
    for rendered in &rows {
        assert!(
            !rendered.starts_with(' ') && !rendered.ends_with(' '),
            "row {rendered:?} has a stray edge space: {rows:?}"
        );
    }
}

// ── punctuation attachment (closing punct after styled text) ──────────
//
// These are not special-cased any more: they hold because the source has no
// whitespace on that side.

#[test]
fn bold_with_exclamation_no_extra_space() {
    assert_eq!(row("hello **bold**!"), "hello bold!");
}

#[test]
fn italic_with_period_no_extra_space() {
    assert_eq!(row("I said *italic*."), "I said italic.");
}

#[test]
fn strong_and_link_with_comma_no_extra_space() {
    assert_eq!(
        row("see **bold**, and [link](http://x.com)."),
        "see bold, and link - http://x.com."
    );
}

#[test]
fn closing_punct_after_strikethrough() {
    assert_eq!(row("done ~~strike~~!"), "done strike!");
}

#[test]
fn opening_bracket_keeps_space() {
    assert_eq!(row("word (paren)"), "word (paren)");
}

// ── opening punct attachment (styled text after opening bracket) ──────

#[test]
fn bold_after_opening_paren_no_extra_space() {
    assert_eq!(row("(**hi**)"), "(hi)");
}

#[test]
fn styled_text_after_opening_paren_in_sentence() {
    assert_eq!(row("a (**hi**) b"), "a (hi) b");
}

#[test]
fn emphasis_after_opening_paren_no_extra_space() {
    assert_eq!(row("(*hi*)"), "(hi)");
}

#[test]
fn code_after_opening_paren_no_extra_space() {
    assert_eq!(row("(`hi`)"), "(hi)");
}

#[test]
fn bold_after_opening_quote_no_extra_space() {
    // Smart punctuation turns "**hi**" into “**hi**”, which splits into
    // Text(“), Strong(hi), Text(”). The opening quote must not get a space.
    assert_eq!(row("\"**hi**\""), "\u{201c}hi\u{201d}");
}

#[test]
fn multiple_styled_parentheses_no_extra_space() {
    assert_eq!(row("(**hi**) and (**there**)"), "(hi) and (there)");
}

#[test]
fn spaced_brackets_keep_spaces() {
    // A literal space between bracket and styled text must be preserved.
    assert_eq!(row("( **hi** )"), "( hi )");
}

#[test]
fn styled_paren_after_word_keeps_space_before_bracket() {
    // The space before the bracket is kept; only the space after it is removed.
    assert_eq!(row("word (**hi**)"), "word (hi)");
}
