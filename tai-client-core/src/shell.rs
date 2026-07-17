use tai_proto::{ClientMessage, ThinkingEffort};
use tracing::debug;

const INVALID_ACCOUNT_NAME: &str =
    "account name must be lowercase alphanumeric, hyphens, or underscores";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnlockMethod {
    /// Read identity.pk directly (no passphrase).
    Raw,
    /// Decrypt identity.pk.enc with the given passphrase.
    Passphrase(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Send(ClientMessage),
    Unlock {
        method: UnlockMethod,
    },
    AddCredential {
        service: String,
        credential_type: String,
        fields: Vec<String>,
        unlock: bool,
    },
    RemoveCredential {
        service: String,
    },
    Undo,
    Redo,
    InvalidCancel(String),
    UnknownCommand(String),
    Empty,
}

/// Returns `true` if `name` is a valid account name: non-empty and matching
/// `[a-z0-9_-]` (lowercase alphanumeric, hyphens, underscores).
pub fn is_valid_account_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

// ── Sub-parsers for grouped shell commands ──────────────────────

fn parse_session_subcommand(rest: &str, attached_session_id: Option<u64>) -> Option<ShellCommand> {
    if let Some(sub) = rest.strip_prefix("session ") {
        let sub = sub.trim();
        if let Some(session_id) = sub.strip_prefix("switch ") {
            return Some(match session_id.trim().parse::<u64>() {
                Ok(id) => ShellCommand::Send(ClientMessage::AttachSession { session_id: id }),
                Err(_) => ShellCommand::UnknownCommand("usage: /session switch <id>".to_string()),
            });
        }
        if let Some(session_id) = sub.strip_prefix("info ") {
            return Some(match session_id.trim().parse::<u64>() {
                Ok(id) => ShellCommand::Send(ClientMessage::GetSessionState { session_id: id }),
                Err(_) => ShellCommand::UnknownCommand("usage: /session info <id>".to_string()),
            });
        }
        if let Some(title) = sub.strip_prefix("new ") {
            let title = title.trim();
            let title = if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            };
            return Some(ShellCommand::Send(ClientMessage::CreateSession {
                title,
                parent_session_id: None,
                working_dir: None,
                max_turns: None,
                context_config: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            }));
        }
        if sub == "new" {
            return Some(ShellCommand::Send(ClientMessage::CreateSession {
                title: None,
                parent_session_id: None,
                working_dir: None,
                max_turns: None,
                context_config: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            }));
        }
        if sub == "list" {
            return Some(ShellCommand::Send(ClientMessage::ListSessions));
        }
        return Some(ShellCommand::UnknownCommand(
            "session subcommands: list, new [title], switch <id>, info <id>".to_string(),
        ));
    }

    if rest == "session" {
        return Some(match attached_session_id {
            Some(id) => ShellCommand::Send(ClientMessage::GetSessionState { session_id: id }),
            None => ShellCommand::UnknownCommand(
                "no session attached. use /session switch <id> to attach".to_string(),
            ),
        });
    }

    None
}

fn parse_account_subcommand(rest: &str) -> Option<ShellCommand> {
    if let Some(args) = rest.strip_prefix("account ") {
        let args = args.trim();
        if args.is_empty() {
            return Some(ShellCommand::UnknownCommand(
                "usage: /account list | /account remove <name> | /account <name>".to_string(),
            ));
        }
        let parts: Vec<&str> = args.split_whitespace().collect();
        return Some(match parts[0] {
            "list" => ShellCommand::Send(ClientMessage::ListAccounts),
            "remove" => {
                let name = args
                    .trim_start()
                    .strip_prefix("remove")
                    .unwrap_or("")
                    .trim();
                if name.is_empty() {
                    ShellCommand::UnknownCommand("usage: /account remove <name>".to_string())
                } else if !is_valid_account_name(name) {
                    ShellCommand::UnknownCommand(INVALID_ACCOUNT_NAME.to_string())
                } else {
                    ShellCommand::Send(ClientMessage::RemoveAccount {
                        name: name.to_string(),
                    })
                }
            }
            _ => {
                let name = args.to_string();
                if !is_valid_account_name(&name) {
                    ShellCommand::UnknownCommand(INVALID_ACCOUNT_NAME.to_string())
                } else {
                    ShellCommand::Send(ClientMessage::SetSessionAccount { name })
                }
            }
        });
    }

    if rest == "account" {
        return Some(ShellCommand::Send(ClientMessage::ListAccounts));
    }

    None
}

/// Handles both `/model` and `/models` — they are aliases.
fn parse_model_subcommand(rest: &str) -> Option<ShellCommand> {
    if rest == "model" || rest == "models" {
        return Some(ShellCommand::Send(ClientMessage::ListModels));
    }
    if let Some(model) = rest
        .strip_prefix("model ")
        .or_else(|| rest.strip_prefix("models "))
    {
        let model = model.trim();
        if !model.is_empty() {
            return Some(ShellCommand::Send(ClientMessage::SetModel {
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
) -> ShellCommand {
    let line = line.trim();
    if line.is_empty() {
        return ShellCommand::Empty;
    }

    if let Some(rest) = line.strip_prefix('/') {
        let cmd = parse_command(rest, next_request_id, attached_session_id);
        debug!("parsed command: {cmd:?}");
        return cmd;
    }

    let request_id = *next_request_id;
    *next_request_id = next_request_id.wrapping_add(1);
    ShellCommand::Send(ClientMessage::RunInput {
        request_id,
        input: line.as_bytes().to_vec(),
    })
}

fn parse_command(
    rest: &str,
    next_request_id: &mut u32,
    attached_session_id: Option<u64>,
) -> ShellCommand {
    // Try grouped sub-command parsers before falling through to the flat commands.
    // Session, account, and model commands each have their own mini grammar and
    // were extracted from this function to keep each parser focused.
    if let Some(cmd) = parse_session_subcommand(rest, attached_session_id) {
        return cmd;
    }
    if let Some(cmd) = parse_account_subcommand(rest) {
        return cmd;
    }
    if let Some(cmd) = parse_model_subcommand(rest) {
        return cmd;
    }

    if let Some(arg) = rest.strip_prefix("cancel ") {
        return match arg.trim().parse::<u32>() {
            Ok(request_id) => ShellCommand::Send(ClientMessage::Cancel { request_id }),
            Err(_) => ShellCommand::InvalidCancel(arg.trim().to_string()),
        };
    }

    if rest == "ping" {
        return ShellCommand::Send(ClientMessage::Ping);
    }

    if let Some(passphrase) = rest.strip_prefix("unlock ") {
        let trimmed = passphrase.trim();
        if trimmed.is_empty() {
            return ShellCommand::UnknownCommand("usage: /unlock [passphrase]".to_string());
        }
        return ShellCommand::Unlock {
            method: UnlockMethod::Passphrase(trimmed.to_string()),
        };
    }
    if rest == "unlock" {
        return ShellCommand::Unlock {
            method: UnlockMethod::Raw,
        };
    }

    if let Some(args) = rest.strip_prefix("add-key ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 2 {
            return ShellCommand::UnknownCommand("usage: /add-key <service> <api_key>".to_string());
        }
        let service = parts[0].to_string();
        if !is_valid_account_name(&service) {
            return ShellCommand::UnknownCommand(INVALID_ACCOUNT_NAME.to_string());
        }
        let key = parts[1].to_string();
        let unlock = parts
            .get(2)
            .copied()
            .map(|s| s.to_lowercase() == "unlock")
            .unwrap_or(false);
        return ShellCommand::AddCredential {
            service,
            credential_type: "api_key".to_string(),
            fields: vec![key],
            unlock,
        };
    }

    if let Some(args) = rest.strip_prefix("add-x ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 6 {
            return ShellCommand::UnknownCommand(
                "usage: /add-x <service> <api_key> <api_key_secret> <access_token> <access_token_secret> <bearer_or_->_ [unlock]".to_string(),
            );
        }
        let service = parts[0].to_string();
        if !is_valid_account_name(&service) {
            return ShellCommand::UnknownCommand(INVALID_ACCOUNT_NAME.to_string());
        }
        let api_key = parts[1].to_string();
        let api_key_secret = parts[2].to_string();
        let access_token = parts[3].to_string();
        let access_token_secret = parts[4].to_string();
        let bearer_token = parts[5].to_string();
        let unlock = parts
            .get(6)
            .copied()
            .map(|s| s.to_lowercase() == "unlock")
            .unwrap_or(false);
        return ShellCommand::AddCredential {
            service,
            credential_type: "x".to_string(),
            fields: vec![
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token,
            ],
            unlock,
        };
    }

    if rest == "remove-key" {
        return ShellCommand::UnknownCommand("usage: /remove-key <service>".to_string());
    }
    if let Some(name) = rest.strip_prefix("remove-key ") {
        let name = name.trim();
        if name.is_empty() {
            return ShellCommand::UnknownCommand("usage: /remove-key <service>".to_string());
        }
        if !is_valid_account_name(name) {
            return ShellCommand::UnknownCommand(INVALID_ACCOUNT_NAME.to_string());
        }
        return ShellCommand::RemoveCredential {
            service: name.to_string(),
        };
    }

    if rest == "lock" {
        return ShellCommand::Send(ClientMessage::Lock);
    }

    if rest == "undo" {
        return ShellCommand::Undo;
    }
    if rest == "redo" {
        return ShellCommand::Redo;
    }

    if rest == "image" {
        let request_id = *next_request_id;
        *next_request_id = next_request_id.wrapping_add(1);
        return ShellCommand::Send(ClientMessage::TestImage { request_id });
    }

    if let Some(effort_s) = rest.strip_prefix("reasoning ") {
        let effort_s = effort_s.trim();
        let effort = match effort_s {
            "off" => ThinkingEffort::Off,
            "low" => ThinkingEffort::Low,
            "medium" => ThinkingEffort::Medium,
            "high" => ThinkingEffort::High,
            _ => {
                return ShellCommand::UnknownCommand(format!(
                    "unknown reasoning effort '{effort_s}'. Usage: /reasoning off | low | medium | high"
                ));
            }
        };
        return ShellCommand::Send(ClientMessage::SetReasoningEffort { effort });
    }
    if rest == "reasoning" {
        return ShellCommand::Send(ClientMessage::GetReasoningEffort);
    }

    ShellCommand::UnknownCommand(format!("unknown command: /{rest}"))
}

pub fn shell_command_echo(command: &ShellCommand) -> Option<String> {
    match command {
        ShellCommand::Send(message) => match message {
            ClientMessage::RunInput { input, .. } => {
                Some(format!("> {}", String::from_utf8_lossy(input)))
            }
            ClientMessage::TestImage { .. } => Some("> /image".to_string()),
            ClientMessage::SetModel { model } => Some(format!("> set model: {model}")),
            ClientMessage::SetReasoningEffort { effort } => {
                Some(format!("> set reasoning effort: {}", effort.as_label()))
            }
            ClientMessage::GetReasoningEffort => Some("> get reasoning effort".to_string()),
            _ => None,
        },
        // Non-Send shell commands: echo the raw line so the user can see
        // what they typed even though no ClientMessage is sent.
        ShellCommand::Unlock { .. } => Some("> /unlock".to_string()),
        ShellCommand::AddCredential {
            service, unlock, ..
        } => {
            let suffix = if *unlock { " (with unlock)" } else { "" };
            Some(format!("> /add-key {service}{suffix}"))
        }
        ShellCommand::RemoveCredential { service } => Some(format!("> /remove-key {service}")),
        ShellCommand::Undo => Some("> undo".to_string()),
        ShellCommand::Redo => Some("> redo".to_string()),
        _ => None,
    }
}

pub use crate::history::StreamingText;
