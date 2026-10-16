//! The welcome panel drawn in place of an empty chat history: a short setup
//! checklist (add an account with its API key, use it in this session, pick a
//! model) that ticks itself off as the daemon confirms each step, followed by
//! a "ready" line once nothing is left. It is the first thing a new user sees,
//! so it names the exact keys for each step rather than leaving them to find
//! `/account` or the help overlay on their own.

use crate::state::{App, SetupProgress};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

/// Widest the panel grows; wider terminals centre it instead of stretching
/// the lines across the screen.
const MAX_WIDTH: u16 = 72;

/// Column the step labels are padded to so the key hints align: the longest
/// label plus a two-space gap.
const LABEL_WIDTH: usize = 45;

/// One checklist row: what to do and the keys that do it.
struct Step {
    label: &'static str,
    keys: &'static str,
    done: bool,
}

/// The checklist rows for `progress`. Step 1's hint depends on whether an
/// account already exists without a key (set its key with `c`) or none
/// exists at all (create one with `n`, which then asks for the key).
fn steps(progress: SetupProgress, has_accounts: bool) -> [Step; 3] {
    [
        Step {
            label: "Add an AI provider account and its API key",
            keys: if has_accounts && !progress.has_keyed_account {
                "alt+a, select it, c"
            } else {
                "alt+a, then n"
            },
            done: progress.has_keyed_account,
        },
        Step {
            label: "Use that account in this session",
            keys: "alt+a, select it, Enter",
            done: progress.session_has_account,
        },
        Step {
            label: "Choose a model",
            keys: "alt+m",
            done: progress.session_has_model,
        },
    ]
}

/// Build the panel's lines. Split from the drawing so tests can check the
/// content without a terminal.
pub(super) fn welcome_lines(progress: SetupProgress, has_accounts: bool) -> Vec<Line<'static>> {
    let title = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let done_style = Style::default().fg(Color::Green);
    let next_style = Style::default().add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(Color::Cyan);

    let mut lines = vec![
        Line::from(Span::styled("Welcome to Choreographr", title)),
        Line::default(),
    ];

    if progress.is_complete() {
        lines.push(Line::from(Span::styled(
            "You're all set. Type a message below and press Enter to start.",
            done_style,
        )));
    } else {
        lines.push(Line::from("Before your first message, finish these steps:"));
        lines.push(Line::default());
        // Only the first unfinished step is highlighted: the steps depend on
        // each other in order, so that is the one to do now.
        let mut next_marked = false;
        for (index, step) in steps(progress, has_accounts).into_iter().enumerate() {
            let number = index + 1;
            let line = if step.done {
                Line::from(vec![
                    Span::styled(format!("  ✓ {number}. "), done_style),
                    Span::styled(step.label, dim),
                ])
            } else {
                let is_next = !next_marked;
                next_marked = true;
                let (marker, label_style) = if is_next {
                    ("→", next_style)
                } else {
                    ("○", dim)
                };
                // Pad the label so every step's keys line up in one column.
                Line::from(vec![
                    Span::styled(format!("  {marker} {number}. "), label_style),
                    Span::styled(format!("{:<LABEL_WIDTH$}", step.label), label_style),
                    Span::styled(step.keys, key_style),
                ])
            };
            lines.push(line);
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Adding a key selects the account and opens the model list for you.",
            dim,
        )));
    }

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("alt+h", key_style),
        Span::styled(" shows every shortcut · type ", dim),
        Span::styled("/", key_style),
        Span::styled(" to browse commands", dim),
    ]));
    lines
}

/// Draw the welcome panel centred in the (empty) history area.
pub(super) fn render_welcome(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = welcome_lines(app.setup_progress(), !app.ai_providers.accounts.is_empty());
    let width = area.width.min(MAX_WIDTH);
    // The line count is a handful of fixed rows, so the cast cannot truncate.
    #[expect(clippy::cast_possible_truncation)]
    let height = (lines.len() as u16).min(area.height);
    let panel = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false }),
        panel,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn fresh_setup_points_at_adding_an_account() {
        let out = text(&welcome_lines(SetupProgress::default(), false));
        assert!(out.contains("→ 1. Add an AI provider account"));
        assert!(out.contains("alt+a, then n"));
        assert!(out.contains("○ 2."));
        assert!(out.contains("○ 3."));
    }

    #[test]
    fn keyless_account_points_at_setting_its_key() {
        let out = text(&welcome_lines(SetupProgress::default(), true));
        assert!(out.contains("alt+a, select it, c"));
    }

    #[test]
    fn finished_steps_are_ticked_and_the_next_one_is_marked() {
        let progress = SetupProgress {
            has_keyed_account: true,
            session_has_account: true,
            session_has_model: false,
        };
        let out = text(&welcome_lines(progress, true));
        assert!(out.contains("✓ 1."));
        assert!(out.contains("✓ 2."));
        assert!(out.contains("→ 3. Choose a model"));
    }

    #[test]
    fn complete_setup_says_ready() {
        let progress = SetupProgress {
            has_keyed_account: true,
            session_has_account: true,
            session_has_model: true,
        };
        let out = text(&welcome_lines(progress, true));
        assert!(out.contains("You're all set"));
        assert!(!out.contains("1."));
    }
}
