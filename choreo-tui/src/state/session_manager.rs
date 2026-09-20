//! Session-manager page state: the session list/detail view model
//! (`SessionManagerState`), its view selector (`SessionManagerView`), and the
//! per-session detail snapshot (`SessionDetailData`).  Split out of
//! `state/pages.rs`, which had grown to hold several unrelated page/popup
//! states — this module owns only the session manager.
//!
//! The rendered list is a *derived* view: `all` is the full list as delivered
//! by the daemon and `sessions` is the partition of it the current `view`
//! shows, kept in sync by `rebuild_view`.  Re-exported from `state/mod.rs`
//! (`pub(crate) use session_manager::*;`), so `crate::state::*` paths are
//! unchanged.

use super::PAGE_SCROLL_LINES;
use choreo_proto::{SessionStatus, SessionSummary, TokenUsage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionManagerView {
    /// The live session list — every session that is NOT archived.
    List,
    /// The archived session list (Tab-toggled from `List`).  Archiving a
    /// session moves it out of `List` and into `Archived`; unarchiving moves
    /// it back.
    Archived,
    Detail,
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
    /// Whether the session is pinned (shown first in every list view).
    pub(crate) pinned: bool,
    /// When the session was archived (Unix-epoch-ms), or `None` when it is
    /// live.
    pub(crate) archived_at: Option<i64>,
}

pub(crate) struct SessionManagerState {
    /// The FULL session list exactly as last delivered by the daemon, sorted
    /// by `(pinned desc, last_modified desc, session_id desc)`.  `sessions`
    /// (the rendered view) is derived from this by [`Self::rebuild_view`] —
    /// the `List` view keeps the non-archived rows, the `Archived` view the
    /// archived ones — so every list mutation must go through `all` and then
    /// rebuild, or the two would drift apart.
    pub(crate) all: Vec<SessionSummary>,
    /// The rows currently rendered for `view` — a filter of `all`.  Navigation,
    /// windowing, rendering and selection all read this, so it always matches
    /// what is on screen.
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) view: SessionManagerView,
    pub(crate) selection: Option<usize>,
    /// Session id to highlight on the next list refresh.  `select_session`
    /// records this when navigating to the session manager (Ctrl+S) so the
    /// freshly fetched list lands on the session the user was just viewing
    /// — even on the first visit, before the daemon's `ListSessions` reply has
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

impl SessionManagerState {
    pub(crate) fn new() -> Self {
        Self {
            all: Vec::new(),
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
        // `select_session`'s one-shot preference takes priority over whatever
        // the user had highlighted, so the fresh list lands on the session
        // they were just viewing.  `rebuild_view` below preserves the current
        // selection by id otherwise.
        let preferred = self.pending_select.take();
        self.all = sessions;
        self.rebuild_view();
        if let Some(id) = preferred {
            // The preferred session may live in the OTHER partition: opening
            // the manager while attached to an ARCHIVED session defaults to
            // the live list, where the target is absent. Switch to the view
            // that actually holds it so the highlight lands on the session the
            // user was viewing instead of falling onto an unrelated first row.
            // (No switch when the id is not in the list at all — the row-0
            // fallback below still applies then.)
            let target_archived = self
                .all
                .iter()
                .find(|s| s.session_id == id)
                .map(|s| s.archived_at.is_some());
            if let Some(want_archived) = target_archived {
                let showing_archived = matches!(self.view, SessionManagerView::Archived);
                if want_archived != showing_archived {
                    self.view = if want_archived {
                        SessionManagerView::Archived
                    } else {
                        SessionManagerView::List
                    };
                    self.rebuild_view();
                }
            }
            self.selection = if self.sessions.is_empty() {
                None
            } else {
                Some(
                    self.sessions
                        .iter()
                        .position(|s| s.session_id == id)
                        .unwrap_or(0),
                )
            };
            // Re-anchor the scroll window so the highlighted row is visible
            // right away rather than waiting for the next navigation step.
            self.reanchor_scroll();
        }
    }

    /// Re-derive the rendered view (`sessions`) from the full list (`all`):
    /// sort `all` by `(pinned desc, last_modified desc, session_id desc)`,
    /// keep only the rows the current `view` shows, and re-point the selection
    /// at the same session by id — clamping to the old row index otherwise, so
    /// archiving the highlighted session lands the cursor on a neighbour
    /// rather than jumping to the top, and an emptied view clears it.
    ///
    /// Every list mutation routes through here so `all`, `sessions`, and
    /// `selection` can never drift apart.
    fn rebuild_view(&mut self) {
        // One definition of list order, shared with the daemon
        // (`SessionSummary::cmp_for_list`): pinned first, then newest, then
        // highest id. Applying it here keeps the client's order identical to
        // the daemon's regardless of arrival order.
        self.all.sort_by(SessionSummary::cmp_for_list);
        // Remember which session was highlighted (and at which row) before
        // the partition, so the cursor can follow it across the rebuild.
        let selected_id = self
            .selection
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.session_id);
        let selected_index = self.selection;
        // `Detail` never hosts a list; treat it as `List` so the underlying
        // `sessions` matches what `leave_detail` returns to.
        let show_archived = matches!(self.view, SessionManagerView::Archived);
        self.sessions = self
            .all
            .iter()
            .filter(|s| s.archived_at.is_some() == show_archived)
            .cloned()
            .collect();
        self.selection = if self.sessions.is_empty() {
            None
        } else {
            let idx = selected_id
                .and_then(|id| self.sessions.iter().position(|s| s.session_id == id))
                // The highlighted session left this view (e.g. it was just
                // archived): clamp the old row index into the new bounds so
                // the cursor lands on a neighbour.
                .unwrap_or_else(|| selected_index.unwrap_or(0).min(self.sessions.len() - 1));
            Some(idx)
        };
    }

    /// Re-order after a live status change and keep the cursor on the same
    /// session, which may have moved to a new index.
    pub(crate) fn resort_after_status_change(&mut self) {
        self.rebuild_view();
    }

    /// Switch between the live (`List`) and archived (`Archived`) views (Tab).
    /// The new view's first row becomes the selection (or `None` when it is
    /// empty) and the window scrolls back to the top.
    pub(crate) fn toggle_view(&mut self) {
        self.view = match self.view {
            SessionManagerView::Archived => SessionManagerView::List,
            // `Detail` never hosts the Tab key, but treat it as `List` so the
            // toggle stays total.
            SessionManagerView::List | SessionManagerView::Detail => SessionManagerView::Archived,
        };
        // Re-partition against the (unchanged) full list; the old selection
        // index is meaningless in the other view, so reset to the first row.
        self.rebuild_view();
        self.selection = if self.sessions.is_empty() {
            None
        } else {
            Some(0)
        };
        self.scroll = 0;
    }

    /// Apply a `pinned`/`archived_at` change broadcast by the daemon — the
    /// success signal for a `SetSessionPinned`/`SetSessionArchived` request
    /// (there is no targeted reply; failures arrive as `SessionFailed`).
    /// Updates the session in the FULL list and re-partitions, so archiving a
    /// session leaves the `List` view immediately; the cursor follows the
    /// session by id when it survives the move, or clamps to a neighbour when
    /// it does not.
    pub(crate) fn apply_session_flags(
        &mut self,
        session_id: u64,
        pinned: bool,
        archived_at: Option<i64>,
    ) {
        if let Some(session) = self.all.iter_mut().find(|s| s.session_id == session_id) {
            session.pinned = pinned;
            session.archived_at = archived_at;
        } else {
            // A flags change for a session not in the list (e.g. one we never
            // learned about): nothing to re-partition, but log so a future
            // mismatch is diagnosable.
            tracing::debug!(
                session_id,
                "SessionFlagsChanged for an unknown session; ignoring",
            );
            return;
        }
        self.rebuild_view();
        // Keep the detail view's own copy in sync while the user is looking at
        // it, so a flag change made elsewhere is reflected without leaving the
        // page (the detail view renders `detail_data`, not `sessions`).
        if let Some(detail) = self.detail_data.as_mut()
            && detail.session_id == session_id
        {
            detail.pinned = pinned;
            detail.archived_at = archived_at;
        }
    }

    /// Highlight `session_id` in the list immediately when it is already
    /// loaded, and remember the preference so the next [`Self::set_sessions`]
    /// refresh re-selects it even if the current list is empty or stale
    /// (e.g. the very first visit to the session manager, before the
    /// `ListSessions` reply has arrived).
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

    /// Move the selection up by a page (`PgUp`).  The render window follows
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

    /// Move the selection down by a page (`PgDn`), clamped to the last row.
    /// The render window follows the selection with the same anchoring as
    /// `select_down`.
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
                pinned: s.pinned,
                archived_at: s.archived_at,
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
        let old_len = self.all.len();
        self.all.retain(|s| s.session_id != id);
        if self.all.len() == old_len {
            return;
        }
        // Re-partition from the updated full list; the selection follows the
        // same session by id, or clamps into the new bounds.
        self.rebuild_view();
        if self
            .detail_data
            .as_ref()
            .is_some_and(|d| d.session_id == id)
        {
            // Leaving Detail returns to List, whose partition `rebuild_view`
            // already produced (`Detail` is treated as `List` there).
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
