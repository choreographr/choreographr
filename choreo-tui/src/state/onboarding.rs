//! First-run setup guidance: the "what is left before I can chat" checklist
//! shown on an empty chat page, the default account name the new-account
//! wizard pre-fills, and the one-shot follow-through that walks a freshly
//! created account straight into the active session and the model picker.
//!
//! Everything here is pure state derivation over `App` (no I/O, no channels),
//! so the rules are unit-testable without a daemon. The render side lives in
//! `render/welcome.rs`; the message-handling side (auto-selecting the account
//! once its credential is confirmed) lives in `connection/daemon.rs`.

use super::App;
use choreo_client_core::is_valid_account_name;

/// How far the attached session is through first-run setup. Each flag is
/// one step of the welcome checklist, in the order the user has to do them:
/// an account must hold an API key before a session can use it, and the
/// model list is fetched through the session's account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetupProgress {
    /// At least one AI provider account has a stored credential (API key).
    pub(crate) has_keyed_account: bool,
    /// The attached session has an account selected.
    pub(crate) session_has_account: bool,
    /// The attached session has a model selected.
    pub(crate) session_has_model: bool,
}

impl SetupProgress {
    /// All steps done: a prompt can be sent.
    pub(crate) fn is_complete(self) -> bool {
        self.has_keyed_account && self.session_has_account && self.session_has_model
    }
}

/// The two-stage first-run follow-through after the user saves an API key
/// while the attached session has no account. Each stage waits for the
/// daemon's confirmation rather than firing optimistically: selecting an
/// account whose key the daemon rejected would only produce a second error,
/// and the model list is fetched through the session's account, so opening
/// the picker before the selection lands would list nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetupFollow {
    /// Waiting for `CredentialAdded` for this account; then select it for
    /// the attached session.
    AwaitCredential(String),
    /// Waiting for `SessionAccountSet` for this account; then open the
    /// model picker.
    AwaitAccountSet(String),
}

impl App {
    /// Arm the follow-through for `account` when its key is being saved and
    /// the attached session has no account yet. A session that already has
    /// an account keeps it: adding a second key must not silently switch
    /// what an in-use session talks to.
    pub(crate) fn arm_setup_follow(&mut self, account: &str) {
        self.setup_follow = (self.attached_session_id.is_some()
            && self.attached_account_slug.is_none())
        .then(|| SetupFollow::AwaitCredential(account.to_string()));
    }

    /// Pick the wizard's highlighted provider (advancing to the name step)
    /// and pre-fill the account name with a suggestion, so the common case
    /// is a single Enter. The field stays editable for anyone who wants a
    /// different name. A no-op when the filtered list is empty.
    pub(crate) fn confirm_wizard_provider(&mut self) {
        self.ai_providers.wizard.confirm_provider(&self.providers);
        let Some(provider) = self.ai_providers.wizard.picked_slug.as_deref() else {
            return;
        };
        tracing::debug!(slug = provider, "provider picked in account wizard");
        let existing: Vec<&str> = self
            .ai_providers
            .accounts
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        let suggestion = suggest_account_name(provider, &existing);
        self.ai_providers.wizard.prefill_slug(suggestion);
    }

    /// The attached session's setup state, read from the accounts list and
    /// the session's display state (the same fields the status bar shows).
    pub(crate) fn setup_progress(&self) -> SetupProgress {
        let display = self.active_display_ref();
        SetupProgress {
            has_keyed_account: self.ai_providers.accounts.iter().any(|a| a.has_credential),
            session_has_account: self.attached_account_slug.is_some(),
            session_has_model: display.is_some_and(|d| d.selected_model.is_some()),
        }
    }

    /// Whether the chat page should show the welcome panel in place of the
    /// (empty) history: a session is attached and nothing is in it yet.
    /// Reads `visible_turn_ids`, so callers must run after the per-frame
    /// `compute_total_height_and_markers` rebuild (as `render_chat` does).
    pub(crate) fn show_welcome(&self) -> bool {
        self.attached_session_id.is_some()
            && self
                .active_display_ref()
                .is_some_and(|d| d.visible_turn_ids.is_empty())
    }
}

/// The default name the new-account wizard pre-fills for an account on
/// `provider_slug`: the provider slug itself, made valid for an account name
/// (lowercase alphanumerics, `-`, `_`), with a `-2`, `-3`, … suffix when an
/// account of that name already exists. A first-time user can just press
/// Enter; someone adding a second key for the same provider gets a distinct
/// name instead of a "name taken" error.
pub(crate) fn suggest_account_name(provider_slug: &str, existing: &[&str]) -> String {
    // Catalog slugs are mostly valid already; the few that are not use dots
    // (`wafer.ai`) or capitals, which map onto a hyphen / lowercase.
    let mut base: String = provider_slug
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if !is_valid_account_name(&base) {
        // An empty or otherwise unusable slug still gets a usable default.
        base = "account".to_string();
    }
    let taken = |name: &str| existing.contains(&name);
    if !taken(&base) {
        return base;
    }
    // `existing.len() + 1` candidates cannot all be taken by `existing.len()`
    // names, so this range always holds a free one; the fallback is only for
    // totality.
    (2..=existing.len() + 2)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::App;
    use choreo_proto::AccountInfo;

    fn account(name: &str, has_credential: bool) -> AccountInfo {
        AccountInfo {
            name: name.to_string(),
            provider: "anthropic".to_string(),
            has_credential,
        }
    }

    #[test]
    fn suggestion_is_the_provider_slug_when_free() {
        assert_eq!(suggest_account_name("anthropic", &[]), "anthropic");
    }

    #[test]
    fn suggestion_sanitizes_invalid_characters() {
        assert_eq!(suggest_account_name("wafer.ai", &[]), "wafer-ai");
        assert_eq!(suggest_account_name("Mixed.Case", &[]), "mixed-case");
    }

    #[test]
    fn suggestion_falls_back_for_an_empty_slug() {
        assert_eq!(suggest_account_name("", &[]), "account");
    }

    #[test]
    fn suggestion_suffixes_past_taken_names() {
        assert_eq!(
            suggest_account_name("openai", &["openai", "openai-2"]),
            "openai-3"
        );
    }

    #[test]
    fn fresh_app_has_no_setup_progress() {
        let app = App::new();
        assert_eq!(app.setup_progress(), SetupProgress::default());
        assert!(!app.setup_progress().is_complete());
    }

    #[test]
    fn an_account_without_a_key_does_not_count() {
        let mut app = App::new();
        app.ai_providers.set_accounts(vec![account("a", false)]);
        assert!(!app.setup_progress().has_keyed_account);
        app.ai_providers
            .set_accounts(vec![account("a", false), account("b", true)]);
        assert!(app.setup_progress().has_keyed_account);
    }

    #[test]
    fn session_account_comes_from_the_attached_slug() {
        let mut app = App::new();
        app.attached_account_slug = Some("anthropic".to_string());
        assert!(app.setup_progress().session_has_account);
    }

    #[test]
    fn complete_requires_every_step() {
        let all = SetupProgress {
            has_keyed_account: true,
            session_has_account: true,
            session_has_model: true,
        };
        assert!(all.is_complete());
        assert!(
            !SetupProgress {
                session_has_model: false,
                ..all
            }
            .is_complete()
        );
    }

    #[test]
    fn follow_is_armed_only_for_a_session_without_an_account() {
        let mut app = App::new();
        // No attached session: nothing to select the account into.
        app.arm_setup_follow("anthropic");
        assert_eq!(app.setup_follow, None);

        app.attached_session_id = Some(1);
        app.arm_setup_follow("anthropic");
        assert_eq!(
            app.setup_follow,
            Some(SetupFollow::AwaitCredential("anthropic".to_string()))
        );

        // A session already using an account keeps it.
        app.attached_account_slug = Some("openai".to_string());
        app.arm_setup_follow("anthropic");
        assert_eq!(app.setup_follow, None);
    }

    #[test]
    fn no_welcome_without_an_attached_session() {
        assert!(!App::new().show_welcome());
    }
}
