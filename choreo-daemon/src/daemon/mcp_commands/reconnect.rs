//! Bringing one MCP server up on the command loop's behalf: the background
//! daemon-tier connect and its reply parking, the post-unlock / post-sign-in
//! resync, and the bounded in-place rebuild of a project/per-session server.
//!
//! A child module of `crate::daemon::mcp_commands`, so the `impl DaemonState`
//! block reaches the state's private fields directly, like its parent.

#[cfg(feature = "mcp")]
use crate::daemon::DaemonCommand;
use crate::daemon::DaemonState;
#[cfg(feature = "mcp")]
use tracing::debug;
use tracing::{info, warn};

impl DaemonState {
    /// Reconnect one MCP server and refresh the catalogue, reporting the
    /// outcome to the requester.
    ///
    /// A daemon-tier SHARED server reconnects on a background worker (see
    /// [`Self::spawn_mcp_connect`]) so the command loop is never blocked on a
    /// handshake. The requester's reply is parked in `pending_mcp_reconnects`
    /// and satisfied by [`Self::handle_mcp_connected`] once the connect lands or
    /// fails, preserving the request/reply contract the connection thread is
    /// waiting on.
    ///
    /// Every other slug — a project server, a daemon `shared = false` server, or
    /// one that matches nothing — has no daemon-tier shared config to connect:
    /// it is rebuilt in place (bounded, and only on this explicit user action)
    /// and the sessions holding it are re-resolved so the new tools reach them.
    /// Routing a `shared = false` server through the background path would
    /// connect a SECOND, daemon-level connection for it instead of rebuilding the
    /// per-session ones, registering duplicate tools.
    pub(crate) fn handle_mcp_reconnect(
        &mut self,
        slug: &str,
        reply: &std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        if self.mcp_manager.is_shared_configured(slug) {
            self.pending_mcp_reconnects
                .entry(slug.to_string())
                .or_default()
                .push(reply.clone());
            self.spawn_mcp_connect(slug);
            return;
        }
        let result = self.mcp_manager.reconnect(slug);
        if result.is_ok() {
            let affected = self.mcp_manager.sessions_for_slug(slug);
            self.refresh_session_overlays(affected);
        }
        match &result {
            Ok(()) => info!(server = %slug, "MCP server reconnected; tool catalogue refreshed"),
            Err(e) => warn!(server = %slug, error = %e, "MCP reconnect failed"),
        }
        let _ = reply.send(result);
    }

    /// Spawn a background connect for the daemon-tier SHARED server `slug`.
    ///
    /// No-op when `slug` is not a configured shared server (see
    /// [`crate::mcp::McpManager::connect_inputs`]) or a connect is already in
    /// flight (the `in_flight` guard coalesces a duplicate unlock, a sign-in
    /// racing the startup sweep, or a repeated `/mcp reconnect`). The worker
    /// connects and lists the server, then hands the result back to the command
    /// loop as [`DaemonCommand::McpConnected`] — the loop is the sole writer of
    /// the pool and the tool catalogue, so the worker never touches either.
    ///
    /// [`DaemonCommand::McpConnected`]: crate::daemon::DaemonCommand::McpConnected
    #[cfg(feature = "mcp")]
    pub(crate) fn spawn_mcp_connect(&mut self, slug: &str) -> bool {
        let Some((config, list_changes)) = self.mcp_manager.connect_inputs(slug) else {
            debug!(
                server = %slug,
                "not a daemon-tier shared server; nothing to connect in the background"
            );
            return false;
        };
        if !self.mcp_manager.mark_in_flight(slug) {
            debug!(server = %slug, "MCP connect already in flight; not spawning a second");
            return false;
        }
        let tx = self.daemon_tx.clone();
        let worker_slug = slug.to_string();
        // A dedicated thread per connect: a cold `npx` handshake is multi-second
        // and must never occupy the command loop or the per-server worker that
        // spawned it. `connect_entry` bounds the handshake, so a wedged server
        // cannot pin this thread (its connect thread is abandoned and the
        // connection it eventually builds is dropped).
        let spawned = std::thread::Builder::new()
            .name(format!("mcp-connect-{slug}"))
            .spawn(move || {
                let outcome = crate::mcp::connect_entry(
                    &config,
                    Some(list_changes),
                    crate::mcp::BACKGROUND_CONNECT_BUDGET,
                );
                let _ = tx.send(DaemonCommand::McpConnected {
                    slug: worker_slug,
                    outcome,
                });
            });
        if let Err(e) = spawned {
            // The worker never started, so no `McpConnected` will arrive to
            // clear the in-flight marker or satisfy any waiter: do both here.
            warn!(server = %slug, error = %e, "failed to spawn MCP connect worker");
            self.mcp_manager.clear_in_flight(slug);
            self.mcp_manager
                .note_connect_failure(slug, format!("failed to spawn connect worker: {e}"));
            self.complete_pending_mcp_reconnect(slug);
            return false;
        }
        true
    }

    /// Without the `mcp` feature there are no servers to connect.
    #[cfg(not(feature = "mcp"))]
    pub(crate) fn spawn_mcp_connect(&mut self, _slug: &str) -> bool {
        false
    }

    /// Install (or record the failure of) a background connect's result, rebuild
    /// the tool catalogue from the cached sets, and satisfy any parked
    /// `/mcp reconnect` reply.
    ///
    /// Runs on the command loop, the sole writer of the tool catalogue: the
    /// swap is atomic and never tears a live load, so a request already holding
    /// the previous registry keeps using it while the next turn sees the server's
    /// tools.
    #[cfg(feature = "mcp")]
    pub(crate) fn handle_mcp_connected(
        &mut self,
        slug: &str,
        outcome: crate::mcp::McpConnectOutcome,
    ) {
        match outcome {
            Ok((server, tools)) => {
                // A server that came up needs no post-unlock retry.
                self.mcp_manager.take_resync_request(slug);
                if let Err(e) = self.mcp_manager.install_connected(slug, server, &tools) {
                    // The slug stopped being a configured daemon-tier shared
                    // server while the connect was in flight; the freshly
                    // connected `server` is dropped here (its dispatcher shut
                    // down). Recording it as a failure would describe a server
                    // the config no longer has, so only the in-flight marker is
                    // cleared.
                    self.mcp_manager.clear_in_flight(slug);
                    debug!(
                        server = %slug,
                        reason = %e,
                        "discarding MCP server connected after its config changed"
                    );
                } else {
                    self.rebuild_tool_catalogue_cached();
                    #[cfg(any(test, feature = "test-utils"))]
                    {
                        self.mcp_connects_completed = self.mcp_connects_completed.saturating_add(1);
                    }
                    let affected = self.mcp_manager.sessions_for_slug(slug);
                    self.refresh_session_overlays(affected);
                    info!(server = %slug, "MCP server connected; tool catalogue refreshed");
                }
                self.complete_pending_mcp_reconnect(slug);
            }
            Err(e) => {
                warn!(server = %slug, error = %e, "MCP background connect failed");
                self.mcp_manager.note_connect_failure(slug, e);
                // An unlock that landed while this connect was in flight could
                // not start its own retry (the in-flight guard skipped it). Now
                // that the credentials are live, retry ONCE so the OAuth server
                // actually comes up; any parked waiter sees the retry's result.
                if self.mcp_manager.take_resync_request(slug) && self.spawn_mcp_connect(slug) {
                    info!(server = %slug, "retrying MCP connect after unlock");
                    return;
                }
                // No retry ran (not requested, or the slug is no longer
                // connectable): satisfy the parked waiters with the failure so a
                // `/mcp reconnect` request never hangs forever.
                self.complete_pending_mcp_reconnect(slug);
            }
        }
    }

    /// Satisfy every `/mcp reconnect` reply parked for `slug` with the current
    /// connected/failed state (called once a connect settles).
    #[cfg(feature = "mcp")]
    fn complete_pending_mcp_reconnect(&mut self, slug: &str) {
        let Some(waiters) = self.pending_mcp_reconnects.remove(slug) else {
            return;
        };
        let result = if self.mcp_manager.is_connected(slug) {
            Ok(())
        } else {
            Err(self
                .mcp_manager
                .failure(slug)
                .unwrap_or_else(|| format!("MCP server {slug:?} is not connected")))
        };
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }

    /// Bring up the MCP server a just-stored OAuth grant belongs to.
    ///
    /// A completed sign-in stores the credential (see
    /// `DaemonState::handle_save_credential`) but does NOT, on its own, rebuild
    /// the connection that failed with an auth-required error before the grant
    /// existed — so the server would stay offline until the user ran
    /// `/mcp reconnect`. Reconnecting here makes a sign-in self-sufficient.
    ///
    /// `credential_key` is the daemon's target-scoped `mcp:<slug>|<hash>` key;
    /// the slug it encodes is brought up (see [`Self::bring_up_mcp_slug`]).
    #[cfg(feature = "mcp")]
    pub(crate) fn reconnect_mcp_for_credential(&mut self, credential_key: &str) {
        let Some(slug) = choreo_mcp::slug_from_credential_key(credential_key) else {
            debug!(%credential_key, "stored credential is not an MCP key; nothing to reconnect");
            return;
        };
        self.bring_up_mcp_slug(slug);
    }

    /// Bring up every MCP server whose OAuth grant just became available after
    /// an unlock (or bind).
    ///
    /// The daemon starts LOCKED and runs its background startup connects while
    /// the grant is still undecryptable, so an OAuth server's first connect
    /// 401s; this retries those servers once the credentials are live, on
    /// background workers, so the unlock reply is never delayed by a handshake.
    ///
    /// Called from the unlock tail AFTER the acting client's `Unlocked` reply
    /// is enqueued (see `DaemonState::handle_unlock`), so the targeted reply and
    /// the lock-state broadcast always precede this work.
    #[cfg(feature = "mcp")]
    pub(crate) fn schedule_mcp_resync_for_credentials(&mut self) {
        for slug in self.mcp_resync_candidates() {
            self.bring_up_mcp_slug(&slug);
        }
    }

    /// Without the `mcp` feature there are no MCP servers to bring up.
    #[cfg(not(feature = "mcp"))]
    pub(crate) fn schedule_mcp_resync_for_credentials(&mut self) {}

    /// The servers a post-unlock resync should (re)connect: every configured
    /// server that has an `mcp:` credential. Sorted and deduplicated so the
    /// spawn order is deterministic.
    #[cfg(feature = "mcp")]
    fn mcp_resync_candidates(&self) -> Vec<String> {
        let mut slugs: Vec<String> = self
            .credentials
            .keys()
            .filter(|key| choreo_mcp::is_mcp_credential_key(key))
            .filter_map(|key| choreo_mcp::slug_from_credential_key(key))
            .filter(|slug| self.mcp_manager.is_configured(slug))
            .map(str::to_string)
            .collect();
        slugs.sort();
        slugs.dedup();
        slugs
    }

    /// Bring up (or rebuild) the MCP server `slug` whose credentials just became
    /// available, without blocking the command loop.
    ///
    /// A daemon-tier SHARED server connects on a background worker, coalescing
    /// with a connect already in flight: the resync is remembered so that
    /// connect's failure retries once with the now-live credentials.
    ///
    /// A daemon-tier `shared = false` server lives per session, so its stale
    /// connections are dropped and the sessions holding them re-resolve — which
    /// plans the entry as pending again and connects it on a worker. A
    /// daemon-tier connect must NOT be used for it: that would pool a second,
    /// shared connection whose tools duplicate the session's private ones.
    ///
    /// A slug that names no configured server (a stale grant, or a project
    /// server on an untrusted root) is a no-op.
    #[cfg(feature = "mcp")]
    fn bring_up_mcp_slug(&mut self, slug: &str) {
        if self.mcp_manager.is_shared_configured(slug) {
            if !self.spawn_mcp_connect(slug) {
                self.mcp_manager.request_resync(slug);
            }
            return;
        }
        if !self.mcp_manager.is_configured(slug) {
            debug!(server = %slug, "no MCP server is configured for this slug; nothing to bring up");
            return;
        }
        let holders = self.mcp_manager.drop_daemon_session_slots(slug);
        if holders.is_empty() {
            debug!(
                server = %slug,
                "per-session MCP server holds no live connection; nothing to rebuild"
            );
            return;
        }
        info!(
            server = %slug,
            sessions = holders.len(),
            "rebuilding a per-session MCP server with the now-available credentials"
        );
        self.refresh_session_overlays(holders);
    }
}
