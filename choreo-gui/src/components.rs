// This module is pure dioxus RSX. The `"{expr}"` text/attribute interpolation
// syntax is idiomatic dioxus and is NOT Rust `format!` — but the rustc macro
// expansion looks like a zero-argument format string, so `clippy::useless_format`
// fires on every interp. clippy's suggested `.to_string()` rewrite is wrong for
// dioxus (it fails to parse and defeats the reactive interpolation), so allow the
// false positive here.
#![expect(clippy::useless_format)]
use crate::client::{DaemonHandle, send_client_message, submit_input};
use crate::onboarding::{
    StepMark, checklist, request_models, select_account, select_model, submit_setup_form,
};
use crate::render::render_turn;
#[cfg(target_os = "ios")]
use crate::settings;
use crate::state::AppState;
use choreo_client_core::FEATURED_PROVIDERS;
use choreo_proto::ClientMessageType;
use dioxus::prelude::*;

#[component]
pub(crate) fn Toolbar(state: Signal<AppState>, tx: Signal<Option<DaemonHandle>>) -> Element {
    let on_ping = {
        let t = tx;
        move |_| {
            let daemon_tx = t.read().clone();
            send_client_message(&mut state.write(), daemon_tx, ClientMessageType::Ping);
        }
    };

    let on_models = {
        let t = tx;
        move |_| {
            let daemon_tx = t.read().clone();
            send_client_message(&mut state.write(), daemon_tx, ClientMessageType::ListModels);
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
            match request_id_text.parse::<u64>() {
                Ok(stream_id) => {
                    // Single write scope for the success path.
                    let mut guard = state.write();
                    let daemon_tx = t.read().clone();
                    send_client_message(
                        &mut guard,
                        daemon_tx,
                        ClientMessageType::Cancel { stream_id },
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
pub(crate) fn HistoryList(state: Signal<AppState>, tx: Signal<Option<DaemonHandle>>) -> Element {
    let guard = state.read();
    let turns: Vec<(u32, choreo_proto::Turn)> = guard
        .session_view
        .turns
        .iter()
        .map(|(&id, t)| (id, t.clone()))
        .collect();
    let status_texts = guard.status_texts.clone();
    // The welcome panel stands in for an empty conversation: a session is
    // attached and nothing is in it yet. It stays up after setup completes
    // (as a "ready" note) until the first turn lands.
    let show_welcome = guard.attached_session_id.is_some() && turns.is_empty();
    drop(guard);

    rsx! {
        div { class: "history",
            if show_welcome {
                WelcomePanel { state, tx }
            }
            for text in status_texts {
                div { class: "history-item text-item", key: "{text}", pre { "{text}" } }
            }
            for (turn_id, turn) in turns {
                {render_turn(turn_id, &turn)}
            }
        }
    }
}

/// The first-run checklist shown on an empty conversation. Each step ticks
/// itself off as the daemon confirms it, and the step to do now carries its
/// own controls — the GUI has no separate accounts page or model picker, so
/// setup happens entirely here. State and sends live in `crate::onboarding`.
#[component]
fn WelcomePanel(state: Signal<AppState>, tx: Signal<Option<DaemonHandle>>) -> Element {
    let progress = state.read().setup.progress();

    if progress.is_complete() {
        return rsx! {
            div { class: "welcome",
                div { class: "welcome-title", "Welcome to Choreographr" }
                div { class: "welcome-ready",
                    "You're all set. Type a message below and press Enter to start."
                }
            }
        };
    }

    let rows = checklist(progress)
        .into_iter()
        .enumerate()
        .map(|(index, (label, mark))| {
            let number = index + 1;
            let (marker, class) = match mark {
                StepMark::Done => ("✓", "welcome-step done"),
                StepMark::Next => ("→", "welcome-step next"),
                StepMark::Pending => ("○", "welcome-step pending"),
            };
            rsx! {
                div { class: "{class}", key: "{number}",
                    div { class: "welcome-step-label", "{marker} {number}. {label}" }
                    if mark == StepMark::Next {
                        match index {
                            0 => rsx! { AccountKeyStep { state, tx } },
                            1 => rsx! { AccountPickStep { state, tx } },
                            _ => rsx! { ModelPickStep { state, tx } },
                        }
                    }
                }
            }
        });

    rsx! {
        div { class: "welcome",
            div { class: "welcome-title", "Welcome to Choreographr" }
            div { "Before your first message, finish these steps:" }
            {rows}
            div { class: "welcome-hint",
                "Saving a key selects the account and loads its models for you."
            }
        }
    }
}

/// Step 1: create an account and store its API key in one submit (or just
/// add the key, when the name is an existing key-less account).
#[component]
fn AccountKeyStep(state: Signal<AppState>, tx: Signal<Option<DaemonHandle>>) -> Element {
    let guard = state.read();
    let form = &guard.setup.form;
    let provider = form.provider.clone();
    let account_name = form.account_name.clone();
    let api_key = form.api_key.clone();
    let error = form.error.clone();
    drop(guard);

    let on_save = move |_| {
        let handle = tx.read().clone();
        submit_setup_form(&mut state.write(), handle.as_ref());
    };

    rsx! {
        div { class: "welcome-body",
            div { class: "chip-row",
                for slug in FEATURED_PROVIDERS.iter().copied() {
                    button {
                        key: "{slug}",
                        class: if provider == slug { "chip selected" } else { "chip" },
                        onclick: move |_| state.write().setup.pick_provider(slug),
                        "{slug}"
                    }
                }
            }
            div { class: "welcome-field",
                label { "Provider" }
                input {
                    placeholder: "provider id, e.g. anthropic",
                    value: "{provider}",
                    oninput: move |event| state.write().setup.form.provider = event.value(),
                }
            }
            div { class: "welcome-field",
                label { "Account name" }
                input {
                    placeholder: "a name for this account",
                    value: "{account_name}",
                    oninput: move |event| state.write().setup.form.account_name = event.value(),
                }
            }
            div { class: "welcome-field",
                label { "API key" }
                input {
                    r#type: "password",
                    placeholder: "paste the provider's API key",
                    value: "{api_key}",
                    oninput: move |event| state.write().setup.form.api_key = event.value(),
                }
            }
            if let Some(error) = error {
                div { class: "welcome-error", "{error}" }
            }
            div { class: "welcome-actions",
                button { onclick: on_save, "Save key" }
            }
        }
    }
}

/// Step 2: one button per keyed account.
#[component]
fn AccountPickStep(state: Signal<AppState>, tx: Signal<Option<DaemonHandle>>) -> Element {
    let accounts: Vec<(String, String)> = state
        .read()
        .setup
        .keyed_accounts()
        .map(|a| (a.name.clone(), a.provider.clone()))
        .collect();

    rsx! {
        div { class: "welcome-body chip-row",
            for (name, provider) in accounts {
                button {
                    key: "{name}",
                    class: "chip",
                    onclick: {
                        let name = name.clone();
                        move |_| select_account(tx.read().as_ref(), &name)
                    },
                    "{name} ({provider})"
                }
            }
        }
    }
}

/// Step 3: a filterable list of the session account's models.
#[component]
fn ModelPickStep(state: Signal<AppState>, tx: Signal<Option<DaemonHandle>>) -> Element {
    let guard = state.read();
    let setup = &guard.setup;
    let filter = setup.form.model_filter.clone();
    let loaded = !setup.models.is_empty();
    let (shown, hidden) = setup.visible_models();
    let shown: Vec<String> = shown.into_iter().map(str::to_string).collect();
    drop(guard);

    let on_load = move |_| {
        if let Some(handle) = tx.read().clone() {
            request_models(&mut state.write(), &handle);
        }
    };

    rsx! {
        div { class: "welcome-body",
            if loaded {
                input {
                    placeholder: "filter models",
                    value: "{filter}",
                    oninput: move |event| state.write().setup.form.model_filter = event.value(),
                }
                div { class: "chip-row",
                    for model in shown {
                        button {
                            key: "{model}",
                            class: "chip",
                            onclick: {
                                let model = model.clone();
                                move |_| select_model(tx.read().as_ref(), &model)
                            },
                            "{model}"
                        }
                    }
                }
                if hidden > 0 {
                    div { class: "welcome-hint", "{hidden} more — type to narrow the list" }
                }
            } else {
                div { class: "welcome-actions",
                    button { onclick: on_load, "Load models" }
                }
            }
        }
    }
}

#[component]
pub(crate) fn Composer(state: Signal<AppState>, tx: Signal<Option<DaemonHandle>>) -> Element {
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
