//! OIDC `id_token` claim extraction for the login flow's identity values.
//!
//! The decoders live in `choreo_shared::oauth_claims` so the login library and
//! the daemon's runtime refresh client read the same claims the same way; this
//! module re-exports them under the login library's `oauth` namespace so callers
//! keep using `choreo_client_core::oauth::{decode_claims, chatgpt_account_id, …}`.
//!
//! **No signature verification is performed**: the token arrived over TLS
//! directly from the provider's token endpoint, so its provenance is trusted and
//! only the claims are needed. The raw JWT is never stored (see the credential
//! model); only the extracted claim values are.

pub use choreo_shared::oauth_claims::{chatgpt_account_id, decode_claims};
