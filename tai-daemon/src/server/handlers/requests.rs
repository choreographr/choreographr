use crate::openai::RequestFormat;
use crate::requests::{emit_demo_image, execute_chat_tool_request, execute_plain_request};
use crate::server::{
    try_client, try_session, REQUEST_TIMEOUT_SECS,
};
use crate::sessions::{ActiveRequest, broadcast_message_appended, broadcast_to_session};
use std::{sync::Arc, time::Duration};
use tai_proto::{DaemonMessage, SessionMessage};
use tokio::sync::mpsc;
use tracing::{info, warn};

async fn send_or_warn(tx: &mpsc::Sender<DaemonMessage>, msg: DaemonMessage) {
    crate::server::send_or_warn(tx, msg).await;
}

pub(crate) async fn handle_run_input(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: Option<u64>,
    request_id: u32,
    input: Vec<u8>,
) -> anyhow::Result<()> {
    let (session_id, session) = try_session!(state, attached_session_id, tx);

    let text = String::from_utf8_lossy(&input).trim().to_string();
    if text.is_empty() {
        warn!(request_id, "request failed: empty input");
        send_or_warn(tx, DaemonMessage::Started { request_id }).await;
        send_or_warn(tx, DaemonMessage::Failed {
            request_id,
            error: "empty input".to_string(),
        }).await;
        return Ok(());
    }

    let client = try_client!(state, tx);

    let model = {
        let mut guard = session.lock().await;
        if let Some(existing) = guard.active_requests.get(&request_id) {
            if existing.handle.is_finished() {
                guard.active_requests.remove(&request_id);
            } else {
                warn!(request_id, session_id, "duplicate request id rejected");
                drop(guard);
                send_or_warn(tx, DaemonMessage::Failed {
                    request_id,
                    error: "request id already active".to_string(),
                }).await;
                return Ok(());
            }
        }
        let Some(model) = guard.selected_model.clone() else {
            warn!(request_id, session_id, "request failed: no model selected");
            send_or_warn(tx, DaemonMessage::Started { request_id }).await;
            send_or_warn(tx, DaemonMessage::Failed {
                request_id,
                error: "no model selected".to_string(),
            }).await;
            return Ok(());
        };
        let message = SessionMessage::UserText {
            content: text.clone(),
        };
        guard.messages.push(message.clone());
        drop(guard);
        broadcast_message_appended(&session, message, Some(client_id)).await;
        model
    };

    let request_format = client.config().request_format_for_model(&model);
    info!(request_id, session_id, input_len = input.len(), selected_model = %model, ?request_format, "starting request");
    let session_clone = Arc::clone(&session);
    let handle = tokio::spawn(async move {
        broadcast_to_session(
            &session_clone,
            DaemonMessage::Started { request_id },
            None,
        )
        .await;

        let result = tokio::time::timeout(
            Duration::from_secs(REQUEST_TIMEOUT_SECS),
            async {
                match request_format {
                    RequestFormat::Responses => {
                        execute_plain_request(&client, &session_clone, &model, request_id).await
                    }
                    RequestFormat::ChatCompletions => {
                        execute_chat_tool_request(&client, &session_clone, &model, request_id)
                            .await
                    }
                }
            },
        )
        .await;

        let inner_result = match result {
            Ok(inner) => inner,
            Err(_elapsed) => {
                warn!(request_id, session_id, "request timed out");
                broadcast_to_session(
                    &session_clone,
                    DaemonMessage::Failed {
                        request_id,
                        error: format!("request timed out after {REQUEST_TIMEOUT_SECS}s"),
                    },
                    None,
                )
                .await;
                return;
            }
        };

        match inner_result {
            Ok(()) => {
                info!(request_id, session_id, "request completed");
                broadcast_to_session(
                    &session_clone,
                    DaemonMessage::Done { request_id },
                    None,
                )
                .await;
            }
            Err(error) => {
                warn!(request_id, session_id, error = %error, "request failed");
                broadcast_to_session(
                    &session_clone,
                    DaemonMessage::Failed {
                        request_id,
                        error: format!("model request failed: {error}"),
                    },
                    None,
                )
                .await;
            }
        }
    });

    session
        .lock()
        .await
        .active_requests
        .insert(request_id, ActiveRequest { handle });
    Ok(())
}

pub(crate) async fn handle_test_image(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    attached_session_id: Option<u64>,
    request_id: u32,
) -> anyhow::Result<()> {
    try_session!(state, attached_session_id, tx);
    info!(request_id, "sending demo image");
    send_or_warn(tx, DaemonMessage::Started { request_id }).await;
    match emit_demo_image(tx, request_id, 1).await {
        Ok(()) => {
            send_or_warn(tx, DaemonMessage::Done { request_id }).await;
        }
        Err(e) => {
            warn!(
                request_id,
                error = %anyhow::Error::from(e),
                "client disconnected before image could be delivered"
            );
        }
    }
    Ok(())
}

pub(crate) async fn handle_cancel(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: Option<u64>,
    request_id: u32,
) -> anyhow::Result<()> {
    let (session_id, session) = try_session!(state, attached_session_id, tx);
    if let Some(active_request) = session.lock().await.active_requests.remove(&request_id) {
        info!(request_id, session_id, "cancelling active request");
        active_request.handle.abort();
        send_or_warn(tx, DaemonMessage::Cancelled { request_id }).await;
        broadcast_to_session(
            &session,
            DaemonMessage::Cancelled { request_id },
            Some(client_id),
        )
        .await;
    } else {
        warn!(
            request_id,
            session_id, "cancel requested for inactive request"
        );
        send_or_warn(tx, DaemonMessage::Failed {
            request_id,
            error: "request id not active".to_string(),
        }).await;
    }
    Ok(())
}
