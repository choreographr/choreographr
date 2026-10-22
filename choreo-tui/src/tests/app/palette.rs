use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::test_app;
use choreo_proto::{ClientMessage, ClientMessageType, ReasoningCapability};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use super::press_enter;

// ── Inline command palette + unified command dispatch ──────────────────
//
// Task 3 wired the palette (typed `/`), the keyboard shortcuts, and the
// `/quit`-and-friends command variants into one dispatch path.  These tests
// drive the full terminal-event pipeline to pin the end-to-end behavior.

/// Send one unmodified key through the full terminal-event pipeline.
fn press(app: &mut App, tx: &crossbeam_channel::Sender<ClientMessage>, code: KeyCode) {
    handle_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), app, tx)
        .expect("handle key");
}

#[test]
fn typing_slash_on_empty_prompt_starts_a_command_line() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    assert!(!app.command_palette_active(), "inactive before typing");

    press(&mut app, &tx, KeyCode::Char('/'));

    assert_eq!(app.input.text, "/", "the `/` is stored in the buffer");
    assert!(app.command_palette_active());
    assert_eq!(
        app.command_palette_matches().len(),
        choreo_client_core::command_catalog().len(),
        "an empty command line shows the whole catalog"
    );
}

#[test]
fn typing_further_narrows_palette_to_model() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    for c in ['m', 'o'] {
        press(&mut app, &tx, KeyCode::Char(c));
    }
    assert_eq!(app.input.text, "/mo", "the command line keeps its slash");
    let names: Vec<&str> = app
        .command_palette_matches()
        .iter()
        .map(|m| m.spec.name)
        .collect();
    assert_eq!(names, vec!["model"]);
}

#[test]
fn palette_up_down_move_the_highlight() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    assert_eq!(app.command_palette_focused(), 0);

    press(&mut app, &tx, KeyCode::Down);
    assert_eq!(app.command_palette_focused(), 1);
    press(&mut app, &tx, KeyCode::Up);
    assert_eq!(app.command_palette_focused(), 0);
    // The move keys must not have edited the input.
    assert_eq!(app.input.text, "/", "navigation must not edit the buffer");
}

#[test]
fn palette_tab_completes_name_without_submitting() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    for c in ['m', 'o'] {
        press(&mut app, &tx, KeyCode::Char(c));
    }

    press(&mut app, &tx, KeyCode::Tab);

    assert_eq!(
        app.input.text, "/model ",
        "Tab completes the name, keeping the slash"
    );
    assert_eq!(app.input.cursor, "/model ".len());
    assert!(
        app.command_palette_active(),
        "Tab keeps the command line active"
    );
    assert!(rx.try_recv().is_err(), "Tab must never submit");
}

#[test]
fn palette_enter_runs_the_command_and_clears_the_line() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    for c in "model".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }

    press(&mut app, &tx, KeyCode::Enter);

    // `Enter` RUNS `/model`, which opens the selector (not completion).
    assert!(
        app.model_selector.is_open(),
        "Enter runs `/model`, opening the model selector"
    );
    assert_eq!(
        rx.recv().expect("ListModels").inner,
        ClientMessageType::ListModels
    );
    assert!(
        !app.command_palette_active(),
        "running a command clears the line"
    );
    assert!(
        app.input.is_empty(),
        "the command line is discarded after running"
    );
}

#[test]
fn palette_shift_enter_runs_the_command_without_a_newline() {
    // Shift must not insert a newline in command mode: the command line is
    // single-line, so `Shift+Enter` runs the command exactly like plain
    // `Enter` (both are intercepted before the prompt's newline binding).
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    for c in "model".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        &mut app,
        &tx,
    )
    .expect("shift+enter");

    assert!(
        app.model_selector.is_open(),
        "Shift+Enter runs the command like Enter"
    );
    assert_eq!(
        rx.recv().expect("ListModels").inner,
        ClientMessageType::ListModels
    );
    assert!(
        !app.command_palette_active(),
        "running the command clears the line"
    );
    assert!(
        app.input.text.is_empty(),
        "no stray newline survives command-line completion"
    );
}

/// Row index of `name` in the palette on an empty command line — the palette
/// lists the shared catalog verbatim, so this is just the catalog position.
/// Deriving the row keeps these tests robust to catalog reordering.
fn palette_row_of(name: &str) -> usize {
    choreo_client_core::command_catalog()
        .iter()
        .position(|spec| spec.name == name)
        .expect("command is in the catalog")
}

#[test]
fn palette_enter_on_empty_line_runs_the_highlighted_command() {
    // The empty line highlights the first catalog row; Enter runs whichever
    // row is highlighted — no preceding `Tab`.  Drive to `/session` (wherever
    // it sits in the alphabetical catalog) so the assertion can name it.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    assert!(app.command_palette_active());
    assert_eq!(app.command_palette_focused(), 0);

    let session_row = palette_row_of("session");
    for _ in 0..session_row {
        press(&mut app, &tx, KeyCode::Down);
    }
    assert_eq!(app.command_palette_focused(), session_row);

    press(&mut app, &tx, KeyCode::Enter);

    assert_eq!(
        app.page,
        Page::SessionManager,
        "Enter ran the highlighted `/session`"
    );
    assert_eq!(
        rx.recv().expect("ListSessions").inner,
        ClientMessageType::ListSessions
    );
    assert_eq!(
        rx.recv().expect("SubscribeSessionsSummary").inner,
        ClientMessageType::SubscribeSessionsSummary
    );
    assert!(
        !app.command_palette_active(),
        "running a command clears the line"
    );
    assert!(app.input.is_empty());
}

#[test]
fn palette_enter_on_a_partial_token_runs_the_highlighted_command() {
    // Typing a prefix narrows the palette; Enter runs the highlighted command
    // without a `Tab` completing it first.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    for c in "mo".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }
    assert_eq!(app.command_palette_matches().len(), 1);

    press(&mut app, &tx, KeyCode::Enter);

    assert!(
        app.model_selector.is_open(),
        "Enter ran the highlighted `/model`"
    );
    assert_eq!(
        rx.recv().expect("ListModels").inner,
        ClientMessageType::ListModels
    );
    assert!(
        !app.command_palette_active(),
        "running a command clears the line"
    );
}

#[test]
fn palette_enter_runs_the_row_the_arrows_selected() {
    // Arrows move the highlight over the whole catalog on an empty line; Enter
    // then runs whichever row is highlighted.  Arrow down to `/model` at its
    // current catalog position (the catalog is alphabetical, so this is not
    // hardcoded to a fixed row).
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    let model_row = palette_row_of("model");
    for _ in 0..model_row {
        press(&mut app, &tx, KeyCode::Down);
    }
    assert_eq!(app.command_palette_focused(), model_row);

    press(&mut app, &tx, KeyCode::Enter);

    assert!(
        app.model_selector.is_open(),
        "Enter ran the arrow-selected `/model`"
    );
    assert_eq!(
        rx.recv().expect("ListModels").inner,
        ClientMessageType::ListModels
    );
    assert!(!app.command_palette_active());
}

#[test]
fn palette_enter_runs_an_unknown_command() {
    // `command_palette_active()` must stay TRUE with zero matches, so an
    // unmatched line still submits (and the daemon/parser reports it).
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    for c in "zzz-no-such-command".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }
    assert!(
        app.command_palette_active(),
        "active even with zero matches"
    );
    assert!(app.command_palette_matches().is_empty());

    press(&mut app, &tx, KeyCode::Enter);

    assert!(rx.try_recv().is_err(), "an unknown command sends nothing");
    assert!(
        !app.command_palette_active(),
        "Enter always clears the command line"
    );
    assert!(app.input.is_empty());
    assert!(
        app.status()
            .is_some_and(|s| s.starts_with("unknown command")),
        "got: {:?}",
        app.status()
    );
}

#[test]
fn palette_esc_discards_the_command_line_without_cancelling() {
    let mut app = test_app();
    // A live session means the normal Esc would send Cancel.
    app.attached_session_id = Some(42);
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    assert!(app.command_palette_active());

    press(&mut app, &tx, KeyCode::Esc);

    assert!(
        !app.command_palette_active(),
        "Esc discards the command line"
    );
    assert!(app.input.text.is_empty(), "Esc returns to an empty prompt");
    assert!(
        rx.try_recv().is_err(),
        "exiting command mode must NOT cancel generation"
    );
}

#[test]
fn slash_with_existing_text_inserts_literally() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.input.text = "hi".to_string();
    app.input.cursor = 2;

    press(&mut app, &tx, KeyCode::Char('/'));

    assert!(
        !app.command_palette_active(),
        "a non-empty prompt keeps `/` literal"
    );
    assert_eq!(app.input.text, "hi/");
}

#[test]
fn deleting_the_slash_hides_the_palette() {
    // The palette is derived from the buffer — a `/` starts a command line —
    // so deleting that one character returns to a plain prompt.
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    assert!(app.command_palette_active());

    press(&mut app, &tx, KeyCode::Backspace);

    assert!(app.input.text.is_empty(), "the slash is deleted");
    assert!(
        !app.command_palette_active(),
        "deleting the leading `/` hides the palette"
    );
}

#[test]
fn a_double_slash_is_run_verbatim_never_silently_prepended() {
    // Regression for the reported bug: the buffer holds the `/` the user
    // typed, so a stray second slash shows `//acl` — and `//acl` is exactly
    // what runs (rejected as unknown), never a hidden extra `/` turning a
    // typed `/acl` into `//acl`.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    press(&mut app, &tx, KeyCode::Char('/'));
    press(&mut app, &tx, KeyCode::Char('/'));
    for c in "acl".chars() {
        press(&mut app, &tx, KeyCode::Char(c));
    }
    assert_eq!(app.input.text, "//acl", "both slashes are visible");

    press(&mut app, &tx, KeyCode::Enter);

    assert!(rx.try_recv().is_err(), "an unknown command sends nothing");
    assert!(app.input.is_empty());
    assert_eq!(
        app.status(),
        Some("unknown command: //acl"),
        "the run reflects exactly what the user saw"
    );
}

#[test]
fn literal_slash_command_line_runs() {
    // A `/model` line set directly (e.g. pasted) is a command line (the buffer
    // starts with `/`) and runs when submitted.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.input.text = "/model".to_string();
    app.input.cursor = "/model".len();

    press(&mut app, &tx, KeyCode::Enter);

    assert!(app.model_selector.is_open(), "`/model` opens the selector");
    assert_eq!(
        rx.recv().expect("ListModels").inner,
        ClientMessageType::ListModels
    );
    assert!(!app.command_palette_active());
}

#[test]
fn attaching_a_session_discards_a_command_line_without_a_draft() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.display_for(1);
    app.input.text = "/model".to_string();
    app.input.cursor = "/model".len();

    app.attach_to_session(2, &tx).expect("attach to session 2");

    assert!(
        !app.command_palette_active(),
        "switching sessions discards the command line"
    );
    assert!(
        app.input.is_empty(),
        "the command line must not become a session draft"
    );
    assert_eq!(
        app.display_for(1).draft,
        "",
        "no command leaked into a draft"
    );
}

#[test]
fn alt_r_cycles_reasoning_effort() {
    let mut app = test_app();
    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec!["off".into(), "low".into(), "high".into()],
    });
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("alt+r");

    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("low"));
    assert_eq!(app.status(), Some("reasoning: low"));
    assert_eq!(
        rx.recv().expect("sent message").inner,
        ClientMessageType::SetReasoningEffort {
            effort: "low".to_string()
        }
    );
}

#[test]
fn alt_s_opens_session_manager() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("alt+s");

    assert_eq!(app.page, Page::SessionManager);
    assert_eq!(
        rx.recv().expect("ListSessions").inner,
        ClientMessageType::ListSessions
    );
    assert_eq!(
        rx.recv().expect("SubscribeSessionsSummary").inner,
        ClientMessageType::SubscribeSessionsSummary
    );
    // A bare keypress must not echo a `> /session` status.
    assert!(app.status().is_none());
}

#[test]
fn alt_m_opens_selector_and_requests_models() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("alt+m");

    assert!(app.model_selector.is_open());
    assert_eq!(
        rx.recv().expect("ListModels").inner,
        ClientMessageType::ListModels
    );
    assert!(app.status().is_none(), "a bare keypress must not echo");
}

#[test]
fn quit_command_sets_should_quit() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.input.text = "/quit ".to_string();

    press_enter(&mut app, &tx);

    assert!(app.should_quit, "/quit exits the TUI");
}
