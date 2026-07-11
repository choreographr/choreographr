use crate::openai::{ChatToolCall, ChatToolDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
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

#[macro_export]
macro_rules! define_tool {
    // With both use_credentials and use_context
    ($struct:ident, $name:literal, $desc:literal, $args_ty:ty, $return_ty:ty,
     $exec_fn:path, $schema:expr, $tool_group:literal, use_credentials, use_context) => {
        impl $crate::tools::Tool for $struct {
            type Args = $args_ty;
            type Return = $return_ty;
            fn name(&self) -> &'static str {
                $name
            }
            fn group(&self) -> &'static str {
                $tool_group
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn schema(&self) -> serde_json::Value {
                $schema
            }
            fn execute(
                &self,
                args: Self::Args,
                x_credentials: Option<&$crate::tools::ServiceCredential>,
                cwd: Option<&std::path::Path>,
                ctx: Option<&$crate::tools::context::ToolContext>,
            ) -> Result<Self::Return, $crate::tools::ToolError> {
                $exec_fn(&args, x_credentials, cwd, ctx)
            }
        }
    };
    // With use_credentials
    ($struct:ident, $name:literal, $desc:literal, $args_ty:ty, $return_ty:ty,
     $exec_fn:path, $schema:expr, $tool_group:literal, use_credentials) => {
        impl $crate::tools::Tool for $struct {
            type Args = $args_ty;
            type Return = $return_ty;
            fn name(&self) -> &'static str {
                $name
            }
            fn group(&self) -> &'static str {
                $tool_group
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn schema(&self) -> serde_json::Value {
                $schema
            }
            fn execute(
                &self,
                args: Self::Args,
                x_credentials: Option<&$crate::tools::ServiceCredential>,
                cwd: Option<&std::path::Path>,
                _ctx: Option<&$crate::tools::context::ToolContext>,
            ) -> Result<Self::Return, $crate::tools::ToolError> {
                $exec_fn(&args, x_credentials, cwd)
            }
        }
    };
    // With use_context
    ($struct:ident, $name:literal, $desc:literal, $args_ty:ty, $return_ty:ty,
     $exec_fn:path, $schema:expr, $tool_group:literal, use_context) => {
        impl $crate::tools::Tool for $struct {
            type Args = $args_ty;
            type Return = $return_ty;
            fn name(&self) -> &'static str {
                $name
            }
            fn group(&self) -> &'static str {
                $tool_group
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn schema(&self) -> serde_json::Value {
                $schema
            }
            fn execute(
                &self,
                args: Self::Args,
                _x_credentials: Option<&$crate::tools::ServiceCredential>,
                cwd: Option<&std::path::Path>,
                ctx: Option<&$crate::tools::context::ToolContext>,
            ) -> Result<Self::Return, $crate::tools::ToolError> {
                $exec_fn(&args, cwd, ctx)
            }
        }
    };
    // Default (no flags) — original behavior
    ($struct:ident, $name:literal, $desc:literal, $args_ty:ty, $return_ty:ty,
     $exec_fn:path, $schema:expr, $tool_group:literal) => {
        impl $crate::tools::Tool for $struct {
            type Args = $args_ty;
            type Return = $return_ty;
            fn name(&self) -> &'static str {
                $name
            }
            fn group(&self) -> &'static str {
                $tool_group
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn schema(&self) -> serde_json::Value {
                $schema
            }
            fn execute(
                &self,
                args: Self::Args,
                _x_credentials: Option<&$crate::tools::ServiceCredential>,
                cwd: Option<&std::path::Path>,
                _ctx: Option<&$crate::tools::context::ToolContext>,
            ) -> Result<Self::Return, $crate::tools::ToolError> {
                $exec_fn(&args, cwd)
            }
        }
    };
}

pub(crate) mod admin;
mod error;
pub(crate) use error::{ToolError, tool_err, tool_ok};

pub(crate) mod context;
pub(crate) mod db;
pub(crate) mod exec;
mod fff;
pub(crate) mod fish;
pub(crate) mod fs;
pub(crate) mod git;
pub(crate) mod groups;
pub(crate) mod http;
mod image;
pub(crate) mod nu;
pub(crate) mod random;
pub(crate) mod sh;
pub(crate) mod shell_util;
pub(crate) mod subsession;
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
    pub(crate) result: ToolResult,
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
    pub name: &'static str,
    pub description: &'static str,
}

/// Typed tool trait. Each tool declares its Args and Return types.
/// Both must be serde-compatible (JSON path uses serde_json, binary path uses postcard).
pub trait Tool: Send + Sync {
    /// Argument type — must be deserializable from both JSON and postcard.
    type Args: DeserializeOwned + 'static;
    /// Return type — must be serializable to both JSON and postcard.
    type Return: Serialize + 'static;

    fn name(&self) -> &'static str;
    fn group(&self) -> &'static str {
        "core"
    }
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;

    /// Execute the tool with typed arguments.
    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&std::path::Path>,
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
        cwd: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ret = self.execute(args, x_credentials, cwd, ctx)?;
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
    fn name(&self) -> &'static str;
    fn group(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;

    /// JSON path (LLM tool calls) — returns ToolExecutionOutput for session.
    fn execute_json(
        &self,
        args_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput;

    /// Binary path (VM ecall) — returns raw postcard-encoded Result<Return, String>.
    fn execute_binary(
        &self,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8>;

    /// Streaming JSON path.
    fn execute_streaming_json(
        &self,
        args_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput;

    /// Streaming binary path.
    fn execute_streaming_binary(
        &self,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&std::path::Path>,
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

    fn execute_json(
        &self,
        args_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&std::path::Path>,
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
        match self.execute(args, x_credentials, cwd, ctx) {
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
        cwd: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8> {
        let args = match postcard::from_bytes::<T::Args>(args_bytes) {
            Ok(a) => a,
            Err(e) => return encode_result::<T::Return>(Err::<T::Return, _>(e)),
        };
        encode_result(self.execute(args, x_credentials, cwd, ctx))
    }

    fn execute_streaming_json(
        &self,
        args_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&std::path::Path>,
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
        match self.execute_streaming(args, x_credentials, cwd, output_tx, ctx) {
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
        cwd: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8> {
        let args = match postcard::from_bytes::<T::Args>(args_bytes) {
            Ok(a) => a,
            Err(e) => return encode_result::<T::Return>(Err::<T::Return, _>(e)),
        };
        encode_result(self.execute_streaming(args, x_credentials, cwd, output_tx, ctx))
    }
}

pub const GROUPS: &[ToolGroup] = &[
    ToolGroup {
        name: "core",
        description: "File system operations, HTTP requests, image display, file search, random values, and time queries",
    },
    ToolGroup {
        name: "db",
        description: "Session-scoped key-value database (redb)",
    },
    ToolGroup {
        name: "git",
        description: "Local Git repository operations (status, diff, log, add, commit, push)",
    },
    ToolGroup {
        name: "shell",
        description: "Shell command execution (bash, nushell, fish, exec)",
    },
    ToolGroup {
        name: "x",
        description: "X/Twitter API (post, search, user lookup)",
    },
    ToolGroup {
        name: "vm",
        description: "RISC-V sandboxed code execution",
    },
];

pub struct ToolRegistry {
    tools: HashMap<&'static str, Box<dyn ToolDyn>>,
    fff_cache: Arc<fff::FffStateCache>,
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
            fff_cache: Arc::new(fff::FffStateCache::new()),
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
        // Only register the shell tool when at least one POSIX variant is found.
        if shell_util::binary_exists("bash")
            || shell_util::binary_exists("dash")
            || shell_util::binary_exists("zsh")
        {
            reg.register(sh::Sh);
        }
        if shell_util::binary_exists("nu") {
            reg.register(nu::NuShell);
        }
        if shell_util::binary_exists("fish") {
            reg.register(fish::FishShell);
        }
        reg.register(exec::Exec);
        reg.register(fff::Fff::new(Arc::clone(&reg.fff_cache)));
        reg.register(random::Random);
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
        let name = tool.name();
        self.tools.insert(name, Box::new(tool));
    }

    pub fn execute(
        &self,
        tool_call: &ChatToolCall,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput {
        match self.tools.get(tool_call.name.as_str()) {
            Some(tool) => {
                tool.execute_json(&tool_call.arguments_json, x_credentials, cwd, ctx, image_tx)
            }
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
        cwd: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> ToolExecutionOutput {
        match self.tools.get(tool_call.name.as_str()) {
            Some(tool) => tool.execute_streaming_json(
                &tool_call.arguments_json,
                x_credentials,
                cwd,
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
        cwd: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8> {
        match self.tools.get(name) {
            Some(tool) => tool.execute_binary(args_bytes, x_credentials, cwd, ctx),
            None => {
                let err: Result<(), String> = Err(format!("unknown tool: {name}"));
                postcard::to_allocvec(&err).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, tool = %name, "failed to postcard-encode unknown-tool error");
                    Vec::new()
                })
            }
        }
    }

    pub fn groups(&self) -> &[ToolGroup] {
        GROUPS
    }

    /// Return group names suitable for a JSON Schema enum (excluding "core", which
    /// is always active and should not appear in load_tools/unload_tools schemas).
    pub fn group_names(&self) -> Vec<&'static str> {
        GROUPS
            .iter()
            .filter(|c| c.name != "core")
            .map(|c| c.name)
            .collect()
    }

    /// Return tool definitions for groups in the active set, plus always-available
    /// meta-tools (load_tools, unload_tools, load_skill, spawn_subsession, etc.).
    pub fn available_definitions(&self, active: &HashSet<String>) -> Vec<ChatToolDefinition> {
        let mut defs: Vec<_> = self
            .tools
            .values()
            .filter(|t| active.contains(t.group()))
            .map(|t| ChatToolDefinition::function(t.name(), t.description(), t.schema()))
            .collect();
        // Always-available meta-tools (not in the registry because they
        // need mutable access to session state or deep coupling with the
        // agent loop — spawn_subsession, load_tools, unload_tools).
        defs.push(groups::load_tools_definition(self));
        defs.push(groups::unload_tools_definition(self));
        defs.push(subsession::spawn_subsession_definition());
        defs
    }
}

pub(crate) fn resolve_path(path: &str, cwd: Option<&std::path::Path>) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(cwd) = cwd {
        cwd.join(p)
    } else {
        p.to_path_buf()
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
