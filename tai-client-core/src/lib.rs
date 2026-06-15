use ammonia::Builder as HtmlSanitizer;
use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html,
};
use std::{collections::HashMap, io};
use tai_proto::{ClientMessage, ImageMetadata, OutputStream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Send(ClientMessage),
    InvalidCancel(String),
    Empty,
}

pub fn parse_input_line(line: &str, next_request_id: &mut u32) -> ShellCommand {
    let line = line.trim();
    if line.is_empty() {
        return ShellCommand::Empty;
    }

    if let Some(rest) = line.strip_prefix(":cancel ") {
        return match rest.trim().parse::<u32>() {
            Ok(request_id) => ShellCommand::Send(ClientMessage::Cancel { request_id }),
            Err(_) => ShellCommand::InvalidCancel(rest.trim().to_string()),
        };
    }

    if line == ":ping" {
        return ShellCommand::Send(ClientMessage::Ping);
    }

    if line == "/image" {
        let request_id = *next_request_id;
        *next_request_id = next_request_id.wrapping_add(1);
        return ShellCommand::Send(ClientMessage::TestImage { request_id });
    }

    if let Some(rest) = line.strip_prefix("/models") {
        let model = rest.trim();
        if model.is_empty() {
            return ShellCommand::Send(ClientMessage::ListModels);
        }
        return ShellCommand::Send(ClientMessage::SetModel {
            model: model.to_string(),
        });
    }

    let request_id = *next_request_id;
    *next_request_id = next_request_id.wrapping_add(1);
    ShellCommand::Send(ClientMessage::RunInput {
        request_id,
        input: line.as_bytes().to_vec(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingText {
    pub request_id: u32,
    pub reasoning: String,
    pub answer: String,
}

impl StreamingText {
    pub fn new(request_id: u32) -> Self {
        Self {
            request_id,
            reasoning: String::new(),
            answer: String::new(),
        }
    }

    pub fn append(&mut self, stream: OutputStream, chunk: &str) {
        match stream {
            OutputStream::Answer => self.answer.push_str(chunk),
            OutputStream::Reasoning => self.reasoning.push_str(chunk),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownDocument {
    pub blocks: Vec<MarkdownBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    Paragraph(Vec<MarkdownInline>),
    Heading {
        level: u8,
        content: Vec<MarkdownInline>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    BlockQuote(Vec<MarkdownBlock>),
    List {
        ordered: bool,
        start: usize,
        items: Vec<Vec<MarkdownBlock>>,
    },
    Table {
        alignments: Vec<MarkdownAlignment>,
        header: Vec<Vec<MarkdownInline>>,
        rows: Vec<Vec<Vec<MarkdownInline>>>,
    },
    Rule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownAlignment {
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownInline {
    Text(String),
    Code(String),
    Emphasis(Vec<MarkdownInline>),
    Strong(Vec<MarkdownInline>),
    Link {
        content: Vec<MarkdownInline>,
        destination: String,
    },
    Image {
        alt: Vec<MarkdownInline>,
        destination: String,
    },
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
    Link {
        destination: String,
        content: Vec<MarkdownInline>,
    },
    Image {
        destination: String,
        alt: Vec<MarkdownInline>,
    },
}

pub fn render_markdown_html(input: &str) -> String {
    let mut html_output = String::new();
    html::push_html(&mut html_output, Parser::new_ext(input, markdown_options()));
    sanitize_html(&html_output)
}

impl MarkdownDocument {
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
                        if let Some(BlockContext::TableCell(cell)) = block_stack.pop()
                            && let Some(BlockContext::TableRow(row)) = block_stack.last_mut()
                        {
                            row.push(cell);
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
                Event::InlineMath(text) | Event::DisplayMath(text) => {
                    push_text(&mut block_stack, &mut inline_stack, &text)
                }
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

    pub fn to_html(&self) -> String {
        render_markdown_html(&self.to_markdown())
    }

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

fn markdown_options() -> Options {
    Options::all()
}

fn sanitize_html(html: &str) -> String {
    let mut sanitizer = HtmlSanitizer::default();
    sanitizer.add_tags(["table", "thead", "tbody", "tr", "th", "td"]);
    sanitizer.add_tag_attributes("th", ["align"]);
    sanitizer.add_tag_attributes("td", ["align"]);
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
    if let Some(context) = stack.last_mut() {
        match context {
            BlockContext::Quote(blocks) | BlockContext::Item(blocks) => blocks.push(block),
            BlockContext::List { .. }
            | BlockContext::Paragraph(_)
            | BlockContext::Heading { .. }
            | BlockContext::CodeBlock { .. }
            | BlockContext::Table { .. }
            | BlockContext::TableRow(_)
            | BlockContext::TableCell(_) => root.push(block),
        }
    } else {
        root.push(block);
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
    if let Some(context) = inline_stack.last_mut() {
        match context {
            InlineContext::Emphasis(content)
            | InlineContext::Strong(content)
            | InlineContext::Link { content, .. }
            | InlineContext::Image { alt: content, .. } => content.push(inline),
        }
        return;
    }

    if let Some(context) = block_stack.last_mut() {
        match context {
            BlockContext::Paragraph(content)
            | BlockContext::Heading { content, .. }
            | BlockContext::TableCell(content) => content.push(inline),
            BlockContext::Item(blocks) => item_paragraph_inlines(blocks).push(inline),
            BlockContext::CodeBlock { code, .. } => match inline {
                MarkdownInline::Text(text) | MarkdownInline::Code(text) => code.push_str(&text),
                MarkdownInline::LineBreak => code.push('\n'),
                MarkdownInline::Emphasis(content)
                | MarkdownInline::Strong(content)
                | MarkdownInline::Link { content, .. }
                | MarkdownInline::Image { alt: content, .. } => {
                    let text = inline_text(&content);
                    code.push_str(&text);
                }
            },
            BlockContext::Quote(_)
            | BlockContext::List { .. }
            | BlockContext::Table { .. }
            | BlockContext::TableRow(_) => {}
        }
    }
}

fn item_paragraph_inlines(blocks: &mut Vec<MarkdownBlock>) -> &mut Vec<MarkdownInline> {
    let needs_paragraph = !matches!(blocks.last(), Some(MarkdownBlock::Paragraph(_)));
    if needs_paragraph {
        blocks.push(MarkdownBlock::Paragraph(Vec::new()));
    }

    match blocks.last_mut() {
        Some(MarkdownBlock::Paragraph(content)) => content,
        _ => unreachable!("item paragraphs are created on demand"),
    }
}

fn inline_text(inlines: &[MarkdownInline]) -> String {
    let mut text = String::new();
    for inline in inlines {
        match inline {
            MarkdownInline::Text(value) | MarkdownInline::Code(value) => text.push_str(value),
            MarkdownInline::Emphasis(content)
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
                markdown.push(' ');
                markdown.push_str(separator);
                markdown.push(' ');
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

#[derive(Debug, Clone)]
pub struct PendingImage {
    metadata: ImageMetadata,
    data: Vec<u8>,
}

impl PendingImage {
    fn new(metadata: ImageMetadata) -> io::Result<Self> {
        let capacity = usize::try_from(metadata.byte_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "image byte length does not fit in memory",
            )
        })?;
        Ok(Self {
            metadata,
            data: Vec::with_capacity(capacity),
        })
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        let expected = usize::try_from(self.metadata.byte_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "image byte length does not fit in memory",
            )
        })?;
        let next_len = self.data.len().saturating_add(chunk.len());
        if next_len > expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("image {} exceeded advertised size", self.metadata.image_id),
            ));
        }
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    fn into_parts(self) -> (ImageMetadata, Vec<u8>) {
        (self.metadata, self.data)
    }
}

#[derive(Debug)]
pub struct ImageAssembler {
    pending: HashMap<(u32, u32), PendingImage>,
}

impl ImageAssembler {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    pub fn start(&mut self, request_id: u32, metadata: ImageMetadata) -> io::Result<()> {
        let key = (request_id, metadata.image_id);
        if self.pending.contains_key(&key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "image {} for request {} already exists",
                    metadata.image_id, request_id
                ),
            ));
        }
        self.pending.insert(key, PendingImage::new(metadata)?);
        Ok(())
    }

    pub fn push_chunk(&mut self, request_id: u32, image_id: u32, data: &[u8]) -> io::Result<()> {
        let pending = self
            .pending
            .get_mut(&(request_id, image_id))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "received image chunk for unknown image {image_id} request {request_id}"
                    ),
                )
            })?;
        pending.push_chunk(data)
    }

    pub fn finish(
        &mut self,
        request_id: u32,
        image_id: u32,
    ) -> io::Result<(ImageMetadata, Vec<u8>)> {
        let pending = self
            .pending
            .remove(&(request_id, image_id))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("received image end for unknown image {image_id} request {request_id}"),
                )
            })?;
        let (metadata, data) = pending.into_parts();
        let actual_len = u64::try_from(data.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "image size does not fit in u64")
        })?;
        if actual_len != metadata.byte_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "image {} for request {} ended with {} bytes but expected {}",
                    image_id, request_id, actual_len, metadata.byte_len
                ),
            ));
        }
        Ok((metadata, data))
    }

    pub fn drop_request(&mut self, request_id: u32) {
        self.pending
            .retain(|(pending_request_id, _), _| *pending_request_id != request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_line() {
        let mut next = 1;
        assert_eq!(parse_input_line("   ", &mut next), ShellCommand::Empty);
        assert_eq!(next, 1);
    }

    #[test]
    fn parses_ping() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":ping", &mut next),
            ShellCommand::Send(ClientMessage::Ping)
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn parses_cancel() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":cancel 42", &mut next),
            ShellCommand::Send(ClientMessage::Cancel { request_id: 42 })
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn rejects_invalid_cancel() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":cancel nope", &mut next),
            ShellCommand::InvalidCancel("nope".to_string())
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn parses_test_image_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/image", &mut next),
            ShellCommand::Send(ClientMessage::TestImage { request_id: 10 })
        );
        assert_eq!(next, 11);
    }

    #[test]
    fn parses_models_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/models", &mut next),
            ShellCommand::Send(ClientMessage::ListModels)
        );
        assert_eq!(next, 10);
    }

    #[test]
    fn parses_set_model_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/models gpt-5.4-nano", &mut next),
            ShellCommand::Send(ClientMessage::SetModel {
                model: "gpt-5.4-nano".to_string(),
            })
        );
        assert_eq!(next, 10);
    }

    #[test]
    fn parses_run_input_and_increments_request_id() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("hello world", &mut next),
            ShellCommand::Send(ClientMessage::RunInput {
                request_id: 10,
                input: b"hello world".to_vec(),
            })
        );
        assert_eq!(next, 11);
    }

    #[test]
    fn streaming_text_appends_to_matching_stream() {
        let mut entry = StreamingText::new(7);
        entry.append(OutputStream::Reasoning, "thinking");
        entry.append(OutputStream::Answer, "hello");
        entry.append(OutputStream::Answer, " world");

        assert_eq!(entry.request_id, 7);
        assert_eq!(entry.reasoning, "thinking");
        assert_eq!(entry.answer, "hello world");
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
    fn image_assembler_tracks_lifecycle() {
        let mut assembler = ImageAssembler::new();
        let metadata = ImageMetadata {
            image_id: 11,
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            byte_len: 4,
            alt: Some("tiny".to_string()),
        };

        assembler.start(7, metadata.clone()).expect("start");
        assembler.push_chunk(7, 11, &[1, 2]).expect("chunk1");
        assembler.push_chunk(7, 11, &[3, 4]).expect("chunk2");
        let (actual_metadata, data) = assembler.finish(7, 11).expect("finish");

        assert_eq!(actual_metadata, metadata);
        assert_eq!(data, vec![1, 2, 3, 4]);
    }
}
