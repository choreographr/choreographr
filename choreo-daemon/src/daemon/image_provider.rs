//! Image-generation provider resolution: the `DaemonCommand::
//! GetImageGenerationProvider` handler and its pure resolution logic.
//!
//! These are the `impl DaemonState` methods that turn an (optional) account
//! name into an opaque [`ImageProviderHandle`] for a tool thread. They live
//! in a child module (same pattern as `daemon/subscriber_handlers.rs`) so
//! `daemon.rs` stays focused on core command handling; the methods are
//! `pub(super)` because `handle_command` in the parent dispatches the
//! corresponding `DaemonCommand` variant here.
//!
//! As a CHILD of `crate::daemon`, this module reaches the parent's private
//! items (`DaemonState` fields, ...) via `use super::*`.
//!
//! [`ImageProviderHandle`] itself stays in `crate::providers`: it is the
//! protocol-erased companion of [`InferenceProvider`](crate::providers::InferenceProvider) (constructed by
//! `InferenceProvider::image_client()`,
//! consumed by `tools/image_gen.rs` next to the other provider facade
//! types), so it belongs with the provider facade, not with the command
//! plumbing that hands it out.

use super::{DaemonState, InferenceProvider, info, warn};
use thiserror::Error;

/// Why image-generation provider resolution failed.
///
/// A structured replacement for the `Result<_, String>` this path used to
/// return: the tool call site (`tools/image_gen.rs`) maps these into its
/// `ToolExecError` message so the model still sees the precise guidance
/// text, but the daemon side gets real variants to match on.
#[derive(Debug, Clone, Error)]
pub enum ImageProviderError {
    /// The keystore is locked (or no credential has ever been stored),
    /// so no provider can be resolved at all.
    #[error("keystore is locked — unlock first")]
    Locked,
    /// The explicitly named account is absent from the configured map or
    /// holds no decrypted credential (typo, wrong session account, or
    /// the keystore is locked).
    #[error(
        "account '{name}' is not configured or has no resolved provider — \
         add it and set it on the session"
    )]
    AccountNotConfigured { name: String },
    /// The account resolved, but its provider has no image backend.
    #[error("provider '{slug}' does not support image generation")]
    NoImageBackend { slug: String },
    /// Unlocked, but NO credentialed account has an image backend — the
    /// slug names the deterministic (sorted-first) provider that was
    /// inspected, so the message names an actual blocker.
    #[error("provider '{slug}' does not support image generation")]
    NoImageCapableAccount { slug: String },
}

impl DaemonState {
    /// Resolve an image-generation handle for a tool thread.
    ///
    /// Every path replies over the caller's crossbeam channel — never the
    /// broadcast machinery — and logs the outcome so a misconfigured tool
    /// call leaves a trace in the daemon log.
    pub(super) fn handle_get_image_generation_provider(
        &self,
        session_id: u64,
        account_name: Option<&str>,
        reply: &crossbeam_channel::Sender<
            Result<crate::providers::ImageProviderHandle, ImageProviderError>,
        >,
    ) {
        let result = self.resolve_image_generation_provider(session_id, account_name);
        match &result {
            Ok(handle) => {
                info!(
                    account = ?account_name,
                    slug = %handle.slug,
                    "resolved image-generation provider"
                );
            }
            Err(err) => {
                warn!(
                    account = ?account_name,
                    error = %err,
                    "image-generation provider resolution failed"
                );
            }
        }
        // Best-effort send: a dropped receiver (tool cancelled mid-call) must
        // not panic the command loop.
        let _ = reply.send(result);
    }

    /// Pure resolution logic for [`Self::handle_get_image_generation_provider`],
    /// split out so the error precedence (lock → no match → no backend) is
    /// unit-testable without threading a channel through the test.
    ///
    /// Resolution order mirrors `handle_list_models_inner`: an explicit
    /// account name wins; `None` falls through to the only/default account —
    /// there is no persistent "default account" concept in `DaemonState`, so
    /// the deterministic pick is the FIRST image-capable credentialed account
    /// in sorted order (sorted, not map order, so the answer does not depend
    /// on `HashMap` iteration noise).
    ///
    /// The client is built against the REQUESTING SESSION's socket registry
    /// (its clone lives in `session_registries`), so image sockets land in
    /// the session's cancellable scope — cancelling the session force-closes
    /// its in-flight image request too. When the session is already gone the
    /// daemon-owned registry is used as a fallback (the sockets then only
    /// close on suspend).
    pub(super) fn resolve_image_generation_provider(
        &self,
        session_id: u64,
        account_name: Option<&str>,
    ) -> Result<crate::providers::ImageProviderHandle, ImageProviderError> {
        // Lock check FIRST: after /lock there are no decrypted credentials,
        // so no client can be built — the same contract the old providers-map
        // emptiness check enforced. (A handle already handed out stays valid
        // until the tool drops it; only NEW resolution is blocked.)
        if self.locked {
            return Err(ImageProviderError::Locked);
        }

        // Registry scope: the session's clone when it is still alive, else
        // the daemon-owned registry.
        let fallback;
        let registry = if let Some(r) = self.session_registries.get(&session_id) {
            r
        } else {
            fallback = &self.daemon_registry;
            fallback
        };
        let build = |name: &str| {
            self.accounts
                .get(name)
                .and_then(|config| {
                    InferenceProvider::from_account_config(config, self.api_key_for(name), registry)
                        .ok()
                })
                .ok_or_else(|| ImageProviderError::AccountNotConfigured {
                    name: name.to_string(),
                })
        };

        if let Some(name) = account_name {
            // Name the missing account explicitly — a generic "no
            // account is configured" here would misdiagnose the
            // (common) typo/wrong-session-account case.
            let provider = build(name)?;
            let client =
                provider
                    .image_client()
                    .ok_or_else(|| ImageProviderError::NoImageBackend {
                        slug: provider.provider_slug().to_string(),
                    })?;
            Ok(crate::providers::ImageProviderHandle {
                slug: provider.provider_slug().to_string(),
                client,
            })
        } else {
            // Deterministic default: lowest credentialed account name
            // that has an image backend. HashMap order is not stable
            // between runs, so sorting keeps "only one image-capable
            // account" unambiguous and repeatable.
            let mut names: Vec<String> = self
                .accounts
                .all_configs()
                .iter()
                .map(|c| c.name.clone())
                .collect();
            names.sort();
            let mut inspected_slug = String::new();
            for name in names {
                let Ok(provider) = build(name.as_str()) else {
                    continue;
                };
                if inspected_slug.is_empty() {
                    inspected_slug = provider.provider_slug().to_string();
                }
                if let Some(client) = provider.image_client() {
                    return Ok(crate::providers::ImageProviderHandle {
                        slug: provider.provider_slug().to_string(),
                        client,
                    });
                }
            }
            Err(ImageProviderError::NoImageCapableAccount {
                slug: inspected_slug,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The glob above only reaches the parent's own imports; these test-only
    // names come from further up the tree and must be named explicitly.
    use crate::DaemonCommand;
    use crate::daemon::tests::make_daemon_state;
    use crate::daemon::{AccountConfig, ServiceCredential};

    /// Send a `GetImageGenerationProvider` command and wait for the crossbeam
    /// reply (the same channel shape the tool thread will use in production).
    fn send_get_image_provider(
        state: &mut DaemonState,
        session_id: u64,
        account_name: Option<String>,
    ) -> Result<crate::providers::ImageProviderHandle, ImageProviderError> {
        let (reply, rx) = crossbeam_channel::unbounded();
        state.handle_command(DaemonCommand::GetImageGenerationProvider {
            session_id,
            account_name,
            reply,
        });
        rx.recv().unwrap()
    }

    /// Seed `state` as an UNLOCKED daemon holding one credentialed account:
    /// the resolution path reads the account config from `state.accounts`
    /// and the decrypted key from `state.credentials` (the provider is built
    /// fresh per resolution — there is no daemon-side cache anymore).
    fn seed_account(state: &mut DaemonState, name: &str, provider_slug: &str) {
        state.locked = false;
        state
            .accounts
            .add(AccountConfig::simple(name, provider_slug))
            .unwrap();
        state.credentials.insert(
            name.to_string(),
            ServiceCredential::ApiKey {
                key: "test-key".to_string(),
            },
        );
    }

    #[test]
    fn get_image_provider_locked_keystore_errors_with_unlock_guidance() {
        let (mut state, _rx) = make_daemon_state();
        // A fresh test state starts locked — the exact shape /lock leaves.
        let err = send_get_image_provider(&mut state, 7, None).unwrap_err();
        assert!(
            matches!(err, ImageProviderError::Locked)
                && err.to_string().contains("keystore is locked"),
            "unlock guidance expected, got: {err}"
        );
    }

    #[test]
    fn get_image_provider_named_account_without_image_backend_names_slug() {
        let (mut state, _rx) = make_daemon_state();
        // An Anthropic-protocol provider has no image backend in v1.
        seed_account(&mut state, "claude", "anthropic");

        let err = send_get_image_provider(&mut state, 7, Some("claude".into())).unwrap_err();
        assert!(
            matches!(&err, ImageProviderError::NoImageBackend { slug } if slug == "anthropic"),
            "error must name the provider slug, got: {err}"
        );
    }

    #[test]
    fn get_image_provider_unknown_named_account_names_the_account() {
        let (mut state, _rx) = make_daemon_state();
        // A configured account that does NOT match the requested name: the
        // error must name the account, not the generic "no account is
        // configured" (which would misdiagnose a typo / wrong session
        // account).
        seed_account(&mut state, "openai", "openai");

        let err = send_get_image_provider(&mut state, 7, Some("oepnai".into())).unwrap_err();
        assert!(
            matches!(&err, ImageProviderError::AccountNotConfigured { name } if name == "oepnai")
                && err
                    .to_string()
                    .contains("account 'oepnai' is not configured"),
            "error must name the missing account, got: {err}"
        );
    }

    #[test]
    fn get_image_provider_happy_path_returns_handle_with_working_client() {
        let (mut state, _rx) = make_daemon_state();
        // A real OpenAI-protocol account with a fake api key — resolves
        // through the same accounts+credentials path production uses.
        seed_account(&mut state, "openai", "openai");

        let handle = send_get_image_provider(&mut state, 7, Some("openai".into())).unwrap();
        // The slug is the catalog slug, and the client is a working
        // `ImageGenerationClient` — its own provider_slug answers.
        assert_eq!(handle.slug, "openai");
        assert_eq!(handle.client.provider_slug(), "openai");
    }

    #[test]
    fn get_image_provider_default_selection_picks_image_capable_account() {
        let (mut state, _rx) = make_daemon_state();
        // One Anthropic provider (no backend) and one OpenAI provider
        // (backend): `account_name: None` must deterministically pick the
        // image-capable one.
        seed_account(&mut state, "claude", "anthropic");
        seed_account(&mut state, "openai", "openai");

        let handle = send_get_image_provider(&mut state, 7, None).unwrap();
        assert_eq!(handle.slug, "openai");
        assert_eq!(handle.client.provider_slug(), "openai");
    }

    #[test]
    fn get_image_provider_revoked_after_lock() {
        let (mut state, _rx) = make_daemon_state();
        seed_account(&mut state, "openai", "openai");
        assert!(send_get_image_provider(&mut state, 7, Some("openai".into())).is_ok());

        // Simulate /lock: the handler clears the credentials map (there is
        // no provider cache to clear anymore). A handle resolved BEFORE the
        // clear stays alive in the tool's hands (the Arc is valid), but no
        // NEW handle can be resolved — the revocation contract.
        state.credentials.clear();
        state.locked = true;

        let err = send_get_image_provider(&mut state, 7, Some("openai".into())).unwrap_err();
        assert!(
            matches!(err, ImageProviderError::Locked),
            "post-lock resolution must fail with unlock guidance, got: {err}"
        );
    }
}
