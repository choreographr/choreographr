use tai_proto::{ClientMessage, OutputStream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Send(ClientMessage),
    InvalidCancel(String),
    Empty,
}

pub fn parse_input_line(line: &str, next_request_id: &mut u32) -> ShellCommand {
    let line = line.trim();
    if line.is_empty() {
        return ShellCommand::Empty;
    }

    if let Some(rest) = line.strip_prefix(":cancel ") {
        return match rest.trim().parse::<u32>() {
            Ok(request_id) => ShellCommand::Send(ClientMessage::Cancel { request_id }),
            Err(_) => ShellCommand::InvalidCancel(rest.trim().to_string()),
        };
    }

    if line == ":ping" {
        return ShellCommand::Send(ClientMessage::Ping);
    }

    if let Some(rest) = line.strip_prefix("/models") {
        let model = rest.trim();
        if model.is_empty() {
            return ShellCommand::Send(ClientMessage::ListModels);
        }
        return ShellCommand::Send(ClientMessage::SetModel {
            model: model.to_string(),
        });
    }

    let request_id = *next_request_id;
    *next_request_id = next_request_id.wrapping_add(1);
    ShellCommand::Send(ClientMessage::RunInput {
        request_id,
        input: line.as_bytes().to_vec(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_line() {
        let mut next = 1;
        assert_eq!(parse_input_line("   ", &mut next), ShellCommand::Empty);
        assert_eq!(next, 1);
    }

    #[test]
    fn parses_ping() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":ping", &mut next),
            ShellCommand::Send(ClientMessage::Ping)
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn parses_cancel() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":cancel 42", &mut next),
            ShellCommand::Send(ClientMessage::Cancel { request_id: 42 })
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn rejects_invalid_cancel() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":cancel nope", &mut next),
            ShellCommand::InvalidCancel("nope".to_string())
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn parses_models_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/models", &mut next),
            ShellCommand::Send(ClientMessage::ListModels)
        );
        assert_eq!(next, 10);
    }

    #[test]
    fn parses_set_model_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/models gpt-5.4-nano", &mut next),
            ShellCommand::Send(ClientMessage::SetModel {
                model: "gpt-5.4-nano".to_string(),
            })
        );
        assert_eq!(next, 10);
    }

    #[test]
    fn parses_run_input_and_increments_request_id() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("hello world", &mut next),
            ShellCommand::Send(ClientMessage::RunInput {
                request_id: 10,
                input: b"hello world".to_vec(),
            })
        );
        assert_eq!(next, 11);
    }

    #[test]
    fn streaming_text_appends_to_matching_stream() {
        let mut entry = StreamingText::new(7);
        entry.append(OutputStream::Reasoning, "thinking");
        entry.append(OutputStream::Answer, "hello");
        entry.append(OutputStream::Answer, " world");

        assert_eq!(entry.request_id, 7);
        assert_eq!(entry.reasoning, "thinking");
        assert_eq!(entry.answer, "hello world");
    }
}
