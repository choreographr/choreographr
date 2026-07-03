use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::rpc::types::eth::Filter;
use std::str::FromStr;

use super::{EvmLogsArgs, alloy_err, connect, parse_block_tag};

pub(crate) fn execute_evm_logs_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_logs_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_evm_logs_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: EvmLogsArgs = serde_json::from_str(arguments_json)?;
    let output = tokio::runtime::Handle::current().block_on(evm_logs_impl(
        &args.rpc_url,
        args.address.as_deref(),
        args.topic0.as_deref(),
        args.from_block.as_deref(),
        args.to_block.as_deref(),
    ))?;
    Ok(truncate_tool_output(&output))
}

async fn evm_logs_impl(
    rpc_url: &str,
    address_str: Option<&str>,
    topic0_str: Option<&str>,
    from_block_str: Option<&str>,
    to_block_str: Option<&str>,
) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;

    let mut filter = Filter::new();

    if let Some(addr) = address_str {
        let address =
            Address::from_str(addr).map_err(|e| ToolError::Other(format!("invalid address: {e}")))?;
        filter = filter.address(address);
    }

    if let Some(t0) = topic0_str {
        let stripped = t0.strip_prefix("0x").unwrap_or(t0);
        let topic =
            B256::from_str(stripped).map_err(|e| ToolError::Other(format!("invalid topic0: {e}")))?;
        filter = filter.event_signature(topic);
    }

    if let Some(fb) = from_block_str {
        filter = filter.from_block(parse_block_tag(fb)?);
    }

    if let Some(tb) = to_block_str {
        filter = filter.to_block(parse_block_tag(tb)?);
    }

    let logs = provider.get_logs(&filter).await.map_err(alloy_err)?;

    if logs.is_empty() {
        return Ok("no logs found for this filter".to_string());
    }

    let mut out = String::new();
    out.push_str(&format!("log_count: {}\n\n", logs.len()));

    for (i, log) in logs.iter().enumerate() {
        let log_address = log.address();
        let topics: Vec<String> = log.topics().iter().map(|t| format!("{t:#x}")).collect();
        let data_hex = hex::encode(log.data().data.clone());
        out.push_str(&format!(
            "log[{}]:\n  address: {log_address}\n  topics: [{}]\n  data: 0x{data_hex}\n\n",
            i,
            topics.join(", ")
        ));
    }

    Ok(out)
}

define_tool!(EvmLogs, "evm_logs",
    "Query event logs on an EVM blockchain with optional filters by contract address, topic0, and block range.",
    execute_evm_logs_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"address":{"type":"string","description":"Optional 0x-prefixed contract address to filter logs by"},"topic0":{"type":"string","description":"Optional 0x-prefixed event signature hash (topic0) to filter by"},"from_block":{"type":"string","description":"Optional starting block number or tag (e.g., '0x0', 'latest')"},"to_block":{"type":"string","description":"Optional ending block number or tag (e.g., '0x0', 'latest')"}},"required":["rpc_url"],"additionalProperties":false})
);
