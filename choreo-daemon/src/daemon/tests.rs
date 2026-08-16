use super::*;
use crate::broadcast::test_sink;
use crate::providers::test_util::make_test_provider;
use crate::sessions::SessionMetadata;
use choreo_proto::{DaemonMessage, SessionStatus, TimestampMs, Turn};
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;
use std::time::Instant;

fn make_daemon_state() -> (DaemonState, mpsc::Receiver<DaemonCommand>) {
    let (daemon_tx, daemon_rx) = mpsc::channel();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let tool_registry = crate::tools::ToolRegistry::new().build();
    let config_dir = tempfile::tempdir().unwrap();
    let accounts_path = config_dir.path().join("accounts.toml");
    let state = DaemonState {
        next_session_id: 1,
        max_turns: 10,
        active_sessions: HashMap::new(),
        session_metadata: HashMap::new(),
        deleted_sessions: HashSet::new(),
        children: HashMap::new(),
        accounts: AccountManager::load(&accounts_path).unwrap(),
        providers: HashMap::new(),
        credentials: HashMap::new(),
        x_credentials: None,
        db,
        tool_registry,
        daemon_tx,
        summary_subscribers: HashMap::new(),
        client_writers: HashMap::new(),
        activity_subscribers: HashMap::new(),
        client_subscribed_sessions: HashMap::new(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        lag_limits: LagLimits::default(),
        model_cache: HashMap::new(),
        mcp_manager: crate::mcp::McpManager::empty(),
        maintenance_tx: None,
        catalog_paths: CatalogPaths::default(),
    };
    (state, daemon_rx)
}

#[test]
fn handle_evict_client_removes_from_maps_and_sends_advisory() {
    let (mut state, _rx) = make_daemon_state();
    // Register the client in every map the way a live connection would.
    let (sink, rx) = test_sink();
    state.client_writers.insert(7, sink.clone());
    state.summary_subscribers.insert(7, sink.clone());
    state.activity_subscribers.insert(7, sink.clone());
    state
        .client_subscribed_sessions
        .insert(7, HashSet::from([1]));

    state.handle_command(DaemonCommand::EvictClient { client_id: 7 });

    // Evicted from every daemon-side map.
    assert!(!state.client_writers.contains_key(&7));
    assert!(!state.summary_subscribers.contains_key(&7));
    assert!(!state.activity_subscribers.contains_key(&7));
    assert!(!state.client_subscribed_sessions.contains_key(&7));
    // The best-effort advisory was enqueued before the sink was dropped.
    assert_eq!(rx.recv().unwrap(), DaemonMessage::Evicted);
}

#[test]
fn handle_evict_client_is_idempotent_for_unknown_client() {
    let (mut state, _rx) = make_daemon_state();
    // Evicting an unknown client must be a silent no-op (multiple
    // producers can signal the same over-lag client before the first
    // eviction lands).
    state.handle_command(DaemonCommand::EvictClient { client_id: 999 });
    assert!(state.client_writers.is_empty());
}

#[test]
fn handle_evict_largest_lagging_evicts_biggest_backlog() {
    let (mut state, _rx) = make_daemon_state();
    let (sink_small, _) = test_sink();
    sink_small.bytes_in_flight.store(10, Ordering::Relaxed);
    state.client_writers.insert(1, sink_small);
    let (sink_big, _) = test_sink();
    sink_big.bytes_in_flight.store(1_000, Ordering::Relaxed);
    state.client_writers.insert(2, sink_big);

    state.handle_command(DaemonCommand::EvictLargestLagging);

    assert!(
        !state.client_writers.contains_key(&2),
        "the largest backlog must be evicted"
    );
    assert!(state.client_writers.contains_key(&1));
}

#[test]
fn handle_evict_largest_lagging_noop_when_all_healthy() {
    let (mut state, _rx) = make_daemon_state();
    let (sink, _) = test_sink();
    state.client_writers.insert(1, sink);
    // Zero backlog: nothing to shed.
    state.handle_command(DaemonCommand::EvictLargestLagging);
    assert!(state.client_writers.contains_key(&1));
}
#[test]
fn handle_list_sessions_empty() {
    let (mut state, _rx) = make_daemon_state();
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::ListSessions { reply });
    let sessions = rx.recv().unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn handle_list_sessions_with_metadata() {
    let (mut state, _rx) = make_daemon_state();
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: Some("test".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 3,
            status: SessionStatus::Inactive,
            active_tool_groups: vec!["core".into()],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    );
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::ListSessions { reply });
    let sessions: Vec<SessionSummary> = rx.recv().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, 1);
    assert_eq!(sessions[0].title.as_deref(), Some("test"));
}

#[test]
fn handle_list_sessions_orders_by_last_modified_desc() {
    let (mut state, _rx) = make_daemon_state();
    // Insert three sessions with distinct modification times, deliberately
    // out of order in the map.
    for (id, created, modified) in [(1, 1000, 1000), (2, 2000, 9000), (3, 3000, 5000)] {
        state.session_metadata.insert(
            id,
            SessionMetadata {
                title: Some(format!("s{id}")),
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: created,
                last_modified: modified,
                turn_count: 0,
                status: SessionStatus::Inactive,
                active_tool_groups: vec![],
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
    }
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::ListSessions { reply });
    let sessions: Vec<SessionSummary> = rx.recv().unwrap();
    let ids: Vec<u64> = sessions.iter().map(|s| s.session_id).collect();
    // Most recently modified first: 2 (9000), 3 (5000), 1 (1000).
    assert_eq!(ids, vec![2, 3, 1]);
}

#[test]
fn handle_list_sessions_tiebreaks_by_session_id_desc() {
    let (mut state, _rx) = make_daemon_state();
    // Equal modification times must order deterministically by id desc.
    for id in [1u64, 2, 3] {
        state.session_metadata.insert(
            id,
            SessionMetadata {
                title: None,
                selected_model: None,
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: None,
                created_at: id as i64 * 1000,
                last_modified: 5000,
                turn_count: 0,
                status: SessionStatus::Inactive,
                active_tool_groups: vec![],
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
        );
    }
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::ListSessions { reply });
    let sessions: Vec<SessionSummary> = rx.recv().unwrap();
    let ids: Vec<u64> = sessions.iter().map(|s| s.session_id).collect();
    assert_eq!(ids, vec![3, 2, 1]);
}

#[test]
fn handle_get_session_missing() {
    let (mut state, _rx) = make_daemon_state();
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::GetSession {
        session_id: 1,
        reply,
    });
    let result = rx.recv().unwrap();
    assert!(result.is_none());
}

#[test]
fn handle_update_metadata() {
    let (mut state, _rx) = make_daemon_state();
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: Some("original".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec!["core".into()],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    );
    let new_meta = SessionMetadata {
        title: Some("updated".into()),
        selected_model: Some("gpt-4".into()),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 2000,
        last_modified: 2000,
        turn_count: 5,
        status: SessionStatus::Inference,
        active_tool_groups: vec!["core".into(), "git".into()],
        account_name: None,
        accumulated_usage: TokenUsage::default(),
        context_window: None,
        last_prompt_tokens: None,
    };
    state.handle_command(DaemonCommand::UpdateMetadata {
        session_id: 1,
        metadata: new_meta.clone(),
    });
    let stored = state.session_metadata.get(&1).unwrap();
    assert_eq!(stored.title.as_deref(), Some("updated"));
    assert_eq!(stored.selected_model.as_deref(), Some("gpt-4"));
    assert_eq!(stored.turn_count, 5);
    assert_eq!(stored.status, SessionStatus::Inference);
}

#[test]
fn handle_update_metadata_preserves_sleeping_status_after_exit() {
    let (mut state, _rx) = make_daemon_state();
    // A session that has exited: present in the metadata index with
    // Sleeping status, and no active session thread (the daemon removed
    // it from active_sessions in handle_session_exited).
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: Some("exited".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 5000,
            turn_count: 3,
            status: SessionStatus::Sleeping,
            active_tool_groups: vec![],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    );
    // Straggler snapshot from the (now dead) session thread — e.g. a
    // RequestFinished handler that raced with exit — claims Inactive.
    let stale = SessionMetadata {
        title: Some("exited".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1000,
        last_modified: 5000,
        turn_count: 4,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        accumulated_usage: TokenUsage::default(),
        context_window: None,
        last_prompt_tokens: None,
    };
    state.handle_command(DaemonCommand::UpdateMetadata {
        session_id: 1,
        metadata: stale,
    });
    let stored = state.session_metadata.get(&1).unwrap();
    // The exit status must win over the stale snapshot…
    assert_eq!(
        stored.status,
        SessionStatus::Sleeping,
        "exited session must not regress to a stale status"
    );
    // …while non-status fields from the snapshot still apply.
    assert_eq!(stored.turn_count, 4);
}

#[test]
fn handle_session_exited_nonexistent() {
    let (mut state, _rx) = make_daemon_state();
    state.handle_command(DaemonCommand::SessionExited { session_id: 999 });
    assert!(!state.session_metadata.contains_key(&999));
}

#[test]
fn handle_get_credential_locked() {
    let (mut state, _rx) = make_daemon_state();
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::GetCredential {
        service: "openai".into(),
        reply,
    });
    let key = rx.recv().unwrap();
    assert!(key.is_none());
}

#[test]
fn handle_register_unregister_subscriber() {
    let (mut state, _rx) = make_daemon_state();
    let (tx, _rx_sub) = test_sink();
    assert!(!state.summary_subscribers.contains_key(&42));
    state.handle_command(DaemonCommand::RegisterSummarySubscriber {
        client_id: 42,
        writer: tx,
    });
    assert!(state.summary_subscribers.contains_key(&42));
    state.handle_command(DaemonCommand::UnregisterSummarySubscriber { client_id: 42 });
    assert!(!state.summary_subscribers.contains_key(&42));
}

#[test]
fn handle_broadcast_session_status() {
    let (mut state, _rx) = make_daemon_state();
    // Seed the metadata index so the broadcast has something to update.
    state.session_metadata.insert(
        42,
        SessionMetadata {
            title: None,
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    );
    let (tx, rx) = test_sink();
    state.handle_command(DaemonCommand::RegisterSummarySubscriber {
        client_id: 1,
        writer: tx,
    });
    state.handle_command(DaemonCommand::BroadcastSessionStatus {
        session_id: 42,
        status: SessionStatus::Inference,
    });
    let msg = rx.recv().unwrap();
    assert!(matches!(
        msg,
        DaemonMessage::SessionStatusChanged {
            session_id: 42,
            status: SessionStatus::Inference,
            ..
        }
    ));
    // The metadata index must stay in sync so a later ListSessions serves
    // the fresh status (this is the stale-status bug fix).
    let meta = state.session_metadata.get(&42).expect("index updated");
    assert_eq!(meta.status, SessionStatus::Inference);
    // Status transitions are internal churn, not modifications: the index
    // status refreshes but the timestamp must stay put so the sessions
    // list does not re-sort mid-request.
    assert_eq!(
        meta.last_modified, 1000,
        "status transitions must not bump last_modified \
         (only completed requests and explicit edits do)"
    );
}

#[test]
fn handle_create_session_succeeds_when_locked() {
    // CreateSession should succeed even when the daemon is locked,
    // because a session is just a container — credentials are only
    // needed to run models, not to create or browse sessions.
    let (mut state, _rx) = make_daemon_state();
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::CreateSession {
        title: None,
        parent_session_id: None,
        working_dir: None,
        reasoning_effort: None,
        selected_model: None,
        context_config: None,
        account_name: None,
        active_tool_groups: Vec::new(),
        reply,
    });
    let result = rx.recv().unwrap();
    assert!(
        result.is_ok(),
        "CreateSession should succeed even when locked: {:?}",
        result.err()
    );
}

#[test]
fn handle_delete_session_succeeds_when_locked() {
    // DeleteSession should succeed even when the daemon is locked,
    // because a session is just a container — credentials are only
    // needed to run models, not to create, browse, or delete sessions.
    let (mut state, _rx) = make_daemon_state();
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::DeleteSession {
        session_id: 1,
        reply,
    });
    // Should succeed (session 1 doesn't exist, so it's a no-op)
    let result = rx.recv().unwrap();
    assert!(
        result.is_ok(),
        "DeleteSession should succeed even when locked: {:?}",
        result.err()
    );
}

#[test]
fn handle_attach_session_rejects_deleted_session() {
    // A deleted session's still-shutting-down thread can leave the DB
    // record in place until `handle_session_exited` finalizes the delete.
    // The attach guard must refuse to resurrect it even though a record
    // exists on disk.
    let (mut state, _daemon_rx) = make_daemon_state();
    let record = SessionRecord {
        title: Some("ghost".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        turn_count: 0,
        created_at: 1000,
        last_modified: 1000,
        active_tool_groups: vec![],
        context_config: ContextConfig::default(),
        account_name: None,
        last_response_id: None,
        last_response_id_producer: None,
    };
    db::write_session(&state.db, 1, &record).unwrap();
    // The deleted marker is set (the session thread has not yet exited,
    // so the record has not been finalized/deleted yet).
    state.deleted_sessions.insert(1);

    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::AttachSession {
        session_id: 1,
        reply,
    });
    let result = rx.recv().unwrap();
    assert!(
        result.is_err(),
        "deleted session must not be resurrected via attach"
    );
    // And it must not have been re-inserted into the in-memory index.
    assert!(!state.session_metadata.contains_key(&1));
    assert!(!state.active_sessions.contains_key(&1));
}

#[test]
fn session_exited_finalizes_pending_delete() {
    // A deleted session's thread exits: `handle_session_exited` must
    // delete the record, clear the deletion tombstone, and drop the
    // deleted marker, so the session cannot resurface and no stale
    // tombstone is left for the startup purge.  The delete now runs on a
    // background thread; wait for its `SessionDeleteFinalized`
    // confirmation — deterministic, because the thread sends it only
    // after the delete and tombstone clear have committed, so `recv`
    // unblocks with the DB already in its final state.
    let (mut state, daemon_rx) = make_daemon_state();
    let record = SessionRecord {
        title: Some("doomed".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        turn_count: 0,
        created_at: 1000,
        last_modified: 1000,
        active_tool_groups: vec![],
        context_config: ContextConfig::default(),
        account_name: None,
        last_response_id: None,
        last_response_id_producer: None,
    };
    db::write_session(&state.db, 7, &record).unwrap();
    db::mark_session_deleted(&state.db, 7).unwrap();
    state.deleted_sessions.insert(7);

    state.handle_command(DaemonCommand::SessionExited { session_id: 7 });

    // The background finalize reports back once the record is gone; route
    // it through the command handler exactly as the daemon loop would so
    // the `deleted_sessions` marker is dropped.
    match daemon_rx.recv() {
        Ok(DaemonCommand::SessionDeleteFinalized { session_id: 7 }) => {
            state.handle_command(DaemonCommand::SessionDeleteFinalized { session_id: 7 });
        }
        other => panic!(
            "expected SessionDeleteFinalized for session 7, got {:?}",
            std::mem::discriminant(&other)
        ),
    }

    // Marker dropped, record gone, tombstone gone (purge is a no-op).
    assert!(!state.deleted_sessions.contains(&7));
    assert!(db::read_session(&state.db, 7).unwrap().is_none());
    assert_eq!(
        db::purge_tombstoned_sessions(&state.db).unwrap(),
        0,
        "tombstone must be cleared once the record is deleted"
    );
}

#[test]
fn delete_finished_session_guards_against_straggler_resurrection() {
    // The fast path (`delete_finished_session`) must set the deleted
    // marker even though the record is gone: the finished thread's
    // `UpdateMetadata` straggler is queued ahead of its `SessionExited`
    // and would otherwise re-insert the session into the index.  The
    // marker blocks that, and the queued `SessionExited` finalizes the
    // delete (no-op here) and drops the marker.
    let (mut state, daemon_rx) = make_daemon_state();
    let record = SessionRecord {
        title: Some("doomed".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        turn_count: 0,
        created_at: 1000,
        last_modified: 1000,
        active_tool_groups: vec![],
        context_config: ContextConfig::default(),
        account_name: None,
        last_response_id: None,
        last_response_id_producer: None,
    };
    db::write_session(&state.db, 12, &record).unwrap();
    // A stale tombstone from an earlier interrupted delete of the same id.
    db::mark_session_deleted(&state.db, 12).unwrap();

    // Drive the extracted fast-path method directly: `is_finished()` is
    // gated by the caller in production and cannot be observed
    // deterministically in a unit test, so we exercise the fast-path body
    // itself.
    state.delete_finished_session(12).unwrap();

    // Record gone, marker set, metadata not present, stale tombstone swept.
    assert!(db::read_session(&state.db, 12).unwrap().is_none());
    assert!(
        state.deleted_sessions.contains(&12),
        "fast path must set the deleted marker so stragglers cannot resurrect the session"
    );
    assert!(!state.session_metadata.contains_key(&12));
    assert_eq!(
        db::purge_tombstoned_sessions(&state.db).unwrap(),
        0,
        "stale tombstone must be cleared on the fast-path delete"
    );

    // The straggler UpdateMetadata queued ahead of SessionExited must be
    // ignored (the marker blocks re-insertion into the index).
    state.handle_command(DaemonCommand::UpdateMetadata {
        session_id: 12,
        metadata: SessionMetadata {
            title: Some("doomed".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 2000,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    });
    assert!(
        !state.session_metadata.contains_key(&12),
        "straggler UpdateMetadata must not resurrect a deleted session"
    );

    // The queued SessionExited runs the standard finalize on a background
    // thread; wait for its SessionDeleteFinalized confirmation and route
    // it through the command handler exactly as the daemon loop would.
    state.handle_command(DaemonCommand::SessionExited { session_id: 12 });
    match daemon_rx.recv() {
        Ok(DaemonCommand::SessionDeleteFinalized { session_id: 12 }) => {
            state.handle_command(DaemonCommand::SessionDeleteFinalized { session_id: 12 });
        }
        other => panic!(
            "expected SessionDeleteFinalized for session 12, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
    assert!(
        !state.deleted_sessions.contains(&12),
        "marker must be dropped once the finalize confirms the record is gone"
    );
}

#[test]
fn delete_session_clears_stale_tombstone_when_no_live_thread() {
    // Deleting a session that has no live thread deletes the record
    // immediately AND sweeps any stale deletion tombstone left by an
    // earlier interrupted delete of the same id, so the tombstone cannot
    // accumulate or trigger a redundant startup purge.
    let (mut state, _daemon_rx) = make_daemon_state();
    let record = SessionRecord {
        title: Some("stale".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        turn_count: 0,
        created_at: 1000,
        last_modified: 1000,
        active_tool_groups: vec![],
        context_config: ContextConfig::default(),
        account_name: None,
        last_response_id: None,
        last_response_id_producer: None,
    };
    db::write_session(&state.db, 3, &record).unwrap();
    db::mark_session_deleted(&state.db, 3).unwrap();

    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::DeleteSession {
        session_id: 3,
        reply,
    });
    assert!(rx.recv().unwrap().is_ok());

    // Record gone, tombstone swept, and no deleted marker leaked (there
    // was no live thread to defer to).
    assert!(db::read_session(&state.db, 3).unwrap().is_none());
    assert_eq!(
        db::purge_tombstoned_sessions(&state.db).unwrap(),
        0,
        "stale tombstone must be cleared on immediate delete"
    );
    assert!(!state.deleted_sessions.contains(&3));
}

#[test]
fn delete_session_defers_when_thread_alive() {
    // Deleting a session whose thread is still running must NOT delete
    // the record synchronously — the thread can re-create it via
    // `persist_and_exit` during shutdown.  Instead it marks the session
    // deleted and writes a tombstone (crash-window safety); the record is
    // removed later by `handle_session_exited`'s background finalize.
    let (mut state, _daemon_rx) = make_daemon_state();
    let record = SessionRecord {
        title: Some("deferred".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        turn_count: 0,
        created_at: 1000,
        last_modified: 1000,
        active_tool_groups: vec![],
        context_config: ContextConfig::default(),
        account_name: None,
        last_response_id: None,
        last_response_id_producer: None,
    };
    db::write_session(&state.db, 4, &record).unwrap();
    // A blocking stand-in session thread: not finished, so the delete
    // must take the deferred path.
    let (_cmd_rx, release_tx) = insert_active_session(&mut state, 4);

    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::DeleteSession {
        session_id: 4,
        reply,
    });
    assert!(rx.recv().unwrap().is_ok());

    // Deferred: record still present, marker set, tombstone written
    // (a startup purge would clean it up if the daemon died now).
    assert!(db::read_session(&state.db, 4).unwrap().is_some());
    assert!(state.deleted_sessions.contains(&4));
    assert_eq!(
        db::purge_tombstoned_sessions(&state.db).unwrap(),
        1,
        "deferred delete must write a tombstone"
    );

    // Release the stand-in thread so it exits (no leaked threads).
    drop(release_tx);
}

#[test]
fn delete_session_keeps_tombstone_while_a_delete_is_pending() {
    // A second DeleteSession arriving while an earlier deferred delete is
    // still shutting its thread down must NOT sweep the pending delete's
    // tombstone: the thread can re-create the record via
    // `persist_and_exit` before its finalize runs, and a swept tombstone
    // would let a crash in that window resurrect the deleted session at
    // the next startup.
    let (mut state, _daemon_rx) = make_daemon_state();
    let record = SessionRecord {
        title: Some("double-deleted".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        turn_count: 0,
        created_at: 1000,
        last_modified: 1000,
        active_tool_groups: vec![],
        context_config: ContextConfig::default(),
        account_name: None,
        last_response_id: None,
        last_response_id_producer: None,
    };
    db::write_session(&state.db, 5, &record).unwrap();
    // First delete defers: live thread → marker set, tombstone written,
    // entry removed (the record stays until the thread exits).
    let (_cmd_rx, release_tx) = insert_active_session(&mut state, 5);
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::DeleteSession {
        session_id: 5,
        reply,
    });
    assert!(rx.recv().unwrap().is_ok());
    assert!(state.deleted_sessions.contains(&5));

    // Second delete arrives before the thread has exited: there is no live
    // entry now, so it takes the immediate-delete branch — but the pending
    // delete still owns the tombstone, which must survive.  (The tombstone
    // is probed with `purge_tombstoned_sessions` only once, at the end,
    // because the purge both deletes the record and clears tombstones.)
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::DeleteSession {
        session_id: 5,
        reply,
    });
    assert!(rx.recv().unwrap().is_ok());
    assert!(
        state.deleted_sessions.contains(&5),
        "marker must stay while the deferred delete is pending"
    );
    assert_eq!(
        db::purge_tombstoned_sessions(&state.db).unwrap(),
        1,
        "the pending delete's tombstone must not be swept by a second delete"
    );

    // Release the stand-in thread so it exits (no leaked threads).
    drop(release_tx);
}

#[test]
fn session_delete_finalized_drops_marker() {
    // The background finalize's confirmation must be the thing that drops
    // the `deleted_sessions` marker — clearing it earlier (e.g. on the
    // zombie's `SessionExited`) would reopen an attach-resurrection
    // window while the record is still on disk.
    let (mut state, _daemon_rx) = make_daemon_state();
    state.deleted_sessions.insert(9);
    state.handle_command(DaemonCommand::SessionDeleteFinalized { session_id: 9 });
    assert!(!state.deleted_sessions.contains(&9));
}

#[test]
fn session_exited_does_not_delete_non_deleted_session() {
    // A normal (non-deleted) session exit must leave the record alone —
    // finalize only runs for sessions whose delete is still pending.
    let (mut state, _daemon_rx) = make_daemon_state();
    let record = SessionRecord {
        title: Some("alive".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        turn_count: 0,
        created_at: 1000,
        last_modified: 1000,
        active_tool_groups: vec![],
        context_config: ContextConfig::default(),
        account_name: None,
        last_response_id: None,
        last_response_id_producer: None,
    };
    db::write_session(&state.db, 8, &record).unwrap();

    state.handle_command(DaemonCommand::SessionExited { session_id: 8 });

    assert!(db::read_session(&state.db, 8).unwrap().is_some());
}

#[test]
fn broadcast_sends_to_subscriber() {
    let (mut state, _rx) = make_daemon_state();
    let (tx, rx) = test_sink();
    state.summary_subscribers.insert(1, tx);
    let msg = DaemonMessage::SessionDeleted { session_id: 42 };
    state.broadcast(msg.clone());
    let received = rx.recv().unwrap();
    assert_eq!(received, msg);
    // Subscriber should still be registered
    assert!(state.summary_subscribers.contains_key(&1));
}

#[test]
fn broadcast_removes_disconnected_subscriber() {
    let (mut state, _rx) = make_daemon_state();
    let (tx, rx) = test_sink();
    state.summary_subscribers.insert(1, tx);
    drop(rx); // Disconnect the receiver
    state.broadcast(DaemonMessage::SessionDeleted { session_id: 42 });
    // Dead subscriber should be removed
    assert!(!state.summary_subscribers.contains_key(&1));
}

#[test]
fn broadcast_enqueues_losslessly_and_evicts_over_lag_client() {
    let (mut state, _rx) = make_daemon_state();
    // Tiny per-client cap so a single message crosses it; the global
    // budget is infinite so only the per-client threshold fires.
    state.lag_limits = LagLimits {
        per_client_cap: 16,
        global_budget: usize::MAX,
    };
    // The subscriber must also be in the writer registry for eviction to
    // have a connection to tear down (handle_evict_client requires it).
    let (sink, rx) = test_sink();
    state.summary_subscribers.insert(7, sink.clone());
    state.client_writers.insert(7, sink);

    let msg = DaemonMessage::SessionDeleted { session_id: 42 };
    state.broadcast(msg.clone());

    // Lossless: the crossing message is still delivered, never dropped.
    assert_eq!(rx.recv().unwrap(), msg);
    // …but the client is evicted for lag, from every map.
    assert!(
        !state.summary_subscribers.contains_key(&7),
        "over-lag subscriber must be evicted from the summary map"
    );
    assert!(
        !state.client_writers.contains_key(&7),
        "over-lag subscriber must be evicted from the writer registry"
    );
}

#[test]
fn handle_validate_model_allows_through_when_no_session() {
    let (mut state, _rx) = make_daemon_state();
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::ValidateModel {
        session_id: 999,
        model: "gpt-4".into(),
        reply,
    });
    let result = rx.recv().unwrap();
    assert_eq!(result, Ok(()));
}

#[test]
fn handle_validate_model_rejects_when_no_provider() {
    let (mut state, _rx) = make_daemon_state();
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: None,
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 0,
            status: SessionStatus::Sleeping,
            active_tool_groups: vec![],
            account_name: Some("locked-account".into()),
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    );
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::ValidateModel {
        session_id: 1,
        model: "gpt-4".into(),
        reply,
    });
    let result = rx.recv().unwrap();
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("locked"), "error should mention locked daemon");
    assert!(
        err.contains("locked-account"),
        "error should mention the account"
    );
}

#[test]
fn handle_validate_model_rejects_unknown_model() {
    let (mut state, _rx) = make_daemon_state();
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: None,
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: Some("test-account".into()),
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    );
    state
        .providers
        .insert("test-account".into(), make_test_provider());
    state.model_cache.insert(
        "test-account".into(),
        (vec!["gpt-4".into(), "gpt-3.5".into()], Instant::now()),
    );
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::ValidateModel {
        session_id: 1,
        model: "nonexistent-model".into(),
        reply,
    });
    let result = rx.recv().unwrap();
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("nonexistent-model"),
        "error should mention the model name"
    );
    assert!(err.contains("gpt-4"), "error should list available models");
}

#[test]
fn handle_validate_model_allows_known_model() {
    let (mut state, _rx) = make_daemon_state();
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: None,
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: Some("test-account".into()),
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    );
    state
        .providers
        .insert("test-account".into(), make_test_provider());
    state.model_cache.insert(
        "test-account".into(),
        (vec!["gpt-4".into(), "gpt-3.5".into()], Instant::now()),
    );
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::ValidateModel {
        session_id: 1,
        model: "gpt-4".into(),
        reply,
    });
    let result = rx.recv().unwrap();
    assert_eq!(result, Ok(()));
}

#[test]
fn handle_set_session_title_forwards_to_session() {
    let (mut state, _daemon_rx) = make_daemon_state();

    // Create an active session entry with a cmd_tx so the daemon
    // can forward the title change.
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (handle_tx, handle_rx) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        // Block until told to stop — we just need the entry to exist.
        let _ = handle_rx.recv();
    });
    state
        .active_sessions
        .insert(1, ActiveSessionEntry { cmd_tx, handle });
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: Some("old title".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec!["core".into()],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        },
    );

    state.handle_command(DaemonCommand::SetSessionTitle {
        session_id: 1,
        title: "new title".into(),
    });

    // Verify the session thread received a SetTitle command.
    // The send is synchronous (handle_command sends on cmd_tx), so
    // try_recv is deterministic — no time-based wait needed.
    match cmd_rx.try_recv() {
        Ok(SessionCommand::SetTitle { title }) => {
            assert_eq!(title, "new title");
        }
        Ok(_) => {
            panic!("expected SetTitle, got a different SessionCommand variant");
        }
        Err(e) => {
            panic!("expected SetTitle, got error: {e}");
        }
    }

    // Clean up the session thread.
    let _ = handle_tx.send(());
}

#[test]
fn handle_set_session_title_nonexistent_session_logs_warning() {
    let (mut state, _rx) = make_daemon_state();

    // Sending SetSessionTitle for a session that doesn't exist should
    // log a warning and not panic.
    state.handle_command(DaemonCommand::SetSessionTitle {
        session_id: 999,
        title: "ghost title".into(),
    });
    // No active session = no message to verify; just checking no panic.
}

// ── Session-config tool forwarding tests ────────────────────────────

/// Insert a minimal active-session entry whose command channel is
/// returned for verification.  The session thread blocks until the
/// returned release sender fires, then exits.
fn insert_active_session(
    state: &mut DaemonState,
    session_id: u64,
) -> (mpsc::Receiver<SessionCommand>, mpsc::Sender<()>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        // Block until told to stop — we just need the entry to exist.
        let _ = release_rx.recv();
    });
    state
        .active_sessions
        .insert(session_id, ActiveSessionEntry { cmd_tx, handle });
    (cmd_rx, release_tx)
}

#[test]
fn handle_set_working_dir_forwards_to_session() {
    let (mut state, _daemon_rx) = make_daemon_state();
    let (cmd_rx, release_tx) = insert_active_session(&mut state, 1);

    state.handle_command(DaemonCommand::SetWorkingDir {
        session_id: 1,
        path: PathBuf::from("/tmp"),
        reply: mpsc::channel().0,
    });

    // The send is synchronous (handle_command sends on cmd_tx), so
    // try_recv is deterministic — no time-based wait needed.
    match cmd_rx.try_recv() {
        Ok(SessionCommand::SetWorkingDir { path, .. }) => {
            assert_eq!(path, PathBuf::from("/tmp"));
        }
        Ok(_) => panic!("expected SetWorkingDir, got a different SessionCommand variant"),
        Err(e) => panic!("expected SetWorkingDir, got error: {e}"),
    }

    // Clean up the session thread.
    let _ = release_tx.send(());
}

#[test]
fn handle_set_working_dir_nonexistent_session_replies_error() {
    let (mut state, _daemon_rx) = make_daemon_state();
    let (reply_tx, reply_rx) = mpsc::channel();

    state.handle_command(DaemonCommand::SetWorkingDir {
        session_id: 999,
        path: PathBuf::from("/tmp"),
        reply: reply_tx,
    });

    // The daemon replies synchronously for inactive sessions so a
    // blocked tool execution never hangs.
    match reply_rx.recv() {
        Ok(Err(msg)) => assert!(msg.contains("not active"), "unexpected msg: {msg}"),
        Ok(Ok(_)) => panic!("expected an error reply for an inactive session"),
        Err(e) => panic!("expected error reply, got {e:?}"),
    }
}

#[test]
fn handle_load_tools_forwards_to_session() {
    let (mut state, _daemon_rx) = make_daemon_state();
    let (cmd_rx, release_tx) = insert_active_session(&mut state, 1);

    state.handle_command(DaemonCommand::LoadTools {
        session_id: 1,
        groups: vec!["x".into()],
        reply: mpsc::channel().0,
    });

    match cmd_rx.try_recv() {
        Ok(SessionCommand::LoadTools { groups, .. }) => {
            assert_eq!(groups, vec!["x"]);
        }
        Ok(_) => panic!("expected LoadTools, got a different SessionCommand variant"),
        Err(e) => panic!("expected LoadTools, got error: {e}"),
    }

    let _ = release_tx.send(());
}

#[test]
fn handle_load_tools_nonexistent_session_replies_error() {
    let (mut state, _daemon_rx) = make_daemon_state();
    let (reply_tx, reply_rx) = mpsc::channel();

    state.handle_command(DaemonCommand::LoadTools {
        session_id: 999,
        groups: vec!["x".into()],
        reply: reply_tx,
    });

    // The daemon replies synchronously for inactive sessions so a
    // blocked tool execution never hangs.
    match reply_rx.recv() {
        Ok(Err(msg)) => assert!(msg.contains("not active"), "unexpected msg: {msg}"),
        Ok(Ok(_)) => panic!("expected an error reply for an inactive session"),
        Err(e) => panic!("expected error reply, got {e:?}"),
    }
}

#[test]
fn handle_unload_tools_forwards_to_session() {
    let (mut state, _daemon_rx) = make_daemon_state();
    let (cmd_rx, release_tx) = insert_active_session(&mut state, 1);

    state.handle_command(DaemonCommand::UnloadTools {
        session_id: 1,
        groups: vec!["x".into()],
        reply: mpsc::channel().0,
    });

    match cmd_rx.try_recv() {
        Ok(SessionCommand::UnloadTools { groups, .. }) => {
            assert_eq!(groups, vec!["x"]);
        }
        Ok(_) => panic!("expected UnloadTools, got a different SessionCommand variant"),
        Err(e) => panic!("expected UnloadTools, got error: {e}"),
    }

    let _ = release_tx.send(());
}

#[test]
fn handle_unload_tools_nonexistent_session_replies_error() {
    let (mut state, _daemon_rx) = make_daemon_state();
    let (reply_tx, reply_rx) = mpsc::channel();

    state.handle_command(DaemonCommand::UnloadTools {
        session_id: 999,
        groups: vec!["x".into()],
        reply: reply_tx,
    });

    match reply_rx.recv() {
        Ok(Err(msg)) => assert!(msg.contains("not active"), "unexpected msg: {msg}"),
        Ok(Ok(_)) => panic!("expected an error reply for an inactive session"),
        Err(e) => panic!("expected error reply, got {e:?}"),
    }
}

// ── Activity subscriber tests ───────────────────────────────────────

/// Drain the send-on-subscribe `CatalogUpdated` that registering an
/// activity subscriber delivers to the fresh client, so tests can assert
/// on the messages that follow registration.
fn drain_send_on_subscribe(rx: &crossbeam_channel::Receiver<DaemonMessage>) {
    let msg = rx.recv().unwrap();
    assert!(
        matches!(&msg, DaemonMessage::CatalogUpdated { providers } if !providers.is_empty()),
        "expected the send-on-subscribe CatalogUpdated, got {msg:?}",
    );
}

#[test]
#[serial_test::serial(catalog)]
fn handle_register_activity_subscriber_adds_to_map() {
    let (mut state, _rx) = make_daemon_state();
    let (tx, _) = test_sink();

    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx,
    });

    assert!(state.activity_subscribers.contains_key(&10));
}

#[test]
#[serial_test::serial(catalog)]
fn handle_register_activity_subscriber_replaces_existing() {
    let (mut state, _rx) = make_daemon_state();
    let (tx1, _) = test_sink();
    let (tx2, _) = test_sink();

    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx1,
    });
    // Re-register with a different writer — should replace without error
    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx2,
    });

    assert!(state.activity_subscribers.contains_key(&10));
}

#[test]
#[serial_test::serial(catalog)]
fn handle_unregister_activity_subscriber_preserves_session_tracking() {
    let (mut state, _rx) = make_daemon_state();
    let (tx, _) = test_sink();

    // Set up: client 10 is subscribed to activity AND subscribed to session 42
    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx,
    });
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });

    // Unsubscribe from all activity — this should NOT clear session tracking
    state.handle_command(DaemonCommand::UnregisterActivitySubscriber { client_id: 10 });

    // Verify: activity subscriber is gone
    assert!(!state.activity_subscribers.contains_key(&10));
    // Verify: session tracking is PRESERVED
    assert!(state.client_subscribed_sessions.contains_key(&10));
    let sessions = state.client_subscribed_sessions.get(&10).unwrap();
    assert!(sessions.contains(&42));
    assert_eq!(sessions.len(), 1);
}

#[test]
#[serial_test::serial(catalog)]
fn handle_client_disconnected_clears_all_tracking() {
    let (mut state, _rx) = make_daemon_state();
    let (tx, _) = test_sink();

    // Set up: client 10 is registered in all three maps
    state.handle_command(DaemonCommand::RegisterSummarySubscriber {
        client_id: 10,
        writer: tx.clone(),
    });
    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx,
    });
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 1,
    });
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 2,
    });

    assert!(state.summary_subscribers.contains_key(&10));
    assert!(state.activity_subscribers.contains_key(&10));
    assert!(state.client_subscribed_sessions.contains_key(&10));

    // Disconnect: clears everything
    state.handle_command(DaemonCommand::ClientDisconnected { client_id: 10 });

    assert!(!state.summary_subscribers.contains_key(&10));
    assert!(!state.activity_subscribers.contains_key(&10));
    assert!(!state.client_subscribed_sessions.contains_key(&10));
}

#[test]
fn handle_client_disconnected_noop_for_unknown_client() {
    let (mut state, _rx) = make_daemon_state();
    state.handle_command(DaemonCommand::ClientDisconnected { client_id: 999 });
    // Just checking no panic
}

#[test]
fn handle_track_session_subscription_adds_entry() {
    let (mut state, _rx) = make_daemon_state();

    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });

    let sessions = state
        .client_subscribed_sessions
        .get(&10)
        .expect("client should have entry");
    assert!(sessions.contains(&42));
    assert_eq!(sessions.len(), 1);
}

#[test]
fn handle_track_session_subscription_idempotent_re_attach() {
    let (mut state, _rx) = make_daemon_state();

    // Attach to same session twice — should be idempotent
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });

    let sessions = state
        .client_subscribed_sessions
        .get(&10)
        .expect("client should have entry");
    assert!(sessions.contains(&42));
    assert_eq!(sessions.len(), 1, "should not duplicate session_id");
}

#[test]
fn handle_track_session_subscription_tracks_multiple_sessions() {
    let (mut state, _rx) = make_daemon_state();

    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 99,
    });

    let sessions = state
        .client_subscribed_sessions
        .get(&10)
        .expect("client should have entry");
    assert!(sessions.contains(&42));
    assert!(sessions.contains(&99));
    assert_eq!(sessions.len(), 2);
}

#[test]
fn handle_untrack_session_subscription_removes_session() {
    let (mut state, _rx) = make_daemon_state();

    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 99,
    });

    // Untrack one session
    state.handle_command(DaemonCommand::UntrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });

    let sessions = state
        .client_subscribed_sessions
        .get(&10)
        .expect("client should still have entry");
    assert!(!sessions.contains(&42));
    assert!(sessions.contains(&99));
    assert_eq!(sessions.len(), 1);
}

#[test]
fn handle_untrack_session_subscription_removes_client_when_empty() {
    let (mut state, _rx) = make_daemon_state();

    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });

    // Untrack the only session
    state.handle_command(DaemonCommand::UntrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });

    // Client entry should be removed entirely when empty
    assert!(!state.client_subscribed_sessions.contains_key(&10));
}

#[test]
fn handle_untrack_session_subscription_noop_for_unknown_session() {
    let (mut state, _rx) = make_daemon_state();

    // Untrack a session that was never tracked — should be a no-op
    state.handle_command(DaemonCommand::UntrackSessionSubscription {
        client_id: 10,
        session_id: 42,
    });

    assert!(!state.client_subscribed_sessions.contains_key(&10));
}

#[test]
fn handle_untrack_session_subscription_noop_for_unknown_client() {
    let (mut state, _rx) = make_daemon_state();
    state.handle_command(DaemonCommand::UntrackSessionSubscription {
        client_id: 999,
        session_id: 42,
    });
    // Just checking no panic
}

#[test]
#[serial_test::serial(catalog)]
fn handle_broadcast_activity_sends_to_subscriber() {
    let (mut state, _rx) = make_daemon_state();
    let (tx, rx) = test_sink();

    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx,
    });
    drain_send_on_subscribe(&rx);

    let msg = DaemonMessage::OutputChunk {
        session_id: 1,
        request_id: 5,
        stream: choreo_proto::OutputStream::Answer,
        data: b"hello".to_vec(),
    };
    state.handle_command(DaemonCommand::BroadcastActivity(msg.clone()));

    let received = rx.recv().unwrap();
    assert_eq!(received, msg);
    // Subscriber should still be registered
    assert!(state.activity_subscribers.contains_key(&10));
}

#[test]
#[serial_test::serial(catalog)]
fn handle_broadcast_activity_skips_dedup_for_session_subscriber() {
    let (mut state, _rx) = make_daemon_state();
    // Use a sync_channel with capacity 1 so we can detect if a message
    // was sent vs skipped.
    let (tx, rx) = test_sink();

    // Client 10 is both an activity subscriber AND a subscriber of session 1
    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx,
    });
    drain_send_on_subscribe(&rx);
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 1,
    });

    // Broadcast a message FROM session 1 — should be SKIPPED for client 10
    // because they're already a direct subscriber of session 1.
    let msg = DaemonMessage::OutputChunk {
        session_id: 1,
        request_id: 5,
        stream: choreo_proto::OutputStream::Answer,
        data: b"hello".to_vec(),
    };
    state.handle_command(DaemonCommand::BroadcastActivity(msg));

    // The client should NOT have received the message (it was suppressed
    // by the dedup filter).  The retain closure returned true, so the
    // subscriber remains registered.
    assert!(
        rx.try_recv().is_err(),
        "message should have been suppressed for session subscriber"
    );
    assert!(state.activity_subscribers.contains_key(&10));
}

#[test]
#[serial_test::serial(catalog)]
fn handle_broadcast_activity_no_dedup_for_different_session() {
    let (mut state, _rx) = make_daemon_state();
    let (tx, rx) = test_sink();

    // Client 10 subscribes to session 1, but the broadcast is about session 2
    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx,
    });
    drain_send_on_subscribe(&rx);
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 1,
    });

    // Broadcast a message FROM session 2 — client 10 is NOT a subscriber
    // of session 2, so the message should be delivered.
    let msg = DaemonMessage::OutputChunk {
        session_id: 2,
        request_id: 5,
        stream: choreo_proto::OutputStream::Answer,
        data: b"hello".to_vec(),
    };
    state.handle_command(DaemonCommand::BroadcastActivity(msg.clone()));

    let received = rx.recv().unwrap();
    assert_eq!(received, msg);
}

#[test]
#[serial_test::serial(catalog)]
fn handle_broadcast_activity_sends_when_no_session_id() {
    // Messages without a session_id (Models, Pong, etc.) should always
    // be delivered to all activity subscribers.
    let (mut state, _rx) = make_daemon_state();
    let (tx, rx) = test_sink();

    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx,
    });
    drain_send_on_subscribe(&rx);

    let msg = DaemonMessage::Models {
        models: vec!["gpt-4".into()],
        selected_model: Some("gpt-4".into()),
    };
    state.handle_command(DaemonCommand::BroadcastActivity(msg.clone()));

    let received = rx.recv().unwrap();
    assert_eq!(received, msg);
}

#[test]
#[serial_test::serial(catalog)]
fn handle_broadcast_activity_removes_disconnected_subscriber() {
    let (mut state, _rx) = make_daemon_state();
    // Use a sync_channel so we can drop the receiver
    let (tx, rx) = test_sink();

    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx,
    });

    // Drop the receiver to simulate a disconnected client
    drop(rx);

    // Broadcast should detect the dead subscriber and remove it
    let msg = DaemonMessage::SessionStatusChanged {
        session_id: 1,
        status: SessionStatus::Inactive,
        last_modified: 0,
    };
    state.handle_command(DaemonCommand::BroadcastActivity(msg));

    // Dead subscriber should be removed
    assert!(!state.activity_subscribers.contains_key(&10));
}

#[test]
#[serial_test::serial(catalog)]
fn handle_broadcast_activity_evicts_over_lag_subscriber() {
    // Lossless + lag-eviction: a subscriber whose queue crosses the lag
    // cap still receives the crossing message (never dropped), but is
    // then EVICTED — disconnecting is the price of never dropping. The
    // TUI's reconnect-and-resync (attach snapshot) is the recovery path
    // (a later phase); the daemon side is tested here.
    let (mut state, _rx) = make_daemon_state();
    state.lag_limits = LagLimits {
        per_client_cap: 16,
        global_budget: usize::MAX,
    };
    let (tx, rx) = test_sink();

    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx.clone(),
    });
    // The writer registry entry is what eviction tears down (the real
    // connection registers it at accept time).
    state.client_writers.insert(10, tx);
    // Drain the send-on-subscribe CatalogUpdated so the assertions below
    // only observe the broadcast.
    drain_send_on_subscribe(&rx);

    // The message crosses the tiny cap (OutputChunk payload ~70 bytes).
    let broadcast = DaemonMessage::OutputChunk {
        session_id: 7,
        request_id: 99,
        stream: choreo_proto::OutputStream::Answer,
        data: b"hello".to_vec(),
    };
    state.handle_command(DaemonCommand::BroadcastActivity(broadcast.clone()));

    // Lossless: the crossing message was delivered, not dropped.
    assert_eq!(rx.recv().unwrap(), broadcast);
    // …and the subscriber is evicted from every map.
    assert!(
        !state.activity_subscribers.contains_key(&10),
        "over-lag subscriber must be evicted from the activity map"
    );
    assert!(
        !state.client_writers.contains_key(&10),
        "over-lag subscriber must be evicted from the writer registry"
    );
}

#[test]
#[serial_test::serial(catalog)]
fn handle_broadcast_activity_handles_multiple_clients() {
    let (mut state, _rx) = make_daemon_state();
    let (tx1, rx1) = test_sink();
    let (tx2, rx2) = test_sink();

    // Client 10: activity subscriber + session 1 subscriber
    // Client 20: activity subscriber only
    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 10,
        writer: tx1,
    });
    state.handle_command(DaemonCommand::RegisterActivitySubscriber {
        client_id: 20,
        writer: tx2,
    });
    drain_send_on_subscribe(&rx1);
    drain_send_on_subscribe(&rx2);
    state.handle_command(DaemonCommand::TrackSessionSubscription {
        client_id: 10,
        session_id: 1,
    });

    let msg = DaemonMessage::OutputChunk {
        session_id: 1,
        request_id: 5,
        stream: choreo_proto::OutputStream::Answer,
        data: b"data".to_vec(),
    };
    state.handle_command(DaemonCommand::BroadcastActivity(msg.clone()));

    // Client 10 (session subscriber) should be skipped
    assert!(
        rx1.try_recv().is_err(),
        "client 10 is a session subscriber, should be suppressed"
    );
    // Client 20 (activity only) should receive the message
    let received = rx2.recv().unwrap();
    assert_eq!(received, msg);
}

// ── DaemonMessage::session_id() tests ───────────────────────────────

#[test]
fn session_id_returns_some_for_session_scoped_variants() {
    let cases: Vec<DaemonMessage> = vec![
        DaemonMessage::SessionCreated {
            session_id: 42,
            title: None,
            parent_session_id: None,
            working_dir: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        },
        DaemonMessage::SessionAttached { session_id: 42 },
        DaemonMessage::SessionState {
            session_id: 42,
            title: None,
            selected_model: None,
            parent_session_id: None,
            working_dir: None,
            turns: BTreeMap::new(),
            active_tool_groups: vec![],
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            status: SessionStatus::Inactive,
            reasoning_effort: None,
            reasoning_capability: None,
        },
        DaemonMessage::SessionStatusChanged {
            session_id: 42,
            status: SessionStatus::Inactive,
            last_modified: 0,
        },
        DaemonMessage::SessionFailed {
            session_id: 42,
            operation: "test".into(),
            error: "err".into(),
        },
        DaemonMessage::SessionDeleted { session_id: 42 },
        DaemonMessage::SessionDeleteFailed {
            session_id: 42,
            error: "err".into(),
        },
        DaemonMessage::TurnAppended {
            session_id: 42,
            turn_id: 1,
            turn: Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: None,
                assistant_text: None,
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
                reasoning_artifact: None,
                reasoning_producer: None,
            },
        },
        DaemonMessage::Started {
            session_id: 42,
            request_id: 1,
            turn_id: 0,
            estimated_prompt_tokens: 0,
        },
        DaemonMessage::OutputChunk {
            session_id: 42,
            request_id: 1,
            stream: choreo_proto::OutputStream::Answer,
            data: vec![],
        },
        DaemonMessage::Done {
            session_id: 42,
            request_id: 1,
            token_usage: None,
            last_prompt_tokens: None,
        },
        DaemonMessage::Failed {
            session_id: 42,
            request_id: 1,
            error: "e".into(),
        },
        DaemonMessage::Cancelled {
            session_id: 42,
            request_id: 1,
        },
        DaemonMessage::ModelSelected {
            session_id: 42,
            model: "gpt-4".into(),
            reasoning_capability: None,
        },
        DaemonMessage::ModelSelectionFailed {
            session_id: 42,
            model: "gpt-4".into(),
            error: "e".into(),
        },
        DaemonMessage::ReasoningEffortSet {
            session_id: 42,
            effort: "off".into(),
        },
        DaemonMessage::SessionAccountSet {
            session_id: 42,
            account: "test".into(),
        },
        DaemonMessage::ContextWindowResolved {
            session_id: 42,
            context_window: 128000,
        },
        DaemonMessage::SessionTitleSet {
            session_id: 42,
            title: "test".into(),
        },
    ];

    for msg in cases {
        assert_eq!(
            msg.session_id(),
            Some(42),
            "expected session_id=42 for {:?}",
            std::mem::discriminant(&msg)
        );
    }
}

#[test]
fn session_id_returns_none_for_non_session_variants() {
    let cases: Vec<DaemonMessage> = vec![
        DaemonMessage::Sessions { sessions: vec![] },
        DaemonMessage::Pong,
        DaemonMessage::Models {
            models: vec![],
            selected_model: None,
        },
        DaemonMessage::ModelsFailed { error: "e".into() },
        DaemonMessage::Unlocked,
        DaemonMessage::Locked,
        DaemonMessage::LockedError { error: "e".into() },
        DaemonMessage::CredentialAdded {
            service: "s".into(),
        },
        DaemonMessage::CredentialAddFailed {
            service: "s".into(),
            error: "e".into(),
        },
        DaemonMessage::Credential {
            service: "s".into(),
            key: None,
        },
        DaemonMessage::AccountAdded { name: "a".into() },
        DaemonMessage::Accounts { accounts: vec![] },
        DaemonMessage::ShuttingDown,
        DaemonMessage::Evicted,
    ];

    for msg in cases {
        assert!(
            msg.session_id().is_none(),
            "expected no session_id for {:?}",
            std::mem::discriminant(&msg)
        );
    }
}

#[test]
fn session_id_extracts_correct_value() {
    let msg = DaemonMessage::OutputChunk {
        session_id: 12345,
        request_id: 1,
        stream: choreo_proto::OutputStream::Answer,
        data: vec![],
    };
    assert_eq!(msg.session_id(), Some(12345));
}

// ── S4: /refresh-models + catalog swaps ────────────────────────────

/// The bundled catalog (embedded base + bundled overlay) — what
/// `PROVIDER_CATALOG` is lazily initialized from, and what the swap tests
/// restore.
fn bundled_catalog() -> Vec<choreo_ai_protocols::ProviderEntry> {
    merge_overlay(
        &choreo_ai_protocols::load_bundled_base(),
        bundled_overlay_src(),
    )
}

/// Restores the bundled catalog when dropped, so a failing swap test can
/// never leave the process-global catalog swapped for later tests (the
/// libtest fallback shares one process; nextest gives per-test processes
/// but the guard keeps the invariant anyway).
struct RestoreBundledCatalogOnDrop;

impl Drop for RestoreBundledCatalogOnDrop {
    fn drop(&mut self) {
        replace_catalog(bundled_catalog());
    }
}

/// A minimal one-provider base for the catalog-swap tests.
fn tiny_base() -> Vec<choreo_ai_protocols::ProviderEntry> {
    vec![choreo_ai_protocols::ProviderEntry {
        slug: "tiny-test".into(),
        display_name: "Tiny Test".into(),
        protocol: choreo_ai_protocols::ProviderProtocol::OpenAi {
            max_tokens_field: choreo_ai_protocols::MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://tiny.example/v1".into(),
        default_model: "tiny-1".into(),
        models: vec![choreo_ai_protocols::ModelEntry {
            model: "tiny-1".into(),
            context_window: 4096,
            reasoning_supported: true,
            openai_reasoning_levels: vec![],
            openai_responses: false,
            reasoning_passback: None,
        }],
    }]
}

#[test]
#[serial_test::serial(catalog)]
fn refresh_models_without_maintenance_thread_replies_error() {
    // A unit-test DaemonState has no maintenance thread; the handler must
    // reply with a structured error instead of hanging or panicking.
    let (mut state, _rx) = make_daemon_state();
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::RefreshModels {
        force: false,
        reply,
    });
    let result = rx.recv().unwrap();
    assert!(result.is_err(), "no maintenance thread → error reply");
    let err = result.unwrap_err();
    assert!(
        err.contains("maintenance thread"),
        "unexpected error: {err}"
    );
}

#[test]
#[serial_test::serial(catalog)]
fn refresh_models_with_dead_maintenance_thread_replies_error() {
    // A maintenance sender whose receiver is gone (the thread panicked)
    // must STILL produce a structured error reply: the client's
    // connection thread blocks in request_daemon until it hears
    // something, so dropping the reply silently would hang it forever.
    let (mut state, _rx) = make_daemon_state();
    let (maintenance_tx, maintenance_rx) = crossbeam_channel::unbounded::<MaintenanceEvent>();
    drop(maintenance_rx); // the maintenance thread is dead
    state.maintenance_tx = Some(maintenance_tx);
    let (reply, rx) = mpsc::channel();
    state.handle_command(DaemonCommand::RefreshModels {
        force: false,
        reply,
    });
    let result = rx.recv().unwrap();
    assert!(result.is_err(), "dead maintenance thread → error reply");
    let err = result.unwrap_err();
    assert!(
        err.contains("maintenance thread"),
        "unexpected error: {err}"
    );
}

#[test]
#[serial_test::serial(catalog)]
fn refresh_models_forwards_to_maintenance_thread() {
    // With a maintenance channel present, the handler hands the request
    // (force + reply) to the thread and does NOT fetch itself.
    let (mut state, _rx) = make_daemon_state();
    let (maintenance_tx, maintenance_rx) = crossbeam_channel::unbounded();
    state.maintenance_tx = Some(maintenance_tx);
    let (reply, _reply_rx) = mpsc::channel();

    state.handle_command(DaemonCommand::RefreshModels { force: true, reply });

    let msg = maintenance_rx.recv().unwrap();
    match msg {
        MaintenanceEvent::RefreshNow { force, .. } => assert!(force),
        other => panic!("expected RefreshNow, got {other:?}"),
    }
}

#[test]
#[serial_test::serial(catalog)]
fn catalog_base_changed_swaps_broadcasts_and_replies() {
    let _restore = RestoreBundledCatalogOnDrop;
    let (mut state, _rx) = make_daemon_state();
    let (writer_tx, writer_rx) = test_sink();
    state.activity_subscribers.insert(1, writer_tx);
    let (reply, reply_rx) = mpsc::channel();

    state.handle_command(DaemonCommand::CatalogBaseChanged {
        base: tiny_base(),
        etag: Some("\"v42\"".into()),
        user_overlay: None,
        persist: false,
        reply: vec![RefreshRequester {
            force: false,
            tx: reply,
        }],
    });

    // The catalog was swapped: the tiny provider is now visible.
    assert_eq!(
        choreo_ai_protocols::lookup_provider("tiny-test")
            .expect("swapped catalog")
            .slug,
        "tiny-test"
    );
    // Activity subscribers got the CatalogUpdated broadcast.
    let broadcast = writer_rx.recv().unwrap();
    assert!(matches!(
        &broadcast,
        DaemonMessage::CatalogUpdated { providers } if providers.iter().any(|p| p.slug == "tiny-test")
    ));
    // The requester got a RefreshReport with the merged counts. The
    // merged catalog is tiny-test + the bundled overlay's wholesale
    // providers (ollama, kimi-code, custom-*, …), so it is strictly
    // larger than the 1-provider base.
    let report = reply_rx.recv().unwrap().expect("refresh succeeds");
    assert!(report.providers > 1, "overlay-only providers must survive");
    assert!(report.models >= 1);
    assert_eq!(report.status, RefreshStatus::Updated);
}

#[test]
#[serial_test::serial(catalog)]
fn catalog_base_changed_user_overlay_merges_on_top() {
    let _restore = RestoreBundledCatalogOnDrop;
    let (mut state, _rx) = make_daemon_state();
    // A user overlay that renames the tiny provider's display name and
    // adds a brand-new provider must win over the base.
    let overlay = r#"
[provider.tiny-test]
display_name = "Renamed By User"

[provider.user-only]
display_name = "User Only"
protocol = "openai"
base_url = "https://user.example/v1"
default_model = "u-1"

[provider.user-only.models."u-1"]
context_window = 1024
"#;
    state.handle_command(DaemonCommand::CatalogBaseChanged {
        base: tiny_base(),
        etag: None,
        user_overlay: Some(overlay.to_string()),
        persist: false,
        reply: Vec::new(),
    });

    let renamed = choreo_ai_protocols::lookup_provider("tiny-test").expect("tiny-test present");
    assert_eq!(renamed.display_name, "Renamed By User");
    let user_only =
        choreo_ai_protocols::lookup_provider("user-only").expect("user overlay provider");
    assert_eq!(user_only.display_name, "User Only");
    assert_eq!(user_only.models.len(), 1);
}

#[test]
#[serial_test::serial(catalog)]
fn catalog_base_changed_empty_base_still_yields_overlay_only_providers() {
    let _restore = RestoreBundledCatalogOnDrop;
    let (mut state, _rx) = make_daemon_state();
    // An empty base (a broken fetch that slipped past the maintenance
    // thread's validation) still merges to a NON-empty catalog: the
    // bundled overlay defines the wholesale overlay-only providers, so
    // the daemon swaps in those and replies Ok with their counts. The
    // `effective.is_empty()` guard is belt-and-suspenders on top of that
    // (defensive; unreachable while the bundled overlay is non-empty).
    let (reply, reply_rx) = mpsc::channel();
    state.handle_command(DaemonCommand::CatalogBaseChanged {
        base: Vec::new(),
        etag: None,
        user_overlay: None,
        persist: false,
        reply: vec![RefreshRequester {
            force: false,
            tx: reply,
        }],
    });
    let report = reply_rx.recv().unwrap().expect("refresh succeeds");
    assert!(report.providers > 1, "overlay-only providers must survive");
    assert_eq!(report.status, RefreshStatus::Updated);
    // The overlay-only providers are actually queryable now.
    assert!(choreo_ai_protocols::lookup_provider("ollama").is_some());
}

#[test]
#[serial_test::serial(catalog)]
fn catalog_not_modified_replies_up_to_date_with_current_counts() {
    // A 304 routes through the command loop (the maintenance thread never
    // replies directly) and reports UpToDate with the CURRENT catalog's
    // counts — even for a requester that asked for --force, because the
    // server said nothing changed.
    let (mut state, _rx) = make_daemon_state();
    let (reply, reply_rx) = mpsc::channel();

    state.handle_command(DaemonCommand::CatalogNotModified {
        reply: vec![RefreshRequester {
            force: true,
            tx: reply,
        }],
    });

    let report = reply_rx.recv().unwrap().expect("304 reply is Ok");
    assert_eq!(report.status, RefreshStatus::UpToDate);
    assert!(
        report.providers > 0,
        "counts come from the currently swapped catalog"
    );
}

#[test]
fn send_catalog_reply_individualizes_status_per_requester() {
    // A mixed coalesced burst: the shared fetch is forced, but only the
    // requester that asked for --force gets Forced; the plain requester
    // folded into the burst is reported Updated.
    let (forced_tx, forced_rx) = mpsc::channel();
    let (plain_tx, plain_rx) = mpsc::channel();

    send_catalog_reply(
        vec![
            RefreshRequester {
                force: true,
                tx: forced_tx,
            },
            RefreshRequester {
                force: false,
                tx: plain_tx,
            },
        ],
        208,
        1234,
    );

    let forced = forced_rx.recv().unwrap().expect("forced reply is Ok");
    assert_eq!(forced.status, RefreshStatus::Forced);
    assert_eq!((forced.providers, forced.models), (208, 1234));
    let plain = plain_rx.recv().unwrap().expect("plain reply is Ok");
    assert_eq!(plain.status, RefreshStatus::Updated);
}

#[test]
#[serial_test::serial(catalog)]
fn activity_subscriber_gets_current_provider_list_on_register() {
    // A freshly-subscribed client must receive the CURRENT provider list
    // immediately (send-on-subscribe), so the TUI's picker tracks the live
    // catalog even when it connects after the startup swap broadcast.
    let (mut state, _rx) = make_daemon_state();
    let (writer_tx, writer_rx) = test_sink();

    state.handle_register_activity_subscriber(1, writer_tx);

    let msg = writer_rx.recv().unwrap();
    match &msg {
        DaemonMessage::CatalogUpdated { providers } => {
            assert!(!providers.is_empty());
            assert!(providers.iter().any(|p| p.slug == "openai"));
        }
        other => panic!("expected CatalogUpdated, got {other:?}"),
    }
}
