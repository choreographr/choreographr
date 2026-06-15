use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/tai.sock";
pub const SOCKET_PATH_ENV: &str = "TAI_SOCKET_PATH";
pub const MAX_IMAGE_CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientMessage {
    RunInput { request_id: u32, input: Vec<u8> },
    TestImage { request_id: u32 },
    Cancel { request_id: u32 },
    Ping,
    ListModels,
    SetModel { model: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutputStream {
    Answer,
    Reasoning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageMetadata {
    pub image_id: u32,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DaemonMessage {
    Started {
        request_id: u32,
    },
    OutputChunk {
        request_id: u32,
        stream: OutputStream,
        data: Vec<u8>,
    },
    ImageStart {
        request_id: u32,
        metadata: ImageMetadata,
    },
    ImageChunk {
        request_id: u32,
        image_id: u32,
        data: Vec<u8>,
    },
    ImageEnd {
        request_id: u32,
        image_id: u32,
    },
    Done {
        request_id: u32,
    },
    Failed {
        request_id: u32,
        error: String,
    },
    Cancelled {
        request_id: u32,
    },
    Pong,
    Models {
        models: Vec<String>,
        selected_model: Option<String>,
    },
    ModelsFailed {
        error: String,
    },
    ModelSelected {
        model: String,
    },
    ModelSelectionFailed {
        model: String,
        error: String,
    },
}

pub fn encode_frame<T: Serialize>(message: &T) -> io::Result<Vec<u8>> {
    let payload =
        bincode::serde::encode_to_vec((PROTOCOL_VERSION, message), bincode::config::standard())
            .map_err(io::Error::other)?;

    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }

    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame<T>(payload: &[u8]) -> io::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let ((version, message), consumed): ((u8, T), usize) =
        bincode::serde::decode_from_slice(payload, bincode::config::standard())
            .map_err(io::Error::other)?;

    if consumed != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes in frame",
        ));
    }

    if version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported protocol version: {version}"),
        ));
    }

    Ok(message)
}

pub fn socket_path() -> String {
    std::env::var(SOCKET_PATH_ENV).unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_string())
}

pub async fn write_message<W, T>(writer: &mut W, message: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let frame = encode_frame(message)?;
    writer.write_all(&frame).await
}

pub async fn read_message<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let payload = read_payload(reader).await?;
    decode_frame(&payload)
}

pub async fn read_payload<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0_u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }

    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[test]
    fn encode_decode_round_trip_client_message() {
        let message = ClientMessage::RunInput {
            request_id: 42,
            input: b"hello".to_vec(),
        };
        let frame = encode_frame(&message).expect("encode");
        let decoded = decode_frame::<ClientMessage>(&frame[4..]).expect("decode");
        assert_eq!(decoded, message);
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let message = ClientMessage::Ping;
        let mut frame = encode_frame(&message).expect("encode");
        frame.extend_from_slice(&[1, 2, 3]);
        let err = decode_frame::<ClientMessage>(&frame[4..]).expect_err("should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_rejects_wrong_version() {
        let payload = bincode::serde::encode_to_vec(
            (PROTOCOL_VERSION + 1, ClientMessage::Ping),
            bincode::config::standard(),
        )
        .expect("encode");
        let err = decode_frame::<ClientMessage>(&payload).expect_err("should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn async_read_write_round_trip() {
        let (mut writer, mut reader) = duplex(1024);
        let expected = DaemonMessage::ImageStart {
            request_id: 5,
            metadata: ImageMetadata {
                image_id: 1,
                mime_type: "image/png".to_string(),
                width: 1,
                height: 1,
                byte_len: 4,
                alt: Some("chunk".to_string()),
            },
        };

        let expected_for_writer = expected.clone();
        let write_task =
            tokio::spawn(async move { write_message(&mut writer, &expected_for_writer).await });
        let actual = read_message::<_, DaemonMessage>(&mut reader)
            .await
            .expect("read");
        write_task.await.expect("join").expect("write");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn read_payload_rejects_oversized_frame() {
        let (mut writer, mut reader) = duplex(16);
        let write_task = tokio::spawn(async move {
            writer
                .write_all(&((MAX_FRAME_SIZE as u32) + 1).to_be_bytes())
                .await
                .expect("write header");
        });

        let err = read_payload(&mut reader).await.expect_err("should fail");
        write_task.await.expect("join");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn socket_path_uses_env_override() {
        unsafe {
            std::env::set_var(SOCKET_PATH_ENV, "/tmp/custom-tai.sock");
        }
        assert_eq!(socket_path(), "/tmp/custom-tai.sock");
        unsafe {
            std::env::remove_var(SOCKET_PATH_ENV);
        }
    }

    #[test]
    fn encode_rejects_oversized_message() {
        let message = ClientMessage::RunInput {
            request_id: 1,
            input: vec![0; MAX_FRAME_SIZE],
        };
        let err = encode_frame(&message).expect_err("should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
