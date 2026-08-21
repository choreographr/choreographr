use crate::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};
use choreo_proto::{ToolResultRecord, Turn};
use choreo_sanitize::is_unsafe_unicode;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;

use std::sync::Arc;

use crate::cache::GlobalLruCache;
use crate::diff_render::try_render_diff_content;
use crate::render::{BG_SHADE, format_timestamp};
use crate::syntax::{highlight_theme, syntax_set, to_ratatui_color};
use tracing::{debug, warn};

fn find_syntax<'a>(
    ss: &'a syntect::parsing::SyntaxSet,
    lang: &str,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    ss.find_syntax_by_token(lang).or_else(move || match lang {
        "typescript" | "tsx" | "mts" | "cts" => ss.find_syntax_by_token("javascript"),
        "vue" | "svelte" => ss.find_syntax_by_token("html"),
        _ => None,
    })
}

fn highlight_code(language: Option<&str>, code: &str) -> Vec<Line<'static>> {
    static CACHE: GlobalLruCache<(String, String), Vec<Line<'static>>, 200> = GlobalLruCache::new();

    let key = (language.unwrap_or("").to_string(), code.to_string());

    CACHE.get_or_insert_with(&key, || {
        let ss = syntax_set();

        let syntax = language
            .and_then(|lang| find_syntax(ss, lang))
            .unwrap_or_else(|| ss.find_syntax_plain_text());

        let theme = highlight_theme();
        let mut highlighter = HighlightLines::new(syntax, theme);
        let mut result = Vec::with_capacity(code.len().max(1));

        for line in code.split('\n') {
            let Ok(ranges) = highlighter.highlight_line(line, ss) else {
                result.push(Line::from(Span::styled(line.to_string(), Style::default())));
                continue;
            };

            let spans: Vec<Span<'static>> = ranges
                .into_iter()
                .map(|(style, text)| {
                    Span::styled(
                        text.to_string(),
                        Style::default().fg(to_ratatui_color(style.foreground)),
                    )
                })
                .collect();

            result.push(Line::from(spans));
        }

        result
    })
}

// ── Public API ────────────────────────────────────────────────────────────

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
fn wrap_plain_line(line: &str, width: usize) -> Vec<String> {
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
                out.push(buf[..cut.0].to_string());
                buf = buf[cut.0..].to_string();
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
fn grapheme_chunks(run: &str, width: usize, floor: usize) -> Vec<String> {
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
fn sanitize_for_terminal(text: &str) -> String {
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
                // ESC + '[' + … , so `seq[1..]` keeps everything after the
                // ESC byte).
                out.push_str("\\u{1b}");
                out.push_str(&seq[1..]);
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
const TAB_STOP: usize = 4;

/// Replace each `\t` with the spaces needed to reach the next `TAB_STOP`
/// column, tracking the column per logical line (`\n` resets it).  Runs on
/// already-sanitized content, so the only control chars that can reach it
/// are `\t`, `\n`, and the bytes of a complete SGR color sequence (`ESC [`
/// params `m`) — the latter are invisible on screen, so they are copied
/// through *without* advancing the column (counting them would shrink the
/// tab padding after a color code).  Every other char advances the column
/// by its display width.  O(1) for content without tabs (the common case),
/// O(n) with one output allocation otherwise.
fn expand_tabs(text: &str) -> String {
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
fn terminal_keeps(c: char) -> bool {
    c == '\t'
        || c == '\n'
        || (c.is_ascii() && (' '..='~').contains(&c))
        || (!c.is_ascii() && !c.is_control() && !is_unsafe_unicode(c))
}

/// Render ANSI-escape-coded text as styled ratatui lines, wrapping at `width`.
/// Falls back to [`plain_text_lines`] on parse failure.
#[cfg(test)]
fn ansi_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    ansi_lines_joined(text, width).0
}

/// [`ansi_lines`] plus the per-line [`LineJoin`] copy metadata (see the enum
/// docs).  Every original `\n` delimited line is a fresh row
/// ([`LineJoin::Break`]); the word-wrap inside an over-long line records
/// [`LineJoin::Space`]/[`LineJoin::Join`] per break via
/// [`wrap_styled_line_joined`].
fn ansi_lines_joined(text: &str, width: u16) -> (Vec<Line<'static>>, Vec<LineJoin>) {
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

/// Word-wrap a pre-styled ratatui line so that no output line exceeds `max_width`.
///
/// Walks the line's styled spans left-to-right, splitting at word (whitespace)
/// boundaries. If a single word is wider than `max_width` it is split by grapheme
/// cluster via [`split_word_to_width`].  Records the [`LineJoin`] of every
/// emitted row in `joins` (aligned with `out`) so the selection copy can undo
/// the wrapping: word-boundary breaks get [`LineJoin::Space`] (the separating
/// whitespace was consumed — the copy re-inserts one space), grapheme-split
/// breaks get [`LineJoin::Join`] (nothing was consumed).
fn wrap_styled_line_joined(
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
    let mut line_spans: Vec<Span<'static>> = Vec::new();
    let mut line_width = 0usize;
    // Did we just add a space at the end?  We keep at most one trailing space
    // so that flush + re-start doesn't introduce a leading space.
    let mut trailing_space = false;
    // The join recorded for the row that will be pushed into `out` next: the
    // very first row of this input line is a fresh line ([`LineJoin::Break`]);
    // after a word-boundary flush the next row continues the sentence
    // ([`LineJoin::Space`]); after a split-word flush the next row is a
    // mid-word continuation ([`LineJoin::Join`]).
    let mut pending_join = LineJoin::Break;

    /// Push the line currently in `line_spans`, recording the join that was
    /// pending for it, then set the join for the row that follows (a mid-word
    /// continuation — used by split-word handling).
    fn push_current(
        out: &mut Vec<Line<'static>>,
        joins: &mut Vec<LineJoin>,
        line_spans: &mut Vec<Span<'static>>,
        pending_join: &mut LineJoin,
        line_width: &mut usize,
    ) {
        out.push(Line::from(std::mem::take(line_spans)));
        joins.push(*pending_join);
        *line_width = 0;
        *pending_join = LineJoin::Join;
    }

    /// Split an over-long word across lines, used when the word alone does
    /// not fit on the current (possibly just-flushed) line.
    #[allow(clippy::too_many_arguments)] // all args are distinct writer state; a struct would obscure the loop
    fn push_split_word(
        text: &str,
        style: Style,
        max_width: usize,
        out: &mut Vec<Line<'static>>,
        joins: &mut Vec<LineJoin>,
        pending_join: &mut LineJoin,
        line_spans: &mut Vec<Span<'static>>,
        line_width: &mut usize,
    ) {
        let chunks = split_word_to_width(text, max_width);
        for (ci, chunk) in chunks.iter().enumerate() {
            if ci > 0 {
                // A new row begins with this chunk — a mid-word continuation
                // of the previous row's text.
                push_current(out, joins, line_spans, pending_join, line_width);
            }
            let cw = display_width(chunk);
            line_spans.push(Span::styled(chunk.clone(), style));
            *line_width += cw;
        }
    }

    for token in &tokens {
        if token.is_space {
            // Collapse runs of whitespace to a single space.
            if !line_spans.is_empty() && !trailing_space {
                line_spans.push(Span::styled(" ".to_string(), token.style));
                line_width += 1;
                trailing_space = true;
            }
            continue;
        }

        let word_width = display_width(&token.text);

        if line_width + word_width <= max_width {
            // Fits on the current line.
            trailing_space = false;
            line_spans.push(Span::styled(token.text.clone(), token.style));
            line_width += word_width;
        } else if line_spans.is_empty() {
            // The word alone is too wide for the empty line — split it.
            trailing_space = false;
            push_split_word(
                &token.text,
                token.style,
                max_width,
                out,
                joins,
                &mut pending_join,
                &mut line_spans,
                &mut line_width,
            );
        } else {
            // Flush the current line and start a fresh line with this word.
            // The previous row is pushed with the join pending for it; the
            // fresh row continues the sentence, so it joins with a space.
            out.push(Line::from(std::mem::take(&mut line_spans)));
            joins.push(pending_join);
            line_width = 0;
            trailing_space = false;
            pending_join = LineJoin::Space;

            if word_width <= max_width {
                line_spans.push(Span::styled(token.text.clone(), token.style));
                line_width = word_width;
            } else {
                push_split_word(
                    &token.text,
                    token.style,
                    max_width,
                    out,
                    joins,
                    &mut pending_join,
                    &mut line_spans,
                    &mut line_width,
                );
            }
        }
    }

    if !line_spans.is_empty() {
        out.push(Line::from(line_spans));
        joins.push(pending_join);
    }
}

pub(crate) fn lines_height(lines: &[Line<'_>], width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    if lines.len() == 1 && lines[0].width() == 0 {
        return 1;
    }

    lines
        .iter()
        .map(|line| wrapped_line_height(line, width))
        .sum::<usize>()
}

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

/// Tools whose result content is only meaningful to the LLM (verbatim file
/// contents, raw HTTP responses) and would spam the user's session history
/// if rendered in full by default.  Their invocation description (e.g.
/// "Reading file `main.rs`.") is the primary UI summary; the full body is
/// one triangle-click away.
const QUIET_TOOLS: &[&str] = &["read_file", "read_file_range", "http_request"];

/// Whether a tool result should default to collapsed in the TUI.
///
/// Quiet tools default to collapsed so the header (triangle + invocation
/// description) is the primary view; clicking the triangle reveals the
/// verbatim body.  Error results are never quiet — the error message is
/// the point — and remain expanded by default.
pub(crate) fn tool_result_default_collapsed(record: &ToolResultRecord) -> bool {
    !record.is_error && QUIET_TOOLS.contains(&record.name.as_str())
}

/// Render a complete Turn as styled lines suitable for the chat history.
/// Each section (user, assistant, tool results) is wrapped in the margin
/// pattern (top separator, padding, content, padding, bottom separator)
/// with role-specific accent colors.
///
/// `reasoning_expanded` controls whether the turn's reasoning body is shown
/// below its collapsible header (the caller derives this from the default
/// plus any user override — see [`reasoning_expanded_default`]).
///
/// `tool_results_collapsed` holds the per-result collapse state, aligned
/// with `turn.tool_results` (the caller derives it from the default — see
/// [`tool_result_default_collapsed`] — plus any user override).
pub(crate) fn render_turn_lines(
    turn: &Turn,
    content_width: u16,
    tool_content_width: u16,
    reasoning_expanded: bool,
    tool_results_collapsed: &[bool],
) -> RenderedTurnLines {
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    // Per-line content column ranges, aligned with `all_lines` (see
    // `RenderedTurnLines::content_ranges`).  Every line kind below records
    // where its real text starts/ends so selection never copies UI chrome.
    let mut all_content_ranges: Vec<Option<(usize, usize)>> = Vec::new();
    // Per-line copy-join metadata, aligned with `all_lines` (see `LineJoin`).
    // The selection extraction uses this to undo the renderer's wrapping.
    let mut all_joins: Vec<LineJoin> = Vec::new();

    // ── User text block (green accent) ───────────────────────
    // Rendered first so a failed request's transcript still shows what the
    // user asked for above the error that stopped it.
    if let Some(ref text) = turn.user_text {
        let (body, body_joins) = markdown_lines_joined(text, content_width);
        let timestamp_ms = Some(turn.created_at.as_millis());
        let (margin_lines, _rows, margin_ranges, margin_joins) =
            add_margin_lines(body, body_joins, content_width, Color::Green, timestamp_ms);
        all_lines.extend(margin_lines);
        all_content_ranges.extend(margin_ranges);
        all_joins.extend(margin_joins);
    }

    // ── Error block (red) ────────────────────────────────────
    // A request-level failure (provider 4xx/5xx, network error, deadline)
    // renders as a red block.  The history Paragraph is non-wrapping — an
    // unwrapped line would clip at the viewport edge, truncating long error
    // text mid-token — so the text is pre-wrapped at the content width via
    // the same plain-text wrapper the tool output uses (preserving every
    // character verbatim); `lines_height`'s div_ceil math then sizes the
    // block correctly.  The body is provider-controlled bytes, so it goes
    // through the same terminal-safety gate as tool output (escape
    // OSC/CSI/control chars, expand tabs) before reaching the screen.
    if let Some(ref err) = turn.error {
        let header = format!("Error: {err}");
        let header = expand_tabs(&sanitize_for_terminal(&header));
        let (lines, joins) = plain_text_lines_joined(&header, content_width);
        let lines: Vec<Line<'static>> = lines
            .into_iter()
            .map(|line| {
                // `plain_text_lines` emits default-styled spans; repaint the
                // whole line red so every continuation matches the header.
                let text: String = line
                    .spans
                    .into_iter()
                    .map(|s| s.content.to_string())
                    .collect();
                Line::from(Span::styled(text, Style::default().fg(Color::Red)))
            })
            .collect();
        for (line, join) in lines.into_iter().zip(joins) {
            // Unboxed error rows: the whole (red) text is content.
            let width = line.width();
            all_content_ranges.push((width > 0).then_some((0, width)));
            all_joins.push(join);
            all_lines.push(line);
        }
        return RenderedTurnLines {
            lines: all_lines,
            joins: all_joins,
            content_ranges: all_content_ranges,
            reasoning_header_idx: None,
            tool_result_header_idxs: Vec::new(),
        };
    }

    // ── Assistant response block (blue accent) ───────────────
    //
    // The response text is the primary content and is rendered first.  The
    // reasoning section sits below it and is collapsible: a header line
    // (arrow glyph + "Reasoning") is always rendered when reasoning content
    // exists, and the reasoning body only when `reasoning_expanded` is true.
    // Reasoning is retained in the turn even after the response streams (see
    // `stream_chunk` in history.rs), so clicking the header lets the user
    // re-expand the thinking after the answer replaces it.  No "Response:"
    // heading is rendered.
    let has_assistant = turn.assistant_text.is_some() || turn.assistant_reasoning.is_some();
    // Semantic-line index of the reasoning header within the final output.
    // The header is always the first reasoning line, so the index is
    // independent of the collapsed/expanded state.
    let mut reasoning_header_idx: Option<usize> = None;
    if has_assistant {
        let mut body: Vec<Line<'static>> = Vec::new();
        let mut body_joins: Vec<LineJoin> = Vec::new();

        let has_reasoning = turn
            .assistant_reasoning
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty());
        let response_present = turn
            .assistant_text
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());

        // Response text — shown whenever present.
        if let Some(ref text) = turn.assistant_text {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let (lines, joins) = markdown_lines_joined(trimmed, content_width);
                body.extend(lines);
                body_joins.extend(joins);
            }
        }

        // Collapsible reasoning header — always shown when reasoning exists
        // so the user can re-expand it.  ▼ = expanded (body shown below the
        // header), ▶ = collapsed (body hidden).  The header is dimmed so it
        // reads as a control rather than message content.
        if has_reasoning {
            if response_present {
                // Separate the response from the reasoning section so they
                // don't merge into one paragraph.
                body.push(Line::from(Span::styled(String::new(), Style::default())));
                body_joins.push(LineJoin::Break);
            }
            let arrow = if reasoning_expanded { "▼" } else { "▶" };
            // Record the header's position *within the body* before pushing
            // it; the final semantic index is resolved after margin wrapping
            // (add_margin_lines prepends a separator + padding row to the
            // body — half of MARGIN_STRUCTURAL_ROWS).
            let header_idx_in_body = body.len();
            body.push(Line::from(vec![
                Span::styled(format!("{arrow} "), Style::default().fg(Color::Gray)),
                Span::styled("Reasoning", Style::default().fg(Color::Gray)),
            ]));
            body_joins.push(LineJoin::Break);
            if reasoning_expanded && let Some(ref reasoning) = turn.assistant_reasoning {
                let (lines, joins) = markdown_lines_joined(reasoning.trim(), content_width);
                body.extend(lines);
                body_joins.extend(joins);
            }
            reasoning_header_idx =
                Some(all_lines.len() + MARGIN_STRUCTURAL_ROWS / 2 + header_idx_in_body);
        }

        // If we have content, wrap with margin lines (no timestamp).
        if !body.is_empty() {
            let (margin_lines, _rows, margin_ranges, margin_joins) =
                add_margin_lines(body, body_joins, content_width, Color::Blue, None);
            all_lines.extend(margin_lines);
            all_content_ranges.extend(margin_ranges);
            all_joins.extend(margin_joins);
        }
    }

    // ── Tool results block (red accent if error, gray otherwise) ─
    //
    // Each tool result is collapsible: a header row (triangle + invocation
    // description, or the standard label when the description is empty)
    // is always rendered, with the body (label row + content) below it only
    // when the result is expanded.  Quiet tools (see
    // `tool_result_default_collapsed`) default to collapsed; everything
    // else — including errors — defaults to expanded.
    let mut tool_result_header_idxs: Vec<usize> = Vec::new();

    /// Tools whose result content is Markdown by design and may therefore be
    /// parsed as markdown. `pdf_to_markdown` emits extracted page text;
    /// `write_file` emits the written file's full contents fenced as a code
    /// block (daemon `tools/fs/write_file.rs`, fence sized by
    /// `fence_content` so file bytes — backtick runs included — can never
    /// close it early, language tag from `ext_to_lang`);
    /// `git_diff`/`git_show`/`git_add`/`edit_file` emit ` ```diff `-fenced
    /// unified diffs (the daemon wraps every diff via `diff_util::generate_diff`
    /// — git tools through `append_fenced_diff`/`git_diff_impl`,
    /// `tools/git/{diff,show,stage}.rs`; `edit_file` inline at
    /// `tools/fs/edit_file.rs`) — parsing those results as markdown is
    /// exactly what lets the renderer's ` ```diff ` handling (see
    /// `render_markdown_block`) turn each fence interior into a
    /// side-by-side/unified diff, and turns `write_file`'s fence into a
    /// syntax-highlighted code block instead of literal fence markers.
    /// Everything else renders as **plain text** —
    /// verbatim — so `**` in a grep match or shell line is data, not emphasis,
    /// and a hostile result cannot weaponize markdown syntax to restyle or
    /// hide part of the output. Fail-closed: a tool not listed here never
    /// reaches the markdown parser, and a ` ```diff ` fence outside one of
    /// these tools is literal data, not diff opt-in.
    const MARKDOWN_TOOLS: &[&str] = &[
        "pdf_to_markdown",
        "git_diff",
        "git_show",
        "git_add",
        "edit_file",
        "write_file",
    ];

    for (i, tr) in turn.tool_results.iter().enumerate() {
        let accent = if tr.is_error {
            Color::Red
        } else {
            Color::Reset
        };
        let label = if tr.is_error {
            "tool error"
        } else {
            "tool result"
        };
        // Per-result collapse state from the caller (aligned with
        // `turn.tool_results`); a missing entry (defensive fallback) is
        // rendered expanded.
        let collapsed = tool_results_collapsed.get(i).copied().unwrap_or(false);
        let arrow = if collapsed { "▶" } else { "▼" };

        let mut body: Vec<Line<'static>> = Vec::new();
        let mut body_joins: Vec<LineJoin> = Vec::new();

        // Invocation description rendered as markdown so inline code and
        // emphasis highlight properly.  Its first line becomes the header
        // row (triangle + description); any continuation lines are part of
        // the always-visible summary (see below).  Wrapped two columns
        // narrower than the content width because the header prepends the
        // triangle glyph ("▶ ") to the first line — wrapping at the full
        // width would push the header row past the right edge.
        let (desc_lines, desc_joins) = if tr.invocation_description.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            markdown_lines_joined(
                &tr.invocation_description,
                tool_content_width.saturating_sub(2),
            )
        };
        let desc_len = desc_lines.len();

        // Header row — always rendered so its position is stable across
        // collapse/expand (mirroring the reasoning header).  The triangle
        // sits left of the description; when no description exists (common
        // while streaming) the standard label carries the row instead.
        let header_idx_in_body = body.len();
        let mut header_spans = vec![Span::styled(
            format!("{arrow} "),
            Style::default().fg(Color::Gray),
        )];
        if let Some(first) = desc_lines.first() {
            header_spans.extend(first.spans.iter().cloned());
        } else {
            header_spans.push(Span::styled(
                format!("{label}: {}", tr.name),
                Style::default().fg(accent),
            ));
        }
        body.push(Line::from(header_spans));
        body_joins.push(LineJoin::Break);
        tool_result_header_idxs.push(all_lines.len() + header_idx_in_body);

        // Continuation lines of a multi-line invocation description are
        // part of the always-visible summary: the full description shows
        // even when the body (label row + content) is collapsed behind the
        // triangle.  Only the label + content are toggled by a click.
        if desc_len > 1 {
            body.extend(desc_lines.into_iter().skip(1));
            // `desc_joins[1..]` describe each continuation relative to the
            // line above it.  Since desc[0] now lives on the header row, the
            // first continuation's join applies to the header row itself.
            body_joins.extend(desc_joins.into_iter().skip(1));
        }

        // Expanded body only — a collapsed result is its header row plus
        // the full description; expanding adds the label row and content.
        if !collapsed {
            // The label row is redundant when the header already shows it
            // (the no-description fallback above), so it appears only when
            // the description carried the header.
            if desc_len > 0 {
                body.push(Line::from(Span::styled(String::new(), Style::default())));
                body_joins.push(LineJoin::Break);
                body.push(Line::from(Span::styled(
                    format!("{label}: {}", tr.name),
                    Style::default().fg(accent),
                )));
                body_joins.push(LineJoin::Break);
            }
            // Full content body — rendered for every expanded result.  The
            // old hard "quiet" suppression is now just the default collapse
            // state: expanding a quiet tool reveals the verbatim content.
            if !tr.content.is_empty() {
                body.push(Line::from(Span::styled(String::new(), Style::default())));
                body_joins.push(LineJoin::Break);
                // Terminal-safety gate: escape everything except SGR color
                // sequences so hostile file/URL/shell bytes (OSC clipboard
                // writes, CSI clears, bidi overrides, …) render as inert text
                // regardless of which tool produced them. SGR survives, so
                // ANSI coloring still works below.
                let content = sanitize_for_terminal(&tr.content);
                // Expand tabs to 4-column spaces (see [`expand_tabs`]):
                // unicode-width measures `\t` as 0 columns and ratatui
                // drops control chars at draw time, so a literal tab would
                // vanish *and* leave every width computation (wrap, height,
                // fill padding) mis-measured.  After expansion all four
                // branches below (ansi/diff/markdown/plain) see exact widths.
                let content = expand_tabs(&content);
                // Content with ANSI escape codes gets colored rendering.
                if content.contains("\x1b[") {
                    let (lines, joins) = ansi_lines_joined(&content, tool_content_width);
                    body.extend(lines);
                    body_joins.extend(joins);
                } else if tr.is_error {
                    let (lines, joins) = plain_text_lines_joined(&content, tool_content_width);
                    body.extend(lines);
                    body_joins.extend(joins);
                } else if MARKDOWN_TOOLS.contains(&tr.name.as_str()) {
                    // Tools that emit markdown by design (pdf_to_markdown's
                    // extracted page text, git_diff/git_show/git_add/edit_file's
                    // fenced diffs) keep the styled renderer — a ` ```diff `
                    // fence inside their output renders as a diff via the
                    // CodeBlock arm of `render_markdown_block`. Everything else
                    // is verbatim data and must NOT be re-interpreted as
                    // markdown (see MARKDOWN_TOOLS); there is no content-based
                    // diff or markdown auto-detection anymore.
                    let (lines, joins) = markdown_lines_joined(&content, tool_content_width);
                    body.extend(lines);
                    body_joins.extend(joins);
                } else {
                    let (lines, joins) = plain_text_lines_joined(&content, tool_content_width);
                    body.extend(lines);
                    body_joins.extend(joins);
                }
            }
        }

        // No left indent (the 2-column margin was removed); every row spans
        // the full area width with exactly 1 column of right margin.
        for (line, join) in body.into_iter().zip(body_joins) {
            let mut line = line;
            let content_sum: usize = line.spans.iter().map(|s| s.width()).sum();
            let fill = (tool_content_width as usize).saturating_sub(content_sum);
            line.spans
                .push(Span::styled(" ".repeat(fill), Style::default()));
            line.spans.push(Span::styled(" ", Style::default()));
            // Unboxed rows: content starts at column 0 and ends where the
            // fill begins.  Blank body rows (the renderer's spacer rows and
            // genuinely blank tool-output lines) carry no characters but are
            // *content*, not chrome: they keep an empty `(0, 0)` range so the
            // selection copies the source's blank lines, while the
            // turn-edge separators/padding stay `None` and are dropped.
            let end = content_sum.min(tool_content_width as usize);
            all_content_ranges.push(Some((0, end)));
            all_joins.push(join);
            all_lines.push(line);
        }
    }

    // If no sections produced output, emit a blank line.
    if all_lines.is_empty() {
        all_lines.push(Line::from(Span::styled(String::new(), Style::default())));
        all_content_ranges.push(None);
        all_joins.push(LineJoin::Break);
    }

    RenderedTurnLines {
        lines: all_lines,
        joins: all_joins,
        content_ranges: all_content_ranges,
        reasoning_header_idx,
        tool_result_header_idxs,
    }
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

// ── Margin helpers (reused from current render system) ─────────────────

/// Structural rows: top separator, top padding, bottom padding, bottom separator.
pub(crate) const MARGIN_STRUCTURAL_ROWS: usize = 4;

/// Return type of [`add_margin_lines`]: the wrapped lines, their total
/// height, and the per-line content column ranges and copy-join metadata.
type MarginLines = (
    Vec<Line<'static>>,
    usize,
    Vec<Option<(usize, usize)>>,
    Vec<LineJoin>,
);

/// Wrap content lines with a vertical accent bar on the left and dark-gray
/// background shading.
///
/// Returns the wrapped lines, their total height, and a per-line content
/// column range aligned with the returned lines: `(3, 3 + line.width())` for
/// content rows (the text between the `"┃  "` gutter and the trailing
/// fill), `None` for the structural chrome rows (separator, padding).  The
/// per-line [`LineJoin`] metadata is carried through unchanged: structural
/// chrome rows are fresh lines, content rows keep the join their producer
/// gave them.
fn add_margin_lines(
    lines: Vec<Line<'static>>,
    joins: Vec<LineJoin>,
    content_width: u16,
    accent: Color,
    timestamp_ms: Option<i64>,
) -> MarginLines {
    let gray = Style::default().bg(BG_SHADE);
    let no_shading = Style::default().bg(Color::Reset);
    let accent_line = Style::default().fg(accent).bg(Color::Reset);
    // Rows span `content_width + 6` columns: a flush `┃` gutter + 2-column
    // shading on the left, the text + trailing fill, then 2 shaded + 1 blank
    // column on the right (the blank is the 1-column margin between the
    // viewport and the scrollbar).  The padding row grabs `content_width + 4`
    // of shaded middle so it lines up with every content row.
    let total_width = content_width as usize + 6;
    let shaded_content = content_width as usize + 4;

    // Top separator: no shading
    let separator = Line::from(vec![Span::styled(" ".repeat(total_width), no_shading)]);

    // Padding row: gutter flush with the left edge (the 2-column margin was
    // removed), ending one blank column before the edge — the 1-column margin
    // between the viewport and the scrollbar.
    let padding = Line::from(vec![
        Span::styled("┃", accent_line),
        Span::styled(" ".repeat(shaded_content), gray),
        Span::styled(" ", no_shading),
    ]);

    let mut result = Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    let mut content_ranges: Vec<Option<(usize, usize)>> =
        Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    let mut box_joins: Vec<LineJoin> = Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    result.push(separator);
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);
    result.push(padding.clone());
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);

    for (line, join) in lines.into_iter().zip(joins) {
        let text_width = line.width();
        let fill = (content_width as usize).saturating_sub(text_width);

        // Gutter flush with the left edge (the 2-column margin was removed).
        let mut spans = vec![Span::styled("┃", accent_line), Span::styled("  ", gray)];
        // Content spans — explicitly set bg so they display correctly even without
        // a paragraph-level background.
        spans.extend(
            line.spans
                .into_iter()
                .map(|s| Span::styled(s.content, s.style.bg(BG_SHADE))),
        );
        spans.push(Span::styled(" ".repeat(fill), gray));
        spans.push(Span::styled("  ", gray));
        // Single blank column: the 1-column margin between the viewport and
        // the scrollbar.
        spans.push(Span::styled(" ", no_shading));

        result.push(Line::from(spans));
        // Content occupies columns [3, 3 + text width): after the `┃  `
        // gutter, up to where the trailing fill begins.
        content_ranges.push(Some((3, 3 + text_width)));
        box_joins.push(join);
    }

    result.push(padding);
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);

    // Bottom separator: right-aligned timestamp (user messages only).
    if let Some(ms) = timestamp_ms {
        // format_timestamp expects milliseconds — pass the value through
        // unchanged.  (Dividing by 1000 here rendered every user message
        // as a 1970 date after format_timestamp switched to millis.)
        let ts_text = format_timestamp(ms);
        let ts_len = ts_text.len();
        let left_fill = total_width.saturating_sub(ts_len + 4);
        result.push(Line::from(vec![
            Span::styled(" ".repeat(left_fill), no_shading),
            Span::styled(ts_text, no_shading),
            Span::styled(" ".repeat(4), no_shading),
        ]));
    } else {
        result.push(Line::from(vec![Span::styled(
            " ".repeat(total_width),
            no_shading,
        )]));
    }
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);

    let total_rows = result.len();
    (result, total_rows, content_ranges, box_joins)
}

#[cfg(test)]
pub(crate) fn markdown_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    markdown_lines_joined(markdown, width).0
}

/// [`markdown_lines`] plus the per-line [`LineJoin`] copy metadata (see the
/// enum docs).  Wrapped continuations of one paragraph rejoin with a space;
/// paragraph/section boundaries, list items, code lines, and table rows are
/// fresh lines.
pub(crate) fn markdown_lines_joined(
    markdown: &str,
    width: u16,
) -> (Vec<Line<'static>>, Vec<LineJoin>) {
    let document = MarkdownDocument::parse(markdown);
    // Normalize heading levels so the document's first heading always renders
    // as level 1.  LLM output sometimes starts a document at `##` (or deeper)
    // instead of `#`; since the decorative prefixes below are anchored to
    // level 1, we shift every heading down by (first_level - 1) so a
    // `## First / ### Sub` document renders as level 1 + level 2.
    let heading_shift = first_heading_level(&document.blocks)
        .map(|level| (level.saturating_sub(1)) as usize)
        .unwrap_or(0);
    let mut lines = Vec::new();
    let mut joins = Vec::new();
    render_markdown_blocks(
        &document.blocks,
        &mut lines,
        &mut joins,
        0,
        width as usize,
        heading_shift,
    );
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
        joins.push(LineJoin::Break);
    }
    while matches!(lines.last(), Some(line) if line_is_blank(line)) {
        lines.pop();
        joins.pop();
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
        joins.push(LineJoin::Break);
    }
    (lines, joins)
}

/// True when a rendered line is visually blank: every span is empty or
/// whitespace-only.  Indented blanks count as blank even though they have
/// nonzero width — e.g. a nested list's after-margin rendered as a
/// continuation line inside an outer item.
fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Push a blank (zero-width) line onto `lines` unless the last line is
/// already blank (zero-width or whitespace-only).  This gives us CSS-like
/// margin collapsing: multiple adjacent blocks that each want vertical
/// space produce at most one blank line between them.
#[cfg(test)]
fn ensure_blank_line(lines: &mut Vec<Line<'static>>) {
    if lines.last().is_none_or(|l| !line_is_blank(l)) {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
}

/// [`ensure_blank_line`] keeping the per-line [`LineJoin`] vector aligned:
/// every blank row it inserts is a fresh line ([`LineJoin::Break`]).
fn ensure_blank_line_joined(lines: &mut Vec<Line<'static>>, joins: &mut Vec<LineJoin>) {
    if lines.last().is_none_or(|l| !line_is_blank(l)) {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
        joins.push(LineJoin::Break);
    }
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

fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    indent: usize,
    width: usize,
    heading_shift: usize,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            ensure_blank_line_joined(lines, joins);
        }
        // Headings get a *second* blank line for extra visual separation —
        // except when the heading is the first block (index 0) of the
        // document (or of a nested quote/list context), which must not be
        // preceded by blank lines.  `ensure_blank_line` above supplies the
        // first blank; this push adds the second.
        if index > 0 && matches!(block, MarkdownBlock::Heading { .. }) {
            lines.push(Line::from(Span::styled(String::new(), Style::default())));
            joins.push(LineJoin::Break);
        }
        render_markdown_block(block, lines, joins, indent, width, heading_shift);
    }
}

fn render_markdown_block(
    block: &MarkdownBlock,
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    indent: usize,
    width: usize,
    heading_shift: usize,
) {
    match block {
        MarkdownBlock::Paragraph(content) => {
            let (para_lines, para_joins) =
                inlines_to_lines(content, indent, None, width, Modifier::empty());
            lines.extend(para_lines);
            joins.extend(para_joins);
        }
        MarkdownBlock::Heading { level, content } => {
            // Normalize the raw markdown level by the document-wide shift so
            // the first heading always renders as level 1 (see markdown_lines).
            let normalized = (*level as usize).saturating_sub(heading_shift).max(1);
            let prefix = heading_prefix(normalized);
            // Headings are rendered bold + underlined for visual distinction.
            let (heading_lines, heading_joins) = inlines_to_lines(
                content,
                indent,
                prefix,
                width,
                Modifier::BOLD | Modifier::UNDERLINED,
            );
            lines.extend(heading_lines);
            joins.extend(heading_joins);
        }
        MarkdownBlock::CodeBlock { language, code } => {
            // A ` ```diff ` fence is an explicit opt-in: the emitting tool
            // chose the markdown `diff` language tag, so the fence interior
            // is handed to the diff renderer instead of the generic code
            // block. The renderer is fed *fence interiors only* — the raw
            // `--- ` / `diff --git` auto-detection sniffs no longer run
            // against whole tool outputs, which is what used to misparse
            // `pdf_to_markdown`'s "--- UNTRUSTED …" delimiter as a diff path
            // header. If the interior does not parse as a diff (junk under
            // the tag) we fall through to the literal-fence code path below
            // so the raw text always stays visible.
            if language.as_deref() == Some("diff") {
                let diff_width = u16::try_from(width.saturating_sub(indent)).unwrap_or(u16::MAX);
                // Log the accept/fallback *decision* only — the fence interior
                // itself passes through the diff renderer and can contain
                // arbitrary tool output, so it is never logged here.
                debug!(fence = "diff", "rendering fenced diff interior");
                if let Some(diff_lines) = try_render_diff_content(code, diff_width) {
                    for line in diff_lines {
                        // Mirror the generic code path's indent handling so a
                        // fenced diff inside a blockquote/list stays inside its
                        // container and never overflows the width.
                        if indent > 0 {
                            let mut spans =
                                vec![Span::styled(" ".repeat(indent), Style::default())];
                            // `line` is consumed right after, so move its span
                            // Vec instead of cloning every span of every diff row.
                            spans.extend(line.spans);
                            lines.push(Line::from(spans));
                        } else {
                            lines.push(line);
                        }
                        // Every diff row is a distinct source line — never
                        // reflowed and never space-joined — so the copy
                        // reproduces the diff verbatim.
                        joins.push(LineJoin::Break);
                    }
                    return;
                }
                debug!(
                    fence = "diff",
                    "fence interior not a parseable diff; falling back to literal code block"
                );
            }

            let header = language
                .as_deref()
                .map(|value| format!("```{value}"))
                .unwrap_or_else(|| "```".to_string());
            lines.push(indented_line(indent, header));
            joins.push(LineJoin::Break);

            let max_code_width = width.saturating_sub(indent);
            let highlighted = highlight_code(language.as_deref(), code);
            for hl_line in highlighted {
                // Wrap code block lines that exceed the available width so
                // they don't overflow the terminal.  Uses word-wrap via
                // wrap_styled_line which falls back to grapheme-cluster
                // splitting for words that don't fit.
                if hl_line.width() > max_code_width {
                    let mut wrapped: Vec<Line<'static>> = Vec::new();
                    let mut wrapped_joins: Vec<LineJoin> = Vec::new();
                    wrap_styled_line_joined(
                        &hl_line,
                        max_code_width,
                        &mut wrapped,
                        &mut wrapped_joins,
                    );
                    for (wi, wl) in wrapped.into_iter().enumerate() {
                        // Strip trailing space spans so they don't get rendered
                        // with shading as an extra column outside the code box.
                        let mut spans = wl.spans;
                        while spans.last().is_some_and(|s| s.content.trim().is_empty()) {
                            spans.pop();
                        }
                        if indent > 0 {
                            let mut with_indent =
                                vec![Span::styled(" ".repeat(indent), Style::default())];
                            with_indent.extend(spans);
                            lines.push(Line::from(with_indent));
                        } else {
                            lines.push(Line::from(spans));
                        }
                        // The wrapper accounts for the actual row break type
                        // (Space at word boundaries, Join for hard splits);
                        // the first row of each source line is a fresh line.
                        joins.push(wrapped_joins[wi]);
                    }
                } else if indent > 0 {
                    let mut spans = vec![Span::styled(" ".repeat(indent), Style::default())];
                    spans.extend(hl_line.spans.clone());
                    lines.push(Line::from(spans));
                    joins.push(LineJoin::Break);
                } else {
                    lines.push(hl_line);
                    joins.push(LineJoin::Break);
                }
            }

            lines.push(indented_line(indent, "```".to_string()));
            joins.push(LineJoin::Break);
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            let mut quoted_joins = Vec::new();
            // Content is rendered at (width - indent - 2) so that when "> " and the
            // outer indent are prepended on each line the total stays within `width`.
            render_markdown_blocks(
                blocks,
                &mut quoted,
                &mut quoted_joins,
                0,
                width.saturating_sub(indent + 2),
                heading_shift,
            );
            for (line, _inner_join) in quoted.into_iter().zip(quoted_joins) {
                let mut spans = line.spans.clone();
                spans.insert(0, Span::styled("> ".to_string(), Style::default()));
                lines.push(indented_line_as_spans(indent, spans));
                // Every quoted row is a distinct line in the copy: the "> "
                // prefix is per-row rendering chrome, and re-glueing wrapped
                // quote rows into one line would merge the markers into the
                // text.  Copying proceeds row by row.
                joins.push(LineJoin::Break);
            }
        }
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => {
            // Render every item into its own buffer first so the whole list's
            // spacing can be decided as a unit: if the *majority* of items wrap
            // to more than one visual line, every item pair gets a blank line
            // (paragraph-style); otherwise the list renders tight with no gaps
            // between items.  A uniform rhythm per list reads better than the
            // old per-item spacing (where one long item created a single
            // lopsided gap in an otherwise tight list).
            //
            // The list also shares a single indentation unit: the width of the
            // *widest* marker (the item with the highest number, e.g. 4 columns
            // for "10. ").  Every marker is padded with trailing spaces up to
            // that width ("9. " -> "9.  ") so every item's *content* starts at
            // the same column, and every continuation line is indented to that
            // same column — so first lines and wrapped lines all line up as one
            // block regardless of how many digits each marker has.
            let max_marker_width = if *ordered {
                // Item numbers run start..=start + len - 1, so the widest marker
                // is always the last one — O(1) per list, no per-item scan.
                // `saturating_add` is cheap overflow hardening: CommonMark caps
                // marker digits at 9, so `start` is small today, but the marker
                // arithmetic must never be able to overflow (and panic in debug)
                // if a parser or future input ever allows a larger start.
                items
                    .len()
                    .checked_sub(1)
                    .map(|last| display_width(&format!("{}. ", start.saturating_add(last))))
                    .unwrap_or(0)
            } else {
                display_width("• ")
            };
            let mut rendered_items: Vec<(String, Vec<Line<'static>>, Vec<LineJoin>)> =
                Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    // saturating_add: a huge literal list start must render,
                    // not overflow (see max_marker_width above).
                    format!("{}. ", start.saturating_add(index))
                } else {
                    "• ".to_string()
                };
                // Pad the marker to the list-wide width so first-line content
                // aligns with every other item's first line (and with the
                // continuation lines below).  Without this, "9. " content sits
                // one column left of its "10. " sibling — the wrapped lines
                // lined up, but the visible first line was still misaligned.
                let marker = pad_marker(&marker, max_marker_width);
                let mut rendered = Vec::new();
                let mut rendered_joins = Vec::new();
                // Content is rendered at (width - indent - max_marker_width):
                // with every marker padded to that width, a first line totals
                // exactly `width`, and continuation lines (indented to the same
                // column) fit as well.
                render_markdown_blocks(
                    item,
                    &mut rendered,
                    &mut rendered_joins,
                    0,
                    width.saturating_sub(indent + max_marker_width),
                    heading_shift,
                );
                rendered_items.push((marker, rendered, rendered_joins));
            }

            // Strict majority: more than half of the items must be multi-line
            // for the list to be spaced out.  A tie (e.g. 2 items, 1 wrapping)
            // stays tight because 1 * 2 == 2 is not > 2.
            let multi_line_count = rendered_items
                .iter()
                .filter(|(_, rendered, _)| rendered.len() > 1)
                .count();
            let spaced = multi_line_count * 2 > items.len();

            for (index, (marker, rendered, rendered_joins)) in
                rendered_items.into_iter().enumerate()
            {
                // All continuation lines align under the widest marker so wrapped
                // text lines up across the whole list.
                let continuation_indent = indent + max_marker_width;
                // Zip the item's rows with the joins their inner renderer
                // recorded: the two vectors stay in lockstep by construction,
                // so `joins` below needs no fallback.  The first row (which
                // carries the marker) is consumed with its join — each item
                // starts a fresh line, whatever the inner renderer said about
                // its first line is superseded.
                let mut zipped = rendered.into_iter().zip(rendered_joins);
                if let Some((first, _first_join)) = zipped.next() {
                    let mut spans = vec![Span::styled(
                        format!("{}{}", " ".repeat(indent), marker),
                        Style::default(),
                    )];
                    spans.extend(first.spans.clone());
                    lines.push(Line::from(spans));
                    joins.push(LineJoin::Break);
                } else {
                    lines.push(indented_line(indent, marker));
                    joins.push(LineJoin::Break);
                }
                for (line, join) in zipped {
                    let mut spans = vec![Span::styled(
                        " ".repeat(continuation_indent),
                        Style::default(),
                    )];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                    // Wrapped continuations inside the item rejoin with
                    // Space/Join exactly as the inner renderer recorded
                    // (their predecessor's text is the line above them).
                    joins.push(join);
                }

                // Blank line between items only when the list is spaced out as
                // a whole (majority of items wrap).  Uses ensure_blank_line so
                // consecutive blanks collapse into one (e.g. when a spaced
                // item ends with a nested list that already produced a blank).
                if index + 1 < items.len() && spaced {
                    ensure_blank_line_joined(lines, joins);
                }
            }

            // A list is delimited: emit a collapsing blank line after its
            // items.  Blocks between lists get their separation from the
            // *next* block's before-margin, but a nested list's successor is
            // the next item marker of the enclosing list, which never
            // receives a before-margin — without this margin the marker would
            // run flush against the nested list's last line.  ensure_blank_line
            // collapses this margin with any following before-margin, and
            // markdown_lines strips it when the list is the document's last
            // block, so the rule is invisible everywhere except the boundary
            // that was previously missing it.
            if !items.is_empty() {
                ensure_blank_line_joined(lines, joins);
            }
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => {
            let (table_lines, table_joins) =
                render_table_lines(alignments, header, rows, indent, width);
            lines.extend(table_lines);
            joins.extend(table_joins);
        }
        MarkdownBlock::Rule => {
            lines.push(indented_line(indent, "---".to_string()));
            joins.push(LineJoin::Break);
        }
    }
}

// ── Table rendering ───────────────────────────────────────────────────────

fn render_table_lines(
    alignments: &[MarkdownAlignment],
    header: &[Vec<MarkdownInline>],
    rows: &[Vec<Vec<MarkdownInline>>],
    indent: usize,
    width: usize,
) -> (Vec<Line<'static>>, Vec<LineJoin>) {
    let column_count = alignments
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        return (
            vec![Line::from(Span::styled(String::new(), Style::default()))],
            vec![LineJoin::Break],
        );
    }
    let mut table_rows = Vec::with_capacity(rows.len() + 1);
    table_rows.push(normalize_table_row(header, column_count));
    table_rows.extend(
        rows.iter()
            .map(|row| normalize_table_row(row, column_count)),
    );
    let mut widths = vec![3usize; column_count];
    for row in &table_rows {
        for (index, cell) in row.iter().enumerate() {
            for line in cell.lines() {
                widths[index] = widths[index].max(display_width(line));
            }
        }
    }
    let border_width = column_count * 3 + 1;
    let available = width
        .saturating_sub(indent)
        .max(border_width + column_count);
    let content_budget = available.saturating_sub(border_width).max(column_count);
    shrink_column_widths(&mut widths, content_budget);
    let header_alignment = normalized_alignments(alignments, column_count);
    let mut lines = Vec::new();
    let mut joins = Vec::new();
    lines.push(table_border_line('┌', '┬', '┐', &widths, indent));
    joins.push(LineJoin::Break);
    let (header_lines, header_joins) =
        render_table_row_wrapped(&table_rows[0], &widths, &header_alignment, indent);
    lines.extend(header_lines);
    joins.extend(header_joins);
    lines.push(table_separator_line(&widths, &header_alignment, indent));
    joins.push(LineJoin::Break);
    for (index, row) in table_rows.iter().enumerate().skip(1) {
        let (row_lines, row_joins) =
            render_table_row_wrapped(row, &widths, &header_alignment, indent);
        lines.extend(row_lines);
        joins.extend(row_joins);
        if index < table_rows.len() - 1 {
            lines.push(table_border_line('├', '┼', '┤', &widths, indent));
            joins.push(LineJoin::Break);
        }
    }
    lines.push(table_border_line('└', '┴', '┘', &widths, indent));
    joins.push(LineJoin::Break);
    (lines, joins)
}

fn normalized_alignments(
    alignments: &[MarkdownAlignment],
    column_count: usize,
) -> Vec<MarkdownAlignment> {
    (0..column_count)
        .map(|index| {
            alignments
                .get(index)
                .copied()
                .unwrap_or(MarkdownAlignment::None)
        })
        .collect()
}

fn normalize_table_row(row: &[Vec<MarkdownInline>], column_count: usize) -> Vec<String> {
    (0..column_count)
        .map(|index| {
            row.get(index)
                .map(|cell| inline_plain_text(cell))
                .unwrap_or_default()
        })
        .collect()
}

fn shrink_column_widths(widths: &mut [usize], budget: usize) {
    let min_width = 3usize;
    while widths.iter().sum::<usize>() > budget {
        if let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > min_width)
            .max_by_key(|(_, width)| **width)
        {
            widths[index] -= 1;
        } else {
            break;
        }
    }
}

fn table_border_line(
    left: char,
    middle: char,
    right: char,
    widths: &[usize],
    indent: usize,
) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    for (index, width) in widths.iter().enumerate() {
        text.push_str(&"─".repeat(*width + 2));
        text.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    indented_line(indent, text)
}

fn table_separator_line(
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    indent: usize,
) -> Line<'static> {
    let mut text = String::new();
    text.push('├');
    for (index, width) in widths.iter().enumerate() {
        text.push_str(&alignment_rule_segment(*width, alignments[index]));
        text.push(if index + 1 == widths.len() {
            '┤'
        } else {
            '┼'
        });
    }
    indented_line(indent, text)
}

fn alignment_rule_segment(width: usize, alignment: MarkdownAlignment) -> String {
    let inner = width + 2;
    match alignment {
        MarkdownAlignment::Left => format!(":{}", "─".repeat(inner.saturating_sub(1))),
        MarkdownAlignment::Center => {
            if inner <= 2 {
                ":".repeat(inner)
            } else {
                format!(":{}:", "─".repeat(inner - 2))
            }
        }
        MarkdownAlignment::Right => format!("{}:", "─".repeat(inner.saturating_sub(1))),
        MarkdownAlignment::None => "─".repeat(inner),
    }
}

fn render_table_row_wrapped(
    row: &[String],
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    indent: usize,
) -> (Vec<Line<'static>>, Vec<LineJoin>) {
    let wrapped_cells: Vec<Vec<String>> = row
        .iter()
        .zip(widths.iter())
        .map(|(cell, width)| wrap_cell_text(cell, *width))
        .collect();
    let row_height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let mut lines = Vec::with_capacity(row_height);
    for line_index in 0..row_height {
        let mut text = String::new();
        text.push('│');
        for column_index in 0..widths.len() {
            let cell_line = wrapped_cells[column_index]
                .get(line_index)
                .map(String::as_str)
                .unwrap_or("");
            text.push(' ');
            text.push_str(&pad_aligned(
                cell_line,
                widths[column_index],
                alignments[column_index],
            ));
            text.push(' ');
            text.push('│');
        }
        lines.push(indented_line(indent, text));
    }
    // Every table row (visual or wrapped) is a distinct line in the copy:
    // the cell borders and padding are per-row rendering chrome that must
    // not be re-glueed into a paragraph.
    let joins = vec![LineJoin::Break; lines.len()];
    (lines, joins)
}

fn wrap_cell_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for word in raw_line.split_whitespace() {
            let word_width = display_width(word);
            let separator_width = usize::from(!current.is_empty());
            if current_width + separator_width + word_width <= width {
                if separator_width == 1 {
                    current.push(' ');
                    current_width += 1;
                }
                current.push_str(word);
                current_width += word_width;
            } else if current.is_empty() {
                lines.extend(split_word_to_width(word, width));
            } else {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                if word_width <= width {
                    current.push_str(word);
                    current_width = word_width;
                } else {
                    lines.extend(split_word_to_width(word, width));
                }
            }
        }
        if current.is_empty() {
            lines.push(String::new());
        } else {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_word_to_width(word: &str, width: usize) -> Vec<String> {
    // Hard-split a word with the shared chunker; the floor of 1 keeps a lone
    // zero-width grapheme (e.g. a combining mark in an isolated word) from
    // vanishing entirely from the chunks.
    grapheme_chunks(word, width, 1)
}

fn pad_aligned(text: &str, width: usize, alignment: MarkdownAlignment) -> String {
    let text_width = display_width(text);
    if text_width >= width {
        return text.to_string();
    }
    let remaining = width - text_width;
    let (left, right) = match alignment {
        MarkdownAlignment::Right => (remaining, 0),
        MarkdownAlignment::Center => (remaining / 2, remaining.div_ceil(2)),
        MarkdownAlignment::Left | MarkdownAlignment::None => (0, remaining),
    };
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

pub(crate) fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Right-pad a list marker with trailing spaces to `width` display columns so
/// every item's content starts at the same column regardless of how wide the
/// marker text is ("9. " -> "9.  " when a sibling is "10. ").  The pad is pure
/// alignment whitespace: it carries no meaning and is simply what makes the
/// whole list read as one block.  Markers are short ASCII (digits + ". " or
/// "• "), so column padding via spaces is exact.
fn pad_marker(marker: &str, width: usize) -> String {
    let pad = width.saturating_sub(display_width(marker));
    format!("{}{}", marker, " ".repeat(pad))
}

fn grapheme_width(grapheme: &str) -> usize {
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

fn inline_plain_text(inlines: &[MarkdownInline]) -> String {
    let mut text = String::new();
    append_inline_plain_text(inlines, &mut text);
    text
}

fn append_inline_plain_text(inlines: &[MarkdownInline], text: &mut String) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(value)
            | MarkdownInline::Code(value)
            | MarkdownInline::InlineMath(value)
            | MarkdownInline::DisplayMath(value) => text.push_str(value),
            MarkdownInline::Strikethrough(content)
            | MarkdownInline::Emphasis(content)
            | MarkdownInline::Strong(content) => append_inline_plain_text(content, text),
            MarkdownInline::Link {
                content,
                destination,
            } => {
                append_inline_plain_text(content, text);
                if !destination.is_empty() {
                    text.push_str(" (");
                    text.push_str(destination);
                    text.push(')');
                }
            }
            MarkdownInline::Image { alt, destination } => {
                text.push_str("[image: ");
                append_inline_plain_text(alt, text);
                if !destination.is_empty() {
                    text.push_str("] (");
                    text.push_str(destination);
                    text.push(')');
                } else {
                    text.push(']');
                }
            }
            MarkdownInline::LineBreak => text.push('\n'),
        }
    }
}

// ── Line-building helpers ─────────────────────────────────────────────────

fn indented_line(indent: usize, text: String) -> Line<'static> {
    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::styled(" ".repeat(indent), Style::default()));
    }
    spans.push(Span::styled(text, Style::default()));
    Line::from(spans)
}

fn indented_line_as_spans(indent: usize, mut spans: Vec<Span<'static>>) -> Line<'static> {
    if indent > 0 {
        spans.insert(0, Span::styled(" ".repeat(indent), Style::default()));
    }
    Line::from(spans)
}

fn inlines_to_lines(
    inlines: &[MarkdownInline],
    indent: usize,
    prefix: Option<String>,
    width: usize,
    modifier: Modifier,
) -> (Vec<Line<'static>>, Vec<LineJoin>) {
    let mut lines = Vec::new();
    let mut joins = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;
    if indent > 0 {
        current_spans.push(Span::styled(" ".repeat(indent), Style::default()));
        current_width += indent;
    }
    if let Some(ref prefix) = prefix {
        current_spans.push(Span::styled(prefix.clone(), Style::default()));
        current_width += display_width(prefix);
    }
    let mut needs_separator = false;
    let final_join = {
        let mut ctx = RenderCtx {
            lines: &mut lines,
            joins: &mut joins,
            current: &mut current_spans,
            current_width: &mut current_width,
            needs_separator: &mut needs_separator,
            indent,
            width,
            modifier,
            // The very first line of a paragraph/heading is a fresh line.
            current_join: LineJoin::Break,
        };
        render_inlines_to_lines(inlines, &mut ctx);
        // Snapshot the final line's join before `ctx` drops its borrows of
        // the local vectors below.
        ctx.current_join
    };
    if !current_spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(std::mem::take(&mut current_spans)));
        joins.push(final_join);
    }
    (lines, joins)
}

/// Bundles all mutable state and parameters needed to render a flat list of
/// [`MarkdownInline`] nodes into Ratatui [`Line`]s, automatically wrapping
/// and applying text styling (bold, italic, colours, etc.).
///
/// The `modifier` field accumulates [`Modifier`] flags as we descend into
/// nested containers (emphasis inside bold, etc.).  Each container saves the
/// current modifier, ORs in its own flag, calls back into the renderer, and
/// then restores the original value — giving us correct modifier stacking
/// with no heap allocation.
struct RenderCtx<'a> {
    /// Output buffer — completed lines are pushed here.
    lines: &'a mut Vec<Line<'static>>,
    /// Per-line [`LineJoin`] copy metadata, pushed in lockstep with `lines`.
    joins: &'a mut Vec<LineJoin>,
    /// Spans being accumulated for the line currently being built.
    current: &'a mut Vec<Span<'static>>,
    /// Display width of `current` (updated alongside every push).
    current_width: &'a mut usize,
    /// The [`LineJoin`] of the line currently being built (how it joins the
    /// previously flushed line).  Set when the line is started by
    /// [`RenderCtx::flush_line_with_next`] and recorded when the line is
    /// finally pushed.
    current_join: LineJoin,
    /// Whether the next word needs a space separator before it (set to
    /// `true` after every word/code chunk, reset to `false` on line break
    /// or flush).
    needs_separator: &'a mut bool,
    /// Left-margin width for blockquote / list nesting.
    indent: usize,
    /// Maximum line width (in columns) before wrapping kicks in.
    width: usize,
    /// Active text modifiers inherited from enclosing containers (e.g.
    /// `BOLD` inside `Strong`, `ITALIC` inside `Emphasis`).  Combined
    /// via bitwise-OR when nesting.
    modifier: Modifier,
}

impl<'a> RenderCtx<'a> {
    fn base_style(&self) -> Style {
        Style::default().add_modifier(self.modifier)
    }

    fn push_span(&mut self, content: String, style: Style) {
        self.current.push(Span::styled(content, style));
    }

    /// Push the current line into the output and start a fresh line at indent.
    /// Indent padding uses `Style::default()` (not `base_style()`) because
    /// styling modifiers on whitespace are invisible and could confuse tests
    /// or terminal renderers.
    ///
    /// The just-flushed line is recorded with its own `current_join`; the
    /// caller passes the join the fresh line has toward it: word-wrap →
    /// [`LineJoin::Space`], a hard source line break → [`LineJoin::Break`],
    /// a mid-word split → [`LineJoin::Join`].
    fn flush_line_with_next(&mut self, next_join: LineJoin) {
        self.lines.push(Line::from(std::mem::take(self.current)));
        self.joins.push(self.current_join);
        *self.current_width = self.indent;
        if self.indent > 0 {
            self.current
                .push(Span::styled(" ".repeat(self.indent), Style::default()));
        }
        self.current_join = next_join;
    }

    /// Split `word` into grapheme-cluster chunks to fit the available width on the
    /// *current* line. The caller is responsible for flushing before calling this
    /// when the current line has content that won't leave enough room.
    fn render_word_split(&mut self, word: &str, style: Style) {
        *self.needs_separator = false;
        let available = self.width.saturating_sub(*self.current_width);
        let chunked = split_word_to_width(word, available);
        for (ci, chunk) in chunked.iter().enumerate() {
            if ci > 0 {
                // The next row is a mid-word continuation of the split.
                self.flush_line_with_next(LineJoin::Join);
            }
            self.push_span(chunk.clone(), style);
            *self.current_width += display_width(chunk);
        }
        *self.needs_separator = true;
    }
}

fn render_inlines_to_lines(inlines: &[MarkdownInline], ctx: &mut RenderCtx) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(text) => {
                render_text_inline(text, ctx);
            }
            MarkdownInline::Code(text) => {
                render_code_inline(text, ctx, Color::Cyan);
            }
            MarkdownInline::InlineMath(text) => {
                render_code_inline(text, ctx, Color::Yellow);
            }
            MarkdownInline::DisplayMath(text) => {
                render_code_inline(text, ctx, Color::Magenta);
            }
            MarkdownInline::Strikethrough(content) => {
                render_style_inline(content, ctx, Modifier::CROSSED_OUT);
            }
            MarkdownInline::Emphasis(content) => {
                render_style_inline(content, ctx, Modifier::ITALIC);
            }
            MarkdownInline::Strong(content) => {
                render_style_inline(content, ctx, Modifier::BOLD);
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                render_link_inline(content, destination, ctx);
            }
            MarkdownInline::Image { alt, destination } => {
                let prefix_text = "[image: ";
                let prefix_width = display_width(prefix_text);
                let projected = *ctx.current_width + prefix_width;
                if projected > ctx.width && *ctx.current_width > ctx.indent {
                    // Mid-inline wrap: the image continues the sentence.
                    ctx.flush_line_with_next(LineJoin::Space);
                }
                ctx.push_span(prefix_text.to_string(), Style::default());
                *ctx.current_width += prefix_width;

                render_inlines_to_lines(alt, ctx);

                let suffix = if !destination.is_empty() {
                    format!("] ({destination})")
                } else {
                    "]".to_string()
                };
                let suffix_width = display_width(&suffix);
                let projected = *ctx.current_width + suffix_width;
                if projected > ctx.width && *ctx.current_width > ctx.indent {
                    ctx.flush_line_with_next(LineJoin::Space);
                }
                ctx.push_span(suffix, Style::default());
                *ctx.current_width += suffix_width;
                *ctx.needs_separator = true;
            }
            MarkdownInline::LineBreak => {
                // A hard source line break: the next line is a fresh line.
                ctx.flush_line_with_next(LineJoin::Break);
                *ctx.needs_separator = false;
            }
        }
    }
}

/// Returns `true` if `text` starts with closing punctuation that should
/// directly follow the preceding word without a space (e.g. "." in "**bold**!").
/// Opening quotes, brackets, and alphanumeric/text keep the space.
fn starts_with_closing_punct(text: &str) -> bool {
    text.chars().next().is_some_and(|c| {
        matches!(
            c,
            '.' | ',' | '!' | '?' | ':' | ';' | ')' | ']' | '}' | '\u{2019}' | '\u{201d}'
        )
    })
}

/// Returns `true` if `text` ends with opening punctuation that the following
/// inline content should directly attach to without a space (e.g. "(" in
/// "(**hi**)" renders as "(hi)", not "( hi)"). This is the mirror image of
/// `starts_with_closing_punct`: that helper keeps trailing punctuation glued
/// to the *preceding* word, this one keeps leading punctuation glued to the
/// *following* inline (bold, emphasis, code, links, …). Only checked when the
/// text itself does not end with whitespace, so explicit source spaces like
/// "( **hi** )" are still preserved.
fn ends_with_opening_punct(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|c| matches!(c, '(' | '[' | '{' | '\u{2018}' | '\u{201c}'))
}

fn render_text_inline(text: &str, ctx: &mut RenderCtx) {
    // If the original text does NOT start with whitespace (e.g. "**bold**!"
    // where "!" directly follows the bold), check whether it starts with
    // trailing/closing punctuation that should attach to the preceding word.
    // Opening quotes and brackets should still get a space before them.
    let has_leading_space = text.starts_with(' ') || text.starts_with('\t');
    if !has_leading_space && starts_with_closing_punct(text) {
        *ctx.needs_separator = false;
    }

    let trimmed = text.trim_start();
    let ends_with_space = text.ends_with(' ') || text.ends_with('\t');
    let words: Vec<&str> = if trimmed.is_empty() {
        *ctx.needs_separator = true;
        return;
    } else {
        trimmed.split_whitespace().collect()
    };

    for (i, word) in words.iter().enumerate() {
        let word_width = display_width(word);
        let separator_width = usize::from(*ctx.needs_separator || i > 0);
        let projected = *ctx.current_width + separator_width + word_width;

        if projected > ctx.width && *ctx.current_width > ctx.indent {
            // Word-wrap: the next row continues the sentence.
            ctx.flush_line_with_next(LineJoin::Space);
            *ctx.needs_separator = i > 0;
        }

        if *ctx.current_width + word_width > ctx.width && *ctx.current_width >= ctx.indent {
            ctx.render_word_split(word, ctx.base_style());
            continue;
        }

        if (*ctx.needs_separator || i > 0)
            && !ctx.current.is_empty()
            && *ctx.current_width > ctx.indent
        {
            ctx.push_span(" ".to_string(), ctx.base_style());
            *ctx.current_width += 1;
        }
        ctx.push_span(word.to_string(), ctx.base_style());
        *ctx.current_width += word_width;
        *ctx.needs_separator = true;
    }

    if ends_with_space && !words.is_empty() {
        *ctx.needs_separator = true;
    } else if ends_with_opening_punct(text) {
        // Text ends with an opening bracket/quote and no whitespace, so the
        // next inline (e.g. "**hi**" inside "(**hi**)") must attach directly
        // without a space. Without this the renderer treats "(" as a word and
        // inserts a spurious space after it.
        *ctx.needs_separator = false;
    }
}

fn render_code_inline(text: &str, ctx: &mut RenderCtx, color: Color) {
    let word_width = display_width(text);

    // Flush if projected width exceeds the available line width
    // (but only when the line already has content — don't flush a blank line).
    let projected = *ctx.current_width + usize::from(*ctx.needs_separator) + word_width;
    if projected > ctx.width && *ctx.current_width > ctx.indent {
        ctx.flush_line_with_next(LineJoin::Space);
    } else if *ctx.needs_separator && !ctx.current.is_empty() && *ctx.current_width > ctx.indent {
        ctx.push_span(" ".to_string(), ctx.base_style());
        *ctx.current_width += 1;
    }

    if *ctx.current_width + word_width > ctx.width && *ctx.current_width >= ctx.indent {
        ctx.render_word_split(text, ctx.base_style().fg(color));
        return;
    }

    ctx.push_span(text.to_string(), ctx.base_style().fg(color));
    *ctx.current_width += word_width;
    *ctx.needs_separator = true;
}

/// Render a styled container (bold, italic, strikethrough) by stacking its
/// [`Modifier`] on top of any modifiers already active from enclosing
/// containers.  The save-OR-restore pattern gives us correct nesting with
/// no heap allocation — e.g. ***bold italic*** becomes
/// `BOLD | ITALIC` for the inner text.
fn render_style_inline(content: &[MarkdownInline], ctx: &mut RenderCtx, modifier: Modifier) {
    let prev = ctx.modifier;
    ctx.modifier = prev | modifier;
    render_inlines_to_lines(content, ctx);
    ctx.modifier = prev;
    *ctx.needs_separator = true;
}

/// Render a hyperlink: link text in **bold**, then ` — `, then the URL
/// <u>underlined</u>.  If `destination` is empty the content is rendered
/// without any link-specific styling (bare `[...]()` with no URL).
///
/// Modifier stacking works the same as [`render_style_inline`]: the BOLD
/// flag is ORed in for the content, then removed for the separator, then
/// UNDERLINED is ORed in for the URL alone, then fully restored.
fn render_link_inline(content: &[MarkdownInline], destination: &str, ctx: &mut RenderCtx) {
    if destination.is_empty() {
        render_inlines_to_lines(content, ctx);
        *ctx.needs_separator = true;
        return;
    }

    // Link content in bold (stacked on any parent modifier).
    let prev = ctx.modifier;
    ctx.modifier = prev | Modifier::BOLD;
    render_inlines_to_lines(content, ctx);

    // Separator: " - " with the parent style (no bold).
    ctx.modifier = prev;
    let sep = " - ";
    let sep_width = display_width(sep);
    let url_width = display_width(destination);
    let projected = *ctx.current_width + sep_width + url_width;
    if projected > ctx.width && *ctx.current_width > ctx.indent {
        // Mid-inline wrap: the link separator/URL continue the sentence.
        ctx.flush_line_with_next(LineJoin::Space);
    }
    ctx.push_span(sep.to_string(), ctx.base_style());
    *ctx.current_width += sep_width;

    // URL underlined (stacked on parent but not bold).
    ctx.modifier = prev | Modifier::UNDERLINED;
    ctx.push_span(destination.to_string(), ctx.base_style());
    ctx.modifier = prev;
    *ctx.current_width += url_width;

    *ctx.needs_separator = true;
}

fn wrapped_line_height(line: &Line<'_>, width: usize) -> usize {
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

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "markdown_render_tests.rs"]
mod tests;
