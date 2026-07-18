use tai_proto::ClientMessage;
use tai_tui::{ShellCommand, parse_input_line};

#[ignore]
#[test]
fn shell_parser_handles_full_command_flow() {
    let mut next_request_id = 1;

    assert_eq!(
        parse_input_line("   ", &mut next_request_id, None),
        ShellCommand::Empty
    );
    assert_eq!(
        parse_input_line("/ping", &mut next_request_id, None),
        ShellCommand::Send(ClientMessage::Ping)
    );
    assert_eq!(
        parse_input_line("/models", &mut next_request_id, None),
        ShellCommand::Send(ClientMessage::ListModels)
    );
    assert_eq!(
        parse_input_line("/models gpt-5.4-nano", &mut next_request_id, None),
        ShellCommand::Send(ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        })
    );
    assert_eq!(
        parse_input_line("run this", &mut next_request_id, None),
        ShellCommand::Send(ClientMessage::RunInput {
            request_id: 1,
            input: b"run this".to_vec(),
        })
    );
    assert_eq!(
        parse_input_line("/cancel 1", &mut next_request_id, None),
        ShellCommand::Send(ClientMessage::Cancel { request_id: 1 })
    );
    assert_eq!(
        parse_input_line("/cancel nope", &mut next_request_id, None),
        ShellCommand::InvalidCancel("nope".to_string())
    );
    assert_eq!(next_request_id, 2);
}
