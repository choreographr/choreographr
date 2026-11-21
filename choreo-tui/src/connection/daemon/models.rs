//! The **models** family of daemon messages: model-list replies, model
//! reasoning, the refresh-models replies, and the background per-account
//! fetch pushes.
//!
//! `Models`, `ModelReasoning`, `ModelsFailed`, `ModelsRefreshed`,
//! `ModelsRefreshFailed`, `ModelsUpdated` and `ModelsUnavailable` all concern
//! the model picker / the staged flow's model step, so they share one file.
//! Several arms correlate on the resolved pending slot
//! ([`super::is_list_models_reply`]) — the reply that answers this client's own
//! `ListModels` populates the selector popup and must not fall through to the
//! generic dispatch.
use super::super::setup_flow;
use super::Consumed;
use crate::state::App;
use choreo_client_core::Pending;
use choreo_proto::{ClientMessage, ModelInfo, ReasoningCapability, RefreshStatus};

/// A `Models` reply: populate the selector popup (or the staged flow's staged
/// picker) when it answers our own `ListModels`, else fall through so `/model`
/// prints the list into the chat history.
pub(super) fn on_models(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    resolved: Option<&Pending>,
    models: &[ModelInfo],
    selected_model: Option<&String>,
    suggested_model: Option<&String>,
    provisional: bool,
) -> Consumed {
    // A `Models` reply that resolved a pending `ListModels` slot is the
    // answer to OUR model-list request, so it populates the selector
    // popup and must NOT fall through to the generic dispatch (which
    // would print the whole list into the chat history). Correlating on
    // the request id replaces the old "is the selector open?" guess:
    // the requester is known structurally.
    if super::is_list_models_reply(resolved) {
        // While the staged flow awaits its list, the reply is the
        // flow's: the account it names is a candidate the session is
        // not bound to, so the popup is opened from the flow's staged
        // list rather than from the attached session's selection.
        if setup_flow::is_awaiting_models(app) {
            tracing::debug!(
                count = models.len(),
                provisional,
                "staged setup: received the candidate account's model list"
            );
            setup_flow::on_models(
                app,
                client_tx,
                models.to_vec(),
                suggested_model.cloned(),
                provisional,
            );
            return Consumed::Yes;
        }
        // Prefer the daemon's reported selection, falling back to the
        // display's cached model when it is absent.
        let selected = selected_model.cloned().or_else(|| {
            app.active_display_ref()
                .and_then(|d| d.selected_model.clone())
        });
        tracing::debug!(
            count = models.len(),
            ?selected,
            provisional,
            "model selector: received model list"
        );
        app.model_selector
            .apply_models(models.to_vec(), selected, provisional);
        return Consumed::Yes;
    }
    // Not our reply (no pending ListModels slot): fall through to
    // dispatch_daemon_message so `/model` keeps printing the list into
    // the chat history.
    Consumed::No
}

/// A `ModelReasoning` reply: the staged flow's own query answer. Never consumes
/// the message — the generic dispatch just logs its kind.
pub(super) fn on_model_reasoning(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    capability: &ReasoningCapability,
    suggested_effort: Option<&String>,
) -> Consumed {
    // The staged flow is the only requester of this query, so a reply
    // arriving while it awaits one is its own; anything else has no
    // surface to land on. No early return: the generic dispatch below
    // has no text for this reply either, it just logs its kind.
    if setup_flow::is_awaiting_reasoning(app) {
        setup_flow::on_reasoned(
            app,
            client_tx,
            capability.clone(),
            suggested_effort.cloned(),
        );
    }
    Consumed::No
}

/// A `ModelsFailed` reply: end the staged flow (or surface the refusal in the
/// selector popup) when it answers our own `ListModels`, else fall through to
/// the generic error handling.
pub(super) fn on_models_failed(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    resolved: Option<&Pending>,
    error: &str,
) -> Consumed {
    if super::is_list_models_reply(resolved) {
        // The flow's own request was refused: end it and surface the
        // refusal where the user is looking. The session is untouched —
        // nothing has been written yet.
        if setup_flow::is_awaiting_models(app) {
            setup_flow::fail(app, client_tx, Some(error));
            return Consumed::Yes;
        }
        tracing::warn!(
            error_len = error.len(),
            "model selector: failed to list models"
        );
        // A genuine refusal ("no accounts configured", "no credential
        // stored for account …"), not a pending fetch: a list that is
        // merely still being fetched comes back as an empty provisional
        // `Models` reply and is settled by a push.
        app.model_selector.apply_error(error.to_string());
        return Consumed::Yes;
    }
    // Not our reply: fall through to the generic error handling.
    Consumed::No
}

/// A `ModelsRefreshed` reply to `/refresh-models`: render a one-line status.
pub(super) fn on_models_refreshed(
    app: &mut App,
    providers: usize,
    models: usize,
    status: RefreshStatus,
) -> Consumed {
    let message = match status {
        RefreshStatus::UpToDate => {
            format!("models up to date ({providers} providers, {models} models)")
        }
        RefreshStatus::Updated => {
            format!("models updated ({providers} providers, {models} models)")
        }
        RefreshStatus::Forced => {
            format!("models refreshed (forced) — {providers} providers, {models} models")
        }
    };
    tracing::info!(%message, "refresh-models reply");
    app.set_status(message);
    Consumed::Yes
}

/// A `ModelsRefreshFailed` reply to `/refresh-models`.
pub(super) fn on_models_refresh_failed(app: &mut App, error: &str) -> Consumed {
    tracing::warn!(error_len = error.len(), "refresh-models failed");
    app.set_error(format!("[daemon] refresh-models failed: {error}"));
    Consumed::Yes
}

/// A `ModelsUpdated` background push: hand the freshly fetched list to whichever
/// surface is showing that account's list.
pub(super) fn on_models_updated(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    account: &str,
    models: &[ModelInfo],
) -> Consumed {
    // A background model-list fetch finished. Whichever surface is
    // showing that account's list swaps it in place — the staged flow's
    // picker (and the list the flow holds, so a model the live fetch
    // added is choosable at once) or the standalone `Alt+M` picker. This
    // replaces the instant catalog seed (or the empty "fetching…" waiting
    // row) with the live list, without the user reopening anything.
    // Another account's list is none of either surface's business.
    setup_flow::on_models_updated(app, client_tx, account, models.to_vec());
    // Never falls through to the generic dispatch (nothing to render).
    Consumed::Yes
}

/// A `ModelsUnavailable` background push: end the staged flow (or surface the
/// provider's error inline in the open picker) so a waiting row never hangs.
pub(super) fn on_models_unavailable(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    account: &str,
    error: &str,
) -> Consumed {
    // The fetch that an empty provisional `Models` reply announced can
    // fail. Announcing it here is what ends the waiting row: the picker
    // shows the provider's error inline (`apply_error`) instead of a
    // "fetching…" row that could never end.
    //
    // Inside the staged flow it ends the flow instead: the candidate
    // account's list never arrived, so there is nothing to choose from
    // and nothing was committed — the error goes to the status line and
    // the popups close.
    if setup_flow::is_staging(app, account) {
        setup_flow::fail(app, client_tx, Some(error));
        return Consumed::Yes;
    }
    let attached = app.attached_account_slug.as_deref();
    if app.model_selector.is_open() && attached == Some(account) {
        tracing::warn!(
            error_len = error.len(),
            "model selector: background fetch failed"
        );
        app.model_selector.apply_error(error.to_string());
    }
    // Never falls through to the generic dispatch (nothing to render).
    Consumed::Yes
}
