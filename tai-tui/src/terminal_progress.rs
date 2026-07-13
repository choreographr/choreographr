use std::io::Write;
use std::sync::OnceLock;

const REMOVE: &str = "\x1b]9;4;0;\x1b\\";
const INDETERMINATE: &str = "\x1b]9;4;3;\x1b\\";

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
                let pct = (current as u64).saturating_mul(100) / cw as u64;
                let pct = pct.min(100);
                format!("\x1b]9;4;1;{pct}\x1b\\")
            }
            None => INDETERMINATE.to_string(),
        },
        _ => REMOVE.to_string(),
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
pub fn update_terminal_progress(last_prompt_tokens: Option<u32>, context_window: Option<u32>) {
    if !supports_progress() {
        return;
    }

    let seq = build_seq(context_window, last_prompt_tokens);

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = write!(handle, "{seq}");
    let _ = handle.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_when_no_context_window() {
        let seq = build_seq(None, None);
        assert_eq!(seq, REMOVE);
    }

    #[test]
    fn remove_when_context_window_zero() {
        let seq = build_seq(Some(0), Some(0));
        assert_eq!(seq, REMOVE);
    }

    #[test]
    fn indeterminate_when_no_prompt_tokens() {
        let seq = build_seq(Some(100), None);
        assert_eq!(seq, INDETERMINATE);
    }

    #[test]
    fn percentage_normal() {
        let seq = build_seq(Some(200), Some(100));
        assert_eq!(seq, "\x1b]9;4;1;50\x1b\\");
    }

    #[test]
    fn percentage_capped_at_100() {
        let seq = build_seq(Some(100), Some(999));
        assert_eq!(seq, "\x1b]9;4;1;100\x1b\\");
    }

    #[test]
    fn percentage_zero() {
        let seq = build_seq(Some(100), Some(0));
        assert_eq!(seq, "\x1b]9;4;1;0\x1b\\");
    }

    #[test]
    fn percentage_exact() {
        let seq = build_seq(Some(500), Some(500));
        assert_eq!(seq, "\x1b]9;4;1;100\x1b\\");
    }
}
