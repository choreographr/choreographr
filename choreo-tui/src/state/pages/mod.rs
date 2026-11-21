//! Page/overlay state: the `Page` enum plus, in one file per subject, the
//! state the pages and their modals carry.
//!
//! - [`picker`] — the pure machinery both picker popups share: the filter
//!   predicate, the visible-window arithmetic, and the pin-at-middle arrow/wheel
//!   navigation.
//! - [`providers`] — the AI-providers page state ([`AIProvidersState`]) and the
//!   row-height/paging constants its account list is laid out with.
//! - [`wizard`] — the new-account wizard ([`AccountWizardState`]).
//! - [`credential`] — the API-key modal ([`CredentialModalState`]).
//! - [`polkadot`] — the Polkadot-account import modal ([`PolkadotImportState`]).
//!
//! One file per subject rather than one file per page: the four modals share a
//! page but no lifetime or data, and each is edited on its own, so keeping them
//! together only made every edit touch a file four subjects were queueing on.
//! The model-selector popup lives in the sibling `state/model_selector.rs`, the
//! session-manager list/detail state in `state/session_manager.rs`, and the
//! history marker (`Marker`) in `state/history/`, beside the height model it
//! indexes.
//!
//! Everything here is re-exported from `state` (`pub(crate) use pages::*`), so
//! `crate::state::X` keeps resolving as it did when this was a single file.

mod credential;
mod picker;
mod polkadot;
mod providers;
mod wizard;

pub(crate) use credential::CredentialModalState;
// `picker_window` is `pub(crate)` because the model selector and the account
// list both lay out their windows with it; the filter predicate and the
// navigation step are only needed by the two picker implementations, which both
// live under `state`, so they stay at `pub(super)`.
pub(crate) use picker::picker_window;
pub(super) use picker::{filter_matches, step_focus};
pub(crate) use polkadot::{PolkadotImportState, PolkadotImportStep};
pub(crate) use providers::{AI_PROVIDER_ITEM_LINES, AIProvidersState, PROVIDER_PAGE_LINES};
pub(crate) use wizard::{AccountWizardState, AccountWizardStep};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Chat,
    SessionManager,
    AIProviders,
}
