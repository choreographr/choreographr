//! First-run setup guidance: the App-side glue over the shared setup rules
//! in `choreo_client_core::onboarding` — reading the checklist state off the
//! attached session, arming the follow-through that walks a freshly created
//! account straight into the session and the model picker, and pre-filling
//! the new-account wizard's name — plus the terminal advice the welcome panel
//! shows when the detected terminal or its host layer costs this client a
//! feature it actually uses.
//!
//! Everything here is pure state derivation over `App` (no I/O, no channels),
//! so the rules are unit-testable without a daemon. The render side lives in
//! `render/welcome.rs`; the message-handling side (auto-selecting the account
//! once its credential is confirmed) lives in `connection/daemon.rs`.

use super::App;
use crate::terminal::profile::{
    Capabilities, Host, Mux, NativeImages, Platform, TerminalKind, TerminalProfile, Version,
};
use choreo_client_core::{SetupFollow, SetupProgress, suggest_account_name};

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

/// tmux gained synchronized output (what the client's atomic frames need) in
/// 3.7; below it the panel says so rather than leaving a user to guess why
/// redraws tear.
const TMUX_FRAME_SYNC: Version = Version::new(3, 7, 0);

impl App {
    /// The launch-time terminal advice, or `None` when the terminal and its
    /// host cost this client nothing.
    pub(crate) fn terminal_notice(&self) -> Option<&TerminalNotice> {
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
            lines: vec![
                "screen does not forward keys, images, or clipboard through its pane; \
                 run this inside tmux or zellij instead."
                    .to_string(),
            ],
        }),
        Host::Hosted(_) => Some(TerminalNotice {
            headline: "Terminal advice does not apply in this pane".to_string(),
            lines: vec![
                "a workspace manager owns this pane and keeps its own screen model, so \
                 the terminal is not ours to recommend."
                    .to_string(),
            ],
        }),
    };

    // The Option-as-Alt line is about the emulator's own key handling, which a
    // multiplexer passes through but a workspace manager's pane does not: under
    // a hosted layer we cannot name the outer terminal, so it is withheld there.
    let option_key = match profile.host {
        Host::Hosted(_) => None,
        Host::Direct | Host::Mux(_) if profile.platform == Platform::MacOs => {
            Some(macos_option_notice(profile.name))
        }
        Host::Direct | Host::Mux(_) => None,
    };

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
    // the sentence still reads without inventing an identity.
    let subject = match profile.name {
        TerminalKind::Unknown => "This terminal".to_string(),
        known => user_name(known).to_string(),
    };
    let headline = format!("{subject} — {} won't work", join_phrases(&gaps));
    let mut lines = vec!["A terminal that supports it:".to_string()];
    lines.extend(
        recommendations(profile.platform)
            .into_iter()
            .map(str::to_string),
    );
    Some(TerminalNotice { headline, lines })
}

/// The tmux configuration lines that let the client's features through a pane.
fn tmux_notice(version: Option<Version>) -> TerminalNotice {
    let mut lines = vec![
        "set -g extended-keys on".to_string(),
        "set -g allow-passthrough on".to_string(),
    ];
    // Only a version we actually read can rule frame synchronization out; an
    // unknown tmux version is told nothing rather than told it is too old.
    if version.is_some_and(|version| !version.at_least(TMUX_FRAME_SYNC)) {
        lines.push("frame synchronization needs tmux 3.7 or newer".to_string());
    }
    TerminalNotice {
        headline: "tmux must be told to forward what this client uses".to_string(),
        lines,
    }
}

/// The macOS Option-as-Alt advice, with the setting for the detected terminal.
///
/// Option+key composes a character on macOS before any application sees it, so
/// an `alt+` shortcut never arrives unless the terminal is told to send Option
/// as Meta. The setting names the exact key and file for the terminal we
/// identified; an unidentified terminal gets the generic sentence, because
/// naming a setting we cannot be sure applies would be worse than naming none.
fn macos_option_notice(name: TerminalKind) -> TerminalNotice {
    let setting = match name {
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
        // An unidentified terminal (or one of the remaining rows) has no
        // setting we can point at, so the sentence stays generic.
        _ => {
            "Set your terminal to send Option as Meta (Alt); the setting is in its \
             keyboard or profile preferences"
        }
    };
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

/// The user-facing name of what the client loses when each capability is
/// asserted unsupported.
///
/// Only the capabilities whose loss is visible in the client's own features:
/// copy-on-select, flicker-free frame redraws, the disambiguated modified keys
/// (Shift+Enter and friends), and inline images. The reporting extras — the
/// OSC 9;4 progress bar, OSC 7501 program status, OSC 22 pointer shape — are
/// silent when unsupported, so their absence never warrants a recommendation.
fn gap_phrases(capabilities: Capabilities) -> Vec<&'static str> {
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

/// The terminals worth recommending on `platform`, best first.
///
/// Android is empty by design: the platform's terminal is the Android app the
/// user already chose, and no alternative is offered.
fn recommendations(platform: Platform) -> Vec<&'static str> {
    match platform {
        Platform::MacOs => vec![
            "Ghostty — brew install --cask ghostty",
            "iTerm2 — brew install --cask iterm2",
        ],
        Platform::Linux => vec![
            "Ghostty — install from your distribution or ghostty.org",
            "kitty — apt install kitty (or dnf install kitty)",
            "WezTerm — install from your distribution or wezterm.org",
        ],
        Platform::Windows => vec![
            "Windows Terminal — winget install Microsoft.WindowsTerminal",
            "WezTerm — winget install wez.wezterm",
        ],
        Platform::Android => Vec::new(),
    }
}

/// A terminal's name as a person reads it, for the notice's copy.
fn user_name(kind: TerminalKind) -> &'static str {
    match kind {
        TerminalKind::Ghostty => "Ghostty",
        TerminalKind::Kitty => "kitty",
        TerminalKind::WezTerm => "WezTerm",
        TerminalKind::Alacritty => "Alacritty",
        TerminalKind::Foot => "foot",
        TerminalKind::ItTerm => "iTerm2",
        TerminalKind::WindowsTerminal => "Windows Terminal",
        TerminalKind::Konsole => "Konsole",
        TerminalKind::Ptyxis => "Ptyxis",
        TerminalKind::Vte => "a VTE terminal",
        TerminalKind::AppleTerminal => "Terminal.app",
        TerminalKind::Termux => "Termux",
        TerminalKind::Rex => "Rex",
        TerminalKind::Unknown => "this terminal",
    }
}

/// Join phrases into one English list: `a`, `a and b`, `a, b, and c`.
fn join_phrases(phrases: &[&str]) -> String {
    match phrases {
        [] => String::new(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

impl App {
    /// Arm the follow-through for `account` when its key is being saved and
    /// the attached session has no account yet. A session that already has
    /// an account keeps it: adding a second key must not silently switch
    /// what an in-use session talks to.
    pub(crate) fn arm_setup_follow(&mut self, account: &str) {
        self.setup_follow = SetupFollow::arm(
            account,
            self.attached_session_id.is_some(),
            self.attached_account_slug.is_some(),
        );
    }

    /// Pick the wizard's highlighted provider (advancing to the name step)
    /// and pre-fill the account name with a suggestion, so the common case
    /// is a single Enter. The field stays editable for anyone who wants a
    /// different name. A no-op when the filtered list is empty.
    pub(crate) fn confirm_wizard_provider(&mut self) {
        self.ai_providers.wizard.confirm_provider(&self.providers);
        let Some(provider) = self.ai_providers.wizard.picked_slug.as_deref() else {
            return;
        };
        tracing::debug!(slug = provider, "provider picked in account wizard");
        let existing: Vec<&str> = self
            .ai_providers
            .accounts
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        let suggestion = suggest_account_name(provider, &existing);
        self.ai_providers.wizard.prefill_slug(suggestion);
    }

    /// The attached session's setup state, read from the accounts list and
    /// the session's display state (the same fields the status bar shows).
    pub(crate) fn setup_progress(&self) -> SetupProgress {
        SetupProgress::derive(
            &self.ai_providers.accounts,
            self.attached_account_slug.as_deref(),
            self.active_display_ref()
                .is_some_and(|d| d.selected_model.is_some()),
        )
    }

    /// Whether the chat page should show the welcome panel in place of the
    /// (empty) history: a session is attached and nothing is in it yet.
    /// Reads `visible_turn_ids`, so callers must run after the per-frame
    /// `compute_total_height_and_markers` rebuild (as `render_chat` does).
    pub(crate) fn show_welcome(&self) -> bool {
        self.attached_session_id.is_some()
            && self
                .active_display_ref()
                .is_some_and(|d| d.visible_turn_ids.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::profile::{HostedKind, ImageSupport};
    use choreo_proto::AccountInfo;

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

    fn account(name: &str, has_credential: bool) -> AccountInfo {
        AccountInfo {
            name: name.to_string(),
            provider: "anthropic".to_string(),
            has_credential,
        }
    }

    // ---- the advice matrix -------------------------------------------------

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
    fn a_hosted_pane_says_advice_does_not_apply() {
        let notice = terminal_notice(&profile(
            TerminalKind::Unknown,
            Platform::MacOs,
            Host::Hosted(HostedKind::Herdr),
        ))
        .expect("a hosted pane is told why the advice is withheld");
        // One line only, and no Option-as-Alt advice: we cannot name the outer
        // terminal under a workspace manager.
        assert_eq!(notice.lines.len(), 1);
        assert!(notice.lines[0].contains("workspace manager"));
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

    // ---- the macOS Option-as-Alt line --------------------------------------

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

    // ---- the App-side glue -------------------------------------------------

    #[test]
    fn fresh_app_has_no_setup_progress() {
        let app = App::new();
        assert_eq!(app.setup_progress(), SetupProgress::default());
        assert!(!app.setup_progress().is_complete());
    }

    #[test]
    fn fresh_app_has_no_terminal_notice() {
        let app = App::new();
        assert!(app.terminal_notice().is_none());
    }

    #[test]
    fn an_account_without_a_key_does_not_count() {
        let mut app = App::new();
        app.ai_providers.set_accounts(vec![account("a", false)]);
        assert!(!app.setup_progress().has_keyed_account);
        app.ai_providers
            .set_accounts(vec![account("a", false), account("b", true)]);
        assert!(app.setup_progress().has_keyed_account);
    }

    #[test]
    fn session_account_comes_from_the_attached_slug() {
        let mut app = App::new();
        app.attached_account_slug = Some("anthropic".to_string());
        assert!(app.setup_progress().session_has_account);
    }

    #[test]
    fn step_one_is_judged_against_the_sessions_account() {
        let mut app = App::new();
        // No session account yet: any keyed account satisfies step 1.
        app.ai_providers.set_accounts(vec![account("a", true)]);
        assert!(app.setup_progress().has_keyed_account);

        // The session uses an account with NO key: step 1 is not done even
        // though another account has one, because the model list is fetched
        // through the session's own account.
        app.attached_session_id = Some(1);
        app.attached_account_slug = Some("b".to_string());
        app.ai_providers
            .set_accounts(vec![account("a", true), account("b", false)]);
        assert!(!app.setup_progress().has_keyed_account);

        // The session's own account is keyed: step 1 is done.
        app.ai_providers
            .set_accounts(vec![account("a", true), account("b", true)]);
        assert!(app.setup_progress().has_keyed_account);
    }

    #[test]
    fn follow_is_armed_only_for_a_session_without_an_account() {
        let mut app = App::new();
        // No attached session: nothing to select the account into.
        app.arm_setup_follow("anthropic");
        assert_eq!(app.setup_follow, None);

        app.attached_session_id = Some(1);
        app.arm_setup_follow("anthropic");
        assert_eq!(
            app.setup_follow,
            Some(SetupFollow::AwaitCredential("anthropic".to_string()))
        );

        // A session already using an account keeps it.
        app.attached_account_slug = Some("openai".to_string());
        app.arm_setup_follow("anthropic");
        assert_eq!(app.setup_follow, None);
    }

    #[test]
    fn no_welcome_without_an_attached_session() {
        assert!(!App::new().show_welcome());
    }
}
