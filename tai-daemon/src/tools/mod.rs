use crate::openai::{ChatToolCall, ChatToolDefinition};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tai_keystore::XCredentials;

#[macro_export]
macro_rules! define_tool {
    ($struct:ident, $name:literal, $desc:literal, $exec_fn:path, $schema:expr) => {
        pub(crate) struct $struct;
        impl $crate::tools::Tool for $struct {
            fn name(&self) -> &'static str {
                $name
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn schema(&self) -> serde_json::Value {
                $schema
            }
            fn execute(
                &self,
                args: &str,
                _x_credentials: Option<&tai_keystore::XCredentials>,
                _cwd: Option<&std::path::Path>,
            ) -> $crate::tools::ToolExecutionOutput {
                $crate::tools::ToolExecutionOutput {
                    result: $exec_fn(args),
                    image: None,
                }
            }
        }
    };
}

#[macro_export]
macro_rules! define_tool_with_cwd {
    ($struct:ident, $name:literal, $desc:literal, $exec_fn:path, $schema:expr) => {
        pub(crate) struct $struct;
        impl $crate::tools::Tool for $struct {
            fn name(&self) -> &'static str {
                $name
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn schema(&self) -> serde_json::Value {
                $schema
            }
            fn execute(
                &self,
                args: &str,
                _x_credentials: Option<&tai_keystore::XCredentials>,
                cwd: Option<&std::path::Path>,
            ) -> $crate::tools::ToolExecutionOutput {
                $crate::tools::ToolExecutionOutput {
                    result: $exec_fn(args, cwd),
                    image: None,
                }
            }
        }
    };
}

mod error;
pub(crate) use error::{ToolError, tool_err, tool_ok};

mod evm;
mod fff;
pub(crate) mod fs;
pub(crate) mod git;
pub(crate) mod http;
mod image;
pub(crate) mod sessions;
pub(crate) mod skill;
pub(crate) mod subsession;
pub(crate) mod x;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug)]
pub struct ToolExecutionOutput {
    pub(crate) result: ToolResult,
    pub(crate) image: Option<PreparedImage>,
}

#[derive(Debug)]
pub(crate) struct PreparedImage {
    pub(crate) mime_type: String,
    pub(crate) data: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) alt: Option<String>,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    fn execute(
        &self,
        arguments_json: &str,
        x_credentials: Option<&XCredentials>,
        cwd: Option<&std::path::Path>,
    ) -> ToolExecutionOutput;
}

pub struct ToolRegistry {
    tools: HashMap<&'static str, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            tools: HashMap::new(),
        };
        reg.register(fs::ReadFile);
        reg.register(fs::ReadFileRange);
        reg.register(fs::ListFiles);
        reg.register(fs::LineCount);
        reg.register(http::HttpRequest);
        reg.register(fs::WriteFile);
        reg.register(fs::EditFile);
        reg.register(image::DisplayImage);
        reg.register(git::GitStatus);
        reg.register(git::GitDiff);
        reg.register(git::GitLog);
        reg.register(git::GitAdd);
        reg.register(git::GitCommit);
        reg.register(git::GitPush);
        reg.register(fff::Fff);
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
        reg.register(x::XPost);
        reg.register(x::XSearchRecent);
        reg.register(x::XUserLookup);
        reg
    }

    fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.name();
        self.tools.insert(name, Box::new(tool));
    }

    pub fn execute(
        &self,
        tool_call: &ChatToolCall,
        x_credentials: Option<&XCredentials>,
        cwd: Option<&std::path::Path>,
    ) -> ToolExecutionOutput {
        match self.tools.get(tool_call.name.as_str()) {
            Some(tool) => tool.execute(&tool_call.arguments_json, x_credentials, cwd),
            None => ToolExecutionOutput {
                result: ToolResult {
                    content: format!("unknown tool: {}", tool_call.name),
                    is_error: true,
                },
                image: None,
            },
        }
    }

    pub fn available_definitions(&self) -> Vec<ChatToolDefinition> {
        let mut defs: Vec<_> = self
            .tools
            .values()
            .map(|t| ChatToolDefinition::function(t.name(), t.description(), t.schema()))
            .collect();
        defs.push(subsession::spawn_subsession_definition());
        defs.push(sessions::list_sessions_definition());
        defs.push(sessions::get_session_definition());
        defs.push(skill::load_skill_definition());
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
