//! Markdown parsing, serialization, and HTML rendering.
//!
//! The crate is split into focused modules:
//!
//! * this module — the public AST types ([`MarkdownDocument`],
//!   [`MarkdownBlock`], [`MarkdownInline`], [`MarkdownAlignment`]) and the
//!   crate's public re-exports;
//! * `parse` — the pulldown-cmark event → AST parser;
//! * `serialize` — the AST → markdown re-serializer;
//! * `html` — markdown → sanitized HTML rendering;
//! * `math` — math/prose classification (`math::detect`) and the LaTeX →
//!   Unicode printer (`math::pretty`).

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

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
        /// The starting index for an ordered list (1-based).  This is the
        /// CommonMark start (the first item's number); it is the fallback for
        /// any item whose own literal number was not captured, so a list whose
        /// source numbers are non-sequential still renders faithfully from
        /// each [`ListItem`]'s own number.
        start: usize,
        /// The list items, each a marker and a sequence of blocks.
        items: Vec<ListItem>,
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

/// One item of a [`MarkdownBlock::List`]: an optional literal marker number
/// plus the item's block content.
///
/// The number is captured from the source so the renderer and serializer can
/// reproduce the exact numbering the author wrote.  CommonMark only preserves a
/// list's *start* number and auto-increments every later item, so recovering the
/// per-item marker is what keeps a displayed ordered list identical to its
/// source (repeated `1.` markers, gaps such as `1. 3. 5.`, and a `N)` delimiter
/// all survive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// The literal 1-based number written for an ordered item (`3` for
    /// `3. text`), or `None` for an unordered item or one built
    /// programmatically without a captured marker.  When `None`, consumers
    /// fall back to the enclosing `List`'s `start`.
    pub number: Option<usize>,
    /// The item's block content (paragraphs, nested lists, code blocks, …).
    pub blocks: Vec<MarkdownBlock>,
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
