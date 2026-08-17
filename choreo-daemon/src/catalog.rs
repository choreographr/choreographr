//! Runtime catalog maintenance (S4): cache persistence, the background
//! models.dev refresh thread, and the user-overlay `notify` watcher.
//!
//! **Threading.** One background thread owns the whole runtime pipeline. It
//! loads the base (disk cache → embedded `catalog.bin`), applies the user
//! overlay, does the conditional GET against models.dev, and watches the
//! config directory for `models-overlay.toml` edits — but it NEVER mutates
//! the catalog itself. Every change is sent to the daemon command loop as a
//! [`DaemonCommand::CatalogBaseChanged`], which is the single writer of the
//! [`choreo_ai_protocols::PROVIDER_CATALOG`] `ArcSwap` (the documented
//! thread-communication exception). All cross-thread communication here is
//! channel-based: the daemon hands `/refresh-models` requests to this thread
//! over a channel (never the command loop doing HTTP), and the notify
//! callback forwards filesystem events over the same channel.
//!
//! **Refresh pacing (S4).** A models.dev fetch is attempted at most once per
//! [`REFRESH_ATTEMPT_INTERVAL`] (25 h), regardless of whether the last attempt
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
//! attempt.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use choreo_ai_protocols::{
    ProviderEntry, RefreshError, RefreshOutcome, fetch_modelsdev, load_bundled_base,
    normalize_modelsdev, write_file_atomic,
};
use choreo_proto::RefreshStatus;
use crossbeam_channel::{Receiver, Sender};
use notify::{RecursiveMode, Watcher};
use tracing::{debug, info, warn};

use crate::daemon::DaemonCommand;
use crate::db::{get_catalog_etag, get_catalog_last_attempt_ms, set_catalog_last_attempt_ms};

/// Cooldown between models.dev fetch attempts — the revalidation cadence AND
/// the no-reattempt window, whatever the last outcome (200/304/failure).
/// The thread waits on its channel with the remaining time as the recv
/// timeout, so the catalog never goes stale and a failure never spins — it
/// just waits for the next trigger (a `/refresh-models` request, an overlay
/// event, or this interval).
///
/// 25 h rather than 24 h: a fixed period that is not a divisor of the day
/// makes each daemon's fetch time drift +1 h/day, so across a population of
/// daemons (or across days for one daemon) the load wraps around the daily
/// cycle instead of a majority always hitting the server during working
/// hours. `/refresh-models` bypasses it anytime.
const REFRESH_ATTEMPT_INTERVAL: Duration = Duration::from_secs(25 * 60 * 60);

/// Postcard cache filename under the data dir.
const CATALOG_BIN_NAME: &str = "catalog.bin";
/// User overlay filename under the config dir.
const USER_OVERLAY_NAME: &str = "models-overlay.toml";

/// Reply payload for a `/refresh-models` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshReport {
    pub providers: usize,
    pub models: usize,
    pub status: RefreshStatus,
}

/// One `/refresh-models` requester folded into a coalesced batch: its reply
/// channel plus whether IT asked for a forced fetch. The batch performs ONE
/// shared fetch, forced if ANY requester asked ([`run_refresh`]'s `force` is
/// the OR), but each requester's reply status reflects its own flag — a
/// plain request folded into a forced burst is reported `Updated`, not
/// `Forced`, matching what it actually asked for.
#[derive(Debug)]
pub struct RefreshRequester {
    pub force: bool,
    pub tx: mpsc::Sender<Result<RefreshReport, String>>,
}

/// Messages on the maintenance thread's single channel: requests from the
/// daemon command loop and filesystem events forwarded by the notify
/// callback. One channel for both so the thread waits on a single receiver
/// (whose recv timeout drives the retry timer).
#[derive(Debug)]
pub enum MaintenanceEvent {
    /// A `/refresh-models` request. The HTTP fetch happens HERE (never in the
    /// command loop — it can block for the whole 30s timeout); the result is
    /// then handed back through the daemon loop, which owns the catalog swap.
    RefreshNow {
        force: bool,
        reply: mpsc::Sender<Result<RefreshReport, String>>,
    },
    /// A raw notify event about the watched config directory. Filtered and
    /// re-read (fingerprint-gated) by the maintenance thread.
    OverlayFsEvent(Result<notify::Event, notify::Error>),
}

/// Filesystem locations the runtime catalog pipeline touches. Kept in one
/// struct so the daemon state, the maintenance-thread spawn, and the unit
/// tests all agree on where things live. The etag and the last-attempt
/// timestamp deliberately do NOT live here — they are persisted in the DB
/// (`catalog_state` table, see [`crate::db`]), not on the filesystem.
#[derive(Debug, Clone, Default)]
pub struct CatalogPaths {
    /// Postcard cache of the normalized models.dev base
    /// (`$XDG_DATA_HOME/choreographr/catalog.bin`).
    pub bin: PathBuf,
    /// User overlay TOML (`$XDG_CONFIG_HOME/choreographr/models-overlay.toml`),
    /// watched for changes.
    pub overlay: PathBuf,
}

impl CatalogPaths {
    /// Resolve the standard locations, mirroring `crate::db::db_path` and
    /// `crate::config::config_path` (same `dirs::data_dir()` /
    /// `dirs::config_dir()` convention, `choreographr` subdirectory). Falls
    /// back to empty paths (everything degrades to the embedded catalog) when
    /// the dirs lookup fails, so startup never hard-fails on an exotic
    /// HOME-less environment.
    pub fn from_dirs() -> Self {
        let data_dir = dirs::data_dir().map(|d| d.join("choreographr"));
        let config_dir = dirs::config_dir().map(|d| d.join("choreographr"));
        match (&data_dir, &config_dir) {
            (Some(data), Some(config)) => Self {
                bin: data.join(CATALOG_BIN_NAME),
                overlay: config.join(USER_OVERLAY_NAME),
            },
            _ => {
                warn!(
                    ?data_dir,
                    ?config_dir,
                    "could not resolve catalog cache/overlay dirs; using the embedded catalog only",
                );
                Self::default()
            }
        }
    }
}

/// Load the cached normalized base from `path` (postcard). Logs a warning and
/// returns `None` when the file is missing or fails to deserialize — the
/// caller then falls back to the embedded `catalog.bin` (load order: valid
/// cache file → embedded).
pub(crate) fn load_cached_base(path: &Path) -> Option<Vec<ProviderEntry>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to read catalog cache");
            return None;
        }
    };
    match postcard::from_bytes(&bytes) {
        Ok(base) => Some(base),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "catalog cache failed to deserialize; falling back to the embedded catalog",
            );
            None
        }
    }
}

/// Read the user overlay file. `Ok(None)` means the file is absent; `Err` is
/// an unreadable-but-present file (permissions, etc.) — callers warn and keep
/// the last-applied value rather than churn on a transient read error.
pub(crate) fn read_user_overlay(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether a freshly re-read user overlay differs from the last-applied value.
///
/// Pure fingerprint compare — this is what makes the notify watcher collapse
/// editor save-event storms deterministically: after the first reload the
/// contents match and nothing is sent until the file *actually* changes again
/// (or appears/disappears). Unit-testable without any time-based logic.
pub(crate) fn overlay_fingerprint_changed(last_applied: Option<&str>, fresh: Option<&str>) -> bool {
    last_applied != fresh
}

/// Persist the cache bin atomically (temp file in the same directory → fsync →
/// rename, so a reader sees either the old or the new file, never a torn one).
/// The models.dev **etag is NOT written here** — it is persisted to the DB by
/// the daemon command loop AFTER this returns ([`crate::daemon::DaemonState::
/// persist_catalog_cache`]), so a crash between the two writes leaves the OLD
/// etag paired with the OLD bin, which self-heals via a 200 on the next fetch
/// (a new etag over old content would 304 forever instead).
pub(crate) fn write_catalog_cache(base: &[ProviderEntry], bin_path: &Path) -> io::Result<()> {
    let bytes = postcard::to_allocvec(base).map_err(io::Error::other)?;
    write_file_atomic(bin_path, &bytes)
}

/// Ensure the runtime directories exist before the maintenance pipeline
/// starts. The **config dir** (the user overlay's parent) is the critical
/// one: nothing ever creates it until the user writes an overlay, and
/// `notify` cannot watch a directory that does not exist — so on a fresh
/// system the initial watch would fail and (previously) only be retried on
/// the revalidation cadence, leaving a later-created overlay unwatched for a
/// day. Creating it up front makes the first watch install succeed.
/// The **data dir** (cache parent) is created for symmetry; `write_file_atomic`
/// would create it on first persist anyway. A creation failure is logged,
/// never fatal — the daemon degrades to the embedded catalog and manual
/// `/refresh-models` reloads, and the loop's watch retry remains as a
/// last-resort fallback.
fn ensure_runtime_dirs(paths: &CatalogPaths) {
    for dir in [paths.overlay.parent(), paths.bin.parent()]
        .into_iter()
        .flatten()
    {
        match std::fs::create_dir_all(dir) {
            Ok(()) => debug!(dir = %dir.display(), "catalog runtime dir ready"),
            Err(e) => warn!(
                dir = %dir.display(),
                error = %e,
                "failed to create catalog runtime dir; overlay auto-reload and cache \
                 persistence may be unavailable",
            ),
        }
    }
}

/// Spawn the ONE background catalog-maintenance thread. Returns the channel
/// sender the daemon command loop uses to hand `/refresh-models` requests to
/// it. The DB is handed in because the thread is the single writer of the
/// catalog refresh state (`catalog_state`: last-attempt timestamp — it
/// observes every fetch outcome, unlike the command loop, which only sees
/// accepted swaps). The thread is detached (the process exits after
/// `run_server` returns; a lingering maintenance thread cannot outlive main,
/// and its sends to the daemon channel fail harmlessly once the command loop
/// is gone).
pub(crate) fn spawn_catalog_maintenance(
    daemon_tx: mpsc::Sender<DaemonCommand>,
    db: Arc<redb::Database>,
    paths: CatalogPaths,
) -> Sender<MaintenanceEvent> {
    let (tx, rx) = crossbeam_channel::unbounded::<MaintenanceEvent>();
    // The notify callback needs its own sender clone; the daemon keeps the
    // original for RefreshNow requests.
    let notify_tx = tx.clone();
    let _ = std::thread::Builder::new()
        .name("catalog-maintenance".into())
        .spawn(move || maintenance_loop(daemon_tx, db, paths, rx, notify_tx));
    tx
}

/// Mutable state of the maintenance thread: the current normalized base +
/// etag (the *facts* the daemon merges overlays onto), the wall-clock
/// last-attempt anchor (loaded from the DB at startup, kept in sync by
/// [`record_attempt`]), and the fingerprint of the last user overlay it
/// handed to the daemon loop.
struct MaintenanceState {
    base: Vec<ProviderEntry>,
    etag: Option<String>,
    /// Unix epoch millis of the last fetch attempt (DB `catalog_state`).
    /// `None` = never attempted (first run / upgrade) → fetch immediately.
    last_attempt_ms: Option<u64>,
    last_applied_user_overlay: Option<String>,
    next_retry_at: Option<Instant>,
}

fn maintenance_loop(
    daemon_tx: mpsc::Sender<DaemonCommand>,
    db: Arc<redb::Database>,
    paths: CatalogPaths,
    rx: Receiver<MaintenanceEvent>,
    notify_tx: Sender<MaintenanceEvent>,
) {
    // ── 0. Ensure the runtime dirs exist (config dir for the overlay watch,
    // data dir for the cache) so the notify watch below installs on the FIRST
    // attempt even on a fresh system — see `ensure_runtime_dirs` for why this
    // ordering matters.
    ensure_runtime_dirs(&paths);

    // ── 1. Load the base: valid cache file first, embedded catalog.bin as
    // the fallback (the S4 load order). The etag is only read from the DB
    // when the cache loaded: a missing/corrupt cache must produce `etag =
    // None` so the next fetch is a plain GET that rebuilds both — a 304
    // with no cache would otherwise leave the daemon on the embedded blob
    // forever (the etag-requires-cache invariant, pinned by tests).
    let (base, etag, cache_valid) = match load_cached_base(&paths.bin) {
        Some(base) => {
            let etag = match get_catalog_etag(&db) {
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
                std::fs::metadata(&paths.bin).map(|m| m.len()).unwrap_or(0),
            );
            (base, etag, true)
        }
        None => {
            let base = load_bundled_base();
            info!(
                providers = base.len(),
                "no valid catalog cache; using the embedded catalog.bin",
            );
            (base, None, false)
        }
    };

    // ── 1b. Load the persisted last-attempt anchor. `None` (never attempted
    // — first run, or an upgrade from a build without the key) means stale:
    // the startup gate below fetches immediately.
    let last_attempt_ms = match get_catalog_last_attempt_ms(&db) {
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
        base,
        etag,
        last_attempt_ms,
        last_applied_user_overlay: user_overlay.clone(),
        next_retry_at: None,
    };

    // ── 3. Apply the initial catalog through the daemon command loop (the
    // single writer of the ArcSwap). No persist: a cache-sourced base is
    // already on disk; a cache-miss will be persisted on the first fetch.
    let _ = daemon_tx.send(DaemonCommand::CatalogBaseChanged {
        base: state.base.clone(),
        etag: state.etag.clone(),
        user_overlay,
        persist: false,
        reply: Vec::new(),
    });

    // ── 4. Register the notify watcher on the config DIRECTORY (rename-safe:
    // an editor that writes temp + rename fires events for the directory, and
    // we filter by basename below). The callback only forwards raw events —
    // all policy (filter, re-read, fingerprint gate) lives here on the
    // maintenance thread, keeping the notify-owned thread trivially small.
    let mut watcher: Option<notify::RecommendedWatcher> =
        match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let _ = notify_tx.send(MaintenanceEvent::OverlayFsEvent(res));
        }) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    error = %e,
                    "failed to create the filesystem watcher; user overlay changes \
                     will not reload automatically",
                );
                None
            }
        };
    // The watch targets the config DIRECTORY, not the file itself. The dirs
    // were created in step 0, so the initial watch normally succeeds even on
    // a fresh system. The re-arm below is a last-resort fallback for a
    // directory that is deleted at runtime or whose creation/permission
    // failed at startup — the `/refresh-models` path remains the documented
    // manual reload. The watcher is kept alive either way — a failed watch
    // simply never fires until re-armed.
    let watch_dir = paths.overlay.parent().map(Path::to_path_buf);
    let mut watch_armed = false;
    if let (Some(w), Some(dir)) = (watcher.as_mut(), watch_dir.as_deref()) {
        match w.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => watch_armed = true,
            Err(e) => {
                warn!(
                    dir = %dir.display(),
                    error = %e,
                    "failed to watch the config directory; retrying in the \
                     maintenance loop (overlay changes also reload on `/refresh-models`)",
                );
            }
        }
    }

    // ── 5. Initial conditional GET — gated on cache freshness. Fetch
    // immediately iff there is no valid cache, no recorded attempt (first
    // run / upgrade), or the last attempt is stale (≥ REFRESH_ATTEMPT_INTERVAL
    // wall-clock ago). Otherwise the cache is fresh enough: skip the startup
    // fetch entirely and arm the in-run timer for the remaining time, derived
    // from the persisted attempt timestamp — so a daemon restarted within the
    // cooldown window does NOT hit the network at every start, and the 25 h
    // drift of the fetch time across the daily cycle survives restarts.
    if should_fetch_at_startup(cache_valid, state.last_attempt_ms, wall_now_ms()) {
        record_attempt(&db, &mut state);
        run_refresh(&daemon_tx, &mut state, false, Vec::new());
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

    // ── 6. Event loop: wait on the channel (requests + overlay events) with
    // the retry timer as the recv timeout.
    loop {
        // Last-resort re-arm of the config-dir watch if the initial attempt
        // failed (a dir deleted at runtime, or a creation failure in step 0
        // that has since been fixed). This runs on the loop's natural cadence
        // (channel events + the revalidation timeout); the step-0 dir
        // creation is the primary fix, so this path is rarely taken. Cheap
        // while unarmed (a failed watch is a quick syscall); a no-op once
        // armed.
        if !watch_armed && let (Some(w), Some(dir)) = (watcher.as_mut(), watch_dir.as_deref()) {
            match w.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    info!(
                        dir = %dir.display(),
                        "config directory is now watchable; overlay auto-reload armed",
                    );
                    watch_armed = true;
                }
                Err(e) => {
                    // Debug, not warn: this retries on every loop iteration
                    // until the dir exists, so a warn would spam the log.
                    tracing::debug!(
                        dir = %dir.display(),
                        error = %e,
                        "config directory still not watchable; will retry",
                    );
                }
            }
        }
        let timeout = state
            .next_retry_at
            .map(|at| at.saturating_duration_since(Instant::now()))
            .unwrap_or(REFRESH_ATTEMPT_INTERVAL);
        match rx.recv_timeout(timeout) {
            Ok(MaintenanceEvent::RefreshNow { force, reply }) => {
                // /refresh-models is the documented fallback for overlay
                // reloads (e.g. when the notify watch could not start). Re-read
                // the file so the command re-syncs the user layer too, not
                // just the models.dev base — and so an overlay edit is applied
                // even when the conditional GET below returns 304 (a 304 sends
                // no CatalogBaseChanged, so without this the reload is lost).
                reload_user_overlay(&daemon_tx, &mut state, &paths.overlay);
                // Coalesce: drain any RefreshNows queued while we were idle so
                // a burst of /refresh-models becomes ONE fetch. Fold the force
                // flag (a --force anywhere in the burst forces) and keep every
                // reply sender so no requester is left hanging. The whole
                // burst is ONE attempt (one timestamp write below).
                let (any_force, replies) = fold_refresh_nows(&rx, force, reply);
                // Explicit user intent bypasses the cooldown, but the attempt
                // is still recorded (and the timer re-armed by run_refresh) so
                // the DB anchor reflects reality — otherwise the next startup
                // would re-fetch immediately.
                record_attempt(&db, &mut state);
                run_refresh(&daemon_tx, &mut state, any_force, replies);
            }
            Ok(MaintenanceEvent::OverlayFsEvent(Ok(event))) => {
                if is_overlay_event(&event, &paths.overlay) {
                    reload_user_overlay(&daemon_tx, &mut state, &paths.overlay);
                }
            }
            Ok(MaintenanceEvent::OverlayFsEvent(Err(e))) => {
                warn!(error = %e, "filesystem watcher error");
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // The recv timeout fired. If a retry was scheduled and is due,
                // revalidate; otherwise (no retry pending) just loop.
                if let Some(at) = state.next_retry_at
                    && Instant::now() >= at
                {
                    state.next_retry_at = None;
                    record_attempt(&db, &mut state);
                    run_refresh(&daemon_tx, &mut state, false, Vec::new());
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                info!("catalog maintenance channel closed; exiting");
                break;
            }
        }
    }
}

/// Whether the startup path should fetch immediately: no valid cache, no
/// recorded attempt (first run / upgrade from a build without the key), or a
/// stale attempt (`now − last_attempt ≥ REFRESH_ATTEMPT_INTERVAL`). The gate
/// is deliberately conservative — anything unknown fetches — because the
/// cost of a wrong "fetch" is one polite conditional GET, while the cost of
/// a wrong "skip" is an arbitrarily stale cache.
///
/// Pure function of injected wall-clock `now_ms` so it is unit-testable
/// without any time-based logic.
fn should_fetch_at_startup(cache_valid: bool, last_attempt_ms: Option<u64>, now_ms: u64) -> bool {
    if !cache_valid {
        return true;
    }
    match last_attempt_ms {
        None => true,
        Some(at) => now_ms.saturating_sub(at) >= REFRESH_ATTEMPT_INTERVAL.as_millis() as u64,
    }
}

/// Derive the in-run revalidation deadline from the persisted wall-clock
/// attempt anchor: `now + (REFRESH_ATTEMPT_INTERVAL − elapsed)`, saturated at
/// `now` when the deadline has already passed (the next loop iteration then
/// fires the refresh immediately). `None` when there is no recorded attempt
/// (nothing to derive from — the startup gate fetches instead).
///
/// The wall↔instant correspondence is captured here once, at startup: `now`
/// (monotonic) and `now_ms` (wall) are read at the same moment, so the
/// computed duration maps correctly onto the monotonic timeline. A suspend
/// inside a single run therefore pauses the countdown (the monotonic clock
/// does not advance during sleep), which is the accepted awake-time
/// semantics; a restart re-derives from the DB anchor and gets strict wall
/// time.
fn next_retry_deadline(last_attempt_ms: Option<u64>, now: Instant, now_ms: u64) -> Option<Instant> {
    let at = last_attempt_ms?;
    let elapsed = Duration::from_millis(now_ms.saturating_sub(at));
    let remaining = REFRESH_ATTEMPT_INTERVAL.saturating_sub(elapsed);
    Some(now + remaining)
}

/// Wall-clock epoch millis (`u64`). The cooldown anchor must be wall time so
/// it survives restarts; the in-run deadline is derived from it at startup
/// (see [`next_retry_deadline`]). Falls back to 0 (ancient → stale → fetch)
/// if the clock is before the Unix epoch, which never happens in practice.
fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Record the start of a fetch attempt (wall-clock epoch millis) in the DB
/// BEFORE the fetch runs. This is the crash-safe cooldown: a daemon that
/// dies mid-fetch and restarts immediately reads a fresh timestamp and
/// honors the remaining cooldown instead of re-fetching. Every attempt —
/// startup refresh, timer revalidation, or `/refresh-models` (a coalesced
/// burst is ONE attempt) — goes through this; the outcome (200/304/failure)
/// is irrelevant to the pacing, which is the point of the no-reattempt rule.
/// A DB write failure is logged, never fatal: the timestamp is advisory
/// pacing, and the worst case is the next startup re-fetching.
fn record_attempt(db: &redb::Database, state: &mut MaintenanceState) {
    let now_ms = wall_now_ms();
    state.last_attempt_ms = Some(now_ms);
    if let Err(e) = set_catalog_last_attempt_ms(db, now_ms) {
        warn!(
            error = %e,
            "failed to persist the catalog attempt timestamp; the cooldown will \
             not survive a restart (the next startup re-fetches)",
        );
    }
}

/// Perform one models.dev refresh on the maintenance thread.
///
/// * `NotModified` → log + route the `UpToDate` reply through the daemon
///   command loop ([`DaemonCommand::CatalogNotModified`], so any overlay
///   reload queued just before is applied first) + schedule the next
///   revalidation.
/// * `Fetched` → normalize, validate non-empty, then hand the new base to the
///   daemon command loop ([`DaemonCommand::CatalogBaseChanged`] with
///   `persist: true`), which swaps the catalog, writes the cache, broadcasts
///   `CatalogUpdated`, and replies to the requester(s).
/// * `Err` → log + reply the error + schedule a retry.
///
/// `reply` holds one requester per `/refresh-models` request (empty for
/// background refreshes). Every outcome arms `next_retry_at` (the
/// recv-timeout cadence), so the catalog revalidates on a fixed schedule no
/// matter what the last fetch did.
fn run_refresh(
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    state: &mut MaintenanceState,
    force: bool,
    reply: Vec<RefreshRequester>,
) {
    // The fetch is injected so the outcome→state machine is unit-testable
    // without a network round trip; production always uses the real ureq GET.
    run_refresh_impl(daemon_tx, state, force, reply, |etag, force| {
        fetch_modelsdev(etag, force)
    });
}

/// The refresh state machine behind [`run_refresh`], with the models.dev fetch
/// abstracted out. Every branch arms `next_retry_at` so the catalog keeps a
/// steady revalidation cadence; the daemon command loop receives a
/// `CatalogBaseChanged` (the single-writer swap) only when a new base was
/// actually fetched, and a `CatalogNotModified` (a pure reply, no swap) on a
/// 304. `reply` holds one requester per `/refresh-models` request (empty for
/// background refreshes); a fetched outcome fans the same report out to every
/// requester, with each requester's own `force` flag individualizing
/// `Forced` vs `Updated`.
fn run_refresh_impl<F>(
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    state: &mut MaintenanceState,
    force: bool,
    reply: Vec<RefreshRequester>,
    fetch: F,
) where
    F: FnOnce(Option<&str>, bool) -> Result<RefreshOutcome, RefreshError>,
{
    match fetch(state.etag.as_deref(), force) {
        Ok(RefreshOutcome::NotModified) => {
            info!(
                force,
                "models.dev catalog unchanged (304); keeping the current catalog",
            );
            // Route the reply through the daemon command loop rather than
            // replying directly: the `/refresh-models` arm re-reads the user
            // overlay just before this refresh, so an overlay reload may be
            // queued ahead of us on the command channel. FIFO ordering makes
            // the daemon apply that swap FIRST, so the `UpToDate` counts it
            // replies with reflect the post-reload catalog — a direct reply
            // here could report stale pre-reload counts.
            if !reply.is_empty() {
                let _ = daemon_tx.send(DaemonCommand::CatalogNotModified { reply });
            }
            // The cache is still valid; revalidate later to keep it fresh
            // without hammering models.dev.
            state.next_retry_at = Some(Instant::now() + REFRESH_ATTEMPT_INTERVAL);
        }
        Ok(RefreshOutcome::Fetched { json, etag }) => {
            let new_base = normalize_modelsdev(&json);
            if new_base.is_empty() {
                // The remote returned something that did not normalize to a
                // catalog (schema drift, truncated body, …). Keep the current
                // catalog rather than swapping in nothing.
                warn!(
                    "models.dev response did not normalize into a non-empty catalog; \
                     keeping the current catalog",
                );
                for r in reply {
                    let _ = r.tx.send(Err(
                        "models.dev response did not parse into a non-empty catalog".to_string(),
                    ));
                }
                state.next_retry_at = Some(Instant::now() + REFRESH_ATTEMPT_INTERVAL);
                return;
            }
            info!(
                providers = new_base.len(),
                ?etag,
                force,
                "models.dev refresh fetched a new catalog",
            );
            state.base = new_base;
            state.etag = etag;
            // Even a successful fetch arms the next revalidation: without
            // this the catalog would go permanently stale after the first 200
            // (the event loop only refreshes when next_retry_at is set), and
            // the etag makes the next conditional GET cheap.
            state.next_retry_at = Some(Instant::now() + REFRESH_ATTEMPT_INTERVAL);
            let _ = daemon_tx.send(DaemonCommand::CatalogBaseChanged {
                base: state.base.clone(),
                etag: state.etag.clone(),
                user_overlay: state.last_applied_user_overlay.clone(),
                persist: true,
                reply,
            });
        }
        Err(e) => {
            warn!(error = %e, "models.dev refresh failed; will retry later");
            for r in reply {
                let _ = r.tx.send(Err(e.to_string()));
            }
            state.next_retry_at = Some(Instant::now() + REFRESH_ATTEMPT_INTERVAL);
        }
    }
}

/// Fold a burst of queued [`MaintenanceEvent::RefreshNow`] events into the
/// one being processed: OR the force flags and collect every reply sender, so
/// a burst of `/refresh-models` requests performs a single fetch while every
/// requester still gets a reply. `try_recv` never blocks — this only drains
/// what has already been queued. Returns the effective force flag and the
/// folded requesters (the first event's plus any queued behind it), each
/// carrying its own force flag so replies can be individualized.
fn fold_refresh_nows(
    rx: &Receiver<MaintenanceEvent>,
    force: bool,
    first_reply: mpsc::Sender<Result<RefreshReport, String>>,
) -> (bool, Vec<RefreshRequester>) {
    let mut any_force = force;
    let mut replies = vec![RefreshRequester {
        force,
        tx: first_reply,
    }];
    while let Ok(MaintenanceEvent::RefreshNow { force, reply }) = rx.try_recv() {
        any_force |= force;
        replies.push(RefreshRequester { force, tx: reply });
    }
    (any_force, replies)
}

/// Re-read the user overlay and, if its contents changed since the
/// last-applied value (the fingerprint gate), hand the new value to the
/// daemon command loop so it re-merges and swaps. Shared by the notify
/// watcher and the `/refresh-models` path, so the overlay reload policy lives
/// in exactly one place. A deleted file sends an explicit `None` so the daemon
/// falls back to bundled-only; an unreadable-but-present file warns and keeps
/// the last-applied value rather than churn on a transient read error.
fn reload_user_overlay(
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    state: &mut MaintenanceState,
    overlay_path: &Path,
) {
    match read_user_overlay(overlay_path) {
        Ok(contents) => {
            if overlay_fingerprint_changed(
                state.last_applied_user_overlay.as_deref(),
                contents.as_deref(),
            ) {
                debug!(
                    path = %overlay_path.display(),
                    present = contents.is_some(),
                    "user overlay changed; reloading",
                );
                state.last_applied_user_overlay = contents.clone();
                let _ = daemon_tx.send(DaemonCommand::CatalogBaseChanged {
                    base: state.base.clone(),
                    etag: state.etag.clone(),
                    user_overlay: contents,
                    persist: false,
                    reply: Vec::new(),
                });
            }
        }
        Err(e) => {
            warn!(
                path = %overlay_path.display(),
                error = %e,
                "failed to re-read the user overlay after a change; keeping the \
                 last-applied value",
            );
        }
    }
}

/// Whether a notify event concerns the user overlay file. The watch is on the
/// config *directory* (rename-safe), so unrelated files' events arrive too —
/// filter by basename and by event kind (ignore pure `Access`/`Other` noise,
/// e.g. macOS FSEvents access reports).
fn is_overlay_event(event: &notify::Event, overlay_path: &Path) -> bool {
    use notify::EventKind;
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    let Some(name) = overlay_path.file_name() else {
        return false;
    };
    event.paths.iter().any(|p| p.file_name() == Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_ai_protocols::{ModelEntry, ProviderProtocol};
    use choreo_proto::CatalogProvider;

    /// A tiny two-provider base for the cache round-trip tests.
    fn tiny_base() -> Vec<ProviderEntry> {
        vec![
            ProviderEntry {
                slug: "acme".into(),
                display_name: "Acme".into(),
                protocol: ProviderProtocol::OpenAi {
                    max_tokens_field: choreo_ai_protocols::MaxTokensField::MaxCompletionTokens,
                },
                base_url: "https://api.acme.dev/v1".into(),
                default_model: "acme-1".into(),
                models: vec![ModelEntry {
                    model: "acme-1".into(),
                    context_window: 8192,
                    reasoning_supported: true,
                    openai_reasoning_levels: vec!["off".into(), "high".into()],
                    openai_responses: false,
                    reasoning_passback: None,
                    reasoning_content_required: None,
                }],
            },
            ProviderEntry {
                slug: "zoocorp".into(),
                display_name: "Zoo Corp".into(),
                protocol: ProviderProtocol::AnthropicMessages,
                base_url: "https://api.zoocorp.dev".into(),
                default_model: "zoo-1".into(),
                models: Vec::new(),
            },
        ]
    }

    #[test]
    fn fingerprint_compare_ignores_unchanged_contents() {
        // The fingerprint gate: identical contents (the common case after an
        // editor save storm) must NOT trigger a reload.
        assert!(!overlay_fingerprint_changed(Some("a"), Some("a")));
        assert!(!overlay_fingerprint_changed(None, None));
        // Any difference — including appearance/disappearance — must trigger.
        assert!(overlay_fingerprint_changed(Some("a"), Some("b")));
        assert!(overlay_fingerprint_changed(None, Some("a")));
        assert!(overlay_fingerprint_changed(Some("a"), None));
    }

    #[test]
    fn cached_base_round_trips_through_postcard() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("catalog.bin");

        // No cache yet → None, and the caller falls back to embedded.
        assert!(load_cached_base(&bin).is_none());

        write_catalog_cache(&tiny_base(), &bin).unwrap();

        let loaded = load_cached_base(&bin).expect("cache loads");
        // ProviderEntry has no PartialEq; compare the load-bearing fields.
        assert_eq!(loaded.len(), tiny_base().len());
        assert_eq!(loaded[0].slug, "acme");
        assert_eq!(loaded[1].slug, "zoocorp");
    }

    #[test]
    fn corrupted_cache_falls_back_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("catalog.bin");
        std::fs::write(&bin, b"not postcard data").unwrap();
        // A corrupt cache must not brick the daemon: it logs a warning and
        // returns None so the embedded catalog.bin is used instead.
        assert!(load_cached_base(&bin).is_none());
    }

    #[test]
    fn user_overlay_read_distinguishes_missing_from_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models-overlay.toml");
        assert_eq!(read_user_overlay(&path).unwrap(), None);

        std::fs::write(&path, "[provider.acme]\nbase_url = \"x\"\n").unwrap();
        assert_eq!(
            read_user_overlay(&path).unwrap().as_deref(),
            Some("[provider.acme]\nbase_url = \"x\"\n")
        );
    }

    #[test]
    fn overlay_event_filter_matches_basename_only() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = dir.path().join("models-overlay.toml");
        let other = dir.path().join("accounts.toml");

        let mk = |path: &Path, kind: notify::EventKind| notify::Event {
            kind,
            paths: vec![path.to_path_buf()],
            attrs: notify::event::EventAttributes::default(),
        };

        // Our file: Create/Modify/Remove all qualify.
        assert!(is_overlay_event(
            &mk(
                &overlay,
                notify::EventKind::Create(notify::event::CreateKind::File)
            ),
            &overlay
        ));
        assert!(is_overlay_event(
            &mk(
                &overlay,
                notify::EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any
                )),
            ),
            &overlay
        ));
        assert!(is_overlay_event(
            &mk(
                &overlay,
                notify::EventKind::Remove(notify::event::RemoveKind::File)
            ),
            &overlay
        ));
        // A different file in the same directory does not.
        assert!(!is_overlay_event(
            &mk(
                &other,
                notify::EventKind::Create(notify::event::CreateKind::File)
            ),
            &overlay
        ));
        // Pure access/other noise never qualifies.
        assert!(!is_overlay_event(
            &mk(
                &overlay,
                notify::EventKind::Access(notify::event::AccessKind::Read)
            ),
            &overlay
        ));
        assert!(!is_overlay_event(
            &mk(&overlay, notify::EventKind::Other),
            &overlay
        ));
    }

    #[test]
    fn catalog_paths_resolve_under_choreographr_dirs() {
        // from_dirs() follows the same convention as db_path/accounts_config_path.
        // We can't assert the absolute value (the XDG dirs are env-dependent),
        // but the file NAMES must match the documented contract. The etag and
        // last-attempt timestamp are deliberately NOT here — they live in the
        // DB (catalog_state), not on the filesystem.
        let paths = CatalogPaths::from_dirs();
        assert!(paths.bin.ends_with("choreographr/catalog.bin"));
        assert!(paths.overlay.ends_with("choreographr/models-overlay.toml"));
    }

    #[test]
    fn catalog_base_changed_payload_is_complete() {
        // Guard the message the maintenance thread sends: every field the
        // daemon handler needs to merge, persist, and reply is present. This
        // pins the shape so a refactor cannot silently drop a field.
        let (reply, _rx) = mpsc::channel();
        let _ = DaemonCommand::CatalogBaseChanged {
            base: tiny_base(),
            etag: Some("\"v9\"".into()),
            user_overlay: Some("[provider.acme]\nbase_url = \"x\"\n".into()),
            persist: true,
            reply: vec![RefreshRequester {
                force: true,
                tx: reply,
            }],
        };
    }

    // ── run_refresh_impl (the refresh state machine, fetcher injected) ──

    /// A minimal models.dev snapshot that normalizes to exactly one provider
    /// — the payload for the injected `Fetched` branch.
    const SNAPSHOT_JSON: &str = r#"{
        "acme": {
            "name": "Acme",
            "npm": "@ai-sdk/openai-compatible",
            "models": {
                "acme-1": {"reasoning": false, "limit": {"context": 8192}}
            }
        }
    }"#;

    /// A starting maintenance state for the state-machine tests: a small
    /// base, a cached etag, a recent (fresh) attempt timestamp, no
    /// revalidation pending.
    fn maintenance_state() -> MaintenanceState {
        MaintenanceState {
            base: tiny_base(),
            etag: Some("\"v1\"".into()),
            last_attempt_ms: Some(1_700_000_000_000),
            last_applied_user_overlay: None,
            next_retry_at: None,
        }
    }

    #[test]
    fn run_refresh_fetched_arms_revalidation_and_sends_base_changed() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let mut state = maintenance_state();
        let (reply_tx, _reply_rx) = mpsc::channel();

        run_refresh_impl(
            &daemon_tx,
            &mut state,
            false,
            vec![RefreshRequester {
                force: false,
                tx: reply_tx,
            }],
            |_etag, _force| {
                Ok(RefreshOutcome::Fetched {
                    json: SNAPSHOT_JSON.into(),
                    etag: Some("\"v2\"".into()),
                })
            },
        );

        // The new base + etag were adopted and the NEXT revalidation is
        // armed — a successful fetch must not stop the cadence (that would
        // leave the catalog permanently stale after the first 200).
        assert_eq!(state.etag.as_deref(), Some("\"v2\""));
        assert_eq!(state.base.len(), 1, "snapshot normalizes to one provider");
        assert!(
            state.next_retry_at.is_some(),
            "a successful fetch must schedule the next revalidation"
        );

        // The daemon loop gets the swap command with persist set and the
        // requester's own (non-forced) flag, so the daemon can individualize
        // the reply status.
        match daemon_rx.recv().unwrap() {
            DaemonCommand::CatalogBaseChanged { persist, reply, .. } => {
                assert!(persist, "a live fetch must persist the cache");
                assert_eq!(
                    reply.len(),
                    1,
                    "the /refresh-models reply must be routed through the command"
                );
                assert!(
                    !reply[0].force,
                    "a plain requester must not be marked forced"
                );
            }
            other => panic!(
                "expected CatalogBaseChanged, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn run_refresh_forced_fetch_marks_the_requester_forced() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let mut state = maintenance_state();
        let (reply_tx, _reply_rx) = mpsc::channel();

        run_refresh_impl(
            &daemon_tx,
            &mut state,
            true,
            vec![RefreshRequester {
                force: true,
                tx: reply_tx,
            }],
            |_etag, _force| {
                Ok(RefreshOutcome::Fetched {
                    json: SNAPSHOT_JSON.into(),
                    etag: Some("\"v3\"".into()),
                })
            },
        );

        match daemon_rx.recv().unwrap() {
            DaemonCommand::CatalogBaseChanged { reply, .. } => {
                assert_eq!(reply.len(), 1);
                assert!(reply[0].force, "a --force requester keeps its forced flag");
            }
            other => panic!(
                "expected CatalogBaseChanged, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn run_refresh_not_modified_routes_reply_through_daemon_and_revalidates() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let mut state = maintenance_state();
        let (reply_tx, reply_rx) = mpsc::channel();

        run_refresh_impl(
            &daemon_tx,
            &mut state,
            false,
            vec![RefreshRequester {
                force: false,
                tx: reply_tx,
            }],
            |etag, force| {
                // The injected fetcher must see the cached etag and no force.
                assert_eq!(etag, Some("\"v1\""));
                assert!(!force);
                Ok(RefreshOutcome::NotModified)
            },
        );

        // The 304 reply is routed through the daemon command loop (so any
        // overlay reload queued just before is applied first); the requester's
        // sender travels in the command. Simulate the daemon's handler.
        match daemon_rx.recv().unwrap() {
            DaemonCommand::CatalogNotModified { reply } => {
                assert_eq!(reply.len(), 1, "the requester's sender is routed");
                let mut reply = reply;
                let requester = reply.pop().expect("one requester");
                let _ = requester.tx.send(Ok(RefreshReport {
                    providers: 2,
                    models: 1,
                    status: RefreshStatus::UpToDate,
                }));
            }
            other => panic!(
                "expected CatalogNotModified, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        let report = reply_rx.recv().unwrap().expect("reply is Ok");
        assert_eq!(report.status, RefreshStatus::UpToDate);
        assert!(
            state.next_retry_at.is_some(),
            "a 304 must schedule the next revalidation"
        );
    }

    #[test]
    fn run_refresh_error_replies_and_schedules_retry() {
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let mut state = maintenance_state();
        let (reply_tx, reply_rx) = mpsc::channel();

        run_refresh_impl(
            &daemon_tx,
            &mut state,
            false,
            vec![RefreshRequester {
                force: false,
                tx: reply_tx,
            }],
            |_etag, _force| Err(choreo_ai_protocols::RefreshError::Network("boom".into())),
        );

        let err = reply_rx.recv().unwrap().expect_err("reply is Err");
        assert!(err.contains("boom"), "unexpected error: {err}");
        assert!(
            state.next_retry_at.is_some(),
            "a failure must schedule a retry"
        );
    }

    #[test]
    fn run_refresh_empty_normalization_keeps_current_catalog() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let mut state = maintenance_state();
        let (reply_tx, reply_rx) = mpsc::channel();

        run_refresh_impl(
            &daemon_tx,
            &mut state,
            false,
            vec![RefreshRequester {
                force: false,
                tx: reply_tx,
            }],
            |_etag, _force| {
                Ok(RefreshOutcome::Fetched {
                    json: "not json at all".into(),
                    etag: Some("\"v4\"".into()),
                })
            },
        );

        // No swap command is sent (the current catalog stays) and the
        // requester gets a structured error, but the retry cadence is armed.
        assert!(
            daemon_rx.try_recv().is_err(),
            "no swap for an empty normalize"
        );
        let err = reply_rx.recv().unwrap().expect_err("reply is Err");
        assert!(err.contains("non-empty"), "unexpected error: {err}");
        assert!(state.next_retry_at.is_some());
    }

    // ── should_fetch_at_startup (the startup gate) ──

    /// A wall-clock `now` for the gate/deadline tests, far enough past the
    /// epoch that the arithmetic is realistic.
    const NOW_MS: u64 = 1_700_000_000_000;
    /// The interval as millis (25 h), for boundary tests.
    const INTERVAL_MS: u64 = 25 * 60 * 60 * 1000;

    #[test]
    fn startup_gate_fetches_without_a_valid_cache() {
        // No valid cache → fetch immediately, whatever the recorded attempt
        // says (even a fresh one): a missing/corrupt catalog.bin must be
        // rebuilt, not sat on forever.
        assert!(should_fetch_at_startup(false, None, NOW_MS));
        assert!(should_fetch_at_startup(false, Some(NOW_MS - 1_000), NOW_MS));
        assert!(should_fetch_at_startup(false, Some(NOW_MS), NOW_MS));
    }

    #[test]
    fn startup_gate_fetches_without_a_recorded_attempt() {
        // Missing timestamp = first run or an upgrade from a build without
        // the key → unknown freshness → fetch (conservative: a wrong fetch is
        // one polite conditional GET; a wrong skip is a stale cache).
        assert!(should_fetch_at_startup(true, None, NOW_MS));
    }

    #[test]
    fn startup_gate_skips_fetch_while_attempt_is_fresh() {
        // A valid cache + a recorded attempt inside the cooldown window →
        // skip the startup fetch (the daemon does not hit the network at
        // every start, and the 25 h drift survives restarts).
        assert!(!should_fetch_at_startup(true, Some(NOW_MS - 1_000), NOW_MS));
        assert!(!should_fetch_at_startup(
            true,
            Some(NOW_MS - INTERVAL_MS / 2),
            NOW_MS
        ));
    }

    #[test]
    fn startup_gate_fetches_at_or_after_the_interval() {
        // Exactly at the boundary (elapsed == 25 h) is STALE — the window is
        // `now − last_attempt >= interval`.
        assert!(should_fetch_at_startup(
            true,
            Some(NOW_MS - INTERVAL_MS),
            NOW_MS
        ));
        assert!(should_fetch_at_startup(
            true,
            Some(NOW_MS - INTERVAL_MS - 60_000),
            NOW_MS
        ));
        // A timestamp from the future (clock skew) reads as fresh.
        assert!(!should_fetch_at_startup(
            true,
            Some(NOW_MS + 3_600_000),
            NOW_MS
        ));
    }

    // ── next_retry_deadline (the in-run timer derivation) ──

    #[test]
    fn retry_deadline_is_none_without_a_recorded_attempt() {
        // Nothing to derive from — the startup gate fetches instead.
        assert_eq!(next_retry_deadline(None, Instant::now(), NOW_MS), None);
    }

    #[test]
    fn retry_deadline_is_remaining_time_after_a_fresh_attempt() {
        // A fresh attempt arms the timer for the REMAINING time, not a full
        // interval — so a daemon that restarts 1 h into the cooldown waits
        // 24 more hours, preserving the original wall-clock deadline.
        let now = Instant::now();
        let deadline = next_retry_deadline(Some(NOW_MS - 3_600_000), now, NOW_MS)
            .expect("a fresh attempt yields a deadline");
        let expected = Duration::from_millis(INTERVAL_MS - 3_600_000);
        assert_eq!(deadline.duration_since(now), expected);
    }

    #[test]
    fn retry_deadline_saturates_at_now_when_already_due() {
        // The deadline has already passed (stale but the gate somehow skipped
        // the fetch): saturate to `now` so the next loop iteration fires the
        // refresh immediately instead of waiting.
        let now = Instant::now();
        let deadline = next_retry_deadline(Some(NOW_MS - INTERVAL_MS - 60_000), now, NOW_MS)
            .expect("a stale attempt still yields a deadline");
        assert_eq!(deadline, now);
    }

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

    #[test]
    fn fold_refresh_nows_folds_queued_bursts() {
        // A burst of /refresh-models must fold into ONE refresh: the force
        // flags are OR-ed and every reply sender is kept, so no requester is
        // left hanging and the maintenance thread fetches at most once.
        let (tx, rx) = crossbeam_channel::unbounded::<MaintenanceEvent>();
        let (reply_a, _ra) = mpsc::channel();
        let (reply_b, _rb) = mpsc::channel();
        let (reply_c, _rc) = mpsc::channel();

        // Queue two more refresh requests behind the first (force only on the
        // last one — the fold must OR it in).
        tx.send(MaintenanceEvent::RefreshNow {
            force: false,
            reply: reply_b,
        })
        .unwrap();
        tx.send(MaintenanceEvent::RefreshNow {
            force: true,
            reply: reply_c,
        })
        .unwrap();

        let (force, replies) = fold_refresh_nows(&rx, false, reply_a);
        assert!(
            force,
            "a --force anywhere in the burst must force the fetch"
        );
        assert_eq!(replies.len(), 3, "every requester's reply sender is kept");
        // Each requester keeps its OWN force flag so the daemon can
        // individualize reply statuses (plain requesters in a forced burst
        // are reported Updated, not Forced).
        assert!(!replies[0].force);
        assert!(!replies[1].force);
        assert!(replies[2].force, "the --force requester keeps its flag");
        // The channel is drained by the fold.
        assert!(rx.try_recv().is_err(), "the burst is fully drained");
    }

    #[test]
    fn fold_refresh_nows_keeps_first_reply_when_queue_empty() {
        let (_tx, rx) = crossbeam_channel::unbounded::<MaintenanceEvent>();
        let (reply, _r) = mpsc::channel();
        let (force, replies) = fold_refresh_nows(&rx, true, reply);
        assert!(force);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].force, "the first requester's flag is preserved");
    }

    #[test]
    fn reload_user_overlay_fingerprint_gates_the_daemon_command() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = dir.path().join("models-overlay.toml");
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let mut state = maintenance_state();

        // No file yet → nothing applied, nothing sent.
        reload_user_overlay(&daemon_tx, &mut state, &overlay);
        assert!(daemon_rx.try_recv().is_err(), "absent file sends nothing");
        assert!(state.last_applied_user_overlay.is_none());

        // Creating the file changes the fingerprint → CatalogBaseChanged with
        // the fresh contents.
        std::fs::write(&overlay, "[provider.acme]\nbase_url = \"x\"\n").unwrap();
        reload_user_overlay(&daemon_tx, &mut state, &overlay);
        match daemon_rx.try_recv().unwrap() {
            DaemonCommand::CatalogBaseChanged {
                user_overlay,
                persist,
                reply,
                ..
            } => {
                assert_eq!(
                    user_overlay.as_deref(),
                    Some("[provider.acme]\nbase_url = \"x\"\n")
                );
                assert!(!persist, "an overlay reload must not persist the cache");
                assert!(reply.is_empty());
            }
            other => panic!(
                "expected CatalogBaseChanged, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // An unchanged file (editor save storm) → fingerprint gate: nothing.
        reload_user_overlay(&daemon_tx, &mut state, &overlay);
        assert!(
            daemon_rx.try_recv().is_err(),
            "unchanged contents must not trigger a reload"
        );

        // Editing the file again → a new command with the new contents.
        std::fs::write(&overlay, "[provider.acme]\nbase_url = \"y\"\n").unwrap();
        reload_user_overlay(&daemon_tx, &mut state, &overlay);
        match daemon_rx.try_recv().unwrap() {
            DaemonCommand::CatalogBaseChanged { user_overlay, .. } => {
                assert_eq!(
                    user_overlay.as_deref(),
                    Some("[provider.acme]\nbase_url = \"y\"\n")
                );
            }
            other => panic!(
                "expected CatalogBaseChanged, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // Deleting the file → explicit None so the daemon falls back to
        // bundled-only.
        std::fs::remove_file(&overlay).unwrap();
        reload_user_overlay(&daemon_tx, &mut state, &overlay);
        match daemon_rx.try_recv().unwrap() {
            DaemonCommand::CatalogBaseChanged { user_overlay, .. } => {
                assert_eq!(
                    user_overlay, None,
                    "a deleted overlay falls back to bundled-only"
                );
            }
            other => panic!(
                "expected CatalogBaseChanged, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn ensure_runtime_dirs_creates_overlay_and_data_dirs() {
        // The dir-creation fix: the config dir (overlay parent) must exist
        // before the notify watch is installed, so `ensure_runtime_dirs`
        // creates it (and the cache data dir) up front — even on a fresh
        // system where neither exists yet.
        let dir = tempfile::tempdir().unwrap();
        let paths = CatalogPaths {
            bin: dir.path().join("data/choreographr/catalog.bin"),
            overlay: dir.path().join("config/choreographr/models-overlay.toml"),
        };

        ensure_runtime_dirs(&paths);
        assert!(
            paths.bin.parent().unwrap().is_dir(),
            "data dir must exist after ensure_runtime_dirs"
        );
        assert!(
            paths.overlay.parent().unwrap().is_dir(),
            "config dir must exist after ensure_runtime_dirs"
        );

        // Idempotent: a second pass must not error.
        ensure_runtime_dirs(&paths);
    }

    #[test]
    fn ensure_runtime_dirs_tolerates_empty_paths() {
        // A HOME-less fallback (CatalogPaths::default()) has empty paths —
        // no parent to create, no panic.
        ensure_runtime_dirs(&CatalogPaths::default());
    }
}
