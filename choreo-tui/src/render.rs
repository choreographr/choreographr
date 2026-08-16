use crate::RenderedImage;
use crate::diff_render::truncate_str;
use crate::markdown_render::{display_width, reasoning_expanded_default, render_turn_lines};
use crate::scrollbar::{SmoothScrollbar, SmoothScrollbarState};
use crate::selection;
use crate::state::{
    AI_PROVIDER_ITEM_LINES, AIProvidersView, App, CTRL_HELP_LINE1, CTRL_HELP_LINE2, INPUT_PAD,
    Page, RenderCacheKey, SessionManagerView, cached_or_compute_lines, cached_visual_lines,
    input_inner_width,
};
use choreo_proto::{SessionStatus, TokenUsage};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, Wrap},
};
use ratatui_image::StatefulImage;
use std::sync::Arc;

use tui_prompts::{Prompt, TextPrompt};

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

    match app.page {
        Page::Chat => render_chat(frame, app),
        Page::SessionManager => render_session_manager(frame, app),
        Page::AIProviders => render_ai_providers(frame, app),
    }

    // The model selector overlay draws on top of the Chat page content.
    if app.model_selector.is_open() {
        render_model_selector(frame, app);
    }
}

/// Look up the fullscreen image by (turn_id, img_idx) and render it.
pub(crate) fn render_fullscreen_only(frame: &mut Frame<'_>, app: &mut App) -> bool {
    let Some((session_id, turn_id, img_idx)) = app.fullscreen_image_target else {
        return false;
    };
    if !app
        .rendered_images
        .get(&session_id)
        .is_some_and(|m| m.contains_key(&turn_id))
        && !app
            .display_for(session_id)
            .view
            .turns
            .get(&turn_id)
            .is_some_and(|t| !t.displayed_images.is_empty())
    {
        app.fullscreen_image_target = None;
        return false;
    }
    render_fullscreen_image(frame, session_id, turn_id, img_idx, app);
    true
}

fn render_fullscreen_placeholder(frame: &mut Frame<'_>) {
    let area = frame.area();
    let block = Block::bordered()
        .title(" Loading image … ")
        .title_alignment(Alignment::Center);
    frame.render_widget(block, area);
}

fn render_fullscreen_image(
    frame: &mut Frame<'_>,
    session_id: u64,
    turn_id: u32,
    img_idx: usize,
    app: &mut App,
) {
    let area = frame.area();
    let full = Size::new(area.width, area.height);

    // Ensure the rendered_images entry exists — create from turn data if missing.
    if !app.rendered_images.contains_key(&session_id) {
        let Some(turn) = app.display_for(session_id).view.turns.get(&turn_id) else {
            return;
        };
        let Some(record) = turn.displayed_images.get(img_idx) else {
            return;
        };
        let placeholder =
            RenderedImage::new_placeholder(record.metadata.clone(), Arc::from(record.data.clone()));
        app.rendered_images
            .entry(session_id)
            .or_default()
            .entry(turn_id)
            .or_default()
            .insert(img_idx, placeholder);
    }

    // Fast path — already encoded at full size.
    let should_submit = match app
        .rendered_images
        .get_mut(&session_id)
        .and_then(|imgs| imgs.get_mut(&turn_id))
        .and_then(|images| images.get_mut(&img_idx))
    {
        Some(img) => {
            if let Some(protocol) = img.protocols.get_mut(&full) {
                let target = protocol.size_for(crate::IMAGE_RESIZE, full);
                let centered = Rect {
                    x: area.x + (area.width.saturating_sub(target.width)) / 2,
                    y: area.y + (area.height.saturating_sub(target.height)) / 2,
                    width: target.width.min(area.width),
                    height: target.height.min(area.height),
                };
                frame.render_stateful_widget(
                    StatefulImage::new().resize(crate::IMAGE_RESIZE),
                    centered,
                    protocol,
                );
                return;
            }
            // Submit job if not pending/failed/cached.
            img.pending_job.is_none()
                && !img.failed_sizes.contains(&full)
                && !img.protocols.contains_key(&full)
        }
        None => false,
    };

    if should_submit
        && let Some(images) = app.rendered_images.get(&session_id)
        && let Some(imgs) = images.get(&turn_id)
        && let Some(img) = imgs.get(&img_idx)
    {
        app.submit_image_job(
            session_id,
            turn_id,
            img_idx,
            img.data.clone(),
            img.metadata.clone(),
            full,
            crate::IMAGE_RESIZE,
        );
    }

    render_fullscreen_placeholder(frame);
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App) {
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
    render_history(frame, history_chunks[0], app);

    // ── Scrollbar ────────────────────────────────────────────
    let viewport_height = app.history_viewport.height as usize;
    let total_height = app.total_history_height();
    if app.scrollbar_visible() {
        let position = app
            .max_scroll_offset()
            .saturating_sub(app.effective_scroll());
        let marker_slots: Vec<usize> = app
            .active_display_ref()
            .map(|d| d.markers.iter().map(|m| m.virtual_slot).collect())
            .unwrap_or_default();
        frame.render_stateful_widget(
            vertical_scrollbar().with_markers(&marker_slots),
            history_chunks[1],
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
    if let Some(ref err) = app.error {
        let err_para = Paragraph::new(Text::from(err.clone()))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false });
        frame.render_widget(err_para, notify_area);
    } else if let Some(ref status) = app.status {
        let status_para = Paragraph::new(Text::from(status.clone()))
            .style(Style::default().fg(Color::Green))
            .wrap(Wrap { trim: false });
        frame.render_widget(status_para, notify_area);
    }

    // ── Help overlay (2 lines, conditional) ───────────────────
    if app.show_ctrl_help {
        let help_inner = Rect {
            x: help_area.x + 1,
            width: help_area.width.saturating_sub(2),
            ..help_area
        };
        let help = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                CTRL_HELP_LINE1,
                Style::default().fg(Color::Cyan),
            )),
            Line::from(Span::styled(
                CTRL_HELP_LINE2,
                Style::default().fg(Color::Cyan),
            )),
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

    let input = Paragraph::new(Text::from(text_lines)).block(
        Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .padding(Padding::new(INPUT_PAD, INPUT_PAD, 0, 0)),
    );
    frame.render_widget(input, input_area);
    // Clamp to visible area so the cursor is always inside the box,
    // even when scroll_offset hasn't been adjusted yet (e.g. after
    // loading a long history entry that ends at scroll_offset = 0).
    let max_display_row = (visible_count as u16).saturating_sub(1);
    let display_vrow = vrow.saturating_sub(offset as u16).min(max_display_row);
    let cursor_x = input_area.x.saturating_add(INPUT_PAD).saturating_add(vcol);
    let cursor_y = input_area.y.saturating_add(1).saturating_add(display_vrow);
    frame.set_cursor_position((cursor_x, cursor_y));

    // ── Status bar (single line) ───────────────────────────────
    let has_session = app.attached_session_id.is_some();

    let status_line = if has_session {
        // Session-identity values (wd, provider, model, reasoning) — stable
        // across the session — go first (left side) so the bar's leading edge
        // stays fixed.  Runtime metrics (tokens, context fill) follow on the
        // right where their per-turn updates don't shift the identity fields.
        let wd = app
            .active_display_ref()
            .and_then(|d| d.working_dir.as_deref())
            .unwrap_or("-");
        let provider = app.attached_provider_slug.as_deref().unwrap_or("-");
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
        let context = match (
            app.active_display_ref().and_then(|d| d.context_window),
            app.active_display_ref().and_then(|d| d.last_prompt_tokens),
        ) {
            (Some(limit), Some(current)) => {
                let ratio = if limit > 0 {
                    current as f64 / limit as f64
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
        };

        // Tool groups can change at runtime via load_tools/unload_tools.
        let tool_groups = app.attached_tool_groups.join(", ");

        // Order: stable identity first (wd, provider, model, reasoning) so
        // the bar doesn't visually jitter when per-turn metrics appear or
        // disappear; tools in the middle; runtime metrics (tokens, context
        // window fill, active status) on the right.
        // ── Session identity (always present, default to "-") ──
        let mut spans: Vec<Span> = vec![
            Span::raw(" "),
            Span::styled(wd, Style::default().fg(Color::White)),
            Span::raw(" | "),
            Span::styled(provider, Style::default().fg(Color::White)),
            Span::raw(" | "),
            Span::styled(model, Style::default().fg(Color::White)),
            Span::raw(" | "),
            Span::styled(reasoning, Style::default().fg(Color::White)),
        ];

        // ── Tool groups (conditionally present) ──
        if !tool_groups.is_empty() {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                tool_groups,
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

        Line::from(spans)
    } else {
        Line::from("")
    };
    let status_bar = Paragraph::new(status_line).style(Style::default().bg(Color::Reset));
    frame.render_widget(status_bar, status_bar_area);
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let content_width = area.width.saturating_sub(9);
    let tool_content_width = area.width.saturating_sub(4);

    app.ensure_cache_synced();

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let mut rows_to_skip = app.effective_scroll();

    // Clone visible turn IDs upfront to avoid borrow conflicts when
    // accessing display state via app.display_for inside the loop.
    let session_id = match app.active_session_id {
        Some(sid) => sid,
        None => return,
    };
    let visible_turn_ids: Vec<u32> = app.display_for(session_id).visible_turn_ids.clone();
    let len = visible_turn_ids.len();

    // Iterate visible turns from newest to oldest.  clipped_area consumes
    // rows_to_skip from the bottom (newest end) so that turns fully below
    // the viewport are skipped before any content is rendered.
    for raw_i in 0..len {
        let i = len - 1 - raw_i;
        let turn_id = visible_turn_ids[i];

        if rows_remaining == 0 {
            break;
        }

        // Get cached lines (Arc clone is O(1)), the pre-computed height,
        // cumulative visual-row offsets for O(log n) row→line lookups, and
        // the per-line content column ranges (for selection clamping).
        let (text_lines_arc, text_height, text_offsets, content_ranges, img_count) = {
            let display = app.display_for(session_id);
            let Some(turn) = display.view.turns.get(&turn_id) else {
                continue;
            };
            if turn.undone {
                continue;
            }
            let count = turn.displayed_images.len();
            // Effective reasoning visibility: explicit override (header click)
            // wins, else the streaming-derived default.  The default is read
            // from the precomputed turn layout — rebuilt in lockstep with
            // `visible_turn_ids` before every render — keeping this per-frame
            // path free of string scanning; the trim-based derivation is only
            // a defensive fallback for a missing layout.
            let reasoning_expanded = {
                let default = display
                    .turn_layouts
                    .get(i)
                    .map(|l| l.reasoning_default_expanded)
                    .unwrap_or_else(|| reasoning_expanded_default(turn));
                display.effective_reasoning_expanded(turn_id, default)
            };
            // Effective per-result collapse state (aligned with
            // `turn.tool_results`), part of the render-cache key like
            // `reasoning_expanded`.  Built only for turns that actually
            // have tool results — the common case allocates nothing and the
            // key comparison short-circuits on the empty slice.
            let tool_results_collapsed: Vec<bool> = if turn.tool_results.is_empty() {
                Vec::new()
            } else {
                turn.tool_results
                    .iter()
                    .map(|r| display.effective_tool_result_collapsed(turn_id, r))
                    .collect()
            };
            let key = RenderCacheKey {
                turn_id,
                width: content_width,
                viewport_width: area.width,
                reasoning_expanded,
                tool_results_collapsed,
                content_version: display.turn_content_version(turn_id),
            };
            let rendered = cached_or_compute_lines(&mut display.render_cache, i, &key, || {
                render_turn_lines(
                    turn,
                    content_width,
                    tool_content_width,
                    key.reasoning_expanded,
                    &key.tool_results_collapsed,
                )
            });
            (
                rendered.lines,
                rendered.height,
                rendered.visual_offsets,
                rendered.content_ranges,
                count,
            )
        };

        // ── Images (rendered first so they sit below text) ──
        let full_img_height = app.image_block_height() as usize;
        for img_idx in (0..img_count).rev() {
            if let Some((_top_line, visible_height)) = clipped_area(
                full_img_height,
                &mut rows_to_skip,
                &mut rows_remaining,
                &mut y,
            ) {
                let fully_visible = visible_height >= full_img_height;
                let img_rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: visible_height as u16,
                };
                render_turn_image(
                    frame,
                    img_rect,
                    session_id,
                    turn_id,
                    img_idx,
                    app,
                    fully_visible,
                );
            }
        }

        // ── Text content (render above images) ──
        if let Some((top_line, visible_height)) =
            clipped_area(text_height, &mut rows_to_skip, &mut rows_remaining, &mut y)
        {
            // Clone only the visible slice from the Arc — O(visible_lines)
            // instead of O(total_lines_in_turn).
            // Binary-search the precomputed cumulative offsets to find which
            // semantic lines the visible visual rows span — O(log n).
            let row_start = top_line;
            let row_end = top_line + visible_height;
            let line_start = text_offsets.partition_point(|&o| o <= row_start);
            let line_end = text_offsets.partition_point(|&o| o <= row_end);
            let mut visible_lines = text_lines_arc[line_start..line_end].to_vec();
            // Apply the in-progress text-selection highlight to the visible
            // slice at draw time — the render cache stays pure, and the same
            // cached lines drive both the highlight and the copy, so what is
            // highlighted is exactly what gets copied.  `i` is the
            // visible-turn index; the turn's first content row is
            // `height_prefix[i-1]` (0 for the first turn).
            let turn_start = i
                .checked_sub(1)
                .and_then(|prev| {
                    app.active_display_ref()
                        .and_then(|d| d.height_prefix.get(prev))
                        .copied()
                })
                .unwrap_or(0);
            selection::apply_selection_to_lines(
                app,
                turn_start,
                &text_offsets[..],
                &content_ranges[..],
                line_start,
                &mut visible_lines,
            );
            render_text_block(
                frame,
                area,
                visible_lines,
                visible_height,
                &mut y,
                Style::default(),
            );
        }
    }
}

/// Return the cached rendered lines (as an `Arc` slice for O(1) sharing) and
/// their pre-computed height for a turn at the given cache index, or compute,
/// cache, and return them.
///
/// On cache hit the height is returned from the cache (avoids re-computing
/// `lines_height`, which iterates every line).  On cache miss the height is
/// computed inline and stored alongside the lines.
///
fn clipped_area(
    full_height: usize,
    rows_to_skip: &mut usize,
    rows_remaining: &mut usize,
    y: &mut u16,
) -> Option<(usize, usize)> {
    if *rows_to_skip >= full_height {
        *rows_to_skip -= full_height;
        return None;
    }

    let visible_height = (full_height.saturating_sub(*rows_to_skip)).min(*rows_remaining);
    if visible_height == 0 {
        return None;
    }

    let bottom_line = full_height.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;

    Some((top_line, visible_height))
}

/// Render a text block into the given area with wrapping.
///
/// `lines` must already be clipped to the visible portion (no `scroll` offset
/// is applied since the slice starts at the correct position).
fn render_text_block(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    visible_height: usize,
    y: &mut u16,
    paragraph_style: Style,
) {
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };

    frame.render_widget(
        Paragraph::new(lines).style(paragraph_style).scroll((0, 0)),
        rect,
    );
}

/// Render a single turn-displayed image block.
///
/// Height is always `image_block_height()` regardless of encoding state,
/// so scroll positions remain stable.
///
/// When `fully_visible` is true the image is centered within its block
/// using the protocol's actual rendered dimensions (via `size_for`). When
/// only a slice of the block is visible it is rendered without centering
/// to prevent visual reflow during scrolling.
fn render_turn_image(
    frame: &mut Frame<'_>,
    area: Rect,
    session_id: u64,
    turn_id: u32,
    img_idx: usize,
    app: &mut App,
    fully_visible: bool,
) {
    let inline_size = Size::new(area.width, app.image_block_height());

    // Extract data we need while the borrow is active.
    let (needs_job, data, meta) = match app
        .rendered_images
        .get_mut(&session_id)
        .and_then(|imgs| imgs.get_mut(&turn_id))
        .and_then(|images| images.get_mut(&img_idx))
    {
        Some(img) => {
            if let Some(protocol) = img.protocols.get_mut(&inline_size) {
                let title = format!(
                    "image {} ({} {}x{})",
                    turn_id, img.metadata.mime_type, img.metadata.width, img.metadata.height,
                );
                let block = Block::default().title(title);
                let inner = block.inner(area);
                frame.render_widget(block, area);
                if fully_visible {
                    // Center the image within the block using the protocol's actual
                    // rendered dimensions, preventing visual reflow when only part
                    // of the block is visible.
                    let rendered_at = protocol.size_for(crate::IMAGE_RESIZE, inline_size);
                    let centered = Rect {
                        x: inner.x + (inner.width.saturating_sub(rendered_at.width)) / 2,
                        y: inner.y + (inner.height.saturating_sub(rendered_at.height)) / 2,
                        width: rendered_at.width.min(inner.width),
                        height: rendered_at.height.min(inner.height),
                    };
                    frame.render_stateful_widget(
                        StatefulImage::new().resize(crate::IMAGE_RESIZE),
                        centered,
                        protocol,
                    );
                }
                return;
            }
            (
                img.pending_job.is_none()
                    && !img.failed_sizes.contains(&inline_size)
                    && !img.protocols.contains_key(&inline_size),
                img.data.clone(),
                img.metadata.clone(),
            )
        }
        None => {
            let block = Block::default().title(format!("image {turn_id}[{img_idx}] (pending)"));
            frame.render_widget(block, area);
            return;
        }
    };

    if needs_job {
        app.submit_image_job(
            session_id,
            turn_id,
            img_idx,
            data,
            meta.clone(),
            inline_size,
            crate::IMAGE_RESIZE,
        );
    }

    // Render placeholder frame while encoding is pending.
    // Use metadata from the RenderedImage entry (already populated by
    // sync_turn_images), not from the session turns.
    let placeholder_title = format!(
        "image {} ({} {}x{})",
        turn_id, meta.mime_type, meta.width, meta.height,
    );
    let block = Block::default().title(placeholder_title);
    frame.render_widget(block, area);
}

// ── Session Manager ──────────────────────────────────────────

fn render_session_manager(frame: &mut Frame<'_>, app: &mut App) {
    match app.session_mgr.view {
        SessionManagerView::List => render_session_list_view(frame, app),
        SessionManagerView::Detail => render_session_detail_view(frame, app),
    }
}

fn render_session_list_view(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" Session Manager ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let list_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let max_rows = list_chunks[0].height as usize;
    let total_items = app.session_mgr.sessions.len();
    // The table header occupies one of `max_rows` rows, so only `list_rows`
    // session rows fit below it.  The window and scrollbar math must use
    // this content height — otherwise the highlighted row can sit one row
    // below the last drawn session row.  `window()` is pure (no state
    // mutation during `draw()`), and the returned start doubles as the
    // scrollbar position.
    let list_rows = max_rows.saturating_sub(1);
    let (scroll, _count) = app.session_mgr.window(list_rows);

    if let Some(ref err) = app.session_mgr.error {
        let err_style = Style::default().fg(Color::Red);
        let err_text = format!("Error: {err}");
        let err_para = Paragraph::new(Line::from(Span::styled(err_text, err_style)));
        let err_area = Rect {
            x: list_chunks[0].x + 1,
            y: list_chunks[0].y + 1,
            width: list_chunks[0].width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(err_para, err_area);
    }

    if total_items == 0 {
        let msg = Paragraph::new("No sessions. Press 'n' to create one.");
        frame.render_widget(msg, list_chunks[0]);
    } else {
        // ── Column layout ────────────────────────────────────────────────
        // The title column is LAST so it absorbs the remaining width via
        // Constraint::Fill(1) — long titles truncate (with an ellipsis)
        // instead of squeezing the fixed columns.  Session and parent ids are
        // fixed-width numeric columns; long ids truncate with an ellipsis.
        let session_w = 8u16;
        let parent_w = 8u16;
        let marker_w = 2u16; // ">" selection + "*" attached markers
        let status_w = 14u16;
        let model_w = 16u16;
        let turns_w = 5u16;
        let modified_w = 11u16;
        let fixed_w = session_w + parent_w + marker_w + status_w + model_w + turns_w + modified_w;
        // The Table adds `column_spacing(1)` between the 8 columns (7 gaps),
        // so the title column gets the remaining width minus those gaps.
        let title_w = list_chunks[0].width.saturating_sub(fixed_w + 7).max(1) as usize;

        let header = Row::new(vec![
            Cell::from(""),
            Cell::from("Session"),
            Cell::from("Parent"),
            Cell::from("Status"),
            Cell::from("Model"),
            Cell::from("Turns"),
            Cell::from("Modified"),
            Cell::from("Title"),
        ])
        .style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

        // Only render the visible slice; the scrollbar below reflects the
        // full list length.
        let end = (scroll + list_rows).min(total_items);
        let mut rows = Vec::with_capacity(end.saturating_sub(scroll));
        for i in scroll..end {
            let session = &app.session_mgr.sessions[i];
            let is_selected = Some(i) == app.session_mgr.selection;
            let is_attached = Some(session.session_id) == app.attached_session_id;
            let row_style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };

            let sel = if is_selected { ">" } else { " " };
            let att = if is_attached { "*" } else { " " };
            // Child sessions show their parent's id; top-level sessions get a
            // dash so the column stays readable at a glance.
            let parent = session
                .parent_session_id
                .map_or_else(|| "-".to_string(), |id| id.to_string());
            let (status_text, status_color) = status_display(&session.status);
            let model = session.selected_model.as_deref().unwrap_or("-");
            let model_display =
                if let Some(effort) = session.reasoning_effort.as_deref().filter(|e| *e != "off") {
                    format!("{model} ({effort})")
                } else {
                    model.to_string()
                };
            let modified = format_timestamp(session.last_modified);
            let title = session.title.as_deref().unwrap_or("untitled");

            // Apply the selection style to the whole Row (not to each cell's
            // span) so the highlight background is solid across the entire
            // content width: ratatui paints `Row::style` over the full row
            // area, whereas span-level styles only cover the characters they
            // render.
            let status_cell_style = row_style.fg(status_color);
            rows.push(
                Row::new(vec![
                    Cell::from(format!("{sel}{att}")),
                    Cell::from(truncate_str(
                        &session.session_id.to_string(),
                        session_w as usize,
                    )),
                    Cell::from(truncate_str(&parent, parent_w as usize)),
                    // Keep the status colour even on the highlighted row so
                    // active/inferring sessions stay recognisable at a glance.
                    Cell::from(truncate_str(&status_text, status_w as usize))
                        .style(status_cell_style),
                    Cell::from(truncate_str(&model_display, model_w as usize)),
                    Cell::from(format!(
                        "{:>width$}",
                        session.turn_count,
                        width = turns_w as usize
                    )),
                    Cell::from(truncate_str(&modified, modified_w as usize)),
                    Cell::from(truncate_str(title, title_w)),
                ])
                .style(row_style),
            );
        }

        let table = Table::new(
            rows,
            [
                Constraint::Length(marker_w),
                Constraint::Length(session_w),
                Constraint::Length(parent_w),
                Constraint::Length(status_w),
                Constraint::Length(model_w),
                Constraint::Length(turns_w),
                Constraint::Length(modified_w),
                Constraint::Fill(1),
            ],
        )
        .header(header)
        .column_spacing(1);
        frame.render_widget(table, list_chunks[0]);
    }

    if total_items > list_rows {
        frame.render_stateful_widget(
            vertical_scrollbar(),
            list_chunks[1],
            &mut SmoothScrollbarState::new(total_items)
                .position(scroll)
                .viewport_content_length(list_rows),
        );
    }

    let status = if let Some((_id, title)) = &app.session_mgr.confirm_delete {
        Paragraph::new(Line::from(format!(" Delete \"{title}\"? (y/N)  ")))
    } else {
        Paragraph::new(Line::from(format!(
            " <j/k nav>  <Enter switch>  <i details>  <n new>  <d delete>  <Esc back>  —  {} sessions",
            total_items
        )))
    };
    frame.render_widget(status, chunks[1]);
}

fn render_session_detail_view(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" Session Details ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(ref detail) = app.session_mgr.detail_data {
        let lines = vec![
            Line::from(format!("Session ID:    {}", detail.session_id)),
            Line::from(format!("Title:         {}", detail.title)),
            Line::from(format!("Model:         {}", detail.selected_model)),
            Line::from(format!(
                "Reasoning:     {}",
                detail
                    .reasoning_effort
                    .as_deref()
                    .filter(|e| *e != "off")
                    .unwrap_or("off")
            )),
            Line::from(format!(
                "Parent:        {}",
                detail
                    .parent_session_id
                    .map_or("none".to_string(), |id| id.to_string())
            )),
            Line::from(format!("Working Dir:   {}", detail.working_dir)),
            Line::from(format!(
                "Created:       {}",
                format_timestamp(detail.created_at)
            )),
            Line::from(format!(
                "Last Modified: {}",
                format_timestamp(detail.last_modified)
            )),
            Line::from(format!("Turn Count:    {}", detail.turn_count)),
            Line::from(format!(
                "Account:       {}",
                detail.account_name.as_deref().unwrap_or("-")
            )),
            Line::from(match &detail.accumulated_usage {
                Some(usage) => session_detail_tokens_line(usage),
                None => "Tokens:        -".to_string(),
            }),
            Line::from(match (detail.context_window, detail.last_prompt_tokens) {
                (Some(limit), Some(current)) => {
                    let ratio = if limit > 0 {
                        current as f64 / limit as f64
                    } else {
                        0.0
                    };
                    format!(
                        "Context:       {} / {} ({})",
                        humfmt::number(current),
                        humfmt::number(limit),
                        humfmt::percent(ratio),
                    )
                }
                (Some(limit), None) => {
                    format!("Context:       ? / {}", humfmt::number(limit))
                }
                (None, Some(current)) => format!("Context:       {} / ?", humfmt::number(current)),
                (None, None) => "Context:       unknown".to_string(),
            }),
            Line::from(format!("Status:        {}", format_status(&detail.status))),
            Line::from(format!(
                "Tool Groups:   {}",
                humfmt::list(&detail.active_tool_groups)
            )),
        ];
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    let status = Paragraph::new(Line::from(" <b back>  <Enter switch to this session>"));
    frame.render_widget(status, chunks[1]);
}

/// Format a Unix-epoch-milliseconds timestamp as simplified absolute time.
///
/// - today → "14:32" (time only)
/// - this calendar year → "Mar 5"
/// - older → "Mar 5 2024"
pub(crate) fn format_timestamp(ts_ms: i64) -> String {
    if ts_ms <= 0 {
        return "-".to_string();
    }

    use chrono::{Datelike, Local, TimeZone};

    let dt = match Local.timestamp_millis_opt(ts_ms) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return "-".to_string(),
    };

    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else if dt.year() == now.year() {
        dt.format("%b %d").to_string()
    } else {
        dt.format("%b %d %Y").to_string()
    }
}

// ── AI Provider Accounts ──────────────────────────────────

fn render_ai_providers(frame: &mut Frame<'_>, app: &mut App) {
    if app.ai_providers.credential_target.is_some() {
        render_ai_providers_credential(frame, app);
        return;
    }
    match app.ai_providers.view {
        AIProvidersView::List => render_ai_providers_list(frame, app),
        AIProvidersView::SelectProvider => render_ai_providers_select_provider(frame, app),
        AIProvidersView::SetSlug => render_ai_providers_set_slug(frame, app),
    }
}

fn render_ai_providers_list(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" AI Provider Accounts ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let list_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let scroll = app.ai_providers.scroll;
    let max_rows = list_chunks[0].height as usize;
    let total_items = app.ai_providers.accounts.len();

    if total_items == 0 {
        let msg = Paragraph::new("No AI provider accounts configured. Press 'n' to add one.");
        frame.render_widget(msg, list_chunks[0]);
    } else {
        let mut lines: Vec<Line> = Vec::new();

        for i in scroll..total_items {
            if lines.len() + 3 > max_rows && i != scroll {
                break;
            }
            let account = &app.ai_providers.accounts[i];
            let is_selected = Some(i) == app.ai_providers.selection;

            let sel = if is_selected { ">" } else { " " };

            let style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };

            let name_line = format!("{sel} {} ", account.name);
            let name_spans = vec![ratatui::text::Span::styled(name_line, style)];
            lines.push(Line::from(name_spans));

            let provider_label = format!("   Provider: {}", account.provider);
            let provider_style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![ratatui::text::Span::styled(
                provider_label,
                provider_style,
            )]));

            let cred_label = if account.has_credential {
                "   Credential: yes".to_string()
            } else {
                "   Credential: no".to_string()
            };
            let cred_style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else if account.has_credential {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![ratatui::text::Span::styled(
                cred_label, cred_style,
            )]));

            if lines.len() < max_rows && i + 1 < total_items {
                lines.push(Line::from(Span::styled(String::new(), Style::default())));
            }
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, list_chunks[0]);
    }

    let items_per_page = (max_rows / AI_PROVIDER_ITEM_LINES).max(1);
    if total_items > items_per_page {
        frame.render_stateful_widget(
            vertical_scrollbar(),
            list_chunks[1],
            &mut SmoothScrollbarState::new(total_items)
                .position(scroll)
                .viewport_content_length(items_per_page),
        );
    }

    let status = if let Some(ref name) = app.ai_providers.confirm_remove {
        Paragraph::new(Line::from(format!(" Remove \"{name}\"? (y/N)  ")))
    } else {
        Paragraph::new(Line::from(format!(
            " <j/k nav>  <r remove>  <c credential>  <n new>  <Esc back>  —  {} accounts",
            total_items
        )))
    };
    frame.render_widget(status, chunks[1]);
}

fn render_ai_providers_credential(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let account_name = app
        .ai_providers
        .credential_target
        .as_deref()
        .unwrap_or("(unknown)");

    let block = Block::default()
        .title(format!(" API Key for \"{account_name}\" "))
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let mut lines: Vec<Line> = Vec::new();

    let dim = Style::default().fg(Color::DarkGray);
    let input_style = Style::default().fg(Color::Cyan);

    lines.push(Line::from(Span::styled(String::new(), Style::default())));
    lines.push(Line::from(Span::styled(
        "  Paste the API key for this account:",
        dim,
    )));
    lines.push(Line::from(Span::styled(String::new(), Style::default())));

    let text = &app.ai_providers.credential_input.text;
    let cursor = app.ai_providers.credential_input.cursor;
    let masked = if text.is_empty() {
        String::new()
    } else if text.len() > 12 {
        format!("{}...{}", &text[..4], &text[text.len().saturating_sub(4)..])
    } else {
        "••••••••".to_string()
    };
    let display = if text.is_empty() {
        "> ".to_string()
    } else {
        format!("> {masked}")
    };
    lines.push(Line::from(Span::styled(display, input_style)));
    let input_line = lines.len() as u16 - 1;
    lines.push(Line::from(Span::styled(String::new(), Style::default())));

    if let Some(ref err) = app.ai_providers.add_error {
        lines.push(Line::from(Span::styled(
            format!("  Error: {err}"),
            Style::default().fg(Color::Red),
        )));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);

    let masked_before = if text.is_empty() {
        ""
    } else if cursor >= text.len() {
        &masked
    } else {
        let pos = (cursor as f64 / text.len() as f64 * masked.len() as f64).round() as usize;
        let pos = pos.min(masked.len());
        &masked[..pos]
    };
    set_input_cursor(frame, inner, input_line, 2, masked_before);

    let status = Paragraph::new(Line::from(" <Enter save>  <Esc cancel>"));
    frame.render_widget(status, chunks[1]);
}

/// Phase 1 of the new-account wizard: browse `PROVIDER_OPTIONS` and pick
/// one.  Rendered as a compact scrollable one-line-per-provider list of
/// display names (the canonical slug is not shown), matching the accounts
/// list's look.
fn render_ai_providers_select_provider(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" Select AI Provider (1/2) ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let list_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let max_rows = list_chunks[0].height as usize;
    let total = app.providers.len();
    let mut lines: Vec<Line> = Vec::new();

    // One line per provider, so the visible window holds max_rows entries.
    // `provider_window` keeps the highlighted row on screen regardless of
    // how far the user scrolled.
    let items_per_page = max_rows.max(1);
    let (win_start, win_count) = app
        .ai_providers
        .provider_window(&app.providers, items_per_page);
    let win_end = (win_start + win_count).min(total);

    for (i, provider) in app
        .providers
        .iter()
        .enumerate()
        .take(win_end)
        .skip(win_start)
    {
        let is_selected = i == app.ai_providers.provider_selection;
        let sel = if is_selected { ">" } else { " " };

        let style = if is_selected {
            Style::default().bg(Color::Blue).fg(Color::White)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![Span::styled(
            format!("{sel} {} ", provider.display_name),
            style,
        )]));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, list_chunks[0]);

    if total > items_per_page {
        frame.render_stateful_widget(
            vertical_scrollbar(),
            list_chunks[1],
            &mut SmoothScrollbarState::new(total)
                .position(win_start)
                .viewport_content_length(items_per_page),
        );
    }

    let status = Paragraph::new(Line::from(format!(
        " <j/k nav>  <PgUp/PgDn page>  <Enter select>  <Esc back>  —  {} providers",
        total
    )));
    frame.render_widget(status, chunks[1]);
}

/// Phase 2 of the new-account wizard: enter a slug (the account name).
/// Explains what a slug is, shows the provider picked in phase 1, and
/// submits `AddAccount` on Enter (handled in connection.rs), after which
/// the flow redirects to the credential page.
fn render_ai_providers_set_slug(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" Set Account Slug (2/2) ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let dim = Style::default().fg(Color::DarkGray);
    let accent = Style::default().fg(Color::Cyan);

    // Provider picked in phase 1, shown for context.
    let provider_name = app
        .ai_providers
        .selected_provider_slug(&app.providers)
        .and_then(|slug| app.providers.iter().find(|p| p.slug == slug))
        .map(|p| p.display_name.as_str())
        .unwrap_or("(none)");
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(String::new(), Style::default()))];
    lines.push(Line::from(Span::styled(
        format!("  Provider: {provider_name}"),
        accent,
    )));
    lines.push(Line::from(Span::styled(String::new(), Style::default())));
    lines.push(Line::from(Span::styled(
        "  A slug is the unique name this account is stored under.",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "  You'll use it to refer to the account, e.g. /account <slug>.",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "  Lowercase letters, numbers, hyphens, and underscores only.",
        dim,
    )));
    frame.render_widget(Paragraph::new(lines), rows[0]);

    let border_style = Style::default().fg(Color::Cyan);
    let slug_prompt = TextPrompt::new(std::borrow::Cow::Borrowed("Slug:"))
        .with_block(Block::bordered().border_style(border_style));
    (&slug_prompt).draw(frame, rows[1], &mut app.ai_providers.slug_state);

    if let Some(ref err) = app.ai_providers.add_error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  Error: {err}"),
                Style::default().fg(Color::Red),
            ))),
            rows[2],
        );
    }

    let status = Paragraph::new(Line::from(" <Enter create account>  <Esc back>"));
    frame.render_widget(status, chunks[1]);
}

fn set_input_cursor(
    frame: &mut Frame<'_>,
    area: Rect,
    line: u16,
    prefix_width: u16,
    text_before_cursor: &str,
) {
    let x = area.x + prefix_width + display_width(text_before_cursor) as u16;
    let y = area.y + line;
    frame.set_cursor_position((x, y));
}

pub(crate) fn status_display(status: &SessionStatus) -> (String, Color) {
    match status {
        SessionStatus::Sleeping => ("sleeping".into(), Color::DarkGray),
        SessionStatus::Inactive => ("idle".into(), Color::Green),
        SessionStatus::Inference => ("inferring".into(), Color::Yellow),
        SessionStatus::ToolCall(name) => (format!("tool call: {name}"), Color::Cyan),
        SessionStatus::Retrying {
            attempt,
            max_attempts,
            delay_ms,
        } => (
            format!(
                "retrying ({attempt}/{max_attempts}, {})",
                humfmt::duration(std::time::Duration::from_millis(*delay_ms)),
            ),
            Color::Magenta,
        ),
        _ => ("unknown".into(), Color::White),
    }
}

pub(crate) fn format_status(status: &SessionStatus) -> String {
    status_display(status).0
}

/// The status bar's cumulative token readout, e.g. `↑15.3K ↓1.2K`.
///
/// Both counters pass through humfmt's compact number formatter so the
/// readout stays consistent with the context-window fill rendered beside it
/// (which already uses `humfmt::number`/`humfmt::percent`): small sessions
/// (< 1_000 tokens) render verbatim, large ones get K/M suffixes.
pub(crate) fn status_token_readout(usage: &TokenUsage) -> String {
    format!(
        "↑{} ↓{}",
        humfmt::number(usage.input_tokens),
        humfmt::number(usage.output_tokens),
    )
}

/// The session-detail "Tokens:" line, e.g.
/// `Tokens:        15.3K in / 1.2K out (16.5K total)`.
///
/// Same humfmt treatment as the status bar's readout so the two token
/// surfaces agree; the `Tokens:        ` label keeps the column aligned with
/// its neighbours (`Working Dir:`, `Turn Count:`, …).
pub(crate) fn session_detail_tokens_line(usage: &TokenUsage) -> String {
    format!(
        "Tokens:        {} in / {} out ({} total)",
        humfmt::number(usage.input_tokens),
        humfmt::number(usage.output_tokens),
        humfmt::number(usage.total_tokens),
    )
}

/// Centered popup listing the models available on the attached session's
/// account.  Drawn last so it covers the Chat page content.  The filter box
/// reuses `InputBuffer` editing semantics; key handling lives in
/// `handle_model_selector_event` (connection.rs).
fn render_model_selector(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // Target ~60% of the terminal width and ~2/3 of the height, floored at a
    // sane minimum and capped so the popup never touches the screen edges.
    // The `.min(area…)` guards keep the arithmetic panic-free on tiny
    // terminals (clamp panics if its bounds are inverted).
    let width = ((area.width as u32 * 3 / 5) as u16)
        .clamp(24, 100)
        .min(area.width.saturating_sub(4))
        .max(1);
    let height = ((area.height as u32 * 2 / 3) as u16)
        .clamp(8, 40)
        .min(area.height.saturating_sub(2))
        .max(1);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    // Erase the region beneath so the popup reads as a solid overlay rather
    // than text drawn on top of the chat history.
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Select Model ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // ── Filter row ──────────────────────────────────────────────
    let filter_row = chunks[0];
    let filter_prefix = "> ";
    let filter_display = format!("{filter_prefix}{}", app.model_selector.filter.text);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            filter_display,
            Style::default().fg(Color::White),
        ))),
        filter_row,
    );
    // Park the terminal cursor right after the filter text so typing feels
    // like the main input box.  `cursor` is a byte offset (InputBuffer's
    // convention); clamp the column so a long filter never pushes the
    // cursor off-screen.
    let before_cursor = app
        .model_selector
        .filter
        .text
        .get(..app.model_selector.filter.cursor)
        .unwrap_or(&app.model_selector.filter.text);
    let cursor_col =
        filter_row.x + filter_prefix.len() as u16 + display_width(before_cursor) as u16;
    let cursor_col = cursor_col.min(filter_row.x + filter_row.width.saturating_sub(1));
    frame.set_cursor_position((cursor_col, filter_row.y));

    // ── Body: error / loading / empty / list ───────────────────
    let body = chunks[1];
    if let Some(ref err) = app.model_selector.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" Error: {err}"),
                Style::default().fg(Color::Red),
            ))),
            body,
        );
        return;
    }
    if app.model_selector.loading {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Loading models…",
                Style::default().fg(Color::DarkGray),
            ))),
            body,
        );
        return;
    }

    // Compute the visible window (pure — `window` never mutates state, so
    // drawing the popup cannot disturb scroll/focus state mid-frame) and
    // build the visible rows from the filtered view.
    let list_height = body.height as usize;
    let (scroll, count) = app.model_selector.window(list_height);
    let filtered = app.model_selector.filtered();
    let selected = app.model_selector.selected.clone();
    let focused = app.model_selector.focused;

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " No models match the filter.",
                Style::default().fg(Color::DarkGray),
            ))),
            body,
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(count);
    for (i, model) in filtered.iter().enumerate().skip(scroll).take(count) {
        let is_current = selected.as_deref() == Some(model);
        let is_focused = i == focused;
        // `●` marks the active model (mirrors opencode's modal); `>` marks
        // the highlight the user is about to select.
        let prefix = if is_focused { "> " } else { "  " };
        let marker = if is_current { "●" } else { " " };
        let style = if is_focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{marker} {model}"),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), body);

    // ── Footer hint ─────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " esc close · enter select",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_render::{RenderedTurnLines, compute_visual_offsets, lines_height};
    use crate::state::{RenderCacheKey, RenderedCache, RenderedTurn};

    /// Wrap a line vector as a rendered turn with no headers.
    fn rendered(lines: Vec<Line<'static>>) -> RenderedTurnLines {
        // Every line gets a full-width content range so the cache-alignment
        // invariant asserted in `cached_or_compute_lines` holds for test
        // fixtures too (each entry must align with `lines`).
        let content_ranges = lines.iter().map(|l| Some((0, l.width()))).collect();
        RenderedTurnLines {
            lines,
            content_ranges,
            reasoning_header_idx: None,
            tool_result_header_idxs: Vec::new(),
        }
    }

    /// Build a cache entry with the given key/output pieces, defaulting the
    /// rest, so tests can focus on the field under test.
    fn cache_entry(
        key: RenderCacheKey,
        lines: Arc<[Line<'static>]>,
        height: usize,
        visual_offsets: Arc<[usize]>,
    ) -> RenderedCache {
        RenderedCache {
            key,
            rendered: RenderedTurn {
                lines,
                height,
                visual_offsets,
                content_ranges: Arc::from([]),
                reasoning_header_idx: None,
                tool_result_header_idxs: Vec::new(),
            },
        }
    }

    /// A key for a one-line entry rendered at content width 80 in a
    /// viewport of width 100, reasoning collapsed, no tool results, at
    /// content version 0 (no mutations recorded).
    fn base_key() -> RenderCacheKey {
        RenderCacheKey {
            turn_id: 0,
            width: 80,
            viewport_width: 100,
            reasoning_expanded: false,
            tool_results_collapsed: Vec::new(),
            content_version: 0,
        }
    }

    // ── mouse_in_history_box ──

    #[test]
    fn mouse_in_history_box_inside() {
        assert!(mouse_in_history_box(5, 10, 80, 24));
    }

    #[test]
    fn mouse_in_history_box_column_too_large() {
        assert!(!mouse_in_history_box(80, 10, 80, 24));
    }

    #[test]
    fn mouse_in_history_box_row_too_large() {
        assert!(!mouse_in_history_box(5, 24, 80, 24));
    }

    #[test]
    fn mouse_in_history_box_both_out_of_bounds() {
        assert!(!mouse_in_history_box(99, 99, 80, 24));
    }

    #[test]
    fn mouse_in_history_box_zero_height_viewport() {
        assert!(!mouse_in_history_box(0, 0, 80, 0));
    }

    #[test]
    fn mouse_in_history_box_zero_width_viewport() {
        assert!(!mouse_in_history_box(0, 0, 0, 24));
    }

    // ── mouse_in_scrollbar_column ──

    #[test]
    fn mouse_in_scrollbar_column_on_scrollbar() {
        assert!(mouse_in_scrollbar_column(80, 10, 80, 24));
    }

    #[test]
    fn mouse_in_scrollbar_column_before_scrollbar() {
        assert!(!mouse_in_scrollbar_column(79, 10, 80, 24));
    }

    #[test]
    fn mouse_in_scrollbar_column_after_scrollbar() {
        assert!(!mouse_in_scrollbar_column(81, 10, 80, 24));
    }

    #[test]
    fn mouse_in_scrollbar_column_row_too_large() {
        assert!(!mouse_in_scrollbar_column(80, 24, 80, 24));
    }

    #[test]
    fn mouse_in_scrollbar_column_zero_height() {
        assert!(!mouse_in_scrollbar_column(80, 0, 80, 0));
    }

    // ── clipped_area ──

    #[test]
    fn clipped_area_skip_when_rows_to_skip_equals_full_height() {
        let mut skip = 10usize;
        let mut remain = 20usize;
        let mut y = 30u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        assert!(
            result.is_none(),
            "should skip when rows_to_skip >= full_height"
        );
        assert_eq!(skip, 0, "rows_to_skip should be decremented by full_height");
        assert_eq!(remain, 20, "rows_remaining should be unchanged");
        assert_eq!(y, 30, "y should be unchanged");
    }

    #[test]
    fn clipped_area_skip_when_rows_to_skip_exceeds_full_height() {
        let mut skip = 15usize;
        let mut remain = 20usize;
        let mut y = 30u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        assert!(
            result.is_none(),
            "should skip when rows_to_skip > full_height"
        );
        assert_eq!(skip, 5, "rows_to_skip should be decremented by full_height");
    }

    #[test]
    fn clipped_area_partial_visibility_at_boundary() {
        let mut skip = 7usize;
        let mut remain = 20usize;
        let mut y = 30u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        let (top_line, visible_height) = result.expect("should be visible");
        // bottom_line = 10 - 7 = 3, top_line = 3 - 3 = 0, visible = 3
        assert_eq!(
            top_line, 0,
            "top_line should be at start of non-skipped region"
        );
        assert_eq!(visible_height, 3, "should show remaining 3 lines");
        assert_eq!(skip, 0, "rows_to_skip should be reset to 0");
        assert_eq!(
            remain, 17,
            "rows_remaining should decrease by visible_height"
        );
        assert_eq!(y, 27, "y should decrease by visible_height");
    }

    #[test]
    fn clipped_area_partial_visibility_skip_some_rows_within_turn() {
        let mut skip = 3usize;
        let mut remain = 10usize;
        let mut y = 50u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        let (top_line, visible_height) = result.expect("should be visible");
        // bottom_line = 10 - 3 = 7, top_line = 7 - 7 = 0, visible = 7...
        // Wait: visible_height = min(10-3, 10) = 7, bottom_line = 10-3 = 7, top_line = 7-7 = 0
        assert_eq!(top_line, 0);
        assert_eq!(visible_height, 7);
        assert_eq!(skip, 0);
        assert_eq!(remain, 3);
        assert_eq!(y, 43);
    }

    #[test]
    fn clipped_area_full_turn_within_viewport() {
        let mut skip = 0usize;
        let mut remain = 20usize;
        let mut y = 40u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        let (top_line, visible_height) = result.expect("should show all");
        assert_eq!(top_line, 0, "top_line should be 0 when nothing is skipped");
        assert_eq!(visible_height, 10, "should show full height");
        assert_eq!(y, 30, "y should decrease by full height");
    }

    #[test]
    fn clipped_area_clamps_to_rows_remaining() {
        let mut skip = 0usize;
        let mut remain = 3usize;
        let mut y = 20u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        let (top_line, visible_height) = result.expect("should clip to remaining");
        // visible_height = min(10, 3) = 3
        // bottom_line = 10 - 0 = 10, top_line = 10 - 3 = 7
        assert_eq!(
            top_line, 7,
            "top_line should be offset from bottom by visible_height"
        );
        assert_eq!(visible_height, 3, "should be clamped by rows_remaining");
        assert_eq!(remain, 0);
        assert_eq!(y, 17);
    }

    #[test]
    fn clipped_area_zero_rows_remaining_returns_none() {
        let mut skip = 0usize;
        let mut remain = 0usize;
        let mut y = 10u16;
        let result = clipped_area(10, &mut skip, &mut remain, &mut y);
        assert!(result.is_none(), "should return None when no rows remain");
        assert_eq!(y, 10, "y should be unchanged");
    }

    #[test]
    fn clipped_area_skip_exactly_full_height_then_show_next() {
        let mut skip = 6usize;
        let mut remain = 10usize;
        let mut y = 30u16;
        // First turn: full height = 6, rows_to_skip = 6 → skip entirely
        let result1 = clipped_area(6, &mut skip, &mut remain, &mut y);
        assert!(result1.is_none());
        assert_eq!(skip, 0);
        // Second turn: full height = 4, rows_to_skip = 0 → show fully
        let result2 = clipped_area(4, &mut skip, &mut remain, &mut y);
        let (_, visible) = result2.unwrap();
        assert_eq!(visible, 4);
        assert_eq!(y, 26);
    }

    // ── cached_or_compute_lines ──

    /// Helper: a simple compute function that returns a single short line.
    fn compute_one_line() -> RenderedTurnLines {
        rendered(vec![Line::from("hello")])
    }

    #[test]
    fn cached_or_compute_lines_cache_miss_stores_result() {
        let mut cache = vec![None];
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), compute_one_line);
        assert_eq!(rendered.lines.len(), 1, "should return computed lines");
        assert_eq!(
            rendered.height, 1,
            "single line at any viewport width has height 1"
        );
        assert_eq!(
            &*rendered.visual_offsets,
            &[1],
            "single short line should occupy one visual row"
        );
        assert_eq!(
            rendered.reasoning_header_idx, None,
            "no reasoning → no header index"
        );
        // Cache should be filled
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.key.width, 80);
        assert_eq!(cached.key.viewport_width, 100);
        assert_eq!(cached.rendered.height, 1);
        assert_eq!(cached.rendered.lines.len(), 1);
        assert_eq!(&*cached.rendered.visual_offsets, &[1]);
        assert!(
            !cached.key.reasoning_expanded,
            "cache should record the reasoning state it was rendered with"
        );
    }

    #[test]
    fn cached_or_compute_lines_cache_hit_returns_stored_height() {
        let mut cache = vec![Some(cache_entry(
            base_key(),
            Arc::from(vec![Line::from("cached")]),
            42,
            Arc::from([99]),
        ))];
        // Cache hit — should return stored height without recomputing
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            panic!("should not be called on cache hit")
        });
        assert_eq!(rendered.height, 42, "should return cached height");
        assert_eq!(rendered.lines.len(), 1, "should return cached lines");
        assert_eq!(rendered.lines[0], Line::from("cached"));
        assert_eq!(
            &*rendered.visual_offsets,
            &[99],
            "should return cached offsets"
        );
        assert_eq!(
            rendered.reasoning_header_idx, None,
            "should return the cached header index"
        );
    }

    #[test]
    fn cached_or_compute_lines_cache_hit_arc_shares_allocation() {
        let stored = Arc::from(vec![Line::from("shared")]);
        let stored_ptr = Arc::as_ptr(&stored);
        let mut cache = vec![Some(cache_entry(base_key(), stored, 7, Arc::from([1])))];
        let returned = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            panic!("should not recompute")
        });
        assert_eq!(
            Arc::as_ptr(&returned.lines),
            stored_ptr,
            "returned Arc should point to the same allocation as cache entry"
        );
    }

    #[test]
    fn cached_or_compute_lines_width_mismatch_recomputes() {
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                width: 40, // different from requested width 80
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            compute_called.set(true);
            rendered(vec![Line::from("fresh")])
        });
        assert!(compute_called.get(), "should recompute on width mismatch");
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        // Height of a single "fresh" line at viewport width 100 is 1
        assert_eq!(rendered.height, 1);
        assert_eq!(
            &*rendered.visual_offsets,
            &[1],
            "offsets recomputed for fresh lines"
        );
        // Cache should be updated
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.key.width, 80);
        assert_eq!(cached.key.viewport_width, 100);
        assert_eq!(cached.rendered.lines[0], Line::from("fresh"));
        assert_eq!(&*cached.rendered.visual_offsets, &[1]);
    }

    #[test]
    fn cached_or_compute_lines_viewport_width_mismatch_recomputes() {
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                viewport_width: 40, // different from requested viewport_width 100
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            compute_called.set(true);
            rendered(vec![Line::from("fresh")])
        });
        assert!(
            compute_called.get(),
            "should recompute on viewport_width mismatch"
        );
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.key.viewport_width, 100);
    }

    #[test]
    fn cached_or_compute_lines_turn_id_mismatch_recomputes() {
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                turn_id: 7, // cached entry is for turn 7
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        // Request turn_id 42 at the same index — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                turn_id: 42,
                ..base_key()
            },
            || {
                compute_called.set(true);
                rendered(vec![Line::from("fresh")])
            },
        );
        assert!(compute_called.get(), "should recompute on turn_id mismatch");
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.key.turn_id, 42,
            "cache entry should be updated to new turn_id"
        );
    }

    #[test]
    fn cached_or_compute_lines_out_of_range_index_does_not_store() {
        let mut cache = vec![None]; // length 1
        // Request index 5 which is out of range
        let rendered = cached_or_compute_lines(&mut cache, 5, &base_key(), compute_one_line);
        assert_eq!(
            rendered.lines.len(),
            1,
            "should still return computed result"
        );
        assert_eq!(rendered.height, 1);
        assert_eq!(&*rendered.visual_offsets, &[1]);
        // Cache should remain unchanged (all entries still None since index 5 doesn't exist)
        assert!(
            cache[0].is_none(),
            "original cache entry should be untouched"
        );
    }

    #[test]
    fn cached_or_compute_lines_height_matches_lines_height() {
        let mut cache = vec![None];
        let lines = vec![
            Line::from("line one"),
            Line::from("line two"),
            Line::from("line three"),
        ];
        let expected_h = lines_height(&lines, 80);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                width: 70, // content_width
                viewport_width: 80,
                ..base_key()
            },
            || rendered(lines.clone()),
        );
        assert_eq!(
            rendered.height, expected_h,
            "returned height should match lines_height"
        );
        assert_eq!(
            *rendered.visual_offsets.last().unwrap(),
            expected_h,
            "last offset should equal total visual height"
        );
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.rendered.height, expected_h,
            "stored height should match"
        );
        assert_eq!(
            *cached.rendered.visual_offsets.last().unwrap(),
            expected_h,
            "stored last offset should equal total height"
        );
    }

    #[test]
    fn cached_or_compute_lines_none_slot_treated_as_miss() {
        let mut cache: Vec<Option<RenderedCache>> = vec![None];
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
            compute_called.set(true);
            compute_one_line()
        });
        assert!(compute_called.get(), "should compute when slot is None");
        assert!(cache[0].is_some(), "should fill the slot");
        drop(rendered.lines);
    }

    #[test]
    fn cached_or_compute_lines_reasoning_expanded_mismatch_recomputes() {
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                reasoning_expanded: false, // cached as collapsed
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        // Request with reasoning expanded — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                reasoning_expanded: true,
                ..base_key()
            },
            || {
                compute_called.set(true);
                rendered(vec![Line::from("fresh")])
            },
        );
        assert!(
            compute_called.get(),
            "should recompute when reasoning_expanded differs"
        );
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert!(
            cached.key.reasoning_expanded,
            "cache entry should record the new reasoning state"
        );
    }

    #[test]
    fn cached_or_compute_lines_tool_results_collapsed_mismatch_recomputes() {
        let mut cache = vec![Some(RenderedCache {
            key: RenderCacheKey {
                tool_results_collapsed: vec![true], // cached as collapsed
                ..base_key()
            },
            rendered: RenderedTurn {
                lines: Arc::from(vec![Line::from("stale")]),
                height: 99,
                visual_offsets: Arc::from([1]),
                content_ranges: Arc::from([]),
                reasoning_header_idx: None,
                tool_result_header_idxs: vec![0],
            },
        })];
        // Request with the result expanded — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                tool_results_collapsed: vec![false],
                ..base_key()
            },
            || {
                compute_called.set(true);
                rendered(vec![Line::from("fresh")])
            },
        );
        assert!(
            compute_called.get(),
            "should recompute when tool_results_collapsed differs"
        );
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.key.tool_results_collapsed,
            vec![false],
            "cache entry should record the new collapse state"
        );
    }

    #[test]
    fn cached_or_compute_lines_content_version_mismatch_recomputes() {
        // The cache key carries a per-turn content version so a rebuild can
        // never reuse a rendering of a turn whose content grew behind the
        // key's other fields (a tool-result chunk appended between a rebuild
        // and the streaming fast path).  Same turn/widths/collapse state, but
        // a newer content version — must be a miss.
        let mut cache = vec![Some(cache_entry(
            RenderCacheKey {
                content_version: 1, // cached from an earlier chunk
                ..base_key()
            },
            Arc::from(vec![Line::from("stale")]),
            99,
            Arc::from([1]),
        ))];
        // Request with a bumped content version — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let rendered = cached_or_compute_lines(
            &mut cache,
            0,
            &RenderCacheKey {
                content_version: 2,
                ..base_key()
            },
            || {
                compute_called.set(true);
                rendered(vec![Line::from("fresh")])
            },
        );
        assert!(
            compute_called.get(),
            "should recompute when content_version differs"
        );
        assert_eq!(rendered.lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.key.content_version, 2,
            "cache entry should record the new content version"
        );
    }

    // ── compute_visual_offsets ─────────────────────────────────

    #[test]
    fn compute_visual_offsets_single_line_fits() {
        let lines = vec![Line::from("hello")];
        let offsets = compute_visual_offsets(&lines, 80);
        assert_eq!(&*offsets, &[1], "short line at wide viewport = 1 row");
    }

    #[test]
    fn compute_visual_offsets_single_line_wraps() {
        let long = "a".repeat(200);
        let lines = vec![Line::from(long)];
        let offsets = compute_visual_offsets(&lines, 80);
        assert_eq!(&*offsets, &[3], "200 chars at 80-wide wraps to 3 rows");
    }

    #[test]
    fn compute_visual_offsets_empty_lines_count_as_one_row_each() {
        let lines = vec![Line::from(""), Line::from(""), Line::from("")];
        let offsets = compute_visual_offsets(&lines, 80);
        assert_eq!(&*offsets, &[1, 2, 3], "each empty line = 1 visual row");
    }

    #[test]
    fn compute_visual_offsets_mixed_lines() {
        let lines = vec![
            Line::from("short"),
            Line::from(""),              // 1 row
            Line::from("x".repeat(150)), // 2 rows at 80
        ];
        let offsets = compute_visual_offsets(&lines, 80);
        assert_eq!(&*offsets, &[1, 2, 4]);
    }

    #[test]
    fn compute_visual_offsets_empty_slice() {
        let lines: Vec<Line<'static>> = vec![];
        let offsets = compute_visual_offsets(&lines, 80);
        assert!(offsets.is_empty(), "no lines → no offsets");
    }

    #[test]
    fn compute_visual_offsets_zero_width_each_line_zero() {
        let lines = vec![Line::from("hello"), Line::from("world")];
        let offsets = compute_visual_offsets(&lines, 0);
        // At width 0 every line contributes 0 visual rows, so each
        // cumulative entry stays 0 (same length as lines).
        assert_eq!(&*offsets, &[0, 0], "zero width → each entry = 0");
    }

    // ── partition_point mapping (visual row → line index) ──────

    #[test]
    fn partition_point_finds_line_at_row_zero() {
        let offsets = [2, 5, 7];
        assert_eq!(offsets.partition_point(|&o| o == 0), 0);
    }

    #[test]
    fn partition_point_finds_line_in_middle() {
        let offsets = [2, 5, 7];
        // row 3 falls in the second line (offset 2 < 3, offset 5 > 3)
        assert_eq!(offsets.partition_point(|&o| o <= 3), 1);
    }

    #[test]
    fn partition_point_finds_line_at_exact_boundary() {
        let offsets = [2, 5, 7];
        // row 2 is the last visual row of line 0 — still maps to line 0
        assert_eq!(offsets.partition_point(|&o| o <= 2), 1);
        // row 5 maps to line 2
        assert_eq!(offsets.partition_point(|&o| o <= 5), 2);
    }

    #[test]
    fn partition_point_past_end_returns_len() {
        let offsets = [2, 5, 7];
        assert_eq!(offsets.partition_point(|&o| o <= 7), 3);
        assert_eq!(offsets.partition_point(|&o| o <= 99), 3);
    }

    #[test]
    fn partition_point_empty_offsets_returns_zero() {
        let offsets: [usize; 0] = [];
        assert_eq!(offsets.partition_point(|&o| o == 0), 0);
        assert_eq!(offsets.partition_point(|&o| o <= 99), 0);
    }
}
