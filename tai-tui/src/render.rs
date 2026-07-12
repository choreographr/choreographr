use crate::diff_render::build_diff_panes;
use crate::markdown_render::{
    display_width, lines_height, session_message_lines, streaming_text_lines,
};
use crate::state::PROVIDER_OPTIONS;
use crate::state::{
    AIProvidersView, App, HOME_MENU_ITEMS, HistoryItem, INPUT_BAR_HEIGHT, Page, RenderedCache,
    SessionManagerView, history_text_height,
};
use image::imageops::FilterType;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::{Resize, StatefulImage, picker::Picker};
use tai_client_core::{DiffLineKind, FileDiff, StreamingText};
use tai_proto::SessionMessage;
use tai_proto::SessionStatus;
use tai_proto::ThinkingEffort;
use tai_tui::RenderedImage;
use tui_prompts::{
    Prompt, SelectOption, SelectOptionList, SelectPrompt, TextPrompt, TextRenderStyle,
};

pub(crate) fn mouse_in_history_box(column: u16, row: u16) -> bool {
    let Ok((width, height)) = crossterm::terminal::size() else {
        return false;
    };

    if width == 0 || height == 0 || height <= 3 {
        return false;
    }

    let input_height = 3;
    let history_height = height.saturating_sub(input_height);
    column < width && row < history_height
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    // When the user just clicked an image, show a loading placeholder on
    // the very first frame.  `run_ui_loop` will follow up with a second
    // draw that actually encodes the image at full size.
    if app.fullscreen_placeholder {
        render_fullscreen_placeholder(frame);
        return;
    }

    // Fullscreen image overlay — rendered directly from the existing
    // `StatefulProtocol` in history (no re-decode of raw bytes on click).
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

/// Look up the fullscreen image by index and render it.  Returns `true` if
/// a valid image was found and rendered, `false` if the index was stale.
/// This is `pub(crate)` so `run_ui_loop` can call it directly for the
/// follow-up draw after the loading placeholder.
pub(crate) fn render_fullscreen_only(frame: &mut Frame<'_>, app: &mut App) -> bool {
    if let Some(idx) = app.fullscreen_image_idx {
        if idx < app.client.history.len()
            && matches!(&app.client.history[idx], HistoryItem::Image(_))
            && let Some(HistoryItem::Image(rendered)) = app.client.history.get_mut(idx)
        {
            render_fullscreen_image(frame, rendered, app.picker.as_ref());
            return true;
        }
        // History mutated (trimmed from front); index no longer valid.
        app.fullscreen_image_idx = None;
    }
    false
}

/// Draw a centered "Loading image …" placeholder that fills the terminal.
/// Used as instant feedback before the protocol encodes at full size.
fn render_fullscreen_placeholder(frame: &mut Frame<'_>) {
    let area = frame.area();
    let block = Block::bordered()
        .title(" Loading image … ")
        .title_alignment(Alignment::Center);
    frame.render_widget(block, area);
}

/// Render the fullscreen image overlay at the full terminal size with
/// Lanczos3 quality.  The protocol's built-in cache avoids re-encoding on
/// subsequent frames (the first frame encodes, all later frames hit the
/// cache).  The cursor is hidden so it doesn't distract.
///
/// If the image is an SVG that hasn't been re-rasterized yet, this is the
/// moment we rasterize the vector graphics directly at the terminal's pixel
/// resolution (using `picker` to map terminal cells to pixels) so that the
/// fullscreen display is crisp rather than upscaled from the SVG's native
/// viewport size.
fn render_fullscreen_image(
    frame: &mut Frame<'_>,
    rendered: &mut Box<RenderedImage>,
    picker: Option<&Picker>,
) {
    let area = frame.area();
    let full = Size::new(area.width, area.height);

    // Ensure the SVG is rasterized at the full terminal pixel resolution.
    // Non-SVG images and cache hits are no-ops internally.
    if let Some(picker) = picker {
        let _ = rendered.ensure_display_resolution(picker, full);
    }

    // Compute the aspect-ratio-correct cell size at terminal dimensions.
    let target = rendered
        .protocol
        .size_for(Resize::Scale(Some(FilterType::Lanczos3)), full);

    // Center the image within the terminal.
    let centered = Rect {
        x: area.x + (area.width.saturating_sub(target.width)) / 2,
        y: area.y + (area.height.saturating_sub(target.height)) / 2,
        width: target.width.min(area.width),
        height: target.height.min(area.height),
    };

    frame.render_stateful_widget(
        StatefulImage::new().resize(Resize::Scale(Some(FilterType::Lanczos3))),
        centered,
        &mut rendered.protocol,
    );
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

/// Render the Home page with the Tai logo and a menu.
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

    // ── Menu ────────────────────────────────────────────────────
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

    // ── Footer help bar ─────────────────────────────────────────
    let status = Paragraph::new(Line::from(
        " <j/k nav>  <Enter select>  <s sessions>  <p ai providers>  <t settings>  <q quit>  <Esc back>",
    ));
    frame.render_widget(status, chunks[1]);
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(INPUT_BAR_HEIGHT)])
        .split(frame.area());

    render_history(frame, chunks[0], app);

    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("command"))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[1]);

    let cursor_x = chunks[1].x.saturating_add(
        1 + display_width(app.input.text.get(..app.input.cursor).unwrap_or("")) as u16,
    );
    let cursor_y = chunks[1].y.saturating_add(1);
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let content_width = area.width.saturating_sub(2);
    let assistant_content_width = area.width.saturating_sub(4);

    // Ensure the render cache is aligned with the history vector before
    // we start iterating.  If items were pushed/inserted/trimmed since the
    // last frame, this rebuilds the cache from scratch (all Nones).
    app.ensure_cache_synced();

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let mut rows_to_skip = app.effective_scroll();

    // Iterate by index so we can borrow the history and render_cache
    // independently (they are separate fields of App).
    let len = app.client.history.len();
    for raw_i in 0..len {
        let i = len - 1 - raw_i;
        let item = &mut app.client.history[i];

        if rows_remaining == 0 {
            break;
        }

        match item {
            HistoryItem::Text(text) => {
                render_history_text(
                    frame,
                    area,
                    text.as_str(),
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                    content_width,
                );
            }
            HistoryItem::SessionMessage(message) => {
                render_item_session_message(
                    frame,
                    area,
                    message,
                    &mut app.render_cache,
                    i,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                    content_width,
                    assistant_content_width,
                );
            }
            HistoryItem::Streaming(text) => {
                render_item_streaming(
                    frame,
                    area,
                    text,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                    content_width,
                );
            }
            HistoryItem::Image(image) => {
                render_item_image(
                    frame,
                    area,
                    image,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                    app.picker.as_ref(),
                );
            }
            HistoryItem::Diff(diffs) => {
                render_history_diff(
                    frame,
                    area,
                    diffs,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
        }
    }
}

/// Return the cached rendered lines for a history item, or compute, cache,
/// and return them.  Only caches items with stable content (not Streaming).
fn cached_or_compute_lines(
    cache: &mut [Option<RenderedCache>],
    index: usize,
    width: u16,
    compute: impl FnOnce() -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    // Fast path: cache hit at the current width.
    if let Some(Some(cached)) = cache.get(index)
        && cached.width == width
    {
        return cached.lines.clone();
    }

    // Cache miss: compute, store, and return.
    let lines = compute();
    let height = lines_height(&lines, width);
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(RenderedCache {
            lines: lines.clone(),
            height,
            width,
        });
    }
    lines
}

/// Compute the visible window for a scrollable content item.
///
/// Given the total height of an item and the current scroll/viewport state,
/// returns `(top_line, visible_height)` if any portion of the item is visible,
/// or `None` if the item is entirely outside the viewport.  Also updates
/// `rows_to_skip`, `rows_remaining`, and `y` to reflect the consumed rows.
fn clipped_area(
    full_height: usize,
    rows_to_skip: &mut usize,
    rows_remaining: &mut usize,
    y: &mut u16,
) -> Option<(usize, usize)> {
    // Entire item is above the visible area — reduce skip counter and move on
    if *rows_to_skip >= full_height {
        *rows_to_skip -= full_height;
        return None;
    }

    // How many rows of this item actually fit in the remaining viewport
    let visible_height = (full_height.saturating_sub(*rows_to_skip)).min(*rows_remaining);
    if visible_height == 0 {
        return None;
    }

    // The index (within the item's content) of the first visible row
    let bottom_line = full_height.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    // Advance the vertical cursor by the visible portion
    *y = (*y).saturating_sub(visible_height as u16);
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;

    Some((top_line, visible_height))
}

pub(crate) fn render_history_text(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
    content_width: u16,
) {
    let base_wrapped = history_text_height(text, content_width).max(1);
    // +1 for the blank-line separator below each text block
    let wrapped = base_wrapped + 1;

    let Some((top_line, visible_height)) = clipped_area(wrapped, rows_to_skip, rows_remaining, y)
    else {
        return;
    };

    let rect = Rect {
        x: area.x + 1,
        y: *y,
        width: content_width,
        height: visible_height as u16,
    };

    // Prepend a blank line for 1-cell vertical margin.
    let display_text = format!("\n{text}");

    frame.render_widget(
        Paragraph::new(display_text)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
}

/// Wrap each content line with green margin characters on the left and right,
/// and prepend a blank separator line that also has margin characters.
/// Returns the display-ready lines and the total row count.
fn add_margin_lines(lines: Vec<Line<'static>>, content_width: u16) -> (Vec<Line<'static>>, usize) {
    let margin_green = Style::default().fg(Color::Green);
    let cw = content_width as usize;

    // Blank separator line: "│" + spaces + "│"
    let separator = Line::from(vec![
        Span::styled("│ ".to_string(), margin_green),
        Span::styled(" ".repeat(cw), Style::default()),
        Span::styled(" │".to_string(), margin_green),
    ]);

    let mut result = Vec::with_capacity(lines.len() + 1);
    result.push(separator);

    for line in lines {
        let text_width: usize = line.spans.iter().map(|s| display_width(&s.content)).sum();
        let mut spans = Vec::with_capacity(line.spans.len() + 3);
        spans.push(Span::styled("│ ".to_string(), margin_green));
        spans.extend(line.spans);
        let padding = cw.saturating_sub(text_width);
        if padding > 0 {
            spans.push(Span::styled(" ".repeat(padding), Style::default()));
        }
        spans.push(Span::styled(" │".to_string(), margin_green));
        result.push(Line::from(spans));
    }

    let total_rows = result.len();
    (result, total_rows)
}

pub(crate) fn render_history_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
    content_width: u16,
) {
    // +1 for the blank-line separator below each text block
    let wrapped = lines_height(&lines, content_width).max(1) + 1;

    let Some((top_line, visible_height)) = clipped_area(wrapped, rows_to_skip, rows_remaining, y)
    else {
        return;
    };

    let rect = Rect {
        x: area.x + 1,
        y: *y,
        width: content_width,
        height: visible_height as u16,
    };

    // Prepend a blank line for 1-cell vertical margin.
    let mut display_lines = vec![Line::from(Span::styled(String::new(), Style::default()))];
    display_lines.extend(lines);

    frame.render_widget(
        Paragraph::new(display_lines)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
}

/// Render assistant-text lines with green margin characters on the left and right.
fn render_assistant_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
    content_width: u16,
) {
    let (display_lines, total_rows) = add_margin_lines(lines, content_width);

    let Some((top_line, visible_height)) =
        clipped_area(total_rows, rows_to_skip, rows_remaining, y)
    else {
        return;
    };

    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };

    frame.render_widget(
        Paragraph::new(display_lines)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
}

/// Render a `HistoryItem::SessionMessage`: retrieve cached lines (or compute
/// on cache miss), then render via the appropriate helper (assistant margin
/// for `AssistantText`, plain lines otherwise).
#[allow(clippy::too_many_arguments)]
fn render_item_session_message(
    frame: &mut Frame<'_>,
    area: Rect,
    message: &SessionMessage,
    cache: &mut [Option<RenderedCache>],
    idx: usize,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
    content_width: u16,
    assistant_content_width: u16,
) {
    if matches!(message, SessionMessage::AssistantText { .. }) {
        let lines = cached_or_compute_lines(cache, idx, assistant_content_width, || {
            session_message_lines(message, assistant_content_width)
        });
        render_assistant_lines(
            frame,
            area,
            lines,
            rows_remaining,
            y,
            rows_to_skip,
            assistant_content_width,
        );
    } else {
        let lines = cached_or_compute_lines(cache, idx, content_width, || {
            session_message_lines(message, content_width)
        });
        render_history_lines(
            frame,
            area,
            lines,
            rows_remaining,
            y,
            rows_to_skip,
            content_width,
        );
    }
}

/// Render a `HistoryItem::Streaming` — text that changes every frame (never cached).
fn render_item_streaming(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &StreamingText,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
    content_width: u16,
) {
    let lines = streaming_text_lines(text, content_width);
    render_history_lines(
        frame,
        area,
        lines,
        rows_remaining,
        y,
        rows_to_skip,
        content_width,
    );
}

/// Render a `HistoryItem::Image`, clipped to the visible scroll area.
/// The underlying image protocol is only rendered when the item is fully
/// visible to avoid rescaling during scrolling.
///
/// If the image is an SVG that hasn't been re-rasterized yet, we rasterize
/// the vector graphics directly at the inline pixel resolution so that the
/// image is crisp rather than upscaled from the SVG's native viewport size
/// by ratatui-image's Lanczos3 filter.
fn render_item_image(
    frame: &mut Frame<'_>,
    area: Rect,
    image: &mut Box<RenderedImage>,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
    picker: Option<&Picker>,
) {
    let inline_size = ratatui::layout::Size::new(area.width, (area.height / 2).max(1));

    // Ensure the SVG is rasterized at the inline display resolution.
    // Non-SVG images and cache hits are no-ops internally.
    if let Some(picker) = picker {
        let _ = image.ensure_display_resolution(picker, inline_size);
    }

    let rendered = image
        .protocol
        .size_for(Resize::Scale(Some(FilterType::Lanczos3)), inline_size);
    let full_height = rendered.height.max(1) as usize;

    // Use the shared clipped_area helper just like the text/diff renderers.
    let Some((_top_line, visible_height)) =
        clipped_area(full_height, rows_to_skip, rows_remaining, y)
    else {
        // The item is entirely above the viewport; carry on to the next item.
        // (Unlike text renderers we return from the helper directly here so the
        // caller can distinguish "fully skipped" from "viewport exhausted".)
        return;
    };

    let height = visible_height as u16;

    let block = Block::default().title(format!(
        "image {} ({} {}x{})",
        image.metadata.image_id,
        image.metadata.mime_type,
        image.metadata.width,
        image.metadata.height
    ));
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height,
    };
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // Only render the image when fully visible — the image is not clipped
    // by scroll offset or viewport space, so its rect is stable and
    // ratatui_image never rescales during scrolling.
    if visible_height == full_height {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Scale(Some(FilterType::Lanczos3))),
            inner,
            &mut image.protocol,
        );
    }
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

    let scroll = app.session_mgr.scroll;
    let max_rows = inner.height as usize;

    // Show error message if one is set (e.g. daemon locked when creating).
    if let Some(ref err) = app.session_mgr.error {
        let err_style = Style::default().fg(Color::Red);
        let err_text = format!("Error: {err}");
        let err_para = Paragraph::new(Line::from(Span::styled(err_text, err_style)));
        let err_area = Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(err_para, err_area);
    }

    if app.session_mgr.sessions.is_empty() {
        let msg = Paragraph::new("No sessions. Press 'n' to create one.");
        frame.render_widget(msg, inner);
    } else {
        let mut lines: Vec<Line> = Vec::new();
        for i in scroll..app.session_mgr.sessions.len() {
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
            let status_style = match &session.status {
                SessionStatus::Sleeping => Color::DarkGray,
                SessionStatus::Inactive => Color::Green,
                SessionStatus::Inference => Color::Yellow,
                SessionStatus::ToolCall(_) => Color::Cyan,
                SessionStatus::Retrying { .. } => Color::Magenta,
                _ => Color::White,
            };
            let row = format!(
                "{sel}{att} {:>4}  \"{title}\"  {model_display}  — {} messages  [",
                session.session_id, session.message_count,
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
        frame.render_widget(paragraph, inner);
    }

    let status = if let Some((_id, title)) = &app.session_mgr.confirm_delete {
        Paragraph::new(Line::from(format!(" Delete “{title}”? (y/N)  ")))
    } else {
        Paragraph::new(Line::from(format!(
            " <j/k nav>  <Enter switch>  <i details>  <n new>  <d delete>  <Esc back>  —  {} sessions",
            app.session_mgr.sessions.len()
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
            Line::from(format!("Message Count: {}", detail.message_count)),
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
            Line::from(format!("Status:        {}", format_status(&detail.status))),
            Line::from(format!(
                "Tool Groups:   {}",
                detail.active_tool_groups.join(", ")
            )),
        ];
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    let status = Paragraph::new(Line::from(" <b back>  <Enter switch to this session>"));
    frame.render_widget(status, chunks[1]);
}

fn format_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return "-".to_string();
    }

    use chrono::{Local, TimeZone};

    // timestamp_opt handles DST ambiguity and out-of-range inputs.
    // If the result is ambiguous or invalid we fall back to a safe epoch
    // display rather than panicking.
    let dt = match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return "-".to_string(),
    };

    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

// ── AI Provider Accounts ──────────────────────────────────

fn render_ai_providers(frame: &mut Frame<'_>, app: &mut App) {
    // If credential input is active, render that instead of the normal view.
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

    let scroll = app.ai_providers.scroll;
    let max_rows = inner.height as usize;

    if app.ai_providers.accounts.is_empty() {
        let msg = Paragraph::new("No AI provider accounts configured. Press 'n' to add one.");
        frame.render_widget(msg, inner);
    } else {
        let mut lines: Vec<Line> = Vec::new();

        for i in scroll..app.ai_providers.accounts.len() {
            // Each account takes 3 lines (name + provider + credential).
            // If we would exceed the available rows, stop.
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

            // Line 1: name
            let name_line = format!("{sel} {} ", account.name);
            let name_spans = vec![ratatui::text::Span::styled(name_line, style)];
            lines.push(Line::from(name_spans));

            // Line 2: provider (indented, dimmer style)
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

            // Line 3: credential status (indented, dimmer style)
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

            // Blank line separator between cards (but not after the last one)
            if lines.len() < max_rows && i + 1 < app.ai_providers.accounts.len() {
                lines.push(Line::from(Span::styled(String::new(), Style::default())));
            }
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    let status = if let Some(ref name) = app.ai_providers.confirm_remove {
        Paragraph::new(Line::from(format!(" Remove \"{name}\"? (y/N)  ")))
    } else {
        Paragraph::new(Line::from(format!(
            " <j/k nav>  <r remove>  <c credential>  <n new>  <Esc back>  —  {} accounts",
            app.ai_providers.accounts.len()
        )))
    };
    frame.render_widget(status, chunks[1]);
}

/// Render the credential-input overlay for setting an API key on an account.
/// Position the terminal cursor at the insertion point of a text field
/// rendered inside a Paragraph at `area[line]`.
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
    // Mask the key: show first 4 + last 4 if long enough
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

    // Set cursor position for the masked credential input
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

    // ── Form layout ──────────────────────────────────────────
    // Each bordered TextPrompt needs 3 lines. SelectPrompt takes
    // the remaining space so it can scroll if the terminal is
    // too short for all provider options.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Name field
            Constraint::Min(3),    // Provider dropdown
            Constraint::Length(3), // API Key field
            Constraint::Length(1), // Error line
        ])
        .split(inner);

    let border_style = Style::default().fg(Color::Cyan);

    // ── Name field ───────────────────────────────────────────
    let name_prompt = TextPrompt::new(std::borrow::Cow::Borrowed("Name:"))
        .with_block(Block::bordered().border_style(border_style));
    (&name_prompt).draw(frame, rows[0], &mut app.ai_providers.new_name_state);

    // ── Provider dropdown ────────────────────────────────────
    let options: SelectOptionList = PROVIDER_OPTIONS
        .iter()
        .map(|p| SelectOption::new(p.display_name))
        .collect::<Vec<_>>()
        .into();
    let provider_prompt = SelectPrompt::new(std::borrow::Cow::Borrowed("Provider:"), options);
    provider_prompt.draw(frame, rows[1], &mut app.ai_providers.new_provider_state);

    // ── API Key field ────────────────────────────────────────
    let key_prompt = TextPrompt::new(std::borrow::Cow::Borrowed("API Key:"))
        .with_block(Block::bordered().border_style(border_style))
        .with_render_style(TextRenderStyle::Password);
    (&key_prompt).draw(frame, rows[2], &mut app.ai_providers.new_api_key_state);

    // ── Error message ────────────────────────────────────────
    if let Some(ref err) = app.ai_providers.add_error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  Error: {err}"),
                Style::default().fg(Color::Red),
            ))),
            rows[3],
        );
    }

    // Footer
    let status = Paragraph::new(Line::from(" <Enter next>  <Esc cancel>"));
    frame.render_widget(status, chunks[1]);
}

pub(crate) fn format_status(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Sleeping => "sleeping".to_string(),
        SessionStatus::Inactive => "idle".to_string(),
        SessionStatus::Inference => "inferring".to_string(),
        SessionStatus::ToolCall(name) => format!("tool call: {name}"),
        SessionStatus::Retrying {
            attempt,
            max_attempts,
            delay_ms,
        } => {
            format!("retrying ({attempt}/{max_attempts}, {delay_ms}ms)")
        }
        _ => "unknown".to_string(),
    }
}

// ── Diff rendering ─────────────────────────────────────────

/// Render a side-by-side diff into the terminal frame.
///
/// The diff is first converted into aligned left/right pane rows via
/// `build_diff_panes`, then clipped to the visible viewport through the
/// `rows_to_skip` / `rows_remaining` scrolling mechanism (shared with text
/// rendering). Each row is split into two panes separated by a `│` gutter.
/// Deletions appear highlighted in red on the left; additions in green on the right.
pub(crate) fn render_history_diff(
    frame: &mut Frame<'_>,
    area: Rect,
    diffs: &[FileDiff],
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
) {
    use crate::diff_render::diff_display_height;

    let raw_height = diff_display_height(diffs);
    let full_height = raw_height + 2; // +1 for leading blank, +1 for trailing blank

    let Some((top_line, visible_height)) =
        clipped_area(full_height, rows_to_skip, rows_remaining, y)
    else {
        return;
    };

    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };

    let pane_width = if area.width > 4 {
        (area.width - 2) / 2
    } else {
        1
    };

    let rows = build_diff_panes(diffs);

    let separator_style = Style::default().fg(Color::DarkGray);

    // Build diff lines with leading and trailing blank lines for vertical spacing.
    let mut diff_lines: Vec<Line> = Vec::with_capacity(rows.len() + 2);
    // Leading blank line
    diff_lines.push(Line::from(Span::styled(String::new(), Style::default())));
    for row in &rows {
        let mut spans = Vec::new();
        spans.extend(diff_cell_spans(
            &row.left_spans,
            row.left_kind,
            pane_width,
            true,
        ));
        spans.push(ratatui::text::Span::styled(
            "│".to_string(),
            separator_style,
        ));
        spans.extend(diff_cell_spans(
            &row.right_spans,
            row.right_kind,
            pane_width,
            false,
        ));
        diff_lines.push(Line::from(spans));
    }
    // Trailing blank line
    diff_lines.push(Line::from(Span::styled(String::new(), Style::default())));

    let visible_lines: Vec<Line> = diff_lines
        .into_iter()
        .skip(top_line)
        .take(visible_height)
        .collect();

    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, rect);
}

/// Return the diff foreground colour for a given line kind and side.
/// Used as a fallback when no syntax highlighting is present.
fn diff_fg(kind: DiffLineKind, is_left: bool) -> Option<Color> {
    match (kind, is_left) {
        (DiffLineKind::Deletion, true) => Some(Color::Red),
        (DiffLineKind::Addition, false) => Some(Color::Green),
        _ => None,
    }
}

/// Apply diff background styling to pre-computed syntax-highlighted spans.
///
/// * Overlays the red (deletion) or green (addition) background on each span
///   while preserving the syntax foreground colour from syntect.
/// * Pads with spaces when the content is narrower than the pane so the
///   background colour fills the entire cell.
/// * Truncates content that exceeds the pane width, appending `…`.
pub(crate) fn diff_cell_spans(
    spans: &[ratatui::text::Span<'static>],
    kind: DiffLineKind,
    pane_width: u16,
    is_left: bool,
) -> Vec<ratatui::text::Span<'static>> {
    let pane_width = pane_width as usize;

    let diff_bg = match (kind, is_left) {
        (DiffLineKind::Deletion, true) => Some(Color::Rgb(80, 0, 0)),
        (DiffLineKind::Addition, false) => Some(Color::Rgb(0, 80, 0)),
        _ => None,
    };

    // Apply background to each span while keeping the syntax foreground.
    // When no syntax foreground is set (plain text), fall back to the diff
    // foreground colour (red for deletions, green for additions) so that
    // header / hunk-header rows and plain-text file types still get coloured.
    let styled: Vec<ratatui::text::Span<'static>> = spans
        .iter()
        .map(|s| {
            let style = match diff_bg {
                Some(bg) => ratatui::style::Style {
                    fg: s.style.fg.or_else(|| diff_fg(kind, is_left)),
                    ..ratatui::style::Style::default()
                }
                .bg(bg),
                None => s.style,
            };
            ratatui::text::Span::styled(s.content.clone(), style)
        })
        .collect();

    let total: usize = styled.iter().map(|s| display_width(&s.content)).sum();

    if total > pane_width {
        // Truncate spans to fit pane_width, preserving syntax colours on
        // the remaining content and appending `…` at the end.
        let mut result = Vec::new();
        let mut remaining = pane_width.saturating_sub(1);
        for span in styled {
            let w = display_width(&span.content);
            if w <= remaining {
                result.push(span);
                remaining -= w;
            } else if remaining > 0 {
                // Walk characters by display width, not by scalar count,
                // so CJK/emoji (width 2) are handled correctly.
                let truncated: String = span
                    .content
                    .chars()
                    .scan(remaining, |budget, c| {
                        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                        if *budget >= cw {
                            *budget -= cw;
                            Some(c)
                        } else {
                            None
                        }
                    })
                    .collect();
                result.push(ratatui::text::Span::styled(truncated, span.style));
                break;
            } else {
                break;
            }
        }
        let ellipsis_style = diff_bg.map_or(Style::default(), |b| Style::default().bg(b));
        result.push(ratatui::text::Span::styled("…".to_string(), ellipsis_style));
        result
    } else {
        // Always pad to pane_width so the `│` separator stays at a fixed
        // column (every row's left and right cells must have the same byte
        // width).  The padding span uses the diff background when present,
        // otherwise default style.
        let mut result = styled;
        let pad_style = diff_bg.map_or(Style::default(), |b| {
            ratatui::style::Style {
                fg: None,
                ..ratatui::style::Style::default()
            }
            .bg(b)
        });
        result.push(ratatui::text::Span::styled(
            " ".repeat(pane_width.saturating_sub(total)),
            pad_style,
        ));
        result
    }
}
