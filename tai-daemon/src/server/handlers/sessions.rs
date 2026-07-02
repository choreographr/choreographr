use crate::sessions::{SessionState, session_snapshot, update_subscription};
use crate::server::send_or_warn;
use std::{collections::HashMap, io, sync::Arc};
use tai_proto::DaemonMessage;
use tokio::sync::{Mutex, mpsc};

pub(crate) async fn handle_create_session(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: &mut Option<u64>,
    title: Option<String>,
) -> io::Result<()> {
    let (session_id, session) = {
        let mut guard = state.lock().await;
        let session_id = guard.next_session_id;
        guard.next_session_id = guard.next_session_id.wrapping_add(1);
        let session = Arc::new(Mutex::new(SessionState {
            title: title.clone(),
            selected_model: None,
            messages: Vec::new(),
            active_requests: HashMap::new(),
            subscribers: HashMap::new(),
        }));
        guard.sessions.insert(session_id, Arc::clone(&session));
        (session_id, session)
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
    send_or_warn!(tx, DaemonMessage::SessionCreated {
        session_id,
        title: title.clone(),
    });
    send_or_warn!(tx, DaemonMessage::SessionAttached { session_id });
    let snapshot = session_snapshot(session_id, &session).await;
    send_or_warn!(tx, snapshot);
    Ok(())
}

pub(crate) async fn handle_attach_session(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: &mut Option<u64>,
    session_id: u64,
) -> io::Result<()> {
    let Some(session) = crate::sessions::session_by_id(state, session_id).await else {
        send_or_warn!(tx, DaemonMessage::SessionFailed {
            operation: "attach_session".to_string(),
            error: format!("unknown session: {session_id}"),
        });
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
    send_or_warn!(tx, DaemonMessage::SessionAttached { session_id });
    let snapshot = session_snapshot(session_id, &session).await;
    send_or_warn!(tx, snapshot);
    Ok(())
}
