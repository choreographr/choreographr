use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::{add_user_text, test_app};
use choreo_proto::Turn;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

// ── Command history tests ──────────────────────────────────────

#[test]
fn navigate_history_up_loads_most_recent() {
    let mut app = test_app();
    // Oldest first, newest last — user_texts() reverses so texts[0] = newest.
    add_user_text(&mut app, "cmd-2");
    add_user_text(&mut app, "cmd-1");
    add_user_text(&mut app, "cmd-0");
    // Recall requires an empty draft, so the input starts empty (its default).

    app.navigate_history_up();

    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "cmd-0");
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn navigate_history_up_with_nonempty_draft_is_a_noop() {
    // Gate: `Up` with a non-empty draft must NOT recall history.  The move to
    // the start of the line is handled by the caller (`connection/chat.rs`),
    // so at the state level this is a plain no-op.
    let mut app = test_app();
    add_user_text(&mut app, "cmd");
    app.input.text = "typing".to_string();
    app.input.cursor = 6;

    app.navigate_history_up();

    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "typing");
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn navigate_history_up_moves_to_older() {
    let mut app = test_app();
    add_user_text(&mut app, "c");
    add_user_text(&mut app, "b");
    add_user_text(&mut app, "a");
    app.history_index = Some(0);

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(1));
    assert_eq!(app.input.text, "b");

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(2));
    assert_eq!(app.input.text, "c");
}

#[test]
fn navigate_history_up_stops_at_oldest() {
    let mut app = test_app();
    add_user_text(&mut app, "c");
    add_user_text(&mut app, "b");
    app.history_index = Some(1);
    app.input.text = "b".to_string();
    app.input.cursor = 1;

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(1));
    assert_eq!(app.input.text, "b");
}

#[test]
fn navigate_history_up_empty_history_does_nothing() {
    let mut app = test_app();
    app.input.text = "hello".to_string();

    app.navigate_history_up();
    assert_eq!(app.input.text, "hello");
    assert!(app.history_index.is_none());
}

#[test]
fn navigate_history_down_exits_browsing_to_empty_draft() {
    let mut app = test_app();
    add_user_text(&mut app, "cmd");
    // Browsing started from an empty draft; Down past the newest entry ends
    // browsing and returns to that empty draft (there is no stash).
    app.history_index = Some(0);
    app.input.text = "cmd".to_string();
    app.input.cursor = 3;

    app.navigate_history_down();

    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn navigate_history_down_moves_to_newer() {
    let mut app = test_app();
    add_user_text(&mut app, "c");
    add_user_text(&mut app, "b");
    add_user_text(&mut app, "a");
    app.history_index = Some(2);
    app.input.text = "c".to_string();

    app.navigate_history_down();
    assert_eq!(app.history_index, Some(1));
    assert_eq!(app.input.text, "b");

    app.navigate_history_down();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "a");

    app.navigate_history_down();
    assert!(app.history_index.is_none());
}

#[test]
fn navigate_history_down_survives_shrunk_history() {
    let mut app = test_app();
    add_user_text(&mut app, "oldest");
    add_user_text(&mut app, "older");
    add_user_text(&mut app, "recent");
    // Simulate having browsed back to the oldest entry...
    app.history_index = Some(2);
    app.input.text = "oldest".to_string();

    // ...then the conversation changes underneath us: the turn list is reset
    // so only a single user text remains (e.g. a session switch mid-nav).
    {
        let view = &mut app.display_for(0).view;
        view.turns.clear();
        view.turns.insert(
            0,
            Turn {
                created_at: choreo_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("recent".to_string()),
                assistant_text: None,
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
                reasoning_artifact: None,
                reasoning_producer: None,
            },
        );
    }
    app.rebuild_height_prefix();

    // The stale index (2) must be clamped down to the newest remaining entry
    // (index 0) instead of panicking with an out-of-bounds index.
    app.navigate_history_down();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "recent");

    // And the next Down exits browsing back to the empty draft.
    app.navigate_history_down();
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "");
}

#[test]
fn navigate_history_down_empty_history_exits_to_empty() {
    let mut app = test_app();
    app.history_index = Some(3);
    app.input.text = "stale".to_string();

    // No turns at all: Down must fall back to the empty draft, not panic.
    app.navigate_history_down();
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "");
}

#[test]
fn navigate_history_up_survives_shrunk_history() {
    let mut app = test_app();
    add_user_text(&mut app, "oldest");
    add_user_text(&mut app, "older");
    add_user_text(&mut app, "recent");
    // Browsed all the way to the oldest entry...
    app.history_index = Some(2);
    app.input.text = "oldest".to_string();

    // ...then the turn list shrinks to a single entry underneath us (e.g. a
    // session switch mid-nav).
    {
        let view = &mut app.display_for(0).view;
        view.turns.clear();
        view.turns.insert(
            0,
            Turn {
                created_at: choreo_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("recent".to_string()),
                assistant_text: None,
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
                reasoning_artifact: None,
                reasoning_producer: None,
            },
        );
    }
    app.rebuild_height_prefix();

    // The stale index (2) must clamp to the oldest remaining entry (0) and
    // resync the displayed text — not silently no-op while showing stale text
    // from the pre-shrink list.
    app.navigate_history_up();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "recent");

    // Further Up presses stay at the oldest remaining entry.
    app.navigate_history_up();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "recent");

    // Down still exits browsing back to the empty draft.
    app.navigate_history_down();
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "");
}

#[test]
fn history_nav_resets_after_commit() {
    let mut app = test_app();
    add_user_text(&mut app, "old");
    app.history_index = Some(0);

    app.commit_to_history();

    assert!(app.history_index.is_none());
}

#[test]
fn terminal_event_up_down_navigates_history() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "older");
    add_user_text(&mut app, "recent");

    // Press Up — loads most recent
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert_eq!(app.input.text, "recent");

    // Press Up again — loads older
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert_eq!(app.input.text, "older");

    // Press Down — goes back to recent
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    assert_eq!(app.input.text, "recent");

    // Press Down — past newest, exits browsing to the empty draft
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    assert_eq!(app.input.text, "");
}

#[test]
fn terminal_event_history_up_empty_does_nothing() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert_eq!(app.input.text, "");
}

#[test]
fn terminal_event_down_on_last_draft_line_goes_to_end_of_line() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    // Editing a multi-line draft with no history entry loaded: Down on the
    // last visual line must land at end-of-line (which for the last logical
    // line is the end of the draft) instead of doing nothing.
    app.input.text = "first\nlast line".to_string();
    app.input.cursor = 3; // on "first" (line 0) — plain downward move

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    // Moved into the second visual line, preserving column 3 (byte 6 + 3).
    assert_eq!(app.input.cursor, 9);

    // Already on the last visual line — Down now terminates at the end.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    assert_eq!(app.input.cursor, app.input.text.len());
    // Still editing the draft — no history was involved.
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "first\nlast line");
}

#[test]
fn terminal_event_down_on_single_line_draft_goes_to_end() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 2;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn terminal_event_up_on_nonempty_draft_moves_to_line_start() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "older");
    add_user_text(&mut app, "recent");
    // The user is editing a multi-line draft when they press Up.
    app.input.text = "line one\ndraft".to_string();
    app.input.cursor = 5; // mid-way through the first visual line

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    // Recall requires an empty draft: with a non-empty draft, Up on the first
    // visual line moves the cursor to the start of the line instead, leaving
    // the draft (and history) untouched.
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "line one\ndraft");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn terminal_event_down_still_navigates_history_after_up() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "older");
    add_user_text(&mut app, "recent");
    // Recall requires an empty draft, so browsing starts from an empty prompt.

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert_eq!(app.input.text, "recent");
    assert!(app.history_index.is_some());

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    // Down exits browsing back to the empty draft.
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "");
    assert_eq!(app.input.cursor, 0);

    // Subsequent Down is back in draft mode on an empty buffer: nothing below
    // it, so it is a no-op at end-of-line.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    assert_eq!(app.input.text, "");
}

#[test]
fn terminal_event_editing_recalled_entry_detaches_it_into_draft() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "recent prompt");

    // Recall from the empty draft, then add a character.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert!(app.history_index.is_some());
    assert_eq!(app.input.text, "recent prompt");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle char");

    // The first edit detaches the recalled entry: browsing ends and the
    // edit stays in the buffer as the draft.
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "recent prompt!");

    // A subsequent session switch stores that edit as the source session's
    // draft (the recalled entry is not stashed — only the user's edit is).
    app.attached_session_id = Some(0);
    app.persist_input_draft(1);
    assert_eq!(app.display_for(0).draft, "recent prompt!");
}

#[test]
fn terminal_event_ctrl_backspace_inert_while_browsing() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "recent prompt");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert!(app.history_index.is_some());
    assert_eq!(app.input.text, "recent prompt");

    // Ctrl+Backspace is inert while browsing: it neither clears the recalled
    // entry nor detaches it.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+backspace");

    assert!(app.history_index.is_some());
    assert_eq!(app.input.text, "recent prompt");
}

#[test]
fn terminal_event_ctrl_a_moves_cursor_to_line_start() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.input.text = "hello world".to_string();
    app.input.cursor = 11;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+a");

    assert_eq!(app.input.cursor, 0, "Ctrl+A is readline beginning-of-line");
    assert_eq!(app.input.text, "hello world", "Ctrl+A must not edit");
}

#[test]
fn terminal_event_ctrl_p_recalls_history_like_up() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "recent prompt");

    // Ctrl+P from an empty draft recalls the previous prompt (readline
    // previous-history), exactly like Up.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+p");
    assert!(app.history_index.is_some());
    assert_eq!(app.input.text, "recent prompt");

    // Ctrl+N walks back to the empty draft (next-history), like Down.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+n");
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "");
}

#[test]
fn terminal_event_ctrl_k_kills_to_line_end() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.input.text = "abc\ndef".to_string();
    app.input.cursor = 1;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+k");

    assert_eq!(app.input.text, "a\ndef");
}

#[test]
fn terminal_event_alt_char_while_browsing_does_not_detach() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "recent prompt");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert!(app.history_index.is_some());

    // An Alt+letter chord is not an edit (`InputBuffer::handle_key` ignores it),
    // so it must not detach the recalled entry — only real buffer mutations do.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+x");

    assert!(
        app.history_index.is_some(),
        "a non-edit key must not detach"
    );
    assert_eq!(app.input.text, "recent prompt");
}

#[test]
fn terminal_event_noop_ctrl_u_while_browsing_does_not_detach() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "recent prompt");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    // Move the cursor to the start of the line, where Ctrl+U deletes nothing.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle home");
    assert_eq!(app.input.cursor, 0);
    assert!(app.history_index.is_some());

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+u");

    // Ctrl+U at the line start edits nothing, so browsing stays intact (the
    // buffer's `generation` is not bumped).
    assert!(app.history_index.is_some(), "a no-op edit must not detach");
    assert_eq!(app.input.text, "recent prompt");
}

#[test]
fn terminal_event_empty_paste_while_browsing_does_not_detach() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    add_user_text(&mut app, "recent prompt");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert!(app.history_index.is_some());

    // An empty paste mutates nothing, so it must leave browsing intact.
    handle_terminal_event(Event::Paste(String::new()), &mut app, &tx).expect("handle empty paste");

    assert!(
        app.history_index.is_some(),
        "an empty paste must not detach"
    );
    assert_eq!(app.input.text, "recent prompt");
}

#[test]
fn terminal_event_down_on_last_draft_line_wrapped_multibyte() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    // Wide characters end up in the middle of the last visual line; Down
    // must land on a char boundary at the very end, not inside a grapheme.
    app.input.text = "日本語テキスト".to_string();
    app.input.cursor = 3; // after '本'

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    assert_eq!(app.input.cursor, app.input.text.len());
    assert!(app.input.text.is_char_boundary(app.input.cursor));
}

#[test]
fn commit_does_not_duplicate_user_text() {
    let mut app = test_app();
    add_user_text(&mut app, "hello");

    app.commit_to_history();

    assert_eq!(app.user_texts().len(), 1);
    assert_eq!(app.user_texts()[0], "hello");
}

#[expect(clippy::cast_possible_truncation)] // test indices/heights far below u16::MAX
#[test]
fn click_on_reasoning_header_toggles_collapse() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("Hidden thinking.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    // No user text: assistant separator (0), padding (1), response (2),
    // blank separator (3), then the reasoning header at rows [4,5).
    let (start, end) = app.active_display().unwrap().turn_layouts[0]
        .reasoning_header_range
        .expect("reasoning header range should exist");
    assert_eq!((start, end), (4, 5), "header sits below the response");

    // `find_turn_at_row` maps screen rows linearly only when the history
    // fills the viewport, so size the viewport to the content and click the
    // header's first row, which corresponds to content line `start`.
    let total = app.active_display().unwrap().total_history_height();
    app.history_viewport.height = total as u16;
    let row = start as u16;

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");

    // Default was collapsed (response present) → the click expands it.
    assert_eq!(
        app.active_display().unwrap().reasoning_override.get(&1),
        Some(&true),
        "clicking the header should expand reasoning"
    );
}

#[expect(clippy::cast_possible_truncation)] // test indices/heights far below u16::MAX
#[test]
fn click_on_reasoning_header_toggles_collapse_when_content_fits_viewport() {
    // Regression: on sessions whose history is shorter than the viewport (no
    // scrollbar), the content is anchored to the bottom of the viewport.
    // Clicking the reasoning header must resolve to the right content line
    // despite the blank band above it.
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("Hidden thinking.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    let (start, total) = {
        let display = app.active_display().unwrap();
        let (start, _end) = display.turn_layouts[0]
            .reasoning_header_range
            .expect("reasoning header range should exist");
        (start, display.total_history_height())
    };
    assert!(
        total < app.history_viewport.height as usize,
        "test requires a session too short to need the scrollbar"
    );

    // The header renders at the bottom-anchored position.
    let row = (app.history_viewport.height as usize - total + start) as u16;
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");

    assert_eq!(
        app.active_display().unwrap().reasoning_override.get(&1),
        Some(&true),
        "clicking the header on a short session should expand reasoning"
    );
}

#[expect(clippy::cast_possible_truncation)] // test indices/heights far below u16::MAX
#[test]
fn click_on_tool_result_header_toggles_collapse() {
    // Clicking a tool result's header row (triangle + description) toggles
    // that result's collapsible body.  A quiet tool (read_file) defaults to
    // collapsed, so the first click expands it and the second collapses it.
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![choreo_proto::ToolResultRecord {
            call_id: "call-1".into(),
            name: "read_file".into(),
            content: "file contents".into(),
            is_error: false,
            invocation_description: "Reading file `src/main.rs`.".into(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    // The collapsed quiet result renders a single header line, so its click
    // range is content lines [0, 1).
    let (start, end) = app.active_display().unwrap().turn_layouts[0]
        .tool_result_header_ranges
        .first()
        .copied()
        .expect("quiet tool result must have a header range");
    assert_eq!((start, end), (0, 1));

    // Size the viewport to the content so screen row == content line.
    let total = app.active_display().unwrap().total_history_height();
    app.history_viewport.height = total as u16;
    let row = start as u16;

    // First click: expands the collapsed quiet result.
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");
    assert_eq!(
        app.active_display()
            .unwrap()
            .tool_collapse_override
            .get(&1)
            .and_then(|m| m.get("call-1")),
        Some(&false),
        "clicking the header of a collapsed quiet result should expand it"
    );

    // Second click: collapses again.
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");
    assert_eq!(
        app.active_display()
            .unwrap()
            .tool_collapse_override
            .get(&1)
            .and_then(|m| m.get("call-1")),
        Some(&true),
        "clicking the header again should collapse the result"
    );
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)] // test-index/scroll arithmetic with values guaranteed in range
#[test]
fn click_on_reasoning_header_toggles_collapse_when_scrolled() {
    // Regression: on sessions with a scrollbar, once the user scrolls away
    // from the bottom the click mapping must account for the scroll offset
    // (content line `c` sits at screen row `vh - total + scroll + c`).  A
    // naive "content starts at row 0" mapping broke header clicks here.
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;

    // Several full turns so the history overflows the viewport.
    for i in 0..5 {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(format!("user {i}")),
            assistant_text: Some(format!("assistant response {i}")),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(i as u32, turn);
    }
    // The reasoning-bearing turn to click on.
    let target = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("Hidden thinking.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(99, target);
    app.rebuild_height_prefix();

    let total = app.active_display().unwrap().total_history_height();
    let vh = app.history_viewport.height as usize;
    assert!(total > vh, "test requires a session with a scrollbar");

    // Find the target turn's reasoning header range, then scroll so the
    // header is visible near the bottom of the viewport.
    let target_idx = {
        let display = app.active_display().unwrap();
        display
            .visible_turn_ids
            .iter()
            .position(|&id| id == 99)
            .expect("target turn should be visible")
    };
    let header_start = {
        let display = app.active_display().unwrap();
        let (start, _end) = display.turn_layouts[target_idx]
            .reasoning_header_range
            .expect("reasoning header range should exist");
        start
    };
    // Absolute content line of the header (turn start + in-turn offset).
    let header_content_line = display_content_line(&app, target_idx) + header_start;

    // Scroll so the header lands at row `vh - 2` (near the bottom, on
    // screen).  Content line `c` sits at screen row `c - (total - scroll -
    // vh)`, so solve for scroll and clamp into [0, max_scroll].
    let max_scroll = total - vh;
    let target_row = (vh - 2) as isize;
    let scroll = (target_row + total as isize - vh as isize - header_content_line as isize)
        .clamp(0, max_scroll as isize) as usize;
    app.scroll_to(scroll);

    // Sanity: the header must be visible at the expected row, mapped to its
    // in-turn offset.
    let header_row =
        (vh as isize - total as isize + scroll as isize + header_content_line as isize) as u16;
    let (_idx, offset) = find_turn_at_row(&app, header_row).expect("row must map");
    assert_eq!(
        offset, header_start,
        "header content line must be at the expected row"
    );

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: header_row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");

    assert_eq!(
        app.active_display().unwrap().reasoning_override.get(&99),
        Some(&true),
        "clicking the header on a scrolled session should expand reasoning"
    );
}

/// Content line where the turn at `turn_idx` starts (0 for the first turn).
fn display_content_line(app: &App, turn_idx: usize) -> usize {
    app.active_display_ref()
        .and_then(|d| {
            turn_idx
                .checked_sub(1)
                .and_then(|prev| d.height_prefix.get(prev))
        })
        .copied()
        .unwrap_or(0)
}

#[test]
fn scroll_mouse_outside_history_box_does_not_update_accumulator() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.history_viewport.height = 1;

    // Pin a fixed terminal instead of querying the real one: the test runner
    // may have no TTY at all, and the mouse arm only compares the row against
    // `history_viewport`, so the bottom row of any terminal taller than the
    // one-row history box is a position outside it.
    let height = 24;
    app.last_terminal_size = Some((80, height));
    let row = height - 1; // input area, outside history box

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle mouse");

    assert_eq!(
        app.scroll_accumulator, 0,
        "accumulator must remain unchanged"
    );
}
