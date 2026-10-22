//! The TUI's frame composition: what each page draws, dispatched from one
//! [`render`] entry point.
//!
//! The module is split by subject, and this file is the seam:
//!
//! - [`history`] — the chat history pane (visible turns, clipping, text blocks)
//! - [`image`] — the fullscreen viewer and the per-turn inline image blocks
//! - [`status`] — the session-status vocabulary and the keystore-lock marker
//! - [`format`] — timestamps and the cumulative-token readouts
//! - the page submodules (`welcome`, `session_manager`, `ai_providers`,
//!   `model_selector`, `command_palette`, `login`), each owning one screen or
//!   overlay
//!
//! What stays here is exactly the frame-level plumbing: the sink normalizer
//! ([`display_text`]/[`display_text_owned`]), the [`BG_SHADE`] the history
//! shading and the scrollbar track share, the Chat page layout and its cursor,
//! the mouse hit-test helpers the connection layer calls, and the [`render`]
//! dispatch that fixes the z-order of the pages and the overlays drawn on top
//! of them.
//!
//! Everything moved out is re-exported below, so a caller keeps writing
//! `crate::render::<name>` and never learns which file an item lives in. The
//! submodules reach back here for the shared sink normalizer via `super::`.
//! Popup sizing/centering and the LIST-popup 3-band layout live in
//! `state/pages.rs` (`centered_popup`, `selector_list_layout`) so the renderers,
//! the connection-layer mouse handlers, and the viewport cache share one
//! geometry.

use crate::markdown_render::display_width;
use crate::scrollbar::{SmoothScrollbar, SmoothScrollbarState};
use crate::selection;
use crate::state::{
    App, HELP_LINE1, HELP_LINE2, INPUT_PAD, Page, cached_visual_lines, input_inner_width,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};

/// Normalize one piece of UI text before a widget draws it.
///
/// Display text reaches the screen through one of two tiers, and this function
/// is the second:
///
/// 1. **Ingest** — text the daemon authors that is *only* displayed is
///    normalized once where it is stored: the status/error line
///    ([`App::set_status`] / [`App::set_error`], via
///    [`crate::state::normalized_display_text`]), the AI-providers sub-modal
///    error lines, and the login modal's status text, authorize URL and device
///    code.  Because the stored value is already inert, it stays inert at every
///    place it is later read — including a non-ratatui consumer (the plain
///    `println!` output path, a log line, a clipboard copy) that has no widget
///    sink to route through.
/// 2. **Sink** — identity-bearing or keyed values stay raw in state and are
///    normalized here as they are drawn: a model id, a reasoning effort, an
///    account slug, a session title (which also feeds the OSC 2 window title
///    under its own strip policy), a tool name compared against
///    `MARKDOWN_TOOLS`, a tool-call id.  Escaping those in place would change
///    what is sent back to the daemon, what is compared, or what key it is
///    looked up by, so the rewriting happens at the last possible moment.
///
/// Both tiers are safe to combine: the normalizer is idempotent and borrows text
/// that is already normalized, so the sink pass over an ingest-normalized
/// string costs one scan and allocates nothing.  The sink therefore stays as
/// belt-and-braces for whatever a future widget forgets to place on the ingest
/// tier.
///
/// What it stops: a control character that would occupy a measured column but
/// draw nothing (misaligning a code box's frame, a table row, or a wrapped
/// block's height) and a live escape sequence riding along into the terminal.
///
/// What the user is *editing* is deliberately excluded: the prompt buffer and
/// every filter field (the command palette's, the model selector's, the
/// provider wizard's) draw their raw bytes.  Those bytes are the ones the
/// daemon receives, and the buffer's cursor is a byte offset into them, so
/// rewriting the text on the way to the screen would both desync the cursor
/// from the row it draws on and change what the user ends up submitting.
///
/// That exemption costs no measurement honesty, because the buffer is sanitized
/// where it is filled: every paste passes the terminal-event choke point's
/// [`sanitize_pasted_text`](crate::text_filter::sanitize_pasted_text), which
/// drops control characters, expands tabs and folds CR before the bytes enter
/// any input buffer.  The raw bytes the widgets draw are therefore already
/// measured-exactly-drawn text.
///
/// The one implementation is the terminal-safety filter's
/// [`normalize_display_text`](crate::text_filter::normalize_display_text);
/// this is the entry point for the plain widgets that share the same sink rule.
pub(crate) fn display_text(text: &str) -> std::borrow::Cow<'_, str> {
    crate::text_filter::normalize_display_text(text)
}

/// [`display_text`] for a sink that stores the text — a [`String`] field, a
/// tuple element, a `Paragraph`'s content — rather than borrowing it for the
/// duration of a draw.
///
/// The borrow fast path only helps a caller that can keep the borrow alive; at
/// an owned-string sink the text has to be copied either way, so this says so
/// at the call site instead of spelling the same thing as `.into_owned()` on
/// every line.
pub(crate) fn display_text_owned(text: &str) -> String {
    display_text(text).into_owned()
}

mod ai_providers;
mod command_palette;
mod format;
mod history;
mod image;
mod login;
mod model_selector;
mod session_manager;
mod status;
mod welcome;

#[cfg(test)]
mod tests;

use self::ai_providers::render_ai_providers;
use self::command_palette::render_command_palette;
use self::login::render_login_modal;
use self::model_selector::render_model_selector;
use self::session_manager::render_session_manager;
use self::welcome::render_welcome;

use self::history::render_history;
use self::image::render_fullscreen_only;
use self::status::KEYS_LOCKED_MARKER;

// Re-exported so every `crate::render::<name>` path outside this file keeps
// resolving after the split: the markdown renderer and its tests import
// `format_timestamp`, and `render_tests` glob-imports the rest.
pub(crate) use self::format::{format_timestamp, session_detail_tokens_line, status_token_readout};
pub(crate) use self::status::{format_status, status_display};
// `slot_label` is drawn only from within `image` itself; its sole outside
// caller is the sink test, so the re-export is test-only and does not trip
// `unused_imports` in the library build.
#[cfg(test)]
pub(crate) use self::image::slot_label;

pub(crate) const BG_SHADE: Color = Color::Rgb(53, 53, 53);

pub(crate) fn mouse_in_history_box(column: u16, row: u16, vp_width: u16, vp_height: u16) -> bool {
    column < vp_width && row < vp_height
}

pub(crate) fn mouse_in_scrollbar_column(
    column: u16,
    row: u16,
    vp_width: u16,
    vp_height: u16,
) -> bool {
    column == vp_width && row < vp_height
}

fn vertical_scrollbar() -> SmoothScrollbar {
    SmoothScrollbar::new()
        .thumb_fg(Color::DarkGray)
        .track_bg(BG_SHADE)
        .marker_fg(Color::Green)
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    if render_fullscreen_only(frame, app) {
        return;
    }

    // `render_chat` returns the command input box `Rect` it just laid out, so
    // the command palette (drawn below) can reuse it instead of recomputing
    // the whole Chat-page layout a second time in the same frame.
    let chat_input_rect = match app.page {
        Page::Chat => Some(render_chat(frame, app)),
        Page::SessionManager => {
            render_session_manager(frame, app);
            None
        }
        Page::AIProviders => {
            render_ai_providers(frame, app);
            None
        }
    };

    // The model selector overlay draws on top of the Chat page content.
    if app.model_selector.is_open() {
        render_model_selector(frame, app);
    }

    // The inline command palette floats above the command input box on the
    // Chat page.  Drawn after the model selector so the selector (when open)
    // covers it; it is suppressed entirely while the selector is open (see
    // `command_palette_active`).  It reuses the input-box rect `render_chat`
    // just computed rather than re-deriving the layout.
    if let Some(input_rect) = chat_input_rect
        && app.command_palette_active()
    {
        render_command_palette(frame, app, input_rect);
    }

    // The OAuth login modal is the topmost overlay: it covers whatever page is
    // beneath and swallows input until the flow ends or the user cancels.
    if app.login.is_some() {
        render_login_modal(frame, app);
    }
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App) -> Rect {
    // Compute the Chat page's vertical layout via the shared helper so that
    // rendering, mouse hit-testing (connection.rs click-to-position), and the
    // history viewport (update_viewport_from_terminal_size) all use identical
    // math — they can never drift apart, even on tiny terminals where the
    // layout solver shrinks the fixed-height chunks.
    let [
        history_area,
        status_error_area,
        help_area,
        input_area,
        status_bar_area,
    ] = app.chat_page_layout(frame.area().width, frame.area().height);

    // Reserve 1 column on the right for the scrollbar
    let history_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(history_area);

    // Build height_prefix and visible_turn_ids BEFORE rendering history,
    // so render_history iterates the correct set of visible turns rather
    // than an empty visible_turn_ids on the first frame after session data arrives.
    app.compute_total_height_and_markers();
    // The rebuild above settles content-induced viewport movement (streaming
    // growth, appended turns, undo/redo); re-anchor an in-progress selection's
    // live head to the content now under the cursor so the highlight (and the
    // copy on release) tracks the pointer even when no mouse event arrived —
    // the anchor stays pinned to its text.  See `selection::follow_cursor`.
    selection::follow_cursor(app);
    // Layout::horizontal([Min(1), Length(1)]) always yields exactly 2 chunks;
    // a zero-area fallback rect is harmless to render into.
    let history_area = history_chunks.first().copied().unwrap_or_default();
    // An empty session shows the setup checklist instead of a blank pane, so
    // a first-time user is told what to do before their first message.
    if app.show_welcome() {
        render_welcome(frame, history_area, app);
    } else {
        render_history(frame, history_area, app);
    }

    // ── Scrollbar ────────────────────────────────────────────
    let viewport_height = app.history_viewport.height as usize;
    let total_height = app.total_history_height();
    if app.scrollbar_visible() {
        let position = app
            .max_scroll_offset()
            .saturating_sub(app.effective_scroll());
        let marker_slots: Vec<usize> = app
            .active_display_ref()
            .map_or_default(|d| d.markers.iter().map(|m| m.virtual_slot).collect());
        frame.render_stateful_widget(
            vertical_scrollbar().with_markers(&marker_slots),
            history_chunks.get(1).copied().unwrap_or_default(), // always present (2-chunk split); safe fallback
            &mut SmoothScrollbarState::new(total_height)
                .position(position)
                .viewport_content_length(viewport_height),
        );
    }

    // ── Status/error bar (above command box) ──────────────────
    let notify_area = Rect {
        x: status_error_area.x + 1,
        width: status_error_area.width.saturating_sub(2),
        ..status_error_area
    };
    if let Some(err) = app.error() {
        // No sink normalization here: `App::set_error` already normalized the
        // text at ingest, so these bytes are inert and width-honest as stored
        // and the span can borrow them directly.  (Text that is not normalized
        // at ingest still goes through `display_text` at its widget.)
        let err_para = Paragraph::new(Text::from(err))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false });
        frame.render_widget(err_para, notify_area);
    } else if let Some(status) = app.status() {
        // Same ingest guarantee as the error line above.
        let status_para = Paragraph::new(Text::from(status))
            .style(Style::default().fg(Color::Green))
            .wrap(Wrap { trim: false });
        frame.render_widget(status_para, notify_area);
    }

    // ── Help overlay (2 lines, conditional) ───────────────────
    if app.show_help_overlay {
        let help_inner = Rect {
            x: help_area.x + 1,
            width: help_area.width.saturating_sub(2),
            ..help_area
        };
        let help = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(HELP_LINE1, Style::default().fg(Color::Cyan))),
            Line::from(Span::styled(HELP_LINE2, Style::default().fg(Color::Cyan))),
        ]));
        frame.render_widget(help, help_inner);
    }

    // ── Command input box ──────────────────────────────────────
    // Inner width = box width minus INPUT_PAD padding on both sides.
    // The box draws no left/right borders, so padding is the only loss.
    // This must match input_inner_width() used by the height estimation
    // (input_bar_content_lines) or wrapped lines won't grow the box.
    let inner_width = input_inner_width(input_area.width);
    let visible_height = (input_area.height.saturating_sub(2)) as usize;

    // Compute cursor position first (populates the lines cache) so we
    // can then borrow separate fields of app.input for the cached lines.
    let (vrow, vcol) = app.input.cursor_visual_pos(inner_width);

    let all_visual_lines = cached_visual_lines(
        &app.input.text,
        inner_width,
        app.input.generation,
        &mut app.input.lines_cache,
    );

    // Apply scroll offset — only show the visible window.
    let visible_count = visible_height.max(1).min(all_visual_lines.len());
    let offset = app
        .input
        .scroll_offset
        .min(all_visual_lines.len().saturating_sub(visible_count));
    let visible_lines = all_visual_lines
        .get(offset..offset + visible_count)
        .unwrap_or(&[]);
    let text_lines: Vec<Line> = visible_lines
        .iter()
        .map(|vl| {
            Line::from(
                app.input
                    .text
                    .get(vl.start_byte..vl.end_byte)
                    .unwrap_or_default(),
            )
        })
        .collect();

    // A command line (the buffer starts with `/`) gets a block title as the
    // visible affordance telling the user Enter will RUN the command rather
    // than submit a prompt; the `/` itself is drawn from the buffer text above.
    // Read the derived predicate before the text borrow above is consumed so
    // the block can be built without a borrow conflict.
    let mut input_block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .padding(Padding::new(INPUT_PAD, INPUT_PAD, 0, 0));
    if app.command_palette_active() {
        input_block = input_block
            .title(" command ")
            .border_style(Style::default().fg(Color::Cyan));
    }

    let input = Paragraph::new(Text::from(text_lines)).block(input_block);
    frame.render_widget(input, input_area);
    // Clamp to visible area so the cursor is always inside the box,
    // even when scroll_offset hasn't been adjusted yet (e.g. after
    // loading a long history entry that ends at scroll_offset = 0).
    // Both casts are safe: `visible_count` is bounded by the input box
    // height (a `u16` area dimension), and `offset <= vrow`, which is itself
    // a `u16` cursor row.
    #[expect(clippy::cast_possible_truncation)]
    let max_display_row = (visible_count as u16).saturating_sub(1);
    #[expect(clippy::cast_possible_truncation)]
    let display_vrow = vrow.saturating_sub(offset as u16).min(max_display_row);
    let cursor_x = input_area.x.saturating_add(INPUT_PAD).saturating_add(vcol);
    let cursor_y = input_area.y.saturating_add(1).saturating_add(display_vrow);
    frame.set_cursor_position((cursor_x, cursor_y));

    // ── Status bar (single line) ───────────────────────────────
    let has_session = app.attached_session_id.is_some();

    let mut spans: Vec<Span> = Vec::new();
    // PERSISTENT lock banner: when the daemon's keystore is locked, lead the
    // status bar with a lock marker. Driven directly by the latched
    // `keystore_locked` flag (set by the daemon's lock-state broadcasts and
    // the subscribe-time push in `handle_daemon_message`), so it is NOT
    // cleared by the per-keypress transient status/error clear — the lock
    // indication survives every keystroke until the daemon reports unlocked.
    if app.keystore_locked {
        spans.push(Span::styled(
            KEYS_LOCKED_MARKER,
            Style::default().fg(Color::Yellow),
        ));
        spans.push(Span::raw(" | "));
    }

    if has_session {
        // Session-identity values (wd, account, model, reasoning) — stable
        // across the session — go first (left side) so the bar's leading edge
        // stays fixed.  Runtime metrics (tokens, context fill) follow on the
        // right where their per-turn updates don't shift the identity fields.
        let wd = app
            .active_display_ref()
            .and_then(|d| d.working_dir.as_deref())
            .unwrap_or("-");
        let account = app.attached_account_slug.as_deref().unwrap_or("-");
        let model = app
            .active_display_ref()
            .and_then(|d| d.selected_model.as_deref())
            .unwrap_or("-");
        let reasoning = app
            .active_display_ref()
            .and_then(|d| d.reasoning_effort.as_deref())
            .unwrap_or("-");

        // Runtime metrics: tokens flow and context-window fill.
        let tokens = match &app.display_token_usage() {
            Some(usage) => status_token_readout(usage),
            None => String::new(),
        };
        // `?` cleanup: while the keystore is locked the context window is not
        // loaded into memory (credentials are undecrypted), so `last_prompt_tokens`
        // being `Some` alongside a `None` context_window would render a
        // misleading `X / ?`. Render NO context readout at all when locked — the
        // lock banner already reports the state, and there is no sensibly
        // interpretable fill percentage.
        let context = if app.keystore_locked {
            String::new()
        } else {
            match (
                app.active_display_ref().and_then(|d| d.context_window),
                app.active_display_ref().and_then(|d| d.last_prompt_tokens),
            ) {
                (Some(limit), Some(current)) => {
                    let ratio = if limit > 0 {
                        f64::from(current) / f64::from(limit)
                    } else {
                        0.0
                    };
                    format!(
                        "{} / {} ({})",
                        humfmt::number(current),
                        humfmt::number(limit),
                        humfmt::percent(ratio),
                    )
                }
                (Some(limit), None) => {
                    format!("0 / {} ({})", humfmt::number(limit), humfmt::percent(0.0))
                }
                (None, Some(current)) => format!("{} / ?", humfmt::number(current)),
                (None, None) => String::new(),
            }
        };

        // Tool groups can change at runtime via load_tools/unload_tools.
        let tool_groups = app.attached_tool_groups.join(", ");

        // Order: stable identity first (wd, account, model, reasoning) so
        // the bar doesn't visually jitter when per-turn metrics appear or
        // disappear; tools in the middle; runtime metrics (tokens, context
        // window fill, active status) on the right.
        // ── Session identity (always present, default to "-") ──
        // Every value this block pushes — working directory, account name, model
        // id, reasoning effort, and the tool-group list — comes from the
        // filesystem, the keystore, the model catalog, or the daemon rather than
        // from the TUI: normalize each so it occupies exactly the columns it
        // draws in this single clipped line.
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            display_text_owned(wd),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            display_text_owned(account),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            display_text_owned(model),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            display_text_owned(reasoning),
            Style::default().fg(Color::White),
        ));

        // ── Tool groups (conditionally present) ──
        if !tool_groups.is_empty() {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                display_text_owned(&tool_groups),
                Style::default().fg(Color::DarkGray),
            ));
        }

        // ── Runtime metrics (tokens → context → status) ──
        if !tokens.is_empty() {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(tokens, Style::default().fg(Color::White)));
        }
        if !context.is_empty() {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(context, Style::default().fg(Color::White)));
        }
        if let Some(status) = &app.attached_status {
            let (label, color) = status_display(status);
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(label, Style::default().fg(color)));
        }
    }

    let status_bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Reset));
    frame.render_widget(status_bar, status_bar_area);

    // Return the command input box rect so the caller can reuse it (e.g. the
    // command palette anchored above the box) without recomputing the layout.
    input_area
}

fn set_input_cursor(
    frame: &mut Frame<'_>,
    area: Rect,
    line: u16,
    prefix_width: u16,
    text_before_cursor: &str,
) {
    // Cursor columns beyond `u16::MAX` cannot be addressed by any terminal
    // (ratatui positions are `u16`), so the cast is harmless in practice.
    #[expect(clippy::cast_possible_truncation)]
    let x = area.x + prefix_width + display_width(text_before_cursor) as u16;
    let y = area.y + line;
    frame.set_cursor_position((x, y));
}
