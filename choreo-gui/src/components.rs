// This module is pure dioxus RSX. The `"{expr}"` text/attribute interpolation
// syntax is idiomatic dioxus and is NOT Rust `format!` — but the rustc macro
// expansion looks like a zero-argument format string, so `clippy::useless_format`
// fires on every interp. clippy's suggested `.to_string()` rewrite is wrong for
// dioxus (it fails to parse and defeats the reactive interpolation), so allow the
// false positive here.
#![allow(clippy::useless_format)]
use crate::client::{send_client_message, submit_input};
use crate::render::render_turn;
#[cfg(target_os = "ios")]
use crate::settings;
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
            // The on-device tools toggle exists ONLY on iOS (the setting
            // gates the Swift bridge there); the desktop/Android build gets
            // an empty component so the rsx call site compiles unchanged.
            OnDeviceToolsToggle { state }
        }
    }
}

/// iOS toolbar toggle for the persisted "on-device tools" setting
/// (`gui-settings.toml`). The bridge is handed to `DaemonState::open` during
/// startup, so the label and the confirmation status BOTH state that a
/// change applies on the next app start — toggling mid-session cannot
/// re-register the protected tool group live.
#[component]
#[cfg(target_os = "ios")]
fn OnDeviceToolsToggle(state: Signal<AppState>) -> Element {
    let label = if settings::on_device_tools_cached() {
        "ON"
    } else {
        "OFF"
    };

    let on_toggle = move |_| match settings::toggle_on_device_tools() {
        Ok(enabled) => {
            // The status push is also the rerender trigger: the label above
            // reads the cache atomic, which is re-evaluated on rerender.
            let verb = if enabled { "enabled" } else { "disabled" };
            state.write().status_texts.push(format!(
                "[settings] on-device tools {verb} — takes effect after restarting the app"
            ));
        }
        Err(error) => {
            tracing::warn!(error = %error, "on-device tools toggle persist failed");
            state.write().status_texts.push(format!(
                "[settings] could not save on-device tools setting: {error}"
            ));
        }
    };

    rsx! {
        div { class: "settings-row",
            button { onclick: on_toggle, "On-device tools: {label} (next app start)" }
        }
    }
}

/// Desktop/Android fallback: the setting is iOS-only (the bridge exists only
/// there), so the toolbar slot renders nothing — one call site, no cfg in
/// the parent rsx.
#[component]
#[cfg(not(target_os = "ios"))]
fn OnDeviceToolsToggle(state: Signal<AppState>) -> Element {
    let _ = state;
    rsx! {}
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
