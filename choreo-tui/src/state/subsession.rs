//! Sub-session navigation: entering the child an attached session spawned, and
//! switching back to the parent when it finishes.
//!
//! `spawn_subsession` is a *blocking* tool — the daemon creates a child session,
//! runs it to completion, and only then returns — so the TUI can follow the
//! child's work as it streams and then drop back to the parent. That round trip
//! is summary-independent: the parent id and the "is the child done" signals
//! come from the creation event and the tool's own lifecycle, never from a
//! possibly stale session-summary row.
//!
//! **Two finish paths, one record.** A child that reports `active → idle` is
//! caught by [`App::note_spawned_child_status`]; a child that finishes before it
//! is ever observed active is caught by [`App::note_spawned_child_tool_finished`],
//! driven by the parent's `spawn_subsession` tool completing. Both consume the
//! single [`SpawnedChildNav`] record, so exactly one of them switches back.
//!
//! **Auto-enter is gated.** Navigation happens only when the child's parent is
//! the attached session, the user is on the Chat page, and exactly one
//! `spawn_subsession` call is in flight for that session — which tells our own
//! model's spawn apart from a sub-session created by another client and refuses
//! to thrash the view across parallel spawns.
//!
//! The `App` fields this module reads and writes (`spawned_child_nav`,
//! `spawn_subsession_inflight`, …) stay on the struct in `state/`.

use super::{App, Page};
use choreo_client_core::ClientError;
use choreo_proto::{ClientMessage, SessionStatus};
use crossbeam_channel::Sender;

/// The tool whose call makes the TUI auto-enter the sub-session it spawns.
///
/// Kept as a constant so the in-flight tracker in `state/turn.rs`, the reporting
/// path in `connection/daemon.rs`, and the navigation gate here cannot drift on
/// the tool's wire name.
pub(crate) const SPAWN_SUBSESSION_TOOL: &str = "spawn_subsession";

/// Navigation state for a sub-session the TUI auto-entered because the attached
/// session ran a single `spawn_subsession` tool call.
///
/// This is the summary-independent half of the sub-session round trip: the
/// parent comes straight from the `SessionCreated` broadcast (never a possibly
/// stale or as-yet-unfetched summary row), and `seen_active` lets a
/// creation-time `Inactive` status be told apart from a real finish. See
/// [`App::note_spawned_child_status`] for the status-based finish detection,
/// [`App::note_spawned_child_tool_finished`] for the tool-based one, and
/// [`App::navigate_to_spawned_child`] for how the record is created.
pub(crate) struct SpawnedChildNav {
    /// The child session the view was moved into.
    pub(crate) child_id: u64,
    /// The parent (the session the user was viewing when the spawn fired).
    pub(crate) parent_id: u64,
    /// Latched once the child is observed active. The daemon broadcasts an
    /// `Inactive` status right after `SessionCreated` (the child has not started
    /// yet), so every idle before this latch is set is not a finish and must not
    /// bounce the view back to the parent.
    pub(crate) seen_active: bool,
}

impl App {
    /// Whether a spawned child should pull the view into it, and from which
    /// parent.
    ///
    /// Navigation happens only when the child was created by the attached
    /// session (`parent == attached`), the user is on the Chat page (so the
    /// round trip mirrors the finish switch-back, which refuses to jump off the
    /// Session Manager), and exactly ONE `spawn_subsession` call is in flight
    /// for that session. The single-call gate does double duty: it distinguishes
    /// our own model's spawn from a sub-session created by another client (which
    /// has no local in-flight call and must not hijack the view), and it keeps
    /// parallel spawns in one turn from thrashing the view between children.
    pub(crate) fn spawned_child_should_navigate(
        &self,
        parent_session_id: Option<u64>,
    ) -> Option<u64> {
        let parent = parent_session_id?;
        if self.page != Page::Chat || self.attached_session_id != Some(parent) {
            return None;
        }
        (self.spawn_subsession_inflight_count(parent) == 1).then_some(parent)
    }

    /// The number of `spawn_subsession` calls currently in flight for
    /// `session_id` (0 when none). Maintained by `state/turn.rs`'s tool-call
    /// handler and reconciled at request end and on session deletion.
    pub(crate) fn spawn_subsession_inflight_count(&self, session_id: u64) -> usize {
        self.spawn_subsession_inflight
            .get(&session_id)
            .copied()
            .unwrap_or(0)
    }

    /// Record that a `spawn_subsession` call started for `session_id`.
    ///
    /// The tally feeds the single-in-flight gate in
    /// [`Self::spawned_child_should_navigate`]; it is incremented on the tool's
    /// start and decremented when it returns (or reconciled on request end / a
    /// session deletion), never derived from the child's own status.
    pub(crate) fn note_spawn_tool_started(&mut self, session_id: u64) {
        *self
            .spawn_subsession_inflight
            .entry(session_id)
            .or_insert(0) += 1;
    }

    /// Decrement the in-flight `spawn_subsession` tally for `session_id` when a
    /// spawn tool returns, removing the entry once it reaches zero.
    pub(crate) fn note_spawn_tool_finished(&mut self, session_id: u64) {
        if let Some(count) = self.spawn_subsession_inflight.get_mut(&session_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.spawn_subsession_inflight.remove(&session_id);
            }
        }
    }

    /// Drop the in-flight `spawn_subsession` tally for `session_id`.
    ///
    /// A request that ends — or a session that is deleted — cannot still have a
    /// spawn tool running, so a residual count is stale. Clearing it keeps the
    /// "exactly one in flight" gate from being poisoned by a `ToolCallStarted`
    /// whose terminal event was never delivered.
    pub(crate) fn clear_spawn_inflight(&mut self, session_id: u64) {
        self.spawn_subsession_inflight.remove(&session_id);
    }

    /// Forget the spawned-child round trip when `session_id` — the child or the
    /// parent — is deleted. A round trip through a session that no longer exists
    /// can never complete, so the record is dropped rather than left to linger.
    pub(crate) fn clear_spawn_nav_for(&mut self, session_id: u64) {
        if self
            .spawned_child_nav
            .as_ref()
            .is_some_and(|nav| nav.child_id == session_id || nav.parent_id == session_id)
        {
            self.spawned_child_nav = None;
        }
    }

    /// Enter the sub-session the attached session just spawned.
    ///
    /// Records the round-trip intent ([`SpawnedChildNav`]) from the creation
    /// event's authoritative ids, primes the status fields, and runs the shared
    /// attach sequence. No `ListSessions` is sent: the round trip is
    /// summary-independent (the nav record carries the parent and the status
    /// latch), and the Session Manager refetches on open, so keeping this from
    /// rewriting the freshly-navigated view's status line is the better trade.
    pub(crate) fn navigate_to_spawned_child(
        &mut self,
        child_id: u64,
        parent_id: u64,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
        client_tx: &Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        tracing::info!(
            child_id,
            parent_id,
            "spawn_subsession: entering the spawned child session",
        );
        // Record before attaching: `attach_to_session` clears a stale nav record
        // whose child differs from the target, so the record must name the child
        // we are about to enter.
        self.spawned_child_nav = Some(SpawnedChildNav {
            child_id,
            parent_id,
            seen_active: false,
        });
        self.prime_created_display(child_id, account_name, selected_model, reasoning_effort);
        self.attach_to_session(child_id, client_tx)
    }

    /// Fold a status broadcast for `session_id` into the spawned-child nav
    /// record, returning the parent id to switch back to when the auto-entered
    /// child just finished.
    ///
    /// Latching `seen_active` on the first active status is what makes the
    /// daemon's creation-time `Inactive` broadcast (sent immediately after
    /// `SessionCreated`, before the child runs) a no-op rather than an instant
    /// bounce-back. A finish is the subsequent idle transition while the user is
    /// still reading the child on the Chat page; the record is consumed either
    /// way, so it never lingers past the child's run.
    pub(crate) fn note_spawned_child_status(
        &mut self,
        session_id: u64,
        status: &SessionStatus,
    ) -> Option<u64> {
        let nav = self.spawned_child_nav.as_ref()?;
        if nav.child_id != session_id {
            return None;
        }
        let parent_id = nav.parent_id;
        let seen_active = nav.seen_active;
        if status.is_active() {
            if let Some(nav) = self.spawned_child_nav.as_mut() {
                nav.seen_active = true;
            }
            return None;
        }
        // An idle before the child was ever seen active is the creation-time
        // status (or another pre-start idle): not a finish.
        if !seen_active {
            return None;
        }
        // The child finished. Consume the record; only jump back when the user is
        // still looking at it on the Chat page (matching the manual-open path's
        // gate — never yank the view off the Session Manager or a session the
        // user moved to).
        self.spawned_child_nav = None;
        (self.page == Page::Chat && self.attached_session_id == Some(session_id))
            .then_some(parent_id)
    }

    /// Fold the parent's `spawn_subsession` tool completing into the nav record,
    /// returning the `(child, parent)` pair to switch back to when the
    /// auto-entered child is still on screen.
    ///
    /// The spawn tool blocks until the child finishes, so its terminal event is
    /// a definitive "child done" signal that does not depend on the child ever
    /// reporting an active status. It closes the gap [`Self::note_spawned_child_status`]
    /// leaves for a child that finished before the TUI observed it running: the
    /// record is consumed here, so the two paths can never both switch back.
    /// `parent_id` is the session the tool call ran in (the nav record's parent).
    pub(crate) fn note_spawned_child_tool_finished(
        &mut self,
        parent_id: u64,
    ) -> Option<(u64, u64)> {
        let nav = self.spawned_child_nav.as_ref()?;
        if nav.parent_id != parent_id {
            return None;
        }
        let child_id = nav.child_id;
        // The child's run is over either way, so consume the record before
        // deciding whether to move the view.
        self.spawned_child_nav = None;
        (self.page == Page::Chat && self.attached_session_id == Some(child_id))
            .then_some((child_id, parent_id))
    }

    /// Detect when the user is reading an agent-spawned sub-session on the
    /// Chat page and that sub-session just finished running.
    ///
    /// A sub-session "finishes" when its status transitions from an active
    /// state (inference / tool call / retrying) to an idle one (inactive /
    /// sleeping) — the daemon broadcasts exactly one such transition when the
    /// child's request completes.  The check reads the *pre-update* summary
    /// status (the caller invokes this before applying the new status), so
    /// duplicate idle→idle broadcasts — summary refreshes, or re-attaching to
    /// a child that finished earlier — never re-fire the switch.
    ///
    /// Returns the parent session id to switch back to, or `None` when the
    /// user is not viewing a finishing sub-session.  The parent id (and the
    /// titles for the notification) come from the summary list, so a missing
    /// summary — or a parent that no longer exists in it — is a graceful
    /// no-op rather than a misdirected switch.
    pub(crate) fn attached_subsession_finished(
        &self,
        session_id: u64,
        new_status: &SessionStatus,
    ) -> Option<u64> {
        // Only the Chat page: the Session Manager is a browsing view, and
        // auto-jumping away from it would fight the user's navigation.
        if self.page != Page::Chat || self.attached_session_id != Some(session_id) {
            return None;
        }
        // The finishing session must be an agent-spawned sub-session; its
        // parent is only known from the session summary list.
        let summary = self
            .session_mgr
            .all
            .iter()
            .find(|s| s.session_id == session_id)?;
        let parent_id = summary.parent_session_id?;
        // Only the active → idle transition counts as "finished".  Idle →
        // idle (e.g. a summary refresh after the child already finished)
        // must not yank the view away while the user is still reading.
        if summary.status.is_active()
            && !new_status.is_active()
            // The parent must still exist in the summary: if it was deleted
            // while the child ran, switching would attach to a dead session
            // id and strand the user on a session the daemon rejects.
            && self
                .session_mgr
                .all
                .iter()
                .any(|s| s.session_id == parent_id)
        {
            Some(parent_id)
        } else {
            None
        }
    }

    /// Switch the Chat view back to the parent session of a sub-session that
    /// just finished, and surface a status notification explaining the jump.
    ///
    /// Delegates to [`attach_to_session`](App::attach_to_session) — the same
    /// sequence the Session Manager Enter handlers use — so the daemon messages
    /// are sent *before* the local state is mutated, and a broken pipe leaves the
    /// view on the finished sub-session instead of stranding the user on a
    /// session that was never attached.
    pub(crate) fn switch_back_to_parent(
        &mut self,
        finished_session_id: u64,
        parent_id: u64,
        client_tx: &Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        // Titles come from the summary list — the same source that told us
        // the sub-session's parent — falling back to "untitled" exactly like
        // the session list renderer does.
        let title = |id: u64| {
            self.session_mgr
                .all
                .iter()
                .find(|s| s.session_id == id)
                .and_then(|s| s.title.clone())
                .unwrap_or_else(|| "untitled".to_string())
        };
        let subsession_title = title(finished_session_id);
        let parent_title = title(parent_id);

        self.attach_to_session(parent_id, client_tx)?;

        self.set_status(format!(
            "Subsession \"{subsession_title}\" finished. Switched back to parent \"{parent_title}\"."
        ));
        Ok(())
    }
}
