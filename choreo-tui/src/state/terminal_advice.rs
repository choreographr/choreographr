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
};

/// tmux gained synchronized output (what the client's atomic frames need) in
/// 3.7; below it the advice says so rather than leaving a user to guess why
/// redraws tear.
const TMUX_FRAME_SYNC: Version = Version::new(3, 7, 0);

/// The line a hosted pane gets in place of terminal advice: the layer above us
/// owns the pane's screen model, so the terminal is not ours to recommend.
pub(crate) const HOSTED_PANE_SENTENCE: &str = "a workspace manager owns this pane and keeps its own \
     screen model, so the terminal is not ours to recommend.";

/// The terminals worth recommending on `platform`, best first.
///
/// Android is empty by design: the platform's terminal is the Android app the
/// user already chose, and no alternative is offered.
pub(crate) fn recommendations(platform: Platform) -> &'static [&'static str] {
    match platform {
        Platform::MacOs => &[
            "Ghostty — brew install --cask ghostty",
            "iTerm2 — brew install --cask iterm2",
        ],
        Platform::Linux => &[
            "Ghostty — install from your distribution or ghostty.org",
            "kitty — apt install kitty (or dnf install kitty)",
            "WezTerm — install from your distribution or wezterm.org",
        ],
        Platform::Windows => &[
            "Windows Terminal — winget install Microsoft.WindowsTerminal",
            "WezTerm — winget install wez.wezterm",
        ],
        Platform::Android => &[],
    }
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
        lines.push("frame synchronization needs tmux 3.7 or newer");
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
}
