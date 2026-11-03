//! The terminal advice matrix, shared by the welcome panel's notice
//! (`state/onboarding.rs`) and the Terminal page (`render/terminal_help.rs`).
//!
//! It lives in `state/` rather than `render/` because it is **policy, not
//! styling**: which terminal to recommend on a platform, which setting turns
//! Option into Meta, which tmux lines let the client's features through a pane,
//! and which user-facing phrase names what a capability gap costs. Two surfaces
//! read these tables, so keeping them here is what stops the welcome panel and
//! the Terminal page from ever disagreeing about the advice they print — and
//! the renderer stays a pure view over data it does not own.
//!
//! Everything here is a pure function of a [`TerminalProfile`] (or of one of
//! its fields), so each rule is unit-testable without a terminal.

use crate::terminal::profile::{
    Capabilities, Host, NativeImages, Platform, TerminalKind, TerminalProfile, Version,
    table_capabilities,
};

/// tmux gained synchronized output (what the client's atomic frames need) in
/// 3.7; below it the advice says so rather than leaving a user to guess why
/// redraws tear.
const TMUX_FRAME_SYNC: Version = Version::new(3, 7, 0);

/// One terminal a platform offers: how it is identified, the name a person
/// reads, and how to install it there.
///
/// One row per terminal, so the short user-facing name and the install command
/// cannot disagree about which terminals a platform has — the recommendation
/// list and the "keeps every feature" filter are both derived from this table
/// rather than hand-kept beside it.
struct Recommendation {
    kind: TerminalKind,
    name: &'static str,
    install: &'static str,
}

impl Recommendation {
    /// The line the Terminal page prints: `<name> — <install>`.
    fn line(&self) -> String {
        format!("{} — {}", self.name, self.install)
    }
}

/// The terminals macOS offers, in recommendation order.
const MACOS_TERMINALS: &[Recommendation] = &[
    Recommendation {
        kind: TerminalKind::Ghostty,
        name: "Ghostty",
        install: "brew install --cask ghostty",
    },
    Recommendation {
        kind: TerminalKind::ItTerm,
        name: "iTerm2",
        install: "brew install --cask iterm2",
    },
];

/// The terminals Linux offers, in recommendation order.
const LINUX_TERMINALS: &[Recommendation] = &[
    Recommendation {
        kind: TerminalKind::Ghostty,
        name: "Ghostty",
        install: "install from your distribution or ghostty.org",
    },
    Recommendation {
        kind: TerminalKind::Kitty,
        name: "kitty",
        install: "apt install kitty (or dnf install kitty)",
    },
    Recommendation {
        kind: TerminalKind::WezTerm,
        name: "WezTerm",
        install: "install from your distribution or wezterm.org",
    },
];

/// The terminals Windows offers, in recommendation order.
const WINDOWS_TERMINALS: &[Recommendation] = &[
    Recommendation {
        kind: TerminalKind::WindowsTerminal,
        name: "Windows Terminal",
        install: "winget install Microsoft.WindowsTerminal",
    },
    Recommendation {
        kind: TerminalKind::WezTerm,
        name: "WezTerm",
        install: "winget install wez.wezterm",
    },
];

/// The terminals a platform offers.
///
/// Android is empty by design: the platform's terminal is the Android app the
/// user already chose, and no alternative is offered.
fn platform_terminals(platform: Platform) -> &'static [Recommendation] {
    match platform {
        Platform::MacOs => MACOS_TERMINALS,
        Platform::Linux => LINUX_TERMINALS,
        Platform::Windows => WINDOWS_TERMINALS,
        Platform::Android => &[],
    }
}

/// The terminals on `platform` that cost the client nothing: every feature its
/// own advice tracks is *asserted* supported, so nothing is lost and nothing has
/// to be configured.
///
/// A terminal whose image support is merely unverified is left out rather than
/// promised (a Windows console drops the picker's query, so its images are not
/// asserted even though it can draw them).
pub(crate) fn full_support_terminals(platform: Platform) -> Vec<&'static str> {
    platform_terminals(platform)
        .iter()
        .filter(|recommendation| loses_nothing(recommendation.kind))
        .map(|recommendation| recommendation.name)
        .collect()
}

/// Whether `kind` asserts every feature the client's advice tracks.
///
/// The table's own answer, with no negotiated protocol and no host layer folded
/// in: a recommendation has to rest on what the terminal's documentation
/// settles, not on what this one launch happened to negotiate.
fn loses_nothing(kind: TerminalKind) -> bool {
    let capabilities = table_capabilities(kind, None);
    capabilities.clipboard_write == Some(true)
        && capabilities.synchronized_output == Some(true)
        && capabilities.kitty_keyboard == Some(true)
        && !matches!(
            capabilities.images,
            NativeImages::None | NativeImages::Unknown
        )
}

/// Join a list of alternatives: "A", "A or B", "A, B or C".
///
/// A choice is not a union, so the conjunction is *or*, and the serial comma is
/// omitted — the list reads as one sentence rather than as an enumeration. (The
/// shared [`join_phrases`] joins cumulative lists — "costs you: A, B, and C" —
/// which is why this is a separate helper rather than a flag on it.)
fn join_choices(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// The sentence a hosted pane gets in place of terminal advice.
///
/// The layer above us owns the pane's screen model and forwards what it
/// chooses, so the fix is to run the client in a terminal directly — and only a
/// terminal that costs the client nothing will do, so the platform's ones are
/// named rather than left as "your terminal".
pub(crate) fn hosted_pane_sentence(platform: Platform) -> String {
    let full = full_support_terminals(platform);
    if full.is_empty() {
        // Nothing on this platform is verified to keep every feature, so the
        // advice names none rather than promising one.
        return "Run choreo-tui directly in a terminal for full functionality.".to_string();
    }
    format!(
        "Run choreo-tui directly in a terminal that supports every feature: {}.",
        join_choices(&full)
    )
}

/// The one sentence a GNU screen pane gets in place of terminal advice.
///
/// Screen does not forward the sequences the client relies on through its
/// pane, so the fix is to run inside a multiplexer that does. Both the welcome
/// notice and the Terminal page print this, so screen advice lives with the
/// rest of the shared tables rather than in a single surface.
pub(crate) fn screen_sentence() -> &'static str {
    "Screen does not forward keys, images, or clipboard through its pane; run this inside tmux or zellij instead."
}

/// The terminals worth recommending on `platform`, best first, as the lines the
/// Terminal page prints.
///
/// Android yields nothing by design: the platform's terminal is the Android app
/// the user already chose, and no alternative is offered.
pub(crate) fn recommendations(platform: Platform) -> Vec<String> {
    platform_terminals(platform)
        .iter()
        .map(Recommendation::line)
        .collect()
}

/// The user-facing names of what the client loses when each capability is
/// asserted unsupported.
///
/// Only the capabilities whose loss is visible in the client's own features:
/// copy-on-select, flicker-free frame redraws, the disambiguated modified keys
/// (Shift+Enter and friends), and inline images. The reporting extras — the
/// OSC 9;4 progress bar, OSC 7501 program status, OSC 22 pointer shape — are
/// silent when unsupported, so their absence never warrants a recommendation.
pub(crate) fn gap_phrases(capabilities: Capabilities) -> Vec<&'static str> {
    let mut gaps = Vec::new();
    if capabilities.clipboard_write == Some(false) {
        gaps.push("copy-on-select");
    }
    if capabilities.synchronized_output == Some(false) {
        gaps.push("flicker-free redraws");
    }
    if capabilities.kitty_keyboard == Some(false) {
        gaps.push("Shift+Enter and other modified keys");
    }
    if capabilities.images == NativeImages::None {
        gaps.push("inline images");
    }
    gaps
}

/// The tmux settings that let the client's features through a pane, plus the
/// frame-sync version floor when the running tmux is known to be older.
///
/// Only a version we actually read can rule frame synchronization out; an
/// unknown tmux version is told nothing rather than told it is too old.
pub(crate) fn tmux_lines(version: Option<Version>) -> Vec<&'static str> {
    let mut lines = vec!["set -g extended-keys on", "set -g allow-passthrough on"];
    if version.is_some_and(|version| !version.at_least(TMUX_FRAME_SYNC)) {
        lines.push("Frame synchronization needs tmux 3.7 or newer.");
    }
    lines
}

/// The macOS Option-as-Meta setting for the detected terminal, or `None` where
/// that advice does not apply.
///
/// Option+key composes a character on macOS before any application sees it, so
/// an `alt+` shortcut never arrives unless the terminal is told to send Option
/// as Meta. The advice is about the emulator's own key handling, which a
/// multiplexer passes through but a workspace manager's pane does not: under a
/// hosted layer the outer terminal cannot be named, so it is withheld there.
pub(crate) fn option_key_setting(profile: &TerminalProfile) -> Option<&'static str> {
    match profile.host {
        Host::Hosted(_) => None,
        Host::Direct | Host::Mux(_) if profile.platform == Platform::MacOs => {
            Some(macos_option_setting(profile.name))
        }
        Host::Direct | Host::Mux(_) => None,
    }
}

/// The setting that makes `kind` send Option as Meta.
///
/// The sentence names the exact key and file for the terminal we identified;
/// an unidentified terminal (or one of the remaining rows) gets the generic
/// sentence, because naming a setting we cannot be sure applies would be worse
/// than naming none.
fn macos_option_setting(kind: TerminalKind) -> &'static str {
    match kind {
        TerminalKind::Ghostty => {
            "Ghostty: set `macos-option-as-alt = true` in ~/.config/ghostty/config (the \
             U.S. Standard and U.S. International layouts already default to on)"
        }
        TerminalKind::Kitty => {
            "kitty: set `macos_option_as_alt left` in ~/.config/kitty/kitty.conf"
        }
        TerminalKind::Alacritty => {
            "Alacritty: set `option_as_alt = \"Both\"` under [window] in \
             ~/.config/alacritty/alacritty.toml"
        }
        TerminalKind::WezTerm => {
            "WezTerm: set `send_composed_key_when_left_alt_is_pressed = false` in ~/.wezterm.lua"
        }
        TerminalKind::ItTerm => "iTerm2: Profiles → Keys → Left/Right Option Key = Esc+",
        TerminalKind::AppleTerminal => "Terminal.app: Profiles → Keyboard → Use Option as Meta key",
        TerminalKind::WindowsTerminal
        | TerminalKind::Konsole
        | TerminalKind::Ptyxis
        | TerminalKind::Vte
        | TerminalKind::Foot
        | TerminalKind::Termux
        | TerminalKind::Rex
        | TerminalKind::Unknown => {
            "Set your terminal to send Option as Meta (Alt); the setting is in its \
             keyboard or profile preferences"
        }
    }
}

/// Join phrases into one English list: `a`, `a and b`, `a, b, and c`.
pub(crate) fn join_phrases(phrases: &[&str]) -> String {
    match phrases {
        [] => String::new(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::profile::{Mux, Version};

    fn profile(name: TerminalKind, platform: Platform, host: Host) -> TerminalProfile {
        TerminalProfile {
            name,
            version: None,
            platform,
            images: crate::terminal::profile::ImageSupport::Halfblocks,
            host,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn the_recommendation_table_is_one_source_of_truth() {
        // The recommendation lines and the "keeps every feature" names are both
        // derived from the platform's table, so they cannot disagree about which
        // terminals a platform has; this pins that the derivation still holds.
        for platform in [Platform::MacOs, Platform::Linux, Platform::Windows] {
            let table = platform_terminals(platform);
            assert!(!table.is_empty(), "{platform} must offer something");
            let lines = recommendations(platform);
            assert_eq!(lines.len(), table.len(), "{platform}");
            for (line, row) in lines.iter().zip(table) {
                assert!(line.starts_with(row.name), "{line:?} names {:?}", row.name);
                assert!(!row.install.is_empty(), "{} has no installer", row.name);
                assert!(line.contains(row.install), "{line:?}");
            }
            // Every terminal promised as "keeps every feature" is one of the
            // platform's own rows.
            for name in full_support_terminals(platform) {
                assert!(table.iter().any(|row| row.name == name), "{name:?}");
            }
        }
        // Android offers nothing at all.
        assert!(platform_terminals(Platform::Android).is_empty());
        assert_eq!(recommendations(Platform::Android), Vec::<String>::new());
    }

    #[test]
    fn each_platform_keeps_its_own_recommendations() {
        assert!(
            recommendations(Platform::MacOs)
                .iter()
                .any(|r| r.contains("iterm2"))
        );
        assert!(
            recommendations(Platform::Linux)
                .iter()
                .any(|r| r.contains("apt install kitty"))
        );
        assert!(
            recommendations(Platform::Windows)
                .iter()
                .any(|r| r.contains("winget install wez.wezterm"))
        );
        assert!(
            recommendations(Platform::Android).is_empty(),
            "Android offers no alternative terminal"
        );
        // No platform recommends another platform's installer.
        assert!(
            !recommendations(Platform::Linux)
                .iter()
                .any(|r| r.contains("brew install"))
        );
    }

    #[test]
    fn gap_phrases_name_only_the_asserted_gaps() {
        let caps =
            profile(TerminalKind::AppleTerminal, Platform::MacOs, Host::Direct).capabilities();
        let gaps = gap_phrases(caps);
        assert!(gaps.contains(&"copy-on-select"));
        assert!(gaps.contains(&"inline images"));
        // The reporting extras are silent when unsupported.
        assert!(!gaps.iter().any(|gap| gap.contains("progress")));

        // Nothing asserted (unidentified terminal) is no gap at all.
        assert_eq!(
            gap_phrases(
                profile(TerminalKind::Unknown, Platform::Linux, Host::Direct).capabilities()
            ),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn tmux_lines_carry_the_frame_sync_floor_only_for_a_known_old_version() {
        let old = tmux_lines(Some(Version::new(3, 5, 0)));
        assert!(old.contains(&"set -g extended-keys on"));
        assert!(old.contains(&"set -g allow-passthrough on"));
        assert!(old.iter().any(|line| line.contains("tmux 3.7")));

        assert!(
            !tmux_lines(Some(Version::new(3, 8, 0)))
                .iter()
                .any(|line| line.contains("tmux 3.7"))
        );
        assert!(
            !tmux_lines(None)
                .iter()
                .any(|line| line.contains("tmux 3.7"))
        );
    }

    #[test]
    fn the_option_setting_applies_only_on_macos_outside_a_hosted_pane() {
        assert!(
            option_key_setting(&profile(
                TerminalKind::Ghostty,
                Platform::MacOs,
                Host::Direct
            ))
            .is_some_and(|setting| setting.contains("macos-option-as-alt"))
        );
        // A multiplexer passes the emulator's own key handling through, so the
        // advice still applies inside one.
        assert!(
            option_key_setting(&profile(
                TerminalKind::Kitty,
                Platform::MacOs,
                Host::Mux(Mux::Tmux { version: None })
            ))
            .is_some_and(|setting| setting.contains("macos_option_as_alt"))
        );
        // A hosted pane, and every non-macOS platform, get nothing.
        assert_eq!(
            option_key_setting(&profile(
                TerminalKind::Unknown,
                Platform::MacOs,
                Host::Hosted(crate::terminal::profile::HostedKind::Rex)
            )),
            None
        );
        assert_eq!(
            option_key_setting(&profile(
                TerminalKind::Ghostty,
                Platform::Linux,
                Host::Direct
            )),
            None
        );
    }

    #[test]
    fn an_unidentified_macos_terminal_gets_the_generic_setting() {
        let setting = option_key_setting(&profile(
            TerminalKind::Unknown,
            Platform::MacOs,
            Host::Direct,
        ))
        .expect("macOS always composes Option characters");
        assert!(setting.contains("Option as Meta"));
        assert!(!setting.contains("~/.config"));
    }

    #[test]
    fn join_phrases_reads_as_english() {
        assert_eq!(join_phrases(&[]), "");
        assert_eq!(join_phrases(&["a"]), "a");
        assert_eq!(join_phrases(&["a", "b"]), "a and b");
        assert_eq!(join_phrases(&["a", "b", "c"]), "a, b, and c");
    }

    #[test]
    fn join_choices_offers_alternatives() {
        assert_eq!(join_choices(&[]), "");
        assert_eq!(join_choices(&["a"]), "a");
        assert_eq!(join_choices(&["a", "b"]), "a or b");
        // No serial comma: the list is a choice, not an enumeration.
        assert_eq!(join_choices(&["a", "b", "c"]), "a, b or c");
    }

    #[test]
    fn full_support_terminals_are_the_platforms_flawless_ones() {
        assert_eq!(
            full_support_terminals(Platform::Linux),
            vec!["Ghostty", "kitty", "WezTerm"]
        );
        assert_eq!(
            full_support_terminals(Platform::MacOs),
            vec!["Ghostty", "iTerm2"]
        );
        // A console's images are unverified (it drops the picker's query), so it
        // is not promised; WezTerm is.
        assert_eq!(full_support_terminals(Platform::Windows), vec!["WezTerm"]);
        assert_eq!(
            full_support_terminals(Platform::Android),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn the_screen_sentence_names_the_alternative_not_a_setting() {
        let sentence = screen_sentence();
        assert!(sentence.contains("does not forward"));
        assert!(sentence.contains("tmux or zellij"));
    }

    #[test]
    fn the_hosted_sentence_names_terminals_in_sentence_case() {
        assert_eq!(
            hosted_pane_sentence(Platform::Linux),
            "Run choreo-tui directly in a terminal that supports every feature: \
             Ghostty, kitty or WezTerm."
        );
        assert_eq!(
            hosted_pane_sentence(Platform::MacOs),
            "Run choreo-tui directly in a terminal that supports every feature: Ghostty or iTerm2."
        );
        assert_eq!(
            hosted_pane_sentence(Platform::Windows),
            "Run choreo-tui directly in a terminal that supports every feature: WezTerm."
        );
        // Nothing on Android is verified to keep every feature, so no terminal
        // is named rather than promising one.
        assert_eq!(
            hosted_pane_sentence(Platform::Android),
            "Run choreo-tui directly in a terminal for full functionality."
        );
    }
}
