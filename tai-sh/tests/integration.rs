use tai_proto::ClientMessage;
use tai_sh::{parse_input_line, ShellCommand};

#[test]
fn shell_parser_handles_full_command_flow() {
    let mut next_request_id = 1;

    assert_eq!(parse_input_line("   ", &mut next_request_id), ShellCommand::Empty);
    assert_eq!(
        parse_input_line(":ping", &mut next_request_id),
        ShellCommand::Send(ClientMessage::Ping)
    );
    assert_eq!(
        parse_input_line("run this", &mut next_request_id),
        ShellCommand::Send(ClientMessage::RunInput {
            request_id: 1,
            input: b"run this".to_vec(),
        })
    );
    assert_eq!(
        parse_input_line(":cancel 1", &mut next_request_id),
        ShellCommand::Send(ClientMessage::Cancel { request_id: 1 })
    );
    assert_eq!(
        parse_input_line(":cancel nope", &mut next_request_id),
        ShellCommand::InvalidCancel("nope".to_string())
    );
    assert_eq!(next_request_id, 2);
}
