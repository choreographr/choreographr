use crate::sessions::{SessionState, session_by_id, session_snapshot, update_subscription};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tai_proto::DaemonMessage;
use tokio::sync::{Mutex, mpsc};
use tracing::warn;

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
) -> anyhow::Result<()> {
    let resolved_cwd = if cwd.is_some() {
        cwd.map(std::path::PathBuf::from)
    } else if let Some(parent_id) = parent_session_id {
        if let Some(parent) = session_by_id(state, parent_id).await {
            parent.lock().await.cwd.clone()
        } else {
            None
        }
    } else {
        None
    };

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let (session_id, session) = {
        let mut guard = state.lock().await;
        let session_id = guard.next_session_id;
        guard.next_session_id = guard.next_session_id.wrapping_add(1);
        let session = Arc::new(Mutex::new(SessionState {
            title: title.clone(),
            selected_model: None,
            parent_session_id,
            cwd: resolved_cwd.clone(),
            created_at,
            messages: Vec::new(),
            active_requests: HashMap::new(),
            subscribers: HashMap::new(),
        }));
        guard.sessions.insert(session_id, Arc::clone(&session));
        (session_id, session)
    };

    let db = {
        let guard = state.lock().await;
        Arc::clone(&guard.db)
    };
    let record = crate::db::SessionRecord {
        title: title.clone(),
        selected_model: None,
        parent_session_id,
        cwd: resolved_cwd.as_ref().map(|p| p.display().to_string()),
        message_count: 0,
        created_at,
    };
    if let Err(e) = crate::db::write_session(&db, session_id, &record) {
        warn!(session_id, error = %e, "failed to persist new session to DB");
    }

    update_subscription(
        state,
        client_id,
        *attached_session_id,
        Some(session_id),
        tx,
    )
    .await;
    *attached_session_id = Some(session_id);
    send_or_warn(
        tx,
        DaemonMessage::SessionCreated {
            session_id,
            title: title.clone(),
            parent_session_id,
            cwd: resolved_cwd.map(|p| p.display().to_string()),
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
