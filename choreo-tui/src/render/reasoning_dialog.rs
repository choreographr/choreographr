// The reasoning-effort dialog, drawn over the Chat page as the last step of the
// staged account choice.  It is deliberately not a `render/model_selector.rs`
// clone: the ladder is a handful of rows, so there is no filter box and no
// scroll window — only the centered box, its rows, and a footer hint.  The
// geometry comes from `state::dialog_layout` so the box is exactly as tall as
// the rows it draws.
use crate::state::{App, dialog_layout};

use super::display_text;
use ratatui::{
    Frame,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

/// Centered modal listing a model's reasoning-effort levels, with the daemon's
/// suggestion marked so a pre-highlighted row is explained rather than
/// arbitrary.  Key handling lives in `connection/setup_flow.rs`
/// (`handle_dialog_event`).
// Dispatched from the top-level `render()` in render/mod.rs.
pub(super) fn render_reasoning_dialog(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // The rows the dialog will draw, so the box is sized to its content.  The
    // row count is a model's effort ladder (a handful of slugs), well inside a
    // `u16`.
    #[expect(clippy::cast_possible_truncation)]
    let rows = app.reasoning_dialog.levels.len() as u16;
    let layout = dialog_layout(area, rows);
    // Erase the region beneath so the dialog reads as a solid overlay rather
    // than text drawn on top of the chat history.
    frame.render_widget(Clear, layout.popup);

    let block = Block::default()
        .title(" Reasoning Effort ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(block, layout.popup);

    if app.reasoning_dialog.levels.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " No effort levels reported for this model.",
                Style::default().fg(Color::DarkGray),
            ))),
            layout.body,
        );
        return;
    }

    let focused = app.reasoning_dialog.focused;
    let levels = app.reasoning_dialog.levels.clone();
    let mut lines: Vec<Line> = Vec::with_capacity(levels.len());
    for (i, level) in levels.iter().enumerate() {
        let is_focused = i == focused;
        // `>` marks the row Enter will commit. The suggestion is marked in
        // words, not with a second glyph, so "why is this row highlighted?" is
        // answered without a legend.
        let prefix = if is_focused { "> " } else { "  " };
        let marker = if app.reasoning_dialog.is_suggested(level) {
            "  (suggested)"
        } else {
            ""
        };
        let style = if is_focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        // Effort slugs come from the catalog (and, through the daemon, the
        // provider's own vocabulary): normalize so an odd slug cannot shift the
        // row it is drawn on.
        lines.push(Line::from(vec![
            Span::styled(format!("{prefix}{}", display_text(level)), style),
            Span::styled(marker, Style::default().fg(Color::DarkGray)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), layout.body);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " enter select · esc cancel",
            Style::default().fg(Color::DarkGray),
        ))),
        layout.footer,
    );
}
