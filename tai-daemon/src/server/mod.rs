mod connection;
mod handlers;
mod lifecycle;

pub use connection::handle_client;
pub use lifecycle::run_server;

use crate::openai::OpenAiClient;
use crate::sessions::{list_sessions, session_by_id, session_snapshot};
use handlers::credentials::{
    handle_add_api_key, handle_add_x_credential, handle_get_credential, handle_lock,
    handle_remove_credential, handle_unlock,
};
use handlers::models::{handle_list_models, handle_set_model};
use handlers::requests::{handle_cancel, handle_run_input, handle_test_image};
use handlers::sessions::{handle_attach_session, handle_create_session};
use std::sync::Arc;
use tai_keystore::keystore_path;
use tai_proto::{ClientMessage, DaemonMessage};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub(crate) const REQUEST_TIMEOUT_SECS: u64 = 300;

async fn send_or_warn(tx: &mpsc::Sender<DaemonMessage>, msg: DaemonMessage) {
    if let Err(e) = tx.send(msg).await {
        warn!(
            error = %anyhow::Error::from(e),
            "failed to send daemon message, client likely disconnected"
        );
    }
}

macro_rules! try_session {
    ($state:expr, $attached:expr, $tx:expr) => {
        match $crate::sessions::require_attached_session($state, $attached, $tx).await? {
            Some((session_id, session)) => (session_id, session),
            None => return Ok(()),
        }
    };
}
pub(crate) use try_session;

macro_rules! try_client {
    ($state:expr, $tx:expr) => {
        match $crate::server::require_openai_client($state, $tx).await? {
            Some(c) => c,
            None => return Ok(()),
        }
    };
}
pub(crate) use try_client;

pub(crate) async fn require_openai_client(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
) -> anyhow::Result<Option<Arc<OpenAiClient>>> {
    let client = {
        let guard = state.lock().await;
        guard.openai_client.as_ref().map(Arc::clone)
    };
    match client {
        Some(c) => Ok(Some(c)),
        None => {
            send_or_warn(
                tx,
                DaemonMessage::LockedError {
                    error: "daemon is locked. use /unlock <passphrase> to unlock".to_string(),
                },
            ).await;
            Ok(None)
        }
    }
}

pub(crate) async fn try_keystore_path(
    tx: &mpsc::Sender<DaemonMessage>,
    build_error: impl FnOnce(String) -> DaemonMessage,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    match keystore_path() {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            send_or_warn(tx, build_error(format!("failed to determine keystore path: {e}"))).await;
            Ok(None)
        }
    }
}

pub(crate) async fn handle_client_message(
    msg: ClientMessage,
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: &mut Option<u64>,
) -> anyhow::Result<()> {
    match msg {
        ClientMessage::CreateSession { title, parent_session_id, cwd } => {
            handle_create_session(state, tx, client_id, attached_session_id, title, parent_session_id, cwd).await
        }
        ClientMessage::ListSessions => {
            let sessions = list_sessions(state).await;
            send_or_warn(tx, DaemonMessage::Sessions { sessions }).await;
            Ok(())
        }
        ClientMessage::AttachSession { session_id } => {
            handle_attach_session(state, tx, client_id, attached_session_id, session_id).await
        }
        ClientMessage::GetSessionState { session_id } => {
            let Some(session) = session_by_id(state, session_id).await else {
                send_or_warn(tx, DaemonMessage::SessionFailed {
                    operation: "get_session_state".to_string(),
                    error: format!("unknown session: {session_id}"),
                }).await;
                return Ok(());
            };
            let snapshot = session_snapshot(session_id, &session).await;
            send_or_warn(tx, snapshot).await;
            Ok(())
        }
        ClientMessage::RunInput {
            request_id, input,
        } => {
            handle_run_input(
                state,
                tx,
                client_id,
                *attached_session_id,
                request_id,
                input,
            )
            .await
        }
        ClientMessage::TestImage { request_id } => {
            handle_test_image(state, tx, *attached_session_id, request_id).await
        }
        ClientMessage::Cancel { request_id } => {
            handle_cancel(state, tx, client_id, *attached_session_id, request_id).await
        }
        ClientMessage::Ping => {
            debug!("responding to ping");
            send_or_warn(tx, DaemonMessage::Pong).await;
            Ok(())
        }
        ClientMessage::ListModels => {
            handle_list_models(state, tx, *attached_session_id).await
        }
        ClientMessage::SetModel { model } => {
            handle_set_model(state, tx, client_id, *attached_session_id, model).await
        }
        ClientMessage::Unlock { passphrase } => {
            handle_unlock(state, tx, passphrase).await
        }
        ClientMessage::Lock => {
            handle_lock(state, tx).await
        }
        ClientMessage::GetCredential { service } => {
            handle_get_credential(state, tx, service).await
        }
        ClientMessage::AddApiKey {
            service,
            passphrase,
            key,
        } => handle_add_api_key(tx, service, passphrase, key).await,
        ClientMessage::AddXCredential {
            service,
            passphrase,
            api_key,
            api_key_secret,
            access_token,
            access_token_secret,
            bearer_token,
        } => {
            handle_add_x_credential(
                tx,
                service,
                passphrase,
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token,
            )
            .await
        }
        ClientMessage::RemoveCredential {
            service,
            passphrase,
        } => handle_remove_credential(tx, service, passphrase).await,
    }
}
