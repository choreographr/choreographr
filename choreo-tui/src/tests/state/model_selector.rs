//! `ModelSelectorState`: open/close, filtering, focus movement, windowing,
//! submit, and key handling.

use crate::state::{MODEL_LIST_MAX_RETRIES, ModelSelectorState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// ── ModelSelectorState ──

fn selector_with_models(models: &[&str]) -> ModelSelectorState {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(
        models.iter().map(|s| (*s).to_string()).collect(),
        None,
        false,
    );
    sel
}

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against ""
fn model_selector_open_resets_state_and_marks_loading() {
    let mut sel = ModelSelectorState::new();
    sel.all_models = vec!["a".into()];
    sel.selected = Some("a".into());
    sel.filter.text = "stale".to_string();
    sel.filter.cursor = 5;
    sel.focused = 3;
    sel.scroll = 2;
    sel.error.set("old error");

    sel.open();

    assert!(sel.is_open());
    assert!(sel.loading);
    assert!(sel.filter.text.is_empty());
    assert_eq!(sel.focused, 0);
    assert_eq!(sel.scroll, 0);
    assert!(sel.error.get().is_none());
}

#[test]
fn model_selector_close_keeps_model_list() {
    let mut sel = selector_with_models(&["a", "b"]);
    sel.close();

    assert!(!sel.is_open());
    assert_eq!(sel.all_models.len(), 2, "cached list survives close");
}

#[test]
fn model_selector_apply_models_preselects_current() {
    let mut sel = ModelSelectorState::new();
    sel.open();
    // The daemon already orders ids case-insensitively by id, so the picker
    // presents them verbatim and only has to find the active row.
    sel.apply_models(
        vec![
            "gpt-3.5-turbo".into(),
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
        ],
        Some("gpt-4o-mini".into()),
        false,
    );

    assert!(!sel.loading);
    assert_eq!(sel.focused, 2, "highlight lands on the active model");
    assert_eq!(sel.highlighted().as_deref(), Some("gpt-4o-mini"));
    assert!(
        !sel.provisional,
        "a reply that says verified stays verified"
    );
}

#[test]
fn model_selector_apply_models_keeps_the_daemon_order() {
    // Ordering is the daemon's decision (it sorts every list it serves, so
    // `Models`, `ModelsUpdated` and `/model` agree). The picker must not impose
    // a second, client-side order that could disagree with them.
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(
        vec![
            "GPT-4O".into(),
            "claude-3".into(),
            "gpt-4o-mini".into(),
            "o3-deep-research".into(),
        ],
        None,
        false,
    );
    assert_eq!(
        sel.all_models,
        vec!["GPT-4O", "claude-3", "gpt-4o-mini", "o3-deep-research"]
    );
}

#[test]
fn model_selector_apply_models_records_provisional() {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(vec!["seed-a".into()], None, true);
    assert!(
        sel.provisional,
        "a catalog seed is a stand-in, not a live list"
    );
}

#[test]
fn model_selector_refresh_keeps_the_highlighted_row_and_clears_provisional() {
    // A background fetch landing while the picker is open must not move the
    // cursor: the user browses rows, and the ACTIVE model (which `apply_models`
    // preselects) is not where they are looking.
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(
        vec!["a-model".into(), "b-model".into(), "c-model".into()],
        Some("a-model".into()),
        true,
    );
    assert_eq!(sel.highlighted().as_deref(), Some("a-model"));
    // Navigate down to the last row, as a user reading the list would.
    sel.focused = 2;
    assert_eq!(sel.highlighted().as_deref(), Some("c-model"));

    // The live list arrives, with one more model appended.
    sel.refresh_models(vec![
        "a-model".into(),
        "b-model".into(),
        "c-model".into(),
        "d-model".into(),
    ]);

    assert_eq!(
        sel.highlighted().as_deref(),
        Some("c-model"),
        "the refresh keeps the row the user was on, not the active model"
    );
    assert_eq!(
        sel.selected.as_deref(),
        Some("a-model"),
        "active model is unchanged"
    );
    assert!(!sel.provisional, "a refreshed list is verified");
}

#[test]
fn model_selector_refresh_clamps_when_the_highlighted_row_vanishes() {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(
        vec!["a-model".into(), "b-model".into(), "c-model".into()],
        None,
        false,
    );
    sel.focused = 2;

    // The model the user was on is gone from the live list.
    sel.refresh_models(vec!["a-model".into(), "b-model".into()]);

    assert_eq!(sel.focused, 1, "focus clamps to the last surviving row");
    assert_eq!(sel.highlighted().as_deref(), Some("b-model"));
}

#[test]
fn model_selector_apply_models_falls_back_to_top_when_selected_missing() {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(vec!["a".into(), "b".into()], Some("missing".into()), false);

    assert_eq!(sel.focused, 0);
    assert_eq!(sel.highlighted().as_deref(), Some("a"));
}

#[test]
fn model_selector_filter_matches_case_insensitive_substring() {
    let mut sel = selector_with_models(&["gpt-4o", "GPT-4O-MINI", "claude-3"]);
    sel.filter.text = "gpt".to_string();
    sel.filter.cursor = 3;

    let filtered = sel.filtered();
    assert_eq!(filtered, vec!["gpt-4o", "GPT-4O-MINI"]);
}

#[test]
fn model_selector_empty_filter_returns_all() {
    let sel = selector_with_models(&["a", "b", "c"]);
    assert_eq!(sel.filtered(), vec!["a", "b", "c"]);
}

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against []
fn model_selector_no_match_returns_empty() {
    let mut sel = selector_with_models(&["a", "b"]);
    sel.filter.text = "zzz".to_string();
    sel.filter.cursor = 3;
    assert!(sel.filtered().is_empty());
}

#[test]
fn model_selector_focus_clamps_when_filter_narrows_list() {
    let mut sel = selector_with_models(&["a", "b", "c"]);
    sel.focused = 2;
    // Narrow to a single row: the highlight must not point past the end.
    sel.filter.text = "a".to_string();
    sel.filter.cursor = 1;
    sel.clamp_focus();
    assert_eq!(sel.focused, 0);
}

#[test]
fn model_selector_move_up_down_clamped() {
    let mut sel = selector_with_models(&["a", "b", "c"]);
    sel.move_down();
    assert_eq!(sel.focused, 1);
    sel.move_down();
    sel.move_down();
    assert_eq!(sel.focused, 2, "move_down clamps at the last row");
    sel.move_up();
    assert_eq!(sel.focused, 1);
    sel.move_up();
    sel.move_up();
    assert_eq!(sel.focused, 0, "move_up clamps at the first row");
}

#[test]
fn model_selector_window_keeps_focus_visible() {
    let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
    sel.focused = 4;
    let filtered = sel.filtered();
    let (start, count) = sel.window(&filtered, 3);
    assert_eq!((start, count), (2, 3), "window slides down to reveal focus");
    assert!(sel.focused >= start && sel.focused < start + count);
}

#[test]
fn model_selector_window_pulls_up_when_focus_above() {
    let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
    sel.scroll = 4;
    sel.focused = 1;
    let filtered = sel.filtered();
    let (start, _) = sel.window(&filtered, 3);
    assert_eq!(start, 1, "window pulls up so focus is visible");
}

#[test]
fn model_selector_window_empty_list_returns_zero() {
    let sel = selector_with_models(&[]);
    assert_eq!(sel.window(&sel.filtered(), 5), (0, 0));
    assert!(sel.highlighted().is_none());
}

#[test]
fn model_selector_window_is_pure_and_idempotent() {
    // The renderer calls `window` during terminal.draw(), which must
    // never mutate scroll/focus state.  Verify repeated calls return
    // identical results and leave the fields untouched.
    let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
    sel.scroll = 3;
    sel.focused = 4;
    let before_scroll = sel.scroll;
    let before_focused = sel.focused;

    let filtered = sel.filtered();
    let first = sel.window(&filtered, 3);
    let second = sel.window(&filtered, 3);

    assert_eq!(first, second, "window must be deterministic");
    assert_eq!(sel.scroll, before_scroll, "window must not mutate scroll");
    assert_eq!(sel.focused, before_focused, "window must not mutate focus");
}

#[test]
fn model_selector_submit_returns_highlighted_and_closes() {
    let mut sel = selector_with_models(&["a", "b"]);
    sel.move_down();
    let model = sel.submit();
    assert_eq!(model.as_deref(), Some("b"));
    assert!(!sel.is_open(), "submit closes the selector");
}

#[test]
fn model_selector_submit_empty_returns_none() {
    let mut sel = selector_with_models(&[]);
    assert!(sel.submit().is_none());
    assert!(!sel.is_open());
}

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against ""
fn model_selector_filter_key_consumes_chars_and_backspace() {
    let mut sel = selector_with_models(&["gpt-4o", "claude-3"]);
    sel.filter_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    assert_eq!(sel.filter.text, "g");
    assert_eq!(sel.filtered(), vec!["gpt-4o"]);

    sel.filter_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert!(sel.filter.text.is_empty());
    assert_eq!(sel.filtered().len(), 2);
}

#[test]
fn model_selector_filter_key_ignores_enter() {
    let mut sel = selector_with_models(&["a"]);
    sel.filter_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(sel.is_open());
}

#[test]
fn model_selector_apply_error_records_and_clears_loading() {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_error("no credential".to_string());
    assert!(!sel.loading);
    assert_eq!(sel.error.get(), Some("no credential"));
}

#[test]
fn warming_retries_stop_at_the_budget() {
    let mut selector = ModelSelectorState::new();
    selector.open();
    for _ in 0..MODEL_LIST_MAX_RETRIES {
        assert!(selector.schedule_warming_retry("warming"));
        assert!(selector.take_retry());
    }
    // Budget spent: the daemon's message is shown and nothing is re-armed.
    assert!(!selector.schedule_warming_retry("warming"));
    assert_eq!(selector.error.get(), Some("warming"));
    assert!(selector.retry_timer.is_none());
}

#[test]
fn a_closed_selector_does_not_retry() {
    let mut selector = ModelSelectorState::new();
    selector.open();
    assert!(selector.schedule_warming_retry("warming"));
    selector.close();
    assert!(!selector.take_retry());
}

#[test]
fn a_landed_list_disarms_an_armed_warming_retry() {
    let mut selector = ModelSelectorState::new();
    selector.open();
    assert!(selector.schedule_warming_retry("warming"));

    // A `Models` reply that raced the timer must cancel it: otherwise the UI
    // loop fires one redundant `ListModels` after the list already landed.
    selector.apply_models(vec!["m".to_string()], None, false);
    assert!(selector.retry_timer.is_none());
    assert!(!selector.take_retry());
}

#[test]
fn a_terminal_error_disarms_an_armed_warming_retry() {
    let mut selector = ModelSelectorState::new();
    selector.open();
    assert!(selector.schedule_warming_retry("warming"));

    // A real failure replaces the warming state, so the retry must stop.
    selector.apply_error("no credential");
    assert_eq!(selector.error.get(), Some("no credential"));
    assert!(selector.retry_timer.is_none());
    assert!(!selector.take_retry());
}
