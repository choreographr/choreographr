pub mod connection;
pub mod credentials;
pub mod diff;
pub mod dispatch;
pub mod error;
pub mod history;
pub mod known_servers;
pub mod shell;

pub use choreo_transport::key::{fingerprint, read_server_pk};
pub use connection::{
    ConnectionMode, PreflightError, own_transport_pubkey, probe_server_key, run_daemon_connection,
    run_daemon_connection_with_mode, run_daemon_reader, run_daemon_tcp_connection,
    run_daemon_tcp_connection_pinned, run_daemon_tcp_connection_xx_first_contact,
    verify_daemon_authorization,
};
pub use credentials::{
    build_add_credential_from_credential, build_add_credential_message, record_unlock_key,
    resolve_private_key, try_auto_unlock_key,
};
pub use diff::{DiffHunk, DiffLine, DiffLineKind, FileDiff};
pub use dispatch::{SessionStateData, ToolCallEvent, TurnEventHandler, dispatch_daemon_message};
pub use error::{ClientError, broken_pipe};
pub use history::SessionView;
pub use known_servers::{KnownServerEntry, KnownServers, known_servers_path};
pub use shell::{
    ShellCommand, UnlockMethod, is_valid_account_name, parse_input_line, shell_command_echo,
};

#[cfg(test)]
mod tests;
