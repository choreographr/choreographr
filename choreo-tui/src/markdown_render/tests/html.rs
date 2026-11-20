//! Raw HTML in the source: what reaches the screen.
//!
//! The parser keeps the text a reader would see and drops the markup, with two
//! exceptions that matter to the renderer: `<br>` becomes a hard line break, and
//! a `<script>`/`<style>` body goes with its tags. These tests assert the drawn
//! rows, because the point of the rule is what the reader sees — a literal
//! `<br>` in a price table is a rendering defect, not a parser detail.

use super::super::*;

/// Render `markdown` and return each row as a string.
fn rows(markdown: &str, width: u16) -> Vec<String> {
    markdown_lines(markdown, width)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn a_hard_break_tag_breaks_the_line() {
    assert_eq!(rows("a<br>b", 40), ["a", "b"]);
}

#[test]
fn a_table_cell_hard_break_tag_breaks_inside_the_cell() {
    // The shape a model writes to put two lines in one GFM cell — the only way a
    // cell can hold a break. The row must keep its frame and grow to two lines,
    // with the sibling cell padded rather than misaligned.
    let rendered = rows(
        "| Function | Original quote |\n|:--|:--|\n| Switching | 8-port PoE 60 W<br>16-port non-PoE |",
        60,
    );
    assert_eq!(
        rendered,
        [
            "╭───────────┬─────────────────╮",
            "│ Function  │ Original quote  │",
            "├───────────┼─────────────────┤",
            "│ Switching │ 8-port PoE 60 W │",
            "│           │ 16-port non-PoE │",
            "╰───────────┴─────────────────╯",
        ]
    );
}

#[test]
fn html_tags_are_dropped_and_their_text_survives() {
    // A terminal cannot show `<b>`, and showing its characters is noise.
    assert_eq!(rows("a<b>bold</b>c", 40), ["aboldc"]);
}

#[test]
fn an_html_block_keeps_the_text_it_wrapped() {
    // A `<div>` wrapper used to render as a blank row: the tags were dropped and
    // the text inside them with them.
    assert_eq!(rows("<div>block</div>", 40), ["block"]);
}

#[test]
fn a_br_inside_an_html_block_breaks_the_line() {
    // `<br>` means the same inside a block element as inline: without this the
    // tags were dropped and the surrounding words glued into "ab".
    assert_eq!(rows("<div>a<br>b</div>", 40), ["a", "b"]);
}

#[test]
fn a_script_body_is_dropped_whole() {
    assert_eq!(rows("a <script>alert(1)</script> b", 40), ["a b"]);
    // Nothing survives the block but the renderer's blank line for an empty
    // document — the script's own text is gone.
    assert_eq!(rows("<script>\nalert(1)\n</script>", 40), [""]);
}
