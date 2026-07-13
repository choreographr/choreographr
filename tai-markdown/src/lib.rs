use std::fmt::{self, Display, Formatter};
use std::str::FromStr;
use std::sync::OnceLock;
use thiserror::Error;

use ammonia::Builder as HtmlSanitizer;
use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html,
};

/// Error type for markdown parsing. Currently all operations are infallible,
/// but the type is defined here to establish the error-handling convention.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("markdown error")]
pub struct MarkdownError;

/// A parsed Markdown document, represented as an ordered list of block-level nodes.
///
/// Use [`MarkdownDocument::parse`] to build a document from a raw markdown string,
/// then call [`MarkdownDocument::to_markdown`] or [`MarkdownDocument::to_html`]
/// to serialize it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownDocument {
    /// The top-level block nodes in document order.
    pub blocks: Vec<MarkdownBlock>,
}

/// A block-level node in a Markdown document.
///
/// Each variant holds its own typed children, forming a recursive tree structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    /// A plain paragraph of inline content.
    Paragraph(Vec<MarkdownInline>),
    /// A section heading.
    Heading {
        /// The heading level (1–6).
        level: u8,
        /// The inline content of the heading.
        content: Vec<MarkdownInline>,
    },
    /// A fenced or indented code block.
    CodeBlock {
        /// The language annotation, if any (e.g. `"rust"` for `` ```rust ```).
        language: Option<String>,
        /// The raw code text.
        code: String,
    },
    /// A block quote containing nested blocks.
    BlockQuote(Vec<MarkdownBlock>),
    /// A list (ordered or unordered).
    List {
        /// Whether the list uses numeric ordering.
        ordered: bool,
        /// The starting index for an ordered list (1-based).
        start: usize,
        /// The list items, each a sequence of blocks.
        items: Vec<Vec<MarkdownBlock>>,
    },
    /// A table with optional column alignment.
    Table {
        /// Per-column alignment hints.
        alignments: Vec<MarkdownAlignment>,
        /// The header row cells.
        header: Vec<Vec<MarkdownInline>>,
        /// The data rows.
        rows: Vec<Vec<Vec<MarkdownInline>>>,
    },
    /// A thematic break (`---`, `***`, `___`).
    Rule,
}

/// Column alignment for a table cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownAlignment {
    /// No explicit alignment (default).
    None,
    /// Left-aligned.
    Left,
    /// Center-aligned.
    Center,
    /// Right-aligned.
    Right,
}

/// An inline node within a Markdown block.
///
/// Inline nodes can be nested (e.g. emphasis inside bold inside a link).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownInline {
    /// Plain text.
    Text(String),
    /// Inline code (backtick-delimited).
    Code(String),
    /// Inline math (`$...$`).
    InlineMath(String),
    /// Display math (`$$...$$`).
    DisplayMath(String),
    /// Strikethrough text (`~~text~~`).
    Strikethrough(Vec<MarkdownInline>),
    /// Emphasized text (`*text*` or `_text_`).
    Emphasis(Vec<MarkdownInline>),
    /// Strongly emphasized text (`**text**` or `__text__`).
    Strong(Vec<MarkdownInline>),
    /// A hyperlink.
    Link {
        /// The link text.
        content: Vec<MarkdownInline>,
        /// The URL destination.
        destination: String,
    },
    /// An image.
    Image {
        /// The alt text.
        alt: Vec<MarkdownInline>,
        /// The image URL.
        destination: String,
    },
    /// A line break.
    LineBreak,
}

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

/// Parse a markdown string and render it to sanitized HTML in one step.
///
/// This is a convenience function that combines parsing and HTML rendering.
/// It sanitizes the output with [`ammonia`] to prevent XSS attacks.
pub fn render_markdown_html(input: &str) -> String {
    let mut html_output = String::new();
    html::push_html(&mut html_output, Parser::new_ext(input, markdown_options()));
    sanitize_html(&html_output)
}

impl MarkdownDocument {
    /// Parse a raw markdown string into an AST.
    ///
    /// Uses `pulldown-cmark` with a curated set of extensions enabled.
    /// The returned document can be inspected, modified, and re-serialized.
    pub fn parse(input: &str) -> Self {
        let parser = Parser::new_ext(input, markdown_options());
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
                        inline_stack.push(InlineContext::Strikethrough(Vec::new()))
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
                    push_text(&mut block_stack, &mut inline_stack, &format!("[{text}]"))
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

    /// Convert the AST back to markdown, then render that to sanitized HTML.
    ///
    /// This is useful when you need to modify the AST and then produce HTML output.
    pub fn to_html(&self) -> String {
        render_markdown_html(&self.to_markdown())
    }

    /// Serialize the AST back to a markdown string.
    ///
    /// The output uses `*` for emphasis, `**` for strong, and standard GFM
    /// formatting throughout.
    pub fn to_markdown(&self) -> String {
        let mut markdown = String::new();
        for (index, block) in self.blocks.iter().enumerate() {
            if index > 0 {
                markdown.push_str("\n\n");
            }
            write_markdown_block(block, &mut markdown);
        }
        markdown
    }
}

impl Display for MarkdownDocument {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_markdown())
    }
}

impl FromStr for MarkdownDocument {
    type Err = MarkdownError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(s))
    }
}

fn markdown_options() -> Options {
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

fn sanitize_html(html: &str) -> String {
    static SANITIZER: OnceLock<HtmlSanitizer> = OnceLock::new();
    let sanitizer = SANITIZER.get_or_init(|| {
        let mut s = HtmlSanitizer::default();
        s.add_tags(["table", "thead", "tbody", "tr", "th", "td"]);
        s.add_tag_attributes("th", ["align"]);
        s.add_tag_attributes("td", ["align"]);
        s
    });
    sanitizer.clean(html).to_string()
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
    if !text.is_empty() {
        push_inline(
            block_stack,
            inline_stack,
            MarkdownInline::Text(text.to_string()),
        );
    }
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
            | InlineContext::Image { alt: content, .. } => content.push(inline),
        }
        return;
    }

    // Otherwise route the inline into the active block context.
    if let Some(context) = block_stack.last_mut() {
        match context {
            BlockContext::Paragraph(content)
            | BlockContext::Heading { content, .. }
            | BlockContext::TableCell(content) => content.push(inline),
            BlockContext::Item(blocks) => {
                // List items wrap inline content in a Paragraph block.
                // Ensure one exists so we have somewhere to push.
                if !matches!(blocks.last(), Some(MarkdownBlock::Paragraph(_))) {
                    blocks.push(MarkdownBlock::Paragraph(Vec::new()));
                }
                // At this point the last block is guaranteed to be a Paragraph.
                if let Some(MarkdownBlock::Paragraph(content)) = blocks.last_mut() {
                    content.push(inline);
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

fn write_markdown_block(block: &MarkdownBlock, markdown: &mut String) {
    match block {
        MarkdownBlock::Paragraph(content) => write_markdown_inlines(content, markdown),
        MarkdownBlock::Heading { level, content } => {
            markdown.push_str(&"#".repeat((*level).into()));
            markdown.push(' ');
            write_markdown_inlines(content, markdown);
        }
        MarkdownBlock::CodeBlock { language, code } => {
            markdown.push_str("```");
            if let Some(language) = language {
                markdown.push_str(language);
            }
            markdown.push('\n');
            markdown.push_str(code);
            if !code.ends_with('\n') {
                markdown.push('\n');
            }
            markdown.push_str("```");
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut inner = String::new();
            for (index, block) in blocks.iter().enumerate() {
                if index > 0 {
                    inner.push_str("\n\n");
                }
                write_markdown_block(block, &mut inner);
            }
            for (index, line) in inner.lines().enumerate() {
                if index > 0 {
                    markdown.push('\n');
                }
                markdown.push_str("> ");
                markdown.push_str(line);
            }
        }
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => {
            for (index, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", start + index)
                } else {
                    "- ".to_string()
                };
                let mut item_markdown = String::new();
                for (block_index, block) in item.iter().enumerate() {
                    if block_index > 0 {
                        item_markdown.push_str("\n\n");
                    }
                    write_markdown_block(block, &mut item_markdown);
                }
                let mut lines = item_markdown.lines();
                if let Some(first_line) = lines.next() {
                    markdown.push_str(&marker);
                    markdown.push_str(first_line);
                    for line in lines {
                        markdown.push('\n');
                        markdown.push_str(&" ".repeat(marker.len()));
                        markdown.push_str(line);
                    }
                } else {
                    markdown.push_str(&marker);
                }
                if index + 1 < items.len() {
                    markdown.push('\n');
                }
            }
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => {
            write_table_row_markdown(header, markdown);
            markdown.push('\n');
            markdown.push('|');
            for alignment in alignments {
                let separator = match alignment {
                    MarkdownAlignment::None => "---",
                    MarkdownAlignment::Left => ":---",
                    MarkdownAlignment::Center => ":---:",
                    MarkdownAlignment::Right => "---:",
                };
                markdown.push_str(separator);
                markdown.push('|');
            }
            for row in rows {
                markdown.push('\n');
                write_table_row_markdown(row, markdown);
            }
        }
        MarkdownBlock::Rule => markdown.push_str("---"),
    }
}

fn write_table_row_markdown(row: &[Vec<MarkdownInline>], markdown: &mut String) {
    markdown.push('|');
    for cell in row {
        markdown.push(' ');
        write_markdown_inlines(cell, markdown);
        markdown.push(' ');
        markdown.push('|');
    }
}

fn write_markdown_inlines(inlines: &[MarkdownInline], markdown: &mut String) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(text) => markdown.push_str(text),
            MarkdownInline::Code(code) => {
                markdown.push('`');
                markdown.push_str(code);
                markdown.push('`');
            }
            MarkdownInline::InlineMath(text) => {
                markdown.push('$');
                markdown.push_str(text);
                markdown.push('$');
            }
            MarkdownInline::DisplayMath(text) => {
                markdown.push_str("$$");
                markdown.push_str(text);
                markdown.push_str("$$");
            }
            MarkdownInline::Strikethrough(content) => {
                markdown.push_str("~~");
                write_markdown_inlines(content, markdown);
                markdown.push_str("~~");
            }
            MarkdownInline::Emphasis(content) => {
                markdown.push('*');
                write_markdown_inlines(content, markdown);
                markdown.push('*');
            }
            MarkdownInline::Strong(content) => {
                markdown.push_str("**");
                write_markdown_inlines(content, markdown);
                markdown.push_str("**");
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                markdown.push('[');
                write_markdown_inlines(content, markdown);
                markdown.push_str("](");
                markdown.push_str(destination);
                markdown.push(')');
            }
            MarkdownInline::Image { alt, destination } => {
                markdown.push_str("![");
                write_markdown_inlines(alt, markdown);
                markdown.push_str("](");
                markdown.push_str(destination);
                markdown.push(')');
            }
            MarkdownInline::LineBreak => markdown.push('\n'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_plain_text(blocks: &[MarkdownBlock]) -> String {
        let mut text = String::new();
        for block in blocks {
            match block {
                MarkdownBlock::Paragraph(content) | MarkdownBlock::Heading { content, .. } => {
                    text.push_str(&inline_text(content));
                }
                MarkdownBlock::CodeBlock { code, .. } => text.push_str(code),
                MarkdownBlock::BlockQuote(content) => text.push_str(&item_plain_text(content)),
                MarkdownBlock::List { items, .. } => {
                    for item in items {
                        text.push_str(&item_plain_text(item));
                    }
                }
                MarkdownBlock::Table { .. } | MarkdownBlock::Rule => {}
            }
        }
        text
    }

    #[test]
    fn markdown_parser_supports_common_llm_output() {
        let document = MarkdownDocument::parse(
            "# Heading\n\nA **bold** [link](https://example.com).\n\n- one\n- two\n\n```rs\nfn main() {}\n```",
        );

        assert!(matches!(document.blocks[0], MarkdownBlock::Heading { .. }));
        assert!(matches!(document.blocks[1], MarkdownBlock::Paragraph(_)));
        assert!(matches!(document.blocks[2], MarkdownBlock::List { .. }));
        assert!(matches!(
            document.blocks[3],
            MarkdownBlock::CodeBlock { .. }
        ));

        let MarkdownBlock::List { items, .. } = &document.blocks[2] else {
            panic!("expected list block");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(item_plain_text(&items[0]), "one");
        assert_eq!(item_plain_text(&items[1]), "two");
    }

    #[test]
    fn markdown_parser_preserves_task_list_item_text() {
        let document = MarkdownDocument::parse("- [x] done\n- [ ] todo");

        let MarkdownBlock::List { items, .. } = &document.blocks[0] else {
            panic!("expected list block");
        };

        assert_eq!(item_plain_text(&items[0]), "[x] done");
        assert_eq!(item_plain_text(&items[1]), "[ ] todo");
    }

    #[test]
    fn markdown_parser_preserves_nested_tight_list_text() {
        let document = MarkdownDocument::parse("- parent\n  - child\n  - child 2");

        let MarkdownBlock::List { items, .. } = &document.blocks[0] else {
            panic!("expected top-level list block");
        };

        assert_eq!(item_plain_text(&items[0]), "parentchildchild 2");

        let nested_list = items[0]
            .iter()
            .find_map(|block| match block {
                MarkdownBlock::List { items, .. } => Some(items),
                _ => None,
            })
            .expect("expected nested list");
        assert_eq!(item_plain_text(&nested_list[0]), "child");
        assert_eq!(item_plain_text(&nested_list[1]), "child 2");
    }

    #[test]
    fn markdown_parser_supports_tables() {
        let document =
            MarkdownDocument::parse("| Name | Role |\n|:--|--:|\n| Ada | Math |\n| Grace | CS |");

        assert!(matches!(document.blocks[0], MarkdownBlock::Table { .. }));
    }

    #[test]
    fn markdown_html_escapes_unsafe_html_and_links() {
        let safe_html = render_markdown_html("[ok](https://example.com)");
        let unsafe_html = render_markdown_html("[x](javascript:alert(1))");

        assert!(safe_html.contains("https://example.com"));
        assert!(!unsafe_html.contains("javascript:alert(1)"));
        assert!(!unsafe_html.contains("href="));
    }

    #[test]
    fn markdown_html_renders_tables() {
        let html =
            render_markdown_html("| Name | Role |\n|---|---|\n| Ada | Math |\n| Grace | CS |");

        assert!(html.contains("<table>"));
        assert!(html.contains("<td>Ada</td>"));
        assert!(html.contains("<td>Grace</td>"));
    }

    #[test]
    fn markdown_round_trip_paragraph() {
        let input = "Hello **world**.";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(doc.to_markdown(), input);
    }

    #[test]
    fn markdown_round_trip_heading() {
        let input = "# Heading\n\n## Subheading\n\n### Deep";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(doc.to_markdown(), input);
    }

    #[test]
    fn markdown_round_trip_code_block() {
        let input = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(doc.to_markdown(), input);
    }

    #[test]
    fn markdown_round_trip_list() {
        let input = "- one\n- two\n- three";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(doc.to_markdown(), input);
    }

    #[test]
    fn markdown_round_trip_table() {
        let input = "| Name | Role |\n|---|---|\n| Ada | Math |\n| Grace | CS |";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(doc.to_markdown(), input);
    }

    #[test]
    fn markdown_round_trip_strikethrough() {
        let input = "~~struck~~";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(doc.to_markdown(), input);
    }

    #[test]
    fn markdown_parser_supports_strikethrough() {
        let document = MarkdownDocument::parse("~~struck~~");

        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(matches!(content[0], MarkdownInline::Strikethrough(_)));
        assert_eq!(inline_text(content), "struck");
    }

    #[test]
    fn markdown_round_trip_math() {
        let input = "$x^2$";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(doc.to_markdown(), input);
    }

    #[test]
    fn markdown_parser_supports_math() {
        let document = MarkdownDocument::parse("$x^2$ and $$\\sum$$");

        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(matches!(content[0], MarkdownInline::InlineMath(_)));
        assert!(matches!(content[2], MarkdownInline::DisplayMath(_)));
        assert_eq!(inline_text(content), "x^2 and \\sum");
    }

    #[test]
    fn markdown_display_from_str() {
        let input = "# Hello\n\nWorld.";
        let doc: MarkdownDocument = input.parse().unwrap();
        assert_eq!(doc.to_string(), input);
    }
}
