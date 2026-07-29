use choreographr::{NotifySendArgs, execute_notify_send};
use std::path::Path;

#[test]
#[ignore]
fn notify_send_success() {
    let result = execute_notify_send(
        &NotifySendArgs {
            summary: "integration test".into(),
            body: Some("hello from choreo".into()),
            urgency: None,
            timeout: None,
            icon: None,
        },
        Some(Path::new("/tmp")),
    );
    // This will only succeed when a DBus session bus is available.
    assert!(result.is_ok(), "expected success: {:?}", result);
    let msg = result.unwrap();
    assert!(msg.contains("Notification sent"), "{}", msg);
}

#[test]
#[ignore]
fn notify_send_empty_summary_rejected() {
    let result = execute_notify_send(
        &NotifySendArgs {
            summary: "".into(),
            body: None,
            urgency: None,
            timeout: None,
            icon: None,
        },
        Some(Path::new("/tmp")),
    );
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("non-empty"), "{}", err);
}
