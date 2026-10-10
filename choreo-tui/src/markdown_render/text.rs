//! Text utilities for the markdown renderer: plain-text wrapping and
//! sanitization, ANSI handling, tab expansion, and width/height measurement.

use super::{Arc, Line, LineJoin, Span, Style, is_unsafe_unicode, warn};
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
/// Unlike [`wrap_styled_line`] — which collapses whitespace runs and drops
/// leading/trailing spaces because it reflows *styled* content — plain tool
/// output (code, JSON, aligned shell output) must render exactly as the tool
/// emitted it.  Breaks happen at whitespace boundaries when one fits within
/// the width; a word wider than the width is hard-split by grapheme cluster.
/// The greedy pass emits each grapheme exactly once, so concatenating the
/// wrapped lines reproduces the input.
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

    for g in unicode_segmentation::UnicodeSegmentation::graphemes(line, true) {
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
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(run, true) {
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

/// Terminal-safe filter for tool-result content: keeps complete SGR color
/// sequences (`ESC [ params m`) so ANSI coloring still works, plus tabs,
/// newlines, printable ASCII, the joiners, and safe non-ASCII; escapes
/// everything else — C0/C1 controls (including lone CR: a carriage return not
/// followed by a line feed would let hostile content overwrite its own
/// rendered line; a CRLF pair is folded to a single line feed), non-SGR ESC
/// sequences (OSC/DCS/CSI — the terminal-injection vector), the
/// line/paragraph separators U+2028/U+2029, and Unicode format chars (bidi,
/// ZWSP, …) except the joiners — via `char::escape_default` (e.g. `\u{1b}`,
/// `\u{202e}`), so hostile content renders as inert text.
///
/// This is the *sink* defense: it protects the terminal from every tool at
/// once, including the streaming shell/VM tools whose raw output the daemon
/// deliberately does not escape (colors are a feature). The daemon separately
/// escapes the same char classes at the source for the line-oriented tools
/// and for the LLM transcript; the render filter is what makes raw content
/// safe to draw. Escaping happens *before* the `contains("\x1b[")` gate so
/// that only genuine SGR sequences ever reach the ANSI parser.
///
/// Iterates the input with a `Peekable<Chars>` (no intermediate `Vec<char>`),
/// so the per-chunk cost during streaming stays O(chunk) with one output
/// allocation plus one reused ESC-sequence buffer — it is called inside
/// `render_turn_lines` for the in-flight turn on every streamed chunk.
pub(crate) fn sanitize_for_terminal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    // Reused ESC-sequence assembly buffer: one heap allocation for all the
    // color sequences in a chunk, not one per sequence (ANSI-heavy shell
    // output can contain hundreds).
    let mut seq = String::new();
    while let Some(c) = chars.next() {
        // Fold CRLF to a single LF: a carriage return followed by a line feed
        // is a normal line ending, but passing the `\r` through would put a
        // control char in the rendered cell stream (crossterm prints it to the
        // terminal). Folding — the same normalization the daemon's line
        // sanitizers apply — keeps the pair off the wire entirely. A *lone*
        // CR (the overwrite vector) is escaped below.
        if c == '\r' && chars.peek() == Some(&'\n') {
            out.push('\n');
            chars.next(); // consume the '\n'
            continue;
        }
        // Keep a complete SGR sequence (ESC [ params m) verbatim so ANSI
        // coloring survives; every other use of ESC is escaped. A non-SGR
        // CSI (`\x1b[2J`) or OSC (`\x1b]…`) therefore renders as the inert
        // `\u{1b}` followed by literal text instead of reaching the
        // terminal as a live control sequence.
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            seq.clear();
            seq.push('\u{1b}');
            seq.push('[');
            let mut sgr = false;
            loop {
                match chars.peek().copied() {
                    // Intermediate bytes are 0x30-0x3F (digits, ';', ':', …).
                    Some(n) if (0x30..=0x3f).contains(&(n as u32)) => {
                        seq.push(n);
                        chars.next();
                    }
                    // Final byte 0x40-0x7E; only 'm' is SGR.
                    Some(n) if (0x40..=0x7e).contains(&(n as u32)) => {
                        seq.push(n);
                        chars.next();
                        sgr = n == 'm';
                        break;
                    }
                    // EOF or a non-CSI byte — not a CSI sequence at all.
                    _ => break,
                }
            }
            if sgr {
                // Valid SGR — keep the whole sequence verbatim.
                out.push_str(&seq);
            } else {
                // Non-SGR ESC use: render the ESC inert and the consumed
                // `[` + intermediate bytes as plain text (`seq` is
                // ESC + '[' + … , so everything after the leading ESC byte
                // is kept; `.get(1..)` is total because `seq` always starts
                // with the one-byte ESC).
                out.push_str("\\u{1b}");
                out.push_str(seq.get(1..).unwrap_or(""));
            }
            continue;
        }
        if terminal_keeps(c) {
            out.push(c);
        } else {
            out.extend(c.escape_default());
        }
    }
    out
}

/// Standard terminal tab-stop interval.  `unicode-width` reports `\t` as
/// **zero** columns, and ratatui (≥0.30) filters control characters out of
/// `Span::styled_graphemes` entirely, so a literal tab is both mis-measured
/// *and* silently dropped from the rendered Paragraph.  Expanding tabs to
/// spaces at 4-column stops (a common editor convention) makes every
/// downstream width computation (grapheme wrap, `Line::width`, height
/// `div_ceil`, fill padding) measure exactly what ratatui draws, and keeps
/// tab-aligned columns aligned — matching what the raw bytes would have
/// shown on a terminal with 4-wide tab stops.
pub(crate) const TAB_STOP: usize = 4;

/// Replace each `\t` with the spaces needed to reach the next `TAB_STOP`
/// column, tracking the column per logical line (`\n` resets it).  Runs on
/// already-sanitized content, so the only control chars that can reach it
/// are `\t`, `\n`, and the bytes of a complete SGR color sequence (`ESC [`
/// params `m`) — the latter are invisible on screen, so they are copied
/// through *without* advancing the column (counting them would shrink the
/// tab padding after a color code).  Every other char advances the column
/// by its display width.  O(1) for content without tabs (the common case),
/// O(n) with one output allocation otherwise.
pub(crate) fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut col = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => {
                // Advance to the next multiple of TAB_STOP from the line
                // start (the same rule a terminal's default tab stops use).
                let pad = TAB_STOP - (col % TAB_STOP);
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            '\n' => {
                out.push('\n');
                col = 0;
            }
            // A complete SGR sequence (the only ESC use the terminal filter
            // keeps, per [`sanitize_for_terminal`]) is invisible — copy it
            // verbatim and leave the column where it was.  Counting the
            // escape bytes as visible columns would under-expand a tab that
            // follows a color code (e.g. `\x1b[32mred\t` would pad to column
            // 8 of the *raw* text instead of the visible column 3).
            '\u{1b}' => {
                out.push('\u{1b}');
                if chars.peek() == Some(&'[') {
                    out.push('[');
                    chars.next();
                    // Params are 0x30-0x3F, then the final byte 0x40-0x7E
                    // ('m' for SGR).  sanitize_for_terminal only keeps
                    // complete sequences, so the loop always terminates on
                    // the final byte.
                    while let Some(&n) = chars.peek() {
                        out.push(n);
                        chars.next();
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
            }
            _ => {
                out.push(c);
                col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
    }
    out
}

/// Whether a single char passes through the terminal filter unchanged: tabs,
/// newlines, printable ASCII, and safe non-ASCII. Everything else — every
/// C0/C1 control (including lone CR; a CRLF pair is folded to `\n` by
/// [`sanitize_for_terminal`] before this predicate runs), the line/paragraph
/// separators, and the non-joiner format-char spoofing class via the shared
/// [`is_unsafe_unicode`] predicate (owned by `choreo-sanitize`, the same
/// policy the daemon's sanitizers use) — is escaped. SGR sequences are
/// handled separately by the filter (kept whole), so the per-char predicate
/// needs no ESC case.
pub(crate) fn terminal_keeps(c: char) -> bool {
    c == '\t'
        || c == '\n'
        || (c.is_ascii() && (' '..='~').contains(&c))
        || (!c.is_ascii() && !c.is_control() && !is_unsafe_unicode(c))
}

/// Render ANSI-escape-coded text as styled ratatui lines, wrapping at `width`.
/// Falls back to [`plain_text_lines`] on parse failure.
#[cfg(test)]
pub(crate) fn ansi_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    ansi_lines_joined(text, width).0
}

/// [`ansi_lines`] plus the per-line [`LineJoin`] copy metadata (see the enum
/// docs).  Every original `\n` delimited line is a fresh row
/// ([`LineJoin::Break`]); the word-wrap inside an over-long line records
/// [`LineJoin::Space`]/[`LineJoin::Join`] per break via
/// [`wrap_styled_line_joined`].
pub(crate) fn ansi_lines_joined(text: &str, width: u16) -> (Vec<Line<'static>>, Vec<LineJoin>) {
    use ansi_to_tui::IntoText as _;

    let width_usize = width as usize;

    match text.as_bytes().into_text() {
        Ok(t) => {
            let mut result: Vec<Line<'static>> = Vec::new();
            let mut joins: Vec<LineJoin> = Vec::new();
            for line in &t.lines {
                if width_usize == 0 || line.width() <= width_usize {
                    let spans: Vec<Span<'static>> = line
                        .spans
                        .iter()
                        .map(|span| Span::styled(span.content.to_string(), span.style))
                        .collect();
                    result.push(Line::from(spans));
                    joins.push(LineJoin::Break);
                } else {
                    // Word-wrap this over-long line at width.
                    wrap_styled_line_joined(line, width_usize, &mut result, &mut joins);
                }
            }
            if result.is_empty() {
                (
                    vec![Line::from(Span::styled(String::new(), Style::default()))],
                    vec![LineJoin::Break],
                )
            } else {
                (result, joins)
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                text_len = text.len(),
                "failed to parse ANSI escape codes, falling back to plain text"
            );
            plain_text_lines_joined(text, width)
        }
    }
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

pub(crate) fn lines_height(lines: &[Line<'_>], width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    // A single zero-width line still occupies one row in the terminal.
    if lines
        .first()
        .is_some_and(|line| lines.len() == 1 && line.width() == 0)
    {
        return 1;
    }

    lines
        .iter()
        .map(|line| wrapped_line_height(line, width))
        .sum::<usize>()
}

pub(crate) fn split_word_to_width(word: &str, width: usize) -> Vec<String> {
    // Hard-split a word with the shared chunker; the floor of 1 keeps a lone
    // zero-width grapheme (e.g. a combining mark in an isolated word) from
    // vanishing entirely from the chunks.
    grapheme_chunks(word, width, 1)
}

pub(crate) fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Build a right-aligned ordered-list marker: the number is left-padded with
/// spaces to `number_width` display columns so the ones digits stack vertically
/// across the list (the "9" of item 9 sits above the "0" of item 10, not above
/// its "1").  The ". " suffix is a fixed two columns, so every marker is exactly
/// `number_width + 2` wide and every item's content — and every continuation
/// line — starts at the same column.  The pad is pure alignment whitespace: it
/// carries no meaning and is simply what makes the whole list read as one block.
pub(crate) fn ordered_marker(number: usize, number_width: usize) -> String {
    format!("{number:>number_width$}. ")
}

pub(crate) fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.is_empty() {
        0
    } else {
        unicode_width::UnicodeWidthStr::width(grapheme).max(
            grapheme
                .chars()
                .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
                .max()
                .unwrap_or(0),
        )
    }
}

pub(crate) fn wrapped_line_height(line: &Line<'_>, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let line_width = line.width();
    if line_width == 0 {
        1
    } else {
        line_width.div_ceil(width)
    }
}

/// Precompute the cumulative visual-row offset for every semantic line.
///
/// `visual_offsets[i]` = total visual rows covered by `lines[0..=i]`,
/// i.e. the sum of `wrapped_line_height` for each line up to and including `i`.
/// An empty slice is returned when `width` is 0 or when there are no lines.
///
/// The resulting array enables O(log n) visual-row → line-index lookups
/// via `partition_point`.
pub(crate) fn compute_visual_offsets(lines: &[Line<'_>], width: u16) -> Arc<[usize]> {
    let w = width as usize;
    let mut offsets = Vec::with_capacity(lines.len());
    let mut acc = 0;
    for line in lines {
        let h = if w == 0 {
            0
        } else {
            wrapped_line_height(line, w)
        };
        acc += h;
        offsets.push(acc);
    }
    Arc::from(offsets)
}
