use tai_client_core::{ClientHistory, HistoryItem as SharedHistoryItem, StreamingText};

use tai_proto::ImageMetadata;

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
    WriterFailed(String),
}

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) input: String,
    pub(crate) next_request_id: u32,
    pub(crate) client: ClientHistory<DisplayImage>,
    pub(crate) pending_cancel: String,
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
        }
    }

    pub(crate) fn push_text(&mut self, text: impl Into<String>) {
        self.client.push_text(text);
    }

    pub(crate) fn push_session_message(&mut self, message: tai_proto::SessionMessage) {
        self.client.push_session_message(message);
    }

    pub(crate) fn begin_stream(&mut self, request_id: u32) {
        self.client.begin_stream(request_id);
    }

    pub(crate) fn append_stream(
        &mut self,
        request_id: u32,
        stream: tai_proto::OutputStream,
        chunk: &str,
    ) {
        self.client.append_stream(request_id, stream, chunk);
    }

    pub(crate) fn finalize_stream(&mut self, request_id: u32) {
        self.client.finalize_stream(request_id);
    }

    pub(crate) fn push_image(&mut self, image: DisplayImage) {
        self.client.push_image(image);
    }
}
