//! The pulldown-cmark event → AST parser and its block/inline context stack.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::math::normalize_math_event;
use crate::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};

#[derive(Debug)]
enum BlockContext {
    Quote(Vec<MarkdownBlock>),
    List {
        ordered: bool,
        start: usize,
        items: Vec<Vec<MarkdownBlock>>,
    },
    Item(Vec<MarkdownBlock>),
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
        let parser = Parser::new_ext(input, markdown_options()).map(normalize_math_event);
        let mut blocks = Vec::new();
        let mut block_stack = Vec::<BlockContext>::new();
        let mut inline_stack = Vec::<InlineContext>::new();

        for event in parser {
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
                    Tag::Item => block_stack.push(BlockContext::Item(Vec::new())),
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
                        if let Some(BlockContext::Item(item_blocks)) = block_stack.pop()
                            && let Some(BlockContext::List { items, .. }) = block_stack.last_mut()
                        {
                            items.push(item_blocks);
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
                Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                    push_text(&mut block_stack, &mut inline_stack, &text);
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
                Event::TaskListMarker(checked) => push_text(
                    &mut block_stack,
                    &mut inline_stack,
                    if checked { "[x] " } else { "[ ] " },
                ),
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
        Some(BlockContext::Quote(blocks) | BlockContext::Item(blocks)) => blocks.push(block),
        _ => root.push(block),
    }
}

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

    if let Some(context) = block_stack.last_mut() {
        match context {
            BlockContext::Paragraph(content)
            | BlockContext::Heading { content, .. }
            | BlockContext::TableCell(content) => push_text_content(content, text),
            BlockContext::Item(blocks) => {
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
            BlockContext::Item(blocks) => {
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
