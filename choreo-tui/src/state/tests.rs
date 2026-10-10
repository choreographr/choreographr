//! Unit tests for `state`, split by subject into the child modules below.
//!
//! Each child owns one concern of the display-state plumbing — input, the
//! session switcher, the scroll/height model, the reasoning/tool-result
//! collapse state, image layout, viewport-change classification, streaming,
//! and the request lifecycle. This parent holds only the fixtures shared
//! across children (`make_session`, `insert_turn`) and declares the modules;
//! `terminal_status/tests.rs` reaches `make_session` as
//! `crate::state::tests::make_session`.

use crate::state::{App, SessionStatus};
use choreo_proto::{SessionSummary, Turn};

mod content_version;
mod find_turn;
mod height;
mod images;
mod input;
mod markers;
mod model_selector;
mod reasoning;
mod request_lifecycle;
mod scroll;
mod session_manager;
mod session_switch;
mod status_height;
mod streaming;
mod tool_result;
mod turn_live_content;
mod undo_pruning;
mod viewport;
mod window;

pub(super) fn make_session(id: u64, title: &str) -> SessionSummary {
    SessionSummary {
        session_id: id,
        title: Some(title.into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1000,
        // Decreasing with id so the session manager's sort keeps the
        // fixtures in ascending-id order (the order these tests assume);
        // the value stays small so an explicit `handle_session_status_changed`
        // timestamp still overrides it in the monotonicity test.
        last_modified: 1000 - id.cast_signed(),
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec!["core".into()],
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    }
}

/// Helper: insert a minimal turn into `app`.
pub(super) fn insert_turn(app: &mut App, id: u32, user_text: &str, assistant_text: &str) {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.into()),
        assistant_text: Some(assistant_text.into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.display_for(0).view.insert_or_replace(id, turn);
}
