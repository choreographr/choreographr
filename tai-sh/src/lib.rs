use image::load_from_memory;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use std::io;
pub use tai_client_core::{ImageAssembler, ShellCommand, StreamingText, parse_input_line};
use tai_proto::ImageMetadata;
use tokio::sync::mpsc;

pub fn channel_closed<T>(_: mpsc::error::SendError<T>) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "connection writer closed")
}

pub struct RenderedImage {
    pub metadata: ImageMetadata,
    pub protocol: StatefulProtocol,
}

pub fn build_picker() -> Picker {
    Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
}

pub fn build_rendered_image(
    picker: &Picker,
    metadata: ImageMetadata,
    data: Vec<u8>,
) -> io::Result<RenderedImage> {
    let image = load_from_memory(&data).map_err(io::Error::other)?;
    let protocol = picker.new_resize_protocol(image);
    Ok(RenderedImage { metadata, protocol })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tai_proto::ClientMessage;

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
    fn parses_test_image_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/image", &mut next),
            ShellCommand::Send(ClientMessage::TestImage { request_id: 10 })
        );
        assert_eq!(next, 11);
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
    fn image_assembler_tracks_lifecycle() {
        let mut assembler = ImageAssembler::new();
        let metadata = ImageMetadata {
            image_id: 11,
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            byte_len: 4,
            alt: Some("tiny".to_string()),
        };

        assembler.start(7, metadata.clone()).expect("start");
        assembler.push_chunk(7, 11, &[1, 2]).expect("chunk1");
        assembler.push_chunk(7, 11, &[3, 4]).expect("chunk2");
        let (actual_metadata, data) = assembler.finish(7, 11).expect("finish");

        assert_eq!(actual_metadata, metadata);
        assert_eq!(data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn image_assembler_rejects_unknown_chunk() {
        let mut assembler = ImageAssembler::new();
        let error = assembler.push_chunk(1, 2, &[3]).expect_err("should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn image_assembler_rejects_duplicate_start() {
        let mut assembler = ImageAssembler::new();
        let metadata = ImageMetadata {
            image_id: 2,
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            byte_len: 1,
            alt: None,
        };

        assembler.start(1, metadata.clone()).expect("first start");
        let error = assembler.start(1, metadata).expect_err("should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn image_assembler_rejects_wrong_final_size() {
        let mut assembler = ImageAssembler::new();
        assembler
            .start(
                1,
                ImageMetadata {
                    image_id: 9,
                    mime_type: "image/png".to_string(),
                    width: 1,
                    height: 1,
                    byte_len: 3,
                    alt: None,
                },
            )
            .expect("start");
        assembler.push_chunk(1, 9, &[1, 2]).expect("chunk");

        let error = assembler.finish(1, 9).expect_err("should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn image_assembler_drop_request_clears_pending_images() {
        let mut assembler = ImageAssembler::new();
        assembler
            .start(
                4,
                ImageMetadata {
                    image_id: 7,
                    mime_type: "image/png".to_string(),
                    width: 1,
                    height: 1,
                    byte_len: 1,
                    alt: None,
                },
            )
            .expect("start");

        assembler.drop_request(4);
        let error = assembler.finish(4, 7).expect_err("should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn build_rendered_image_rejects_invalid_bytes() {
        let picker = Picker::halfblocks();
        let result = build_rendered_image(
            &picker,
            ImageMetadata {
                image_id: 1,
                mime_type: "image/png".to_string(),
                width: 1,
                height: 1,
                byte_len: 3,
                alt: None,
            },
            vec![1, 2, 3],
        );
        let error = match result {
            Ok(_) => panic!("should fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }
}
