//! The profile rules, exercised over injected environments.

use ratatui_image::picker::ProtocolType;

use std::collections::HashMap;

use super::*;

/// An injected environment built from literal pairs.
fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
    move |name| map.get(name).cloned()
}

fn detect_linux(pairs: &[(&str, &str)]) -> TerminalProfile {
    TerminalProfile::detect(env_of(pairs), Platform::Linux, ImageSupport::Halfblocks)
}

#[test]
fn version_parses_dotted_suffixes_and_rejects_malformed() {
    assert_eq!(Version::parse("1.2.0"), Some(Version::new(1, 2, 0)));
    assert_eq!(Version::parse("3.5a"), Some(Version::new(3, 5, 0)));
    assert_eq!(Version::parse("3"), Some(Version::new(3, 0, 0)));
    assert_eq!(Version::parse(" 0.47 "), Some(Version::new(0, 47, 0)));
    // A malformed later component truncates rather than discarding.
    assert_eq!(Version::parse("3.x.1"), Some(Version::new(3, 0, 0)));
    assert_eq!(Version::parse("unknown"), None);
    assert_eq!(Version::parse(""), None);
    assert_eq!(Version::parse("a.b"), None);
}

#[test]
fn version_decodes_the_packed_integer_form() {
    assert_eq!(Version::from_packed(260_400), Version::new(26, 4, 0));
    assert_eq!(Version::from_packed(7_400), Version::new(0, 74, 0));
}

#[test]
fn version_ordering_compares_major_then_minor_then_patch() {
    assert!(Version::new(1, 2, 0).at_least(Version::new(1, 2, 0)));
    assert!(Version::new(1, 3, 0).at_least(Version::new(1, 2, 9)));
    assert!(Version::new(2, 0, 0).at_least(Version::new(1, 99, 99)));
    assert!(!Version::new(1, 2, 0).at_least(Version::new(1, 2, 1)));
    assert!(!Version::new(1, 1, 9).at_least(Version::new(1, 2, 0)));
}

#[test]
fn empty_environment_is_unknown_with_no_evidence() {
    let profile = detect_linux(&[]);
    assert_eq!(profile.name, TerminalKind::Unknown);
    assert_eq!(profile.version, None);
    assert_eq!(profile.host, Host::Direct);
    assert_eq!(profile.evidence, Vec::<&'static str>::new());
    assert_eq!(profile.capabilities().program_status, None);
}

#[test]
fn identifies_each_terminal_from_its_own_marker() {
    let cases: &[(&[(&str, &str)], TerminalKind)] = &[
        (&[("WT_SESSION", "guid")], TerminalKind::WindowsTerminal),
        (&[("TERMUX_VERSION", "0.118.0")], TerminalKind::Termux),
        (&[("KITTY_WINDOW_ID", "1")], TerminalKind::Kitty),
        (&[("TERM_PROGRAM", "ghostty")], TerminalKind::Ghostty),
        (&[("TERM_PROGRAM", "WezTerm")], TerminalKind::WezTerm),
        (&[("TERM_PROGRAM", "iTerm.app")], TerminalKind::ItTerm),
        (
            &[("TERM_PROGRAM", "Apple_Terminal")],
            TerminalKind::AppleTerminal,
        ),
        (&[("KONSOLE_VERSION", "260400")], TerminalKind::Konsole),
        (&[("PTYXIS_VERSION", "47.1")], TerminalKind::Ptyxis),
        (&[("VTE_VERSION", "7400")], TerminalKind::Vte),
        (&[("TERM", "foot-extra")], TerminalKind::Foot),
        (&[("TERM", "alacritty-direct")], TerminalKind::Alacritty),
        (&[("TERM", "xterm-ghostty")], TerminalKind::Ghostty),
    ];
    for (env, expected) in cases {
        let profile = detect_linux(env);
        assert_eq!(profile.name, *expected, "env {env:?}");
        assert_eq!(profile.host, Host::Direct, "env {env:?}");
    }
}

#[test]
fn a_capability_tier_term_is_not_an_identity() {
    let profile = detect_linux(&[("TERM", "xterm-256color")]);
    assert_eq!(profile.name, TerminalKind::Unknown);
    assert_eq!(profile.evidence, Vec::<&'static str>::new());
}

#[test]
fn a_negotiated_image_protocol_outranks_the_table() {
    // The picker asked the terminal and got an answer: that is a
    // measurement, so it stands even where the identity table asserts
    // nothing — the only capability a hosted pane can report.
    let mut profile = detect_linux(&[("TERM", "xterm-256color")]);
    assert_eq!(profile.capabilities().images, NativeImages::Unknown);

    profile.images = ImageSupport::Kitty;
    assert_eq!(profile.capabilities().images, NativeImages::Kitty);

    profile.images = ImageSupport::Sixel;
    assert_eq!(profile.capabilities().images, NativeImages::Sixel);

    // No native protocol negotiated keeps the table's answer: several
    // terminals answer the query that way whether or not they draw images.
    profile.images = ImageSupport::Halfblocks;
    assert_eq!(profile.capabilities().images, NativeImages::Unknown);
}

#[test]
fn a_code_editors_terminal_ends_identification_as_unknown() {
    let profile = detect_linux(&[("TERM_PROGRAM", "vscode"), ("TERM", "xterm-256color")]);
    assert_eq!(profile.name, TerminalKind::Unknown);
    // The application was named, so the audit line records that it was seen.
    assert_eq!(profile.evidence, vec!["TERM_PROGRAM"]);
}

#[test]
fn terminal_version_is_parsed_where_the_environment_carries_one() {
    let ghostty = detect_linux(&[
        ("TERM_PROGRAM", "ghostty"),
        ("TERM_PROGRAM_VERSION", "1.4.0"),
    ]);
    assert_eq!(ghostty.version, Some(Version::new(1, 4, 0)));
    assert_eq!(ghostty.version_label(), "1.4.0");

    let konsole = detect_linux(&[("KONSOLE_VERSION", "260400")]);
    assert_eq!(konsole.version, Some(Version::new(26, 4, 0)));

    let kitty = detect_linux(&[("KITTY_WINDOW_ID", "1")]);
    assert_eq!(kitty.version, None);
    assert_eq!(kitty.version_label(), "unknown");
}

#[test]
fn detects_each_host_layer() {
    let herdr = detect_linux(&[("HERDR_PANE_ID", "pane-7")]);
    assert_eq!(herdr.host, Host::Hosted(HostedKind::Herdr));

    let rex = detect_linux(&[("REX_SESSION", "s-1")]);
    assert_eq!(rex.host, Host::Hosted(HostedKind::Rex));

    let zellij = detect_linux(&[("ZELLIJ", "0")]);
    assert_eq!(zellij.host, Host::Mux(Mux::Zellij));

    let screen = detect_linux(&[("STY", "1234.pts-0.host")]);
    assert_eq!(screen.host, Host::Mux(Mux::Screen));
}

#[test]
fn rex_is_recognized_by_its_terminfo_entry_alone() {
    let profile = detect_linux(&[("TERM", "xterm-rex")]);
    assert_eq!(profile.host, Host::Hosted(HostedKind::Rex));
    // Rex is the terminal in its own panes, so its identity is known.
    assert_eq!(profile.name, TerminalKind::Rex);
}

#[test]
fn a_hosted_layer_outranks_a_multiplexer() {
    let profile = detect_linux(&[("HERDR_ENV", "1"), ("TMUX", "/tmp/tmux-1000/default,1,0")]);
    assert_eq!(profile.host, Host::Hosted(HostedKind::Herdr));
    // The multiplexer was still seen, so the audit line explains the nesting.
    assert!(profile.evidence.contains(&"TMUX"));
    assert!(profile.evidence.contains(&"HERDR_ENV"));
    // A layer above the terminal hides which terminal it is.
    assert_eq!(profile.name, TerminalKind::Unknown);
}

#[test]
fn a_multiplexer_hides_the_terminal_identity() {
    let profile = detect_linux(&[
        ("TMUX", "/tmp/tmux-1000/default,1,0"),
        ("TERM_PROGRAM", "ghostty"),
        ("TERM_PROGRAM_VERSION", "1.4.0"),
        ("KITTY_WINDOW_ID", "1"),
    ]);
    assert_eq!(profile.name, TerminalKind::Unknown);
    assert_eq!(profile.version, None);
}

#[test]
fn tmux_version_is_read_where_present_absent_or_malformed() {
    let present = detect_linux(&[
        ("TMUX", "/tmp/tmux-1000/default,1,0"),
        ("TERM_PROGRAM", "tmux"),
        ("TERM_PROGRAM_VERSION", "3.5a"),
    ]);
    assert_eq!(
        present.host,
        Host::Mux(Mux::Tmux {
            version: Some(Version::new(3, 5, 0))
        })
    );

    // tmux older than 3.2 exports neither variable.
    let absent = detect_linux(&[("TMUX", "/tmp/tmux-1000/default,1,0")]);
    assert_eq!(absent.host, Host::Mux(Mux::Tmux { version: None }));

    let malformed = detect_linux(&[
        ("TMUX", "/tmp/tmux-1000/default,1,0"),
        ("TERM_PROGRAM", "tmux"),
        ("TERM_PROGRAM_VERSION", "not-a-version"),
    ]);
    assert_eq!(malformed.host, Host::Mux(Mux::Tmux { version: None }));
    assert_eq!(malformed.host.to_string(), "tmux");
}

#[test]
fn termux_refines_linux_into_android() {
    let android = detect_linux(&[
        ("TERMUX_VERSION", "0.118.0"),
        ("PREFIX", "/data/data/com.termux/files/usr"),
    ]);
    assert_eq!(android.platform, Platform::Android);
    assert_eq!(android.name, TerminalKind::Termux);

    // The PREFIX alone is enough: it is Termux's own layout.
    let prefix_only = detect_linux(&[("PREFIX", "/data/data/com.termux/files/usr")]);
    assert_eq!(prefix_only.platform, Platform::Android);

    let plain = detect_linux(&[("TERM", "xterm-256color")]);
    assert_eq!(plain.platform, Platform::Linux);
}

#[test]
fn an_explicitly_android_target_keeps_its_platform() {
    let profile = TerminalProfile::detect(env_of(&[]), Platform::Android, ImageSupport::Kitty);
    assert_eq!(profile.platform, Platform::Android);
}

#[test]
fn unknown_asserts_nothing_rather_than_denying() {
    let caps = detect_linux(&[]).capabilities();
    assert_eq!(caps.kitty_keyboard, None);
    assert_eq!(caps.synchronized_output, None);
    assert_eq!(caps.clipboard_write, None);
    assert_eq!(caps.progress, None);
    assert_eq!(caps.program_status, None);
    assert_eq!(caps.pointer_shape, None);
    assert_eq!(caps.images, NativeImages::Unknown);
}

#[test]
fn rex_asserts_its_program_status_and_nothing_else() {
    let caps = detect_linux(&[("REX_SESSION", "s-1")]).capabilities();
    assert_eq!(caps.program_status, Some(true));
    assert_eq!(caps.kitty_keyboard, None);
    assert_eq!(caps.synchronized_output, None);
    assert_eq!(caps.clipboard_write, None);
    assert_eq!(caps.progress, None);
    assert_eq!(caps.pointer_shape, None);
    assert_eq!(caps.images, NativeImages::Unknown);
}

#[test]
fn ghostty_is_the_only_program_status_terminal_with_mode_2026() {
    let caps = detect_linux(&[
        ("TERM_PROGRAM", "ghostty"),
        ("TERM_PROGRAM_VERSION", "1.4.0"),
    ])
    .capabilities();
    assert_eq!(caps.program_status, Some(true));
    assert_eq!(caps.synchronized_output, Some(true));
    assert_eq!(caps.pointer_shape, Some(true));
    assert_eq!(caps.progress, Some(true));
    assert_eq!(caps.images, NativeImages::Kitty);
}

#[test]
fn version_floors_gate_progress_and_images() {
    // Ghostty below the 1.2 floor has no OSC 9;4, and an unknown version
    // cannot rule it out.
    let old_ghostty = detect_linux(&[
        ("TERM_PROGRAM", "ghostty"),
        ("TERM_PROGRAM_VERSION", "1.1.0"),
    ]);
    assert_eq!(old_ghostty.capabilities().progress, Some(false));
    let unknown_version = detect_linux(&[("TERM", "xterm-ghostty")]);
    assert_eq!(unknown_version.capabilities().progress, None);

    // Kitty: OSC 9;4 needs 0.47.
    let kitty_old = detect_linux(&[("TERM_PROGRAM", "kitty")]);
    assert_eq!(kitty_old.capabilities().progress, None);
    let kitty = TerminalProfile {
        name: TerminalKind::Kitty,
        version: Some(Version::new(0, 46, 0)),
        ..detect_linux(&[])
    };
    assert_eq!(kitty.capabilities().progress, Some(false));
    let kitty_new = TerminalProfile {
        name: TerminalKind::Kitty,
        version: Some(Version::new(0, 47, 0)),
        ..detect_linux(&[])
    };
    assert_eq!(kitty_new.capabilities().progress, Some(true));
    assert_eq!(kitty_new.capabilities().images, NativeImages::Kitty);
}

#[test]
fn version_less_alacritty_leaves_its_gated_rows_unasserted() {
    // `TERM=alacritty-direct` names the terminal but carries no version, so
    // the 0.13 floors resolve to `None` rather than asserting a verdict the
    // environment never gave us.
    let caps = detect_linux(&[("TERM", "alacritty-direct")]).capabilities();
    assert_eq!(caps.kitty_keyboard, None);
    assert_eq!(caps.synchronized_output, None);
    assert_eq!(caps.clipboard_write, None);
}

#[test]
fn version_less_foot_leaves_kitty_keyboard_unasserted() {
    // `TERM=foot-extra` names the terminal but carries no version, so the
    // 1.10.3 floor cannot be evaluated and the row stays unasserted.
    let caps = detect_linux(&[("TERM", "foot-extra")]).capabilities();
    assert_eq!(caps.kitty_keyboard, None);
}

#[test]
fn version_less_kitty_leaves_progress_to_the_probe() {
    // `KITTY_WINDOW_ID` identifies kitty but carries no version, so the
    // 0.47 progress floor yields `None`: the OSC 9;4 environment probe is
    // what decides progress at runtime.
    let caps = detect_linux(&[("KITTY_WINDOW_ID", "1")]).capabilities();
    assert_eq!(caps.progress, None);
}

#[test]
fn vte_refuses_mode_2026_and_denies_osc_52() {
    let caps = detect_linux(&[("VTE_VERSION", "7400")]).capabilities();
    assert_eq!(caps.synchronized_output, Some(false));
    assert_eq!(caps.clipboard_write, Some(false));
    assert_eq!(caps.kitty_keyboard, Some(false));
    assert_eq!(caps.progress, Some(true));
    assert_eq!(caps.images, NativeImages::None);
}

#[test]
fn konsole_reports_halfblocks_and_a_2604_progress_floor() {
    let old = detect_linux(&[("KONSOLE_VERSION", "240800")]);
    assert_eq!(old.capabilities().progress, Some(false));
    assert_eq!(old.capabilities().images, NativeImages::Halfblocks);
    let new = detect_linux(&[("KONSOLE_VERSION", "260400")]);
    assert_eq!(new.capabilities().progress, Some(true));
}

#[test]
fn windows_terminal_leaves_images_unasserted() {
    let caps = detect_linux(&[("WT_SESSION", "guid")]).capabilities();
    assert_eq!(caps.images, NativeImages::Unknown);
    assert_eq!(caps.kitty_keyboard, Some(true));
    assert_eq!(caps.program_status, Some(false));
}

#[test]
fn empty_values_are_not_markers() {
    let profile = detect_linux(&[("TMUX", ""), ("KITTY_WINDOW_ID", "")]);
    assert_eq!(profile.host, Host::Direct);
    assert_eq!(profile.name, TerminalKind::Unknown);
    assert_eq!(profile.evidence, Vec::<&'static str>::new());
}

#[test]
fn display_renders_the_log_fields() {
    let profile = detect_linux(&[
        ("TMUX", "/tmp/tmux-1000/default,1,0"),
        ("TERM_PROGRAM", "tmux"),
        ("TERM_PROGRAM_VERSION", "3.5"),
    ]);
    assert_eq!(profile.host.to_string(), "tmux 3.5.0");
    assert_eq!(profile.platform.to_string(), "linux");
    assert_eq!(profile.images.to_string(), "halfblocks");
    assert_eq!(profile.name.to_string(), "unknown");
}

#[test]
fn image_support_follows_the_picker() {
    assert_eq!(
        ImageSupport::from(ProtocolType::Halfblocks),
        ImageSupport::Halfblocks
    );
    assert_eq!(ImageSupport::from(ProtocolType::Sixel), ImageSupport::Sixel);
    assert_eq!(ImageSupport::from(ProtocolType::Kitty), ImageSupport::Kitty);
    assert_eq!(
        ImageSupport::from(ProtocolType::Iterm2),
        ImageSupport::Iterm2
    );
    assert_eq!(ImageSupport::Kitty.to_string(), "kitty");
}

#[test]
fn the_default_profile_is_the_unidentified_one() {
    let profile = TerminalProfile::default();
    assert_eq!(profile.name, TerminalKind::Unknown);
    assert_eq!(profile.host, Host::Direct);
    assert_eq!(profile.images, ImageSupport::Halfblocks);
    assert_eq!(profile.evidence, Vec::<&'static str>::new());
}
