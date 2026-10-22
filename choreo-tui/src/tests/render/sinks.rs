use super::drawn_cells;
use crate::render::*;
use crate::tests::support::test_app;
use choreo_proto::{SessionStatus, SessionSummary};

// ── Untrusted text is normalized at every widget sink ──────────────────

#[test]
fn status_bar_renders_daemon_errors_inert() {
    // `app.error` carries daemon-authored text (a provider's rejected request
    // body, say).  A control character must not reach a cell, and an escape
    // sequence must not ride along; the tab expands to spaces.
    let mut app = test_app();
    app.set_error("bad request\u{7}\tbody\x1b[31m".to_string());
    let content = drawn_cells(&mut app, 80, 24);
    assert!(
        !content.chars().any(char::is_control),
        "no control char may reach a cell: {content:?}"
    );
    assert!(content.contains("bad request"), "the message is shown");
    assert!(
        content.contains("\\u{7}") && content.contains("\\u{1b}[31m"),
        "hostile bytes render as inert text: {content:?}"
    );
}

#[test]
fn status_error_height_measures_the_stored_text_as_drawn() {
    // The reserved height is measured from the STORED text, which ingest already
    // normalized — the same bytes the renderer draws — so the two cannot
    // disagree about how many rows a taboo-laden message occupies.
    let mut app = test_app();
    app.set_error("aaaaaaaaaa\tbbbbbbbbbb\tcccc".to_string());
    let width = 24u16;
    let height = app.status_error_height(width);
    let text = app.error().expect("error stored");
    let drawn_rows = crate::markdown_render::plain_text_lines(text, width - 2).len();
    assert_eq!(
        usize::from(height),
        drawn_rows,
        "height must match the drawn rows for {text:?}"
    );
}

#[test]
fn session_manager_cells_render_untrusted_titles_inert() {
    // Session titles are model-chosen and model ids come from the catalog;
    // neither may carry a control character into a fixed-width column.
    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    app.session_mgr.set_sessions(vec![SessionSummary {
        session_id: 1,
        title: Some("evil\u{202e}title\tx".to_string()),
        selected_model: Some("model\u{7}".to_string()),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1_705_314_000_000,
        last_modified: 1_705_314_000_000,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    }]);

    let content = drawn_cells(&mut app, 100, 24);
    assert!(
        !content.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "no live control or bidi char may reach a cell: {content:?}"
    );
    assert!(
        content.contains("\\u{202e}") && content.contains("model\\u{7}"),
        "untrusted cell text renders inert: {content:?}"
    );
}

#[test]
fn login_modal_renders_untrusted_fields_inert() {
    // The authorization server supplies the status line, the authorize URL and
    // the device code, and the credential supplies the account name.  The three
    // display-only fields are normalized at INGEST (where `apply_login_event`
    // takes them from the event) while `provider`/`account` stay raw, so this
    // asserts both halves: the modal holds inert text, the drawn content is
    // inert end to end, and the identity fields still carry their exact bytes.
    use crate::state::LoginModal;
    use choreo_client_core::LoginEvent;

    let mut app = test_app();
    app.login = Some(LoginModal::test_modal("openai\u{7}", "work\tacct"));
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.apply_login_event(
        LoginEvent::AuthUrl {
            url: "https://x/\u{202e}evil".to_string(),
        },
        &tx,
    );
    app.apply_login_event(
        LoginEvent::DeviceCode {
            code: "AB\u{7}CD".to_string(),
            uri: "https://x/dc\tx".to_string(),
        },
        &tx,
    );
    app.apply_login_event(
        LoginEvent::Prompt {
            message: "waiting\x1b[2J".to_string(),
        },
        &tx,
    );

    let modal = app.login.as_ref().expect("the flow is still running");
    let mut display_only = vec![
        modal.status.as_str(),
        modal.auth_url.as_deref().unwrap_or(""),
    ];
    if let Some((code, uri)) = modal.device_code.as_ref() {
        display_only.push(code.as_str());
        display_only.push(uri.as_str());
    }
    for text in display_only {
        assert!(
            !text.chars().any(char::is_control)
                && !text.contains('\u{202e}')
                && !text.contains('\t'),
            "display-only login fields are stored inert: {text:?}"
        );
    }
    // Deliberately NOT normalized: the account is the credential key the modal
    // hands back, and the provider selects the auth descriptor.
    assert_eq!(modal.provider, "openai\u{7}");
    assert_eq!(modal.account, "work\tacct");

    let content = drawn_cells(&mut app, 100, 30);
    assert!(
        !content.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "no live control or bidi char may reach a cell: {content:?}"
    );
    for inert in ["openai\\u{7}", "waiting\\u{1b}[2J", "AB\\u{7}CD"] {
        assert!(
            content.contains(inert),
            "{inert:?} should render inert: {content:?}"
        );
    }
}

#[test]
fn status_display_renders_a_hostile_tool_name_inert() {
    // The status bar and the session table both show the session status, and a
    // tool name there is MCP-supplied arbitrary text.
    let (label, _color) = status_display(&SessionStatus::ToolCall("evil\u{7}tool".to_string()));
    assert_eq!(label, "tool call: evil\\u{7}tool");
    assert!(!label.chars().any(char::is_control), "{label:?}");
}

#[test]
fn status_bar_identity_fields_render_inert() {
    // Every value in the status bar's identity block is externally supplied —
    // the working directory from the filesystem, the account slug from the
    // keystore, the model id from the catalog, the reasoning effort and tool
    // groups echoed back by the daemon.  None may carry a live control or bidi
    // character into the single clipped line.
    let mut app = test_app();
    // The identity block draws only for an attached session.
    app.attached_session_id = Some(0);
    app.attached_account_slug = Some("acct\u{7}".to_string());
    app.attached_tool_groups = vec!["core\u{202e}".to_string(), "browser".to_string()];
    let display = app.display_for(0);
    display.working_dir = Some("/tmp/\u{7}evil".to_string());
    display.selected_model = Some("gpt\u{202e}4o".to_string());
    display.reasoning_effort = Some("hi\tgh".to_string());

    let content = drawn_cells(&mut app, 200, 24);
    assert!(
        !content.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "no live control or bidi char may reach a cell: {content:?}"
    );
    for inert in [
        "/tmp/\\u{7}evil",
        "acct\\u{7}",
        "gpt\\u{202e}4o",
        // The reasoning tab expands at the 4-column stop instead of dropping.
        "hi  gh",
        "core\\u{202e}, browser",
    ] {
        assert!(
            content.contains(inert),
            "{inert:?} should render inert: {content:?}"
        );
    }
}

#[test]
fn session_manager_detail_renders_tool_groups_inert() {
    // The detail pane prints the session's active tool groups, which the daemon
    // echoes back from the turn — arbitrary text like every other line there.
    use crate::state::{Page, SessionDetailData, SessionManagerView};

    let mut app = test_app();
    app.page = Page::SessionManager;
    app.session_mgr.view = SessionManagerView::Detail;
    app.session_mgr.detail_data = Some(SessionDetailData {
        session_id: 1,
        title: "t".to_string(),
        selected_model: "m".to_string(),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: "/tmp".to_string(),
        created_at: 1_705_314_000_000,
        last_modified: 1_705_314_000_000,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec!["core\u{7}".to_string(), "browser\u{202e}".to_string()],
        account_name: None,
        accumulated_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    });

    let content = drawn_cells(&mut app, 100, 24);
    assert!(
        !content.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "no live control or bidi char may reach a cell: {content:?}"
    );
    assert!(
        content.contains("Tool Groups:") && content.contains("core\\u{7}"),
        "the group list renders inert: {content:?}"
    );
    assert!(
        content.contains("browser\\u{202e}"),
        "every group in the list renders inert: {content:?}"
    );
}

#[test]
fn image_placeholder_label_renders_a_hostile_call_id_inert() {
    // A tool-result vision image's placeholder title embeds the model-chosen
    // tool-call id, so the label itself must be inert — asserting on the label
    // builder rather than drawing a whole image block keeps the test off the
    // image-protocol machinery.
    use crate::state::ImageSlot;

    let label = slot_label(&ImageSlot::ToolResult("call\u{7}\u{202e}".to_string()));
    assert_eq!(label, "[call\\u{7}\\u{202e}]");
    assert!(!label.chars().any(char::is_control), "{label:?}");
    // A positional slot carries no untrusted component.
    assert_eq!(slot_label(&ImageSlot::Displayed(2)), "[2]");
}
