//! The session-thread module: the session control thread's entry point, the
//! process-shutdown join helpers it relies on, and the module wiring that keeps
//! every moved item reachable at `crate::sessions::X`.
//!
//! The concern modules hold the pieces a session thread operates on:
//! [`command`] the `SessionCommand` channel message and the request-pipeline
//! context, [`types`] the persistent config / worker snapshot / metadata
//! types with their persistence conversions, and [`state`] the live
//! `SessionState` with its per-request and per-subscriber substructures.
//! `handlers` dispatches each `SessionCommand`, `tokens` does token
//! accounting, `worker` drives one request's agent loop and owns the
//! exit-persistence helper, and `tests` holds the unit tests. This parent keeps
//! only what cuts across those concerns: the constants, the shutdown join
//! helpers, the shared broadcast / persistence free helpers, and
//! [`session_main`].

use crate::broadcast::{ClientId, LagLimits, ReplyTarget, SubscriberSink, fan_out_evicting};
use crate::cache_warm::WarmPolicy;
use crate::context::{LoadedSkill, SkillMeta};
use crate::daemon::{DaemonCommand, ResolvedAccount};
use crate::db::{self, SessionRecord, write_session_retry, write_turn_retry};
use crate::mcp::{ProjectToolSet, SessionMcpOverlay};
use crate::providers::InferenceProvider;
use crate::requests::run_agent_loop;
use crate::tools::{ToolOutput, ToolRegistry};
use choreo_ai_protocols::model_reasoning_capability;
use choreo_keystore::ServiceCredential;
use choreo_proto::{
    AssistantToolCallRecord, ContextConfig, DaemonMessage, DaemonMessageType, DisplayedImageRecord,
    ImageReference, ReasoningArtifact, ReasoningProducer, SessionEvent, SessionStatus,
    SessionSummary, TimestampMs, TokenUsage, ToolResultRecord, Turn,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, mpsc};
use tracing::{debug, error, info, trace, warn};
use unicode_segmentation::UnicodeSegmentation;

/// Sentinel `stream_id` meaning "cancel whatever is currently active, regardless of its ID".
/// Used in child-session cancellation where we don't know the child's active request ID.
pub(crate) const CANCEL_ALL: u64 = 0;

/// Maximum length of a session title in grapheme clusters (user-perceived
/// characters), not bytes or Unicode scalar values.  Titles are user-facing
/// display strings shown in session listings and the TUI sidebar, so
/// multi-byte scripts and composed emoji (e.g. "👨‍👩‍👧‍👦" = 1 grapheme, 7
/// `char` values) are treated fairly.  Defined here as the single source
/// of truth; the tool-level validator in `set_session_title.rs` imports
/// this constant to avoid duplication.
pub(crate) const MAX_TITLE_CHARS: usize = 200;

/// Grace period allowed for a session thread to persist and exit after
/// `Shutdown` is signalled.  If a request worker is stuck in a provider
/// read that a cancel cannot interrupt, the session thread never receives
/// `RequestFinished` and would otherwise hang the daemon's shutdown join.
/// Abandoning the join is safe: per-turn state is persisted as turns
/// finalize, and process exit (or the delete path's finalize on
/// `SessionExited`) reaps the thread.
pub(crate) const SESSION_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Join a session thread after `Shutdown`, bounded by
/// [`SESSION_SHUTDOWN_GRACE`].  Returns `true` if the thread exited within
/// the grace period, `false` if it was abandoned (the caller then relies on
/// process exit — or the delete path's finalize-on-`SessionExited` — to reap
/// the thread).  Used by the daemon's lifecycle shutdown.
pub(crate) fn join_session_shutdown(handle: std::thread::JoinHandle<()>, session_id: u64) -> bool {
    poll_join_with_grace(
        handle,
        session_id,
        SESSION_SHUTDOWN_GRACE,
        std::time::Instant::now,
        std::thread::sleep,
    )
}

/// Poll a `JoinHandle`'s exit until it finishes or `grace` elapses, then reap
/// it via `join()`.  Returns whether the thread exited within the grace period.
///
/// The clock (`now`) and sleep are injected so unit tests can exercise both
/// outcomes deterministically — no real time-based waits.
fn poll_join_with_grace<F, S>(
    handle: std::thread::JoinHandle<()>,
    session_id: u64,
    grace: std::time::Duration,
    now: F,
    sleep: S,
) -> bool
where
    F: FnMut() -> std::time::Instant,
    S: FnMut(std::time::Duration),
{
    // The handle must be reachable from both the finish-check and the reap
    // closures, so it lives in a RefCell that is created and consumed on this
    // thread only (never shared across threads).
    let handle = std::cell::RefCell::new(Some(handle));
    shutdown_join_poll(
        session_id,
        grace,
        || {
            handle
                .borrow()
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
        },
        || {
            if let Some(h) = handle.borrow_mut().take() {
                let _ = h.join();
            }
        },
        now,
        sleep,
    )
}

/// Poll a thread-exit check until it passes or `grace` elapses.
///
/// `finished` reports whether the target has exited; `reap` is invoked once
/// when it has.  The clock and sleep are injected so unit tests can exercise
/// both outcomes deterministically — no real time-based waits.
fn shutdown_join_poll(
    session_id: u64,
    grace: std::time::Duration,
    mut finished: impl FnMut() -> bool,
    mut reap: impl FnMut(),
    mut now: impl FnMut() -> std::time::Instant,
    mut sleep: impl FnMut(std::time::Duration),
) -> bool {
    let deadline = now() + grace;
    loop {
        if finished() {
            // The thread exited; reap it now so resources are released.
            reap();
            return true;
        }
        // Poll every 50 ms, but never overshoot the deadline.
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() {
            tracing::warn!(
                session_id,
                grace_ms = grace.as_millis(),
                "session thread did not exit within shutdown grace period; abandoning join \
                 (process exit will reap the thread; completed turns are already persisted)",
            );
            return false;
        }
        sleep(remaining.min(std::time::Duration::from_millis(50)));
    }
}

/// Test-only seam: bounded join with a caller-supplied grace period so
/// integration tests can exercise both outcomes in a few hundred ms instead
/// of waiting out the production 5s grace.  Uses the real clock and sleep.
///
/// Only compiled under the `test-utils` feature, which the crate's own
/// dev-dependency enables for test builds, so it never leaks into the
/// published public API.
#[cfg(feature = "test-utils")]
#[doc(hidden)]
pub fn join_session_shutdown_with_grace_for_test(
    handle: std::thread::JoinHandle<()>,
    session_id: u64,
    grace: std::time::Duration,
) -> bool {
    poll_join_with_grace(
        handle,
        session_id,
        grace,
        std::time::Instant::now,
        std::thread::sleep,
    )
}

fn broadcast(
    subscribers: &mut HashMap<ClientId, SubscriberSink>,
    ctx: &RequestContext,
    message: &DaemonMessageType,
) {
    // Wrap the payload as a broadcast (`id: None`): session events fan out to
    // EVERY subscriber (and the all-activity bus), not to a single requester.
    let framed = DaemonMessage::broadcast(message.clone());
    // Forward to daemon-level activity subscribers so clients subscribed
    // to all session activity (e.g. the TUI after SubscribeAllActivity)
    // receive every session-scoped event without having to attach to every
    // session individually. The session thread KNOWS its own id, so the
    // origin is carried explicitly on the command for the daemon's
    // duplicate-suppression (it no longer re-derives the origin from the
    // message shape).
    let _ = ctx.daemon_tx.send(DaemonCommand::BroadcastActivity {
        session_id: Some(ctx.session_id),
        msg: framed.clone(),
    });

    // Lossless + lag-eviction via the ONE shared policy — the same
    // [`crate::broadcast::fan_out_evicting`] the daemon's summary/activity
    // broadcasts use, so the three fan-outs cannot drift. Every message is
    // enqueued into each subscriber's UNBOUNDED queue (never dropped, never
    // stalling this session thread), and a subscriber whose queue crossed
    // the lag limits is evicted. Eviction is signalled to the daemon (which
    // owns the connection) rather than done here: this thread holds only the
    // sink, so it sends `EvictClient`/`EvictLargestLagging` commands and the
    // daemon tears the connection down.
    let (evict_clients, evict_largest) = fan_out_evicting(
        subscribers,
        &framed,
        &ctx.lag_limits,
        &ctx.global_lag,
        |_id, _| false, // session subscribers are never duplicate-suppressed
    );
    for client_id in evict_clients {
        let _ = ctx.daemon_tx.send(DaemonCommand::EvictClient { client_id });
    }
    if evict_largest {
        let _ = ctx.daemon_tx.send(DaemonCommand::EvictLargestLagging);
    }
}

fn fail_request(
    subscribers: &mut HashMap<ClientId, SubscriberSink>,
    ctx: &RequestContext,
    session_id: u64,
    stream_id: u64,
    reply: Option<ReplyTarget>,
    error: impl Into<String>,
) -> bool {
    let error = error.into();
    // Targeted terminal failure to the requester: the request's one `id: Some`
    // reply. Sent BEFORE the broadcast below — the two ride the same per-client
    // writer queue, so the requester sees its rejection, then the broadcast
    // `Failed`.
    if let Some(target) = reply {
        target.fail(error.clone());
    }
    // Only the `Failed` broadcast: a REJECTED run never started, so there is no
    // stream to open. Broadcasting a `Started` here (the old `turn_id: 0`
    // shape) would register a phantom live stream on every subscriber until the
    // follow-up `Failed` cleared it; the `Failed` alone is the whole event.
    broadcast(
        subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::Failed { stream_id, error },
        },
    );
    false
}

/// Notify the daemon of updated metadata and persist the session record
/// to the database.  Shared boilerplate used by session mutation handlers
/// (`SetTitle`, `SetAccount`, `SetModel`, etc.) so that changes are reflected
/// in session listings immediately and survive daemon restarts.
fn persist_session_metadata(state: &mut SessionState, ctx: &RequestContext, label: &str) {
    // Any metadata mutation is a modification: bump the timestamp so the
    // sessions list reorders (newest first) the moment the daemon index and
    // the persisted record are updated.
    let now = TimestampMs::now().as_millis();
    state.config.last_modified = state.config.last_modified.max(now);
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    let record = SessionRecord::from(&*state);
    if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
        warn!(error = %e, "failed to persist session record after {label}");
    }
}

/// The default active tool-group set for a session that persisted none.
///
/// Mirrors the always-on groups (`core`, `git`, `shell`); the Coordination
/// Platform group is included only when the `content` feature is compiled in —
/// the group (and its tools) simply don't exist in a plain build, and
/// `load_tools`/`unload_tools` validation would reject it as unknown.
///
/// Note on stale persisted names: an existing session whose stored
/// `active_tool_groups` still contains the pre-rename `"coord"` (or any group
/// whose feature is off) keeps that name in its set — stale names are
/// *silently ignored*: a group with no registered tools contributes nothing to
/// `available_definitions`, so nothing is exposed to the model. Only explicit
/// `load_tools`/`unload_tools` requests validate names against the live
/// registry, so stale names can never be re-activated.
fn default_active_tool_groups() -> HashSet<String> {
    // `mut` is only needed when the `content` feature inserts its group.
    #[cfg_attr(not(feature = "content"), expect(unused_mut))]
    let mut groups = HashSet::from(["core".to_string(), "git".to_string(), "shell".to_string()]);
    #[cfg(feature = "content")]
    groups.insert("content".to_string());
    // The iOS group is PROTECTED (register_platform_tools) and unioned into
    // the active set at definition time regardless — listing it here is
    // belt-and-suspenders for display honesty (same rationale as the
    // CreateSession default list in daemon.rs).
    #[cfg(target_os = "ios")]
    groups.insert("ios".to_string());
    groups
}

/// The session control thread's entry point.
///
/// Owns the authoritative [`SessionState`] for one session and processes
/// [`SessionCommand`]s in a receive loop until a `Shutdown` arrives or the
/// command channel closes. On startup it rebuilds turn history from the
/// database, reconstructs accumulated token usage, and seeds the daemon's
/// metadata index; on exit it persists the final record. Mutating commands are
/// dispatched to the `handlers` submodule; request work runs on separate worker
/// threads that report back via `RequestFinished`.
///
/// `initial_provider` is `None` in production (the client is resolved lazily on
/// this thread at the first request); tests may seed one directly to skip the
/// daemon resolution round-trip. `init_record` and `ctx` are borrowed for the
/// thread's lifetime.
pub fn session_main(
    rx: &crossbeam_channel::Receiver<SessionCommand>,
    initial_provider: Option<InferenceProvider>,
    registry: choreo_ai_protocols::SocketRegistry,
    account_name: Option<String>,
    // The account's provider slug (catalog key), resolved by the daemon
    // command loop from `AccountManager` WITHOUT touching credentials — a
    // non-secret fact supplied at spawn so session-thread catalog lookups
    // (context window, reasoning capability) work before the keystore
    // unlocks.
    provider_slug: Option<String>,
    // `init_record` is only read (title/model/effort/etc. are cloned out of
    // it); the caller in daemon.rs still owns the record it built.
    init_record: Option<&SessionRecord>,
    // Borrowed only: the context is read throughout the loop (the session
    // thread never outlives the caller's closure, which owns it).
    ctx: &RequestContext,
) {
    let config = SessionConfig {
        title: init_record.as_ref().and_then(|r| r.title.clone()),
        selected_model: init_record.as_ref().and_then(|r| r.selected_model.clone()),
        reasoning_effort: init_record
            .as_ref()
            .and_then(|r| r.reasoning_effort.clone()),
        parent_session_id: init_record.as_ref().and_then(|r| r.parent_session_id),
        working_dir: init_record
            .as_ref()
            .and_then(|r| r.working_dir.as_ref().map(PathBuf::from)),
        created_at: init_record
            .as_ref()
            .map_or_else(|| TimestampMs::now().as_millis(), |r| r.created_at),
        last_modified: init_record
            .as_ref()
            .map_or_else(|| TimestampMs::now().as_millis(), |r| r.last_modified),
        status: SessionStatus::Inactive,
        active_tool_groups: init_record
            .as_ref()
            .map(|r| r.active_tool_groups.iter().cloned().collect())
            .filter(|cats: &HashSet<String>| !cats.is_empty())
            .unwrap_or_else(default_active_tool_groups),
        context_config: init_record
            .as_ref()
            .map_or_default(|r| r.context_config.clone()),
        account_name,
        accumulated_usage: TokenUsage::default(),
        context_window: None,
        last_prompt_tokens: None,
        last_response_id: init_record
            .as_ref()
            .and_then(|r| r.last_response_id.clone()),
        last_response_id_producer: init_record
            .as_ref()
            .and_then(|r| r.last_response_id_producer.clone()),
    };
    let mut state = SessionState {
        config,
        // `initial_provider` is `None` in production (the daemon always
        // resolves lazily on the session thread); tests may seed a provider
        // directly to avoid driving the daemon resolution round-trip.
        provider: initial_provider,
        provider_slug,
        registry,
        warm_policy: ctx.warm_policy,
        ..SessionState::empty()
    };

    // Re-resolve context window from the catalog when loading an existing
    // session whose stored context_window is None (e.g. sessions created
    // before a model was added to the catalog).
    state.resolve_context_window_if_missing(ctx);

    match db::read_turns(&ctx.db, ctx.session_id) {
        Ok(turns) => {
            for (turn_id, turn) in turns {
                state.turns.insert(turn_id, turn);
                state.next_turn_id = state.next_turn_id.max(turn_id + 1);
            }
            // Reconstruct the lifetime billing total from per-turn usage, and
            // derive the context-size readout from the live turns, so both the
            // running total and the context-window display (e.g.
            // "45k / 128k (35%)") survive daemon restarts without storing
            // them redundantly in the session record. The two differ: billing
            // sums every turn (undo does not refund tokens), while the context
            // size reflects only non-undone turns.
            let accumulated_usage = reconstruct_accumulated_usage(&state.turns);
            state.config.accumulated_usage = accumulated_usage;
            state.config.last_prompt_tokens = state.live_last_prompt_tokens();
            trace!(
                last_prompt_tokens = state.config.last_prompt_tokens,
                ?accumulated_usage,
                "reconstructed token state from turns after daemon restart"
            );
        }
        Err(e) => warn!(ctx.session_id, error = %e, "failed to load turns from DB"),
    }

    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&state),
    });

    info!("session {} started", ctx.session_id);

    let mut shutdown_requested = false;
    while let Ok(cmd) = rx.recv() {
        if process_command(cmd, &mut state, &mut shutdown_requested, ctx) {
            break;
        }
    }

    info!("session {} exiting", ctx.session_id);
    persist_and_exit(&state, &ctx.db, ctx.session_id, &ctx.daemon_tx);
}

mod command;
mod handlers;
mod state;
mod tokens;
mod types;
mod worker;

use handlers::process_command;
use tokens::reconstruct_accumulated_usage;
use worker::persist_and_exit;

// Keep every moved item reachable at `crate::sessions::X`: the child modules
// reach the shared types and helpers through `super::…`, and external callers
// (daemon.rs, requests.rs, lib.rs) name them through the same path, so the
// re-exports preserve the pre-split module surface exactly.
pub use command::{ChildResult, RequestContext, SessionCommand};
pub(crate) use state::{ActiveRequest, turn_for_client};
pub use state::{ActiveSessionEntry, AssistantResponse, SessionState};
pub use types::{SessionConfig, SessionMetadata, SessionSnapshot};

#[cfg(test)]
mod tests;
