use crate::state::{
    App, HistoryItem, display_width, image_block_height, lines_height, session_message_lines,
    streaming_text_lines,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::StatefulImage;

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

    for item in app.history.iter_mut().rev() {
        if rows_remaining == 0 {
            break;
        }

        match item {
            HistoryItem::Text(text) => {
                render_history_text(frame, area, text.as_str(), &mut rows_remaining, &mut y, &mut rows_to_skip);
            }
            HistoryItem::SessionMessage(message) => {
                let lines = session_message_lines(message, area.width);
                render_history_lines(frame, area, lines, &mut rows_remaining, &mut y, &mut rows_to_skip);
            }
            HistoryItem::StreamingText(text) => {
                let lines = streaming_text_lines(text, area.width);
                render_history_lines(frame, area, lines, &mut rows_remaining, &mut y, &mut rows_to_skip);
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
                let rect = Rect { x: area.x, y, width: area.width, height };
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
    let wrapped = super::state::history_text_height(text, area.width).max(1);
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
    let rect = Rect { x: area.x, y: *y, width: area.width, height: visible_height as u16 };
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).scroll((top_line as u16, 0)),
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
    let rect = Rect { x: area.x, y: *y, width: area.width, height: visible_height as u16 };
    frame.render_widget(
        Paragraph::new(lines.join("\n")).wrap(Wrap { trim: false }).scroll((top_line as u16, 0)),
        rect,
    );
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;
}
