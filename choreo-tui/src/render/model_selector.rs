// The model-selector overlay, extracted from the former monolithic render.rs.
// Drawn last over the Chat page when `app.model_selector` is open.  The popup
// sizing/centering helpers live in the parent render/mod.rs.
use crate::markdown_render::display_width;
use crate::state::App;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::{PopupSize, centered_popup};

/// Centered popup listing the models available on the attached session's
/// account.  Drawn last so it covers the Chat page content.  The filter box
/// reuses `InputBuffer` editing semantics; key handling lives in
/// `handle_model_selector_event` (connection.rs).
// Dispatched from the top-level `render()` in render/mod.rs.
pub(super) fn render_model_selector(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // ~60% of the terminal width and ~2/3 of the height, floored at a sane
    // minimum and capped so the popup never touches the screen edges.
    let popup = centered_popup(area, PopupSize::LIST);
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
