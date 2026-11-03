//! The welcome panel drawn in place of an empty chat history: a short setup
//! checklist (add an account with its API key, use it in this session, pick a
//! model) that ticks itself off as the daemon confirms each step, followed by
//! a "ready" line once nothing is left. It is the first thing a new user sees,
//! so it names the exact keys for each step rather than leaving them to find
//! `/account` or the help overlay on their own.
//!
//! When the terminal or its host layer costs this client a feature it uses,
//! the panel also carries the advice derived in `state/onboarding.rs`. That
//! block sits between the checklist and the closing `alt+h` hint, and is
//! degraded explicitly when the area is too short for it — see
//! [`append_notice`].

use crate::state::{App, TerminalNotice};
use choreo_client_core::SetupProgress;
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

/// The panel's title style.
fn title_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Secondary prose: unfinished steps and explanatory lines.
fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// A step that is already done.
fn done_style() -> Style {
    Style::default().fg(Color::Green)
}

/// The next step to do.
fn next_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Key chords, so every `alt+` hint reads as a key rather than prose.
fn key_style() -> Style {
    Style::default().fg(Color::Cyan)
}

/// The terminal notice's lead line: warning-coloured because it names
/// something the user is currently losing, not a neutral hint.
fn notice_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// The checklist rows for `progress`. Step 1's hint depends on whether an
/// account already exists without a key (set its key with `c`) or none
/// exists at all (create one with `n`, which then asks for the key).
fn steps(progress: SetupProgress, has_accounts: bool) -> [Step; 3] {
    [
        Step {
            label: "Add an AI provider account and its API key or sign in",
            keys: if has_accounts && !progress.has_keyed_account {
                "alt+a, select it, c (key) or /login <account>"
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

/// Build the panel's lines for an area `max_height` rows tall. Split from the
/// drawing so tests can check the content without a terminal.
///
/// `notice` is the launch-time terminal advice; it is appended after the
/// checklist and before the closing hint, truncated by [`append_notice`] when
/// the area cannot hold it.
pub(super) fn welcome_lines(
    progress: SetupProgress,
    has_accounts: bool,
    notice: Option<&TerminalNotice>,
    max_height: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Welcome to Choreographr", title_style())),
        Line::default(),
    ];

    if progress.is_complete() {
        lines.push(Line::from(Span::styled(
            "You're all set. Type a message below and press Enter to start.",
            done_style(),
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
                    Span::styled(format!("  ✓ {number}. "), done_style()),
                    Span::styled(step.label, dim_style()),
                ])
            } else {
                let is_next = !next_marked;
                next_marked = true;
                let (marker, label_style) = if is_next {
                    ("→", next_style())
                } else {
                    ("○", dim_style())
                };
                // Pad the label so every step's keys line up in one column.
                Line::from(vec![
                    Span::styled(format!("  {marker} {number}. "), label_style),
                    Span::styled(format!("{:<LABEL_WIDTH$}", step.label), label_style),
                    Span::styled(step.keys, key_style()),
                ])
            };
            lines.push(line);
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Adding a key selects the account and opens the model list for you.",
            dim_style(),
        )));
    }

    // The closing hint is reserved *before* the notice is sized: the panel is
    // drawn top-down and clamped to the area, so without reserving these rows
    // the notice would eat the hint line.
    let hint = [
        Line::default(),
        Line::from(vec![
            Span::styled("alt+h", key_style()),
            Span::styled(" shows every shortcut · type ", dim_style()),
            Span::styled("/", key_style()),
            Span::styled(" to browse commands", dim_style()),
        ]),
    ];

    if let Some(notice) = notice {
        append_notice(&mut lines, notice, hint.len(), max_height);
    }
    lines.extend(hint);
    lines
}

/// Append as much of `notice` as the area has room for, after `reserved` rows
/// kept for what follows it.
///
/// The paragraph widget is clamped to the panel rectangle and would silently
/// cut the tail, leaving half a recommendation on screen. Degrading here
/// instead keeps the block honest: the whole notice when it fits, then the
/// headline alone, then nothing at all.
fn append_notice(
    lines: &mut Vec<Line<'static>>,
    notice: &TerminalNotice,
    reserved: usize,
    max_height: u16,
) {
    let free = usize::from(max_height).saturating_sub(lines.len() + reserved);
    let mut block = Vec::with_capacity(notice.lines.len() + 2);
    // The blank line is part of the block: it separates the notice from the
    // checklist above it, so a headline-only degradation takes both.
    block.push(Line::default());
    block.push(Line::from(Span::styled(
        notice.headline.clone(),
        notice_style(),
    )));
    block.extend(
        notice
            .lines
            .iter()
            .map(|text| Line::from(Span::styled(text.clone(), dim_style()))),
    );

    if block.len() <= free {
        lines.extend(block);
    } else if free >= 2 {
        // Room for the separator and the headline, but not the advice under it.
        lines.extend(block.into_iter().take(2));
    }
}

/// Draw the welcome panel centred in the (empty) history area.
pub(super) fn render_welcome(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = welcome_lines(
        app.setup_progress(),
        !app.ai_providers.accounts.is_empty(),
        app.terminal_advice(),
        area.height,
    );
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

    /// A height no test panel runs out of, so the notice is never truncated.
    const TALL: u16 = 100;

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

    /// A two-line notice whose lines are distinctive enough to assert on.
    fn sample_notice() -> TerminalNotice {
        TerminalNotice {
            headline: "NOTICE-HEADLINE".to_string(),
            lines: vec!["NOTICE-LINE-1".to_string(), "NOTICE-LINE-2".to_string()],
        }
    }

    /// Rows the checklist occupies before the notice and the closing hint.
    ///
    /// The panel's fixed hint is two rows (a blank and the `alt+h` line), so
    /// the no-notice line count minus those is the budget the notice competes
    /// for.
    fn base_len() -> u16 {
        let height = welcome_lines(SetupProgress::default(), false, None, TALL).len();
        u16::try_from(height.saturating_sub(2)).unwrap_or(u16::MAX)
    }

    #[test]
    fn fresh_setup_points_at_adding_an_account() {
        let out = text(&welcome_lines(SetupProgress::default(), false, None, TALL));
        assert!(out.contains("→ 1. Add an AI provider account"));
        assert!(out.contains("alt+a, then n"));
        assert!(out.contains("○ 2."));
        assert!(out.contains("○ 3."));
    }

    #[test]
    fn keyless_account_points_at_setting_its_key() {
        let out = text(&welcome_lines(SetupProgress::default(), true, None, TALL));
        assert!(out.contains("alt+a, select it, c"));
    }

    #[test]
    fn finished_steps_are_ticked_and_the_next_one_is_marked() {
        let progress = SetupProgress {
            has_keyed_account: true,
            session_has_account: true,
            session_has_model: false,
        };
        let out = text(&welcome_lines(progress, true, None, TALL));
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
        let out = text(&welcome_lines(progress, true, None, TALL));
        assert!(out.contains("You're all set"));
        assert!(!out.contains("1."));
    }

    #[test]
    fn a_tall_panel_shows_the_whole_notice_before_the_hint() {
        let notice = sample_notice();
        let lines = welcome_lines(SetupProgress::default(), false, Some(&notice), TALL);
        let out = text(&lines);
        assert!(out.contains("NOTICE-HEADLINE"));
        assert!(out.contains("NOTICE-LINE-1"));
        assert!(out.contains("NOTICE-LINE-2"));
        // The closing hint stays last: the notice never displaces it.
        assert!(out.trim_end().ends_with("to browse commands"));
    }

    #[test]
    fn a_short_panel_degrades_to_the_headline_only() {
        let notice = sample_notice();
        // One row short of the whole block (separator, headline, two lines).
        let height = base_len() + 2 + 4 - 1;
        let out = text(&welcome_lines(
            SetupProgress::default(),
            false,
            Some(&notice),
            height,
        ));
        assert!(out.contains("NOTICE-HEADLINE"));
        assert!(!out.contains("NOTICE-LINE-1"));
        assert!(!out.contains("NOTICE-LINE-2"));
    }

    #[test]
    fn a_panel_with_no_room_drops_the_notice_entirely() {
        let notice = sample_notice();
        // Exactly the checklist and the hint, with no row left for a notice.
        let height = base_len() + 2;
        let out = text(&welcome_lines(
            SetupProgress::default(),
            false,
            Some(&notice),
            height,
        ));
        assert!(!out.contains("NOTICE"));
        // The checklist the panel exists for is untouched.
        assert!(out.contains("Welcome to Choreographr"));
    }
}
