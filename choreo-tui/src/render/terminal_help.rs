//! The Terminal page: a read-only modal summarising what the client
//! negotiated with the terminal it launched in, and how to close any gap.
//!
//! Opened with `Alt+T` or `/terminal` (`App::show_terminal_page`), drawn over
//! the Chat page with the same centered `LIST` popup and bordered block as the
//! model selector. Everything it shows is read from the immutable launch-time
//! profile on `App` — this page performs **no capability detection of its own**
//! — and the advice it prints comes from the shared tables in
//! `state/terminal_advice.rs`, so it can never disagree with the welcome panel
//! about which terminal to recommend or which setting to change.
//!
//! The line builder ([`terminal_page_lines`]) is a pure function over
//! `&TerminalProfile`, so the whole page is unit-testable without a terminal,
//! exactly like `welcome_lines`.

use crate::state::{
    HOSTED_PANE_SENTENCE, PopupSize, centered_popup, gap_phrases, join_phrases, option_key_setting,
    recommendations, tmux_lines,
};
use crate::terminal::profile::{
    Capabilities, Host, Mux, NativeImages, TerminalKind, TerminalProfile,
};
use ratatui::{
    Frame,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// Column the capability labels are padded to so every verdict lines up: the
/// longest label plus a two-space gap.
const LABEL_WIDTH: usize = 34;

/// A section heading inside the page.
fn heading_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Secondary prose: explanations and hints.
fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// A capability the terminal is known to support.
fn supported_style() -> Style {
    Style::default().fg(Color::Green)
}

/// A gap worth acting on (the list of features this terminal costs us).
fn gap_style() -> Style {
    Style::default().fg(Color::Yellow)
}

/// One capability row: the label a user reads plus its three-state answer.
#[derive(Clone, Copy)]
struct CapabilityRow {
    label: &'static str,
    support: Option<bool>,
}

/// The seven rows the checklist always shows, in a fixed order.
///
/// The state is the capability table's own three-state answer, so an
/// unasserted capability stays distinct from a settled "no" (see
/// `terminal/profile.rs`).
fn capability_rows(capabilities: Capabilities) -> [CapabilityRow; 7] {
    [
        CapabilityRow {
            label: "kitty keyboard protocol",
            support: capabilities.kitty_keyboard,
        },
        CapabilityRow {
            label: "synchronized output (mode 2026)",
            support: capabilities.synchronized_output,
        },
        CapabilityRow {
            label: "OSC 52 clipboard write",
            support: capabilities.clipboard_write,
        },
        CapabilityRow {
            label: "OSC 9;4 progress bar",
            support: capabilities.progress,
        },
        CapabilityRow {
            label: "OSC 7501 program status",
            support: capabilities.program_status,
        },
        CapabilityRow {
            label: "OSC 22 pointer shape",
            support: capabilities.pointer_shape,
        },
        CapabilityRow {
            label: "inline images",
            support: native_image_support(capabilities.images),
        },
    ]
}

/// Fold the capability table's image answer into the checklist's three states:
/// only a settled "no graphics at all" is a gap, and an unasserted row stays
/// unknown (Windows Terminal cannot answer the picker's query, though it can
/// draw images).
fn native_image_support(images: NativeImages) -> Option<bool> {
    match images {
        NativeImages::None => Some(false),
        NativeImages::Unknown => None,
        NativeImages::Halfblocks
        | NativeImages::Sixel
        | NativeImages::Kitty
        | NativeImages::Iterm2 => Some(true),
    }
}

/// The layer hosting this program, as a person reads it.
fn host_label(host: Host) -> String {
    match host {
        Host::Direct => "direct".to_string(),
        Host::Mux(mux) => mux.to_string(),
        Host::Hosted(kind) => format!("{kind} workspace"),
    }
}

/// The identity line: terminal, version, platform, negotiated image protocol
/// and host layer. The version is omitted when the environment carried none
/// rather than printed as a second "unknown".
fn identity_line(profile: &TerminalProfile) -> String {
    let terminal = match (profile.name, profile.version) {
        (TerminalKind::Unknown, _) => "unknown terminal".to_string(),
        (name, Some(version)) => format!("{name} {version}"),
        (name, None) => name.to_string(),
    };
    format!(
        "{terminal} · {} · images: {} · host: {}",
        profile.platform,
        profile.images,
        host_label(profile.host),
    )
}

/// One checklist row: a glyph and word for the verdict, the label padded so
/// the verdicts align, then the word itself. An unasserted capability prints
/// `?` and "unknown" — never a cross, which would claim a settled "no".
fn capability_line(row: CapabilityRow) -> Line<'static> {
    let (glyph, verdict, style) = match row.support {
        Some(true) => ("✓", "supported", supported_style()),
        Some(false) => ("✗", "not supported", dim_style()),
        None => ("?", "unknown", dim_style()),
    };
    Line::from(vec![
        Span::styled(format!("  {glyph} "), style),
        Span::styled(format!("{:<LABEL_WIDTH$}", row.label), Style::default()),
        Span::styled(verdict, style),
    ])
}

/// The recommendation block: what is missing (if anything) and the terminals
/// on this platform that do not lose it.
///
/// The platform's recommendation list is printed only when a gap actually
/// exists — recommending another terminal to a user whose terminal already
/// supports everything the client uses would be noise. Android offers no
/// alternative at all, so a gap there names itself and stops.
fn recommendation_lines(
    profile: &TerminalProfile,
    capabilities: Capabilities,
) -> Vec<Line<'static>> {
    // A hosted pane has no terminal of its own to recommend, and the fixes
    // block already explains why. Claiming health or a gap here would either
    // contradict that or invent a terminal the user cannot choose.
    if matches!(profile.host, Host::Hosted(_)) {
        return Vec::new();
    }
    let mut lines = vec![Line::from(Span::styled("recommendations", heading_style()))];
    let gaps = gap_phrases(capabilities);
    if gaps.is_empty() {
        // "Nothing is missing" is only true when something was actually
        // asserted. A pane that reported nothing (an unidentified terminal in a
        // multiplexer, for instance) knows no more than its own silence, so it
        // must not be congratulated.
        let (verbatim, style) = if any_capability_asserted(capabilities) {
            (
                "nothing this client uses is missing — no change needed",
                supported_style(),
            )
        } else {
            (
                "this pane did not report what its terminal supports — nothing to recommend",
                dim_style(),
            )
        };
        lines.push(Line::from(Span::styled(verbatim, style)));
        return lines;
    }
    lines.push(Line::from(Span::styled(
        format!("this terminal costs you: {}", join_phrases(&gaps)),
        gap_style(),
    )));
    let picks = recommendations(profile.platform);
    if picks.is_empty() {
        lines.push(Line::from(Span::styled(
            "no alternative terminal is offered on this platform",
            dim_style(),
        )));
        return lines;
    }
    lines.push(Line::from(Span::styled(
        "a terminal that supports it:",
        dim_style(),
    )));
    lines.extend(
        picks
            .iter()
            .map(|pick| Line::from(Span::styled(format!("  {pick}"), Style::default()))),
    );
    lines
}

/// Whether the profile asserted *any* of the capabilities the client's advice
/// is built on, as opposed to reporting nothing at all.
fn any_capability_asserted(capabilities: Capabilities) -> bool {
    capabilities.clipboard_write.is_some()
        || capabilities.synchronized_output.is_some()
        || capabilities.kitty_keyboard.is_some()
        || !matches!(capabilities.images, NativeImages::Unknown)
}

/// The fixes block: the copy-pasteable setting for each gap the profile
/// explains — the macOS Option-as-Meta setting, the tmux forwarding lines, and
/// the one sentence a hosted pane gets instead of any of them.
fn fix_lines(profile: &TerminalProfile) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("fixes", heading_style()))];
    let mut any = false;
    if let Some(setting) = option_key_setting(profile) {
        lines.push(Line::from(Span::styled(
            "option as alt (macOS) — macOS composes Option+key before the app sees it:",
            dim_style(),
        )));
        lines.push(Line::from(Span::styled(
            format!("  {setting}"),
            Style::default(),
        )));
        any = true;
    }
    if let Host::Mux(Mux::Tmux { version }) = profile.host {
        lines.push(Line::from(Span::styled(
            "tmux — put these in ~/.tmux.conf:",
            dim_style(),
        )));
        lines.extend(
            tmux_lines(version)
                .into_iter()
                .map(|line| Line::from(Span::styled(format!("  {line}"), Style::default()))),
        );
        any = true;
    }
    if matches!(profile.host, Host::Hosted(_)) {
        // A hosted pane has no terminal advice to give: the layer above owns
        // the pane's screen model, so saying so is the whole fix block.
        lines.push(Line::from(Span::styled(HOSTED_PANE_SENTENCE, dim_style())));
        any = true;
    }
    if !any {
        lines.push(Line::from(Span::styled(
            "nothing to configure for this host",
            dim_style(),
        )));
    }
    lines
}

/// Build the page's lines from the launch-time profile.
///
/// Split from the drawing so tests can assert on the content without a
/// terminal. Sections, top to bottom: the identity line, the seven-row
/// capability checklist, the recommendations, the copy-pasteable fixes, and
/// the closing key hint.
pub(super) fn terminal_page_lines(profile: &TerminalProfile) -> Vec<Line<'static>> {
    let capabilities = profile.capabilities();
    let mut lines = vec![Line::from(Span::styled(
        identity_line(profile),
        Style::default(),
    ))];
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("capabilities", heading_style())));
    lines.extend(
        capability_rows(capabilities)
            .into_iter()
            .map(capability_line),
    );
    lines.push(Line::default());
    let recommendations = recommendation_lines(profile, capabilities);
    if !recommendations.is_empty() {
        lines.extend(recommendations);
        lines.push(Line::default());
    }
    lines.extend(fix_lines(profile));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Esc or Alt+T to close",
        dim_style(),
    )));
    lines
}

/// Draw the Terminal page centered over the Chat page.
pub(super) fn render_terminal_help(frame: &mut Frame<'_>, app: &crate::state::App) {
    let popup = centered_popup(frame.area(), PopupSize::LIST);
    // Erase the region beneath so the page reads as a solid overlay rather
    // than text drawn on top of the chat history (same as the model selector).
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Terminal ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // Wrapped, because a recommendation or a settings line is longer than the
    // popup on a narrow terminal; the page has no scrollable body, so it
    // simply clips at the bottom when the area is too short for it.
    frame.render_widget(
        Paragraph::new(terminal_page_lines(&app.terminal)).wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::profile::{HostedKind, ImageSupport, Platform, Version};

    /// A synthetic profile; `version` is `None` unless a test sets one.
    fn profile(name: TerminalKind, platform: Platform, host: Host) -> TerminalProfile {
        TerminalProfile {
            name,
            version: None,
            platform,
            images: ImageSupport::Halfblocks,
            host,
            evidence: Vec::new(),
        }
    }

    /// Every line of the page, newline-joined, for substring assertions.
    fn text(profile: &TerminalProfile) -> String {
        terminal_page_lines(profile)
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
    fn identity_line_names_the_terminal_platform_images_and_host() {
        let ghostty = TerminalProfile {
            version: Some(Version::new(1, 4, 0)),
            images: ImageSupport::Kitty,
            ..profile(TerminalKind::Ghostty, Platform::Linux, Host::Direct)
        };
        let out = text(&ghostty);
        assert!(
            out.starts_with("ghostty 1.4.0 · linux · images: kitty · host: direct"),
            "unexpected identity line:\n{out}"
        );

        // A host layer is reported by name (and its version where known).
        let tmux = text(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Mux(Mux::Tmux {
                version: Some(Version::new(3, 5, 0)),
            }),
        ));
        assert!(
            tmux.contains("host: tmux 3.5.0"),
            "unexpected line:\n{tmux}"
        );

        // A workspace manager is generic: it is not the terminal.
        let hosted = text(&profile(
            TerminalKind::Unknown,
            Platform::MacOs,
            Host::Hosted(HostedKind::Herdr),
        ));
        assert!(hosted.contains("host: herdr workspace"));
    }

    #[test]
    fn every_capability_row_states_a_verdict() {
        // Ghostty asserts every row, so each of the seven prints "supported".
        let ghostty = TerminalProfile {
            version: Some(Version::new(1, 4, 0)),
            ..profile(TerminalKind::Ghostty, Platform::Linux, Host::Direct)
        };
        let out = text(&ghostty);
        for label in [
            "kitty keyboard protocol",
            "synchronized output (mode 2026)",
            "OSC 52 clipboard write",
            "OSC 9;4 progress bar",
            "OSC 7501 program status",
            "OSC 22 pointer shape",
            "inline images",
        ] {
            assert!(out.contains(label), "missing row {label:?}");
        }
        assert_eq!(
            out.matches("supported").count(),
            7,
            "every Ghostty row is supported:\n{out}"
        );
    }

    #[test]
    fn an_unasserted_capability_reads_as_unknown_not_unsupported() {
        // An unidentified terminal asserts nothing: every row must read
        // "unknown", and none may be drawn as a cross or as a settled no.
        let caps = profile(TerminalKind::Unknown, Platform::Linux, Host::Direct).capabilities();
        for row in capability_rows(caps) {
            let support = row.support;
            let label = row.label;
            assert_eq!(support, None, "{label} must stay unasserted");
            let drawn = capability_line(row)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(drawn.contains("unknown"), "row reads {drawn:?}");
            assert!(
                !drawn.contains('✗') && !drawn.contains("not supported"),
                "an unasserted capability must never render as a cross: {drawn:?}"
            );
        }
    }

    #[test]
    fn a_settled_no_reads_as_not_supported() {
        // Terminal.app settles every row as unsupported.
        let out = text(&profile(
            TerminalKind::AppleTerminal,
            Platform::MacOs,
            Host::Direct,
        ));
        assert_eq!(out.matches('✗').count(), 7, "rows:\n{out}");
        assert_eq!(out.matches("not supported").count(), 7, "rows:\n{out}");
    }

    #[test]
    fn recommendations_are_filtered_by_platform_and_only_for_a_real_gap() {
        // A gapped macOS terminal is pointed at the macOS installs only.
        let macos = text(&profile(
            TerminalKind::AppleTerminal,
            Platform::MacOs,
            Host::Direct,
        ));
        assert!(macos.contains("brew install --cask ghostty"));
        assert!(!macos.contains("apt install kitty"));
        assert!(macos.contains("copy-on-select"), "names what it costs");

        // The same terminal on Linux gets the Linux list and nothing else.
        let linux = text(&profile(
            TerminalKind::AppleTerminal,
            Platform::Linux,
            Host::Direct,
        ));
        assert!(linux.contains("apt install kitty"));
        assert!(!linux.contains("brew install"));

        // Nothing missing: no terminal is recommended at all.
        let ghostty = text(&profile(
            TerminalKind::Ghostty,
            Platform::Linux,
            Host::Direct,
        ));
        assert!(ghostty.contains("nothing this client uses is missing"));
        assert!(!ghostty.contains("ghostty.org"));
    }

    #[test]
    fn android_names_its_gap_without_offering_an_alternative() {
        let out = text(&profile(
            TerminalKind::Termux,
            Platform::Android,
            Host::Direct,
        ));
        assert!(out.contains("this terminal costs you:"));
        assert!(out.contains("no alternative terminal is offered"));
    }

    #[test]
    fn tmux_lines_are_present_for_a_tmux_host_and_absent_for_a_direct_one() {
        let tmux = text(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Mux(Mux::Tmux {
                version: Some(Version::new(3, 5, 0)),
            }),
        ));
        assert!(tmux.contains("set -g extended-keys on"));
        assert!(tmux.contains("set -g allow-passthrough on"));
        // The frame-sync floor rides along for an old, known tmux.
        assert!(tmux.contains("tmux 3.7"));

        let direct = text(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Direct,
        ));
        assert!(!direct.contains("extended-keys"));
        assert!(!direct.contains("allow-passthrough"));
    }

    #[test]
    fn a_hosted_pane_gets_the_sentence_instead_of_terminal_advice() {
        let out = text(&profile(
            TerminalKind::Unknown,
            Platform::MacOs,
            Host::Hosted(HostedKind::Herdr),
        ));
        assert!(out.contains("workspace manager owns this pane"));
        // No macOS Option advice: the outer terminal cannot be identified, and
        // no tmux lines: the pane is not a multiplexer's.
        assert!(!out.contains("Option as Meta"));
        assert!(!out.contains("extended-keys"));
    }

    #[test]
    fn the_macos_fix_names_the_detected_terminals_own_setting() {
        let out = text(&profile(TerminalKind::Kitty, Platform::MacOs, Host::Direct));
        assert!(out.contains("macos_option_as_alt left"));

        // Nothing to configure on a Linux direct session with no tmux.
        let linux = text(&profile(
            TerminalKind::Ghostty,
            Platform::Linux,
            Host::Direct,
        ));
        assert!(linux.contains("nothing to configure"));
    }

    #[test]
    fn the_page_always_ends_with_the_closing_hint() {
        let lines = terminal_page_lines(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Direct,
        ));
        let last = lines.last().map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        });
        assert_eq!(last.as_deref(), Some("Esc or Alt+T to close"));
    }

    #[test]
    fn a_hosted_pane_offers_no_recommendations_at_all() {
        // The fixes block explains the hosted pane; a recommendations block
        // could only contradict it or invent a terminal the user cannot pick.
        let out = text(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Hosted(HostedKind::Herdr),
        ));
        assert!(!out.contains("recommendations"));
        assert!(!out.contains("nothing this client uses is missing"));
        assert!(out.contains(HOSTED_PANE_SENTENCE));
    }

    #[test]
    fn a_pane_that_asserted_nothing_is_not_called_healthy() {
        // An unidentified terminal under a multiplexer asserts no capability:
        // the page must say so rather than congratulate it.
        let out = text(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Mux(Mux::Zellij),
        ));
        assert!(out.contains("did not report what its terminal supports"));
        assert!(!out.contains("nothing this client uses is missing"));
    }

    #[test]
    fn a_negotiated_image_protocol_shows_as_supported_without_an_identity() {
        // A hosted pane reaches us with no terminal identity, but the picker
        // still negotiated the protocol: that measurement is the one fact the
        // page can report.
        let mut hosted = profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Hosted(HostedKind::Herdr),
        );
        hosted.images = ImageSupport::Kitty;
        let out = text(&hosted);
        assert!(out.contains("inline images"));
        assert!(out.contains("supported"));
        assert!(out.contains("unknown terminal"));
    }
}
