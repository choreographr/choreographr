//! The environment-marker rules that name the terminal and the layer hosting
//! it.
//!
//! Every function takes the environment injected as a closure, so the whole
//! rule set is unit-testable without touching the real process.
//!
//! The marker names and the [`value`] lookup are shared with the components
//! outside this module that read the same environment — the hosted-pane
//! reporter (`state::terminal_status::host_report`) names the host's markers and
//! applies the same "an empty value is not a claim" rule — so one spelling of a
//! variable serves both and the two cannot drift apart.

use super::types::{Host, HostedKind, Mux, TerminalKind};
use super::version::{Version, parse_packed_or_dotted};

/// The opt-in marker every Herdr pane exports.
pub(crate) const HERDR_ENV: &str = "HERDR_ENV";
/// The marker naming the host's own binary, the program a report invokes.
pub(crate) const HERDR_BIN_PATH: &str = "HERDR_BIN_PATH";
/// The marker naming the pane this program runs in.
pub(crate) const HERDR_PANE_ID: &str = "HERDR_PANE_ID";

/// The full marker set a Herdr pane exports, all of it required.
///
/// All three are demanded, not any one: they are the contract the host's
/// reporting API runs on (a pane id to address and a binary to invoke), so a
/// process carrying only a stale inherited slice of them is not a managed pane
/// — and treating it as one would hide the real terminal's identity from the
/// Terminal page and the advice panels for a reporter that never starts.
/// `state::terminal_status::host_report` requires exactly this set before it
/// invokes anything, so the profile and the reporter cannot disagree.
pub(crate) const HERDR_MARKERS: [&str; 3] = [HERDR_ENV, HERDR_BIN_PATH, HERDR_PANE_ID];

/// The markers a Rex pane exports. Any one of them identifies the layer: they
/// are per-session / per-block identifiers rather than one contract, and a Rex
/// pane can also be recognised by its terminfo entry alone (see
/// [`detect_hosted`]).
const REX_MARKERS: [&str; 3] = ["REX_SESSION", "REX_BLOCK", "REX_SERVER"];

/// Read `name`, treating an empty value as absent: a marker variable set to the
/// empty string is not a claim.
pub(crate) fn value(env: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name).filter(|raw| !raw.is_empty())
}

/// Read `name` and record it in `evidence` when it is set, handing the value
/// back.
fn marker(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
    name: &'static str,
) -> Option<String> {
    let raw = value(env, name);
    if raw.is_some() {
        evidence.push(name);
    }
    raw
}

/// Record every variable in `names` that the environment sets, reporting
/// whether any did.
fn any_marker(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
    names: &[&'static str],
) -> bool {
    // A plain loop, not `any()`: that would stop at the first match and leave the
    // layer's other markers out of the evidence, which is the whole point of
    // collecting them.
    let mut matched = false;
    for name in names {
        if marker(env, evidence, name).is_some() {
            matched = true;
        }
    }
    matched
}

/// Record every variable in `names` that the environment sets, reporting
/// whether **all** of them did.
///
/// Same evidence rule as [`any_marker`] — every marker that matched is kept, so
/// the audit line shows a partial set — but the verdict needs the whole set.
fn all_markers(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
    names: &[&'static str],
) -> bool {
    let mut matched = 0;
    for name in names {
        if marker(env, evidence, name).is_some() {
            matched += 1;
        }
    }
    matched == names.len()
}

/// Identify the layer immediately hosting this program.
///
/// Both a hosting workspace manager and a multiplexer are detected on every
/// call, even though the host is one value: the layer that wins is the hosted
/// one (a workspace manager claims a richer relationship with its panes than a
/// session wrapper does), and knowing what else matched is what lets the audit
/// line explain a nested session.
pub(crate) fn detect_host(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Host {
    let hosted = detect_hosted(env, evidence);
    let mux = detect_mux(env, evidence);
    match (hosted, mux) {
        (Some(kind), _) => Host::Hosted(kind),
        (None, Some(mux)) => Host::Mux(mux),
        (None, None) => Host::Direct,
    }
}

/// Identify a hosting workspace manager, its markers outranking a
/// multiplexer's.
fn detect_hosted(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Option<HostedKind> {
    let mut hosted: Option<HostedKind> = None;
    if all_markers(env, evidence, &HERDR_MARKERS) {
        hosted.get_or_insert(HostedKind::Herdr);
    }
    // Rex ships its own terminfo entry under `xterm-rex`, so its panes export
    // that TERM even when none of the REX_* markers leak into the environment.
    let rex_term = value(env, "TERM").is_some_and(|term| term == "xterm-rex");
    if rex_term {
        evidence.push("TERM=xterm-rex");
    }
    if any_marker(env, evidence, &REX_MARKERS) || rex_term {
        hosted.get_or_insert(HostedKind::Rex);
    }
    hosted
}

/// Identify a session multiplexer, recording every multiplexer marker that
/// matched so one nested inside another is visible in the audit line.
fn detect_mux(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Option<Mux> {
    let tmux = value(env, "TMUX").is_some();
    let zellij = value(env, "ZELLIJ").is_some();
    let screen = value(env, "STY").is_some();
    let mux = if tmux {
        Some(Mux::Tmux {
            version: tmux_version(env, evidence),
        })
    } else if zellij {
        Some(Mux::Zellij)
    } else if screen {
        Some(Mux::Screen)
    } else {
        None
    };
    for (set, name) in [(tmux, "TMUX"), (zellij, "ZELLIJ"), (screen, "STY")] {
        if set {
            evidence.push(name);
        }
    }
    mux
}

/// tmux's own version, read from the pair it exports into a pane (3.2+).
///
/// `TMUX` alone says nothing about which tmux it is, so an older or otherwise
/// silent tmux yields `None` rather than a guess.
fn tmux_version(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Option<Version> {
    if value(env, "TERM_PROGRAM").as_deref() != Some("tmux") {
        return None;
    }
    evidence.push("TERM_PROGRAM=tmux");
    value(env, "TERM_PROGRAM_VERSION").and_then(|raw| Version::parse(&raw))
}

/// Identify the terminal emulator, for a [`Host::Direct`] session only.
///
/// Checks run from the most distinctive marker to the least, so a variable only
/// one terminal exports wins over an inherited `TERM`.
pub(crate) fn detect_terminal(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> (TerminalKind, Option<Version>) {
    // Per-pane / per-session identifiers: each is exported by exactly one
    // terminal, so it is unambiguous even when the environment carries an
    // inherited TERM.
    if marker(env, evidence, "WT_SESSION").is_some() {
        return (TerminalKind::WindowsTerminal, None);
    }
    if let Some(raw) = marker(env, evidence, "TERMUX_VERSION") {
        return (TerminalKind::Termux, Version::parse(&raw));
    }
    if marker(env, evidence, "KITTY_WINDOW_ID").is_some() {
        return (TerminalKind::Kitty, None);
    }

    if let Some(program) = marker(env, evidence, "TERM_PROGRAM") {
        let known = match program.as_str() {
            "ghostty" => Some(TerminalKind::Ghostty),
            "iTerm.app" => Some(TerminalKind::ItTerm),
            "WezTerm" => Some(TerminalKind::WezTerm),
            "Apple_Terminal" => Some(TerminalKind::AppleTerminal),
            _ => None,
        };
        if let Some(kind) = known {
            return (kind, terminal_program_version(env, evidence));
        }
        // A TERM_PROGRAM we recognize as an application but have no row for (a
        // code editor's embedded terminal, an Electron shell, mintty, rio) still
        // names the program the user is looking at — the toolkit variables below
        // would describe the engine underneath it, not that program. So its
        // presence ends identification as Unknown.
        return (TerminalKind::Unknown, None);
    }

    // Toolkit version variables, each exported only by terminals built on that
    // toolkit.
    if let Some(raw) = marker(env, evidence, "KONSOLE_VERSION") {
        return (TerminalKind::Konsole, parse_packed_or_dotted(&raw));
    }
    if let Some(raw) = marker(env, evidence, "PTYXIS_VERSION") {
        return (TerminalKind::Ptyxis, Version::parse(&raw));
    }
    if let Some(raw) = marker(env, evidence, "VTE_VERSION") {
        return (TerminalKind::Vte, parse_packed_or_dotted(&raw));
    }

    // TERM's prefix names the emulator, unless it only names a capability tier
    // (`xterm-256color`, `screen-256color`) shared by many — in which case no
    // marker is recorded and identification stays Unknown.
    if let Some(term) = value(env, "TERM") {
        if term.starts_with("foot") {
            evidence.push("TERM=foot");
            return (TerminalKind::Foot, None);
        }
        if term.starts_with("alacritty") {
            evidence.push("TERM=alacritty");
            return (TerminalKind::Alacritty, None);
        }
        if term.starts_with("xterm-ghostty") {
            evidence.push("TERM=xterm-ghostty");
            return (TerminalKind::Ghostty, None);
        }
    }
    (TerminalKind::Unknown, None)
}

/// The version a `TERM_PROGRAM`-identified terminal exports alongside its name.
fn terminal_program_version(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Option<Version> {
    marker(env, evidence, "TERM_PROGRAM_VERSION").and_then(|raw| Version::parse(&raw))
}
