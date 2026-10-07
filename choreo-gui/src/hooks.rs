use crate::client::{DaemonHandle, run_client};
use crate::state::UiEvent;
use choreo_proto::{ClientMessage, ClientMessageType};
use dioxus::prelude::*;
use futures_channel::mpsc::{self, UnboundedReceiver};

type DaemonConnection = (
    Signal<Option<DaemonHandle>>,
    Signal<Option<UnboundedReceiver<UiEvent>>>,
);

pub(crate) fn use_daemon_connection() -> DaemonConnection {
    let mut daemon_tx = use_signal(|| None::<DaemonHandle>);
    let mut events_rx = use_signal(|| None::<UnboundedReceiver<UiEvent>>);

    // Read the global connection mode set from CLI args in main().
    let mode = crate::CONNECTION_MODE.get().cloned().unwrap_or_default();

    // Connect to the daemon and spawn the client reader thread.
    // This runs once on mount.
    //
    // The two immediate sends are safe on EVERY transport, including the iOS
    // embedded link: `EmbeddedDaemon::connect` spawns the daemon-side
    // connection thread BEFORE returning (see choreo-daemon/src/embedded.rs),
    // so these queue in the unbounded channel — there is no handshake window
    // to race, same as the socket transports.
    use_hook(move || {
        let (client_tx, client_rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let (ui_tx, ui_rx) = mpsc::unbounded::<UiEvent>();
        // The handle owns the pending table, so every connect-time request
        // below allocates its id through the same single path the UI uses.
        let handle = DaemonHandle::new(client_tx);
        handle.send(ClientMessageType::ListSessions);
        // The GUI keeps its session list live via daemon push broadcasts
        // (SessionCreated / SessionStatusChanged / SessionDeleted). The daemon
        // no longer auto-registers TCP clients as summary subscribers, so the
        // GUI must opt in explicitly at connect — same as the TUI does on the
        // Unix path.
        handle.send(ClientMessageType::SubscribeSessionsSummary);
        // Connect-time keystore bootstrap (mirrors choreo-im's
        // `establish_keystore`). The GUI does not subscribe to the all-activity
        // bus, so it never receives the daemon's authoritative `Keystore`
        // status push; it drives the keystore itself. If a stored/legacy key
        // resolves, unlock with it; otherwise PROBE with a freshly minted
        // `BindKeystore` — an UNBOUND daemon adopts it (replies `Bound`, now
        // unlocked), a BOUND daemon rejects the mismatch (verify-only, no
        // overwrite) and stays locked until the real key is supplied. The
        // probe key is recorded into known_servers PRE-SEND by
        // `bind_fresh_daemon`, so a lost confirmation cannot orphan it.
        let keystore_addr = crate::client::connection_addr();
        match choreo_client_core::try_auto_unlock_key(&keystore_addr) {
            Some(private_key) => {
                handle.send(ClientMessageType::Unlock { private_key });
            }
            None => match choreo_client_core::bind_fresh_daemon(&keystore_addr) {
                Ok((_key, msg)) => {
                    handle.send(msg);
                }
                Err(e) => tracing::warn!(%e, "connect-time keystore bind probe failed"),
            },
        }
        daemon_tx.set(Some(handle));
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
