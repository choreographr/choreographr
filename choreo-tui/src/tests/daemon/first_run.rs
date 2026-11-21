use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::state::*;
use crate::terminal::profile::Platform;
use crate::tests::support::test_app;
use choreo_proto::{
    AccountInfo, ClientMessage, ClientMessageType, DaemonMessage, DaemonMessageType, MessageKind,
    ModelInfo, ReasoningCapability, SessionEvent,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── The staged account/model/reasoning choice ──
//
// The flow commits nothing until the account, the model and the effort are all
// chosen: a single `SetSessionChoice` at the end. Every test below therefore
// also asserts what was NOT sent — the three-write sequence it replaces
// (`SetSessionAccount` → `SetModel` → `SetReasoningEffort`) must not appear.

/// Every request the handler sent, in order.
fn sent(rx: &crossbeam_channel::Receiver<ClientMessage>) -> Vec<ClientMessageType> {
    rx.try_iter().map(|m| m.inner).collect()
}

/// The id of the request sent last — the pending slot its reply must resolve.
fn last_request_id(rx: &crossbeam_channel::Receiver<ClientMessage>) -> u64 {
    rx.try_iter().last().expect("a request was sent").id
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        reasoning_levels: Vec::new(),
    }
}

fn levels(slugs: &[&str]) -> ReasoningCapability {
    ReasoningCapability {
        available_effort_levels: slugs.iter().map(|s| (*s).to_string()).collect(),
    }
}

fn press(code: KeyCode, app: &mut App, tx: &crossbeam_channel::Sender<ClientMessage>) {
    handle_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), app, tx)
        .expect("handle key");
}

/// A confirmed credential for `account` on an attached, accountless session
/// starts the flow: it asks for the candidate account's models and nothing else.
fn start_flow(app: &mut App, tx: &crossbeam_channel::Sender<ClientMessage>) {
    app.attached_session_id = Some(42);
    app.attached_account_slug = None;
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CredentialAdded {
            service: "anthropic".to_string(),
        }),
        app,
        tx,
    )
    .expect("handle CredentialAdded");
}

/// Answer the flow's `ListModels` with `models`, leaving the picker open.
fn answer_models(
    app: &mut App,
    tx: &crossbeam_channel::Sender<ClientMessage>,
    rx: &crossbeam_channel::Receiver<ClientMessage>,
    models: Vec<ModelInfo>,
    suggested: Option<String>,
) {
    let id = last_request_id(rx);
    handle_daemon_message(
        DaemonMessage::reply(
            id,
            DaemonMessageType::Models {
                models,
                selected_model: None,
                suggested_model: suggested,
                provisional: false,
            },
        ),
        app,
        tx,
    )
    .expect("handle Models");
}

// ── the first-run chain, staged ──────────────────────────────────────

#[test]
fn a_confirmed_key_starts_the_flow_without_touching_the_session() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);

    assert_eq!(
        app.setup_flow,
        Some(SetupFlow::LoadingModels {
            account: "anthropic".to_string()
        })
    );
    assert_eq!(
        sent(&rx),
        vec![
            // The credential confirmation refreshes the accounts page…
            ClientMessageType::ListAccounts,
            // …and the flow asks for the candidate account's models. Nothing
            // is written to the session.
            ClientMessageType::ListModels {
                account: Some("anthropic".to_string()),
            },
        ],
    );
    assert_eq!(app.page, Page::Chat, "the user is returned to the chat");
}

#[test]
fn a_key_for_a_session_that_already_has_an_account_starts_nothing() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.attached_account_slug = Some("openai".to_string());

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CredentialAdded {
            service: "anthropic".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle CredentialAdded");

    assert_eq!(app.setup_flow, None, "an in-use account is never switched");
    assert!(
        rx.try_iter()
            .all(|m| !matches!(m.inner, ClientMessageType::ListModels { .. })),
        "no model list is requested"
    );
}

#[test]
fn a_key_with_no_attached_session_starts_nothing() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = None;
    app.attached_account_slug = None;

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CredentialAdded {
            service: "anthropic".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle CredentialAdded");

    assert_eq!(app.setup_flow, None);
}

#[test]
fn the_model_list_opens_the_picker_on_the_suggestion() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(
        &mut app,
        &tx,
        &rx,
        vec![model("claude-3"), model("gpt-4o")],
        Some("gpt-4o".to_string()),
    );

    assert_eq!(
        app.setup_flow,
        Some(SetupFlow::ChoosingModel {
            account: "anthropic".to_string(),
            models: vec![model("claude-3"), model("gpt-4o")],
        })
    );
    assert!(app.model_selector.is_open());
    assert_eq!(
        app.model_selector.highlighted().as_deref(),
        Some("gpt-4o"),
        "the picker opens on the daemon's suggestion"
    );
}

#[test]
fn choosing_a_model_asks_for_its_levels_and_selects_nothing() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("claude-3")], None);
    // The suggestion falls back to the first row, so Enter takes it.
    press(KeyCode::Enter, &mut app, &tx);

    assert_eq!(
        app.setup_flow,
        Some(SetupFlow::LoadingReasoning {
            account: "anthropic".to_string(),
            model: "claude-3".to_string(),
        })
    );
    let sent = sent(&rx);
    assert_eq!(
        sent.last(),
        Some(&ClientMessageType::ModelReasoning {
            account: "anthropic".to_string(),
            model: "claude-3".to_string(),
        })
    );
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, ClientMessageType::SetModel { .. })),
        "staging a model never selects it"
    );
    assert!(
        !app.model_selector.is_open(),
        "the picker closes on the pick"
    );
}

#[test]
fn a_models_levels_open_the_dialog_pre_highlighted() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("claude-3")], None);
    press(KeyCode::Enter, &mut app, &tx);
    handle_daemon_message(
        DaemonMessage::reply(
            999,
            DaemonMessageType::ModelReasoning {
                capability: levels(&["off", "low", "medium", "high"]),
                suggested_effort: Some("medium".to_string()),
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelReasoning");

    assert!(app.reasoning_dialog.is_open(), "the choice is shown");
    assert_eq!(app.reasoning_dialog.focused_level(), Some("medium"));
    assert!(matches!(
        app.setup_flow,
        Some(SetupFlow::ChoosingReasoning { .. })
    ));
}

#[test]
fn a_model_with_no_levels_commits_off_without_a_dialog() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("gpt-4o")], None);
    press(KeyCode::Enter, &mut app, &tx);
    handle_daemon_message(
        DaemonMessage::reply(
            999,
            DaemonMessageType::ModelReasoning {
                capability: levels(&[]),
                suggested_effort: None,
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelReasoning");

    assert!(!app.reasoning_dialog.is_open(), "no choice to show");
    assert_eq!(
        sent(&rx).last(),
        Some(&ClientMessageType::SetSessionChoice {
            account: "anthropic".to_string(),
            model: "gpt-4o".to_string(),
            effort: "off".to_string(),
        })
    );
    assert_eq!(
        app.setup_flow,
        Some(SetupFlow::Committing {
            account: "anthropic".to_string()
        })
    );
}

#[test]
fn the_reasoning_choice_commits_the_whole_choice_at_once() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("claude-3")], None);
    press(KeyCode::Enter, &mut app, &tx);
    handle_daemon_message(
        DaemonMessage::reply(
            999,
            DaemonMessageType::ModelReasoning {
                capability: levels(&["off", "low", "high"]),
                suggested_effort: Some("low".to_string()),
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelReasoning");
    // Down once: from the suggested "low" onto "high".
    press(KeyCode::Down, &mut app, &tx);
    press(KeyCode::Enter, &mut app, &tx);

    assert!(
        !app.reasoning_dialog.is_open(),
        "committing closes the dialog"
    );
    assert_eq!(
        sent(&rx).last(),
        Some(&ClientMessageType::SetSessionChoice {
            account: "anthropic".to_string(),
            model: "claude-3".to_string(),
            effort: "high".to_string(),
        }),
        "one atomic write carries all three facts"
    );
    assert!(
        !sent(&rx).iter().any(|m| matches!(
            m,
            ClientMessageType::SetSessionAccount { .. }
                | ClientMessageType::SetModel { .. }
                | ClientMessageType::SetReasoningEffort { .. }
        )),
        "the three individual writes are gone from this path"
    );
}

#[test]
fn the_daemons_confirmation_ends_the_flow() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("claude-3")], None);
    press(KeyCode::Enter, &mut app, &tx);
    handle_daemon_message(
        DaemonMessage::reply(
            999,
            DaemonMessageType::ModelReasoning {
                capability: levels(&[]),
                suggested_effort: None,
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelReasoning");
    assert!(matches!(app.setup_flow, Some(SetupFlow::Committing { .. })));

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::SessionAccountSet {
                account: "anthropic".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionAccountSet");

    assert_eq!(app.setup_flow, None, "the commit landed");
    assert!(!app.model_selector.is_open());
    assert!(!app.reasoning_dialog.is_open());
}

// ── cancel: nothing is sent, nothing is committed ────────────────────

#[test]
fn esc_in_the_picker_abandons_the_flow_and_returns_to_the_accounts() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("claude-3")], None);
    let before = sent(&rx).len();

    press(KeyCode::Esc, &mut app, &tx);

    assert_eq!(app.setup_flow, None);
    assert!(!app.model_selector.is_open());
    assert_eq!(app.page, Page::AIProviders, "back where the flow started");
    let sent = sent(&rx);
    assert_eq!(
        sent.len(),
        before + 1,
        "cancelling sends only the accounts refresh"
    );
    assert_eq!(sent.last(), Some(&ClientMessageType::ListAccounts));
}

#[test]
fn esc_in_the_dialog_abandons_the_whole_flow() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("claude-3")], None);
    press(KeyCode::Enter, &mut app, &tx);
    handle_daemon_message(
        DaemonMessage::reply(
            999,
            DaemonMessageType::ModelReasoning {
                capability: levels(&["off", "low"]),
                suggested_effort: None,
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelReasoning");

    press(KeyCode::Esc, &mut app, &tx);

    assert_eq!(app.setup_flow, None);
    assert!(!app.reasoning_dialog.is_open());
    assert_eq!(app.page, Page::AIProviders);
    assert!(
        !sent(&rx)
            .iter()
            .any(|m| matches!(m, ClientMessageType::SetSessionChoice { .. })),
        "a cancelled flow commits nothing"
    );
}

#[test]
fn esc_while_a_query_is_in_flight_abandons_the_flow() {
    // No popup is up while the flow awaits the daemon, so Esc would otherwise
    // fall through to the Chat page and cancel the session's turn. It abandons
    // the staged choice instead.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    assert!(matches!(
        app.setup_flow,
        Some(SetupFlow::LoadingModels { .. })
    ));
    let _ = sent(&rx);

    press(KeyCode::Esc, &mut app, &tx);

    assert_eq!(app.setup_flow, None);
    assert_eq!(app.page, Page::AIProviders);
    assert!(
        !sent(&rx)
            .iter()
            .any(|m| matches!(m, ClientMessageType::Cancel { .. })),
        "the session's turn is not cancelled"
    );
}

#[test]
fn esc_while_committing_is_swallowed() {
    // The commit is already on the wire: Esc can neither take it back nor be
    // allowed through to the Chat page, where it would cancel the session's
    // running turn. It does nothing at all — not even a status line.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("gpt-4o")], None);
    press(KeyCode::Enter, &mut app, &tx);
    handle_daemon_message(
        DaemonMessage::reply(
            999,
            DaemonMessageType::ModelReasoning {
                capability: levels(&[]),
                suggested_effort: None,
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelReasoning");
    let committing = Some(SetupFlow::Committing {
        account: "anthropic".to_string(),
    });
    assert_eq!(app.setup_flow, committing);
    let status = app.status().map(str::to_string);
    let _ = sent(&rx);

    press(KeyCode::Esc, &mut app, &tx);

    assert_eq!(app.setup_flow, committing, "the flow is unchanged");
    assert!(
        !sent(&rx)
            .iter()
            .any(|m| matches!(m, ClientMessageType::Cancel { .. })),
        "the session's turn is not cancelled"
    );
    assert!(
        sent(&rx).is_empty(),
        "committing swallows the key: nothing is sent"
    );
    assert_eq!(
        app.status().map(str::to_string),
        status,
        "no status line is written either"
    );
}

// ── failure: the flow ends, the session was never touched ────────────

#[test]
fn a_refused_model_list_ends_the_flow_with_the_daemons_error() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    let id = last_request_id(&rx);
    handle_daemon_message(
        DaemonMessage::reply(
            id,
            DaemonMessageType::ModelsFailed {
                error: "no credential stored for account 'anthropic'".to_string(),
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelsFailed");

    assert_eq!(app.setup_flow, None);
    assert!(!app.model_selector.is_open());
    assert!(
        app.error()
            .is_some_and(|e| e.contains("no credential stored")),
        "the daemon's error reaches the status line, got {:?}",
        app.error()
    );
}

#[test]
fn a_failed_background_fetch_ends_the_flow() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    // An EMPTY provisional reply is the daemon saying it has nothing to paint
    // yet and a fetch is running — the picker waits rather than claiming the
    // account has no models.
    let id = last_request_id(&rx);
    handle_daemon_message(
        DaemonMessage::reply(
            id,
            DaemonMessageType::Models {
                models: Vec::new(),
                selected_model: None,
                suggested_model: None,
                provisional: true,
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle Models");
    assert!(
        app.model_selector.loading,
        "an empty provisional list is a wait, not an empty account"
    );

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsUnavailable {
            account: "anthropic".to_string(),
            error: "provider unreachable".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsUnavailable");

    assert_eq!(app.setup_flow, None);
    assert!(!app.model_selector.is_open());
    assert!(
        app.error()
            .is_some_and(|e| e.contains("provider unreachable")),
        "got {:?}",
        app.error()
    );
}

#[test]
fn a_rejected_commit_ends_the_flow() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    answer_models(&mut app, &tx, &rx, vec![model("claude-3")], None);
    press(KeyCode::Enter, &mut app, &tx);
    handle_daemon_message(
        DaemonMessage::reply(
            999,
            DaemonMessageType::ModelReasoning {
                capability: levels(&[]),
                suggested_effort: None,
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelReasoning");
    assert!(matches!(app.setup_flow, Some(SetupFlow::Committing { .. })));

    handle_daemon_message(
        DaemonMessage::reply(
            1000,
            DaemonMessageType::Failed {
                kind: MessageKind::SetSessionChoice,
                error: "model 'claude-3' is not served by account 'anthropic'".to_string(),
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle Failed");

    assert_eq!(app.setup_flow, None);
    // The line the user reads is the generic dispatch's, with the daemon's own
    // wording — the flow closes and must not also write a second error.
    assert_eq!(
        app.error(),
        Some(
            "[daemon] SetSessionChoice failed: model 'claude-3' is not served by account 'anthropic'"
        ),
    );
}

// ── the accounts page, and swapping accounts mid-session ─────────────

#[test]
fn selecting_an_account_on_the_page_stages_instead_of_switching() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.attached_account_slug = Some("openai".to_string());
    app.page = Page::AIProviders;
    app.ai_providers.selection = Some(0);
    app.ai_providers.set_accounts(vec![AccountInfo {
        name: "anthropic".to_string(),
        provider: "anthropic".to_string(),
        has_credential: true,
    }]);

    press(KeyCode::Enter, &mut app, &tx);

    assert_eq!(
        app.setup_flow,
        Some(SetupFlow::LoadingModels {
            account: "anthropic".to_string()
        })
    );
    assert_eq!(app.page, Page::Chat);
    let sent = sent(&rx);
    assert_eq!(
        sent,
        vec![ClientMessageType::ListModels {
            account: Some("anthropic".to_string())
        }],
        "the account is only staged; nothing is written to the session"
    );
}

#[test]
fn selecting_the_sessions_current_account_is_a_no_op() {
    // The rule lives in the flow's `start`, so the page never moves when the
    // choice is refused: re-selecting the account in use is what `Alt+M` is for.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.attached_account_slug = Some("anthropic".to_string());
    app.page = Page::AIProviders;
    app.ai_providers.selection = Some(0);
    app.ai_providers.set_accounts(vec![AccountInfo {
        name: "anthropic".to_string(),
        provider: "anthropic".to_string(),
        has_credential: true,
    }]);

    press(KeyCode::Enter, &mut app, &tx);

    assert_eq!(app.setup_flow, None);
    assert!(sent(&rx).is_empty(), "nothing is sent at all");
    assert_eq!(
        app.page,
        Page::AIProviders,
        "the picker is not a way to re-set the same account"
    );
}

#[test]
fn a_second_account_choice_while_one_is_in_flight_is_refused() {
    // A second staged choice would replace the first, and the superseded
    // account's `Models` reply (which carries no account) would then open the
    // picker on ITS list under the new account's name.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    let first = app.setup_flow.clone();
    assert!(matches!(first, Some(SetupFlow::LoadingModels { .. })));
    let _ = sent(&rx);

    app.page = Page::AIProviders;
    app.ai_providers.selection = Some(0);
    app.ai_providers.set_accounts(vec![AccountInfo {
        name: "openai".to_string(),
        provider: "openai".to_string(),
        has_credential: true,
    }]);
    press(KeyCode::Enter, &mut app, &tx);

    assert_eq!(app.setup_flow, first, "the first choice stays installed");
    assert!(
        !sent(&rx)
            .iter()
            .any(|m| matches!(m, ClientMessageType::ListModels { .. })),
        "no second model list is requested"
    );
    assert_eq!(
        app.status(),
        Some("finish choosing the account's model and reasoning effort first")
    );
}

#[test]
fn the_picker_outside_a_flow_still_selects_immediately() {
    // The mid-session path is unchanged: Alt+M selects with `SetModel` and
    // never touches the staged flow.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.attached_account_slug = Some("anthropic".to_string());

    // The chat's own Alt+M shortcut, with the modifier the dispatcher reads.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+m");
    assert!(app.model_selector.is_open());
    assert_eq!(app.setup_flow, None);

    let id = last_request_id(&rx);
    handle_daemon_message(
        DaemonMessage::reply(
            id,
            DaemonMessageType::Models {
                models: vec![model("claude-3")],
                selected_model: Some("claude-3".to_string()),
                suggested_model: Some("claude-3".to_string()),
                provisional: false,
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle Models");
    press(KeyCode::Enter, &mut app, &tx);

    assert_eq!(
        sent(&rx).last(),
        Some(&ClientMessageType::SetModel {
            model: "claude-3".to_string()
        }),
        "outside a flow the picker keeps its one-write behaviour"
    );
    assert_eq!(app.setup_flow, None);
}

#[test]
fn switching_sessions_drops_a_flow_in_flight() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    assert!(app.setup_flow.is_some());

    app.reset_for_session_switch(43);

    assert_eq!(
        app.setup_flow, None,
        "nothing was committed, so nothing to carry"
    );
}

#[test]
fn a_flow_hides_the_accounts_chord_for_a_second_picker() {
    // While a flow owns the model picker, Alt+M must not race it with a second
    // list request for the attached session's account.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    start_flow(&mut app, &tx);
    // Drain the flow's own requests so the check below sees only what Alt+M
    // adds (nothing).
    let _ = sent(&rx);
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+m");

    assert!(
        sent(&rx).is_empty(),
        "no second ListModels is sent while the flow is in flight"
    );
    assert_eq!(
        app.status(),
        Some("finish choosing the account's model and reasoning effort first")
    );
    let _ = Platform::current();
}
