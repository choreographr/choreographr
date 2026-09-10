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
use crate::tools::ios_bridge::IosToolBridge;
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
    /// Optional bridge to the host's platform-native tools (clipboard,
    /// open_url, notify on iOS). When `Some`, the `ios` tool group is
    /// registered and PROTECTED (always active, unloadable by no one); the
    /// bridge's PRESENCE is the gate — a desktop embedder passes `None` and
    /// the group simply never exists. No `cfg` here on purpose: that keeps
    /// the registration testable on every platform.
    pub platform_tool_bridge: Option<Arc<dyn IosToolBridge>>,
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
        // Register the platform tools (if a bridge was supplied) BEFORE
        // `build_for_policy`: `register_platform_tools` needs `&mut self`,
        // and `build_for_policy` consumes the registry into the shared `Arc`
        // (its `Arc::new_cyclic` closure keeps the `protected_groups` field
        // alive — it moves with the value).
        if let Some(bridge) = opts.platform_tool_bridge {
            tool_registry.register_platform_tools(bridge);
        }
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
            // std `mpsc` deliberately: this must match the pre-existing
            // `DaemonState::daemon_tx` field type (the convention converts
            // existing std channels opportunistically only).
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
            // ONE registry for the whole daemon lifetime: every provider
            // client and the cancel/suspend handlers share this Arc, so
            // `shutdown_all` covers all provider sockets process-wide.
            socket_registry: Arc::new(choreo_ai_protocols::SocketRegistry::new()),
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

    /// The four platform tools (shared by the registration test's two
    /// assertion loops).
    const IOS_TOOLS: [&str; 4] = ["clipboard_write", "clipboard_read", "open_url", "notify"];

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
            platform_tool_bridge: None,
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
            platform_tool_bridge: None,
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

    /// Supplying a `platform_tool_bridge` registers the four iOS tools as a
    /// PROTECTED group even on a non-iOS build: the registration is
    /// bridge-gated, not platform-gated. They are available WITHOUT "ios"
    /// being in the caller-supplied active set (the protected-groups union
    /// rule), excluded from `group_names()` (no load/unload schema slot),
    /// and an unload_tools("ios") style request is impossible through the
    /// schema — the group is simply always active.
    #[test]
    fn open_with_platform_bridge_registers_protected_ios_group() {
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::open(OpenOptions {
            db_path: dir.path().join("state.redb"),
            accounts_path: dir.path().join("accounts.toml"),
            catalog_paths: CatalogPaths::default(),
            tool_policy: ToolPolicy::Mobile,
            max_turns: 0,
            platform_tool_bridge: Some(std::sync::Arc::new(
                crate::tools::ios_bridge::MockBridge::default(),
            )),
        })
        .unwrap();

        // Available WITHOUT "ios" in the caller-supplied active set.
        let active: HashSet<String> = HashSet::new();
        let defs = state.tool_registry.available_definitions(&active);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        for tool in IOS_TOOLS {
            assert!(names.contains(&tool), "missing {tool}: {names:?}");
        }

        // "ios" is protected: excluded from the load/unload schema enum and
        // present in the protected set.
        assert!(
            !state.tool_registry.group_names().iter().any(|g| g == "ios"),
            "protected groups must not appear in group_names()"
        );
        assert!(
            state.tool_registry.protected_groups().contains("ios"),
            "ios must be marked protected"
        );
        // Direct-only callers (exfiltration-chain mitigation) — checked on
        // the four ios tools specifically (the union also surfaces core
        // tools, which keep their own caller policy).
        let resp_defs = state
            .tool_registry
            .available_definitions_for_responses(&active);
        for def in resp_defs
            .iter()
            .filter(|d| IOS_TOOLS.contains(&d.function.name.as_str()))
        {
            assert_eq!(
                def.function.allowed_callers.as_deref(),
                Some(&[choreo_ai_protocols::openai::AllowedCaller::Direct][..]),
                "{} must be Direct-only",
                def.function.name
            );
        }
    }
}
