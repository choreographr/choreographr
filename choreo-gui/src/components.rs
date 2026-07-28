use crate::client::{send_client_message, submit_input};
use crate::render::render_turn;
use crate::state::AppState;
use choreo_proto::ClientMessage;
use dioxus::prelude::*;

#[component]
pub(crate) fn Toolbar(
    state: Signal<AppState>,
    tx: Signal<Option<std::sync::mpsc::Sender<ClientMessage>>>,
) -> Element {
    let on_ping = {
        let t = tx;
        move |_| {
            let daemon_tx = t.read().clone();
            send_client_message(&mut state.write(), daemon_tx, ClientMessage::Ping)
        }
    };

    let on_models = {
        let t = tx;
        move |_| {
            let daemon_tx = t.read().clone();
            send_client_message(&mut state.write(), daemon_tx, ClientMessage::ListModels)
        }
    };

    let on_cancel = {
        let t = tx;
        move |_| {
            let request_id_text = state.read().pending_cancel.trim().to_string();
            if request_id_text.is_empty() {
                state
                    .write()
                    .status_texts
                    .push("[client] enter a request id to cancel".to_string());
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
                    .status_texts
                    .push(format!("invalid request id: {request_id_text}")),
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
    let guard = state.read();
    let turns: Vec<(u32, choreo_proto::Turn)> = guard
        .session_view
        .turns
        .iter()
        .map(|(&id, t)| (id, t.clone()))
        .collect();
    let status_texts = guard.status_texts.clone();
    drop(guard);

    rsx! {
        div { class: "history",
            for text in status_texts {
                div { class: "history-item text-item", key: "{text}", pre { "{text}" } }
            }
            for (turn_id, turn) in turns {
                {render_turn(turn_id, &turn)}
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
        let t = tx;
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
                    let mut os = on_submit;
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
                    let mut os = on_submit;
                    move |_| os()
                }, "Send" }
            }
        }
    }
}
