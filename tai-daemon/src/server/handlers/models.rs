use crate::server::{send_or_warn, try_client, try_session};
use crate::sessions::{broadcast_to_session, session_by_id};
use std::io;
use tai_proto::DaemonMessage;
use tokio::sync::mpsc;
use tracing::debug;

pub(crate) async fn handle_list_models(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    attached_session_id: Option<u64>,
) -> io::Result<()> {
    let client = try_client!(state, tx);
    let config = client.config();
    debug!(base_url = %config.base_url, model_list_path = %config.model_list_path, responses_path = %config.responses_path, "listing configured models");
    let selected_model = match attached_session_id {
        Some(session_id) => match session_by_id(state, session_id).await {
            Some(session) => session.lock().await.selected_model.clone(),
            None => None,
        },
        None => None,
    };
    match client.validate_and_list_models().await {
        Ok(models) => {
            send_or_warn!(tx, DaemonMessage::Models {
                models,
                selected_model,
            });
        }
        Err(error) => {
            send_or_warn!(tx, DaemonMessage::ModelsFailed {
                error: format!("failed to list models: {error}"),
            });
        }
    }
    Ok(())
}

pub(crate) async fn handle_set_model(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    _client_id: u64,
    attached_session_id: Option<u64>,
    model: String,
) -> io::Result<()> {
    let (session_id, session) = try_session!(state, attached_session_id, tx);
    let client = try_client!(state, tx);
    let config = client.config();
    debug!(base_url = %config.base_url, model_list_path = %config.model_list_path, responses_path = %config.responses_path, requested_model = %model, session_id, "setting selected model");
    match client.validate_and_list_models().await {
        Ok(models) => {
            if models.iter().any(|candidate| candidate == &model) {
                session.lock().await.selected_model = Some(model.clone());
                broadcast_to_session(
                    &session,
                    DaemonMessage::ModelSelected { model },
                    None,
                )
                .await;
            } else {
                send_or_warn!(tx, DaemonMessage::ModelSelectionFailed {
                    model: model.clone(),
                    error: format!("unknown model: {model}"),
                });
            }
        }
        Err(error) => {
            send_or_warn!(tx, DaemonMessage::ModelSelectionFailed {
                model,
                error: format!("failed to list models: {error}"),
            });
        }
    }
    Ok(())
}
