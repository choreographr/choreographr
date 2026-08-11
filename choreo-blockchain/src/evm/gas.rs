use super::{RpcUrlArgs, alloy_err, block_on, connect, log_execution, rpc_call};
use crate::{BlockchainError, truncate_tool_output};
use alloy::providers::Provider;

async fn evm_gas_impl(rpc_url: &str) -> Result<String, BlockchainError> {
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

/// Synchronous entry point: runs [`evm_gas_impl`] on the sidecar runtime and
/// caps the output at the shared byte budget.
pub fn execute_evm_gas(args: &RpcUrlArgs) -> Result<String, BlockchainError> {
    log_execution("evm_gas", &args.rpc_url);
    let output = block_on(rpc_call(evm_gas_impl(&args.rpc_url)))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_gas_invocation(args: &RpcUrlArgs) -> String {
    format!("Querying gas fee estimates on {}.", args.rpc_url)
}
