//! The welcome panel drawn in place of an empty chat history: the first thing a
//! user sees on a fresh session, so it names the one thing to do next and the
//! exact key for it, in a single plain sentence per setup state. Once nothing is
//! left to set up it says so and invites the first message.
//!
//! The panel stands in for a transcript that is known to be empty, not for one
//! that has not arrived: [`crate::state::App::show_welcome`] requires the
//! session's snapshot to have been applied.  An attach in flight therefore
//! draws an empty history area with `loading` in the status bar, and a failed
//! attach draws the same area with `error` there, rather than inviting a
//! first message into a session whose state is still unknown.
//!
//! When the terminal or its host layer costs this client a feature it uses, the
//! panel also carries the advice derived in `state/terminal_notice.rs`. That
//! block sits between the sentence and the closing `alt+h` hint, and is
//! degraded explicitly when the area is too short for it — see
//! [`append_notice`].

use super::style::{dim_style, heading_style};
use crate::state::{App, TerminalNotice, alt_chord_label};
use crate::terminal::profile::Platform;
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
///
/// The value is the width of the longest sentence the panel can print — the
/// "no provider yet" line with the accounts chord spelled `Alt+A`, the widest
/// of the platform spellings (`⌥A` on macOS) — so the copy and this constant
/// move together. A sentence that no longer fits fails the width guard in the
/// tests below by name, instead of word-wrapping into a ragged second row.
const MAX_WIDTH: u16 = 75;

/// The sentence that invites the first message once nothing is left to set up.
const READY: &str = "You're all set. Type a message below and press Enter to start.";

/// A finished setup.
fn done_style() -> Style {
    Style::default().fg(Color::Green)
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

/// The panel's one sentence of guidance for `progress`: what is left to do, and
/// the chord that does it, spelled for `platform`.
///
/// `has_accounts` is what separates "nothing is set up yet" (create an account,
/// which then asks for its key) from "an account exists but holds no key"
/// (select it and set the key). A missing key is named before a missing session
/// account for a reason: an account must hold a key before a session can use
/// it, so the key is the thing to fix first.
///
/// The attached session's model is no longer a reachable gap — an account is
/// committed to a session together with its model and reasoning effort — so it
/// produces no branch here. A keyed account whose session carries a model but
/// no account is therefore unreachable too; it falls through to the ready
/// sentence rather than naming a state the user cannot be in.
fn guidance_line(progress: SetupProgress, has_accounts: bool, platform: Platform) -> Line<'static> {
    if progress.is_complete() {
        return Line::from(Span::styled(READY, done_style()));
    }
    if !progress.has_keyed_account {
        let (before, after) = if has_accounts {
            (
                "An account still needs its API key. Press ",
                ", select it, then c.",
            )
        } else {
            (
                "No AI provider is set up yet. Press ",
                " to add one and paste its API key.",
            )
        };
        return Line::from(vec![
            Span::raw(before),
            Span::styled(alt_chord_label(platform, "A"), key_style()),
            Span::raw(after),
        ]);
    }
    if !progress.session_has_account {
        return Line::from(vec![
            Span::raw("This session has no account. Press "),
            Span::styled(alt_chord_label(platform, "A"), key_style()),
            Span::raw(", then Enter on one to start it up."),
        ]);
    }
    Line::from(Span::styled(READY, done_style()))
}

/// Build the panel's lines for a panel `width` columns wide and `max_height`
/// rows tall. Split from the drawing so tests can check the content without a
/// terminal.
///
/// `platform` decides how the `Alt` modifier is spelled in the two chords the
/// panel prints, and is injected rather than read from
/// [`Platform::current`] so both spellings are testable.
///
/// `notice` is the launch-time terminal advice; it is appended after the
/// guidance sentence and before the closing hint, truncated by [`append_notice`]
/// when the area cannot hold it.
pub(super) fn welcome_lines(
    progress: SetupProgress,
    has_accounts: bool,
    platform: Platform,
    notice: Option<&TerminalNotice>,
    width: u16,
    max_height: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Welcome to Choreographr", heading_style())),
        Line::default(),
        guidance_line(progress, has_accounts, platform),
    ];

    // The closing hint is reserved *before* the notice is sized: the panel is
    // drawn top-down and clamped to the area, so without reserving these rows
    // the notice would eat the hint line.
    let hint = [
        Line::default(),
        Line::from(vec![
            Span::styled(alt_chord_label(platform, "H"), key_style()),
            Span::styled(" shows every shortcut · type ", dim_style()),
            Span::styled("/", key_style()),
            Span::styled(" to browse commands", dim_style()),
        ]),
    ];

    if let Some(notice) = notice {
        let reserved = lines_rows(&hint, usize::from(width));
        append_notice(&mut lines, notice, reserved, width, max_height);
    }
    lines.extend(hint);
    lines
}

/// The terminal rows `lines` occupy when drawn in the panel's wrapping
/// `Paragraph` at `width` columns.
///
/// The panel is drawn with wrapping on, so one logical line can cost several
/// rows. Budgeting by line count would under-count a long advice line and let
/// the paragraph widget clip it mid-sentence — the exact failure the degradation
/// below exists to avoid. Measuring with the same wrapper the renderer uses
/// ([`crate::state::wrapped_lines_rows`]) also stops a word-wrapped row from
/// being under-charged by a `div_ceil` approximation.
fn lines_rows(lines: &[Line<'_>], width: usize) -> usize {
    // Widths arrive as `usize::from(u16)` (the panel is at most `u16`-wide); the
    // fallback only guards the impossible overflow and keeps the panel bounded.
    let Ok(width) = u16::try_from(width) else {
        return lines.len();
    };
    usize::from(crate::state::wrapped_lines_rows(lines, width))
}

/// Append as much of `notice` as the area has room for, after `reserved` rows
/// kept for what follows it.
///
/// The paragraph widget is clamped to the panel rectangle and would silently
/// cut the tail, leaving half a recommendation on screen. Degrading here
/// instead keeps the block honest: the whole notice when it fits, then the
/// headline alone, then nothing at all. Room is measured in *drawn* rows, so a
/// notice line that wraps is charged for every row it takes.
fn append_notice(
    lines: &mut Vec<Line<'static>>,
    notice: &TerminalNotice,
    reserved: usize,
    width: u16,
    max_height: u16,
) {
    let width = usize::from(width);
    let free = usize::from(max_height)
        .saturating_sub(lines_rows(lines, width))
        .saturating_sub(reserved);
    let mut block = Vec::with_capacity(notice.lines.len() + 2);
    // The blank line is part of the block: it separates the notice from the
    // guidance sentence above it, so a headline-only degradation takes both.
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

    // The separator and the headline are what a degraded block keeps; the
    // advice lines under them are the part that gets dropped.
    let headline_rows = block
        .get(..2)
        .map_or(usize::MAX, |head| lines_rows(head, width));
    if lines_rows(&block, width) <= free {
        lines.extend(block);
    } else if headline_rows <= free {
        lines.extend(block.into_iter().take(2));
    }
}

/// Draw the welcome panel centred in the (empty) history area.
pub(super) fn render_welcome(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width.min(MAX_WIDTH);
    let lines = welcome_lines(
        app.setup_progress(),
        !app.ai_providers.accounts.is_empty(),
        // The detected platform, so the panel's chords match the platform the
        // Terminal page prints rather than the compile target.
        app.platform(),
        app.terminal_advice(),
        width,
        area.height,
    );
    // The panel's height is the rows its lines *draw*, not how many lines it
    // has: a wrapped line occupies several, and sizing by the line count would
    // leave the panel shorter than its own content. Both terms are bounded by
    // the area, so the conversion cannot truncate.
    let drawn = lines_rows(&lines, usize::from(width));
    let height = u16::try_from(drawn).unwrap_or(area.height).min(area.height);
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
    use crate::markdown_render::display_width;

    /// A height no test panel runs out of, so the notice is never truncated.
    const TALL: u16 = 100;

    /// A width wide enough that no panel line wraps, so a row count equals the
    /// line count in the tests that reason in lines.
    const WIDE: u16 = 200;

    /// The platform whose chord spelling is the widest (`Alt+A` rather than
    /// `⌥A`), used by the tests that are about the copy rather than the
    /// platform.
    const LINUX: Platform = Platform::Linux;

    /// One line's text, styles discarded.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    /// A two-line notice whose lines are distinctive enough to assert on.
    fn sample_notice() -> TerminalNotice {
        TerminalNotice {
            headline: "NOTICE-HEADLINE".to_string(),
            lines: vec!["NOTICE-LINE-1".to_string(), "NOTICE-LINE-2".to_string()],
        }
    }

    /// Every state the panel can be in: a name for failure messages, the setup
    /// progress, and whether any account exists at all.
    fn states() -> [(&'static str, SetupProgress, bool); 5] {
        let unkeyed = SetupProgress::default();
        let sessionless = SetupProgress {
            has_keyed_account: true,
            session_has_account: false,
            session_has_model: false,
        };
        [
            (
                "setup complete",
                SetupProgress {
                    has_keyed_account: true,
                    session_has_account: true,
                    session_has_model: true,
                },
                true,
            ),
            ("no provider at all", unkeyed, false),
            ("an account with no key", unkeyed, true),
            (
                "a keyed account the session does not use",
                sessionless,
                true,
            ),
            // Unreachable (an account is committed with its model), but the
            // panel still has to render something sensible for it.
            (
                "a keyed account whose session has a model but no account",
                SetupProgress {
                    has_keyed_account: true,
                    session_has_account: false,
                    session_has_model: true,
                },
                true,
            ),
        ]
    }

    /// Rows the panel's own copy occupies before the notice and the closing
    /// hint.
    ///
    /// The closing hint is a blank plus the chord line, so the no-notice row
    /// count minus those is the budget the notice competes for.
    fn base_len() -> u16 {
        let rows = lines_rows(
            &welcome_lines(SetupProgress::default(), false, LINUX, None, WIDE, TALL),
            usize::from(WIDE),
        );
        u16::try_from(rows.saturating_sub(2)).unwrap_or(u16::MAX)
    }

    #[test]
    fn every_panel_line_fits_the_panel_width() {
        // The panel draws its lines at `MAX_WIDTH` without wrapping them; a
        // longer line would wrap raggedly and push its tail onto rows the
        // layout did not budget. Widths are charged in display columns, so a
        // wide glyph (macOS's `⌥`) costs what it draws.
        //
        // The notice is short here on purpose: the real advice lines are long
        // prose that the panel wraps and budgets by drawn rows (see
        // `append_notice`), so holding them to one line would be wrong. What
        // this guard covers is the panel's own copy — the heading, the guidance
        // sentence and the closing hint — under each state.
        for platform in [Platform::MacOs, Platform::Linux] {
            for (state, progress, has_accounts) in states() {
                for notice in [None, Some(sample_notice())] {
                    let lines = welcome_lines(
                        progress,
                        has_accounts,
                        platform,
                        notice.as_ref(),
                        MAX_WIDTH,
                        TALL,
                    );
                    let (widest, line) = lines
                        .iter()
                        .map(|line| {
                            let text = line_text(line);
                            (display_width(&text), text)
                        })
                        .max_by_key(|(width, _)| *width)
                        .expect("the panel always draws at least its heading");
                    assert!(
                        widest <= usize::from(MAX_WIDTH),
                        "{state} on {platform} draws a {widest}-column line, over the \
                         {MAX_WIDTH}-column panel: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_fresh_setup_names_adding_a_provider_and_its_key() {
        let out = text(&welcome_lines(
            SetupProgress::default(),
            false,
            LINUX,
            None,
            WIDE,
            TALL,
        ));
        assert!(out.contains(
            "No AI provider is set up yet. Press Alt+A to add one and paste its API key."
        ));
    }

    #[test]
    fn a_keyless_account_names_setting_its_key() {
        let out = text(&welcome_lines(
            SetupProgress::default(),
            true,
            LINUX,
            None,
            WIDE,
            TALL,
        ));
        assert!(
            out.contains("An account still needs its API key. Press Alt+A, select it, then c.")
        );
    }

    #[test]
    fn a_keyed_account_the_session_does_not_use_names_selecting_it() {
        let progress = SetupProgress {
            has_keyed_account: true,
            session_has_account: false,
            session_has_model: false,
        };
        let out = text(&welcome_lines(progress, true, LINUX, None, WIDE, TALL));
        assert!(out.contains(
            "This session has no account. Press Alt+A, then Enter on one to start it up."
        ));
    }

    #[test]
    fn complete_setup_says_ready_and_names_no_chord() {
        let progress = SetupProgress {
            has_keyed_account: true,
            session_has_account: true,
            session_has_model: true,
        };
        let out = text(&welcome_lines(progress, true, LINUX, None, WIDE, TALL));
        assert!(out.contains(READY));
        assert!(!out.contains("Alt+A"));
    }

    #[test]
    fn the_chords_are_spelled_for_the_platform() {
        // Both chords in the panel — the accounts shortcut in the guidance
        // sentence and the help shortcut in the closing hint — go through the
        // shared spelling, so a Mac carries neither `Alt+` label.
        let mac = text(&welcome_lines(
            SetupProgress::default(),
            false,
            Platform::MacOs,
            None,
            WIDE,
            TALL,
        ));
        assert!(mac.contains("Press ⌥A to add one"), "{mac}");
        assert!(mac.contains("⌥H shows every shortcut"), "{mac}");
        assert!(!mac.contains("Alt+"), "{mac}");

        let linux = text(&welcome_lines(
            SetupProgress::default(),
            false,
            Platform::Linux,
            None,
            WIDE,
            TALL,
        ));
        assert!(linux.contains("Press Alt+A to add one"), "{linux}");
        assert!(linux.contains("Alt+H shows every shortcut"), "{linux}");
    }

    #[test]
    fn a_tall_panel_shows_the_whole_notice_before_the_hint() {
        let notice = sample_notice();
        let lines = welcome_lines(
            SetupProgress::default(),
            false,
            LINUX,
            Some(&notice),
            WIDE,
            TALL,
        );
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
            LINUX,
            Some(&notice),
            WIDE,
            height,
        ));
        assert!(out.contains("NOTICE-HEADLINE"));
        assert!(!out.contains("NOTICE-LINE-1"));
        assert!(!out.contains("NOTICE-LINE-2"));
    }

    #[test]
    fn a_wrapping_notice_line_is_budgeted_as_the_rows_it_draws() {
        // A narrow panel wraps both the panel's own copy and the advice, so the
        // budget has to be counted in drawn rows. Under a line count this
        // four-line block would look like it fits and then be clipped
        // mid-sentence by the paragraph widget.
        let width = 20;
        let notice = TerminalNotice {
            headline: "H".to_string(),
            lines: vec!["x".repeat(2 * usize::from(width))],
        };
        let base = lines_rows(
            &welcome_lines(SetupProgress::default(), false, LINUX, None, width, TALL),
            usize::from(width),
        );
        // Two free rows: the separator and the headline, nothing else.
        let height = u16::try_from(base + 2).expect("the test panel fits a u16");
        let out = text(&welcome_lines(
            SetupProgress::default(),
            false,
            LINUX,
            Some(&notice),
            width,
            height,
        ));
        assert!(out.contains('H'), "the headline still fits:\n{out}");
        assert!(
            !out.contains('x'),
            "a two-row advice line cannot fit:\n{out}"
        );
    }

    #[test]
    fn a_panel_with_no_room_drops_the_notice_entirely() {
        let notice = sample_notice();
        // Exactly the panel's own copy and the hint, with no row left for a
        // notice.
        let height = base_len() + 2;
        let out = text(&welcome_lines(
            SetupProgress::default(),
            false,
            LINUX,
            Some(&notice),
            WIDE,
            height,
        ));
        assert!(!out.contains("NOTICE"));
        // The guidance the panel exists for is untouched.
        assert!(out.contains("Welcome to Choreographr"));
    }
}
