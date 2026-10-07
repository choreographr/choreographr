use choreo_proto::ClientMessageType;
use choreo_tui::{Command, parse_input_line};

// Ignored by default: part of the #[ignore] integration suite, exercised via
// `cargo test-integration` (it binds sockets and runs the full parser flow).
#[ignore = "integration test; run explicitly via nextest --ignored"]
#[test]
fn shell_parser_handles_full_command_flow() {
    assert_eq!(parse_input_line("   "), Command::Empty);
    assert_eq!(
        parse_input_line("/ping"),
        Command::Send(ClientMessageType::Ping)
    );
    // `/models` was removed from the unified command model: it is no longer a
    // known command (the bare `/model` opens the selector instead).
    assert!(matches!(
        parse_input_line("/models"),
        Command::UnknownCommand(_)
    ));
    // Bare `/model` opens the model selector; `/model <id>` sets the model.
    assert_eq!(parse_input_line("/model"), Command::OpenModelSelector);
    assert_eq!(
        parse_input_line("/model gpt-5.4-nano"),
        Command::Send(ClientMessageType::SetModel {
            model: "gpt-5.4-nano".to_string(),
        })
    );
    assert_eq!(
        parse_input_line("run this"),
        Command::Send(ClientMessageType::RunInput {
            input: b"run this".to_vec(),
        })
    );
    assert_eq!(
        parse_input_line("/cancel 1"),
        Command::Send(ClientMessageType::Cancel { stream_id: 1 })
    );
    assert_eq!(
        parse_input_line("/cancel nope"),
        Command::InvalidCancel("nope".to_string())
    );
}
