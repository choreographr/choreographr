//! OAuth credential plumbing for remote MCP servers.
//!
//! The interactive sign-in (discovery, dynamic client registration, PKCE) is
//! owned by the frontend login library, which stores a
//! [`ServiceCredential::OAuth`] under a target-scoped key. This module is the
//! daemon half: a [`CredentialBackend`] trait the daemon implements over its
//! keystore, and the `rmcp`-facing adapter (in [`adapter`]) that maps the
//! durable credential to and from `rmcp`'s `StoredCredentials` so `rmcp`'s
//! `AuthorizationManager` can refresh the access token on connect.
//!
//! Two invariants shape the adapter:
//!
//! - **The access token never reaches the keystore.** `save` strips it and
//!   serves it from an in-process cache; `load` synthesises a token response
//!   carrying the durable refresh token when the cache is cold, so `rmcp`
//!   refreshes on connect rather than trusting a persisted bearer.
//! - **No `rmcp` type crosses the crate boundary.** The [`CredentialBackend`]
//!   trait is expressed entirely in this crate's own types plus the shared
//!   [`ServiceCredential`]; the adapter (which does name `rmcp` types) lives in
//!   [`adapter`] and never leaks out.
//!
//! The module is split three ways: this file owns the crate-boundary types (the
//! [`CredentialBackend`] trait, the [`McpAuthWiring`] bundle, and the
//! [`RefreshGuard`]); [`keying`] owns target canonicalization and the
//! target-scoped credential key; [`adapter`] owns the `rmcp` adapter and the
//! connect-time auth-client construction.

mod adapter;
mod keying;

pub use adapter::CredentialStoreAdapter;
pub(crate) use adapter::build_auth_client;
pub use keying::{canonical_target, credential_key, credential_key_for, credential_matches_target};

use std::sync::Arc;

use choreo_shared::ServiceCredential;

/// An owned guard held across a credential refresh; dropping it releases the
/// single-flight lock.
///
/// A backend produces one from its per-credential refresh lock. It must be
/// `Send + 'static` (the guard is held across an `await` on the sidecar
/// runtime), so a backend wraps an owned lock guard rather than a borrow.
pub struct RefreshGuard {
    _inner: Box<dyn Send>,
}

impl RefreshGuard {
    /// Wrap an owned guard whose `Drop` releases the refresh lock.
    #[must_use]
    pub fn new(inner: impl Send + 'static) -> Self {
        Self {
            _inner: Box::new(inner),
        }
    }
}

/// A daemon-provided backing store for one MCP server's OAuth credential.
///
/// The daemon implements this over its keystore; the [`CredentialStoreAdapter`]
/// drives it. Every method is blocking — the adapter calls each on a
/// `spawn_blocking` task, so the daemon's keystore round-trip (which crosses to
/// the command loop) never blocks the sidecar runtime.
pub trait CredentialBackend: Send + Sync + 'static {
    /// Load the durable OAuth credential for this server, if one is stored.
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot read its store.
    fn load(&self) -> Result<Option<ServiceCredential>, String>;
    /// Persist the durable OAuth credential (the access token stripped).
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot write its store.
    fn save(&self, credential: ServiceCredential) -> Result<(), String>;
    /// Delete the stored credential (logout).
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot delete from its store.
    fn clear(&self) -> Result<(), String>;
    /// Acquire the per-credential single-flight refresh guard, if the backend
    /// coordinates refreshes.
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot acquire the guard.
    fn acquire_refresh_guard(&self) -> Result<Option<RefreshGuard>, String>;
    /// Record that this server's refresh token was terminally rejected, so the
    /// daemon surfaces a re-authentication request instead of retrying.
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot record the rejection.
    fn mark_reauthentication(&self) -> Result<(), String>;
}

/// The OAuth wiring for one remote server: its target-scoped credential key
/// and the daemon-provided backend.
///
/// Bundled so the connect path takes one argument rather than two, and kept out
/// of [`McpServerConfig`](crate::McpServerConfig) (which derives `PartialEq` for
/// the reload comparison) because the backend is a trait object.
#[derive(Clone)]
pub struct McpAuthWiring {
    /// The credential key (`mcp:<slug>|<target-hash>`), computed by the daemon.
    pub key: String,
    /// The backend that stores the durable credential.
    pub backend: Arc<dyn CredentialBackend>,
}

impl std::fmt::Debug for McpAuthWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is not secret, but keep the shape redacted-safe and omit the
        // backend (a trait object) — never print a credential-bearing value.
        f.debug_struct("McpAuthWiring")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}
