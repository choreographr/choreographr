//! The command-line grammar's data model: a parsed [`Command`] (and the
//! `/unlock` and `/mcp` sub-vocabularies), plus the canonical transcript echo
//! for one.

use choreo_proto::ClientMessageType;

/// How a bare or keyed `/unlock` should resolve its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnlockMethod {
    /// Unlock with the key ALREADY associated with this daemon: the stored
    /// `known_servers` `unlock_key`, falling back to the legacy raw `identity.pk`
    /// file (which is then COPIED into `known_servers.toml` so the store becomes
    /// the single source of truth).
    Raw,
    /// Unlock with the base64-encoded 32-byte key given by the user. The key
    /// is WRITE-FREE here: it is decoded, validated, and sent as an `Unlock`,
    /// and recorded into `known_servers.toml` for `addr` only AFTER the daemon
    /// CONFIRMS it (`Unlocked`) — a rejected key never pollutes the store.
    Key(String),
}

/// A parsed input line: either a message to send to the daemon or a local-UI
/// action the front-end handles itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Send this pre-built message to the daemon verbatim.
    Send(ClientMessageType),
    /// Unlock the daemon keystore, by the given [`UnlockMethod`].
    Unlock {
        /// Where the unlock key comes from (stored/legacy, or a supplied key).
        method: UnlockMethod,
    },
    /// Add a credential, from the typed field strings (the front-end resolves
    /// the unlock key and builds the wire message).
    AddCredential {
        /// The service the credential is for.
        service: String,
        /// The credential type (`api_key`, `x`, …) selecting the field layout.
        credential_type: String,
        /// The type-specific field values, in the order `credential_type`
        /// defines.
        fields: Vec<String>,
    },
    /// Remove the stored credential for a service.
    RemoveCredential {
        /// The service whose credential is removed.
        service: String,
    },
    /// `/login <account>` (alias `/add-oauth`): sign an existing account into
    /// its provider with OAuth. The front-end drives the login library (which
    /// builds the credential) and stores the result under `account`; the
    /// account's provider selects the OAuth flow. Not a message to the daemon
    /// by itself — the front-end issues the `ListOAuthProviders` request and,
    /// on completion, the `AddCredential`.
    Login {
        /// The existing account to sign in (its provider selects the flow).
        account: String,
        /// Whether to use the RFC 8628 device-code flow (`--device`).
        device: bool,
    },
    /// `/acl add <base64-pubkey>` — enroll a new client in the daemon's
    /// ACL. Rejected by the daemon unless this connection is local (Unix
    /// socket): the approver must be physically at the machine.
    AclAdd {
        /// The base64-encoded 32-byte transport public key to enroll.
        pubkey: String,
    },
    /// Undo the last turn on the attached session.
    Undo,
    /// Redo the last undone turn on the attached session.
    Redo,
    /// Continue a stopped/idle session — sends a "Please continue." prompt
    /// to the currently attached session.
    Continue,
    /// Stop a running session — cancels whatever request is currently active
    /// on the attached session, equivalent to `Cancel` with `CANCEL_ALL`.
    Stop,
    /// Refresh the models.dev catalog from upstream (conditional GET against
    /// the cached etag). `--force` bypasses the etag.
    RefreshModels {
        /// Whether to bypass the cached etag and force an unconditional
        /// fetch.
        force: bool,
    },
    /// `/mcp` — the MCP server control surface (see [`McpCommand`]).
    Mcp(McpCommand),
    /// `/quit` — exit the TUI.
    Quit,
    /// Bare `/session` — open the session manager (a local-UI command, not a
    /// message to the daemon).
    OpenSessions,
    /// Bare `/account` — open the accounts page.
    OpenAccounts,
    /// Bare `/model` — open the model picker.
    OpenModelSelector,
    /// Bare `/terminal` — open the read-only Terminal page, which reports what
    /// the client negotiated with this terminal and how to close any gap. A
    /// local-UI command with no daemon message of its own: the page is a view
    /// over data the client already holds.
    OpenTerminalHelp,
    /// Bare `/help` — open the read-only Help modal listing every keyboard
    /// shortcut. A local-UI command with no daemon message of its own.
    OpenHelp,
    /// Bare `/reasoning` — cycle the reasoning effort on the attached session.
    ReasoningCycle,
    /// `/reasoning list` — list the available reasoning levels.
    ReasoningList,
    /// `/cancel` with a non-numeric argument; carries the offending argument
    /// so the front-end can show a usage error.
    InvalidCancel(String),
    /// A slash command with no matching parse arm; carries the rest of the
    /// line for the error message.
    UnknownCommand(String),
    /// An empty (whitespace-only) line — nothing to do.
    Empty,
}

/// The `/mcp` subcommand: the client-facing MCP server control surface.
///
/// Bare `/mcp` reports the state of every configured server; a subcommand
/// acts on one server by slug or reloads the whole set. The surface is
/// deliberately request/reply — the front-end sends the request and renders
/// the daemon's reply — so the variants here mirror the wire messages
/// one-for-one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    /// `/mcp` — request and display the status of every configured server.
    Status,
    /// `/mcp reconnect <slug>` — rebuild one server's connection.
    Reconnect {
        /// The server's config key (its tool-name prefix).
        slug: String,
    },
    /// `/mcp reload` — reconcile the active session's project `.mcp.json`
    /// (add/remove/restart) without restarting the daemon.
    Reload,
    /// `/mcp trust` — trust the active session's project MCP root.
    Trust,
    /// `/mcp untrust` — revoke trust for the active session's project MCP root.
    Untrust,
    /// `/mcp trust list` — list the trusted project MCP roots.
    TrustList,
    /// `/mcp login <slug>` — start the OAuth sign-in for a remote server that
    /// declares an `oauth` block.
    Login {
        /// The server's config key (its tool-name prefix).
        slug: String,
    },
    /// `/mcp logout <slug>` — remove a remote server's stored OAuth credential.
    Logout {
        /// The server's config key (its tool-name prefix).
        slug: String,
    },
}

/// The canonical one-line echo for a parsed [`Command`], or `None` when the
/// command should not be echoed (e.g. a plain prompt, which the front-end
/// renders as the user's message).
///
/// Used to show a transcript entry for commands that do not otherwise produce
/// visible output. Secret-bearing arguments (an unlock key) are deliberately
/// omitted from the echo.
#[must_use]
pub fn command_echo(command: &Command) -> Option<String> {
    match command {
        Command::Send(message) => match message {
            ClientMessageType::RunInput { .. } => None,
            ClientMessageType::SetModel { model } => Some(format!("> set model: {model}")),
            ClientMessageType::SetReasoningEffort { effort } => {
                Some(format!("> set reasoning effort: {effort}"))
            }
            ClientMessageType::GetReasoningEffort => Some("> get reasoning effort".to_string()),
            _ => None,
        },
        // Non-Send shell commands: echo the raw line so the user can see
        // what they typed even though no ClientMessageType is sent.
        Command::Unlock { .. } => Some("> /unlock".to_string()),
        Command::AddCredential { service, .. } => Some(format!("> /add-key {service}")),
        Command::Login { account, device } => Some(if *device {
            format!("> /login {account} --device")
        } else {
            format!("> /login {account}")
        }),
        Command::RemoveCredential { service } => Some(format!("> /remove-key {service}")),
        Command::AclAdd { pubkey } => Some(format!("> /acl add {pubkey}")),
        Command::Undo => Some("> undo".to_string()),
        Command::Redo => Some("> redo".to_string()),
        Command::Continue => Some("> continue".to_string()),
        Command::Stop => Some("> stop".to_string()),
        Command::RefreshModels { force } => {
            let suffix = if *force { " --force" } else { "" };
            Some(format!("> /refresh-models{suffix}"))
        }
        Command::Mcp(McpCommand::Status) => Some("> /mcp".to_string()),
        Command::Mcp(McpCommand::Reconnect { slug }) => Some(format!("> /mcp reconnect {slug}")),
        Command::Mcp(McpCommand::Reload) => Some("> /mcp reload".to_string()),
        Command::Mcp(McpCommand::Trust) => Some("> /mcp trust".to_string()),
        Command::Mcp(McpCommand::Untrust) => Some("> /mcp untrust".to_string()),
        Command::Mcp(McpCommand::TrustList) => Some("> /mcp trust list".to_string()),
        Command::Mcp(McpCommand::Login { slug }) => Some(format!("> /mcp login {slug}")),
        Command::Mcp(McpCommand::Logout { slug }) => Some(format!("> /mcp logout {slug}")),
        _ => None,
    }
}
