use image::load_from_memory;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use std::{collections::HashMap, io};
use tai_proto::{ClientMessage, ImageMetadata};
use tokio::sync::mpsc;

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

pub fn channel_closed<T>(_: mpsc::error::SendError<T>) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "connection writer closed")
}

#[derive(Debug, Clone)]
pub struct PendingImage {
    metadata: ImageMetadata,
    data: Vec<u8>,
}

impl PendingImage {
    fn new(metadata: ImageMetadata) -> io::Result<Self> {
        let capacity = usize::try_from(metadata.byte_len).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "image byte length does not fit in memory")
        })?;
        Ok(Self {
            metadata,
            data: Vec::with_capacity(capacity),
        })
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        let expected = usize::try_from(self.metadata.byte_len).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "image byte length does not fit in memory")
        })?;
        let next_len = self.data.len().saturating_add(chunk.len());
        if next_len > expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("image {} exceeded advertised size", self.metadata.image_id),
            ));
        }
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    fn into_parts(self) -> (ImageMetadata, Vec<u8>) {
        (self.metadata, self.data)
    }
}

pub struct ImageAssembler {
    pending: HashMap<(u32, u32), PendingImage>,
}

impl ImageAssembler {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    pub fn start(&mut self, request_id: u32, metadata: ImageMetadata) -> io::Result<()> {
        let key = (request_id, metadata.image_id);
        if self.pending.contains_key(&key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("image {} for request {} already exists", metadata.image_id, request_id),
            ));
        }
        self.pending.insert(key, PendingImage::new(metadata)?);
        Ok(())
    }

    pub fn push_chunk(&mut self, request_id: u32, image_id: u32, data: &[u8]) -> io::Result<()> {
        let pending = self.pending.get_mut(&(request_id, image_id)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("received image chunk for unknown image {image_id} request {request_id}"),
            )
        })?;
        pending.push_chunk(data)
    }

    pub fn finish(&mut self, request_id: u32, image_id: u32) -> io::Result<(ImageMetadata, Vec<u8>)> {
        let pending = self.pending.remove(&(request_id, image_id)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("received image end for unknown image {image_id} request {request_id}"),
            )
        })?;
        let (metadata, data) = pending.into_parts();
        let actual_len = u64::try_from(data.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "image size does not fit in u64")
        })?;
        if actual_len != metadata.byte_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "image {} for request {} ended with {} bytes but expected {}",
                    image_id, request_id, actual_len, metadata.byte_len
                ),
            ));
        }
        Ok((metadata, data))
    }

    pub fn drop_request(&mut self, request_id: u32) {
        self.pending.retain(|(pending_request_id, _), _| *pending_request_id != request_id);
    }
}

pub struct RenderedImage {
    pub metadata: ImageMetadata,
    pub protocol: StatefulProtocol,
}

pub fn build_picker() -> Picker {
    Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
}

pub fn build_rendered_image(picker: &Picker, metadata: ImageMetadata, data: Vec<u8>) -> io::Result<RenderedImage> {
    let image = load_from_memory(&data).map_err(io::Error::other)?;
    let protocol = picker.new_resize_protocol(image);
    Ok(RenderedImage { metadata, protocol })
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
        assert_eq!(parse_input_line(":ping", &mut next), ShellCommand::Send(ClientMessage::Ping));
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
