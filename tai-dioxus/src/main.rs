mod client;
mod components;
mod hooks;
mod render;
mod state;

use crate::client::apply_daemon_message;
use crate::components::{Composer, HistoryList, Toolbar};
use crate::hooks::use_daemon_connection;
use crate::state::{AppState, UiEvent};
use dioxus::desktop::{Config, WindowBuilder};
use dioxus::prelude::*;
use futures_util::StreamExt as _;
use tai_proto::socket_path;

fn main() {
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(WindowBuilder::new().with_title("tai-dioxus")))
        .launch(App);
}

#[component]
fn App() -> Element {
    let (daemon_tx, mut events_rx) = use_daemon_connection();
    let mut state = use_signal(|| AppState::new(socket_path()));

    let tx = daemon_tx.read().clone();

    use_future({
        let tx = tx.clone();
        move || {
            let tx = tx.clone();
            async move {
                loop {
                    let event = {
                        let mut guard = events_rx.write();
                        match guard.as_mut() {
                            Some(rx) => rx.next().await,
                            None => break,
                        }
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
                                state.write().client.push_text(format!(
                                    "[client] failed to process daemon message: {error}"
                                ));
                            }
                        }
                        UiEvent::ReaderClosed => {
                            state.write().client.push_text("daemon connection closed");
                        }
                        UiEvent::ReaderFailed(error) => {
                            state
                                .write()
                                .client
                                .push_text(format!("[client] connection error: {error}"));
                        }
                    }
                }
            }
        }
    });

    rsx! {
        document::Style { {APP_CSS} }
        div { class: "app-shell",
            Toolbar { state, tx: daemon_tx }
            HistoryList { state }
            Composer { state, tx: daemon_tx }
        }
    }
}

const APP_CSS: &str = include_str!("style.css");

#[cfg(test)]
mod app_tests;
