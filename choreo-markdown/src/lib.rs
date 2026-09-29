//! Markdown parsing, serialization, and HTML rendering.
//!
//! The crate is split into focused modules:
//!
//! * this module — the public AST types ([`MarkdownDocument`],
//!   [`MarkdownBlock`], [`MarkdownInline`], [`MarkdownAlignment`]) and the
//!   crate's public re-exports;
//! * [`parse`] — the pulldown-cmark event → AST parser;
//! * [`serialize`] — the AST → markdown re-serializer;
//! * [`html`] — markdown → sanitized HTML rendering;
//! * [`math`] — math/prose classification and the LaTeX → Unicode printer.

use thiserror::Error;

mod html;
mod math;
mod parse;
mod serialize;

#[cfg(test)]
mod tests;

pub use html::render_markdown_html;
pub use math::render_math_pretty;
pub use parse::inline_text;

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
        /// The language annotation, if any (e.g. `"rust"` for a ```` ```rust ```` fence).
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
