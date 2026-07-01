use tai_proto::{ClientMessage, OutputStream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Send(ClientMessage),
    InvalidCancel(String),
    UnknownCommand(String),
    Empty,
}

pub fn parse_input_line(line: &str, next_request_id: &mut u32) -> ShellCommand {
    let line = line.trim();
    if line.is_empty() {
        return ShellCommand::Empty;
    }

    if let Some(rest) = line.strip_prefix('/') {
        return parse_command(rest, next_request_id);
    }

    let request_id = *next_request_id;
    *next_request_id = next_request_id.wrapping_add(1);
    ShellCommand::Send(ClientMessage::RunInput {
        request_id,
        input: line.as_bytes().to_vec(),
    })
}

fn parse_command(rest: &str, next_request_id: &mut u32) -> ShellCommand {
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
        return ShellCommand::Send(ClientMessage::Unlock {
            passphrase: passphrase.trim().to_string(),
        });
    }

    if rest == "unlock" {
        return ShellCommand::UnknownCommand(
            "usage: /unlock <passphrase>".to_string(),
        );
    }

    if rest == "lock" {
        return ShellCommand::Send(ClientMessage::Lock);
    }

    if let Some(args) = rest.strip_prefix("add-key ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 3 {
            return ShellCommand::UnknownCommand(
                "usage: /add-key <service> <api_key> <passphrase>".to_string(),
            );
        }
        let service = parts[0].to_string();
        let key = parts[1].to_string();
        let passphrase = parts[2..].join(" ");
        return ShellCommand::Send(ClientMessage::AddApiKey { service, passphrase, key });
    }

    if let Some(args) = rest.strip_prefix("add-x ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 7 {
            return ShellCommand::UnknownCommand(
                "usage: /add-x <service> <api_key> <api_key_secret> <access_token> <access_token_secret> <bearer_or_->_ <passphrase>".to_string(),
            );
        }
        let service = parts[0].to_string();
        let api_key = parts[1].to_string();
        let api_key_secret = parts[2].to_string();
        let access_token = parts[3].to_string();
        let access_token_secret = parts[4].to_string();
        let bearer_token = if parts[5] == "-" {
            None
        } else {
            Some(parts[5].to_string())
        };
        let passphrase = parts[6..].join(" ");
        return ShellCommand::Send(ClientMessage::AddXCredential {
            service,
            passphrase,
            api_key,
            api_key_secret,
            access_token,
            access_token_secret,
            bearer_token,
        });
    }

    if let Some(args) = rest.strip_prefix("remove-key ") {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 2 {
            return ShellCommand::UnknownCommand(
                "usage: /remove-key <service> <passphrase>".to_string(),
            );
        }
        let service = parts[0].to_string();
        let passphrase = parts[1..].join(" ");
        return ShellCommand::Send(ClientMessage::RemoveCredential { service, passphrase });
    }

    if rest == "image" {
        let request_id = *next_request_id;
        *next_request_id = next_request_id.wrapping_add(1);
        return ShellCommand::Send(ClientMessage::TestImage { request_id });
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingText {
    pub request_id: u32,
    pub reasoning: String,
    pub answer: String,
}

impl StreamingText {
    pub fn new(request_id: u32) -> Self {
        Self {
            request_id,
            reasoning: String::new(),
            answer: String::new(),
        }
    }

    pub fn append(&mut self, stream: OutputStream, chunk: &str) {
        match stream {
            OutputStream::Answer => self.answer.push_str(chunk),
            OutputStream::Reasoning => self.reasoning.push_str(chunk),
        }
    }
}
