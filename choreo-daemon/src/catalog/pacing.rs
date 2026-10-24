//! Refresh pacing (S4): how often the maintenance thread may revalidate the
//! catalog against models.dev, and the two wall-clock↔monotonic conversions
//! that let that cadence survive restarts.
//!
//! Everything here is either a pure function of an injected clock (`now_ms`,
//! `now: Instant`) or the single DB write that anchors the cooldown, so the
//! policy is unit-testable without any time-based sleeps.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::warn;

use super::MaintenanceState;
use crate::db::set_catalog_last_attempt_ms;

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
pub(super) const REFRESH_ATTEMPT_INTERVAL: Duration = Duration::from_hours(25);

/// Whether the startup path should fetch immediately: no valid cache, no
/// recorded attempt (first run / upgrade from a build without the key), or a
/// stale attempt (`now − last_attempt ≥ REFRESH_ATTEMPT_INTERVAL`). The gate
/// is deliberately conservative — anything unknown fetches — because the
/// cost of a wrong "fetch" is one polite conditional GET, while the cost of
/// a wrong "skip" is an arbitrarily stale cache.
///
/// Pure function of injected wall-clock `now_ms` so it is unit-testable
/// without any time-based logic.
pub(super) fn should_fetch_at_startup(
    cache_valid: bool,
    last_attempt_ms: Option<u64>,
    now_ms: u64,
) -> bool {
    if !cache_valid {
        return true;
    }
    match last_attempt_ms {
        None => true,
        // u128→u64 interval millis: the constant is a Duration well under u64::MAX.
        Some(at) => {
            #[expect(clippy::cast_possible_truncation)]
            let interval_ms = REFRESH_ATTEMPT_INTERVAL.as_millis() as u64;
            now_ms.saturating_sub(at) >= interval_ms
        }
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
pub(super) fn next_retry_deadline(
    last_attempt_ms: Option<u64>,
    now: Instant,
    now_ms: u64,
) -> Option<Instant> {
    let at = last_attempt_ms?;
    let elapsed = Duration::from_millis(now_ms.saturating_sub(at));
    let remaining = REFRESH_ATTEMPT_INTERVAL.saturating_sub(elapsed);
    Some(now + remaining)
}

/// Wall-clock epoch millis (`u64`). The cooldown anchor must be wall time so
/// it survives restarts; the in-run deadline is derived from it at startup
/// (see [`next_retry_deadline`]). Falls back to 0 (ancient → stale → fetch)
/// if the clock is before the Unix epoch, which never happens in practice.
pub(super) fn wall_now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        // u128→u64 epoch millis: fits u64 for the next ~292 million years.
        #[expect(clippy::cast_possible_truncation)]
        {
            d.as_millis() as u64
        }
    })
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
pub(super) fn record_attempt(db: &redb::Database, state: &mut MaintenanceState) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::get_catalog_last_attempt_ms;

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
    fn record_attempt_persists_the_anchor_and_updates_the_state() {
        // The cooldown anchor is crash-safe only if it reaches the DB before
        // the fetch runs, so pin both halves of the write.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("pacing.redb")).unwrap();
        let mut state = MaintenanceState {
            etag: None,
            last_attempt_ms: None,
            last_applied_user_overlay: None,
            next_retry_at: None,
        };

        record_attempt(&db, &mut state);

        let recorded = state.last_attempt_ms.expect("in-memory anchor is set");
        assert_eq!(
            get_catalog_last_attempt_ms(&db).unwrap(),
            Some(recorded),
            "the DB anchor must match the in-memory one"
        );
    }
}
