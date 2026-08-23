//! Page/overlay state: the `Page` enum, session-manager list/detail state
//! (`SessionManagerState`, `SessionDetailData`, `Marker`), the AI-providers
//! page (`AIProvidersState`, `AccountWizardState`, `CredentialModalState`),
//! and the model-selector popup (`ModelSelectorState`), plus the page-layout
//! constants they share.

use super::{InputBuffer, PAGE_SCROLL_LINES, ProviderInfo};
use choreo_proto::{AccountInfo, SessionStatus, SessionSummary, TokenUsage};
use crossterm::event::KeyEvent;
use tui_prompts::{State, TextState};
use zeroize::Zeroize;

pub(crate) const AI_PROVIDER_ITEM_LINES: usize = 4;

/// The two pickers' shared case-insensitive substring predicate: does `s`
/// contain the already-lowercased `needle`?  Both `filtered()` methods and
/// their `filtered_len()` count helpers apply the same predicate, so a
/// filter change can never behave differently between rendering and
/// navigation.
fn filter_matches(s: &str, needle: &str) -> bool {
    s.to_lowercase().contains(needle)
}

/// Rows a single PgUp/PgDn press jumps in the new-account wizard's provider
/// picker (step 1).  The render window always follows the focus, so paging the
/// focus is what actually scrolls the list.
pub(crate) const PROVIDER_PAGE_LINES: usize = 10;

/// Shared pure window arithmetic for the two picker popups (the wizard's
/// provider picker and the model selector): compute the `(start, count)` slice
/// of a `len`-item list to render in a window of `height` rows, keeping the
/// `focused` row visible.  `scroll` is a hint only and is corrected locally
/// (clamped to the valid range, pulled up when focus drifts above the window,
/// pushed down when focus falls below the fold), so repeated calls with the
/// same inputs return identical results — render can never mutate focus/scroll
/// state during `draw()`.
fn picker_window(scroll: usize, focused: usize, len: usize, height: usize) -> (usize, usize) {
    if len == 0 || height == 0 {
        return (0, 0);
    }
    let focused = focused.min(len - 1);
    let max_scroll = len.saturating_sub(height);
    let mut scroll = scroll.min(max_scroll);
    if focused < scroll {
        // Focus drifted above the window (e.g. after a filter that shrunk
        // the list) — pull the window up.
        scroll = focused;
    } else if focused >= scroll + height {
        // Focus is below the fold — push the window down.
        scroll = focused + 1 - height;
    }
    (scroll, height.min(len - scroll))
}

/// Shared pin-at-middle arrow/wheel navigation for the two picker popups (the
/// wizard's provider picker and the model selector): move the highlight one
/// row toward the top/bottom of a `len`-item list rendered in a window of
/// `height` rows, returning the new `(focused, scroll)` pair.
///
/// ↓ walks the highlight down through a static window; once it reaches the
/// middle row (`height / 2`) further presses keep it pinned there while
/// `scroll` increments — the list slides under the highlight.  At the bottom
/// edge (`scroll == max_scroll = len.saturating_sub(height)`) it un-pins and
/// the highlight walks the rest of the way to the last item.  ↑ is the exact
/// mirror: pinned at the middle while `scroll > 0` (decrementing `scroll`),
/// un-pinned at `scroll == 0` to walk up to row 0.  When `height == 0`
/// (viewport unknown — 0 until the first frame) it falls back to focus-only
/// moves with the render-time `picker_window` clamp, preserving the original
/// navigation; when the list fits the window (`len <= height`, so
/// `max_scroll == 0`) the plain `picker_window` behavior applies (highlight
/// walks edge to edge, window static).  `focused`/`scroll` are hints only — a
/// stale pair is clamped locally, exactly like `picker_window` (`scroll` to
/// the true `max_scroll`, symmetrically for both directions).
fn step_focus(
    focused: usize,
    scroll: usize,
    len: usize,
    height: usize,
    down: bool,
) -> (usize, usize) {
    if len == 0 {
        // Nothing to focus: reset the pair so a stale highlight cannot linger
        // on a list the filter just emptied.
        return (0, 0);
    }
    if height == 0 {
        // Viewport unknown: focus-only moves, `picker_window` keeps the
        // highlight visible at render time.
        return if down {
            if focused + 1 < len {
                (focused + 1, scroll)
            } else {
                (focused, scroll)
            }
        } else if focused > 0 {
            (focused - 1, scroll)
        } else {
            (focused, scroll)
        };
    }
    // Clamp a stale pair before stepping (a filter can narrow the list under
    // a highlight that pointed past the end; `clamp_focus` normally does this
    // at mutation time, this is the pure-function safety net).  `scroll` is
    // clamped to the true max_scroll *symmetrically* for both directions: a
    // hint left past the bottom edge must not make ↑ decrement it one press
    // at a time while ↓ clamps it in one step — both treat the window as
    // already pinned at the edge the hint points past.
    let focused = focused.min(len - 1);
    let max_scroll = len.saturating_sub(height);
    let scroll = scroll.min(max_scroll);
    let middle = height / 2;
    if down {
        if focused + 1 >= len {
            // Already at the last item.
            return (focused, scroll);
        }
        if scroll >= max_scroll {
            // Window pinned at the bottom edge: un-pin and walk the highlight
            // down (clamping a stale scroll hint to the valid range).
            (focused + 1, max_scroll)
        } else if focused < scroll + middle {
            // Highlight above the middle row: walk down inside the static
            // window.
            (focused + 1, scroll)
        } else {
            // Pinned at (or below) the middle row: the list slides under the
            // highlight, which stays at the same visual offset.
            (focused + 1, scroll + 1)
        }
    } else {
        if focused == 0 {
            // Already at the first item.
            return (focused, scroll);
        }
        if scroll == 0 {
            // Window pinned at the top edge: un-pin and walk the highlight up.
            (focused - 1, scroll)
        } else if focused > scroll + middle {
            // Highlight below the middle row: walk up inside the static
            // window.
            (focused - 1, scroll)
        } else {
            // Pinned at (or above) the middle row: the list slides under the
            // highlight, which stays at the same visual offset.
            (focused - 1, scroll - 1)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Chat,
    SessionManager,
    AIProviders,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionManagerView {
    List,
    Detail,
}

/// Steps of the new-account wizard modal (AI providers page, `n`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountWizardStep {
    /// Step 1: pick a provider from the searchable list.
    Provider,
    /// Step 2: enter the account slug (a separate modal).
    Slug,
}

pub(crate) struct AIProvidersState {
    pub(crate) accounts: Vec<AccountInfo>,
    pub(crate) selection: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) confirm_remove: Option<String>,
    /// The new-account wizard modal (`n`): searchable provider picker, then
    /// slug entry.
    pub(crate) wizard: AccountWizardState,
    /// The API-key modal (`c`, or auto-opened right after account creation).
    pub(crate) credential: CredentialModalState,
}

impl AIProvidersState {
    pub(crate) fn new() -> Self {
        Self {
            accounts: Vec::new(),
            selection: None,
            scroll: 0,
            confirm_remove: None,
            wizard: AccountWizardState::new(),
            credential: CredentialModalState::new(),
        }
    }

    pub(crate) fn set_accounts(&mut self, accounts: Vec<AccountInfo>) {
        let was_selected = self
            .selection
            .and_then(|i| self.accounts.get(i))
            .map(|a| a.name.clone());
        self.accounts = accounts;
        self.selection = if self.accounts.is_empty() {
            None
        } else {
            let idx = was_selected
                .and_then(|name| self.accounts.iter().position(|a| a.name == name))
                .unwrap_or(0);
            Some(idx)
        };
        self.scroll = 0;
    }

    pub(crate) fn select_up(&mut self) {
        let sel = self.selection.unwrap_or(0);
        if sel > 0 {
            self.selection = Some(sel - 1);
            if sel - 1 < self.scroll {
                self.scroll = self.scroll.saturating_sub(1);
            }
        }
    }

    pub(crate) fn select_down(&mut self) {
        let max = self.accounts.len().saturating_sub(1);
        let sel = self.selection.unwrap_or(0);
        if sel < max {
            self.selection = Some(sel + 1);
        }
    }

    pub(crate) fn scroll_up_page(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub(crate) fn scroll_down_page(&mut self) {
        if !self.accounts.is_empty() {
            let max_scroll = self.accounts.len().saturating_sub(1);
            self.scroll = (self.scroll + 1).min(max_scroll);
        }
    }

    pub(crate) fn remove_account(&mut self, name: &str) {
        let old_len = self.accounts.len();
        self.accounts.retain(|a| a.name != name);
        if self.accounts.len() == old_len {
            return;
        }
        if let Some(sel) = self.selection
            && sel >= self.accounts.len()
        {
            self.selection = if self.accounts.is_empty() {
                None
            } else {
                Some(self.accounts.len().saturating_sub(1))
            };
        }
        let max_scroll = self.accounts.len().saturating_sub(1);
        self.scroll = self.scroll.min(max_scroll);
        if self.confirm_remove.as_deref() == Some(name) {
            self.confirm_remove = None;
        }
    }
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
    /// Wizard-scoped error text (e.g. slug validation failures).
    pub(crate) error: Option<String>,
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
            error: None,
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
        self.error = None;
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
        self.error = None;
        self.slug.focus();
    }

    /// Back out of step 2 (slug entry) to step 1 (provider picker), keeping
    /// the previously picked provider highlighted (re-clamped in case a
    /// catalog refresh changed the list while the slug modal was up).
    pub(crate) fn back_to_provider(&mut self, providers: &[ProviderInfo]) {
        self.step = AccountWizardStep::Provider;
        self.slug = TextState::default();
        self.error = None;
        self.clamp_focus(providers);
    }
}

/// The API-key modal (AI providers page).  Open ⇔ `target` is `Some(account
/// name)`.  Reached from `c` on an existing account or auto-opened right after
/// the new-account wizard creates one.  The key is masked while typing and
/// encrypted with the daemon's identity key on save (see
/// `build_add_credential_message`).
pub(crate) struct CredentialModalState {
    /// The account the key is for; `Some` means the modal is open.
    pub(crate) target: Option<String>,
    pub(crate) input: InputBuffer,
    pub(crate) error: Option<String>,
}

impl CredentialModalState {
    pub(crate) fn new() -> Self {
        Self {
            target: None,
            input: InputBuffer::new(),
            error: None,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.target.is_some()
    }

    pub(crate) fn open(&mut self, account_name: String) {
        self.target = Some(account_name);
        self.wipe_input();
        self.error = None;
    }

    pub(crate) fn close(&mut self) {
        self.target = None;
        self.wipe_input();
        self.error = None;
    }

    /// Discard the typed key and wipe its bytes from the input buffer's heap
    /// allocation before the `String` is dropped.  The daemon already zeroizes
    /// its own stored `ServiceCredential` copies (choreo-keystore's
    /// `#[zeroize(drop)]` on `ServiceCredential` plus the daemon's explicit key
    /// wipes); this covers the TUI's transient copy of a pasted/typed API key,
    /// which the daemon never sees.  Called by the modal's own open/close and
    /// by the connection layer's Enter handler.
    pub(crate) fn wipe_input(&mut self) {
        // Take the typed key's String out and convert it to its backing
        // `Vec<u8>` so the WHOLE heap allocation can be zeroized — including
        // the spare capacity beyond `len`, which may hold remnants of a longer
        // key deleted while editing (`String::zeroize()` only covers the
        // current length).  Growing to `capacity` with zeros first makes the
        // subsequent `zeroize()` reach every byte, so the freed allocation is
        // all zeros rather than a mix of zeros and stale key material.
        let mut bytes = std::mem::take(&mut self.input.text).into_bytes();
        bytes.resize(bytes.capacity(), 0);
        bytes.zeroize();
        self.input = InputBuffer::new();
    }
}

pub(crate) struct SessionDetailData {
    pub(crate) session_id: u64,
    pub(crate) title: String,
    pub(crate) selected_model: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) parent_session_id: Option<u64>,
    pub(crate) working_dir: String,
    pub(crate) created_at: i64,
    pub(crate) last_modified: i64,
    pub(crate) turn_count: u32,
    pub(crate) status: SessionStatus,
    pub(crate) active_tool_groups: Vec<String>,
    pub(crate) account_name: Option<String>,
    pub(crate) accumulated_usage: Option<TokenUsage>,
    pub(crate) context_window: Option<u32>,
    pub(crate) last_prompt_tokens: Option<u32>,
}

pub(crate) struct SessionManagerState {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) view: SessionManagerView,
    pub(crate) selection: Option<usize>,
    /// Session id to highlight on the next list refresh.  `select_session`
    /// records this when navigating to the session manager (Ctrl+S) so the
    /// freshly fetched list lands on the session the user was just viewing
    /// — even on the first visit, before the daemon's ListSessions reply has
    /// (re)populated `sessions`.
    pub(crate) pending_select: Option<u64>,
    /// Index of the first visible session row.  Navigation shifts this
    /// anchor directionally — only when the selection would leave the
    /// window — which is what keeps the list from scrolling back up
    /// immediately after scrolling down (see [`SessionManagerState::window`]).
    pub(crate) scroll: usize,
    /// Session-row viewport height, cached by `update_viewport_from_terminal_size`
    /// (outside the draw closure) so navigation can decide when to shift
    /// `scroll`.  0 until the first frame; navigation defers shifts then.
    pub(crate) viewport_height: usize,
    pub(crate) detail_data: Option<SessionDetailData>,
    pub(crate) confirm_delete: Option<(u64, String)>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Marker {
    pub content_line: usize,
    pub virtual_slot: usize,
}

/// State for the model-selector popup (Chat page, Ctrl+M).
///
/// The selector lists the models available on the attached session's account
/// (fetched from the daemon via `ClientMessage::ListModels`) and lets the user
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
    /// All models for the attached session's account, in daemon order.
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
}

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
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Open the selector and request a fresh model list.
    ///
    /// The caller is responsible for sending `ClientMessage::ListModels`;
    /// `loading` stays true until `apply_models` or `apply_error` arrives.
    /// The previous filter/focus is discarded so each opening starts clean.
    pub(crate) fn open(&mut self) {
        self.open = true;
        self.loading = true;
        self.error = None;
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
    }

    /// Populate the model list from a `Models` reply and preselect the
    /// current model.  `selected` falls back to `None` when the daemon does
    /// not report one; the caller may pass the display's cached value.
    pub(crate) fn apply_models(&mut self, models: Vec<String>, selected: Option<String>) {
        self.all_models = models;
        self.selected = selected;
        self.loading = false;
        self.error = None;
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

    /// Record a `ModelsFailed` reply so the popup can show the error inline.
    pub(crate) fn apply_error(&mut self, error: impl Into<String>) {
        self.loading = false;
        self.error = Some(error.into());
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

    /// Page the highlight up by `PROVIDER_PAGE_LINES` rows (PgUp); the render
    /// window follows the focus.  Like `move_up`, paging up cannot drift past
    /// the top of the list, so no clamp is needed.
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
        self.filtered().get(self.focused).map(|s| s.to_string())
    }

    /// Return the highlighted model and close the selector.  The caller
    /// sends `SetModel` with the returned value.
    pub(crate) fn submit(&mut self) -> Option<String> {
        let model = self.highlighted();
        self.close();
        model
    }
}

impl SessionManagerState {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Vec::new(),
            view: SessionManagerView::List,
            selection: None,
            pending_select: None,
            scroll: 0,
            viewport_height: 0,
            detail_data: None,
            confirm_delete: None,
            error: None,
        }
    }

    pub(crate) fn set_sessions(&mut self, sessions: Vec<SessionSummary>) {
        self.error = None;
        // A pending highlight from `select_session` (set when navigating to
        // the session manager) takes priority over the current selection so
        // the fresh list lands on the session the user was just viewing;
        // otherwise keep following the session currently selected.
        let preferred = self.pending_select.or_else(|| {
            self.selection
                .and_then(|i| self.sessions.get(i))
                .map(|s| s.session_id)
        });
        self.sessions = sessions;
        self.sort_by_last_modified();
        self.selection = if self.sessions.is_empty() {
            None
        } else {
            preferred
                .and_then(|id| self.sessions.iter().position(|s| s.session_id == id))
                .unwrap_or(0)
                .into()
        };
        // The preference is one-shot: consume it once it has been applied to
        // a fresh list, so later refreshes fall back to preserving whatever
        // the user has navigated to since.
        self.pending_select = None;
    }

    /// Highlight `session_id` in the list immediately when it is already
    /// loaded, and remember the preference so the next [`Self::set_sessions`]
    /// refresh re-selects it even if the current list is empty or stale
    /// (e.g. the very first visit to the session manager, before the
    /// ListSessions reply has arrived).
    pub(crate) fn select_session(&mut self, session_id: u64) {
        self.pending_select = Some(session_id);
        if let Some(idx) = self
            .sessions
            .iter()
            .position(|s| s.session_id == session_id)
        {
            self.selection = Some(idx);
            // Re-anchor the scroll window so the highlighted row is visible
            // right away rather than waiting for the next navigation step.
            self.reanchor_scroll();
        }
    }

    /// Order sessions newest-first by `last_modified`.  Uses a STABLE sort:
    /// equal timestamps keep whatever order the daemon sent (which is already
    /// id-desc tiebroken in `handle_list_sessions`), so the TUI doesn't need
    /// to re-implement that tiebreak here.
    fn sort_by_last_modified(&mut self) {
        // `sort_by_key` is stable (see the doc comment above); Reverse gives
        // newest-first without a custom comparator.
        self.sessions
            .sort_by_key(|s| std::cmp::Reverse(s.last_modified));
    }

    /// Re-order after a live status change and keep the cursor on the same
    /// session, which may have moved to a new index.
    pub(crate) fn resort_after_status_change(&mut self) {
        let selected_id = self
            .selection
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.session_id);
        self.sort_by_last_modified();
        self.selection =
            selected_id.and_then(|id| self.sessions.iter().position(|s| s.session_id == id));
    }

    pub(crate) fn select_up(&mut self) {
        self.reanchor_scroll();
        let sel = self.selection.unwrap_or(0);
        if sel > 0 {
            let new_sel = sel - 1;
            self.selection = Some(new_sel);
            // Shift the window up only once the selection reaches the top
            // edge of the visible area; until then it climbs freely inside
            // the window.  This is the fix for the list scrolling back up
            // immediately after scrolling down.
            if new_sel < self.scroll {
                self.scroll = new_sel;
            }
        }
    }

    pub(crate) fn select_down(&mut self) {
        self.reanchor_scroll();
        let max = self.sessions.len().saturating_sub(1);
        let sel = self.selection.unwrap_or(0);
        if sel < max {
            let new_sel = sel + 1;
            self.selection = Some(new_sel);
            // Shift the window down only once the selection passes the
            // bottom edge, pinning it to the last visible row.  The
            // viewport height is cached by the renderer; until the first
            // frame it is unknown (0), so navigation defers the shift and
            // `window()` clamps at render time.
            let h = self.viewport_height;
            if h > 0 && new_sel >= self.scroll + h {
                self.scroll = new_sel + 1 - h;
            }
        }
    }

    /// Move the selection up by a page (PgUp).  The render window follows
    /// the selection with the same directional anchoring as `select_up`.
    pub(crate) fn scroll_up_page(&mut self) {
        self.reanchor_scroll();
        if let Some(sel) = self.selection {
            let new_sel = sel.saturating_sub(PAGE_SCROLL_LINES);
            self.selection = Some(new_sel);
            if new_sel < self.scroll {
                self.scroll = new_sel;
            }
        }
    }

    /// Move the selection down by a page (PgDn), clamped to the last row.
    pub(crate) fn scroll_down_page(&mut self) {
        self.reanchor_scroll();
        let max = self.sessions.len().saturating_sub(1);
        if let Some(sel) = self.selection {
            let new_sel = (sel + PAGE_SCROLL_LINES).min(max);
            self.selection = Some(new_sel);
            let h = self.viewport_height;
            if h > 0 && new_sel >= self.scroll + h {
                self.scroll = new_sel + 1 - h;
            }
        }
    }

    /// Sync `scroll` with the window the renderer derives, so directional
    /// shifts always start from what is actually displayed.  Reorders,
    /// removals, terminal resizes, and direct selection changes can leave
    /// the stored anchor stale; re-anchoring from `window()` (which clamps
    /// the anchor to keep the selection visible) resolves that before every
    /// navigation step.  A no-op while the viewport height is unknown.
    fn reanchor_scroll(&mut self) {
        let start = self.window(self.viewport_height).0;
        self.scroll = start;
    }

    /// Compute the `(start, count)` slice of `sessions` to render for a
    /// window of `height` rows.  Pure (`&self`): the renderer must not mutate
    /// focus state during `draw()`, so repeated calls with the same inputs
    /// return identical results.
    ///
    /// The window is anchored on the navigation-maintained [`Self::scroll`]
    /// (the first visible row), clamped only so the highlighted row stays
    /// visible: the window shifts by the minimum amount when the selection
    /// would leave it, and not otherwise.  That directional behaviour is what
    /// lets the user scroll down and then press up without the window
    /// scrolling back immediately.  Reorders, removals and terminal resizes
    /// can leave `scroll` stale; the clamp re-anchors it to the selection.
    pub(crate) fn window(&self, height: usize) -> (usize, usize) {
        let len = self.sessions.len();
        if len == 0 || height == 0 {
            return (0, 0);
        }
        let focused = self.selection.unwrap_or(0).min(len - 1);
        let max_start = len.saturating_sub(height);
        // Pull the window up when the selection is above it (upper bound),
        // push it down when below (lower bound), otherwise keep the anchor.
        let start = self
            .scroll
            .min(max_start)
            .min(focused)
            .max(focused.saturating_add(1).saturating_sub(height));
        (start, height.min(len - start))
    }

    pub(crate) fn enter_detail(&mut self) {
        let sel = self.selection;
        let sum = sel.and_then(|i| self.sessions.get(i));
        self.detail_data = sum.map(|s| {
            let session_id = s.session_id;
            let title = s.title.clone().unwrap_or_else(|| "untitled".to_string());
            let selected_model = s.selected_model.clone().unwrap_or_else(|| "-".to_string());
            let parent_session_id = s.parent_session_id;
            let working_dir = s.working_dir.clone().unwrap_or_else(|| "-".to_string());
            let created_at = s.created_at;
            let last_modified = s.last_modified;
            let turn_count = s.turn_count;
            SessionDetailData {
                session_id,
                title,
                selected_model,
                reasoning_effort: s.reasoning_effort.clone(),
                parent_session_id,
                working_dir,
                created_at,
                last_modified,
                turn_count,
                status: s.status.clone(),
                active_tool_groups: s.active_tool_groups.clone(),
                account_name: s.account_name.clone(),
                accumulated_usage: s.token_usage,
                context_window: s.context_window,
                last_prompt_tokens: s.last_prompt_tokens,
            }
        });
        if self.detail_data.is_some() {
            self.view = SessionManagerView::Detail;
        }
    }

    pub(crate) fn leave_detail(&mut self) {
        self.view = SessionManagerView::List;
        self.detail_data = None;
    }

    pub(crate) fn remove_session(&mut self, id: u64) {
        let old_len = self.sessions.len();
        self.sessions.retain(|s| s.session_id != id);
        let removed = old_len - self.sessions.len();
        if removed == 0 {
            return;
        }
        if let Some(sel) = self.selection
            && sel >= self.sessions.len()
        {
            self.selection = if self.sessions.is_empty() {
                None
            } else {
                Some(self.sessions.len().saturating_sub(1))
            };
        }
        if self
            .detail_data
            .as_ref()
            .is_some_and(|d| d.session_id == id)
        {
            self.view = SessionManagerView::List;
            self.detail_data = None;
        }
        if self.confirm_delete.as_ref().map(|(sid, _)| *sid) == Some(id) {
            self.confirm_delete = None;
        }
    }

    pub(crate) fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountWizardState, ModelSelectorState, ProviderInfo, picker_window, step_focus};

    #[test]
    fn picker_window_empty_list_or_zero_height_returns_zero() {
        // No rows to show and/or no room to show them: the window is empty.
        assert_eq!(picker_window(0, 0, 0, 10), (0, 0), "empty list");
        assert_eq!(picker_window(0, 0, 5, 0), (0, 0), "zero height");
    }

    #[test]
    fn picker_window_focus_below_the_fold_pushes_window_down() {
        // 5 rows in a 3-row window: focus 4 must be the last visible row.
        assert_eq!(picker_window(0, 4, 5, 3), (2, 3));
    }

    #[test]
    fn picker_window_focus_above_the_window_pulls_it_up() {
        // A stale scroll hint points past the focused row — the window snaps
        // up so the focus is the first visible row.
        assert_eq!(picker_window(4, 1, 5, 3), (1, 3));
    }

    #[test]
    fn picker_window_stale_scroll_hint_is_clamped() {
        // `scroll` is a hint only: past the valid range it is clamped locally
        // (the stored field is never written back by render).
        assert_eq!(picker_window(10, 4, 5, 3), (2, 3));
    }

    #[test]
    fn picker_window_count_never_exceeds_the_list_tail() {
        // A window taller than the list renders the whole list.
        assert_eq!(picker_window(0, 0, 2, 10), (0, 2));
        assert_eq!(picker_window(0, 1, 2, 10), (0, 2));
    }

    #[test]
    fn picker_window_focus_is_always_inside_the_window() {
        // The invariant the two pickers rely on: the focused row is always
        // visible for any (scroll, focused, len, height) combination.
        for &(scroll, focused, len, height) in &[
            (0, 0, 1, 1),
            (3, 4, 5, 3),
            (1, 4, 5, 3),
            (2, 0, 5, 3),
            (4, 4, 5, 3),
            (9, 9, 10, 4),
        ] {
            let (start, count) = picker_window(scroll, focused, len, height);
            assert!(count > 0, "non-empty list must render rows");
            assert!(
                (start..start + count).contains(&focused.min(len - 1)),
                "focus must stay visible for ({scroll}, {focused}, {len}, {height})"
            );
        }
    }

    // ── step_focus (pin-at-middle arrow/wheel navigation) ───────────────

    #[test]
    fn step_focus_pins_at_middle_going_down() {
        // 20 items in a 10-row window: the middle row is 5.  The first five
        // ↓ presses walk the highlight down through the static window…
        let mut focused = 0usize;
        let mut scroll = 0usize;
        for _ in 0..5 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!(
            (focused, scroll),
            (5, 0),
            "highlight reaches the middle row"
        );

        // …the next five keep it pinned there while the list slides under it
        // (focused − scroll stays 5).
        for _ in 0..5 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!(
            (focused, scroll),
            (10, 5),
            "pinned at the middle while scrolling"
        );

        // Still pinned when scroll reaches max_scroll (20 − 10 = 10).
        for _ in 0..5 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!(
            (focused, scroll),
            (15, 10),
            "max_scroll reached, still pinned"
        );

        // At the bottom edge it un-pins and walks to the last item.
        for _ in 0..4 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!((focused, scroll), (19, 10), "walked to the last item");

        // Further presses are no-ops.
        (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        assert_eq!((focused, scroll), (19, 10), "no-op at the last item");
    }

    #[test]
    fn step_focus_un_pins_at_bottom_edge() {
        // When the window is already pinned at max_scroll, ↓ moves the
        // highlight without scrolling — walking from the middle row to the
        // last item.
        let mut focused = 5usize;
        let mut scroll = 10usize; // max_scroll for 20 items in 10 rows
        for _ in 0..14 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!((focused, scroll), (19, 10));
    }

    #[test]
    fn step_focus_mirror_going_up() {
        // Mirror of the down walk: start at the last item with the window at
        // max_scroll.  The first presses walk the highlight up through the
        // static window to the middle…
        let mut focused = 19usize;
        let mut scroll = 10usize;
        for _ in 0..4 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, false);
        }
        assert_eq!((focused, scroll), (15, 10), "walked up to the middle row");

        // …then it pins at the middle while the list scrolls up until
        // scroll reaches 0…
        for _ in 0..10 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, false);
        }
        assert_eq!(
            (focused, scroll),
            (5, 0),
            "pinned at the middle while scrolling up"
        );

        // …and finally un-pins at the top edge to walk to row 0.
        for _ in 0..5 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, false);
        }
        assert_eq!((focused, scroll), (0, 0), "walked to the first row");

        // Further presses are no-ops.
        (focused, scroll) = step_focus(focused, scroll, 20, 10, false);
        assert_eq!((focused, scroll), (0, 0), "no-op at the first row");
    }

    #[test]
    fn step_focus_list_fits_in_window() {
        // 3 items in a 10-row window: max_scroll == 0, so the plain
        // picker_window behavior applies — the highlight walks edge to edge
        // while the window stays at row 0.
        assert_eq!(step_focus(0, 0, 3, 10, true), (1, 0));
        assert_eq!(step_focus(1, 0, 3, 10, true), (2, 0));
        assert_eq!(
            step_focus(2, 0, 3, 10, true),
            (2, 0),
            "no-op at the last item"
        );

        assert_eq!(step_focus(2, 0, 3, 10, false), (1, 0));
        assert_eq!(step_focus(1, 0, 3, 10, false), (0, 0));
        assert_eq!(
            step_focus(0, 0, 3, 10, false),
            (0, 0),
            "no-op at the first item"
        );
    }

    #[test]
    fn step_focus_height_zero_falls_back_to_focus_only() {
        // Viewport unknown (0 until the first frame): focus-only moves with
        // the render-time clamp — scroll is never touched.
        assert_eq!(step_focus(0, 7, 20, 0, true), (1, 7));
        assert_eq!(step_focus(5, 7, 20, 0, false), (4, 7));
        assert_eq!(step_focus(0, 7, 20, 0, false), (0, 7), "no-op at the top");
        assert_eq!(
            step_focus(19, 7, 20, 0, true),
            (19, 7),
            "no-op at the bottom"
        );
    }

    #[test]
    fn step_focus_height_one() {
        // A 1-row window pins at row 0 (height / 2): every press scrolls by
        // one row.
        assert_eq!(step_focus(0, 0, 3, 1, true), (1, 1));
        assert_eq!(step_focus(1, 1, 3, 1, true), (2, 2));
        assert_eq!(
            step_focus(2, 2, 3, 1, true),
            (2, 2),
            "no-op at the last item"
        );

        assert_eq!(step_focus(2, 2, 3, 1, false), (1, 1));
        assert_eq!(step_focus(1, 1, 3, 1, false), (0, 0));
        assert_eq!(
            step_focus(0, 0, 3, 1, false),
            (0, 0),
            "no-op at the first item"
        );
    }

    #[test]
    fn step_focus_empty_list_resets_pair() {
        // An empty list (e.g. a filter with no matches) resets the pair so a
        // stale highlight cannot linger.
        assert_eq!(step_focus(3, 5, 0, 10, true), (0, 0));
        assert_eq!(step_focus(3, 5, 0, 10, false), (0, 0));
    }

    #[test]
    fn step_focus_stale_scroll_is_clamped_symmetrically() {
        // A stale scroll hint (past the new max_scroll after a filter
        // narrowing) is clamped to the valid range before stepping, so ↑ and
        // ↓ treat the window identically: both clamp in one step instead of
        // the old ↑ behaviour of decrementing the stale hint one press at a
        // time.  len=11 in a 10-row window → max_scroll=1; scroll=10 is stale.
        assert_eq!(
            step_focus(10, 10, 11, 10, false),
            (9, 1),
            "↑ clamps scroll and walks the highlight up"
        );
        assert_eq!(
            step_focus(5, 10, 11, 10, true),
            (6, 1),
            "↓ clamps scroll and walks the highlight down"
        );
    }

    // ── filtered_len (count without building the list) ──────────────────

    #[test]
    fn filtered_len_matches_filtered_count() {
        // The count-only helper must agree with building the full filtered
        // list (they share `filter_matches`), so navigation and rendering
        // can never disagree about the list length.
        let mut sel = ModelSelectorState::new();
        sel.apply_models(
            vec![
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string(),
                "claude-3".to_string(),
            ],
            None,
        );
        assert_eq!(sel.filtered_len(), 3, "empty filter returns every model");
        sel.filter.text = "gpt".to_string();
        assert_eq!(sel.filtered_len(), 2, "case-insensitive substring");
        assert_eq!(sel.filtered().len(), 2, "agrees with the built list");
        sel.filter.text = "nope".to_string();
        assert_eq!(sel.filtered_len(), 0);
    }

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
