use choreo_ai_protocols::ChatToolCall;
pub(crate) use choreo_ai_protocols::openai::AllowedCaller;
use choreo_ai_protocols::openai::ChatToolDefinition;
use choreo_keystore::ServiceCredential;
use humfmt::{BytesOptions, bytes_with};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
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
pub(crate) mod find;
pub(crate) mod fish;
pub(crate) mod fs;
pub(crate) mod git;
pub(crate) mod glob_util;
pub(crate) mod grep;
pub mod http;
mod image;
pub(crate) mod notify;
pub(crate) mod nu;
pub(crate) mod pdf;
pub(crate) mod random;
pub(crate) mod read_file;
pub(crate) mod read_file_range;
pub(crate) mod series;
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
    /// The tool's structured return value, captured after a successful
    /// execution (`serde_json::to_value(ret)`). `None` for error/timeout
    /// outputs and for returns that fail to serialize.  The request worker
    /// reads this to mirror session-config mutations (e.g. the canonical
    /// path from `set_working_dir`) onto its config copy without
    /// re-executing or re-resolving the tool's logic.
    pub result_json: Option<serde_json::Value>,
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

/// Strip `$schema`, `title`, and `$defs`/`$ref` patterns from a
/// schemars-generated JSON Schema so it is compatible with providers
/// that do not support JSON Schema Draft 2020-12 meta-schema features.
///
/// When `add_additional_properties` is true, inserts `additionalProperties: false`
/// at the root — suitable for tool `parameters` (object schemas), but not for
/// `output_schema` (which may be a non-object type).
fn sanitize_schema(
    mut schema: serde_json::Value,
    add_additional_properties: bool,
) -> serde_json::Value {
    let defs = schema.as_object_mut().and_then(|obj| {
        obj.remove("$schema");
        obj.remove("title");
        obj.remove("$defs")
    });
    if let Some(serde_json::Value::Object(defs_map)) = defs {
        resolve_refs(&mut schema, &defs_map);
    }
    if add_additional_properties && let Some(obj) = schema.as_object_mut() {
        obj.insert("additionalProperties".into(), false.into());
    }
    schema
}

fn sanitize_params_schema(schema: serde_json::Value) -> serde_json::Value {
    let mut s = sanitize_schema(schema, true);
    // Unit type () generates {"type": "null"} from schemars, but OpenAI
    // tool parameters must be a JSON Schema object. Convert to an empty
    // object schema which is the standard "no arguments" representation.
    if s.get("type") == Some(&serde_json::Value::String("null".into())) {
        s = serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });
    }
    s
}

fn sanitize_output_schema(schema: serde_json::Value) -> serde_json::Value {
    sanitize_schema(schema, false)
}

/// Recursively walk `value` and replace `{"$ref": "#/$defs/Name"}` with
/// the corresponding definition from `defs`.
fn resolve_refs(value: &mut serde_json::Value, defs: &serde_json::Map<String, serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(ref_path) = map.get("$ref").and_then(|v| v.as_str())
                && let Some(def_key) = ref_path.strip_prefix("#/$defs/")
                && let Some(resolved) = defs.get(def_key)
            {
                let mut resolved = resolved.clone();
                // Preserve any description carried alongside the $ref.
                if let Some(desc) = map.remove("description")
                    && let Some(resolved_obj) = resolved.as_object_mut()
                {
                    resolved_obj.insert("description".into(), desc);
                }
                *value = resolved;
                return;
            }
            for v in map.values_mut() {
                resolve_refs(v, defs);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                resolve_refs(v, defs);
            }
        }
        _ => {}
    }
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
        _output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Non-streaming tools deliver their result via TurnAppended —
        // no ToolResultChunk traffic needed.
        tracing::trace!("non-streaming tool called via execute_streaming, delegating to execute");
        self.execute(args, x_credentials, working_dir, ctx)
    }

    /// Optional: extract a PreparedImage from the return value.
    /// Only display_image overrides this.
    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> {
        None
    }

    /// Whether this tool produces streaming output via `execute_streaming`.
    ///
    /// When `true`, `execute_streaming_json` sends the invocation description as
    /// the first chunk so the client sees immediate context.  When `false` (the
    /// default for non-streaming tools like `read_file`), sending the description
    /// as a chunk would create a misleading stub `ToolResultRecord` on the client
    /// side with the description in `content` and empty `invocation_description`.
    /// The description arrives correctly through `ToolOutput.invocation_description`
    /// in the subsequent `TurnAppended`.
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
        output_tx: mpsc::Sender<Vec<u8>>,
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
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&context::ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let args = serde_json::from_str::<T::Args>(args_json)?;
        let desc = T::describe_invocation(self, &args);
        // Only send the invocation description as the first streaming chunk
        // for tools that override `execute_streaming`.  For non-streaming tools
        // the description arrives through `ToolOutput.invocation_description`
        // in the `TurnAppended`; sending it as a chunk would create a stub
        // `ToolResultRecord` on the client with the description in `content`
        // and empty `invocation_description`, masking the real output.
        if T::supports_streaming_output() {
            let _ = output_tx.send(desc.as_bytes().to_vec());
        }
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
            result_json: serde_json::to_value(&ret).ok(),
        })
    }
}

pub fn static_groups() -> &'static [ToolGroup] {
    static GROUPS: OnceLock<Vec<ToolGroup>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        vec![
            ToolGroup {
                name: "core".into(),
                description: "File system operations, HTTP requests, image display, PDF classification and Markdown extraction, file search, random values, time queries, and series execution".into(),
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
        output_tx: mpsc::Sender<Vec<u8>>,
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

/// Shared byte budget for tool output (128 KiB ≈ ~32K tokens for ASCII,
/// ~43K for CJK — far below any modern context window, yet a single call
/// can never flood the conversation). Measured in *bytes* rather than chars
/// so the effective token cost is roughly uniform across scripts: ASCII and
/// CJK both sit at ~3-4 bytes per token, whereas char counts vary 4x.
pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 128 * 1024;

/// Number of leading bytes inspected when deciding whether a file is binary.
/// Mirrors ripgrep's heuristic: a NUL byte in the head marks the file as
/// binary (text files virtually never contain NUL).
pub(crate) const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Per-line display cap for the file-read tools. Guards against pathological
/// single-line files (minified bundles, base64 blobs, 1 GiB one-liners) that
/// would otherwise force an unbounded line buffer into memory.
pub(crate) const MAX_LINE_DISPLAY_BYTES: usize = 64 * 1024;

pub(crate) fn truncate_tool_output(content: &str) -> String {
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return content.to_string();
    }
    // Cut on a char boundary so we never split a multi-byte UTF-8 char.
    let split = content.floor_char_boundary(MAX_TOOL_OUTPUT_BYTES);
    let mut truncated = content[..split].to_string();
    truncated.push_str("\n...[truncated]");
    truncated
}

/// Escape control characters and Unicode line/paragraph separators in a
/// string so a hostile name or content cannot corrupt the line-oriented tool
/// output (every entry must stay on exactly one line) or inject terminal
/// escape sequences.
///
/// - C0/C1 control characters (`char::is_control`) are escaped via
///   `escape_default` (`\n`, `\t`, `\u{1b}`, …).
/// - U+2028 LINE SEPARATOR / U+2029 PARAGRAPH SEPARATOR are **not**
///   `is_control` (categories Zl/Zp), yet terminals render them as line
///   breaks — they must be escaped to preserve the one-line-per-result
///   invariant.
/// - Bidi override/isolate characters (U+202A..=U+202E, U+2066..=U+2069,
///   category Cf) are invisible but can reorder/spoof rendered text, so they
///   are escaped too.
///
/// `keep_tabs` leaves TAB literal — legitimate in source-line *content* (grep
/// match/context lines) — while names still escape it.
pub(crate) fn sanitize_text(text: &str, keep_tabs: bool) -> String {
    // Fast path: ASCII printables (plus tabs when kept) — nothing to escape.
    // Multi-byte UTF-8 bytes are all >= 0x80, so any non-ASCII text falls
    // through to the slow path (it may hide a separator or bidi char).
    if text
        .bytes()
        .all(|b| (b == b'\t' && keep_tabs) || (0x20..=0x7e).contains(&b))
    {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if (c == '\t' && keep_tabs) || (!c.is_control() && !is_unsafe_unicode(c)) {
            out.push(c);
        } else {
            // escape_default renders the special escapes (`\t`, `\r`, `\n`,
            // …) and everything else control-related as `\u{...}` — all inert
            // ASCII text, so nothing terminal-affecting or line-splitting leaks.
            out.extend(c.escape_default());
        }
    }
    out
}

/// The subset of non-control Unicode that must still be escaped: line /
/// paragraph separators and bidi format characters (see [`sanitize_text`]).
fn is_unsafe_unicode(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
        || ('\u{202a}'..='\u{202e}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

/// Escape control characters in a name so a pathological name (e.g. one
/// containing a newline) cannot corrupt the line-oriented tool output — every
/// entry must stay on exactly one line for the LLM to parse the listing.
/// Tabs are escaped too, unlike [`sanitize_content`].
///
/// Shared by the line-oriented tools (`list_files`, `find`, `grep`) so a
/// hostile filename can't break any of them.
pub(crate) fn sanitize_name(name: &str) -> String {
    sanitize_text(name, false)
}

/// Escape control characters in matched line *content*, keeping tabs literal —
/// tabs are ubiquitous in code and harmless, while a hostile line (embedded
/// ESC, backspace, U+2028, …) must not corrupt output or inject terminal
/// escape sequences. Used by `grep` on match/context lines; path labels go
/// through the stricter [`sanitize_name`].
pub(crate) fn sanitize_content(content: &str) -> String {
    sanitize_text(content, true)
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

/// Cap `body` at the shared byte budget, then append `marker` (if any)
/// **after** the cap so the truncation signal always survives the byte cut.
/// The marker is short and critical ("N of many more"), so it is appended
/// past the budget — the same convention the file-read tools use for their
/// truncation markers (see ARCHITECTURE.md).
pub(crate) fn finish_tool_output(body: &str, marker: Option<String>) -> String {
    let mut out = truncate_tool_output(body);
    if let Some(marker) = marker {
        out.push_str(&format!("\n{marker}"));
    }
    out
}

/// Marker appended when a search tool (`find`/`grep`) stops at its
/// `max_results` cap, so the LLM can tell "exactly N results" from
/// "N of many more". `None` when the walk completed naturally.
///
/// Note the marker means **at least** N matches exist: it fires as soon as
/// the cap is hit, so a tree with exactly N matching entries also reports it
/// (proving "more exist" would require walking one extra entry).
pub(crate) fn truncation_marker(truncated: bool, cap: usize, noun: &str) -> Option<String> {
    truncated.then(|| format!("...[truncated at {cap} {noun}]"))
}

/// Open `path` for streaming text reads, rejecting binary files up front.
///
/// The first [`BINARY_SNIFF_BYTES`] are *peeked* via `fill_buf` (not
/// consumed), so the returned reader can continue streaming from the start
/// of the file. A NUL byte in the head marks the file as binary. Invalid
/// UTF-8 in the head is also rejected, unless the invalid sequence is a
/// multi-byte char merely split at the sniff boundary (`error_len() == None`)
/// — the per-line UTF-8 validation in the read tools handles that case.
pub(crate) fn open_text_reader(path: &std::path::Path) -> Result<BufReader<File>, ToolExecError> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(BINARY_SNIFF_BYTES, file);
    let head = reader.fill_buf()?;
    if let Some(pos) = head.iter().position(|&b| b == 0) {
        return Err(ToolExecError(format!(
            "'{}' appears to be a binary file (NUL byte at offset {pos}); \
             read_file/read_file_range are for UTF-8 text files",
            path.display()
        )));
    }
    if let Err(e) = std::str::from_utf8(head)
        && e.error_len().is_some()
    {
        return Err(ToolExecError(format!(
            "'{}' is not valid UTF-8 text (invalid byte sequence at offset {})",
            path.display(),
            e.valid_up_to()
        )));
    }
    Ok(reader)
}

/// Read one line (up to and including `\n`) into `buf`, stopping early once
/// `buf` reaches `cap` bytes.
///
/// Returns `Ok(true)` when the line is complete (terminated by `\n` or EOF)
/// and `Ok(false)` when the line is longer than `cap` — in that case `buf`
/// holds the first `cap` bytes (no trailing `\n`) and the caller should
/// drain the remainder with [`drain_rest_of_line`] before reading on.
/// Memory stays bounded: `buf` never grows past `cap`.
pub(crate) fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<bool> {
    buf.clear();
    loop {
        // Scope the `fill_buf` borrow so `available` is dropped before
        // `consume` re-borrows the reader (BufRead requires this).
        let (consumed, done) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                // EOF: a final partial line (possibly empty) counts as complete.
                return Ok(true);
            }
            let remaining = cap.saturating_sub(buf.len());
            if remaining == 0 {
                // Reached the display cap before finding a newline.
                return Ok(false);
            }
            let take = available.len().min(remaining);
            match available[..take].iter().position(|&b| b == b'\n') {
                Some(idx) => {
                    buf.extend_from_slice(&available[..=idx]);
                    (idx + 1, true)
                }
                None => {
                    buf.extend_from_slice(&available[..take]);
                    (take, false)
                }
            }
        };
        reader.consume(consumed);
        if done {
            return Ok(true);
        }
    }
}

/// Consume the remainder of an over-cap line (up to and including `\n`),
/// returning the number of bytes drained. Keeps line *counting* correct
/// after [`read_line_capped`] bailed out, without ever buffering the whole
/// line — a fixed chunk is reused, so memory stays O(1) in line size.
pub(crate) fn drain_rest_of_line<R: BufRead>(reader: &mut R) -> io::Result<u64> {
    let mut drained: u64 = 0;
    loop {
        let (consumed, done) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                // EOF mid-line: no trailing newline to find.
                return Ok(drained);
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(idx) => (idx + 1, true),
                None => (available.len(), false),
            }
        };
        drained += consumed as u64;
        reader.consume(consumed);
        if done {
            return Ok(drained);
        }
    }
}

/// One line streamed from a text file by [`TextStream`]: the capped byte
/// content plus the byte accounting the read tools need for accurate error
/// offsets and totals.
pub(crate) struct StreamedLine {
    /// 1-based line number within the file.
    pub line_number: u64,
    /// First [`MAX_LINE_DISPLAY_BYTES`] bytes of the line. For over-cap
    /// lines this is a prefix without the trailing `\n` (see `complete`).
    pub content: Vec<u8>,
    /// `true` when the line was fully read (terminated by `\n` or EOF);
    /// `false` when the display cap cut the line short.
    pub complete: bool,
    /// Byte offset of this line's first byte within the file, used to
    /// report NUL / invalid-UTF-8 positions accurately.
    pub start_offset: u64,
}

/// Streaming, memory-bounded line iterator shared by the file-read tools.
///
/// Wraps the [`read_line_capped`] / [`drain_rest_of_line`] helpers so
/// `read_file` and `read_file_range` don't each re-implement the loop:
/// memory stays bounded at one capped line regardless of file size, over-cap
/// lines are drained (counted, never buffered) so byte totals stay exact,
/// and EOF is signalled by `None`.
pub(crate) struct TextStream<R: BufRead> {
    reader: R,
    line_buf: Vec<u8>,
    lines_read: u64,
    total_bytes: u64,
    finished: bool,
}

impl<R: BufRead> TextStream<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            // Pre-size the line buffer to the display cap so long-line files
            // don't trigger repeated reallocations while growing.
            line_buf: Vec::with_capacity(MAX_LINE_DISPLAY_BYTES),
            lines_read: 0,
            total_bytes: 0,
            finished: false,
        }
    }

    /// Number of lines read so far (the file total once the iterator is
    /// exhausted). Over-cap lines count as one line each.
    pub(crate) fn total_lines(&self) -> u64 {
        self.lines_read
    }

    /// Total file bytes consumed so far — exact even for over-cap lines,
    /// whose tails are drained rather than buffered.
    pub(crate) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

impl<R: BufRead> Iterator for TextStream<R> {
    type Item = io::Result<StreamedLine>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        // `read_line_capped` clears the buffer on entry, so the previous
        // line's bytes never linger across iterations.
        let complete =
            match read_line_capped(&mut self.reader, &mut self.line_buf, MAX_LINE_DISPLAY_BYTES) {
                Ok(complete) => complete,
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            };
        if self.line_buf.is_empty() {
            // EOF (empty file, or the trailing newline was already consumed):
            // there is no extra final line to report.
            self.finished = true;
            return None;
        }
        let start_offset = self.total_bytes;
        let line_total = if complete {
            self.line_buf.len() as u64
        } else {
            // Over-cap line: count its full length (draining keeps memory
            // bounded) but hand back only the capped prefix below.
            match drain_rest_of_line(&mut self.reader) {
                Ok(drained) => self.line_buf.len() as u64 + drained,
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        };
        self.total_bytes += line_total;
        self.lines_read += 1;
        Some(Ok(StreamedLine {
            line_number: self.lines_read,
            content: self.line_buf.clone(),
            complete,
            start_offset,
        }))
    }
}

/// Accumulates tool output line-by-line under the shared byte budget so a
/// single tool call can never flood the conversation with more than
/// [`MAX_TOOL_OUTPUT_BYTES`] of returned content.
///
/// Once the budget is exhausted the budget is marked truncated and further
/// pushes are rejected; the caller keeps counting lines/bytes for an honest
/// truncation report but stops validating content it will never return.
pub(crate) struct OutputBudget {
    max_bytes: usize,
    shown_bytes: usize,
    truncated: bool,
}

impl OutputBudget {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            shown_bytes: 0,
            truncated: false,
        }
    }

    /// Bytes of content accepted so far (excluding anything rejected after
    /// truncation and the caller's trailing marker/header).
    pub(crate) fn shown_bytes(&self) -> usize {
        self.shown_bytes
    }

    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Append `line` plus a trailing newline, honoring the budget. Returns
    /// `true` when the line fit and was appended; `false` — marking the
    /// output truncated — when the budget is exhausted.
    pub(crate) fn push_line(&mut self, out: &mut String, line: &str) -> bool {
        // +1 accounts for the newline re-appended below.
        let display_len = line.len() + 1;
        if self.truncated || self.shown_bytes + display_len > self.max_bytes {
            self.truncated = true;
            return false;
        }
        out.push_str(line);
        out.push('\n');
        self.shown_bytes += display_len;
        true
    }
}

/// Validate and render one streamed line for tool output.
///
/// Shared by `read_file` and `read_file_range`: rejects NUL bytes and
/// invalid UTF-8 in lines that are actually returned (reporting the byte
/// offset into the file), normalizes line endings to `str::lines()`
/// semantics (strip one `\n`, then one `\r`), and appends a
/// `...[line truncated]` marker when the display cap cut the line short.
/// With `numbered`, the line is prefixed with its 1-based file line number
/// (`read_file_range` rendering).
pub(crate) fn render_streamed_line(
    line: &StreamedLine,
    path: &std::path::Path,
    numbered: bool,
) -> Result<String, ToolExecError> {
    if let Some(pos) = line.content.iter().position(|&b| b == 0) {
        return Err(ToolExecError(format!(
            "'{}' appears to be a binary file (NUL byte at offset {})",
            path.display(),
            line.start_offset + pos as u64
        )));
    }
    let line_str = match std::str::from_utf8(&line.content) {
        Ok(s) => s,
        Err(e) if !line.complete && e.error_len().is_none() => {
            // The display cap split a multi-byte char mid-sequence; the
            // prefix before the split is valid and that is all we show.
            std::str::from_utf8(&line.content[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(e) => {
            return Err(ToolExecError(format!(
                "'{}' is not valid UTF-8 text (invalid byte sequence at offset {})",
                path.display(),
                line.start_offset + e.valid_up_to() as u64
            )));
        }
    };

    // Match `str::lines()` display semantics: strip one trailing '\n' and
    // then one trailing '\r', then re-append a single '\n'.
    let mut display = line_str;
    if let Some(stripped) = display.strip_suffix('\n') {
        display = stripped;
    }
    if let Some(stripped) = display.strip_suffix('\r') {
        display = stripped;
    }
    let mut display_line = String::new();
    if numbered {
        display_line.push_str(&format!("{} | {display}", line.line_number));
    } else {
        display_line.push_str(display);
    }
    if !line.complete {
        display_line.push_str("\n...[line truncated: exceeds 64 KiB]");
    }
    Ok(display_line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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

    // ── sanitize_schema tests ─────────────────────────────────────────

    #[test]
    fn sanitize_schema_strips_metadata() {
        let input = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "MySchema",
            "$defs": { "Foo": { "type": "string" } },
            "type": "object"
        });
        let result = super::sanitize_schema(input, false);
        assert!(result.get("$schema").is_none(), "should strip $schema");
        assert!(result.get("title").is_none(), "should strip title");
        assert!(result.get("$defs").is_none(), "should strip $defs");
        assert_eq!(result["type"], "object");
    }

    #[test]
    fn sanitize_schema_inlines_refs() {
        let input = serde_json::json!({
            "$defs": { "Point": { "type": "object", "properties": { "x": {"type": "integer"} } } },
            "type": "object",
            "properties": {
                "location": { "$ref": "#/$defs/Point" }
            }
        });
        let result = super::sanitize_schema(input, false);
        // The $ref should have been replaced by the definition inlined
        let location = &result["properties"]["location"];
        assert!(location.get("$ref").is_none(), "$ref should be resolved");
        assert_eq!(location["type"], "object");
        assert_eq!(location["properties"]["x"]["type"], "integer");
    }

    #[test]
    fn sanitize_schema_preserves_description_across_ref() {
        let input = serde_json::json!({
            "$defs": { "Str": { "type": "string" } },
            "items": { "$ref": "#/$defs/Str", "description": "A string item" }
        });
        let result = super::sanitize_schema(input, false);
        assert_eq!(result["items"]["type"], "string");
        assert_eq!(result["items"]["description"], "A string item");
    }

    #[test]
    fn sanitize_schema_adds_additional_properties() {
        let input = serde_json::json!({ "type": "object", "properties": {} });
        let result = super::sanitize_schema(input, true);
        assert_eq!(result["additionalProperties"], false);
    }

    #[test]
    fn sanitize_schema_skips_additional_properties_when_false() {
        let input = serde_json::json!({ "type": "string" });
        let result = super::sanitize_schema(input, false);
        assert!(result.get("additionalProperties").is_none());
    }

    #[test]
    fn sanitize_schema_passthrough_clean_schema() {
        let input = serde_json::json!({ "type": "integer" });
        let result = super::sanitize_schema(input.clone(), false);
        assert_eq!(result, input);
    }

    #[test]
    fn sanitize_schema_resolves_refs_in_arrays() {
        let input = serde_json::json!({
            "$defs": { "Tag": { "type": "string" } },
            "type": "array",
            "prefixItems": [
                { "$ref": "#/$defs/Tag" },
                { "type": "integer" }
            ]
        });
        let result = super::sanitize_schema(input, false);
        assert!(result["prefixItems"][0].get("$ref").is_none());
        assert_eq!(result["prefixItems"][0]["type"], "string");
        assert_eq!(result["prefixItems"][1]["type"], "integer");
    }

    // ── sanitize_params_schema tests ──────────────────────────────────

    #[test]
    fn sanitize_params_schema_converts_null_to_object() {
        // Unit type () generates {"type": "null"} from schemars.
        let input = serde_json::json!({ "type": "null" });
        let result = super::sanitize_params_schema(input);
        assert_eq!(result["type"], "object");
        assert_eq!(result["properties"], serde_json::json!({}));
        assert_eq!(result["additionalProperties"], false);
    }

    #[test]
    fn sanitize_params_schema_preserves_normal_schema() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let result = super::sanitize_params_schema(input);
        assert_eq!(result["type"], "object");
        assert_eq!(result["properties"]["name"]["type"], "string");
        assert_eq!(result["additionalProperties"], false);
    }

    #[test]
    fn sanitize_params_schema_strips_schema_title_defs() {
        let input = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Args",
            "$defs": { "X": { "type": "string" } },
            "type": "object"
        });
        let result = super::sanitize_params_schema(input);
        assert!(result.get("$schema").is_none());
        assert!(result.get("title").is_none());
        assert!(result.get("$defs").is_none());
    }

    // ── sanitize_output_schema tests ──────────────────────────────────

    #[test]
    fn sanitize_output_schema_no_additional_properties() {
        let input = serde_json::json!({ "type": "string" });
        let result = super::sanitize_output_schema(input);
        assert_eq!(result["type"], "string");
        assert!(result.get("additionalProperties").is_none());
    }

    #[test]
    fn sanitize_output_schema_strips_metadata() {
        let input = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Return",
            "type": "integer"
        });
        let result = super::sanitize_output_schema(input);
        assert!(result.get("$schema").is_none());
        assert!(result.get("title").is_none());
    }

    // ── resolve_refs tests ────────────────────────────────────────────

    #[test]
    fn resolve_refs_basic() {
        let mut value = serde_json::json!({ "$ref": "#/$defs/MyType" });
        let defs = [("MyType".to_string(), serde_json::json!({"type": "string"}))]
            .into_iter()
            .collect();
        super::resolve_refs(&mut value, &defs);
        assert_eq!(value, serde_json::json!({"type": "string"}));
    }

    #[test]
    fn resolve_refs_no_match_unchanged() {
        let original = serde_json::json!({ "$ref": "#/$defs/Unknown" });
        let mut value = original.clone();
        let defs = serde_json::Map::new();
        super::resolve_refs(&mut value, &defs);
        // Unknown refs are left as-is (schemars shouldn't produce these).
        assert_eq!(value, original);
    }

    #[test]
    fn resolve_refs_no_ref_unchanged() {
        let original = serde_json::json!({ "type": "object", "properties": {} });
        let mut value = original.clone();
        let defs = serde_json::Map::new();
        super::resolve_refs(&mut value, &defs);
        assert_eq!(value, original);
    }

    #[test]
    fn resolve_refs_nested_skipped() {
        // Known limitation: resolve_refs does NOT recursively resolve
        // $refs inside the resolved definition. If $defs/B points to
        // $defs/A, only the first level is resolved.
        let mut value = serde_json::json!({ "$ref": "#/$defs/B" });
        let mut defs = serde_json::Map::new();
        defs.insert("A".into(), serde_json::json!({"type": "string"}));
        defs.insert("B".into(), serde_json::json!({"$ref": "#/$defs/A"}));
        super::resolve_refs(&mut value, &defs);
        // B resolves to {"$ref": "#/$defs/A"} — nested ref is NOT resolved.
        assert_eq!(value, serde_json::json!({"$ref": "#/$defs/A"}));
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
        let (output_tx, _output_rx) = mpsc::channel();
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
        let (output_tx, output_rx) = mpsc::channel();
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
            Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => {
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
    fn text_stream_counts_lines_bytes_and_offsets() {
        use std::io::Cursor;
        let mut stream = TextStream::new(Cursor::new(b"a\nbb\nccc\n".to_vec()));
        let lines: Vec<StreamedLine> = stream.by_ref().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_number, 1);
        assert_eq!(lines[0].content, b"a\n");
        assert_eq!(lines[0].start_offset, 0);
        assert_eq!(lines[1].line_number, 2);
        assert_eq!(lines[1].content, b"bb\n");
        assert_eq!(lines[1].start_offset, 2);
        assert_eq!(lines[2].line_number, 3);
        assert_eq!(lines[2].content, b"ccc\n");
        assert_eq!(lines[2].start_offset, 5);
        assert_eq!(stream.total_lines(), 3);
        assert_eq!(stream.total_bytes(), 9);
    }

    #[test]
    fn text_stream_handles_over_cap_lines() {
        use std::io::Cursor;
        // A single 70 KiB line exceeds the 64 KiB display cap: the iterator
        // hands back the capped prefix yet counts the full length.
        let content = vec![b'x'; 70 * 1024];
        let mut stream = TextStream::new(Cursor::new(content.clone()));
        let line = stream.next().unwrap().unwrap();
        assert!(!line.complete);
        assert_eq!(line.content.len(), MAX_LINE_DISPLAY_BYTES);
        assert_eq!(stream.total_bytes(), content.len() as u64);
        assert!(stream.next().is_none());
    }

    #[test]
    fn output_budget_rejects_lines_past_cap() {
        let mut out = String::new();
        let mut budget = OutputBudget::new(10);
        assert!(budget.push_line(&mut out, "abc")); // 4 bytes
        assert!(budget.push_line(&mut out, "def")); // 8 bytes
        assert!(!budget.push_line(&mut out, "ghi")); // 12 > 10 → truncated
        assert!(budget.is_truncated());
        assert_eq!(budget.shown_bytes(), 8);
        assert_eq!(out, "abc\ndef\n");
        // Pushes after truncation are rejected without growing the output.
        assert!(!budget.push_line(&mut out, "x"));
        assert_eq!(out, "abc\ndef\n");
    }

    #[test]
    fn render_streamed_line_rejects_binary_and_bad_utf8() {
        let path = Path::new("f.txt");
        let nul = StreamedLine {
            line_number: 1,
            content: b"ok\x00no".to_vec(),
            complete: true,
            start_offset: 0,
        };
        let err = render_streamed_line(&nul, path, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("binary file"), "{err}");

        let bad = StreamedLine {
            line_number: 2,
            content: b"ok\xff".to_vec(),
            complete: true,
            start_offset: 10,
        };
        let err = render_streamed_line(&bad, path, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        // Offsets are reported relative to the file, not the line.
        assert!(err.contains("offset 12"), "{err}");
    }

    #[test]
    fn render_streamed_line_normalizes_endings_and_numbers() {
        let path = Path::new("f.txt");
        let line = StreamedLine {
            line_number: 3,
            content: b"hi\r\n".to_vec(),
            complete: true,
            start_offset: 0,
        };
        assert_eq!(render_streamed_line(&line, path, true).unwrap(), "3 | hi");
        assert_eq!(render_streamed_line(&line, path, false).unwrap(), "hi");
    }

    #[test]
    fn render_streamed_line_handles_mid_char_cap_cut() {
        // 3-byte chars where 65536 % 3 == 1 guarantee the cap cut lands
        // mid-character; the rendered line must stay valid UTF-8.
        let path = Path::new("f.txt");
        let content = "€".repeat(21846); // 65_538 bytes
        let line = StreamedLine {
            line_number: 1,
            content: content.into_bytes(),
            complete: false,
            start_offset: 0,
        };
        let out = render_streamed_line(&line, path, false).unwrap();
        assert!(out.contains("...[line truncated: exceeds 64 KiB]"), "{out}");
        std::str::from_utf8(out.as_bytes()).expect("output must be valid UTF-8");
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

    #[test]
    fn sanitize_name_escapes_control_chars() {
        assert_eq!(sanitize_name("plain.txt"), "plain.txt");
        assert_eq!(sanitize_name("a\nb"), "a\\nb");
        assert_eq!(sanitize_name("a\tb"), "a\\tb");
    }

    #[test]
    fn sanitize_name_escapes_unicode_separators_and_bidi() {
        // U+2028/U+2029 are Zl/Zp — not is_control — but terminals render
        // them as line breaks, so they must be escaped to keep the
        // one-line-per-result invariant. Bidi format chars are invisible but
        // can reorder rendered text.
        assert_eq!(sanitize_name("a\u{2028}b"), "a\\u{2028}b");
        assert_eq!(sanitize_name("a\u{2029}b"), "a\\u{2029}b");
        assert_eq!(sanitize_name("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(sanitize_name("a\u{2066}b"), "a\\u{2066}b");
        // Non-ASCII but safe chars pass through untouched.
        assert_eq!(sanitize_name("café"), "café");
    }

    #[test]
    fn sanitize_content_keeps_tabs_but_escapes_separators() {
        // Content keeps tabs literal (legitimate in source) but still escapes
        // every other control/separator.
        assert_eq!(sanitize_content("a\tb"), "a\tb");
        assert_eq!(sanitize_content("a\nb"), "a\\nb");
        assert_eq!(sanitize_content("a\u{2028}b"), "a\\u{2028}b");
        assert_eq!(sanitize_content("a\u{2029}b"), "a\\u{2029}b");
        assert_eq!(sanitize_content("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(sanitize_content("a\u{1b}b"), "a\\u{1b}b");
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

    #[test]
    fn finish_tool_output_keeps_marker_past_byte_cap() {
        // A body larger than the shared byte budget: the byte-cap truncation
        // marker appears, and the caller's marker must survive appended after
        // it — the count signal is the whole point of the marker.
        let big = "x".repeat(super::MAX_TOOL_OUTPUT_BYTES + 100);
        let out = finish_tool_output(&big, Some("...[truncated at 5 results]".to_string()));
        assert!(out.contains("...[truncated]"), "expected byte-cap marker");
        assert!(
            out.ends_with("...[truncated at 5 results]"),
            "marker must survive the cap: …{}",
            &out[out.len().saturating_sub(60)..]
        );
    }

    #[test]
    fn finish_tool_output_without_marker_is_plain_cap() {
        let body = "a\nb";
        assert_eq!(finish_tool_output(body, None), body);
    }

    #[test]
    fn truncation_marker_only_when_capped() {
        assert_eq!(truncation_marker(false, 50, "results"), None);
        assert_eq!(
            truncation_marker(true, 50, "results").as_deref(),
            Some("...[truncated at 50 results]")
        );
        assert_eq!(
            truncation_marker(true, 200, "matches").as_deref(),
            Some("...[truncated at 200 matches]")
        );
    }
}
