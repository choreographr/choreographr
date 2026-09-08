//! Construction of a [`DaemonState`] outside the CLI binary.
//!
//! Step 3 of the embedded-daemon refactor: the embedder (a GUI process) must
//! be able to open daemon state — database, accounts, tool registry — without
//! the CLI's `anyhow` chain or its global-path assumptions. Everything here is
//! the exact code the CLI used to inline (see `cli.rs`), lifted behind
//! [`OpenOptions`] so the paths and the tool policy become explicit
//! parameters. The CLI delegates to this constructor with the standard paths
//! and [`ToolPolicy::Full`], so its behavior is unchanged.

use crate::accounts::AccountManager;
use crate::catalog::CatalogPaths;
use crate::daemon::DaemonState;
use crate::db;
use crate::mcp::McpManager;
use crate::tools::{ToolPolicy, ToolRegistry};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use tracing::{info, warn};

/// Parameters for [`DaemonState::open`]. Explicit paths (not global helpers)
/// so an embedder can point the daemon at its own sandbox (app-container
/// directories, test tempdirs) without environment-variable overrides.
pub struct OpenOptions {
    /// Database file path (`state.redb`). Created and migrated if needed.
    pub db_path: PathBuf,
    /// `accounts.toml` path. Missing file = empty account set (same tolerant
    /// policy as the CLI).
    pub accounts_path: PathBuf,
    /// Catalog cache bin + user overlay locations (see `crate::catalog`).
    pub catalog_paths: CatalogPaths,
    /// Which tool groups to register (see [`ToolPolicy`]).
    pub tool_policy: ToolPolicy,
    /// Tool-loop iteration limit (0 = unlimited). The CLI resolves this from
    /// env/config; an embedder decides it directly.
    pub max_turns: u32,
}

impl DaemonState {
    /// Open daemon state from explicit paths: database (with the same
    /// Windows-safe open → version → drop → backup → reopen → migrate
    /// sequence the CLI ran), tombstone purge, session index load, accounts,
    /// and the tool registry under `tool_policy`.
    ///
    /// Returns `io::Result` (not anyhow) so an embedder without the CLI's
    /// error-reporting stack can consume it directly; every failure carries
    /// its stage in the message.
    pub fn open(opts: OpenOptions) -> io::Result<Self> {
        // Windows-safe startup sequence (verbatim from the CLI): redb holds a
        // whole-file exclusive lock on the database for as long as a
        // `redb::Database` handle is open, and on Windows that lock blocks
        // even same-process reads of the file (os error 33). The pre-migration
        // backup therefore must be taken BEFORE the file is opened/locked.
        // Sequence: open → read the schema version → drop the handle
        // (releasing the lock) → if a real migration is pending
        // (migration_backup_version returns Some exactly when run_migrations_at
        // would back up), copy the backup from the unlocked file → reopen →
        // migrate. The in-runner backup step is a no-op then (skip-if-exists).
        let db = Arc::new({
            let opened = db::open_db_at(&opts.db_path)?;
            match db::migration_backup_version(&opened)? {
                Some(version) => {
                    // Release redb's whole-file exclusive lock before the
                    // copy — reading the open file is exactly what fails on
                    // Windows.
                    drop(opened);
                    db::backup_database(&opts.db_path, version)?;
                    db::open_db_at(&opts.db_path)?
                }
                // No pending real migration (fresh DB, up-to-date,
                // newer-version refusal, or unversioned): keep the original
                // handle.
                None => opened,
            }
        });

        // Bring the database up to the current schema version before any table
        // access (same invariant as the CLI; see db::run_migrations).
        db::run_migrations_at(&db, &opts.db_path)?;

        // Purge any sessions that were deleted while their still-shutting-down
        // thread was alive and re-created the record before the daemon
        // crashed. Runs before the session index is loaded so the record never
        // surfaces. Warn-and-continue, exactly as the CLI did.
        match db::purge_tombstoned_sessions(&db) {
            Ok(n) if n > 0 => warn!(
                purged = n,
                "purged records left behind by interrupted session deletions"
            ),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "failed to purge tombstoned sessions; continuing"),
        }

        let mut session_metadata = HashMap::new();
        match db::read_all_sessions(&db) {
            Ok(sessions) => {
                for (id, record) in sessions {
                    session_metadata.insert(id, record.into());
                }
            }
            Err(e) => {
                warn!("failed to load sessions from database: {e}");
            }
        }
        info!(
            count = session_metadata.len(),
            "loaded sessions from database"
        );

        // Load accounts (may be empty — unlock will reload them). Missing
        // config path/file = empty manager, same tolerant policy as the CLI.
        let accounts = AccountManager::load(&opts.accounts_path).unwrap_or_else(|e| {
            warn!(error = %e, "failed to load accounts; starting empty");
            AccountManager::empty()
        });

        // Build the tool registry under the requested policy, and initialize
        // MCP servers only under `Full` — a `Mobile` daemon must not spawn
        // subprocesses at all, so the manager stays empty (registration-time
        // filtering; see ToolPolicy).
        let mut tool_registry = ToolRegistry::new_for_policy(opts.tool_policy);
        let mcp_manager = if opts.tool_policy == ToolPolicy::Full {
            McpManager::from_config(&mut tool_registry)
        } else {
            info!("tool policy Mobile: MCP servers not spawned");
            McpManager::empty()
        };
        let tool_registry = tool_registry.build_for_policy(opts.tool_policy);

        Ok(DaemonState {
            // Placeholder command channel: `start_daemon_core` overwrites
            // `daemon_tx` with the real command-loop channel before any
            // consumer exists, so the dropped receiver here never matters.
            daemon_tx: mpsc::channel().0,
            // Derive the next session ID from the highest existing record so a
            // fresh daemon never collides with a persisted session.
            next_session_id: session_metadata
                .keys()
                .max()
                .copied()
                .map(|m| m + 1)
                .unwrap_or(1),
            max_turns: opts.max_turns,
            active_sessions: HashMap::new(),
            session_metadata,
            deleted_sessions: HashSet::new(),
            children: HashMap::new(),
            accounts,
            providers: HashMap::new(),
            credentials: HashMap::new(),
            x_credentials: None,
            // The daemon starts locked: credentials are only decrypted into
            // memory once a client presents the valid unlock key.
            locked: true,
            db,
            tool_registry,
            summary_subscribers: HashMap::new(),
            client_writers: HashMap::new(),
            activity_subscribers: HashMap::new(),
            client_subscribed_sessions: HashMap::new(),
            // One daemon-wide lag counter shared by every connection's sink
            // and every session thread (see `broadcast::SubscriberSink`).
            global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            lag_limits: crate::broadcast::LagLimits::default(),
            model_cache: HashMap::new(),
            model_prefetch_in_flight: HashSet::new(),
            mcp_manager,
            // Populated by `start_daemon_core`, which spawns the maintenance
            // thread (it needs the real command-loop channel).
            maintenance_tx: None,
            // Installed by `start_daemon_core` from its `CoreOptions.acl`
            // (the command loop needs the same Arc the accept paths read).
            acl: None,
            catalog_paths: opts.catalog_paths,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-policy state opens on explicit temp paths: DB migrated, session
    /// index empty but valid, and the shell tools present (the policy default
    /// must not restrict the CLI-like case).
    #[test]
    fn open_full_policy_on_temp_paths() {
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::open(OpenOptions {
            db_path: dir.path().join("state.redb"),
            accounts_path: dir.path().join("accounts.toml"),
            catalog_paths: CatalogPaths {
                bin: dir.path().join("catalog.bin"),
                overlay: dir.path().join("models-overlay.toml"),
            },
            tool_policy: ToolPolicy::Full,
            max_turns: 0,
        })
        .unwrap();

        assert_eq!(state.next_session_id, 1);
        assert!(state.locked, "a fresh daemon starts locked");
        let active: HashSet<String> = ["shell".into()].into_iter().collect();
        let defs = state.tool_registry.available_definitions(&active);
        assert!(
            defs.iter().any(|d| d.function.name == "sh"),
            "Full policy must register the shell tools"
        );
    }

    /// The Mobile policy filters at registration time: no `sh`, no `exec`, no
    /// `run_riscv` — and no MCP manager contents. Unregistered tools cannot
    /// be activated later by any client message.
    #[test]
    fn open_mobile_policy_omits_shell_and_vm() {
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::open(OpenOptions {
            db_path: dir.path().join("state.redb"),
            accounts_path: dir.path().join("accounts.toml"),
            catalog_paths: CatalogPaths::default(),
            tool_policy: ToolPolicy::Mobile,
            max_turns: 0,
        })
        .unwrap();

        let all: HashSet<String> = state
            .tool_registry
            .group_names()
            .into_iter()
            .chain(["core".to_string()])
            .collect();
        let defs = state.tool_registry.available_definitions(&all);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        for absent in ["sh", "exec", "run_riscv"] {
            assert!(
                !names.contains(&absent),
                "Mobile policy must not register {absent}: {names:?}"
            );
        }
    }
}
