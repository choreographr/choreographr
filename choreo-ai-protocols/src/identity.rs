//! Provider OAuth client-identity policy.
//!
//! Every provider OAuth flow needs a `client_id` identifying the application to
//! the authorization server. This module is the compiled **policy** layer that
//! records the posture Choreographr uses per provider — register its own client,
//! borrow the vendor CLI's client id, or delegate to the vendor's tooling — kept
//! distinct from the user-editable catalog **data** (a provider's `oauth`
//! profile). It is keyed by provider slug and is deliberately *not* overridable
//! by the overlay.
//!
//! Shipping or using a borrowed vendor client id is gated behind an explicit,
//! per-install opt-in flag (default off): a borrowed id is the vendor's, not
//! ours, and using it is at the user's own risk. See [`borrowed_ids_enabled`].

/// The posture Choreographr takes to identify itself to a provider's OAuth
/// authorization server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientIdentity {
    /// Choreographr registers (or dynamically obtains) its own client id.
    Registered,
    /// Choreographr borrows the vendor CLI's public client id; only used when
    /// [`borrowed_ids_enabled`] is on.
    Borrowed,
    /// Choreographr does not run the flow itself and delegates to the vendor's
    /// CLI/SDK.
    Delegate,
}

/// The client-identity policy for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClientIdentity {
    /// The posture to use.
    pub posture: ClientIdentity,
    /// The borrowed vendor client id — only meaningful when `posture` is
    /// [`ClientIdentity::Borrowed`].
    pub borrowed_client_id: Option<String>,
}

/// The environment flag that opts an install into using borrowed vendor client
/// ids. Any value other than an empty string, `0`, `false`, `off`, or `no`
/// counts as enabled.
pub const BORROWED_IDS_ENV: &str = "CHOREOGRAPHR_OAUTH_ALLOW_BORROWED_IDS";

/// The compiled client-identity table, keyed by provider slug. The tuple is
/// `(slug, borrowed client id — empty when none, posture)`.
///
/// - `openai` — the `ChatGPT` "token sharing" route registers its own client
///   (`dynamic_agent_client`; `OpenAI` issues an `oaiapp_…` id on the callback).
/// - `chatgpt` — the `ChatGPT`/Codex subscription backend borrows the vendor
///   CLI's public client id (opt-in, default off).
static CLIENT_IDENTITIES: &[(&str, &str, ClientIdentity)] = &[
    ("openai", "", ClientIdentity::Registered),
    (
        "chatgpt",
        "app_EMoamEEZ73f0CkXaXp7hrann",
        ClientIdentity::Borrowed,
    ),
];

/// Return the OAuth client-identity policy for `slug`, if the provider has one.
///
/// Providers not listed here have no OAuth identity: the caller falls back to
/// an API key or an unsupported-auth error.
#[must_use]
pub fn oauth_client_identity(slug: &str) -> Option<OAuthClientIdentity> {
    CLIENT_IDENTITIES
        .iter()
        .find(|(candidate, _, _)| *candidate == slug)
        .map(|(_, borrowed, posture)| OAuthClientIdentity {
            posture: *posture,
            borrowed_client_id: if borrowed.is_empty() {
                None
            } else {
                Some((*borrowed).to_string())
            },
        })
}

/// Parse a raw flag value (or absence) into the boolean opt-in. `None` (unset)
/// is off; a set value is on unless it is an explicit negative spelling.
fn flag_enabled(value: Option<&str>) -> bool {
    match value {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        ),
        None => false,
    }
}

/// Whether shipping or using borrowed vendor client ids is opted into.
///
/// Default OFF (a per-install gate): a borrowed id is the vendor's, and using it
/// is at the user's own risk, so it must be an explicit choice. Reads
/// [`BORROWED_IDS_ENV`].
#[must_use]
pub fn borrowed_ids_enabled() -> bool {
    flag_enabled(std::env::var(BORROWED_IDS_ENV).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_token_sharing_route_registers_its_own_client() {
        let identity = oauth_client_identity("openai").expect("openai has an OAuth identity");
        assert_eq!(identity.posture, ClientIdentity::Registered);
        assert!(identity.borrowed_client_id.is_none());
    }

    #[test]
    fn chatgpt_subscription_route_borrows_the_vendor_client_id() {
        let identity = oauth_client_identity("chatgpt").expect("chatgpt has an OAuth identity");
        assert_eq!(identity.posture, ClientIdentity::Borrowed);
        assert_eq!(
            identity.borrowed_client_id.as_deref(),
            Some("app_EMoamEEZ73f0CkXaXp7hrann")
        );
    }

    #[test]
    fn unknown_slug_has_no_identity() {
        assert!(oauth_client_identity("anthropic").is_none());
        assert!(oauth_client_identity("no-such-provider").is_none());
    }

    #[test]
    fn borrowed_id_flag_defaults_off_and_parses_negative_spellings() {
        // Unset (and every explicit negative spelling) is off.
        assert!(!flag_enabled(None));
        for off in ["", "0", "false", "off", "no", "FALSE", " Off "] {
            assert!(!flag_enabled(Some(off)), "{off:?} must be off");
        }
        // Any other value opts in.
        for on in ["1", "true", "yes", "on", "enable", "anything"] {
            assert!(flag_enabled(Some(on)), "{on:?} must be on");
        }
    }
}
