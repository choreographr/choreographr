use crate::client::{send_client_message, submit_input};
use crate::render::render_history_item;
use crate::state::AppState;
use dioxus::prelude::*;
use tai_proto::ClientMessage;

#[component]
pub(crate) fn Toolbar(
    state: Signal<AppState>,
    tx: Signal<Option<std::sync::mpsc::Sender<ClientMessage>>>,
) -> Element {
    let on_ping = {
        let t = tx.clone();
        move |_| {
            let daemon_tx = t.read().clone();
            send_client_message(&mut state.write(), daemon_tx, ClientMessage::Ping)
        }
    };

    let on_models = {
        let t = tx.clone();
        move |_| {
            let daemon_tx = t.read().clone();
            send_client_message(&mut state.write(), daemon_tx, ClientMessage::ListModels)
        }
    };

    let on_cancel = {
        let t = tx.clone();
        move |_| {
            let request_id_text = state.read().pending_cancel.trim().to_string();
            if request_id_text.is_empty() {
                state
                    .write()
                    .client
                    .push_text("[client] enter a request id to cancel");
                return;
            }
            match request_id_text.parse::<u32>() {
                Ok(request_id) => {
                    // Single write scope for the success path.
                    let mut guard = state.write();
                    let daemon_tx = t.read().clone();
                    send_client_message(
                        &mut guard,
                        daemon_tx,
                        ClientMessage::Cancel { request_id },
                    );
                    guard.pending_cancel.clear();
                }
                Err(_) => state
                    .write()
                    .client
                    .push_text(format!("invalid request id: {request_id_text}")),
            }
        }
    };

    let cancel_value = state.read().pending_cancel.clone();

    rsx! {
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
    }
}

#[component]
pub(crate) fn HistoryList(state: Signal<AppState>) -> Element {
    let history = state.read().client.history.clone();

    rsx! {
        div { class: "history",
            for item in history {
                {render_history_item(item)}
            }
        }
    }
}

#[component]
pub(crate) fn Composer(
    state: Signal<AppState>,
    tx: Signal<Option<std::sync::mpsc::Sender<ClientMessage>>>,
) -> Element {
    let on_submit = {
        let t = tx.clone();
        move || submit_input(&mut state, t.read().clone())
    };

    let input_value = state.read().input.clone();

    rsx! {
        div { class: "composer",
            textarea {
                rows: "4",
                placeholder: "Enter a prompt, /image, /ping, /models, /models <model>, or /cancel <id>",
                value: "{input_value}",
                oninput: move |event| state.write().input = event.value(),
                onkeydown: {
                    let mut os = on_submit.clone();
                    move |event| {
                        if event.key() == Key::Enter && !event.modifiers().shift() {
                            event.prevent_default();
                            os();
                        }
                    }
                },
            }
            div { class: "composer-actions",
                button { onclick: {
                    let mut os = on_submit.clone();
                    move |_| os()
                }, "Send" }
            }
        }
    }
}
