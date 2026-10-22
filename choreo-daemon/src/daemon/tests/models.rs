//! Unit tests for the model-list handling in `daemon/models.rs`: the prefetch
//! gate and its in-flight dedup guard, the prefetch result's cache insert /
//! broadcast / discard paths, the `ListModels` never-block contract (stale
//! cache, catalog seed, "warming" error), `ValidateModel`, and the catalog seed
//! helper `catalog_models_for_account`.
//!
//! The fixtures shared with the parent (`make_daemon_state`,
//! `seed_credentialed_account`, `seed_credentialed_account_with_url`,
//! `dead_base_url`, `put_client`, `test_sink`) come in via `super::*`; the
//! moved items themselves are named explicitly below.

use super::super::models::{MODEL_CACHE_TTL, catalog_models_for_account};
use super::*;

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
            pinned: false,
            archived_at: None,
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
            pinned: false,
            archived_at: None,
        },
    );
    seed_credentialed_account(&mut state, "test-account", "openai");
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
            pinned: false,
            archived_at: None,
        },
    );
    seed_credentialed_account(&mut state, "test-account", "openai");
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

// ── Background model prefetch ────────────────────────────────────────────

#[test]
fn should_prefetch_models_gates_on_account_flight_and_freshness() {
    let (mut state, _rx) = make_daemon_state();

    // No credentialed account → nothing to prefetch.
    assert!(!state.should_prefetch_models("acct"));

    // Account exists WITH a credential, no cache → prefetch needed.
    seed_credentialed_account(&mut state, "acct", "openai");
    assert!(state.should_prefetch_models("acct"));

    // In-flight → no duplicate prefetch (the dedup guard).
    state.model_prefetch_in_flight.insert("acct".into());
    assert!(!state.should_prefetch_models("acct"));
    state.model_prefetch_in_flight.clear();

    // Fresh cache (inside MODEL_CACHE_TTL) → no prefetch.
    state
        .model_cache
        .insert("acct".into(), (vec!["m".into()], Instant::now()));
    assert!(!state.should_prefetch_models("acct"));

    // Stale cache (past MODEL_CACHE_TTL) → prefetch again.
    state.model_cache.insert(
        "acct".into(),
        (
            vec!["m".into()],
            // checked_sub instead of `-`: Instant - Duration can panic on
            // underflow (denied lint); the unwrap is test-allowed.
            Instant::now()
                .checked_sub(MODEL_CACHE_TTL)
                .unwrap()
                .checked_sub(Duration::from_secs(1))
                .unwrap(),
        ),
    );
    assert!(state.should_prefetch_models("acct"));
}

#[test]
fn handle_model_prefetch_result_success_populates_cache_and_releases_guard() {
    let (mut state, _rx) = make_daemon_state();
    // A credentialed account is the real precondition for a prefetch (the
    // spawn gate requires one), and the handler discards results for
    // accounts whose credential vanished mid-flight.
    seed_credentialed_account(&mut state, "acct", "openai");
    state.model_prefetch_in_flight.insert("acct".into());

    state.handle_command(DaemonCommand::ModelPrefetchResult {
        account: "acct".into(),
        result: Ok(vec!["m1".into(), "m2".into()]),
    });

    // The guard must be released so a later stale-cache join re-prefetches.
    assert!(!state.model_prefetch_in_flight.contains("acct"));
    let (models, cached_at) = state.model_cache.get("acct").expect("cache populated");
    assert_eq!(models, &["m1".to_string(), "m2".to_string()]);
    // Freshness stamp is "now": within a TTL of the handler running.
    assert!(cached_at.elapsed() < MODEL_CACHE_TTL);
}

#[test]
fn handle_model_prefetch_result_success_broadcasts_models_updated() {
    // When the background fetch lands, an already-open picker must learn the
    // fresh list without the user reopening it — so the result is broadcast as
    // `ModelsUpdated` (keyed by account) alongside the cache insert.
    let (mut state, _rx) = make_daemon_state();
    seed_credentialed_account(&mut state, "acct", "openai");
    state.model_prefetch_in_flight.insert("acct".into());
    let (writer_tx, writer_rx) = test_sink();
    put_client(&mut state, 1, &writer_tx, false, true, &[]);

    state.handle_command(DaemonCommand::ModelPrefetchResult {
        account: "acct".into(),
        result: Ok(vec!["m1".into(), "m2".into()]),
    });

    let msg = writer_rx.recv().unwrap();
    match &msg.inner {
        DaemonMessageType::ModelsUpdated { account, models } => {
            assert_eq!(account, "acct");
            assert_eq!(models, &["m1".to_string(), "m2".to_string()]);
        }
        other => panic!("expected ModelsUpdated, got {other:?}"),
    }
}

#[test]
#[serial_test::serial(catalog)]
fn catalog_models_for_account_resolves_known_provider_and_rejects_unknown() {
    // The instant-first-paint seed: a catalog-known provider yields its curated
    // model ids; an unknown provider (or unknown account) yields None so the
    // caller falls back to the retryable "warming" error instead of an empty
    // list.
    let (mut state, _rx) = make_daemon_state();
    seed_credentialed_account(&mut state, "acct", "openai");
    let seeded = catalog_models_for_account(&state, "acct").expect("openai is in the catalog");
    assert!(
        !seeded.is_empty(),
        "the openai catalog entry carries curated model ids"
    );

    seed_credentialed_account(&mut state, "other", "not-in-catalog");
    assert!(catalog_models_for_account(&state, "other").is_none());
    assert!(catalog_models_for_account(&state, "ghost").is_none());
}

#[test]
fn handle_model_prefetch_result_failure_releases_guard_without_caching() {
    let (mut state, _rx) = make_daemon_state();
    state.model_prefetch_in_flight.insert("acct".into());

    state.handle_command(DaemonCommand::ModelPrefetchResult {
        account: "acct".into(),
        result: Err("provider unreachable".into()),
    });

    // A failed fetch must NOT wedge the account: guard released, cache
    // untouched, so the next session join retries the prefetch and the
    // on-demand ListModels path serves stale data or a retryable "warming"
    // error in the meantime.
    assert!(!state.model_prefetch_in_flight.contains("acct"));
    assert!(!state.model_cache.contains_key("acct"));
}

#[test]
fn update_metadata_account_change_spawns_background_prefetch() {
    // The account is seeded against a dead local port, so the spawned
    // prefetch thread fails instantly (ECONNREFUSED, no network) and sends
    // its result back over the daemon channel right away.
    let (mut state, rx) = make_daemon_state();
    seed_credentialed_account_with_url(&mut state, "acct", "openai", Some(dead_base_url()));
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: Some("s".into()),
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
            pinned: false,
            archived_at: None,
        },
    );

    // Attach an account: a real change → prefetch spawned, guard set.
    let mut meta = state.session_metadata.get(&1).unwrap().clone();
    meta.account_name = Some("acct".into());
    state.handle_command(DaemonCommand::UpdateMetadata {
        session_id: 1,
        metadata: meta.clone(),
    });
    assert!(
        state.model_prefetch_in_flight.contains("acct"),
        "account change must spawn a prefetch"
    );

    // The fetch thread reports back through the command channel; feed the
    // message through the command loop like the real loop would, which
    // releases the in-flight guard and records the failure.
    let msg = rx.recv().unwrap();
    assert!(
        matches!(
            &msg,
            DaemonCommand::ModelPrefetchResult { account, result }
                if account == "acct" && result.is_err()
        ),
        "expected ModelPrefetchResult for 'acct' with Err (failing provider)"
    );
    state.handle_command(msg);
    assert!(!state.model_prefetch_in_flight.contains("acct"));

    // Repeating the SAME account on the next request (the common
    // UpdateMetadata-per-request case) must NOT spawn another prefetch: the
    // account didn't CHANGE between the stored metadata and this update,
    // even though the guard is clear and the cache is empty.
    state.handle_command(DaemonCommand::UpdateMetadata {
        session_id: 1,
        metadata: meta.clone(),
    });
    assert!(
        rx.try_recv().is_err(),
        "no prefetch thread may be spawned for an unchanged account"
    );
}

#[test]
fn create_session_with_account_spawns_background_prefetch() {
    let (mut state, _rx) = make_daemon_state();
    seed_credentialed_account_with_url(&mut state, "acct", "openai", Some(dead_base_url()));
    let (reply, rx) = mpsc::channel();

    state.handle_command(DaemonCommand::CreateSession {
        title: None,
        parent_session_id: None,
        working_dir: None,
        reasoning_effort: None,
        selected_model: None,
        context_config: None,
        account_name: Some("acct".into()),
        active_tool_groups: Vec::new(),
        reply,
    });
    rx.recv().unwrap().expect("session created");

    assert!(
        state.model_prefetch_in_flight.contains("acct"),
        "session create with an account must spawn a prefetch"
    );
}

// ── ListModels never blocks the command loop ─────────────────────────────

/// Build a state whose session 1 points at `account`, backed by a
/// credentialed account whose endpoint is a dead local port (instant fetch
/// failure). Shared by the `ListModels` prefetch tests to keep the verbose
/// `SessionMetadata` boilerplate in one place.
fn state_with_session_account(
    account: &str,
) -> (DaemonState, crossbeam_channel::Receiver<DaemonCommand>) {
    state_with_session_account_provider(account, "openai")
}

/// Like [`state_with_session_account`], but the account's provider slug is
/// caller-chosen — used to exercise the "no catalog seed for this provider"
/// branch (a slug unknown to the catalog).
fn state_with_session_account_provider(
    account: &str,
    provider: &str,
) -> (DaemonState, crossbeam_channel::Receiver<DaemonCommand>) {
    let (mut state, rx) = make_daemon_state();
    seed_credentialed_account_with_url(&mut state, account, provider, Some(dead_base_url()));
    state.session_metadata.insert(
        1,
        SessionMetadata {
            title: Some("s".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            last_modified: 1000,
            turn_count: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: Some(account.to_string()),
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
            pinned: false,
            archived_at: None,
        },
    );
    (state, rx)
}

#[test]
fn list_models_serves_stale_cache_without_duplicate_fetch_while_prefetch_in_flight() {
    // In-flight prefetch + a stale cache: the on-demand path must serve the
    // stale list and NOT spawn a second (duplicate) HTTP fetch behind the
    // running one.
    let (mut state, rx) = state_with_session_account("acct");
    state.model_cache.insert(
        "acct".into(),
        (
            vec!["old-model".into()],
            // checked_sub instead of `-`: Instant - Duration can panic on
            // underflow (denied lint); the unwrap is test-allowed.
            Instant::now()
                .checked_sub(MODEL_CACHE_TTL)
                .unwrap()
                .checked_sub(Duration::from_secs(1))
                .unwrap(),
        ),
    );
    state.model_prefetch_in_flight.insert("acct".into());

    let (models, _) = state
        .handle_list_models(Some(1))
        .expect("stale list served");
    assert_eq!(models, vec!["old-model".to_string()]);

    // No second fetch may have been spawned for the in-flight account.
    assert!(rx.try_recv().is_err(), "no duplicate prefetch spawned");
}

#[test]
#[serial_test::serial(catalog)]
fn list_models_with_cold_cache_serves_catalog_seed_and_warms_background() {
    // Cold cache, no prefetch running: the fetch must be handed to the
    // background thread (never the command loop), AND the picker must paint
    // instantly from the local catalog's curated ids for the account's
    // provider instead of blocking on the (potentially slow) live fetch.
    let (mut state, rx) = state_with_session_account("acct");

    let (models, _) = state
        .handle_list_models(Some(1))
        .expect("catalog seed served");
    assert!(
        !models.is_empty(),
        "the catalog seeds a known provider (openai) so the picker is never empty while warming"
    );
    assert!(
        state.model_prefetch_in_flight.contains("acct"),
        "a background prefetch must still be spawned to refresh the seed"
    );

    // The spawned fetch reports back through the command channel; feed it
    // through the loop like the real command loop would.
    let msg = rx.recv().unwrap();
    assert!(matches!(
        &msg,
        DaemonCommand::ModelPrefetchResult { account, result }
            if account == "acct" && result.is_err()
    ));
    state.handle_command(msg);
    assert!(!state.model_prefetch_in_flight.contains("acct"));
    // The (dead-endpoint) fetch failed, so nothing was cached; the catalog
    // seed will be served again on the next request until a fetch lands.
    assert!(!state.model_cache.contains_key("acct"));
}

#[test]
#[serial_test::serial(catalog)]
fn list_models_with_no_cache_and_no_catalog_models_reports_warming() {
    // An account whose provider is unknown to the catalog has no seed to paint:
    // the caller gets the retryable "warming" error so it can ask again once
    // the live fetch lands (rather than an empty list that looks like "no
    // models").
    let (mut state, _rx) = state_with_session_account_provider("acct", "not-in-catalog");

    let err = state
        .handle_list_models(Some(1))
        .expect_err("no seed → warming");
    assert!(err.contains("warming"), "unexpected error: {err}");
}

#[test]
fn list_models_with_stale_cache_and_no_prefetch_serves_stale_and_warms_background() {
    // Stale-but-present cache, nothing in flight: serve the stale list (it
    // beats nothing) AND kick off a background refresh.
    let (mut state, rx) = state_with_session_account("acct");
    state.model_cache.insert(
        "acct".into(),
        (
            vec!["old-model".into()],
            // checked_sub instead of `-`: Instant - Duration can panic on
            // underflow (denied lint); the unwrap is test-allowed.
            Instant::now()
                .checked_sub(MODEL_CACHE_TTL)
                .unwrap()
                .checked_sub(Duration::from_secs(1))
                .unwrap(),
        ),
    );

    let (models, _) = state
        .handle_list_models(Some(1))
        .expect("stale list served");
    assert_eq!(models, vec!["old-model".to_string()]);
    assert!(
        state.model_prefetch_in_flight.contains("acct"),
        "stale cache must trigger a background refresh"
    );
    // Drain the spawned fetch's result. A blocking recv (not a timed wait)
    // is deterministic here: the failing provider errors instantly and the
    // thread always sends exactly one message.
    let _ = rx.recv().unwrap();
}

// ── Prefetch guard robustness ─────────────────────────────────────────

#[test]
fn prefetch_result_for_removed_account_is_discarded_not_cached() {
    // The account's credential was removed while the fetch was in flight
    // (RemoveCredential / a rebuild from AccountsReload): the result must
    // not populate the cache — a dead account's list would otherwise be
    // served for a full TTL.
    let (mut state, _rx) = make_daemon_state();
    state.model_prefetch_in_flight.insert("acct".into());
    // No provider for "acct" — it was removed mid-flight.

    state.handle_command(DaemonCommand::ModelPrefetchResult {
        account: "acct".into(),
        result: Ok(vec!["stale".into()]),
    });

    assert!(!state.model_prefetch_in_flight.contains("acct"));
    assert!(!state.model_cache.contains_key("acct"), "result discarded");
}
