//! Session-config tool machinery for the agent loop: the tool-name predicate
//! (`is_session_config_tool`), the typed capture of an accepted mutation
//! ([`PendingConfigChange`] via `pending_config_change`), and its application to
//! the worker's throwaway config copy (`apply_pending_config_change`).
//! Re-exported from `requests.rs` via `pub(crate) use config_tools::*;`.
//!
//! The authoritative mutation is applied by the session main loop; this module
//! only mirrors it onto the worker copy so the next agent-loop iteration sees
//! the change. It is separate from `run_agent_loop` because the loop owns the
//! dispatch ORDER (config tools serially, everything else concurrently) while
//! this module owns the argument parsing and canonical-path rules that decide
//! what a mirrored change means.
use crate::sessions::SessionState;
use crate::tools::ToolOutput;
use crate::tools::load_tools::{LoadToolsArgs, apply_load_tools};
use crate::tools::set_working_dir::{SetWorkingDirArgs, resolve_working_dir_path};
use crate::tools::unload_tools::{UnloadToolsArgs, apply_unload_tools};
use choreo_ai_protocols::ChatToolCall;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// A successful session-config tool mutation, captured in Phase 1 and
/// applied to the worker's config copy in Phase 3.
///
/// The authoritative mutation is applied by the session main loop (via
/// `DaemonCommand` → `SessionCommand` routing); this worker copy must be updated
/// as well so the NEXT agent-loop iteration observes the change when it
/// rebuilds tool definitions, system content, and working-dir-relative file
/// operations.
pub(crate) enum PendingConfigChange {
    LoadTools(Vec<String>),
    UnloadTools(Vec<String>),
    /// The canonical path the tool resolved and the session main loop applied
    /// verbatim, taken from the tool's EXECUTED result (so no re-resolution
    /// and therefore no TOCTOU window).  `None` only when the executed result
    /// was unavailable AND re-resolution failed — the worker then skips the
    /// path update but still invalidates its skill cache so a stale cache
    /// never survives the request boundary.
    SetWorkingDir(Option<PathBuf>),
}

/// Whether `name` is one of the session-config tools that must run serially
/// and whose successful mutations are mirrored onto the worker config copy.
/// Single source of truth for the tool-name list used by the dispatch
/// partition and the mirror capture.
pub(crate) fn is_session_config_tool(name: &str) -> bool {
    matches!(name, "load_tools" | "unload_tools" | "set_working_dir")
}

/// Capture a successful session-config tool's mutation into a typed
/// [`PendingConfigChange`] for later application.  Called only for tools that
/// actually executed without error.
///
/// `base_working_dir` is the working directory in effect when the response
/// was planned — every `set_working_dir` call in the response resolved
/// against it, so the (rare) re-resolution fallback must too (chaining
/// relative resolutions against the mutated copy would diverge from the
/// canonical paths the tools sent to the main loop, which applies them
/// verbatim in call order).
pub(crate) fn pending_config_change(
    tool_call: &ChatToolCall,
    output: &ToolOutput,
    base_working_dir: Option<&Path>,
) -> Option<PendingConfigChange> {
    if !is_session_config_tool(&tool_call.name) {
        return None;
    }
    match tool_call.name.as_str() {
        "load_tools" => {
            let Ok(args) = serde_json::from_str::<LoadToolsArgs>(&tool_call.arguments_json) else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "load_tools: could not parse args to mirror onto worker config",
                );
                return None;
            };
            Some(PendingConfigChange::LoadTools(args.groups))
        }
        "unload_tools" => {
            let Ok(args) = serde_json::from_str::<UnloadToolsArgs>(&tool_call.arguments_json)
            else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "unload_tools: could not parse args to mirror onto worker config",
                );
                return None;
            };
            Some(PendingConfigChange::UnloadTools(args.groups))
        }
        "set_working_dir" => {
            // Prefer the canonical path from the tool's EXECUTED result: it
            // matches byte-for-byte what the session main loop applied, with
            // no re-resolution (and therefore no TOCTOU window in which the
            // directory could vanish between the tool's resolution and this
            // mirror).
            if let Some(path) = output
                .result_json
                .as_ref()
                .and_then(|v| v.get("path"))
                .and_then(|v| v.as_str())
            {
                return Some(PendingConfigChange::SetWorkingDir(Some(PathBuf::from(
                    path,
                ))));
            }
            // Fallback (practically unreachable — result_json is populated on
            // every successful execution): re-run the tool's own shared
            // resolution.  If even that fails, still return a None-path change
            // so the caller invalidates the worker's skill cache — a stale
            // cache must never survive the request boundary.
            let Ok(args) = serde_json::from_str::<SetWorkingDirArgs>(&tool_call.arguments_json)
            else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "set_working_dir: could not parse args to mirror onto worker config",
                );
                return Some(PendingConfigChange::SetWorkingDir(None));
            };
            let path = resolve_working_dir_path(&args.path, base_working_dir).ok();
            Some(PendingConfigChange::SetWorkingDir(path))
        }
        _ => None,
    }
}

/// Apply a captured session-config mutation to the worker's config copy.
pub(crate) fn apply_pending_config_change(
    session: &mut SessionState,
    change: &PendingConfigChange,
    protected: &HashSet<String>,
) {
    match change {
        PendingConfigChange::LoadTools(groups) => {
            apply_load_tools(&mut session.config.active_tool_groups, groups);
            debug!(groups = ?groups, "mirrored load_tools onto worker session config");
        }
        PendingConfigChange::UnloadTools(groups) => {
            apply_unload_tools(&mut session.config.active_tool_groups, groups, protected);
            debug!(groups = ?groups, "mirrored unload_tools onto worker session config");
        }
        PendingConfigChange::SetWorkingDir(path) => {
            if let Some(path) = path {
                session.config.working_dir = Some(path.clone());
            }
            // Always invalidate the skill cache: even when we could not
            // determine the new path, the authoritative state changed and a
            // stale cache would leak across the request boundary
            // (RequestFinished merges the worker's discovered_skills over the
            // main loop's invalidated None).  The main-loop handler does the
            // same for the authoritative state.
            session.discovered_skills = None;
            debug!(path = ?path, "mirrored set_working_dir onto worker session config");
        }
    }
}
