use crate::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};
use choreo_markdown::render_math_pretty;
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

mod block;
mod inline;
mod tables;
mod text;

// Re-export every submodule item so siblings, the test module, and the rest of
// the crate reach them through `markdown_render::…` (the paths external callers
// such as `render/` and `state/` already use).  Single-segment glob re-exports
// (`use text::*`) are the idiomatic way to hoist a private submodule's API into
// its parent.
pub(crate) use block::*;
pub(crate) use inline::*;
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

/// Number of leading display columns of `line` occupied by block-quote chrome
/// (the run of [`QUOTE_BAR`] gutter spans that `render_markdown_block`'s
/// `BlockQuote` arm prepends, two columns each).
///
/// The selection/copy machinery starts a quote row's content *after* this run
/// (see `add_margin_lines` and the tool-body loop in `render_turn_lines`) so
/// the bar is never copied — it is per-row rendering chrome, exactly like the
/// `┃` margin gutter.  Recognition keys on the exact span content *and* the
/// reserved [`QUOTE_BAR_COLOR`] foreground, so ordinary prose that happens to
/// begin with `│ ` (default-styled) is never mistaken for a gutter, and a code
/// span that renders `│ ` stays upright (its fg is `Cyan`, not the reserved
/// grey).
///
/// Only a *leading* run counts: a block quote nested directly inside a list
/// item has the list marker prepended in front of its bar, so the bar is no
/// longer leading and falls back to being copied.  That nesting is rare and
/// the fallback degrades gracefully (the whole row is copied).
fn leading_quote_prefix(line: &Line<'_>) -> usize {
    let mut width = 0;
    for span in &line.spans {
        if span.content.as_ref() == QUOTE_BAR && span.style.fg == Some(QUOTE_BAR_COLOR) {
            width += QUOTE_BAR_WIDTH;
        } else {
            break;
        }
    }
    width
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
            // Block-quote bars in a markdown tool body are chrome: start the
            // copy range after them (see `leading_quote_prefix`).  Computed
            // before the trailing fill spans are appended.
            let quote_prefix = leading_quote_prefix(&line);
            let content_sum: usize = line.spans.iter().map(ratatui::prelude::Span::width).sum();
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
            all_content_ranges.push(Some((quote_prefix.min(end), end)));
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
/// column range aligned with the returned lines: `(5, 5 + line.width())` for
/// content rows (the text between the `"  ┃  "` gutter and the trailing
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
    result.push(separator);
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);
    result.push(padding.clone());
    content_ranges.push(None);
    box_joins.push(LineJoin::Break);

    for (line, join) in lines.into_iter().zip(joins) {
        let text_width = line.width();
        // Leading block-quote chrome (the `│ ` bars) is drawn but excluded
        // from the row's copyable range, so a selection never picks up the
        // gutter.  Computed from the raw line, before the spans are restyled
        // with the message background below.
        let quote_prefix = leading_quote_prefix(&line);
        let fill = (content_width as usize).saturating_sub(text_width);

        // 2-column blank margin, then the gutter, then 2 shaded columns before
        // the text (the symmetric 2-column margin layout for message blocks).
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
        // 2-column blank margin between the shaded box and the scrollbar.
        spans.push(Span::styled("  ", no_shading));

        result.push(Line::from(spans));
        // Content occupies columns [5, 5 + text width): after the
        // `"  ┃  "` gutter (2-col margin + gutter + 2-col shading), up to
        // where the trailing fill begins.  A quoted row starts after its
        // leading bar run (`quote_prefix`), so the bar is copy-proof.
        content_ranges.push(Some((5 + quote_prefix, 5 + text_width)));
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
    let heading_shift =
        first_heading_level(&document.blocks).map_or(0, |level| (level.saturating_sub(1)) as usize);
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

#[cfg(test)]
mod tests;
