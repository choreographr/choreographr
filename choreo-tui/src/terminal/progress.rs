//! Terminal-native progress bar (OSC 9;4).
//!
//! Emits the `OSC 9;4` escape sequence that terminal emulators render as a
//! native progress indicator, so a long model turn shows progress outside the
//! TUI's own viewport (taskbar/tab). The sequence is a no-op on terminals that
//! don't advertise support, which is why the support probe is cached.
//!
//! OSC 9;4 writes the terminal-wide ROOT record of the OSC 7501 program-status
//! protocol ([`super::status`]) in terminals that map 9;4 onto it. That is why
//! every OSC 7501 report is emitted as a CHILD record keyed by session id: a
//! report replaces its record completely, so sharing the root would let a 9;4
//! update wipe a real status report.

use std::sync::OnceLock;

use super::{osc, write};

/// Whether the terminal supports OSC 9;4 progress sequences.
/// Checked once and cached to avoid re-querying every frame.
static TERM_SUPPORTS_PROGRESS: OnceLock<bool> = OnceLock::new();

fn supports_progress() -> bool {
    *TERM_SUPPORTS_PROGRESS.get_or_init(|| anstyle_progress::supports_term_progress(true))
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
/// This is a no-op on terminals that don't support OSC 9;4.
pub(crate) fn update(last_prompt_tokens: Option<u32>, context_window: Option<u32>) {
    if !supports_progress() {
        return;
    }
    write(&build_seq(context_window, last_prompt_tokens));
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
