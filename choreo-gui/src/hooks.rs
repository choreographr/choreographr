use crate::client::run_client;
use crate::state::UiEvent;
use choreo_proto::ClientMessage;
use dioxus::prelude::*;
use futures_channel::mpsc::{self, UnboundedReceiver};

type DaemonConnection = (
    Signal<Option<std::sync::mpsc::Sender<ClientMessage>>>,
    Signal<Option<UnboundedReceiver<UiEvent>>>,
);

pub(crate) fn use_daemon_connection() -> DaemonConnection {
    let mut daemon_tx = use_signal(|| None::<std::sync::mpsc::Sender<ClientMessage>>);
    let mut events_rx = use_signal(|| None::<UnboundedReceiver<UiEvent>>);

    // Read the global connection mode set from CLI args in main().
    let mode = crate::CONNECTION_MODE.get().cloned().unwrap_or_default();

    // Connect to the daemon and spawn the client reader thread.
    // This runs once on mount.
    use_hook(move || {
        let (client_tx, client_rx) = std::sync::mpsc::channel::<ClientMessage>();
        let (ui_tx, ui_rx) = mpsc::unbounded::<UiEvent>();
        if let Err(e) = client_tx.send(ClientMessage::ListSessions) {
            tracing::error!("failed to send ListSessions: {e}");
        }
        // The GUI keeps its session list live via daemon push broadcasts
        // (SessionCreated / SessionStatusChanged / SessionDeleted). The daemon
        // no longer auto-registers TCP clients as summary subscribers, so the
        // GUI must opt in explicitly at connect — same as the TUI does on the
        // Unix path.
        if let Err(e) = client_tx.send(ClientMessage::SubscribeSessionsSummary) {
            tracing::error!("failed to send SubscribeSessionsSummary: {e}");
        }
        daemon_tx.set(Some(client_tx));
        events_rx.set(Some(ui_rx));
        let tx = ui_tx.clone();
        std::thread::spawn(move || {
            let error_tx = tx.clone();
            let panic_fallback = error_tx.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                if let Err(error) = run_client(mode, client_rx, tx)
                    && let Err(e) =
                        error_tx.unbounded_send(UiEvent::ReaderFailed(error.to_string()))
                {
                    tracing::error!("failed to send ReaderFailed: {e}");
                }
            }));
            if result.is_err()
                && let Err(e) = panic_fallback.unbounded_send(UiEvent::ReaderFailed(
                    "client reader task panicked".to_string(),
                ))
            {
                tracing::error!("failed to send panic notification: {e}");
            }
        });
    });

    (daemon_tx, events_rx)
}
