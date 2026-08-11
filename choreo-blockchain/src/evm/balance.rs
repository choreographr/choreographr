use super::{EvmBalanceArgs, alloy_err, block_on, connect, log_execution, rpc_call};
use crate::{BlockchainError, truncate_tool_output};
use alloy::primitives::Address;
use alloy::providers::Provider;
use std::str::FromStr;

async fn evm_balance_impl(rpc_url: &str, address_str: &str) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;
    let address = Address::from_str(address_str)
        .map_err(|e| BlockchainError::Other(format!("invalid address: {e}")))?;

    let balance = provider.get_balance(address).await.map_err(alloy_err)?;

    Ok(format!("address: {address_str}\nbalance: {balance} wei"))
}

/// Synchronous entry point: runs [`evm_balance_impl`] on the sidecar runtime
/// and caps the output at the shared byte budget.
pub fn execute_evm_balance(args: &EvmBalanceArgs) -> Result<String, BlockchainError> {
    log_execution("evm_balance", &args.rpc_url);
    let output = block_on(rpc_call(evm_balance_impl(&args.rpc_url, &args.address)))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_balance_invocation(args: &EvmBalanceArgs) -> String {
    format!(
        "Querying native balance of {} on {}.",
        args.address, args.rpc_url
    )
}
