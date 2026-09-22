//! The Chat page's command dispatcher: the single path that turns a parsed
//! [`Command`] — whether typed as `/name …` on the prompt or synthesised from a
//! keyboard shortcut — into its side effect.
//!
//! Split out of `connection/chat.rs` so that module stays focused on
//! terminal-event routing.  Everything here runs on the UI thread and is driven
//! either by the Enter/command-mode handlers in `chat.rs` or by the Chat-page
//! shortcut dispatch (which resolves a key to a command name via
//! `crate::state::binding_for` and calls [`run_named`]).
//!
//! The local-UI command bodies (open the model selector / session manager /
//! accounts page, cycle or list reasoning effort) live alongside the daemon
//! sends so a shortcut and its typed spelling always share one implementation.

use crate::state::{App, Page};
use crate::{Command, parse_input_line};
use choreo_client_core::{
    ClientError, broken_pipe, build_add_credential_message, command_echo, resolve_private_key,
};
use choreo_proto::ClientMessage;

/// Send a `ContinueGeneration` for the attached session — the shared body of
/// the two new-turn triggers that map to it, Alt+Enter and `/continue`.
///
/// Runs the shared client-side submit guard ([`App::new_turn_rejection`])
/// first: a not-idle session or a locked keystore is refused locally with the
/// guard's status message instead of a round-trip the daemon would only answer
/// with a transient failure.  On success it allocates the request id, records
/// the in-flight request on the active display (when one exists), ships the
/// message, and scrolls to the newest turn.  `echo` shows the `> continue`
/// shell echo — set for `/continue` (a typed command) but not for Alt+Enter
/// (a bare keypress is its own feedback).
///
/// With no session attached it reports "no session attached" and sends
/// nothing.  Returns `Ok(())` whether the turn was sent or refused; only a
/// broken client channel is an error, so callers can `?` this directly.
fn send_continue_generation(
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    echo: bool,
) -> Result<(), ClientError> {
    if app.attached_session_id.is_none() {
        tracing::debug!("continue ignored — no session attached");
        app.status = Some("no session attached".to_string());
        return Ok(());
    }
    // Shared guard with the plain-prompt path: a `ContinueGeneration` becomes a
    // fresh `RunInput` turn, so it cannot begin while the session is busy or the
    // keystore is locked.  See `App::new_turn_rejection`.
    if let Some(reason) = app.new_turn_rejection() {
        tracing::debug!(reason, "[choreo-tui] continue rejected client-side");
        app.status = Some(reason.to_string());
        app.error = None;
        return Ok(());
    }
    if echo && let Some(text) = command_echo(&Command::Continue) {
        app.status = Some(text);
    }
    let request_id = app.next_request_id;
    app.next_request_id = app.next_request_id.wrapping_add(1);
    // The guard above already checked `attached_session_id`, but the display
    // entry may not exist yet — track the in-flight request only when there is
    // a display to hold it, never panic on a missing one.
    if let Some(display) = app.active_display() {
        display.active.insert(request_id);
    }
    client_tx
        .send(ClientMessage::ContinueGeneration { request_id })
        .map_err(broken_pipe)?;
    app.scroll_to(0);
    Ok(())
}

/// Open the model selector and request a fresh model list — the shared body of
/// the `Alt+M` shortcut and the bare `/model` command.
///
/// Clears any armed text selection first: the selector is a modal overlay that
/// routes mouse events away from the history-pane selection arms, so a mid-drag
/// open must not leave a dangling gesture that swallows the first click after
/// the selector closes.
fn open_model_selector(
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    tracing::debug!("[choreo-tui] opening model selector");
    app.text_selection = None;
    app.model_selector.open();
    client_tx
        .send(ClientMessage::ListModels)
        .map_err(broken_pipe)?;
    Ok(())
}

/// Open the session manager, highlighting the session the user was just viewing
/// — the shared body of the `Alt+S` shortcut and the bare `/session` command.
fn open_session_manager(
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    tracing::debug!("[choreo-tui] navigating to session manager");
    // Record the viewed session so the ListSessions reply lands the highlight
    // on it (the selection survives the round-trip via `pending_select`).
    if let Some(session_id) = app.attached_session_id {
        app.session_mgr.select_session(session_id);
    }
    app.set_page(Page::SessionManager);
    client_tx
        .send(ClientMessage::ListSessions)
        .map_err(broken_pipe)?;
    client_tx
        .send(ClientMessage::SubscribeSessionsSummary)
        .map_err(broken_pipe)?;
    Ok(())
}

/// Open the AI-provider accounts page — the shared body of the `Alt+A`
/// shortcut and the bare `/account` command.
fn open_accounts_page(
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    tracing::debug!("[choreo-tui] navigating to AI provider accounts");
    app.set_page(Page::AIProviders);
    client_tx
        .send(ClientMessage::ListAccounts)
        .map_err(broken_pipe)?;
    Ok(())
}

/// Cycle the attached session's reasoning effort to the next level — the shared
/// body of the `Alt+R` shortcut and the bare `/reasoning` command.
///
/// Writes the status line directly (there is no shell echo): "no session
/// attached" when there is no display, the daemon's own `SetReasoningEffort`
/// bounds-checked against the cached capability, and a distinct message for
/// "not supported" (an empty level set) versus "not yet known" (`None` with a
/// model selected) versus "no model selected".
fn cycle_reasoning(
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let Some(display) = app.active_display_ref() else {
        // No session attached — there is no display whose capability could be
        // consulted.  Mirror the wording the other session-bound shortcuts use
        // (Alt+Enter, Esc, /stop).
        app.status = Some("no session attached".to_string());
        tracing::warn!(
            session_id = ?app.attached_session_id,
            "reasoning cycle ignored — no active display (no session attached)",
        );
        return Ok(());
    };
    // Snapshot the display fields before mutating `app` below so the immutable
    // borrow of the display ends before the status writes.
    let capability = display.reasoning_capability.clone();
    let current_effort = display.reasoning_effort.clone();
    let selected_model = display.selected_model.clone();
    match capability.as_ref() {
        // A present-but-empty capability is the daemon's explicit "reasoning
        // not supported" signal.  This must stay distinct from `None`, which
        // only means "capability not yet known".
        Some(c) if c.available_effort_levels.is_empty() => {
            app.status = Some("model does not support reasoning".to_string());
        }
        Some(c) => {
            let current = current_effort.unwrap_or_else(|| "off".to_string());
            if let Some(next) = c.cycle_from(&current) {
                if let Some(d) = app.active_display() {
                    d.reasoning_effort = Some(next.clone());
                }
                app.status = Some(format!("reasoning: {next}"));
                tracing::info!(
                    session_id = ?app.attached_session_id,
                    current = %current,
                    next = %next,
                    "cycling reasoning effort",
                );
                client_tx
                    .send(ClientMessage::SetReasoningEffort { effort: next })
                    .map_err(broken_pipe)?;
            } else {
                // `cycle_from` only returns None for an empty level set, which
                // the guard above already handled — this is a defensive
                // fallback.
                app.status = Some("model does not support reasoning".to_string());
            }
        }
        // A model is selected but the daemon has not reported its effort
        // levels yet.  `None` here must NOT be conflated with "reasoning
        // unsupported".
        None if selected_model.is_some() => {
            app.status = Some("reasoning capability not yet available".to_string());
            tracing::info!(
                session_id = ?app.attached_session_id,
                model = ?selected_model,
                "reasoning cycle pressed before reasoning capability was reported",
            );
        }
        None => {
            app.status = Some(format!(
                "no model selected — pick one with {}",
                App::model_selector_label()
            ));
            tracing::warn!(
                session_id = ?app.attached_session_id,
                "reasoning cycle pressed with no model selected",
            );
        }
    }
    Ok(())
}

/// List the attached session's available reasoning levels — the `/reasoning
/// list` command.  Reuses [`cycle_reasoning`]'s not-supported /
/// not-yet-available / no-model wording so the two share one vocabulary.
fn reasoning_list(app: &mut App) {
    let (capability, selected_model) = if let Some(display) = app.active_display_ref() {
        (
            display.reasoning_capability.clone(),
            display.selected_model.clone(),
        )
    } else {
        tracing::debug!(
            session_id = ?app.attached_session_id,
            "reasoning list ignored — no active display (no session attached)",
        );
        app.status = Some("no session attached".to_string());
        return;
    };
    match capability.as_ref() {
        Some(c) if c.available_effort_levels.is_empty() => {
            app.status = Some("model does not support reasoning".to_string());
        }
        Some(c) => {
            let levels = c.available_effort_levels.join(", ");
            tracing::debug!(levels = %levels, "listing reasoning effort levels");
            app.status = Some(format!("reasoning levels: {levels}"));
        }
        None if selected_model.is_some() => {
            app.status = Some("reasoning capability not yet available".to_string());
        }
        None => {
            app.status = Some(format!(
                "no model selected — pick one with {}",
                App::model_selector_label()
            ));
        }
    }
}

/// Dispatch a parsed [`Command`] into its side effect(s).
///
/// Shared by the typed `/`-command path (Enter), the command-entry palette
/// (Enter in command mode), and the keyboard-shortcut path ([`run_named`]).
/// `echo` controls whether the command's shell echo ([`command_echo`]) is
/// written to the status line: a typed command echoes (`true`), a bare
/// keypress does not (`false`) — preserving the historical "keypresses don't
/// echo" behavior.
pub(super) fn run_command(
    command: Command,
    echo: bool,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match command {
        Command::Empty => {}
        Command::InvalidCancel(value) => {
            app.status = Some(format!("invalid request id: {value}"));
        }
        Command::UnknownCommand(error) => app.status = Some(error),
        Command::Send(message) => {
            // Client-side validation: reject reasoning slugs that the attached
            // model's capability set does not include.  This provides faster
            // feedback than waiting for the daemon to reply with
            // ReasoningEffortSetFailed.
            if let ClientMessage::SetReasoningEffort { ref effort } = message
                && effort != "off"
            {
                let valid = app
                    .active_display_ref()
                    .and_then(|d| d.reasoning_capability.as_ref())
                    // No capability cached → let daemon validate.
                    .is_none_or(|c| c.available_effort_levels.iter().any(|l| l == effort));
                if !valid {
                    tracing::warn!(
                        %effort,
                        "TUI rejected reasoning slug not in capability set",
                    );
                    app.status = Some(format!("model does not support reasoning '{effort}'"));
                    return Ok(());
                }
            }
            let message = match message {
                ClientMessage::CreateSession {
                    title,
                    parent_session_id,
                    working_dir,
                    context_config,
                    account_name,
                    selected_model,
                    reasoning_effort,
                } => ClientMessage::CreateSession {
                    title,
                    parent_session_id,
                    // Inherit fields from the currently attached session when
                    // not explicitly provided by the user.
                    working_dir: working_dir
                        .or_else(|| app.active_display_ref().and_then(|d| d.working_dir.clone())),
                    context_config,
                    account_name: account_name.or_else(|| {
                        app.active_display_ref()
                            .and_then(|d| d.account_name.clone())
                    }),
                    selected_model: selected_model.or_else(|| {
                        app.active_display_ref()
                            .and_then(|d| d.selected_model.clone())
                    }),
                    reasoning_effort: reasoning_effort.or_else(|| {
                        app.active_display_ref()
                            .and_then(|d| d.reasoning_effort.clone())
                    }),
                },
                other => other,
            };
            if echo && let Some(text) = command_echo(&Command::Send(message.clone())) {
                app.status = Some(text);
            }
            if let ClientMessage::RunInput { request_id, .. } = &message {
                app.error = None;
                // The active display tracks in-flight request ids for the
                // spinner; with no session active there is nothing to track, so
                // skip instead of panicking.
                if let Some(display) = app.active_display() {
                    display.active.insert(*request_id);
                }
            }
            client_tx.send(message).map_err(broken_pipe)?;

            // Scroll the history view to the bottom so the user can see their
            // submitted message appear as the daemon processes it.
            app.scroll_to(0);
        }
        Command::Unlock { method } => {
            match resolve_private_key(&method, &app.connection_addr) {
                Ok(private_key) => {
                    // Hold the key until the daemon CONFIRMS the unlock, then
                    // record it per-daemon.
                    app.pending_unlock_key = Some(private_key.clone());
                    let _ = client_tx.send(ClientMessage::Unlock { private_key });
                }
                Err(e) => {
                    tracing::warn!("[choreo-tui] unlock failed: {e}");
                    // Surface the failure (e.g. NoUnlockKey, or a malformed
                    // base64 key from /unlock <key>) in the status bar — a
                    // warn-only log would look like the command silently did
                    // nothing.
                    app.status = Some(format!("[error] {e}"));
                    app.error = None;
                }
            }
        }
        Command::AddCredential {
            ref service,
            ref credential_type,
            ref fields,
        } => {
            match build_add_credential_message(
                &app.connection_addr,
                service.clone(),
                credential_type.clone(),
                fields.clone(),
            ) {
                Ok((msg, key)) => {
                    // Record only after the daemon CONFIRMS (CredentialAdded /
                    // Unlocked) — see `record_confirmed_unlock_key`.
                    app.pending_unlock_key = Some(key);
                    let _ = client_tx.send(msg);
                }
                Err(e) => {
                    tracing::warn!("[choreo-tui] add credential failed: {e}");
                }
            }
        }
        Command::AclAdd { ref pubkey } => {
            if echo
                && let Some(text) = command_echo(&Command::AclAdd {
                    pubkey: pubkey.clone(),
                })
            {
                app.status = Some(text);
            }
            let _ = client_tx.send(ClientMessage::AclAdd {
                pubkey: pubkey.clone(),
            });
        }
        Command::RemoveCredential { ref service } => {
            if echo
                && let Some(text) = command_echo(&Command::RemoveCredential {
                    service: service.clone(),
                })
            {
                app.status = Some(text);
            }
            let _ = client_tx.send(ClientMessage::RemoveCredential {
                service: service.clone(),
            });
        }
        Command::Undo => {
            if echo && let Some(text) = command_echo(&Command::Undo) {
                app.status = Some(text);
            }
            let _ = client_tx.send(ClientMessage::Undo);
        }
        Command::Redo => {
            if echo && let Some(text) = command_echo(&Command::Redo) {
                app.status = Some(text);
            }
            let _ = client_tx.send(ClientMessage::Redo);
        }
        Command::Continue => {
            // `/continue` (or Alt+Enter) — same guard, same
            // `ContinueGeneration`; echoes `> continue` only for the typed
            // command (`echo`).
            send_continue_generation(app, client_tx, echo)?;
        }
        Command::Stop => {
            if echo && let Some(text) = command_echo(&Command::Stop) {
                app.status = Some(text);
            }
            // Send Cancel with request_id 0 (the CANCEL_ALL sentinel) to stop
            // whatever request is currently active on the attached session and
            // all its children.
            if app.attached_session_id.is_some() {
                client_tx
                    .send(ClientMessage::Cancel { request_id: 0 })
                    .map_err(broken_pipe)?;
            } else {
                app.status = Some("no session attached".to_string());
            }
        }
        Command::RefreshModels { force } => {
            // Show immediate feedback; the daemon replies asynchronously via
            // ModelsRefreshed / ModelsRefreshFailed.
            let suffix = if force { " (forced)" } else { "" };
            app.status = Some(format!("refreshing models…{suffix}"));
            client_tx
                .send(ClientMessage::RefreshModels { force })
                .map_err(broken_pipe)?;
        }
        // Local-UI commands (the unified command model's non-daemon variants).
        Command::Quit => {
            tracing::info!("[choreo-tui] /quit requested");
            app.should_quit = true;
        }
        Command::OpenModelSelector => {
            tracing::debug!("[choreo-tui] dispatching /model (open selector)");
            open_model_selector(app, client_tx)?;
        }
        Command::OpenSessions => {
            tracing::debug!("[choreo-tui] dispatching /session (open manager)");
            open_session_manager(app, client_tx)?;
        }
        Command::OpenAccounts => {
            tracing::debug!("[choreo-tui] dispatching /account (open accounts page)");
            open_accounts_page(app, client_tx)?;
        }
        Command::ReasoningCycle => {
            tracing::debug!("[choreo-tui] dispatching /reasoning (cycle)");
            cycle_reasoning(app, client_tx)?;
        }
        Command::ReasoningList => {
            tracing::debug!("[choreo-tui] dispatching /reasoning list");
            reasoning_list(app);
        }
    }
    Ok(())
}

/// Run a command by name through the unified command path: parse `"/<name>"`
/// and [`run_command`] it.
///
/// This is the bridge from a resolved keyboard shortcut (see
/// `crate::state::binding_for`) to the same implementation the typed
/// `/name` spelling runs, so a key and its typed spelling can never drift.  The
/// shortcut is a bare keypress, so it passes `echo = false`.
pub(super) fn run_named(
    name: &str,
    echo: bool,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let command = parse_input_line(&format!("/{name}"), &mut app.next_request_id);
    run_command(command, echo, app, client_tx)
}
