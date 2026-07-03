use super::*;
use std::io::Cursor;
use std::sync::Mutex;

static SOCKET_PATH_TEST_MUTEX: Mutex<()> = Mutex::new(());

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

#[test]
fn sync_read_write_round_trip() {
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

    let frame = encode_frame(&expected).expect("encode");
    let mut cursor = Cursor::new(&frame[..]);
    let actual = read_message_sync::<_, DaemonMessage>(&mut cursor).expect("read");
    assert_eq!(actual, expected);
}

#[test]
fn read_payload_rejects_oversized_frame() {
    let oversized_len = (MAX_FRAME_SIZE as u32) + 1;
    let mut cursor = Cursor::new(oversized_len.to_be_bytes().to_vec());
    let err = read_payload_sync(&mut cursor).expect_err("should fail");
    assert!(matches!(err, ProtoError::FrameTooLarge));
}

#[test]
fn socket_path_uses_env_override() {
    let _guard = SOCKET_PATH_TEST_MUTEX.lock().unwrap();
    let original = std::env::var(SOCKET_PATH_ENV).ok();
    unsafe {
        std::env::set_var(SOCKET_PATH_ENV, "/tmp/custom-tai.sock");
    }
    assert_eq!(socket_path(), "/tmp/custom-tai.sock");
    match original {
        Some(value) => unsafe { std::env::set_var(SOCKET_PATH_ENV, value) },
        None => unsafe { std::env::remove_var(SOCKET_PATH_ENV) },
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
