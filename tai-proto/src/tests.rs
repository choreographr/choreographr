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
    assert!(matches!(err, ProtoError::TrailingBytes));
}

#[test]
fn decode_rejects_wrong_version() {
    let payload = bincode::serde::encode_to_vec(
        (PROTOCOL_VERSION + 1, ClientMessage::Ping),
        bincode::config::standard(),
    )
    .expect("encode");
    let err = decode_frame::<ClientMessage>(&payload).expect_err("should fail");
    assert!(matches!(err, ProtoError::UnsupportedVersion { .. }));
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
    assert!(matches!(err, ProtoError::FrameTooLarge));
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
    assert!(matches!(err, ProtoError::FrameTooLarge));
}
