use choreo_proto::ClientMessage;
use tracing::debug;

const INVALID_ACCOUNT_NAME: &str =
    "account name must be lowercase alphanumeric, hyphens, or underscores";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Send(ClientMessage),
    Unlock {
        method: UnlockMethod,
    },
    AddCredential {
        service: String,
        credential_type: String,
        fields: Vec<String>,
    },
    RemoveCredential {
        service: String,
    },
    /// `/acl add <base64-pubkey>` — enroll a new client in the daemon's
    /// ACL. Rejected by the daemon unless this connection is local (Unix
    /// socket): the approver must be physically at the machine.
    AclAdd {
        pubkey: String,
    },
    Undo,
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
        force: bool,
    },
    /// `/quit` — exit the TUI.
    Quit,
    /// Bare `/session` — open the session manager (a local-UI command, not a
    /// message to the daemon).
    OpenSessions,
    /// Bare `/account` — open the accounts page.
    OpenAccounts,
    /// Bare `/model` — open the model picker.
    OpenModelSelector,
    /// Bare `/reasoning` — cycle the reasoning effort on the attached session.
    ReasoningCycle,
    /// `/reasoning list` — list the available reasoning levels.
    ReasoningList,
    InvalidCancel(String),
    UnknownCommand(String),
    Empty,
}

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

fn parse_session_subcommand(rest: &str) -> Option<Command> {
    if let Some(sub) = rest.strip_prefix("session ") {
        let sub = sub.trim();
        if let Some(session_id) = sub.strip_prefix("switch ") {
            return Some(match session_id.trim().parse::<u64>() {
                Ok(id) => Command::Send(ClientMessage::AttachSession { session_id: id }),
                Err(_) => Command::UnknownCommand("usage: /session switch <id>".to_string()),
            });
        }
        if let Some(session_id) = sub.strip_prefix("info ") {
            return Some(match session_id.trim().parse::<u64>() {
                Ok(id) => Command::Send(ClientMessage::GetSessionState { session_id: id }),
                Err(_) => Command::UnknownCommand("usage: /session info <id>".to_string()),
            });
        }
        if let Some(title) = sub.strip_prefix("new ") {
            let title = title.trim();
            let title = if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            };
            return Some(Command::Send(ClientMessage::CreateSession {
                title,
                parent_session_id: None,
                working_dir: None,
                context_config: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            }));
        }
        if sub == "new" {
            return Some(Command::Send(ClientMessage::CreateSession {
                title: None,
                parent_session_id: None,
                working_dir: None,
                context_config: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            }));
        }
        if sub == "list" {
            return Some(Command::Send(ClientMessage::ListSessions));
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
            Some("list") => Command::Send(ClientMessage::ListAccounts),
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
                    Command::Send(ClientMessage::RemoveAccount {
                        name: name.to_string(),
                    })
                }
            }
            _ => {
                let name = args.to_string();
                if is_valid_account_name(&name) {
                    Command::Send(ClientMessage::SetSessionAccount { name })
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
            return Some(Command::Send(ClientMessage::SetModel {
                model: model.to_string(),
            }));
        }
    }
    None
}

pub fn parse_input_line(
    line: &str,
    next_request_id: &mut u32,
    attached_session_id: Option<u64>,
) -> Command {
    let line = line.trim();
    if line.is_empty() {
        return Command::Empty;
    }

    if let Some(rest) = line.strip_prefix('/') {
        let cmd = parse_command(rest, next_request_id, attached_session_id);
        debug!("parsed command: {cmd:?}");
        return cmd;
    }

    let request_id = *next_request_id;
    *next_request_id = next_request_id.wrapping_add(1);
    Command::Send(ClientMessage::RunInput {
        request_id,
        input: line.as_bytes().to_vec(),
    })
}

fn parse_command(
    rest: &str,
    _next_request_id: &mut u32,
    _attached_session_id: Option<u64>,
) -> Command {
    // Try grouped sub-command parsers before falling through to the flat commands.
    // Session, account, and model commands each have their own mini grammar and
    // were extracted from this function to keep each parser focused.
    if let Some(cmd) = parse_session_subcommand(rest) {
        return cmd;
    }
    if let Some(cmd) = parse_account_subcommand(rest) {
        return cmd;
    }
    if let Some(cmd) = parse_model_command(rest) {
        return cmd;
    }

    if let Some(arg) = rest.strip_prefix("cancel ") {
        return match arg.trim().parse::<u32>() {
            Ok(request_id) => Command::Send(ClientMessage::Cancel { request_id }),
            Err(_) => Command::InvalidCancel(arg.trim().to_string()),
        };
    }

    if rest == "ping" {
        return Command::Send(ClientMessage::Ping);
    }

    if rest == "quit" {
        return Command::Quit;
    }

    if let Some(key) = rest.strip_prefix("unlock ") {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Command::UnknownCommand("usage: /unlock [base64-unlock-key]".to_string());
        }
        // The argument is the UNLOCK KEY ITSELF (base64 of the 32 raw bytes),
        // not a passphrase: /unlock <key> records it into known_servers.toml
        // and then unlocks with it. Decoding/validation happens in
        // `resolve_private_key` (which owns the store), so the parser stays a
        // pure syntax layer.
        return Command::Unlock {
            method: UnlockMethod::Key(trimmed.to_string()),
        };
    }
    if rest == "unlock" {
        return Command::Unlock {
            method: UnlockMethod::Raw,
        };
    }

    if let Some(args) = rest.strip_prefix("add-key ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 2 {
            return Command::UnknownCommand("usage: /add-key <service> <api_key>".to_string());
        }
        // The len check above bounds-guarantees both fields; get() keeps the
        // accesses total.
        let service = parts.first().copied().unwrap_or_default().to_string();
        if !is_valid_account_name(&service) {
            return Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string());
        }
        let key = parts.get(1).copied().unwrap_or_default().to_string();
        // The unlock key is always resolved per-addr by
        // `build_add_credential_message` (stored → legacy → fresh), so the
        // shell takes no `unlock` argument anymore.
        return Command::AddCredential {
            service,
            credential_type: "api_key".to_string(),
            fields: vec![key],
        };
    }

    if let Some(args) = rest.strip_prefix("add-x ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 6 {
            return Command::UnknownCommand(
                "usage: /add-x <service> <api_key> <api_key_secret> <access_token> <access_token_secret> <bearer_or_->_".to_string(),
            );
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
            return Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string());
        }
        let api_key = api_key.copied().unwrap_or_default().to_string();
        let api_key_secret = api_key_secret.copied().unwrap_or_default().to_string();
        let access_token = access_token.copied().unwrap_or_default().to_string();
        let access_token_secret = access_token_secret.copied().unwrap_or_default().to_string();
        let bearer_token = bearer_token.copied().unwrap_or_default().to_string();
        // The unlock key is always resolved per-addr by
        // `build_add_credential_message` (stored → legacy → fresh), so the
        // shell takes no `unlock` argument anymore.
        return Command::AddCredential {
            service,
            credential_type: "x".to_string(),
            fields: vec![
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token,
            ],
        };
    }

    if rest == "remove-key" {
        return Command::UnknownCommand("usage: /remove-key <service>".to_string());
    }
    if let Some(name) = rest.strip_prefix("remove-key ") {
        let name = name.trim();
        if name.is_empty() {
            return Command::UnknownCommand("usage: /remove-key <service>".to_string());
        }
        if !is_valid_account_name(name) {
            return Command::UnknownCommand(INVALID_ACCOUNT_NAME.to_string());
        }
        return Command::RemoveCredential {
            service: name.to_string(),
        };
    }

    // /acl subcommand group: currently only `add`, but parsed as a group so
    // future subcommands (`list`, `remove`) extend without touching the
    // top-level dispatch. Usage errors name the accepted forms.
    if let Some(sub) = rest.strip_prefix("acl ") {
        let parts: Vec<&str> = sub.split_whitespace().collect();
        match parts.first().copied() {
            Some("add") => {
                if parts.len() != 2 {
                    return Command::UnknownCommand("usage: /acl add <base64-pubkey>".to_string());
                }
                // Syntactic validation only (base64 shape + 32-byte length);
                // the daemon re-validates authoritatively. parts.len() == 2
                // is checked above, so get(1) is always Some.
                let pubkey = match parts.get(1) {
                    Some(pubkey) => {
                        if let Err(e) = validate_pubkey_b64(pubkey) {
                            return Command::UnknownCommand(e);
                        }
                        pubkey.to_string()
                    }
                    None => {
                        return Command::UnknownCommand(
                            "usage: /acl add <base64-pubkey>".to_string(),
                        );
                    }
                };
                return Command::AclAdd { pubkey };
            }
            _ => {
                return Command::UnknownCommand("usage: /acl add <base64-pubkey>".to_string());
            }
        }
    }

    if rest == "lock" {
        return Command::Send(ClientMessage::Lock);
    }

    // /refresh-models [--force]: refresh the models.dev catalog. Kept as a
    // non-Send variant so the TUI can set a "refreshing models…" status
    // BEFORE the request goes out (and the reply is asynchronous).
    if rest == "refresh-models" {
        return Command::RefreshModels { force: false };
    }
    if let Some(arg) = rest.strip_prefix("refresh-models ") {
        return match arg.trim() {
            "--force" | "force" => Command::RefreshModels { force: true },
            other => {
                Command::UnknownCommand(format!("usage: /refresh-models [--force] (got '{other}')"))
            }
        };
    }

    if rest == "undo" {
        return Command::Undo;
    }
    if rest == "redo" {
        return Command::Redo;
    }
    if rest == "continue" {
        return Command::Continue;
    }
    if rest == "stop" {
        return Command::Stop;
    }

    // Bare `/reasoning` is the "most useful form": cycle to the next effort
    // level on the attached session. The direct forms are `list` and `<level>`.
    if rest == "reasoning" {
        return Command::ReasoningCycle;
    }
    if let Some(effort_s) = rest.strip_prefix("reasoning ") {
        let raw = effort_s.trim();
        if raw == "list" {
            return Command::ReasoningList;
        }
        let slug = raw.to_lowercase();
        let effort = match slug.as_str() {
            // Normalise common aliases to canonical slugs.
            "off" | "none" | "disabled" => "off",
            "med" => "medium",
            // Pass through arbitrary slugs (e.g. "max", "xhigh") for
            // models with custom effort sets (DeepSeek, etc.).
            // The daemon validates the slug against the model's
            // capability set and rejects unsupported values.
            other => other,
        };
        return Command::Send(ClientMessage::SetReasoningEffort {
            effort: effort.to_string(),
        });
    }

    Command::UnknownCommand(format!("unknown command: /{rest}"))
}

#[must_use]
pub fn command_echo(command: &Command) -> Option<String> {
    match command {
        Command::Send(message) => match message {
            ClientMessage::RunInput { .. } => None,
            ClientMessage::SetModel { model } => Some(format!("> set model: {model}")),
            ClientMessage::SetReasoningEffort { effort } => {
                Some(format!("> set reasoning effort: {effort}"))
            }
            ClientMessage::GetReasoningEffort => Some("> get reasoning effort".to_string()),
            _ => None,
        },
        // Non-Send shell commands: echo the raw line so the user can see
        // what they typed even though no ClientMessage is sent.
        Command::Unlock { .. } => Some("> /unlock".to_string()),
        Command::AddCredential { service, .. } => Some(format!("> /add-key {service}")),
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
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_models_parses_plain() {
        let mut id = 0;
        assert_eq!(
            parse_input_line("/refresh-models", &mut id, None),
            Command::RefreshModels { force: false },
        );
    }

    #[test]
    fn refresh_models_parses_force() {
        let mut id = 0;
        assert_eq!(
            parse_input_line("/refresh-models --force", &mut id, None),
            Command::RefreshModels { force: true },
        );
        assert_eq!(
            parse_input_line("/refresh-models force", &mut id, None),
            Command::RefreshModels { force: true },
        );
    }

    #[test]
    fn refresh_models_rejects_unknown_args() {
        let mut id = 0;
        assert!(matches!(
            parse_input_line("/refresh-models --bogus", &mut id, None),
            Command::UnknownCommand(_),
        ));
    }

    #[test]
    fn refresh_models_echo_mentions_force() {
        assert_eq!(
            command_echo(&Command::RefreshModels { force: false }).as_deref(),
            Some("> /refresh-models"),
        );
        assert_eq!(
            command_echo(&Command::RefreshModels { force: true }).as_deref(),
            Some("> /refresh-models --force"),
        );
    }
}
