use tai_client_core::ClientError;
use tai_proto::{ClientMessage, ImageMetadata};
use tai_tui::{ImageAssembler, ShellCommand, parse_input_line};

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

#[ignore]
#[test]
fn image_assembler_rejects_oversized_chunks() {
    let mut assembler = ImageAssembler::new();
    assembler
        .start(
            1,
            ImageMetadata {
                image_id: 3,
                mime_type: "image/png".to_string(),
                width: 1,
                height: 1,
                byte_len: 2,
                alt: None,
            },
        )
        .expect("start");

    let error = assembler
        .push_chunk(1, 3, &[1, 2, 3])
        .expect_err("should fail");
    assert!(matches!(error, ClientError::ImageExceedsSize { image_id: 3 }));
}
