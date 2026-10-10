use crate::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};
use choreo_markdown::render_math_pretty;
use choreo_proto::{ToolResultRecord, Turn};
use choreo_sanitize::is_unsafe_unicode;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use smallvec::SmallVec;
use syntect::easy::HighlightLines;

use std::sync::Arc;

use crate::cache::GlobalLruCache;
use crate::diff_render::try_render_diff_content;
use crate::syntax::{highlight_theme, syntax_set, to_ratatui_color};
use tracing::{debug, warn};

mod assemble;
mod block;
mod code;
mod incremental;
mod inline;
mod list;
mod tables;
mod text;

// Re-export every submodule item so siblings, the test module, and the rest of
// the crate reach them through `markdown_render::…` (the paths external callers
// such as `render/` and `state/` already use).  Single-segment glob re-exports
// (`use text::*`) are the idiomatic way to hoist a private submodule's API into
// its parent.
pub(crate) use assemble::*;
pub(crate) use block::*;
pub(crate) use code::*;
pub(crate) use incremental::*;
pub(crate) use inline::*;
pub(crate) use list::*;
pub(crate) use tables::*;
pub(crate) use text::*;

/// The block-quote gutter: a light vertical bar plus one space.  Two display
/// columns wide — exactly the footprint of the legacy literal `"> "` marker —
/// so the content width handed to a quote body (`width - indent - 2`) and
/// every wrap-budget test stay valid.
const QUOTE_BAR: &str = "│ ";

/// Display width of [`QUOTE_BAR`], in columns — the bar glyph plus the
/// trailing space.  Kept as a constant so the copy-range arithmetic never
/// re-measures the string per row.
const QUOTE_BAR_WIDTH: usize = 2;

/// Colour of the block-quote bar.  `DarkGray` reads as a quiet rule against
/// the message's shaded background (#353535) — the same colour the diff
/// renderer uses for its own `│` gutter (see `diff_render.rs`).
const QUOTE_BAR_COLOR: Color = Color::DarkGray;

/// How a rendered line joins the rendered line *before* it when both end up
/// in a copied selection.
///
/// The renderer pre-wraps long content so nothing overflows the viewport — a
/// single original line (a markdown paragraph, a plain-text line, a code
/// line) becomes several rendered rows.  A naive copy that separates every
/// row with a newline therefore reproduces the *wrapped* text instead of the
/// original.  Every rendered line records how it must glue to its predecessor
/// to undo the renderer's wrapping:
///
/// - [`LineJoin::Break`] — a fresh paragraph/block (or a genuinely separate
///   line: the next item of a list, the next line of a code block, the next
///   row of a table).  The copy separates the two rows with a newline.
/// - [`LineJoin::Space`] — a wrapped continuation that broke at a word
///   boundary; the reflow consumed the separating whitespace (and the caller
///   may or may not keep a placeholder of it in the rendered spans).  The
///   copy trims both rows at the seam and re-inserts exactly one space.
/// - [`LineJoin::Join`] — a wrapped continuation that broke mid-word (a hard
///   grapheme split of an over-long word, or a plain-text wrap, which keeps
///   its whitespace on the previous row).  The copy concatenates the two rows
///   directly, preserving whatever whitespace the rows already carry.
///
/// The vector is aligned with [`RenderedTurnLines::lines`]: `joins[i]`
/// describes row `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LineJoin {
    #[default]
    Break,
    Space,
    Join,
}

/// Identity of a rendered line as a row of a data table.
///
/// Emitted alongside a table row's chrome so the selection can group a table's
/// wrapped display lines back into rows and cells (a cell's text is the rejoin
/// of that cell across the row's display lines) without a separate per-line
/// buffer.  A line with `None` is not part of any table; the frame/separator
/// rules carry [`TableRowId::RULE`] as their row index so a contiguous run of
/// table lines stays unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableRowId {
    /// Table ordinal, unique across the whole rendered session history (every
    /// turn of every table), so the selection can never merge two distinct
    /// tables into one reading-order run.  Allocated from a monotonic counter
    /// thread-local to the render thread (see [`next_table_id`]).
    pub table: u32,
    /// Row index within the table, 0 = header; [`TableRowId::RULE`] marks a
    /// frame/separator rule (no cells).
    pub row: u32,
}

impl TableRowId {
    /// Sentinel row index for a table's frame/separator rules.
    pub(crate) const RULE: u32 = u32::MAX;

    /// Whether this identity is a table rule row rather than a data row.
    pub(crate) fn is_rule(self) -> bool {
        self.row == Self::RULE
    }
}

/// Display-column intervals of a rendered line that are **non-selectable
/// chrome** (the value is relative to the line buffer its producer built).
///
/// The renderer is the single source of truth for what is chrome: it emits
/// these intervals per line as first-class, typed metadata so the
/// selection/copy machinery can subtract them without re-scanning the
/// finished [`Line`]'s spans for a magic `(content string, foreground
/// colour)` pair.  A block-quote bar (`QUOTE_BAR`), a fenced-code box's
/// frame and padding (see `render_code_box`), and the `│` borders and frame of
/// a data table are the producers today.
///
/// The record also carries the line's [`TableRowId`] (if any) together with
/// the row line's **per-cell copy-joins**: it all travels with the chrome
/// through [`LineChrome::extend_shifted`] and the render cache, so the
/// selection can regroup a table's wrapped rows into cells *and* rejoin each
/// cell to its original text without a separate parallel buffer.
///
/// Empty for the overwhelming majority of lines; a [`SmallVec`] keeps the
/// common empty case allocation-free while still allowing more than one
/// interval on the rare row that nests chrome (a block quote inside a list
/// item, or a code box's two frame-plus-fill intervals — the inline capacity
/// of 2 holds both without a heap spill).  Each entry is `(lo, hi)` in display
/// columns, half-open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LineChrome {
    intervals: SmallVec<[(u16, u16); 2]>,
    table: Option<TableRowId>,
    /// Per-cell copy-joins of a data-table *row line*, aligned with the row's
    /// selectable cell bands (the gaps between the `│` borders).  Entry `c`
    /// records how cell `c`'s text on this line glues to the same cell's text
    /// on the row line above: [`LineJoin::Space`] at a word-wrap seam,
    /// [`LineJoin::Join`] at a hard mid-word split, [`LineJoin::Break`] at the
    /// cell's first line or an embedded newline.  The selection's
    /// reading-order fill rejoins a wrapped cell through these, so a hard
    /// split is never given a spurious space.  Empty for every non-table line.
    cell_joins: SmallVec<[LineJoin; 4]>,
}

impl LineChrome {
    /// Record one chrome interval `[lo, hi)` in display columns.
    ///
    /// Columns are viewport-bounded, so `u16` is ample; the cast saturates
    /// (mirroring the renderer's other viewport-bounded width casts) and a
    /// `debug_assert!` flags an out-of-range value in development builds.
    pub(crate) fn push(&mut self, lo: usize, hi: usize) {
        debug_assert!(
            u16::try_from(lo).is_ok() && u16::try_from(hi).is_ok(),
            "chrome column out of u16 range"
        );
        self.intervals.push((
            u16::try_from(lo).unwrap_or(u16::MAX),
            u16::try_from(hi).unwrap_or(u16::MAX),
        ));
    }

    /// Append `other`'s intervals translated right by `by` columns, and adopt
    /// `other`'s table identity if this record has none.
    ///
    /// Used by the producers that prepend a prefix in front of already-built
    /// inner rows (the `List` marker, the `BlockQuote` bar, and the assembly
    /// layer's `"  ┃  "` gutter) to move the inner chrome into the emitted
    /// row's column space, so nested chrome accumulates rather than being lost
    /// (and a table nested in a list/quote keeps its row identity).
    pub(crate) fn extend_shifted(&mut self, other: &LineChrome, by: usize) {
        for &(lo, hi) in &other.intervals {
            self.push(usize::from(lo) + by, usize::from(hi) + by);
        }
        if self.table.is_none() {
            self.table = other.table;
        }
        if self.cell_joins.is_empty() {
            self.cell_joins.clone_from(&other.cell_joins);
        }
    }

    /// True when the line has no non-selectable chrome.
    pub(crate) fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    /// The recorded chrome intervals, in display columns.
    pub(crate) fn intervals(&self) -> &[(u16, u16)] {
        &self.intervals
    }

    /// The data-table row this line belongs to, if any.
    pub(crate) fn table(&self) -> Option<TableRowId> {
        self.table
    }

    /// Tag this record as part of `id`'s table row.
    pub(crate) fn set_table(&mut self, id: TableRowId) {
        self.table = Some(id);
    }

    /// Record one data-table cell's copy-join for this row line, in column
    /// order (aligned with the row's selectable cell bands).
    pub(crate) fn push_cell_join(&mut self, join: LineJoin) {
        self.cell_joins.push(join);
    }

    /// The per-cell copy-joins of a data-table row line, in column order.
    pub(crate) fn cell_joins(&self) -> &[LineJoin] {
        &self.cell_joins
    }
}

/// A turn rendered into styled lines, plus the metadata the TUI needs to
/// hit-test the collapsible reasoning header without re-scanning the output.
pub(crate) struct RenderedTurnLines {
    pub lines: Vec<Line<'static>>,
    /// Per-line [`LineJoin`] copy metadata, aligned with `lines` (`None` has
    /// no analogue here — every row, chrome or content, carries a join; the
    /// selection clamps chrome rows away instead).  The selection extraction
    /// uses this to rejoin wrapped continuations into the original text.
    pub joins: Vec<LineJoin>,
    /// Display-column range `(start, end)` of each line's meaningful content,
    /// aligned with `lines` — the text the user sees as content, excluding UI
    /// chrome such as the `┃` margin gutter, indents, and trailing fill.
    /// `None` for pure-chrome rows (box separators/padding, blank spacers).
    /// Mouse selection highlights and copies only these cells, so dragging
    /// over an assistant response never grabs the box around it.
    pub content_ranges: Vec<Option<(usize, usize)>>,
    /// Per-line [`LineChrome`] copy metadata, aligned with `lines`: the
    /// display-column intervals of each row that are **non-selectable chrome**
    /// (renderer-emitted, see [`LineChrome`]).  The selection/copy machinery
    /// subtracts these from the row's `content_ranges` interval so a drag over
    /// a block quote copies the text and never the `│ ` bar.  Empty for the
    /// overwhelming majority of rows.
    pub chrome_ranges: Vec<LineChrome>,
    /// Semantic-line index of the reasoning header line within `lines`,
    /// present iff the turn has non-whitespace reasoning content.  The
    /// index is stable across collapse/expand (the header is always the
    /// first reasoning line; expansion only appends body lines *after* it),
    /// so it can be cached alongside the rendered lines.
    pub reasoning_header_idx: Option<usize>,
    /// Semantic-line index of each tool result's header line within `lines`,
    /// one entry per result in `turn.tool_results` order (empty when the
    /// turn has no tool results or short-circuits on the error block).
    /// Every result renders exactly one header row — the first line of the
    /// invocation description (or the label fallback); any continuation
    /// lines of a multi-line description follow it in `lines` and are
    /// always visible.  A result's header index depends on the body lengths
    /// of the results *before* it, so indexes are only meaningful for the
    /// collapse state they were rendered with — the cache key (and the
    /// per-state `TurnLayout` ranges) guard against reuse across states.
    pub tool_result_header_idxs: Vec<usize>,
}

/// Tools whose result content is typically bulky and only meaningful to the
/// LLM (verbatim file contents, raw HTTP responses, search matches, rendered
/// web pages, sub-session reports, session listings, shell/exec command
/// output) and would spam the user's session history if rendered in full by
/// default.  Their invocation description (e.g. "Reading file `main.rs`.") is
/// the primary UI summary; the full body is one triangle-click away.
const QUIET_TOOLS: &[&str] = &[
    "read_file",
    "http_request",
    "grep",
    "retrieve_webpage",
    "spawn_subsession",
    "list_sessions",
    // Shell/exec family — every command runner emits the same kind of bulky
    // log output, so they share one collapse default.
    "sh",
    "nushell",
    "fish",
    "powershell",
    "exec",
];

/// Whether a tool result should default to collapsed in the TUI.
///
/// Quiet tools default to collapsed so the header (triangle + invocation
/// description) is the primary view; clicking the triangle reveals the
/// verbatim body.  Error results are never quiet — the error message is
/// the point — and remain expanded by default.
pub(crate) fn tool_result_default_collapsed(record: &ToolResultRecord) -> bool {
    !record.is_error && QUIET_TOOLS.contains(&record.name.as_str())
}

/// Whether a turn's reasoning section should be shown expanded by default.
///
/// Reasoning defaults to expanded only while no response text exists yet
/// (e.g. while it is still streaming); once a response arrives it defaults
/// to collapsed so the response is the primary content.  The user can
/// override this per turn by clicking the reasoning header.
pub(crate) fn reasoning_expanded_default(turn: &Turn) -> bool {
    let has_reasoning = turn
        .assistant_reasoning
        .as_deref()
        .is_some_and(|r| !r.trim().is_empty());
    let has_response = turn
        .assistant_text
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    has_reasoning && !has_response
}

#[cfg(test)]
pub(crate) fn markdown_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    markdown_lines_joined(markdown, width).0
}

/// [`markdown_lines`] plus the per-line [`LineJoin`] copy metadata (see the
/// enum docs) and the aligned per-line [`LineChrome`] buffer.  Wrapped
/// continuations of one paragraph rejoin with a space; paragraph/section
/// boundaries, list items, code lines, and table rows are fresh lines.
pub(crate) fn markdown_lines_joined(
    markdown: &str,
    width: u16,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
    let document = MarkdownDocument::parse(markdown);
    // Normalize heading levels so the document's first heading always renders
    // as level 1.  LLM output sometimes starts a document at `##` (or deeper)
    // instead of `#`; since the decorative prefixes below are anchored to
    // level 1, we shift every heading down by (first_level - 1) so a
    // `## First / ### Sub` document renders as level 1 + level 2.
    let heading_shift =
        first_heading_level(&document.blocks).map_or(0, |level| (level.saturating_sub(1)) as usize);
    render_document_lines(&document, width, heading_shift)
}

/// Render an already-parsed, heading-shift-normalized [`MarkdownDocument`] into
/// styled lines plus the aligned per-line copy metadata.
///
/// This is the body shared by [`markdown_lines_joined`] and the incremental
/// streaming renderer ([`IncrementalMarkdown`]), which parse their input in
/// pieces but must produce byte-identical output to a whole-document render.
/// Trailing blank rows are dropped and an empty document yields the single
/// blank placeholder row, so a caller never hands the selection an empty
/// buffer.
pub(crate) fn render_document_lines(
    document: &MarkdownDocument,
    width: u16,
    heading_shift: usize,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
    let (mut lines, mut joins, mut chrome) =
        render_blocks_untrimmed(document, width, heading_shift);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
        joins.push(LineJoin::Break);
        chrome.push(LineChrome::default());
    }
    while matches!(lines.last(), Some(line) if line_is_blank(line)) {
        lines.pop();
        joins.pop();
        chrome.pop();
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
        joins.push(LineJoin::Break);
        chrome.push(LineChrome::default());
    }
    (lines, joins, chrome)
}

/// Render a document's blocks **without** [`render_document_lines`]'s trailing
/// blank-row strip and empty-document placeholder.
///
/// The incremental streaming renderer commits source pieces one at a time; a
/// piece's trailing blank rows (e.g. the blank line an empty heading renders)
/// are interior once a later piece is spliced on, so stripping them per piece
/// would drop rows a whole-document render keeps.  Only the true tail piece —
/// the end of the document — is stripped.
pub(crate) fn render_blocks_untrimmed(
    document: &MarkdownDocument,
    width: u16,
    heading_shift: usize,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
    let mut lines = Vec::new();
    let mut joins = Vec::new();
    let mut chrome = Vec::new();
    render_markdown_blocks(
        &document.blocks,
        &mut lines,
        &mut joins,
        &mut chrome,
        0,
        width as usize,
        heading_shift,
    );
    (lines, joins, chrome)
}

/// True when a rendered line is visually blank: every span is empty or
/// whitespace-only.  Indented blanks count as blank even though they have
/// nonzero width — e.g. a nested list's after-margin rendered as a
/// continuation line inside an outer item.
fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// How a rendered row's copy content classifies for the selection, given its
/// renderer-emitted [`LineChrome`].
///
/// The assembly layer (`add_margin_lines` and the tool-body loop) turns this
/// into a `content_ranges` entry.  A single [`line_is_blank`] check no longer
/// suffices: a code box's padding row (`│` borders around spaces) is
/// non-whitespace, yet its *non-chrome* columns are blank, so it must still
/// copy as a genuinely blank line.
enum RowContent {
    /// The row is entirely chrome (a box rule, a quote bar with no text): no
    /// selectable cells, so the selection drops the row (`content_range =
    /// None`).
    Chrome,
    /// The row's non-chrome columns are empty or whitespace-only: an empty
    /// content range, so the selection copies it as a blank line rather than
    /// dropping it.
    Blank,
    /// The row carries real content: the full base range.
    Content,
}

/// Classify `line`'s copy content against its recorded `chrome`.
///
/// A row with no chrome keeps the historical [`line_is_blank`] rule (a plain
/// spacer is blank, a text/code row is content) so no existing selectable
/// result changes.  A row with chrome is *pure* chrome when the chrome covers
/// its whole width, *blank* when its non-chrome columns are whitespace-only,
/// and content otherwise.
fn classify_row_content(line: &Line<'_>, chrome: &LineChrome) -> RowContent {
    if chrome.is_empty() {
        return if line_is_blank(line) {
            RowContent::Blank
        } else {
            RowContent::Content
        };
    }
    let width = line.width();
    // Pure chrome: the intervals cover every column (the box's top/bottom
    // rules).  A zero-width line is never "covered" — it falls through to the
    // blank arm below, matching the legacy blank-spacer behaviour.  The
    // covered width is merged first so overlapping/touching intervals are not
    // double-counted into a false "covers the line" verdict.
    if width > 0 && chrome_covered_width(chrome) >= width {
        return RowContent::Chrome;
    }
    if non_chrome_is_blank(line, chrome.intervals()) {
        RowContent::Blank
    } else {
        RowContent::Content
    }
}

/// The total display width covered by `chrome`'s intervals, merging any that
/// overlap or touch first so a shared column is not counted twice.
///
/// Used only to decide whether the chrome covers a whole row (see
/// [`classify_row_content`]); producers emit disjoint, ordered intervals
/// today, but merging keeps the verdict correct for any input (and drops
/// empty or inverted intervals rather than crediting them a column).
fn chrome_covered_width(chrome: &LineChrome) -> usize {
    let mut intervals: SmallVec<[(u16, u16); 2]> = chrome.intervals().iter().copied().collect();
    intervals.sort_unstable();
    let mut covered = 0usize;
    // End of the current merged run, or `None` before the first run.
    let mut run_hi: Option<u16> = None;
    for (lo, hi) in intervals {
        if hi <= lo {
            // Empty or inverted interval — credits no column.
            continue;
        }
        match run_hi {
            Some(end) if lo <= end => {
                if hi > end {
                    covered += usize::from(hi - end);
                    run_hi = Some(hi);
                }
            }
            _ => {
                covered += usize::from(hi - lo);
                run_hi = Some(hi);
            }
        }
    }
    covered
}

/// Whether every non-chrome display column of `line` is whitespace.
///
/// Walks the spans with their display widths, skipping the columns named by
/// `chrome`; the first non-whitespace character outside chrome proves the row
/// has content.  Used only on rows that carry chrome (the common empty-chrome
/// case short-circuits in [`classify_row_content`]).
fn non_chrome_is_blank(line: &Line<'_>, intervals: &[(u16, u16)]) -> bool {
    let mut col = 0usize;
    for span in &line.spans {
        for ch in span.content.chars() {
            // Zero-width graphemes occupy no column and never carry content.
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if w == 0 {
                continue;
            }
            let is_chrome = intervals
                .iter()
                .any(|&(lo, hi)| col >= usize::from(lo) && col < usize::from(hi));
            if !is_chrome && !ch.is_whitespace() {
                return false;
            }
            col += w;
        }
    }
    true
}

/// Test-only convenience wrapper over [`ensure_blank_line_joined`]: the
/// margin-collapsing rule (push a blank line unless the last line is already
/// blank, so adjacent blocks that each want vertical space produce at most one
/// blank line) lives in exactly one place, and the tests exercise that same
/// path.  The renderer itself always calls the joined variant so the parallel
/// `joins`/`chrome` buffers stay aligned.
#[cfg(test)]
fn ensure_blank_line(lines: &mut Vec<Line<'static>>) {
    let mut joins = Vec::new();
    let mut chrome = Vec::new();
    ensure_blank_line_joined(lines, &mut joins, &mut chrome);
}

/// [`ensure_blank_line`] keeping the per-line [`LineJoin`] and [`LineChrome`]
/// vectors aligned: every blank row it inserts is a fresh line
/// ([`LineJoin::Break`]) carrying no chrome ([`LineChrome::default`]).
fn ensure_blank_line_joined(
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
) {
    if lines.last().is_none_or(|l| !line_is_blank(l)) {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
        joins.push(LineJoin::Break);
        chrome.push(LineChrome::default());
    }
}

/// Top-level markdown block kind, as far as the incremental streaming renderer
/// cares: whether a blank line adjacent to the block is a genuine, independent
/// block boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Paragraph,
    Heading,
    Rule,
    CodeBlock,
    Table,
    List,
    BlockQuote,
}

impl BlockKind {
    /// Whether a blank line touching a block of this kind can be treated as a
    /// split point for independent re-rendering.
    ///
    /// A list or block quote may *contain* the blank line (a loose list whose
    /// items are blank-separated, or a multi-paragraph quote), so rendering the
    /// two sides separately would drop the block's cross-line state (loose
    /// spacing, marker alignment) and diverge from a whole-document render.
    /// Every other kind ends cleanly at its blank line, so the two sides are
    /// independent.
    pub(crate) fn is_hard(self) -> bool {
        !matches!(self, BlockKind::List | BlockKind::BlockQuote)
    }
}

/// Classify a top-level block for the incremental renderer's boundary check.
pub(crate) fn block_kind(block: &MarkdownBlock) -> BlockKind {
    match block {
        MarkdownBlock::Paragraph(_) => BlockKind::Paragraph,
        MarkdownBlock::Heading { .. } => BlockKind::Heading,
        MarkdownBlock::Rule => BlockKind::Rule,
        MarkdownBlock::CodeBlock { .. } => BlockKind::CodeBlock,
        MarkdownBlock::Table { .. } => BlockKind::Table,
        MarkdownBlock::List { .. } => BlockKind::List,
        MarkdownBlock::BlockQuote(_) => BlockKind::BlockQuote,
    }
}

/// The kind of a document's first top-level block, if it has any.
pub(crate) fn first_block_kind(blocks: &[MarkdownBlock]) -> Option<BlockKind> {
    blocks.first().map(block_kind)
}

/// The kind of a document's last top-level block, if it has any.
pub(crate) fn last_block_kind(blocks: &[MarkdownBlock]) -> Option<BlockKind> {
    blocks.last().map(block_kind)
}

/// Find the level of the first heading in the block tree, walking nested
/// blockquotes and list items in document order.  Returns `None` when the
/// document contains no headings at all.
fn first_heading_level(blocks: &[MarkdownBlock]) -> Option<u8> {
    for block in blocks {
        match block {
            MarkdownBlock::Heading { level, .. } => return Some(*level),
            MarkdownBlock::BlockQuote(inner) => {
                if let Some(level) = first_heading_level(inner) {
                    return Some(level);
                }
            }
            MarkdownBlock::List { items, .. } => {
                for item in items {
                    if let Some(level) = first_heading_level(item) {
                        return Some(level);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Build the decorative prefix for a heading line from its *normalized*
/// level: level 1 has no prefix (the `# ` marker is dropped entirely), level 2
/// gets a single powerline wedge (U+E0B4), and deeper levels get one solid
/// block per extra level stacked before the wedge (`██ ` + title for level 4).
/// Returns `None` for level 1 so the heading text renders flush left.
fn heading_prefix(level: usize) -> Option<String> {
    match level {
        0 | 1 => None,
        2 => Some("\u{e0b4} ".to_string()),
        _ => Some(format!("{}\u{e0b4} ", "█".repeat(level - 2))),
    }
}

#[cfg(test)]
mod tests;
