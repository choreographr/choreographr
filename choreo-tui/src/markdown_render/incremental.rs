//! Incremental re-rendering of a streaming assistant response.
//!
//! A streaming response only ever grows by appending, so the rendered lines for
//! a *stable prefix* of the source — everything up to the last blank line that
//! is a genuine block boundary — never change from frame to frame.  Parsing and
//! rendering that prefix again on every chunk is what made the streaming path
//! O(response) per frame (O(N²) over the whole response).
//!
//! [`IncrementalMarkdown`] keeps the committed prefix's rendered lines and, on
//! each call, parses and renders only the *tail* after that boundary.  The
//! output is byte-identical to a full [`markdown_lines_joined`](super::markdown_lines_joined):
//! commits happen only at hard boundaries (see
//! [`BlockKind::is_hard`](super::BlockKind::is_hard)), and the inter-block
//! separator that `render_markdown_blocks` would have inserted between the two
//! halves — one blank line, plus a second before a heading — is reproduced
//! manually.
//!
//! Three details keep the splice faithful to a whole-document parse:
//!
//! * **Heading shift.**  The renderer normalizes every heading by the
//!   document's *first* heading level (see [`super::markdown_lines_joined`]).
//!   Since a later piece may carry that first heading, the shift is pinned once
//!   seen and reused for every subsequent piece; a piece rendered before the
//!   first heading appears has no headings, so the un-pinned shift cannot
//!   affect its output.
//! * **Fences.**  A blank line inside a fenced code block is code content, not
//!   a boundary, so the boundary scan tracks fence open/close state across the
//!   tail.
//! * **Indented code.**  An indented (four-column) code block may also contain
//!   blank lines between its lines; a blank line whose preceding non-blank line
//!   is indented is therefore withheld as a boundary (see
//!   [`last_blank_boundary`]).  Withholding a boundary is always safe — a
//!   missed boundary only costs a whole-tail re-render, never a divergent
//!   result — so this stays conservative rather than trying to prove the blank
//!   interior.
//!
//! The source is also normalized incrementally ([`NormalizedSource`]): the
//! same reasoning applies to [`normalize_display_text`](crate::text_filter::normalize_display_text),
//! whose tab expansion makes it stateful along a line.  A line start is the only
//! position at which that state is empty again, so each frame re-normalizes only
//! from the last line start onward, and the cached text stays byte-identical to
//! normalizing the whole source.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{
    BlockKind, LineChrome, LineJoin, first_block_kind, last_block_kind, line_is_blank,
    render_blocks_untrimmed, render_document_lines,
};
use crate::MarkdownDocument;
use crate::text_filter::normalize_display_text;

/// A committed render of a source prefix plus the incremental state needed to
/// extend it.  See the module docs.
pub(crate) struct IncrementalMarkdown {
    /// Incremental normalization cache for the source text.
    source: NormalizedSource,
    /// The committed render and its bookkeeping.
    committed: Committed,
}

/// Incremental normalization cache for a streaming source.
///
/// Holds the raw bytes seen so far and their normalized form, and on each frame
/// re-normalizes only the text from the last line start onward.  A line start is
/// the only raw position at which normalization is independent of what precedes
/// it:
///
/// * tab-stop columns reset at `\n`, and the print column is the whole of the
///   normalizer's run state;
/// * a `\r` whose `\n` has not arrived yet always sits *after* the last line
///   start, so the CRLF-fold decision is re-made from scratch rather than
///   inherited;
/// * an ESC sequence cannot contain `\n`, so no sequence straddles the boundary.
///
/// Invariant: `normalized == normalize_display_text(&raw)` after every update.
/// That is the cache's contract — the streaming render and the whole-document
/// render normalize to the same bytes for the same source, so they splice the
/// same rows.
#[derive(Default)]
struct NormalizedSource {
    /// Every raw byte of the source seen so far.
    raw: String,
    /// The normalized form of [`raw`](Self::raw).
    normalized: String,
}

/// The committed prefix render and the state needed to extend it.  See the
/// module docs.
#[derive(Default)]
struct Committed {
    /// Wrap width of the committed lines; a change resets the cache.
    width: u16,
    /// Stable source prefix the committed lines were rendered from.  It ends
    /// immediately after a blank line outside any fence.
    committed_src: String,
    committed: Vec<Line<'static>>,
    committed_joins: Vec<LineJoin>,
    committed_chrome: Vec<LineChrome>,
    /// Document-wide heading shift, pinned once the first heading is seen.
    /// `None` means no heading has appeared yet, so the shift is still free.
    heading_shift: Option<usize>,
    /// Bytes handed to the parser, for the reuse test.
    #[cfg(test)]
    parsed_bytes: usize,
    /// Whole-source renders (the fallback path), for the reuse test.
    #[cfg(test)]
    full_parses: usize,
}

/// One rendered source piece plus the metadata the caller needs to place it.
struct SegmentRender {
    lines: Vec<Line<'static>>,
    joins: Vec<LineJoin>,
    chrome: Vec<LineChrome>,
    /// The document-wide heading shift if it is now pinned (`Some`), else
    /// `None` (no heading seen in this piece or earlier).
    shift: Option<usize>,
    /// Kind of the piece's first top-level block.
    first: Option<BlockKind>,
    /// Kind of the piece's last top-level block.
    last: Option<BlockKind>,
}

impl Default for IncrementalMarkdown {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalMarkdown {
    pub(crate) fn new() -> Self {
        Self {
            source: NormalizedSource::default(),
            committed: Committed::default(),
        }
    }

    /// Render the source, reusing the committed prefix when possible.
    ///
    /// `src` must be the *whole* response text for this frame (the caller trims
    /// it, matching [`markdown_lines_joined`](super::markdown_lines_joined)).
    /// Byte-identical to `markdown_lines_joined(src, width)` at every prefix.
    pub(crate) fn render(
        &mut self,
        src: &str,
        width: u16,
    ) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
        // Normalize first, and incrementally: tab stops are per line and every
        // commit boundary is a line start, so a piece normalizes to exactly the
        // prefix of normalizing the whole source (see [`NormalizedSource`]).
        self.source.update(src);
        // Split the borrow by field: the normalized text lives in `source` and
        // the commit bookkeeping in `committed`, so the render can read the one
        // while mutating the other.
        let Self { source, committed } = self;
        committed.render(source.text(), width)
    }
}

#[cfg(test)]
impl IncrementalMarkdown {
    /// Bytes handed to the parser across all frames, for the reuse test.
    pub(crate) fn parsed_bytes(&self) -> usize {
        self.committed.parsed_bytes
    }

    /// Whole-source renders (the fallback path), for the reuse test.
    pub(crate) fn full_parses(&self) -> usize {
        self.committed.full_parses
    }

    /// The cached normalized source, so a test can pin the cache's contract:
    /// it always equals normalizing the whole raw source.
    pub(crate) fn normalized_source(&self) -> &str {
        self.source.text()
    }
}

impl NormalizedSource {
    /// The normalized text of every raw byte seen so far.
    fn text(&self) -> &str {
        &self.normalized
    }

    /// Bring the cached text up to date with `src`, which is this frame's whole
    /// source.
    ///
    /// Cost is the appended bytes plus one re-normalization of the *last line*
    /// — not of the whole source — because everything before the last line start
    /// is already correct in both strings.
    fn update(&mut self, src: &str) {
        // A source that does not extend the bytes already seen (a replaced or
        // edited response) has no reusable prefix at all: rebuild from scratch.
        if !src.starts_with(self.raw.as_str()) {
            self.raw.clear();
            self.raw.push_str(src);
            self.normalized.clear();
            self.normalized.push_str(&normalize_display_text(src));
            return;
        }
        let appended = src.get(self.raw.len()..).unwrap_or("");
        if appended.is_empty() {
            return;
        }
        // Resume at the last line start of each string.  The two locate the same
        // boundary: every input `\n` normalizes to exactly one output `\n` and
        // nothing else in the output is one, so `rfind` finds it in both.
        // Scanning back to it touches only the last line — the same region that
        // has to be normalized again anyway.
        let raw_line_start = line_start(&self.raw);
        let out_line_start = line_start(&self.normalized);
        let tail = src.get(raw_line_start..).unwrap_or("");
        let tail_normalized = normalize_display_text(tail);
        // Truncating to the last line start's image drops nothing that is still
        // valid; the re-normalized tail then restores the invariant.
        self.normalized.truncate(out_line_start);
        self.normalized.push_str(&tail_normalized);
        self.raw.push_str(appended);
    }
}

/// Byte offset just past the last line start in `text`: 0 when it carries no
/// newline, because then its only line start is its beginning.
fn line_start(text: &str) -> usize {
    text.rfind('\n').map_or(0, |i| i + 1)
}

impl Committed {
    /// Drop all committed state so the next render starts from scratch.  Used
    /// when the wrap width or the source identity changes.
    fn reset(&mut self) {
        self.committed_src.clear();
        self.committed.clear();
        self.committed_joins.clear();
        self.committed_chrome.clear();
        self.heading_shift = None;
    }

    /// Render one source piece, returning its lines plus the boundary metadata
    /// the caller needs.  Counts parsed bytes and pins the heading shift once a
    /// heading is found.  `trim` selects [`render_document_lines`] (drop
    /// trailing blank rows — only correct for the document's true tail) versus
    /// [`render_blocks_untrimmed`] (keep them — for committed prefix pieces).
    /// The shift is pinned only from a committed region: a heading in the
    /// still-growing tail may change level as it is typed (`#` vs `##`), so its
    /// level must be re-derived every frame until it settles inside a commit.
    fn parse_segment(&mut self, src: &str, width: u16, trim: bool) -> SegmentRender {
        #[cfg(test)]
        {
            self.parsed_bytes += src.len();
        }
        let document = MarkdownDocument::parse(src);
        // An already-pinned shift always wins; otherwise this piece may itself
        // carry the document's first heading.
        let shift = match self.heading_shift {
            Some(pinned) => Some(pinned),
            None => super::first_heading_level(&document.blocks)
                .map(|level| usize::from(level.saturating_sub(1))),
        };
        let effective = shift.unwrap_or(0);
        let (lines, joins, chrome) = if trim {
            render_document_lines(&document, width, effective)
        } else {
            render_blocks_untrimmed(&document, width, effective)
        };
        SegmentRender {
            lines,
            joins,
            chrome,
            shift,
            first: first_block_kind(&document.blocks),
            last: last_block_kind(&document.blocks),
        }
    }

    /// Render the normalized source, reusing the committed prefix when
    /// possible.
    ///
    /// `src` is this frame's whole source *after* [`NormalizedSource`]
    /// normalization, so the committed byte offsets are offsets into the text
    /// the parser sees.  Byte-identical to `markdown_lines_joined(src, width)` at
    /// every prefix.
    fn render(
        &mut self,
        src: &str,
        width: u16,
    ) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
        // A different width invalidates the wrapped lines; a source that is not
        // an extension of the committed prefix (a replaced/edited response)
        // invalidates the committed blocks too.  Either way, start over.
        if width != self.width || !src.starts_with(self.committed_src.as_str()) {
            self.reset();
            self.width = width;
        }

        self.try_commit(src);

        // Nothing stable yet: render the whole source.  This is the first few
        // frames of a response, or a response whose prefix has no hard block
        // boundary (e.g. one giant paragraph or a run of list items).
        if self.committed.is_empty() {
            #[cfg(test)]
            {
                self.full_parses += 1;
            }
            let seg = self.parse_segment(src, width, true);
            return (seg.lines, seg.joins, seg.chrome);
        }

        // Everything after the committed boundary is blank: the whole-document
        // render would strip the committed piece's trailing blanks too, so
        // commit-strip and return.
        let tail = src.get(self.committed_src.len()..).unwrap_or("");
        if tail.trim().is_empty() {
            return self.trimmed_committed();
        }

        let seg = self.parse_segment(tail, width, true);
        // The tail may itself render to nothing (a partial/empty block, e.g. a
        // lone `#`); the whole-document render would then strip it away too.
        if seg.lines.iter().all(line_is_blank) {
            return self.trimmed_committed();
        }
        // The committed prefix is cloned into the assembled output every
        // frame.  That copy is O(committed), but it is not the re-parse the
        // cache exists to avoid: the caller materializes the turn's full
        // rendered lines (the render cache owns them) regardless of this
        // cache, so the clone is the same order as building the output.
        // Parsing is the expensive part, and it is now O(appended).
        let mut lines = self.committed.clone();
        let mut joins = self.committed_joins.clone();
        let mut chrome = self.committed_chrome.clone();
        push_separator(&mut lines, &mut joins, &mut chrome, seg.first);
        lines.extend(seg.lines);
        joins.extend(seg.joins);
        chrome.extend(seg.chrome);
        (lines, joins, chrome)
    }

    /// The committed lines with trailing blank rows dropped, exactly as a
    /// whole-document render would strip them.
    fn trimmed_committed(&self) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
        let mut lines = self.committed.clone();
        let mut joins = self.committed_joins.clone();
        let mut chrome = self.committed_chrome.clone();
        while lines.last().is_some_and(line_is_blank) {
            lines.pop();
            joins.pop();
            chrome.pop();
        }
        if lines.is_empty() {
            push_blank(&mut lines, &mut joins, &mut chrome);
        }
        (lines, joins, chrome)
    }

    /// Advance the committed boundary to the last blank line in the tail whose
    /// preceding block is a hard boundary, rendering and appending that region.
    ///
    /// Only the *last* boundary is considered: an earlier one would have been
    /// committed on an earlier frame, so committing the region up to the last
    /// boundary maximises the reused prefix.  When that boundary's preceding
    /// block is soft (a list or quote the blank line may sit inside), no commit
    /// happens this frame and the tail is re-rendered whole next time — the
    /// documented fallback.
    fn try_commit(&mut self, src: &str) {
        let start = self.committed_src.len();
        if start >= src.len() {
            return;
        }
        let tail = src.get(start..).unwrap_or("");
        let Some(boundary) = last_blank_boundary(tail) else {
            return;
        };
        let Some(region) = tail.get(..boundary) else {
            return;
        };
        if region.trim().is_empty() {
            return;
        }
        let seg = self.parse_segment(region, self.width, false);
        let Some(last) = seg.last else {
            return;
        };
        if !last.is_hard() {
            return;
        }
        push_separator(
            &mut self.committed,
            &mut self.committed_joins,
            &mut self.committed_chrome,
            seg.first,
        );
        self.committed.extend(seg.lines);
        self.committed_joins.extend(seg.joins);
        self.committed_chrome.extend(seg.chrome);
        self.committed_src.push_str(region);
        // A heading here is now a settled (committed) block, so its level — the
        // document's first heading — can be pinned for every later piece.
        self.heading_shift = self.heading_shift.or(seg.shift);
    }
}

/// Append the blank line(s) `render_markdown_blocks` inserts before a block:
/// one blank unless the previous line is already blank (`ensure_blank_line`'s
/// collapse rule), plus a second when the block is a heading.  A no-op before
/// the very first block.
fn push_separator(
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
    first: Option<BlockKind>,
) {
    if lines.is_empty() {
        return;
    }
    // `ensure_blank_line_joined` only inserts when the last row is not blank;
    // the heading spacer below is unconditional, mirroring
    // `render_markdown_blocks`.
    if !lines.last().is_some_and(line_is_blank) {
        push_blank(lines, joins, chrome);
    }
    if first == Some(BlockKind::Heading) {
        push_blank(lines, joins, chrome);
    }
}

fn push_blank(
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
) {
    lines.push(Line::from(Span::styled(String::new(), Style::default())));
    joins.push(LineJoin::Break);
    chrome.push(LineChrome::default());
}

/// Byte offset, within `src`, just past the last blank line that is a genuine
/// block boundary.  `None` when no such line exists.
///
/// A blank line is not a boundary when it is interior to a block that spans it:
/// inside a fenced code block (tracked by [`fence_open`]/[`fence_close`]), or
/// directly after an *indented* (four-column) code line — an indented code
/// block may contain blank lines between its lines, so a blank whose preceding
/// non-blank line is indented could be its interior.  Withholding such a blank
/// is always safe: a missed boundary only costs a whole-tail re-render, never a
/// divergent result.  Because every returned boundary sits outside any fence or
/// indented code block, the scan needs no cross-call state — it starts fresh
/// from the committed boundary each frame.
///
/// The offset points at the first byte after the blank line's terminating
/// newline, i.e. the start of the next line, so `&src[..offset]` ends on a line
/// boundary and `&src[offset..]` begins at a line boundary.  A trailing blank
/// line without a following newline is not counted (it is not a complete line,
/// so it cannot yet be a settled boundary).
fn last_blank_boundary(src: &str) -> Option<usize> {
    // Fence state within this scan; a blank line inside a fence is code, not a
    // boundary.
    let mut fence: Option<(u8, usize)> = None;
    // Whether the most recent non-blank line was an indented code line, so a
    // following blank line may be interior to that code block.
    let mut prev_indented = false;
    let mut last = None;
    let mut offset = 0usize;
    for piece in src.split_inclusive('\n') {
        let complete = piece.ends_with('\n');
        let content = piece.strip_suffix('\n').unwrap_or(piece);
        if let Some((ch, min)) = fence {
            // Inside a fence: only a closing fence changes state, and no blank
            // line here is a boundary.
            if fence_close(content, ch, min) {
                fence = None;
            }
            prev_indented = false;
        } else if let Some(open) = fence_open(content) {
            fence = Some(open);
            prev_indented = false;
        } else if complete && content.trim().is_empty() {
            // A blank line: a boundary unless it may sit inside an indented
            // code block (its preceding non-blank line was an indented code
            // line).
            if !prev_indented {
                last = Some(offset + piece.len());
            }
        } else {
            prev_indented = is_indented_code_line(content);
        }
        offset += piece.len();
    }
    last
}

/// Whether `line` begins (or continues) an indented code block: its leading
/// whitespace reaches column 4.  A tab advances to the next multiple of four,
/// matching CommonMark's indentation rule; a blank line is never a code line.
fn is_indented_code_line(line: &str) -> bool {
    if line.trim().is_empty() {
        return false;
    }
    let mut cols = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => cols += 1,
            '\t' => cols += 4 - (cols % 4),
            _ => return false,
        }
        if cols >= 4 {
            return true;
        }
    }
    false
}

/// Fence opener for `line`: an optional run of up to three leading spaces, then
/// at least three backticks or tildes.  A backtick fence's info string may not
/// contain a backtick (CommonMark).  Returns the fence char and run length.
fn fence_open(line: &str) -> Option<(u8, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let ch = *trimmed.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let count = trimmed.bytes().take_while(|b| *b == ch).count();
    if count < 3 {
        return None;
    }
    if ch == b'`' && trimmed.get(count..)?.contains('`') {
        return None;
    }
    Some((ch, count))
}

/// Whether `line` closes a fence opened with `ch` and minimum run `min`: up to
/// three leading spaces, at least `min` of `ch`, and nothing else but spaces.
fn fence_close(line: &str, ch: u8, min: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    let count = trimmed.bytes().take_while(|b| *b == ch).count();
    count >= min
        && trimmed
            .get(count..)
            .is_some_and(|rest| rest.trim().is_empty())
}
