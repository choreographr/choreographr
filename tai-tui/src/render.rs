use crate::markdown_render::{display_width, lines_height, render_turn_lines};
use crate::scrollbar::{SmoothScrollbar, SmoothScrollbarState};
use crate::state::{
    AI_PROVIDER_ITEM_LINES, AIProvidersView, App, HOME_MENU_ITEMS, IMAGE_BLOCK_HEIGHT,
    INPUT_BAR_HEIGHT, PROVIDER_OPTIONS, Page, RenderedCache, STATUS_BAR_HEIGHT,
    STATUS_ERROR_BAR_HEIGHT, SessionManagerView,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::StatefulImage;
use tai_proto::{SessionStatus, ThinkingEffort};

use tui_prompts::{
    Prompt, SelectOption, SelectOptionList, SelectPrompt, TextPrompt, TextRenderStyle,
};

const BG_SHADE: Color = Color::Rgb(60, 60, 60);

fn history_area() -> Option<(u16, u16)> {
    let Ok((width, height)) = crossterm::terminal::size() else {
        return None;
    };
    let bottom_height = INPUT_BAR_HEIGHT + STATUS_BAR_HEIGHT + STATUS_ERROR_BAR_HEIGHT;
    if width < 2 || height <= bottom_height {
        return None;
    }
    Some((width, height.saturating_sub(bottom_height)))
}

pub(crate) fn mouse_in_history_box(column: u16, row: u16) -> bool {
    let (width, history_height) = match history_area() {
        Some(v) => v,
        None => return false,
    };
    column < width.saturating_sub(1) && row < history_height
}

pub(crate) fn mouse_in_scrollbar_column(column: u16, row: u16) -> bool {
    let (width, history_height) = match history_area() {
        Some(v) => v,
        None => return false,
    };
    column == width.saturating_sub(1) && row < history_height
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
        Page::Settings => render_settings(frame, app),
        Page::Home => render_home(frame, app),
    }
}

/// Look up the fullscreen image by (turn_id, img_idx) and render it.
pub(crate) fn render_fullscreen_only(frame: &mut Frame<'_>, app: &mut App) -> bool {
    let Some((turn_id, img_idx)) = app.fullscreen_image_target else {
        return false;
    };
    if !app.rendered_images.contains_key(&turn_id) {
        app.fullscreen_image_target = None;
        return false;
    }
    render_fullscreen_image(frame, turn_id, img_idx, app);
    true
}

fn render_fullscreen_placeholder(frame: &mut Frame<'_>) {
    let area = frame.area();
    let block = Block::bordered()
        .title(" Loading image … ")
        .title_alignment(Alignment::Center);
    frame.render_widget(block, area);
}

fn render_fullscreen_image(frame: &mut Frame<'_>, turn_id: u32, img_idx: usize, app: &mut App) {
    let area = frame.area();
    let full = Size::new(area.width, area.height);

    // Fast path — already encoded at full size.
    let should_submit = match app
        .rendered_images
        .get_mut(&turn_id)
        .and_then(|images| images.get_mut(&img_idx))
    {
        Some(img) => {
            if let Some(protocol) = img.protocols.get_mut(&full) {
                let target = protocol.size_for(tai_tui::IMAGE_RESIZE, full);
                let centered = Rect {
                    x: area.x + (area.width.saturating_sub(target.width)) / 2,
                    y: area.y + (area.height.saturating_sub(target.height)) / 2,
                    width: target.width.min(area.width),
                    height: target.height.min(area.height),
                };
                frame.render_stateful_widget(
                    StatefulImage::new().resize(tai_tui::IMAGE_RESIZE),
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
        && let Some(images) = app.rendered_images.get(&turn_id)
        && let Some(img) = images.get(&img_idx)
    {
        app.submit_image_job(
            turn_id,
            img_idx,
            img.data.clone(),
            img.metadata.clone(),
            full,
            tai_tui::IMAGE_RESIZE,
        );
    }

    render_fullscreen_placeholder(frame);
}

fn render_settings(frame: &mut Frame<'_>, _app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default().title(" Settings ").borders(Borders::ALL);
    frame.render_widget(block, chunks[0]);

    let status = Paragraph::new(Line::from(" <Esc home>  <Ctrl+C quit>"));
    frame.render_widget(status, chunks[1]);
}

fn render_home(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(12),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(area);

    let menu_area = chunks[0];
    let menu_items: Vec<Line> = HOME_MENU_ITEMS
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let is_selected = i == app.home_selection;
            let prefix = if is_selected { " > " } else { "   " };
            let label = item.label();
            let hint = item.key_hint();
            let text = format!("{prefix}{label} {hint}");
            let style = if is_selected {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(text, style))
        })
        .collect();

    let menu = Paragraph::new(menu_items).alignment(Alignment::Center);
    frame.render_widget(menu, menu_area);

    let status = Paragraph::new(Line::from(
        " <j/k nav>  <Enter select>  <s sessions>  <p ai providers>  <t settings>  <q quit>  <Esc back>",
    ));
    frame.render_widget(status, chunks[1]);
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(STATUS_ERROR_BAR_HEIGHT),
            Constraint::Length(INPUT_BAR_HEIGHT),
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
        let marker_slots: Vec<usize> = app.markers.iter().map(|m| m.virtual_slot).collect();
        frame.render_stateful_widget(
            vertical_scrollbar().with_markers(marker_slots),
            history_chunks[1],
            &mut SmoothScrollbarState::new(total_height)
                .position(position)
                .viewport_content_length(viewport_height),
        );
    }

    // ── Status/error bar (above command box) ──────────────────
    if let Some(ref err) = app.error {
        let err_para = Paragraph::new(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red),
        )))
        .block(Block::default().borders(Borders::ALL).title("error"));
        frame.render_widget(err_para, chunks[1]);
    } else if let Some(ref status) = app.status {
        let status_para = Paragraph::new(Line::from(Span::styled(
            status.clone(),
            Style::default().fg(Color::Cyan),
        )))
        .block(Block::default().borders(Borders::ALL).title("status"));
        frame.render_widget(status_para, chunks[1]);
    }

    // ── Command input box ──────────────────────────────────────
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("command"))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[2]);

    let cursor_x = chunks[2].x.saturating_add(
        1 + display_width(app.input.text.get(..app.input.cursor).unwrap_or("")) as u16,
    );
    let cursor_y = chunks[2].y.saturating_add(1);
    frame.set_cursor_position((cursor_x, cursor_y));

    // ── Status bar ────────────────────────────────────────────
    let status_area = chunks[3];
    let status_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(status_area);

    let has_session = app.attached_session_id.is_some();

    let status_line = if has_session {
        let wd = app.attached_working_dir.as_deref().unwrap_or("-");
        let provider = app.attached_provider_slug.as_deref().unwrap_or("-");
        let model = app.attached_model.as_deref().unwrap_or("-");
        let reasoning = app
            .attached_reasoning_effort
            .as_ref()
            .map(|e| e.as_label())
            .unwrap_or("-");

        Line::from(vec![
            Span::styled(wd, Style::default().fg(Color::White)),
            Span::raw("  |  "),
            Span::styled(provider, Style::default().fg(Color::White)),
            Span::raw("  |  "),
            Span::styled(model, Style::default().fg(Color::White)),
            Span::raw("  |  "),
            Span::styled(reasoning, Style::default().fg(Color::White)),
        ])
    } else {
        Line::from("")
    };
    let status_bar = Paragraph::new(status_line).style(Style::default().bg(Color::Rgb(30, 30, 30)));
    frame.render_widget(status_bar, status_rows[0]);

    let tokens_line = if has_session {
        let tokens = match &app.attached_token_usage {
            Some(usage) => format!("↑{}  ↓{}", usage.output_tokens, usage.input_tokens),
            None => String::new(),
        };
        let context = match (app.attached_context_window, app.attached_last_prompt_tokens) {
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
            (Some(limit), None) => format!("? / {}", humfmt::number(limit)),
            (None, Some(current)) => format!("{} / ?", humfmt::number(current)),
            (None, None) => String::new(),
        };
        let mut spans: Vec<Span> = Vec::new();
        if !tokens.is_empty() {
            spans.push(Span::styled(tokens, Style::default().fg(Color::White)));
        }
        if !context.is_empty() {
            if !spans.is_empty() {
                spans.push(Span::raw("  |  "));
            }
            spans.push(Span::styled(context, Style::default().fg(Color::White)));
        }
        if let Some(status) = &app.attached_status {
            let (label, color) = status_display(status);
            if !spans.is_empty() {
                spans.push(Span::raw("  |  "));
            }
            spans.push(Span::styled(label, Style::default().fg(color)));
        }
        Line::from(spans)
    } else {
        Line::from("")
    };
    let tokens_bar = Paragraph::new(tokens_line).style(Style::default().bg(Color::Rgb(30, 30, 30)));
    frame.render_widget(tokens_bar, status_rows[1]);
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let content_width = area.width.saturating_sub(9);

    app.ensure_cache_synced();

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let mut rows_to_skip = app.effective_scroll();

    let len = app.visible_turn_ids.len();
    for raw_i in 0..len {
        let i = len - 1 - raw_i;
        let turn_id = app.visible_turn_ids[i];

        if rows_remaining == 0 {
            break;
        }

        // Clone turn data before borrowing render_cache mutably.
        let (text_lines, img_count) = {
            let Some(turn) = app.session_view.turns.get(&turn_id) else {
                continue;
            };
            if turn.undone {
                continue;
            }
            let count = turn.displayed_images.len();
            // ── Phase A: text content ──────────────────────────
            let lines = cached_or_compute_lines(&mut app.render_cache, i, content_width, || {
                render_turn_lines(turn, content_width)
            });
            (lines, count)
        };

        let text_height = lines_height(&text_lines, area.width).max(1);

        if let Some((top_line, visible_height)) =
            clipped_area(text_height, &mut rows_to_skip, &mut rows_remaining, &mut y)
        {
            render_margin_lines(
                frame,
                area,
                text_lines,
                top_line,
                visible_height,
                &mut y,
                Style::default().bg(BG_SHADE),
            );
        }

        // ── Phase B: images ────────────────────────────────────
        for img_idx in 0..img_count {
            let img_height = IMAGE_BLOCK_HEIGHT as usize;
            if let Some((_top_line, visible_height)) =
                clipped_area(img_height, &mut rows_to_skip, &mut rows_remaining, &mut y)
            {
                let img_rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: visible_height as u16,
                };
                render_turn_image(frame, img_rect, turn_id, img_idx, app);
            }
        }
    }
}

/// Return the cached rendered lines for a turn at the given cache index,
/// or compute, cache, and return them.
fn cached_or_compute_lines(
    cache: &mut [Option<RenderedCache>],
    index: usize,
    width: u16,
    compute: impl FnOnce() -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    if let Some(Some(cached)) = cache.get(index)
        && cached.width == width
    {
        return cached.lines.clone();
    }

    let lines = compute();
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(RenderedCache {
            lines: lines.clone(),
            width,
        });
    }
    lines
}

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

/// Render pre-margin-decorated lines with viewport clipping.
fn render_margin_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    top_line: usize,
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
        Paragraph::new(lines)
            .style(paragraph_style)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
}

/// Render a single turn-displayed image block.
///
/// Height is always `IMAGE_BLOCK_HEIGHT` regardless of encoding state,
/// so scroll positions remain stable.
fn render_turn_image(
    frame: &mut Frame<'_>,
    area: Rect,
    turn_id: u32,
    img_idx: usize,
    app: &mut App,
) {
    let inline_size = Size::new(area.width, IMAGE_BLOCK_HEIGHT);

    // Extract data we need while the borrow is active.
    let (needs_job, data, meta) = match app
        .rendered_images
        .get_mut(&turn_id)
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
                frame.render_stateful_widget(
                    StatefulImage::new().resize(tai_tui::IMAGE_RESIZE),
                    inner,
                    protocol,
                );
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
            turn_id,
            img_idx,
            data,
            meta,
            inline_size,
            tai_tui::IMAGE_RESIZE,
        );
    }

    // Render placeholder frame while encoding is pending.
    // Reconstruct metadata fields from turn's displayed_images record.
    let placeholder_title = app
        .session_view
        .turns
        .get(&turn_id)
        .and_then(|t| t.displayed_images.get(img_idx))
        .map(|r| {
            format!(
                "image {} ({} {}x{})",
                turn_id, r.metadata.mime_type, r.metadata.width, r.metadata.height,
            )
        })
        .unwrap_or_else(|| format!("image {turn_id}[{img_idx}] (pending)"));
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
            let model_display = if let Some(effort) = session
                .reasoning_effort
                .filter(|e| *e != ThinkingEffort::Off)
            {
                format!("({model}, {})", effort.as_label())
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
                    .filter(|e| *e != ThinkingEffort::Off)
                    .map(|e| e.as_label().to_string())
                    .unwrap_or_else(|| "off".to_string())
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

fn format_timestamp(ts_secs: i64) -> String {
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
