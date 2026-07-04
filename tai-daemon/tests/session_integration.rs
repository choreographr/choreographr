use std::sync::Arc;
use std::sync::mpsc;
use tai_daemon::{SessionCommand, session_main};
use tai_proto::DaemonMessage;
use tokio::sync::mpsc as tokio_mpsc;

mod common;

fn spawn_session(
    db: Arc<redb::Database>,
    session_id: u64,
) -> (mpsc::Sender<SessionCommand>, tokio::task::JoinHandle<()>) {
    let (daemon_tx, _daemon_rx) = tokio_mpsc::unbounded_channel();
    let (session_tx, session_rx) = mpsc::channel();

    let tool_registry = Arc::new(tai_daemon::tools::ToolRegistry::new());
    let db2 = Arc::clone(&db);
    let daemon_tx2 = daemon_tx.clone();
    let cmd_tx = session_tx.clone();

    let handle = tokio::task::spawn_blocking(move || {
        session_main(
            cmd_tx,
            session_rx,
            session_id,
            db2,
            None,
            tool_registry,
            daemon_tx2,
            None,
            25,
        );
    });

    (session_tx, handle)
}

#[ignore]
#[tokio::test]
async fn session_starts_and_accepts_commands() {
    let db = Arc::new(common::test_db());
    let (session_tx, _handle) = spawn_session(db, 1);

    let (writer_tx, writer_rx) = mpsc::channel();
    let client_id = 42;

    session_tx
        .send(SessionCommand::Attach {
            client_id,
            tx: writer_tx,
        })
        .unwrap();

    let msg = writer_rx.recv().unwrap();
    assert!(matches!(msg, DaemonMessage::SessionState { .. }));

    session_tx
        .send(SessionCommand::SetModel {
            model: "gpt-4".to_string(),
        })
        .unwrap();

    let (reply_tx, reply_rx) = mpsc::channel();
    session_tx
        .send(SessionCommand::GetSummary { reply: reply_tx })
        .unwrap();
    let summary = reply_rx.recv().unwrap();
    assert_eq!(summary.session_id, 1);

    session_tx
        .send(SessionCommand::Detach { client_id })
        .unwrap();
    drop(session_tx);
    drop(writer_rx);
}

#[ignore]
#[tokio::test]
async fn session_shutdown_exits_without_active_requests() {
    let db = Arc::new(common::test_db());
    let (session_tx, handle) = spawn_session(db, 1);

    session_tx.send(SessionCommand::Shutdown).unwrap();
    drop(session_tx);

    handle.await.unwrap();
}
