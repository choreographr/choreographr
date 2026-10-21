//! Assembly of the OAuth login descriptors the daemon serves to frontends.
//!
//! The catalog is the single source for `ListOAuthProviders`: it pairs each
//! provider's [`OAuthProfile`](super::OAuthProfile) with the compiled
//! client-identity policy, so a frontend learns the issuer, scopes, redirect
//! ports, encodings, and client-identity posture from one place instead of
//! embedding a copy. Everything here is a pure projection of catalog data plus
//! that policy — no network, no process state beyond the compiled policy table.

use super::ProviderEntry;

/// Assemble the OAuth login descriptors for every provider that carries an
/// OAuth profile, applying the compiled client-identity policy.
///
/// This is the daemon's single source for the `ListOAuthProviders` reply: it
/// pairs each catalog [`OAuthProfile`](super::OAuthProfile) with the compiled
/// client-identity policy, so a frontend learns the issuer, scopes, redirect
/// ports, encodings, and client-identity posture from one place instead of
/// embedding a copy. A provider whose posture is `Borrowed` yields no
/// `client_id` unless the per-install opt-in is enabled.
#[must_use]
pub fn oauth_provider_descriptors() -> Vec<choreo_proto::OAuthProviderDescriptor> {
    let borrowed_enabled = crate::identity::borrowed_ids_enabled();
    crate::catalog::catalog_snapshot()
        .iter()
        .filter_map(|entry| assemble_oauth_descriptor(entry, borrowed_enabled))
        .collect()
}

/// Assemble one provider's OAuth descriptor, or `None` when the provider has
/// no [`OAuthProfile`](super::OAuthProfile).
///
/// Split from [`oauth_provider_descriptors`] so the assembly — including the
/// borrowed-id withholding — is unit-testable without reading process-global
/// state: the opt-in flag is passed in, and the identity policy is resolved
/// per entry.
#[must_use]
pub(super) fn assemble_oauth_descriptor(
    entry: &ProviderEntry,
    borrowed_enabled: bool,
) -> Option<choreo_proto::OAuthProviderDescriptor> {
    let profile = entry.oauth.as_ref()?;
    let policy = crate::identity::oauth_client_identity(&entry.slug);
    // A provider with an OAuth profile but no explicit policy entry registers
    // its own client: the profile's public id is authoritative.
    let posture = policy
        .as_ref()
        .map_or(crate::identity::ClientIdentity::Registered, |p| p.posture);
    let client_id = match posture {
        crate::identity::ClientIdentity::Registered => profile.client_id.clone(),
        // A borrowed id is the vendor's, not ours, so it is released only when
        // the per-install opt-in is on; otherwise the client is told nothing
        // and must obtain its own (dynamic registration) or fall back.
        crate::identity::ClientIdentity::Borrowed => {
            if borrowed_enabled {
                policy
                    .as_ref()
                    .and_then(|p| p.borrowed_client_id.clone())
                    .or_else(|| profile.client_id.clone())
            } else {
                None
            }
        }
        // A delegate flow does not run in-process, so it carries no client id.
        crate::identity::ClientIdentity::Delegate => None,
    };
    Some(choreo_proto::OAuthProviderDescriptor {
        provider: entry.slug.clone(),
        issuer: profile.issuer.clone(),
        authorize_path: profile.authorize_path.clone(),
        token_path: profile.token_path.clone(),
        client_id,
        scopes: profile.scopes.clone(),
        redirect_ports: profile.redirect_ports.clone(),
        code_exchange_encoding: profile.code_exchange_encoding,
        refresh_encoding: profile.refresh_encoding,
        client_identity: client_identity(posture),
        device_code: profile.device_code,
        device_authorization_path: profile.device_authorization_path.clone(),
        device_token_path: profile.device_token_path.clone(),
    })
}

/// Map the compiled client-identity policy to its wire twin, one-for-one.
fn client_identity(posture: crate::identity::ClientIdentity) -> choreo_proto::OAuthClientIdentity {
    match posture {
        crate::identity::ClientIdentity::Registered => {
            choreo_proto::OAuthClientIdentity::Registered
        }
        crate::identity::ClientIdentity::Borrowed => choreo_proto::OAuthClientIdentity::Borrowed,
        crate::identity::ClientIdentity::Delegate => choreo_proto::OAuthClientIdentity::Delegate,
    }
}

// These tests read the process-global `PROVIDER_CATALOG` (for the bundled
// `chatgpt` entry) and share the `catalog` serial key with the facade's own
// suite, so a libtest run that shares one process cannot interleave them with a
// catalog swap.
#[cfg(test)]
#[serial_test::serial(catalog)]
mod tests {
    use super::*;
    use crate::catalog::AuthKind;
    use crate::catalog::test_util;
    use crate::catalog::{GrantEncoding, OAuthProfile, lookup_provider};

    /// A provider entry carrying an OAuth profile, for the assembly tests.
    fn entry_with_oauth(slug: &str, client_id: Option<&str>) -> ProviderEntry {
        let mut entry = test_util::tiny_catalog().remove(0);
        entry.slug = slug.into();
        entry.oauth = Some(OAuthProfile {
            issuer: "https://auth.example".into(),
            authorize_path: "/authorize".into(),
            token_path: "/token".into(),
            scopes: vec!["openid".into(), "offline_access".into()],
            redirect_ports: vec![1455, 1457],
            code_exchange_encoding: GrantEncoding::Form,
            refresh_encoding: GrantEncoding::Json,
            device_code: true,
            device_authorization_path: Some("/deviceauth/usercode".into()),
            device_token_path: Some("/deviceauth/token".into()),
            client_id: client_id.map(str::to_string),
        });
        entry
    }

    #[test]
    fn descriptor_absent_without_an_oauth_profile() {
        let entry = test_util::tiny_catalog().remove(0);
        assert!(entry.oauth.is_none());
        assert!(assemble_oauth_descriptor(&entry, false).is_none());
    }

    #[test]
    fn registered_posture_releases_the_profile_client_id() {
        // `openai` is a Registered policy entry; the profile's public id is
        // authoritative and released regardless of the opt-in.
        let entry = entry_with_oauth("openai", Some("dynamic_agent_client"));
        let descriptor = assemble_oauth_descriptor(&entry, false).expect("descriptor");
        assert_eq!(descriptor.provider, "openai");
        assert_eq!(
            descriptor.client_id.as_deref(),
            Some("dynamic_agent_client")
        );
        assert_eq!(
            descriptor.client_identity,
            choreo_proto::OAuthClientIdentity::Registered
        );
        assert_eq!(descriptor.redirect_ports, vec![1455, 1457]);
        assert!(descriptor.device_code);
    }

    #[test]
    fn borrowed_posture_withholds_the_client_id_when_opted_out() {
        // `chatgpt` borrows the vendor CLI's public id; with the opt-in off the
        // daemon must release no client id at all.
        let entry = entry_with_oauth("chatgpt", None);
        let withheld = assemble_oauth_descriptor(&entry, false).expect("descriptor");
        assert_eq!(
            withheld.client_identity,
            choreo_proto::OAuthClientIdentity::Borrowed
        );
        assert!(withheld.client_id.is_none(), "borrowed id must be withheld");

        let released = assemble_oauth_descriptor(&entry, true).expect("descriptor");
        assert_eq!(
            released.client_id.as_deref(),
            Some("app_EMoamEEZ73f0CkXaXp7hrann"),
            "opt-in releases the borrowed vendor id"
        );
    }

    #[test]
    fn unknown_slug_defaults_to_registered() {
        // A provider with an OAuth profile but no policy entry registers its
        // own client, so the profile id flows through unchanged.
        let entry = entry_with_oauth("acme", Some("acme-public-client"));
        let descriptor = assemble_oauth_descriptor(&entry, false).expect("descriptor");
        assert_eq!(
            descriptor.client_identity,
            choreo_proto::OAuthClientIdentity::Registered
        );
        assert_eq!(descriptor.client_id.as_deref(), Some("acme-public-client"));
    }

    #[test]
    fn chatgpt_descriptor_withholds_the_borrowed_id_by_default() {
        // `ListOAuthProviders` assembles the chatgpt descriptor from the
        // overlay profile paired with the compiled `Borrowed` identity: the
        // vendor client id is released only under the per-install opt-in.
        let entry = lookup_provider("chatgpt").expect("chatgpt");
        let withheld = assemble_oauth_descriptor(&entry, false).expect("descriptor");
        assert_eq!(withheld.provider, "chatgpt");
        assert_eq!(
            withheld.client_identity,
            choreo_proto::OAuthClientIdentity::Borrowed
        );
        assert!(withheld.client_id.is_none(), "borrowed id must be withheld");
        assert!(withheld.device_code);
        assert_eq!(withheld.redirect_ports, vec![1455, 1457]);

        let released = assemble_oauth_descriptor(&entry, true).expect("descriptor");
        assert_eq!(
            released.client_id.as_deref(),
            Some("app_EMoamEEZ73f0CkXaXp7hrann"),
            "the opt-in releases the borrowed vendor id"
        );
        // The profile is the overlay's, so it also carries the auth kind the
        // catalog derives from it.
        assert_eq!(entry.auth_kind(), AuthKind::OAuth);
    }

    #[test]
    fn oauth_provider_descriptors_include_chatgpt() {
        // The daemon's single `ListOAuthProviders` source must serve the
        // chatgpt descriptor end-to-end. The client_id is not asserted here:
        // it depends on the process-wide opt-in flag, which per-environment
        // may be set — the withholding is pinned by the explicit-parameter
        // test above.
        let descriptors = oauth_provider_descriptors();
        assert!(
            descriptors.iter().any(|d| d.provider == "chatgpt"),
            "ListOAuthProviders must serve the chatgpt descriptor"
        );
    }
}
