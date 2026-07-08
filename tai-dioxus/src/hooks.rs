use crate::client::run_client;
use crate::state::UiEvent;
use dioxus::prelude::*;
use futures_channel::mpsc::{self, UnboundedReceiver};
use tai_proto::{ClientMessage, socket_path};

pub(crate) fn use_daemon_connection(
) -> (
    Signal<Option<std::sync::mpsc::Sender<ClientMessage>>>,
    Signal<Option<UnboundedReceiver<UiEvent>>>,
) {
    let socket = use_signal(socket_path);
    let mut daemon_tx = use_signal(|| None::<std::sync::mpsc::Sender<ClientMessage>>);
    let mut events_rx = use_signal(|| None::<UnboundedReceiver<UiEvent>>);

    // Connect to the daemon socket and spawn the client reader thread.
    // This runs once on mount; the returned signals are populated before
    // any downstream hooks or the event loop read them.
    use_hook(move || {
        let socket = socket.read().clone();
        let (client_tx, client_rx) = std::sync::mpsc::channel::<ClientMessage>();
        let (ui_tx, ui_rx) = mpsc::unbounded::<UiEvent>();
        if let Err(e) = client_tx.send(ClientMessage::ListSessions) {
            tracing::error!("failed to send ListSessions: {e}");
        }
        daemon_tx.set(Some(client_tx));
        events_rx.set(Some(ui_rx));
        let tx = ui_tx.clone();
        std::thread::spawn(move || {
            let error_tx = tx.clone();
            // Clone one more handle so the catch_unwind closure can own its copy
            // while the outer scope retains one for the panic path.
            let panic_fallback = error_tx.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                if let Err(error) = run_client(socket, client_rx, tx) {
                    if let Err(e) = error_tx.unbounded_send(UiEvent::ReaderFailed(error.to_string()))
                    {
                        tracing::error!("failed to send ReaderFailed: {e}");
                    }
                }
            }));
            if result.is_err() {
                if let Err(e) = panic_fallback.unbounded_send(UiEvent::ReaderFailed(
                    "client reader task panicked".to_string(),
                )) {
                    tracing::error!("failed to send panic notification: {e}");
                }
            }
        });
    });

    (daemon_tx, events_rx)
}
