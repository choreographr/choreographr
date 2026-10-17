//! Shared support for the suite's CLI binaries.
//!
//! This micro-crate hosts the small, binary-facing helpers that every CLI crate
//! in the workspace would otherwise duplicate:
//!
//! - [`release_name`] — the dance-style release-name metadata compiled into
//!   every binary (`--version`, startup banners) and read by CI for the GitHub
//!   release title.
//! - [`clap_styles()`] — the one shared clap [`Styles`](clap::builder::Styles)
//!   used by every CLI (previously copy-pasted into each crate).
//! - [`logging`] — the shared `-v`/`-q` verbosity flags, the log-level
//!   resolution every binary applies identically, and the one subscriber
//!   initializer (an always-on hardened pid-keyed log file plus an optional
//!   per-binary stderr mirror) every binary installs.
//! - [`paths`] — the one filesystem-layout resolver: the XDG config/data/
//!   runtime/state locations and the `--base-dir` override that relocates an
//!   entire instance under a single root.
//! - [`credential`] — the workspace's shared serializable credential domain
//!   type ([`ServiceCredential`]) and its borrowed views, hosted here (not in
//!   `choreo-keystore`) so both `choreo-proto` and `choreo-keystore` can carry
//!   it without a dependency cycle or a new crate.
//! - [`oauth_claims`] — the OIDC `id_token` claim extraction that fills an OAuth
//!   credential's durable identity fields, shared so the provider login library
//!   and the runtime refresh client decode the same claims the same way.
//! - [`oauth_endpoint`] — the one endpoint-security policy (`https`, or a
//!   loopback `http` address) both the login library and the runtime refresh
//!   client apply before sending a code or token, so the two cannot drift into
//!   different rules for the same request shape.
//! - [`url`] — the slash-tolerant base-plus-path join ([`url::join_path`]) the
//!   login library, the MCP login, and the runtime refresh material share, so
//!   the same issuer/path pair cannot join to different URLs in each path.
//! - [`form`] — the `application/x-www-form-urlencoded` encoder and its paired
//!   decoder ([`form::form_encode`] / [`form::parse_query`]) the login and
//!   refresh paths share, so an OAuth request body or query is escaped — and
//!   decoded — the same way on every path.
//!
//! It is deliberately a *leaf* (deps: `base64`, `clap`, `dirs`, `serde`,
//! `serde_json`, `tracing`, `tracing-subscriber`, and `zeroize`) and holds no
//! transport logic — `choreo-proto` stays the wire protocol.
//!
//! Logging precedence follows the Unix convention: **explicit CLI flags win
//! over the ambient environment** (see [`logging`]).

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod clap_styles;
pub mod credential;
pub mod form;
pub mod logging;
pub mod oauth_claims;
pub mod oauth_endpoint;
pub mod paths;
pub mod release_name;
pub mod url;

pub use clap_styles::clap_styles;
pub use credential::{
    OAuthCredentialView, OAuthGrantEncoding, OAuthKind, ServiceCredential, SubstrateCredentialView,
    XCredentialView,
};
