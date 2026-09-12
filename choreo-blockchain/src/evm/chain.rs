use super::{RpcUrlArgs, alloy_err, block_on, connect, log_execution, rpc_call, sanitize_value};
use crate::{BlockchainError, truncate_tool_output};
use alloy::providers::Provider;
use std::fmt::Write;

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
    let _ = write!(out, "chain_id: {chain_id}\n");
    let _ = write!(out, "block_number: {block_number}\n");
    let _ = write!(out, "gas_price: {gas_price} wei\n");
    let _ = write!(out, "max_priority_fee: {max_priority_fee} wei\n");
    // client_version is a free-form node-supplied string (may embed control
    // chars / terminal escapes from a hostile endpoint) — sanitize it.
    let _ = writeln!(out, "client_version: {}", sanitize_value(&client_version));

    Ok(out)
}

/// Synchronous entry point: runs [`evm_chain_impl`] on the sidecar runtime and
/// caps the output at the shared byte budget.
///
/// # Errors
///
/// Returns [`BlockchainError`] when the node is unreachable, the RPC call
/// fails, or the capped sanitized output cannot be produced.
pub fn execute_evm_chain(args: &RpcUrlArgs) -> Result<String, BlockchainError> {
    log_execution("evm_chain", &args.rpc_url);
    let output = block_on(rpc_call(evm_chain_impl(&args.rpc_url)))??;
    Ok(truncate_tool_output(&output))
}

#[must_use]
pub fn describe_evm_chain_invocation(args: &RpcUrlArgs) -> String {
    format!("Querying EVM chain info from {}.", args.rpc_url)
}
