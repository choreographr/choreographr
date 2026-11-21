//! The reasoning-effort dialog's state: the modal the staged account flow shows
//! before it commits a model, so the user is told which effort the session will
//! run with instead of having one applied silently.
//!
//! The state is deliberately free of I/O and of the flow's own bookkeeping: it
//! holds a ladder of effort slugs, which row is highlighted, and which row the
//! daemon suggests, so the renderer can mark the default as such. Opening it,
//! moving it and taking its value are the whole API; who opens it and what the
//! chosen slug is committed *with* belongs to
//! [`SetupFlow`](super::SetupFlow).
//!
//! The list is short by construction (a model's ladder is a handful of slugs:
//! `off`, `low`, `medium`, `high`, …), so the dialog has no filter box and no
//! scroll window — [`move_up`](ReasoningDialogState::move_up) and
//! [`move_down`](ReasoningDialogState::move_down) walk the whole ladder.

/// State for the reasoning-effort dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReasoningDialogState {
    /// Whether the dialog is currently visible.
    pub(crate) open: bool,
    /// The model's effort levels, in the daemon's (ascending) order.
    pub(crate) levels: Vec<String>,
    /// Index into `levels` of the highlighted row.
    pub(crate) focused: usize,
    /// The level the daemon suggests for this model — the session's current
    /// effort when the model supports it, else the model's middle level. Drawn
    /// with a marker so the pre-highlight is explained rather than arbitrary.
    pub(crate) suggested: Option<String>,
}

impl ReasoningDialogState {
    /// A closed dialog with no levels.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether the dialog is visible.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Open the dialog on `levels`, highlighting `suggested` when the ladder
    /// contains it (else the first row).
    pub(crate) fn open(&mut self, levels: Vec<String>, suggested: Option<String>) {
        self.open = true;
        self.focused = suggested
            .as_deref()
            .and_then(|s| levels.iter().position(|level| level == s))
            .unwrap_or(0);
        self.suggested = suggested;
        self.levels = levels;
    }

    /// Dismiss the dialog, keeping the ladder so a reopen paints immediately.
    pub(crate) fn close(&mut self) {
        self.open = false;
    }

    /// Move the highlight up one row (no wrap: the top row is the top).
    pub(crate) fn move_up(&mut self) {
        self.focused = self.focused.saturating_sub(1);
    }

    /// Move the highlight down one row, stopping at the last level.
    pub(crate) fn move_down(&mut self) {
        if self.focused + 1 < self.levels.len() {
            self.focused += 1;
        }
    }

    /// The highlighted level, if the ladder is non-empty.
    pub(crate) fn focused_level(&self) -> Option<&str> {
        self.levels.get(self.focused).map(String::as_str)
    }

    /// Whether `level` is the daemon-suggested one (so the renderer can mark
    /// it). Compared as a plain slug: the ladder's own spelling is the key.
    pub(crate) fn is_suggested(&self, level: &str) -> bool {
        self.suggested.as_deref() == Some(level)
    }

    /// Take the highlighted level and close the dialog. `None` when there is
    /// nothing to take (an empty ladder), which leaves the dialog open — there
    /// is no choice to commit, so `Enter` must not imply one.
    pub(crate) fn take_selection(&mut self) -> Option<String> {
        let level = self.focused_level()?.to_string();
        self.close();
        Some(level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder() -> Vec<String> {
        ["off", "low", "medium", "high"]
            .iter()
            .map(|l| (*l).to_string())
            .collect()
    }

    #[test]
    fn a_new_dialog_is_closed() {
        let dialog = ReasoningDialogState::new();
        assert!(!dialog.is_open());
        assert!(dialog.focused_level().is_none());
    }

    #[test]
    fn opening_pre_highlights_the_suggestion() {
        let mut dialog = ReasoningDialogState::new();
        dialog.open(ladder(), Some("medium".to_string()));
        assert!(dialog.is_open());
        assert_eq!(dialog.focused_level(), Some("medium"));
        assert!(dialog.is_suggested("medium"));
        assert!(!dialog.is_suggested("high"));
    }

    #[test]
    fn an_unknown_or_missing_suggestion_highlights_the_first_row() {
        for suggested in [None, Some("xhigh".to_string())] {
            let mut dialog = ReasoningDialogState::new();
            dialog.open(ladder(), suggested);
            assert_eq!(dialog.focused_level(), Some("off"));
        }
    }

    #[test]
    fn arrows_walk_the_ladder_and_stop_at_the_ends() {
        let mut dialog = ReasoningDialogState::new();
        dialog.open(ladder(), Some("low".to_string()));

        dialog.move_down();
        assert_eq!(dialog.focused_level(), Some("medium"));
        dialog.move_down();
        dialog.move_down();
        assert_eq!(dialog.focused_level(), Some("high"), "clamped at the last");
        dialog.move_up();
        assert_eq!(dialog.focused_level(), Some("medium"));
        dialog.move_up();
        dialog.move_up();
        assert_eq!(dialog.focused_level(), Some("off"), "clamped at the first");
    }

    #[test]
    fn taking_the_selection_closes_with_the_highlighted_level() {
        let mut dialog = ReasoningDialogState::new();
        dialog.open(ladder(), Some("medium".to_string()));
        dialog.move_down();
        assert_eq!(dialog.take_selection().as_deref(), Some("high"));
        assert!(!dialog.is_open(), "committing closes the dialog");
    }

    #[test]
    fn cancelling_closes_without_a_value() {
        let mut dialog = ReasoningDialogState::new();
        dialog.open(ladder(), Some("low".to_string()));
        dialog.close();
        assert!(!dialog.is_open());
        assert!(
            dialog.focused_level().is_some(),
            "cancelling keeps no choice, the caller never asks for one"
        );
    }

    #[test]
    fn entering_an_empty_ladder_takes_nothing() {
        let mut dialog = ReasoningDialogState::new();
        dialog.open(Vec::new(), None);
        assert_eq!(dialog.take_selection(), None);
        assert!(
            dialog.is_open(),
            "an empty ladder has no choice to commit, so nothing is implied"
        );
    }
}
