//! The daemon-message **router**: [`handle_daemon_message`] is a dispatch table
//! over the per-subject sibling modules, one arm per wire variant.
//!
//! The connection module receives every daemon message here and must (a) apply
//! the TUI-specific state its family cares about before (b) letting the generic
//! `choreo_client_core::dispatch_daemon_message` render the rest. The arm bodies
//! that do (a) live in the sibling family modules, each owning one coherent
//! subject:
//!
//! * [`session`] — every `DaemonMessageType::Session` envelope event plus the
//!   `Sessions` snapshot (it owns its own inner `match` over `(session_id,
//!   event)` because all its arms are one wire variant).
//! * [`models`] — `Models*` and `ModelReasoning`.
//! * [`accounts`] — `Accounts`, `Account*`, `Credential*`, `Acl*`.
//! * [`mcp`] — `Mcp*` and `CatalogUpdated`.
//! * [`keystore`] — `Unlocked`, `Bound`, `Locked`, `LockedError`,
//!   `CredentialAddFailed`, `KeystoreUnbound`, `Keystore`.
//! * [`lifecycle`] — the connection-level termination (`ShuttingDown`,
//!   `Evicted`) and the staged-flow `SetSessionChoice` commit failure.
//!
//! Each family function returns [`Consumed`] to make the arm's control flow
//! explicit: [`Consumed::Yes`] stops here (the family rendered its own state and
//! the generic dispatch would duplicate or misroute it), while [`Consumed::No`]
//! falls through to `dispatch_daemon_message`. This router preserves the exact
//! per-arm `return Ok(())`-vs-fall-through the original match relied on.
//!
//! A **newly added wire variant that no family claims** falls to the `_` arm
//! here, is treated as [`Consumed::No`], and still reaches
//! `dispatch_daemon_message` — whose exhaustive `match` in `choreo_client_core`
//! is the compile-time tripwire that forces every new variant to be handled
//! somewhere.
//!
//! The pending-reply slot is resolved **once** here and threaded into the arms
//! that need it (the `models` family's `is_list_models_reply` gates the
//! model-list arms; the `keystore` arms record or discard the pending unlock
//! key) — never resolved twice. Everything a single family needs beyond that
//! slot — a correlation predicate, the TOFU key record, the auto-bind wrapper —
//! lives in the family module that uses it, so this file stays the dispatch
//! table and nothing else.
use crate::state::App;
use choreo_client_core::{ClientError, dispatch_daemon_message};
use choreo_proto::{ClientMessage, DaemonMessage, DaemonMessageType, MessageKind};

mod accounts;
mod keystore;
mod lifecycle;
mod mcp;
mod models;
mod session;

/// Whether a family handler **consumed** a daemon message, i.e. whether the
/// router must stop and skip the generic dispatch.
///
/// The two states encode the split the original match made with `return Ok(())`
/// versus falling out of the arm: some handlers render their own output (a
/// session attach, a model list, a `/mcp` status) that the generic dispatch
/// would duplicate or push to the wrong surface, while the rest only apply
/// per-session display state and must still fall through so `/model`,
/// `/account`, `/unlock` and friends keep printing their confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Consumed {
    /// The family handled the message fully; do not run the generic dispatch.
    Yes,
    /// The family applied its state but the message must still reach
    /// `dispatch_daemon_message`.
    No,
}

// Owned-message call sites live in test files outside connection/, so the
// by-value signature is kept for those ergonomic owned-value call sites.
pub(crate) fn handle_daemon_message(
    message: DaemonMessage,
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // Resolve the reply's pending slot FIRST, before any state handling: a
    // `Some(id)` message answers one of our requests, and the resolved slot's
    // kind/context tell the arms below what the reply means (the model-list
    // popup, a keystore key to record). A broadcast (`id: None`) resolves
    // nothing. This is a side table: resolving never replaces the payload's own
    // state dispatch at the bottom.
    let mut resolved = message.id.and_then(|id| app.pending.resolve(id));
    // Dispatch per-variant handlers first, then let the generic dispatch in
    // choreo_client_core handle the rest (text notifications, stream appends,
    // image assembly, etc.). Each arm delegates to its subject family and
    // reports whether it already rendered the message.
    let consumed = match &message.inner {
        DaemonMessageType::Session { session_id, event } => {
            session::on_session_message(app, client_tx, message.id, *session_id, event)?
        }
        DaemonMessageType::Sessions { sessions } => session::on_sessions(app, client_tx, sessions)?,

        // ── Accounts / credentials / ACL ────────────────────────────────
        DaemonMessageType::Accounts { accounts } => accounts::on_accounts(app, accounts),
        DaemonMessageType::AccountListFailed { error } => {
            accounts::on_account_list_failed(app, error)
        }
        DaemonMessageType::AccountAdded { name } => {
            accounts::on_account_added(app, client_tx, name)
        }
        DaemonMessageType::AccountAddFailed { name, error } => {
            accounts::on_account_add_failed(app, name, error)
        }
        DaemonMessageType::AccountRemoved { name } => {
            accounts::on_account_removed(app, client_tx, name)
        }
        DaemonMessageType::AccountRemoveFailed { name, error } => {
            accounts::on_account_remove_failed(app, name, error)
        }
        DaemonMessageType::CredentialAdded { service } => {
            accounts::on_credential_added(app, client_tx, service)
        }
        DaemonMessageType::CredentialRemoved { service } => {
            accounts::on_credential_removed(app, client_tx, service)
        }
        DaemonMessageType::AclAddResult { ok, message } => {
            accounts::on_acl_add_result(app, *ok, message)
        }
        DaemonMessageType::AclUpdated { clients } => accounts::on_acl_updated(app, *clients),

        // ── Models + reasoning + /refresh-models ────────────────────────
        DaemonMessageType::Models {
            models,
            selected_model,
            suggested_model,
            provisional,
        } => models::on_models(
            app,
            client_tx,
            resolved.as_ref(),
            models,
            selected_model.as_ref(),
            suggested_model.as_ref(),
            *provisional,
        ),
        DaemonMessageType::ModelReasoning {
            capability,
            suggested_effort,
        } => models::on_model_reasoning(app, client_tx, capability, suggested_effort.as_ref()),
        DaemonMessageType::ModelsFailed { error } => {
            models::on_models_failed(app, client_tx, resolved.as_ref(), error)
        }
        DaemonMessageType::ModelsRefreshed {
            providers,
            models,
            status,
        } => models::on_models_refreshed(app, *providers, *models, *status),
        DaemonMessageType::ModelsRefreshFailed { error } => {
            models::on_models_refresh_failed(app, error)
        }
        DaemonMessageType::ModelsUpdated { account, models } => {
            models::on_models_updated(app, client_tx, account, models)
        }
        DaemonMessageType::ModelsUnavailable { account, error } => {
            models::on_models_unavailable(app, client_tx, account, error)
        }

        // ── MCP + catalog ───────────────────────────────────────────────
        DaemonMessageType::McpStatus {
            servers,
            project_root,
            project_trusted,
            ignored_project_servers,
        } => mcp::on_mcp_status(
            app,
            client_tx,
            servers,
            project_root.as_ref(),
            *project_trusted,
            ignored_project_servers,
        ),
        DaemonMessageType::McpTrustUpdated {
            trusted, message, ..
        } => mcp::on_mcp_trust_updated(app, *trusted, message),
        DaemonMessageType::McpTrustList { roots } => mcp::on_mcp_trust_list(app, roots),
        DaemonMessageType::McpReconnectFailed { slug, error } => {
            mcp::on_mcp_reconnect_failed(app, slug, error)
        }
        DaemonMessageType::McpReloaded { summary, servers } => {
            mcp::on_mcp_reloaded(app, summary, servers)
        }
        DaemonMessageType::McpReloadFailed { error } => mcp::on_mcp_reload_failed(app, error),
        DaemonMessageType::CatalogUpdated { providers } => mcp::on_catalog_updated(app, providers),

        // ── Keystore ────────────────────────────────────────────────────
        DaemonMessageType::Unlocked => keystore::on_unlocked(app, resolved.as_ref()),
        DaemonMessageType::Bound => keystore::on_bound(app, resolved.as_ref()),
        DaemonMessageType::Locked => keystore::on_locked(app),
        DaemonMessageType::LockedError { .. } => keystore::on_locked_error(app, &mut resolved),
        DaemonMessageType::CredentialAddFailed { .. } => {
            keystore::on_credential_add_failed(app, &mut resolved)
        }
        DaemonMessageType::KeystoreUnbound { .. } => {
            keystore::on_keystore_unbound(app, client_tx, &mut resolved)
        }
        DaemonMessageType::Keystore { state } => {
            keystore::on_keystore_state(app, client_tx, *state, &mut resolved)
        }

        // ── Connection lifecycle ────────────────────────────────────────
        DaemonMessageType::Failed {
            kind: MessageKind::SetSessionChoice,
            ..
        } => lifecycle::on_set_session_choice_failed(app, client_tx),
        DaemonMessageType::ShuttingDown => lifecycle::on_shutting_down(app),
        DaemonMessageType::Evicted => lifecycle::on_evicted(app),

        _ => Consumed::No,
    };

    // Dispatch remaining variants through the generic turn-event handler.
    if matches!(consumed, Consumed::No) {
        dispatch_daemon_message(message, app);
    }
    Ok(())
}

/// Drive one daemon message through [`handle_daemon_message`].
///
/// The router's own dispatch tests and the `keystore` family's lock/auto-bind
/// tests both need this exact round-trip, so it has one definition here beside
/// the function under test. The generic dispatch's status/error handling needs
/// a sender, but none of the lock-state messages send anything, so a
/// disconnected sender works.
#[cfg(test)]
pub(super) fn dispatch(message: DaemonMessage, app: &mut App) {
    let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
    handle_daemon_message(message, app, &tx).expect("handle_daemon_message");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::test_app;

    #[test]
    fn failed_reply_surfaces_an_error_line() {
        // A terminal `Failed` reply (the request's one id-bearing reply, for a
        // request with no richer session-scoped failure shape — e.g. an empty
        // `/undo`) must reach the user as an error, not be silently dropped.
        // The correlation id rides the envelope; the payload becomes the error.
        let mut app = test_app();
        dispatch(
            DaemonMessage::reply(
                1,
                DaemonMessageType::Failed {
                    kind: MessageKind::Undo,
                    error: "nothing to undo".into(),
                },
            ),
            &mut app,
        );
        assert!(
            app.error().is_some_and(|e| e.contains("nothing to undo")),
            "a Failed reply must set the error line, got {:?}",
            app.error()
        );
    }

    #[test]
    fn accepted_reply_is_a_silent_success() {
        // `Accepted` carries no state of its own (the mutation's broadcast is
        // the state), so it must not write an error or a status line.
        let mut app = test_app();
        dispatch(
            DaemonMessage::reply(
                2,
                DaemonMessageType::Accepted {
                    kind: MessageKind::SetSessionPinned,
                },
            ),
            &mut app,
        );
        assert!(app.error().is_none(), "Accepted must not surface an error");
    }
}
