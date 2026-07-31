use choreo_proto::DaemonMessage;
use choreographr::{RequestContext, SessionCommand, db, session_main};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

mod common;

const CHANNEL_CAPACITY: usize = 128;

fn spawn_session(
    db: Arc<redb::Database>,
    session_id: u64,
) -> (mpsc::Sender<SessionCommand>, std::thread::JoinHandle<()>) {
    let (daemon_tx, _daemon_rx) = mpsc::channel();
    let (session_tx, session_rx) = mpsc::channel();

    let tool_registry = choreographr::tools::ToolRegistry::new().build();
    let cmd_tx = session_tx.clone();

    let handle = std::thread::spawn(move || {
        session_main(
            session_rx,
            None,
            None,
            None,
            RequestContext {
                cmd_tx,
                session_id,
                db,
                tool_registry,
                daemon_tx,
                max_turns_default: 0,
            },
        );
    });

    (session_tx, handle)
}

#[ignore]
#[test]
fn session_starts_and_accepts_commands() {
    let db = Arc::new(common::test_db());
    let (session_tx, _handle) = spawn_session(db, 1);

    let (writer_tx, writer_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
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
#[test]
fn session_shutdown_exits_without_active_requests() {
    let db = Arc::new(common::test_db());
    let (session_tx, handle) = spawn_session(db, 1);

    session_tx.send(SessionCommand::Shutdown).unwrap();
    drop(session_tx);

    handle.join().unwrap();
}

#[ignore]
#[test]
fn session_cancel_nonexistent_request_does_not_panic() {
    let db = Arc::new(common::test_db());
    let (session_tx, handle) = spawn_session(db, 1);

    // Cancel on a request_id that doesn't exist should not panic or hang.
    session_tx
        .send(SessionCommand::Cancel { request_id: 999 })
        .unwrap();

    // Session should still be functional afterwards.
    let (writer_tx, _writer_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
    session_tx
        .send(SessionCommand::Attach {
            client_id: 10,
            tx: writer_tx,
        })
        .unwrap();

    session_tx.send(SessionCommand::Shutdown).unwrap();
    drop(session_tx);

    handle.join().unwrap();
}

#[ignore]
#[test]
fn session_config_tools_mutate_authoritative_state_and_persist() {
    let db = Arc::new(common::test_db());
    let (session_tx, handle) = spawn_session(db.clone(), 1);

    // set_working_dir round-trip — the synchronous reply confirms the change
    // was applied by the session main loop's AUTHORITATIVE config, not a
    // throwaway worker copy (the regression this guards: the old inline
    // meta-tools mutated the request worker's snapshot, which was discarded at
    // request end, silently reverting the change on the next turn).
    let (wd_reply_tx, wd_reply_rx) = mpsc::channel();
    session_tx
        .send(SessionCommand::SetWorkingDir {
            path: PathBuf::from("/tmp/new-wd"),
            reply: wd_reply_tx,
        })
        .unwrap();
    match wd_reply_rx.recv().unwrap() {
        Ok(path) => assert_eq!(path, "/tmp/new-wd"),
        Err(e) => panic!("set_working_dir rejected: {e}"),
    }

    // load_tools round-trip.
    let (lt_reply_tx, lt_reply_rx) = mpsc::channel();
    session_tx
        .send(SessionCommand::LoadTools {
            groups: vec!["x".into()],
            reply: lt_reply_tx,
        })
        .unwrap();
    match lt_reply_rx.recv().unwrap() {
        Ok(msg) => assert!(msg.contains("x"), "unexpected summary: {msg}"),
        Err(e) => panic!("load_tools rejected: {e}"),
    }

    // The authoritative state must reflect both changes immediately — a
    // subsequent GetSummary sees them because the main loop holds them.
    let (summary_tx, summary_rx) = mpsc::channel();
    session_tx
        .send(SessionCommand::GetSummary { reply: summary_tx })
        .unwrap();
    let summary = summary_rx.recv().unwrap();
    assert_eq!(summary.working_dir.as_deref(), Some("/tmp/new-wd"));
    assert!(summary.active_tool_groups.contains(&"x".to_string()));

    // Shut down cleanly; the handlers persist via write_session_retry so the
    // record survives a daemon restart.
    session_tx.send(SessionCommand::Shutdown).unwrap();
    drop(session_tx);
    handle.join().unwrap();

    // The persisted record carries the mutations — the exact thing the
    // lost-update bug silently reverted.
    let record = db::read_session(&db, 1).unwrap().expect("session record");
    assert_eq!(record.working_dir.as_deref(), Some("/tmp/new-wd"));
    assert!(record.active_tool_groups.contains(&"x".to_string()));
}
