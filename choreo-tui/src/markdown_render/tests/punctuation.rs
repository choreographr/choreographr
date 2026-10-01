use super::super::*;

// ── starts_with_closing_punct ────────────────────────────────────────

#[test]
fn starts_with_closing_punct_period() {
    assert!(starts_with_closing_punct("."));
    assert!(starts_with_closing_punct("..."));
    assert!(starts_with_closing_punct(".not"));
}

#[test]
fn starts_with_closing_punct_comma() {
    assert!(starts_with_closing_punct(","));
    assert!(starts_with_closing_punct(", "));
}

#[test]
fn starts_with_closing_punct_exclamation() {
    assert!(starts_with_closing_punct("!"));
    assert!(starts_with_closing_punct("!important"));
}

#[test]
fn starts_with_closing_punct_question() {
    assert!(starts_with_closing_punct("?"));
    assert!(starts_with_closing_punct("? "));
}

#[test]
fn starts_with_closing_punct_colon_semicolon() {
    assert!(starts_with_closing_punct(":"));
    assert!(starts_with_closing_punct(";"));
}

#[test]
fn starts_with_closing_punct_brackets() {
    assert!(starts_with_closing_punct(")"));
    assert!(starts_with_closing_punct("]"));
    assert!(starts_with_closing_punct("}"));
}

#[test]
fn starts_with_closing_punct_unicode_quotes() {
    assert!(starts_with_closing_punct("\u{2019}")); // right single quote
    assert!(starts_with_closing_punct("\u{201d}")); // right double quote
}

#[test]
fn starts_with_closing_punct_non_closing_chars() {
    assert!(!starts_with_closing_punct("hello"));
    assert!(!starts_with_closing_punct(""));
    assert!(!starts_with_closing_punct("("));
    assert!(!starts_with_closing_punct("["));
    assert!(!starts_with_closing_punct("{"));
    assert!(!starts_with_closing_punct("\u{2018}")); // left single quote
    assert!(!starts_with_closing_punct("\u{201c}")); // left double quote
}

// ── ends_with_opening_punct ──────────────────────────────────────────

#[test]
fn ends_with_opening_punct_brackets() {
    assert!(ends_with_opening_punct("("));
    assert!(ends_with_opening_punct("(("));
    assert!(ends_with_opening_punct("word ("));
    assert!(ends_with_opening_punct("["));
    assert!(ends_with_opening_punct("{"));
}

#[test]
fn ends_with_opening_punct_unicode_quotes() {
    assert!(ends_with_opening_punct("\u{2018}")); // left single quote
    assert!(ends_with_opening_punct("\u{201c}")); // left double quote
    assert!(ends_with_opening_punct("said \u{201c}"));
}

#[test]
fn ends_with_opening_punct_non_opening_chars() {
    assert!(!ends_with_opening_punct(""));
    assert!(!ends_with_opening_punct("hello"));
    assert!(!ends_with_opening_punct(")"));
    // A trailing space means the source had a gap; the next inline must
    // stay separated, so this must NOT count as ending with a bracket.
    assert!(!ends_with_opening_punct("( "));
    assert!(!ends_with_opening_punct("\u{201d}")); // right double quote
    assert!(!ends_with_opening_punct("\u{2019}")); // right single quote
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
