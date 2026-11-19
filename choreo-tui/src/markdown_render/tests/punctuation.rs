use super::super::*;

// ── source adjacency between inline nodes ────────────────────────────
//
// Whether a space separates two inline nodes is decided by the whitespace the
// *source* had between them, never by what the neighbouring characters look
// like. A gap lives either in the following text node's leading whitespace or
// in the preceding node's trailing whitespace, and only those make a space.
// (Punctuation therefore needs no special case: `(**hi**)` and `( **hi** )` are
// both answered by the source's own spacing.)

#[test]
fn styled_text_glued_to_words_stays_glued() {
    // `a**b**c` has no whitespace anywhere, so nothing gets a space.
    let result = markdown_lines("a**b**c", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("abc"),
        "expected 'abc' with no inserted spaces, got: {whole:?}"
    );
}

#[test]
fn code_span_glued_to_a_plus_sign_stays_glued() {
    // The case that reads wrong in a table: `(+`num-traits`)` must not become
    // `(+ num-traits`.
    let result = markdown_lines("rmp (+`num-traits`, already present)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("(+num-traits,"),
        "expected '(+num-traits,' without an inserted space, got: {whole:?}"
    );
    assert!(
        !whole.contains("(+ num-traits"),
        "must not insert a space after '+': {whole:?}"
    );
}

#[test]
fn code_span_glued_after_a_word_stays_glued() {
    let result = markdown_lines("foo`bar`", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("foobar"),
        "expected 'foobar', got: {whole:?}"
    );
}

#[test]
fn adjacent_styled_spans_separated_by_source_whitespace_keep_one_space() {
    // `**a** **b**` puts a whitespace-only text node between two styled
    // containers: that node *is* the gap, and it must survive as one space.
    let result = markdown_lines("**a** **b**", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("a b"), "expected 'a b', got: {whole:?}");
    assert!(!whole.contains("ab"), "must not drop the gap: {whole:?}");
}

#[test]
fn image_after_a_word_keeps_the_source_space() {
    // The rendered `[image: …]` marker sits on a word boundary like any other
    // inline, so the source's space before the image is preserved.
    let result = markdown_lines("see ![alt](u.png) here", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("see [image: alt] (u.png) here"),
        "expected the image marker spaced on both sides, got: {whole:?}"
    );
    assert!(
        !whole.contains("see[image:"),
        "must not glue the marker to the preceding word: {whole:?}"
    );
}

#[test]
fn link_destination_and_following_text_follow_the_source_spacing() {
    let spaced = markdown_lines("see [a](u) b", 80);
    let spaced: String = spaced.iter().map(ToString::to_string).collect();
    assert!(
        spaced.contains("a - u b"),
        "expected 'a - u b', got: {spaced:?}"
    );
}

// ── punctuation attachment (closing punct after styled text) ──────────

#[test]
fn bold_with_exclamation_no_extra_space() {
    // "**bold**!" should render as "bold!", not "bold !"
    let result = markdown_lines("hello **bold**!", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("bold!"),
        "expected 'bold!' without space, got: {whole:?}"
    );
    assert!(
        !whole.contains("bold !"),
        "should not have space before '!'"
    );
    assert!(
        !whole.contains("**bold**"),
        "markdown syntax should not appear"
    );
}

#[test]
fn italic_with_period_no_extra_space() {
    let result = markdown_lines("I said *italic*.", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("italic."),
        "expected 'italic.' without space, got: {whole:?}"
    );
    assert!(
        !whole.contains("italic ."),
        "should not have space before '.'"
    );
}

#[test]
fn strong_and_link_with_comma_no_extra_space() {
    let result = markdown_lines("see **bold**, and [link](http://x.com).", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(whole.contains("bold,"), "expected 'bold,' without space");
    assert!(
        whole.contains("link - http://x.com."),
        "link content and trailing period"
    );
    assert!(
        !whole.contains("bold ,"),
        "should not have space before ','"
    );
}

#[test]
fn closing_punct_after_strikethrough() {
    let result = markdown_lines("done ~~strike~~!", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("strike!"),
        "expected 'strike!' without space"
    );
    assert!(
        !whole.contains("strike !"),
        "should not have space before '!'"
    );
}

#[test]
fn opening_bracket_keeps_space() {
    // Opening brackets should still get a space before them
    let result = markdown_lines("word (paren)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("word ("),
        "expected space before opening paren"
    );
}

// ── opening punct attachment (styled text after opening bracket) ──────

#[test]
fn bold_after_opening_paren_no_extra_space() {
    // "(**hi**)" should render as "(hi)", not "( hi)"
    let result = markdown_lines("(**hi**)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("(hi)"),
        "expected '(hi)' without space, got: {whole:?}"
    );
    assert!(
        !whole.contains("( hi"),
        "should not have space after opening bracket"
    );
    assert!(
        !whole.contains("**hi**"),
        "markdown syntax should not appear"
    );
}

#[test]
fn styled_text_after_opening_paren_in_sentence() {
    let result = markdown_lines("a (**hi**) b", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("a (hi) b"),
        "expected 'a (hi) b', got: {whole:?}"
    );
}

#[test]
fn emphasis_after_opening_paren_no_extra_space() {
    let result = markdown_lines("(*hi*)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("(hi)"),
        "expected '(hi)' without space, got: {whole:?}"
    );
}

#[test]
fn code_after_opening_paren_no_extra_space() {
    let result = markdown_lines("(`hi`)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("(hi)"),
        "expected '(hi)' without space, got: {whole:?}"
    );
}

#[test]
fn bold_after_opening_quote_no_extra_space() {
    // Smart punctuation turns "**hi**" into “**hi**”, which splits into
    // Text(“), Strong(hi), Text(”). The opening quote must not get a space.
    let result = markdown_lines("\"**hi**\"", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("\u{201c}hi\u{201d}"),
        "expected curly-quoted 'hi' without space, got: {whole:?}"
    );
    assert!(
        !whole.contains("\u{201c} hi"),
        "should not have space after opening quote"
    );
}

#[test]
fn multiple_styled_parentheses_no_extra_space() {
    let result = markdown_lines("(**hi**) and (**there**)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("(hi) and (there)"),
        "expected '(hi) and (there)', got: {whole:?}"
    );
}

#[test]
fn spaced_brackets_keep_spaces() {
    // A literal space between bracket and styled text must be preserved.
    let result = markdown_lines("( **hi** )", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("( hi )"),
        "expected '( hi )' with spaces preserved, got: {whole:?}"
    );
}

#[test]
fn styled_paren_after_word_keeps_space_before_bracket() {
    // The space before the bracket is kept; only the space after it is removed.
    let result = markdown_lines("word (**hi**)", 80);
    let whole: String = result.iter().map(ToString::to_string).collect();
    assert!(
        whole.contains("word (hi)"),
        "expected 'word (hi)', got: {whole:?}"
    );
    assert!(
        !whole.contains("word(hi)"),
        "space before opening bracket should remain"
    );
}
