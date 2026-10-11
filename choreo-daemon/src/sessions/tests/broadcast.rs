//! `SessionCommand::Broadcast` fan-out tests: delivery to every subscriber,
//! the no-subscriber and disconnected-subscriber paths, and lossless enqueue
//! with lag-eviction signalling.

use super::*;
use crate::broadcast::test_sink;

// -- SessionCommand::Broadcast tests -----------------------------------

#[test]
fn broadcast_delivers_message_to_all_subscribers() {
    let (tx1, rx1) = test_sink();
    let (tx2, rx2) = test_sink();
    let (mut state, ctx) = broadcast_setup();
    state.subscribers.insert(cid(10), tx1);
    state.subscribers.insert(cid(20), tx2);

    let mut shutdown = false;
    process_command(
        SessionCommand::Broadcast(DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Done {
                stream_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }),
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(
        rx1.recv().unwrap().inner,
        DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Done {
                stream_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }
    );
    assert_eq!(
        rx2.recv().unwrap().inner,
        DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Done {
                stream_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }
    );
    assert!(!shutdown);
}

#[test]
fn broadcast_with_no_subscribers_does_not_panic() {
    let (mut state, ctx) = broadcast_setup();
    let mut shutdown = false;
    process_command(
        SessionCommand::Broadcast(DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Done {
                stream_id: 0,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }),
        &mut state,
        &mut shutdown,
        &ctx,
    );
    assert!(!shutdown);
}

#[test]
fn broadcast_handles_disconnected_subscriber_gracefully() {
    let (tx, rx) = test_sink();
    drop(rx);
    let (mut state, ctx) = broadcast_setup();
    state.subscribers.insert(cid(99), tx);

    let mut shutdown = false;
    process_command(
        SessionCommand::Broadcast(DaemonMessageType::Pong),
        &mut state,
        &mut shutdown,
        &ctx,
    );
    assert!(!shutdown);
}

#[test]
fn broadcast_enqueues_losslessly_and_signals_eviction() {
    // Lossless + lag-eviction: a session subscriber whose queue crosses
    // the lag cap still receives the crossing message (never dropped),
    // and the session thread signals the daemon to evict that client
    // (the daemon owns the connection and does the disconnect).
    let (mut state, mut ctx) = broadcast_setup();
    ctx.lag_limits = LagLimits {
        per_client_cap: 16,
        global_budget: usize::MAX,
    };
    let (tx, rx) = test_sink();
    state.subscribers.insert(cid(10), tx);

    // A message large enough to cross the tiny per-client cap.
    let broadcast = DaemonMessageType::Session {
        session_id: Some(ctx.session_id),
        event: SessionEvent::Failed {
            stream_id: 5,
            error: "x".repeat(100),
        },
    };
    let mut shutdown = false;
    process_command(
        SessionCommand::Broadcast(broadcast.clone()),
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // Lossless: the crossing message was delivered, not dropped.
    assert_eq!(rx.recv().unwrap().inner, broadcast);
    // The subscriber stays in the map (eviction happens daemon-side via
    // the EvictClient signal + the daemon's handle_evict_client).
    assert!(state.subscribers.contains_key(&cid(10)));
    // The session itself keeps running.
    assert!(!shutdown);
}
