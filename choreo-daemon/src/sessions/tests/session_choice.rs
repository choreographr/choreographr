//! `handle_set_session_choice`: the atomic account/model/effort commit.
//!
//! Each test drives the real handler over a fake daemon thread that serves the
//! two lookups it makes — `ResolveAccountCmd` and `ValidateModel` — and asserts
//! both the session state and what reached a subscriber. Nothing here waits on a
//! timer: the handler's blocking `recv` is the only wait, and the fake daemon
//! always answers.

use super::*;
use crate::broadcast::test_sink;
use std::thread;

/// One account the fake daemon knows about, with the model ids its cache holds
/// (`None` means nothing is cached, which is the fail-open case).
struct FakeAccount {
    name: &'static str,
    slug: &'static str,
    cached_models: Option<Vec<&'static str>>,
}

/// Build a `RequestContext` whose daemon answers `ResolveAccountCmd` and
/// `ValidateModel` from `accounts`, plus the serving thread's handle.
///
/// The fake daemon reproduces the real handlers' two relevant rules exactly: an
/// account it does not know resolves to `None`, and a model is rejected only
/// when a non-empty cached list exists and does not contain it.
fn choice_setup(accounts: Vec<FakeAccount>) -> (RequestContext, thread::JoinHandle<()>) {
    let db = test_db();
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry: crate::tools::ToolRegistry::new().build().into_shared(),
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };
    let server = thread::spawn(move || {
        while let Ok(command) = daemon_rx.recv() {
            match command {
                DaemonCommand::ResolveAccountCmd { account, reply } => {
                    let resolved =
                        accounts
                            .iter()
                            .find(|known| known.name == account)
                            .map(|known| crate::daemon::ResolvedAccount {
                                config: crate::accounts::AccountConfig::simple(
                                    known.name, known.slug,
                                ),
                                // No credential: the handler must still record the
                                // slug and apply the choice, exactly as it does for a
                                // locked keystore.
                                auth: None,
                                warm_policy: crate::cache_warm::WarmPolicy::default(),
                            });
                    let _ = reply.send(resolved);
                }
                DaemonCommand::ValidateModel {
                    account,
                    model,
                    reply,
                    ..
                } => {
                    let cached = accounts
                        .iter()
                        .find(|known| Some(known.name) == account.as_deref())
                        .and_then(|known| known.cached_models.as_ref());
                    let result = match cached {
                        Some(ids) if !ids.is_empty() => {
                            if ids.iter().any(|id| id.eq_ignore_ascii_case(&model)) {
                                Ok(())
                            } else {
                                Err(format!(
                                    "model '{model}' not found. Available: {}",
                                    ids.join(", ")
                                ))
                            }
                        }
                        // Fail-open: nothing cached is no evidence of anything.
                        _ => Ok(()),
                    };
                    let _ = reply.send(result);
                }
                // `UpdateMetadata` from `persist_session_metadata` and anything
                // else: nothing to answer.
                _ => {}
            }
        }
    });
    (ctx, server)
}

/// A sink subscribed to the session, plus the reply target's own sink, so a test
/// can assert both the broadcasts and the targeted terminal reply.
fn subscriber(
    state: &mut SessionState,
) -> (SubscriberSink, crossbeam_channel::Receiver<DaemonMessage>) {
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    (sink, rx)
}

/// The `SessionEvent`s a session's subscriber received, in arrival order.
fn events(rx: &crossbeam_channel::Receiver<DaemonMessage>) -> Vec<SessionEvent> {
    rx.try_iter()
        .map(|message| match message.inner {
            DaemonMessageType::Session { event, .. } => event,
            other => panic!("expected a session-scoped event, got {other:?}"),
        })
        .collect()
}

/// The single targeted reply a request answered with.
fn reply_of(rx: &crossbeam_channel::Receiver<DaemonMessage>) -> DaemonMessageType {
    let replies: Vec<DaemonMessage> = rx.try_iter().filter(|m| m.id.is_some()).collect();
    assert_eq!(replies.len(), 1, "exactly one terminal reply");
    replies[0].inner.clone()
}

#[test]
fn unknown_account_is_rejected_and_the_session_is_untouched() {
    let (ctx, server) = choice_setup(Vec::new());
    let mut state = SessionState::empty();
    let (sink, rx) = subscriber(&mut state);
    let (reply_tx, reply_rx) = test_sink();
    let target = ReplyTarget::new(
        7,
        MessageKind::SetSessionChoice,
        reply_tx,
        Arc::clone(&ctx.global_lag),
    );

    let mut shutdown = false;
    process_command(
        SessionCommand::SetSessionChoice {
            account: "ghost".into(),
            model: "gpt-5.4".into(),
            effort: "medium".into(),
            reply: Some(target),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(
        reply_of(&reply_rx),
        DaemonMessageType::Failed {
            kind: MessageKind::SetSessionChoice,
            error: "account 'ghost' not found".into(),
        }
    );
    assert!(
        events(&rx).is_empty(),
        "a rejected choice broadcasts nothing"
    );
    assert_eq!(state.config.account_name, None);
    assert_eq!(state.config.selected_model, None);
    assert_eq!(state.config.reasoning_effort, None);
    assert_eq!(state.provider_slug, None);
    drop(sink);
    drop(ctx);
    server.join().expect("fake daemon exits");
}

#[test]
fn a_model_missing_from_the_cached_list_is_rejected() {
    let (ctx, server) = choice_setup(vec![FakeAccount {
        name: "work",
        slug: "openai",
        cached_models: Some(vec!["gpt-5.4", "gpt-4.1"]),
    }]);
    let mut state = SessionState::empty();
    let (sink, rx) = subscriber(&mut state);
    let (reply_tx, reply_rx) = test_sink();
    let target = ReplyTarget::new(
        8,
        MessageKind::SetSessionChoice,
        reply_tx,
        Arc::clone(&ctx.global_lag),
    );

    let mut shutdown = false;
    process_command(
        SessionCommand::SetSessionChoice {
            account: "work".into(),
            model: "not-a-model".into(),
            effort: "off".into(),
            reply: Some(target),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    match reply_of(&reply_rx) {
        DaemonMessageType::Failed { kind, error } => {
            assert_eq!(kind, MessageKind::SetSessionChoice);
            assert!(
                error.contains("not-a-model"),
                "the failure names the model: {error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        events(&rx).is_empty(),
        "a rejected choice broadcasts nothing"
    );
    assert_eq!(state.config.account_name, None, "the account was not bound");
    drop(sink);
    drop(ctx);
    server.join().expect("fake daemon exits");
}

#[test]
fn a_model_is_accepted_when_nothing_is_cached_for_the_account() {
    // Fail-open: an account whose list could not be fetched is no evidence that
    // the model is invalid, so the choice must still commit.
    let (ctx, server) = choice_setup(vec![FakeAccount {
        name: "work",
        slug: "openai",
        cached_models: None,
    }]);
    let mut state = SessionState::empty();
    let (sink, rx) = subscriber(&mut state);
    let (reply_tx, reply_rx) = test_sink();
    let target = ReplyTarget::new(
        9,
        MessageKind::SetSessionChoice,
        reply_tx,
        Arc::clone(&ctx.global_lag),
    );

    let mut shutdown = false;
    process_command(
        SessionCommand::SetSessionChoice {
            account: "work".into(),
            model: "gpt-5.4".into(),
            effort: "medium".into(),
            reply: Some(target),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(
        reply_of(&reply_rx),
        DaemonMessageType::Accepted {
            kind: MessageKind::SetSessionChoice
        }
    );
    assert_eq!(state.config.account_name.as_deref(), Some("work"));
    assert_eq!(state.config.selected_model.as_deref(), Some("gpt-5.4"));
    assert_eq!(state.config.reasoning_effort.as_deref(), Some("medium"));
    let _ = events(&rx);
    drop(sink);
    drop(ctx);
    server.join().expect("fake daemon exits");
}

#[test]
fn an_unsupported_effort_is_rejected() {
    // `gpt-5.4` supports off/low/medium/high/xhigh — "max" is not one of them.
    let (ctx, server) = choice_setup(vec![FakeAccount {
        name: "work",
        slug: "openai",
        cached_models: Some(vec!["gpt-5.4"]),
    }]);
    let mut state = SessionState::empty();
    let (sink, rx) = subscriber(&mut state);
    let (reply_tx, reply_rx) = test_sink();
    let target = ReplyTarget::new(
        10,
        MessageKind::SetSessionChoice,
        reply_tx,
        Arc::clone(&ctx.global_lag),
    );

    let mut shutdown = false;
    process_command(
        SessionCommand::SetSessionChoice {
            account: "work".into(),
            model: "gpt-5.4".into(),
            effort: "max".into(),
            reply: Some(target),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    match reply_of(&reply_rx) {
        DaemonMessageType::Failed { kind, error } => {
            assert_eq!(kind, MessageKind::SetSessionChoice);
            assert_eq!(
                error,
                "model 'gpt-5.4' does not support reasoning effort 'max'"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(events(&rx), Vec::<SessionEvent>::new());
    assert_eq!(state.config.selected_model, None);
    drop(sink);
    drop(ctx);
    server.join().expect("fake daemon exits");
}

#[test]
fn a_non_off_effort_is_rejected_when_the_model_has_no_levels() {
    // `gpt-4.1` supports no reasoning (the catalog declares an empty level
    // set), so "off" is the only valid effort.
    let (ctx, server) = choice_setup(vec![FakeAccount {
        name: "work",
        slug: "openai",
        cached_models: Some(vec!["gpt-4.1"]),
    }]);
    let mut state = SessionState::empty();
    let (sink, rx) = subscriber(&mut state);
    let (reply_tx, reply_rx) = test_sink();
    let target = ReplyTarget::new(
        11,
        MessageKind::SetSessionChoice,
        reply_tx,
        Arc::clone(&ctx.global_lag),
    );

    let mut shutdown = false;
    process_command(
        SessionCommand::SetSessionChoice {
            account: "work".into(),
            model: "gpt-4.1".into(),
            effort: "low".into(),
            reply: Some(target),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    match reply_of(&reply_rx) {
        DaemonMessageType::Failed { error, .. } => assert!(
            error.contains("does not support reasoning effort 'low'"),
            "the rejection names the effort: {error}"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(events(&rx), Vec::<SessionEvent>::new());

    // The same model with "off" is accepted: reasoning-free models still commit.
    let (reply_tx, reply_rx) = test_sink();
    let target = ReplyTarget::new(
        12,
        MessageKind::SetSessionChoice,
        reply_tx,
        Arc::clone(&ctx.global_lag),
    );
    process_command(
        SessionCommand::SetSessionChoice {
            account: "work".into(),
            model: "gpt-4.1".into(),
            effort: "off".into(),
            reply: Some(target),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );
    assert_eq!(
        reply_of(&reply_rx),
        DaemonMessageType::Accepted {
            kind: MessageKind::SetSessionChoice
        }
    );
    assert_eq!(state.config.reasoning_effort.as_deref(), Some("off"));
    drop(sink);
    drop(ctx);
    server.join().expect("fake daemon exits");
}

#[test]
fn a_valid_choice_applies_everything_at_once_and_broadcasts_it_in_order() {
    let (ctx, server) = choice_setup(vec![FakeAccount {
        name: "work",
        slug: "openai",
        cached_models: Some(vec!["gpt-5.4"]),
    }]);
    let mut state = SessionState::empty();
    let (sink, rx) = subscriber(&mut state);
    let (reply_tx, reply_rx) = test_sink();
    let target = ReplyTarget::new(
        13,
        MessageKind::SetSessionChoice,
        reply_tx,
        Arc::clone(&ctx.global_lag),
    );

    let mut shutdown = false;
    process_command(
        SessionCommand::SetSessionChoice {
            account: "work".into(),
            model: "gpt-5.4".into(),
            effort: "medium".into(),
            reply: Some(target),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(
        reply_of(&reply_rx),
        DaemonMessageType::Accepted {
            kind: MessageKind::SetSessionChoice
        }
    );

    // The whole choice landed in the session config in one step.
    assert_eq!(state.config.account_name.as_deref(), Some("work"));
    assert_eq!(state.config.selected_model.as_deref(), Some("gpt-5.4"));
    assert_eq!(state.config.reasoning_effort.as_deref(), Some("medium"));
    assert_eq!(state.provider_slug.as_deref(), Some("openai"));
    assert_eq!(
        state.config.context_window,
        Some(1_050_000),
        "the model's catalog context window resolved with the choice"
    );

    // One persistence write carried all three fields.
    let persisted = crate::db::read_session(&ctx.db, ctx.session_id)
        .expect("read the persisted record")
        .expect("the session record was written");
    assert_eq!(persisted.account_name.as_deref(), Some("work"));
    assert_eq!(persisted.selected_model.as_deref(), Some("gpt-5.4"));
    assert_eq!(persisted.reasoning_effort.as_deref(), Some("medium"));

    // Account first, then the model it applies to, then the effort, then the
    // context window the model resolved.
    let events = events(&rx);
    assert_eq!(
        events.iter().map(SessionEvent::kind).collect::<Vec<_>>(),
        [
            "SessionAccountSet",
            "ModelSelected",
            "ReasoningEffortSet",
            "ContextWindowResolved"
        ]
    );
    assert!(matches!(
        &events[0],
        SessionEvent::SessionAccountSet { account } if account == "work"
    ));
    assert!(matches!(
        &events[1],
        SessionEvent::ModelSelected { model, reasoning_capability: Some(capability) }
            if model == "gpt-5.4"
                && capability.available_effort_levels.iter().any(|level| level == "medium")
    ));
    assert!(matches!(
        &events[2],
        SessionEvent::ReasoningEffortSet { effort } if effort == "medium"
    ));
    drop(sink);
    drop(ctx);
    server.join().expect("fake daemon exits");
}

#[test]
fn re_choosing_the_current_choice_succeeds_as_a_no_op() {
    let (ctx, server) = choice_setup(vec![FakeAccount {
        name: "work",
        slug: "openai",
        cached_models: Some(vec!["gpt-5.4"]),
    }]);
    let mut state = SessionState::empty();
    state.config.account_name = Some("work".into());
    state.config.selected_model = Some("gpt-5.4".into());
    state.config.reasoning_effort = Some("medium".into());
    let (sink, rx) = subscriber(&mut state);
    let (reply_tx, reply_rx) = test_sink();
    let target = ReplyTarget::new(
        14,
        MessageKind::SetSessionChoice,
        reply_tx,
        Arc::clone(&ctx.global_lag),
    );

    let mut shutdown = false;
    process_command(
        SessionCommand::SetSessionChoice {
            account: "work".into(),
            model: "gpt-5.4".into(),
            effort: "medium".into(),
            reply: Some(target),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(
        reply_of(&reply_rx),
        DaemonMessageType::Accepted {
            kind: MessageKind::SetSessionChoice
        }
    );
    assert_eq!(state.config.account_name.as_deref(), Some("work"));
    assert_eq!(state.config.selected_model.as_deref(), Some("gpt-5.4"));
    assert_eq!(state.config.reasoning_effort.as_deref(), Some("medium"));
    let _ = events(&rx);
    drop(sink);
    drop(ctx);
    server.join().expect("fake daemon exits");
}
