use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::providers::Provider;

use super::{RpcUrlArgs, alloy_err, connect};

pub(crate) fn execute_evm_chain_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_chain_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_evm_chain_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: RpcUrlArgs = serde_json::from_str(arguments_json)?;
    let output = tokio::runtime::Handle::current().block_on(evm_chain_impl(&args.rpc_url))?;
    Ok(truncate_tool_output(&output))
}

async fn evm_chain_impl(rpc_url: &str) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;

    let chain_id = provider.get_chain_id().await.map_err(alloy_err)?;
    let block_number = provider.get_block_number().await.map_err(alloy_err)?;
    let gas_price = provider.get_gas_price().await.map_err(alloy_err)?;
    let client_version = provider.get_client_version().await.map_err(alloy_err)?;
    let max_priority_fee = provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(alloy_err)?;

    let mut out = String::new();
    out.push_str(&format!("chain_id: {chain_id}\n"));
    out.push_str(&format!("block_number: {block_number}\n"));
    out.push_str(&format!("gas_price: {gas_price} wei\n"));
    out.push_str(&format!("max_priority_fee: {max_priority_fee} wei\n"));
    out.push_str(&format!("client_version: {client_version}"));
    Ok(out)
}

define_tool!(EvmChain, "evm_chain",
    "Query information about an EVM blockchain node: chain ID, latest block number, gas price, max priority fee, and client version.",
    execute_evm_chain_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node (e.g., https://ethereum-rpc.publicnode.com)"}},"required":["rpc_url"],"additionalProperties":false})
);
