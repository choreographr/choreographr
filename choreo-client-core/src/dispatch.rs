//! Daemon-message dispatch: turn decoded protocol events into UI callbacks.
//!
//! [`dispatch_daemon_message`] is the entry point the connection layer (and
//! any front-end) calls for every [`DaemonMessage`] it reads. It routes each
//! message to the [`TurnEventHandler`] methods the front-end implements, so
//! the TUI and GUI share one wire-to-UI mapping instead of decoding the
//! protocol themselves. The dispatch splits the two message families —
//! session-scoped [`SessionEvent`]s (wrapped in the `Session` envelope) and
//! the flat connection/reply variants — and enumerates every variant
//! explicitly, so a new wire variant forces a compile-time decision here
//! rather than being silently swallowed.

use choreo_proto::{
    DaemonMessage, DaemonMessageType, OutputStream, ReasoningCapability, SessionEvent,
    SessionStatus, TokenUsage, Turn,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tracing::{debug, warn};

/// A tool-call lifecycle event, resolved from the wire's
/// `ToolCall{Started,Finished,Failed}` session events for
/// [`TurnEventHandler::handle_tool_call_event`].
#[derive(Debug, Clone)]
pub enum ToolCallEvent {
    /// A tool call started; carries the invocation details.
    Started {
        /// The daemon-assigned id correlating this call's streamed result
        /// chunks and terminal event.
        call_id: String,
        /// The tool's registered name (e.g. `read_file`).
        tool_name: String,
        /// The call's arguments as the model supplied them, serialized JSON.
        arguments_json: String,
        /// Human-readable invocation description from `ToolCallStarted` (e.g.
        /// "Running command: `cargo build`.") so clients can render the tool's
        /// context immediately, before any streaming output arrives.
        invocation_description: String,
    },
    /// A tool call completed successfully.
    Finished {
        /// The id correlating this call's result chunks.
        call_id: String,
        /// The tool's registered name.
        tool_name: String,
    },
    /// A tool call failed.
    Failed {
        /// The id correlating this call's result chunks.
        call_id: String,
        /// The tool's registered name.
        tool_name: String,
        /// The failure message the tool reported.
        error: String,
    },
}

/// Grouped payload for [`TurnEventHandler::handle_session_state`].
#[derive(Debug, Clone)]
pub struct SessionStateData {
    /// The session this state describes.
    pub session_id: u64,
    /// The session's turns keyed by turn id, in turn order.
    pub turns: BTreeMap<u32, Turn>,
    /// The session title, if set.
    pub title: Option<String>,
    /// The model currently selected for the session, if pinned.
    pub selected_model: Option<String>,
    /// The tool groups currently active on the session.
    pub active_tool_groups: Vec<String>,
    /// Cumulative token usage for the session, when known.
    pub token_usage: Option<TokenUsage>,
    /// The model's context-window size in tokens, once resolved.
    pub context_window: Option<u32>,
    /// The prompt-token count of the most recent request.
    pub last_prompt_tokens: Option<u32>,
    /// The session's current lifecycle status.
    pub status: SessionStatus,
    /// The session's current reasoning-effort setting, if any.
    pub reasoning_effort: Option<String>,
    /// The selected model's reasoning capability, once the catalog resolves
    /// it.
    pub reasoning_capability: Option<ReasoningCapability>,
}

/// The callback surface a front-end implements to receive dispatched daemon
/// events.
///
/// Each method corresponds to a client-visible wire event; the dispatcher
/// ([`dispatch_daemon_message`]) is the only caller. Implementors own their
/// render state and update it here — the trait itself carries no state.
/// Methods with a default body are events most front-ends can ignore.
pub trait TurnEventHandler {
    /// A turn was appended to the session, given its id and full [`Turn`].
    fn handle_turn_appended(&mut self, session_id: u64, turn_id: u32, turn: Turn);
    /// The turns with the given ids were undone.
    fn handle_turns_undone(&mut self, session_id: u64, turn_ids: &[u32]);
    /// The given turns were redone (keyed by turn id, in turn order).
    fn handle_turns_redone(&mut self, session_id: u64, turns: BTreeMap<u32, Turn>);
    /// A chunk of streamed output (`stream`) arrived for `stream_id`,
    /// carrying `data` (lossily converted to UTF-8).
    fn handle_request_stream(
        &mut self,
        session_id: u64,
        stream_id: u64,
        stream: OutputStream,
        data: Cow<'_, str>,
    );
    /// A request started on `stream_id`, producing turn `turn_id` with the
    /// given estimated prompt-token count.
    fn handle_started(
        &mut self,
        session_id: u64,
        stream_id: u64,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    );
    /// A request completed, with the final token usage and last prompt-token
    /// count (either `None` when the model reported none).
    fn handle_done(
        &mut self,
        session_id: u64,
        stream_id: u64,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    );
    /// A request failed. `session_id` is `None` for connection-level failures
    /// with no originating session.
    fn handle_failed(&mut self, session_id: Option<u64>, stream_id: u64, error: String);
    /// A request was cancelled by the user. Unlike [`Self::handle_failed`], this
    /// is not an error — implementations should report it as a cancel, not a
    /// failure. `session_id` is `None` for connection-level cancels with no
    /// originating session.
    fn handle_cancelled(&mut self, session_id: Option<u64>, stream_id: u64);
    /// A tool-call lifecycle event occurred for `stream_id`.
    fn handle_tool_call_event(&mut self, session_id: u64, stream_id: u64, event: ToolCallEvent);
    /// A chunk of raw tool-result bytes arrived for `call_id` on `stream_id`.
    fn handle_tool_result_chunk(
        &mut self,
        session_id: u64,
        stream_id: u64,
        call_id: String,
        data: Vec<u8>,
    );
    /// The session's full state snapshot arrived (at subscribe time and on
    /// change).
    fn handle_session_state(&mut self, state: SessionStateData);
    /// A human-readable status line arrived for display.
    fn handle_status_text(&mut self, text: String);
    /// An error message arrived for display.
    fn handle_error(&mut self, error: String);
    /// This client became attached to `session_id`.
    fn handle_session_attached(&mut self, session_id: u64);
    /// A session this client created was confirmed, with its initial
    /// metadata.
    fn handle_session_created(
        &mut self,
        session_id: u64,
        title: Option<String>,
        working_dir: Option<String>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
    );
    /// A session's lifecycle status changed; `last_modified` is the daemon's
    /// update timestamp in milliseconds.
    fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: SessionStatus,
        last_modified: i64,
    );
    /// A session's cumulative token usage advanced.
    fn handle_token_usage_update(
        &mut self,
        session_id: u64,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    );
    /// A previously-requested turn attachment arrived (the reply to
    /// `ClientMessageType::GetImage`). `data: Some(bytes)` carries the image; `None`
    /// means it was not found (deleted/evicted session or turn, or a stale
    /// key). `key` echoes the request so a client with several fetches in flight
    /// can route the reply to the right slot. Only clients that render image
    /// *bytes* need this — the default is a no-op so metadata-only frontends
    /// (e.g. the GUI's label) ignore it.
    fn handle_image(
        &mut self,
        session_id: u64,
        turn_id: u32,
        key: choreo_proto::ImageKey,
        data: Option<Vec<u8>>,
    ) {
        let _ = (session_id, turn_id, key, data);
    }
    /// A session's `pinned`/`archived_at` flags changed (the daemon's
    /// `SessionFlagsChanged` broadcast, delivered with the origin session).
    /// The default is a no-op so frontends that do not surface pin/archive
    /// need no code; a frontend that shows them updates its view here.
    fn handle_session_flags_changed(
        &mut self,
        _session_id: u64,
        _pinned: bool,
        _archived_at: Option<i64>,
    ) {
    }
}

/// Dispatch a [`DaemonMessage`] to the [`TurnEventHandler`], splitting the
/// two v4 families before any per-arm work:
/// - [`DaemonMessageType::Session`] — a session-scoped [`SessionEvent`] wrapped in
///   the envelope that hoists the origin session id. `dispatch_session_event`
///   resolves that origin exactly once (the reference is destructured in its
///   value position there, so every arm below reads a single `session_id`)
///   and then handles the inner event; the flat variants never appear in its
///   match.
/// - The 29 flat connection/reply/global variants — replies to the client's
///   own requests (`Sessions`, `Models`, `Pong`, keystore/account replies,
///   catalog/refresh replies, on-demand `Image`, …), handled by
///   `dispatch_flat_message`.
pub fn dispatch_daemon_message(msg: DaemonMessage, handler: &mut impl TurnEventHandler) {
    debug!("dispatching daemon message: {msg:?}");
    // The correlation id rides the envelope (`msg.id`); the front-end resolves
    // the matching pending slot before calling this (it owns the
    // `PendingReplies` table), so dispatch keys purely on the payload. The
    // session-event / flat split is on the inner payload.
    match msg.inner {
        DaemonMessageType::Session { session_id, event } => {
            // The session-event dispatch keeps borrowing its inputs: the
            // envelope is owned now, so `.as_ref()` / `&event` hand it the
            // exact `&u64` / `&SessionEvent` it always took — no clone, and no
            // need to change `dispatch_session_event`'s signature.
            dispatch_session_event(session_id.as_ref(), &event, handler);
        }
        // Move the flat envelope into `dispatch_flat_message` so its `Image`
        // arm can MOVE the (potentially multi-MB) image buffer into the
        // handler instead of cloning it.
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
fn dispatch_flat_message(msg: DaemonMessageType, handler: &mut impl TurnEventHandler) {
    match msg {
        DaemonMessageType::Sessions { .. } => {
            // Handled upstream by the caller before dispatch.
        }
        DaemonMessageType::Pong => {
            handler.handle_status_text("[daemon] pong".to_string());
        }
        DaemonMessageType::ShuttingDown => {
            handler.handle_status_text("[daemon] shutting down".to_string());
        }
        DaemonMessageType::Models {
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
        DaemonMessageType::ModelsFailed { error } => {
            handler.handle_error(format!("[daemon] models failed: {error}"));
        }
        DaemonMessageType::Unlocked => {
            handler.handle_status_text(
                "[daemon] keystore unlocked, credentials available".to_string(),
            );
        }
        DaemonMessageType::Locked => {
            handler.handle_status_text("[daemon] keystore locked, credentials cleared".to_string());
        }
        // Authoritative keystore status push (subscribe-time + transitions).
        // `Unbound` is the first-run signal from which the frontends trigger
        // their auto-bind; the status text here is informational.
        DaemonMessageType::Keystore { state } => {
            let text = match state {
                choreo_proto::KeystoreState::Unbound => {
                    "[daemon] keystore not initialized — a binding will be created automatically"
                }
                choreo_proto::KeystoreState::Locked => {
                    "[daemon] keystore locked, credentials cleared"
                }
                choreo_proto::KeystoreState::Unlocked => {
                    "[daemon] keystore unlocked, credentials available"
                }
            };
            handler.handle_status_text(text.to_string());
        }
        DaemonMessageType::LockedError { error } => {
            handler.handle_error(format!("[daemon] locked: {error}"));
        }
        // Targeted reply to a successful BindKeystore: the binding was
        // created and the daemon unlocked. Text only — the caller that SENT
        // the bind records the key on this confirmation.
        DaemonMessageType::Bound => {
            handler.handle_status_text("[daemon] keystore bound and unlocked".to_string());
        }
        // Verify-only operation against an unbound keystore: distinct from
        // LockedError so callers can distinguish "never bound — auto-bind
        // with a fresh key" from "bound but wrong key".
        DaemonMessageType::KeystoreUnbound { error } => {
            handler.handle_error(format!("[daemon] {error}"));
        }
        DaemonMessageType::CredentialAdded { service } => {
            handler.handle_status_text(format!("[daemon] credential added: {service}"));
        }
        DaemonMessageType::CredentialAddFailed { service, error } => {
            handler.handle_error(format!(
                "[daemon] credential add failed ({service}): {error}"
            ));
        }
        DaemonMessageType::CredentialRemoved { service } => {
            handler.handle_status_text(format!("[daemon] credential removed: {service}"));
        }
        DaemonMessageType::CredentialRemoveFailed { service, error } => {
            handler.handle_error(format!(
                "[daemon] credential remove failed ({service}): {error}"
            ));
        }
        DaemonMessageType::AclAddResult { ok, message } => {
            if ok {
                handler.handle_status_text(format!("[daemon] {message}"));
            } else {
                handler.handle_error(format!("[daemon] acl add failed: {message}"));
            }
        }
        DaemonMessageType::AclUpdated { clients } => {
            handler.handle_status_text(format!(
                "[daemon] ACL updated — {clients} authorized client(s)"
            ));
        }
        DaemonMessageType::Credential { .. } => {}
        DaemonMessageType::AccountAdded { name } => {
            handler.handle_status_text(format!("[daemon] account added: {name}"));
        }
        DaemonMessageType::AccountAddFailed { name, error } => {
            handler.handle_error(format!("[daemon] failed to add account {name}: {error}"));
        }
        DaemonMessageType::AccountRemoved { name } => {
            handler.handle_status_text(format!("[daemon] account removed: {name}"));
        }
        DaemonMessageType::AccountRemoveFailed { name, error } => {
            handler.handle_error(format!("[daemon] failed to remove account {name}: {error}"));
        }
        DaemonMessageType::Accounts { accounts } => {
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
        DaemonMessageType::AccountListFailed { error } => {
            handler.handle_error(format!("[daemon] failed to list accounts: {error}"));
        }
        DaemonMessageType::McpStatus {
            servers,
            project_root,
            project_trusted,
            ignored_project_servers,
        } => {
            let mut lines = Vec::new();
            if servers.is_empty() {
                lines.push("[daemon] no MCP servers configured".to_string());
            } else {
                lines.push(format!("[daemon] MCP servers ({})", servers.len()));
                for s in &servers {
                    lines.push(format!("  [{}] {}", s.tier, s.summary()));
                }
            }
            if let Some(root) = project_root {
                let state = if project_trusted {
                    "trusted"
                } else {
                    "UNTRUSTED (use /mcp trust)"
                };
                lines.push(format!("project root: {root} ({state})"));
            }
            if !ignored_project_servers.is_empty() {
                lines.push(format!(
                    "{} project server(s) ignored (untrusted): {}",
                    ignored_project_servers.len(),
                    ignored_project_servers.join(", ")
                ));
            }
            handler.handle_status_text(lines.join("\n"));
        }
        DaemonMessageType::McpReconnectFailed { slug, error } => {
            handler.handle_error(format!("[daemon] mcp reconnect {slug} failed: {error}"));
        }
        DaemonMessageType::McpReloaded { summary, servers } => {
            // The summary names what changed; the refreshed status list follows
            // so the operator sees the post-reload state without a second
            // `/mcp` request.
            let mut lines = vec![format!("[daemon] {summary}")];
            for s in &servers {
                lines.push(format!("  {}", s.summary()));
            }
            handler.handle_status_text(lines.join("\n"));
        }
        DaemonMessageType::McpReloadFailed { error } => {
            handler.handle_error(format!("[daemon] mcp reload failed: {error}"));
        }
        DaemonMessageType::McpTrustUpdated {
            root,
            trusted,
            message,
        } => {
            let _ = (root, trusted);
            handler.handle_status_text(format!("[daemon] {message}"));
        }
        DaemonMessageType::McpTrustList { roots } => {
            if roots.is_empty() {
                handler.handle_status_text("[daemon] no trusted project MCP roots".to_string());
            } else {
                let mut lines = vec![format!(
                    "[daemon] trusted project MCP roots ({})",
                    roots.len()
                )];
                for root in roots {
                    lines.push(format!("  {root}"));
                }
                handler.handle_status_text(lines.join("\n"));
            }
        }
        // On-demand turn-attachment reply (displayed image or tool-result
        // vision image). The connection layer does NOT intercept this (unlike
        // `Sessions`, handled before the generic dispatch), so it flows to the
        // handler's `handle_image`, which fills the matching placeholder (data)
        // or marks the fetch failed (None).
        DaemonMessageType::Image {
            session_id,
            turn_id,
            key,
            data,
        } => {
            // By-value match: `data` is owned here, so it MOVES into the
            // handler. This is the whole point of the by-value dispatch — a
            // default-no-op `handle_image` (GUI/ACP) still pays nothing, and
            // an image-rendering handler gets the bytes without a clone.
            handler.handle_image(session_id, turn_id, key, data);
        }
        // Explicit no-ops, enumerated so a new flat variant still forces this
        // match to grow:
        // - ModelsRefreshed/ModelsRefreshFailed/CatalogUpdated: catalog-level
        //   replies surfaced by the connection layer, not by the generic text
        //   dispatch.
        // - Evicted: the best-effort advisory travels ahead of the
        //   disconnect and the connection layer shows it.
        // The `@` binding keeps the whole owned envelope available for the
        // debug line even though the arm matches several variants by shape.
        msg @ (DaemonMessageType::ModelsRefreshed { .. }
        | DaemonMessageType::ModelsRefreshFailed { .. }
        | DaemonMessageType::CatalogUpdated { .. }
        | DaemonMessageType::Evicted) => {
            debug!("flat daemon message has no generic-dispatch text: {msg:?}");
        }
        // Terminal acknowledgement replies to the client's own requests.
        // `Accepted` is a silent success: the request's own outcome broadcast
        // (if any) carries the state, and the correlation table has already
        // resolved the pending slot. `Failed` is the terminal failure for a
        // request whose failure has no richer, session-scoped shape; surface it
        // as an error line so an otherwise-silent mutation (an empty
        // `/undo`, an unsupported request) is never swallowed.
        DaemonMessageType::Accepted { kind } => {
            debug!(?kind, "request accepted");
        }
        DaemonMessageType::Failed { kind, error } => {
            handler.handle_error(format!("[daemon] {kind:?} failed: {error}"));
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
        DaemonMessageType::Session {
            session_id, event, ..
        } => {
            warn!(
                ?session_id,
                "session envelope reached the flat-message dispatch; event is dropped: {event:?}"
            );
        }
    }
}

/// Dispatch the inner [`SessionEvent`] of a [`DaemonMessageType::Session`]
/// envelope to the handler, resolving the origin session id exactly once on
/// the envelope.
///
/// Connection-level replies arrive without an origin session (`None`) — the
/// daemon synthesizes them on its connection dispatch when there is no
/// session task to supply an origin (e.g. `Failed` "no session attached").
/// Six events are None-capable: the two request-terminal ones below —
/// `Failed` (routed to `handle_failed`) and `Cancelled` (routed to
/// `handle_cancelled`) — plus
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
    session_id: Option<&u64>,
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
        SessionEvent::Failed { stream_id, error } => {
            // `session_id` is `Option<&u64>` here, so `copied()` yields the
            // `Option<u64>` the handler wants.
            handler.handle_failed(session_id.copied(), *stream_id, error.clone());
            return;
        }
        SessionEvent::Cancelled { stream_id } => {
            // A cancel is its own terminal outcome, not a failure: route it to
            // `handle_cancelled` so the front-end reports `idle` rather than a
            // red error block.
            handler.handle_cancelled(session_id.copied(), *stream_id);
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
        SessionEvent::SessionCreatedForRequester {
            title,
            working_dir,
            account_name,
            selected_model,
            reasoning_effort,
            ..
        } => {
            // Direct reply to THIS client's CreateSession. This is the only
            // create-driven event allowed to move the view: `handle_session_
            // created` carries requester-relative intent (attach to the
            // session the local user just asked for).
            handler.handle_session_created(
                *session_id,
                title.clone(),
                working_dir.clone(),
                account_name.clone(),
                selected_model.clone(),
                reasoning_effort.clone(),
            );
        }
        SessionEvent::SessionCreated { .. } => {
            // Broadcast notification that a session now exists (created by
            // ANY client). Deliberately a no-op: routing it to
            // `handle_session_created` would attach every client to another
            // client's creation. Keeping the session list fresh is each
            // frontend's own concern (it refreshes via `ListSessions`).
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
            handler.handle_turn_appended(*session_id, *turn_id, turn.clone());
        }
        SessionEvent::TurnsUndone { turn_ids } => {
            handler.handle_turns_undone(*session_id, turn_ids);
        }
        SessionEvent::TurnsRedone { turns } => {
            handler.handle_turns_redone(*session_id, turns.clone());
        }
        SessionEvent::Started {
            stream_id,
            turn_id,
            estimated_prompt_tokens,
        } => handler.handle_started(*session_id, *stream_id, *turn_id, *estimated_prompt_tokens),
        SessionEvent::OutputChunk {
            stream_id,
            stream,
            data,
        } => handler.handle_request_stream(
            *session_id,
            *stream_id,
            stream.clone(),
            String::from_utf8_lossy(data),
        ),
        SessionEvent::ToolCallStarted {
            stream_id,
            call_id,
            tool_name,
            arguments_json,
            invocation_description,
        } => handler.handle_tool_call_event(
            *session_id,
            *stream_id,
            ToolCallEvent::Started {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                invocation_description: invocation_description.clone(),
            },
        ),
        SessionEvent::ToolCallFinished {
            stream_id,
            call_id,
            tool_name,
        } => handler.handle_tool_call_event(
            *session_id,
            *stream_id,
            ToolCallEvent::Finished {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
            },
        ),
        SessionEvent::ToolCallFailed {
            stream_id,
            call_id,
            tool_name,
            error,
        } => handler.handle_tool_call_event(
            *session_id,
            *stream_id,
            ToolCallEvent::Failed {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                error: error.clone(),
            },
        ),
        SessionEvent::ToolResultChunk {
            stream_id,
            call_id,
            data,
        } => {
            handler.handle_tool_result_chunk(
                *session_id,
                *stream_id,
                call_id.clone(),
                data.clone(),
            );
        }
        SessionEvent::Done {
            stream_id,
            token_usage,
            last_prompt_tokens,
        } => handler.handle_done(*session_id, *stream_id, *token_usage, *last_prompt_tokens),
        SessionEvent::SessionStatusChanged {
            status,
            last_modified,
        } => handler.handle_session_status_changed(*session_id, status.clone(), *last_modified),
        SessionEvent::ModelSelected { model, .. } => {
            handler.handle_status_text(format!("[daemon] selected model: {model}"));
        }
        SessionEvent::SessionDeleted => {}
        SessionEvent::SessionDeleteFailed { .. } => {}
        SessionEvent::SessionFlagsChanged {
            pinned,
            archived_at,
        } => handler.handle_session_flags_changed(*session_id, *pinned, *archived_at),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal recording [`TurnEventHandler`] that captures which terminal
    /// handler each dispatched event reached, so a test can prove a cancel is
    /// routed to `handle_cancelled` and never to `handle_failed`.
    #[derive(Default)]
    struct Recorder {
        failed: Vec<(Option<u64>, u64)>,
        cancelled: Vec<(Option<u64>, u64)>,
    }

    impl TurnEventHandler for Recorder {
        fn handle_turn_appended(&mut self, _session_id: u64, _turn_id: u32, _turn: Turn) {}
        fn handle_turns_undone(&mut self, _session_id: u64, _turn_ids: &[u32]) {}
        fn handle_turns_redone(&mut self, _session_id: u64, _turns: BTreeMap<u32, Turn>) {}
        fn handle_request_stream(
            &mut self,
            _session_id: u64,
            _stream_id: u64,
            _stream: OutputStream,
            _data: Cow<'_, str>,
        ) {
        }
        fn handle_started(
            &mut self,
            _session_id: u64,
            _stream_id: u64,
            _turn_id: u32,
            _estimated_prompt_tokens: u32,
        ) {
        }
        fn handle_done(
            &mut self,
            _session_id: u64,
            _stream_id: u64,
            _token_usage: Option<TokenUsage>,
            _last_prompt_tokens: Option<u32>,
        ) {
        }
        fn handle_failed(&mut self, session_id: Option<u64>, stream_id: u64, _error: String) {
            self.failed.push((session_id, stream_id));
        }
        fn handle_cancelled(&mut self, session_id: Option<u64>, stream_id: u64) {
            self.cancelled.push((session_id, stream_id));
        }
        fn handle_tool_call_event(
            &mut self,
            _session_id: u64,
            _stream_id: u64,
            _event: ToolCallEvent,
        ) {
        }
        fn handle_tool_result_chunk(
            &mut self,
            _session_id: u64,
            _stream_id: u64,
            _call_id: String,
            _data: Vec<u8>,
        ) {
        }
        fn handle_session_state(&mut self, _state: SessionStateData) {}
        fn handle_status_text(&mut self, _text: String) {}
        fn handle_error(&mut self, _error: String) {}
        fn handle_session_attached(&mut self, _session_id: u64) {}
        fn handle_session_created(
            &mut self,
            _session_id: u64,
            _title: Option<String>,
            _working_dir: Option<String>,
            _account_name: Option<String>,
            _selected_model: Option<String>,
            _reasoning_effort: Option<String>,
        ) {
        }
        fn handle_session_status_changed(
            &mut self,
            _session_id: u64,
            _status: SessionStatus,
            _last_modified: i64,
        ) {
        }
        fn handle_token_usage_update(
            &mut self,
            _session_id: u64,
            _token_usage: TokenUsage,
            _last_prompt_tokens: Option<u32>,
        ) {
        }
    }

    /// Wrap a session event in the `Session` envelope the daemon uses for
    /// session-scoped broadcasts (origin session present).
    fn session_msg(event: SessionEvent) -> DaemonMessage {
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(1),
            event,
        })
    }

    #[test]
    fn cancelled_reaches_handle_cancelled_not_handle_failed() {
        let mut rec = Recorder::default();
        dispatch_daemon_message(
            session_msg(SessionEvent::Cancelled { stream_id: 7 }),
            &mut rec,
        );
        assert_eq!(rec.cancelled, vec![(Some(1), 7)]);
        assert!(
            rec.failed.is_empty(),
            "a cancel must be routed away from handle_failed"
        );
    }

    #[test]
    fn failed_reaches_handle_failed_not_handle_cancelled() {
        let mut rec = Recorder::default();
        dispatch_daemon_message(
            session_msg(SessionEvent::Failed {
                stream_id: 9,
                error: "boom".to_string(),
            }),
            &mut rec,
        );
        assert_eq!(rec.failed, vec![(Some(1), 9)]);
        assert!(
            rec.cancelled.is_empty(),
            "a failure must be routed away from handle_cancelled"
        );
    }
}
