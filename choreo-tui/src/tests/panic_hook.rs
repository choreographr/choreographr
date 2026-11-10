//! Tests for the panic hook's fatal/non-fatal classification
//! ([`crate::classify_panic`]).
//!
//! The hook itself is never installed from a test: `set_hook` is a
//! process-global side effect and would replace the harness's own hook for
//! every other test in the same process. The classification is a pure function
//! of the panicking thread's identity and name, so it is the thing under test.

use crate::classify_panic;
use std::thread::{self, ThreadId};

/// The id of a freshly spawned thread carrying `name`, so a test can build the
/// very disagreement the identity rule exists for: a thread that is named like
/// the UI loop without being it. The thread is joined immediately; no waiting
/// is involved, and joining is what hands back the id it recorded.
fn spawned_thread_id(name: Option<&str>) -> ThreadId {
    let mut builder = thread::Builder::new();
    if let Some(name) = name {
        builder = builder.name(name.to_string());
    }
    let handle = builder
        .spawn(|| thread::current().id())
        .expect("test thread should spawn");
    handle.join().expect("test thread should not panic")
}

#[test]
fn ui_thread_is_fatal() {
    let ui_thread = thread::current().id();
    let (fatal, thread) = classify_panic(ui_thread, ui_thread, thread::current().name());
    assert!(fatal, "a panic on the installing (UI loop) thread is fatal");
    // The label still names whatever the UI loop thread is called; it carries
    // no part in the fatal decision.
    assert!(!thread.is_empty(), "the log label is always populated");
}

#[test]
fn differently_named_main_thread_is_not_fatal() {
    // The regression this identity rule exists to prevent: a worker thread
    // whose name happens to be `main` is still not the UI loop, so misreading
    // it as fatal would tear down a UI that is rendering.
    let worker = spawned_thread_id(Some("main"));
    let ui_thread = thread::current().id();
    assert_ne!(worker, ui_thread, "the spawned thread has its own identity");
    let (fatal, thread) = classify_panic(worker, ui_thread, Some("main"));
    assert!(!fatal, "only the installing thread's identity is fatal");
    assert_eq!(
        thread, "main",
        "the label reports the panicking thread's name"
    );
}

#[test]
fn unnamed_thread_is_not_fatal_and_labelled() {
    let worker = spawned_thread_id(None);
    let ui_thread = thread::current().id();
    let (fatal, thread) = classify_panic(worker, ui_thread, None);
    assert!(!fatal, "a non-UI thread's panic is contained, not fatal");
    assert_eq!(
        thread, "<unnamed>",
        "a nameless thread gets the placeholder"
    );
}

#[test]
fn named_worker_thread_is_not_fatal_and_labelled() {
    let worker = spawned_thread_id(Some("image-encoder"));
    let ui_thread = thread::current().id();
    let (fatal, thread) = classify_panic(worker, ui_thread, Some("image-encoder"));
    assert!(!fatal, "a worker panic is contained, not fatal");
    assert_eq!(thread, "image-encoder");
}
