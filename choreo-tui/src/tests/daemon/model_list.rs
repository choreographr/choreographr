use crate::connection::handle_daemon_message;
use crate::tests::support::test_app;
use choreo_proto::{ClientMessageType, DaemonMessage, DaemonMessageType};

// ── Model list warming retry ──

#[test]
fn warming_model_list_schedules_a_retry_instead_of_an_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.model_selector.open();
    let id = app.pending.send(&tx, ClientMessageType::ListModels);

    handle_daemon_message(
        DaemonMessage::reply(
            id,
            DaemonMessageType::ModelsFailed {
                error: choreo_proto::models_warming_error("anthropic"),
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelsFailed");

    assert!(
        app.model_selector.loading,
        "the popup keeps showing loading"
    );
    assert_eq!(app.model_selector.error.get(), None);
    assert!(app.model_selector.retry_timer.is_some());
}

#[test]
fn a_real_model_list_failure_is_shown_without_retrying() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.model_selector.open();
    let id = app.pending.send(&tx, ClientMessageType::ListModels);

    handle_daemon_message(
        DaemonMessage::reply(
            id,
            DaemonMessageType::ModelsFailed {
                error: "no accounts configured".to_string(),
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelsFailed");

    assert_eq!(
        app.model_selector.error.get(),
        Some("no accounts configured")
    );
    assert!(app.model_selector.retry_timer.is_none());
}
