//! The Help modal: a read-only overlay listing the keyboard shortcuts.
//!
//! Opened with `Alt+H` or `/help` (`App::show_help_modal`), drawn over the Chat
//! page with the same centered `LIST` popup and bordered block as the Terminal
//! page and the model selector, and closed by `Esc` or the same chord in
//! `handle_terminal_event`'s overlay guard, which swallows every other key so
//! nothing reaches the composer underneath.
//!
//! The command chords are read from the shortcut table via
//! [`crate::state::shortcut_label_for`] (the single source of truth shared with
//! dispatch and the command palette's labels), so the chords this page prints
//! can never drift from the ones the client actually binds. Only the
//! non-`Alt+` chords (`Esc`, the readline `Ctrl+` editing keys) are spelled out
//! here as literals, because they are not in the table.
//!
//! The line builder ([`help_lines`]) is a pure function, so the whole page is
//! unit-testable without a terminal, exactly like `terminal_page_lines`.

use crate::state::{PopupSize, centered_popup, shortcut_label_for};
use ratatui::{
    Frame,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// Column the shortcut labels are padded to so every description lines up: wide
/// enough for the longest literal chord (`Ctrl+A / Ctrl+E`) plus a two-space gap.
const LABEL_WIDTH: usize = 18;

/// A section heading inside the page.
fn heading_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// A key chord, so every shortcut reads as a key rather than prose.
fn key_style() -> Style {
    Style::default().fg(Color::Yellow)
}

/// Secondary prose: the explanatory note under the command row.
fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// The display label for a command from the shortcut table — the same string the
/// command palette shows — or the command name itself if it has no binding (a
/// programming miss, but it keeps the page rendering rather than panicking).
fn shortcut(command: &str) -> String {
    shortcut_label_for(command).unwrap_or_else(|| command.to_string())
}

/// One shortcut row: the chord (key-styled, padded so the descriptions align),
/// then the description.
fn row(label: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<LABEL_WIDTH$}"), key_style()),
        Span::styled(desc.to_string(), Style::default()),
    ])
}

/// Build the page's lines.
///
/// Sections, top to bottom: turn control, navigate & settings, editing, and the
/// slash-command affordance, followed by the closing key hint. Every command
/// chord is looked up from the shortcut table, so this page and dispatch cannot
/// disagree about which key runs which command.
pub(super) fn help_lines() -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    lines.push(Line::from(Span::styled("turn control", heading_style())));
    lines.push(row("Esc", "stop the running request"));
    lines.push(row(&shortcut("continue"), "continue the session"));
    lines.push(row(
        &format!("{} / {}", shortcut("undo"), shortcut("redo")),
        "undo / redo",
    ));
    lines.push(Line::default());

    lines.push(Line::from(Span::styled(
        "navigate & settings",
        heading_style(),
    )));
    lines.push(row(&shortcut("model"), "choose the model"));
    lines.push(row(&shortcut("reasoning"), "cycle the reasoning effort"));
    lines.push(row(&shortcut("terminal"), "terminal capabilities page"));
    lines.push(row(&shortcut("account"), "accounts"));
    lines.push(row(&shortcut("session"), "sessions"));
    lines.push(row(&shortcut("help"), "show this help"));
    lines.push(row(&shortcut("quit"), "quit"));
    lines.push(Line::default());

    lines.push(Line::from(Span::styled("editing", heading_style())));
    lines.push(row("Shift+Enter", "insert a new line"));
    lines.push(row("Ctrl+A / Ctrl+E", "start / end of line"));
    lines.push(row("Ctrl+B / Ctrl+F", "back / forward one character"));
    lines.push(row("Ctrl+K / Ctrl+U", "kill to end / start of line"));
    lines.push(row("Ctrl+W / Ctrl+D", "delete word / character"));
    lines.push(row("Ctrl+P / Ctrl+N", "previous / next history"));
    lines.push(row("↑ / ↓", "history or move between wrapped lines"));
    lines.push(row("PgUp / PgDn", "scroll the transcript"));
    lines.push(Line::default());

    lines.push(Line::from(Span::styled("commands", heading_style())));
    lines.push(row("/", "browse slash commands"));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Enter runs the highlighted command, Tab completes it, Esc dismisses.",
            dim_style(),
        ),
    ]));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("Esc or {} to close", shortcut("help")),
        dim_style(),
    )));

    lines
}

/// Draw the Help modal centered over the Chat page.
pub(super) fn render_help(frame: &mut Frame<'_>) {
    let popup = centered_popup(frame.area(), PopupSize::LIST);
    // Erase the region beneath so the page reads as a solid overlay rather than
    // text drawn on top of the chat history (same as the Terminal page).
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // Wrapped, because the explanatory line is longer than the popup on a narrow
    // terminal; the page has no scrollable body, so it clips at the bottom when
    // the area is too short for it — the content is ordered so the most-used
    // sections come first.
    frame.render_widget(
        Paragraph::new(help_lines()).wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line of the page, newline-joined, for substring assertions.
    fn text() -> String {
        help_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_shortcut_command_is_listed_with_its_table_chord() {
        let out = text();
        // Each table-backed command appears with the label the shortcut table
        // renders, so the page can never advertise a chord dispatch does not bind.
        for command in [
            "model",
            "reasoning",
            "continue",
            "undo",
            "redo",
            "terminal",
            "account",
            "session",
            "help",
            "quit",
        ] {
            let label = shortcut(command);
            assert!(
                out.contains(&label),
                "the help page is missing {command:?}'s chord {label:?}:\n{out}"
            );
        }
    }

    #[test]
    fn the_help_page_ends_with_the_closing_hint() {
        let lines = help_lines();
        let last = lines.last().map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        });
        assert_eq!(last.as_deref(), Some("Esc or Alt+H to close"));
    }

    #[test]
    fn the_editing_and_command_sections_are_present() {
        let out = text();
        assert!(out.contains("editing"));
        assert!(out.contains("Shift+Enter"));
        assert!(out.contains("browse slash commands"));
        assert!(out.contains("Tab completes"));
    }
}
