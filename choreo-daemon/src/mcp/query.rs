//! The `McpManager` read/observe and per-session cancel surface: the `/mcp`
//! status snapshots and the in-flight call cancellation.
//!
//! These methods only READ the pool (status) or reach into it to cancel calls
//! (cancel); none of them reconcile connections. They are split out of the
//! manager's root so `mod.rs` stays focused on construction, registration, and
//! the pool-reconciliation orchestration, and so the status/outcome types in
//! `status.rs` sit next to the `impl` that produces them. Their `impl
//! McpManager` block reaches the manager's private fields as a child module.
//!
//! Everything here drives a live manager, so the module body is `mcp`-feature
//! gated; without the feature the feature-off stub in `stub.rs` provides the
//! same method signatures.

#[cfg(feature = "mcp")]
use super::{McpServerStatus, ServerSlot};
#[cfg(feature = "mcp")]
use std::collections::HashSet;
#[cfg(feature = "mcp")]
use std::path::Path;

#[cfg(feature = "mcp")]
impl super::McpManager {
    /// A snapshot of every daemon-tier SHARED server's state, in stable slug
    /// order.
    ///
    /// Daemon-tier `shared = false` servers are per-session (they live in
    /// `session_slots`, never in `servers`), so they are NOT reported here —
    /// they would appear as a "not connected" duplicate of the per-session row
    /// [`McpManager::session_status`] owns.
    ///
    /// [`McpManager::session_status`]: Self::session_status
    #[must_use]
    pub fn status(&self) -> Vec<McpServerStatus> {
        self.order
            .iter()
            .filter(|slug| self.configs.get(*slug).is_some_and(|e| e.shared))
            .map(|slug| self.daemon_status(slug))
            .collect()
    }

    fn daemon_status(&self, slug: &str) -> McpServerStatus {
        let config = self.configs.get(slug).map(|e| &e.config);
        let transport = config.map_or_else(String::new, |c| c.transport.label().to_string());
        let target = config.map_or_else(String::new, |c| c.transport.target().to_string());
        match self.servers.get(slug) {
            Some(slot) => McpServerStatus {
                slug: slug.to_string(),
                tier: "daemon".to_string(),
                transport,
                target,
                connected: true,
                tool_count: slot.tool_count,
                server_name: Some(slot.handle.name().to_string()),
                server_version: Some(slot.handle.version().to_string()),
                last_error: None,
            },
            None => McpServerStatus {
                slug: slug.to_string(),
                tier: "daemon".to_string(),
                transport,
                target,
                connected: false,
                tool_count: 0,
                server_name: None,
                server_version: None,
                last_error: self.failures.get(slug).cloned(),
            },
        }
    }

    /// Statuses of a session's private (project + per-session) servers, in
    /// stable `(tier, slug)` order.
    #[must_use]
    pub fn session_status(&self, session_id: u64) -> Vec<McpServerStatus> {
        let mut out = Vec::new();
        // Project-shared connections this session references.
        for ((_, slug), shared) in &self.project_shared {
            if shared.sessions.contains(&session_id) {
                out.push(Self::slot_status(slug, "project", &shared.slot));
            }
        }
        // Per-session connections.
        for ((sid, root, slug), slot) in &self.session_slots {
            if *sid == session_id {
                let tier = if root.is_some() { "project" } else { "daemon" };
                out.push(Self::slot_status(slug, tier, slot));
            }
        }
        out.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| a.slug.cmp(&b.slug)));
        out
    }

    fn slot_status(slug: &str, tier: &str, slot: &ServerSlot) -> McpServerStatus {
        McpServerStatus {
            slug: slug.to_string(),
            tier: tier.to_string(),
            transport: slot.config.transport.label().to_string(),
            target: slot.config.transport.target().to_string(),
            connected: true,
            tool_count: slot.tool_count,
            server_name: Some(slot.handle.name().to_string()),
            server_version: Some(slot.handle.version().to_string()),
            last_error: None,
        }
    }

    /// Cancel every in-flight tool call started by `session_id`.
    pub fn cancel_session(&self, session_id: u64) {
        for slot in self.servers.values() {
            slot.handle.cancel_session(session_id);
        }
        for shared in self.project_shared.values() {
            shared.slot.handle.cancel_session(session_id);
        }
        for ((sid, _, _), slot) in &self.session_slots {
            if *sid == session_id {
                slot.handle.cancel_session(session_id);
            }
        }
    }

    /// Cancel `session_id`'s in-flight tool calls to the servers of ONE project
    /// root: its project-shared connections for that root plus its per-session
    /// project connections under it.
    ///
    /// The session's daemon-tier calls (shared or per-session) are left running:
    /// a working-directory change that leaves a project must not disturb an
    /// unrelated in-flight call.
    pub fn cancel_session_project(&self, session_id: u64, project_root: &Path) {
        for ((root, _slug), shared) in &self.project_shared {
            if root.as_path() == project_root && shared.sessions.contains(&session_id) {
                shared.slot.handle.cancel_session(session_id);
            }
        }
        for ((sid, root, _slug), slot) in &self.session_slots {
            if *sid == session_id && root.as_deref() == Some(project_root) {
                slot.handle.cancel_session(session_id);
            }
        }
    }

    /// The sessions that hold a project-tier or per-session server with `slug`
    /// (project or daemon tier) — i.e. the sessions whose PRIVATE overlay
    /// includes a server with that slug.
    ///
    /// A daemon-tier SHARED server with the same slug is NOT included: its tools
    /// live in the daemon-wide catalogue, which the command loop refreshes
    /// separately (`register_all`). Used to route a list change into exactly the
    /// sessions whose overlay a project server's change affects.
    #[must_use]
    pub fn sessions_for_slug(&self, slug: &str) -> HashSet<u64> {
        let mut out = HashSet::new();
        for ((_root, s), shared) in &self.project_shared {
            if s == slug {
                out.extend(shared.sessions.iter().copied());
            }
        }
        for (sid, _root, s) in self.session_slots.keys() {
            if s == slug {
                out.insert(*sid);
            }
        }
        out
    }
}
