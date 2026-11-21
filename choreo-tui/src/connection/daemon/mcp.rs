//! The **mcp / catalog** family of daemon messages: the `/mcp` status, trust,
//! reconnect and reload replies, and the provider-catalog update broadcast.
//!
//! `Mcp*` and `CatalogUpdated` share one file because both render a whole
//! server/provider list into the status area and both are connection-level
//! (they carry no session scope). Every arm consumes the message: it either
//! renders its own multi-line status or surfaces an error, so none should fall
//! through to the generic dispatch.
use super::Consumed;
use crate::state::{App, ProviderInfo};
use choreo_proto::{CatalogProvider, ClientMessage, McpServerStatus};

/// An `McpStatus` reply: bare `/mcp`'s server list and a successful
/// `/mcp reconnect`, rendered one server per line.
pub(super) fn on_mcp_status(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    servers: &[McpServerStatus],
    project_root: Option<&String>,
    project_trusted: bool,
    ignored_project_servers: &[String],
) -> Consumed {
    // Bare `/mcp`'s status list AND a successful `/mcp reconnect` both
    // arrive as McpStatus. Render one server per line; the status bar
    // reserves the needed rows (status_error_height counts newlines), so
    // the whole list is visible rather than wrapped to one line.
    let mut lines = Vec::new();
    if servers.is_empty() {
        lines.push("no MCP servers configured".to_string());
    } else {
        lines.push(format!("MCP servers ({})", servers.len()));
        for server in servers {
            lines.push(format!("[{}] {}", server.tier, server.summary()));
        }
    }
    if let Some(root) = project_root {
        let state = if project_trusted {
            "trusted"
        } else {
            "UNTRUSTED (use /mcp trust)"
        };
        lines.push(format!("project root: {root} ({state})"));
    }
    if !ignored_project_servers.is_empty() {
        lines.push(format!(
            "{} project server(s) ignored (untrusted): {}",
            ignored_project_servers.len(),
            ignored_project_servers.join(", ")
        ));
    }
    app.set_status(lines.join("\n"));
    // Start any login/logout that was waiting on this reply: it reads
    // the daemon's LIVE oauth block and credential key straight from
    // THIS reply, so there is no cache to go stale.
    app.on_mcp_status(servers, client_tx);
    Consumed::Yes
}

/// An `McpTrustUpdated` reply (or its untrust counterpart).
pub(super) fn on_mcp_trust_updated(app: &mut App, trusted: bool, message: &str) -> Consumed {
    let _ = trusted;
    app.set_status(message.to_string());
    Consumed::Yes
}

/// An `McpTrustList` reply: the trusted project MCP roots.
pub(super) fn on_mcp_trust_list(app: &mut App, roots: &[String]) -> Consumed {
    if roots.is_empty() {
        app.set_status("no trusted project MCP roots".to_string());
    } else {
        let mut lines = vec![format!("trusted project MCP roots ({})", roots.len())];
        for root in roots {
            lines.push(root.clone());
        }
        app.set_status(lines.join("\n"));
    }
    Consumed::Yes
}

/// An `McpReconnectFailed` reply.
pub(super) fn on_mcp_reconnect_failed(app: &mut App, slug: &str, error: &str) -> Consumed {
    tracing::warn!(%slug, error_len = error.len(), "mcp reconnect failed");
    app.set_error(format!("[daemon] mcp reconnect {slug} failed: {error}"));
    Consumed::Yes
}

/// An `McpReloaded` reply: the summary line plus the refreshed server list.
pub(super) fn on_mcp_reloaded(
    app: &mut App,
    summary: &str,
    servers: &[McpServerStatus],
) -> Consumed {
    // A successful `/mcp reload` arrives as McpReloaded: the summary line
    // plus the refreshed per-server list (rendered the same way McpStatus
    // is — one server per line so the whole set is visible).
    let mut lines = vec![summary.to_string()];
    for server in servers {
        lines.push(server.summary());
    }
    app.set_status(lines.join("\n"));
    Consumed::Yes
}

/// An `McpReloadFailed` reply.
pub(super) fn on_mcp_reload_failed(app: &mut App, error: &str) -> Consumed {
    tracing::warn!(error_len = error.len(), "mcp reload failed");
    app.set_error(format!("[daemon] mcp reload failed: {error}"));
    Consumed::Yes
}

/// A `CatalogUpdated` broadcast: replace the live provider list (the picker's
/// source of truth).
pub(super) fn on_catalog_updated(app: &mut App, providers: &[CatalogProvider]) -> Consumed {
    // Replace the live provider list (the picker's source of truth)
    // and clamp the wizard selection if the list shrank. Only churn
    // the status line when the list actually changed — the daemon
    // also sends one on every activity-subscribe, which would
    // otherwise overwrite unrelated status messages at connect.
    let mapped = providers
        .iter()
        .map(|p| ProviderInfo {
            slug: p.slug.clone(),
            display_name: p.display_name.clone(),
        })
        .collect();
    if app.set_providers(mapped) {
        app.set_status(format!(
            "catalog updated ({} providers)",
            app.providers.len()
        ));
    }
    Consumed::Yes
}
