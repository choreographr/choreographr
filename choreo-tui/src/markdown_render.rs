use choreo_proto::Turn;
use choreo_tui::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;

use std::sync::Arc;

use crate::cache::GlobalLruCache;
use crate::diff_render::try_render_diff_content;
use crate::render::{BG_SHADE, format_timestamp};
use crate::syntax::{highlight_theme, syntax_set, to_ratatui_color};
use tracing::warn;

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

pub(crate) fn plain_text_lines(text: &str) -> Vec<Line<'static>> {
    if text.is_empty() {
        vec![Line::from(Span::styled(String::new(), Style::default()))]
    } else {
        text.split('\n')
            .map(|line| Line::from(Span::styled(line.to_string(), Style::default())))
            .collect()
    }
}

/// Render ANSI-escape-coded text as styled ratatui lines, wrapping at `width`.
/// Falls back to [`plain_text_lines`] on parse failure.
fn ansi_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    use ansi_to_tui::IntoText as _;

    let width = width as usize;

    match text.as_bytes().into_text() {
        Ok(t) => {
            let mut result: Vec<Line<'static>> = Vec::new();
            for line in &t.lines {
                if width == 0 || line.width() <= width {
                    let spans: Vec<Span<'static>> = line
                        .spans
                        .iter()
                        .map(|span| Span::styled(span.content.to_string(), span.style))
                        .collect();
                    result.push(Line::from(spans));
                } else {
                    // Word-wrap this over-long line at width.
                    wrap_styled_line(line, width, &mut result);
                }
            }
            if result.is_empty() {
                vec![Line::from(Span::styled(String::new(), Style::default()))]
            } else {
                result
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                text_len = text.len(),
                "failed to parse ANSI escape codes, falling back to plain text"
            );
            plain_text_lines(text)
        }
    }
}

/// Word-wrap a pre-styled ratatui line so that no output line exceeds `max_width`.
///
/// Walks the line's styled spans left-to-right, splitting at word (whitespace)
/// boundaries. If a single word is wider than `max_width` it is split by grapheme
/// cluster via [`split_word_to_width`].
fn wrap_styled_line(
    line: &ratatui::text::Line<'_>,
    max_width: usize,
    out: &mut Vec<Line<'static>>,
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
        return;
    }

    // ── 2. Word-wrap the token stream onto lines of at most max_width ──
    let mut line_spans: Vec<Span<'static>> = Vec::new();
    let mut line_width = 0usize;
    // Did we just add a space at the end?  We keep at most one trailing space
    // so that flush + re-start doesn't introduce a leading space.
    let mut trailing_space = false;

    // Helper: split an over-long word across lines, used when the word alone
    // does not fit on the current (possibly just-flushed) line.
    let push_split_word = |text: &str,
                           style: Style,
                           max_width: usize,
                           out: &mut Vec<Line<'static>>,
                           line_spans: &mut Vec<Span<'static>>,
                           line_width: &mut usize|
     -> () {
        let chunks = split_word_to_width(text, max_width);
        for (ci, chunk) in chunks.iter().enumerate() {
            if ci > 0 {
                out.push(Line::from(std::mem::take(line_spans)));
                *line_width = 0;
            }
            let cw = display_width(chunk);
            line_spans.push(Span::styled(chunk.clone(), style));
            *line_width += cw;
        }
    };

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
                &mut *out,
                &mut line_spans,
                &mut line_width,
            );
        } else {
            // Flush the current line and start a fresh line with this word.
            out.push(Line::from(std::mem::take(&mut line_spans)));
            line_width = 0;
            trailing_space = false;

            if word_width <= max_width {
                line_spans.push(Span::styled(token.text.clone(), token.style));
                line_width = word_width;
            } else {
                push_split_word(
                    &token.text,
                    token.style,
                    max_width,
                    &mut *out,
                    &mut line_spans,
                    &mut line_width,
                );
            }
        }
    }

    if !line_spans.is_empty() {
        out.push(Line::from(line_spans));
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

/// Render a complete Turn as styled lines suitable for the chat history.
/// Each section (user, assistant, tool results) is wrapped in the margin
/// pattern (top separator, padding, content, padding, bottom separator)
/// with role-specific accent colors.
pub(crate) fn render_turn_lines(
    turn: &Turn,
    content_width: u16,
    tool_content_width: u16,
) -> Vec<Line<'static>> {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    // ── Error block ──────────────────────────────────────────
    if let Some(ref err) = turn.error {
        let header = format!("Error: {err}");
        let lines = vec![Line::from(Span::styled(
            header,
            Style::default().fg(Color::Red),
        ))];
        all_lines.extend(lines);
        return all_lines;
    }

    // ── User text block (green accent) ───────────────────────
    if let Some(ref text) = turn.user_text {
        let body = markdown_lines(text, content_width);
        let timestamp_ms = Some(turn.created_at.as_millis());
        let margin = add_margin_lines(body, content_width, Color::Green, timestamp_ms);
        all_lines.extend(margin.0);
    }

    // ── Assistant response block (blue accent) ───────────────
    //
    // During streaming the reasoning section is shown first.  When the
    // response starts streaming, `assistant_reasoning` is cleared (see
    // `stream_chunk` in history.rs) so only the response appears.
    // After completion, if both fields exist, only the response text
    // is shown; if no response exists, the reasoning is shown instead.
    // No "Reasoning:" or "Response:" headings are rendered.
    let has_assistant = turn.assistant_text.is_some() || turn.assistant_reasoning.is_some();
    if has_assistant {
        let mut body: Vec<Line<'static>> = Vec::new();

        // Show response text preferentially — if present, it replaces
        // any reasoning content that was shown during streaming.
        if let Some(ref text) = turn.assistant_text {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                body.extend(markdown_lines(trimmed, content_width));
            }
        }
        // Only show reasoning when there is no response text.
        if turn.assistant_text.is_none()
            && let Some(ref reasoning) = turn.assistant_reasoning
        {
            let trimmed = reasoning.trim();
            if !trimmed.is_empty() {
                body.extend(markdown_lines(trimmed, content_width));
            }
        }

        // If we have content, wrap with margin lines (no timestamp).
        if !body.is_empty() {
            let margin = add_margin_lines(body, content_width, Color::Blue, None);
            all_lines.extend(margin.0);
        }
    }

    // ── Tool results block (red accent if error, gray otherwise) ─
    //
    // Quiet tools are those whose content is only meaningful to the LLM
    // and would spam the user's session history if rendered in full.
    // Their invocation description (e.g. "Reading file `main.rs`.") is
    // shown instead, giving the user enough context without the verbatim
    // content.
    const QUIET_TOOLS: &[&str] = &["read_file", "read_file_range"];

    for tr in &turn.tool_results {
        let accent = if tr.is_error {
            Color::Red
        } else {
            Color::Reset
        };
        // Quiet tools suppress their full content body from the UI (the
        // invocation description is sufficient context), but the label
        // remains the standard "tool result" — only error results get
        // a distinct label.
        let is_quiet = !tr.is_error && QUIET_TOOLS.contains(&tr.name.as_str());
        let label = if tr.is_error {
            "tool error"
        } else {
            "tool result"
        };

        let mut body: Vec<Line<'static>> = Vec::new();

        // Invocation description (rendered as markdown so code blocks
        // highlight properly) appears before the tool result/error label.
        if !tr.invocation_description.is_empty() {
            body.extend(markdown_lines(
                &tr.invocation_description,
                tool_content_width,
            ));
            body.push(Line::from(Span::styled(String::new(), Style::default())));
        }

        // Header line
        body.push(Line::from(Span::styled(
            format!("{label}: {}", tr.name),
            Style::default().fg(accent),
        )));

        // Skip rendering the full content body for quiet tools — the
        // invocation description above already tells the user what was
        // read, and the raw file contents are only needed by the LLM.
        if !is_quiet && !tr.content.is_empty() {
            body.push(Line::from(Span::styled(String::new(), Style::default())));
            // Content with ANSI escape codes gets colored rendering.
            if tr.content.contains("\x1b[") {
                body.extend(ansi_lines(&tr.content, tool_content_width));
            } else if tr.is_error {
                body.extend(plain_text_lines(&tr.content));
            } else if let Some(diff_lines) =
                try_render_diff_content(&tr.content, tool_content_width)
            {
                body.extend(diff_lines);
            } else {
                body.extend(markdown_lines(&tr.content, tool_content_width));
            }
        }

        // Apply 2-column left/right indent so every row spans the full
        // area width with exactly 2 columns of right margin.
        for line in &mut body {
            line.spans.insert(0, Span::styled("  ", Style::default()));
            let content_sum: usize = line.spans.iter().skip(1).map(|s| s.width()).sum();
            let fill = (tool_content_width as usize).saturating_sub(content_sum);
            line.spans
                .push(Span::styled(" ".repeat(fill), Style::default()));
            line.spans.push(Span::styled("  ", Style::default()));
        }
        all_lines.extend(body);
    }

    // If no sections produced output, emit a blank line.
    if all_lines.is_empty() {
        all_lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }

    all_lines
}

// ── Margin helpers (reused from current render system) ─────────────────

/// Structural rows: top separator, top padding, bottom padding, bottom separator.
pub(crate) const MARGIN_STRUCTURAL_ROWS: usize = 4;

/// Wrap content lines with a vertical accent bar on the left and dark-gray
/// background shading.
fn add_margin_lines(
    lines: Vec<Line<'static>>,
    content_width: u16,
    accent: Color,
    timestamp_ms: Option<i64>,
) -> (Vec<Line<'static>>, usize) {
    let gray = Style::default().bg(BG_SHADE);
    let no_shading = Style::default().bg(Color::Reset);
    let accent_line = Style::default().fg(accent).bg(Color::Reset);
    let total_width = content_width as usize + 9;
    let shaded_content = content_width as usize + 4;

    // Top separator: no shading
    let separator = Line::from(vec![Span::styled(" ".repeat(total_width), no_shading)]);

    // Padding row: shading on cols 3..W-3, no text
    let padding = Line::from(vec![
        Span::styled("  ", no_shading),
        Span::styled("┃", accent_line),
        Span::styled(" ".repeat(shaded_content), gray),
        Span::styled("  ", no_shading),
    ]);

    let mut result = Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    result.push(separator);
    result.push(padding.clone());

    for line in lines {
        let fill = (content_width as usize).saturating_sub(line.width());

        let mut spans = vec![
            Span::styled("  ", no_shading),
            Span::styled("┃", accent_line),
            Span::styled("  ", gray),
        ];
        // Content spans — explicitly set bg so they display correctly even without
        // a paragraph-level background.
        spans.extend(
            line.spans
                .into_iter()
                .map(|s| Span::styled(s.content, s.style.bg(BG_SHADE))),
        );
        spans.push(Span::styled(" ".repeat(fill), gray));
        spans.push(Span::styled("  ", gray));
        spans.push(Span::styled("  ", no_shading));

        result.push(Line::from(spans));
    }

    result.push(padding);

    // Bottom separator: right-aligned timestamp (user messages only).
    if let Some(ms) = timestamp_ms {
        let ts_text = format_timestamp(ms / 1000);
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

    let total_rows = result.len();
    (result, total_rows)
}

pub(crate) fn markdown_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let document = MarkdownDocument::parse(markdown);
    let mut lines = Vec::new();
    render_markdown_blocks(&document.blocks, &mut lines, 0, width as usize);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
    while matches!(lines.last(), Some(line) if line.width() == 0) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
    lines
}

/// Push a blank (zero-width) line onto `lines` unless the last line is
/// already blank.  This gives us CSS-like margin collapsing: multiple
/// adjacent blocks that each want vertical space produce at most one
/// blank line between them.
fn ensure_blank_line(lines: &mut Vec<Line<'static>>) {
    if lines.last().is_none_or(|l| l.width() > 0) {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
}

fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    lines: &mut Vec<Line<'static>>,
    indent: usize,
    width: usize,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            ensure_blank_line(lines);
        }
        render_markdown_block(block, lines, indent, width);
    }
}

fn render_markdown_block(
    block: &MarkdownBlock,
    lines: &mut Vec<Line<'static>>,
    indent: usize,
    width: usize,
) {
    match block {
        MarkdownBlock::Paragraph(content) => lines.extend(inlines_to_lines(
            content,
            indent,
            None,
            width,
            Modifier::empty(),
        )),
        MarkdownBlock::Heading { level, content } => {
            // Two blank lines before every heading for visual separation.
            // render_markdown_blocks already supplies one via ensure_blank_line;
            // the unconditional push here adds the second.
            lines.push(Line::from(Span::styled(String::new(), Style::default())));
            let prefix = Some(format!("{} ", "#".repeat(*level as usize)));
            // Headings are rendered bold + underlined for visual distinction.
            lines.extend(inlines_to_lines(
                content,
                indent,
                prefix,
                width,
                Modifier::BOLD | Modifier::UNDERLINED,
            ));
        }
        MarkdownBlock::CodeBlock { language, code } => {
            let header = language
                .as_deref()
                .map(|value| format!("```{value}"))
                .unwrap_or_else(|| "```".to_string());
            lines.push(indented_line(indent, header));

            let max_code_width = width.saturating_sub(indent);
            let highlighted = highlight_code(language.as_deref(), code);
            for hl_line in highlighted {
                // Wrap code block lines that exceed the available width so
                // they don't overflow the terminal.  Uses word-wrap via
                // wrap_styled_line which falls back to grapheme-cluster
                // splitting for words that don't fit.
                if hl_line.width() > max_code_width {
                    let mut wrapped: Vec<Line<'static>> = Vec::new();
                    wrap_styled_line(&hl_line, max_code_width, &mut wrapped);
                    for wl in wrapped {
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
                    }
                } else if indent > 0 {
                    let mut spans = vec![Span::styled(" ".repeat(indent), Style::default())];
                    spans.extend(hl_line.spans.clone());
                    lines.push(Line::from(spans));
                } else {
                    lines.push(hl_line);
                }
            }

            lines.push(indented_line(indent, "```".to_string()));
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            // Content is rendered at (width - indent - 2) so that when "> " and the
            // outer indent are prepended on each line the total stays within `width`.
            render_markdown_blocks(blocks, &mut quoted, 0, width.saturating_sub(indent + 2));
            for line in quoted {
                let mut spans = line.spans.clone();
                spans.insert(0, Span::styled("> ".to_string(), Style::default()));
                lines.push(indented_line_as_spans(indent, spans));
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
                    "• ".to_string()
                };
                let marker_width = display_width(&marker);
                let continuation_indent = indent + marker_width;
                let mut rendered = Vec::new();
                // Content is rendered at (width - indent - marker_width) so that
                // when the marker and outer indent are prepended the total fits
                // within `width`.
                render_markdown_blocks(
                    item,
                    &mut rendered,
                    0,
                    width.saturating_sub(indent + marker_width),
                );
                // Track whether the item spans more than one visual line.
                // Multi-line items get a blank line after them; single-line
                // items are compact (no gap) for a tight list feel.
                let item_multi_line = rendered.len() > 1;
                let mut rendered_iter = rendered.into_iter();
                if let Some(first) = rendered_iter.next() {
                    let mut spans = vec![Span::styled(
                        format!("{}{}", " ".repeat(indent), marker),
                        Style::default(),
                    )];
                    spans.extend(first.spans.clone());
                    lines.push(Line::from(spans));
                } else {
                    lines.push(indented_line(indent, marker));
                }
                for line in rendered_iter {
                    let mut spans = vec![Span::styled(
                        " ".repeat(continuation_indent),
                        Style::default(),
                    )];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }

                // Blank line after multi-line items only, so simple
                // single-line items stay compact (no gaps).  Uses
                // ensure_blank_line so consecutive blanks collapse into
                // one (e.g. when a multi-line item ends with a nested
                // list that already produced a blank line).
                if index + 1 < items.len() && item_multi_line {
                    ensure_blank_line(lines);
                }
            }
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => lines.extend(render_table_lines(alignments, header, rows, indent, width)),
        MarkdownBlock::Rule => lines.push(indented_line(indent, "---".to_string())),
    }
}

// ── Table rendering ───────────────────────────────────────────────────────

fn render_table_lines(
    alignments: &[MarkdownAlignment],
    header: &[Vec<MarkdownInline>],
    rows: &[Vec<Vec<MarkdownInline>>],
    indent: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let column_count = alignments
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        return vec![Line::from(Span::styled(String::new(), Style::default()))];
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
    lines.push(table_border_line('┌', '┬', '┐', &widths, indent));
    lines.extend(render_table_row_wrapped(
        &table_rows[0],
        &widths,
        &header_alignment,
        indent,
    ));
    lines.push(table_separator_line(&widths, &header_alignment, indent));
    for (index, row) in table_rows.iter().enumerate().skip(1) {
        lines.extend(render_table_row_wrapped(
            row,
            &widths,
            &header_alignment,
            indent,
        ));
        if index < table_rows.len() - 1 {
            lines.push(table_border_line('├', '┼', '┤', &widths, indent));
        }
    }
    lines.push(table_border_line('└', '┴', '┘', &widths, indent));
    lines
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
) -> Vec<Line<'static>> {
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
    lines
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
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(word, true) {
        let grapheme_width = grapheme_width(grapheme).max(1);
        if !current.is_empty() && current_width + grapheme_width > width {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width += grapheme_width;
        if current_width >= width {
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

fn pad_aligned(text: &str, width: usize, alignment: MarkdownAlignment) -> String {
    let text_width = display_width(text);
    if text_width >= width {
        return text.to_string();
    }
    let remaining = width - text_width;
    let (left, right) = match alignment {
        MarkdownAlignment::Right => (remaining, 0),
        MarkdownAlignment::Center => (remaining / 2, remaining - (remaining / 2)),
        MarkdownAlignment::Left | MarkdownAlignment::None => (0, remaining),
    };
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

pub(crate) fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
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
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
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
    let mut ctx = RenderCtx {
        lines: &mut lines,
        current: &mut current_spans,
        current_width: &mut current_width,
        needs_separator: &mut needs_separator,
        indent,
        width,
        modifier,
    };
    render_inlines_to_lines(inlines, &mut ctx);
    if !current_spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(std::mem::take(&mut current_spans)));
    }
    lines
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
    /// Spans being accumulated for the line currently being built.
    current: &'a mut Vec<Span<'static>>,
    /// Display width of `current` (updated alongside every push).
    current_width: &'a mut usize,
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
    fn flush_line(&mut self) {
        self.lines.push(Line::from(std::mem::take(self.current)));
        *self.current_width = self.indent;
        if self.indent > 0 {
            self.current
                .push(Span::styled(" ".repeat(self.indent), Style::default()));
        }
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
                self.flush_line();
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
                    ctx.flush_line();
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
                    ctx.flush_line();
                }
                ctx.push_span(suffix, Style::default());
                *ctx.current_width += suffix_width;
                *ctx.needs_separator = true;
            }
            MarkdownInline::LineBreak => {
                ctx.flush_line();
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
            ctx.flush_line();
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
    }
}

fn render_code_inline(text: &str, ctx: &mut RenderCtx, color: Color) {
    let word_width = display_width(text);

    // Flush if projected width exceeds the available line width
    // (but only when the line already has content — don't flush a blank line).
    let projected = *ctx.current_width + usize::from(*ctx.needs_separator) + word_width;
    if projected > ctx.width && *ctx.current_width > ctx.indent {
        ctx.flush_line();
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
        ctx.flush_line();
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
mod tests {
    use super::*;

    // ── find_syntax ──────────────────────────────────────────────────────

    #[test]
    fn find_syntax_rust() {
        let ss = syntax_set();
        let result = find_syntax(ss, "rust");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "Rust");
    }

    #[test]
    fn find_syntax_typescript_maps_to_javascript() {
        let ss = syntax_set();
        let result = find_syntax(ss, "typescript");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "JavaScript");
    }

    #[test]
    fn find_syntax_tsx_maps_to_javascript() {
        let ss = syntax_set();
        let result = find_syntax(ss, "tsx");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "JavaScript");
    }

    #[test]
    fn find_syntax_vue_maps_to_html() {
        let ss = syntax_set();
        let result = find_syntax(ss, "vue");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "HTML");
    }

    #[test]
    fn find_syntax_svelte_maps_to_html() {
        let ss = syntax_set();
        let result = find_syntax(ss, "svelte");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "HTML");
    }

    #[test]
    fn find_syntax_unknown_returns_none() {
        let ss = syntax_set();
        let result = find_syntax(ss, "not-a-real-language-12345");
        assert!(result.is_none());
    }

    // ── highlight_code ───────────────────────────────────────────────────

    #[test]
    fn highlight_code_known_language_produces_coloured_spans() {
        let lines = highlight_code(Some("rust"), "fn main() {}");
        assert!(!lines.is_empty(), "should produce at least one line");

        let has_colour = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(has_colour, "highlighted Rust should have coloured spans");
    }

    #[test]
    fn highlight_code_unknown_language_produces_output() {
        let lines = highlight_code(Some("this-is-not-a-real-language"), "some text");
        assert!(!lines.is_empty(), "should still produce output");
    }

    #[test]
    fn highlight_code_none_language_uses_plain_text() {
        let lines = highlight_code(None, "plain text");
        assert!(!lines.is_empty());
    }

    #[test]
    fn highlight_code_empty_string() {
        let lines = highlight_code(Some("rust"), "");
        assert!(!lines.is_empty());
    }

    #[test]
    fn highlight_code_multi_line() {
        let lines = highlight_code(Some("python"), "def foo():\n    pass");
        assert_eq!(lines.len(), 2, "should have one line per code line");
    }

    // ── plain_text_lines ─────────────────────────────────────────────────

    #[test]
    fn plain_text_lines_empty() {
        let result = plain_text_lines("");
        assert_eq!(result.len(), 1, "empty input → one empty line");
        assert_eq!(result[0].width(), 0);
    }

    #[test]
    fn plain_text_lines_single() {
        let result = plain_text_lines("hello");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_string(), "hello");
    }

    #[test]
    fn plain_text_lines_multi() {
        let result = plain_text_lines("a\nb\nc");
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].to_string(), "a");
        assert_eq!(result[1].to_string(), "b");
        assert_eq!(result[2].to_string(), "c");
    }

    // ── lines_height ────────────────────────────────────────────────────

    #[test]
    fn lines_height_simple() {
        let lines = vec![Line::from("hello")];
        assert_eq!(lines_height(&lines, 80), 1);
    }

    #[test]
    fn lines_height_zero_width() {
        let lines = vec![Line::from("hello")];
        assert_eq!(lines_height(&lines, 0), 0);
    }

    #[test]
    fn lines_height_wrapping() {
        let text = "x".repeat(100);
        let lines = vec![Line::from(text)];
        assert_eq!(lines_height(&lines, 40), 3);
    }

    #[test]
    fn lines_height_multiple_lines() {
        let lines = vec![Line::from("short"), Line::from("a".repeat(50))];
        assert_eq!(lines_height(&lines, 30), 3);
    }

    #[test]
    fn lines_height_empty() {
        let lines = vec![Line::from("")];
        assert_eq!(lines_height(&lines, 80), 1);
    }

    #[test]
    fn lines_height_empty_slice_returns_zero() {
        let lines: Vec<Line<'static>> = vec![];
        assert_eq!(lines_height(&lines, 80), 0);
    }

    // ── markdown_lines ───────────────────────────────────────────────────

    #[test]
    fn markdown_lines_empty() {
        let result = markdown_lines("", 80);
        assert!(!result.is_empty(), "should not return empty vec");
        assert_eq!(result[0].width(), 0);
    }

    #[test]
    fn markdown_lines_paragraph() {
        let result = markdown_lines("hello world", 80);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_string(), "hello world");
    }

    #[test]
    fn markdown_lines_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let result = markdown_lines(md, 80);
        assert!(result.len() >= 3, "code block should have at least 3 lines");
        assert_eq!(result[0].to_string(), "```rust");
        assert_eq!(result.last().unwrap().to_string(), "```");
    }

    #[test]
    fn markdown_lines_code_block_no_language() {
        let md = "```\nplain code\n```";
        let result = markdown_lines(md, 80);
        assert!(result.len() >= 3);
        assert_eq!(result[0].to_string(), "```");
    }

    // ── BlockQuote ──────────────────────────────────────────────────────

    #[test]
    fn markdown_lines_blockquote_simple() {
        let md = "> hello world";
        let result = markdown_lines(md, 80);
        assert!(!result.is_empty());
        assert_eq!(result[0].to_string(), "> hello world");
    }

    #[test]
    fn markdown_lines_blockquote_within_budget() {
        let md = "> hello world";
        let result = markdown_lines(md, 20);
        let text = result
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for line in &result {
            assert!(
                line.width() <= 20,
                "blockquote line width {} exceeds 20",
                line.width()
            );
        }
        assert!(text.contains("> hello world"), "text should be present");
    }

    // ── List ─────────────────────────────────────────────────────────────

    #[test]
    fn markdown_lines_unordered_list_simple() {
        let md = "- item one\n- item two";
        let result = markdown_lines(md, 80);
        let text = result
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("• item one"), "first item should render");
        assert!(text.contains("• item two"), "second item should render");
    }

    #[test]
    fn markdown_lines_ordered_list_simple() {
        let md = "1. first\n2. second";
        let result = markdown_lines(md, 80);
        let text = result
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("1. first"), "first ordered item");
        assert!(text.contains("2. second"), "second ordered item");
    }

    #[test]
    fn markdown_lines_list_within_budget() {
        let md = "- hello world";
        let result = markdown_lines(md, 10);
        let text = result
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("•"), "bullet should be present");
        assert!(text.contains("hello"), "content should be present");
    }

    #[test]
    fn markdown_lines_list_continuation_preserves_spans() {
        let md = "- **bold** and `code`";
        let result = markdown_lines(md, 80);
        assert!(!result.is_empty());
        let first = &result[0];
        // At minimum the text should not have markdown syntax literals.
        let text = first.to_string();
        assert!(
            !text.contains("**bold**"),
            "bold syntax should not appear literally"
        );
        assert!(text.contains("bold"), "bold text should appear");
    }

    // ── display_width ────────────────────────────────────────────────────

    #[test]
    fn display_width_ascii() {
        assert_eq!(display_width("hello"), 5);
    }

    #[test]
    fn display_width_unicode() {
        assert_eq!(display_width("café"), 4);
    }

    #[test]
    fn display_width_empty() {
        assert_eq!(display_width(""), 0);
    }

    // ── render_turn_lines ────────────────────────────────────────────────

    #[test]
    fn render_turn_lines_error_shows_red_header() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: Some("something went wrong".into()),
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        assert!(!lines.is_empty());
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Error: something went wrong"));
    }

    #[test]
    fn render_turn_lines_user_text() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello world".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("hello world"), "user text should appear");
    }

    #[test]
    fn render_turn_lines_assistant_text() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("The answer is 42.".into()),
            assistant_reasoning: Some("Let me think...".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // When response text is present, only the response is shown;
        // reasoning is hidden and no headings are rendered.
        assert!(
            !text.contains("Reasoning:"),
            "reasoning header should NOT appear"
        );
        assert!(
            !text.contains("Let me think"),
            "reasoning body should NOT appear"
        );
        assert!(
            !text.contains("Response:"),
            "response header should NOT appear"
        );
        assert!(
            text.contains("The answer is 42."),
            "response body should appear"
        );
    }

    #[test]
    fn render_turn_lines_reasoning_bold_markdown() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("Okay.".into()),
            assistant_reasoning: Some("Use **bold** for emphasis.".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Response text is present, so only the response shows.
        assert!(
            !text.contains("Reasoning:"),
            "reasoning header should NOT appear"
        );
        assert!(
            !text.contains("Use **bold"),
            "reasoning body should NOT appear"
        );
        assert!(text.contains("Okay."), "response text should appear");
        assert!(
            !text.contains("**bold**"),
            "markdown bold syntax should not appear literally in output"
        );
    }

    #[test]
    fn render_turn_lines_reasoning_inline_code() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("Use `code` inline.".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // No response text, so reasoning is shown. No heading rendered.
        assert!(
            !text.contains("Reasoning:"),
            "reasoning header should NOT appear"
        );
        assert!(text.contains("code"), "code content should appear");
        assert!(
            !text.contains("`code`"),
            "markdown inline code backticks should not appear literally"
        );
    }

    #[test]
    fn render_turn_lines_reasoning_whitespace_only() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("Response text.".into()),
            assistant_reasoning: Some("   ".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Response is present, whitespace-only reasoning is skipped.
        // No headings rendered.
        assert!(
            !text.contains("Reasoning:"),
            "reasoning header should NOT appear"
        );
        assert!(
            !text.contains("Response:"),
            "response header should NOT appear"
        );
        assert!(
            text.contains("Response text."),
            "response body should appear"
        );
    }

    #[test]
    fn render_turn_lines_reasoning_code_block() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("Here is code:\n```rust\nfn main() {}\n```".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // No response text, so reasoning is shown. No heading rendered.
        assert!(
            !text.contains("Reasoning:"),
            "reasoning header should NOT appear"
        );
        assert!(
            text.contains("fn main() {}"),
            "code block content should appear"
        );
        assert!(text.contains("```"), "code block fences should be visible");
    }

    #[test]
    fn render_turn_lines_tool_calls() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![choreo_proto::AssistantToolCallRecord {
                call_id: "call1".into(),
                name: "read_file".into(),
                arguments_json: r#"{"path":"/tmp/x"}"#.into(),
            }],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // The turn has only tool_calls (no text, no reasoning), so no
        // assistant block is rendered. Tool calls are now only visible
        // through their streaming output and subsequent tool results.
        assert!(!text.contains("tool:"), "tool: label should not appear");
    }

    #[test]
    fn render_turn_lines_tool_results() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![choreo_proto::ToolResultRecord {
                call_id: "call1".into(),
                name: "read_file".into(),
                content: "file contents".into(),
                is_error: false,
                invocation_description: "Reading file `src/main.rs`.".into(),
            }],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Quiet tools show the standard "tool result" label and the
        // invocation description but suppress the full content body
        // — the LLM still gets it.
        assert!(text.contains("tool result: read_file"));
        assert!(text.contains("src/main.rs"));
        assert!(!text.contains("file contents"));
    }

    #[test]
    fn render_turn_lines_tool_results_error() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![choreo_proto::ToolResultRecord {
                call_id: "call1".into(),
                name: "run".into(),
                content: "command failed".into(),
                is_error: true,
                invocation_description: String::new(),
            }],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("tool error: run"));
        assert!(text.contains("command failed"));
    }

    #[test]
    fn render_turn_lines_empty_turn_produces_blank_line() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width(), 0);
    }

    #[test]
    fn render_turn_lines_user_with_assistant_renders_both_blocks() {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("Hello".into()),
            assistant_text: Some("Hi there!".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Hello"), "user block should appear");
        assert!(text.contains("Hi there!"), "assistant block should appear");
    }

    // ── ansi_lines ──────────────────────────────────────────────────────

    #[test]
    fn ansi_lines_colors() {
        let result = ansi_lines("\x1b[31mhello\x1b[0m", 80);
        assert_eq!(result.len(), 1, "should produce one line");
        let has_red = result[0]
            .spans
            .iter()
            .any(|s| s.style.fg == Some(Color::Red));
        assert!(has_red, "ANSI red should translate to ratatui red fg");
    }

    #[test]
    fn ansi_lines_fallback_on_junk() {
        let result = ansi_lines("\x1b[z", 80); // incomplete/invalid ANSI sequence
        assert_eq!(result.len(), 1, "junk bytes should fall back to one line");
        // The fallback (plain_text_lines) produces spans with default style.
        let all_default = result[0].spans.iter().all(|s| s.style == Style::default());
        assert!(all_default, "fallback output should have default style");
    }

    #[test]
    fn ansi_lines_empty() {
        let result = ansi_lines("", 80);
        assert_eq!(result.len(), 1, "empty input → one line");
        assert_eq!(result[0].width(), 0, "line should be empty");
    }

    #[test]
    fn ansi_lines_multi_line() {
        let result = ansi_lines("line1\nline2\nline3", 80);
        assert_eq!(
            result.len(),
            3,
            "ANSI text with newlines should produce one line per segment"
        );
    }

    #[test]
    fn ansi_lines_wrapping() {
        // A single-line input that is wide enough to require wrapping.
        let long = "hello world ".repeat(20);
        let result = ansi_lines(&long, 40);
        assert!(
            result.len() > 1,
            "wide content should wrap into multiple lines, got {}",
            result.len()
        );
    }

    #[test]
    fn ansi_lines_no_wrap_when_fits() {
        let result = ansi_lines("short", 80);
        assert_eq!(result.len(), 1, "short content should not wrap");
    }

    #[test]
    fn ansi_lines_wrap_long_word() {
        // A single word wider than the wrap width should be split.
        let result = ansi_lines("superlongword", 5);
        assert!(result.len() > 1, "over-long word should wrap");
        assert!(
            result.iter().all(|l| l.width() <= 5),
            "every wrapped line must be ≤ 5 wide"
        );
    }

    // ── Inline styling (bold, italic, strikethrough, code) ──────────────

    #[test]
    fn markdown_bold_applies_bold_modifier() {
        let result = markdown_lines("**bold text**", 80);
        let line = &result[0];
        let has_bold = line
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "bold markdown should apply BOLD modifier");
        let text = line.to_string();
        assert!(text.contains("bold text"), "bold content should appear");
        assert!(
            !text.contains("**"),
            "markdown syntax should not appear literally"
        );
    }

    #[test]
    fn markdown_italic_applies_italic_modifier() {
        let result = markdown_lines("*italic text*", 80);
        let line = &result[0];
        let has_italic = line
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::ITALIC));
        assert!(has_italic, "italic markdown should apply ITALIC modifier");
        let text = line.to_string();
        assert!(text.contains("italic text"), "italic content should appear");
        assert!(
            !text.contains('*'),
            "markdown syntax should not appear literally"
        );
    }

    #[test]
    fn markdown_strikethrough_applies_crossed_out_modifier() {
        let result = markdown_lines("~~strike~~", 80);
        let line = &result[0];
        let has_crossed = line
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(
            has_crossed,
            "strikethrough markdown should apply CROSSED_OUT modifier"
        );
        let text = line.to_string();
        assert!(
            text.contains("strike"),
            "strikethrough content should appear"
        );
        assert!(
            !text.contains("~~"),
            "markdown syntax should not appear literally"
        );
    }

    #[test]
    fn markdown_inline_code_applies_cyan_color() {
        let result = markdown_lines("use `code` here", 80);
        let line = &result[0];
        let has_cyan = line.spans.iter().any(|s| s.style.fg == Some(Color::Cyan));
        assert!(has_cyan, "inline code should be rendered in Cyan");
        let text = line.to_string();
        assert!(text.contains("code"), "code content should appear");
        assert!(!text.contains('`'), "backticks should not appear literally");
    }

    #[test]
    fn markdown_bold_and_italic_nested() {
        let result = markdown_lines("***nested***", 80);
        let line = &result[0];
        let has_bold = line
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        let has_italic = line
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::ITALIC));
        assert!(has_bold, "nested *** should apply BOLD");
        assert!(has_italic, "nested *** should apply ITALIC");
        let text = line.to_string();
        assert!(text.contains("nested"), "content should appear");
    }

    #[test]
    fn markdown_styled_text_within_budget_wraps_correctly() {
        // Long styled content at a narrow width — should wrap without overflow.
        let words = (0..20).map(|_| "word").collect::<Vec<_>>().join(" ");
        let long_bold = format!("**{words}**");
        let result = markdown_lines(&long_bold, 20);
        assert!(result.len() > 1, "wide bold content should wrap");
        for line in &result {
            assert!(
                line.width() <= 20,
                "no wrapped bold line should exceed width, got {}",
                line.width()
            );
        }
        let has_bold = result
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "wrapped content should still have BOLD modifier");
    }

    #[test]
    fn markdown_styled_text_with_indent_does_not_overflow() {
        // Styled content inside a blockquote (which adds indent).
        let md = "> **bold content inside blockquote**";
        let result = markdown_lines(md, 20);
        for line in &result {
            assert!(
                line.width() <= 20,
                "indented styled line must not exceed width, got {}",
                line.width()
            );
        }
        let text = result
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("bold content"),
            "styled content should be present"
        );
    }

    #[test]
    fn markdown_inline_code_in_blockquote_is_colored() {
        let md = "> `short_code`";
        let result = markdown_lines(md, 20);
        for line in &result {
            assert!(
                line.width() <= 20,
                "indented inline code must not exceed width, got {}",
                line.width()
            );
        }
        let has_cyan = result
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg == Some(Color::Cyan));
        assert!(has_cyan, "inline code in blockquote should be Cyan");
    }

    #[test]
    fn markdown_inline_code_wider_than_width_splits() {
        // An inline code segment wider than the available width.
        let long_code = "abcdefghijklmnopqrstuvwxyz0123456789";
        let md = format!("`{long_code}`");
        let result = markdown_lines(&md, 10);
        // Should have wrapped onto multiple lines.
        assert!(result.len() > 1, "over-wide inline code should split");
        for line in &result {
            assert!(
                line.width() <= 10,
                "split code chunk must not exceed width, got {}",
                line.width()
            );
        }
        // All chunks should be cyan.
        for line in &result {
            for span in &line.spans {
                if !span.content.trim().is_empty() {
                    assert_eq!(
                        span.style.fg,
                        Some(Color::Cyan),
                        "every code chunk should be Cyan"
                    );
                }
            }
        }
        // Full content should appear across the lines.
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(
            whole.contains(long_code),
            "all characters of the code must appear in the output"
        );
    }

    // ── Links ─────────────────────────────────────────────────

    #[test]
    fn markdown_link_renders_bold_content_with_underlined_url() {
        let result = markdown_lines("[click here](http://example.com)", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(whole.contains("click"), "word 'click' should appear");
        assert!(whole.contains("here"), "word 'here' should appear");
        assert!(whole.contains("http://example.com"), "URL should appear");
        assert!(
            !whole.contains("[click here]"),
            "markdown syntax should not appear literally"
        );
        // The link content should have BOLD modifier
        let has_bold = result
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains("click") && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "link content should be bold");
        // The URL should have UNDERLINED modifier
        let has_underlined = result.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.content.contains("http://") && s.style.add_modifier.contains(Modifier::UNDERLINED)
        });
        assert!(has_underlined, "URL should be underlined");
    }

    #[test]
    fn markdown_link_empty_destination_no_url() {
        let result = markdown_lines("[text]()", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(whole.contains("text"), "link text should appear");
        assert!(
            !whole.contains("http"),
            "no URL should appear for empty destination"
        );
        // Without a destination, the content should have no BOLD modifier
        let has_bold = result
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(!has_bold, "empty link should not apply bold");
    }

    #[test]
    fn markdown_link_inside_bold_applies_both() {
        let result = markdown_lines("[**bold link**](http://example.com)", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(whole.contains("bold"), "bold word should appear");
        assert!(whole.contains("link"), "link word should appear");
        assert!(
            !whole.contains("**"),
            "markdown syntax should not appear literally"
        );
        assert!(whole.contains("http://example.com"), "URL should appear");
        // The content inherits BOLD from markdown **plus** the link's BOLD
        let has_bold = result
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains("bold") && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "link content should be bold");
    }

    #[test]
    fn markdown_link_with_code_is_colored() {
        let result = markdown_lines("[`code`](http://example.com)", 80);
        let has_cyan = result
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg == Some(Color::Cyan));
        assert!(has_cyan, "inline code should be Cyan inside a link");
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(whole.contains("code"), "code content should appear");
        assert!(
            !whole.contains('`'),
            "backticks should not appear literally"
        );
    }

    #[test]
    fn markdown_link_wrapping_does_not_overflow() {
        let long = "a".repeat(30);
        let md = format!("[{long}](http://example.com)");
        let result = markdown_lines(&md, 10);
        // Should wrap onto multiple lines: content wraps, then URL on its own line.
        assert!(
            result.len() >= 3,
            "long link text should wrap onto multiple lines, got {}",
            result.len()
        );
        // The first 3 lines are the bold content — each must be ≤ width.
        // The last line(s) contain the separator + URL, which may exceed width.
        for line in result.iter().take(3) {
            assert!(
                line.width() <= 10,
                "wrapped link content line width {} exceeds 10",
                line.width()
            );
        }
        // The URL should appear somewhere.
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(whole.contains("http://example.com"), "URL should appear");
    }

    // ── starts_with_closing_punct ────────────────────────────────────────

    #[test]
    fn starts_with_closing_punct_period() {
        assert!(starts_with_closing_punct("."));
        assert!(starts_with_closing_punct("..."));
        assert!(starts_with_closing_punct(".not"));
    }

    #[test]
    fn starts_with_closing_punct_comma() {
        assert!(starts_with_closing_punct(","));
        assert!(starts_with_closing_punct(", "));
    }

    #[test]
    fn starts_with_closing_punct_exclamation() {
        assert!(starts_with_closing_punct("!"));
        assert!(starts_with_closing_punct("!important"));
    }

    #[test]
    fn starts_with_closing_punct_question() {
        assert!(starts_with_closing_punct("?"));
        assert!(starts_with_closing_punct("? "));
    }

    #[test]
    fn starts_with_closing_punct_colon_semicolon() {
        assert!(starts_with_closing_punct(":"));
        assert!(starts_with_closing_punct(";"));
    }

    #[test]
    fn starts_with_closing_punct_brackets() {
        assert!(starts_with_closing_punct(")"));
        assert!(starts_with_closing_punct("]"));
        assert!(starts_with_closing_punct("}"));
    }

    #[test]
    fn starts_with_closing_punct_unicode_quotes() {
        assert!(starts_with_closing_punct("\u{2019}")); // right single quote
        assert!(starts_with_closing_punct("\u{201d}")); // right double quote
    }

    #[test]
    fn starts_with_closing_punct_non_closing_chars() {
        assert!(!starts_with_closing_punct("hello"));
        assert!(!starts_with_closing_punct(""));
        assert!(!starts_with_closing_punct("("));
        assert!(!starts_with_closing_punct("["));
        assert!(!starts_with_closing_punct("{"));
        assert!(!starts_with_closing_punct("\u{2018}")); // left single quote
        assert!(!starts_with_closing_punct("\u{201c}")); // left double quote
    }

    // ── punctuation attachment (closing punct after styled text) ──────────

    #[test]
    fn bold_with_exclamation_no_extra_space() {
        // "**bold**!" should render as "bold!", not "bold !"
        let result = markdown_lines("hello **bold**!", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(
            whole.contains("bold!"),
            "expected 'bold!' without space, got: {whole:?}"
        );
        assert!(
            !whole.contains("bold !"),
            "should not have space before '!'"
        );
        assert!(
            !whole.contains("**bold**"),
            "markdown syntax should not appear"
        );
    }

    #[test]
    fn italic_with_period_no_extra_space() {
        let result = markdown_lines("I said *italic*.", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(
            whole.contains("italic."),
            "expected 'italic.' without space, got: {whole:?}"
        );
        assert!(
            !whole.contains("italic ."),
            "should not have space before '.'"
        );
    }

    #[test]
    fn strong_and_link_with_comma_no_extra_space() {
        let result = markdown_lines("see **bold**, and [link](http://x.com).", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(whole.contains("bold,"), "expected 'bold,' without space");
        assert!(
            whole.contains("link - http://x.com."),
            "link content and trailing period"
        );
        assert!(
            !whole.contains("bold ,"),
            "should not have space before ','"
        );
    }

    #[test]
    fn closing_punct_after_strikethrough() {
        let result = markdown_lines("done ~~strike~~!", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(
            whole.contains("strike!"),
            "expected 'strike!' without space"
        );
        assert!(
            !whole.contains("strike !"),
            "should not have space before '!'"
        );
    }

    #[test]
    fn opening_bracket_keeps_space() {
        // Opening brackets should still get a space before them
        let result = markdown_lines("word (paren)", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(
            whole.contains("word ("),
            "expected space before opening paren"
        );
    }

    // ── heading modifiers ────────────────────────────────────────────────

    #[test]
    fn heading_has_bold_and_underlined_modifier() {
        let result = markdown_lines("# heading text", 80);
        let has_modifiers = result.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.style.add_modifier.contains(Modifier::BOLD)
                && s.style.add_modifier.contains(Modifier::UNDERLINED)
        });
        assert!(
            has_modifiers,
            "heading spans should have BOLD | UNDERLINED modifiers"
        );
    }

    #[test]
    fn heading_content_not_literal() {
        let result = markdown_lines("# **bold** heading", 80);
        let whole: String = result.iter().map(|l| l.to_string()).collect();
        assert!(
            !whole.contains("**bold**"),
            "markdown syntax should not appear"
        );
        assert!(whole.contains("bold"), "bold content should appear");
    }

    #[test]
    fn heading_has_two_blank_lines_before() {
        // Two blank lines should precede a heading when preceded by content.
        let result = markdown_lines("some text\n# heading\nmore text", 80);
        // Walk through lines and find the heading line.
        let heading_idx = result
            .iter()
            .position(|l| l.to_string().contains("heading"));
        assert!(heading_idx.is_some(), "heading text should appear");
        let idx = heading_idx.unwrap();
        // Verify two blank lines precede it.
        assert!(
            idx >= 2 && result[idx - 1].width() == 0 && result[idx - 2].width() == 0,
            "expected two blank lines before heading, got lines around index {idx}: \
             lines[{}]='{}' lines[{}]='{}' lines[{}]='{}'",
            idx.saturating_sub(2),
            result
                .get(idx - 2)
                .map(|l| format!("{l}"))
                .unwrap_or_default(),
            idx - 1,
            result[idx - 1],
            idx,
            result[idx]
        );
    }

    #[test]
    fn first_heading_no_extra_blank_on_top() {
        // A heading at the very start of the document should not have two
        // blank lines above it (there's no content before it).
        let result = markdown_lines("# first heading", 80);
        let heading_idx = result.iter().position(|l| l.to_string().contains("first"));
        assert!(heading_idx.is_some(), "heading should appear");
        // The heading should be the first non-empty line, or at line 0.
        // There shouldn't be two blank lines before it.
        let idx = heading_idx.unwrap();
        assert!(
            idx < 2,
            "first heading should not have two blank lines above, got {idx} lines before it"
        );
    }

    // ── ensure_blank_line ──────────────────────────────────────────────────

    #[test]
    fn ensure_blank_line_empty() {
        let mut lines = vec![];
        ensure_blank_line(&mut lines);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width(), 0);
    }

    #[test]
    fn ensure_blank_line_after_nonblank() {
        let mut lines = vec![Line::from("hello")];
        ensure_blank_line(&mut lines);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].width(), 0);
    }

    #[test]
    fn ensure_blank_line_collapses() {
        let mut lines = vec![
            Line::from("hello"),
            Line::from(Span::styled(String::new(), Style::default())),
        ];
        ensure_blank_line(&mut lines);
        assert_eq!(lines.len(), 2, "should not add another blank line");
    }

    #[test]
    fn ensure_blank_line_twice_collapses() {
        let mut lines = vec![Line::from("hello")];
        ensure_blank_line(&mut lines); // adds blank
        ensure_blank_line(&mut lines); // should collapse
        assert_eq!(lines.len(), 2);
    }

    // ── list blank-line collapsing ────────────────────────────────────────

    #[test]
    fn list_items_compact_when_single_line() {
        // Single-line list items should not have blank lines between them.
        let result = markdown_lines("- alpha\n- beta\n- gamma", 80);
        let whole: String = result
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(whole.contains("• alpha"), "first item should render");
        assert!(whole.contains("• beta"), "second item should render");
        assert!(whole.contains("• gamma"), "third item should render");
        // No blank lines between single-line items.
        let blank_lines: Vec<bool> = result
            .windows(2)
            .map(|w| w[0].width() == 0 && w[1].width() > 0)
            .collect();
        assert_eq!(
            blank_lines.iter().filter(|&&b| b).count(),
            0,
            "single-line list items should have no blank lines between them\n{whole}"
        );
    }

    #[test]
    fn multi_line_list_item_has_blank_after() {
        // A multi-line (wrapping) item should get a blank line after it.
        let long = "a".repeat(60);
        let md = format!("- {long}\n- short");
        let result = markdown_lines(&md, 40);
        // The long item wraps to multiple visual lines → blank line before "• short".
        let short_idx = result.iter().position(|l| l.to_string().contains("short"));
        assert!(short_idx.is_some(), "second item should appear");
        let idx = short_idx.unwrap();
        assert!(
            idx >= 1 && result[idx - 1].width() == 0,
            "expected a blank line before multi-line item's successor, got lines[{}]='{}'",
            idx - 1,
            result[idx - 1]
        );
    }

    #[test]
    fn nested_list_outer_gets_blank_inner_compact() {
        // Outer item spans multiple lines (nested list) so it gets a blank line
        // after it.  Inner items are single-line so they stay compact (no gaps).
        let md = "- outer\n  - inner\n  - inner2\n- next";
        let result = markdown_lines(md, 80);
        let whole: String = result
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(whole.contains("• outer"), "first outer item");
        assert!(whole.contains("• inner"), "first inner item");
        assert!(whole.contains("• next"), "second outer item");
        // Inner items should be compact (no blank between "• inner" and "• inner2").
        let inner_idx = result.iter().position(|l| l.to_string().contains("inner2"));
        assert!(inner_idx.is_some(), "inner2 should appear");
        let i = inner_idx.unwrap();
        // The line before inner2 should be "  • inner", not a blank line.
        assert!(
            i >= 1 && result[i - 1].width() > 0,
            "inner items should be compact (no blank before inner2)"
        );
        // But there should be one blank line before "• next" (outer is multi-line).
        let next_idx = result.iter().position(|l| l.to_string().contains("next"));
        assert!(next_idx.is_some(), "next should appear");
        let n = next_idx.unwrap();
        assert!(
            n >= 1 && result[n - 1].width() == 0,
            "expected blank line before '• next', got lines[{}]='{}'",
            n - 1,
            result[n - 1]
        );
        // No consecutive blank lines anywhere.
        let has_double_blank = result
            .windows(2)
            .any(|w| w[0].width() == 0 && w[1].width() == 0);
        assert!(
            !has_double_blank,
            "should not have two consecutive blank lines\n{whole}"
        );
    }

    #[test]
    fn ordered_list_items_compact_when_single_line() {
        let result = markdown_lines("1. first\n2. second\n3. third", 80);
        let blank: Vec<bool> = result
            .windows(2)
            .map(|w| w[0].width() == 0 && w[1].width() > 0)
            .collect();
        assert_eq!(
            blank.iter().filter(|&&b| b).count(),
            0,
            "single-line ordered items should have no blank lines between them"
        );
    }

    #[test]
    fn mixed_list_and_paragraph_separated_by_one_blank() {
        let md = "paragraph\n- list";
        let result = markdown_lines(md, 80);
        let blank: Vec<bool> = result
            .windows(2)
            .map(|w| w[0].width() == 0 && w[1].width() > 0)
            .collect();
        assert_eq!(
            blank.iter().filter(|&&b| b).count(),
            1,
            "one blank line between para and list"
        );
    }

    // ── code block wrapping ───────────────────────────────────────────────

    #[test]
    fn code_block_wraps_long_line() {
        let long = "x".repeat(200);
        let md = format!("```rust\n{long}\n```");
        let result = markdown_lines(&md, 40);
        // The code content should be wrapped. Each wrapped segment should be
        // at most 40 columns wide.
        for line in &result {
            let text = line.to_string();
            // Skip fence lines
            if text.starts_with("```") {
                continue;
            }
            assert!(
                line.width() <= 40,
                "wrapped code line width {} exceeds 40: {text:?}",
                line.width()
            );
        }
        // Count non-fence lines to verify wrapping actually happened.
        let content_line_count = result
            .iter()
            .filter(|l| !l.to_string().starts_with("```"))
            .count();
        assert!(
            content_line_count > 3,
            "long code line should wrap into {content_line_count} lines, expected > 3"
        );
    }

    #[test]
    fn code_block_wrap_trailing_whitespace_stripped() {
        // A line that *exactly* fills the width produces a trailing space
        // span from the word-wrapper; the code-block renderer should strip it.
        let md = format!("```\n{}\n```", "a".repeat(30));
        let result = markdown_lines(&md, 30);
        // The code line should not end with a visible trailing whitespace span.
        // Every span's content should be non-empty or absent.
        for line in &result {
            let text = line.to_string();
            if text.starts_with("```") {
                continue;
            }
            // The string representation of ratatui trims trailing whitespace
            // but the spans are what matter.  Verify no span is pure whitespace.
            for span in &line.spans {
                let trimmed = span.content.trim();
                if trimmed.is_empty() {
                    // Allow empty spans only at width 0 (blank lines)
                    assert_eq!(
                        span.width(),
                        0,
                        "non-empty whitespace-only span should not exist"
                    );
                }
            }
        }
    }

    #[test]
    fn code_block_no_wrap_when_fits() {
        let md = "```\nshort\n```";
        let result = markdown_lines(md, 80);
        assert!(!result.is_empty());
        let code_line = result.get(1).expect("second line should be code");
        assert_eq!(
            code_line.to_string(),
            "short",
            "code should not wrap when short"
        );
    }

    #[test]
    fn code_block_indented_wrapping() {
        let long = "x".repeat(100);
        let md = format!("> ```\n> {long}\n> ```");
        let result = markdown_lines(&md, 40);
        // Each code content line in the blockquote should be ≤ 40 (indent 2 + " > " prefix).
        for line in &result {
            let text = line.to_string();
            if text.starts_with(" ```") || text.starts_with("> ```") || text.starts_with(">  ```") {
                continue;
            }
            assert!(
                line.width() <= 40,
                "indented code line width {} exceeds 40: {text:?}",
                line.width()
            );
        }
    }
}
