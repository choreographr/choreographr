use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tai_client_core::{
    ClientHistory, DaemonMessageHandler, HistoryItem as SharedHistoryItem, StreamingText,
};
use tai_proto::{ImageMetadata, OutputStream, SessionMessage};

pub(crate) type StreamingEntry = StreamingText;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisplayImage {
    pub(crate) metadata: ImageMetadata,
    pub(crate) data_url: String,
}

pub(crate) type HistoryItem = SharedHistoryItem<DisplayImage>;

#[derive(Debug, Clone)]
pub(crate) enum UiEvent {
    Daemon(tai_proto::DaemonMessage),
    ReaderClosed,
    ReaderFailed(String),
}

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) input: String,
    pub(crate) next_request_id: u32,
    pub(crate) client: ClientHistory<DisplayImage>,
    pub(crate) pending_cancel: String,
    pub(crate) attached_session_id: Option<u64>,
}

impl AppState {
    pub(crate) fn new(socket_path: String) -> Self {
        Self {
            input: String::new(),
            next_request_id: 1,
            client: ClientHistory::new(vec![HistoryItem::Text(format!(
                "Connected to tai-daemon at {socket_path}"
            ))]),
            pending_cancel: String::new(),
            attached_session_id: None,
        }
    }
}

impl DaemonMessageHandler for AppState {
    fn push_text(&mut self, text: String) {
        self.client.push_text(text);
    }

    fn push_tool_text(&mut self, request_id: u32, text: String) {
        self.client.insert_text_before_stream(request_id, text);
    }

    fn push_session_message(&mut self, message: SessionMessage) {
        // DisplayedImage is delivered post-turn as SessionMessageAppended
        // and should be converted to a renderable image item, same as the
        // TUI client does — the old ImageStart/Chunk/End streaming path
        // is no longer used.
        if let SessionMessage::DisplayedImage(record) = &message {
            self.client.push_image(DisplayImage {
                data_url: format!(
                    "data:{};base64,{}",
                    record.metadata.mime_type,
                    BASE64.encode(&record.data),
                ),
                metadata: record.metadata.clone(),
            });
        } else {
            self.client.push_session_message(message);
        }
    }

    fn insert_session_message_before_stream(&mut self, request_id: u32, message: SessionMessage) {
        // Unlike the TUI client, dioxus doesn't render diffs natively, so we
        // skip `try_parse_as_diff` and always store as SessionMessage.
        self.client
            .insert_before_stream(request_id, HistoryItem::SessionMessage(message));
    }

    fn begin_stream(&mut self, request_id: u32) {
        self.client.begin_stream(request_id);
    }

    fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str) {
        self.client.append_stream(request_id, stream, chunk);
    }

    fn finalize_stream(&mut self, request_id: u32) {
        self.client.finalize_stream(request_id);
    }

    fn drop_request(&mut self, request_id: u32) {
        self.client.drop_request(request_id);
    }
}
