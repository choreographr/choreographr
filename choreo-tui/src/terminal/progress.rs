//! Terminal-native progress bar (OSC 9;4).
//!
//! Emits the `OSC 9;4` escape sequence that terminal emulators render as a
//! native progress indicator, so a long model turn shows progress outside the
//! TUI's own viewport (taskbar/tab). The sequence is a no-op on terminals that
//! don't advertise support, which is why the support probe is cached.
//!
//! **Gating.** Whether to emit is decided by [`should_emit`] from three inputs:
//! the detected terminal's `progress` capability (three-state), the host layer
//! the program runs under, and the cached environment probe. The probe is a
//! fallback, never a veto — see that function for the precedence.
//!
//! OSC 9;4 writes the terminal-wide ROOT record of the OSC 7501 program-status
//! protocol ([`super::status`]) in terminals that map 9;4 onto it. That is why
//! every OSC 7501 report is emitted as a CHILD record keyed by session id: a
//! report replaces its record completely, so sharing the root would let a 9;4
//! update wipe a real status report.

use std::sync::OnceLock;

use super::profile::{Host, TerminalProfile};
use super::{osc, write};

/// Whether the environment probe reports OSC 9;4 progress support.
/// Checked once and cached to avoid re-querying every frame.
static TERM_SUPPORTS_PROGRESS: OnceLock<bool> = OnceLock::new();

fn probe_support() -> bool {
    *TERM_SUPPORTS_PROGRESS.get_or_init(|| anstyle_progress::supports_term_progress(true))
}

/// Decide whether an OSC 9;4 sequence should be written.
///
/// The two answers combine as a fallback chain rather than a pair of vetoes:
///
/// - **Support is asserted by the profile *or* the probe.** The detected
///   terminal's capability is the richer signal (it knows kitty's 0.47 floor,
///   that a bare VTE renders 9;4, and so on), but the environment probe is kept
///   as an OR fallback so a terminal the probe already recognised keeps working
///   even if the capability table withholds an answer for it.
/// - **`None` never enables on its own.** "Not asserted" means the table has no
///   evidence, so it is not a yes.
/// - **`Some(false)` never disables what the probe affirms.** A settled "no"
///   from the table reflects what that terminal's version documents; it is not
///   grounds to override a probe that saw support in the live environment. Only
///   when both sources decline does the bar stay off.
/// - **A hosted pane suppresses it regardless.** Under [`Host::Hosted`] a
///   workspace manager owns the pane's progress surface (it reports state for
///   the block itself), so writing our own bar would fight it. A multiplexer
///   ([`Host::Mux`]) passes the sequence through to the real terminal, so it
///   does not suppress.
fn should_emit(cap: Option<bool>, host: &Host, probe: bool) -> bool {
    let supported = cap == Some(true) || probe;
    supported && !matches!(host, Host::Hosted(_))
}

/// Build the OSC 9;4 escape sequence string without writing to stdout.
fn build_seq(context_window: Option<u32>, last_prompt_tokens: Option<u32>) -> String {
    match context_window {
        Some(cw) if cw > 0 => match last_prompt_tokens {
            Some(current) => {
                // Use u64 for intermediate arithmetic to avoid any surprise
                // around u32::MAX * 100 overflowing.
                // u32 -> u64 is provably lossless, so `From` is preferred
                // over `as`.
                let pct = u64::from(current).saturating_mul(100) / u64::from(cw);
                let pct = pct.min(100);
                osc(9, &format!("4;1;{pct}"))
            }
            None => osc(9, "4;3;"),
        },
        _ => osc(9, "4;0;"),
    }
}

/// Update (or remove) the terminal-native progress bar.
///
/// - If `context_window` is `None` or 0 → removes the progress bar.
/// - If `last_prompt_tokens` is `None` → shows indeterminate progress (spinner).
/// - Otherwise → shows a percentage bar: `last_prompt_tokens / context_window`,
///   capped at 100%.
///
/// The launch-time `profile` gates the write (see [`should_emit`]), so this is a
/// no-op on terminals that don't support OSC 9;4 and inside a hosted pane.
pub(crate) fn update(
    last_prompt_tokens: Option<u32>,
    context_window: Option<u32>,
    profile: &TerminalProfile,
) {
    if !should_emit(
        profile.capabilities().progress,
        &profile.host,
        probe_support(),
    ) {
        return;
    }
    write(&build_seq(context_window, last_prompt_tokens));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::profile::{HostedKind, Mux};

    #[test]
    fn remove_when_no_context_window() {
        assert_eq!(build_seq(None, None), "\x1b]9;4;0;\x1b\\");
    }

    #[test]
    fn remove_when_context_window_zero() {
        assert_eq!(build_seq(Some(0), Some(0)), "\x1b]9;4;0;\x1b\\");
    }

    #[test]
    fn indeterminate_when_no_prompt_tokens() {
        assert_eq!(build_seq(Some(100), None), "\x1b]9;4;3;\x1b\\");
    }

    #[test]
    fn percentage_normal() {
        assert_eq!(build_seq(Some(200), Some(100)), "\x1b]9;4;1;50\x1b\\");
    }

    #[test]
    fn percentage_capped_at_100() {
        assert_eq!(build_seq(Some(100), Some(999)), "\x1b]9;4;1;100\x1b\\");
    }

    #[test]
    fn percentage_zero() {
        assert_eq!(build_seq(Some(100), Some(0)), "\x1b]9;4;1;0\x1b\\");
    }

    #[test]
    fn percentage_exact() {
        assert_eq!(build_seq(Some(500), Some(500)), "\x1b]9;4;1;100\x1b\\");
    }

    /// The host layers the matrix exercises: nothing between us and the
    /// emulator, both multiplexers that pass 9;4 through, and both workspace
    /// managers that own the pane's progress surface.
    fn hosts() -> Vec<(Host, bool)> {
        vec![
            (Host::Direct, true),
            (Host::Mux(Mux::Tmux { version: None }), true),
            (Host::Mux(Mux::Zellij), true),
            (Host::Hosted(HostedKind::Herdr), false),
            (Host::Hosted(HostedKind::Rex), false),
        ]
    }

    #[test]
    fn every_capability_host_and_probe_combination_agrees_with_the_rule() {
        for cap in [Some(true), Some(false), None] {
            for (host, host_allows) in hosts() {
                for probe in [true, false] {
                    let expected = (cap == Some(true) || probe) && host_allows;
                    assert_eq!(
                        should_emit(cap, &host, probe),
                        expected,
                        "cap {cap:?}, host {host}, probe {probe}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_settled_no_does_not_override_the_probe() {
        // The probe is a fallback, not a veto: a terminal whose row says no but
        // whose environment probe says yes still gets the bar.
        assert!(should_emit(Some(false), &Host::Direct, true));
        assert!(should_emit(Some(false), &Host::Mux(Mux::Zellij), true));
        // Inside a hosted pane it stays off, whatever the sources say.
        assert!(!should_emit(
            Some(false),
            &Host::Hosted(HostedKind::Rex),
            true
        ));
    }

    #[test]
    fn an_unasserted_capability_needs_the_probe() {
        assert!(!should_emit(None, &Host::Direct, false));
        assert!(should_emit(None, &Host::Direct, true));
        assert!(!should_emit(None, &Host::Mux(Mux::Zellij), false));
    }
}
