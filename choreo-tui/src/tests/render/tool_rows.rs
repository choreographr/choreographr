//! Where a tool row's text ends relative to the scrollbar column.
//!
//! A tool row is unboxed: its text runs from the viewport's left edge to one
//! column short of the reserved scrollbar column, and nothing else separates
//! it from the scrollbar.  The assertion is on the cells the backend actually
//! holds, so a wrap budget that leaves the row short of that column (a gap of
//! several blank columns before the scrollbar) is caught however it arises.

use crate::tests::support::test_app;
use choreo_proto::{ToolResultRecord, Turn};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[test]
fn tool_rows_stop_one_column_before_the_scrollbar() {
    const W: u16 = 80;
    const H: u16 = 24;

    let mut app = test_app();
    // A long unbreakable token hard-splits to fill each row exactly, so the
    // rightmost inked column measures the row's wrap budget rather than the
    // ragged edge a word boundary happens to leave.
    let desc = format!("Running command: `{}`.", "x".repeat(2000));
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "call1".into(),
            name: "sh".into(),
            content: String::new(),
            is_error: false,
            invocation_description: desc,
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.display_for(0).view.insert_or_replace(1, turn);
    app.rebuild_height_prefix();
    // The description is far taller than the viewport, so the history scrolls
    // and the scrollbar column is live.
    app.history_viewport.width = W - 1;
    app.history_viewport.height = H;

    let [history_area, ..] = app.chat_page_layout(W, H);

    let mut terminal = Terminal::new(TestBackend::new(W, H)).expect("test terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut app))
        .expect("render a frame");
    let buffer = terminal.backend().buffer();

    // The rightmost inked column over the whole history pane: every row of the
    // viewport is a wrapped description row, so this is the wrap budget.  The
    // scan stops at the scrollbar column, which inks itself (track and thumb)
    // rather than the text.
    let mut rightmost = 0;
    for y in history_area.top()..history_area.bottom() {
        for x in 0..W - 1 {
            if !buffer[(x, y)].symbol().trim().is_empty() {
                rightmost = rightmost.max(x);
            }
        }
    }

    // Column W-1 is the scrollbar and W-2 the row's one-column right margin,
    // so the text may reach W-3 and stops there.
    assert_eq!(
        rightmost,
        W - 3,
        "tool rows must fill to one column before the scrollbar"
    );
}
