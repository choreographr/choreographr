pub(crate) use crate::openai::AllowedCaller;
use crate::openai::ChatToolCall;
use crate::openai::ChatToolDefinition;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc;
use tai_keystore::ServiceCredential;

/// Helper: encode Result<R, impl ToString> as postcard-encoded Result<R, String>.
fn encode_result<R: Serialize>(result: Result<R, impl ToString>) -> Vec<u8> {
    let wrapped: Result<R, String> = result.map_err(|e| e.to_string());
    postcard::to_allocvec(&wrapped).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to postcard-encode tool result");
        Vec::new()
    })
}

/// Simplified tool-definition macro.
///
/// Covers the common case (`Return = String`, no credentials/context).
/// For tools that need custom `output_schema`, `allowed_callers`, non-`String`
/// returns, or `use_credentials`, write `impl Tool` manually.
macro_rules! define_tool {
    ($struct:ident, $name:literal, $desc:literal, $args_ty:ty,
     $exec_fn:path, $group:literal) => {
        impl $crate::tools::Tool for $struct {
            type Args = $args_ty;
            type Return = String;
            fn name(&self) -> &'static str {
                $name
            }
            fn group(&self) -> &'static str {
                $group
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn execute(
                &self,
                args: Self::Args,
                _x_credentials: Option<&$crate::tools::ServiceCredential>,
                working_dir: Option<&std::path::Path>,
                _ctx: Option<&$crate::tools::context::ToolContext>,
            ) -> Result<Self::Return, $crate::tools::ToolError> {
                $exec_fn(&args, working_dir)
            }
        }
    };
}

pub(crate) mod admin;
mod error;
pub use error::ToolError;
pub(crate) use error::{tool_err, tool_ok};

pub mod context;
pub(crate) mod db;
pub(crate) mod exec;
pub(crate) mod find;
pub(crate) mod fish;
pub(crate) mod fs;
pub(crate) mod git;
pub(crate) mod grep;
pub(crate) mod groups;
pub mod http;
mod image;
pub(crate) mod notify;
pub(crate) mod nu;
pub(crate) mod random;
pub(crate) mod sh;
pub(crate) mod shell_util;
pub mod subsession;
pub(crate) mod time;
pub(crate) mod vm;
pub(crate) mod x;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug)]
pub struct ToolExecutionOutput {
    pub result: ToolResult,
}

#[derive(Debug)]
pub struct PreparedImage {
    pub(crate) mime_type: String,
    pub(crate) data: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) alt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolGroup {
    pub name: String,
    pub description: String,
}

/// Typed tool trait. Each tool declares its Args and Return types.
/// Both must be serde-compatible (JSON path uses serde_json, binary path uses postcard).
pub trait Tool: Send + Sync {
    /// Argument type — must be deserializable from both JSON and postcard.
    type Args: DeserializeOwned + JsonSchema + 'static;
    /// Return type — must be serializable to both JSON and postcard.
    type Return: Serialize + JsonSchema + 'static;

    fn name(&self) -> &'static str;
    fn group(&self) -> &'static str {
        "core"
    }
    fn description(&self) -> &'static str;

    /// Auto-derived JSON Schema for the tool's input arguments.
    fn schema(&self) -> serde_json::Value {
        let mut schema =
            serde_json::to_value(schemars::schema_for!(Self::Args)).unwrap_or_default();
        if let Some(obj) = schema.as_object_mut() {
            obj.insert("additionalProperties".into(), false.into());
        }
        schema
    }

    /// JSON Schema for the tool's return value (for Programmatic Tool Calling).
    /// The default auto-derives the schema from the return type. Override this
    /// for tools with custom deserialization that schemars cannot represent.
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::to_value(schemars::schema_for!(Self::Return)).unwrap_or_default())
    }

    /// Controls which callers can invoke this tool.
    /// - `[AllowedCaller::Direct]` — model can call directly
    /// - `[AllowedCaller::Direct, AllowedCaller::Programmatic]` — model or JS program (default)
    ///
    ///   Return the list of allowed caller types.
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct, AllowedCaller::Programmatic]
    }

    /// Execute the tool with typed arguments.
    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Result<Self::Return, ToolError>;

    /// Execute with streaming output.
    ///
    /// The default implementation calls execute() and sends the serialized
    /// return value as one chunk through output_tx. Tools that produce
    /// incremental output (shell commands, VM execution) override this.
    fn execute_streaming(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ret = self.execute(args, x_credentials, working_dir, ctx)?;
        let bytes = postcard::to_allocvec(&ret).map_err(ToolError::Postcard)?;
        let _ = output_tx.send(bytes);
        Ok(ret)
    }

    /// Optional: extract a PreparedImage from the return value.
    /// Only display_image overrides this.
    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> {
        None
    }
}

/// Type-erased dispatch trait stored in ToolRegistry.
/// Converts between JSON/binary and the typed Tool::execute().
pub trait ToolDyn: Send + Sync {
    fn name(&self) -> &str;
    fn group(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    fn output_schema(&self) -> Option<serde_json::Value>;
    fn allowed_callers(&self) -> Vec<AllowedCaller>;

    /// JSON path (LLM tool calls) — returns ToolExecutionOutput for session.
    fn execute_json(
        &self,
        args_json: &str,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput;

    /// Binary path (VM ecall) — returns raw postcard-encoded Result<Return, String>.
    fn execute_binary(
        &self,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8>;

    /// Streaming JSON path.
    fn execute_streaming_json(
        &self,
        args_json: &str,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput;

    /// Streaming binary path.
    fn execute_streaming_binary(
        &self,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8>;
}

/// Blanket impl: every TypedTool is also a ToolDyn.
impl<T: Tool + 'static> ToolDyn for T {
    fn name(&self) -> &'static str {
        Tool::name(self)
    }
    fn group(&self) -> &'static str {
        Tool::group(self)
    }
    fn description(&self) -> &'static str {
        Tool::description(self)
    }
    fn schema(&self) -> serde_json::Value {
        Tool::schema(self)
    }
    fn output_schema(&self) -> Option<serde_json::Value> {
        Tool::output_schema(self)
    }
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        Tool::allowed_callers(self)
    }

    fn execute_json(
        &self,
        args_json: &str,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput {
        let args = match serde_json::from_str::<T::Args>(args_json) {
            Ok(a) => a,
            Err(e) => {
                return ToolExecutionOutput {
                    result: ToolResult {
                        content: format!("invalid arguments: {e}"),
                        is_error: true,
                    },
                };
            }
        };
        match self.execute(args, x_credentials, working_dir, ctx) {
            Ok(ret) => {
                if let Some(tx) = image_tx
                    && let Some(image) = self.extract_image(&ret)
                {
                    let _ = tx.send(image);
                }
                ToolExecutionOutput {
                    result: ToolResult {
                        content: serde_json::to_string(&ret).unwrap_or_default(),
                        is_error: false,
                    },
                }
            }
            Err(e) => ToolExecutionOutput {
                result: ToolResult {
                    content: e.to_string(),
                    is_error: true,
                },
            },
        }
    }

    fn execute_binary(
        &self,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8> {
        let args = match postcard::from_bytes::<T::Args>(args_bytes) {
            Ok(a) => a,
            Err(e) => return encode_result::<T::Return>(Err::<T::Return, _>(e)),
        };
        encode_result(self.execute(args, x_credentials, working_dir, ctx))
    }

    fn execute_streaming_json(
        &self,
        args_json: &str,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput {
        let args = match serde_json::from_str::<T::Args>(args_json) {
            Ok(a) => a,
            Err(e) => {
                return ToolExecutionOutput {
                    result: ToolResult {
                        content: format!("invalid arguments: {e}"),
                        is_error: true,
                    },
                };
            }
        };
        match self.execute_streaming(args, x_credentials, working_dir, output_tx, ctx) {
            Ok(ret) => {
                if let Some(tx) = image_tx
                    && let Some(image) = self.extract_image(&ret)
                {
                    let _ = tx.send(image);
                }
                ToolExecutionOutput {
                    result: ToolResult {
                        content: serde_json::to_string(&ret).unwrap_or_default(),
                        is_error: false,
                    },
                }
            }
            Err(e) => ToolExecutionOutput {
                result: ToolResult {
                    content: e.to_string(),
                    is_error: true,
                },
            },
        }
    }

    fn execute_streaming_binary(
        &self,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8> {
        let args = match postcard::from_bytes::<T::Args>(args_bytes) {
            Ok(a) => a,
            Err(e) => return encode_result::<T::Return>(Err::<T::Return, _>(e)),
        };
        encode_result(self.execute_streaming(args, x_credentials, working_dir, output_tx, ctx))
    }
}

pub fn static_groups() -> &'static [ToolGroup] {
    static GROUPS: OnceLock<Vec<ToolGroup>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        vec![
            ToolGroup {
                name: "core".into(),
                description: "File system operations, HTTP requests, image display, file search, random values, and time queries".into(),
            },
            ToolGroup {
                name: "desktop".into(),
                description: "Desktop notifications via notify-send".into(),
            },
            ToolGroup {
                name: "db".into(),
                description: "Session-scoped key-value database (redb)".into(),
            },
            ToolGroup {
                name: "git".into(),
                description: "Local Git repository operations (status, diff, log, add, commit, push)".into(),
            },
            ToolGroup {
                name: "shell".into(),
                description: "Shell command execution (bash, nushell, fish, exec)".into(),
            },
            ToolGroup {
                name: "x".into(),
                description: "X/Twitter API (post, search, user lookup)".into(),
            },
            ToolGroup {
                name: "vm".into(),
                description: "RISC-V sandboxed code execution".into(),
            },
        ]
    })
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn ToolDyn>>,
    dynamic_groups: Vec<(String, String)>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            tools: HashMap::new(),
            dynamic_groups: Vec::new(),
        };
        reg.register(fs::ReadFile);
        reg.register(fs::ReadFileRange);
        reg.register(fs::ListFiles);
        reg.register(fs::LineCount);
        reg.register(http::HttpRequest);
        reg.register(fs::WriteFile);
        reg.register(fs::EditFile);
        reg.register(image::DisplayImage::new());
        reg.register(git::GitStatus);
        reg.register(git::GitDiff);
        reg.register(git::GitLog);
        reg.register(git::GitAdd);
        reg.register(git::GitCommit);
        reg.register(git::GitPush);
        reg.register(sh::Sh);
        if shell_util::binary_exists("nu") {
            reg.register(nu::NuShell);
        }
        if shell_util::binary_exists("fish") {
            reg.register(fish::FishShell);
        }
        reg.register(exec::Exec);
        reg.register(grep::Grep);
        reg.register(find::Find);
        reg.register(random::Random);
        reg.register(notify::NotifySend);
        reg.register(time::GetCurrentTime);
        reg.register(x::XPost);
        reg.register(x::XSearchRecent);
        reg.register(x::XUserLookup);
        reg.register(db::DbSet);
        reg.register(db::DbGet);
        reg.register(db::DbDelete);
        reg.register(db::DbDeleteRange);
        reg.register(db::DbGetRange);
        reg.register(db::DbList);
        reg.register(db::DbCount);
        reg.register(admin::ListSessions);
        reg.register(admin::GetSession);
        reg.register(admin::LoadSkill);
        reg.register(subsession::SpawnSubsession);
        reg
    }

    /// Build a shared registry with the RunRiscV tool registered.
    ///
    /// Uses `Arc::new_cyclic` to give the RISC-V sandbox a weak reference to
    /// the registry so guest tool calls can be dispatched without a global.
    pub fn build(self) -> Arc<Self> {
        Arc::new_cyclic(|weak| {
            let mut reg = self;
            reg.register(vm::RunRiscV::new(weak.clone()));
            reg
        })
    }

    pub(crate) fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.name().to_string();
        self.tools.insert(name, Box::new(tool));
    }

    pub fn execute(
        &self,
        tool_call: &ChatToolCall,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput {
        match self.tools.get(tool_call.name.as_str()) {
            Some(tool) => tool.execute_json(
                &tool_call.arguments_json,
                x_credentials,
                working_dir,
                ctx,
                image_tx,
            ),
            None => ToolExecutionOutput {
                result: ToolResult {
                    content: format!("unknown tool: {}", tool_call.name),
                    is_error: true,
                },
            },
        }
    }

    pub fn execute_streaming(
        &self,
        tool_call: &ChatToolCall,
        output_tx: mpsc::Sender<Vec<u8>>,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput {
        match self.tools.get(tool_call.name.as_str()) {
            Some(tool) => tool.execute_streaming_json(
                &tool_call.arguments_json,
                x_credentials,
                working_dir,
                output_tx,
                ctx,
                image_tx,
            ),
            None => ToolExecutionOutput {
                result: ToolResult {
                    content: format!("unknown tool: {}", tool_call.name),
                    is_error: true,
                },
            },
        }
    }

    /// Execute a tool via postcard binary dispatch (VM path).
    pub fn execute_dyn(
        &self,
        name: &str,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8> {
        match self.tools.get(name) {
            Some(tool) => tool.execute_binary(args_bytes, x_credentials, working_dir, ctx),
            None => {
                let err: Result<(), String> = Err(format!("unknown tool: {name}"));
                postcard::to_allocvec(&err).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, tool = %name, "failed to postcard-encode unknown-tool error");
                    Vec::new()
                })
            }
        }
    }

    /// Register a dynamically-loaded tool (e.g. from an MCP server).
    /// The group name must already be registered via `register_dynamic_group`.
    pub fn register_dynamic(&mut self, name: String, group: String, tool: Box<dyn ToolDyn>) {
        tracing::debug!(tool = %name, group = %group, "registered dynamic tool");
        self.tools.insert(name, tool);
    }

    /// Register a dynamic tool group name so it appears in group listings.
    pub fn register_dynamic_group(&mut self, name: String, description: String) {
        self.dynamic_groups.push((name, description));
    }

    /// Remove all tools belonging to a dynamic group and return their names.
    pub fn unregister_group(&mut self, group: &str) -> Vec<String> {
        let mut removed = Vec::new();
        self.tools.retain(|name, tool| {
            if tool.group() == group {
                removed.push(name.clone());
                false
            } else {
                true
            }
        });
        self.dynamic_groups.retain(|(g, _)| g != group);
        if !removed.is_empty() {
            tracing::debug!(group = %group, count = removed.len(), "unregistered dynamic group");
        }
        removed
    }

    pub fn groups(&self) -> Vec<ToolGroup> {
        let mut groups: Vec<ToolGroup> = static_groups().to_vec();
        for (name, desc) in &self.dynamic_groups {
            groups.push(ToolGroup {
                name: name.clone(),
                description: desc.clone(),
            });
        }
        groups
    }

    /// Return group names suitable for a JSON Schema enum (excluding "core", which
    /// is always active and should not appear in load_tools/unload_tools schemas).
    pub fn group_names(&self) -> Vec<String> {
        self.groups()
            .into_iter()
            .filter(|g| g.name != "core")
            .map(|g| g.name)
            .collect()
    }

    /// Return tool definitions for groups in the active set, plus always-available
    /// meta-tools (load_tools, unload_tools, etc.).
    pub fn available_definitions(&self, active: &HashSet<String>) -> Vec<ChatToolDefinition> {
        let mut defs: Vec<_> = self
            .tools
            .values()
            .filter(|t| active.contains(t.group()))
            .map(|t| {
                ChatToolDefinition::function_with_options(
                    t.name(),
                    t.description(),
                    t.schema(),
                    t.output_schema(),
                    Some(t.allowed_callers()),
                )
            })
            .collect();
        // Always-available meta-tools (not in the registry because they
        // need mutable access to session state — load_tools, unload_tools).
        defs.push(groups::load_tools_definition(self));
        defs.push(groups::unload_tools_definition(self));
        defs.push(groups::set_working_dir_definition());
        defs
    }
}

pub(crate) fn resolve_path(
    path: &str,
    working_dir: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(working_dir) = working_dir {
        working_dir.join(p)
    } else {
        p.to_path_buf()
    }
}

/// Resolve a path relative to `working_dir` and verify it stays within
/// the session's working directory boundary.
///
/// When `working_dir` is `None`, confinement is skipped and the path is
/// returned as resolved by [`resolve_path`] (relative to the daemon's
/// process working directory).
pub(crate) fn confine_path(
    path: &str,
    working_dir: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, ToolError> {
    let resolved = resolve_path(path, working_dir);
    if let Some(wd) = working_dir {
        let wd_canonical = wd.canonicalize().map_err(|e| {
            ToolError::Other(format!(
                "cannot resolve session working directory '{}': {e}",
                wd.display()
            ))
        })?;
        // For paths that may not exist yet (e.g. a file about to be
        // created by write_file), walk up to the nearest existing
        // ancestor and canonicalize that for the confinement check.
        let anchor = resolve_existing_ancestor(&resolved).map_err(|_| {
            ToolError::Other(format!(
                "path '{}' has no existing ancestor within the filesystem",
                resolved.display()
            ))
        })?;
        let anchor_canonical = anchor.canonicalize().map_err(|e| {
            ToolError::Other(format!(
                "cannot resolve path component '{}': {e}",
                anchor.display()
            ))
        })?;
        if !anchor_canonical.starts_with(&wd_canonical) {
            return Err(ToolError::Other(format!(
                "path '{}' is outside the session working directory '{}'",
                resolved.display(),
                wd.display(),
            )));
        }
    }
    Ok(resolved)
}

/// Walk up from `path` until a component exists on disk, allowing
/// confinement checks for paths that have not been created yet.
fn resolve_existing_ancestor(path: &std::path::Path) -> std::io::Result<&std::path::Path> {
    let mut p = path;
    loop {
        if p.exists() {
            return Ok(p);
        }
        p = p.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no existing ancestor found for '{}'", path.display()),
            )
        })?;
    }
}

pub(crate) fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    hex::encode(digest)
}

pub(crate) fn truncate_tool_output(content: &str) -> String {
    const MAX_TOOL_OUTPUT_CHARS: usize = 16 * 1024;
    if content.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return content.to_string();
    }
    let truncated = content
        .chars()
        .take(MAX_TOOL_OUTPUT_CHARS)
        .collect::<String>();
    format!("{truncated}\n...[truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn confine_path_within_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = confine_path("subdir", Some(dir.path()));
        let expected = dir.path().join("subdir");
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn confine_path_nonexistent_file_within_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = confine_path("nonexistent/file.txt", Some(dir.path()));
        let expected = dir.path().join("nonexistent/file.txt");
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn confine_path_outside_dir_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = confine_path("..", Some(dir.path()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::Other(_)));
    }

    #[test]
    fn confine_path_absolute_outside_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = confine_path("/etc/passwd", Some(dir.path()));
        assert!(result.is_err());
    }

    #[test]
    fn confine_path_no_working_dir_returns_path() {
        let result = confine_path("relative/path", None);
        assert_eq!(result.unwrap(), Path::new("relative/path").to_path_buf());
    }

    #[test]
    fn confine_path_absolute_no_working_dir() {
        let result = confine_path("/tmp", None);
        assert_eq!(result.unwrap(), Path::new("/tmp").to_path_buf());
    }

    #[test]
    fn resolve_existing_ancestor_finds_root() {
        let path = Path::new("/nonexistent_dir_12345/file.txt");
        let ancestor = resolve_existing_ancestor(path).unwrap();
        assert_eq!(ancestor, Path::new("/"));
    }

    #[test]
    fn resolve_existing_ancestor_finds_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new_file.txt");
        let ancestor = resolve_existing_ancestor(&path).unwrap();
        assert_eq!(ancestor, dir.path());
    }

    #[test]
    fn confine_path_deep_path_inside_dir_allowed() {
        let dir = tempfile::tempdir().unwrap();
        // only the workspace dir exists, not a/b/c
        let result = confine_path("a/b/c/d/file.txt", Some(dir.path()));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path().join("a/b/c/d/file.txt"));
    }

    #[test]
    fn confine_path_symlink_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        // Create a symlink inside the workspace that points outside
        let link = dir.path().join("escape");
        std::os::unix::fs::symlink(target.path(), &link).unwrap();
        // Accessing a file through the symlink should be rejected
        let result = confine_path("escape/outside.txt", Some(dir.path()));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_existing_ancestor_errors_on_empty() {
        // An obviously non-existent deeply nested path should walk up to root,
        // which always exists on Unix, so it should succeed.
        let path = Path::new("/tmp/__tai_test_nonexistent_dir_abcdefg/h/i/j/k/file.txt");
        let ancestor = resolve_existing_ancestor(path).unwrap();
        assert!(ancestor.exists());
    }

    // ── Tool trait default method tests ──────────────────────────────

    /// A minimal tool that uses all defaults for the new methods.
    struct DefaultTool;

    impl Tool for DefaultTool {
        type Args = ();
        type Return = String;

        fn name(&self) -> &'static str {
            "default_tool"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "A tool with default settings"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _x_credentials: Option<&ServiceCredential>,
            _working_dir: Option<&std::path::Path>,
            _ctx: Option<&crate::tools::context::ToolContext>,
        ) -> Result<Self::Return, ToolError> {
            Ok("ok".to_string())
        }
    }

    #[test]
    fn default_output_schema_is_string() {
        let tool = DefaultTool;
        let schema = Tool::output_schema(&tool).expect("schema");
        assert_eq!(schema["type"], "string");
    }

    #[test]
    fn default_allowed_callers_includes_both() {
        let tool = DefaultTool;
        let callers = Tool::allowed_callers(&tool);
        assert_eq!(callers.len(), 2);
        assert!(callers.contains(&AllowedCaller::Direct));
        assert!(callers.contains(&AllowedCaller::Programmatic));
    }

    #[test]
    fn default_tool_name_description_schema() {
        let tool = DefaultTool;
        assert_eq!(Tool::name(&tool), "default_tool");
        assert_eq!(Tool::group(&tool), "test");
        assert_eq!(Tool::description(&tool), "A tool with default settings");
    }

    // ── ToolDyn delegation tests ─────────────────────────────────────

    #[test]
    fn tooldyn_delegates_output_schema() {
        let tool: Box<dyn ToolDyn> = Box::new(DefaultTool);
        let schema = tool.output_schema().expect("schema");
        assert_eq!(schema["type"], "string");
    }

    #[test]
    fn tooldyn_delegates_allowed_callers() {
        let tool: Box<dyn ToolDyn> = Box::new(DefaultTool);
        let callers = tool.allowed_callers();
        assert!(callers.contains(&AllowedCaller::Direct));
        assert!(callers.contains(&AllowedCaller::Programmatic));
    }

    #[test]
    fn tooldyn_delegates_group() {
        let tool: Box<dyn ToolDyn> = Box::new(DefaultTool);
        assert_eq!(tool.group(), "test");
    }

    /// A tool that overrides output_schema and allowed_callers.
    struct RestrictedTool;

    impl Tool for RestrictedTool {
        type Args = ();
        type Return = u64;

        fn name(&self) -> &'static str {
            "restricted_tool"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "A tool with restricted callers"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn output_schema(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({"type": "integer"}))
        }
        fn allowed_callers(&self) -> Vec<AllowedCaller> {
            vec![AllowedCaller::Direct]
        }
        fn execute(
            &self,
            _args: Self::Args,
            _x_credentials: Option<&ServiceCredential>,
            _working_dir: Option<&std::path::Path>,
            _ctx: Option<&crate::tools::context::ToolContext>,
        ) -> Result<Self::Return, ToolError> {
            Ok(42)
        }
    }

    #[test]
    fn restricted_tool_uses_overridden_output_schema() {
        let tool = RestrictedTool;
        assert_eq!(
            Tool::output_schema(&tool),
            Some(serde_json::json!({"type": "integer"}))
        );
    }

    #[test]
    fn restricted_tool_uses_overridden_allowed_callers() {
        let tool = RestrictedTool;
        assert_eq!(Tool::allowed_callers(&tool), vec![AllowedCaller::Direct]);
        assert!(!Tool::allowed_callers(&tool).contains(&AllowedCaller::Programmatic));
    }

    #[test]
    fn tooldyn_delegates_restricted_output_schema() {
        let tool: Box<dyn ToolDyn> = Box::new(RestrictedTool);
        assert_eq!(
            tool.output_schema(),
            Some(serde_json::json!({"type": "integer"}))
        );
    }

    #[test]
    fn tooldyn_delegates_restricted_allowed_callers() {
        let tool: Box<dyn ToolDyn> = Box::new(RestrictedTool);
        assert_eq!(tool.allowed_callers(), vec![AllowedCaller::Direct]);
    }
}
