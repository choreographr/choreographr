//! The pulldown-cmark event → AST parser and its block/inline context stack.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::math::normalize_math_event;
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
        // that follows is dropped along with the tags that opened it. `Event::Html`
        // streams an HTML block one line per event, so this state also carries
        // across an element that spans several of them.
        let mut in_script_body = false;
        // The text an HTML block has contributed so far. pulldown-cmark delivers a
        // block as `Start(HtmlBlock)` + one `Html` event per line + `End(HtmlBlock)`,
        // so the kept text is gathered here and emitted as a single paragraph when
        // the block closes.
        let mut html_block: Option<String> = None;

        for (event, range) in parser {
            // An unterminated `<script>` must not swallow the rest of the document:
            // the suppression is bounded to the block that opened it, so an
            // unbalanced tag can only ever show text it should have hidden, never
            // hide the text that follows it.
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
                    Tag::HtmlBlock => html_block = Some(String::new()),
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
                        // Emit the block's kept text as one paragraph, wherever the
                        // block sits (root, a quote, a list item).
                        if let Some(buffer) = html_block.take() {
                            let kept = buffer.trim();
                            if !kept.is_empty()
                                && !push_text(&mut block_stack, &mut inline_stack, kept)
                            {
                                // No paragraph is open around a block, so the text
                                // its tags wrapped keeps one of its own rather than
                                // vanishing with them.
                                push_block(
                                    &mut blocks,
                                    &mut block_stack,
                                    MarkdownBlock::Paragraph(vec![MarkdownInline::Text(
                                        kept.to_string(),
                                    )]),
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
                    if is_hard_break_tag(&text) {
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
                    // text they wrapped is gathered for the block's paragraph.
                    let kept = strip_html_block(&text, &mut in_script_body);
                    match html_block.as_mut() {
                        Some(buffer) => buffer.push_str(&kept),
                        // `Event::Html` belongs to an HTML block, so this is
                        // unreachable in practice; keeping the text is the safe
                        // direction if a future parser sends one outside.
                        None => {
                            push_text(&mut block_stack, &mut inline_stack, kept.trim());
                        }
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

// ── raw HTML ──────────────────────────────────────────────────────────────
//
// Raw HTML is never interpreted: a terminal renders text, so the parser keeps
// the text a reader would see and discards the markup. `<br>` is the exception —
// a GFM table cell can only break a line that way, so the tag becomes the same
// `LineBreak` a source hard break does. `<script>`/`<style>` lose their body as
// well, because what they hold is code rather than prose.

/// The element name of a raw tag (`<br>`, `</div>`, `<img src="x">`) and whether
/// it closes.
///
/// Returns `None` when the slice is not an element tag: a comment, a
/// `<!DOCTYPE>`, a processing instruction, or a stray `<`. Callers drop those as
/// markup, exactly like an element they do not recognise.
fn html_tag(text: &str) -> Option<(bool, &str)> {
    let body = text.strip_prefix('<')?;
    let (closing, body) = match body.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let name_len = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(body.len());
    let name = body.get(..name_len)?;
    // A name starts with a letter, which is what separates a tag from `<!` and
    // `<?` without special-casing either.
    name.starts_with(|c: char| c.is_ascii_alphabetic())
        .then_some((closing, name))
}

/// Whether `text` is a `<br>`, `<br/>`, or `<br />` tag — the only raw tag whose
/// meaning is a hard line break.
fn is_hard_break_tag(text: &str) -> bool {
    matches!(html_tag(text), Some((false, name)) if name.eq_ignore_ascii_case("br"))
}

/// Whether `text` opens (`Some(false)`) or closes (`Some(true)`) a
/// `<script>`/`<style>` element, whose body is dropped along with the tags.
fn scripting_tag(text: &str) -> Option<bool> {
    let (closing, name) = html_tag(text)?;
    let scripting = name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style");
    scripting.then_some(closing)
}

/// Reduce one `Event::Html` run to the text a terminal can show.
///
/// `dropping_script_body` carries the "inside a `<script>`/`<style>` body" state
/// in and out, because pulldown-cmark streams an HTML block one line per event
/// and an element can span several of them (`Html("<script>\n")`, then its body,
/// then `Html("</script>")`). A `<script>`/`<style>` element therefore goes
/// whole — opening tag, body, closing tag — and one that never closes keeps
/// dropping until the block ends, which is what HTML itself would do. Every other
/// tag is removed and the text around it kept. Character references are left as
/// written (`&amp;` stays `&amp;`): decoding them would be HTML parsing, which
/// this renderer deliberately avoids.
fn strip_html_block(text: &str, dropping_script_body: &mut bool) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        if !*dropping_script_body {
            kept.push_str(rest.get(..open).unwrap_or_default());
        }
        let tail = rest.get(open..).unwrap_or_default();
        // A comment runs to its own terminator, which `>`-scanning would miss.
        if let Some(body) = tail.strip_prefix("<!--") {
            rest = body.split_once("-->").map_or("", |(_, after)| after);
            continue;
        }
        // A `<` with no closing `>` is literal text, not markup.
        let Some(close) = tail.find('>') else {
            if !*dropping_script_body {
                kept.push('<');
            }
            rest = tail.get(1..).unwrap_or_default();
            continue;
        };
        let tag = tail.get(..=close).unwrap_or_default();
        rest = tail.get(close + 1..).unwrap_or_default();
        match scripting_tag(tag) {
            Some(false) => *dropping_script_body = true,
            Some(true) => *dropping_script_body = false,
            None => {}
        }
    }
    if !*dropping_script_body {
        kept.push_str(rest);
    }
    kept
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
///
/// Returns whether an active context took the text. A quote, list, or table
/// shell has nowhere to put bare inline text, and that `false` is what tells the
/// caller an HTML block's kept text needs a paragraph of its own.
fn push_text(
    block_stack: &mut [BlockContext],
    inline_stack: &mut [InlineContext],
    text: &str,
) -> bool {
    if text.is_empty() {
        return false;
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
        return true;
    }

    let Some(context) = block_stack.last_mut() else {
        return false;
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
        | BlockContext::TableRow(_) => return false,
    }
    true
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
