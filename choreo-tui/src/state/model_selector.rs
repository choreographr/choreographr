//! The model-selector popup state (Chat page, Alt+M): the `ModelSelectorState`
//! the picker renders from, plus its warming-retry budget constants.
//!
//! The selector lists the models available on the attached session's account
//! (requested from the daemon via `ClientMessageType::ListModels`) and lets the
//! user pick one with the keyboard. It is a sibling of `state/pages.rs` — which
//! keeps the new-account wizard's provider picker — because the two pickers are
//! separate concerns that only share geometry: [`picker_window`],
//! [`step_focus`], [`PROVIDER_PAGE_LINES`], and [`filter_matches`] stay in
//! `pages.rs` and are used here unchanged, so both popups track focus and
//! scroll identically.
//!
//! The state is deliberately free of I/O: it holds what the daemon reported and
//! what the user has typed, and every decision about *when* to ask the daemon
//! again is the event loop's (which waits on `retry_timer` like any other
//! event, never polling).

use super::InputBuffer;
use super::pages::{PROVIDER_PAGE_LINES, filter_matches, picker_window, step_focus};
use crossterm::event::KeyEvent;
use std::time::{Duration, Instant};

/// State for the model-selector popup (Chat page, Alt+M).
///
/// The selector lists the models available on the attached session's account
/// (fetched from the daemon via `ClientMessageType::ListModels`) and lets the user
/// pick one with the keyboard.  The filter is a plain case-insensitive
/// substring match over model IDs — no fuzzy matching.  The currently active
/// model is highlighted with a `●` marker so the user can see where they are
/// before committing.
pub(crate) struct ModelSelectorState {
    /// Whether the popup is currently visible.
    pub(crate) open: bool,
    /// True between `open()` and the arrival of the `Models`/`ModelsFailed`
    /// reply; the popup shows a "loading" row instead of the list.
    pub(crate) loading: bool,
    /// All models for the attached session's account, in the daemon's order
    /// (case-insensitively by id — the daemon sorts at the source so `Models`,
    /// `ModelsUpdated` and `/model` all agree).
    pub(crate) all_models: Vec<String>,
    /// The currently active model (marked with `●`), if known.
    pub(crate) selected: Option<String>,
    /// Filter text; reuses `InputBuffer` so editing is grapheme-aware and
    /// consistent with the main command input.
    pub(crate) filter: InputBuffer,
    /// Index into the *filtered* list of the highlighted row.
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
    /// Popup-scoped error text (e.g. the daemon failed to list models).
    pub(crate) error: Option<String>,
    /// True while the painted list is a stand-in the daemon could not verify
    /// for this account: a stale cache entry or the local catalog's curated set
    /// for the provider, with the live fetch still running (or failed). The
    /// renderer says so, so a seeded list is never mistaken for the live one.
    /// A `ModelsUpdated` broadcast clears it.
    pub(crate) provisional: bool,
    /// Fires when the list should be requested again: armed while the daemon
    /// reports the account's list as still warming (a fresh account's first
    /// fetch runs in the background). A `crossbeam_channel::after` timer, so
    /// the UI loop waits on it like any other event instead of polling.
    pub(crate) retry_timer: Option<crossbeam_channel::Receiver<Instant>>,
    /// Warming retries spent since the popup opened, capped at
    /// [`MODEL_LIST_MAX_RETRIES`] so a fetch that never lands ends in the
    /// daemon's own message instead of a spinner that never stops.
    pub(crate) retries: u32,
}

/// Delay between model-list requests while the daemon's background fetch is
/// still running. A provider's model endpoint usually answers in well under
/// a second.
pub(crate) const MODEL_LIST_RETRY_DELAY: Duration = Duration::from_millis(750);

/// Most warming retries per popup opening (about 15 s in total) — longer
/// than the daemon's own fetch takes to succeed or fall back.
pub(crate) const MODEL_LIST_MAX_RETRIES: u32 = 20;

impl ModelSelectorState {
    pub(crate) fn new() -> Self {
        Self {
            open: false,
            loading: false,
            all_models: Vec::new(),
            selected: None,
            filter: InputBuffer::new(),
            focused: 0,
            scroll: 0,
            viewport_height: 0,
            error: None,
            provisional: false,
            retry_timer: None,
            retries: 0,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Handle a `ModelsFailed` reply that says the list is still warming:
    /// keep showing "loading" and arm the retry timer, or — once the retry
    /// budget is spent — show the error like any other failure. Returns
    /// whether a retry was scheduled.
    pub(crate) fn schedule_warming_retry(&mut self, error: impl Into<String>) -> bool {
        if !self.open || self.retries >= MODEL_LIST_MAX_RETRIES {
            self.apply_error(error);
            return false;
        }
        self.retries += 1;
        self.loading = true;
        self.error = None;
        self.retry_timer = Some(crossbeam_channel::after(MODEL_LIST_RETRY_DELAY));
        true
    }

    /// Take the fired retry timer. Returns whether the list should be
    /// requested again — only while the popup is still open.
    pub(crate) fn take_retry(&mut self) -> bool {
        self.retry_timer.take().is_some() && self.open
    }

    /// Open the selector and request a fresh model list.
    ///
    /// The caller is responsible for sending `ClientMessageType::ListModels`;
    /// `loading` stays true until `apply_models` or `apply_error` arrives.
    /// The previous filter/focus is discarded so each opening starts clean.
    pub(crate) fn open(&mut self) {
        self.open = true;
        self.loading = true;
        self.error = None;
        self.retry_timer = None;
        self.retries = 0;
        self.filter.clear();
        self.focused = 0;
        self.scroll = 0;
    }

    /// Dismiss the popup.  Keeps `all_models` so a re-open shows results
    /// immediately while the fresh `ListModels` reply is in flight.
    pub(crate) fn close(&mut self) {
        self.open = false;
        self.loading = false;
        self.error = None;
        self.retry_timer = None;
    }

    /// Populate the model list from a `Models` reply and preselect the
    /// current model.  `selected` falls back to `None` when the daemon does
    /// not report one; the caller may pass the display's cached value.
    /// `provisional` marks the list as an unverified stand-in (see
    /// [`Self::provisional`]).
    pub(crate) fn apply_models(
        &mut self,
        models: Vec<String>,
        selected: Option<String>,
        provisional: bool,
    ) {
        self.selected = selected;
        self.install_models(models, provisional);
        // Preselect the active model when it survives the current filter;
        // otherwise the highlight stays at the top of the list.
        if let Some(sel) = &self.selected
            && let Some(idx) = self.filtered().iter().position(|m| m == sel)
        {
            self.focused = idx;
            self.scroll = idx;
        }
        self.clamp_focus();
    }

    /// Replace the painted list after a LIVE refresh (the daemon's
    /// `ModelsUpdated` broadcast) without moving the user's cursor.
    ///
    /// `apply_models` derives the highlight from the ACTIVE model, which is
    /// right for a first paint but wrong for a refresh: the active model is not
    /// where the user is looking. So this keeps the highlighted row's model
    /// selected across the swap (re-finding it in the new list) and leaves the
    /// scroll window alone; a row that disappeared clamps, rather than jumping
    /// to the active model. A refreshed list is always verified, so the
    /// provisional flag clears.
    pub(crate) fn refresh_models(&mut self, models: Vec<String>) {
        let highlighted = self.highlighted();
        self.install_models(models, false);
        if let Some(idx) = highlighted
            .as_deref()
            .and_then(|id| self.filtered().iter().position(|m| *m == id))
        {
            self.focused = idx;
        }
        self.clamp_focus();
    }

    /// The shared body of `apply_models`/`refresh_models`: install a new list
    /// and drop the per-request state that the arrival of a list makes moot
    /// (loading row, error text, armed warming retry). Focus/scroll staying
    /// put is what the two callers differ on, so it is not touched here.
    fn install_models(&mut self, models: Vec<String>, provisional: bool) {
        self.all_models = models;
        self.provisional = provisional;
        self.loading = false;
        self.error = None;
        // The list landed, so any armed warming retry is moot: disarming it
        // here stops the UI loop from firing one more `ListModels` after a
        // success that raced the timer.
        self.retry_timer = None;
    }

    /// Record a `ModelsFailed` reply so the popup can show the error inline.
    pub(crate) fn apply_error(&mut self, error: impl Into<String>) {
        self.loading = false;
        self.error = Some(error.into());
        // A terminal error ends the warming retry: an armed timer here would
        // re-request a list the daemon has already refused.
        self.retry_timer = None;
    }

    /// Models matching the current filter (case-insensitive substring).
    /// Borrows from `all_models`; an empty query returns every model.
    pub(crate) fn filtered(&self) -> Vec<&str> {
        let needle = self.filter.text.to_lowercase();
        if needle.is_empty() {
            return self.all_models.iter().map(String::as_str).collect();
        }
        self.all_models
            .iter()
            .filter(|m| filter_matches(m, &needle))
            .map(String::as_str)
            .collect()
    }

    /// Number of models matching the current filter — the count only,
    /// without building the filtered list.  Navigation (arrows/wheel) and
    /// clamping need just the length, so they skip the per-press Vec that
    /// `filtered()` would allocate (the renderer and click handler still use
    /// `filtered()` because they need the rows).
    pub(crate) fn filtered_len(&self) -> usize {
        let needle = self.filter.text.to_lowercase();
        if needle.is_empty() {
            return self.all_models.len();
        }
        self.all_models
            .iter()
            .filter(|m| filter_matches(m, &needle))
            .count()
    }

    /// Clamp `focused` and `scroll` against the filtered list length.
    /// Called after every filter mutation so the highlight never points
    /// past the end of a narrowed list.
    pub(crate) fn clamp_focus(&mut self) {
        let len = self.filtered_len();
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

    pub(crate) fn move_up(&mut self) {
        // Arrow/↑ and wheel-up share the pin-at-middle scroll behavior with
        // the wizard's provider picker (see `step_focus`): while the viewport
        // height is known the highlight pins at the middle row and the list
        // scrolls under it; at the top edge it un-pins and walks to row 0.
        // When the height is 0 (before the first frame) it degrades to a
        // focus-only move.
        let len = self.filtered_len();
        (self.focused, self.scroll) =
            step_focus(self.focused, self.scroll, len, self.viewport_height, false);
    }

    pub(crate) fn move_down(&mut self) {
        let len = self.filtered_len();
        (self.focused, self.scroll) =
            step_focus(self.focused, self.scroll, len, self.viewport_height, true);
    }

    /// Page the highlight up by `PROVIDER_PAGE_LINES` rows (`PgUp`); the render
    /// window follows the focus.  Like [`Self::move_up`], paging up cannot
    /// drift past the top of the list, so no clamp is needed.
    pub(crate) fn page_up(&mut self) {
        self.focused = self.focused.saturating_sub(PROVIDER_PAGE_LINES);
    }

    pub(crate) fn page_down(&mut self) {
        let len = self.filtered_len();
        if len == 0 {
            return;
        }
        self.focused = (self.focused + PROVIDER_PAGE_LINES).min(len - 1);
    }

    /// Route a key to the filter input and re-clamp focus against the
    /// narrowed/expanded filtered list.  Enter/Esc are handled by the modal
    /// event handler before this is reached, so the key either edits the
    /// filter or is ignored — nothing needs to be returned to the caller.
    pub(crate) fn filter_key(&mut self, key: KeyEvent) {
        if self.filter.handle_key(key) {
            self.clamp_focus();
        }
    }

    /// Compute the `(start, count)` slice of the filtered list to render for
    /// a window of `height` rows, keeping the focused row visible.
    ///
    /// The arithmetic itself lives in the shared `picker_window` helper (both
    /// pickers use identical focus-tracking rules); the renderer just draws
    /// `filtered[start..start + count]`.  It is deliberately **pure** (takes
    /// `&self`): render must never mutate scroll/focus state — that happens in
    /// the event loop before `terminal.draw()` (see the module docs in
    /// render/mod.rs).  `scroll` is used only as a hint and is corrected
    /// locally, so repeated calls return identical results.
    ///
    /// Takes the already-filtered list — the renderer filters once and reuses
    /// the slice for both the window and the row loop, so the filter is not
    /// re-applied per call.
    pub(crate) fn window(&self, filtered: &[&str], height: usize) -> (usize, usize) {
        // The window arithmetic is shared with the wizard's provider picker
        // (`picker_window`); both pickers use identical focus-tracking rules.
        picker_window(self.scroll, self.focused, filtered.len(), height)
    }

    /// The highlighted model ID, if the filtered list is non-empty.
    pub(crate) fn highlighted(&self) -> Option<String> {
        self.filtered()
            .get(self.focused)
            .copied()
            .map(str::to_string)
    }

    /// Return the highlighted model and close the selector.  The caller
    /// sends `SetModel` with the returned value.
    pub(crate) fn submit(&mut self) -> Option<String> {
        let model = self.highlighted();
        self.close();
        model
    }
}
