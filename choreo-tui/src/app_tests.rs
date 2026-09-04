use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::markdown_render::*;
use crate::state::*;
use crate::test_util::{make_session, test_app};
use choreo_proto::{
    AccountInfo, CatalogProvider, ClientMessage, DaemonMessage, RefreshStatus, Turn,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;

#[test]
fn history_text_height_accounts_for_wrapping_and_blank_lines() {
    assert_eq!(history_text_height("hello", 10), 1);
    assert_eq!(history_text_height("hello world", 5), 3);
    assert_eq!(history_text_height("a\nb\n", 10), 3);
    assert_eq!(history_text_height("", 10), 1);
    assert_eq!(history_text_height("\n", 10), 2);
}

#[test]
fn display_width_treats_emoji_as_terminal_cells() {
    assert_eq!(display_width("😀"), 2);
    assert_eq!(display_width("A😀B"), 4);
    assert_eq!(display_width("👨‍👩‍👧‍👦"), 2);
}

#[test]
fn wrapped_line_height_uses_terminal_display_width() {
    assert_eq!(lines_height(&[Line::from("😀😀")], 2), 2);
    assert_eq!(lines_height(&[Line::from("👨‍👩‍👧‍👦")], 2), 1);
}

#[test]
fn markdown_lines_render_tables() {
    let lines = markdown_lines(
        "| Name | Role | Years |\n|:--|:--:|--:|\n| Ada Lovelace | Mathematician | 1842 |\n| Grace Hopper | Computer Scientist | 1943 |",
        60,
    );

    let rendered = lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("┌"));
    assert!(rendered.contains("Ada Lovelace"));
    assert!(rendered.contains("Grace Hopper"));
    assert!(rendered.contains("Mathematician"));
}

#[test]
fn markdown_lines_render_lists_with_item_text() {
    let lines = markdown_lines("- one\n- [x] done\n1. first\n2. second", 80);

    let rendered = lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("• one"));
    assert!(rendered.contains("• [x] done"));
    assert!(rendered.contains("1. first"));
    assert!(rendered.contains("2. second"));
}

#[test]
fn oversized_history_item_keeps_visible_tail() {
    let wrapped = history_text_height("123456789", 3);
    assert_eq!(wrapped, 3);

    let rows_remaining = 2;
    let rows_to_skip = 0;
    let bottom_line = wrapped.saturating_sub(rows_to_skip);
    let top_line = bottom_line.saturating_sub(rows_remaining);

    assert_eq!(top_line, 1);
}

#[test]
fn terminal_event_appends_characters() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle key");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle key");

    assert_eq!(app.input.text, "hi");
    assert_eq!(app.input.cursor, 2);
    assert!(rx.try_recv().is_err());
}

#[test]
fn terminal_event_submits_run_input() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(app.input.is_empty());
    assert_eq!(app.input.cursor, 0);
    let message = rx.recv().expect("sent message");
    assert_eq!(
        message,
        ClientMessage::RunInput {
            request_id: 1,
            input: b"hello".to_vec(),
        }
    );
}

#[test]
fn submitting_prompt_while_locked_is_rejected_with_feedback() {
    // Submitting a prompt (RunInput) to a locked daemon must produce clear
    // feedback instead of silence — and must NOT send the RunInput (the
    // daemon cannot run inference with no credentials in memory, so a sent
    // message would only come back as a transient "no credential stored"
    // failure that a keypress clears).
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.keystore_locked = true;
    app.input.text = "hello".to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    let status = app.status.as_deref().expect("lock feedback status");
    assert!(
        status.contains("locked"),
        "must tell the user the daemon is locked, got: {status}"
    );
    assert!(
        rx.try_recv().is_err(),
        "no RunInput may be sent to a locked daemon"
    );
    assert!(app.keystore_locked, "the lock flag stays latched");
}

#[test]
fn terminal_event_esc_noop_on_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();

    let mut app = test_app();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert_eq!(app.page, Page::Chat);
    assert!(!app.should_quit);
}

#[test]
fn terminal_event_ctrl_c_noop_on_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+c");

    assert_eq!(app.page, Page::Chat);
    assert!(!app.should_quit);
    assert!(
        app.input.text.is_empty(),
        "Ctrl+C should not insert literal 'c'"
    );
    assert_eq!(app.input.cursor, 0, "cursor should remain at 0");
}

#[test]
fn global_ctrl_q_quits_from_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+q");

    assert!(app.should_quit);
}

#[test]
fn global_ctrl_q_quits_from_session_manager() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.page = Page::SessionManager;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+q from session manager");

    assert!(app.should_quit);
}

#[test]
fn global_ctrl_q_quits_from_ai_providers() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.page = Page::AIProviders;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+q from ai providers");

    assert!(app.should_quit);
}

#[test]
fn ctrl_p_does_not_insert_char_on_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+p");

    assert_eq!(app.input.text, "hello", "Ctrl+P should not insert 'p'");
    assert_eq!(app.input.cursor, 5);
    assert!(!app.should_quit);
}

#[test]
fn alt_x_does_not_insert_char_on_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+x");

    assert_eq!(app.input.text, "hello", "Alt+X should not insert 'x'");
    assert_eq!(app.input.cursor, 5);
    assert!(!app.should_quit);
}

#[test]
fn chat_ctrl_h_toggles_help() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.show_ctrl_help = false;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+h");

    assert!(app.show_ctrl_help, "first press should enable help");
}

#[test]
fn chat_ctrl_h_double_toggle_returns_to_off() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.show_ctrl_help = false;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+h (first)");
    assert!(app.show_ctrl_help, "first press should enable help");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+h (second)");
    assert!(!app.show_ctrl_help, "second press should disable help");
}

#[test]
fn chat_ctrl_a_enters_ai_providers() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+a");

    assert_eq!(app.page, Page::AIProviders);
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::ListAccounts);
}

#[test]
fn chat_ctrl_up_sends_undo() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+up");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::Undo);
}

#[test]
fn chat_ctrl_down_sends_redo() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+down");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::Redo);
}

#[test]
fn chat_esc_stops_active_session() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.attached_session_id = Some(42);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::Cancel { request_id: 0 });
    assert!(app.status.is_none(), "no status message on success");
}

#[test]
fn chat_esc_no_session_shows_status() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.attached_session_id = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert_eq!(app.status.as_deref(), Some("no session attached"));
}

#[test]
fn chat_alt_enter_continues_generation() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.attached_session_id = Some(42);
    let next_id = app.next_request_id;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+enter");

    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::ContinueGeneration {
            request_id: next_id
        }
    );
    assert!(
        app.display_for(0).active.contains(&next_id),
        "request_id should be in active set"
    );
    assert_eq!(
        app.next_request_id,
        next_id.wrapping_add(1),
        "next_request_id should be incremented"
    );
    assert!(app.status.is_none(), "no status message on success");
}

#[test]
fn chat_alt_enter_no_session_shows_status() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.attached_session_id = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+enter");

    assert_eq!(app.status.as_deref(), Some("no session attached"));
}

// ── Unsent prompt drafts are stored per session ──
//
// The input bar is a single shared buffer, so when the user switches
// sessions an unsubmitted prompt must be stashed against the session it was
// typed in and the target session's own draft loaded in its place.  These
// tests pin the hand-off in `attach_to_session` / `handle_session_created`
// and the clear-on-submit behaviour in the Enter handler.

#[cfg(test)]
mod unsent_draft_tests {
    use super::*;
    use crate::connection::handle_terminal_event;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// Attach to a session with a fresh (empty) display, mirroring what
    /// `attach_to_session` does for a session the user has never opened.
    fn attach(app: &mut App, session_id: u64) {
        let (tx, _rx) = std::sync::mpsc::channel();
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
        let (tx, _rx) = std::sync::mpsc::channel();
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
    fn switching_while_in_history_navigation_stashes_real_draft() {
        let (tx, _rx) = std::sync::mpsc::channel();
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
        // The user typed a fresh draft and pressed Up: the buffer now shows
        // the history entry, with the real draft stashed for the round trip.
        // The cursor sits mid-text (not at the end) to pin the position
        // fidelity of the stash/restore.
        app.input.text = "my draft".to_string();
        app.input.cursor = 5;
        app.navigate_history_up();
        assert_eq!(app.input.text, "past prompt");
        assert_eq!(app.saved_draft, "my draft");
        assert_eq!(app.saved_draft_cursor, 5, "cursor stashed on Up");

        // Switching sessions must stash the *real* draft, not the history
        // entry the buffer happens to be showing.
        app.attach_to_session(2, &tx).expect("attach to session 2");
        assert_eq!(app.display_for(1).draft, "my draft");
        assert_eq!(
            app.display_for(1).draft_cursor,
            5,
            "mid-text cursor survives the switch"
        );
        assert!(app.saved_draft.is_empty(), "history stash must be consumed");
        assert_eq!(app.history_index, None);
        assert!(app.input.is_empty(), "session 2 starts with no draft");

        // Returning to session 1 restores the user's own draft, cursor
        // position included.
        app.attach_to_session(1, &tx)
            .expect("attach back to session 1");
        assert_eq!(app.input.text, "my draft");
        assert_eq!(app.input.cursor, 5, "mid-text cursor restored");
        assert_eq!(app.history_index, None);
    }

    #[test]
    fn session_manager_switch_preserves_unsent_prompt_per_session() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = test_app();
        app.page = Page::SessionManager;
        app.session_mgr.set_sessions(vec![
            make_session(1, "first", "gpt-4", 3),
            make_session(2, "second", "claude", 5),
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
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
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
        let (tx, _rx) = std::sync::mpsc::channel();
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
        app.input.text = "my draft".to_string();
        app.input.cursor = 3;
        app.navigate_history_up();
        assert_eq!(app.input.text, "past prompt");

        // The user types over the history entry instead of exiting it.
        app.input.insert_char_at_cursor('X');
        assert_eq!(app.input.text, "past promptX");

        // Switching sessions must keep the edited buffer — the user's real
        // draft — rather than restoring the pre-Up stash and discarding the
        // edits.
        app.attach_to_session(2, &tx).expect("attach to session 2");
        assert_eq!(
            app.display_for(1).draft,
            "past promptX",
            "edits on top of a history entry become the session draft"
        );
        assert!(app.saved_draft.is_empty());
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
        let (tx, rx) = std::sync::mpsc::channel();

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
        assert_eq!(msg, ClientMessage::RefreshModels { force: true });
        assert_eq!(
            app.status.as_deref(),
            Some("refreshing models… (forced)"),
            "status set before the reply arrives"
        );
    }

    #[test]
    fn models_refreshed_updates_status() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();

        handle_daemon_message(
            DaemonMessage::ModelsRefreshed {
                providers: 208,
                models: 1234,
                status: RefreshStatus::Updated,
            },
            &mut app,
            &tx,
        )
        .expect("handle ModelsRefreshed");
        assert_eq!(
            app.status.as_deref(),
            Some("models updated (208 providers, 1234 models)")
        );

        handle_daemon_message(
            DaemonMessage::ModelsRefreshed {
                providers: 208,
                models: 1234,
                status: RefreshStatus::UpToDate,
            },
            &mut app,
            &tx,
        )
        .expect("handle ModelsRefreshed 304");
        assert_eq!(
            app.status.as_deref(),
            Some("models up to date (208 providers, 1234 models)")
        );

        handle_daemon_message(
            DaemonMessage::ModelsRefreshed {
                providers: 208,
                models: 1234,
                status: RefreshStatus::Forced,
            },
            &mut app,
            &tx,
        )
        .expect("handle ModelsRefreshed forced");
        assert!(app.status.as_deref().unwrap().contains("forced"));
    }

    #[test]
    fn models_refresh_failed_sets_error() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();

        handle_daemon_message(
            DaemonMessage::ModelsRefreshFailed {
                error: "network error".to_string(),
            },
            &mut app,
            &tx,
        )
        .expect("handle ModelsRefreshFailed");
        assert_eq!(
            app.error.as_deref(),
            Some("[daemon] refresh-models failed: network error")
        );
    }

    #[test]
    fn catalog_updated_replaces_provider_list_and_clamps_selection() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();

        // The default list starts from PROVIDER_OPTIONS (208 entries); park
        // the wizard highlight deep in the list.
        let default_len = app.providers.len();
        assert!(default_len > 10, "default provider list is large");
        app.ai_providers.wizard.focused = default_len - 1;

        // A catalog update with a small list replaces it and clamps.
        handle_daemon_message(
            DaemonMessage::CatalogUpdated {
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
            },
            &mut app,
            &tx,
        )
        .expect("handle CatalogUpdated");

        assert_eq!(app.providers.len(), 2);
        // The picker is always alphabetical, so the daemon's provenance order
        // is re-sorted: Anthropic leads.
        assert_eq!(app.providers[0].slug, "anthropic");
        assert_eq!(app.providers[0].display_name, "Anthropic");
        assert_eq!(app.providers[1].slug, "openai");
        // Highlight was beyond the new list's end → clamped to the last row.
        assert_eq!(app.ai_providers.wizard.focused, 1);
        assert_eq!(app.status.as_deref(), Some("catalog updated (2 providers)"));
    }

    #[test]
    fn catalog_updated_identical_list_does_not_churn_status() {
        // The daemon sends CatalogUpdated on every activity-subscribe; an
        // identical payload must not overwrite an unrelated status message.
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();
        app.status = Some("busy".to_string());

        let providers = app
            .providers
            .iter()
            .map(|p| CatalogProvider {
                slug: p.slug.clone(),
                display_name: p.display_name.clone(),
            })
            .collect();
        handle_daemon_message(DaemonMessage::CatalogUpdated { providers }, &mut app, &tx)
            .expect("handle CatalogUpdated");

        assert_eq!(app.status.as_deref(), Some("busy"), "status preserved");
        assert_eq!(app.providers.len(), 208);
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
    fn provider_picker_is_always_alphabetical() {
        // The static PROVIDER_OPTIONS default is in catalog (provenance)
        // order, not alphabetical; App::new must sort it by display name so
        // the wizard's picker reads A→Z from the very first frame.
        let app = test_app();
        let display_names = app
            .providers
            .iter()
            .map(|p| p.display_name.to_lowercase())
            .collect::<Vec<_>>();
        let mut sorted = display_names.clone();
        sorted.sort();
        assert_eq!(
            display_names, sorted,
            "default provider list must be alphabetical by display name"
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
        // Open the wizard and park the highlight deep in the (208-entry)
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
        let (tx, _rx) = std::sync::mpsc::channel();
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
        let (tx, _rx) = std::sync::mpsc::channel();
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
            app.ai_providers.credential.error.as_deref(),
            Some("API key cannot be empty")
        );
    }

    #[test]
    fn account_add_failed_closes_credential_modal() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();
        // The credential modal auto-opens right after submit; the daemon then
        // rejects the account.
        app.ai_providers.credential.open("my-account".to_string());

        handle_daemon_message(
            DaemonMessage::AccountAddFailed {
                name: "my-account".to_string(),
                error: "duplicate slug".to_string(),
            },
            &mut app,
            &tx,
        )
        .expect("handle AccountAddFailed");

        // No phantom account: the credential modal is dropped and the failure
        // is on the global error line.
        assert!(!app.ai_providers.credential.is_open());
        assert_eq!(
            app.error.as_deref(),
            Some("[daemon] failed to add account my-account: duplicate slug")
        );
    }

    #[test]
    fn account_add_failed_does_not_close_unrelated_credential_modal() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();
        // The failure reply can arrive AFTER the user has dismissed the
        // wizard's auto-opened modal and opened a different account's key
        // modal — that in-progress input must survive the late reply.
        app.ai_providers
            .credential
            .open("other-account".to_string());
        app.ai_providers.credential.input.text = "sk-in-progress".to_string();
        app.ai_providers.credential.input.cursor = 4;

        handle_daemon_message(
            DaemonMessage::AccountAddFailed {
                name: "my-account".to_string(),
                error: "duplicate slug".to_string(),
            },
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
            app.error.as_deref(),
            Some("[daemon] failed to add account my-account: duplicate slug")
        );
    }

    // ── Connection-level termination (eviction / server shutdown) ───────

    #[test]
    fn daemon_message_evicted_quits_with_message() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();

        handle_daemon_message(DaemonMessage::Evicted, &mut app, &tx).expect("handle Evicted");

        assert!(app.should_quit, "eviction must terminate the TUI");
        let msg = app.quit_message.as_deref().expect("quit message set");
        assert!(
            msg.contains("evicted"),
            "quit message must explain the eviction, got: {msg}"
        );
    }

    #[test]
    fn daemon_message_shutting_down_quits_with_message() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();

        handle_daemon_message(DaemonMessage::ShuttingDown, &mut app, &tx)
            .expect("handle ShuttingDown");

        assert!(app.should_quit, "server shutdown must terminate the TUI");
        let msg = app.quit_message.as_deref().expect("quit message set");
        assert!(
            msg.contains("shutting down"),
            "quit message must explain the shutdown, got: {msg}"
        );
    }
}
