//! Runtime catalog maintenance (S4): cache persistence, the background
//! models.dev refresh thread, and the user-overlay reload.
//!
//! **Threading.** One background thread owns the whole runtime pipeline. It
//! does the conditional GET against models.dev and reacts to
//! `models-overlay.toml` edits surfaced by the unified config-watching
//! transport ([`crate::config_watch`]) — but it NEVER mutates the catalog
//! itself. Every change is sent to the daemon command loop as a
//! [`DaemonCommand`], which is the single writer of the
//! [`choreo_ai_protocols::PROVIDER_CATALOG`] `ArcSwap` (the documented
//! thread-communication exception). All cross-thread communication here is
//! channel-based: the daemon hands `/refresh-models` requests to this thread
//! over a channel (never the command loop doing HTTP), and the config
//! transport forwards overlay events over another channel.
//!
//! **No retained catalog.** The thread deliberately keeps NO copy of the
//! catalog base between events — only the small scalars it needs to pace
//! fetches and detect overlay edits (see `MaintenanceState`). A fetched base
//! travels through [`DaemonCommand::CatalogBaseChanged`] as a one-shot value
//! and is not stored on the thread; an overlay edit travels as
//! [`DaemonCommand::CatalogOverlayChanged`], which carries no base at all. The
//! process's only resident catalog is therefore the merged `ArcSwap` the
//! command loop owns.
//!
//! **The base's single reader is its single writer.** `catalog.bin` is the
//! base's durable home between events, and the command loop is the only code
//! that writes it — so the command loop is also the only code that can
//! re-read it without a race, which is why the overlay re-merge runs there
//! (see `DaemonState::handle_catalog_overlay_changed`) instead of here. The
//! maintenance thread keeps the overlay *policy* (the change fingerprint) and
//! stays free of base state.
//!
//! **Refresh pacing (S4).** A models.dev fetch is attempted at most once per
//! `REFRESH_ATTEMPT_INTERVAL` (25 h), regardless of whether the last attempt
//! succeeded, 304'd, or failed. The cooldown is anchored on a **wall-clock
//! attempt timestamp persisted in the DB** ([`crate::db`] `catalog_state`),
//! written BEFORE the fetch starts — so the cadence survives restarts (a
//! daemon restarted every few hours fetches once per ~day of wall time, not
//! once per start) and a crash mid-fetch cannot re-trigger an immediate
//! re-fetch. At startup the thread fetches immediately iff there is no valid
//! cache, no recorded attempt, or the attempt is stale; otherwise it arms the
//! in-run timer for the remaining time. The thread sleeps on its channel with
//! a timeout, which doubles as the revalidation cadence — the next conditional
//! GET fires when the timeout elapses — so the cache stays fresh with no busy
//! loops. Within a single run the countdown is monotonic (suspend pauses it:
//! a laptop that sleeps overnight fetches after 25 h of *awake* time);
//! restart behavior is strict wall time via the DB anchor. `/refresh-models`
//! bypasses the cooldown (explicit user intent) but still records the
//! attempt. The policy itself lives in `pacing`, as pure functions of an
//! injected clock.
//!
//! # Layout
//!
//! * `cache` — the postcard cache file, its paths, and the base re-read.
//! * `pacing` — the cooldown arithmetic and the DB attempt anchor.
//! * `refresh` — the fetch state machine and the burst coalescing.
//! * `overlay` — reading the user overlay, its fingerprint, its reload.
//! * this module — the state the loop carries and the loop itself.

mod cache;
#[cfg(test)]
mod fixtures;
mod overlay;
mod pacing;
mod refresh;

use std::sync::{Arc, mpsc};
use std::time::Instant;

use choreo_ai_protocols::load_bundled_base;
use crossbeam_channel::{Receiver, Sender, after, select};
use tracing::{info, warn};

use crate::config_watch::ConfigChange;
use crate::daemon::DaemonCommand;
use crate::db::{get_catalog_etag, get_catalog_last_attempt_ms};

use cache::ensure_runtime_dirs;
use overlay::{read_user_overlay, reload_user_overlay};
use pacing::{
    REFRESH_ATTEMPT_INTERVAL, next_retry_deadline, record_attempt, should_fetch_at_startup,
    wall_now_ms,
};
use refresh::{fold_refresh_nows, run_refresh};

pub use cache::{CatalogPaths, USER_OVERLAY_NAME};
pub use refresh::{RefreshReport, RefreshRequester};
// The base readers/writer are crate-internal: the cache file is plumbing
// between the maintenance loop and the command loop, not part of the crate's
// public surface.
pub(crate) use cache::{load_cached_base, load_persisted_base, write_catalog_cache};

/// Messages on the maintenance thread's single channel: requests from the
/// daemon command loop. The thread waits on this channel AND the config
/// transport's overlay-event channel (via `select!`), whose recv timeout
/// drives the retry timer.
#[derive(Debug)]
pub enum MaintenanceEvent {
    /// A `/refresh-models` request. The HTTP fetch happens HERE (never in the
    /// command loop — it can block for the whole 30s timeout); the result is
    /// then handed back through the daemon loop, which owns the catalog swap.
    RefreshNow {
        /// Whether the user forced the fetch (bypasses the refresh cooldown).
        force: bool,
        /// One-shot reply channel carrying the fetch result back to the requester.
        reply: mpsc::Sender<Result<RefreshReport, String>>,
    },
}

/// Mutable state of the maintenance thread. It deliberately holds **no
/// catalog data**: only the small scalars needed to pace fetches and detect
/// overlay edits — the etag for the next conditional GET, the wall-clock
/// last-attempt anchor (loaded from the DB at startup, kept in sync by
/// [`record_attempt`]), the fingerprint of the last user overlay it handed to
/// the daemon loop, and the next-retry deadline.
///
/// The base has no in-memory home at all: `catalog.bin` holds it between
/// events, and the command loop — its only writer — re-reads it when an
/// overlay edit needs a re-merge. A fetch's newly fetched base is likewise
/// moved straight into the command and never stored here.
struct MaintenanceState {
    etag: Option<String>,
    /// Unix epoch millis of the last fetch attempt (DB `catalog_state`).
    /// `None` = never attempted (first run / upgrade) → fetch immediately.
    last_attempt_ms: Option<u64>,
    last_applied_user_overlay: Option<String>,
    next_retry_at: Option<Instant>,
}

/// Spawn the ONE background catalog-maintenance thread. Returns the channel
/// sender the daemon command loop uses to hand `/refresh-models` requests to
/// it. `overlay_rx` is the config transport's subscription for
/// `models-overlay.toml` edits — the thread reacts to them in its event loop
/// and re-reads the overlay. The DB is handed in because the thread is the
/// single writer of the catalog refresh state (`catalog_state`: last-attempt
/// timestamp — it observes every fetch outcome, unlike the command loop,
/// which only sees accepted swaps). The thread is detached (the process exits
/// after `run_server` returns; a lingering maintenance thread cannot outlive
/// main, and its sends to the daemon channel fail harmlessly once the command
/// loop is gone).
pub(crate) fn spawn_catalog_maintenance(
    daemon_tx: Sender<DaemonCommand>,
    db: Arc<redb::Database>,
    paths: CatalogPaths,
    overlay_rx: Receiver<ConfigChange>,
) -> Sender<MaintenanceEvent> {
    let (tx, rx) = crossbeam_channel::unbounded::<MaintenanceEvent>();
    let _ = std::thread::Builder::new()
        .name("catalog-maintenance".into())
        .spawn(move || maintenance_loop(&daemon_tx, &db, &paths, &rx, &overlay_rx));
    tx
}

fn maintenance_loop(
    daemon_tx: &Sender<DaemonCommand>,
    db: &Arc<redb::Database>,
    paths: &CatalogPaths,
    rx: &Receiver<MaintenanceEvent>,
    overlay_rx: &Receiver<ConfigChange>,
) {
    // ── 0. Ensure the catalog data dir exists so the cache can be written.
    // (The config dir is created by the shared config transport, not here.)
    ensure_runtime_dirs(paths);

    // ── 1. Load the base: valid cache file first, embedded catalog.bin as
    // the fallback (the S4 load order). The etag is only read from the DB
    // when the cache loaded: a missing/corrupt cache must produce `etag =
    // None` so the next fetch is a plain GET that rebuilds both — a 304
    // with no cache would otherwise leave the daemon on the embedded blob
    // forever (the etag-requires-cache invariant, pinned by tests).
    let (base, etag, cache_valid) = if let Some(base) = load_cached_base(&paths.bin) {
        let etag = match get_catalog_etag(db) {
            Ok(etag) => etag,
            Err(e) => {
                warn!(error = %e, "failed to read the catalog etag from the DB; \
                      the next refresh will be a plain GET");
                None
            }
        };
        info!(
            providers = base.len(),
            "loaded catalog cache from disk ({} bytes)",
            std::fs::metadata(&paths.bin).map_or(0, |m| m.len()),
        );
        (base, etag, true)
    } else {
        let base = load_bundled_base();
        info!(
            providers = base.len(),
            "no valid catalog cache; using the embedded catalog.bin",
        );
        (base, None, false)
    };

    // ── 1b. Load the persisted last-attempt anchor. `None` (never attempted
    // — first run, or an upgrade from a build without the key) means stale:
    // the startup gate below fetches immediately.
    let last_attempt_ms = match get_catalog_last_attempt_ms(db) {
        Ok(last_attempt) => last_attempt,
        Err(e) => {
            warn!(
                error = %e,
                "failed to read the catalog last-attempt timestamp; treating it as stale",
            );
            None
        }
    };

    // ── 2. Read the user overlay (if present).
    let user_overlay = match read_user_overlay(&paths.overlay) {
        Ok(Some(contents)) => Some(contents),
        Ok(None) => None,
        Err(e) => {
            warn!(
                path = %paths.overlay.display(),
                error = %e,
                "failed to read user overlay; starting without it",
            );
            None
        }
    };

    let mut state = MaintenanceState {
        etag,
        last_attempt_ms,
        last_applied_user_overlay: user_overlay.clone(),
        next_retry_at: None,
    };

    // ── 3. Apply the initial catalog through the daemon command loop (the
    // single writer of the ArcSwap). The base is moved into the command and
    // dropped here: the thread keeps no copy of it — `catalog.bin` (or the
    // embedded blob on a cache miss) is the durable source the command loop
    // re-reads when an overlay edit needs to re-merge. No persist: a
    // cache-sourced base is already on disk; a cache-miss will be persisted
    // on the first fetch.
    let _ = daemon_tx.send(DaemonCommand::CatalogBaseChanged {
        base,
        etag: state.etag.clone(),
        user_overlay,
        persist: false,
        reply: Vec::new(),
    });

    // ── 4. Initial conditional GET — gated on cache freshness. Fetch
    // immediately iff there is no valid cache, no recorded attempt (first
    // run / upgrade), or the last attempt is stale (≥ REFRESH_ATTEMPT_INTERVAL
    // wall-clock ago). Otherwise the cache is fresh enough: skip the startup
    // fetch entirely and arm the in-run timer for the remaining time, derived
    // from the persisted attempt timestamp — so a daemon restarted within the
    // cooldown window does NOT hit the network at every start, and the 25 h
    // drift of the fetch time across the daily cycle survives restarts.
    if should_fetch_at_startup(cache_valid, state.last_attempt_ms, wall_now_ms()) {
        record_attempt(db, &mut state);
        run_refresh(daemon_tx, &mut state, false, Vec::new());
    } else if let Some(deadline) =
        next_retry_deadline(state.last_attempt_ms, Instant::now(), wall_now_ms())
    {
        state.next_retry_at = Some(deadline);
        info!(
            ?deadline,
            "catalog cache is fresh; skipping the startup fetch and arming the \
             revalidation timer for the remaining time",
        );
    }

    // ── 5. Event loop: wait on the maintenance channel, the config
    // transport's overlay channel, and the retry timer — multiplexed with
    // `select!`. The timer (`after(timeout)`) fires when a revalidation is
    // due; the two channels wake the loop on requests and overlay edits.
    loop {
        let timeout = state.next_retry_at.map_or(REFRESH_ATTEMPT_INTERVAL, |at| {
            at.saturating_duration_since(Instant::now())
        });
        select! {
            recv(rx) -> msg => match msg {
                Ok(MaintenanceEvent::RefreshNow { force, reply }) => {
                    // /refresh-models is the documented fallback for overlay
                    // reloads (e.g. when the config transport could not start).
                    // Re-read the file so the command re-syncs the user layer
                    // too, not just the models.dev base — and so an overlay
                    // edit is applied even when the conditional GET below
                    // returns 304 (a 304 sends no CatalogBaseChanged, so
                    // without this the reload is lost).
                    reload_user_overlay(daemon_tx, &mut state, &paths.overlay);
                    // Coalesce: drain any RefreshNows queued while idle so a
                    // burst of /refresh-models becomes ONE fetch. Fold the
                    // force flag (a --force anywhere in the burst forces) and
                    // keep every reply sender. The whole burst is ONE attempt.
                    let (any_force, replies) = fold_refresh_nows(rx, force, reply);
                    // Explicit user intent bypasses the cooldown, but the
                    // attempt is still recorded (and the timer re-armed by
                    // run_refresh) so the DB anchor reflects reality.
                    record_attempt(db, &mut state);
                    run_refresh(daemon_tx, &mut state, any_force, replies);
                }
                Err(_) => {
                    info!("catalog maintenance channel closed; exiting");
                    break;
                }
            },
            recv(overlay_rx) -> evt => match evt {
                Ok(_change) => {
                    // The transport already filtered to this basename and a
                    // create/modify/remove kind; re-read + fingerprint-gate.
                    reload_user_overlay(daemon_tx, &mut state, &paths.overlay);
                }
                Err(_) => {
                    // The config transport died; continue on the maintenance
                    // channel and timer alone (overlay reloads fall back to
                    // /refresh-models).
                    warn!("config transport channel closed; overlay auto-reload unavailable");
                }
            },
            recv(after(timeout)) -> _ => {
                // The retry timer fired. If a retry was scheduled and is due,
                // revalidate; otherwise (no retry pending) just loop.
                if let Some(at) = state.next_retry_at
                    && Instant::now() >= at
                {
                    state.next_retry_at = None;
                    record_attempt(db, &mut state);
                    run_refresh(daemon_tx, &mut state, false, Vec::new());
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use choreo_proto::CatalogProvider;

    #[test]
    fn catalog_updated_payload_round_trips_catalog_provider() {
        // CatalogProvider is the wire pair the daemon broadcasts; make sure
        // the TUI-facing shape stays slug+display_name.
        let p = CatalogProvider {
            slug: "openai".into(),
            display_name: "OpenAI".into(),
        };
        assert_eq!(p.slug, "openai");
        assert_eq!(p.display_name, "OpenAI");
    }
}
