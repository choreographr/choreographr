//! AST → markdown re-serialization, plus the `Display`/`FromStr` façades.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use crate::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownError, MarkdownInline};

impl MarkdownDocument {
    /// Serialize the AST back to a markdown string.
    ///
    /// The output uses `*` for emphasis, `**` for strong, and standard GFM
    /// formatting throughout.
    #[must_use]
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
