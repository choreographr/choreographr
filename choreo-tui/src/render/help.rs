//! The Help modal: a read-only overlay listing the keyboard shortcuts.
//!
//! Opened with `Alt+H` or `/help` (`App::show_help_modal`), drawn over the Chat
//! page with the shared reference-popup chrome (`render/popup.rs`) and closed by
//! `Esc` or the same chord in `handle_terminal_event`'s overlay guard, which
//! swallows every other key so nothing reaches the composer underneath.
//!
//! The page is a fixed reference, taller than most terminals, so its popup is
//! **content-fitted and scrollable**: the shared popup measures the wrapped text
//! and grows to it (clamped to the screen), and the overlay guard drives
//! `App::help_modal_scroll` with `Up`/`Down`/`PgUp`/`PgDn` when it overflows.
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

use super::popup::render_reference_popup;
use super::style::{dim_style, heading_style};
use crate::state::{App, PopupSize, shortcut_label_for};
use ratatui::{
    Frame,
    style::{Color, Style},
    text::{Line, Span},
};

/// Column the shortcut labels are padded to so every description lines up: wide
/// enough for the longest literal chord (`Ctrl+A / Ctrl+E`) plus a two-space gap.
const LABEL_WIDTH: usize = 18;

/// A key chord, so every shortcut reads as a key rather than prose. Yellow here
/// (the reference-page style module keeps only the styles its users share; the
/// welcome panel paints its chords cyan).
fn key_style() -> Style {
    Style::default().fg(Color::Yellow)
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
    lines.push(row(
        &shortcut("reasoning-toggle"),
        "expand / collapse every reasoning section",
    ));
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

/// Draw the Help modal centered over the Chat page, sized to its content and
/// scrolled by the overlay guard's `App::help_modal_scroll`.
pub(super) fn render_help(frame: &mut Frame<'_>, app: &mut App) {
    render_reference_popup(
        frame,
        "Help",
        PopupSize::LIST,
        help_lines(),
        &mut app.help_modal_scroll,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::lines_as_text;

    /// Every line of the page, newline-joined, for substring assertions.
    fn text() -> String {
        lines_as_text(&help_lines())
    }

    #[test]
    fn every_shortcut_command_is_listed_with_its_table_chord() {
        let out = text();
        // Each table-backed command appears with the label the shortcut table
        // renders, so the page can never advertise a chord dispatch does not bind.
        for command in [
            "model",
            "reasoning",
            "reasoning-toggle",
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

    #[test]
    fn every_row_label_fits_the_description_column() {
        // `row` pads the label to `LABEL_WIDTH`; a longer label would silently
        // push its description out of alignment rather than failing anywhere.
        // Each `row` line is exactly two spans whose first is the two-space
        // indent plus the padded label — its width is `2 + LABEL_WIDTH` — while
        // the single note line's first span is the bare two-space indent, and
        // headings/closing hint are single-span lines. So a first span wider
        // than the fixed column means a label overflowed it.
        for line in help_lines() {
            if let [first, _] = line.spans.as_slice() {
                let label_field = first.content.as_ref();
                if label_field.len() > 2 {
                    assert_eq!(
                        label_field.chars().count(),
                        2 + LABEL_WIDTH,
                        "row label overflows the {LABEL_WIDTH}-column field: {label_field:?}"
                    );
                }
            }
        }
    }
}
