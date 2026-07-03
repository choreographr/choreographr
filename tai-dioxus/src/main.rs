mod client;
mod render;
mod state;

use crate::client::{
    apply_daemon_message, run_client, send_client_message, submit_input,
};
use crate::render::render_history_item;
use crate::state::{AppState, UiEvent};
use dioxus::desktop::{Config, WindowBuilder};
use dioxus::prelude::*;
use tai_proto::{ClientMessage, socket_path};
use tokio::sync::mpsc::{self, UnboundedReceiver};

fn main() {
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(WindowBuilder::new().with_title("tai-dioxus")))
        .launch(App);
}

#[component]
fn App() -> Element {
    let socket = use_signal(socket_path);
    let mut state = use_signal(|| AppState::new(socket.read().clone()));
    let mut daemon_tx = use_signal(|| None::<std::sync::mpsc::Sender<ClientMessage>>);
    let mut events_rx = use_signal(|| None::<UnboundedReceiver<UiEvent>>);

    use_hook(move || {
        let socket = socket.read().clone();
        let (client_tx, client_rx) = std::sync::mpsc::channel::<ClientMessage>();
        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        if let Err(e) = client_tx.send(ClientMessage::ListSessions) {
            eprintln!("[tai-dioxus] failed to send ListSessions: {e}");
        }
        daemon_tx.set(Some(client_tx));
        events_rx.set(Some(ui_rx));
        let reader_tx = ui_tx.clone();
        let handle = tokio::task::spawn_blocking(move || {
            if let Err(error) = run_client(socket, client_rx, reader_tx.clone()) {
                if let Err(e) = reader_tx.send(UiEvent::ReaderFailed(error.to_string())) {
                    eprintln!("[tai-dioxus] failed to send ReaderFailed: {e}");
                }
            }
        });
        let monitor_tx = ui_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle.await && e.is_panic() {
                if let Err(e) = monitor_tx.send(UiEvent::ReaderFailed(
                    "client reader task panicked".to_string(),
                )) {
                    eprintln!("[tai-dioxus] failed to send panic notification: {e}");
                }
            }
        });
    });

    let tx = daemon_tx.read().clone();

    use_future({
        let tx = tx.clone();
        move || {
            let tx = tx.clone();
            async move {
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
                            apply_daemon_message(&mut app_state, message, tx.clone())
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
                }
            }
        }
        }
    });

    let mut on_submit_keydown = { let t = tx.clone(); move || submit_input(&mut state, t.clone()) };

    let mut on_submit_click = { let t = tx.clone(); move || submit_input(&mut state, t.clone()) };

    let on_ping = {
        let t = tx.clone();
        move |_| {
            send_client_message(
                &mut state.write(),
                t.clone(),
                ClientMessage::Ping,
            )
        }
    };

    let on_models = {
        let t = tx.clone();
        move |_| {
            send_client_message(
                &mut state.write(),
                t.clone(),
                ClientMessage::ListModels,
            )
        }
    };

    let on_cancel = {
        let t = tx.clone();
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
                        t.clone(),
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

    let history = state.read().client.history.clone();
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
                    placeholder: "Enter a prompt, /image, /ping, /models, /models <model>, or /cancel <id>",
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

const APP_CSS: &str = include_str!("style.css");

#[cfg(test)]
mod app_tests;
