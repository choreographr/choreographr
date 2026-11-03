//! The one-shot `--attach <SESSION_ID>` startup latch.
//!
//! `App::dispatch_pending_attach` is fired from the daemon handler right after
//! the initial `Sessions` snapshot is applied. These tests pin the two
//! properties that make it safe there: it sends the requested attach exactly
//! once (a later snapshot/refresh is inert), and it sends nothing when no flag
//! was given.

use crate::tests::support::test_app;
use choreo_proto::{ClientMessage, ClientMessageType};

/// The first drain sends `AttachSession` for the latched id; every later drain
/// (a subsequent `Sessions` reply) sends nothing, because the latch was taken.
#[test]
fn dispatch_pending_attach_is_one_shot() {
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let mut app = test_app();
    app.pending_attach = Some(7);

    assert!(app.dispatch_pending_attach(&tx), "first drain dispatches");
    let sent = rx.try_recv().expect("AttachSession is sent");
    assert_eq!(
        sent.inner,
        ClientMessageType::AttachSession { session_id: 7 },
        "the latch is dispatched as an AttachSession for the named id"
    );

    assert!(
        !app.dispatch_pending_attach(&tx),
        "a second drain dispatches nothing"
    );
    assert!(
        rx.try_recv().is_err(),
        "the latch is taken exactly once; no second message is sent"
    );
}

/// With no `--attach` flag the latch is `None`, so no attach is dispatched even
/// though the snapshot is applied and the drain runs.
#[test]
fn dispatch_without_a_latched_id_is_a_noop() {
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let mut app = test_app();
    assert!(app.pending_attach.is_none(), "no flag means no latch");

    assert!(!app.dispatch_pending_attach(&tx));
    assert!(rx.try_recv().is_err(), "nothing is sent without a latch");
}
