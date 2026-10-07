//! Turn assembly: renders a [`Turn`] into the chat-history margin pattern and
//! records the per-line copy metadata — content ranges, copy joins, and copy
//! chrome — that the selection/copy layer consumes.
//!
//! Split out of the [`markdown_render`](super) façade so the latter stays the
//! public API surface plus the block-level entry points; the margin/assembly
//! code and its per-row classification helpers live here.

use super::{
    IncrementalMarkdown, LineChrome, LineJoin, RenderedTurnLines, RowContent, ansi_lines_joined,
    classify_row_content, expand_tabs, markdown_lines_joined, plain_text_lines_joined,
    sanitize_for_terminal,
};
use crate::render::{BG_SHADE, format_timestamp};
use choreo_proto::Turn;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Push `n` empty [`LineChrome`] entries onto `chrome`.
///
/// Used for the sources that do not emit chrome intervals (plain-text and ANSI
/// tool bodies): they still need one aligned entry per emitted row so the
/// chrome buffer stays in lockstep with `lines`/`joins`.
fn push_empty_chrome(chrome: &mut Vec<LineChrome>, n: usize) {
    chrome.extend(std::iter::repeat_with(LineChrome::default).take(n));
}

// ── Margin helpers (reused from current render system) ─────────────────

/// Structural rows: top separator, top padding, bottom padding, bottom separator.
pub(crate) const MARGIN_STRUCTURAL_ROWS: usize = 4;

/// Return type of [`add_margin_lines`]: the wrapped lines, their total
/// height, the per-line content column ranges, and the copy-join and
/// copy-chrome metadata.
type MarginLines = (
    Vec<Line<'static>>,
    usize,
    Vec<Option<(usize, usize)>>,
    Vec<LineJoin>,
    Vec<LineChrome>,
);

/// Wrap content lines with a vertical accent bar on the left and dark-gray
/// background shading.
///
/// Returns the wrapped lines, their total height, and a per-line content
/// column range aligned with the returned lines: `(5, 5 + line.width())` for
/// content rows (the base content interval between the `"  ┃  "` gutter and
/// the trailing fill), `None` for the structural chrome rows (separator,
/// padding).  The per-line [`LineJoin`] metadata is carried through
/// unchanged: structural chrome rows are fresh lines, content rows keep the
/// join their producer gave them.  The [`LineChrome`] buffer is translated by
/// the gutter's 5-column prefix and carried through: the inner block-layer
/// chrome (block-quote bars, code-box frame/padding) then sits in the same
/// column space as the base range, so the selection/copy can subtract it.
pub(crate) fn add_margin_lines(
    lines: Vec<Line<'static>>,
    joins: Vec<LineJoin>,
    chrome: Vec<LineChrome>,
    content_width: u16,
    accent: Color,
    timestamp_ms: Option<i64>,
) -> MarginLines {
    let gray = Style::default().bg(BG_SHADE);
    let no_shading = Style::default().bg(Color::Reset);
    let accent_line = Style::default().fg(accent).bg(Color::Reset);
    // Rows span `content_width + 9` columns: a 2-column blank margin, a `┃`
    // gutter + 2-column shading on the left, the text + trailing fill, then
    // 2 shaded + 2 blank columns on the right (the blanks are the 2-column
    // margin between the viewport and the scrollbar).  The padding row grabs
    // `content_width + 4` of shaded middle so it lines up with every content
    // row.
    let total_width = content_width as usize + 9;
    let shaded_content = content_width as usize + 4;

    // Top separator: no shading
    let separator = Line::from(vec![Span::styled(" ".repeat(total_width), no_shading)]);

    // Padding row: 2-column left margin, gutter, shaded middle, 2-column
    // right margin ending before the scrollbar column.
    let padding = Line::from(vec![
        Span::styled("  ", no_shading),
        Span::styled("┃", accent_line),
        Span::styled(" ".repeat(shaded_content), gray),
        Span::styled("  ", no_shading),
    ]);

    let mut result = Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    let mut content_ranges: Vec<Option<(usize, usize)>> =
        Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    let mut box_joins: Vec<LineJoin> = Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    let mut box_chrome: Vec<LineChrome> = Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    result.push(separator);
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);
    box_chrome.push(LineChrome::default());
    result.push(padding.clone());
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);
    box_chrome.push(LineChrome::default());

    for ((line, join), chrome) in lines.into_iter().zip(joins).zip(chrome) {
        // The base content interval ends where the trailing fill begins; the
        // fill is layout chrome outside any content range and needs no
        // exclusion interval of its own.
        let row_width = line.width();
        // Classify the row against its recorded chrome: a code box's bordered
        // padding row is *blank content* (its non-chrome columns are spaces), so
        // it keeps an empty base range and the selection copies a blank line
        // rather than the padding spaces; a pure-chrome row (a box rule) is
        // dropped; everything else is real content.
        let row_content = classify_row_content(&line, &chrome);
        let fill = (content_width as usize).saturating_sub(row_width);

        // 2-column blank margin, then the gutter, then 2 shaded columns before
        // the text (the symmetric 2-column margin layout for message blocks).
        let mut spans = vec![
            Span::styled("  ", no_shading),
            Span::styled("┃", accent_line),
            Span::styled("  ", gray),
        ];
        // Content spans — stamp the message background on every span so the
        // content sits inside the shaded box.  No producer sets its own
        // background anymore (the code box draws a table-style frame instead of
        // a filled panel), so the shading is unconditional.
        spans.extend(
            line.spans
                .into_iter()
                .map(|s| Span::styled(s.content, s.style.bg(BG_SHADE))),
        );
        spans.push(Span::styled(" ".repeat(fill), gray));
        spans.push(Span::styled("  ", gray));
        // 2-column blank margin between the shaded box and the scrollbar.
        spans.push(Span::styled("  ", no_shading));

        result.push(Line::from(spans));
        // Content occupies the row's base content interval, offset by the
        // `"  ┃  "` gutter (2-col margin + gutter + 2-col shading = 5 columns);
        // the producer's chrome is shifted into that same column space.  A blank
        // row keeps an empty range so the selection copies a blank line.
        content_ranges.push(match row_content {
            RowContent::Chrome => None,
            RowContent::Blank => Some((5, 5)),
            RowContent::Content => Some((5, 5 + row_width)),
        });
        box_joins.push(join);
        let mut row_chrome = LineChrome::default();
        row_chrome.extend_shifted(&chrome, 5);
        box_chrome.push(row_chrome);
    }

    result.push(padding);
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);
    box_chrome.push(LineChrome::default());

    // Bottom separator: right-aligned timestamp (user messages only).
    if let Some(ms) = timestamp_ms {
        // format_timestamp expects milliseconds — pass the value through
        // unchanged.  (Dividing by 1000 here rendered every user message
        // as a 1970 date after format_timestamp switched to millis.)
        //
        // The timestamp is right-aligned to the message's shaded block: the
        // shaded area's last column is `total_width - 3`, so ending the text
        // at column `total_width - 4` leaves exactly one blank column
        // (total_width - 3) between it and the shading — the timestamp sits
        // under the shaded area, clear of the 2-column right margin.
        let ts_text = format_timestamp(ms);
        let ts_len = ts_text.len();
        let left_fill = total_width.saturating_sub(ts_len + 3);
        result.push(Line::from(vec![
            Span::styled(" ".repeat(left_fill), no_shading),
            Span::styled(ts_text, no_shading),
            Span::styled(" ".repeat(3), no_shading),
        ]));
    } else {
        result.push(Line::from(vec![Span::styled(
            " ".repeat(total_width),
            no_shading,
        )]));
    }
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);
    box_chrome.push(LineChrome::default());

    let total_rows = result.len();
    (result, total_rows, content_ranges, box_joins, box_chrome)
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
    render_turn_lines_impl(
        turn,
        content_width,
        tool_content_width,
        reasoning_expanded,
        tool_results_collapsed,
        None,
    )
}

/// [`render_turn_lines`] for the streaming fast path: the assistant response is
/// rendered through `response_cache` ([`IncrementalMarkdown`]), so each frame
/// re-parses only the response tail rather than the whole response.  The output
/// is byte-identical to [`render_turn_lines`] at every prefix.
///
/// Only the response is incremental; the reasoning section and tool-result
/// bodies still render whole each frame (they are stable while the response
/// streams, and were out of scope for the response-parse fix).
pub(crate) fn render_turn_lines_streaming(
    turn: &Turn,
    content_width: u16,
    tool_content_width: u16,
    reasoning_expanded: bool,
    tool_results_collapsed: &[bool],
    response_cache: &mut IncrementalMarkdown,
) -> RenderedTurnLines {
    render_turn_lines_impl(
        turn,
        content_width,
        tool_content_width,
        reasoning_expanded,
        tool_results_collapsed,
        Some(response_cache),
    )
}

fn render_turn_lines_impl(
    turn: &Turn,
    content_width: u16,
    tool_content_width: u16,
    reasoning_expanded: bool,
    tool_results_collapsed: &[bool],
    response_cache: Option<&mut IncrementalMarkdown>,
) -> RenderedTurnLines {
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

    let mut all_lines: Vec<Line<'static>> = Vec::new();
    // Per-line content column ranges, aligned with `all_lines` (see
    // `RenderedTurnLines::content_ranges`).  Every line kind below records
    // where its real text starts/ends so selection never copies UI chrome.
    let mut all_content_ranges: Vec<Option<(usize, usize)>> = Vec::new();
    // Per-line copy-join metadata, aligned with `all_lines` (see `LineJoin`).
    // The selection extraction uses this to undo the renderer's wrapping.
    let mut all_joins: Vec<LineJoin> = Vec::new();
    // Per-line non-selectable-chrome metadata, aligned with `all_lines` (see
    // `LineChrome`).  The block layer records the block-quote bars and the
    // code box's frame/padding here; the assembly translates them into each
    // row's gutter-offset column space so the selection/copy can subtract them
    // without re-scanning span text.
    let mut all_chrome: Vec<LineChrome> = Vec::new();

    // ── User text block (green accent) ───────────────────────
    // Rendered first so a failed request's transcript still shows what the
    // user asked for above the error that stopped it.
    if let Some(ref text) = turn.user_text {
        let (body, body_joins, body_chrome) = markdown_lines_joined(text, content_width);
        let timestamp_ms = Some(turn.created_at.as_millis());
        let (margin_lines, _rows, margin_ranges, margin_joins, margin_chrome) = add_margin_lines(
            body,
            body_joins,
            body_chrome,
            content_width,
            Color::Green,
            timestamp_ms,
        );
        all_lines.extend(margin_lines);
        all_content_ranges.extend(margin_ranges);
        all_joins.extend(margin_joins);
        all_chrome.extend(margin_chrome);
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
            all_chrome.push(LineChrome::default());
            all_lines.push(line);
        }
        return RenderedTurnLines {
            lines: all_lines,
            joins: all_joins,
            content_ranges: all_content_ranges,
            chrome_ranges: all_chrome,
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
        let mut body_chrome: Vec<LineChrome> = Vec::new();

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
                // Streaming reuses the committed prefix; a one-shot render parses
                // the whole response.
                let (lines, joins, chrome) = match response_cache {
                    Some(cache) => cache.render(trimmed, content_width),
                    None => markdown_lines_joined(trimmed, content_width),
                };
                body.extend(lines);
                body_joins.extend(joins);
                body_chrome.extend(chrome);
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
                body_chrome.push(LineChrome::default());
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
            body_chrome.push(LineChrome::default());
            if reasoning_expanded && let Some(ref reasoning) = turn.assistant_reasoning {
                let (lines, joins, chrome) = markdown_lines_joined(reasoning.trim(), content_width);
                body.extend(lines);
                body_joins.extend(joins);
                body_chrome.extend(chrome);
            }
            reasoning_header_idx =
                Some(all_lines.len() + MARGIN_STRUCTURAL_ROWS / 2 + header_idx_in_body);
        }

        // If we have content, wrap with margin lines (no timestamp).
        if !body.is_empty() {
            let (margin_lines, _rows, margin_ranges, margin_joins, margin_chrome) =
                add_margin_lines(
                    body,
                    body_joins,
                    body_chrome,
                    content_width,
                    Color::Blue,
                    None,
                );
            all_lines.extend(margin_lines);
            all_content_ranges.extend(margin_ranges);
            all_joins.extend(margin_joins);
            all_chrome.extend(margin_chrome);
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
        let mut body_chrome: Vec<LineChrome> = Vec::new();

        // Invocation description rendered as markdown so inline code and
        // emphasis highlight properly.  Its first line becomes the header
        // row (triangle + description); any continuation lines are part of
        // the always-visible summary (see below).  Wrapped two columns
        // narrower than the content width because the header prepends the
        // triangle glyph ("▶ ") to the first line — wrapping at the full
        // width would push the header row past the right edge.
        let (desc_lines, desc_joins, desc_chrome) = if tr.invocation_description.is_empty() {
            (Vec::new(), Vec::new(), Vec::new())
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
        body_chrome.push(LineChrome::default());
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
            body_chrome.extend(desc_chrome.into_iter().skip(1));
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
                body_chrome.push(LineChrome::default());
                body.push(Line::from(Span::styled(
                    format!("{label}: {}", tr.name),
                    Style::default().fg(accent),
                )));
                body_joins.push(LineJoin::Break);
                body_chrome.push(LineChrome::default());
            }
            // Full content body — rendered for every expanded result.  The
            // old hard "quiet" suppression is now just the default collapse
            // state: expanding a quiet tool reveals the verbatim content.
            if !tr.content.is_empty() {
                body.push(Line::from(Span::styled(String::new(), Style::default())));
                body_joins.push(LineJoin::Break);
                body_chrome.push(LineChrome::default());
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
                    push_empty_chrome(&mut body_chrome, lines.len());
                    body.extend(lines);
                    body_joins.extend(joins);
                } else if tr.is_error {
                    let (lines, joins) = plain_text_lines_joined(&content, tool_content_width);
                    push_empty_chrome(&mut body_chrome, lines.len());
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
                    let (lines, joins, chrome) =
                        markdown_lines_joined(&content, tool_content_width);
                    body.extend(lines);
                    body_joins.extend(joins);
                    body_chrome.extend(chrome);
                } else {
                    let (lines, joins) = plain_text_lines_joined(&content, tool_content_width);
                    push_empty_chrome(&mut body_chrome, lines.len());
                    body.extend(lines);
                    body_joins.extend(joins);
                }
            }
        }

        // No left indent (the 2-column margin was removed); every row spans
        // the full area width with exactly 1 column of right margin.
        debug_assert_eq!(
            body.len(),
            body_chrome.len(),
            "tool-body chrome must align with its rows"
        );
        for ((line, join), chrome) in body.into_iter().zip(body_joins).zip(body_chrome) {
            let mut line = line;
            // The row's content span before the unboxed trailing fill is
            // appended: the base content interval ends where the fill begins.
            let content_sum: usize = line.spans.iter().map(ratatui::prelude::Span::width).sum();
            // Classify the row against its recorded chrome (see
            // `classify_row_content`): a code box's bordered padding row is
            // blank content, a box rule is pure chrome (dropped), and any other
            // row is content.  This replaces the old whitespace-only test, which
            // can no longer see that a `│`-bordered row is blank.
            let row_content = classify_row_content(&line, &chrome);
            let fill = (tool_content_width as usize).saturating_sub(content_sum);
            line.spans
                .push(Span::styled(" ".repeat(fill), Style::default()));
            line.spans.push(Span::styled(" ", Style::default()));
            // Unboxed rows: no gutter, so the base content interval is the
            // full row width and the block-layer chrome sits in the same
            // column space already (no prefix to translate).
            let end = content_sum.min(tool_content_width as usize);
            all_content_ranges.push(match row_content {
                RowContent::Chrome => None,
                RowContent::Blank => Some((0, 0)),
                RowContent::Content => Some((0, end)),
            });
            all_joins.push(join);
            all_chrome.push(chrome);
            all_lines.push(line);
        }
    }

    // If no sections produced output, emit a blank line.
    if all_lines.is_empty() {
        all_lines.push(Line::from(Span::styled(String::new(), Style::default())));
        all_content_ranges.push(None);
        all_joins.push(LineJoin::Break);
        all_chrome.push(LineChrome::default());
    }

    RenderedTurnLines {
        lines: all_lines,
        joins: all_joins,
        content_ranges: all_content_ranges,
        chrome_ranges: all_chrome,
        reasoning_header_idx,
        tool_result_header_idxs,
    }
}
