//! The new-account wizard modal (AI providers page, `n`): its two steps
//! ([`AccountWizardStep`]) and the state each carries
//! ([`AccountWizardState`]).
//!
//! The picker arithmetic it drives lives in [`super::picker`], shared with the
//! model selector; the paging step it uses is the page's own
//! [`PROVIDER_PAGE_LINES`](super::providers::PROVIDER_PAGE_LINES).

use super::picker::{filter_matches, picker_window, step_focus};
use super::providers::PROVIDER_PAGE_LINES;
use crate::state::{DisplayLine, InputBuffer, ProviderInfo};
use crossterm::event::KeyEvent;
use tui_prompts::{State, TextState};

/// Steps of the new-account wizard modal (AI providers page, `n`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountWizardStep {
    /// Step 1: pick a provider from the searchable list.
    Provider,
    /// Step 2: enter the account slug (a separate modal).
    Slug,
}

/// The new-account wizard modal (AI providers page, `n`).  Mirrors the model
/// selector's modal pattern: step 1 is a centered, searchable provider picker
/// (case-insensitive substring over display names — the canonical slug is
/// deliberately NOT shown, since it is easily confused with the account slug
/// entered in step 2); step 2 is a separate slug-entry modal.
pub(crate) struct AccountWizardState {
    pub(crate) open: bool,
    pub(crate) step: AccountWizardStep,
    /// Filter text for the provider picker (step 1).
    pub(crate) filter: InputBuffer,
    /// Index into the *filtered* provider list of the highlighted row.
    pub(crate) focused: usize,
    /// First row of the visible window into the filtered list.
    pub(crate) scroll: usize,
    /// List-body viewport height, cached by
    /// `update_viewport_from_terminal_size` (outside the draw closure) so
    /// arrow/wheel navigation can pin the highlight at the middle row and
    /// scroll the list under it.  0 until the first frame; navigation then
    /// falls back to focus-only moves with a render-time clamp (mirrors
    /// `SessionManagerState::viewport_height`).
    pub(crate) viewport_height: usize,
    /// Provider chosen in step 1 (slug + display-name snapshot).  A catalog
    /// refresh arriving mid-wizard cannot shift the pick, because it is an
    /// owned snapshot rather than an index into the live list.
    pub(crate) picked_slug: Option<String>,
    pub(crate) picked_name: Option<String>,
    /// Slug (account name) entry field (step 2).
    pub(crate) slug: TextState<'static>,
    /// Wizard-scoped error text (e.g. slug validation failures).  A
    /// [`DisplayLine`] stores it normalized for display, so every reader sees
    /// inert bytes without re-scanning.
    pub(crate) error: DisplayLine,
}

impl AccountWizardState {
    pub(crate) fn new() -> Self {
        Self {
            open: false,
            step: AccountWizardStep::Provider,
            filter: InputBuffer::new(),
            focused: 0,
            scroll: 0,
            viewport_height: 0,
            picked_slug: None,
            picked_name: None,
            slug: TextState::default(),
            error: DisplayLine::none(),
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Open the wizard fresh: step 1 (provider picker), filter/focus/scroll
    /// and the slug field reset, nothing picked yet.
    pub(crate) fn open(&mut self) {
        self.open = true;
        self.reset();
    }

    /// Dismiss the wizard entirely (Esc on the provider step), discarding any
    /// partial state.
    pub(crate) fn close(&mut self) {
        self.open = false;
        self.reset();
    }

    /// Reset every field the wizard owns.  Shared by the open (fresh start)
    /// and close (discard) paths so the two can never drift apart.
    fn reset(&mut self) {
        self.step = AccountWizardStep::Provider;
        self.filter = InputBuffer::new();
        self.focused = 0;
        self.scroll = 0;
        self.picked_slug = None;
        self.picked_name = None;
        self.slug = TextState::default();
        self.error.clear();
    }

    /// Providers matching the current filter (case-insensitive substring over
    /// display names).  Borrows from `providers` (the live list on `App`); an
    /// empty query returns every provider.
    pub(crate) fn filtered<'a>(&self, providers: &'a [ProviderInfo]) -> Vec<&'a ProviderInfo> {
        let needle = self.filter.text.to_lowercase();
        if needle.is_empty() {
            return providers.iter().collect();
        }
        providers
            .iter()
            .filter(|p| filter_matches(&p.display_name, &needle))
            .collect()
    }

    /// Number of providers matching the current filter — the count only,
    /// without building the filtered list.  Navigation (arrows/wheel) and
    /// clamping need just the length, so they skip the per-press Vec that
    /// `filtered()` would allocate (the renderer and click handler still use
    /// `filtered()` because they need the rows).
    pub(crate) fn filtered_len(&self, providers: &[ProviderInfo]) -> usize {
        let needle = self.filter.text.to_lowercase();
        if needle.is_empty() {
            return providers.len();
        }
        providers
            .iter()
            .filter(|p| filter_matches(&p.display_name, &needle))
            .count()
    }

    /// Clamp `focused` and `scroll` against the filtered provider list length.
    /// Called after every filter mutation so the highlight never points past
    /// the end of a narrowed list.
    pub(crate) fn clamp_focus(&mut self, providers: &[ProviderInfo]) {
        let len = self.filtered_len(providers);
        if len == 0 {
            self.focused = 0;
            self.scroll = 0;
            return;
        }
        if self.focused >= len {
            self.focused = len - 1;
        }
        // Clamp `scroll` to the true max_scroll (`len - viewport height`),
        // not `len - 1`: a hint left past the bottom of the window used to
        // make the click handler select a different row than the renderer
        // drew (a filter narrowing or a PgUp/PgDn jump left it stale past the
        // new max_scroll).  `.max(1)` keeps the pre-first-frame fallback
        // (viewport height unknown, 0) at the old `len - 1` behaviour so the
        // hint can never index past the last row.
        self.scroll = self
            .scroll
            .min(len.saturating_sub(self.viewport_height.max(1)));
    }

    pub(crate) fn move_up(&mut self, providers: &[ProviderInfo]) {
        // Arrow/↑ and wheel-up share the pin-at-middle scroll behavior with
        // the model selector (see `step_focus`): while the viewport height is
        // known the highlight pins at the middle row and the list scrolls
        // under it; at the top edge it un-pins and walks to row 0.  When the
        // height is 0 (before the first frame) it degrades to today's
        // focus-only move.  `providers` is the live list, used to compute the
        // filtered length (mirroring `move_down`).
        let len = self.filtered_len(providers);
        (self.focused, self.scroll) =
            step_focus(self.focused, self.scroll, len, self.viewport_height, false);
    }

    pub(crate) fn move_down(&mut self, providers: &[ProviderInfo]) {
        let len = self.filtered_len(providers);
        (self.focused, self.scroll) =
            step_focus(self.focused, self.scroll, len, self.viewport_height, true);
    }

    /// Page the highlight up/down by `PROVIDER_PAGE_LINES` rows (PgUp/PgDn);
    /// the render window follows the focus.  Like `move_up`, paging up cannot
    /// drift past the top of the list, so no clamp is needed.
    pub(crate) fn page_up(&mut self) {
        self.focused = self.focused.saturating_sub(PROVIDER_PAGE_LINES);
    }

    pub(crate) fn page_down(&mut self, providers: &[ProviderInfo]) {
        let len = self.filtered_len(providers);
        if len == 0 {
            return;
        }
        self.focused = (self.focused + PROVIDER_PAGE_LINES).min(len - 1);
    }

    /// Route a key to the filter input and re-clamp focus against the
    /// narrowed/expanded filtered list.  Enter/Esc are handled by the modal
    /// event handler before this is reached, so the key either edits the
    /// filter or is ignored — nothing needs to be returned to the caller.
    pub(crate) fn filter_key(&mut self, key: KeyEvent, providers: &[ProviderInfo]) {
        if self.filter.handle_key(key) {
            self.clamp_focus(providers);
        }
    }

    /// Compute the `(start, count)` slice of the filtered provider list to
    /// render for a window of `height` rows, keeping the focused row visible.
    /// Pure (`&self`): render must never mutate focus state during `draw()`, so
    /// repeated calls with the same inputs return identical results.
    ///
    /// Takes the already-filtered list — the renderer filters once and reuses
    /// the slice for both the window and the row loop, so filtering is not
    /// repeated per call.
    pub(crate) fn window(&self, filtered: &[&ProviderInfo], height: usize) -> (usize, usize) {
        // The window arithmetic is shared with the model selector
        // (`picker_window`); both pickers use identical focus-tracking rules.
        picker_window(self.scroll, self.focused, filtered.len(), height)
    }

    /// The highlighted provider, if the filtered list is non-empty.
    pub(crate) fn highlighted<'a>(
        &self,
        providers: &'a [ProviderInfo],
    ) -> Option<&'a ProviderInfo> {
        self.filtered(providers).get(self.focused).copied()
    }

    /// Pick the highlighted provider and advance to step 2 (slug entry).
    /// Snapshot the slug + display name so later steps don't depend on the
    /// live list.
    pub(crate) fn confirm_provider(&mut self, providers: &[ProviderInfo]) {
        let Some(picked) = self.highlighted(providers) else {
            return;
        };
        self.picked_slug = Some(picked.slug.clone());
        self.picked_name = Some(picked.display_name.clone());
        self.step = AccountWizardStep::Slug;
        self.slug = TextState::default();
        self.error.clear();
        self.slug.focus();
    }

    /// Seed the name field with a suggested account name, cursor at the end
    /// so typing extends it and Backspace edits it.
    pub(crate) fn prefill_slug(&mut self, suggestion: String) {
        self.slug = TextState::default().with_value(suggestion);
        self.slug.move_end();
        self.slug.focus();
    }

    /// Back out of step 2 (slug entry) to step 1 (provider picker), keeping
    /// the previously picked provider highlighted (re-clamped in case a
    /// catalog refresh changed the list while the slug modal was up).
    pub(crate) fn back_to_provider(&mut self, providers: &[ProviderInfo]) {
        self.step = AccountWizardStep::Provider;
        self.slug = TextState::default();
        self.error.clear();
        self.clamp_focus(providers);
    }
}

#[cfg(test)]
mod tests {
    use super::AccountWizardState;
    use crate::state::ProviderInfo;

    #[test]
    fn wizard_filtered_len_matches_filtered_count() {
        let providers = vec![
            ProviderInfo {
                slug: "a".into(),
                display_name: "Alpha".into(),
            },
            ProviderInfo {
                slug: "b".into(),
                display_name: "Beta".into(),
            },
            ProviderInfo {
                slug: "c".into(),
                display_name: "Alphabet Soup".into(),
            },
        ];
        let mut wizard = AccountWizardState::new();
        assert_eq!(wizard.filtered_len(&providers), 3);
        wizard.filter.text = "alp".to_string();
        assert_eq!(
            wizard.filtered_len(&providers),
            2,
            "case-insensitive substring"
        );
        assert_eq!(
            wizard.filtered(&providers).len(),
            2,
            "agrees with the built list"
        );
    }
}
