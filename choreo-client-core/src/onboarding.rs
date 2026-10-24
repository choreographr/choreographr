//! First-run setup rules shared by the front-ends: the three-step "what is
//! left before I can chat" checklist ([`SetupProgress`]), the follow-through
//! that walks a freshly keyed account into the attached session and on to
//! model selection ([`SetupFollow`]), and the default account name a new
//! account is offered ([`suggest_account_name`]).
//!
//! Everything here is pure derivation over values the front-ends already
//! hold (the account list and the attached session's account/model), so the
//! TUI and GUI judge setup identically and the rules are testable without a
//! daemon. Each front-end owns only its rendering and the sends.

use crate::is_valid_account_name;
use choreo_proto::AccountInfo;

/// Providers offered first when a user creates an account, in this order.
/// These are the ones most people arrive with a key for, so a first-time user
/// sees a familiar name up front instead of a page of aggregators that happen
/// to sort early. A front-end skips any slug its catalog does not list.
pub const FEATURED_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "openrouter",
    "xai",
    "deepseek",
    "mistral",
    "groq",
    "ollama",
];

/// How far the attached session is through first-run setup. Each flag is one
/// step of the welcome checklist, in the order the user has to do them: an
/// account must hold an API key before a session can use it, and the model
/// list is fetched through the session's account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetupProgress {
    /// The account the session will use holds a stored credential (see
    /// [`SetupProgress::derive`] for which account that is).
    pub has_keyed_account: bool,
    /// The attached session has an account selected.
    pub session_has_account: bool,
    /// The attached session has a model selected.
    pub session_has_model: bool,
}

impl SetupProgress {
    /// Judge setup from the account list and the attached session's selected
    /// account and model.
    ///
    /// Step 1 is judged against the account the session will actually use:
    /// the selected one when there is one, otherwise any keyed account the
    /// user could select next. A global "some account has a key" would tick
    /// the box for an account this session is NOT using, while the model list
    /// is fetched through the session's own account — so a session pointed at
    /// a key-less account would look ready and then fail at step 3.
    #[must_use]
    pub fn derive(
        accounts: &[AccountInfo],
        session_account: Option<&str>,
        session_has_model: bool,
    ) -> Self {
        let has_keyed_account = match session_account {
            Some(name) => accounts.iter().any(|a| a.name == name && a.has_credential),
            None => accounts.iter().any(|a| a.has_credential),
        };
        Self {
            has_keyed_account,
            session_has_account: session_account.is_some(),
            session_has_model,
        }
    }

    /// All steps done: a prompt can be sent.
    #[must_use]
    pub fn is_complete(self) -> bool {
        self.has_keyed_account && self.session_has_account && self.session_has_model
    }
}

/// The two-stage first-run follow-through after the user saves an API key
/// while the attached session has no account. Each stage waits for the
/// daemon's confirmation rather than firing optimistically: selecting an
/// account whose key the daemon rejected would only produce a second error,
/// and the model list is fetched through the session's account, so asking
/// for models before the selection lands would list nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupFollow {
    /// Waiting for `CredentialAdded` for this account; then select it for
    /// the attached session.
    AwaitCredential(String),
    /// Waiting for `SessionAccountSet` for this account; then move on to
    /// model selection.
    AwaitAccountSet(String),
}

impl SetupFollow {
    /// The follow-through to arm when `account`'s key is being saved: only
    /// when a session is attached and has no account yet. A session that
    /// already has an account keeps it — adding a second key must not
    /// silently switch what an in-use session talks to.
    #[must_use]
    pub fn arm(account: &str, session_attached: bool, session_has_account: bool) -> Option<Self> {
        (session_attached && !session_has_account)
            .then(|| Self::AwaitCredential(account.to_string()))
    }
}

/// The default name offered for a new account on `provider_slug`: the
/// provider slug itself, made valid for an account name (lowercase
/// alphanumerics, `-`, `_`), with a `-2`, `-3`, … suffix when an account of
/// that name already exists. A first-time user can just accept it; someone
/// adding a second key for the same provider gets a distinct name instead of
/// a "name taken" error.
#[must_use]
pub fn suggest_account_name(provider_slug: &str, existing: &[&str]) -> String {
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
    // `base` is taken here, so strip to a `-N` suffix (N from 2). `existing`
    // is finite, so this search always terminates on a free name; returning it
    // directly (rather than an `unwrap_or` fallback) keeps the result a name
    // the caller does not already hold.
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn suggestion_skips_every_taken_suffix() {
        // The suffix search must advance past an unbroken run of taken names
        // rather than stop at some bound.
        assert_eq!(
            suggest_account_name("svc", &["svc", "svc-2", "svc-3", "svc-4"]),
            "svc-5"
        );
    }

    #[test]
    fn nothing_configured_has_no_progress() {
        let progress = SetupProgress::derive(&[], None, false);
        assert_eq!(progress, SetupProgress::default());
        assert!(!progress.is_complete());
    }

    #[test]
    fn an_account_without_a_key_does_not_count() {
        assert!(!SetupProgress::derive(&[account("a", false)], None, false).has_keyed_account);
        assert!(
            SetupProgress::derive(&[account("a", false), account("b", true)], None, false)
                .has_keyed_account
        );
    }

    #[test]
    fn step_one_is_judged_against_the_sessions_account() {
        // No session account yet: any keyed account satisfies step 1.
        assert!(SetupProgress::derive(&[account("a", true)], None, false).has_keyed_account);

        // The session uses an account with NO key: step 1 is not done even
        // though another account has one, because the model list is fetched
        // through the session's own account.
        let accounts = [account("a", true), account("b", false)];
        let progress = SetupProgress::derive(&accounts, Some("b"), false);
        assert!(!progress.has_keyed_account);
        assert!(progress.session_has_account);

        // The session's own account is keyed: step 1 is done.
        let accounts = [account("a", true), account("b", true)];
        assert!(SetupProgress::derive(&accounts, Some("b"), false).has_keyed_account);
    }

    #[test]
    fn complete_requires_every_step() {
        let all = SetupProgress::derive(&[account("a", true)], Some("a"), true);
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
    fn follow_is_armed_only_for_an_attached_session_without_an_account() {
        // No attached session: nothing to select the account into.
        assert_eq!(SetupFollow::arm("anthropic", false, false), None);
        assert_eq!(
            SetupFollow::arm("anthropic", true, false),
            Some(SetupFollow::AwaitCredential("anthropic".to_string()))
        );
        // A session already using an account keeps it.
        assert_eq!(SetupFollow::arm("anthropic", true, true), None);
    }
}
