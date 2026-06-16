mod client;
mod render;
mod state;

use crate::client::{apply_daemon_message, initial_socket_path, run_client, send_client_message, submit_input};
use crate::render::render_history_item;
use crate::state::{AppState, UiEvent};
use dioxus::desktop::{Config, WindowBuilder};
use dioxus::prelude::*;
use tai_proto::ClientMessage;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

fn main() {
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(WindowBuilder::new().with_title("tai-dioxus")))
        .launch(App);
}

#[component]
fn App() -> Element {
    let socket = use_signal(initial_socket_path);
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
        move || submit_input(&mut state, daemon_tx.read().clone())
    };

    let mut on_submit_click = {
        let mut state = state;
        move || submit_input(&mut state, daemon_tx.read().clone())
    };

    let on_ping = {
        let mut state = state;
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
                    {render_history_item(item)}
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

const APP_CSS: &str = r#"
:root {
    color-scheme: dark;
    font-family: Inter, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", sans-serif;
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
.stream-section pre,
.plain-body {
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
    font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", monospace;
    line-height: 1.45;
}

.message-label {
    margin-bottom: 6px;
    color: #8b949e;
    font-size: 12px;
    text-transform: lowercase;
    letter-spacing: 0.04em;
}

.markdown-body {
    line-height: 1.55;
}

.markdown-body p,
.markdown-body pre,
.markdown-body ul,
.markdown-body ol,
.markdown-body blockquote,
.markdown-body h1,
.markdown-body h2,
.markdown-body h3,
.markdown-body h4,
.markdown-body h5,
.markdown-body h6 {
    margin: 0 0 0.75em;
}

.markdown-body p:last-child,
.markdown-body pre:last-child,
.markdown-body ul:last-child,
.markdown-body ol:last-child,
.markdown-body blockquote:last-child,
.markdown-body h1:last-child,
.markdown-body h2:last-child,
.markdown-body h3:last-child,
.markdown-body h4:last-child,
.markdown-body h5:last-child,
.markdown-body h6:last-child {
    margin-bottom: 0;
}

.markdown-body code,
.markdown-body pre {
    font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", monospace;
}

.markdown-body code {
    background: #161b22;
    border: 1px solid #30363d;
    border-radius: 6px;
    padding: 0.1em 0.35em;
}

.markdown-body pre {
    background: #161b22;
    border: 1px solid #30363d;
    border-radius: 8px;
    padding: 12px;
    overflow-x: auto;
}

.markdown-body pre code {
    background: transparent;
    border: 0;
    padding: 0;
}

.markdown-body blockquote {
    border-left: 3px solid #30363d;
    padding-left: 12px;
    color: #a5b3c2;
}

.markdown-body a {
    color: #58a6ff;
    text-decoration: none;
}

.markdown-body a:hover {
    text-decoration: underline;
}

.request-id {
    margin-bottom: 6px;
    color: #8b949e;
    font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", monospace;
}

.image-meta {
    margin-bottom: 8px;
    color: #8b949e;
    font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", monospace;
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
    use crate::state::HistoryItem;
    use tai_client_core::{ShellCommand, parse_input_line};
    use tai_proto::{DaemonMessage, ImageMetadata, OutputStream};

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
