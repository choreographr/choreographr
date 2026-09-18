use choreo_proto::ClientMessage;
use choreo_tui::{Command, parse_input_line};

// Ignored by default: part of the #[ignore] integration suite, exercised via
// `cargo test-integration` (it binds sockets and runs the full parser flow).
#[ignore = "integration test; run explicitly via nextest --ignored"]
#[test]
fn shell_parser_handles_full_command_flow() {
    let mut next_request_id = 1;

    assert_eq!(
        parse_input_line("   ", &mut next_request_id, None),
        Command::Empty
    );
    assert_eq!(
        parse_input_line("/ping", &mut next_request_id, None),
        Command::Send(ClientMessage::Ping)
    );
    // `/models` was removed from the unified command model: it is no longer a
    // known command (the bare `/model` opens the selector instead).
    assert!(matches!(
        parse_input_line("/models", &mut next_request_id, None),
        Command::UnknownCommand(_)
    ));
    // Bare `/model` opens the model selector; `/model <id>` sets the model.
    assert_eq!(
        parse_input_line("/model", &mut next_request_id, None),
        Command::OpenModelSelector
    );
    assert_eq!(
        parse_input_line("/model gpt-5.4-nano", &mut next_request_id, None),
        Command::Send(ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        })
    );
    assert_eq!(
        parse_input_line("run this", &mut next_request_id, None),
        Command::Send(ClientMessage::RunInput {
            request_id: 1,
            input: b"run this".to_vec(),
        })
    );
    assert_eq!(
        parse_input_line("/cancel 1", &mut next_request_id, None),
        Command::Send(ClientMessage::Cancel { request_id: 1 })
    );
    assert_eq!(
        parse_input_line("/cancel nope", &mut next_request_id, None),
        Command::InvalidCancel("nope".to_string())
    );
    assert_eq!(next_request_id, 2);
}
