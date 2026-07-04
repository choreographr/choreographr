use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::primitives::B256;
use alloy::providers::Provider;
use std::str::FromStr;

use super::{EvmTransactionArgs, alloy_err, connect};

pub(crate) fn execute_evm_transaction_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_transaction_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_evm_transaction_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: EvmTransactionArgs = serde_json::from_str(arguments_json)?;
    let output = crate::runtime::get().block_on(evm_transaction_impl(&args.rpc_url, &args.tx_hash))?;
    Ok(truncate_tool_output(&output))
}

async fn evm_transaction_impl(rpc_url: &str, tx_hash_str: &str) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;

    let stripped = tx_hash_str
        .strip_prefix("0x")
        .or_else(|| tx_hash_str.strip_prefix("0X"))
        .unwrap_or(tx_hash_str);
    let hash =
        B256::from_str(stripped).map_err(|e| ToolError::Other(format!("invalid tx hash: {e}")))?;

    let receipt = provider
        .get_transaction_receipt(hash)
        .await
        .map_err(alloy_err)?
        .ok_or_else(|| ToolError::Other("transaction not found".to_string()))?;

    let tx_hash = receipt.transaction_hash;
    let block_number = receipt.block_number.unwrap_or(0);
    let from = receipt.from;
    let to = receipt
        .to
        .map(|t| t.to_string())
        .unwrap_or_else(|| "contract_creation".to_string());
    let gas_used = receipt.gas_used;
    let effective_gas_price = receipt.effective_gas_price;
    let log_count = receipt.logs().len();

    let mut out = String::new();
    out.push_str(&format!("hash: {tx_hash:#x}\n"));
    out.push_str(&format!("block: #{block_number}\n"));
    out.push_str(&format!("from: {from}\n"));
    out.push_str(&format!("to: {to}\n"));
    out.push_str(&format!("gas_used: {gas_used}\n"));
    out.push_str(&format!("effective_gas_price: {effective_gas_price} wei\n"));
    out.push_str(&format!("logs: {log_count}"));
    Ok(out)
}

define_tool!(
    EvmTransaction,
    "evm_transaction",
    "Get details about a transaction on an EVM blockchain by its hash. Returns hash, block number, from/to, gas used, effective gas price, and log count.",
    execute_evm_transaction_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"tx_hash":{"type":"string","description":"0x-prefixed transaction hash"}},"required":["rpc_url","tx_hash"],"additionalProperties":false})
);
