//! First-run setup guidance: the App-side glue over the shared setup rules
//! in `choreo_client_core::onboarding` — reading the checklist state off the
//! attached session, arming the follow-through that walks a freshly created
//! account straight into the session and the model picker, and pre-filling
//! the new-account wizard's name.
//!
//! Everything here is pure state derivation over `App` (no I/O, no channels),
//! so the rules are unit-testable without a daemon. The render side lives in
//! `render/welcome.rs`; the message-handling side (auto-selecting the account
//! once its credential is confirmed) lives in `connection/daemon.rs`.

use super::App;
use choreo_client_core::{SetupFollow, SetupProgress, suggest_account_name};

impl App {
    /// Arm the follow-through for `account` when its key is being saved and
    /// the attached session has no account yet. A session that already has
    /// an account keeps it: adding a second key must not silently switch
    /// what an in-use session talks to.
    pub(crate) fn arm_setup_follow(&mut self, account: &str) {
        self.setup_follow = SetupFollow::arm(
            account,
            self.attached_session_id.is_some(),
            self.attached_account_slug.is_some(),
        );
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
        SetupProgress::derive(
            &self.ai_providers.accounts,
            self.attached_account_slug.as_deref(),
            self.active_display_ref()
                .is_some_and(|d| d.selected_model.is_some()),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::AccountInfo;

    fn account(name: &str, has_credential: bool) -> AccountInfo {
        AccountInfo {
            name: name.to_string(),
            provider: "anthropic".to_string(),
            has_credential,
        }
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
    fn step_one_is_judged_against_the_sessions_account() {
        let mut app = App::new();
        // No session account yet: any keyed account satisfies step 1.
        app.ai_providers.set_accounts(vec![account("a", true)]);
        assert!(app.setup_progress().has_keyed_account);

        // The session uses an account with NO key: step 1 is not done even
        // though another account has one, because the model list is fetched
        // through the session's own account.
        app.attached_session_id = Some(1);
        app.attached_account_slug = Some("b".to_string());
        app.ai_providers
            .set_accounts(vec![account("a", true), account("b", false)]);
        assert!(!app.setup_progress().has_keyed_account);

        // The session's own account is keyed: step 1 is done.
        app.ai_providers
            .set_accounts(vec![account("a", true), account("b", true)]);
        assert!(app.setup_progress().has_keyed_account);
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
