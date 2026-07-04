use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::providers::Provider;

use super::{RpcUrlArgs, alloy_err, connect};

pub(crate) fn execute_evm_gas_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_gas_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_evm_gas_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: RpcUrlArgs = serde_json::from_str(arguments_json)?;
    let output = tokio::runtime::Handle::current().block_on(evm_gas_impl(&args.rpc_url))?;
    Ok(truncate_tool_output(&output))
}

async fn evm_gas_impl(rpc_url: &str) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;

    let gas_price = provider.get_gas_price().await.map_err(alloy_err)?;
    let max_priority_fee = provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(alloy_err)?;
    let estimation = provider.estimate_eip1559_fees().await.map_err(alloy_err)?;

    let mut out = String::new();
    out.push_str(&format!("gas_price: {gas_price} wei\n"));
    out.push_str(&format!(
        "max_priority_fee_per_gas: {max_priority_fee} wei\n"
    ));
    out.push_str(&format!(
        "estimated_max_fee_per_gas: {} wei\n",
        estimation.max_fee_per_gas
    ));
    out.push_str(&format!(
        "estimated_max_priority_fee_per_gas: {} wei",
        estimation.max_priority_fee_per_gas
    ));
    Ok(out)
}

define_tool!(
    EvmGas,
    "evm_gas",
    "Get current gas fee estimates on an EVM blockchain: gas price, max priority fee, and EIP-1559 fee estimation.",
    execute_evm_gas_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"}},"required":["rpc_url"],"additionalProperties":false})
);
