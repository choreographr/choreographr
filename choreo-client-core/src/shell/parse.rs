//! The slash-command grammar: the grouped sub-parsers (`/new`, `/session`,
//! `/account`, `/model`, `/mcp`) and the flat-verb dispatcher that falls back to
//! them, turning a command remainder into a [`Command`].

use super::{Command, McpCommand, UnlockMethod};
use choreo_proto::ClientMessageType;

const INVALID_ACCOUNT_NAME: &str =
    "account name must be lowercase alphanumeric, hyphens, or underscores";

/// Returns `true` if `name` is a valid account name: non-empty and matching
/// `[a-z0-9_-]` (lowercase alphanumeric, hyphens, underscores).
#[must_use]
pub fn is_valid_account_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Syntactic check for a base64-encoded 32-byte transport public key (the
/// form both the daemon ACL and the fingerprint renderer use). Returns the
/// usage-error message on failure. The daemon re-validates authoritatively —
/// this only keeps obvious typos from a network round trip.
fn validate_pubkey_b64(b64: &str) -> Result<(), String> {
    use base64::Engine as _;
    match base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
        Ok(bytes) if bytes.len() == 32 => Ok(()),
        Ok(bytes) => Err(format!(
            "invalid pubkey: base64 decodes to {} bytes, expected 32",
            bytes.len()
        )),
        Err(e) => Err(format!("invalid pubkey: not valid base64: {e}")),
    }
}

// ── Sub-parsers for grouped shell commands ──────────────────────

/// Build the `CreateSession` message shared by `/new` and `/session new`.
///
/// Always a top-level session (`parent_session_id: None` — sub-sessions are
/// only ever spawned by the daemon's `spawn_subsession` tool) with an optional
/// title. Every other field is left `None` so the client fills in the attached
/// session's defaults and the daemon applies its own, keeping `/new` and
/// `/session new` byte-for-byte identical on the wire.
fn create_session(title: Option<String>) -> Command {
    Command::Send(ClientMessageType::CreateSession {
        title,
        parent_session_id: None,
        working_dir: None,
        context_config: None,
        account_name: None,
        selected_model: None,
        reasoning_effort: None,
    })
}

/// The optional title from the text following `new` — an all-whitespace
/// remainder (or nothing) means "no title".
fn parse_new_title(rest: &str) -> Option<String> {
    let title = rest.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// `/new [title]` — the top-level shortcut for `/session new`. A plain alias
/// in behavior, but a first-class catalog command in its own right (so it is
/// discoverable in the command palette).
fn parse_new_command(rest: &str) -> Option<Command> {
    if rest == "new" {
        return Some(create_session(None));
    }
    if let Some(title) = rest.strip_prefix("new ") {
        return Some(create_session(parse_new_title(title)));
    }
    None
}

fn parse_session_subcommand(rest: &str) -> Option<Command> {
    if let Some(sub) = rest.strip_prefix("session ") {
        let sub = sub.trim();
        if let Some(session_id) = sub.strip_prefix("switch ") {
            return Some(match session_id.trim().parse::<u64>() {
                Ok(id) => Command::Send(ClientMessageType::AttachSession { session_id: id }),
                Err(_) => Command::UnknownCommand("usage: /session switch <id>".to_string()),
            });
        }
        if let Some(session_id) = sub.strip_prefix("info ") {
            return Some(match session_id.trim().parse::<u64>() {
                Ok(id) => Command::Send(ClientMessageType::GetSessionState { session_id: id }),
                Err(_) => Command::UnknownCommand("usage: /session info <id>".to_string()),
            });
        }
        if let Some(title) = sub.strip_prefix("new ") {
            return Some(create_session(parse_new_title(title)));
        }
        if sub == "new" {
            return Some(create_session(None));
        }
        if sub == "list" {
            return Some(Command::Send(ClientMessageType::ListSessions));
        }
        return Some(Command::UnknownCommand(
            "session subcommands: list, new [title], switch <id>, info <id>".to_string(),
        ));
    }

    // Bare `/session` is the "most useful form": open the interactive session
    // manager. The direct forms (`switch`, `info`) live above.
    if rest == "session" {
        return Some(Command::OpenSessions);
    }

    None
}

fn parse_account_subcommand(rest: &str) -> Option<Command> {
    if let Some(args) = rest.strip_prefix("account ") {
        let args = args.trim();
        if args.is_empty() {
            return Some(Command::UnknownCommand(
                "usage: /account list | /account remove <name> | /account <name>".to_string(),
            ));
        }
        let parts: Vec<&str> = args.split_whitespace().collect();
        // args was checked non-empty above, so first() is always Some; the
        // catch-all arm treats the None case identically anyway.
        return Some(match parts.first().copied() {
            Some("list") => Command::Send(ClientMessageType::ListAccounts),
            Some("remove") => {
                let name = args
                    .trim_start()
                    .strip_prefix("remove")
                    .unwrap_or("")
                    .trim();
                if name.is_empty() {
                    Command::UnknownCommand("usage: /account remove <name>".to_string())
                } else if !is_valid_account_name(name) {
                    Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string())
                } else {
                    Command::Send(ClientMessageType::RemoveAccount {
                        name: name.to_string(),
                    })
                }
            }
            _ => {
                let name = args.to_string();
                if is_valid_account_name(&name) {
                    Command::Send(ClientMessageType::SetSessionAccount { name })
                } else {
                    Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string())
                }
            }
        });
    }

    // Bare `/account` is the "most useful form": open the accounts page. The
    // direct forms (`list`, `remove`, `<name>`) live above.
    if rest == "account" {
        return Some(Command::OpenAccounts);
    }

    None
}

/// `/model` has no alias: bare `model` opens the picker, `model <id>` sets the
/// model directly.
fn parse_model_command(rest: &str) -> Option<Command> {
    if rest == "model" {
        return Some(Command::OpenModelSelector);
    }
    if let Some(model) = rest.strip_prefix("model ") {
        let model = model.trim();
        if !model.is_empty() {
            return Some(Command::Send(ClientMessageType::SetModel {
                model: model.to_string(),
            }));
        }
    }
    None
}

/// `/mcp` — the MCP server control surface. Bare `/mcp` requests the status
/// list; `mcp reconnect <slug>` rebuilds one server; `mcp reload` re-reads the
/// config. Enable/disable has no daemon command (a runtime add/remove would be
/// needed), so those forms are deliberately not parsed and fall through to the
/// unknown-command usage error.
fn parse_mcp_command(rest: &str) -> Option<Command> {
    if rest == "mcp" {
        return Some(Command::Mcp(McpCommand::Status));
    }
    if let Some(sub) = rest.strip_prefix("mcp ") {
        let mut parts = sub.split_whitespace();
        return Some(match parts.next() {
            Some("reconnect") => match (parts.next(), parts.next()) {
                (Some(slug), None) => Command::Mcp(McpCommand::Reconnect {
                    slug: slug.to_string(),
                }),
                _ => Command::UnknownCommand("usage: /mcp reconnect <slug>".to_string()),
            },
            Some("reload") => match (parts.next(), parts.next()) {
                (None, None) => Command::Mcp(McpCommand::Reload),
                _ => Command::UnknownCommand("usage: /mcp reload".to_string()),
            },
            Some("trust") => match (parts.next(), parts.next()) {
                (None, None) => Command::Mcp(McpCommand::Trust),
                (Some("list"), None) => Command::Mcp(McpCommand::TrustList),
                _ => Command::UnknownCommand("usage: /mcp trust [list]".to_string()),
            },
            Some("untrust") => match (parts.next(), parts.next()) {
                (None, None) => Command::Mcp(McpCommand::Untrust),
                _ => Command::UnknownCommand("usage: /mcp untrust".to_string()),
            },
            Some("login") => match (parts.next(), parts.next()) {
                (Some(slug), None) => Command::Mcp(McpCommand::Login {
                    slug: slug.to_string(),
                }),
                _ => Command::UnknownCommand("usage: /mcp login <slug>".to_string()),
            },
            Some("logout") => match (parts.next(), parts.next()) {
                (Some(slug), None) => Command::Mcp(McpCommand::Logout {
                    slug: slug.to_string(),
                }),
                _ => Command::UnknownCommand("usage: /mcp logout <slug>".to_string()),
            },
            other => Command::UnknownCommand(format!(
                "usage: /mcp [reload | reconnect <slug> | login <slug> | logout <slug> | trust [list] | untrust] (got '{}')",
                other.unwrap_or("")
            )),
        });
    }
    None
}

/// `/cancel <request-id>` — cancel a running request, or report the argument
/// that is not a request id.
fn parse_cancel_command(rest: &str) -> Option<Command> {
    let arg = rest.strip_prefix("cancel ")?.trim();
    Some(match arg.parse::<u64>() {
        Ok(stream_id) => Command::Send(ClientMessageType::Cancel { stream_id }),
        Err(_) => Command::InvalidCancel(arg.to_string()),
    })
}

/// The bare single-word commands: no arguments, no sub-grammar, one variant
/// each.  A table rather than a run of `if`s, so a new one is a line here.
fn parse_bare_command(rest: &str) -> Option<Command> {
    Some(match rest {
        "ping" => Command::Send(ClientMessageType::Ping),
        "quit" => Command::Quit,
        // Bare `/terminal` opens the read-only Terminal page, which renders the
        // launch-time terminal profile; it has no daemon message of its own.
        "terminal" => Command::OpenTerminalHelp,
        // Bare `/help` opens the read-only Help modal (`Alt+H` is the same
        // command through the shortcut table); it has no daemon message either.
        "help" => Command::OpenHelp,
        // Bare `/reasoning-toggle` flips the bulk reasoning-visibility mode
        // (`Alt+Shift+R` is the same command through the shortcut table).  The
        // hyphen keeps it clear of `/reasoning`'s exact and `"reasoning "`-
        // prefixed arms, so the two commands can never shadow one another.
        "reasoning-toggle" => Command::ReasoningToggle,
        "lock" => Command::Send(ClientMessageType::Lock),
        "undo" => Command::Undo,
        "redo" => Command::Redo,
        "continue" => Command::Continue,
        "stop" => Command::Stop,
        _ => return None,
    })
}

/// `/unlock` — bare (use the key already stored for this daemon) or with an
/// explicit base64 unlock key.
fn parse_unlock_command(rest: &str) -> Option<Command> {
    if let Some(key) = rest.strip_prefix("unlock ") {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Some(Command::UnknownCommand(
                "usage: /unlock [base64-unlock-key]".to_string(),
            ));
        }
        // The argument is the UNLOCK KEY ITSELF (base64 of the 32 raw bytes),
        // not a passphrase: /unlock <key> records it into known_servers.toml
        // and then unlocks with it. Decoding/validation happens in
        // `resolve_private_key` (which owns the store), so the parser stays a
        // pure syntax layer.
        return Some(Command::Unlock {
            method: UnlockMethod::Key(trimmed.to_string()),
        });
    }
    if rest == "unlock" {
        return Some(Command::Unlock {
            method: UnlockMethod::Raw,
        });
    }
    None
}

/// `/login <account> [--device]` and its `/add-oauth` alias. The account must
/// already exist (its provider selects the OAuth flow); the front-end drives the
/// library. `--device` requests the headless device-code flow.
fn parse_login_command(rest: &str) -> Option<Command> {
    if rest == "login" || rest == "add-oauth" {
        return Some(Command::UnknownCommand(
            "usage: /login <account> [--device]".to_string(),
        ));
    }
    for verb in ["login ", "add-oauth "] {
        if let Some(args) = rest.strip_prefix(verb) {
            let mut tokens = args.split_whitespace();
            let Some(account) = tokens.next() else {
                return Some(Command::UnknownCommand(
                    "usage: /login <account> [--device]".to_string(),
                ));
            };
            if !is_valid_account_name(account) {
                return Some(Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string()));
            }
            let device = tokens.any(|t| t == "--device");
            return Some(Command::Login {
                account: account.to_string(),
                device,
            });
        }
    }
    None
}

/// The credential verbs: `/add-key <svc> <key>`, `/add-x <svc> <five fields>`,
/// `/remove-key <svc>`.  Each carries a typed plaintext credential and no unlock
/// key — the daemon encrypts it to its own binding — so none takes an unlock
/// argument.
fn parse_credential_command(rest: &str) -> Option<Command> {
    if let Some(args) = rest.strip_prefix("add-key ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 2 {
            return Some(Command::UnknownCommand(
                "usage: /add-key <service> <api_key>".to_string(),
            ));
        }
        // The len check above bounds-guarantees both fields; get() keeps the
        // accesses total.
        let service = parts.first().copied().unwrap_or_default().to_string();
        if !is_valid_account_name(&service) {
            return Some(Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string()));
        }
        let key = parts.get(1).copied().unwrap_or_default().to_string();
        return Some(Command::AddCredential {
            service,
            credential_type: "api_key".to_string(),
            fields: vec![key],
        });
    }

    if let Some(args) = rest.strip_prefix("add-x ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 6 {
            return Some(Command::UnknownCommand(
                "usage: /add-x <service> <api_key> <api_key_secret> <access_token> <access_token_secret> <bearer_or_->_".to_string(),
            ));
        }
        // The len check above bounds-guarantees all six fields; get() keeps
        // the accesses total.
        let [
            service,
            api_key,
            api_key_secret,
            access_token,
            access_token_secret,
            bearer_token,
        ] = [
            parts.first(),
            parts.get(1),
            parts.get(2),
            parts.get(3),
            parts.get(4),
            parts.get(5),
        ];
        let service = service.copied().unwrap_or_default().to_string();
        if !is_valid_account_name(&service) {
            return Some(Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string()));
        }
        let api_key = api_key.copied().unwrap_or_default().to_string();
        let api_key_secret = api_key_secret.copied().unwrap_or_default().to_string();
        let access_token = access_token.copied().unwrap_or_default().to_string();
        let access_token_secret = access_token_secret.copied().unwrap_or_default().to_string();
        let bearer_token = bearer_token.copied().unwrap_or_default().to_string();
        return Some(Command::AddCredential {
            service,
            credential_type: "x".to_string(),
            fields: vec![
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token,
            ],
        });
    }

    if rest == "remove-key" {
        return Some(Command::UnknownCommand(
            "usage: /remove-key <service>".to_string(),
        ));
    }
    if let Some(name) = rest.strip_prefix("remove-key ") {
        let name = name.trim();
        if name.is_empty() {
            return Some(Command::UnknownCommand(
                "usage: /remove-key <service>".to_string(),
            ));
        }
        if !is_valid_account_name(name) {
            return Some(Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string()));
        }
        return Some(Command::RemoveCredential {
            service: name.to_string(),
        });
    }

    None
}

/// `/acl <subcommand>`: currently only `add`, but parsed as a group so future
/// subcommands (`list`, `remove`) extend without touching the dispatcher. Usage
/// errors name the accepted forms.
fn parse_acl_command(rest: &str) -> Option<Command> {
    let sub = rest.strip_prefix("acl ")?;
    let parts: Vec<&str> = sub.split_whitespace().collect();
    // Only `add` exists, and it needs exactly one argument; everything else
    // (including `add` with the wrong arity) gets the usage error below.
    if parts.first().copied() != Some("add") || parts.len() != 2 {
        return Some(Command::UnknownCommand(
            "usage: /acl add <base64-pubkey>".to_string(),
        ));
    }
    // Syntactic validation only (base64 shape + 32-byte length); the daemon
    // re-validates authoritatively.
    let Some(pubkey) = parts.get(1) else {
        return Some(Command::UnknownCommand(
            "usage: /acl add <base64-pubkey>".to_string(),
        ));
    };
    if let Err(e) = validate_pubkey_b64(pubkey) {
        return Some(Command::UnknownCommand(e));
    }
    Some(Command::AclAdd {
        pubkey: pubkey.to_string(),
    })
}

/// `/reasoning` — bare cycles the effort level on the attached session, `list`
/// names the levels, and `<level>` sets one directly.
fn parse_reasoning_command(rest: &str) -> Option<Command> {
    if rest == "reasoning" {
        return Some(Command::ReasoningCycle);
    }
    let raw = rest.strip_prefix("reasoning ")?.trim();
    if raw == "list" {
        return Some(Command::ReasoningList);
    }
    let slug = raw.to_lowercase();
    let effort = match slug.as_str() {
        // Normalise common aliases to canonical slugs.
        "off" | "none" | "disabled" => "off",
        "med" => "medium",
        // Pass through arbitrary slugs (e.g. "max", "xhigh") for models with
        // custom effort sets (DeepSeek, etc.). The daemon validates the slug
        // against the model's capability set and rejects unsupported values.
        other => other,
    };
    Some(Command::Send(ClientMessageType::SetReasoningEffort {
        effort: effort.to_string(),
    }))
}

/// `/refresh-models [--force]`: refresh the models.dev catalog. Kept as a
/// non-`Send` variant so the TUI can set a "refreshing models…" status BEFORE
/// the request goes out (the reply is asynchronous).
fn parse_refresh_models_command(rest: &str) -> Option<Command> {
    if rest == "refresh-models" {
        return Some(Command::RefreshModels { force: false });
    }
    let arg = rest.strip_prefix("refresh-models ")?.trim();
    Some(match arg {
        "--force" | "force" => Command::RefreshModels { force: true },
        other => {
            Command::UnknownCommand(format!("usage: /refresh-models [--force] (got '{other}')"))
        }
    })
}

/// Turn a command remainder (the line after [`command_remainder`](super::command_remainder)'s
/// one marker slash) into a [`Command`].
///
/// Each grammar below claims a distinct verb form and returns `None` when the
/// line is not its own, so the dispatcher only has to say which grammars exist:
/// a new verb is a new parser added to this list, never another branch in a
/// growing function.  A line no grammar claims is an [`Command::UnknownCommand`]
/// echoing what was typed — the feedback a mistyped name needs.
pub(super) fn parse_command(rest: &str) -> Command {
    [
        parse_new_command,
        parse_session_subcommand,
        parse_account_subcommand,
        parse_model_command,
        parse_mcp_command,
        parse_unlock_command,
        parse_login_command,
        parse_credential_command,
        parse_acl_command,
        parse_reasoning_command,
        parse_refresh_models_command,
        parse_cancel_command,
        parse_bare_command,
    ]
    .into_iter()
    .find_map(|parse| parse(rest))
    .unwrap_or_else(|| Command::UnknownCommand(format!("unknown command: /{rest}")))
}
