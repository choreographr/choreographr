use crate::context::{self, LoadedSkill, SkillMeta};
use crate::sessions::SessionState;
use crate::tools::{ToolOutput, ToolRegistry};
use choreo_ai_protocols::{ChatToolCall, ToolResultItem};
use choreo_proto::ContextConfig;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, trace, warn};
pub(crate) fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get(key)?.as_str().map(|s| s.to_string())
}

pub(crate) struct SystemContentParams<'a> {
    pub(crate) working_dir: Option<&'a Path>,
    pub(crate) context_config: &'a ContextConfig,
    pub(crate) skills: &'a [SkillMeta],
    pub(crate) loaded_skill_bodies: &'a [LoadedSkill],
    pub(crate) tool_registry: &'a ToolRegistry,
    pub(crate) pending_hints: &'a [String],
    /// The session title, if one has been set, so the LLM can maintain
    /// awareness of the agreed-upon session purpose.
    pub(crate) session_title: Option<&'a str>,
}

pub(crate) fn build_system_content(
    params: SystemContentParams,
    context_cache: &mut Option<(u64, Arc<String>)>,
) -> Option<String> {
    let working_dir = match params.working_dir {
        Some(wd) => wd,
        None => {
            warn!("cannot build system content: no working directory on session");
            return None;
        }
    };
    let groups = params.tool_registry.groups();
    let base_prompt =
        context::build_base_prompt(params.skills, &groups, params.loaded_skill_bodies);
    let mut content = base_prompt;

    // Context files with fingerprint caching
    if let Ok(bundle) = context::discover_context(working_dir, params.context_config) {
        let context_str = match context_cache {
            Some((fp, cached)) if *fp == bundle.fingerprint => {
                debug!("context cache HIT (fp={})", fp);
                cached.as_str().to_string()
            }
            _ => {
                let s = context::assemble_context(&bundle);
                debug!(
                    "context cache MISS — rebuilt context ({} bytes from {} file(s))",
                    s.len(),
                    bundle.files.len()
                );
                *context_cache = Some((bundle.fingerprint, Arc::new(s.clone())));
                s
            }
        };
        if !context_str.is_empty() {
            content.push_str("\n\n");
            content.push_str(&context_str);
        }
    }

    // Inject the current session title so the LLM can see the agreed-upon
    // session purpose across turns without re-deriving it from conversation
    // history.  Only included when a title has been explicitly set.
    if let Some(title) = params.session_title
        && !title.is_empty()
    {
        content.push_str("\n\n## Current Session Title\n");
        content.push_str(title);
    }

    // Pending subdirectory hints
    if !params.pending_hints.is_empty() {
        content.push_str("\n\n## New context from project subdirectories\n");
        for hint in params.pending_hints {
            content.push('\n');
            content.push_str(hint);
        }
    }

    Some(content)
}

/// Detect a `load_skill` tool call and persist the loaded skill body into
/// the session's loaded_skill_bodies accumulator so it appears in subsequent
/// system prompts.
pub(crate) fn persist_loaded_skill(
    session: &mut SessionState,
    tool_name: &str,
    arguments_json: &str,
) {
    if tool_name != "load_skill" {
        return;
    }
    let Some(name) = extract_json_string(arguments_json, "name") else {
        warn!("load_skill tool call missing 'name' argument");
        return;
    };
    if session.loaded_skill_bodies.iter().any(|ls| ls.name == name) {
        debug!("skill '{}' already loaded, skipping", name);
        return;
    }
    let Some(ref working_dir) = session.config.working_dir else {
        warn!("cannot load skill '{}': no working directory", name);
        return;
    };
    if let Some(body) = context::load_skill_body(&name, working_dir) {
        info!("loaded skill body: '{}' ({} bytes)", name, body.len());
        session.loaded_skill_bodies.push(LoadedSkill { name, body });
    } else {
        warn!("skill '{}' not found or has empty body", name);
    }
}

/// Check whether a tool call touches a new subdirectory with an AGENTS.md /
/// CLAUDE.md file and, if so, collect the hint text and newly discovered paths.
pub(crate) fn check_subdirectory_hints(
    working_dir: Option<&Path>,
    tool_name: &str,
    arguments_json: &str,
    known_hint_paths: &mut Vec<PathBuf>,
    pending_hints: &mut Vec<String>,
) {
    if let Some((hint_text, new_paths)) =
        context::subdirectory_hints(tool_name, arguments_json, working_dir, known_hint_paths)
    {
        debug!(
            "subdirectory hints for '{}': {} new path(s)",
            tool_name,
            new_paths.len()
        );
        known_hint_paths.extend(new_paths);
        pending_hints.push(hint_text);
    }
}

pub(crate) struct CollectToolResultParams<'a> {
    pub(crate) tool_results: &'a mut Vec<ToolResultItem>,
    pub(crate) session: &'a mut SessionState,
    pub(crate) tool_call: &'a ChatToolCall,
    pub(crate) output: &'a ToolOutput,
    pub(crate) known_hint_paths: &'a mut Vec<PathBuf>,
    pub(crate) pending_hints: &'a mut Vec<String>,
}

/// Collect tool execution output into the result accumulator, persist any
/// `load_skill` call to the session, and check for new subdirectory hints.
/// Called after every tool execution in both the serial and concurrent phases.
pub(crate) fn collect_tool_result(params: CollectToolResultParams) {
    let CollectToolResultParams {
        tool_results,
        session,
        tool_call,
        output,
        known_hint_paths,
        pending_hints,
    } = params;
    trace!(
        "collecting tool result for call {} (tool: '{}')",
        tool_call.id, tool_call.name
    );
    tool_results.push(ToolResultItem {
        call_id: tool_call.id.clone(),
        output: output.content.clone(),
        caller: tool_call.caller.clone(),
    });
    persist_loaded_skill(session, &tool_call.name, &tool_call.arguments_json);
    check_subdirectory_hints(
        session.config.working_dir.as_deref(),
        &tool_call.name,
        &tool_call.arguments_json,
        known_hint_paths,
        pending_hints,
    );
}
