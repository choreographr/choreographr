//! Session token accounting: the live context-size readout and the lifetime
//! billing total, both pure functions of the turn history. The two differ
//! deliberately — context size is live-only, billing sums every turn.

use choreo_proto::{TokenUsage, Turn};
use std::collections::BTreeMap;

use super::SessionState;

impl SessionState {
    /// The exact context size the session currently stands on, read from the
    /// turn history: the last non-undone turn's provider-reported prompt size.
    ///
    /// Each turn records the `input_tokens` its producing request billed
    /// ([`Turn::token_usage`]), which is the exact size of the conversation
    /// sent at that point. Reading the last *live* turn therefore makes the
    /// context-size readout track undo/redo exactly, with no re-estimation:
    /// undone turns and turns without usage (cancelled / user-only / providers
    /// that report no usage) are skipped, and `None` means no live turn carries
    /// a measurement, so there is nothing to display.
    #[must_use]
    pub fn live_last_prompt_tokens(&self) -> Option<u32> {
        self.turns
            .values()
            .rev()
            .filter(|turn| !turn.undone)
            .find_map(|turn| turn.token_usage.map(|u| u.input_tokens))
    }
}

/// Sum per-turn provider usage into the session's lifetime billing total.
///
/// Every turn's `token_usage` is added, undone turns included: undo hides
/// turns from the model but does not refund tokens already billed. This total
/// is deliberately distinct from the context-size readout, which is live-only
/// ([`SessionState::live_last_prompt_tokens`]).
pub(super) fn reconstruct_accumulated_usage(turns: &BTreeMap<u32, Turn>) -> TokenUsage {
    let mut accumulated = TokenUsage::default();
    for turn in turns.values() {
        if let Some(u) = turn.token_usage {
            accumulated.input_tokens += u.input_tokens;
            accumulated.output_tokens += u.output_tokens;
            accumulated.total_tokens += u.total_tokens;
        }
    }
    accumulated
}
