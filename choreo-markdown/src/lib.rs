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
fn push_text_content(content: &mut Vec<MarkdownInline>, text: &str) {
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

// ── LaTeX math → Unicode pretty-printing ─────────────────────────────────
//
// The parser captures `$...$` / `$$...$$` in the AST as `InlineMath` /
// `DisplayMath`, but raw LaTeX source is not pleasant to read in a terminal.
// [`render_math_pretty`] re-renders the inner TeX into a best-effort Unicode
// approximation (Greek letters, operator symbols, sub/superscripts, fractions
// and environments) so chat transcripts display math inline instead of as
// source text.
//
// The renderer is a small, depth-bounded, table-driven parser over the math
// source.  It is a *total* function: every construct it does not understand
// (unknown commands, unbalanced braces, deeply nested arguments) falls through
// to its raw source so output degrades gracefully instead of losing data.

/// Maximum accepted input length (bytes); longer input is returned verbatim to
/// bound the work done on any single expression.
const MAX_MATH_INPUT_LEN: usize = 4096;

/// Maximum nesting depth for grouped arguments and environments.  Deeply
/// nested input (pathological or adversarial) hits the cap and renders the
/// remainder verbatim, bounding recursion and stack use.
const MAX_MATH_DEPTH: u32 = 24;

/// Render a LaTeX math expression — the inner text of `$...$` or `$$...$$`
/// with the delimiters stripped — into a best-effort Unicode string for
/// terminal display.
///
/// The output is intentionally flat: a terminal has no scaled delimiters or
/// stacked fractions, so `\frac{a}{b}` becomes `a/b`, fences collapse to plain
/// brackets, and unknown commands are preserved verbatim.  When nothing can be
/// mapped the source is returned unchanged, so this is safe to call on
/// arbitrary provider-produced math.
pub fn render_math_pretty(tex: &str) -> String {
    if tex.is_empty() || tex.len() > MAX_MATH_INPUT_LEN {
        return tex.to_string();
    }
    let mut parser = MathPretty::new(tex.chars().collect(), 0);
    let out = parser.render_sequence();
    let trimmed = out.trim();
    if trimmed.is_empty() {
        // The mapping annihilated the input (e.g. only spacing commands);
        // emitting nothing would lose the expression, so fall back to source.
        tex.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Stateful single-pass renderer for one LaTeX math expression.
struct MathPretty {
    chars: Vec<char>,
    pos: usize,
    depth: u32,
}

impl MathPretty {
    fn new(chars: Vec<char>, depth: u32) -> Self {
        Self {
            chars,
            pos: 0,
            depth,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Render the whole remaining token stream into a single string.
    fn render_sequence(&mut self) -> String {
        if self.depth >= MAX_MATH_DEPTH {
            // Depth cap: emit the rest verbatim rather than recursing deeper.
            return self.chars[self.pos..].iter().collect();
        }
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            match ch {
                '\\' => self.parse_command(&mut out),
                '^' => self.parse_script(&mut out, true),
                '_' => self.parse_script(&mut out, false),
                // Prime markers (`f''`) become Unicode prime symbols.
                '\'' => self.parse_primes(&mut out),
                // TeX math collapses inter-token whitespace entirely; the only
                // whitespace that survives is inside `\text{...}`, which is
                // copied verbatim before the main loop resumes.
                c if c.is_whitespace() => {
                    self.bump();
                }
                c => {
                    out.push(c);
                    self.bump();
                }
            }
        }
        out
    }

    /// Skip runs of math-mode whitespace (which carries no meaning).
    fn skip_math_space(&mut self) {
        while self.peek().is_some_and(|c| c.is_whitespace()) {
            self.bump();
        }
    }

    /// Render a sub-sequence over a borrowed char span (a group body or an
    /// environment cell), recursing one level deeper so nesting is bounded.
    fn render_inner(&self, inner: &[char]) -> String {
        let mut sub = MathPretty::new(inner.to_vec(), self.depth + 1);
        sub.render_sequence()
    }

    /// Parse one mandatory argument: either a balanced `{...}` group or a
    /// single atom (used for `\frac`, `\sqrt`, `^`/`_` and accents, whose
    /// TeX syntax allows unbraced arguments).  Returns the rendered argument
    /// and whether it came from an explicit braced group.
    fn parse_group_or_single(&mut self) -> (String, bool) {
        self.skip_math_space();
        if self.peek() == Some('{') {
            if let Some(inner) = self.scan_group_content() {
                return (self.render_inner(&inner), true);
            }
            // Unbalanced `{`: keep the brace and render the next atom, then
            // let the main loop continue — lossless, never a panic.
            self.bump();
            let atom = self.render_single_atom();
            return (format!("{{{atom}"), true);
        }
        (self.render_single_atom(), false)
    }

    /// Render exactly one atom: a command (with its own argument handling) or
    /// a single literal character.
    fn render_single_atom(&mut self) -> String {
        match self.peek() {
            Some('\\') => {
                let mut s = String::new();
                self.parse_command(&mut s);
                s
            }
            Some(c) => {
                self.bump();
                c.to_string()
            }
            None => String::new(),
        }
    }

    /// Consume a balanced `{...}` group (if the current char is `{`) and
    /// return its contents without the braces.  On an unbalanced group the
    /// position is rewound to the `{` so callers can emit it literally.
    fn scan_group_content(&mut self) -> Option<Vec<char>> {
        if self.peek() != Some('{') {
            return None;
        }
        self.bump();
        let start = self.pos;
        let mut group_depth = 1usize;
        while let Some(c) = self.peek() {
            match c {
                '{' => {
                    group_depth += 1;
                    self.bump();
                }
                '}' => {
                    group_depth -= 1;
                    self.bump();
                    if group_depth == 0 {
                        return Some(self.chars[start..self.pos - 1].to_vec());
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
        // No closing brace: restore position to the `{` we consumed.
        self.pos = start - 1;
        None
    }

    /// Parse `^...` (superscript) or `_...` (subscript), mapping the group
    /// onto the Unicode super/subscript alphabet when possible.
    fn parse_script(&mut self, out: &mut String, is_superscript: bool) {
        self.bump(); // consume `^` or `_`
        let (rendered, was_group) = self.parse_group_or_single();
        if rendered.is_empty() {
            return;
        }
        match map_script_run(&rendered, is_superscript) {
            Some(mapped) => out.push_str(&mapped),
            None => {
                // Unmappable (semantic letters have no Unicode script forms):
                // keep the LaTeX so the formula stays recoverable.
                out.push(if is_superscript { '^' } else { '_' });
                if was_group || rendered.chars().count() > 1 {
                    out.push('(');
                    out.push_str(&rendered);
                    out.push(')');
                } else {
                    out.push_str(&rendered);
                }
            }
        }
    }

    /// Map runs of prime markers (`''`) to Unicode prime symbols.
    fn parse_primes(&mut self, out: &mut String) {
        let mut count = 0;
        while self.peek() == Some('\'') {
            self.bump();
            count += 1;
        }
        out.push_str(match count {
            1 => "′",
            2 => "″",
            3 => "‴",
            _ => "′",
        });
    }

    /// Handle a backslash token: a named command, an escaped character, or the
    /// `\ ` / `\\` specials.
    fn parse_command(&mut self, out: &mut String) {
        debug_assert!(self.peek() == Some('\\'));
        self.bump(); // the backslash
        match self.peek() {
            Some(' ') => {
                // `\ ` — an explicit space in math.
                self.bump();
                out.push(' ');
            }
            Some('\\') => {
                // `\\` — an (env) row break; outside environments it carries
                // no displayable meaning.
                self.bump();
            }
            _ => {}
        }
        let name_start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                self.bump();
            } else {
                break;
            }
        }
        let name: String = self.chars[name_start..self.pos].iter().collect();
        if name.is_empty() {
            // Escaped single character: `\{`, `\&`, `\,`, `\alpha` is handled
            // above via the letter loop; here we reach non-letter escapes.
            if let Some(c) = self.bump() {
                out.push(escaped_math_char(c));
            }
            return;
        }
        if self.dispatch_command(&name, out) {
            return;
        }
        // Unknown command: emit it verbatim and do NOT consume a following
        // argument group, so `\foo{x}` renders as source rather than `foo`
        // swallowing the braces.
        out.push('\\');
        out.push_str(&name);
    }

    /// Dispatch a known command to its renderer.  Returns `false` when the
    /// name is unknown so the caller can emit it verbatim.
    fn dispatch_command(&mut self, name: &str, out: &mut String) -> bool {
        match name {
            "begin" | "end" => {
                self.parse_environment(name, out);
                true
            }
            "frac" => {
                let (num, _) = self.parse_group_or_single();
                let (den, _) = self.parse_group_or_single();
                out.push_str(&frac_join(&num, &den));
                true
            }
            "sqrt" => {
                // Optional `\sqrt[n]{...}` index in square brackets.
                let mut index: Vec<char> = Vec::new();
                self.skip_math_space();
                if self.peek() == Some('[') {
                    self.bump();
                    while let Some(c) = self.peek() {
                        if c == ']' {
                            break;
                        }
                        index.push(c);
                        self.bump();
                    }
                    if self.peek() == Some(']') {
                        self.bump();
                    }
                }
                let (radicand, _) = self.parse_group_or_single();
                if radicand.is_empty() {
                    return true;
                }
                if index.as_slice() == ['3'] {
                    out.push('∛');
                } else if !index.is_empty() {
                    if let Some(sup) = map_script_run(&index.iter().collect::<String>(), true) {
                        out.push_str(&sup);
                    }
                    out.push('√');
                } else {
                    out.push('√');
                }
                if needs_frac_parens(&radicand) {
                    out.push('(');
                    out.push_str(&radicand);
                    out.push(')');
                } else {
                    out.push_str(&radicand);
                }
                true
            }
            // Text-style commands: copy their argument verbatim (whitespace
            // preserved), because these carry prose, not math structure.
            "text" | "mathrm" | "textrm" | "operatorname" | "mbox" | "hbox" | "mathbf"
            | "mathit" | "boldsymbol" | "textbf" | "textit" | "mathsf" | "mathtt"
            | "textnormal" | "underline" => {
                self.skip_math_space();
                match self.scan_group_content() {
                    Some(inner) => out.push_str(&inner.iter().collect::<String>()),
                    None => out.push('{'),
                }
                true
            }
            "mathbb" => {
                self.skip_math_space();
                match self.scan_group_content() {
                    Some(inner) => {
                        for c in inner {
                            match blackboard_char(c) {
                                Some(mapped) => out.push(mapped),
                                None => out.push(c),
                            }
                        }
                    }
                    None => out.push('{'),
                }
                true
            }
            "pmod" => {
                out.push_str("(mod");
                self.skip_math_space();
                if let Some(inner) = self.scan_group_content() {
                    out.push(' ');
                    out.push_str(&inner.iter().collect::<String>());
                }
                out.push(')');
                true
            }
            "binom" => {
                let (top, _) = self.parse_group_or_single();
                let (bottom, _) = self.parse_group_or_single();
                out.push('(');
                out.push_str(&top);
                out.push(' ');
                out.push_str(&bottom);
                out.push(')');
                true
            }
            // Combining accents: `\hat{a}` → `â`.  The mark is attached to the
            // first character; multi-character arguments keep the mark on the
            // first glyph, which is the best a flat terminal can do.
            "hat" | "widehat" => {
                out.push_str(&self.accent_combining('\u{0302}'));
                true
            }
            "bar" | "overline" | "overbar" => {
                out.push_str(&self.accent_combining('\u{0304}'));
                true
            }
            "vec" => {
                out.push_str(&self.accent_combining('\u{20D7}'));
                true
            }
            "dot" => {
                out.push_str(&self.accent_combining('\u{0307}'));
                true
            }
            "ddot" => {
                out.push_str(&self.accent_combining('\u{0308}'));
                true
            }
            "acute" => {
                out.push_str(&self.accent_combining('\u{0301}'));
                true
            }
            "grave" => {
                out.push_str(&self.accent_combining('\u{0300}'));
                true
            }
            "tilde" | "widetilde" => {
                out.push_str(&self.accent_combining('\u{0303}'));
                true
            }
            "check" => {
                out.push_str(&self.accent_combining('\u{030C}'));
                true
            }
            "breve" => {
                out.push_str(&self.accent_combining('\u{0306}'));
                true
            }
            "left" | "right" => {
                let fence = self.parse_fence_delim();
                if !fence.is_empty() {
                    out.push_str(&fence);
                }
                true
            }
            _ => math_symbol(name).is_some_and(|sym| {
                out.push_str(sym);
                true
            }),
        }
    }

    /// Attach a combining accent mark to a (single or first) argument char.
    fn accent_combining(&mut self, mark: char) -> String {
        let (arg, _) = self.parse_group_or_single();
        let mut chars = arg.chars();
        let Some(first) = chars.next() else {
            return String::new();
        };
        let mut s = String::new();
        s.push(first);
        s.push(mark);
        for rest in chars {
            s.push(rest);
        }
        s
    }

    /// Consume the delimiter that follows `\left` / `\right` and return its
    /// flattened Unicode equivalent (`.` means "no delimiter").
    fn parse_fence_delim(&mut self) -> String {
        self.skip_math_space();
        match self.peek() {
            Some('.') => {
                self.bump();
                String::new()
            }
            Some('\\') => {
                self.bump();
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphabetic() {
                        self.bump();
                    } else {
                        break;
                    }
                }
                let name: String = self.chars[start..self.pos].iter().collect();
                let mapped = match name.as_str() {
                    "lvert" | "rvert" => "|",
                    "Vert" | "lVert" | "rVert" => "‖",
                    "langle" => "⟨",
                    "rangle" => "⟩",
                    "lfloor" => "⌊",
                    "rfloor" => "⌋",
                    "lceil" => "⌈",
                    "rceil" => "⌉",
                    "uparrow" => "↑",
                    "downarrow" => "↓",
                    "updownarrow" => "↕",
                    _ => "",
                };
                if mapped.is_empty() {
                    // Non-letter escaped delimiter (`\left\{`): fall back to
                    // the escaped-character table.
                    self.bump()
                        .map(|c| escaped_math_char(c).to_string())
                        .unwrap_or_default()
                } else {
                    mapped.to_string()
                }
            }
            Some('{' | '}' | '(' | ')' | '[' | ']' | '|') => {
                // The match guard guarantees a char is present.
                if let Some(c) = self.bump() {
                    c.to_string()
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        }
    }

    /// Handle `\begin{env}...\end{env}`: find the matching end, split the
    /// body into rows/cells (respecting nested environments), and flatten it.
    fn parse_environment(&mut self, name: &str, out: &mut String) {
        if name != "begin" {
            // A stray `\end` is emitted literally; its `{...}` renders as
            // regular characters in the main loop.
            out.push_str("\\end");
            return;
        }
        self.skip_math_space();
        let env = match self.scan_group_content() {
            Some(inner) => inner,
            None => {
                out.push_str("\\begin");
                return;
            }
        };
        // `\begin{array}{cc}` carries a column spec after the name — skip it.
        let env_str: String = env.iter().collect();
        self.skip_math_space();
        if matches!(env_str.as_str(), "array" | "tabular") && self.peek() == Some('{') {
            let _ = self.scan_group_content();
        }
        let body_start = self.pos;
        match self.find_env_end(&env, body_start) {
            Some((end_token_start, after_end)) => {
                let body: Vec<char> = self.chars[body_start..end_token_start].to_vec();
                self.pos = after_end;
                out.push_str(&self.render_environment_body(&body, &env_str));
            }
            None => {
                // No matching `\end`: emit the `\begin{...}` literally.
                out.push_str("\\begin{");
                out.push_str(&env_str);
                out.push('}');
            }
        }
    }

    /// Scan from `from` to the `\end{...}` that closes the environment opened
    /// at `from-1`, honouring nested `\begin`/`\end` pairs.  Returns the index
    /// of the `\` that starts the matching `\end` (for slicing off the body)
    /// and the index just past its closing `}` (for resuming the main loop).
    fn find_env_end(&self, _env: &[char], from: usize) -> Option<(usize, usize)> {
        let chars = &self.chars;
        let mut i = from;
        let mut depth = 1usize;
        while i < chars.len() {
            if chars[i] != '\\' {
                i += 1;
                continue;
            }
            let cmd_start = i + 1;
            let mut j = cmd_start;
            while j < chars.len() && chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            let cmd: String = chars[cmd_start..j].iter().collect();
            if cmd == "begin" || cmd == "end" {
                let mut k = j;
                while k < chars.len() && chars[k] != '{' {
                    k += 1;
                }
                if k < chars.len() {
                    let name_start = k + 1;
                    let mut name_end = name_start;
                    while name_end < chars.len() && chars[name_end] != '}' {
                        name_end += 1;
                    }
                    if name_end >= chars.len() {
                        return None; // unbalanced `\end{...`
                    }
                    if cmd == "begin" {
                        depth += 1;
                    } else {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return Some((i, name_end + 1));
                        }
                    }
                    i = name_end + 1;
                    continue;
                }
            }
            i = j;
        }
        None
    }

    /// Flatten an environment body to a single line: rows on `\\`, cells on
    /// `&`, then per-environment bracket styles.
    fn render_environment_body(&self, body: &[char], env: &str) -> String {
        let rows = split_environment_rows(body);
        let rendered_rows: Vec<String> = rows
            .iter()
            .map(|cells| {
                let rendered: Vec<String> =
                    cells.iter().map(|cell| self.render_inner(cell)).collect();
                // `cases` rows are "value & condition" — joined with a space;
                // matrix-like environments separate columns with commas.
                if matches!(env, "cases" | "dcases") {
                    rendered.join(" ")
                } else {
                    rendered.join(", ")
                }
            })
            .collect();
        let inner = rendered_rows.join("; ");
        match env {
            "cases" | "dcases" => format!("{{ {inner} }}"),
            "pmatrix" => format!("({inner})"),
            "bmatrix" | "Bmatrix" => format!("[{inner}]"),
            "vmatrix" | "Vmatrix" => format!("|{inner}|"),
            // matrix / array / aligned / gathered / split and any future
            // environment flatten to their plain cell content.
            _ => inner,
        }
    }
}

/// Map an escaped single character (from `\{`, `\&`, `\,`, …).
fn escaped_math_char(c: char) -> char {
    match c {
        '{' | '}' | '%' | '$' | '&' | '#' | '_' | '(' | ')' | '[' | ']' => c,
        // Thin/medium/thick/negative spaces and `\ ` collapse to one space.
        ',' | ':' | ';' | '!' => ' ',
        _ => c,
    }
}

/// Whether a rendered fraction component reads as compound (operators, other
/// punctuation) and therefore benefits from explicit parentheses.
fn needs_frac_parens(expr: &str) -> bool {
    expr.chars().any(|c| {
        matches!(
            c,
            '+' | '-'
                | '='
                | '/'
                | '*'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | ' '
                | ';'
                | '≤'
                | '≥'
                | '≠'
                | '·'
        )
    })
}

/// Render `num/den`, parenthesising either side when it is compound.
fn frac_join(num: &str, den: &str) -> String {
    let mut s = String::new();
    if needs_frac_parens(num) {
        s.push('(');
        s.push_str(num);
        s.push(')');
    } else {
        s.push_str(num);
    }
    s.push('/');
    if needs_frac_parens(den) {
        s.push('(');
        s.push_str(den);
        s.push(')');
    } else {
        s.push_str(den);
    }
    s
}

/// Map a whole rendered argument onto the Unicode super/subscript alphabet.
///
/// Any *letter* that has no script form (e.g. `q`, most capitals) aborts the
/// whole mapping so semantic identifiers keep their literal form; operators,
/// arrows and digits that lack a script form are emitted literally so common
/// phrases like `\lim_{x \to 0}` still read naturally (`limₓ→₀`).
fn map_script_run(expr: &str, is_superscript: bool) -> Option<String> {
    if expr.is_empty() {
        return None;
    }
    let mut mapped = String::new();
    for c in expr.chars() {
        match if is_superscript {
            superscript_char(c)
        } else {
            subscript_char(c)
        } {
            Some(m) => mapped.push(m),
            None if c.is_ascii_alphabetic() => return None,
            None => mapped.push(c),
        }
    }
    Some(mapped)
}

/// Unicode superscript (power) form of a character, when one exists.
fn superscript_char(c: char) -> Option<char> {
    let cp = match c {
        '0' => 0x2070,
        '1' => 0x00B9,
        '2' => 0x00B2,
        '3' => 0x00B3,
        '4'..='9' => 0x2074 + (c as u32 - '4' as u32),
        '+' => 0x207A,
        '-' => 0x207B,
        '=' => 0x207C,
        '(' => 0x207D,
        ')' => 0x207E,
        'n' => 0x207F,
        'i' => 0x2071,
        // Superscript Latin small letters (from Phonetic / Cyrillic-related
        // extensions).  `q` and capitals have no forms and are left to the
        // literal fallback by `map_script_run`.
        'a' => 0x1D43,
        'b' => 0x1D47,
        'c' => 0x1D9C,
        'd' => 0x1D48,
        'e' => 0x1D49,
        'f' => 0x1DA0,
        'g' => 0x1D4D,
        'h' => 0x02B0,
        'j' => 0x02B2,
        'k' => 0x1D4F,
        'l' => 0x02E1,
        'm' => 0x1D50,
        'o' => 0x1D52,
        'p' => 0x1D56,
        'r' => 0x02B3,
        's' => 0x02E2,
        't' => 0x1D57,
        'u' => 0x1D58,
        'v' => 0x1D5B,
        'w' => 0x02B7,
        'x' => 0x02E3,
        'y' => 0x02B8,
        'z' => 0x1DBB,
        _ => return None,
    };
    char::from_u32(cp)
}

/// Unicode subscript form of a character, when one exists.
fn subscript_char(c: char) -> Option<char> {
    let cp = match c {
        '0'..='9' => 0x2080 + (c as u32 - '0' as u32),
        '+' => 0x208A,
        '-' => 0x208B,
        '=' => 0x208C,
        '(' => 0x208D,
        ')' => 0x208E,
        // Latin Subscripts + the Phonetic-Extensions i/r/u/v forms.
        'a' => 0x2090,
        'e' => 0x2091,
        'o' => 0x2092,
        'x' => 0x2093,
        'h' => 0x2095,
        'k' => 0x2096,
        'l' => 0x2097,
        'm' => 0x2098,
        'n' => 0x2099,
        'p' => 0x209A,
        's' => 0x209B,
        't' => 0x209C,
        'i' => 0x1D62,
        'r' => 0x1D63,
        'u' => 0x1D64,
        'v' => 0x1D65,
        _ => return None,
    };
    char::from_u32(cp)
}

/// Double-struck (blackboard bold) form of a character, used by `\mathbb`.
fn blackboard_char(c: char) -> Option<char> {
    // Capitals are not a contiguous run in Unicode (ℂ ℍ ℕ ℙ ℚ ℝ ℤ live in the
    // Letterlike Symbols block), so they need an explicit table.
    const BLACKBOARD_CAPITALS: [u32; 26] = [
        0x1D538, 0x1D539, 0x2102, 0x1D53B, 0x1D53C, 0x1D53D, 0x1D53E, 0x210D, 0x1D540, 0x1D541,
        0x1D542, 0x1D543, 0x1D544, 0x2115, 0x1D546, 0x2119, 0x211A, 0x211D, 0x1D54A, 0x1D54B,
        0x1D54C, 0x1D54D, 0x1D54E, 0x1D54F, 0x1D550, 0x2124,
    ];
    match c {
        'A'..='Z' => char::from_u32(BLACKBOARD_CAPITALS[(c as u32 - 'A' as u32) as usize]),
        // Lowercase and digits are contiguous runs.
        'a'..='z' => char::from_u32(0x1D552 + (c as u32 - 'a' as u32)),
        '0'..='9' => char::from_u32(0x1D7D8 + (c as u32 - '0' as u32)),
        _ => None,
    }
}

/// Split an environment body into `rows[cell][]`, honouring nested
/// environments so `&` / `\\` inside a matrix-in-a-case are not treated as
/// separators of the outer environment.
fn split_environment_rows(body: &[char]) -> Vec<Vec<Vec<char>>> {
    let mut rows: Vec<Vec<Vec<char>>> = Vec::new();
    let mut row: Vec<Vec<char>> = Vec::new();
    let mut cell: Vec<char> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < body.len() {
        let c = body[i];
        if c == '\\' {
            if body.get(i + 1) == Some(&'\\') {
                if depth == 0 {
                    row.push(std::mem::take(&mut cell));
                    rows.push(std::mem::take(&mut row));
                } else {
                    cell.push('\\');
                    cell.push('\\');
                }
                i += 2;
                continue;
            }
            // A nested `\begin` / `\end` — track depth but keep the command
            // text in the cell so the nested structure survives re-parsing.
            let mut j = i + 1;
            while j < body.len() && body[j].is_ascii_alphabetic() {
                j += 1;
            }
            let cmd: String = body[i + 1..j].iter().collect();
            if cmd == "begin" || cmd == "end" {
                let mut end_of_cmd = j;
                let mut k = j;
                while k < body.len() && body[k] != '{' {
                    k += 1;
                }
                let mut name_end = k;
                if k < body.len() {
                    while name_end < body.len() && body[name_end] != '}' {
                        name_end += 1;
                    }
                    if name_end < body.len() {
                        end_of_cmd = name_end + 1;
                    }
                }
                if cmd == "begin" {
                    depth += 1;
                } else {
                    depth = depth.saturating_sub(1);
                }
                for ch in &body[i..end_of_cmd] {
                    cell.push(*ch);
                }
                i = end_of_cmd;
                continue;
            }
            cell.push(c);
            i += 1;
            continue;
        }
        if c == '&' && depth == 0 {
            row.push(std::mem::take(&mut cell));
            i += 1;
            continue;
        }
        cell.push(c);
        i += 1;
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(std::mem::take(&mut cell));
        rows.push(row);
    }
    rows
}

/// Look up a single-token math command (Greek letters, operator/relation
/// symbols, named functions) — every name that needs no argument handling.
#[allow(clippy::too_many_lines)]
fn math_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Greek letters.
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "varepsilon" => "ϵ",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "omicron" => "ο",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "φ",
        "varphi" => "ϕ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        // Large operators.
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "bigcap" => "⋂",
        "bigcup" => "⋃",
        "bigvee" => "⋁",
        "bigwedge" => "⋀",
        "bigoplus" => "⊕",
        "bigotimes" => "⊗",
        "bigodot" => "⊙",
        // Binary operators.
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "odot" => "⊙",
        "dagger" => "†",
        "ddagger" => "‡",
        "wedge" => "∧",
        "vee" => "∨",
        "land" => "∧",
        "lor" => "∨",
        "cap" => "∩",
        "cup" => "∪",
        "sqcap" => "⊓",
        "sqcup" => "⊔",
        "uplus" => "⊎",
        "sqsubset" => "⊏",
        "sqsupset" => "⊐",
        "setminus" => "∖",
        // Relations.
        "le" => "≤",
        "leq" => "≤",
        "ge" => "≥",
        "geq" => "≥",
        "ne" => "≠",
        "neq" => "≠",
        "equiv" => "≡",
        "approx" => "≈",
        "approxeq" => "≊",
        "cong" => "≅",
        "sim" => "∼",
        "simeq" => "≃",
        "propto" => "∝",
        "asymp" => "≍",
        "ll" => "≪",
        "gg" => "≫",
        "prec" => "≺",
        "succ" => "≻",
        "preceq" => "⪯",
        "succeq" => "⪰",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "owns" => "∋",
        "mid" => "∣",
        "nmid" => "∤",
        "parallel" => "∥",
        "perp" => "⊥",
        "top" => "⊤",
        "bot" => "⊥",
        "vdash" => "⊢",
        "dashv" => "⊣",
        "models" => "⊨",
        "vDash" => "⊨",
        "doteq" => "≐",
        "triangleq" => "≜",
        "therefore" => "∴",
        "because" => "∵",
        "neg" => "¬",
        "lnot" => "¬",
        // Arrows.
        "to" => "→",
        "gets" => "←",
        "leftarrow" => "←",
        "rightarrow" => "→",
        "Leftarrow" => "⇐",
        "Rightarrow" => "⇒",
        "leftrightarrow" => "↔",
        "Leftrightarrow" => "⇔",
        "iff" => "⟺",
        "mapsto" => "↦",
        "longmapsto" => "⟼",
        "longrightarrow" => "⟶",
        "longleftarrow" => "⟵",
        "Longrightarrow" => "⟹",
        "implies" => "⟹",
        "impliedby" => "⟸",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "updownarrow" => "↕",
        "nearrow" => "↗",
        "searrow" => "↘",
        "swarrow" => "↙",
        "nwarrow" => "↖",
        "rightleftharpoons" => "⇌",
        "leftrightharpoons" => "⇋",
        // Fences & punctuation.
        "Vert" => "‖",
        "lVert" => "‖",
        "rVert" => "‖",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "ulcorner" => "⌜",
        "urcorner" => "⌝",
        "llcorner" => "⌞",
        "lrcorner" => "⌟",
        "lvert" => "|",
        "rvert" => "|",
        "prime" => "′",
        "colon" => ":",
        // Decorations & misc symbols.
        "infty" => "∞",
        "nabla" => "∇",
        "partial" => "∂",
        "hbar" => "ℏ",
        "hslash" => "ℏ",
        "ell" => "ℓ",
        "wp" => "℘",
        "imath" => "ı",
        "jmath" => "ȷ",
        "aleph" => "ℵ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "emptyset" => "∅",
        "varnothing" => "∅",
        "angle" => "∠",
        "measuredangle" => "∡",
        "sphericalangle" => "∢",
        "degree" => "°",
        "surd" => "√",
        "flat" => "♭",
        "natural" => "♮",
        "sharp" => "♯",
        "dots" => "…",
        "ldots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "checkmark" => "✓",
        "square" => "□",
        "blacksquare" => "■",
        "triangle" => "△",
        "copyright" => "©",
        "pounds" => "£",
        "S" => "§",
        "P" => "¶",
        // Named functions: kept as plain words — a terminal cannot fake the
        // upright-vs-italic distinction, so styling would only add noise.
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "cot" => "cot",
        "sec" => "sec",
        "csc" => "csc",
        "arcsin" => "arcsin",
        "arccos" => "arccos",
        "arctan" => "arctan",
        "sinh" => "sinh",
        "cosh" => "cosh",
        "tanh" => "tanh",
        "coth" => "coth",
        "log" => "log",
        "ln" => "ln",
        "exp" => "exp",
        "lim" => "lim",
        "max" => "max",
        "min" => "min",
        "inf" => "inf",
        "sup" => "sup",
        "arg" => "arg",
        "deg" => "deg",
        "det" => "det",
        "dim" => "dim",
        "gcd" => "gcd",
        "hom" => "hom",
        "ker" => "ker",
        "Pr" => "Pr",
        "mod" => "mod",
        "bmod" => "mod",
        // `{a \over b}` — the binary fraction operator.
        "over" => " / ",
        _ => return None,
    })
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

    #[test]
    fn push_text_content_merges_adjacent_text() {
        let mut content: Vec<MarkdownInline> = Vec::new();
        push_text_content(&mut content, "I");
        push_text_content(&mut content, "'");
        push_text_content(&mut content, "ll");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], MarkdownInline::Text("I'll".to_string()));
    }

    #[test]
    fn push_text_content_creates_new_text_when_last_is_not_text() {
        let mut content: Vec<MarkdownInline> = Vec::new();
        content.push(MarkdownInline::Code("x".to_string()));
        push_text_content(&mut content, "hello");
        assert_eq!(content.len(), 2);
        assert_eq!(content[1], MarkdownInline::Text("hello".to_string()));
    }

    #[test]
    fn push_text_content_creates_new_text_when_empty() {
        let mut content: Vec<MarkdownInline> = Vec::new();
        push_text_content(&mut content, "hello");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], MarkdownInline::Text("hello".to_string()));
    }

    #[test]
    fn smart_punctuation_merges_text_around_inline_boundary() {
        // Smart punctuation triggers text splitting around inline
        // boundaries (e.g. the ` before a code span).  The merging
        // logic should reassemble adjacent text into single nodes.
        let doc = MarkdownDocument::parse("I'll `code` there.");
        let MarkdownBlock::Paragraph(content) = &doc.blocks[0] else {
            panic!("expected paragraph");
        };
        // With smart punctuation "I'll" becomes one text event
        // containing the curly apostrophe.
        assert_eq!(content[0], MarkdownInline::Text("I\u{2019}ll ".to_string()));
        assert_eq!(content[1], MarkdownInline::Code("code".to_string()));
        assert_eq!(content[2], MarkdownInline::Text(" there.".to_string()));
        assert_eq!(content.len(), 3);
    }

    #[test]
    fn smart_punctuation_round_trip() {
        // Smart punctuation transforms straight quotes/apostrophes to
        // their typographic equivalents, so the round-trip output
        // contains curly quotes.
        let input = "I'll be there.";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(doc.to_markdown(), "I\u{2019}ll be there.");
    }

    #[test]
    fn smart_quotes_round_trip() {
        let input = "She said \"hello\" and left.";
        let doc = MarkdownDocument::parse(input);
        assert_eq!(
            doc.to_markdown(),
            "She said \u{201c}hello\u{201d} and left."
        );
    }

    #[test]
    fn text_never_adjacent_across_inline_boundary() {
        // Code nodes should not merge with adjacent text.
        let doc = MarkdownDocument::parse("a `code` b");
        let MarkdownBlock::Paragraph(content) = &doc.blocks[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(content.len(), 3);
        assert_eq!(content[0], MarkdownInline::Text("a ".to_string()));
        assert_eq!(content[1], MarkdownInline::Code("code".to_string()));
        assert_eq!(content[2], MarkdownInline::Text(" b".to_string()));
    }

    // ── render_math_pretty ────────────────────────────────────────────────

    #[test]
    fn math_pretty_basic_powers_and_subscripts() {
        assert_eq!(render_math_pretty("x^2 + y_1"), "x²+y₁");
    }

    #[test]
    fn math_pretty_frac() {
        assert_eq!(render_math_pretty("\\frac{1}{2}"), "1/2");
        assert_eq!(render_math_pretty("\\frac{x+1}{x-1}"), "(x+1)/(x-1)");
        assert_eq!(render_math_pretty("\\frac{dy}{dx}"), "dy/dx");
    }

    #[test]
    fn math_pretty_nested_frac() {
        assert_eq!(render_math_pretty("\\frac{\\frac{a}{b}}{c}"), "(a/b)/c");
    }

    #[test]
    fn math_pretty_sqrt() {
        assert_eq!(render_math_pretty("\\sqrt{x}"), "√x");
        assert_eq!(render_math_pretty("\\sqrt{x+y}"), "√(x+y)");
        assert_eq!(render_math_pretty("\\sqrt[3]{8}"), "∛8");
    }

    #[test]
    fn math_pretty_greek_and_relations() {
        assert_eq!(render_math_pretty("\\alpha + \\beta \\le \\gamma"), "α+β≤γ");
    }

    #[test]
    fn math_pretty_sum_with_limits() {
        assert_eq!(render_math_pretty("\\sum_{i=1}^{n}"), "∑ᵢ₌₁ⁿ");
    }

    #[test]
    fn math_pretty_limits_flatten_arrows() {
        // `\to` has no subscript glyph, but the surrounding letters/digits do;
        // the partial mapping keeps the common `\lim_{x \to 0}` readable.
        assert_eq!(render_math_pretty("\\lim_{x \\to 0}"), "limₓ→₀");
    }

    #[test]
    fn math_pretty_exponent_group() {
        assert_eq!(render_math_pretty("e^{-x}"), "e⁻ˣ");
        assert_eq!(render_math_pretty("x^{n+1}"), "xⁿ⁺¹");
    }

    #[test]
    fn math_pretty_text_command_keeps_prose() {
        assert_eq!(render_math_pretty("\\text{if } n"), "if n");
    }

    #[test]
    fn math_pretty_mathbb() {
        assert_eq!(render_math_pretty("x \\in \\mathbb{R}"), "x∈ℝ");
        assert_eq!(render_math_pretty("\\mathbb{Z}"), "ℤ");
    }

    #[test]
    fn math_pretty_fences() {
        assert_eq!(render_math_pretty("\\left( x \\right)"), "(x)");
        assert_eq!(render_math_pretty("\\left\\{ x \\right\\}"), "{x}");
    }

    #[test]
    fn math_pretty_escaped_chars() {
        assert_eq!(render_math_pretty("\\{a\\}"), "{a}");
    }

    #[test]
    fn math_pretty_primes() {
        assert_eq!(render_math_pretty("f''(x)"), "f″(x)");
    }

    #[test]
    fn math_pretty_cases_environment() {
        assert_eq!(
            render_math_pretty(
                "\\begin{cases} 1 & \\text{if } x \\\\ 0 & \\text{otherwise} \\end{cases}"
            ),
            "{ 1 if x; 0 otherwise }"
        );
    }

    #[test]
    fn math_pretty_unknown_command_falls_back_to_source() {
        assert_eq!(render_math_pretty("\\foo{x}"), "\\foo{x}");
    }

    #[test]
    fn math_pretty_unknown_script_keeps_literal() {
        // `q` has no subscript glyph, and `b` aborts the `ab` group's mapping;
        // the semantic letters keep their literal form rather than emitting
        // a misleading partial subscript.
        assert_eq!(render_math_pretty("x_q"), "x_q");
        assert_eq!(render_math_pretty("x_{ab}"), "x_(ab)");
        // `k`, `+` and digits all have subscript glyphs, so this stays readable.
        assert_eq!(render_math_pretty("x_{k+1}"), "xₖ₊₁");
    }

    #[test]
    fn math_pretty_empty_and_overlong_input() {
        assert_eq!(render_math_pretty(""), "");
        let long = "a".repeat(5000);
        assert_eq!(render_math_pretty(&long), long);
    }

    #[test]
    fn math_pretty_is_total_on_adversarial_input() {
        // Unbalanced braces, stray delimiters, and truncated environments must
        // never panic and must not collapse a non-empty input to nothing.
        for s in [
            "\\begin{matrix} a & b",
            "\\frac{1}{2",
            "\\left(",
            "\\sqrt[3]{2",
            "{}",
            "\\\\",
        ] {
            let out = render_math_pretty(s);
            assert!(!out.is_empty(), "unexpected collapse: {out:?}");
        }
    }
}
