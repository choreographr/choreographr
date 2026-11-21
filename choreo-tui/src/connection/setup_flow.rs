//! The I/O driver for the staged account/model/reasoning flow: it feeds
//! [`SetupEvent`]s to the pure machine in `state/setup_flow.rs`, installs the
//! phase it returns, and performs the [`SetupStep`]s it asks for (send the
//! query, open the picker or the dialog, commit, close).
//!
//! Two entry points start a flow, and both mean the same thing — "this session
//! should be bound to this account, and the user has to pick the rest":
//!
//! - [`start`], from the AI-provider accounts page (Enter or a click on a row).
//! - [`start_after_credential`], when the daemon confirms a credential for an
//!   account while the attached session has no account yet.
//!
//! Everything a flow needs afterwards arrives here as well: the `Models` /
//! `ModelReasoning` replies, the live `ModelsUpdated` / `ModelsUnavailable`
//! pushes, the account-set confirmation of the commit, and the keys pressed in
//! the picker and the dialog. Nothing polls and nothing is timed — a flow is
//! always either waiting for a daemon message or for a key.

use crate::state::{App, FlowEnd, Page, SetupEvent, SetupFlow, SetupStep, Transition};
use choreo_proto::{ClientMessage, ClientMessageType, ModelInfo, ReasoningCapability};
use crossterm::event::{Event, KeyCode, KeyEventKind};

/// Perform a transition: install the phase it leaves behind (if it leaves one)
/// and run its steps. Returns how the flow ended, when it did, so a caller that
/// owns an error message can surface it.
fn apply(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    transition: Transition,
) -> Option<FlowEnd> {
    let (next, steps, end) = match transition {
        Transition::Continue { next, steps } => (Some(next), steps, None),
        Transition::End { end, steps } => (None, steps, Some(end)),
    };
    // Install the phase BEFORE the steps run: the picker is opened from the
    // list the phase holds, so the two can never describe different lists.
    app.setup_flow = next;
    for step in steps {
        match step {
            SetupStep::RequestModels { account } => {
                tracing::debug!(
                    %account,
                    "staged setup: asking for the candidate account's models"
                );
                app.pending.send(
                    client_tx,
                    ClientMessageType::ListModels {
                        account: Some(account),
                    },
                );
            }
            SetupStep::ShowModelPicker {
                models,
                suggested,
                provisional,
            } => {
                tracing::debug!(
                    count = models.len(),
                    provisional,
                    "staged setup: opening the model picker"
                );
                // Opening a popup swaps the whole screen under the cursor, so a
                // drag in progress is abandoned exactly as a mid-drag open of
                // the plain selector does (see `open_model_selector`).
                app.text_selection = None;
                app.model_selector.open();
                // `None` for the active model: nothing is selected yet — the
                // account is not even bound — so no row is marked `●`. The
                // suggestion is the highlight, which is what Enter will take.
                app.model_selector.apply_models(models, None, provisional);
                if let Some(id) = suggested {
                    app.model_selector.highlight(&id);
                }
            }
            SetupStep::RefreshModelPicker { models } => {
                tracing::debug!(
                    count = models.len(),
                    "staged setup: model picker refreshed by the live list"
                );
                app.model_selector.refresh_models(models);
            }
            SetupStep::RequestReasoning { account, model } => {
                tracing::debug!(%account, %model, "staged setup: asking for its reasoning levels");
                app.pending.send(
                    client_tx,
                    ClientMessageType::ModelReasoning { account, model },
                );
            }
            SetupStep::ShowReasoningDialog { levels, suggested } => {
                tracing::debug!(
                    count = levels.len(),
                    "staged setup: opening the reasoning dialog"
                );
                app.reasoning_dialog.open(levels, suggested);
            }
            SetupStep::Commit {
                account,
                model,
                effort,
            } => {
                // The one write of the whole flow: account, model and effort
                // land together or not at all.
                tracing::info!(
                    %account,
                    %model,
                    %effort,
                    "staged setup: committing the account, model and effort"
                );
                app.pending.send(
                    client_tx,
                    ClientMessageType::SetSessionChoice {
                        account,
                        model,
                        effort,
                    },
                );
            }
            SetupStep::ClosePopups => {
                app.model_selector.close();
                app.reasoning_dialog.close();
            }
            SetupStep::ReturnToAccounts => {
                // The flow was entered from the accounts page, so an abandoned
                // one returns the user there — with a fresh list, so the page
                // reflects any account the flow touched.
                app.set_page(Page::AIProviders);
                app.pending.send(client_tx, ClientMessageType::ListAccounts);
            }
        }
    }
    end
}

/// Feed one event to the flow in flight, if there is one.
fn feed(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    event: SetupEvent,
) -> Option<FlowEnd> {
    let flow = app.setup_flow.take()?;
    apply(app, client_tx, flow.step(event))
}

/// Start the staged flow for `account`.
///
/// Nothing is written to the session: the flow only asks the daemon what the
/// candidate account offers, and the single `SetSessionChoice` at its end binds
/// the account, the model and the effort together. An account the attached
/// session already uses is left alone — re-selecting it is what `Alt+M` and
/// `/account` are for, and it would throw away the model in use.
///
/// These two guards are the only way into a flow, so both live here: the page
/// switch and the staging below happen only when a choice actually starts.
pub(super) fn start(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    account: String,
) {
    if app.attached_account_slug.as_deref() == Some(account.as_str()) {
        tracing::debug!(%account, "staged setup: the session already uses this account");
        return;
    }
    // One flow at a time. A second account choice has nowhere to land: the
    // superseded account's `Models` reply carries no account name, so it would
    // be matched by pending-request id alone and paint ITS models under the new
    // account's name. Refuse the second choice and leave the first in flight.
    if app.setup_flow.is_some() {
        tracing::debug!("staged setup: a choice is already in flight");
        app.set_status("finish choosing the account's model and reasoning effort first");
        return;
    }
    tracing::info!(%account, "staged setup: staging an account choice");
    // The key was saved from the accounts page; the choice that follows is the
    // chat's setup, so show the chat the popups overlay.
    app.set_page(Page::Chat);
    apply(app, client_tx, SetupFlow::begin(account));
}

/// Start the flow for an account whose credential the daemon just confirmed,
/// when the attached session has no account at all — so a first-run user is not
/// left to discover `/account` on their own.
///
/// A session that already has an account keeps it: adding a second key must not
/// silently switch what an in-use session talks to. The flow itself is then the
/// same one every other entry point stages.
pub(super) fn start_after_credential(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    account: String,
) {
    if app.attached_session_id.is_none() || app.attached_account_slug.is_some() {
        return;
    }
    tracing::info!(%account, "staged setup: credential confirmed for an accountless session");
    start(app, client_tx, account);
}

/// The `Models` reply answering the flow's own `ListModels`.
pub(super) fn on_models(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    models: Vec<ModelInfo>,
    suggested: Option<String>,
    provisional: bool,
) {
    feed(
        app,
        client_tx,
        SetupEvent::Models {
            models,
            suggested,
            provisional,
        },
    );
}

/// The `ModelReasoning` reply answering the flow's own query.
pub(super) fn on_reasoned(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    capability: ReasoningCapability,
    suggested: Option<String>,
) {
    feed(
        app,
        client_tx,
        SetupEvent::Reasoned {
            capability,
            suggested,
        },
    );
}

/// Whether the flow in flight is staging `account` (so a message keyed by an
/// account name belongs to it).
pub(super) fn is_staging(app: &App, account: &str) -> bool {
    app.setup_flow
        .as_ref()
        .is_some_and(|flow| flow.account() == account)
}

/// Whether the flow is waiting for the answer to a `ListModels` — the state in
/// which a model-list reply belongs to the flow rather than to the standalone
/// picker.
pub(super) fn is_awaiting_models(app: &App) -> bool {
    matches!(app.setup_flow, Some(SetupFlow::LoadingModels { .. }))
}

/// Whether the flow is waiting for the answer to a `ModelReasoning` query.
pub(super) fn is_awaiting_reasoning(app: &App) -> bool {
    matches!(app.setup_flow, Some(SetupFlow::LoadingReasoning { .. }))
}

/// Whether the flow owns the model picker (it is open to choose the staged
/// account's model, so Enter and a row click stage a model instead of
/// selecting one).
pub(super) fn owns_model_picker(app: &App) -> bool {
    matches!(app.setup_flow, Some(SetupFlow::ChoosingModel { .. }))
}

/// A live model list for `account` landed. The flow's staged list is replaced
/// (so a model the live list added is choosable at once) or — outside a flow —
/// the standalone picker refreshes in place, exactly as before.
pub(super) fn on_models_updated(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    account: &str,
    models: Vec<ModelInfo>,
) {
    if is_staging(app, account) {
        feed(
            app,
            client_tx,
            SetupEvent::ModelsRefreshed {
                account: account.to_string(),
                models,
            },
        );
        return;
    }
    if app.model_selector.is_open() && app.attached_account_slug.as_deref() == Some(account) {
        tracing::debug!(
            count = models.len(),
            "model selector: list refreshed by background fetch"
        );
        app.model_selector.refresh_models(models);
    }
}

/// The user chose the highlighted model in the flow's picker. The picker closes
/// as the choice is taken — the same dismissal a plain `Alt+M` selection does —
/// and the flow moves on to the model's reasoning levels.
pub(super) fn model_chosen(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    model: String,
) {
    app.model_selector.close();
    feed(app, client_tx, SetupEvent::ChoseModel(model));
}

/// The user chose the highlighted level in the reasoning dialog, which the
/// state has already closed.
pub(super) fn effort_chosen(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    effort: String,
) {
    feed(app, client_tx, SetupEvent::ChoseEffort(effort));
}

/// Handle keys while the flow's reasoning dialog is open: Up/Down walk the
/// ladder, Enter commits the highlighted level, Esc abandons the whole flow
/// (nothing is sent, so nothing is left half-done).
pub(super) fn handle_dialog_event(
    event: &Event,
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
) {
    let Event::Key(key) = event else {
        return;
    };
    if key.kind != KeyEventKind::Press {
        return;
    }
    match key.code {
        KeyCode::Esc => cancel(app, client_tx),
        KeyCode::Enter => {
            if let Some(effort) = app.reasoning_dialog.take_selection() {
                effort_chosen(app, client_tx, effort);
            }
        }
        KeyCode::Up => app.reasoning_dialog.move_up(),
        KeyCode::Down => app.reasoning_dialog.move_down(),
        _ => {}
    }
}

/// Abandon the flow (Esc, or a popup dismissed): close what it opened, send
/// nothing, and put the user back on the accounts page they started from.
pub(super) fn cancel(app: &mut App, client_tx: &crossbeam_channel::Sender<ClientMessage>) {
    tracing::debug!("staged setup: cancelled");
    feed(app, client_tx, SetupEvent::Cancelled);
}

/// End the flow in failure. The session is untouched: no step before the
/// terminal `SetSessionChoice` writes anything, and a rejected
/// `SetSessionChoice` leaves it as it was.
///
/// `error` is the daemon's text — `Some` when this arm is the only thing that
/// will report the failure, `None` when the caller falls through to a generic
/// dispatch that prints it (so the two cannot both write the status line, the
/// later write winning). Returns how the flow ended, so a caller can tell a
/// failure that ended it from one that arrived too late to.
pub(super) fn fail(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    error: Option<&str>,
) -> Option<FlowEnd> {
    let error_len = error.map_or(0, str::len);
    tracing::warn!(error_len, "staged setup: abandoned after a failure");
    let end = feed(app, client_tx, SetupEvent::Failed);
    if let Some(error) = error
        && end == Some(FlowEnd::Failed)
    {
        app.set_error(format!("[daemon] {error}"));
    }
    end
}

/// The attached session's account was set to `account`: if that is the flow's
/// own commit landing, the flow is complete and its popups go.
pub(super) fn on_account_set(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    account: &str,
) {
    if feed(
        app,
        client_tx,
        SetupEvent::Committed {
            account: account.to_string(),
        },
    ) == Some(FlowEnd::Completed)
    {
        tracing::info!(%account, "staged setup: the session's choice was committed");
    }
}

/// Drop the flow without navigating or reporting: its context vanished (the
/// connection closed, the attached session changed, the client restarted), so a
/// flow that was never committed simply goes away.
pub(super) fn abandon(app: &mut App) {
    if app.setup_flow.is_none() && !app.reasoning_dialog.is_open() {
        return;
    }
    tracing::debug!("staged setup: abandoned (its session or connection went away)");
    app.clear_setup_flow();
}
