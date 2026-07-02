use super::*;
use crate::state::HistoryItem;
use tai_proto::{DaemonMessage, ImageMetadata, OutputStream};

#[test]
fn app_state_stream_updates_history() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    state.begin_stream(7);
    state.append_stream(7, OutputStream::Reasoning, "thinking");
    state.append_stream(7, OutputStream::Answer, "hello");
    state.append_stream(7, OutputStream::Answer, " world");

    let index = state.client.in_progress[&7];
    match &state.client.history[index] {
        HistoryItem::Streaming(entry) => {
            assert_eq!(entry.request_id, 7);
            assert_eq!(entry.reasoning, "thinking");
            assert_eq!(entry.answer, "hello world");
        }
        _ => panic!("expected streaming entry"),
    }
}

#[test]
fn apply_daemon_image_messages_pushes_renderable_image() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    let metadata = ImageMetadata {
        image_id: 5,
        mime_type: "image/png".to_string(),
        width: 1,
        height: 1,
        byte_len: 68,
        alt: Some("tiny".to_string()),
    };
    let png = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xB5,
        0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xFC,
        0xFF, 0x1F, 0x00, 0x03, 0x03, 0x01, 0xFF, 0xA5, 0xC2, 0xB9, 0x81, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    apply_daemon_message(
        &mut state,
        DaemonMessage::ImageStart {
            request_id: 7,
            metadata: metadata.clone(),
        },
        None,
    )
    .expect("start");
    apply_daemon_message(
        &mut state,
        DaemonMessage::ImageChunk {
            request_id: 7,
            image_id: 5,
            data: png,
        },
        None,
    )
    .expect("chunk");
    apply_daemon_message(
        &mut state,
        DaemonMessage::ImageEnd {
            request_id: 7,
            image_id: 5,
        },
        None,
    )
    .expect("end");

    match state.client.history.last().expect("image history item") {
        HistoryItem::Image(image) => {
            assert_eq!(image.metadata, metadata);
            assert!(image.data_url.starts_with("data:image/png;base64,"));
        }
        other => panic!("expected image item, got {other:?}"),
    }
}
