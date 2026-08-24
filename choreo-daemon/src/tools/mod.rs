use choreo_ai_protocols::ChatToolCall;
pub(crate) use choreo_ai_protocols::openai::AllowedCaller;
use choreo_ai_protocols::openai::ChatToolDefinition;
use choreo_keystore::ServiceCredential;
use choreo_proto::ImageReference;
use crossbeam_channel;
use humfmt::{BytesOptions, bytes_with};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc;

/// Helper: encode Result<Result<R, E>, ToolError> as postcard bytes.
/// Used by `execute_postcard` to produce a single byte buffer containing
/// all possible outcomes for the VM guest:
///
///   Ok(Ok(ret))  → tool succeeded, `ret: R`
///   Ok(Err(e))   → tool failed, `e: E` (structured)
///   Err(e)       → infrastructure failure, `e: ToolError`
pub(crate) fn encode_outer<R: Serialize, E: Serialize>(
    result: Result<Result<R, E>, ToolError>,
) -> Vec<u8> {
    postcard::to_allocvec(&result).unwrap_or_else(|e| {
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
     $exec_fn:path, $group:literal, $invoke_fn:path) => {
        impl $crate::tools::Tool for $struct {
            type Args = $args_ty;
            type Return = String;
            type Error = $crate::tools::ToolExecError;
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
            ) -> Result<Self::Return, Self::Error> {
                $exec_fn(&args, working_dir).map_err(Into::into)
            }
            fn return_string(ret: &Self::Return) -> String {
                ret.clone()
            }
            fn describe_invocation(&self, args: &Self::Args) -> String {
                $invoke_fn(args)
            }
        }
    };
}

pub(crate) mod admin;
mod error;
pub(crate) mod load_tools;
pub(crate) mod set_session_title;
pub(crate) mod set_working_dir;
pub(crate) mod unload_tools;
pub use error::ToolError;
pub use error::ToolExecError;
pub(crate) use error::{tool_err, tool_ok};

// The sanitization suite, streaming read helpers, and JSON-Schema sanitizers
// were split out of this module to keep it manageable. They are re-exported
// here so every existing `crate::tools::X` reference keeps resolving unchanged.
mod sanitize;
pub(crate) use sanitize::*;
mod schema;
pub(crate) use schema::*;
mod text_stream;
pub(crate) use text_stream::*;

#[cfg(feature = "blockchain")]
impl From<choreo_blockchain::BlockchainError> for ToolExecError {
    fn from(e: choreo_blockchain::BlockchainError) -> Self {
        ToolExecError(e.to_string())
    }
}

/// Capacity of the bounded channel between a tool's execution thread and its
/// forwarding thread (`requests.rs`'s `spawn_tool_execution`), and between a
/// `run_series` sub-tool and its relay thread (`series.rs`).
///
/// Bounding the channel applies backpressure: a tool that streams output
/// faster than the forwarder can broadcast to subscribers blocks on `send`
/// instead of buffering an unbounded number of chunks in memory. The
/// forwarder drains continuously and the session command channel it forwards
/// into is unbounded (std `mpsc::Sender::send` never blocks), so this cannot
/// deadlock; on kill the forwarder exits and drops the receiver, failing any
/// blocked `send`. Matches the SSE reader's bounded-channel design
/// (`SSE_CHANNEL_CAPACITY` in choreo-ai-protocols).
pub(crate) const STREAMING_CHANNEL_CAPACITY: usize = 64;

/// Tool arguments for tools that take no parameters.
///
/// Accepts both `null` and `{}` from JSON (serde_json deserializes `()` only
/// from `null`, but OpenAI-style tool schemas advertise `{"type": "object",
/// "properties": {}}`, leading the model to send `{}`). This wrapper accepts
/// both forms so the schema and the actual deserialization agree.
#[derive(Debug, Clone, Serialize)]
pub struct EmptyArgs {}

impl JsonSchema for EmptyArgs {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("EmptyArgs")
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
}

impl<'de> Deserialize<'de> for EmptyArgs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Null => Ok(EmptyArgs {}),
            serde_json::Value::Object(m) if m.is_empty() => Ok(EmptyArgs {}),
            other => Err(D::Error::custom(format!(
                "expected null or empty object, got {other}"
            ))),
        }
    }
}

pub mod context;
pub(crate) mod db;
pub(crate) mod exec;
// Blockchain tools (EVM + Substrate/Polkadot) — behind the `blockchain`
// feature (off by default). The implementations live in the `choreo-blockchain`
// crate (which owns the tokio sidecar runtime); these modules are thin `Tool`
// wrappers over its synchronous `execute_*` entry points.
#[cfg(feature = "blockchain")]
pub(crate) mod evm;
pub(crate) mod find;
pub(crate) mod fish;
pub(crate) mod fs;
pub(crate) mod git;
pub(crate) mod glob_util;
pub(crate) mod grep;
pub mod http;
mod image;
pub(crate) mod nu;
#[cfg(feature = "blockchain")]
pub(crate) mod subxt;
// Native PDF tools (pdf_classify / pdf_to_markdown) — unconditional since
// pdf-inspector 1.15.0 ships the RUSTSEC-2026-0187 fix (lopdf >= 0.42).
pub(crate) mod pdf;
pub(crate) mod random;
pub(crate) mod read_file;
pub(crate) mod read_file_range;
pub(crate) mod read_image;
pub(crate) mod retrieve_webpage;
pub(crate) mod series;
pub(crate) mod session_inspect;
pub(crate) mod sh;
pub mod shell_util;
pub mod subsession;
pub(crate) mod time;
pub(crate) mod vm;
pub(crate) mod x;

#[derive(Debug, Clone, Copy)]
pub enum ToolOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub invocation_description: String,
    /// A vision image reference this tool produced (e.g. `read_image`), fed
    /// back to a vision-capable model on the next request. Carried as a
    /// reference, not bytes — see [`Tool::extract_image_ref`].
    pub image_ref: Option<ImageReference>,
    /// The tool's structured return value, captured after a successful
    /// execution (`serde_json::to_value(ret)`). `None` for error/timeout
    /// outputs and for returns that fail to serialize.  The request worker
    /// reads this to mirror session-config mutations (e.g. the canonical
    /// path from `set_working_dir`) onto its config copy without
    /// re-executing or re-resolving the tool's logic.
    pub result_json: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
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

/// Typed tool trait. Each tool declares its Args, Return, and Error types.
/// Args and Return must be serde-compatible (JSON path uses serde_json, binary path uses postcard).
/// Error must implement `std::error::Error` and be serializable (for the structured-error postcard path).
pub trait Tool: Send + Sync {
    /// Argument type — must be deserializable from both JSON and postcard.
    type Args: DeserializeOwned + JsonSchema + 'static;
    /// Return type — must be serializable to both JSON and postcard.
    type Return: Serialize + JsonSchema + 'static;
    /// Error type — each tool defines its own. Simple tools use `ToolExecError`;
    /// tools whose structured errors are consumed by VM guests define a `thiserror` enum.
    type Error: std::error::Error + Send + Sync + Serialize + DeserializeOwned + 'static;

    fn name(&self) -> &'static str;
    fn group(&self) -> &'static str {
        "core"
    }
    fn description(&self) -> &'static str;

    /// Auto-derived JSON Schema for the tool's input arguments.
    fn schema(&self) -> serde_json::Value {
        sanitize_params_schema(
            serde_json::to_value(schemars::schema_for!(Self::Args)).unwrap_or_default(),
        )
    }

    /// JSON Schema for the tool's return value (for Programmatic Tool Calling).
    /// The default auto-derives the schema from the return type. Override this
    /// for tools with custom deserialization that schemars cannot represent.
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(sanitize_output_schema(
            serde_json::to_value(schemars::schema_for!(Self::Return)).unwrap_or_default(),
        ))
    }

    /// Controls which callers can invoke this tool.
    /// - `[AllowedCaller::Direct]` — model can call directly
    /// - `[AllowedCaller::Direct, AllowedCaller::Programmatic]` — model or JS program (default)
    ///
    ///   Return the list of allowed caller types.
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct, AllowedCaller::Programmatic]
    }

    /// Describe the invocation in human-readable form for logging/presentation.
    fn describe_invocation(&self, args: &Self::Args) -> String;

    /// Execute the tool with typed arguments.
    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Result<Self::Return, Self::Error>;

    /// Execute with streaming output.
    ///
    /// The default implementation calls execute() and returns the result.
    /// Tools that produce incremental output (shell commands, VM execution)
    /// override this and send intermediate chunks through `output_tx`.
    fn execute_streaming(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        _output_tx: crossbeam_channel::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Non-streaming tools deliver their result via TurnAppended —
        // no ToolResultChunk traffic needed.
        tracing::trace!("non-streaming tool called via execute_streaming, delegating to execute");
        self.execute(args, x_credentials, working_dir, ctx)
    }

    /// Optional: extract a [`PreparedImage`] from the return value.
    ///
    /// Image-producing tools carry their image directly in their `Return`
    /// value (read from `ret`), with **no shared-state parking** — the tool is
    /// registered once and shared across sessions, so an image parked in a
    /// `Mutex` slot on `execute` could be overwritten by a concurrent session's
    /// invocation before this hook (called with the current invocation's `ret`)
    /// reads it back. Each image-bearing tool's `Return` struct holds its
    /// `PreparedImage` and `impl Serialize` emits only the text handle, so the
    /// JSON wire format is unchanged. Only `display_image` and
    /// `retrieve_webpage` (Screenshot action) override this.
    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> {
        None
    }

    /// Optional: extract a vision image *reference* from the return value, so
    /// the daemon can feed it back to a vision-capable model on the next
    /// request (reference-based: the durable record stores the path + metadata,
    /// and the request builder re-reads + normalizes the bytes at request
    /// time). The reference is carried in the tool's `Return` value (read from
    /// `ret`) with **no shared-state parking** — same rationale as
    /// [`Tool::extract_image`]: the `read_image` tool is shared across sessions,
    /// so the per-invocation reference must travel with its `Return` rather
    /// than a `Mutex` slot that a concurrent session could overwrite. Only the
    /// `read_image` tool overrides this.
    fn extract_image_ref(&self, _ret: &Self::Return) -> Option<ImageReference> {
        None
    }

    /// Whether this tool produces streaming output via `execute_streaming`.
    ///
    /// Streaming tools forward their live output as `ToolResultChunk`s.  The
    /// invocation description is *not* sent as a chunk: it rides on the
    /// `ToolCallStarted` broadcast (queued before the tool even starts) and on
    /// the seeded placeholder result, so clients render the same header live
    /// and in the final record — a chunk can be dropped under load, and a
    /// no-output tool emits no chunks at all.  Non-streaming tools (the
    /// default, e.g. `read_file`) emit no chunks; their description arrives
    /// via `ToolOutput.invocation_description` in the `TurnAppended`.
    fn supports_streaming_output() -> bool {
        false
    }

    /// Produce a human-readable string from the return value.
    ///
    /// Every `impl Tool` must define this. The `define_tool!` macro generates
    /// a `ret.clone()` implementation automatically. For `Return = String`,
    /// implement `ret.clone()`. For structured types, format the value
    /// however is most readable.
    fn return_string(ret: &Self::Return) -> String;
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

    fn describe_invocation_json(&self, args_json: &str) -> String;

    /// Whether this tool produces streaming output.
    /// Delegates to `Tool::supports_streaming_output()` in the blanket impl.
    fn supports_streaming_output(&self) -> bool;

    /// JSON path — takes JSON args, returns Result for the caller to handle.
    fn execute_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError>;

    #[expect(clippy::too_many_arguments)]
    /// Streaming JSON path.
    fn execute_streaming_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError>;

    /// Postcard binary path — args from postcard, returns bytes encoding
    /// `Result<Result<T::Return, T::Error>, ToolError>`. All outcomes (infra
    /// error, tool error, tool success) are contained in the byte buffer.
    fn execute_postcard(
        &self,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
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

    fn describe_invocation_json(&self, args_json: &str) -> String {
        match serde_json::from_str::<T::Args>(args_json) {
            Ok(args) => T::describe_invocation(self, &args),
            Err(_) => Tool::description(self).to_string(),
        }
    }

    fn supports_streaming_output(&self) -> bool {
        T::supports_streaming_output()
    }

    fn execute_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let args = serde_json::from_str::<T::Args>(args_json)?;
        let desc = T::describe_invocation(self, &args);
        let ret = match self.execute(args, x_credentials, working_dir, ctx) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput {
                    content: e.to_string(),
                    is_error: true,
                    invocation_description: desc,
                    ..Default::default()
                });
            }
        };
        if let Some(tx) = image_tx
            && let Some(image) = self.extract_image(&ret)
        {
            let _ = tx.send(image);
        }
        let image_ref = self.extract_image_ref(&ret);
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => T::return_string(&ret),
                ToolOutputFormat::Json => serde_json::to_string(&ret).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to JSON-encode tool return");
                    String::new()
                }),
            },
            is_error: false,
            invocation_description: desc,
            image_ref,
            result_json: serde_json::to_value(&ret).ok(),
        })
    }

    fn execute_postcard(
        &self,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8> {
        let args = match postcard::from_bytes::<T::Args>(args_bytes) {
            Ok(a) => a,
            Err(e) => {
                return encode_outer::<T::Return, T::Error>(Err(ToolError::Postcard(
                    e.to_string(),
                )));
            }
        };
        let result: Result<T::Return, T::Error> =
            self.execute(args, x_credentials, working_dir, ctx);
        encode_outer::<T::Return, T::Error>(Ok(result))
    }

    fn execute_streaming_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let args = serde_json::from_str::<T::Args>(args_json)?;
        let desc = T::describe_invocation(self, &args);
        // The invocation description is deliberately NOT sent as a streaming
        // chunk: it would be mashed against the first output line (no trailing
        // newline), and a chunk can be dropped under load, leaving the live
        // view without the tool's context.  It is delivered reliably instead —
        // on the `ToolCallStarted` broadcast and on the seeded placeholder
        // result — so the client renders the header identically during
        // streaming and in the final record.
        let ret = match self.execute_streaming(args, x_credentials, working_dir, output_tx, ctx) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput {
                    content: e.to_string(),
                    is_error: true,
                    invocation_description: desc,
                    ..Default::default()
                });
            }
        };
        if let Some(tx) = image_tx
            && let Some(image) = self.extract_image(&ret)
        {
            let _ = tx.send(image);
        }
        let image_ref = self.extract_image_ref(&ret);
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => T::return_string(&ret),
                ToolOutputFormat::Json => serde_json::to_string(&ret).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to JSON-encode tool return");
                    String::new()
                }),
            },
            is_error: false,
            invocation_description: desc,
            image_ref,
            result_json: serde_json::to_value(&ret).ok(),
        })
    }
}

pub fn static_groups() -> &'static [ToolGroup] {
    static GROUPS: OnceLock<Vec<ToolGroup>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        // `mut` is only needed when the `blockchain` feature pushes its group.
        #[allow(unused_mut)]
        let mut groups = vec![
            ToolGroup {
                name: "core".into(),
                description: "File system operations, HTTP requests, image display, PDF classification and Markdown extraction, file search, random values, time queries, and series execution".into(),
            },
            ToolGroup {
                name: "db".into(),
                description: "Session-scoped key-value database (redb)".into(),
            },
            ToolGroup {
                name: "git".into(),
                description:                 "Local Git repository operations (status, diff, log, add, commit, push, show)".into(),
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
            // Read-only diagnostics and request dry-runs; opt-in via load_tools.
            ToolGroup {
                name: "debug".into(),
                description: "Read-only diagnostics and request dry-runs (session_inspect)".into(),
            },
        ];
        // The blockchain group only exists when the `blockchain` feature is
        // compiled in (the tools are registered conditionally too), so
        // `load_tools` never advertises a group whose tools don't exist.
        #[cfg(feature = "blockchain")]
        groups.push(ToolGroup {
            name: "blockchain".into(),
            description: "EVM and Substrate/Polkadot blockchain queries (alloy/subxt)".into(),
        });
        groups
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
        reg.register(read_file::ReadFile);
        reg.register(read_file_range::ReadFileRange);
        reg.register(fs::ListFiles);
        reg.register(fs::DeleteFiles);
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
        reg.register(git::GitShow);
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
        reg.register(pdf::PdfClassify);
        reg.register(pdf::PdfToMarkdown);
        reg.register(read_image::ReadImage::new());
        // Blockchain tools — registered only when the `blockchain` feature is
        // enabled; the tools themselves live in the `choreo-blockchain` crate.
        #[cfg(feature = "blockchain")]
        {
            reg.register(evm::EvmChain);
            reg.register(evm::EvmBalance);
            reg.register(evm::EvmTokenBalance);
            reg.register(evm::EvmBlock);
            reg.register(evm::EvmTransaction);
            reg.register(evm::EvmCall);
            reg.register(evm::EvmGas);
            reg.register(evm::EvmLogs);
            reg.register(evm::EvmNonce);
            reg.register(evm::EvmResolve);
            reg.register(subxt::SubxtChain);
            reg.register(subxt::SubxtBalance);
            reg.register(subxt::SubxtQuery);
            reg.register(subxt::SubxtBlock);
        }
        reg.register(random::Random);
        reg.register(time::GetCurrentTime);
        reg.register(retrieve_webpage::RetrieveWebpage::default());
        reg.register(session_inspect::SessionInspect);
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
        reg.register(set_session_title::SetSessionTitle);
        reg.register(set_working_dir::SetWorkingDir);
        reg.register(subsession::SpawnSubsession);
        reg
    }

    /// Build a shared registry with the RunRiscV tool registered.
    ///
    /// Uses `Arc::new_cyclic` to give the RISC-V sandbox a weak reference to
    /// the registry so guest tool calls can be dispatched without a global.
    /// `load_tools`/`unload_tools` also receive a weak reference so their
    /// JSON Schema enums can list the live group catalog at definition time.
    pub fn build(self) -> Arc<Self> {
        Arc::new_cyclic(|weak| {
            let mut reg = self;
            reg.register(vm::RunRiscV::new(weak.clone()));
            reg.register(series::RunSeries::new(weak.clone()));
            reg.register(load_tools::LoadTools::new(weak.clone()));
            reg.register(unload_tools::UnloadTools::new(weak.clone()));
            reg
        })
    }

    pub(crate) fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.name().to_string();
        self.tools.insert(name, Box::new(tool));
    }

    /// JSON path — caller picks Text (LLM) or Json (PTC).
    pub fn execute_json(
        &self,
        tool_call: &ChatToolCall,
        format: ToolOutputFormat,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        match self.tools.get(tool_call.name.as_str()) {
            Some(tool) => tool.execute_json(
                &tool_call.arguments_json,
                format,
                x_credentials,
                working_dir,
                ctx,
                image_tx,
            ),
            None => Err(ToolError::Other(format!(
                "unknown tool: {}",
                tool_call.name
            ))),
        }
    }

    #[expect(clippy::too_many_arguments)]
    /// Streaming JSON path.
    pub fn execute_streaming_json(
        &self,
        tool_call: &ChatToolCall,
        format: ToolOutputFormat,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        match self.tools.get(tool_call.name.as_str()) {
            Some(tool) => tool.execute_streaming_json(
                &tool_call.arguments_json,
                format,
                x_credentials,
                working_dir,
                output_tx,
                ctx,
                image_tx,
            ),
            None => Err(ToolError::Other(format!(
                "unknown tool: {}",
                tool_call.name
            ))),
        }
    }

    pub fn describe_invocation(&self, tool_call: &ChatToolCall) -> String {
        match self.tools.get(tool_call.name.as_str()) {
            Some(tool) => tool.describe_invocation_json(&tool_call.arguments_json),
            None => tool_call.name.clone(),
        }
    }

    pub fn describe_invocation_for(&self, name: &str, args_json: &str) -> Option<String> {
        self.tools
            .get(name)
            .map(|t| t.describe_invocation_json(args_json))
    }

    /// Postcard binary dispatch (VM path).
    pub fn execute_postcard(
        &self,
        name: &str,
        args_bytes: &[u8],
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&context::ToolContext>,
    ) -> Vec<u8> {
        match self.tools.get(name) {
            Some(tool) => tool.execute_postcard(args_bytes, x_credentials, working_dir, ctx),
            None => encode_outer::<(), ()>(Err(ToolError::Other(format!("unknown tool: {name}")))),
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

    /// The set of group names valid as `load_tools`/`unload_tools` arguments:
    /// every registry group plus the always-on "core" group (which is loadable
    /// as a no-op and protected from unload, but never appears in the schema
    /// enum).  Used by the tools and the session handlers to reject unknown
    /// group names before they can be persisted into a session's active set.
    pub(crate) fn known_group_names(&self) -> HashSet<String> {
        let mut s: HashSet<String> = self.group_names().into_iter().collect();
        s.insert("core".into());
        s
    }

    /// Return tool definitions for groups in the active set.
    ///
    /// Uses plain `ChatToolDefinition::function()` — no `output_schema` or
    /// `allowed_callers` — so the definitions are compatible with both Chat
    /// Completions and Responses API paths.  The Responses API path should
    /// call [`available_definitions_for_responses`] instead when it needs
    /// those fields.
    pub fn available_definitions(&self, active: &HashSet<String>) -> Vec<ChatToolDefinition> {
        self.tools
            .values()
            .filter(|t| active.contains(t.group()))
            .map(|t| ChatToolDefinition::function(t.name(), t.description(), t.schema()))
            .collect()
    }

    /// Like [`available_definitions`] but includes `output_schema` and
    /// `allowed_callers` for the Responses API (programmatic tool calling).
    /// Only use this when sending requests to a Responses API endpoint.
    pub fn available_definitions_for_responses(
        &self,
        active: &HashSet<String>,
    ) -> Vec<ChatToolDefinition> {
        self.tools
            .values()
            .filter(|t| active.contains(t.group()))
            .map(|t| {
                let callers = t.allowed_callers();
                ChatToolDefinition::function_with_options(
                    t.name(),
                    t.description(),
                    t.schema(),
                    t.output_schema(),
                    if callers.is_empty() {
                        None
                    } else {
                        Some(callers)
                    },
                )
            })
            .collect()
    }
}

/// Validate a `load_tools`/`unload_tools` group list against the known group
/// set.  Returns `Some(unknown)` with the offending names when any group is not
/// in `known`, or `None` when every name is valid.  Shared by the tools (primary
/// validation) and the session handlers (defense-in-depth) so the two can never
/// drift.
pub(crate) fn unknown_group_names(
    groups: &[String],
    known: &HashSet<String>,
) -> Option<Vec<String>> {
    let unknown: Vec<String> = groups
        .iter()
        .filter(|g| !known.contains(*g))
        .cloned()
        .collect();
    if unknown.is_empty() {
        None
    } else {
        Some(unknown)
    }
}

/// Build the JSON Schema for the `groups` argument of `load_tools`/`unload_tools`
/// from the live registry group catalog (including dynamic MCP groups).  The
/// schema enum is advisory — the tools validate at execution time — but keeping
/// the two schema builders in one place prevents drift.
pub(crate) fn groups_enum_schema(names: Vec<String>, description: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "groups": {
                "type": "array",
                "items": {
                    "type": "string",
                    "enum": names
                },
                "description": description
            }
        },
        "required": ["groups"]
    })
}

/// Expand a leading tilde (`~` or `~/...`) to the user's home directory.
///
/// Handles `~` alone (maps to home dir), `~/path` (prepends home dir),
/// and plain paths (returned unchanged).  Does **not** handle `~user`
/// forms — those are passed through unmodified.
pub(crate) fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        match dirs::home_dir() {
            Some(home) => {
                let home_str = home.to_string_lossy();
                if path == "~" {
                    home_str.into_owned()
                } else {
                    // path starts with "~/" — replace the tilde with the home dir
                    format!("{home_str}{}", &path[1..])
                }
            }
            None => {
                // No home directory known (unusual on Linux/macOS, but possible
                // in containerised or embedded environments).  Pass through.
                tracing::warn!(
                    "expand_tilde: no home directory found, leaving '{}' unchanged",
                    path
                );
                path.to_string()
            }
        }
    } else {
        path.to_string()
    }
}

pub(crate) fn resolve_path(
    path: &str,
    working_dir: Option<&std::path::Path>,
) -> std::path::PathBuf {
    // Expand leading tilde so callers can write `~/project` instead of the
    // full absolute path.  Only `~` and `~/...` are expanded; `~user` is
    // passed through unchanged.
    let expanded = expand_tilde(path);
    let p = std::path::Path::new(&expanded);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(working_dir) = working_dir {
        // Path::join(".") appends a literal `.` component, polluting paths
        // with `/.` separators that confuse glob matchers and walkers.
        if path == "." || path == "./" {
            working_dir.to_path_buf()
        } else {
            working_dir.join(p)
        }
    } else {
        p.to_path_buf()
    }
}

pub(crate) fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    hex::encode(digest)
}

/// Formatting for byte sizes: binary (IEC) units with a separating space
/// (`"1.5 KiB"`). humfmt trims trailing fractional zeros by default, so
/// `1.0 KiB` renders as `1 KiB` and columns stay compact — exact `u128`
/// integer arithmetic throughout.
const BYTE_OPTIONS: BytesOptions = BytesOptions::new().binary().space(true);

/// Human-readable byte size: `"512 B"`, `"1.5 KiB"`, `"100 MiB"`. Delegates
/// to humfmt's byte formatter (binary units, separating space, trimmed
/// fractional zeros) so the exact integer math lives in a maintained crate.
pub(crate) fn human_size(bytes: u64) -> String {
    bytes_with(bytes, BYTE_OPTIONS).to_string()
}

/// Render a symlink's target for `name -> target` display, appending `/`
/// when the target resolves to a directory so dir-links are visually
/// distinct from file-links. Degrades to `<unreadable target>` instead of
/// failing the whole listing or tool call.
///
/// The returned label is sanitized: a target containing a control character
/// (a newline is legal in POSIX file names) would otherwise split the
/// line-oriented output, defeating the one-line-per-entry invariant that
/// `sanitize_name` enforces for the entry names themselves.
pub(crate) fn symlink_target_label(path: &Path) -> String {
    let target = match std::fs::read_link(path) {
        Ok(target) => target.to_string_lossy().into_owned(),
        Err(err) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "failed to resolve symlink target"
            );
            return "<unreadable target>".to_string();
        }
    };
    // std::fs::metadata follows the link; on failure (e.g. dangling link) we
    // keep the bare target rather than failing the whole listing.
    let label = match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => format!("{target}/"),
        _ => target,
    };
    sanitize_name(&label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_definitions_includes_session_config_tools() {
        // Regression guard: the session-config tools (formerly inline
        // meta-tools) must be registered as real tools so they appear in
        // the API tool definitions for the always-on core group.
        let registry = ToolRegistry::new().build();
        let active: HashSet<String> = ["core".into()].into_iter().collect();
        let defs = registry.available_definitions(&active);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        for tool in [
            "set_working_dir",
            "load_tools",
            "unload_tools",
            "set_session_title",
        ] {
            assert!(
                names.contains(&tool),
                "missing {tool} in core definitions: {names:?}"
            );
        }
    }

    #[cfg(feature = "blockchain")]
    #[test]
    fn blockchain_group_registers_all_tools() {
        // With the `blockchain` feature enabled, every EVM + Substrate tool
        // must be registered under the "blockchain" group and the group must
        // appear in the catalog (so `load_tools blockchain` works).
        let registry = ToolRegistry::new().build();
        let active: HashSet<String> = ["blockchain".into()].into_iter().collect();
        let defs = registry.available_definitions(&active);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        for tool in [
            "evm_chain",
            "evm_balance",
            "evm_token_balance",
            "evm_block",
            "evm_transaction",
            "evm_call",
            "evm_gas",
            "evm_logs",
            "evm_nonce",
            "evm_resolve",
            "subxt_chain",
            "subxt_balance",
            "subxt_query",
            "subxt_block",
        ] {
            assert!(
                names.contains(&tool),
                "missing {tool} in blockchain definitions: {names:?}"
            );
        }
        let groups: Vec<String> = registry.groups().into_iter().map(|g| g.name).collect();
        assert!(
            groups.iter().any(|g| g == "blockchain"),
            "blockchain group missing: {groups:?}"
        );
    }

    #[test]
    fn available_definitions_responses_restricts_session_config_tools() {
        let registry = ToolRegistry::new().build();
        let active: HashSet<String> = ["core".into()].into_iter().collect();
        let defs = registry.available_definitions_for_responses(&active);

        // Session-config tools are Direct-only so programmatic callers
        // cannot silently redirect the session's working directory or
        // tool surface.
        let set_wd = defs
            .iter()
            .find(|d| d.function.name == "set_working_dir")
            .expect("set_working_dir should be defined");
        assert_eq!(
            set_wd.function.allowed_callers.as_deref(),
            Some(&[AllowedCaller::Direct][..])
        );

        // Ordinary tools keep the default (model or program).
        let read_file = defs
            .iter()
            .find(|d| d.function.name == "read_file")
            .expect("read_file should be defined");
        assert_eq!(
            read_file.function.allowed_callers.as_deref(),
            Some(&[AllowedCaller::Direct, AllowedCaller::Programmatic][..])
        );
    }

    // ── expand_tilde tests ────────────────────────────────────────────

    #[test]
    fn expand_tilde_plain_path_unchanged() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
        assert_eq!(expand_tilde("./dots"), "./dots");
        assert_eq!(expand_tilde(""), "");
    }

    #[test]
    fn expand_tilde_expands_to_home_dir() {
        let expanded = expand_tilde("~");
        let home = dirs::home_dir().expect("home dir should exist in test env");
        assert_eq!(expanded, home.to_string_lossy());
    }

    #[test]
    fn expand_tilde_expands_with_slash() {
        let expanded = expand_tilde("~/choreographr");
        let home = dirs::home_dir().expect("home dir should exist in test env");
        let expected = format!("{}/choreographr", home.to_string_lossy());
        assert_eq!(expanded, expected);
    }

    #[test]
    fn expand_tilde_expands_nested() {
        let expanded = expand_tilde("~/projects/foo/bar");
        let home = dirs::home_dir().expect("home dir should exist in test env");
        let expected = format!("{}/projects/foo/bar", home.to_string_lossy());
        assert_eq!(expanded, expected);
    }

    #[test]
    fn expand_tilde_user_form_left_alone() {
        // ~user is intentionally not expanded.
        assert_eq!(expand_tilde("~other/project"), "~other/project");
        assert_eq!(expand_tilde("~other"), "~other");
    }

    #[test]
    fn expand_tilde_mid_path_left_alone() {
        // Tilde not at the start is not expanded.
        assert_eq!(expand_tilde("/path/~foo"), "/path/~foo");
    }

    // ── Tool trait default method tests ──────────────────────────────

    /// A minimal tool that uses all defaults for the new methods.
    struct DefaultTool;

    impl Tool for DefaultTool {
        type Args = ();
        type Return = String;
        type Error = ToolExecError;

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
        ) -> Result<Self::Return, Self::Error> {
            Ok("ok".to_string())
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", Tool::description(self))
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
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "restricted_tool"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "A tool with restricted callers"
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.to_string()
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", Tool::description(self))
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
        ) -> Result<Self::Return, Self::Error> {
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

    // ── Default schema from () args test ──────────────────────────────

    /// A tool with unit args that exercises the default schema() path.
    struct UnitArgsTool;

    impl Tool for UnitArgsTool {
        type Args = ();
        type Return = String;
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "unit_args_tool"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "Tool with unit args"
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", Tool::description(self))
        }
        fn execute(
            &self,
            _args: Self::Args,
            _x_credentials: Option<&ServiceCredential>,
            _working_dir: Option<&std::path::Path>,
            _ctx: Option<&crate::tools::context::ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            Ok("ok".to_string())
        }
    }

    #[test]
    fn unit_args_tool_schema_is_empty_object() {
        // () args should produce {"type": "object", "properties": {}, "additionalProperties": false}
        let schema = Tool::schema(&UnitArgsTool);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"], serde_json::json!({}));
        assert_eq!(schema["additionalProperties"], false);
    }

    // ── return_string tests ─────────────────────────────────────────

    /// A tool whose `Return` is `String` — the Display impl returns the raw string.
    struct RawOutputTool;

    impl Tool for RawOutputTool {
        type Args = ();
        type Return = String;
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "raw_output_tool"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "Tool with default return_string (Display)"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _credentials: Option<&ServiceCredential>,
            _working_dir: Option<&std::path::Path>,
            _ctx: Option<&context::ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            Ok("raw\noutput".to_string())
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", Tool::description(self))
        }
    }

    #[test]
    fn return_string_default_for_string_is_raw() {
        let content = <DefaultTool as Tool>::return_string(&"hello".to_string());
        assert_eq!(content, "hello");
    }

    #[test]
    fn return_string_default_for_integer_is_plain_number() {
        let content = <RestrictedTool as Tool>::return_string(&42u64);
        assert_eq!(content, "42");
    }

    #[test]
    fn return_string_through_execute_json_text_format() {
        // execute_json with Text format calls T::return_string.
        let tool = RawOutputTool;
        let result = tool
            .execute_json("null", ToolOutputFormat::Text, None, None, None, None)
            .unwrap();
        assert!(!result.is_error, "should succeed");
        assert_eq!(result.content, "raw\noutput");
        assert!(
            result
                .invocation_description
                .contains("Tool with default return_string")
        );
    }

    #[test]
    fn return_string_through_execute_json_json_format() {
        // execute_json with Json format calls serde_json::to_string.
        let tool = RawOutputTool;
        let result = tool
            .execute_json("null", ToolOutputFormat::Json, None, None, None, None)
            .unwrap();
        assert!(!result.is_error, "should succeed");
        assert_eq!(result.content, r#""raw\noutput""#);
    }

    // ── encode_outer tests ──────────────────────────────────────────

    #[test]
    fn encode_outer_ok_ok() {
        let bytes = encode_outer::<String, ToolExecError>(Ok(Ok("hello".into())));
        let decoded: Result<Result<String, ToolExecError>, ToolError> =
            postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(decoded, Ok(Ok(v)) if v == "hello"));
    }

    #[test]
    fn encode_outer_ok_err() {
        let bytes = encode_outer::<String, ToolExecError>(Ok(Err(ToolExecError("fail".into()))));
        let decoded: Result<Result<String, ToolExecError>, ToolError> =
            postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(decoded, Ok(Err(e)) if e.to_string() == "fail"));
    }

    #[test]
    fn encode_outer_err_infra() {
        let bytes =
            encode_outer::<String, ToolExecError>(Err(ToolError::Other("infra fail".into())));
        let decoded: Result<Result<String, ToolExecError>, ToolError> =
            postcard::from_bytes(&bytes).unwrap();
        assert!(matches!(decoded, Err(e) if e.to_string() == "infra fail"));
    }

    // ── EmptyArgs deserialization tests ────────────────────────────

    #[test]
    fn empty_args_from_null() {
        let args: EmptyArgs = serde_json::from_str("null").unwrap();
        let _ = args;
    }

    #[test]
    fn empty_args_from_empty_object() {
        let args: EmptyArgs = serde_json::from_str("{}").unwrap();
        let _ = args;
    }

    #[test]
    fn empty_args_rejects_nonempty_object() {
        let result: Result<EmptyArgs, _> = serde_json::from_str(r#"{"key": "value"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn empty_args_schema_is_empty_object() {
        let schema = serde_json::to_value(schemars::schema_for!(EmptyArgs)).unwrap();
        let schema = sanitize_params_schema(schema);
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(false),
            "should forbid extra properties"
        );
    }

    #[test]
    fn describe_invocation_json_uses_tool_description_fallback_on_bad_args() {
        let tool = DefaultTool;
        let wrapper: Box<dyn ToolDyn> = Box::new(tool);
        // () deserializes from null, not from arbitrary strings or maps.
        let desc = wrapper.describe_invocation_json("\"this is a string\"");
        assert_eq!(desc, "A tool with default settings");
    }

    #[test]
    fn describe_invocation_json_returns_description_for_valid_args() {
        let tool = DefaultTool;
        let wrapper: Box<dyn ToolDyn> = Box::new(tool);
        // For type Args = (), valid JSON is "null".
        let desc = wrapper.describe_invocation_json("null");
        assert_eq!(desc, "A tool with default settings.");
    }

    #[test]
    fn describe_invocation_in_tool_output_is_populated_on_success() {
        let tool = DefaultTool;
        let wrapper: Box<dyn ToolDyn> = Box::new(tool);
        let (output_tx, _output_rx) = crossbeam_channel::unbounded();
        let result = wrapper
            .execute_streaming_json(
                "null",
                ToolOutputFormat::Text,
                None,
                None,
                output_tx,
                None,
                None,
            )
            .unwrap();
        assert!(
            !result.invocation_description.is_empty(),
            "invocation_description should be populated: {:?}",
            result.invocation_description,
        );
    }

    #[test]
    fn describe_invocation_in_tool_output_is_populated_on_execute_json() {
        let tool = DefaultTool;
        let wrapper: Box<dyn ToolDyn> = Box::new(tool);
        let result = wrapper
            .execute_json("null", ToolOutputFormat::Text, None, None, None, None)
            .unwrap();
        assert!(
            !result.invocation_description.is_empty(),
            "invocation_description should be populated: {:?}",
            result.invocation_description,
        );
    }

    #[test]
    fn non_streaming_tool_sends_no_chunk() {
        let tool = DefaultTool;
        let wrapper: Box<dyn ToolDyn> = Box::new(tool);
        let (output_tx, output_rx) = crossbeam_channel::unbounded();
        let result = wrapper
            .execute_streaming_json(
                "null",
                ToolOutputFormat::Text,
                None,
                None,
                output_tx,
                None,
                None,
            )
            .unwrap();
        assert!(
            !result.invocation_description.is_empty(),
            "invocation_description should be populated even for non-streaming tools: {:?}",
            result.invocation_description,
        );
        assert!(!result.is_error, "tool should succeed: {}", result.content);
        match output_rx.try_recv() {
            Err(crossbeam_channel::TryRecvError::Empty)
            | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                // expected — no chunk sent (channel may already be closed)
            }
            Ok(chunk) => {
                panic!(
                    "non-streaming tool should NOT send streaming chunks, got: {:?}",
                    chunk
                );
            }
        }
    }

    #[test]
    fn human_size_formats() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1 KiB");
        assert_eq!(human_size(1500), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1 MiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5 MiB");
        assert_eq!(human_size(100 * 1024 * 1024), "100 MiB");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_label_sanitizes_control_chars() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::TempDir::new().expect("temp dir");
        // A symlink whose *target* name contains a literal newline (legal on
        // POSIX) must render escaped so line-oriented output stays intact.
        let target_name = "evil\ntarget.txt";
        std::fs::write(dir.path().join(target_name), "hi").expect("write target");
        symlink(target_name, dir.path().join("link")).expect("symlink");
        let label = symlink_target_label(&dir.path().join("link"));
        assert_eq!(label, "evil\\ntarget.txt");
    }
}
