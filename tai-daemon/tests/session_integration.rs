use tai_daemon::{handle_client, new_daemon_state};
use tai_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use tokio::{
    net::UnixStream,
    time::{Duration, timeout},
};

mod common;

async fn recv(client: &mut UnixStream) -> DaemonMessage {
    timeout(
        Duration::from_secs(2),
        read_message::<_, DaemonMessage>(client),
    )
    .await
    .expect("timed out")
    .expect("read failed")
}

#[ignore]
#[tokio::test]
async fn create_session_persists_to_db() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let db_clone = state.lock().await.db.clone();
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::CreateSession {
            title: Some("persist-test".to_string()),
            parent_session_id: None,
            cwd: Some("/tmp/test-cwd".to_string()),
            max_turns: None,
        },
    )
    .await
    .expect("write create-session");

    match recv(&mut client).await {
        DaemonMessage::SessionCreated {
            session_id,
            title,
            parent_session_id,
            cwd,
            ..
        } => {
            assert!(session_id > 0);
            assert_eq!(title, Some("persist-test".to_string()));
            assert_eq!(parent_session_id, None);
            assert_eq!(cwd, Some("/tmp/test-cwd".to_string()));
        }
        other => panic!("expected SessionCreated, got {other:?}"),
    }
    assert!(matches!(
        recv(&mut client).await,
        DaemonMessage::SessionAttached { .. }
    ));
    assert!(matches!(
        recv(&mut client).await,
        DaemonMessage::SessionState { .. }
    ));

    let db_sessions = tai_daemon::db::read_all_sessions(&db_clone).expect("read sessions");
    assert!(!db_sessions.is_empty(), "session should be persisted to DB");
    let (_id, record) = &db_sessions[0];
    assert_eq!(record.title, Some("persist-test".to_string()));
    assert_eq!(record.cwd, Some("/tmp/test-cwd".to_string()));

    drop(client);
    server_task.await.expect("join").expect("server ok");
}

#[ignore]
#[tokio::test]
async fn create_sub_session_inherits_parent_cwd() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::CreateSession {
            title: Some("parent".to_string()),
            parent_session_id: None,
            cwd: Some("/tmp/parent-cwd".to_string()),
            max_turns: None,
        },
    )
    .await
    .expect("write parent");

    let parent_id = match recv(&mut client).await {
        DaemonMessage::SessionCreated { session_id, .. } => session_id,
        other => panic!("expected SessionCreated, got {other:?}"),
    };
    assert!(matches!(recv(&mut client).await, DaemonMessage::SessionAttached { .. }));
    assert!(matches!(recv(&mut client).await, DaemonMessage::SessionState { .. }));

    write_message(
        &mut client,
        &ClientMessage::CreateSession {
            title: Some("child".to_string()),
            parent_session_id: Some(parent_id),
            cwd: None,
            max_turns: None,
        },
    )
    .await
    .expect("write child");

    match recv(&mut client).await {
        DaemonMessage::SessionCreated {
            session_id: child_id,
            title,
            parent_session_id,
            cwd,
            ..
        } => {
            assert!(child_id > parent_id);
            assert_eq!(title, Some("child".to_string()));
            assert_eq!(parent_session_id, Some(parent_id));
            assert_eq!(cwd, Some("/tmp/parent-cwd".to_string()));
        }
        other => panic!("expected SessionCreated for child, got {other:?}"),
    }

    drop(client);
    server_task.await.expect("join").expect("server ok");
}

#[ignore]
#[tokio::test]
async fn list_sessions_includes_new_fields() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::CreateSession {
            title: Some("list-test".to_string()),
            parent_session_id: None,
            cwd: Some("/tmp/list-cwd".to_string()),
            max_turns: None,
        },
    )
    .await
    .expect("write create-session");

    let session_id = match recv(&mut client).await {
        DaemonMessage::SessionCreated { session_id, .. } => session_id,
        other => panic!("expected SessionCreated, got {other:?}"),
    };
    assert!(matches!(recv(&mut client).await, DaemonMessage::SessionAttached { .. }));
    assert!(matches!(recv(&mut client).await, DaemonMessage::SessionState { .. }));

    write_message(&mut client, &ClientMessage::ListSessions)
        .await
        .expect("write list-sessions");

    match recv(&mut client).await {
        DaemonMessage::Sessions { sessions } => {
            let created = sessions
                .iter()
                .find(|s| s.session_id == session_id)
                .expect("created session in list");
            assert_eq!(created.title.as_deref(), Some("list-test"));
            assert_eq!(created.cwd.as_deref(), Some("/tmp/list-cwd"));
            assert_eq!(created.parent_session_id, None);
            assert!(created.created_at > 0);
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    drop(client);
    server_task.await.expect("join").expect("server ok");
}

#[ignore]
#[tokio::test]
async fn session_state_snapshot_includes_new_fields() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::CreateSession {
            title: Some("snapshot-test".to_string()),
            parent_session_id: Some(42),
            cwd: Some("/tmp/snap-cwd".to_string()),
            max_turns: None,
        },
    )
    .await
    .expect("write create-session");

    let session_id = match recv(&mut client).await {
        DaemonMessage::SessionCreated { session_id, .. } => session_id,
        other => panic!("expected SessionCreated, got {other:?}"),
    };
    assert!(matches!(recv(&mut client).await, DaemonMessage::SessionAttached { .. }));
    match recv(&mut client).await {
        DaemonMessage::SessionState {
            session_id: got_id,
            parent_session_id,
            cwd,
            ..
        } => {
            assert_eq!(got_id, session_id);
            assert_eq!(parent_session_id, Some(42));
            assert_eq!(cwd, Some("/tmp/snap-cwd".to_string()));
        }
        other => panic!("expected SessionState, got {other:?}"),
    }

    drop(client);
    server_task.await.expect("join").expect("server ok");
}

#[ignore]
#[tokio::test]
async fn session_survives_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.redb");

    let session_id = {
        let db = redb::Database::create(&db_path).expect("create db");
        let (server, mut client) = UnixStream::pair().expect("pair");
        let state = new_daemon_state(db, 25).await;
        let server_task = tokio::spawn(handle_client(server, state));

        write_message(
            &mut client,
            &ClientMessage::CreateSession {
                title: Some("restart-test".to_string()),
                parent_session_id: None,
                cwd: Some("/tmp/restart-cwd".to_string()),
                max_turns: None,
            },
        )
        .await
        .expect("write create-session");

        let session_id = match recv(&mut client).await {
            DaemonMessage::SessionCreated { session_id, .. } => session_id,
            other => panic!("expected SessionCreated, got {other:?}"),
        };
        assert!(matches!(recv(&mut client).await, DaemonMessage::SessionAttached { .. }));
        assert!(matches!(recv(&mut client).await, DaemonMessage::SessionState { .. }));

        drop(client);
        server_task.await.expect("join").expect("server ok");
        session_id
    };

    let db2 = redb::Database::open(&db_path).expect("reopen db");
    let (server2, mut client2) = UnixStream::pair().expect("pair2");
    let state2 = new_daemon_state(db2, 25).await;
    let server_task2 = tokio::spawn(handle_client(server2, state2));

    write_message(&mut client2, &ClientMessage::ListSessions)
        .await
        .expect("write list-sessions");

    match recv(&mut client2).await {
        DaemonMessage::Sessions { sessions } => {
            let created = sessions
                .iter()
                .find(|s| s.session_id == session_id)
                .expect("restored session in list");
            assert_eq!(created.title.as_deref(), Some("restart-test"));
            assert_eq!(created.cwd.as_deref(), Some("/tmp/restart-cwd"));
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    drop(client2);
    server_task2.await.expect("join").expect("server ok");
}
