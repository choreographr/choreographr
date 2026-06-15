use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use dioxus::desktop::{Config, WindowBuilder};
use dioxus::prelude::*;
use std::{collections::HashMap, io};
use tai_client_core::{ImageAssembler, ShellCommand, StreamingText, parse_input_line};
use tai_proto::{
    ClientMessage, DaemonMessage, ImageMetadata, OutputStream, read_message, socket_path,
    write_message,
};
use tokio::{
    net::UnixStream,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

type StreamingEntry = StreamingText;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayImage {
    metadata: ImageMetadata,
    data_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HistoryItem {
    Text(String),
    Streaming(StreamingEntry),
    Image(DisplayImage),
}

#[derive(Debug, Clone)]
enum UiEvent {
    Daemon(DaemonMessage),
    ReaderClosed,
    ReaderFailed(String),
    WriterFailed(String),
}

#[derive(Debug)]
struct AppState {
    input: String,
    next_request_id: u32,
    history: Vec<HistoryItem>,
    in_progress: HashMap<u32, usize>,
    pending_images: ImageAssembler,
    pending_cancel: String,
}

impl AppState {
    fn new(socket_path: String) -> Self {
        Self {
            input: String::new(),
            next_request_id: 1,
            history: vec![HistoryItem::Text(format!(
                "Connected to tai-daemon at {socket_path}"
            ))],
            in_progress: HashMap::new(),
            pending_images: ImageAssembler::new(),
            pending_cancel: String::new(),
        }
    }

    fn push_text(&mut self, text: impl Into<String>) {
        self.history.push(HistoryItem::Text(text.into()));
        self.trim_history();
    }

    fn begin_stream(&mut self, request_id: u32) {
        if self.in_progress.contains_key(&request_id) {
            return;
        }
        let index = self.history.len();
        self.history
            .push(HistoryItem::Streaming(StreamingEntry::new(request_id)));
        self.in_progress.insert(request_id, index);
        self.trim_history();
    }

    fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str) {
        if !self.in_progress.contains_key(&request_id) {
            self.begin_stream(request_id);
        }
        if let Some(&index) = self.in_progress.get(&request_id)
            && let Some(HistoryItem::Streaming(entry)) = self.history.get_mut(index)
        {
            entry.append(stream, chunk);
        }
    }

    fn finalize_stream(&mut self, request_id: u32) {
        self.in_progress.remove(&request_id);
        self.pending_images.drop_request(request_id);
    }

    fn push_image(&mut self, image: DisplayImage) {
        self.history.push(HistoryItem::Image(image));
        self.trim_history();
    }

    fn trim_history(&mut self) {
        if self.history.len() <= 500 {
            return;
        }
        let excess = self.history.len() - 500;
        self.history.drain(0..excess);
        for index in self.in_progress.values_mut() {
            *index = index.saturating_sub(excess);
        }
        self.in_progress
            .retain(|_, index| *index < self.history.len());
    }
}

fn main() {
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(WindowBuilder::new().with_title("tai-dioxus")))
        .launch(App);
}

#[component]
fn App() -> Element {
    let socket = use_signal(socket_path);
    let mut state = use_signal(|| AppState::new(socket.read().clone()));
    let daemon_tx = use_signal(|| None::<UnboundedSender<ClientMessage>>);
    let events_rx = use_signal(|| None::<UnboundedReceiver<UiEvent>>);

    use_hook({
        let socket = socket.read().clone();
        let mut daemon_tx = daemon_tx;
        let mut events_rx = events_rx;
        move || {
            let (client_tx, client_rx) = mpsc::unbounded_channel::<ClientMessage>();
            let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiEvent>();
            let _ = client_tx.send(ClientMessage::ListSessions);
            daemon_tx.set(Some(client_tx));
            events_rx.set(Some(ui_rx));
            tokio::spawn(async move {
                if let Err(error) = run_client(socket, client_rx, ui_tx.clone()).await {
                    let _ = ui_tx.send(UiEvent::ReaderFailed(error.to_string()));
                }
            });
        }
    });

    use_future({
        let mut events_rx = events_rx;
        let mut state = state;
        move || async move {
            loop {
                let event = {
                    let mut guard = events_rx.write();
                    let Some(rx) = guard.as_mut() else {
                        drop(guard);
                        tokio::task::yield_now().await;
                        continue;
                    };
                    rx.recv().await
                };

                let Some(event) = event else {
                    break;
                };

                match event {
                    UiEvent::Daemon(message) => {
                        let result = {
                            let mut app_state = state.write();
                            apply_daemon_message(&mut app_state, message, daemon_tx.read().clone())
                        };
                        if let Err(error) = result {
                            state.write().push_text(format!(
                                "[client] failed to process daemon message: {error}"
                            ));
                        }
                    }
                    UiEvent::ReaderClosed => {
                        state.write().push_text("daemon connection closed");
                    }
                    UiEvent::ReaderFailed(error) => {
                        state
                            .write()
                            .push_text(format!("[client] connection error: {error}"));
                    }
                    UiEvent::WriterFailed(error) => {
                        state
                            .write()
                            .push_text(format!("[client] write error: {error}"));
                    }
                }
            }
        }
    });

    let mut on_submit_keydown = {
        let mut state = state;
        let daemon_tx = daemon_tx;
        move || submit_input(&mut state, daemon_tx.read().clone())
    };

    let mut on_submit_click = {
        let mut state = state;
        let daemon_tx = daemon_tx;
        move || submit_input(&mut state, daemon_tx.read().clone())
    };

    let on_ping = {
        let mut state = state;
        let daemon_tx = daemon_tx;
        move |_| {
            send_client_message(
                &mut state.write(),
                daemon_tx.read().clone(),
                ClientMessage::Ping,
            )
        }
    };

    let on_models = {
        let mut state = state;
        let daemon_tx = daemon_tx;
        move |_| {
            send_client_message(
                &mut state.write(),
                daemon_tx.read().clone(),
                ClientMessage::ListModels,
            )
        }
    };

    let on_cancel = {
        let mut state = state;
        let daemon_tx = daemon_tx;
        move |_| {
            let request_id_text = state.read().pending_cancel.trim().to_string();
            if request_id_text.is_empty() {
                state
                    .write()
                    .push_text("[client] enter a request id to cancel");
                return;
            }
            match request_id_text.parse::<u32>() {
                Ok(request_id) => {
                    send_client_message(
                        &mut state.write(),
                        daemon_tx.read().clone(),
                        ClientMessage::Cancel { request_id },
                    );
                    state.write().pending_cancel.clear();
                }
                Err(_) => state
                    .write()
                    .push_text(format!("invalid request id: {request_id_text}")),
            }
        }
    };

    let history = state.read().history.clone();
    let input_value = state.read().input.clone();
    let cancel_value = state.read().pending_cancel.clone();

    rsx! {
        document::Style { {APP_CSS} }
        div { class: "app-shell",
            div { class: "toolbar",
                button { onclick: on_ping, "Ping" }
                button { onclick: on_models, "Models" }
                div { class: "cancel-row",
                    input {
                        placeholder: "Request id",
                        value: "{cancel_value}",
                        oninput: move |event| state.write().pending_cancel = event.value(),
                    }
                    button { onclick: on_cancel, "Cancel" }
                }
            }

            div { class: "history",
                for item in history {
                    match item {
                        HistoryItem::Text(text) => rsx! {
                            div { class: "history-item text-item",
                                pre { "{text}" }
                            }
                        },
                        HistoryItem::Streaming(entry) => rsx! {
                            div { class: "history-item stream-item",
                                div { class: "request-id", "[{entry.request_id}]" }
                                if !entry.reasoning.is_empty() {
                                    div { class: "stream-section reasoning",
                                        div { class: "label", "reasoning" }
                                        pre { "{entry.reasoning}" }
                                    }
                                }
                                if !entry.answer.is_empty() {
                                    div { class: "stream-section answer",
                                        div { class: "label", "answer" }
                                        pre { "{entry.answer}" }
                                    }
                                }
                            }
                        },
                        HistoryItem::Image(image) => rsx! {
                            div { class: "history-item image-item",
                                div { class: "image-meta",
                                    {format!(
                                        "image {} ({} {}x{})",
                                        image.metadata.image_id,
                                        image.metadata.mime_type,
                                        image.metadata.width,
                                        image.metadata.height
                                    )}
                                }
                                img {
                                    class: "history-image",
                                    src: image.data_url.clone(),
                                    alt: image
                                        .metadata
                                        .alt
                                        .clone()
                                        .unwrap_or_else(|| String::from("image"))
                                }
                            }
                        },
                    }
                }
            }

            div { class: "composer",
                textarea {
                    rows: "4",
                    placeholder: "Enter a prompt, /image, :ping, /models, /models <model>, or :cancel <id>",
                    value: "{input_value}",
                    oninput: move |event| state.write().input = event.value(),
                    onkeydown: move |event| {
                        if event.key() == Key::Enter && !event.modifiers().shift() {
                            event.prevent_default();
                            on_submit_keydown();
                        }
                    },
                }
                div { class: "composer-actions",
                    button { onclick: move |_| on_submit_click(), "Send" }
                }
            }
        }
    }
}

async fn run_client(
    socket_path: String,
    mut client_rx: UnboundedReceiver<ClientMessage>,
    ui_tx: UnboundedSender<UiEvent>,
) -> io::Result<()> {
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut reader, mut writer) = stream.into_split();

    let writer_ui_tx = ui_tx.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(message) = client_rx.recv().await {
            if let Err(error) = write_message(&mut writer, &message).await {
                let _ = writer_ui_tx.send(UiEvent::WriterFailed(error.to_string()));
                return Err(error);
            }
        }
        Ok::<(), io::Error>(())
    });

    loop {
        match read_message::<_, DaemonMessage>(&mut reader).await {
            Ok(message) => {
                if ui_tx.send(UiEvent::Daemon(message)).is_err() {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                let _ = ui_tx.send(UiEvent::ReaderClosed);
                break;
            }
            Err(error) => {
                writer_task.abort();
                match writer_task.await {
                    Ok(Ok(())) | Err(_) => {}
                    Ok(Err(writer_error)) => return Err(writer_error),
                }
                return Err(error);
            }
        }
    }

    match writer_task.await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(error)) => return Err(error),
    }

    Ok(())
}

fn submit_input(state: &mut Signal<AppState>, daemon_tx: Option<UnboundedSender<ClientMessage>>) {
    let line = state.read().input.trim().to_string();
    state.write().input.clear();
    let command = {
        let mut guard = state.write();
        parse_input_line(&line, &mut guard.next_request_id)
    };
    handle_shell_command(&mut state.write(), daemon_tx, command);
}

fn handle_shell_command(
    state: &mut AppState,
    daemon_tx: Option<UnboundedSender<ClientMessage>>,
    command: ShellCommand,
) {
    match command {
        ShellCommand::Empty => {}
        ShellCommand::InvalidCancel(value) => {
            state.push_text(format!("invalid request id: {value}"))
        }
        ShellCommand::Send(message) => send_client_message(state, daemon_tx, message),
    }
}

fn send_client_message(
    state: &mut AppState,
    daemon_tx: Option<UnboundedSender<ClientMessage>>,
    message: ClientMessage,
) {
    let Some(sender) = daemon_tx else {
        state.push_text("[client] not connected");
        return;
    };

    match &message {
        ClientMessage::RunInput { input, .. } => {
            state.push_text(format!("> {}", String::from_utf8_lossy(input)));
        }
        ClientMessage::TestImage { .. } => state.push_text("> /image"),
        _ => {}
    }

    if let Err(error) = sender.send(message) {
        state.push_text(format!("[client] failed to send command: {error}"));
    }
}

fn apply_daemon_message(
    state: &mut AppState,
    message: DaemonMessage,
    daemon_tx: Option<UnboundedSender<ClientMessage>>,
) -> io::Result<()> {
    match message {
        DaemonMessage::SessionCreated { session_id, title } => {
            let label = title.unwrap_or_else(|| "untitled".to_string());
            state.push_text(format!("[daemon] created session {session_id}: {label}"));
        }
        DaemonMessage::Sessions { sessions } => {
            if let Some(sender) = daemon_tx {
                let message = if let Some(session) = sessions.first() {
                    ClientMessage::AttachSession {
                        session_id: session.session_id,
                    }
                } else {
                    ClientMessage::CreateSession {
                        title: Some("default".to_string()),
                    }
                };
                let _ = sender.send(message);
            }
        }
        DaemonMessage::SessionAttached { session_id } => {
            state.push_text(format!("[daemon] attached session: {session_id}"));
        }
        DaemonMessage::SessionState {
            session_id,
            title,
            selected_model,
            messages,
        } => {
            let title = title.unwrap_or_else(|| "untitled".to_string());
            state.push_text(format!("[daemon] session {session_id}: {title}"));
            if let Some(model) = selected_model {
                state.push_text(format!("[daemon] selected model: {model}"));
            }
            for message in messages {
                state.push_text(message.render_line());
            }
        }
        DaemonMessage::SessionFailed { operation, error } => {
            state.push_text(format!("[daemon] {operation} failed: {error}"));
        }
        DaemonMessage::SessionMessageAppended { message } => {
            state.push_text(message.render_line());
        }
        DaemonMessage::Started { request_id } => {
            state.begin_stream(request_id);
        }
        DaemonMessage::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
        } => {
            state.push_text(format!("[{request_id}] tool {tool_name}#{call_id} start {arguments_json}"));
        }
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
            output,
        } => {
            state.push_text(format!("[{request_id}] tool {tool_name}#{call_id} ok: {output}"));
        }
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => {
            state.push_text(format!("[{request_id}] tool {tool_name}#{call_id} failed: {error}"));
        }
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        } => {
            let text = String::from_utf8(data)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            state.append_stream(request_id, stream, &text);
        }
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        } => {
            state.pending_images.start(request_id, metadata)?;
        }
        DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data,
        } => {
            state
                .pending_images
                .push_chunk(request_id, image_id, &data)?;
        }
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        } => {
            let (metadata, data) = state.pending_images.finish(request_id, image_id)?;
            state.push_image(DisplayImage {
                data_url: format!("data:{};base64,{}", metadata.mime_type, BASE64.encode(data)),
                metadata,
            });
        }
        DaemonMessage::Done { request_id } => {
            state.finalize_stream(request_id);
            state.push_text(format!("[{request_id}] done"));
        }
        DaemonMessage::Failed { request_id, error } => {
            state.finalize_stream(request_id);
            state.push_text(format!("[{request_id}] failed: {error}"));
        }
        DaemonMessage::Cancelled { request_id } => {
            state.finalize_stream(request_id);
            state.push_text(format!("[{request_id}] cancelled"));
        }
        DaemonMessage::Pong => state.push_text("[daemon] pong"),
        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if models.is_empty() {
                state.push_text("[daemon] no models available");
            } else {
                state.push_text(format!("[daemon] supported models ({})", models.len()));
                for model in models {
                    let prefix = if selected_model.as_deref() == Some(model.as_str()) {
                        "*"
                    } else {
                        "-"
                    };
                    state.push_text(format!("{prefix} {model}"));
                }
            }
        }
        DaemonMessage::ModelsFailed { error } => {
            state.push_text(format!("[daemon] models failed: {error}"));
        }
        DaemonMessage::ModelSelected { model } => {
            state.push_text(format!("[daemon] selected model: {model}"));
        }
        DaemonMessage::ModelSelectionFailed { model, error } => {
            state.push_text(format!("[daemon] failed to select model {model}: {error}"));
        }
    }
    Ok(())
}

const APP_CSS: &str = r#"
:root {
    color-scheme: dark;
    font-family: Inter, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}

html, body {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100%;
    background: #101217;
    color: #e6edf3;
}

body {
    overflow: hidden;
}

#main, .app-shell {
    width: 100%;
    height: 100%;
}

.app-shell {
    display: flex;
    flex-direction: column;
    gap: 12px;
    box-sizing: border-box;
    padding: 12px;
}

.toolbar {
    display: flex;
    gap: 8px;
    align-items: center;
}

.cancel-row {
    display: flex;
    gap: 8px;
    margin-left: auto;
}

.history {
    flex: 1;
    overflow-y: auto;
    border: 1px solid #2f3742;
    border-radius: 8px;
    background: #0b0d12;
    padding: 12px;
}

.history-item {
    margin-bottom: 10px;
}

.text-item pre,
.stream-section pre {
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
    font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
    line-height: 1.45;
}

.request-id {
    margin-bottom: 6px;
    color: #8b949e;
    font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
}

.image-meta {
    margin-bottom: 8px;
    color: #8b949e;
    font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
}

.history-image {
    display: block;
    max-width: min(100%, 640px);
    height: auto;
    border-radius: 8px;
    border: 1px solid #30363d;
}

.stream-section {
    margin-top: 6px;
}

.stream-section .label {
    margin-bottom: 4px;
    color: #7ee787;
    font-size: 12px;
    text-transform: lowercase;
    letter-spacing: 0.04em;
}

.reasoning .label {
    color: #79c0ff;
}

.composer {
    display: flex;
    flex-direction: column;
    gap: 8px;
}

.composer-actions {
    display: flex;
    justify-content: flex-end;
}

textarea,
input,
button {
    border-radius: 8px;
    border: 1px solid #30363d;
    background: #161b22;
    color: #e6edf3;
    padding: 10px 12px;
    font: inherit;
    box-sizing: border-box;
}

textarea,
input {
    outline: none;
}

textarea:focus,
input:focus {
    border-color: #58a6ff;
}

textarea {
    width: 100%;
    resize: vertical;
    min-height: 84px;
}

button {
    cursor: pointer;
    background: #1f6feb;
    border-color: #1f6feb;
}

button:hover {
    background: #388bfd;
}
"#;

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
    fn app_state_stream_updates_history() {
        let mut state = AppState::new("/tmp/tai.sock".to_string());
        state.begin_stream(7);
        state.append_stream(7, OutputStream::Reasoning, "thinking");
        state.append_stream(7, OutputStream::Answer, "hello");
        state.append_stream(7, OutputStream::Answer, " world");

        let index = state.in_progress[&7];
        match &state.history[index] {
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
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
            0x00, 0xB5, 0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xDA, 0x63, 0xFC, 0xFF, 0x1F, 0x00, 0x03, 0x03, 0x01, 0xFF, 0xA5, 0xC2, 0xB9, 0x81,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
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

        match state.history.last().expect("image history item") {
            HistoryItem::Image(image) => {
                assert_eq!(image.metadata, metadata);
                assert!(image.data_url.starts_with("data:image/png;base64,"));
            }
            other => panic!("expected image item, got {other:?}"),
        }
    }
}
