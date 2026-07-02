use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::eth::TransactionRequest;
use std::str::FromStr;

use super::{EvmCallArgs, alloy_err, connect};

pub(crate) async fn execute_evm_call_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_call_inner(arguments_json).await {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

async fn execute_evm_call_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: EvmCallArgs = serde_json::from_str(arguments_json)?;
    let output =
        evm_call_impl(&args.rpc_url, &args.to, &args.data, args.block_tag.as_deref()).await?;
    Ok(truncate_tool_output(&output))
}

async fn evm_call_impl(
    rpc_url: &str,
    to_str: &str,
    data_str: &str,
    _block_tag: Option<&str>,
) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;
    let to = Address::from_str(to_str).map_err(|e| ToolError::Other(format!("invalid 'to' address: {e}")))?;

    let data_hex = data_str
        .strip_prefix("0x")
        .or_else(|| data_str.strip_prefix("0X"))
        .unwrap_or(data_str);
    let input_data =
        hex::decode(data_hex).map_err(|e| ToolError::Other(format!("invalid hex data: {e}")))?;
    let tx = TransactionRequest::default()
        .with_to(to)
        .with_input(input_data);

    let result: Bytes = provider.call(tx).await.map_err(alloy_err)?;

    Ok(format!(
        "to: {to_str}\ndata: {data_str}\nresult: 0x{}",
        hex::encode(result.as_ref())
    ))
}

define_tool!(EvmCall, "evm_call",
    "Execute a read-only smart contract call (eth_call) on an EVM blockchain. Returns the raw hex-encoded result bytes.",
    execute_evm_call_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"to":{"type":"string","description":"0x-prefixed contract address to call"},"data":{"type":"string","description":"0x-prefixed hex-encoded call data (method selector + ABI-encoded params)"},"block_tag":{"type":"string","description":"Block number (decimal or 0x-hex), or 'latest', 'finalized', 'safe', 'pending', 'earliest'","default":"latest"}},"required":["rpc_url","to","data"],"additionalProperties":false})
);
