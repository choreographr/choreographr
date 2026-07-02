use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::primitives::Address;
use alloy::providers::Provider;
use std::str::FromStr;

use super::{EvmBalanceArgs, alloy_err, connect};

pub(crate) async fn execute_evm_balance_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_balance_inner(arguments_json).await {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

async fn execute_evm_balance_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: EvmBalanceArgs = serde_json::from_str(arguments_json)?;
    let output = evm_balance_impl(&args.rpc_url, &args.address).await?;
    Ok(truncate_tool_output(&output))
}

async fn evm_balance_impl(rpc_url: &str, address_str: &str) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;
    let address =
        Address::from_str(address_str).map_err(|e| ToolError::Other(format!("invalid address: {e}")))?;

    let balance = provider.get_balance(address).await.map_err(alloy_err)?;

    Ok(format!("address: {address_str}\nbalance: {balance} wei"))
}

define_tool!(EvmBalance, "evm_balance",
    "Query the native ETH/coin balance of an address on an EVM blockchain.",
    execute_evm_balance_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"address":{"type":"string","description":"0x-prefixed hex address"}},"required":["rpc_url","address"],"additionalProperties":false})
);
