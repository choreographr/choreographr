use tai_proto::ClientMessage;

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
    InvalidCancel(String),
    UnknownCommand(String),
    Empty,
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
        return parse_command(rest, next_request_id, attached_session_id);
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
    if let Some(service) = rest.strip_prefix("remove-key ") {
        let service = service.trim();
        if service.is_empty() {
            return ShellCommand::UnknownCommand("usage: /remove-key <service>".to_string());
        }
        return ShellCommand::RemoveCredential {
            service: service.to_string(),
        };
    }

    if rest == "lock" {
        return ShellCommand::Send(ClientMessage::Lock);
    }

    if rest == "image" {
        let request_id = *next_request_id;
        *next_request_id = next_request_id.wrapping_add(1);
        return ShellCommand::Send(ClientMessage::TestImage { request_id });
    }

    if let Some(sub) = rest.strip_prefix("session ") {
        let sub = sub.trim();
        if let Some(session_id) = sub.strip_prefix("switch ") {
            return match session_id.trim().parse::<u64>() {
                Ok(id) => ShellCommand::Send(ClientMessage::AttachSession { session_id: id }),
                Err(_) => ShellCommand::UnknownCommand("usage: /session switch <id>".to_string()),
            };
        }
        if let Some(session_id) = sub.strip_prefix("info ") {
            return match session_id.trim().parse::<u64>() {
                Ok(id) => ShellCommand::Send(ClientMessage::GetSessionState { session_id: id }),
                Err(_) => ShellCommand::UnknownCommand("usage: /session info <id>".to_string()),
            };
        }
        if let Some(title) = sub.strip_prefix("new ") {
            let title = title.trim();
            let title = if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            };
            return ShellCommand::Send(ClientMessage::CreateSession {
                title,
                parent_session_id: None,
                cwd: None,
                max_turns: None,
            });
        }
        if sub == "new" {
            return ShellCommand::Send(ClientMessage::CreateSession {
                title: None,
                parent_session_id: None,
                cwd: None,
                max_turns: None,
            });
        }
        if sub == "list" {
            return ShellCommand::Send(ClientMessage::ListSessions);
        }
        return ShellCommand::UnknownCommand(
            "session subcommands: list, new [title], switch <id>, info <id>".to_string(),
        );
    }

    if rest == "session" {
        return match attached_session_id {
            Some(id) => ShellCommand::Send(ClientMessage::GetSessionState { session_id: id }),
            None => ShellCommand::UnknownCommand(
                "no session attached. use /session switch <id> to attach".to_string(),
            ),
        };
    }

    if let Some(model) = rest.strip_prefix("models ") {
        let model = model.trim();
        if model.is_empty() {
            return ShellCommand::Send(ClientMessage::ListModels);
        }
        return ShellCommand::Send(ClientMessage::SetModel {
            model: model.to_string(),
        });
    }

    if rest == "models" {
        return ShellCommand::Send(ClientMessage::ListModels);
    }

    if let Some(model) = rest.strip_prefix("model ") {
        let model = model.trim();
        if model.is_empty() {
            return ShellCommand::Send(ClientMessage::ListModels);
        }
        return ShellCommand::Send(ClientMessage::SetModel {
            model: model.to_string(),
        });
    }

    if rest == "model" {
        return ShellCommand::Send(ClientMessage::ListModels);
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
            _ => None,
        },
        _ => None,
    }
}

pub use crate::history::StreamingText;
