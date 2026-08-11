use super::{EvmNonceArgs, alloy_err, block_on, connect};
use crate::{BlockchainError, truncate_tool_output};
use alloy::primitives::Address;
use alloy::providers::Provider;
use std::str::FromStr;

async fn evm_nonce_impl(rpc_url: &str, address_str: &str) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;
    let address = Address::from_str(address_str)
        .map_err(|e| BlockchainError::Other(format!("invalid address: {e}")))?;

    let nonce = provider
        .get_transaction_count(address)
        .await
        .map_err(alloy_err)?;

    Ok(format!(
        "address: {address_str}\ntransaction_count (nonce): {nonce}"
    ))
}

/// Synchronous entry point: runs [`evm_nonce_impl`] on the sidecar runtime and
/// caps the output at the shared byte budget.
pub fn execute_evm_nonce(args: &EvmNonceArgs) -> Result<String, BlockchainError> {
    let output = block_on(evm_nonce_impl(&args.rpc_url, &args.address))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_nonce_invocation(args: &EvmNonceArgs) -> String {
    format!(
        "Querying transaction count (nonce) of {} on {}.",
        args.address, args.rpc_url
    )
}
