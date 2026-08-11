use super::{EvmCallArgs, alloy_err, block_on, connect};
use crate::{BlockchainError, truncate_tool_output};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::eth::TransactionRequest;
use std::str::FromStr;

async fn evm_call_impl(
    rpc_url: &str,
    to_str: &str,
    data_str: &str,
) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;
    let to = Address::from_str(to_str)
        .map_err(|e| BlockchainError::Other(format!("invalid 'to' address: {e}")))?;

    let data_hex = data_str
        .strip_prefix("0x")
        .or_else(|| data_str.strip_prefix("0X"))
        .unwrap_or(data_str);
    let input_data = hex::decode(data_hex)
        .map_err(|e| BlockchainError::Other(format!("invalid hex data: {e}")))?;
    let tx = TransactionRequest::default()
        .with_to(to)
        .with_input(input_data);

    let result: Bytes = provider.call(tx).await.map_err(alloy_err)?;

    Ok(format!(
        "to: {to_str}\ndata: {data_str}\nresult: 0x{}",
        hex::encode(result.as_ref())
    ))
}

/// Synchronous entry point: runs [`evm_call_impl`] on the sidecar runtime and
/// caps the output at the shared byte budget.
pub fn execute_evm_call(args: &EvmCallArgs) -> Result<String, BlockchainError> {
    let output = block_on(evm_call_impl(&args.rpc_url, &args.to, &args.data))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_call_invocation(args: &EvmCallArgs) -> String {
    format!(
        "Executing read-only call to {} on {}.",
        args.to, args.rpc_url
    )
}
