use crate::markdown_render::{display_width, render_turn_lines};
use crate::scrollbar::{SmoothScrollbar, SmoothScrollbarState};
use crate::state::{
    AI_PROVIDER_ITEM_LINES, AIProvidersView, App, CTRL_HELP_LINE1, CTRL_HELP_LINE2,
    PROVIDER_OPTIONS, Page, STATUS_BAR_HEIGHT, SessionManagerView, cached_or_compute_lines,
    cached_visual_lines,
};
use choreo_proto::SessionStatus;
use choreo_tui::RenderedImage;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};
use ratatui_image::StatefulImage;
use std::sync::Arc;

use tui_prompts::{
    Prompt, SelectOption, SelectOptionList, SelectPrompt, TextPrompt, TextRenderStyle,
};

pub(crate) const BG_SHADE: Color = Color::Rgb(53, 53, 53);

/// Horizontal padding (columns) on each side of the command input box.
const INPUT_PAD: u16 = 2;

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
}

/// Look up the fullscreen image by (turn_id, img_idx) and render it.
pub(crate) fn render_fullscreen_only(frame: &mut Frame<'_>, app: &mut App) -> bool {
    let Some((session_id, turn_id, img_idx)) = app.fullscreen_image_target else {
        return false;
    };
    if !app.rendered_images.get(&session_id).is_some_and(|m| m.contains_key(&turn_id))
        && !app.display_for(session_id).view.turns.get(&turn_id).is_some_and(|t| !t.displayed_images.is_empty())
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

fn render_fullscreen_image(frame: &mut Frame<'_>, session_id: u64, turn_id: u32, img_idx: usize, app: &mut App) {
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
                let target = protocol.size_for(choreo_tui::IMAGE_RESIZE, full);
                let centered = Rect {
                    x: area.x + (area.width.saturating_sub(target.width)) / 2,
                    y: area.y + (area.height.saturating_sub(target.height)) / 2,
                    width: target.width.min(area.width),
                    height: target.height.min(area.height),
                };
                frame.render_stateful_widget(
                    StatefulImage::new().resize(choreo_tui::IMAGE_RESIZE),
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
            choreo_tui::IMAGE_RESIZE,
        );
    }

    render_fullscreen_placeholder(frame);
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App) {
    let status_error_height = app.status_error_height(frame.area().width);
    let help_height = if app.show_ctrl_help { 2u16 } else { 0u16 };
    let input_height = app.input_bar_height(frame.area().width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(status_error_height),
            Constraint::Length(help_height),
            Constraint::Length(input_height),
            Constraint::Length(STATUS_BAR_HEIGHT),
        ])
        .split(frame.area());

    // Reserve 1 column on the right for the scrollbar
    let history_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(chunks[0]);

    // Build height_prefix and visible_turn_ids BEFORE rendering history,
    // so render_history iterates the correct set of visible turns rather
    // than an empty visible_turn_ids on the first frame after session data arrives.
    app.compute_total_height_and_markers();
    render_history(frame, history_chunks[0], app);

    // ── Scrollbar ────────────────────────────────────────────
    let viewport_height = app.history_viewport.height as usize;
    let total_height = app.total_history_height();
    if total_height > viewport_height {
        let position = app
            .max_scroll_offset()
            .saturating_sub(app.effective_scroll());
        let marker_slots: Vec<usize> = app.active_display_ref()
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
        x: chunks[1].x + 1,
        width: chunks[1].width.saturating_sub(2),
        ..chunks[1]
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
        let help_area = Rect {
            x: chunks[2].x + 1,
            width: chunks[2].width.saturating_sub(2),
            ..chunks[2]
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
        frame.render_widget(help, help_area);
    }

    // ── Command input box ──────────────────────────────────────
    // Account for INPUT_PAD padding on both sides (left + right = 2 * INPUT_PAD).
    let inner_width = chunks[3].width.saturating_sub(INPUT_PAD * 2) as usize;
    let visible_height = (chunks[3].height.saturating_sub(2)) as usize;

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
    frame.render_widget(input, chunks[3]);
    // Clamp to visible area so the cursor is always inside the box,
    // even when scroll_offset hasn't been adjusted yet (e.g. after
    // loading a long history entry that ends at scroll_offset = 0).
    let max_display_row = (visible_count as u16).saturating_sub(1);
    let display_vrow = vrow.saturating_sub(offset as u16).min(max_display_row);
    let cursor_x = chunks[3].x.saturating_add(INPUT_PAD).saturating_add(vcol);
    let cursor_y = chunks[3].y.saturating_add(1).saturating_add(display_vrow);
    frame.set_cursor_position((cursor_x, cursor_y));

    // ── Status bar (single line) ───────────────────────────────
    let status_area = chunks[4];

    let has_session = app.attached_session_id.is_some();

    let status_line = if has_session {
        // Session-identity values (wd, provider, model, reasoning) — stable
        // across the session — go first (left side) so the bar's leading edge
        // stays fixed.  Runtime metrics (tokens, context fill) follow on the
        // right where their per-turn updates don't shift the identity fields.
        let wd = app.active_display_ref().and_then(|d| d.working_dir.as_deref()).unwrap_or("-");
        let provider = app.attached_provider_slug.as_deref().unwrap_or("-");
        let model = app.active_display_ref().and_then(|d| d.selected_model.as_deref()).unwrap_or("-");
        let reasoning = app.active_display_ref().and_then(|d| d.reasoning_effort.as_deref()).unwrap_or("-");

        // Runtime metrics: tokens flow and context-window fill.
        let tokens = match &app.display_token_usage() {
            Some(usage) => format!("↑{} ↓{}", usage.input_tokens, usage.output_tokens),
            None => String::new(),
        };
        let context = match (app.active_display_ref().and_then(|d| d.context_window), app.active_display_ref().and_then(|d| d.last_prompt_tokens)) {
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
    frame.render_widget(status_bar, status_area);
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
        // and cumulative visual-row offsets for O(log n) row→line lookups.
        let (text_lines_arc, text_height, text_offsets, img_count) = {
            let display = app.display_for(session_id);
            let Some(turn) = display.view.turns.get(&turn_id) else {
                continue;
            };
            if turn.undone {
                continue;
            }
            let count = turn.displayed_images.len();
            let (arc, height, offsets) = cached_or_compute_lines(
                &mut display.render_cache,
                i,
                turn_id,
                content_width,
                area.width,
                || render_turn_lines(turn, content_width, tool_content_width),
            );
            (arc, height, offsets, count)
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
                render_turn_image(frame, img_rect, session_id, turn_id, img_idx, app, fully_visible);
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
            let visible_lines = text_lines_arc[line_start..line_end].to_vec();
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
                    let rendered_at = protocol.size_for(choreo_tui::IMAGE_RESIZE, inline_size);
                    let centered = Rect {
                        x: inner.x + (inner.width.saturating_sub(rendered_at.width)) / 2,
                        y: inner.y + (inner.height.saturating_sub(rendered_at.height)) / 2,
                        width: rendered_at.width.min(inner.width),
                        height: rendered_at.height.min(inner.height),
                    };
                    frame.render_stateful_widget(
                        StatefulImage::new().resize(choreo_tui::IMAGE_RESIZE),
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
            choreo_tui::IMAGE_RESIZE,
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

    let scroll = app.session_mgr.scroll;
    let max_rows = list_chunks[0].height as usize;
    let total_items = app.session_mgr.sessions.len();

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
        let mut lines: Vec<Line> = Vec::new();
        for i in scroll..total_items {
            if lines.len() >= max_rows {
                break;
            }
            let session = &app.session_mgr.sessions[i];
            let is_selected = Some(i) == app.session_mgr.selection;
            let is_attached = Some(session.session_id) == app.attached_session_id;

            let sel = if is_selected { ">" } else { " " };
            let att = if is_attached { "*" } else { " " };
            let title = session.title.as_deref().unwrap_or("untitled");
            let model = session.selected_model.as_deref().unwrap_or("-");
            let model_display =
                if let Some(effort) = session.reasoning_effort.as_deref().filter(|e| *e != "off") {
                    format!("({model}, {effort})")
                } else {
                    format!("({model})")
                };
            let status_str = match &session.status {
                SessionStatus::Sleeping => "sleep",
                SessionStatus::Inactive => "idle",
                SessionStatus::Inference => "infer",
                SessionStatus::ToolCall(name) => name,
                SessionStatus::Retrying { .. } => "retry",
                _ => "unknown",
            };
            let status_style = status_color(&session.status);
            let row = format!(
                "{sel}{att} {:>4}  \"{title}\"  {model_display}  — {} turns  [",
                session.session_id, session.turn_count,
            );

            let style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };
            let status_label = format!("{}]", status_str);
            lines.push(Line::from(vec![
                ratatui::text::Span::styled(row, style),
                ratatui::text::Span::styled(status_label, Style::default().fg(status_style)),
            ]));
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, list_chunks[0]);
    }

    if total_items > max_rows {
        frame.render_stateful_widget(
            vertical_scrollbar(),
            list_chunks[1],
            &mut SmoothScrollbarState::new(total_items)
                .position(scroll)
                .viewport_content_length(max_rows),
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
            Line::from(format!("Turn Count:    {}", detail.turn_count)),
            Line::from(format!(
                "Max Turns:     {}",
                detail
                    .max_turns
                    .map_or("unlimited".to_string(), |mt| mt.to_string())
            )),
            Line::from(format!(
                "Account:       {}",
                detail.account_name.as_deref().unwrap_or("-")
            )),
            Line::from(match &detail.accumulated_usage {
                Some(usage) => format!(
                    "Tokens:        {} in / {} out ({} total)",
                    usage.input_tokens, usage.output_tokens, usage.total_tokens
                ),
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

pub(crate) fn format_timestamp(ts_secs: i64) -> String {
    if ts_secs <= 0 {
        return "-".to_string();
    }

    use chrono::{Local, TimeZone};

    let dt = match Local.timestamp_opt(ts_secs, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return "-".to_string(),
    };

    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M").to_string()
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
        AIProvidersView::NewForm => render_ai_providers_new_form(frame, app),
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

fn render_ai_providers_new_form(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" New AI Provider Account ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);

    let border_style = Style::default().fg(Color::Cyan);

    let name_prompt = TextPrompt::new(std::borrow::Cow::Borrowed("Name:"))
        .with_block(Block::bordered().border_style(border_style));
    (&name_prompt).draw(frame, rows[0], &mut app.ai_providers.new_name_state);

    let options: SelectOptionList = PROVIDER_OPTIONS
        .iter()
        .map(|p| SelectOption::new(p.display_name))
        .collect::<Vec<_>>()
        .into();
    let provider_prompt = SelectPrompt::new(std::borrow::Cow::Borrowed("Provider:"), options);
    provider_prompt.draw(frame, rows[1], &mut app.ai_providers.new_provider_state);

    let key_prompt = TextPrompt::new(std::borrow::Cow::Borrowed("API Key:"))
        .with_block(Block::bordered().border_style(border_style))
        .with_render_style(TextRenderStyle::Password);
    (&key_prompt).draw(frame, rows[2], &mut app.ai_providers.new_api_key_state);

    if let Some(ref err) = app.ai_providers.add_error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  Error: {err}"),
                Style::default().fg(Color::Red),
            ))),
            rows[3],
        );
    }

    let status = Paragraph::new(Line::from(" <Enter next>  <Esc cancel>"));
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

pub(crate) fn status_color(status: &SessionStatus) -> Color {
    status_display(status).1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_render::{compute_visual_offsets, lines_height};
    use crate::state::RenderedCache;

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
    fn compute_one_line() -> Vec<Line<'static>> {
        vec![Line::from("hello")]
    }

    #[test]
    fn cached_or_compute_lines_cache_miss_stores_result() {
        let mut cache = vec![None];
        let (lines, height, offsets) =
            cached_or_compute_lines(&mut cache, 0, 0, 80, 100, compute_one_line);
        assert_eq!(lines.len(), 1, "should return computed lines");
        assert_eq!(height, 1, "single line at any viewport width has height 1");
        assert_eq!(
            &*offsets,
            &[1],
            "single short line should occupy one visual row"
        );
        // Cache should be filled
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.width, 80);
        assert_eq!(cached.viewport_width, 100);
        assert_eq!(cached.height, 1);
        assert_eq!(cached.lines.len(), 1);
        assert_eq!(&*cached.visual_offsets, &[1]);
    }

    #[test]
    fn cached_or_compute_lines_cache_hit_returns_stored_height() {
        let mut cache = vec![Some(RenderedCache {
            turn_id: 0,
            lines: Arc::from(vec![Line::from("cached")]),
            width: 80,
            viewport_width: 100,
            height: 42,
            visual_offsets: Arc::from([99]),
        })];
        // Cache hit — should return stored height without recomputing
        let (lines, height, offsets) = cached_or_compute_lines(&mut cache, 0, 0, 80, 100, || {
            panic!("should not be called on cache hit")
        });
        assert_eq!(height, 42, "should return cached height");
        assert_eq!(lines.len(), 1, "should return cached lines");
        assert_eq!(lines[0], Line::from("cached"));
        assert_eq!(&*offsets, &[99], "should return cached offsets");
    }

    #[test]
    fn cached_or_compute_lines_cache_hit_arc_shares_allocation() {
        let stored = Arc::from(vec![Line::from("shared")]);
        let stored_ptr = Arc::as_ptr(&stored);
        let mut cache = vec![Some(RenderedCache {
            turn_id: 0,
            lines: stored,
            width: 80,
            viewport_width: 100,
            height: 7,
            visual_offsets: Arc::from([1]),
        })];
        let (returned, _, _) =
            cached_or_compute_lines(&mut cache, 0, 0, 80, 100, || panic!("should not recompute"));
        assert_eq!(
            Arc::as_ptr(&returned),
            stored_ptr,
            "returned Arc should point to the same allocation as cache entry"
        );
    }

    #[test]
    fn cached_or_compute_lines_width_mismatch_recomputes() {
        let mut cache = vec![Some(RenderedCache {
            turn_id: 0,
            lines: Arc::from(vec![Line::from("stale")]),
            width: 40, // different from requested width 80
            viewport_width: 100,
            height: 99,
            visual_offsets: Arc::from([1]),
        })];
        let compute_called = std::cell::Cell::new(false);
        let (lines, height, offsets) = cached_or_compute_lines(&mut cache, 0, 0, 80, 100, || {
            compute_called.set(true);
            vec![Line::from("fresh")]
        });
        assert!(compute_called.get(), "should recompute on width mismatch");
        assert_eq!(lines[0], Line::from("fresh"));
        // Height of a single "fresh" line at viewport width 100 is 1
        assert_eq!(height, 1);
        assert_eq!(&*offsets, &[1], "offsets recomputed for fresh lines");
        // Cache should be updated
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.width, 80);
        assert_eq!(cached.viewport_width, 100);
        assert_eq!(cached.lines[0], Line::from("fresh"));
        assert_eq!(&*cached.visual_offsets, &[1]);
    }

    #[test]
    fn cached_or_compute_lines_viewport_width_mismatch_recomputes() {
        let mut cache = vec![Some(RenderedCache {
            turn_id: 0,
            lines: Arc::from(vec![Line::from("stale")]),
            width: 80,
            viewport_width: 40, // different from requested viewport_width 100
            height: 99,
            visual_offsets: Arc::from([1]),
        })];
        let compute_called = std::cell::Cell::new(false);
        let (lines, _height, _offsets) = cached_or_compute_lines(&mut cache, 0, 0, 80, 100, || {
            compute_called.set(true);
            vec![Line::from("fresh")]
        });
        assert!(
            compute_called.get(),
            "should recompute on viewport_width mismatch"
        );
        assert_eq!(lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.viewport_width, 100);
    }

    #[test]
    fn cached_or_compute_lines_turn_id_mismatch_recomputes() {
        let mut cache = vec![Some(RenderedCache {
            turn_id: 7, // cached entry is for turn 7
            lines: Arc::from(vec![Line::from("stale")]),
            width: 80,
            viewport_width: 100,
            height: 99,
            visual_offsets: Arc::from([1]),
        })];
        // Request turn_id 42 at the same index — should be a miss.
        let compute_called = std::cell::Cell::new(false);
        let (lines, _height, _offsets) =
            cached_or_compute_lines(&mut cache, 0, 42, 80, 100, || {
                compute_called.set(true);
                vec![Line::from("fresh")]
            });
        assert!(compute_called.get(), "should recompute on turn_id mismatch");
        assert_eq!(lines[0], Line::from("fresh"));
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(
            cached.turn_id, 42,
            "cache entry should be updated to new turn_id"
        );
    }

    #[test]
    fn cached_or_compute_lines_out_of_range_index_does_not_store() {
        let mut cache = vec![None]; // length 1
        // Request index 5 which is out of range
        let (lines, height, offsets) =
            cached_or_compute_lines(&mut cache, 5, 0, 80, 100, compute_one_line);
        assert_eq!(lines.len(), 1, "should still return computed result");
        assert_eq!(height, 1);
        assert_eq!(&*offsets, &[1]);
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
        let (_, height, offsets) = cached_or_compute_lines(
            &mut cache,
            0,
            0,
            70, // content_width
            80, // viewport_width
            || lines.clone(),
        );
        assert_eq!(
            height, expected_h,
            "returned height should match lines_height"
        );
        assert_eq!(
            *offsets.last().unwrap(),
            expected_h,
            "last offset should equal total visual height"
        );
        let cached = cache[0].as_ref().unwrap();
        assert_eq!(cached.height, expected_h, "stored height should match");
        assert_eq!(
            *cached.visual_offsets.last().unwrap(),
            expected_h,
            "stored last offset should equal total height"
        );
    }

    #[test]
    fn cached_or_compute_lines_none_slot_treated_as_miss() {
        let mut cache: Vec<Option<RenderedCache>> = vec![None];
        let compute_called = std::cell::Cell::new(false);
        let (lines, _height, _offsets) = cached_or_compute_lines(&mut cache, 0, 0, 80, 100, || {
            compute_called.set(true);
            compute_one_line()
        });
        assert!(compute_called.get(), "should compute when slot is None");
        assert!(cache[0].is_some(), "should fill the slot");
        drop(lines);
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
