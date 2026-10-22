use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::state::*;
use crate::tests::support::{make_session_full, test_app};
use choreo_proto::{
    AccountInfo, CatalogProvider, ClientMessage, ClientMessageType, DaemonMessage,
    DaemonMessageType, McpAuthState, McpServerStatus, RefreshStatus, SessionStatus, Turn,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── Unsent prompt drafts are stored per session ──
//
// The input bar is a single shared buffer, so when the user switches
// sessions an unsubmitted prompt must be stashed against the session it was
// typed in and the target session's own draft loaded in its place.  These
// tests pin the hand-off in `attach_to_session` / `handle_session_created`
// and the clear-on-submit behaviour in the Enter handler.

/// Attach to a session with a fresh (empty) display, mirroring what
/// `attach_to_session` does for a session the user has never opened.
fn attach(app: &mut App, session_id: u64) {
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attach_to_session(session_id, &tx)
        .expect("attach_to_session succeeds");
}

#[test]
fn switching_sessions_keeps_unsent_prompt_with_its_session() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.display_for(1); // ensure session 1 has a display to stash into
    app.input.text = "hello from session 1".to_string();
    app.input.cursor = 5;

    // Switch to session 2: session 2 has no draft yet, so the input bar
    // must come up empty — the prompt must not follow the user over.
    attach(&mut app, 2);
    assert_eq!(app.attached_session_id, Some(2));
    assert!(
        app.input.is_empty(),
        "new session must not inherit the prompt"
    );
    assert_eq!(app.display_for(1).draft, "hello from session 1");
    assert_eq!(app.display_for(1).draft_cursor, 5);

    // Switch back to session 1: its unsent prompt is restored, cursor
    // position included.
    attach(&mut app, 1);
    assert_eq!(app.input.text, "hello from session 1");
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn empty_draft_does_not_clobber_target_session_draft() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.display_for(1);
    // Session 1 has a saved draft from an earlier visit.
    app.display_for(2).draft = "session 2 draft".to_string();
    app.display_for(2).draft_cursor = 3;
    // The user is typing a *different* prompt in session 1…
    app.input.text = "session 1 draft".to_string();

    // …and switches to session 2: session 1's in-progress text is
    // stashed (replacing its old draft) and session 2's draft loaded.
    attach(&mut app, 2);
    assert_eq!(app.display_for(1).draft, "session 1 draft");
    assert_eq!(app.input.text, "session 2 draft");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn submitting_prompt_clears_session_draft() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.display_for(1);
    app.input.text = "hello".to_string();
    app.input.cursor = 5;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("submit prompt");

    assert!(app.input.is_empty());
    assert_eq!(
        app.display_for(1).draft,
        "",
        "submitted prompt must not resurface"
    );
    assert_eq!(app.display_for(1).draft_cursor, 0);

    // Round-trip through another session: still gone.
    attach(&mut app, 2);
    attach(&mut app, 1);
    assert!(app.input.is_empty());
}

#[test]
fn switching_while_browsing_history_does_not_stash_recalled_entry() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.active_session_id = Some(1);
    // Session 1 has one past prompt the user can navigate back to.
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("past prompt".to_string()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.display_for(1).view.insert_or_replace(0, turn);
    // Recall requires an empty draft: the user starts from an empty prompt
    // and presses Up, so the buffer now shows the history entry.
    app.navigate_history_up();
    assert_eq!(app.input.text, "past prompt");
    assert_eq!(app.history_index, Some(0));

    // Switching sessions mid-browse must NOT stash the recalled entry as
    // session 1's draft — recall began from an empty draft, and exiting
    // browsing returns to it.
    app.attach_to_session(2, &tx).expect("attach to session 2");
    assert_eq!(
        app.display_for(1).draft,
        "",
        "the recalled entry must not become the draft"
    );
    assert_eq!(app.history_index, None);
    assert!(app.input.is_empty(), "session 2 starts with no draft");

    // Returning to session 1 starts from the empty draft again.
    app.attach_to_session(1, &tx)
        .expect("attach back to session 1");
    assert!(app.input.is_empty());
    assert_eq!(app.history_index, None);
}

#[test]
fn session_manager_switch_preserves_unsent_prompt_per_session() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::SessionManager;
    app.session_mgr.set_sessions(vec![
        make_session_full(1, "first", "gpt-4", 3),
        make_session_full(2, "second", "claude", 5),
    ]);
    // The user was viewing session 1 with an unsent prompt.
    app.attached_session_id = Some(1);
    app.input.text = "draft for session 1".to_string();
    app.input.cursor = 5;

    // Session manager → Down → Enter attaches to session 2.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("move down to session 2");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("attach to session 2");
    assert_eq!(app.page, Page::Chat);
    assert_eq!(app.attached_session_id, Some(2));
    assert!(
        app.input.is_empty(),
        "session 2 must not inherit session 1's unsent prompt"
    );

    // Back to the session manager, Up to session 1, Enter to attach.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("open session manager");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("move up to session 1");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("attach back to session 1");

    assert_eq!(app.attached_session_id, Some(1));
    assert_eq!(
        app.input.text, "draft for session 1",
        "returning to session 1 restores its unsent prompt"
    );
    assert_eq!(app.input.cursor, 5, "cursor position is restored too");
}

#[test]
fn deleting_attached_session_drops_its_unsent_prompt() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.display_for(1).draft = "orphaned draft".to_string();
    app.input.text = "orphaned draft".to_string();
    app.input.cursor = 7;

    app.handle_session_deleted(1);

    assert_eq!(app.attached_session_id, None);
    assert!(
        app.input.is_empty(),
        "deleting the attached session must not leak its prompt into the next attach"
    );
    // The draft lives in the removed display, so nothing is left to
    // resurface on a later switch either.
    assert!(!app.session_displays.contains_key(&1));
}

#[test]
fn deleting_attached_session_clears_cached_status() {
    // The cached status/tool-groups describe the attachment; once the
    // attached session is deleted there is nothing left to describe, so
    // they must be cleared.  Leaving `attached_status` set (e.g. Responding)
    // would render a stale status bar AND make the submit-time idle-guard
    // in `connection/chat.rs` reject a plain prompt as "session not idle"
    // even though no session is attached at all.
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Responding);
    app.attached_tool_groups = vec!["core".to_string()];

    app.handle_session_deleted(1);

    assert_eq!(app.attached_session_id, None);
    assert_eq!(app.attached_status, None);
    assert_eq!(app.attached_tool_groups, [] as [std::string::String; 0]);
}

#[test]
fn auto_attach_keeps_pre_attach_input() {
    // Startup race: no session is attached yet, but the user has already
    // typed into the input bar.  The auto-attach must not clobber that
    // text — there is no outgoing session to stash it into, so replacing
    // it with the target's (empty) draft would destroy the typing.
    let mut app = test_app();
    app.attached_session_id = None;
    app.input.text = "startup prompt".to_string();
    app.input.cursor = 3;

    app.persist_input_draft(7);

    assert_eq!(
        app.input.text, "startup prompt",
        "pre-attach input must survive the auto-attach"
    );
    assert_eq!(app.input.cursor, 3);
    assert_eq!(app.display_for(7).draft, "");
}

#[test]
fn auto_attach_loads_target_draft_when_input_is_empty() {
    // With an empty input bar and nothing attached, the auto-attach loads
    // the target session's saved draft like every other attach path.
    let mut app = test_app();
    app.attached_session_id = None;
    app.display_for(9).draft = "saved for 9".to_string();
    app.display_for(9).draft_cursor = 4;

    app.persist_input_draft(9);

    assert_eq!(app.input.text, "saved for 9");
    assert_eq!(app.input.cursor, 4);
}

#[test]
fn editing_history_entry_becomes_the_draft_on_switch() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.active_session_id = Some(1);
    // Session 1 has one past prompt the user can navigate back to.
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("past prompt".to_string()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.display_for(1).view.insert_or_replace(0, turn);
    // Recall requires an empty draft, so the user starts from an empty
    // prompt and presses Up.
    app.navigate_history_up();
    assert_eq!(app.input.text, "past prompt");
    assert_eq!(app.history_index, Some(0));

    // The user types over the recalled history entry: the first edit
    // detaches it into the draft (browsing ends, the edit stays in the
    // buffer).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("edit recalled entry");
    assert_eq!(app.input.text, "past promptX");
    assert_eq!(app.history_index, None);

    // Switching sessions keeps the edited buffer as session 1's draft.
    app.attach_to_session(2, &tx).expect("attach to session 2");
    assert_eq!(
        app.display_for(1).draft,
        "past promptX",
        "edits on top of a history entry become the session draft"
    );
    assert_eq!(app.history_index, None);

    // Round-trip restores the edited text.
    app.attach_to_session(1, &tx)
        .expect("attach back to session 1");
    assert_eq!(app.input.text, "past promptX");
}

// ── S4: /refresh-models + dynamic provider list ─────────────────────

#[test]
fn refresh_models_sets_status_and_sends_message() {
    // The slash command shows immediate feedback and sends RefreshModels;
    // the reply arrives asynchronously.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter on empty input is a no-op");

    // Type the command and submit it.
    app.input.text = "/refresh-models --force".to_string();
    app.input.cursor = app.input.text.len();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("submit refresh-models");

    let msg = rx.recv().expect("RefreshModels sent");
    assert_eq!(msg.inner, ClientMessageType::RefreshModels { force: true });
    assert_eq!(
        app.status(),
        Some("refreshing models… (forced)"),
        "status set before the reply arrives"
    );
}

#[test]
fn models_refreshed_updates_status() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsRefreshed {
            providers: 208,
            models: 1234,
            status: RefreshStatus::Updated,
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsRefreshed");
    assert_eq!(
        app.status(),
        Some("models updated (208 providers, 1234 models)")
    );

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsRefreshed {
            providers: 208,
            models: 1234,
            status: RefreshStatus::UpToDate,
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsRefreshed 304");
    assert_eq!(
        app.status(),
        Some("models up to date (208 providers, 1234 models)")
    );

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsRefreshed {
            providers: 208,
            models: 1234,
            status: RefreshStatus::Forced,
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsRefreshed forced");
    assert!(app.status().unwrap().contains("forced"));
}

#[test]
fn models_refresh_failed_sets_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsRefreshFailed {
            error: "network error".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsRefreshFailed");
    assert_eq!(
        app.error(),
        Some("[daemon] refresh-models failed: network error")
    );
}

// ── /mcp control surface ────────────────────────────────────────────

#[test]
fn mcp_requests_status_and_sends_message() {
    // Bare `/mcp` echoes `> /mcp` and sends McpStatusRequest; the reply
    // arrives asynchronously.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    app.input.text = "/mcp".to_string();
    app.input.cursor = app.input.text.len();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("submit /mcp");

    let msg = rx.recv().expect("McpStatusRequest sent");
    assert_eq!(msg.inner, ClientMessageType::McpStatusRequest);
    assert_eq!(app.status(), Some("> /mcp"));
}

#[test]
fn mcp_reconnect_sends_message_with_slug() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    app.input.text = "/mcp reconnect docs".to_string();
    app.input.cursor = app.input.text.len();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("submit /mcp reconnect");

    let msg = rx.recv().expect("McpReconnect sent");
    assert_eq!(
        msg.inner,
        ClientMessageType::McpReconnect {
            slug: "docs".to_string(),
        }
    );
    assert_eq!(app.status(), Some("reconnecting MCP server docs…"));
}

/// An MCP server status row carrying an `oauth` block and a credential
/// key, for the login/logout freshness tests.
fn mcp_oauth_status_key(slug: &str, credential_key: &str) -> McpServerStatus {
    McpServerStatus {
        slug: slug.to_string(),
        tier: "daemon".to_string(),
        transport: "http".to_string(),
        target: "https://example.com/mcp".to_string(),
        connected: false,
        tool_count: 0,
        server_name: None,
        server_version: None,
        last_error: Some("authorization required".to_string()),
        credential_key: Some(credential_key.to_string()),
        auth: McpAuthState::NotLoggedIn,
        oauth: Some(choreo_proto::McpOAuthConfig {
            client_id: Some("client-1".to_string()),
            callback_port: None,
            scopes: vec!["read".to_string()],
            authorization_server: None,
            client_secret_env: None,
        }),
    }
}

fn mcp_oauth_status(slug: &str) -> McpServerStatus {
    mcp_oauth_status_key(slug, &format!("mcp:{slug}|abc123"))
}

fn submit_mcp(app: &mut App, text: &str, tx: &crossbeam_channel::Sender<ClientMessage>) {
    app.input.text = text.to_string();
    app.input.cursor = app.input.text.len();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        app,
        tx,
    )
    .expect("submit MCP command");
}

#[test]
fn mcp_login_requests_fresh_status_before_starting() {
    // A `/mcp login` must resolve the server's oauth block from a FRESH
    // status: the daemon owns it and it changes with the config (e.g. an
    // added scope), so a cached copy would request the stale scopes. The
    // flow waits for the reply rather than starting from any cache.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    submit_mcp(&mut app, "/mcp login docs", &tx);

    assert_eq!(
        rx.recv().expect("McpStatusRequest sent").inner,
        ClientMessageType::McpStatusRequest
    );
    assert_eq!(app.pending_mcp_login.as_deref(), Some("docs"));
    assert!(
        app.login.is_none(),
        "the flow must not start before the fresh status arrives"
    );
}

#[test]
fn mcp_login_supersedes_a_pending_logout() {
    // The newest command wins: a login clears a logout still awaiting its
    // status reply, so one reply cannot drive both flows.
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    submit_mcp(&mut app, "/mcp logout docs", &tx);
    assert_eq!(app.pending_mcp_logout.as_deref(), Some("docs"));

    submit_mcp(&mut app, "/mcp login docs", &tx);
    assert_eq!(app.pending_mcp_login.as_deref(), Some("docs"));
    assert!(
        app.pending_mcp_logout.is_none(),
        "login must clear the pending logout"
    );
}

#[test]
fn mcp_login_starts_from_the_fresh_status_reply() {
    let mut app = test_app();
    app.pending_mcp_login = Some("docs".to_string());
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::McpStatus {
            servers: vec![mcp_oauth_status("docs")],
            project_root: None,
            project_trusted: false,
            ignored_project_servers: Vec::new(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle McpStatus");

    assert!(
        app.pending_mcp_login.is_none(),
        "the pending login is taken"
    );
    assert!(app.login.is_some(), "the fresh status starts the sign-in");
}

#[test]
fn mcp_logout_removes_the_status_reply_key() {
    // A `/mcp logout` must read the credential key from the FRESH status,
    // so a config change cannot make it remove the wrong credential.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    submit_mcp(&mut app, "/mcp logout docs", &tx);
    assert_eq!(
        rx.recv().expect("McpStatusRequest sent").inner,
        ClientMessageType::McpStatusRequest
    );
    assert_eq!(app.pending_mcp_logout.as_deref(), Some("docs"));

    // The reply carries the CURRENT key; logout must use THAT.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::McpStatus {
            servers: vec![mcp_oauth_status_key("docs", "mcp:docs|FRESH")],
            project_root: None,
            project_trusted: false,
            ignored_project_servers: Vec::new(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle McpStatus");

    assert_eq!(
        rx.recv().expect("RemoveCredential sent").inner,
        ClientMessageType::RemoveCredential {
            service: "mcp:docs|FRESH".to_string()
        }
    );
    assert_eq!(
        rx.recv().expect("McpReconnect sent").inner,
        ClientMessageType::McpReconnect {
            slug: "docs".to_string()
        }
    );
}

#[test]
fn mcp_status_reply_renders_one_server_per_line() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::McpStatus {
            servers: vec![
                McpServerStatus {
                    slug: "docs".to_string(),
                    tier: "daemon".to_string(),
                    transport: "stdio".to_string(),
                    target: "npx docs".to_string(),
                    connected: true,
                    tool_count: 4,
                    server_name: Some("docs".to_string()),
                    server_version: Some("1.0.0".to_string()),
                    last_error: None,
                    credential_key: None,
                    auth: McpAuthState::NotApplicable,
                    oauth: None,
                },
                McpServerStatus {
                    slug: "fs".to_string(),
                    tier: "project".to_string(),
                    transport: "http".to_string(),
                    target: "https://example.com/mcp".to_string(),
                    connected: false,
                    tool_count: 0,
                    server_name: None,
                    server_version: None,
                    last_error: Some("connect timed out".to_string()),
                    credential_key: None,
                    auth: McpAuthState::NotApplicable,
                    oauth: None,
                },
            ],
            project_root: None,
            project_trusted: false,
            ignored_project_servers: Vec::new(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle McpStatus");

    let status = app.status().expect("status set from McpStatus");
    let lines: Vec<&str> = status.lines().collect();
    assert_eq!(lines[0], "MCP servers (2)");
    assert!(lines[1].contains("docs") && lines[1].contains("4 tool(s)"));
    assert!(lines[2].contains("fs") && lines[2].contains("connect timed out"));
}

#[test]
fn mcp_status_reply_empty_lists_none_configured() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::McpStatus {
            servers: Vec::new(),
            project_root: None,
            project_trusted: false,
            ignored_project_servers: Vec::new(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle empty McpStatus");
    assert_eq!(app.status(), Some("no MCP servers configured"));
}

#[test]
fn mcp_reconnect_failed_sets_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::McpReconnectFailed {
            slug: "docs".to_string(),
            error: "connection refused".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle McpReconnectFailed");
    assert_eq!(
        app.error(),
        Some("[daemon] mcp reconnect docs failed: connection refused")
    );
}

#[test]
fn mcp_reload_sends_message() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    app.input.text = "/mcp reload".to_string();
    app.input.cursor = app.input.text.len();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("submit /mcp reload");

    let msg = rx.recv().expect("McpReload sent");
    assert_eq!(msg.inner, ClientMessageType::McpReload);
    assert_eq!(app.status(), Some("reloading MCP configuration…"));
}

#[test]
fn mcp_reloaded_renders_summary_and_servers() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::McpReloaded {
            summary: "MCP reload: 1 added, 0 removed, 1 restarted, 0 unchanged, 0 failed"
                .to_string(),
            servers: vec![McpServerStatus {
                slug: "docs".to_string(),
                tier: "daemon".to_string(),
                transport: "stdio".to_string(),
                target: "npx docs".to_string(),
                connected: true,
                tool_count: 4,
                server_name: Some("docs".to_string()),
                server_version: Some("1.0.0".to_string()),
                last_error: None,
                credential_key: None,
                auth: McpAuthState::NotApplicable,
                oauth: None,
            }],
        }),
        &mut app,
        &tx,
    )
    .expect("handle McpReloaded");

    let status = app.status().expect("status set from McpReloaded");
    let lines: Vec<&str> = status.lines().collect();
    assert_eq!(
        lines[0],
        "MCP reload: 1 added, 0 removed, 1 restarted, 0 unchanged, 0 failed"
    );
    assert!(lines[1].contains("docs") && lines[1].contains("4 tool(s)"));
}

#[test]
fn mcp_reload_failed_sets_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::McpReloadFailed {
            error: "failed to parse mcp.json".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle McpReloadFailed");
    assert_eq!(
        app.error(),
        Some("[daemon] mcp reload failed: failed to parse mcp.json")
    );
}

#[test]
fn catalog_updated_replaces_provider_list_and_clamps_selection() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    // The default list starts from PROVIDER_OPTIONS (209 entries); park
    // the wizard highlight deep in the list.
    let default_len = app.providers.len();
    assert!(default_len > 10, "default provider list is large");
    app.ai_providers.wizard.focused = default_len - 1;

    // A catalog update with a small list replaces it and clamps.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CatalogUpdated {
            providers: vec![
                CatalogProvider {
                    slug: "openai".into(),
                    display_name: "OpenAI".into(),
                },
                CatalogProvider {
                    slug: "anthropic".into(),
                    display_name: "Anthropic".into(),
                },
            ],
        }),
        &mut app,
        &tx,
    )
    .expect("handle CatalogUpdated");

    assert_eq!(app.providers.len(), 2);
    // The picker has a fixed order, so the daemon's provenance order is
    // re-sorted: Anthropic leads.
    assert_eq!(app.providers[0].slug, "anthropic");
    assert_eq!(app.providers[0].display_name, "Anthropic");
    assert_eq!(app.providers[1].slug, "openai");
    // Highlight was beyond the new list's end → clamped to the last row.
    assert_eq!(app.ai_providers.wizard.focused, 1);
    assert_eq!(app.status(), Some("catalog updated (2 providers)"));
}

#[test]
fn catalog_updated_identical_list_does_not_churn_status() {
    // The daemon sends CatalogUpdated on every activity-subscribe; an
    // identical payload must not overwrite an unrelated status message.
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.set_status("busy".to_string());

    let providers = app
        .providers
        .iter()
        .map(|p| CatalogProvider {
            slug: p.slug.clone(),
            display_name: p.display_name.clone(),
        })
        .collect();
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CatalogUpdated { providers }),
        &mut app,
        &tx,
    )
    .expect("handle CatalogUpdated");

    assert_eq!(app.status(), Some("busy"), "status preserved");
    assert_eq!(app.providers.len(), 209);
}

#[test]
fn set_providers_returns_whether_list_changed() {
    let mut app = test_app();
    let unchanged = app
        .providers
        .iter()
        .map(|p| ProviderInfo {
            slug: p.slug.clone(),
            display_name: p.display_name.clone(),
        })
        .collect::<Vec<_>>();
    assert!(!app.set_providers(unchanged), "identical list → no change");

    assert!(
        app.set_providers(vec![ProviderInfo {
            slug: "openai".into(),
            display_name: "OpenAI".into(),
        }]),
        "different list → change"
    );
    assert_eq!(app.ai_providers.wizard.focused, 0);
}

#[test]
fn provider_picker_is_featured_then_alphabetical() {
    // The static PROVIDER_OPTIONS default is in catalog (provenance)
    // order; App::new must sort it so the wizard's picker leads with the
    // featured providers and reads A→Z after them from the first frame.
    let app = test_app();
    let featured = FEATURED_PROVIDERS.len();
    let leading = app
        .providers
        .iter()
        .take(featured)
        .map(|p| p.slug.as_str())
        .collect::<Vec<_>>();
    assert_eq!(leading, FEATURED_PROVIDERS, "featured providers lead");
    let display_names = app
        .providers
        .iter()
        .skip(featured)
        .map(|p| p.display_name.to_lowercase())
        .collect::<Vec<_>>();
    let mut sorted = display_names.clone();
    sorted.sort();
    assert_eq!(
        display_names, sorted,
        "the rest of the provider list must be alphabetical by display name"
    );

    // Live catalog updates arrive in provenance order too; set_providers
    // must re-sort them.  Feed deliberately shuffled, case-mixed input and
    // check the stored list comes back alphabetical.
    let mut app = test_app();
    app.set_providers(vec![
        ProviderInfo {
            slug: "z".into(),
            display_name: "zebra".into(),
        },
        ProviderInfo {
            slug: "n".into(),
            display_name: "NEAR AI Cloud".into(),
        },
        ProviderInfo {
            slug: "a".into(),
            display_name: "Alibaba Coding Plan".into(),
        },
        ProviderInfo {
            slug: "ab".into(),
            display_name: "abliteration.ai".into(),
        },
    ]);
    let got = app
        .providers
        .iter()
        .map(|p| p.display_name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        got,
        vec![
            // Case-insensitive: 'a'-'b' precedes 'a'-'l', and 'n'/'z'
            // follow regardless of ASCII case folding.
            "abliteration.ai",
            "Alibaba Coding Plan",
            "NEAR AI Cloud",
            "zebra",
        ]
    );
}

#[test]
fn catalog_refresh_clamps_wizard_highlight() {
    let mut app = test_app();
    // Open the wizard and park the highlight deep in the (209-entry)
    // list.
    app.ai_providers.wizard.open();
    app.ai_providers.wizard.focused = app.providers.len() - 1;
    app.ai_providers.wizard.scroll = app.providers.len() - 1;

    // A catalog refresh drops most providers.
    app.set_providers(vec![
        ProviderInfo {
            slug: "openai".into(),
            display_name: "OpenAI".into(),
        },
        ProviderInfo {
            slug: "anthropic".into(),
            display_name: "Anthropic".into(),
        },
    ]);

    // The highlight is clamped to the last row and the scroll hint stays
    // in range, so the picker never points past the end.
    assert_eq!(app.ai_providers.wizard.focused, 1);
    assert_eq!(app.ai_providers.wizard.scroll, 1);
    let filtered = app.ai_providers.wizard.filtered(&app.providers);
    let (start, count) = app.ai_providers.wizard.window(&filtered, 10);
    assert!(start + count <= app.providers.len());
}

#[test]
fn credential_modal_c_opens_and_esc_cancels() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.page = Page::AIProviders;
    app.ai_providers.set_accounts(vec![AccountInfo {
        name: "main".to_string(),
        provider: "openai".to_string(),
        has_credential: false,
    }]);

    // 'c' on the selected account opens the credential modal.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("c opens credential modal");
    assert_eq!(app.ai_providers.credential.target.as_deref(), Some("main"));

    // Esc cancels and returns to the list (nothing sent).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("esc closes credential modal");
    assert!(!app.ai_providers.credential.is_open());
}

#[test]
fn credential_modal_empty_key_shows_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.ai_providers.credential.open("main".to_string());

    // Enter with no key pasted shows the error and keeps the modal open.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter empty key");
    assert!(app.ai_providers.credential.is_open());
    assert_eq!(
        app.ai_providers.credential.error.get(),
        Some("API key cannot be empty")
    );
}

#[test]
fn account_add_failed_closes_credential_modal() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The credential modal auto-opens right after submit; the daemon then
    // rejects the account.
    app.ai_providers.credential.open("my-account".to_string());

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::AccountAddFailed {
            name: "my-account".to_string(),
            error: "duplicate slug".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle AccountAddFailed");

    // No phantom account: the credential modal is dropped and the failure
    // is on the global error line.
    assert!(!app.ai_providers.credential.is_open());
    assert_eq!(
        app.error(),
        Some("[daemon] failed to add account my-account: duplicate slug")
    );
}

#[test]
fn account_add_failed_does_not_close_unrelated_credential_modal() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The failure reply can arrive AFTER the user has dismissed the
    // wizard's auto-opened modal and opened a different account's key
    // modal — that in-progress input must survive the late reply.
    app.ai_providers
        .credential
        .open("other-account".to_string());
    app.ai_providers.credential.input.text = "sk-in-progress".to_string();
    app.ai_providers.credential.input.cursor = 4;

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::AccountAddFailed {
            name: "my-account".to_string(),
            error: "duplicate slug".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle AccountAddFailed for a different account");

    // The unrelated modal stays open with its typed input intact.
    assert!(
        app.ai_providers.credential.is_open(),
        "a failure for another account must not yank the open modal"
    );
    assert_eq!(
        app.ai_providers.credential.target.as_deref(),
        Some("other-account")
    );
    assert_eq!(app.ai_providers.credential.input.text, "sk-in-progress");
    assert_eq!(app.ai_providers.credential.input.cursor, 4);
    // The failure is still surfaced on the global error line.
    assert_eq!(
        app.error(),
        Some("[daemon] failed to add account my-account: duplicate slug")
    );
}

// ── Connection-level termination (eviction / server shutdown) ───────

#[test]
fn daemon_message_evicted_quits_with_message() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Evicted),
        &mut app,
        &tx,
    )
    .expect("handle Evicted");

    assert!(app.should_quit, "eviction must terminate the TUI");
    let msg = app.quit_message().expect("quit message set");
    assert!(
        msg.contains("evicted"),
        "quit message must explain the eviction, got: {msg}"
    );
}

#[test]
fn daemon_message_shutting_down_quits_with_message() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ShuttingDown),
        &mut app,
        &tx,
    )
    .expect("handle ShuttingDown");

    assert!(app.should_quit, "server shutdown must terminate the TUI");
    let msg = app.quit_message().expect("quit message set");
    assert!(
        msg.contains("shutting down"),
        "quit message must explain the shutdown, got: {msg}"
    );
}
