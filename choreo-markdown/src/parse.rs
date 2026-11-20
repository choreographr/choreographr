//! The pulldown-cmark event → AST parser and its block/inline context stack.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::math::normalize_math_event;
use crate::raw_html::{is_hard_break_tag, scripting_tag, strip_html_block};
use crate::{ListItem, MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};

#[derive(Debug)]
enum BlockContext {
    Quote(Vec<MarkdownBlock>),
    List {
        ordered: bool,
        start: usize,
        items: Vec<ListItem>,
    },
    Item {
        number: Option<usize>,
        blocks: Vec<MarkdownBlock>,
    },
    Paragraph(Vec<MarkdownInline>),
    Heading {
        level: u8,
        content: Vec<MarkdownInline>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Table {
        alignments: Vec<MarkdownAlignment>,
        header: Vec<Vec<MarkdownInline>>,
        rows: Vec<Vec<Vec<MarkdownInline>>>,
        in_header: bool,
    },
    TableRow(Vec<Vec<MarkdownInline>>),
    TableCell(Vec<MarkdownInline>),
}

#[derive(Debug)]
enum InlineContext {
    Emphasis(Vec<MarkdownInline>),
    Strong(Vec<MarkdownInline>),
    Strikethrough(Vec<MarkdownInline>),
    Link {
        destination: String,
        content: Vec<MarkdownInline>,
    },
    Image {
        destination: String,
        alt: Vec<MarkdownInline>,
    },
}

impl MarkdownDocument {
    /// Parse a raw markdown string into an AST.
    ///
    /// Uses `pulldown-cmark` with a curated set of extensions enabled.
    /// The returned document can be inspected, modified, and re-serialized.
    pub fn parse(input: &str) -> Self {
        let parser = Parser::new_ext(input, markdown_options())
            .into_offset_iter()
            .map(|(event, range)| (normalize_math_event(event), range));
        let mut blocks = Vec::new();
        let mut block_stack = Vec::<BlockContext>::new();
        let mut inline_stack = Vec::<InlineContext>::new();
        // Set while an inline `<script>`/`<style>` element is open, so the body
        // that follows is dropped along with the tags that opened it. The state
        // is cleared at every `End` event, which bounds the suppression to the
        // block that opened it: an inline script whose close tag is missing only
        // drops the rest of its own paragraph, never the enclosing document. (A
        // *block-level* `<script>` is different — pulldown's HTML block runs to
        // `</script>` or the end of input, so an unterminated one drops to the
        // end of input, the same extent HTML itself would consume.)
        let mut in_script_body = false;
        // What an HTML block has contributed so far, as inline nodes. pulldown
        // delivers a block as `Start(HtmlBlock)` + one `Html` event per line +
        // `End(HtmlBlock)`, so the kept text is gathered here and emitted as a
        // single paragraph when the block closes.
        let mut html_block: Option<Vec<MarkdownInline>> = None;

        for (event, range) in parser {
            // An unterminated `<script>` must not swallow more than its own
            // block: clearing the flag at every `End` bounds an inline script to
            // the paragraph that opened it.
            if matches!(&event, Event::End(_)) {
                in_script_body = false;
            }
            match event {
                Event::Start(tag) => match tag {
                    Tag::Paragraph => block_stack.push(BlockContext::Paragraph(Vec::new())),
                    Tag::Heading { level, .. } => block_stack.push(BlockContext::Heading {
                        level: heading_level(level),
                        content: Vec::new(),
                    }),
                    Tag::BlockQuote(_) => block_stack.push(BlockContext::Quote(Vec::new())),
                    Tag::List(start) => block_stack.push(BlockContext::List {
                        ordered: start.is_some(),
                        start: start
                            .and_then(|value| usize::try_from(value).ok())
                            .unwrap_or(1),
                        items: Vec::new(),
                    }),
                    Tag::Item => {
                        // pulldown-cmark reports only the list start, so the
                        // item's own marker number is recovered from its source
                        // range (see `item_marker_number`).
                        let number = item_marker_number(input, &range);
                        block_stack.push(BlockContext::Item {
                            number,
                            blocks: Vec::new(),
                        });
                    }
                    Tag::CodeBlock(kind) => block_stack.push(BlockContext::CodeBlock {
                        language: match kind {
                            CodeBlockKind::Indented => None,
                            CodeBlockKind::Fenced(language) => {
                                let language = language.trim().to_string();
                                (!language.is_empty()).then_some(language)
                            }
                        },
                        code: String::new(),
                    }),
                    Tag::Table(alignments) => block_stack.push(BlockContext::Table {
                        alignments: alignments.into_iter().map(markdown_alignment).collect(),
                        header: Vec::new(),
                        rows: Vec::new(),
                        in_header: false,
                    }),
                    Tag::TableHead => {
                        if let Some(BlockContext::Table { in_header, .. }) = block_stack.last_mut()
                        {
                            *in_header = true;
                        }
                    }
                    Tag::TableRow => block_stack.push(BlockContext::TableRow(Vec::new())),
                    Tag::TableCell => block_stack.push(BlockContext::TableCell(Vec::new())),
                    Tag::HtmlBlock => html_block = Some(Vec::new()),
                    Tag::Emphasis => inline_stack.push(InlineContext::Emphasis(Vec::new())),
                    Tag::Strong => inline_stack.push(InlineContext::Strong(Vec::new())),
                    Tag::Strikethrough => {
                        inline_stack.push(InlineContext::Strikethrough(Vec::new()));
                    }
                    Tag::Link { dest_url, .. } => inline_stack.push(InlineContext::Link {
                        destination: dest_url.to_string(),
                        content: Vec::new(),
                    }),
                    Tag::Image { dest_url, .. } => inline_stack.push(InlineContext::Image {
                        destination: dest_url.to_string(),
                        alt: Vec::new(),
                    }),
                    _ => {}
                },
                Event::End(tag) => match tag {
                    TagEnd::Paragraph => {
                        if let Some(BlockContext::Paragraph(content)) = block_stack.pop() {
                            push_block(
                                &mut blocks,
                                &mut block_stack,
                                MarkdownBlock::Paragraph(content),
                            );
                        }
                    }
                    TagEnd::Heading(_) => {
                        if let Some(BlockContext::Heading { level, content }) = block_stack.pop() {
                            push_block(
                                &mut blocks,
                                &mut block_stack,
                                MarkdownBlock::Heading { level, content },
                            );
                        }
                    }
                    TagEnd::BlockQuote(_) => {
                        if let Some(BlockContext::Quote(content)) = block_stack.pop() {
                            push_block(
                                &mut blocks,
                                &mut block_stack,
                                MarkdownBlock::BlockQuote(content),
                            );
                        }
                    }
                    TagEnd::List(_) => {
                        if let Some(BlockContext::List {
                            ordered,
                            start,
                            items,
                        }) = block_stack.pop()
                        {
                            push_block(
                                &mut blocks,
                                &mut block_stack,
                                MarkdownBlock::List {
                                    ordered,
                                    start,
                                    items,
                                },
                            );
                        }
                    }
                    TagEnd::Item => {
                        if let Some(BlockContext::Item { number, blocks }) = block_stack.pop()
                            && let Some(BlockContext::List { items, .. }) = block_stack.last_mut()
                        {
                            items.push(ListItem { number, blocks });
                        }
                    }
                    TagEnd::CodeBlock => {
                        if let Some(BlockContext::CodeBlock { language, code }) = block_stack.pop()
                        {
                            push_block(
                                &mut blocks,
                                &mut block_stack,
                                MarkdownBlock::CodeBlock { language, code },
                            );
                        }
                    }
                    TagEnd::Table => {
                        if let Some(BlockContext::Table {
                            alignments,
                            header,
                            rows,
                            ..
                        }) = block_stack.pop()
                        {
                            push_block(
                                &mut blocks,
                                &mut block_stack,
                                MarkdownBlock::Table {
                                    alignments,
                                    header,
                                    rows,
                                },
                            );
                        }
                    }
                    TagEnd::TableHead => {
                        if let Some(BlockContext::Table { in_header, .. }) = block_stack.last_mut()
                        {
                            *in_header = false;
                        }
                    }
                    TagEnd::TableRow => {
                        if let Some(BlockContext::TableRow(row)) = block_stack.pop()
                            && let Some(BlockContext::Table {
                                header,
                                rows,
                                in_header,
                                ..
                            }) = block_stack.last_mut()
                        {
                            if *in_header {
                                *header = row;
                            } else {
                                rows.push(row);
                            }
                        }
                    }
                    TagEnd::TableCell => {
                        if let Some(BlockContext::TableCell(cell)) = block_stack.pop() {
                            // Header cells in pulldown-cmark sit directly under
                            // TableHead without a wrapping TableRow, so fall back
                            // to pushing directly into the table's header row.
                            if let Some(BlockContext::TableRow(row)) = block_stack.last_mut() {
                                row.push(cell);
                            } else if let Some(BlockContext::Table { header, .. }) =
                                block_stack.last_mut()
                            {
                                header.push(cell);
                            }
                        }
                    }
                    TagEnd::HtmlBlock => {
                        // Emit the block's kept content as one paragraph, wherever
                        // the block sits (root, a quote, a list item).
                        if let Some(buffer) = html_block.take() {
                            let run = trim_inline_run(buffer);
                            if !run.is_empty()
                                && !push_inline_run(&mut block_stack, &mut inline_stack, &run)
                            {
                                // No paragraph-bearing context is open around the
                                // block, so the text its tags wrapped keeps a
                                // paragraph of its own rather than vanishing with
                                // them.
                                push_block(
                                    &mut blocks,
                                    &mut block_stack,
                                    MarkdownBlock::Paragraph(run),
                                );
                            }
                        }
                    }
                    TagEnd::Emphasis => {
                        if let Some(InlineContext::Emphasis(content)) = inline_stack.pop() {
                            push_inline(
                                &mut block_stack,
                                &mut inline_stack,
                                MarkdownInline::Emphasis(content),
                            );
                        }
                    }
                    TagEnd::Strong => {
                        if let Some(InlineContext::Strong(content)) = inline_stack.pop() {
                            push_inline(
                                &mut block_stack,
                                &mut inline_stack,
                                MarkdownInline::Strong(content),
                            );
                        }
                    }
                    TagEnd::Strikethrough => {
                        if let Some(InlineContext::Strikethrough(content)) = inline_stack.pop() {
                            push_inline(
                                &mut block_stack,
                                &mut inline_stack,
                                MarkdownInline::Strikethrough(content),
                            );
                        }
                    }
                    TagEnd::Link => {
                        if let Some(InlineContext::Link {
                            destination,
                            content,
                        }) = inline_stack.pop()
                        {
                            push_inline(
                                &mut block_stack,
                                &mut inline_stack,
                                MarkdownInline::Link {
                                    content,
                                    destination,
                                },
                            );
                        }
                    }
                    TagEnd::Image => {
                        if let Some(InlineContext::Image { destination, alt }) = inline_stack.pop()
                        {
                            push_inline(
                                &mut block_stack,
                                &mut inline_stack,
                                MarkdownInline::Image { alt, destination },
                            );
                        }
                    }
                    _ => {}
                },
                Event::Text(text) => {
                    // The body of an inline `<script>`/`<style>` is code, not prose:
                    // it is dropped with the tags that opened it.
                    if !in_script_body {
                        push_text(&mut block_stack, &mut inline_stack, &text);
                    }
                }
                Event::InlineHtml(text) => {
                    if in_script_body {
                        // Inside a dropped `<script>`/`<style>` body only the
                        // matching close tag ends the suppression; a `<br>` or any
                        // other tag there is code, not prose, and stays dropped.
                        if let Some(true) = scripting_tag(&text) {
                            in_script_body = false;
                        }
                    } else if is_hard_break_tag(&text) {
                        // `<br>` is the one raw tag with a textual meaning — a GFM
                        // table cell can only break a line this way — so it becomes
                        // the same `LineBreak` a source hard break produces.
                        push_inline(
                            &mut block_stack,
                            &mut inline_stack,
                            MarkdownInline::LineBreak,
                        );
                    } else if let Some(closing) = scripting_tag(&text) {
                        in_script_body = !closing;
                    }
                    // Every other tag is markup a terminal cannot show: it is
                    // dropped, leaving the text events around it untouched.
                }
                Event::Html(text) => {
                    // One line of a raw HTML block: its tags are stripped and the
                    // content they wrapped is gathered for the block's paragraph.
                    let run = strip_html_block(&text, &mut in_script_body);
                    if let Some(buffer) = html_block.as_mut() {
                        extend_inlines(buffer, &run);
                    } else {
                        // `Event::Html` belongs to an HTML block, so this is
                        // unreachable in practice; routing it as inline content is
                        // the safe direction if a future parser sends one outside.
                        let run = trim_inline_run(run);
                        push_inline_run(&mut block_stack, &mut inline_stack, &run);
                    }
                }
                Event::Code(text) => push_inline(
                    &mut block_stack,
                    &mut inline_stack,
                    MarkdownInline::Code(text.to_string()),
                ),
                Event::SoftBreak | Event::HardBreak => push_inline(
                    &mut block_stack,
                    &mut inline_stack,
                    MarkdownInline::LineBreak,
                ),
                Event::Rule => push_block(&mut blocks, &mut block_stack, MarkdownBlock::Rule),
                Event::InlineMath(text) => push_inline(
                    &mut block_stack,
                    &mut inline_stack,
                    MarkdownInline::InlineMath(text.to_string()),
                ),
                Event::DisplayMath(text) => push_inline(
                    &mut block_stack,
                    &mut inline_stack,
                    MarkdownInline::DisplayMath(text.to_string()),
                ),
                Event::FootnoteReference(text) => {
                    push_text(&mut block_stack, &mut inline_stack, &format!("[{text}]"));
                }
                Event::TaskListMarker(checked) => {
                    push_text(
                        &mut block_stack,
                        &mut inline_stack,
                        if checked { "[x] " } else { "[ ] " },
                    );
                }
            }
        }

        Self { blocks }
    }
}

pub(crate) fn markdown_options() -> Options {
    // Enable a curated set of extensions that we handle explicitly in the AST.
    // Features like YAML metadata blocks, plus-delimited metadata, and definition
    // lists are excluded because they either have no structural representation in
    // our AST or are rare in the primary use case (LLM-generated markdown).
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION
        | Options::ENABLE_MATH
        | Options::ENABLE_HEADING_ATTRIBUTES
}

fn markdown_alignment(alignment: Alignment) -> MarkdownAlignment {
    match alignment {
        Alignment::None => MarkdownAlignment::None,
        Alignment::Left => MarkdownAlignment::Left,
        Alignment::Center => MarkdownAlignment::Center,
        Alignment::Right => MarkdownAlignment::Right,
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn push_block(root: &mut Vec<MarkdownBlock>, stack: &mut [BlockContext], block: MarkdownBlock) {
    match stack.last_mut() {
        Some(BlockContext::Quote(blocks) | BlockContext::Item { blocks, .. }) => blocks.push(block),
        _ => root.push(block),
    }
}

/// Recover the literal ordered-list number from an item's source range.
///
/// pulldown-cmark reports only a list's start number and drops every item's own
/// marker, so the item's source range — whose start is the marker's line prefix
/// (horizontal whitespace only) — is the only place the written number survives.
/// Skipping that whitespace and reading the leading ASCII digits recovers the
/// number; an unordered marker (`-`/`*`/`+`) or any non-numeric first token
/// yields `None`.  Requiring a `.` or `)` delimiter after the digits rejects a
/// stray in-prose number that happens to begin a line.
fn item_marker_number(input: &str, range: &Range<usize>) -> Option<usize> {
    let slice = input.get(range.start..range.end)?;
    let marker = slice.trim_start_matches([' ', '\t']);
    let digits_len = marker
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits_len == 0
        || !matches!(
            marker.as_bytes().get(digits_len).copied(),
            Some(b'.' | b')')
        )
    {
        return None;
    }
    marker.get(..digits_len)?.parse().ok()
}

/// Route a text slice into the active context, merging with a neighbouring
/// `Text` node where possible.
fn push_text(block_stack: &mut [BlockContext], inline_stack: &mut [InlineContext], text: &str) {
    if text.is_empty() {
        return;
    }

    // Route the text into the active context, trying to merge into an
    // existing Text node before allocating a new String.  This mirrors
    // the match arms in `push_inline` but uses `push_text_content`
    // instead of `push_inline_content` so we can pass &str directly.
    if let Some(context) = inline_stack.last_mut() {
        match context {
            InlineContext::Emphasis(content)
            | InlineContext::Strong(content)
            | InlineContext::Strikethrough(content)
            | InlineContext::Link { content, .. }
            | InlineContext::Image { alt: content, .. } => {
                push_text_content(content, text);
            }
        }
        return;
    }

    let Some(context) = block_stack.last_mut() else {
        return;
    };
    match context {
        BlockContext::Paragraph(content)
        | BlockContext::Heading { content, .. }
        | BlockContext::TableCell(content) => push_text_content(content, text),
        BlockContext::Item { blocks, .. } => {
            if !matches!(blocks.last(), Some(MarkdownBlock::Paragraph(_))) {
                blocks.push(MarkdownBlock::Paragraph(Vec::new()));
            }
            if let Some(MarkdownBlock::Paragraph(content)) = blocks.last_mut() {
                push_text_content(content, text);
            }
        }
        BlockContext::CodeBlock { code, .. } => code.push_str(text),
        BlockContext::Quote(_)
        | BlockContext::List { .. }
        | BlockContext::Table { .. }
        | BlockContext::TableRow(_) => {}
    }
}

/// Route a run of inline nodes (an HTML block's kept content) into the active
/// context, merging with a neighbouring `Text` node where possible.
///
/// Returns whether a context took the run. Only a paragraph-bearing block (or an
/// open inline span) can; a quote, list, table, or code block has nowhere to put
/// it, and that `false` is what tells the caller an HTML block's kept content
/// needs a paragraph of its own.
fn push_inline_run(
    block_stack: &mut [BlockContext],
    inline_stack: &mut [InlineContext],
    run: &[MarkdownInline],
) -> bool {
    if run.is_empty() {
        return false;
    }
    if let Some(context) = inline_stack.last_mut() {
        match context {
            InlineContext::Emphasis(content)
            | InlineContext::Strong(content)
            | InlineContext::Strikethrough(content)
            | InlineContext::Link { content, .. }
            | InlineContext::Image { alt: content, .. } => extend_inlines(content, run),
        }
        return true;
    }
    let Some(context) = block_stack.last_mut() else {
        return false;
    };
    let content = match context {
        BlockContext::Paragraph(content)
        | BlockContext::Heading { content, .. }
        | BlockContext::TableCell(content) => content,
        BlockContext::Item { blocks, .. } => {
            if !matches!(blocks.last(), Some(MarkdownBlock::Paragraph(_))) {
                blocks.push(MarkdownBlock::Paragraph(Vec::new()));
            }
            match blocks.last_mut() {
                Some(MarkdownBlock::Paragraph(content)) => content,
                _ => return false,
            }
        }
        // An HTML block cannot sit inside a code block, quote shell, list shell,
        // table shell, or table row; if it somehow does there is nowhere to put
        // it, so the caller falls back to a paragraph of its own.
        BlockContext::CodeBlock { .. }
        | BlockContext::Quote(_)
        | BlockContext::List { .. }
        | BlockContext::Table { .. }
        | BlockContext::TableRow(_) => return false,
    };
    extend_inlines(content, run);
    true
}

/// Append `run` to `content`, merging adjacent `Text` nodes.
fn extend_inlines(content: &mut Vec<MarkdownInline>, run: &[MarkdownInline]) {
    for inline in run {
        push_inline_content(content, inline.clone());
    }
}

/// Trim an HTML block's kept content: drop leading and trailing whitespace and
/// stray breaks so a block that opened or closed with a tag does not start or end
/// with a blank line.
fn trim_inline_run(mut run: Vec<MarkdownInline>) -> Vec<MarkdownInline> {
    // A node the block can shed at either end: a stray break, or text that is
    // only whitespace.
    fn boundary(node: &MarkdownInline) -> bool {
        match node {
            MarkdownInline::LineBreak => true,
            MarkdownInline::Text(text) => text.trim().is_empty(),
            _ => false,
        }
    }

    let mut start = 0;
    while run.get(start).is_some_and(boundary) {
        start += 1;
    }
    let mut end = run.len();
    while end > start && run.get(end - 1).is_some_and(boundary) {
        end -= 1;
    }
    // Trim the whitespace that survives on the boundary text nodes, then drop what
    // fell outside the kept range.
    if let Some(MarkdownInline::Text(text)) = run.get_mut(start) {
        *text = text.trim_start().to_string();
    }
    if end > start
        && let Some(MarkdownInline::Text(text)) = run.get_mut(end - 1)
    {
        *text = text.trim_end().to_string();
    }
    run.drain(end..);
    run.drain(..start);
    run
}

/// Push a text slice into a content vector, merging with the last element
/// if it is also a `Text` node — avoids allocating a new `String` when
/// we can extend the existing one.
pub(crate) fn push_text_content(content: &mut Vec<MarkdownInline>, text: &str) {
    if let Some(MarkdownInline::Text(last)) = content.last_mut() {
        last.push_str(text);
    } else {
        content.push(MarkdownInline::Text(text.to_string()));
    }
}

/// Push an inline node into a content vector, merging adjacent Text nodes
/// to avoid artifacts from pulldown-cmark's smart punctuation splitting
/// (e.g. `I'll` being split into `Text("I")`, `Text("'")`, `Text("ll")`).
fn push_inline_content(content: &mut Vec<MarkdownInline>, inline: MarkdownInline) {
    if let MarkdownInline::Text(text) = &inline
        && let Some(MarkdownInline::Text(last)) = content.last_mut()
    {
        last.push_str(text);
        return;
    }
    content.push(inline);
}

fn push_inline(
    block_stack: &mut [BlockContext],
    inline_stack: &mut [InlineContext],
    inline: MarkdownInline,
) {
    // If there's an open inline formatting context, push into that first.
    if let Some(context) = inline_stack.last_mut() {
        match context {
            InlineContext::Emphasis(content)
            | InlineContext::Strong(content)
            | InlineContext::Strikethrough(content)
            | InlineContext::Link { content, .. }
            | InlineContext::Image { alt: content, .. } => {
                push_inline_content(content, inline);
            }
        }
        return;
    }

    // Otherwise route the inline into the active block context.
    if let Some(context) = block_stack.last_mut() {
        match context {
            BlockContext::Paragraph(content)
            | BlockContext::Heading { content, .. }
            | BlockContext::TableCell(content) => push_inline_content(content, inline),
            BlockContext::Item { blocks, .. } => {
                // List items wrap inline content in a Paragraph block.
                // Ensure one exists so we have somewhere to push.
                if !matches!(blocks.last(), Some(MarkdownBlock::Paragraph(_))) {
                    blocks.push(MarkdownBlock::Paragraph(Vec::new()));
                }
                // At this point the last block is guaranteed to be a Paragraph.
                if let Some(MarkdownBlock::Paragraph(content)) = blocks.last_mut() {
                    push_inline_content(content, inline);
                }
            }
            BlockContext::CodeBlock { code, .. } => match inline {
                MarkdownInline::Text(text)
                | MarkdownInline::Code(text)
                | MarkdownInline::InlineMath(text)
                | MarkdownInline::DisplayMath(text) => code.push_str(&text),
                MarkdownInline::LineBreak => code.push('\n'),
                MarkdownInline::Strikethrough(content)
                | MarkdownInline::Emphasis(content)
                | MarkdownInline::Strong(content)
                | MarkdownInline::Link { content, .. }
                | MarkdownInline::Image { alt: content, .. } => {
                    code.push_str(&inline_text(&content));
                }
            },
            // Quote, List, Table, and TableRow contexts don't accept inlines directly.
            BlockContext::Quote(_)
            | BlockContext::List { .. }
            | BlockContext::Table { .. }
            | BlockContext::TableRow(_) => {}
        }
    }
}

/// Extract the plain text content from a sequence of inline nodes.
///
/// Recursively flattens all inline formatting (emphasis, links, etc.)
/// and returns only the raw text without any markdown delimiters.
#[must_use]
pub fn inline_text(inlines: &[MarkdownInline]) -> String {
    let mut text = String::new();
    for inline in inlines {
        match inline {
            MarkdownInline::Text(value)
            | MarkdownInline::Code(value)
            | MarkdownInline::InlineMath(value)
            | MarkdownInline::DisplayMath(value) => text.push_str(value),
            MarkdownInline::Strikethrough(content)
            | MarkdownInline::Emphasis(content)
            | MarkdownInline::Strong(content)
            | MarkdownInline::Link { content, .. }
            | MarkdownInline::Image { alt: content, .. } => text.push_str(&inline_text(content)),
            MarkdownInline::LineBreak => text.push('\n'),
        }
    }
    text
}
