use crate::sessions::{create_session_internal, session_snapshot, update_subscription};
use tai_proto::DaemonMessage;
use tokio::sync::mpsc;

async fn send_or_warn(tx: &mpsc::Sender<DaemonMessage>, msg: DaemonMessage) {
    crate::server::send_or_warn(tx, msg).await;
}

pub(crate) async fn handle_create_session(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: &mut Option<u64>,
    title: Option<String>,
    parent_session_id: Option<u64>,
    cwd: Option<String>,
    max_turns: Option<u32>,
) -> anyhow::Result<()> {
    let cwd_path = cwd.map(std::path::PathBuf::from);

    let (session_id, session) = create_session_internal(
        state,
        title.clone(),
        parent_session_id,
        cwd_path,
        max_turns,
    )
    .await?;

    update_subscription(
        state,
        client_id,
        *attached_session_id,
        Some(session_id),
        tx,
    )
    .await;
    *attached_session_id = Some(session_id);

    let cwd_display = session.lock().await.cwd.as_ref().map(|p| p.display().to_string());
    send_or_warn(
        tx,
        DaemonMessage::SessionCreated {
            session_id,
            title: title.clone(),
            parent_session_id,
            cwd: cwd_display,
            max_turns,
        },
    )
    .await;
    send_or_warn(tx, DaemonMessage::SessionAttached { session_id }).await;
    let snapshot = session_snapshot(session_id, &session).await;
    send_or_warn(tx, snapshot).await;
    Ok(())
}

pub(crate) async fn handle_attach_session(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: &mut Option<u64>,
    session_id: u64,
) -> anyhow::Result<()> {
    let Some(session) = crate::sessions::session_by_id(state, session_id).await else {
        send_or_warn(tx, DaemonMessage::SessionFailed {
            operation: "attach_session".to_string(),
            error: format!("unknown session: {session_id}"),
        }).await;
        return Ok(());
    };
    update_subscription(
        state,
        client_id,
        *attached_session_id,
        Some(session_id),
        tx,
    )
    .await;
    *attached_session_id = Some(session_id);
    send_or_warn(tx, DaemonMessage::SessionAttached { session_id }).await;
    let snapshot = session_snapshot(session_id, &session).await;
    send_or_warn(tx, snapshot).await;
    Ok(())
}
