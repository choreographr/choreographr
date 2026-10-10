//! Session-lifecycle and session-summary handling.
//!
//! Owns the client side of a session's lifecycle: attaching to a session and
//! the switches around it ([`App::attach_to_session`],
//! [`App::reset_for_session_switch`], [`App::switch_back_to_parent`]), the
//! session-created attach-vs-broadcast split
//! ([`App::handle_session_created`] navigates the requester while
//! [`App::note_session_created`] only refreshes an open list), the daemon
//! message handlers that fold a session's status/model/effort/account/title/
//! working-dir changes into that session's display — and, only for the attached
//! session, into the status-bar summary via `App::mirror_to_attached_summary` —
//! the accounts/sessions list replies, the delete and flag-change handlers,
//! and the token-usage readout and merge ([`App::display_token_usage`],
//! [`merge_token_usage`]).
//!
//! Every item here is an inherent `App` method plus the [`merge_token_usage`]
//! free function; the `App`/`SessionDisplayState` fields they read and write
//! stay on those structs in `state/`. This module is the session-lifecycle half
//! of the `state` module, split out so `mod.rs` keeps only the struct
//! definitions and the input/layout plumbing.

use super::{App, Page};
use choreo_client_core::ClientError;
use choreo_proto::{
    AccountInfo, ClientMessage, ClientMessageType, ReasoningCapability, SessionStatus,
    SessionSummary, TokenUsage,
};

impl App {
    /// Enter `session_id`: rebind the active session and reset only the
    /// transient *render* state that the next layout pass will rebuild.  The
    /// per-session reading position (`history_scroll`) and prompt draft are
    /// preserved so a session the user returns to looks the way they left it.
    pub(crate) fn reset_for_session_switch(&mut self, session_id: u64) {
        // Capture the outgoing session's reading position before rebinding the
        // active session: the history viewport height reflows when the help /
        // status bands change on attach (and background sessions keep
        // streaming), so a raw from-bottom offset cannot be restored verbatim.
        // The captured anchor is applied once on the target's first rebuild.
        if let Some(prev) = self.active_session_id {
            let vp = self.history_viewport;
            if let Some(prev_display) = self.session_displays.get_mut(&prev) {
                prev_display.capture_scroll_restore(&vp);
            }
        }
        self.active_session_id = Some(session_id);
        // A command line belongs to the session the user was editing; it must
        // never become the newly-attached session's draft.  Discarding it clears
        // the buffer before any draft hand-off happens elsewhere.
        self.discard_command_line();
        // A selection is keyed to the previous session's rendered content in
        // screen coordinates; it must not linger and highlight the next
        // session's history.
        self.text_selection = None;
        // A history-recalled entry belongs to the session being left; end
        // browsing so the new session can never show, or stash, the previous
        // session's recalled prompt.  `persist_input_draft` — which runs before
        // this on every real switch — already exits browsing, so this is the
        // defensive backstop that makes the field's "reset on session switch"
        // contract hold even if a future caller skips the input hand-off.
        self.history_index = None;
        let display = self.display_for(session_id);
        // Keep the session's live state: `view.turns` and `view.request_to_turn`
        // (accumulated via the all-activity subscription while the user was
        // viewing another session), the active-request set, live token
        // estimates, and per-turn reasoning preferences.  Destroying these on
        // switch was the root cause of "switching to a streaming session shows
        // nothing until the next turn": the accumulated content AND the
        // request→turn routing map were wiped exactly when they were needed,
        // and the attach snapshot only holds the empty in-flight placeholder.
        //
        // Only transient *render* state is reset here — it is rebuilt on the
        // next layout pass because `markers_dirty` forces a full rebuild from
        // the preserved `view.turns`.  The reading position is NOT reset: the
        // outgoing session's absolute anchor was captured above, and the
        // target's own `scroll_restore` (set when it was last left) is applied
        // on the rebuild below, so a session reopens showing the content the
        // user left it on rather than snapping to the bottom.  A session never
        // visited has no anchor and opens at scroll 0 (the bottom) because
        // `or_default()` builds a fresh display behind `display_for`.
        display.render_cache.clear();
        display.visible_turn_ids.clear();
        display.markers.clear();
        display.height_prefix.clear();
        display.turn_heights.clear();
        display.turn_layouts.clear();
        display.streaming_turn_index = None;
        display.streaming_response = None;
        display.streaming_dirty = false;
        display.markers_dirty = true;
        display.content_dirty = false;
        display.status = None;
        display.error = None;
        display.progress_dirty = true;
        self.fullscreen_image_target = None;
    }
    // ── Legacy per-session daemon message handlers ─────────────────────

    pub(crate) fn handle_session_created(
        &mut self,
        session_id: u64,
        parent_session_id: Option<u64>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        // Agent-spawned sub-sessions (parent_session_id = Some) are transient
        // tool artifacts, not sessions the user opened.  Navigating to one
        // would hijack the Chat view away from the session the user is reading
        // and destroy its scroll position, so treat it like background noise.
        if let Some(parent_id) = parent_session_id {
            tracing::info!(
                session_id,
                parent_session_id = parent_id,
                "sub-session created — not navigating",
            );
            return Ok(());
        }

        // Direct reply to THIS client's create (`SessionCreatedForRequester`):
        // navigate the requester to the session it just made — attach and
        // switch to the Chat page.  This is the requester-side half of the
        // requester-vs-broadcast split; every OTHER client sees only the
        // `SessionCreated` broadcast (`note_session_created`) and refreshes its
        // list without moving.  All three create entry points funnel here —
        // `/new`, `/session new`, and `n` on the Session Manager — so the client
        // that asked for the session always lands on it (previously the Session
        // Manager branch returned early, leaving the creator on the list).
        //
        // Prime the display fields from the creation params BEFORE attaching:
        // the session summary (from the `ListSessions` below) may not have
        // arrived yet, and the status bar should read correctly the instant the
        // attach lands.  `attach_to_session` -> `reset_for_session_switch`
        // preserves these (it clears only transient render state).
        {
            let display = self.display_for(session_id);
            display.account_name = account_name;
            display.selected_model = selected_model;
            display.reasoning_effort = reasoning_effort;
        }
        // Fetch the session summary before attaching ONLY when leaving another
        // page (Chat): the reply populates `session_mgr.all`, so the attach's
        // status-bar priming and the daemon's `SessionAttached` gap-fill have
        // the data.  On the Session Manager page the fetch is deliberately
        // SKIPPED: the broadcast `SessionCreated` already refreshes an open list
        // (`note_session_created`), and the direct reply races that broadcast —
        // so fetching here too would send a redundant `ListSessions` for a
        // create the user is navigating away from (the page re-fetches on the
        // next `open_session_manager` anyway).  This is what restores the
        // original "one list refresh per create, not two" invariant now that a
        // create from ANY page funnels through this handler.
        if self.page != Page::SessionManager {
            self.pending
                .send(client_tx, ClientMessageType::ListSessions);
        }
        // Shared attach sequence (also used by the Session Manager's Enter):
        // it sends UnsubscribeSessionsSummary + AttachSession, hands the input
        // bar over, rebinds the active session, and switches to the Chat page.
        // A broken pipe leaves the view on the previous session instead of
        // stranding the user on an un-attached one.
        self.attach_to_session(session_id, client_tx)
    }

    /// Handle the BROADCAST notification that a session was created — by any
    /// client, this one included (a create arrives both as the direct
    /// `SessionCreatedForRequester` reply and as the `SessionCreated`
    /// notification).
    ///
    /// Unlike [`App::handle_session_created`] — the direct reply to THIS
    /// client's create, which auto-attaches — a notification must NEVER change
    /// the attached session. This is the fix for the phone-view-follows-laptop
    /// bug: before the split, a broadcast create was indistinguishable from
    /// the reply and every client attached to it.
    ///
    /// The only action is to keep the session list current *when the user is
    /// looking at it*: on the Session Manager page an unsolicited `ListSessions`
    /// renders a fresh list; on the Chat page it would rewrite the status line
    /// for an event the user did not initiate, so it is skipped — matching the
    /// sub-session-on-the-Chat-page rule in `handle_session_created`.
    pub(crate) fn note_session_created(
        &mut self,
        session_id: u64,
        parent_session_id: Option<u64>,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) {
        tracing::debug!(
            session_id,
            parent_session_id,
            "session created elsewhere — refreshing list only, not attaching",
        );
        if self.page == Page::SessionManager {
            // Best-effort refresh: a broken channel means the whole connection
            // is tearing down, and the reply renders into the session list
            // (never the status line), so there is nothing to propagate.
            self.pending
                .send(client_tx, ClientMessageType::ListSessions);
        }
    }

    pub(crate) fn handle_session_attached(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        self.attached_session_id = Some(session_id);
        // Attaching (re)points the terminal records at this session.
        self.term_status_dirty = true;
        // Copy session summary fields before borrowing display.
        let (
            token_usage,
            context_window,
            last_prompt_tokens,
            account_name,
            selected_model,
            reasoning_effort,
            working_dir,
            status,
        ) = self
            .session_mgr
            .all
            .iter()
            .find(|s| s.session_id == session_id)
            .map_or((None, None, None, None, None, None, None, None), |s| {
                (
                    s.token_usage,
                    s.context_window,
                    s.last_prompt_tokens,
                    s.account_name.clone(),
                    s.selected_model.clone(),
                    s.reasoning_effort.clone(),
                    s.working_dir.clone(),
                    Some(s.status.clone()),
                )
            });
        {
            let display = self.display_for(session_id);
            // Fill gaps from the (potentially stale) session summary, but never
            // clobber values already accumulated via the all-activity
            // subscription while this session was in the background: the
            // summary is refreshed on ListSessions, whereas the display may
            // hold fresher per-turn token usage / live counts / model that
            // arrived mid-stream.  Overwriting here would regress the status
            // bar right after switching into a streaming session.
            if display.token_usage.is_none() {
                display.token_usage = token_usage;
            }
            if display.context_window.is_none() {
                display.context_window = context_window;
            }
            if display.last_prompt_tokens.is_none() {
                display.last_prompt_tokens = last_prompt_tokens;
            }
            if display.account_name.is_none() {
                display.account_name = account_name;
            }
            if display.selected_model.is_none() {
                display.selected_model = selected_model;
            }
            if display.reasoning_effort.is_none() {
                display.reasoning_effort = reasoning_effort;
            }
            if display.working_dir.is_none() {
                display.working_dir = working_dir;
            }
            if let Some(ref st) = status {
                display.status = Some(format!("{st:?}"));
            }
        }
        self.attached_status = status;
        self.refresh_attached_account_slug();
        self.show_help_overlay = true;
        if let Some(d) = self.active_display() {
            d.progress_dirty = true;
        }
    }

    /// The account slug shown in the status bar: the attached session's
    /// account name (the account name is the slug users enter when creating
    /// one). Previously this showed the inference provider slug resolved via
    /// the accounts list, but the account itself is the more useful identity.
    pub(crate) fn refresh_attached_account_slug(&mut self) {
        self.attached_account_slug = self
            .active_display_ref()
            .and_then(|d| d.account_name.clone());
    }

    pub(crate) fn attached_session_mut(&mut self) -> Option<&mut SessionSummary> {
        self.session_mgr
            .all
            .iter_mut()
            .find(|s| Some(s.session_id) == self.attached_session_id)
    }

    /// The summary of `session_id`, but only when it is the attached session.
    ///
    /// Per-session display updates mirror into the status bar's summary
    /// exclusively for the attached session — a background session's model,
    /// effort or account change must never rewrite the identity fields of the
    /// session on screen.
    fn mirror_to_attached_summary(&mut self, session_id: u64) -> Option<&mut SessionSummary> {
        if self.attached_session_id == Some(session_id) {
            self.attached_session_mut()
        } else {
            None
        }
    }

    /// A model was selected on the session `session_id`.  Only that session's
    /// display (and, when it is the attached session, the summary used by the
    /// status bar) is updated — a `ModelSelected` broadcast for a background
    /// session must never overwrite the display the user is currently viewing.
    pub(crate) fn handle_model_selected(
        &mut self,
        session_id: u64,
        model: &str,
        reasoning_capability: Option<ReasoningCapability>,
    ) {
        let display = self.display_for(session_id);
        display.selected_model = Some(model.to_owned());
        display.reasoning_capability = reasoning_capability;
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.selected_model = Some(model.to_owned());
        }
    }

    /// A reasoning-effort change was accepted on the session `session_id`.
    /// Routed to that session's own display only — see `handle_model_selected`.
    pub(crate) fn handle_reasoning_effort_set(&mut self, session_id: u64, effort: String) {
        let display = self.display_for(session_id);
        display.reasoning_effort = Some(effort.clone());
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.reasoning_effort = Some(effort);
        }
    }

    // Call sites in `connection/daemon.rs` pass `&Option<String>`; changing
    // the signature would touch files outside this one.
    #[expect(clippy::ref_option)]
    pub(crate) fn handle_session_working_dir_set(
        &mut self,
        session_id: u64,
        path: &Option<String>,
    ) {
        if self.attached_session_id == Some(session_id) {
            if let Some(d) = self.active_display() {
                d.working_dir.clone_from(path);
                d.progress_dirty = true;
            }
            if let Some(s) = self.attached_session_mut() {
                s.working_dir.clone_from(path);
            }
        }
    }

    pub(crate) fn handle_session_title_set(&mut self, session_id: u64, title: &str) {
        if self.attached_session_id == Some(session_id) {
            self.status = Some(format!("Session title changed to '{title}'"));
            if let Some(s) = self.attached_session_mut() {
                s.title = Some(title.to_owned());
            }
        }
        // The window title (OSC 2) and this session's program-status record
        // (OSC 7501 `title=`) both carry the title.
        self.term_status_dirty = true;
    }

    /// The account for the session `session_id` was set.  Only that session's
    /// display is updated; the status bar's provider slug and the session
    /// summary are refreshed only when the message belongs to the attached
    /// session (a background session's account change must not alter the
    /// attached session's identity fields).
    pub(crate) fn handle_session_account_set(&mut self, session_id: u64, account: &str) {
        let display = self.display_for(session_id);
        display.account_name = Some(account.to_owned());
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.account_name = Some(account.to_owned());
        }
        // Refresh the status-bar account slug only for the attached session
        // (it reads the display account name set above).
        if self.attached_session_id == Some(session_id) {
            self.refresh_attached_account_slug();
        }
    }

    pub(crate) fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: &SessionStatus,
        last_modified: i64,
    ) {
        if let Some(session) = self
            .session_mgr
            .all
            .iter_mut()
            .find(|s| s.session_id == session_id)
        {
            session.status = status.clone();
            // last_modified is monotonic; guard against duplicate or
            // out-of-order deliveries (per-session + summary paths).
            session.last_modified = session.last_modified.max(last_modified);
        }
        // A status change bumps last_modified on the daemon, so the list may
        // reorder while the user is looking at it — re-sort but keep the
        // cursor on the same session.
        self.session_mgr.resort_after_status_change();
        if let Some(ref mut detail) = self.session_mgr.detail_data
            && detail.session_id == session_id
        {
            detail.status = status.clone();
        }
        if self.attached_session_id == Some(session_id) {
            self.attached_status = Some(status.clone());
        }
        // A new turn (or the session going to sleep) clears the previous
        // turn's done/error outcome; the trailing idle status of a
        // just-finished turn leaves it in place so the outcome survives the
        // prompt. Either way the published records may have changed.
        if !matches!(status, SessionStatus::Inactive) {
            self.term_status_override.remove(&session_id);
        }
        self.term_status_dirty = true;
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

    /// Attach the Chat view to `session_id` via the shared sequence every
    /// attach path follows (Session Manager list/detail Enter, and the
    /// sub-session finish switch-back).
    ///
    /// The daemon messages are sent *before* the local state is mutated, so a
    /// broken pipe leaves the view on the previous session instead of
    /// stranding the user on a session that was never attached.
    /// `UnsubscribeSessionsSummary` is idempotent on the daemon (removing a
    /// client that was never registered is a no-op), so it is safe to send
    /// unconditionally.  `reset_for_session_switch` runs before `set_page` so
    /// the subsequent `set_page` marks the target's display dirty — the one
    /// that will actually render next — and `attached_status` is refreshed
    /// immediately from the summary instead of waiting for the daemon's
    /// `SessionAttached` reply to arrive.
    #[expect(clippy::unnecessary_wraps)]
    pub(crate) fn attach_to_session(
        &mut self,
        session_id: u64,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        self.pending
            .send(client_tx, ClientMessageType::UnsubscribeSessionsSummary);
        self.pending
            .send(client_tx, ClientMessageType::AttachSession { session_id });
        // Discard a command line BEFORE the input hand-off below: it is not a
        // prompt, so it must be dropped (not stashed as the outgoing session's
        // draft) and the target session's draft loaded in its place.
        self.discard_command_line();
        // Hand the input bar over to the target session (stash the outgoing
        // session's input, load the target's draft) before `attached_session_id`
        // is rebound below — it still names the session the input bar's
        // current contents belong to.  See `persist_input_draft`.
        self.persist_input_draft(session_id);
        // reset_for_session_switch first so the subsequent set_page marks the
        // target's display dirty — the one that will actually render next.
        self.reset_for_session_switch(session_id);
        self.set_page(Page::Chat);
        self.attached_session_id = Some(session_id);
        // Refresh the status bar right away from the summary; the daemon's
        // SessionAttached reply re-applies the same (possibly newer) value.
        self.attached_status = self
            .session_mgr
            .all
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|s| s.status.clone());
        // The attached session changed, so both the window title (OSC 2) and
        // the program-status records (OSC 7501) must be re-published.
        self.term_status_dirty = true;
        Ok(())
    }

    /// Switch the Chat view back to the parent session of a sub-session that
    /// just finished, and surface a status notification explaining the jump.
    ///
    /// Delegates to [`attach_to_session`] — the same sequence the Session
    /// Manager Enter handlers use — so the daemon messages are sent *before*
    /// the local state is mutated, and a broken pipe leaves the view on the
    /// finished sub-session instead of stranding the user on a session that
    /// was never attached.
    pub(crate) fn switch_back_to_parent(
        &mut self,
        finished_session_id: u64,
        parent_id: u64,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
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

        self.status = Some(format!(
            "Subsession \"{subsession_title}\" finished. Switched back to parent \"{parent_title}\"."
        ));
        Ok(())
    }

    pub(crate) fn handle_accounts(&mut self, accounts: &[AccountInfo]) {
        self.ai_providers.set_accounts(accounts.to_vec());
        self.refresh_attached_account_slug();
    }

    #[expect(clippy::unnecessary_wraps)]
    pub(crate) fn handle_sessions(
        &mut self,
        sessions: &[SessionSummary],
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        self.session_mgr.set_sessions(sessions.to_vec());
        // `Sessions` is a full replacement, so drop terminal-outcome overrides
        // for sessions that no longer exist. `SessionDeleted` also removes an
        // entry, but a refresh is the catch-all — this keeps
        // `term_status_override` bounded by the live session set instead of
        // growing once per session ever seen with an outcome.
        self.term_status_override
            .retain(|id, _| sessions.iter().any(|s| s.session_id == *id));
        if self.page == Page::Chat {
            if sessions.is_empty() {
                self.status = Some("[daemon] no sessions".to_string());
            } else {
                self.status = Some(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == self.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    self.status = Some(format!(
                        "{} {}: \"{title}\" ({model}) — {} turns",
                        prefix, session.session_id, session.turn_count,
                    ));
                }
            }
            if self.attached_session_id.is_none() {
                // Auto-attach to a LIVE session only. Archived sessions are
                // deliberately hidden from the sessions list's live view, so
                // they must not hijack the Chat page either — a restart with a
                // pinned/archived session would otherwise silently open it.
                // Filtered into a local slice FIRST so the existing top-level
                // preference (and the sub-session fallback) keep their exact
                // semantics over the non-archived subset.
                //
                // Prefer the most recently modified *top-level* session.
                // Agent-spawned sub-sessions (parent_session_id = Some) are
                // transient tool artifacts whose last_modified is bumped as
                // they stream, so they'd otherwise top the list and silently
                // hijack the view to a session the user never opened — e.g.
                // its streaming token count would appear on the chat page.
                let live: Vec<&SessionSummary> = sessions
                    .iter()
                    .filter(|s| s.archived_at.is_none())
                    .collect();
                let target = live
                    .iter()
                    .copied()
                    .find(|s| s.parent_session_id.is_none())
                    .or_else(|| live.first().copied());
                if let Some(first) = target {
                    // Set attachment state immediately (mirroring the session
                    // manager Enter handler) so a second Sessions reply in the
                    // same tick cannot auto-attach again to a different
                    // session, and so the page renders the target session
                    // instead of a blank screen until SessionAttached arrives.
                    // Hand the input bar over like every other attach path;
                    // with nothing attached yet this only loads the target's
                    // draft (see `persist_input_draft`).
                    self.persist_input_draft(first.session_id);
                    self.reset_for_session_switch(first.session_id);
                    self.attached_session_id = Some(first.session_id);
                    // The auto-attach changed the attached session, so the
                    // window title and program-status records must refresh.
                    self.term_status_dirty = true;
                    self.pending.send(
                        client_tx,
                        ClientMessageType::AttachSession {
                            session_id: first.session_id,
                        },
                    );
                } else {
                    // Inherit account_name from the first available account,
                    // so the auto-created default session doesn't lose the
                    // account selection that was already configured.
                    let default_account =
                        self.ai_providers.accounts.first().map(|a| a.name.clone());
                    // Deliberately no working directory: the daemon may serve a
                    // remote client over TCP, so the TUI process's own cwd is
                    // meaningless on the daemon host and could set a nonexistent
                    // session working directory. The working directory is chosen
                    // later (e.g. via `set_working_dir` or when attaching a
                    // session that already has one).
                    self.pending.send(
                        client_tx,
                        ClientMessageType::CreateSession {
                            title: Some("default".to_string()),
                            parent_session_id: None,
                            working_dir: None,
                            context_config: None,
                            account_name: default_account,
                            selected_model: None,
                            reasoning_effort: None,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_session_deleted(&mut self, session_id: u64) {
        self.session_mgr.remove_session(session_id);
        self.session_displays.remove(&session_id);
        self.rendered_images.remove(&session_id);
        if self.attached_session_id == Some(session_id) {
            self.attached_session_id = None;
            self.active_session_id = None;
            self.attached_account_slug = None;
            // Drop the deleted session's cached status and tool groups too.
            // They describe the attachment that just went away; leaving them
            // set would both render a stale status bar (there is no attached
            // session to describe anymore) and mislead the submit-time
            // idle-guard in `connection/chat.rs`, which reads `attached_status`
            // and would wrongly reject a prompt as "session not idle" when
            // nothing is attached at all.
            //
            // This restores the invariant `attached_session_id == None`
            // implies `attached_status == None`, which the auto-attach path in
            // `handle_sessions` relies on: it re-binds `attached_session_id`
            // (and only that) before the daemon's `SessionAttached` reply
            // refreshes the status, so a stale `attached_status` would leak
            // across the switch.  Every path that clears `attached_session_id`
            // must clear these two alongside it.
            self.attached_status = None;
            self.attached_tool_groups.clear();
            // The deleted session's unsent prompt dies with it — the display
            // (and its draft) are gone above, so drop the input bar too
            // rather than leak the orphaned text into whichever session gets
            // attached next.
            let had_input = !self.input.text.is_empty();
            tracing::debug!(
                session_id,
                had_input,
                "deleted attached session: dropping its input draft",
            );
            self.input.clear();
            self.commit_to_history();
        }
        // Drop the deleted session's terminal records too, and re-publish:
        // its child record must be cleared (the publisher's sync clears an id
        // that vanished from the desired set) and, if it was attached, the
        // window title falls back to the plain program name.
        self.term_status_override.remove(&session_id);
        self.term_status_dirty = true;
    }

    pub(crate) fn handle_session_delete_failed(&mut self, session_id: u64, error: &str) {
        self.status = Some(format!("failed to delete session {session_id}: {error}"));
    }

    /// A per-session `pinned`/`archived_at` flag change was broadcast by the
    /// daemon — the success signal for a `SetSessionPinned`/`SetSessionArchived`
    /// request.  There is no targeted success reply, so the TUI deliberately
    /// does NOT mutate its own list on the keypress; this handler is what
    /// applies the change (a failure instead arrives as `SessionEvent::SessionFailed`).
    pub(crate) fn handle_session_flags_changed(
        &mut self,
        session_id: u64,
        pinned: bool,
        archived_at: Option<i64>,
    ) {
        self.session_mgr
            .apply_session_flags(session_id, pinned, archived_at);
    }

    pub(crate) fn display_token_usage(&self) -> Option<TokenUsage> {
        let display = self.active_display_ref()?;
        let usage = display.token_usage.as_ref()?;
        Some(TokenUsage {
            input_tokens: usage.input_tokens + display.live_input_estimate,
            output_tokens: usage.output_tokens + display.live_output_tokens,
            total_tokens: usage.total_tokens
                + display.live_input_estimate
                + display.live_output_tokens,
            // Cached tokens are only reported in settled usage, not in the live
            // per-chunk estimates, so carry the settled value through untouched.
            // The cache-write count is settled-only for the same reason.
            cached_tokens: usage.cached_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        })
    }
}

/// Merge a daemon-provided token usage into the display's accumulated value,
/// never regressing it.
///
/// Cumulative token usage only ever increases (the daemon accumulates per-turn
/// usage monotonically), so the merge is a per-field max via
/// [`TokenUsage::merge_max`].  This matters when switching into a session that
/// is mid-turn: the attach `SessionState` snapshot is built from the session
/// thread's config, which can lag the request worker's live accumulation (and,
/// in the worker→main sync window, the value already broadcast to this client).
/// A blind overwrite would regress the status bar's `↑in ↓out` readout until
/// the next `TokenUsageUpdate` — i.e. until the turn ends — while a `None`
/// snapshot must never wipe an accumulated total.
// Call sites in `connection/daemon.rs` pass `&Option<TokenUsage>`;
// changing the signature would touch files outside this one.
#[expect(clippy::ref_option)]
pub(crate) fn merge_token_usage(
    current: &Option<TokenUsage>,
    incoming: &Option<TokenUsage>,
) -> Option<TokenUsage> {
    match (current, incoming) {
        (Some(cur), Some(inc)) => {
            let mut merged = *cur;
            merged.merge_max(*inc);
            Some(merged)
        }
        (Some(cur), None) => Some(*cur),
        (None, Some(inc)) => Some(*inc),
        (None, None) => None,
    }
}
