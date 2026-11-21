//! The **connection lifecycle** family: the daemon's own going-away signals and
//! the staged-flow commit failure that ends a setup flow.
//!
//! These three arms are not a *message subject* the way sessions or models are —
//! they concern the **connection and the staged setup flow** and so have no more
//! natural home. `ShuttingDown`/`Evicted` are the connection-level termination
//! signals (the writer closes the socket right after), and the
//! `Failed { kind: SetSessionChoice }` arm is the atomic commit of the staged
//! account/model/reasoning choice failing to land. They live together because
//! each is about ending something at the connection/popup level rather than
//! updating a domain model, which is why the module is documented here rather
//! than folded into one of the subject families.
use super::super::setup_flow;
use super::Consumed;
use crate::state::App;
use choreo_proto::ClientMessage;

/// The staged flow's atomic `SetSessionChoice` commit did not land.
pub(super) fn on_set_session_choice_failed(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
) -> Consumed {
    // A rejected atomic commit: the staged flow's one write did not land, so
    // the flow ends here with the popups closed and the session untouched
    // (no step before it wrote anything). No early return — the generic
    // dispatch below prints the daemon's error on the status line, which is
    // why the flow's own failure path is told to leave the line alone.
    setup_flow::fail(app, client_tx, None);
    Consumed::No
}

/// The daemon announced a graceful shutdown.
pub(super) fn on_shutting_down(app: &mut App) -> Consumed {
    // The daemon is going away (graceful shutdown). The writer closes the
    // socket right after the message, so the TUI must stop and tell the user
    // why — the message is printed once the alternate screen is restored
    // (see `run_app`). An early return keeps the generic dispatch from
    // also pushing text into the chat history we are about to leave.
    tracing::info!("daemon announced shutdown; quitting");
    app.should_quit = true;
    app.set_quit_message("the server is shutting down");
    Consumed::Yes
}

/// The daemon evicted this client for lagging.
pub(super) fn on_evicted(app: &mut App) -> Consumed {
    // The daemon has evicted this client for lagging. As with `ShuttingDown`,
    // the writer closes the socket right after the message, so the TUI must
    // stop and tell the user why, and an early return keeps the generic
    // dispatch from pushing text into the chat history we are about to leave.
    tracing::warn!("evicted by the daemon for lag; quitting");
    app.should_quit = true;
    app.set_quit_message(
        "disconnected by the daemon: evicted for falling behind the streaming lag limit",
    );
    Consumed::Yes
}
