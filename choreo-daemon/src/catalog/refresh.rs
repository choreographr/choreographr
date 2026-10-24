//! The models.dev refresh state machine: one conditional GET, whatever its
//! outcome, folded into a single command for the daemon command loop.
//!
//! The fetch is injected ([`run_refresh_impl`]'s `fetch` parameter) so the
//! outcome→state machine is unit-testable without a network round trip;
//! [`run_refresh`] is the production wiring to the real HTTP GET.

use std::sync::mpsc;
use std::time::Instant;

use choreo_ai_protocols::{RefreshError, RefreshOutcome, fetch_modelsdev, normalize_modelsdev};
use choreo_proto::RefreshStatus;
use crossbeam_channel::Sender;
use tracing::{info, warn};

use super::MaintenanceEvent;
use super::MaintenanceState;
use super::pacing::REFRESH_ATTEMPT_INTERVAL;
use crate::daemon::DaemonCommand;

/// Reply payload for a `/refresh-models` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshReport {
    /// Number of providers in the refreshed catalog.
    pub providers: usize,
    /// Total number of models across those providers.
    pub models: usize,
    /// How the fetch resolved for the requester (updated / forced / up-to-date).
    pub status: RefreshStatus,
}

/// One `/refresh-models` requester folded into a coalesced batch: its reply
/// channel plus whether IT asked for a forced fetch. The batch performs ONE
/// shared fetch, forced if ANY requester asked (`run_refresh`'s `force` is
/// the OR), but each requester's reply status reflects its own flag — a
/// plain request folded into a forced burst is reported `Updated`, not
/// `Forced`, matching what it actually asked for.
#[derive(Debug)]
pub struct RefreshRequester {
    /// Whether THIS requester asked for a forced fetch (bypassing the cooldown).
    pub force: bool,
    /// One-shot reply channel this requester's coalesced-batch result is sent on.
    pub tx: mpsc::Sender<Result<RefreshReport, String>>,
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
pub(super) fn run_refresh(
    daemon_tx: &Sender<DaemonCommand>,
    state: &mut MaintenanceState,
    force: bool,
    reply: Vec<RefreshRequester>,
) {
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
pub(super) fn run_refresh_impl<F>(
    daemon_tx: &Sender<DaemonCommand>,
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
            state.etag = etag;
            // Even a successful fetch arms the next revalidation: without
            // this the catalog would go permanently stale after the first 200
            // (the event loop only refreshes when next_retry_at is set), and
            // the etag makes the next conditional GET cheap.
            state.next_retry_at = Some(Instant::now() + REFRESH_ATTEMPT_INTERVAL);
            // The fetched base is moved into the command (which persists it)
            // and not stored on this thread.
            let _ = daemon_tx.send(DaemonCommand::CatalogBaseChanged {
                base: new_base,
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
pub(super) fn fold_refresh_nows(
    rx: &crossbeam_channel::Receiver<MaintenanceEvent>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fixtures::tiny_base;
    use crossbeam_channel::unbounded;

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

    /// A starting maintenance state for the state-machine tests: a cached
    /// etag, a recent (fresh) attempt timestamp, no revalidation pending.
    fn maintenance_state() -> MaintenanceState {
        MaintenanceState {
            etag: Some("\"v1\"".into()),
            last_attempt_ms: Some(1_700_000_000_000),
            last_applied_user_overlay: None,
            next_retry_at: None,
        }
    }

    #[test]
    fn run_refresh_fetched_arms_revalidation_and_sends_base_changed() {
        let (daemon_tx, daemon_rx) = unbounded::<DaemonCommand>();
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

        // The new etag was adopted and the NEXT revalidation is armed — a
        // successful fetch must not stop the cadence (that would leave the
        // catalog permanently stale after the first 200). The base itself is
        // NOT retained on the thread; it travels in the command below.
        assert_eq!(state.etag.as_deref(), Some("\"v2\""));
        assert!(
            state.next_retry_at.is_some(),
            "a successful fetch must schedule the next revalidation"
        );

        // The daemon loop gets the swap command with persist set and the
        // requester's own (non-forced) flag, so the daemon can individualize
        // the reply status.
        match daemon_rx.recv().unwrap() {
            DaemonCommand::CatalogBaseChanged {
                base,
                persist,
                reply,
                ..
            } => {
                assert_eq!(base.len(), 1, "snapshot normalizes to one provider");
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
        let (daemon_tx, daemon_rx) = unbounded::<DaemonCommand>();
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
        let (daemon_tx, daemon_rx) = unbounded::<DaemonCommand>();
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
        let (daemon_tx, _daemon_rx) = unbounded::<DaemonCommand>();
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
            |_etag, _force| Err(RefreshError::Network("boom".into())),
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
        let (daemon_tx, daemon_rx) = unbounded::<DaemonCommand>();
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

    #[test]
    fn fold_refresh_nows_folds_queued_bursts() {
        // A burst of /refresh-models must fold into ONE refresh: the force
        // flags are OR-ed and every reply sender is kept, so no requester is
        // left hanging and the maintenance thread fetches at most once.
        let (tx, rx) = unbounded::<MaintenanceEvent>();
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
        let (_tx, rx) = unbounded::<MaintenanceEvent>();
        let (reply, _r) = mpsc::channel();
        let (force, replies) = fold_refresh_nows(&rx, true, reply);
        assert!(force);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].force, "the first requester's flag is preserved");
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
}
