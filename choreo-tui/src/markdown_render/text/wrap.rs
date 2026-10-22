//! Wrapping: the two wrappers that turn text into rows of at most a given
//! display width, plus the grapheme hard-splitter they share.
//!
//! They differ in what they are allowed to do to the text. [`wrap_plain_line`]
//! reflows *plain* tool output and preserves every character, whitespace
//! included (code, JSON, and aligned shell output must render exactly as the
//! tool emitted it). [`wrap_styled_line_joined`] reflows *styled* content —
//! markdown paragraphs, ANSI-colored output — so it collapses whitespace runs
//! and drops leading/trailing spaces, because a reflowed sentence has no
//! meaningful indentation.
//!
//! Both record a [`LineJoin`] per emitted row so the selection copy can undo the
//! wrapping and reproduce the original text.

use unicode_segmentation::UnicodeSegmentation;

use super::super::{Line, LineJoin, Span, Style};
use super::measure::{display_width, grapheme_width};

/// Wrap `text` into plain, unstyled lines of at most `width` display columns.
pub(crate) fn plain_text_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    plain_text_lines_joined(text, width).0
}

/// [`plain_text_lines`] plus the per-line [`LineJoin`] metadata the copy
/// path needs to undo the wrapping (see the enum docs).
///
/// Wrapped chunks of one original line are marked [`LineJoin::Join`]:
/// [`wrap_plain_line`] cuts at whitespace boundaries keeping the whitespace
/// run on the previous chunk, so directly concatenating the chunks
/// reproduces the input byte-for-byte (the function doc says exactly that:
/// "concatenating the wrapped lines reproduces the input").  Each original
/// `\n` (and the first chunk of each original line) is [`LineJoin::Break`].
pub(crate) fn plain_text_lines_joined(
    text: &str,
    width: u16,
) -> (Vec<Line<'static>>, Vec<LineJoin>) {
    if text.is_empty() {
        (
            vec![Line::from(Span::styled(String::new(), Style::default()))],
            vec![LineJoin::Break],
        )
    } else {
        let width = width as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut joins: Vec<LineJoin> = Vec::new();
        for raw in text.split('\n') {
            for (i, wrapped) in wrap_plain_line(raw, width).into_iter().enumerate() {
                lines.push(Line::from(Span::styled(wrapped, Style::default())));
                joins.push(if i == 0 {
                    LineJoin::Break
                } else {
                    LineJoin::Join
                });
            }
        }
        (lines, joins)
    }
}

/// Wrap a single plain-text line at `width` display columns so no output line
/// exceeds it, preserving every character (whitespace included) verbatim.
///
/// Unlike [`wrap_styled_line_joined`] — which collapses whitespace runs and
/// drops leading/trailing spaces because it reflows *styled* content — plain
/// tool output (code, JSON, aligned shell output) must render exactly as the
/// tool emitted it.  Breaks happen at whitespace boundaries when one fits
/// within the width; a word wider than the width is hard-split by grapheme
/// cluster.  The greedy pass emits each grapheme exactly once, so
/// concatenating the wrapped lines reproduces the input.
///
/// The pre-wrap is what keeps the rest of the pipeline consistent: the
/// renderer draws lines into a non-wrapping `Paragraph`, and the height math
/// (`wrapped_line_height` = `line_width.div_ceil(width)`) assumes no rendered
/// line exceeds the content width.  `markdown_lines`/`ansi_lines`/the diff
/// renderer all pre-wrap for the same reason — this is the plain-text sibling.
pub(crate) fn wrap_plain_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    // Fast path: the line already fits — one verbatim line, no work.
    if display_width(line) <= width {
        return vec![line.to_string()];
    }
    // A line with no whitespace at all can never break at a word boundary:
    // every overflow is a hard grapheme split, which [`grapheme_chunks`]
    // implements (the shared hard-splitter).  With floor 0 it credits each
    // grapheme exactly its display width — the same measure the main loop
    // uses — so the chunk boundaries coincide exactly, whether or not the
    // run happens to contain a zero-width grapheme (combining marks pass
    // the terminal filter, but ratatui drops them at draw time, so they are
    // genuinely invisible here).  Common for huge single tokens — base64,
    // URLs, minified JSON.  This also keeps the main loop below free of a
    // per-overflow whitespace scan.
    if !line.contains(char::is_whitespace) {
        return grapheme_chunks(line, width, 0);
    }

    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut buf_width = 0usize;
    // Byte length + display width of the last whitespace boundary seen on the
    // current line.  When the line overflows we cut here (keeping the whole
    // whitespace run on the previous line) so words stay whole where possible.
    let mut last_space: Option<(usize, usize)> = None;

    for g in UnicodeSegmentation::graphemes(line, true) {
        let g_width = grapheme_width(g);
        buf.push_str(g);
        buf_width += g_width;
        if g.trim().is_empty() {
            last_space = Some((buf.len(), buf_width));
        }
        if buf_width > width {
            // Prefer cutting at the last whitespace boundary, but only if it
            // itself fits within the width (a boundary beyond it would leave
            // the cut line over-wide); otherwise hard-split at the grapheme
            // that overflowed.  A cut at byte 0 means the single grapheme
            // alone is wider than the line — emit it on its own line.
            let cut = match last_space {
                Some((b, w)) if w <= width => (b, w),
                _ => (buf.len() - g.len(), buf_width - g_width),
            };
            if cut.0 == 0 {
                out.push(std::mem::take(&mut buf));
                buf_width = 0;
            } else {
                // `cut.0` is either a recorded whitespace boundary or the byte
                // offset of the grapheme that overflowed — both are char
                // boundaries within `buf` by construction.
                out.push(buf.get(..cut.0).unwrap_or("").to_string());
                buf = buf.get(cut.0..).unwrap_or("").to_string();
                buf_width -= cut.1;
            }
            last_space = None;
        }
    }
    if !buf.is_empty() || out.is_empty() {
        out.push(buf);
    }
    out
}

/// Hard-split a whitespace-free run into chunks of at most `width` display
/// columns, breaking only at grapheme boundaries.  `floor` is the minimum
/// width credited to a single grapheme: 1 for [`split_word_to_width`] (a lone
/// combining mark still occupies a column when a word renders in isolation),
/// 0 for the plain-text wrapper (where zero-width graphemes are genuinely
/// invisible).  One shared implementation so the two hard-split paths can
/// never drift apart.
pub(crate) fn grapheme_chunks(run: &str, width: usize, floor: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for grapheme in UnicodeSegmentation::graphemes(run, true) {
        let grapheme_width = grapheme_width(grapheme).max(floor);
        if !current.is_empty() && current_width + grapheme_width > width {
            // The next grapheme would push this chunk over the width — flush.
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width += grapheme_width;
        if current_width >= width {
            // A chunk that exactly fills the width is flushed immediately so
            // the next grapheme starts a fresh chunk.
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Running state for [`wrap_styled_line_joined`]: the rows emitted so far and
/// their aligned joins (`out`/`joins`), the row currently being built
/// (`line_spans`/`line_width`), and the join pending for the next row.
///
/// Exists so the wrap loop and its split-word helper share this state through
/// `&mut self` methods rather than threading five distinct `&mut` arguments —
/// the argument count that previously forced a `too_many_arguments` suppression.
struct LineBuilder<'a> {
    out: &'a mut Vec<Line<'static>>,
    joins: &'a mut Vec<LineJoin>,
    line_spans: Vec<Span<'static>>,
    line_width: usize,
    /// The [`LineJoin`] recorded for the row pushed next: a fresh line is
    /// [`LineJoin::Break`]; after a word-boundary flush the next row continues
    /// the sentence ([`LineJoin::Space`]); after a split-word flush it is a
    /// mid-word continuation ([`LineJoin::Join`]).
    pending_join: LineJoin,
}

impl LineBuilder<'_> {
    /// Push the row currently accumulated in `line_spans`, recording the join
    /// pending for it and resetting the width. `next` is the join recorded for
    /// the FOLLOWING row: [`LineJoin::Join`] for a mid-word continuation (the
    /// split-word case), [`LineJoin::Space`] for a row that continues the
    /// sentence at a word boundary.
    fn flush(&mut self, next: LineJoin) {
        self.out
            .push(Line::from(std::mem::take(&mut self.line_spans)));
        self.joins.push(self.pending_join);
        self.line_width = 0;
        self.pending_join = next;
    }

    /// Split an over-long word across rows, used when the word alone does not
    /// fit on the current (possibly just-flushed) row. Each continuation row
    /// joins mid-word ([`LineJoin::Join`]).
    fn push_split_word(&mut self, text: &str, style: Style, max_width: usize) {
        let chunks = split_word_to_width(text, max_width);
        for (ci, chunk) in chunks.iter().enumerate() {
            if ci > 0 {
                // A new row begins with this chunk — a mid-word continuation
                // of the previous row's text.
                self.flush(LineJoin::Join);
            }
            let cw = display_width(chunk);
            self.line_spans.push(Span::styled(chunk.clone(), style));
            self.line_width += cw;
        }
    }
}

/// Word-wrap a pre-styled ratatui line so that no output line exceeds `max_width`.
///
/// Walks the line's styled spans left-to-right, splitting at word (whitespace)
/// boundaries. If a single word is wider than `max_width` it is split by grapheme
/// cluster via [`split_word_to_width`].  Records the [`LineJoin`] of every
/// emitted row in `joins` (aligned with `out`) so the selection copy can undo
/// the wrapping: word-boundary breaks get [`LineJoin::Space`] (the separating
/// whitespace was consumed — the copy re-inserts one space), grapheme-split
/// breaks get [`LineJoin::Join`] (nothing was consumed).
pub(crate) fn wrap_styled_line_joined(
    line: &ratatui::text::Line<'_>,
    max_width: usize,
    out: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
) {
    // ── 1. Tokenize the line into (style, text, is_space) triplets ───────
    //
    // We split each span's content at whitespace boundaries so that we can
    // later break at word boundaries.  Spaces are kept as separate tokens so
    // they can be dropped at line-start or line-end.
    struct StyledToken {
        text: String,
        style: Style,
        is_space: bool,
    }

    let mut tokens: Vec<StyledToken> = Vec::new();

    for span in &line.spans {
        let s = span.content.as_ref();
        let mut current = String::new();
        // Track whether the current run is whitespace or non-whitespace.
        let mut in_space = false;
        for ch in s.chars() {
            let ch_is_space = ch.is_whitespace();
            if ch_is_space != in_space && !current.is_empty() {
                // Finished a run — push the accumulated token.
                tokens.push(StyledToken {
                    text: std::mem::take(&mut current),
                    style: span.style,
                    is_space: in_space,
                });
            }
            current.push(ch);
            in_space = ch_is_space;
        }
        if !current.is_empty() {
            tokens.push(StyledToken {
                text: current,
                style: span.style,
                is_space: in_space,
            });
        }
    }

    if tokens.is_empty() {
        out.push(Line::from(vec![Span::styled(
            String::new(),
            Style::default(),
        )]));
        joins.push(LineJoin::Break);
        return;
    }

    // ── 2. Word-wrap the token stream onto lines of at most max_width ──
    // `out`/`joins` (the function's output vectors) move into the builder;
    // `pending_join` starts at Break — the first row is a fresh line.
    let mut builder = LineBuilder {
        out,
        joins,
        line_spans: Vec::new(),
        line_width: 0,
        pending_join: LineJoin::Break,
    };
    // Did we just add a space at the end?  We keep at most one trailing space
    // so that flush + re-start doesn't introduce a leading space.
    let mut trailing_space = false;

    for token in &tokens {
        if token.is_space {
            // Collapse runs of whitespace to a single space.
            if !builder.line_spans.is_empty() && !trailing_space {
                builder
                    .line_spans
                    .push(Span::styled(" ".to_string(), token.style));
                builder.line_width += 1;
                trailing_space = true;
            }
            continue;
        }

        let word_width = display_width(&token.text);

        if builder.line_width + word_width <= max_width {
            // Fits on the current line.
            trailing_space = false;
            builder
                .line_spans
                .push(Span::styled(token.text.clone(), token.style));
            builder.line_width += word_width;
        } else if builder.line_spans.is_empty() {
            // The word alone is too wide for the empty line — split it.
            trailing_space = false;
            builder.push_split_word(&token.text, token.style, max_width);
        } else {
            // Flush the current line and start a fresh line with this word.
            // The fresh row continues the sentence, so it joins with a space.
            builder.flush(LineJoin::Space);
            trailing_space = false;

            if word_width <= max_width {
                builder
                    .line_spans
                    .push(Span::styled(token.text.clone(), token.style));
                builder.line_width = word_width;
            } else {
                builder.push_split_word(&token.text, token.style, max_width);
            }
        }
    }

    if !builder.line_spans.is_empty() {
        builder
            .out
            .push(Line::from(std::mem::take(&mut builder.line_spans)));
        builder.joins.push(builder.pending_join);
    }
}

/// Split an over-long word into chunks of at most `width` display columns.
pub(crate) fn split_word_to_width(word: &str, width: usize) -> Vec<String> {
    // Hard-split a word with the shared chunker; the floor of 1 keeps a lone
    // zero-width grapheme (e.g. a combining mark in an isolated word) from
    // vanishing entirely from the chunks.
    grapheme_chunks(word, width, 1)
}
