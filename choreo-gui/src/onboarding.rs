//! First-run setup for the GUI: the state behind the setup popup that opens
//! over an empty session, and the daemon-message handling that keeps it
//! current.
//!
//! The popup walks the same three steps as the TUI's welcome checklist (the
//! rules themselves live in `choreo_client_core::onboarding`): give an
//! account an API key, use that account in the attached session, choose a
//! model. The GUI has no accounts page or model picker to point at, so each
//! step is done right in the popup — a form that creates the account and
//! stores its key in one submit, a button per keyed account, and a filterable
//! model list.
//!
//! A new account's key is held back until the daemon confirms the account
//! (`AccountAdded`), so a rejected account never leaves a stored key behind
//! under a name nothing uses.
//!
//! Saving a key while the session has no account arms the shared
//! [`SetupFollow`] chain: the daemon's `CredentialAdded` selects the account
//! for the session, and its `SessionAccountSet` fetches the model list, so a
//! new user goes from pasting a key to picking a model without another click.
//!
//! Rendering lives in `components.rs` (`SetupDialog`); everything here is
//! plain state plus sends through the [`DaemonHandle`], so it is unit-tested
//! without a daemon or a renderer.

use crate::client::DaemonHandle;
use crate::state::AppState;
use choreo_client_core::{
    SetupFollow, SetupProgress, build_add_credential_message, is_valid_account_name,
    suggest_account_name,
};
use choreo_proto::{
    AccountInfo, ClientMessageType, DaemonMessage, DaemonMessageType, SessionEvent,
};
use std::collections::{BTreeSet, HashSet};
use std::fmt;
use tracing::{debug, info};
use zeroize::{Zeroize, Zeroizing};

/// Most models the popup lists at once. Some providers serve hundreds; past
/// this the user narrows the list with the filter instead of scrolling.
pub(crate) const MODEL_LIST_CAP: usize = 40;

/// Most quiet request ids remembered at once. A request whose reply never
/// arrives (a dropped connection, a daemon error with no id) would otherwise
/// keep its id for the rest of the run; the popup has at most a few requests
/// in flight, so evicting the oldest past this bound never drops a live one.
const QUIET_REQUEST_CAP: usize = 16;

/// Where a checklist step stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StepMark {
    /// Finished.
    Done,
    /// The first unfinished step: the one to do now, shown with its controls.
    Next,
    /// Waiting on an earlier step.
    Pending,
}

/// The three checklist rows for `progress`, in order. Only the first
/// unfinished step is [`StepMark::Next`]: the steps depend on each other in
/// order, so that is the one the user can act on.
pub(crate) fn checklist(progress: SetupProgress) -> [(&'static str, StepMark); 3] {
    let done = [
        progress.has_keyed_account,
        progress.session_has_account,
        progress.session_has_model,
    ];
    let next = done.iter().position(|d| !d);
    let mark = |i: usize| match (done.get(i).copied().unwrap_or_default(), next) {
        (true, _) => StepMark::Done,
        (false, Some(n)) if n == i => StepMark::Next,
        (false, _) => StepMark::Pending,
    };
    [
        ("Add an AI provider account and its API key", mark(0)),
        ("Use that account in this session", mark(1)),
        ("Choose a model", mark(2)),
    ]
}

/// The setup popup's state: what the daemon has told us about accounts and
/// the attached session, plus the popup's own form fields.
#[derive(Debug, Clone, Default)]
pub(crate) struct SetupState {
    /// Every configured account, from the latest `Accounts` reply.
    pub(crate) accounts: Vec<AccountInfo>,
    /// The attached session's account, when it has one.
    pub(crate) session_account: Option<String>,
    /// The attached session's model, when it has one.
    pub(crate) session_model: Option<String>,
    /// Models available through the session's account, from the latest
    /// popup-requested `Models` reply.
    pub(crate) models: Vec<String>,
    /// The session `session_account`/`session_model` describe. A popup that
    /// opens on its own waits until this is the attached session, so it never
    /// judges one session's setup by another's account and model.
    pub(crate) seeded_session: Option<u64>,
    /// The armed first-run follow-through, if any (see the module docs).
    pub(crate) follow: Option<SetupFollow>,
    /// A new account's key, held until the daemon confirms the account.
    pub(crate) pending_key: Option<PendingKey>,
    /// Ids of the popup's own `ListAccounts`/`ListModels`/`ListSessions`
    /// requests. Their replies only feed the popup; the generic dispatch would
    /// otherwise print every account, model and session into the history.
    /// Ordered so the oldest can be evicted (see [`QUIET_REQUEST_CAP`]).
    pub(crate) quiet_requests: BTreeSet<u64>,
    /// The step-1 form.
    pub(crate) form: SetupForm,
    /// The user opened the setup popup from the toolbar: it stays up, even
    /// once setup is complete, until they close it.
    pub(crate) opened: bool,
    /// The user closed the popup: it no longer opens on its own for the rest
    /// of this run (the toolbar button still opens it).
    pub(crate) dismissed: bool,
}

/// The step-1 form: one submit creates the account (when the name is new) and
/// stores its API key. Naming an existing key-less account just adds the key,
/// so the same form also finishes an account created elsewhere.
#[derive(Debug, Clone, Default)]
pub(crate) struct SetupForm {
    /// The provider slug the new account targets.
    pub(crate) provider: String,
    /// The account name.
    pub(crate) account_name: String,
    /// The API key being typed. Secret: zeroized as soon as it is sent.
    pub(crate) api_key: String,
    /// The step-3 model filter.
    pub(crate) model_filter: String,
    /// The last validation or daemon error for the form, shown inline.
    pub(crate) error: Option<String>,
}

/// An API key waiting for its account to be created. The key is wiped when
/// this is dropped, whether it was sent or the account was rejected.
#[derive(Clone)]
pub(crate) struct PendingKey {
    /// The account the key belongs to, as named in the `AddAccount`.
    pub(crate) account: String,
    key: Zeroizing<String>,
}

// Hand-written so a `Debug` of the setup state can never print the key.
impl fmt::Debug for PendingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingKey")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

impl SetupState {
    /// The checklist state for the attached session.
    pub(crate) fn progress(&self) -> SetupProgress {
        SetupProgress::derive(
            &self.accounts,
            self.session_account.as_deref(),
            self.session_model.is_some(),
        )
    }

    /// Whether the setup popup is showing. It opens on its own over an empty
    /// session whose setup is unfinished — the moment a new user needs it —
    /// and so closes on its own once the last step lands. Opened from the
    /// toolbar it stays up regardless; closed by the user it stops opening on
    /// its own, so it never nags someone who chose to set up later.
    /// It also waits until the attached session's own account and model are
    /// known (`seeded_session`), so a switch of sessions never opens or shuts
    /// it on the previous session's state.
    pub(crate) fn popup_visible(&self, attached: Option<u64>, session_empty: bool) -> bool {
        let Some(attached) = attached else {
            return false;
        };
        self.opened
            || (!self.dismissed
                && self.seeded_session == Some(attached)
                && session_empty
                && !self.progress().is_complete())
    }

    /// Forget the previous session's account, model and model list, and any
    /// follow-through armed for it, after attaching to a different session.
    fn forget_session(&mut self) {
        debug!("setup: attached session changed, re-reading its state");
        self.seeded_session = None;
        self.session_account = None;
        self.session_model = None;
        self.models.clear();
        self.follow = None;
    }

    /// Open the popup from the toolbar.
    pub(crate) fn open_popup(&mut self) {
        debug!("setup: popup opened");
        self.opened = true;
    }

    /// Close the popup and keep it from reopening on its own.
    pub(crate) fn close_popup(&mut self) {
        debug!("setup: popup closed");
        self.opened = false;
        self.dismissed = true;
    }

    /// Accounts that hold a key: the ones step 2 offers.
    pub(crate) fn keyed_accounts(&self) -> impl Iterator<Item = &AccountInfo> {
        self.accounts.iter().filter(|a| a.has_credential)
    }

    /// The models matching the filter (case-insensitive substring), at most
    /// [`MODEL_LIST_CAP`] of them, plus how many more matched beyond the cap.
    pub(crate) fn visible_models(&self) -> (Vec<&str>, usize) {
        let needle = self.form.model_filter.trim().to_lowercase();
        let matching: Vec<&str> = self
            .models
            .iter()
            .map(String::as_str)
            .filter(|m| needle.is_empty() || m.to_lowercase().contains(&needle))
            .collect();
        let hidden = matching.len().saturating_sub(MODEL_LIST_CAP);
        (matching.into_iter().take(MODEL_LIST_CAP).collect(), hidden)
    }

    /// Pick `provider` for a new account and pre-fill a free account name for
    /// it, so the common case needs only the key. The name stays editable.
    pub(crate) fn pick_provider(&mut self, provider: &str) {
        debug!(provider, "setup: provider picked");
        let existing: Vec<&str> = self.accounts.iter().map(|a| a.name.as_str()).collect();
        self.form.account_name = suggest_account_name(provider, &existing);
        self.form.provider = provider.to_string();
        self.form.error = None;
    }

    /// Pre-fill an empty form from a key-less account, preferring the one
    /// the session already uses: that account only needs its key, and naming
    /// it in the form turns the submit into a plain key add.
    fn prefill_from_keyless_account(&mut self) {
        if !self.form.account_name.is_empty() || !self.form.provider.is_empty() {
            // Never overwrite what the user is typing.
            return;
        }
        let session = self.session_account.as_deref();
        let keyless = self
            .accounts
            .iter()
            .filter(|a| !a.has_credential)
            .find(|a| session.is_none_or(|s| s == a.name));
        if let Some(account) = keyless {
            self.form.account_name = account.name.clone();
            self.form.provider = account.provider.clone();
        }
    }
}

/// Send a request whose reply only feeds the popup (see
/// [`SetupState::quiet_requests`]).
fn send_quiet(state: &mut AppState, handle: &DaemonHandle, request: ClientMessageType) {
    let id = handle.send(request);
    let quiet = &mut state.setup.quiet_requests;
    quiet.insert(id);
    // Ids are allocated in increasing order, so the first is the oldest.
    while quiet.len() > QUIET_REQUEST_CAP {
        quiet.pop_first();
    }
}

/// Ask for the account list on behalf of the popup.
pub(crate) fn request_accounts(state: &mut AppState, handle: &DaemonHandle) {
    send_quiet(state, handle, ClientMessageType::ListAccounts);
}

/// Ask for the session account's model list on behalf of the popup.
pub(crate) fn request_models(state: &mut AppState, handle: &DaemonHandle) {
    send_quiet(state, handle, ClientMessageType::ListModels);
}

/// Submit the step-1 form: create the account if the name is new, store the
/// key, and arm the follow-through. Validation failures land in the form's
/// error and send nothing, so a half-filled form never creates a key-less
/// account. A new account's key waits in [`SetupState::pending_key`] until
/// the daemon confirms the account (see [`track_setup_message`]).
pub(crate) fn submit_setup_form(state: &mut AppState, daemon_tx: Option<&DaemonHandle>) {
    // Take the key out of the form up front: from here on it lives only in
    // this local, which is zeroized (or consumed by the builder) on every path.
    let mut raw_key = std::mem::take(&mut state.setup.form.api_key);
    let key = raw_key.trim().to_string();
    raw_key.zeroize();
    let name = state.setup.form.account_name.trim().to_string();
    let provider = state.setup.form.provider.trim().to_string();
    let exists = state.setup.accounts.iter().any(|a| a.name == name);
    // A resubmit while this account's creation is still in flight only
    // replaces the waiting key: a second `AddAccount` would be rejected as a
    // duplicate, and that rejection would discard the key.
    let awaiting = state
        .setup
        .pending_key
        .as_ref()
        .is_some_and(|p| p.account == name);

    let problem = if daemon_tx.is_none() {
        Some("Not connected to the daemon.")
    } else if !exists && provider.is_empty() {
        Some("Choose a provider for the new account.")
    } else if !is_valid_account_name(&name) {
        Some("Account names use lowercase letters, digits, '-' and '_'.")
    } else if key.is_empty() {
        Some("Paste the account's API key.")
    } else {
        None
    };
    let Some(handle) = daemon_tx.filter(|_| problem.is_none()) else {
        let mut key = key;
        key.zeroize();
        state.setup.form.error = problem.map(str::to_string);
        return;
    };

    state.setup.form.error = None;
    if exists {
        send_key(state, handle, &name, key);
        return;
    }
    if !awaiting {
        info!(account = %name, %provider, "setup: creating account");
        handle.send(ClientMessageType::AddAccount {
            name: name.clone(),
            provider,
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
            total_timeout_secs: None,
        });
    }
    // The daemon stores a credential under any name, so sending the key now
    // would keep it even if the account is rejected. Hold it until
    // `AccountAdded`; replacing an earlier pending key wipes that one.
    debug!(account = %name, "setup: key held until the account is confirmed");
    state.setup.pending_key = Some(PendingKey {
        account: name,
        key: Zeroizing::new(key),
    });
}

/// Store `key` under `account` and arm the follow-through. The builder
/// zeroizes the field strings once it has parsed them.
fn send_key(state: &mut AppState, handle: &DaemonHandle, account: &str, key: String) {
    match build_add_credential_message(account.to_string(), "api_key".to_string(), vec![key]) {
        Ok(message) => {
            info!(%account, "setup: storing API key");
            handle.send(message);
            state.setup.follow = SetupFollow::arm(
                account,
                state.attached_session_id.is_some(),
                state.setup.session_account.is_some(),
            );
        }
        Err(e) => state.setup.form.error = Some(e.to_string()),
    }
}

/// Step 2: use `account` in the attached session.
pub(crate) fn select_account(daemon_tx: Option<&DaemonHandle>, account: &str) {
    if let Some(handle) = daemon_tx {
        info!(%account, "setup: selecting account for the session");
        handle.send(ClientMessageType::SetSessionAccount {
            name: account.to_string(),
        });
    }
}

/// Step 3: use `model` in the attached session.
pub(crate) fn select_model(daemon_tx: Option<&DaemonHandle>, model: &str) {
    if let Some(handle) = daemon_tx {
        info!(%model, "setup: selecting model for the session");
        handle.send(ClientMessageType::SetModel {
            model: model.to_string(),
        });
    }
}

/// Fold `message` into the setup state and advance the follow-through.
///
/// Runs before the generic dispatch. Returns `true` when the message was a
/// reply to one of the popup's quiet requests and must NOT be dispatched
/// (the popup shows it instead of the history).
pub(crate) fn track_setup_message(
    state: &mut AppState,
    message: &DaemonMessage,
    daemon_tx: Option<&DaemonHandle>,
) -> bool {
    let quiet = message
        .id
        .is_some_and(|id| state.setup.quiet_requests.remove(&id));

    match &message.inner {
        DaemonMessageType::Accounts { accounts } => {
            debug!(count = accounts.len(), "setup: accounts updated");
            state.setup.accounts.clone_from(accounts);
            state.setup.prefill_from_keyless_account();
            return quiet;
        }
        DaemonMessageType::Models {
            models,
            selected_model,
            ..
        } => {
            // Any model list answers the session's account, so keep it for
            // the popup even when the toolbar asked for it. Each id is kept
            // once, first occurrence first: the popup keys a chip per model,
            // and a list can repeat an id (the catalog-seeded stand-in merges
            // several sources), which would give two siblings the same key.
            let mut seen = HashSet::new();
            state.setup.models = models
                .iter()
                .filter(|m| seen.insert(m.as_str()))
                .cloned()
                .collect();
            if selected_model.is_some() {
                state.setup.session_model.clone_from(selected_model);
            }
            return quiet;
        }
        // A credential or account change does not carry the new list, so
        // re-request it: `has_credential` is what ticks step 1.
        DaemonMessageType::AccountAdded { name } => {
            if let Some(handle) = daemon_tx {
                request_accounts(state, handle);
                // The account the held key waits on exists now: store it.
                if let Some(PendingKey { account, mut key }) =
                    state.setup.pending_key.take_if(|p| p.account == *name)
                {
                    // Taking the key out leaves the wrapper empty; the
                    // builder wipes the taken copy.
                    send_key(state, handle, &account, std::mem::take(&mut *key));
                }
            }
        }
        DaemonMessageType::AccountRemoved { .. } | DaemonMessageType::CredentialRemoved { .. } => {
            if let Some(handle) = daemon_tx {
                request_accounts(state, handle);
            }
        }
        DaemonMessageType::CredentialAdded { service } => {
            if let Some(handle) = daemon_tx {
                request_accounts(state, handle);
                // Follow-through, first stage: the new key is confirmed and
                // the session has no account, so select it.
                if state.setup.follow == Some(SetupFollow::AwaitCredential(service.clone())) {
                    info!(%service, "setup: key confirmed, selecting the account");
                    handle.send(ClientMessageType::SetSessionAccount {
                        name: service.clone(),
                    });
                    state.setup.follow = Some(SetupFollow::AwaitAccountSet(service.clone()));
                }
            }
        }
        DaemonMessageType::AccountAddFailed { name, error } => {
            // The key was never sent: dropping it here wipes it.
            if state
                .setup
                .pending_key
                .take_if(|p| p.account == *name)
                .is_some()
            {
                info!(account = %name, "setup: account rejected, key discarded");
                state.setup.follow = None;
            }
            if *name == state.setup.form.account_name.trim() {
                state.setup.form.error = Some(format!("Could not add the account: {error}"));
            }
        }
        DaemonMessageType::CredentialAddFailed { service, error } => {
            // A rejected key ends the follow-through: selecting an account
            // the daemon cannot use would only add a second error.
            state.setup.follow = None;
            if *service == state.setup.form.account_name.trim() {
                state.setup.form.error = Some(format!("Could not save the key: {error}"));
            }
        }
        DaemonMessageType::Sessions { sessions } => {
            // The connection attaches to the first listed session when none
            // is attached yet (`handle_session_message`), so read that one's
            // account and model; otherwise refresh the attached one's.
            let attached = state.attached_session_id;
            let summary = match attached {
                Some(id) => sessions.iter().find(|s| s.session_id == id),
                None => sessions.first(),
            };
            if let Some(summary) = summary {
                state.setup.seeded_session = Some(summary.session_id);
                state
                    .setup
                    .session_account
                    .clone_from(&summary.account_name);
                state
                    .setup
                    .session_model
                    .clone_from(&summary.selected_model);
                // Resuming a session stuck at step 3: fetch its models so the
                // popup can offer them straight away.
                if summary.account_name.is_some()
                    && summary.selected_model.is_none()
                    && let Some(handle) = daemon_tx
                {
                    request_models(state, handle);
                }
            }
            return quiet;
        }
        DaemonMessageType::Session {
            session_id,
            event:
                SessionEvent::SessionCreatedForRequester {
                    account_name,
                    selected_model,
                    ..
                },
        } => {
            // The direct reply to this connection's own CreateSession: the
            // GUI attaches to it, so its account and model are the popup's.
            debug!(?session_id, "setup: attached to a newly created session");
            state.setup.seeded_session = *session_id;
            state.setup.session_account.clone_from(account_name);
            state.setup.session_model.clone_from(selected_model);
        }
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionAttached,
        } => {
            // An attach ack carries no account or model, and this runs before
            // the session handling records the new attachment. Unless the
            // session list already seeded this very session (the attach that
            // follows the first `Sessions`), the setup state describes the
            // previous session: drop it and read this one's.
            if state.setup.seeded_session != Some(*session_id) {
                state.setup.forget_session();
                if let Some(handle) = daemon_tx {
                    send_quiet(state, handle, ClientMessageType::ListSessions);
                }
            }
        }
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event,
        } if Some(*session_id) == state.attached_session_id => {
            track_attached_session_event(state, event, daemon_tx);
        }
        _ => {}
    }
    false
}

/// The attached-session half of [`track_setup_message`].
fn track_attached_session_event(
    state: &mut AppState,
    event: &SessionEvent,
    daemon_tx: Option<&DaemonHandle>,
) {
    match event {
        SessionEvent::SessionState { selected_model, .. } => {
            state.setup.session_model.clone_from(selected_model);
        }
        SessionEvent::ModelSelected { model, .. } => {
            state.setup.session_model = Some(model.clone());
        }
        SessionEvent::SessionAccountSet { account } => {
            state.setup.session_account = Some(account.clone());
            // A different account serves a different model list.
            state.setup.models.clear();
            // Follow-through, second stage — and any account change on a
            // session without a model: fetch the list step 3 offers.
            let followed =
                state.setup.follow == Some(SetupFollow::AwaitAccountSet(account.clone()));
            if followed {
                info!(%account, "setup: account selected, fetching models");
                state.setup.follow = None;
            }
            if (followed || state.setup.session_model.is_none())
                && let Some(handle) = daemon_tx
            {
                request_models(state, handle);
            }
        }
        // A rejected account selection ends the follow-through; leaving it
        // armed would only mis-fire on a later, unrelated `SessionAccountSet`.
        SessionEvent::SessionFailed { operation, .. } if operation == "set_account" => {
            state.setup.follow = None;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::{ClientMessage, SessionStatus, SessionSummary};

    fn connected() -> (DaemonHandle, crossbeam_channel::Receiver<ClientMessage>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (DaemonHandle::new(tx), rx)
    }

    fn sent(rx: &crossbeam_channel::Receiver<ClientMessage>) -> Vec<ClientMessageType> {
        rx.try_iter().map(|m| m.inner).collect()
    }

    fn account(name: &str, has_credential: bool) -> AccountInfo {
        AccountInfo {
            name: name.to_string(),
            provider: "openai".to_string(),
            has_credential,
        }
    }

    fn attached_state() -> AppState {
        let mut state = AppState::new("/tmp/choreographr.sock");
        state.attached_session_id = Some(7);
        state
    }

    fn session_event(session_id: u64, event: SessionEvent) -> DaemonMessage {
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(session_id),
            event,
        })
    }

    fn summary(session_id: u64, account: Option<&str>, model: Option<&str>) -> SessionSummary {
        SessionSummary {
            session_id,
            title: None,
            selected_model: model.map(str::to_string),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 0,
            last_modified: 0,
            turn_count: 0,
            status: SessionStatus::default(),
            active_tool_groups: Vec::new(),
            account_name: account.map(str::to_string),
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            pinned: false,
            archived_at: None,
        }
    }

    #[test]
    fn only_the_first_unfinished_step_is_next() {
        let marks = |p| checklist(p).map(|(_, m)| m);
        assert_eq!(
            marks(SetupProgress::default()),
            [StepMark::Next, StepMark::Pending, StepMark::Pending]
        );
        let keyed_and_selected = SetupProgress {
            has_keyed_account: true,
            session_has_account: true,
            session_has_model: false,
        };
        assert_eq!(
            marks(keyed_and_selected),
            [StepMark::Done, StepMark::Done, StepMark::Next]
        );
    }

    #[test]
    fn the_popup_opens_on_its_own_only_while_it_is_needed() {
        let mut setup = SetupState {
            seeded_session: Some(7),
            ..SetupState::default()
        };
        // No session to set up yet.
        assert!(!setup.popup_visible(None, true));
        // An empty session with setup left to do.
        assert!(setup.popup_visible(Some(7), true));
        // A session that already has turns is someone's conversation.
        assert!(!setup.popup_visible(Some(7), false));
        // Another session's state says nothing about this one.
        assert!(!setup.popup_visible(Some(8), true));

        // Once setup is complete it closes on its own.
        setup.accounts = vec![account("a", true)];
        setup.session_account = Some("a".to_string());
        setup.session_model = Some("m".to_string());
        assert!(!setup.popup_visible(Some(7), true));
    }

    #[test]
    fn closing_the_popup_stops_it_reopening_but_the_toolbar_still_can() {
        let mut setup = SetupState {
            seeded_session: Some(7),
            ..SetupState::default()
        };
        setup.close_popup();
        assert!(!setup.popup_visible(Some(7), true));

        setup.open_popup();
        assert!(
            setup.popup_visible(Some(7), false),
            "opened by hand, it stays"
        );
        setup.close_popup();
        assert!(!setup.popup_visible(Some(7), true));
    }

    #[test]
    fn picking_a_provider_suggests_a_free_name() {
        let mut setup = SetupState {
            accounts: vec![account("openai", true)],
            ..SetupState::default()
        };
        setup.pick_provider("openai");
        assert_eq!(setup.form.provider, "openai");
        assert_eq!(setup.form.account_name, "openai-2");
    }

    fn account_added(name: &str) -> DaemonMessage {
        DaemonMessage::broadcast(DaemonMessageType::AccountAdded {
            name: name.to_string(),
        })
    }

    #[test]
    fn a_new_account_is_created_then_keyed_and_the_follow_is_armed() {
        let (handle, rx) = connected();
        let mut state = attached_state();
        state.setup.pick_provider("anthropic");
        state.setup.form.api_key = "  sk-test  ".to_string();

        submit_setup_form(&mut state, Some(&handle));

        // Only the account goes out; the key waits for its confirmation.
        assert!(matches!(
            &sent(&rx)[..],
            [ClientMessageType::AddAccount { name, provider, .. }]
                if name == "anthropic" && provider == "anthropic"
        ));
        assert!(
            state.setup.form.api_key.is_empty(),
            "the key leaves the form"
        );
        assert_eq!(state.setup.form.error, None);
        assert!(
            state
                .setup
                .pending_key
                .as_ref()
                .is_some_and(|p| p.account == "anthropic")
        );

        // An unrelated account's confirmation does not release the key.
        track_setup_message(&mut state, &account_added("other"), Some(&handle));
        assert!(
            !sent(&rx)
                .iter()
                .any(|m| matches!(m, ClientMessageType::AddCredential { .. }))
        );

        track_setup_message(&mut state, &account_added("anthropic"), Some(&handle));
        assert!(sent(&rx).iter().any(|m| matches!(
            m,
            ClientMessageType::AddCredential { service, .. } if service == "anthropic"
        )));
        assert!(state.setup.pending_key.is_none());
        assert_eq!(
            state.setup.follow,
            Some(SetupFollow::AwaitCredential("anthropic".to_string()))
        );
    }

    #[test]
    fn a_rejected_account_discards_its_key_unsent() {
        let (handle, rx) = connected();
        let mut state = attached_state();
        state.setup.pick_provider("anthropic");
        state.setup.form.api_key = "sk".to_string();
        submit_setup_form(&mut state, Some(&handle));
        sent(&rx);

        let failed = DaemonMessage::broadcast(DaemonMessageType::AccountAddFailed {
            name: "anthropic".to_string(),
            error: "unknown provider".to_string(),
        });
        track_setup_message(&mut state, &failed, Some(&handle));
        assert!(state.setup.pending_key.is_none());
        assert_eq!(state.setup.follow, None);
        assert!(
            state
                .setup
                .form
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unknown provider"))
        );

        // A late confirmation finds no key to send.
        track_setup_message(&mut state, &account_added("anthropic"), Some(&handle));
        assert!(
            !sent(&rx)
                .iter()
                .any(|m| matches!(m, ClientMessageType::AddCredential { .. }))
        );
    }

    #[test]
    fn a_resubmit_while_the_account_is_pending_only_replaces_the_key() {
        let (handle, rx) = connected();
        let mut state = attached_state();
        state.setup.pick_provider("anthropic");
        state.setup.form.api_key = "sk-1".to_string();
        submit_setup_form(&mut state, Some(&handle));
        state.setup.form.api_key = "sk-2".to_string();
        submit_setup_form(&mut state, Some(&handle));

        let adds = sent(&rx)
            .iter()
            .filter(|m| matches!(m, ClientMessageType::AddAccount { .. }))
            .count();
        assert_eq!(adds, 1, "the account is created once");
        assert!(state.setup.pending_key.is_some());
    }

    #[test]
    fn the_pending_key_never_appears_in_debug_output() {
        let mut state = attached_state();
        state.setup.pick_provider("anthropic");
        state.setup.form.api_key = "sk-secret-value".to_string();
        let (handle, _rx) = connected();
        submit_setup_form(&mut state, Some(&handle));
        assert!(!format!("{:?}", state.setup).contains("sk-secret-value"));
    }

    #[test]
    fn naming_an_existing_account_only_adds_its_key() {
        let (handle, rx) = connected();
        let mut state = attached_state();
        state.setup.accounts = vec![account("work", false)];
        state.setup.form.account_name = "work".to_string();
        // An existing account already has its provider, so an empty
        // provider field is not an error.
        assert_eq!(state.setup.form.provider, "");
        state.setup.form.api_key = "sk".to_string();

        submit_setup_form(&mut state, Some(&handle));

        assert!(matches!(
            &sent(&rx)[..],
            [ClientMessageType::AddCredential { service, .. }] if service == "work"
        ));
    }

    #[test]
    fn an_incomplete_form_sends_nothing() {
        let (handle, rx) = connected();
        let mut state = attached_state();
        state.setup.pick_provider("openai");
        submit_setup_form(&mut state, Some(&handle));
        assert_eq!(sent(&rx), []);
        assert!(state.setup.form.error.is_some(), "missing key is reported");

        state.setup.form.account_name = "Bad Name".to_string();
        state.setup.form.api_key = "sk".to_string();
        submit_setup_form(&mut state, Some(&handle));
        assert_eq!(sent(&rx), []);
        assert!(
            state.setup.form.api_key.is_empty(),
            "a refused key is still cleared"
        );
    }

    #[test]
    fn a_confirmed_key_selects_the_account_then_fetches_models_quietly() {
        let (handle, rx) = connected();
        let mut state = attached_state();
        state.setup.follow = Some(SetupFollow::AwaitCredential("anthropic".to_string()));

        let added = DaemonMessage::broadcast(DaemonMessageType::CredentialAdded {
            service: "anthropic".to_string(),
        });
        assert!(!track_setup_message(&mut state, &added, Some(&handle)));
        let first = sent(&rx);
        assert!(
            first
                .iter()
                .any(|m| matches!(m, ClientMessageType::ListAccounts))
        );
        assert!(first.iter().any(|m| matches!(
            m,
            ClientMessageType::SetSessionAccount { name } if name == "anthropic"
        )));

        let set = session_event(
            7,
            SessionEvent::SessionAccountSet {
                account: "anthropic".to_string(),
            },
        );
        track_setup_message(&mut state, &set, Some(&handle));
        assert_eq!(state.setup.follow, None);
        assert_eq!(state.setup.session_account.as_deref(), Some("anthropic"));
        let models_request = rx.try_recv().expect("models requested");
        assert!(matches!(
            models_request.inner,
            ClientMessageType::ListModels
        ));

        // The reply to the popup's own request feeds the popup and is kept
        // out of the history.
        let mut reply = DaemonMessage::broadcast(DaemonMessageType::Models {
            models: vec!["m1".to_string(), "m2".to_string()],
            selected_model: None,
            provisional: false,
        });
        reply.id = Some(models_request.id);
        assert!(track_setup_message(&mut state, &reply, Some(&handle)));
        assert_eq!(state.setup.models, ["m1", "m2"]);
    }

    #[test]
    fn a_rejected_key_ends_the_follow_and_reports_inline() {
        let mut state = attached_state();
        state.setup.form.account_name = "anthropic".to_string();
        state.setup.follow = Some(SetupFollow::AwaitCredential("anthropic".to_string()));
        let failed = DaemonMessage::broadcast(DaemonMessageType::CredentialAddFailed {
            service: "anthropic".to_string(),
            error: "locked".to_string(),
        });
        track_setup_message(&mut state, &failed, None);
        assert_eq!(state.setup.follow, None);
        assert!(
            state
                .setup
                .form
                .error
                .as_deref()
                .is_some_and(|e| e.contains("locked"))
        );
    }

    #[test]
    fn events_for_another_session_are_ignored() {
        let mut state = attached_state();
        let other = session_event(
            99,
            SessionEvent::ModelSelected {
                model: "m".to_string(),
                reasoning_capability: None,
            },
        );
        track_setup_message(&mut state, &other, None);
        assert_eq!(state.setup.session_model, None);

        let ours = session_event(
            7,
            SessionEvent::ModelSelected {
                model: "m".to_string(),
                reasoning_capability: None,
            },
        );
        track_setup_message(&mut state, &ours, None);
        assert_eq!(state.setup.session_model.as_deref(), Some("m"));
        assert!(!state.setup.progress().session_has_account);
    }

    #[test]
    fn the_session_list_seeds_the_session_to_be_attached() {
        let (handle, rx) = connected();
        let mut state = AppState::new("/tmp/choreographr.sock");
        let sessions = DaemonMessage::broadcast(DaemonMessageType::Sessions {
            sessions: vec![summary(3, Some("work"), None), summary(4, None, None)],
        });
        track_setup_message(&mut state, &sessions, Some(&handle));
        assert_eq!(state.setup.session_account.as_deref(), Some("work"));
        assert_eq!(state.setup.session_model, None);
        // Stuck at step 3: the models are fetched for the popup.
        assert!(
            sent(&rx)
                .iter()
                .any(|m| matches!(m, ClientMessageType::ListModels))
        );
    }

    #[test]
    fn the_initial_attach_keeps_the_state_the_session_list_seeded() {
        let (handle, rx) = connected();
        let mut state = AppState::new("/tmp/choreographr.sock");
        let sessions = DaemonMessage::broadcast(DaemonMessageType::Sessions {
            sessions: vec![summary(3, Some("work"), Some("m"))],
        });
        track_setup_message(&mut state, &sessions, Some(&handle));
        track_setup_message(
            &mut state,
            &session_event(3, SessionEvent::SessionAttached),
            Some(&handle),
        );
        assert_eq!(state.setup.session_account.as_deref(), Some("work"));
        assert_eq!(state.setup.seeded_session, Some(3));
        assert_eq!(sent(&rx), [], "nothing to re-read");
    }

    #[test]
    fn attaching_another_session_rereads_its_state_quietly() {
        let (handle, rx) = connected();
        let mut state = attached_state();
        state.setup.seeded_session = Some(7);
        state.setup.session_account = Some("work".to_string());
        state.setup.session_model = Some("m".to_string());
        state.setup.models = vec!["m".to_string()];
        state.setup.follow = Some(SetupFollow::AwaitAccountSet("work".to_string()));

        // `/attach 9`: the ack lands before the attachment is recorded.
        track_setup_message(
            &mut state,
            &session_event(9, SessionEvent::SessionAttached),
            Some(&handle),
        );
        assert_eq!(state.setup.session_account, None);
        assert_eq!(state.setup.session_model, None);
        assert_eq!(state.setup.models, Vec::<String>::new());
        assert_eq!(state.setup.follow, None);
        assert!(!state.setup.popup_visible(Some(9), true), "not yet known");
        let request = rx.try_recv().expect("session list requested");
        assert!(matches!(request.inner, ClientMessageType::ListSessions));

        // The quiet reply seeds the new session and stays out of the history.
        state.attached_session_id = Some(9);
        let mut reply = DaemonMessage::broadcast(DaemonMessageType::Sessions {
            sessions: vec![summary(7, Some("work"), Some("m")), summary(9, None, None)],
        });
        reply.id = Some(request.id);
        assert!(track_setup_message(&mut state, &reply, Some(&handle)));
        assert_eq!(state.setup.seeded_session, Some(9));
        assert_eq!(state.setup.session_account, None);
        assert!(state.setup.popup_visible(Some(9), true));
    }

    #[test]
    fn unanswered_quiet_requests_are_evicted_oldest_first() {
        let (handle, _rx) = connected();
        let mut state = attached_state();
        for _ in 0..QUIET_REQUEST_CAP + 5 {
            request_accounts(&mut state, &handle);
        }
        let quiet = &state.setup.quiet_requests;
        assert_eq!(quiet.len(), QUIET_REQUEST_CAP);
        let newest = quiet.last().copied().unwrap_or_default();
        assert_eq!(
            quiet.first().copied(),
            Some(newest - (QUIET_REQUEST_CAP as u64 - 1)),
            "the most recent ids are the ones kept"
        );
    }

    #[test]
    fn accounts_prefill_the_form_from_a_keyless_account() {
        let mut state = attached_state();
        let accounts = DaemonMessage::broadcast(DaemonMessageType::Accounts {
            accounts: vec![account("done", true), account("todo", false)],
        });
        track_setup_message(&mut state, &accounts, None);
        assert_eq!(state.setup.form.account_name, "todo");
        assert_eq!(state.setup.form.provider, "openai");
    }

    #[test]
    fn a_repeated_model_id_is_listed_once() {
        let mut state = attached_state();
        let reply = DaemonMessage::broadcast(DaemonMessageType::Models {
            models: ["b", "a", "b", "c", "a"].map(str::to_string).to_vec(),
            selected_model: None,
            provisional: true,
        });
        track_setup_message(&mut state, &reply, None);
        assert_eq!(state.setup.models, ["b", "a", "c"]);
    }

    #[test]
    fn the_model_list_is_filtered_and_capped() {
        let mut setup = SetupState {
            models: (0..100).map(|i| format!("model-{i}")).collect(),
            ..SetupState::default()
        };
        let (shown, hidden) = setup.visible_models();
        assert_eq!(shown.len(), MODEL_LIST_CAP);
        assert_eq!(hidden, 100 - MODEL_LIST_CAP);

        setup.form.model_filter = "MODEL-9".to_string();
        let (shown, hidden) = setup.visible_models();
        // model-9 and model-90..model-99.
        assert_eq!(shown.len(), 11);
        assert_eq!(hidden, 0);
    }
}
