use crate::openai::OpenAiClient;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tai_keystore::{Keystore, XCredentials};
use tai_proto::{DaemonMessage, SessionMessage, SessionSummary};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tracing::warn;

pub(crate) struct ActiveRequest {
    pub(crate) handle: JoinHandle<()>,
}

pub(crate) struct SessionState {
    pub(crate) title: Option<String>,
    pub(crate) selected_model: Option<String>,
    pub(crate) parent_session_id: Option<u64>,
    pub(crate) cwd: Option<std::path::PathBuf>,
    pub(crate) max_turns: Option<u32>,
    pub(crate) created_at: i64,
    pub(crate) messages: Vec<SessionMessage>,
    pub(crate) active_requests: HashMap<u32, ActiveRequest>,
    pub(crate) subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>,
}

pub struct DaemonStateInner {
    pub(crate) next_session_id: u64,
    pub(crate) next_client_id: u64,
    pub(crate) max_turns: u32,
    pub(crate) sessions: HashMap<u64, Arc<Mutex<SessionState>>>,
    pub openai_client: Option<Arc<OpenAiClient>>,
    pub keystore: Option<Arc<Keystore>>,
    pub x_credentials: Option<XCredentials>,
    pub db: Arc<redb::Database>,
}

pub type DaemonState = Arc<Mutex<DaemonStateInner>>;

pub async fn new_daemon_state(db: redb::Database, max_turns: u32) -> DaemonState {
    let db = Arc::new(db);

    let stored_sessions =
        crate::db::read_all_sessions(&db).unwrap_or_else(|e| {
            warn!(error = %e, "failed to read sessions from DB, starting fresh");
            Vec::new()
        });

    let mut sessions: HashMap<u64, Arc<Mutex<SessionState>>> = HashMap::new();
    let mut max_id: u64 = 0;

    for (id, record) in stored_sessions {
        let messages = crate::db::read_messages(&db, id).unwrap_or_else(|e| {
            warn!(session_id = id, error = %e, "failed to read messages for session, using empty");
            Vec::new()
        });
        sessions.insert(
            id,
            Arc::new(Mutex::new(SessionState {
                title: record.title,
                selected_model: record.selected_model,
                parent_session_id: record.parent_session_id,
                cwd: record.cwd.map(std::path::PathBuf::from),
                max_turns: record.max_turns,
                created_at: record.created_at,
                messages,
                active_requests: HashMap::new(),
                subscribers: HashMap::new(),
            })),
        );
        max_id = max_id.max(id);
    }

    if sessions.is_empty() {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        sessions.insert(
            1,
            Arc::new(Mutex::new(SessionState {
                title: Some("default".into()),
                selected_model: None,
                parent_session_id: None,
                cwd: None,
                max_turns: None,
                created_at,
                messages: Vec::new(),
                active_requests: HashMap::new(),
                subscribers: HashMap::new(),
            })),
        );
        max_id = 1;
    }

    Arc::new(Mutex::new(DaemonStateInner {
        next_session_id: max_id.wrapping_add(1),
        next_client_id: 1,
        max_turns,
        sessions,
        openai_client: None,
        keystore: None,
        x_credentials: None,
        db,
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
        if let Err(e) = tx.send(message.clone()).await {
            warn!(error = %e, "failed to send broadcast message, subscriber disconnected");
        }
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
        if let Err(e) = tx
            .send(DaemonMessage::SessionMessageAppended {
                message: message.clone(),
            })
            .await
        {
            warn!(error = %e, "failed to send broadcast message_appended, subscriber disconnected");
        }
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
            parent_session_id: guard.parent_session_id,
            cwd: guard.cwd.as_ref().map(|p| p.display().to_string()),
            created_at: guard.created_at,
            message_count: guard.messages.len() as u32,
            max_turns: guard.max_turns,
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
        parent_session_id: guard.parent_session_id,
        cwd: guard.cwd.as_ref().map(|p| p.display().to_string()),
        max_turns: guard.max_turns,
        messages: guard.messages.clone(),
    }
}

pub(crate) async fn require_attached_session(
    state: &DaemonState,
    attached_session_id: Option<u64>,
    tx: &mpsc::Sender<DaemonMessage>,
) -> anyhow::Result<Option<(u64, Arc<Mutex<SessionState>>)>> {
    let Some(session_id) = attached_session_id else {
        if let Err(e) = tx
            .send(DaemonMessage::SessionFailed {
                operation: "require_attached_session".to_string(),
                error: "no session attached".to_string(),
            })
            .await
        {
            warn!("failed to notify subscriber of session failure: {e}");
        }
        return Ok(None);
    };
    let Some(session) = session_by_id(state, session_id).await else {
        if let Err(e) = tx
            .send(DaemonMessage::SessionFailed {
                operation: "require_attached_session".to_string(),
                error: format!("unknown session: {session_id}"),
            })
            .await
        {
            warn!("failed to notify subscriber of session failure: {e}");
        }
        return Ok(None);
    };
    Ok(Some((session_id, session)))
}

pub(crate) async fn create_session_internal(
    state: &DaemonState,
    title: Option<String>,
    parent_session_id: Option<u64>,
    cwd: Option<std::path::PathBuf>,
    max_turns: Option<u32>,
) -> anyhow::Result<(u64, Arc<Mutex<SessionState>>)> {
    let resolved_cwd = if cwd.is_some() {
        cwd
    } else if let Some(parent_id) = parent_session_id {
        if let Some(parent) = session_by_id(state, parent_id).await {
            parent.lock().await.cwd.clone()
        } else {
            None
        }
    } else {
        None
    };

    let resolved_max_turns = if max_turns.is_some() {
        max_turns
    } else if let Some(parent_id) = parent_session_id {
        if let Some(parent) = session_by_id(state, parent_id).await {
            parent.lock().await.max_turns
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

    let (session_id, session, db) = {
        let mut guard = state.lock().await;
        let session_id = guard.next_session_id;
        guard.next_session_id = guard.next_session_id.wrapping_add(1);

        let session = Arc::new(Mutex::new(SessionState {
            title: title.clone(),
            selected_model: None,
            parent_session_id,
            cwd: resolved_cwd.clone(),
            max_turns: resolved_max_turns,
            created_at,
            messages: Vec::new(),
            active_requests: HashMap::new(),
            subscribers: HashMap::new(),
        }));

        guard.sessions.insert(session_id, Arc::clone(&session));
        (session_id, session, Arc::clone(&guard.db))
    };

    let record = crate::db::SessionRecord {
        title,
        selected_model: None,
        parent_session_id,
        cwd: resolved_cwd.map(|p| p.display().to_string()),
        max_turns: resolved_max_turns,
        message_count: 0,
        created_at,
    };
    if let Err(e) = crate::db::write_session(&db, session_id, &record) {
        warn!(session_id, error = %e, "failed to persist new session to DB");
    }

    Ok((session_id, session))
}

pub(crate) async fn append_message_and_persist(
    session: &Arc<Mutex<SessionState>>,
    db: &Arc<redb::Database>,
    session_id: u64,
    message: SessionMessage,
) -> u32 {
    let index;
    {
        let mut guard = session.lock().await;
        index = guard.messages.len() as u32;
        guard.messages.push(message.clone());
    }
    if let Err(e) = crate::db::write_message(db, session_id, index, &message) {
        warn!(session_id, index, error = %e, "failed to persist message to DB");
    }
    index
}


