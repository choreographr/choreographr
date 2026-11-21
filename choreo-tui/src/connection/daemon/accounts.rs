//! The **accounts** family of daemon messages: the provider-account list and
//! add/remove replies, the credential add/remove replies, and the ACL
//! enrollment replies.
//!
//! `Accounts`, `Account*`, `Credential*` and `Acl*` all concern the AI-provider
//! account surface (the accounts page, its credential modal, and the wizard's
//! first-run credential step), so they share one file. Most arms fall through
//! to the generic dispatch so a `/account`-style command still prints its
//! confirmation; only the list-failure arm consumes the message.
use super::super::setup_flow;
use super::Consumed;
use crate::state::App;
use choreo_proto::{AccountInfo, ClientMessage, ClientMessageType};

/// The full `Accounts` list reply.
pub(super) fn on_accounts(app: &mut App, accounts: &[AccountInfo]) -> Consumed {
    app.handle_accounts(accounts);
    // Don't return early here — fall through to dispatch_daemon_message
    // which will push the account list to the chat history so the
    // user sees the response to their `/account` command.
    Consumed::No
}

/// An `AccountListFailed` reply.
pub(super) fn on_account_list_failed(app: &mut App, error: &str) -> Consumed {
    app.set_error(format!("[daemon] failed to list accounts: {error}"));
    Consumed::Yes
}

/// An `AccountAdded` reply: refresh the list so the page reflects the new entry.
pub(super) fn on_account_added(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    name: &str,
) -> Consumed {
    app.set_status(format!("[daemon] account added: {name}"));
    app.pending.send(client_tx, ClientMessageType::ListAccounts);
    Consumed::No
}

/// An `AccountAddFailed` reply: drop the wizard's credential modal (only when it
/// still targets the failing account) and surface the error.
pub(super) fn on_account_add_failed(app: &mut App, name: &str, error: &str) -> Consumed {
    // The account never got created, so drop the credential modal the
    // wizard auto-opened right after submit — but ONLY when it is still
    // aimed at the failing account.  The daemon's failure reply can
    // arrive after the user has already dismissed that modal and
    // opened a DIFFERENT account's key modal; closing unconditionally
    // would discard their in-progress input.  The error line is still
    // surfaced unconditionally either way.
    if app.ai_providers.credential.target.as_deref() == Some(name) {
        app.ai_providers.credential.close();
    }
    app.set_error(format!("[daemon] failed to add account {name}: {error}"));
    Consumed::No
}

/// An `AccountRemoved` reply: drop the account locally and refresh the list.
pub(super) fn on_account_removed(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    name: &str,
) -> Consumed {
    app.set_status(format!("[daemon] account removed: {name}"));
    app.ai_providers.remove_account(name);
    app.pending.send(client_tx, ClientMessageType::ListAccounts);
    Consumed::No
}

/// An `AccountRemoveFailed` reply.
pub(super) fn on_account_remove_failed(app: &mut App, name: &str, error: &str) -> Consumed {
    app.set_error(format!("[daemon] failed to remove account {name}: {error}"));
    Consumed::No
}

/// A `CredentialAdded` reply: refresh the account list and start the first-run
/// staged account/model/reasoning flow.
///
/// A credential mutation does not carry the updated account list, so
/// re-request it: the accounts page renders `has_credential` per
/// account, and without a refresh it would keep showing the stale
/// pre-credential state until the user leaves and re-enters the page.
pub(super) fn on_credential_added(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    service: &str,
) -> Consumed {
    app.set_status(format!("[daemon] credential added: {service}"));
    // The add carries no unlock key, so there is nothing to record
    // per-daemon here; refresh the account list so the page reflects
    // the new credential's `has_credential` state.
    app.pending.send(client_tx, ClientMessageType::ListAccounts);
    // First-run guidance: a key the daemon just confirmed for an
    // accountless attached session starts the staged
    // account/model/reasoning choice, so the user is not left to
    // discover `/account <name>` on their own.
    setup_flow::start_after_credential(app, client_tx, service.to_string());
    Consumed::No
}

/// A `CredentialRemoved` reply.
pub(super) fn on_credential_removed(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    service: &str,
) -> Consumed {
    app.set_status(format!("[daemon] credential removed: {service}"));
    app.pending.send(client_tx, ClientMessageType::ListAccounts);
    Consumed::No
}

/// An `AclAddResult` reply: the direct outcome of this client's add.
pub(super) fn on_acl_add_result(app: &mut App, ok: bool, message: &str) -> Consumed {
    // ACL enrollment feedback: the direct reply says whether THIS add
    // worked; the broadcast tells every connected client the new total
    // (this client included, so one status line suffices per event).
    if ok {
        app.set_status(format!("[daemon] {message}"));
    } else {
        app.set_status(format!("[daemon] acl add failed: {message}"));
    }
    Consumed::No
}

/// An `AclUpdated` broadcast: the new total of authorized clients.
pub(super) fn on_acl_updated(app: &mut App, clients: u64) -> Consumed {
    app.set_status(format!(
        "[daemon] ACL updated — {clients} authorized client(s)"
    ));
    Consumed::No
}
