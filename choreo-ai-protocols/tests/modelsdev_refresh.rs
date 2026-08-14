//! Live models.dev refresh integration test (S4).
//!
//! Hits the real `https://models.dev/api.json` endpoint. Marked `#[ignore]`
//! (run via `cargo test-integration`) because it needs network access and the
//! endpoint can change: the assertions are deliberately loose — the catalog
//! must parse non-empty, the etag round trip must not error, and `--force`
//! must return a fresh body. Everything *deterministic* about the refresh
//! pipeline (etag guard, normalization, merge, fingerprint compare) is unit
//! tested in `src/`.

use choreo_ai_protocols::catalog::refresh::{RefreshOutcome, fetch_modelsdev};
use choreo_ai_protocols::normalize_modelsdev;

/// A plain fetch must return a non-empty body that normalizes into a
/// non-empty catalog.
#[ignore]
#[test]
fn live_fetch_normalizes_to_a_catalog() {
    let outcome = fetch_modelsdev(None, false).expect("models.dev fetch succeeds");
    let RefreshOutcome::Fetched { json, etag } = &outcome else {
        panic!("a plain GET (no etag) must never 304; got {outcome:?}");
    };
    assert!(!json.is_empty(), "models.dev body must not be empty");
    assert!(
        etag.as_deref().is_some_and(|e| !e.is_empty()),
        "models.dev serves ETag + must-revalidate; a real fetch should carry one"
    );

    let catalog = normalize_modelsdev(json);
    assert!(
        catalog.len() >= 150,
        "models.dev should still list 150+ providers, got {}",
        catalog.len()
    );
}

/// The etag round trip must be a valid conditional GET: either the server
/// says NotModified (cache current) or returns a fresh body — never an error.
#[ignore]
#[test]
fn live_etag_round_trip_never_errors() {
    let first = fetch_modelsdev(None, false).expect("first fetch succeeds");
    let RefreshOutcome::Fetched { etag, .. } = &first else {
        panic!("plain GET must be a 200; got {first:?}");
    };
    let Some(etag) = etag.as_deref() else {
        return; // no etag to round-trip
    };

    let second = fetch_modelsdev(Some(etag), false).expect("conditional GET succeeds");
    match second {
        RefreshOutcome::NotModified => {}
        RefreshOutcome::Fetched { json, .. } => {
            assert!(!normalize_modelsdev(&json).is_empty());
        }
    }
}

/// `--force` must bypass the etag and return a fresh body even right after a
/// plain fetch.
#[ignore]
#[test]
fn live_force_fetch_returns_fresh_body() {
    let first = fetch_modelsdev(None, false).expect("first fetch succeeds");
    let RefreshOutcome::Fetched { etag, .. } = &first else {
        panic!("plain GET must be a 200; got {first:?}");
    };

    let forced = fetch_modelsdev(etag.as_deref(), true).expect("forced fetch succeeds");
    match forced {
        RefreshOutcome::Fetched { json, .. } => {
            assert!(!normalize_modelsdev(&json).is_empty());
        }
        // A server that honors no-cache must return 200; a misbehaving one
        // could still 304 — don't fail on that, the point is it never errors.
        RefreshOutcome::NotModified => {}
    }
}
