use super::{
    EvmCallArgs, alloy_err, block_id, block_on, connect, log_execution, parse_block_tag, rpc_call,
    strip_hex_prefix,
};
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
    block_tag: Option<&str>,
) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;
    let to = Address::from_str(to_str)
        .map_err(|e| BlockchainError::Other(format!("invalid 'to' address: {e}")))?;

    let data_hex = strip_hex_prefix(data_str);
    let input_data = hex::decode(data_hex)
        .map_err(|e| BlockchainError::Other(format!("invalid hex data: {e}")))?;
    let tx = TransactionRequest::default()
        .with_to(to)
        .with_input(input_data);

    // Apply the requested block tag (defaults to `latest` when omitted) so the
    // advertised `block_tag` argument actually selects the state the call runs
    // against instead of silently querying the latest block.
    let mut call = provider.call(tx);
    if let Some(tag) = block_tag {
        call = call.block(block_id(parse_block_tag(tag)?));
    }
    let result: Bytes = call.await.map_err(alloy_err)?;

    Ok(format!(
        "to: {to_str}\ndata: {data_str}\nresult: 0x{}",
        hex::encode(result.as_ref())
    ))
}

/// Synchronous entry point: runs [`evm_call_impl`] on the sidecar runtime and
/// caps the output at the shared byte budget.
pub fn execute_evm_call(args: &EvmCallArgs) -> Result<String, BlockchainError> {
    log_execution("evm_call", &args.rpc_url);
    let output = block_on(rpc_call(evm_call_impl(
        &args.rpc_url,
        &args.to,
        &args.data,
        args.block_tag.as_deref(),
    )))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_call_invocation(args: &EvmCallArgs) -> String {
    match args.block_tag.as_deref() {
        Some(tag) => format!(
            "Executing read-only call to {} on {} at block {tag}.",
            args.to, args.rpc_url
        ),
        None => format!(
            "Executing read-only call to {} on {}.",
            args.to, args.rpc_url
        ),
    }
}
