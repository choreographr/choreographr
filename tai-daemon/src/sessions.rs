use crate::openai::OpenAiClient;
use std::{collections::HashMap, io, sync::Arc};
use tai_keystore::Keystore;
use tai_keystore::XCredentials;
use tai_proto::{DaemonMessage, SessionMessage, SessionSummary};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

pub(crate) struct ActiveRequest {
    pub(crate) handle: JoinHandle<()>,
}

pub(crate) struct SessionState {
    pub(crate) title: Option<String>,
    pub(crate) selected_model: Option<String>,
    pub(crate) messages: Vec<SessionMessage>,
    pub(crate) active_requests: HashMap<u32, ActiveRequest>,
    pub(crate) subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>,
}

pub struct DaemonStateInner {
    pub(crate) next_session_id: u64,
    pub(crate) next_client_id: u64,
    pub(crate) sessions: HashMap<u64, Arc<Mutex<SessionState>>>,
    pub openai_client: Option<Arc<OpenAiClient>>,
    pub keystore: Option<Arc<Keystore>>,
    pub x_credentials: Option<XCredentials>,
}

pub type DaemonState = Arc<Mutex<DaemonStateInner>>;

pub fn new_daemon_state() -> DaemonState {
    let mut sessions = HashMap::new();
    sessions.insert(
        1,
        Arc::new(Mutex::new(SessionState {
            title: Some("default".to_string()),
            selected_model: None,
            messages: Vec::new(),
            active_requests: HashMap::new(),
            subscribers: HashMap::new(),
        })),
    );
    Arc::new(Mutex::new(DaemonStateInner {
        next_session_id: 2,
        next_client_id: 1,
        sessions,
        openai_client: None,
        keystore: None,
        x_credentials: None,
    }))
}

pub(crate) async fn default_session_id(state: &DaemonState) -> Option<u64> {
    state.lock().await.sessions.keys().min().copied()
}

pub(crate) async fn update_subscription(
    state: &DaemonState,
    client_id: u64,
    previous_session_id: Option<u64>,
    next_session_id: Option<u64>,
    tx: &mpsc::Sender<DaemonMessage>,
) {
    if previous_session_id == next_session_id {
        return;
    }

    if let Some(session_id) = previous_session_id
        && let Some(session) = session_by_id(state, session_id).await
    {
        session.lock().await.subscribers.remove(&client_id);
    }

    if let Some(session_id) = next_session_id
        && let Some(session) = session_by_id(state, session_id).await
    {
        session
            .lock()
            .await
            .subscribers
            .insert(client_id, tx.clone());
    }
}

pub(crate) async fn broadcast_to_session(
    session: &Arc<Mutex<SessionState>>,
    message: DaemonMessage,
    exclude_client_id: Option<u64>,
) {
    let subscribers = {
        let guard = session.lock().await;
        guard
            .subscribers
            .iter()
            .filter(|(client_id, _)| Some(**client_id) != exclude_client_id)
            .map(|(_, tx)| tx.clone())
            .collect::<Vec<_>>()
    };
    for tx in subscribers {
        let _ = tx.send(message.clone()).await;
    }
}

pub(crate) async fn broadcast_message_appended(
    session: &Arc<Mutex<SessionState>>,
    message: SessionMessage,
    exclude_client_id: Option<u64>,
) {
    let subscribers = {
        let guard = session.lock().await;
        guard
            .subscribers
            .iter()
            .filter(|(client_id, _)| Some(**client_id) != exclude_client_id)
            .map(|(_, tx)| tx.clone())
            .collect::<Vec<_>>()
    };
    for tx in subscribers {
        let _ = tx
            .send(DaemonMessage::SessionMessageAppended {
                message: message.clone(),
            })
            .await;
    }
}

pub(crate) async fn session_by_id(
    state: &DaemonState,
    session_id: u64,
) -> Option<Arc<Mutex<SessionState>>> {
    state.lock().await.sessions.get(&session_id).cloned()
}

pub(crate) async fn list_sessions(state: &DaemonState) -> Vec<SessionSummary> {
    let sessions: Vec<(u64, Arc<Mutex<SessionState>>)> = state
        .lock()
        .await
        .sessions
        .iter()
        .map(|(session_id, session)| (*session_id, Arc::clone(session)))
        .collect();
    let mut summaries = Vec::with_capacity(sessions.len());
    for (session_id, session) in sessions {
        let guard = session.lock().await;
        summaries.push(SessionSummary {
            session_id,
            title: guard.title.clone(),
            selected_model: guard.selected_model.clone(),
            message_count: guard.messages.len() as u32,
        });
    }
    summaries.sort_by_key(|summary| summary.session_id);
    summaries
}

pub(crate) async fn session_snapshot(
    session_id: u64,
    session: &Arc<Mutex<SessionState>>,
) -> DaemonMessage {
    let guard = session.lock().await;
    DaemonMessage::SessionState {
        session_id,
        title: guard.title.clone(),
        selected_model: guard.selected_model.clone(),
        messages: guard.messages.clone(),
    }
}

pub(crate) async fn require_attached_session(
    state: &DaemonState,
    attached_session_id: Option<u64>,
    tx: &mpsc::Sender<DaemonMessage>,
) -> io::Result<Option<(u64, Arc<Mutex<SessionState>>)>> {
    let Some(session_id) = attached_session_id else {
        let _ = tx
            .send(DaemonMessage::SessionFailed {
                operation: "require_attached_session".to_string(),
                error: "no session attached".to_string(),
            })
            .await;
        return Ok(None);
    };
    let Some(session) = session_by_id(state, session_id).await else {
        let _ = tx
            .send(DaemonMessage::SessionFailed {
                operation: "require_attached_session".to_string(),
                error: format!("unknown session: {session_id}"),
            })
            .await;
        return Ok(None);
    };
    Ok(Some((session_id, session)))
}
