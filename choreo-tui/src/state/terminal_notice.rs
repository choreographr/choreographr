//! The welcome panel's terminal advice: the headline-plus-lines notice shown
//! when the detected terminal or its host layer costs this client a feature it
//! actually uses.
//!
//! It is a thin arrangement over the shared advice tables in
//! [`super::terminal_advice`] — the per-platform recommendations, the tmux
//! lines, the macOS Option-as-Meta settings, and the phrases naming each
//! capability gap. The Terminal page prints the same tables, so this module only
//! lays them out in the welcome panel's shape and can never disagree with the
//! page about what to recommend or configure.
//!
//! Derivation is a pure function of an immutable [`TerminalProfile`], computed
//! once at launch (`run_app`) and stored on [`App`]; the render path
//! (`render/welcome.rs`) reads the stored [`TerminalNotice`] rather than
//! re-deriving it. The first-run setup glue that shares this module's former
//! home lives in `state/onboarding.rs`.

use super::App;
use super::terminal_advice::{
    gap_phrases, hosted_pane_sentence, join_phrases, option_key_setting, recommendations,
    screen_sentence, tmux_lines,
};
use crate::terminal::profile::{Host, Mux, Platform, TerminalKind, TerminalProfile, Version};

/// What the welcome panel says about the terminal the client is running in.
///
/// One bold lead line naming the situation, then the short lines that follow
/// from it — a recommendation list, the tmux settings, or a single sentence.
/// Kept as plain strings so the renderer owns all styling and the derivation
/// owns only the copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalNotice {
    /// The lead line, drawn emphasised.
    pub(crate) headline: String,
    /// The supporting lines, drawn dim beneath the headline.
    pub(crate) lines: Vec<String>,
}

impl App {
    /// The launch-time terminal advice, or `None` when the terminal and its
    /// host cost this client nothing.
    ///
    /// Named for the advice it returns rather than for the notice field it
    /// prints from, so a read of the field cannot be mistaken for a call.
    pub(crate) fn terminal_advice(&self) -> Option<&TerminalNotice> {
        self.terminal_notice.as_ref()
    }
}

/// Derive the welcome panel's terminal advice from an immutable profile.
///
/// Pure, so every rule in the advice matrix is unit-testable from a synthetic
/// [`TerminalProfile`]. The result is computed once at launch (the profile can
/// never change afterwards) and stored on [`App`].
///
/// Advice is withheld rather than guessed: an unidentified terminal asserts no
/// capability, so it never produces a capability notice, and Android gets no
/// recommendation at all.
pub(crate) fn terminal_notice(profile: &TerminalProfile) -> Option<TerminalNotice> {
    let host = match profile.host {
        Host::Direct => direct_notice(profile),
        Host::Mux(Mux::Tmux { version }) => Some(tmux_notice(version)),
        // Zellij already forwards every feature the client uses, so there is
        // nothing to configure and nothing to warn about.
        Host::Mux(Mux::Zellij) => None,
        Host::Mux(Mux::Screen) => Some(TerminalNotice {
            headline: "GNU screen blocks these features".to_string(),
            // The sentence is the shared one the Terminal page prints, so the two
            // surfaces cannot disagree about a screen pane.
            lines: vec![screen_sentence().to_string()],
        }),
        Host::Hosted(_) => Some(TerminalNotice {
            headline: "This pane cannot forward everything the client uses".to_string(),
            lines: vec![hosted_pane_sentence(profile.platform)],
        }),
    };

    // The second block is the macOS Option-as-Meta setting, which
    // `option_key_setting` withholds under a hosted layer (the outer terminal
    // cannot be named there) and on every non-macOS platform.
    let option_key = option_key_setting(profile).map(macos_option_notice);

    match (host, option_key) {
        (None, None) => None,
        (Some(notice), None) | (None, Some(notice)) => Some(notice),
        (Some(mut notice), Some(option_key)) => {
            // A blank line separates the two subjects so the panel reads as two
            // short paragraphs instead of one run-on list.
            notice.lines.push(String::new());
            notice.lines.push(option_key.headline);
            notice.lines.extend(option_key.lines);
            Some(notice)
        }
    }
}

/// The advice for a terminal talking straight to its emulator: name what the
/// client loses, then the terminals on this platform that do not lose it.
fn direct_notice(profile: &TerminalProfile) -> Option<TerminalNotice> {
    // Android has no terminal worth recommending — the user has ruled out
    // suggesting another one — so the whole notice is suppressed there rather
    // than naming a gap with no cure.
    if profile.platform == Platform::Android {
        return None;
    }
    let gaps = gap_phrases(profile.capabilities());
    if gaps.is_empty() {
        return None;
    }
    // A known terminal is named; an unidentified one is addressed neutrally so
    // the sentence still reads without inventing an identity. The shared
    // `display_name` is the single source of truth for the spelling, with only
    // the sentence-initial case handled here.
    let subject = if profile.name == TerminalKind::Unknown {
        "This terminal".to_string()
    } else {
        profile.name.display_name().to_string()
    };
    let headline = format!("{subject} — {} won't work", join_phrases(&gaps));
    // The recommendations are already the display lines the Terminal page
    // prints, so the panel and the page name the same terminals and installers.
    let mut lines = vec!["A terminal that supports it:".to_string()];
    lines.extend(recommendations(profile.platform));
    Some(TerminalNotice { headline, lines })
}

/// The tmux configuration lines that let the client's features through a pane.
fn tmux_notice(version: Option<Version>) -> TerminalNotice {
    TerminalNotice {
        headline: "tmux must be told to forward what this client uses".to_string(),
        // The lines (plus the frame-sync floor when the running tmux is known
        // to be too old) come from the shared advice table the Terminal page
        // reads, so the two surfaces cannot drift.
        lines: tmux_lines(version)
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// The macOS Option-as-Meta advice, carrying the setting for the detected
/// terminal (see `terminal_advice::option_key_setting`).
fn macos_option_notice(setting: &'static str) -> TerminalNotice {
    TerminalNotice {
        headline: "Option+key shortcuts never arrive on macOS".to_string(),
        lines: vec![
            "macOS composes a character for Option+key before the app sees it, so `alt+` \
             shortcuts only arrive once the terminal sends Option as Meta."
                .to_string(),
            setting.to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::profile::{HostedKind, ImageSupport, NativeImages};

    /// A synthetic profile with the fields the notice reads: a known (or
    /// unknown) terminal, a platform, and the host layer.
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

    /// Every line of a notice, headline first, for substring assertions.
    fn all_text(notice: &TerminalNotice) -> String {
        std::iter::once(&notice.headline)
            .chain(notice.lines.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn fresh_app_has_no_terminal_notice() {
        assert!(App::new().terminal_advice().is_none());
    }

    #[test]
    fn a_direct_gapped_terminal_names_the_gap_and_the_platform_cures() {
        let notice = terminal_notice(&profile(
            TerminalKind::AppleTerminal,
            Platform::MacOs,
            Host::Direct,
        ))
        .expect("Terminal.app on macOS has gaps that warrant advice");
        assert!(notice.headline.contains("Terminal.app"));
        assert!(notice.headline.contains("copy-on-select"));
        assert!(notice.headline.contains("inline images"));
        let text = all_text(&notice);
        assert!(text.contains("brew install --cask ghostty"));
        assert!(text.contains("brew install --cask iterm2"));
    }

    #[test]
    fn a_direct_terminal_with_nothing_missing_gets_no_notice() {
        // Ghostty asserts every capability the notice looks at, on a platform
        // that appends no Option-as-Alt line.
        assert_eq!(
            terminal_notice(&profile(
                TerminalKind::Ghostty,
                Platform::Linux,
                Host::Direct
            )),
            None
        );
    }

    #[test]
    fn tmux_gets_its_own_settings_and_a_version_floor() {
        let old = terminal_notice(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Mux(Mux::Tmux {
                version: Some(Version::new(3, 5, 0)),
            }),
        ))
        .expect("tmux always has settings to name");
        let text = all_text(&old);
        assert!(text.contains("set -g extended-keys on"));
        assert!(text.contains("set -g allow-passthrough on"));
        assert!(text.contains("tmux 3.7"));

        let current = terminal_notice(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Mux(Mux::Tmux {
                version: Some(Version::new(3, 8, 0)),
            }),
        ))
        .expect("tmux always has settings to name");
        assert!(!all_text(&current).contains("tmux 3.7"));

        // An unknown version is told nothing rather than told it is too old.
        let unknown = terminal_notice(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Mux(Mux::Tmux { version: None }),
        ))
        .expect("tmux always has settings to name");
        assert!(!all_text(&unknown).contains("tmux 3.7"));
    }

    #[test]
    fn zellij_gets_no_notice() {
        assert_eq!(
            terminal_notice(&profile(
                TerminalKind::Unknown,
                Platform::Linux,
                Host::Mux(Mux::Zellij)
            )),
            None
        );
    }

    #[test]
    fn screen_says_it_cannot_forward_and_names_the_alternative() {
        let notice = terminal_notice(&profile(
            TerminalKind::Unknown,
            Platform::Linux,
            Host::Mux(Mux::Screen),
        ))
        .expect("screen warrants one line");
        assert_eq!(notice.lines.len(), 1);
        let text = all_text(&notice);
        assert!(text.contains("does not forward"));
        assert!(text.contains("tmux or zellij"));
    }

    #[test]
    fn a_hosted_pane_points_at_running_in_a_terminal_directly() {
        let notice = terminal_notice(&profile(
            TerminalKind::Unknown,
            Platform::MacOs,
            Host::Hosted(HostedKind::Herdr),
        ))
        .expect("a hosted pane is told what to do instead");
        // One line only, and no Option-as-Alt advice: we cannot name the outer
        // terminal under a workspace manager.
        assert_eq!(notice.lines.len(), 1);
        assert!(notice.lines[0].contains("directly in a terminal"));
        assert!(!all_text(&notice).contains("Option"));
    }

    #[test]
    fn an_unidentified_terminal_gets_no_notice() {
        // `Unknown` asserts nothing, so no capability can be read as missing.
        assert_eq!(
            terminal_notice(&profile(
                TerminalKind::Unknown,
                Platform::Linux,
                Host::Direct
            )),
            None
        );
        // Same for a terminal whose whole row is unasserted (Ptyxis).
        assert_eq!(
            terminal_notice(&profile(
                TerminalKind::Ptyxis,
                Platform::Linux,
                Host::Direct
            )),
            None
        );
    }

    #[test]
    fn android_gets_no_recommendation() {
        // Termux lacks the keyboard protocol, mode 2026 and a modern image
        // protocol, so a gap exists — but Android offers no alternative, so the
        // notice is suppressed rather than naming a gap with no cure.
        assert_eq!(
            terminal_notice(&profile(
                TerminalKind::Termux,
                Platform::Android,
                Host::Direct
            )),
            None
        );
    }

    #[test]
    fn a_capability_that_is_not_asserted_never_produces_a_notice() {
        // Every capability `None` (the unidentified row): nothing may be read
        // as unsupported, so no notice.
        let caps = profile(TerminalKind::Unknown, Platform::Linux, Host::Direct).capabilities();
        assert_eq!(caps.clipboard_write, None);
        assert_eq!(caps.synchronized_output, None);
        assert_eq!(caps.kitty_keyboard, None);
        assert_eq!(caps.images, NativeImages::Unknown);
        assert_eq!(gap_phrases(caps), Vec::<&str>::new());
    }

    #[test]
    fn each_macos_terminal_gets_its_own_option_setting_and_no_other() {
        let settings = [
            (TerminalKind::Ghostty, "macos-option-as-alt = true"),
            (TerminalKind::Kitty, "macos_option_as_alt left"),
            (TerminalKind::Alacritty, "option_as_alt = \"Both\""),
            (
                TerminalKind::WezTerm,
                "send_composed_key_when_left_alt_is_pressed = false",
            ),
            (TerminalKind::ItTerm, "Left/Right Option Key = Esc+"),
            (TerminalKind::AppleTerminal, "Use Option as Meta key"),
        ];
        for (kind, needle) in settings {
            let notice = terminal_notice(&profile(kind, Platform::MacOs, Host::Direct))
                .unwrap_or_else(|| panic!("{kind:?} on macOS should get the Option advice"));
            let text = all_text(&notice);
            assert!(text.contains(needle), "{kind:?} should name {needle:?}");
            assert!(text.contains("Option+key"));
            for (other_kind, other_needle) in settings {
                if other_kind != kind {
                    assert!(
                        !text.contains(other_needle),
                        "{kind:?} must not carry {other_kind:?}'s setting {other_needle:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_unidentified_macos_terminal_gets_the_generic_option_sentence() {
        let notice = terminal_notice(&profile(
            TerminalKind::Unknown,
            Platform::MacOs,
            Host::Direct,
        ))
        .expect("macOS always composes Option characters");
        let text = all_text(&notice);
        assert!(text.contains("Option as Meta"));
        // No setting is named, because we do not know which terminal to name.
        assert!(!text.contains("~/.config"));
        assert!(!text.contains("Profiles →"));
    }

    #[test]
    fn the_option_line_is_suppressed_under_a_hosted_layer() {
        assert_eq!(
            terminal_notice(&profile(
                TerminalKind::Unknown,
                Platform::MacOs,
                Host::Hosted(HostedKind::Rex)
            ))
            .map(|notice| all_text(&notice).contains("Option")),
            Some(false)
        );
    }

    #[test]
    fn a_gapped_macos_terminal_keeps_both_subjects() {
        // Terminal.app on macOS both loses features and composes Option keys;
        // the panel carries the gap advice, then the Option advice under it.
        let notice = terminal_notice(&profile(
            TerminalKind::AppleTerminal,
            Platform::MacOs,
            Host::Direct,
        ))
        .expect("both subjects apply");
        let text = all_text(&notice);
        assert!(text.contains("Terminal.app —"));
        assert!(text.contains("Use Option as Meta key"));
    }
}
