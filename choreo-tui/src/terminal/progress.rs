//! Terminal-native progress bar (OSC 9;4).
//!
//! Emits the `OSC 9;4` escape sequence that terminal emulators render as a
//! native progress indicator, so a long model turn shows progress outside the
//! TUI's own viewport (taskbar/tab).
//!
//! **Gating.** Whether to emit is decided by [`should_emit`] from two inputs:
//! the detected terminal's `progress` capability (three-state) and the host
//! layer the program runs under. The rule rests on one property of the
//! sequence: where a terminal does not implement it, it is **inert** — an
//! unrecognised OSC is discarded, and a set command produces no reply, so
//! nothing can reach the input stream. Emitting into that silence costs
//! nothing; withholding costs the user a progress bar. So only a *settled*
//! `Some(false)` withholds, and an unasserted `None` emits. That is what makes
//! the bar reach kitty, whose `KITTY_WINDOW_ID` marker carries no version and
//! whose row therefore stays unasserted at runtime.
//!
//! OSC 9;4 writes the terminal-wide ROOT record of the OSC 7501 program-status
//! protocol ([`super::status`]) in terminals that map 9;4 onto it. That is why
//! every OSC 7501 report is emitted as a CHILD record keyed by session id: a
//! report replaces its record completely, so sharing the root would let a 9;4
//! update wipe a real status report.

use super::profile::{Host, Mux, TerminalProfile};
use super::{osc, write};

/// Decide whether an OSC 9;4 sequence should be written.
///
/// - **A settled `Some(false)` withholds.** That answer comes from the
///   terminal's own version or documentation (foot, alacritty, Apple Terminal,
///   …), so there is nothing to show.
/// - **`Some(true)` and `None` emit.** "Not asserted" means the table has no
///   evidence, not that the feature is absent — and an unsupported sequence is
///   inert (see the module docs), so the honest choice is to send it. Never
///   invert this: requiring `Some(true)` is what silently disabled the bar on
///   every terminal identified by a version-less marker, kitty first among
///   them.
/// - **A GNU screen pane suppresses it.** Screen forwards none of the client's
///   sequences through its pane — the same fact the shared
///   `terminal_advice::screen_sentence` states — so the sequence would be
///   written into a void. tmux and zellij pass it through and do not suppress.
/// - **A hosted pane defers to the host's own answer.** A workspace manager owns
///   the pane's progress surface, so writing our own bar would fight it — unless
///   the host itself asserts it renders OSC 9;4. Under [`Host::Hosted`] the
///   merged `progress` answer can only be `Some(true)` because the host says so
///   (the terminal identity never reaches us), and then the host maps the
///   sequence onto its own surface. That keeps the drawn bar and the Terminal
///   page's "supported (host)" row in agreement.
fn should_emit(cap: Option<bool>, host: &Host) -> bool {
    // GNU screen forwards none of the client's sequences through its pane.
    if matches!(host, Host::Mux(Mux::Screen)) {
        return false;
    }
    // A workspace manager owns the pane's progress surface: emit only when the
    // host itself asserts the sequence (the merged `cap` is then `Some(true)`),
    // never on the default unasserted answer.
    if matches!(host, Host::Hosted(_)) {
        return cap == Some(true);
    }
    // Direct or a forwarding multiplexer: only a settled "no" withholds.
    cap != Some(false)
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
/// no-op on a terminal the table settles as not supporting OSC 9;4 and inside a
/// hosted pane or a GNU screen pane.
pub(crate) fn update(
    last_prompt_tokens: Option<u32>,
    context_window: Option<u32>,
    profile: &TerminalProfile,
) {
    if !should_emit(profile.capabilities().progress, &profile.host) {
        return;
    }
    write(&build_seq(context_window, last_prompt_tokens));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::profile::HostedKind;

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
    /// emulator, both multiplexers that pass 9;4 through, GNU screen (which
    /// forwards nothing), and both workspace managers that own the pane's
    /// progress surface.
    fn hosts() -> Vec<Host> {
        vec![
            Host::Direct,
            Host::Mux(Mux::Tmux { version: None }),
            Host::Mux(Mux::Zellij),
            Host::Mux(Mux::Screen),
            Host::Hosted(HostedKind::Herdr),
            Host::Hosted(HostedKind::Rex),
        ]
    }

    #[test]
    fn every_capability_host_combination_agrees_with_the_rule() {
        for cap in [Some(true), Some(false), None] {
            for host in hosts() {
                let expected = match &host {
                    // Screen forwards nothing; a hosted pane emits only when the
                    // host itself asserts the sequence.
                    Host::Mux(Mux::Screen) => false,
                    Host::Hosted(_) => cap == Some(true),
                    Host::Direct | Host::Mux(_) => cap != Some(false),
                };
                assert_eq!(
                    should_emit(cap, &host),
                    expected,
                    "cap {cap:?}, host {host}"
                );
            }
        }
    }

    #[test]
    fn a_host_that_provides_progress_emits_and_one_that_does_not_suppresses() {
        // A workspace manager that asserts it renders OSC 9;4 maps it onto its
        // own surface: the bar is drawn. One that asserts nothing (or a settled
        // no) keeps our bar off its surface.
        assert!(should_emit(Some(true), &Host::Hosted(HostedKind::Herdr)));
        assert!(!should_emit(Some(false), &Host::Hosted(HostedKind::Herdr)));
        assert!(!should_emit(None, &Host::Hosted(HostedKind::Rex)));
    }

    #[test]
    fn an_unasserted_capability_still_draws_the_bar() {
        // kitty's row: `KITTY_WINDOW_ID` carries no version, so the 0.47 floor
        // cannot be evaluated and the answer is `None`. An unasserted row must
        // emit — an unsupported 9;4 is inert, and requiring `Some(true)` here
        // is what kept the bar off kitty entirely.
        assert!(should_emit(None, &Host::Direct));
        assert!(should_emit(None, &Host::Mux(Mux::Zellij)));
        // It is still suppressed where the surface is not ours to draw on.
        assert!(!should_emit(None, &Host::Mux(Mux::Screen)));
        assert!(!should_emit(None, &Host::Hosted(HostedKind::Herdr)));
    }

    #[test]
    fn a_settled_no_withholds_and_is_never_overridden() {
        // foot and alacritty settle this row as unsupported; nothing else may
        // turn the bar back on for them.
        assert!(!should_emit(Some(false), &Host::Direct));
        assert!(!should_emit(
            Some(false),
            &Host::Mux(Mux::Tmux { version: None })
        ));
        assert!(!should_emit(Some(false), &Host::Mux(Mux::Screen)));
        assert!(!should_emit(Some(false), &Host::Hosted(HostedKind::Rex)));
    }

    #[test]
    fn screen_suppresses_even_a_supported_row() {
        assert!(!should_emit(Some(true), &Host::Mux(Mux::Screen)));
        // A multiplexer that forwards does not.
        assert!(should_emit(Some(true), &Host::Mux(Mux::Zellij)));
    }
}
