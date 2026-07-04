use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::primitives::Address;
use alloy::providers::Provider;
use std::str::FromStr;

use super::{EvmNonceArgs, alloy_err, connect};

pub(crate) fn execute_evm_nonce_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_nonce_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_evm_nonce_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: EvmNonceArgs = serde_json::from_str(arguments_json)?;
    let output =
        tokio::runtime::Handle::current().block_on(evm_nonce_impl(&args.rpc_url, &args.address))?;
    Ok(truncate_tool_output(&output))
}

async fn evm_nonce_impl(rpc_url: &str, address_str: &str) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;
    let address = Address::from_str(address_str)
        .map_err(|e| ToolError::Other(format!("invalid address: {e}")))?;

    let nonce = provider
        .get_transaction_count(address)
        .await
        .map_err(alloy_err)?;

    Ok(format!(
        "address: {address_str}\ntransaction_count (nonce): {nonce}"
    ))
}

define_tool!(
    EvmNonce,
    "evm_nonce",
    "Get the transaction count (nonce) for an address on an EVM blockchain.",
    execute_evm_nonce_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"address":{"type":"string","description":"0x-prefixed hex address"}},"required":["rpc_url","address"],"additionalProperties":false})
);
