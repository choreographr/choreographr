//! The user overlay (`models-overlay.toml`): reading it, the change
//! fingerprint that collapses editor save-event storms, and the reload path
//! that hands an edit to the daemon command loop.
//!
//! A reload carries **only the overlay**. The base it is merged onto is the
//! command loop's business: that loop is the catalog's single writer and the
//! cache file's only writer, so it is also the only place where re-reading the
//! base is guaranteed to see the base it applied (see [`super::cache::
//! load_persisted_base`]). Keeping the base out of this thread is what makes
//! the reload free of the ordering hazard a carried base would have: a carried
//! base is whatever the thread last *sent*, which a landing fetch may already
//! have superseded.

use std::io;
use std::path::Path;

use crossbeam_channel::Sender;
use tracing::{debug, warn};

use super::MaintenanceState;
use crate::daemon::DaemonCommand;

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

/// Re-read the user overlay and, if its contents changed since the
/// last-applied value (the fingerprint gate), hand the new value to the daemon
/// command loop so it re-merges and swaps. Shared by the notify watcher and the
/// `/refresh-models` path, so the overlay reload policy lives in exactly one
/// place. A deleted file sends an explicit `None` so the daemon falls back to
/// bundled-only; an unreadable-but-present file warns and keeps the
/// last-applied value rather than churn on a transient read error.
pub(super) fn reload_user_overlay(
    daemon_tx: &Sender<DaemonCommand>,
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
                state.last_applied_user_overlay.clone_from(&contents);
                // No base travels with the event: the command loop re-merges
                // the base it is running (`catalog.bin`, or its in-memory
                // override after a failed persist), so an edit landing right
                // after a fetch cannot revert that fetch's base.
                let _ = daemon_tx.send(DaemonCommand::CatalogOverlayChanged {
                    user_overlay: contents,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    /// A starting maintenance state for the overlay tests: no etag, no recorded
    /// attempt, no last-applied overlay, no pending revalidation.
    fn maintenance_state() -> MaintenanceState {
        MaintenanceState {
            etag: None,
            last_attempt_ms: None,
            last_applied_user_overlay: None,
            next_retry_at: None,
        }
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
    fn reload_user_overlay_fingerprint_gates_the_daemon_command() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = dir.path().join("models-overlay.toml");
        let (daemon_tx, daemon_rx) = unbounded::<DaemonCommand>();
        let mut state = maintenance_state();

        // No file yet → nothing applied, nothing sent.
        reload_user_overlay(&daemon_tx, &mut state, &overlay);
        assert!(daemon_rx.try_recv().is_err(), "absent file sends nothing");
        assert!(state.last_applied_user_overlay.is_none());

        // Creating the file changes the fingerprint → the overlay travels to
        // the command loop, which re-merges it onto the base it is running.
        std::fs::write(&overlay, "[provider.acme]\nbase_url = \"x\"\n").unwrap();
        reload_user_overlay(&daemon_tx, &mut state, &overlay);
        match daemon_rx.try_recv().unwrap() {
            DaemonCommand::CatalogOverlayChanged { user_overlay } => {
                assert_eq!(
                    user_overlay.as_deref(),
                    Some("[provider.acme]\nbase_url = \"x\"\n"),
                    "the edit is carried verbatim"
                );
            }
            other => panic!(
                "expected CatalogOverlayChanged, got {:?}",
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
            DaemonCommand::CatalogOverlayChanged { user_overlay } => {
                assert_eq!(
                    user_overlay.as_deref(),
                    Some("[provider.acme]\nbase_url = \"y\"\n")
                );
            }
            other => panic!(
                "expected CatalogOverlayChanged, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // Deleting the file → explicit None so the daemon falls back to
        // bundled-only.
        std::fs::remove_file(&overlay).unwrap();
        reload_user_overlay(&daemon_tx, &mut state, &overlay);
        match daemon_rx.try_recv().unwrap() {
            DaemonCommand::CatalogOverlayChanged { user_overlay } => {
                assert_eq!(
                    user_overlay, None,
                    "a deleted overlay falls back to bundled-only"
                );
            }
            other => panic!(
                "expected CatalogOverlayChanged, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn reload_user_overlay_keeps_the_last_applied_value_on_a_read_error() {
        // An unreadable-but-present file must NOT send a reload (a transient
        // read error must not churn the catalog down to bundled-only).
        let dir = tempfile::tempdir().unwrap();
        let overlay = dir.path().join("models-overlay.toml");
        // A directory at the overlay's path makes the read fail with EISDIR —
        // present, but unreadable as a file.
        std::fs::create_dir(&overlay).unwrap();
        let (daemon_tx, daemon_rx) = unbounded::<DaemonCommand>();
        let mut state = maintenance_state();
        state.last_applied_user_overlay = Some("[provider.acme]\n".into());

        reload_user_overlay(&daemon_tx, &mut state, &overlay);

        assert!(
            daemon_rx.try_recv().is_err(),
            "a read error must not trigger a reload"
        );
        assert_eq!(
            state.last_applied_user_overlay.as_deref(),
            Some("[provider.acme]\n"),
            "the last-applied value survives a read error"
        );
    }
}
