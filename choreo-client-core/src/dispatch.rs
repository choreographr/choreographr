use choreo_proto::{
    DaemonMessage, OutputStream, ReasoningCapability, SessionEvent, SessionStatus, TokenUsage, Turn,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub enum ToolCallEvent {
    Started {
        call_id: String,
        tool_name: String,
        arguments_json: String,
        /// Human-readable invocation description from `ToolCallStarted` (e.g.
        /// "Running command: `cargo build`.") so clients can render the tool's
        /// context immediately, before any streaming output arrives.
        invocation_description: String,
    },
    Finished {
        call_id: String,
        tool_name: String,
    },
    Failed {
        call_id: String,
        tool_name: String,
        error: String,
    },
}

/// Grouped payload for [`TurnEventHandler::handle_session_state`].
#[derive(Debug, Clone)]
pub struct SessionStateData {
    pub session_id: u64,
    pub turns: BTreeMap<u32, Turn>,
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub active_tool_groups: Vec<String>,
    pub token_usage: Option<TokenUsage>,
    pub context_window: Option<u32>,
    pub last_prompt_tokens: Option<u32>,
    pub status: SessionStatus,
    pub reasoning_effort: Option<String>,
    pub reasoning_capability: Option<ReasoningCapability>,
}

pub trait TurnEventHandler {
    fn handle_turn_appended(&mut self, session_id: u64, turn_id: u32, turn: Turn);
    fn handle_turns_undone(&mut self, session_id: u64, turn_ids: &[u32]);
    fn handle_turns_redone(&mut self, session_id: u64, turns: BTreeMap<u32, Turn>);
    fn handle_request_stream(
        &mut self,
        session_id: u64,
        request_id: u32,
        stream: OutputStream,
        data: Cow<'_, str>,
    );
    fn handle_started(
        &mut self,
        session_id: u64,
        request_id: u32,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    );
    fn handle_done(
        &mut self,
        session_id: u64,
        request_id: u32,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    );
    fn handle_failed(&mut self, session_id: Option<u64>, request_id: u32, error: String);
    fn handle_tool_call_event(&mut self, session_id: u64, request_id: u32, event: ToolCallEvent);
    fn handle_tool_result_chunk(
        &mut self,
        session_id: u64,
        request_id: u32,
        call_id: String,
        data: Vec<u8>,
    );
    fn handle_session_state(&mut self, state: SessionStateData);
    fn handle_status_text(&mut self, text: String);
    fn handle_error(&mut self, error: String);
    fn handle_session_attached(&mut self, session_id: u64);
    // The parameter list mirrors the SessionCreated message fields 1:1; a
    // struct would just re-wrap fields the dispatcher already destructures.
    #[allow(clippy::too_many_arguments)]
    fn handle_session_created(
        &mut self,
        session_id: u64,
        title: Option<String>,
        working_dir: Option<String>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
    );
    fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: SessionStatus,
        last_modified: i64,
    );
    fn handle_token_usage_update(
        &mut self,
        session_id: u64,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    );
}

/// Dispatch a [`DaemonMessage`] to the [`TurnEventHandler`], splitting the
/// two v4 families before any per-arm work:
/// - [`DaemonMessage::Session`] — a session-scoped [`SessionEvent`] wrapped in
///   the envelope that hoists the origin session id. `dispatch_session_event`
///   resolves that origin exactly once (the reference is destructured in its
///   value position there, so every arm below reads a single `session_id`)
///   and then handles the inner event; the flat variants never appear in its
///   match.
/// - The 23 flat connection/reply/global variants — replies to the client's
///   own requests (`Sessions`, `Models`, `Pong`, keystore/account replies,
///   catalog/refresh replies, …), handled by `dispatch_flat_message`.
pub fn dispatch_daemon_message(msg: &DaemonMessage, handler: &mut impl TurnEventHandler) {
    debug!("dispatching daemon message: {msg:?}");
    match msg {
        DaemonMessage::Session { session_id, event } => {
            dispatch_session_event(session_id, event, handler);
        }
        flat => dispatch_flat_message(flat, handler),
    }
}

/// Dispatch a flat (non-session-scoped) [`DaemonMessage`]: connection control
/// replies (`Pong`, `ShuttingDown`, `Evicted`), request replies (`Sessions`,
/// `Models`/`ModelsFailed`, keystore + account replies), and catalog/refresh
/// replies. Every variant is enumerated explicitly — the variant set IS the
/// wire contract, so a NEW `DaemonMessage` variant must be triaged here at
/// compile time instead of being silently swallowed by a wildcard arm
/// (matching the same rule `dispatch_session_event` applies to its
/// `SessionEvent` match).
fn dispatch_flat_message(msg: &DaemonMessage, handler: &mut impl TurnEventHandler) {
    match msg {
        DaemonMessage::Sessions { .. } => {
            // Handled upstream by the caller before dispatch.
        }
        DaemonMessage::Pong => {
            handler.handle_status_text("[daemon] pong".to_string());
        }
        DaemonMessage::ShuttingDown => {
            handler.handle_status_text("[daemon] shutting down".to_string());
        }
        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if models.is_empty() {
                handler.handle_status_text("[daemon] no models available".to_string());
            } else {
                let mut lines = vec![format!("[daemon] supported models ({})", models.len())];
                for model in models {
                    let prefix = if selected_model.as_deref() == Some(model.as_str()) {
                        "*"
                    } else {
                        "-"
                    };
                    lines.push(format!("{prefix} {model}"));
                }
                handler.handle_status_text(lines.join("\n"));
            }
        }
        DaemonMessage::ModelsFailed { error } => {
            handler.handle_error(format!("[daemon] models failed: {error}"));
        }
        DaemonMessage::Unlocked => {
            handler.handle_status_text(
                "[daemon] keystore unlocked, credentials available".to_string(),
            );
        }
        DaemonMessage::Locked => {
            handler.handle_status_text("[daemon] keystore locked, credentials cleared".to_string());
        }
        DaemonMessage::LockedError { error } => {
            handler.handle_error(format!("[daemon] locked: {error}"));
        }
        // Targeted reply to a successful BindKeystore: the binding was
        // created and the daemon unlocked. Text only — the caller that SENT
        // the bind records the key on this confirmation.
        DaemonMessage::Bound => {
            handler.handle_status_text("[daemon] keystore bound and unlocked".to_string());
        }
        // Verify-only operation against an unbound keystore: distinct from
        // LockedError so callers can distinguish "never bound — auto-bind
        // with a fresh key" from "bound but wrong key".
        DaemonMessage::KeystoreUnbound { error } => {
            handler.handle_error(format!("[daemon] {error}"));
        }
        DaemonMessage::CredentialAdded { service } => {
            handler.handle_status_text(format!("[daemon] credential added: {service}"));
        }
        DaemonMessage::CredentialAddFailed { service, error } => {
            handler.handle_error(format!(
                "[daemon] credential add failed ({service}): {error}"
            ));
        }
        DaemonMessage::CredentialRemoved { service } => {
            handler.handle_status_text(format!("[daemon] credential removed: {service}"));
        }
        DaemonMessage::CredentialRemoveFailed { service, error } => {
            handler.handle_error(format!(
                "[daemon] credential remove failed ({service}): {error}"
            ));
        }
        DaemonMessage::AclAddResult { ok, message } => {
            if *ok {
                handler.handle_status_text(format!("[daemon] {message}"));
            } else {
                handler.handle_error(format!("[daemon] acl add failed: {message}"));
            }
        }
        DaemonMessage::AclUpdated { clients } => {
            handler.handle_status_text(format!(
                "[daemon] ACL updated — {clients} authorized client(s)"
            ));
        }
        DaemonMessage::Credential { .. } => {}
        DaemonMessage::AccountAdded { name } => {
            handler.handle_status_text(format!("[daemon] account added: {name}"));
        }
        DaemonMessage::AccountAddFailed { name, error } => {
            handler.handle_error(format!("[daemon] failed to add account {name}: {error}"));
        }
        DaemonMessage::AccountRemoved { name } => {
            handler.handle_status_text(format!("[daemon] account removed: {name}"));
        }
        DaemonMessage::AccountRemoveFailed { name, error } => {
            handler.handle_error(format!("[daemon] failed to remove account {name}: {error}"));
        }
        DaemonMessage::Accounts { accounts } => {
            if accounts.is_empty() {
                handler.handle_status_text("[daemon] no accounts configured".to_string());
            } else {
                let mut lines = vec![format!("[daemon] accounts ({})", accounts.len())];
                for a in accounts {
                    lines.push(format!("  {}: {}", a.name, a.provider));
                }
                handler.handle_status_text(lines.join("\n"));
            }
        }
        DaemonMessage::AccountListFailed { error } => {
            handler.handle_error(format!("[daemon] failed to list accounts: {error}"));
        }
        // Explicit no-ops, enumerated so a new flat variant still forces this
        // match to grow:
        // - ModelsRefreshed/ModelsRefreshFailed/CatalogUpdated: catalog-level
        //   replies surfaced by the connection layer, not by the generic text
        //   dispatch.
        // - Evicted: the best-effort advisory travels ahead of the
        //   disconnect and the connection layer shows it.
        DaemonMessage::ModelsRefreshed { .. }
        | DaemonMessage::ModelsRefreshFailed { .. }
        | DaemonMessage::CatalogUpdated { .. }
        | DaemonMessage::Evicted => {
            debug!("flat daemon message has no generic-dispatch text: {msg:?}");
        }
        // A `Session` envelope here is a routing bug — `dispatch_daemon_message`
        // splits the two families before calling this function, so only
        // non-session messages can reach it at runtime. The arm is still
        // REQUIRED at compile time: this match is on the full `DaemonMessage`
        // enum and, with no wildcard (the variant set IS the wire contract),
        // the `Session` variant must be named explicitly for the match to
        // compile. That also makes the arm the tripwire: if a future refactor
        // ever routes an envelope here, it fails loudly instead of silently
        // dropping the event.
        DaemonMessage::Session {
            session_id, event, ..
        } => {
            warn!(
                session_id,
                "session envelope reached the flat-message dispatch; event is dropped: {event:?}"
            );
        }
    }
}

/// Dispatch the inner [`SessionEvent`] of a [`DaemonMessage::Session`]
/// envelope to the handler, resolving the origin session id exactly once on
/// the envelope.
///
/// Connection-level replies arrive without an origin session (`None`) — the
/// daemon synthesizes them on its connection dispatch when there is no
/// session task to supply an origin (e.g. `Failed` "no session attached").
/// Six events are None-capable: the two failure-shaped ones below, plus
/// `ModelSelectionFailed`/`ReasoningEffortSet(`/`Failed`)/`SessionFailed` —
/// which never use the origin in this generic dispatch (they surface via
/// `handle_error`/`handle_status_text`), so the `None` case must not be
/// dropped before them. All six can ALSO arrive with `Some` from
/// session-task broadcasts, so they are handled here for both origins (the
/// `Some`-origin variants fall through the pre-match's early `return`s only
/// when the arm has nothing to do with the id). Every remaining
/// `SessionEvent` requires `Some`, enforced by the guard below. This keeps
/// the no-origin case explicit instead of a magic `session_id: 0` leaking
/// into handler code.
fn dispatch_session_event(
    session_id: &Option<u64>,
    event: &SessionEvent,
    handler: &mut impl TurnEventHandler,
) {
    // None-capable events: their handlers take the origin as-is
    // (`Option<u64>` for `handle_failed`, or not at all), so a `None` envelope
    // must not be treated as "drop the event".
    //
    // PAIRING RULE: each event handled here MUST also be listed in the
    // explicit dead arm at the bottom of the requires-origin match below.
    // The compiler enforces the pairing in the direction that matters — a
    // pre-handled event missing from the lower match leaves it non-exhaustive
    // (compile error). The other direction is a runtime risk, so keep it in
    // mind when extending: an event added ONLY to the lower match is treated
    // as requires-origin, so a `None`-origin instance of it would be dropped
    // with a warn instead of reaching its handler. New None-capable events
    // touch BOTH sites; new requires-origin events touch only the lower match.
    match event {
        SessionEvent::Failed { request_id, error } => {
            // `session_id` is `&Option<u64>` here, so `as_ref().copied()`
            // yields the `Option<u64>` the handler wants (`copied()` itself
            // only exists on `Option<&T>`).
            handler.handle_failed(session_id.as_ref().copied(), *request_id, error.clone());
            return;
        }
        SessionEvent::Cancelled { request_id } => {
            handler.handle_failed(
                session_id.as_ref().copied(),
                *request_id,
                "cancelled".to_string(),
            );
            return;
        }
        SessionEvent::ModelSelectionFailed { model, error } => {
            handler.handle_error(format!("[daemon] failed to select model {model}: {error}"));
            return;
        }
        SessionEvent::ReasoningEffortSet { effort, .. } => {
            handler.handle_status_text(format!("[daemon] reasoning effort: {effort}"));
            return;
        }
        SessionEvent::ReasoningEffortSetFailed { effort, error, .. } => {
            handler.handle_error(format!(
                "[daemon] failed to set reasoning effort {effort}: {error}"
            ));
            return;
        }
        SessionEvent::SessionFailed { error, .. } => {
            handler.handle_error(error.clone());
            return;
        }
        _ => {}
    }

    // Every event left after the pre-match needs a real origin session. A
    // `None` here is a producer bug or a malformed frame — dropping the event
    // would silently lose client-visible data, so this is a warn, not a
    // debug, and the event is not dispatched.
    let Some(session_id) = session_id else {
        warn!("session-scoped event without an origin session, dropping it: {event:?}");
        return;
    };

    match event {
        SessionEvent::SessionCreated {
            title,
            working_dir,
            account_name,
            selected_model,
            reasoning_effort,
            ..
        } => {
            handler.handle_session_created(
                *session_id,
                title.clone(),
                working_dir.clone(),
                account_name.clone(),
                selected_model.clone(),
                reasoning_effort.clone(),
            );
        }
        SessionEvent::SessionAttached => {
            handler.handle_session_attached(*session_id);
        }
        SessionEvent::SessionState {
            title,
            selected_model,
            turns,
            active_tool_groups,
            token_usage,
            context_window,
            last_prompt_tokens,
            status,
            reasoning_effort,
            reasoning_capability,
            ..
        } => {
            handler.handle_session_state(SessionStateData {
                session_id: *session_id,
                turns: turns.clone(),
                title: title.clone(),
                selected_model: selected_model.clone(),
                active_tool_groups: active_tool_groups.clone(),
                token_usage: *token_usage,
                context_window: *context_window,
                last_prompt_tokens: *last_prompt_tokens,
                status: status.clone(),
                reasoning_effort: reasoning_effort.clone(),
                reasoning_capability: reasoning_capability.clone(),
            });
        }
        SessionEvent::TurnAppended { turn_id, turn } => {
            handler.handle_turn_appended(*session_id, *turn_id, turn.clone())
        }
        SessionEvent::TurnsUndone { turn_ids } => {
            handler.handle_turns_undone(*session_id, turn_ids)
        }
        SessionEvent::TurnsRedone { turns } => {
            handler.handle_turns_redone(*session_id, turns.clone())
        }
        SessionEvent::Started {
            request_id,
            turn_id,
            estimated_prompt_tokens,
        } => handler.handle_started(*session_id, *request_id, *turn_id, *estimated_prompt_tokens),
        SessionEvent::OutputChunk {
            request_id,
            stream,
            data,
        } => handler.handle_request_stream(
            *session_id,
            *request_id,
            stream.clone(),
            String::from_utf8_lossy(data),
        ),
        SessionEvent::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
            invocation_description,
        } => handler.handle_tool_call_event(
            *session_id,
            *request_id,
            ToolCallEvent::Started {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                invocation_description: invocation_description.clone(),
            },
        ),
        SessionEvent::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
        } => handler.handle_tool_call_event(
            *session_id,
            *request_id,
            ToolCallEvent::Finished {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
            },
        ),
        SessionEvent::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => handler.handle_tool_call_event(
            *session_id,
            *request_id,
            ToolCallEvent::Failed {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                error: error.clone(),
            },
        ),
        SessionEvent::ToolResultChunk {
            request_id,
            call_id,
            data,
        } => handler.handle_tool_result_chunk(
            *session_id,
            *request_id,
            call_id.clone(),
            data.clone(),
        ),
        SessionEvent::Done {
            request_id,
            token_usage,
            last_prompt_tokens,
        } => handler.handle_done(*session_id, *request_id, *token_usage, *last_prompt_tokens),
        SessionEvent::SessionStatusChanged {
            status,
            last_modified,
        } => handler.handle_session_status_changed(*session_id, status.clone(), *last_modified),
        SessionEvent::ModelSelected { model, .. } => {
            handler.handle_status_text(format!("[daemon] selected model: {model}"));
        }
        SessionEvent::SessionDeleted => {}
        SessionEvent::SessionDeleteFailed { .. } => {}
        SessionEvent::SessionAccountSet { account, .. } => {
            handler.handle_status_text(format!("[daemon] session account set: {account}"));
        }
        SessionEvent::SessionWorkingDirSet { .. } => {}
        SessionEvent::SessionTitleSet { .. } => {
            // Session title changes are metadata-only (no conversation
            // content) and are handled at the TUI layer directly via
            // the connection.rs routing — no generic dispatch needed.
        }
        SessionEvent::TokenUsageUpdate {
            token_usage,
            last_prompt_tokens,
        } => handler.handle_token_usage_update(*session_id, *token_usage, *last_prompt_tokens),
        SessionEvent::LiveOutputTokenCount { .. } => {
            // Handled at the TUI layer in connection.rs — no generic dispatch needed.
        }
        SessionEvent::ContextWindowResolved { .. } => {
            // Context-window resolution is metadata-only (no conversation
            // content) and is handled at the TUI layer in connection.rs — no
            // generic dispatch needed.
        }
        // The six None-capable events were already handled (and returned) by
        // the pre-match block above — every other `SessionEvent` reaches this
        // match with a guaranteed `Some` origin. They are listed explicitly
        // instead of a wildcard so a NEW `SessionEvent` variant still fails
        // this exhaustive match at compile time (the variant set IS the wire
        // contract); see the pairing rule in the pre-match note above.
        SessionEvent::Failed { .. }
        | SessionEvent::Cancelled { .. }
        | SessionEvent::ModelSelectionFailed { .. }
        | SessionEvent::ReasoningEffortSet { .. }
        | SessionEvent::ReasoningEffortSetFailed { .. }
        | SessionEvent::SessionFailed { .. } => {}
    }
}
