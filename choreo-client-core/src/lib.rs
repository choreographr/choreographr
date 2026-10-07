//! Shared client-side core for the suite's front-ends.
//!
//! This crate holds the protocol-facing half of a Choreographr client — the
//! pieces the TUI and GUI would otherwise duplicate: the daemon connection
//! machinery, the connect-time keystore handshake, the local `known_servers`
//! trust store, the client-side command catalog, and the dispatch layer that
//! turns decoded daemon messages into UI-renderable events.
//!
//! ## Module layout
//!
//! - [`connection`] — the daemon-connection entry points (`run_daemon_*`),
//!   the server-key preflight ([`probe_server_key`], [`own_transport_pubkey`]),
//!   and the authorization handshake.
//! - [`credentials`] — the connect-time unlock / auto-bind / probe-bind
//!   handshake and the `AddCredential` message builders.
//! - [`known_servers`] — the TOFU `known_servers.toml` store of pinned server
//!   public keys and stored unlock keys ([`KnownServers`]).
//! - [`mod@command_catalog`] — the client-side catalog of slash commands and
//!   local match/dispatch helpers.
//! - [`dispatch`] — the daemon-message dispatch layer ([`TurnEventHandler`])
//!   that turns protocol events into UI callbacks.
//! - [`shell`] — the shared command-line parser ([`parse_input_line`]) for the
//!   TUI's input box.
//! - [`pending`] — the client's pending-request table ([`PendingReplies`]),
//!   the single outbound path plus the reply-correlation side table every
//!   front-end routes its requests through.
//! - [`history`] — the read-only session view ([`SessionView`]) assembling
//!   transcript state for rendering.
//! - [`diff`] — the unified-diff data model ([`FileDiff`] and friends) the
//!   front-ends render.
//! - [`error`] — the crate-wide [`ClientError`] type.
//!
//! The `test-support` feature additionally compiles `test_support`, a set of
//! fixtures shared by other crates' tests; production consumers must never
//! enable it (see its module docs).

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod command_catalog;
pub mod connection;
pub mod credentials;
pub mod diff;
pub mod dispatch;
pub mod error;
pub mod history;
pub mod known_servers;
pub mod pending;
pub mod shell;

// Test-only fixtures, compiled solely when a dependent crate opts into the
// `test-support` feature (see the module docs for why it must stay off in
// production builds).
#[cfg(feature = "test-support")]
pub mod test_support;

pub use choreo_transport::key::{fingerprint, read_server_pk};
pub use command_catalog::{
    CommandGroup, CommandMatch, CommandSpec, command_catalog, match_commands,
};
pub use connection::{
    ConnectionMode, PreflightError, own_transport_pubkey, probe_server_key, run_daemon_connection,
    run_daemon_connection_with_autostart, run_daemon_connection_with_mode, run_daemon_reader,
    run_daemon_tcp_connection, run_daemon_tcp_connection_pinned,
    run_daemon_tcp_connection_xx_first_contact, verify_daemon_authorization,
};
pub use credentials::{
    AutoBindAttempt, KeystoreAutoBind, attempt_keystore_auto_bind, bind_fresh_daemon,
    build_add_credential_from_credential, build_add_credential_message, record_unlock_key,
    resolve_private_key, try_auto_unlock_key,
};
pub use diff::{DiffHunk, DiffLine, DiffLineKind, FileDiff};
pub use dispatch::{SessionStateData, ToolCallEvent, TurnEventHandler, dispatch_daemon_message};
pub use error::{ClientError, broken_pipe};
pub use history::SessionView;
pub use known_servers::{KnownServerEntry, KnownServers, known_servers_path};
pub use pending::{Pending, PendingContext, PendingReplies, Timeout, deadline_for};
pub use shell::{
    Command, McpCommand, UnlockMethod, command_echo, is_valid_account_name, parse_input_line,
};

#[cfg(test)]
mod tests;
