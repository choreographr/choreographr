//! The AI-providers page state: the account list ([`AIProvidersState`]) and the
//! row-height/paging constants the page lays that list out with.
//!
//! The page's three modals are separate subjects with their own files — the
//! new-account wizard ([`super::wizard`]), the API-key modal
//! ([`super::credential`]) and the Polkadot-account import modal
//! ([`super::polkadot`]) — but they are all owned here, because their
//! open/closed state is part of what the page holds while it is on screen.

use super::{AccountWizardState, CredentialModalState, PolkadotImportState};
use choreo_proto::AccountInfo;

pub(crate) const AI_PROVIDER_ITEM_LINES: usize = 4;

/// Rows a single PgUp/PgDn press jumps in the new-account wizard's provider
/// picker (step 1).  The render window always follows the focus, so paging the
/// focus is what actually scrolls the list.
pub(crate) const PROVIDER_PAGE_LINES: usize = 10;

pub(crate) struct AIProvidersState {
    pub(crate) accounts: Vec<AccountInfo>,
    pub(crate) selection: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) confirm_remove: Option<String>,
    /// The new-account wizard modal (`n`): searchable provider picker, then
    /// slug entry.
    pub(crate) wizard: AccountWizardState,
    /// The API-key modal (`c`, or auto-opened right after account creation).
    pub(crate) credential: CredentialModalState,
    /// The Polkadot-account import modal (`p`): three-step wizard that
    /// decrypts a Polkadot-JS keystore export in the TUI and stores the
    /// resulting Substrate credential in the daemon.
    pub(crate) polkadot_import: PolkadotImportState,
}

impl AIProvidersState {
    pub(crate) fn new() -> Self {
        Self {
            accounts: Vec::new(),
            selection: None,
            scroll: 0,
            confirm_remove: None,
            wizard: AccountWizardState::new(),
            credential: CredentialModalState::new(),
            polkadot_import: PolkadotImportState::new(),
        }
    }

    pub(crate) fn set_accounts(&mut self, accounts: Vec<AccountInfo>) {
        let was_selected = self
            .selection
            .and_then(|i| self.accounts.get(i))
            .map(|a| a.name.clone());
        self.accounts = accounts;
        self.selection = if self.accounts.is_empty() {
            None
        } else {
            let idx = was_selected
                .and_then(|name| self.accounts.iter().position(|a| a.name == name))
                .unwrap_or(0);
            Some(idx)
        };
        self.scroll = 0;
    }

    pub(crate) fn select_up(&mut self) {
        let sel = self.selection.unwrap_or(0);
        if sel > 0 {
            self.selection = Some(sel - 1);
            if sel - 1 < self.scroll {
                self.scroll = self.scroll.saturating_sub(1);
            }
        }
    }

    pub(crate) fn select_down(&mut self) {
        let max = self.accounts.len().saturating_sub(1);
        let sel = self.selection.unwrap_or(0);
        if sel < max {
            self.selection = Some(sel + 1);
        }
    }

    pub(crate) fn scroll_up_page(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub(crate) fn scroll_down_page(&mut self) {
        if !self.accounts.is_empty() {
            let max_scroll = self.accounts.len().saturating_sub(1);
            self.scroll = (self.scroll + 1).min(max_scroll);
        }
    }

    pub(crate) fn remove_account(&mut self, name: &str) {
        let old_len = self.accounts.len();
        self.accounts.retain(|a| a.name != name);
        if self.accounts.len() == old_len {
            return;
        }
        if let Some(sel) = self.selection
            && sel >= self.accounts.len()
        {
            self.selection = if self.accounts.is_empty() {
                None
            } else {
                Some(self.accounts.len().saturating_sub(1))
            };
        }
        let max_scroll = self.accounts.len().saturating_sub(1);
        self.scroll = self.scroll.min(max_scroll);
        if self.confirm_remove.as_deref() == Some(name) {
            self.confirm_remove = None;
        }
    }
}
