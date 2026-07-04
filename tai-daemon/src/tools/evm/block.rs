use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::providers::Provider;
use alloy::rpc::types::eth::BlockNumberOrTag;

use super::{EvmBlockArgs, alloy_err, connect, parse_block_tag};

pub(crate) fn execute_evm_block_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_block_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_evm_block_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: EvmBlockArgs = serde_json::from_str(arguments_json)?;
    let output = crate::runtime::get().block_on(evm_block_impl(&args.rpc_url, args.block_tag.as_deref()))?;
    Ok(truncate_tool_output(&output))
}

async fn evm_block_impl(rpc_url: &str, block_tag: Option<&str>) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;

    let block_num = match block_tag {
        Some(tag) => parse_block_tag(tag)?,
        None => BlockNumberOrTag::Latest,
    };

    let block = provider
        .get_block_by_number(block_num)
        .await
        .map_err(alloy_err)?
        .ok_or_else(|| ToolError::Other("block not found".to_string()))?;

    let number = block.header.number;
    let hash = block.header.hash;
    let timestamp = block.header.timestamp;
    let gas_used = block.header.gas_used;
    let gas_limit = block.header.gas_limit;
    let tx_count = block.transactions.len();
    let base_fee = block.header.base_fee_per_gas.unwrap_or(0);

    let mut out = String::new();
    out.push_str(&format!("block: #{number}\n"));
    out.push_str(&format!("hash: {hash:#x}\n"));
    out.push_str(&format!("timestamp: {timestamp}\n"));
    out.push_str(&format!("transactions: {tx_count}\n"));
    out.push_str(&format!("gas_used: {gas_used}\n"));
    out.push_str(&format!("gas_limit: {gas_limit}\n"));
    out.push_str(&format!("base_fee: {base_fee} wei"));
    Ok(out)
}

define_tool!(
    EvmBlock,
    "evm_block",
    "Get details about a block on an EVM blockchain: block number, hash, timestamp, transaction count, gas used/limit, and base fee.",
    execute_evm_block_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"block_tag":{"type":"string","description":"Block number (decimal or 0x-hex), or 'latest', 'finalized', 'safe', 'pending', 'earliest'","default":"latest"}},"required":["rpc_url"],"additionalProperties":false})
);
