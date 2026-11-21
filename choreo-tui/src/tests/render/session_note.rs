//! The status bar's status slot and the welcome panel's gate: what the Chat
//! page draws while an attach is in flight, when it fails, and once the
//! transcript has arrived.
//!
//! These read the drawn cells rather than the widget arguments, so a test
//! asserts on what reaches the screen — including the label's colour, which is
//! the only thing distinguishing the two client-side states from each other.

use super::{drawn_buffer, drawn_cells};
use crate::state::*;
use crate::tests::support::test_app;
use choreo_proto::{DaemonMessage, DaemonMessageType, SessionEvent, SessionStatus, Turn};
use ratatui::buffer::Buffer;

/// The harness's display anchor (see `tests/support.rs`): session `0` already
/// has a display and is the active session, so the tests attach to it.
const SESSION: u64 = 0;

/// The screen the tests draw: wide enough that nothing wraps or is clipped.
const WIDTH: u16 = 80;
const HEIGHT: u16 = 24;

/// The cell the first occurrence of `needle` starts on, searched row-major.
///
/// Colour assertions need the cell itself, which the flat symbol string cannot
/// carry. Every needle used here is single-width ASCII, so counting characters
/// gives the column directly.
fn cell_at(buffer: &Buffer, needle: &str) -> ratatui::buffer::Cell {
    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)).map(ratatui::buffer::Cell::symbol))
            .collect();
        if let Some(byte_index) = row.find(needle) {
            let x = row
                .get(..byte_index)
                .map(|prefix| prefix.chars().count())
                .and_then(|col| u16::try_from(col).ok())
                .expect("the column fits a u16");
            return buffer
                .cell((x, y))
                .expect("the cell the needle starts on")
                .clone();
        }
    }
    panic!("{needle:?} was not drawn");
}

/// An app attached to [`SESSION`] whose snapshot has not arrived yet.
fn loading_app() -> App {
    let mut app = test_app();
    app.attached_session_id = Some(SESSION);
    app
}

#[test]
fn the_status_slot_reports_loading_in_blue() {
    let mut app = loading_app();
    // No daemon status at all: the note is the only thing that can report the
    // state, and the slot still renders it.
    let buffer = drawn_buffer(&mut app, WIDTH, HEIGHT);
    let cell = cell_at(&buffer, "loading");
    assert_eq!(cell.fg, ratatui::style::Color::Blue);
    assert_ne!(
        cell.fg,
        ratatui::style::Color::DarkGray,
        "the note must not be dim"
    );
}

#[test]
fn the_status_slot_reports_a_failed_attach_in_red() {
    let mut app = loading_app();
    // The failed attach leaves no daemon status known (`attached_status` is
    // `None`) — exactly the case the note has to cover on its own.
    app.display_for(SESSION).attach_failed = true;
    assert_eq!(app.attached_status, None);

    let buffer = drawn_buffer(&mut app, WIDTH, HEIGHT);
    let cell = cell_at(&buffer, "error");
    assert_eq!(cell.fg, ratatui::style::Color::Red);
}

#[test]
fn the_status_slot_falls_back_to_the_daemon_status() {
    let mut app = loading_app();
    // The transcript arrived, so there is no note to report.
    app.display_for(SESSION).history_loaded = true;
    app.attached_status = Some(SessionStatus::Responding);

    let buffer = drawn_buffer(&mut app, WIDTH, HEIGHT);
    let cell = cell_at(&buffer, "responding");
    assert_eq!(cell.fg, ratatui::style::Color::LightGreen);
    assert_eq!(app.session_note(), None);
}

#[test]
fn the_welcome_panel_is_withheld_until_the_snapshot_arrives() {
    // An attached session with no turns is indistinguishable from an empty one
    // until the snapshot lands, so the panel must not stand in for a transcript
    // that has not arrived.
    let mut app = loading_app();
    let content = drawn_cells(&mut app, WIDTH, HEIGHT);
    assert!(
        !content.contains("Welcome to Choreographr"),
        "the panel must wait for the snapshot: {content:?}"
    );
    assert!(content.contains("loading"));
}

#[test]
fn the_welcome_panel_is_withheld_after_a_failed_attach() {
    let mut app = loading_app();
    app.display_for(SESSION).attach_failed = true;
    let content = drawn_cells(&mut app, WIDTH, HEIGHT);
    assert!(
        !content.contains("Welcome to Choreographr"),
        "a failed attach is not an empty session: {content:?}"
    );
    assert!(content.contains("error"));
}

#[test]
fn an_empty_snapshot_draws_the_welcome_panel() {
    let mut app = loading_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    crate::connection::handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(SESSION),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: std::collections::BTreeMap::new(),
                active_tool_groups: vec![],
                token_usage: None,
                context_window: None,
                last_prompt_tokens: None,
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("the handler accepts the snapshot");

    let content = drawn_cells(&mut app, WIDTH, HEIGHT);
    assert!(
        content.contains("Welcome to Choreographr"),
        "a genuinely empty session still gets the panel: {content:?}"
    );
}

#[test]
fn a_turn_before_the_snapshot_draws_a_transcript_not_a_blank_pane() {
    // The all-activity subscription can deliver `TurnAppended` before the
    // snapshot: the turn must be drawn, not held back behind `history_loaded`.
    let mut app = loading_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    crate::connection::handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(SESSION),
            event: SessionEvent::TurnAppended {
                turn_id: 1,
                turn: Turn {
                    created_at: choreo_proto::TimestampMs::now(),
                    undone: false,
                    error: None,
                    user_text: Some("BEFORE-SNAPSHOT".to_string()),
                    assistant_text: Some("answer".to_string()),
                    assistant_reasoning: None,
                    tool_calls: vec![],
                    token_usage: None,
                    tool_results: vec![],
                    displayed_images: vec![],
                    reasoning_artifact: None,
                    reasoning_producer: None,
                },
            },
        }),
        &mut app,
        &tx,
    )
    .expect("the handler accepts the turn");

    let content = drawn_cells(&mut app, WIDTH, HEIGHT);
    assert!(
        content.contains("BEFORE-SNAPSHOT"),
        "the turn is drawn as a transcript: {content:?}"
    );
    assert!(!content.contains("Welcome to Choreographr"));
}
