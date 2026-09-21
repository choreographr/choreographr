//! Per-session prompt-draft capture and past-prompt recall.
//!
//! The Chat composer is a single shared input buffer, so the text a user has
//! typed but not yet submitted must be stashed per session and restored on the
//! next visit — see [`App::persist_input_draft`].  This module owns that draft
//! hand-off together with the `Up`/`Down` recall of past prompts
//! ([`App::navigate_history_up`]/[`App::navigate_history_down`]), the eager
//! detach of a recalled entry into the draft on the first real edit
//! ([`App::edit_input`]), and the exit/clear paths.  All of these are inherent
//! `App` methods living in this sibling module; their fields (`App::history_index`
//! and `SessionDisplayState::draft`/`draft_cursor`) stay on those structs in
//! `state/mod.rs`.
//!
//! Named `draft` rather than `history` because "history" in this crate already
//! unambiguously means the conversation scrollback pane (`HistoryScrollState`,
//! `render_history`, …); this module is about the prompt draft and the
//! recall-of-past-prompts machinery that feeds it.

use super::App;
use crossterm::event::KeyEvent;

impl App {
    pub(crate) fn user_texts(&self) -> Vec<String> {
        self.active_display_ref()
            .map(|d| {
                d.view
                    .turns
                    .iter()
                    .rev()
                    .filter_map(|(_, turn)| turn.user_text.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn navigate_history_up(&mut self) {
        // Recall requires an empty draft: with a non-empty prompt (and not
        // already browsing) history is unreachable, so this is a no-op.  The
        // caller (`connection/chat.rs`) handles the "move to line start"
        // behavior for a non-empty first-line Up.
        if self.history_index.is_none() && !self.input.text.is_empty() {
            return;
        }
        let texts = self.user_texts();
        if texts.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            // First Up press, from the empty draft: load the newest entry.
            self.load_history_entry(0, &texts);
            return;
        }
        if let Some(raw_idx) = self.history_index {
            // history_index was recorded against the turn list as it existed
            // when the user first pressed Up. The list may have shrunk since
            // (turns replaced, session switched), leaving the index past the
            // oldest remaining entry — clamp it and resync the displayed text
            // so Up mirrors Down (which clamps too) instead of silently
            // no-op'ing on a stale index.
            let idx = raw_idx.min(texts.len().saturating_sub(1));
            if idx != raw_idx {
                tracing::debug!(
                    stale = raw_idx,
                    clamped = idx,
                    "[choreo-tui] history index stale on Up; clamped to oldest remaining entry"
                );
                self.load_history_entry(idx, &texts);
                return;
            }
            let next = idx + 1;
            if next < texts.len() {
                self.load_history_entry(next, &texts);
            }
        }
    }

    pub(crate) fn navigate_history_down(&mut self) {
        if let Some(idx) = self.history_index {
            let texts = self.user_texts();
            if texts.is_empty() {
                // The conversation changed out from under us (e.g. the user
                // switched sessions mid-navigation) and no history remains to
                // walk back through — exit browsing to the empty draft.
                self.exit_history_browsing();
                return;
            }
            if idx == 0 {
                // Already at the newest entry: Down exits back to the draft.
                self.exit_history_browsing();
                return;
            }
            // history_index was recorded against the turn list as it existed
            // when the user pressed Up. The list may have shrunk since (turns
            // replaced, session switched), so a step toward the newest entry
            // can land past the end — clamp to the newest remaining entry
            // instead of indexing out of bounds.
            let prev = (idx - 1).min(texts.len() - 1);
            if idx >= texts.len() {
                tracing::debug!(
                    stale = idx,
                    clamped = prev,
                    "[choreo-tui] history index stale on Down; clamped to newest remaining entry"
                );
            }
            self.load_history_entry(prev, &texts);
        }
    }

    /// Load the history entry at `idx` into the input: record the index, set
    /// the text, move the cursor to the end, and keep it visible.  Shared by
    /// all the "step through history" paths so they can't drift apart.  The
    /// recalled text is never stashed: recall is only reachable from an empty
    /// draft, and editing it detaches it into the per-session draft (see
    /// `detach_history_on_edit`).
    fn load_history_entry(&mut self, idx: usize, texts: &[String]) {
        self.history_index = Some(idx);
        // Callers always pass a valid history index, but a stale one must not
        // panic the render path — fall back to an empty entry (no text).
        let entry = texts.get(idx).cloned().unwrap_or_default();
        self.input.text = entry;
        self.input.generation += 1;
        self.input.cursor = self.input.text.len();
        self.ensure_input_cursor_visible();
    }

    /// Leave history browsing, returning the input to the empty draft that
    /// recall began from.  Recall is only reachable from an empty prompt (see
    /// `navigate_history_up`), so there is no stash to restore — the draft was
    /// empty when browsing began.  Shared by all the "exit browsing" paths
    /// (`navigate_history_down` and `persist_input_draft`).
    fn exit_history_browsing(&mut self) {
        if self.history_index.is_none() {
            return;
        }
        self.history_index = None;
        self.input.clear();
        self.ensure_input_cursor_visible();
    }

    /// Detach the currently recalled history entry the moment the user edits
    /// it: end browsing, keeping the edit in the buffer as the session's
    /// draft.  Called eagerly, *before* any buffer-mutating keystroke, so the
    /// first edit makes the recalled text the draft instead of being discarded
    /// when browsing later ends.
    pub(crate) fn detach_history_on_edit(&mut self) {
        if self.history_index.is_some() {
            self.history_index = None;
            tracing::debug!("[choreo-tui] history entry edited; detaching it into the draft");
        }
    }

    /// Feed a key to the shared input buffer and, if it actually changed the
    /// buffer's text, detach a recalled history entry into the draft.
    ///
    /// This is the single place that answers *"did this key edit the draft?"*
    /// so the connection-layer key handler never has to re-enumerate the
    /// mutating chords — that hand-maintained list lived next to the routing
    /// code and silently drifted from `InputBuffer::handle_key`, so a newly
    /// mutating chord would fail to detach a recalled entry.  A pure cursor
    /// move (`Left`/`Right`/`Home`/`End`, Ctrl+arrows) or an ignored key (an
    /// `Alt`+letter chord, a NUL) leaves browsing intact, while any real edit —
    /// typing, Backspace/Delete, Ctrl+W/Ctrl+U/Ctrl+Delete — ends browsing and
    /// keeps the edit in the buffer as the session's draft.
    ///
    /// The buffer's `generation` counter is bumped by every text mutation and
    /// by nothing else (cursor moves and no-op deletes leave it untouched), so
    /// a change in it is the precise "the text was edited" signal — no need to
    /// classify keys by hand.
    pub(crate) fn edit_input(&mut self, key: KeyEvent) {
        let before = self.input.generation;
        self.input.handle_key(key);
        if self.input.generation != before {
            self.detach_history_on_edit();
        }
        self.ensure_input_cursor_visible();
    }

    pub(crate) fn commit_to_history(&mut self) {
        self.history_index = None;
    }

    /// Clear the draft stashed for the currently attached session, mirroring
    /// the input bar being cleared when a prompt is submitted.  Without this
    /// a submitted prompt would resurface the next time the user returns to
    /// the session — the draft must reflect only *unsent* text.
    pub(crate) fn clear_current_draft(&mut self) {
        if let Some(session_id) = self.attached_session_id {
            let display = self.display_for(session_id);
            if !display.draft.is_empty() {
                tracing::trace!(session_id, "cleared per-session draft after submit");
            }
            display.draft.clear();
            display.draft_cursor = 0;
        }
    }

    /// Persist the input bar's current contents as the draft of the session
    /// the user is leaving, then load the target session's saved draft into
    /// the input bar.
    ///
    /// The input bar is a single shared buffer; without this hand-off an
    /// unsent prompt typed in one session would follow the user into the
    /// next.  Drafts live in the per-session display state, so they survive
    /// session switches and are dropped only when the session itself is
    /// deleted (`handle_session_deleted` removes the whole display).
    ///
    /// Callers invoke this *before* rebinding `attached_session_id` so it
    /// still names the session the input bar's current contents belong to.
    /// History browsing interacts with the draft: if the user is mid-browse
    /// (Up), the buffer holds a recalled history entry rather than a real
    /// draft — first exit browsing (`exit_history_browsing`) so the recalled
    /// entry is not stashed as the outgoing session's draft (recall began
    /// from an empty draft, so exiting restores that empty buffer).  With
    /// nothing attached (the startup auto-attach), there is no outgoing
    /// session to stash into; text already in the bar is kept rather than
    /// clobbered, and only a target with an empty bar gets its draft loaded.
    pub(crate) fn persist_input_draft(&mut self, target_session_id: u64) {
        if self.history_index.is_some() {
            self.exit_history_browsing();
        }
        // Destructure `self` so the input buffer and the per-session display
        // map can be borrowed mutably at the same time (disjoint fields via
        // the pattern) — the hand-off below then moves `String`s around
        // instead of cloning them.
        let Self {
            input,
            session_displays,
            attached_session_id,
            ..
        } = self;
        // Stash the outgoing session's input into its display, overwriting any
        // earlier draft.  The text is moved out of the buffer (not cloned), so
        // switching sessions transfers the bytes without allocating.
        if let Some(prev_id) = attached_session_id {
            let had_draft = !input.text.is_empty();
            let text = std::mem::take(&mut input.text);
            let cursor = input.cursor;
            let display = session_displays.entry(*prev_id).or_default();
            display.draft = text;
            display.draft_cursor = cursor;
            tracing::debug!(
                from_session = *prev_id,
                to_session = target_session_id,
                had_draft,
                "session switch: stashed outgoing input as per-session draft",
            );
        } else if !input.text.is_empty() {
            // Nothing is attached yet (the startup auto-attach path): the
            // input bar holds text typed before any session existed.  There is
            // no session to stash it into, so clobbering it with the target's
            // (empty) draft would silently destroy the user's typing — keep it.
            tracing::debug!(
                to_session = target_session_id,
                "auto-attach: keeping pre-attach input (no outgoing session)",
            );
            return;
        }
        // Move the target session's draft into the input bar.  Swapping
        // rather than cloning transfers the bytes; the target's draft slot is
        // emptied, which is fine — the input bar now owns that content and the
        // slot is overwritten again on the next switch away.
        let display = session_displays.entry(target_session_id).or_default();
        std::mem::swap(&mut input.text, &mut display.draft);
        std::mem::swap(&mut input.cursor, &mut display.draft_cursor);
        input.generation += 1;
        self.ensure_input_cursor_visible();
    }
}
