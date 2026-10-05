//! The pure overlay value types and the project-root walk.
//!
//! [`ProjectToolSet`] is the private tool set a session carries for ITS project
//! (plus any per-session `shared = false` daemon server), [`SessionMcpOverlay`]
//! is what a resolve produces, and [`project_root_for`] is the walk UP from a
//! working directory that identifies the project root (the project's identity
//! AND trust key).
//!
//! Everything here is pure and compiles UNCONDITIONALLY (so a session can hold
//! its overlay in any build); the resolution that (re)connects the servers
//! behind the overlay is `mcp`-feature-gated and lives in `overlay.rs`.

use super::McpServerStatus;
use crate::tools::{ToolDyn, ToolError, ToolOutput, ToolOutputFormat};
use choreo_ai_protocols::openai::ChatToolDefinition;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The set of tool wrappers a session carries for ITS project's MCP servers
/// (and any per-session, `shared = false` daemon-tier servers).
///
/// This is the session's private overlay: it never enters the daemon-wide
/// `ToolRegistry` (which holds only core + daemon-tier shared servers + static
/// groups). The request path merges this set's definitions on top of the
/// registry's (with every daemon-tier group the session's project shadows
/// removed), and the execution path consults it BEFORE the shared registry.
///
/// Compiled unconditionally (it wraps `dyn ToolDyn`, which is always present)
/// so a session's `SessionState` can hold an `Arc<ProjectToolSet>` in every
/// build; without the `mcp` feature the set is simply always empty.
#[derive(Default)]
pub struct ProjectToolSet {
    /// The wrapped tools, in registration order.
    tools: Vec<Box<dyn ToolDyn>>,
    /// `name -> index into tools`, so `has`/`describe_invocation_json`/`execute_*`
    /// are O(1) rather than a linear scan. Names are unique within the set
    /// (`resolve_name` dedupes against the resolve's shared `used` set), so each
    /// tool appears exactly once; kept in lockstep with `tools`, which preserves
    /// registration order for `definitions`.
    index: HashMap<String, usize>,
    /// The `mcp/<slug>` groups these tools belong to — the groups whose
    /// daemon-tier counterparts the session must shadow.
    groups: HashSet<String>,
}

impl ProjectToolSet {
    /// An empty tool set.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            groups: HashSet::new(),
        }
    }

    /// Build a set from a session's collected overlay tools, indexing every
    /// tool by name for O(1) lookup. The `Vec` keeps registration order for
    /// `definitions`; the index is derived from it so the two cannot drift.
    /// Gated with the resolution methods that call it (only the `mcp` feature
    /// builds a non-empty set).
    #[cfg(feature = "mcp")]
    pub(super) fn new(tools: Vec<Box<dyn ToolDyn>>, groups: HashSet<String>) -> Self {
        let index: HashMap<String, usize> = tools
            .iter()
            .enumerate()
            .map(|(i, tool)| (tool.name().to_string(), i))
            .collect();
        Self {
            tools,
            index,
            groups,
        }
    }

    /// The tool named `name`, if the set holds one, via the name index.
    fn find(&self, name: &str) -> Option<&dyn ToolDyn> {
        // `index` is `name -> index into tools` and is built from the same
        // `Vec`, so the lookup is total; `get` keeps it panic-free either way.
        let index = *self.index.get(name)?;
        self.tools.get(index).map(AsRef::as_ref)
    }

    /// Whether the set holds no tools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// How many tools the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// The `mcp/<slug>` groups this set provides.
    #[must_use]
    pub fn groups(&self) -> &HashSet<String> {
        &self.groups
    }

    /// Whether a tool with `name` lives in this set.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// A tool definition for every tool in the set (Text/JSON-compatible).
    #[must_use]
    pub fn definitions(&self) -> Vec<ChatToolDefinition> {
        self.tools
            .iter()
            .map(|t| ChatToolDefinition::function(t.name(), t.description(), t.schema()))
            .collect()
    }

    /// Describe a call against a tool in this set, or `None` if unknown.
    #[must_use]
    pub fn describe_invocation_json(&self, name: &str, args_json: &str) -> Option<String> {
        self.find(name)
            .map(|t| t.describe_invocation_json(args_json))
    }

    /// Execute a JSON tool call against this set, or `None` when the tool is
    /// not held here (so the caller falls back to the shared registry).
    #[must_use]
    pub fn execute_json(
        &self,
        tool_call: &choreo_ai_protocols::ChatToolCall,
        format: ToolOutputFormat,
        x_credentials: Option<&choreo_keystore::ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&crate::tools::context::ToolContext>,
        image_tx: Option<crossbeam_channel::Sender<crate::tools::PreparedImage>>,
    ) -> Option<Result<ToolOutput, ToolError>> {
        self.find(&tool_call.name).map(|t| {
            t.execute_json(
                &tool_call.arguments_json,
                format,
                x_credentials,
                working_dir,
                ctx,
                image_tx,
            )
        })
    }

    /// Execute a streaming JSON tool call against this set, or `None` when the
    /// tool is not held here.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the ToolDyn::execute_streaming_json signature field-for-field so the set is a drop-in front for the registry"
    )]
    pub fn execute_streaming_json(
        &self,
        tool_call: &choreo_ai_protocols::ChatToolCall,
        format: ToolOutputFormat,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        x_credentials: Option<&choreo_keystore::ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&crate::tools::context::ToolContext>,
        image_tx: Option<crossbeam_channel::Sender<crate::tools::PreparedImage>>,
    ) -> Option<Result<ToolOutput, ToolError>> {
        self.find(&tool_call.name).map(|t| {
            t.execute_streaming_json(
                &tool_call.arguments_json,
                format,
                x_credentials,
                working_dir,
                output_tx,
                ctx,
                image_tx,
            )
        })
    }

    /// Execute a postcard tool call against this set, or `None` when the tool
    /// is not held here.
    #[must_use]
    pub fn execute_postcard(
        &self,
        name: &str,
        args_bytes: &[u8],
        x_credentials: Option<&choreo_keystore::ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&crate::tools::context::ToolContext>,
    ) -> Option<Vec<u8>> {
        self.find(name)
            .map(|t| t.execute_postcard(args_bytes, x_credentials, working_dir, ctx))
    }
}

/// A session's MCP overlay, produced by the daemon when a session's project is
/// (re)resolved: the private project/per-session tools plus the daemon-tier
/// groups the session's project shadows.
#[derive(Default, Clone)]
pub struct SessionMcpOverlay {
    /// The session's private tool set (project servers plus `shared = false`
    /// per-session servers).
    pub tools: Arc<ProjectToolSet>,
    /// `mcp/<slug>` groups to remove from the session's registry view (their
    /// project counterpart replaces them).
    pub shadowed_groups: HashSet<String>,
    /// The session's project root, if any.
    pub project_root: Option<PathBuf>,
    /// Whether that root is trusted.
    pub project_trusted: bool,
    /// Slugs declared by an untrusted `.mcp.json` (ignored, never spawned).
    pub ignored_project_servers: Vec<String>,
    /// The project/per-session server statuses (tier-tagged).
    pub statuses: Vec<McpServerStatus>,
}

impl SessionMcpOverlay {
    /// An empty overlay (no project, no per-session servers).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tools: Arc::new(ProjectToolSet::empty()),
            shadowed_groups: HashSet::new(),
            project_root: None,
            project_trusted: false,
            ignored_project_servers: Vec::new(),
            statuses: Vec::new(),
        }
    }
}

/// Resolve a session's project MCP root from its working directory: walk UP
/// from `working_dir` to the git root (inclusive), and return the directory of
/// the first `.mcp.json` found. The owning directory is the project's identity
/// AND its trust key.
///
/// A session without a working directory has no project tier (`None`).
/// Compiled unconditionally (pure path logic) so the daemon can resolve a
/// root regardless of the `mcp` feature.
#[must_use]
pub fn project_root_for(working_dir: &Path) -> Option<PathBuf> {
    let git_root = crate::context::find_git_root(working_dir);
    // The boundary is the git root when there is one (the walk never climbs
    // above it — a `.mcp.json` outside the repository is not this project's),
    // else the filesystem root.
    let boundary = git_root.unwrap_or_else(|| PathBuf::from("/"));
    let mut current = Some(working_dir.to_path_buf());
    while let Some(dir) = current {
        if dir.join(super::PROJECT_CONFIG_FILE).is_file() {
            return Some(dir);
        }
        if dir == boundary {
            break;
        }
        let parent = dir.parent().map(Path::to_path_buf);
        if parent.as_deref() == Some(dir.as_path()) {
            break;
        }
        current = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::project_root_for;

    #[test]
    fn project_root_for_finds_nearest_dot_mcp_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("sub").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(".mcp.json"), "{}").unwrap();
        // The walk climbs to the directory holding `.mcp.json`.
        assert_eq!(project_root_for(&nested), Some(root.to_path_buf()));
    }

    #[test]
    fn project_root_for_returns_none_without_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(project_root_for(dir.path()), None);
    }
}
