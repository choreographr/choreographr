use super::{RpcUrlArgs, alloy_err, block_on, connect};
use crate::{BlockchainError, truncate_tool_output};
use alloy::providers::Provider;

async fn evm_chain_impl(rpc_url: &str) -> Result<String, BlockchainError> {
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

/// Synchronous entry point: runs [`evm_chain_impl`] on the sidecar runtime and
/// caps the output at the shared byte budget.
pub fn execute_evm_chain(args: &RpcUrlArgs) -> Result<String, BlockchainError> {
    let output = block_on(evm_chain_impl(&args.rpc_url))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_chain_invocation(args: &RpcUrlArgs) -> String {
    format!("Querying EVM chain info from {}.", args.rpc_url)
}
