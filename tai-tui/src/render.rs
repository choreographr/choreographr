use crate::markdown_render::{
    display_width, lines_height, session_message_lines, streaming_text_lines,
};
use crate::state::{
    App, HistoryItem, Page, SessionManagerView, history_text_height, image_block_height,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::StatefulImage;
use tai_proto::SessionStatus;

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
    match app.page {
        Page::Chat => render_chat(frame, app),
        Page::SessionManager => render_session_manager(frame, app),
    }
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    app.history_viewport.update(chunks[0]);
    app.clamp_scroll_state();
    render_history(frame, chunks[0], app);

    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("command"))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[1]);

    let cursor_x = chunks[1]
        .x
        .saturating_add(1 + display_width(&app.input) as u16);
    let cursor_y = chunks[1].y.saturating_add(1);
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let mut rows_to_skip = app.effective_scroll();

    for item in app.client.history.iter_mut().rev() {
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
                );
            }
            HistoryItem::SessionMessage(message) => {
                let lines = session_message_lines(message, area.width);
                render_history_lines(
                    frame,
                    area,
                    lines,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
            HistoryItem::Streaming(text) => {
                let lines = streaming_text_lines(text, area.width);
                render_history_lines(
                    frame,
                    area,
                    lines,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
            HistoryItem::Image(image) => {
                let full_height = image_block_height(area.height as usize);
                if rows_to_skip >= full_height {
                    rows_to_skip -= full_height;
                    continue;
                }

                let height = image_block_height(rows_remaining) as u16;
                if height == 0 {
                    break;
                }
                y = y.saturating_sub(height);
                let block = Block::default().title(format!(
                    "image {} ({} {}x{})",
                    image.metadata.image_id,
                    image.metadata.mime_type,
                    image.metadata.width,
                    image.metadata.height
                ));
                let rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height,
                };
                let inner = block.inner(rect);
                frame.render_widget(block, rect);
                frame.render_stateful_widget(StatefulImage::default(), inner, &mut image.protocol);
                rows_remaining = rows_remaining.saturating_sub(height as usize);
                rows_to_skip = 0;
            }
        }
    }
}

fn render_history_text(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
) {
    let wrapped = history_text_height(text, area.width).max(1);
    if *rows_to_skip >= wrapped {
        *rows_to_skip -= wrapped;
        return;
    }

    let visible_height = wrapped.min(*rows_remaining);
    if visible_height == 0 {
        return;
    }

    let bottom_line = wrapped.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;
}

fn render_history_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<String>,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
) {
    let wrapped = lines_height(&lines, area.width).max(1);
    if *rows_to_skip >= wrapped {
        *rows_to_skip -= wrapped;
        return;
    }

    let visible_height = wrapped.min(*rows_remaining);
    if visible_height == 0 {
        return;
    }

    let bottom_line = wrapped.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;
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
            let status_str = match &session.status {
                SessionStatus::Sleeping => "sleep",
                SessionStatus::Inactive => "idle",
                SessionStatus::Inference => "infer",
                SessionStatus::ToolCall(name) => &name,
            };
            let status_style = match &session.status {
                SessionStatus::Sleeping => Color::DarkGray,
                SessionStatus::Inactive => Color::Green,
                SessionStatus::Inference => Color::Yellow,
                SessionStatus::ToolCall(_) => Color::Cyan,
            };
            let row = format!(
                "{sel}{att} {:>4}  \"{title}\"  ({model})  — {} messages  [",
                session.session_id, session.message_count,
            );

            let style = if is_selected {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
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

    let status = Paragraph::new(Line::from(format!(
        " <j/k nav>  <Enter switch>  <i details>  <n new>  <Esc back>  —  {} sessions",
        app.session_mgr.sessions.len()
    )));
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
                "Parent:        {}",
                detail
                    .parent_session_id
                    .map_or("none".to_string(), |id| id.to_string())
            )),
            Line::from(format!("CWD:           {}", detail.cwd)),
            Line::from(format!("Created:       {}", format_timestamp(detail.created_at))),
            Line::from(format!("Message Count: {}", detail.message_count)),
            Line::from(format!(
                "Max Turns:     {}",
                detail
                    .max_turns
                    .map_or("unlimited".to_string(), |mt| mt.to_string())
            )),
            Line::from(format!("Status:        {}", format_status(&detail.status))),
        ];
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    let status =
        Paragraph::new(Line::from(" <b back>  <Enter switch to this session>"));
    frame.render_widget(status, chunks[1]);
}

fn format_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return "-".to_string();
    }

    let mut t = ts as u64;
    let secs = t % 60;
    t /= 60;
    let mins = t % 60;
    t /= 60;
    let hours = t % 24;
    t /= 24;
    let days = t;

    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if d < days_in_year {
            break;
        }
        d -= days_in_year;
        y += 1;
    }

    const MONTH_DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    for &md in &MONTH_DAYS {
        let adj = if m == 1 && is_leap(y) { 29 } else { md };
        if d < adj {
            break;
        }
        d -= adj;
        m += 1;
    }

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m + 1,
        d + 1,
        hours,
        mins,
        secs
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn format_status(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Sleeping => "sleeping".to_string(),
        SessionStatus::Inactive => "idle".to_string(),
        SessionStatus::Inference => "inferring".to_string(),
        SessionStatus::ToolCall(name) => format!("tool call: {name}"),
    }
}
