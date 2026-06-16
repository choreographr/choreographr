use crate::openai::{ChatToolCall, ChatToolDefinition};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

mod fs;
mod http;
mod image;
mod fff;
mod evm;
mod git;
mod subxt;

#[derive(Debug, Clone)]
pub(crate) struct ToolResult {
    pub(crate) content: String,
    pub(crate) is_error: bool,
}

#[derive(Debug)]
pub(crate) struct ToolExecutionOutput {
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

pub(crate) use image::emit_prepared_image;

#[async_trait]
pub(crate) trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput;
}

pub(crate) struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut reg = Self { tools: Vec::new() };
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
        reg.register(subxt::SubxtChain);
        reg.register(subxt::SubxtBalance);
        reg.register(subxt::SubxtQuery);
        reg.register(subxt::SubxtBlock);
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
        reg
    }

    fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.push(Box::new(tool));
    }

    pub async fn execute(&self, tool_call: &ChatToolCall) -> ToolExecutionOutput {
        match self.tools.iter().find(|t| t.name() == tool_call.name) {
            Some(tool) => tool.execute(&tool_call.arguments_json).await,
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
        self.tools
            .iter()
            .map(|t| ChatToolDefinition::function(t.name(), t.description(), t.schema()))
            .collect()
    }
}

fn global_registry() -> &'static ToolRegistry {
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(ToolRegistry::new)
}

pub(crate) async fn execute_tool_call(tool_call: &ChatToolCall) -> ToolExecutionOutput {
    global_registry().execute(tool_call).await
}

pub(crate) fn available_tools() -> Vec<ChatToolDefinition> {
    global_registry().available_definitions()
}

#[cfg(test)]
pub(crate) async fn execute_read_file_range_tool(arguments_json: &str) -> ToolResult {
    fs::execute_read_file_range_tool(arguments_json).await
}

#[cfg(test)]
pub(crate) async fn execute_write_file_tool(arguments_json: &str) -> ToolResult {
    fs::execute_write_file_tool(arguments_json).await
}

#[cfg(test)]
pub(crate) async fn execute_edit_file_tool(arguments_json: &str) -> ToolResult {
    fs::execute_edit_file_tool(arguments_json).await
}

#[cfg(test)]
pub(crate) async fn execute_http_request_tool(arguments_json: &str) -> ToolResult {
    http::execute_http_request_tool(arguments_json).await
}

pub(crate) fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("{digest:x}")
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
