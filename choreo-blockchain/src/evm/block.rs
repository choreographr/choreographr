use super::{EvmBlockArgs, alloy_err, block_on, connect, log_execution, parse_block_tag, rpc_call};
use crate::{BlockchainError, truncate_tool_output};
use alloy::providers::Provider;
use alloy::rpc::types::eth::BlockNumberOrTag;

async fn evm_block_impl(rpc_url: &str, block_tag: Option<&str>) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;

    let block_num = match block_tag {
        Some(tag) => parse_block_tag(tag)?,
        None => BlockNumberOrTag::Latest,
    };

    let block = provider
        .get_block_by_number(block_num)
        .await
        .map_err(alloy_err)?
        .ok_or_else(|| BlockchainError::Other("block not found".to_string()))?;

    let number = block.header.number;
    let hash = block.header.hash;
    let timestamp = block.header.timestamp;
    let gas_used = block.header.gas_used;
    let gas_limit = block.header.gas_limit;
    let tx_count = block.transactions.len();
    let base_fee = block.header.base_fee_per_gas.unwrap_or(0);

    let mut out = String::new();
    out.push_str(&format!("block: #{number}\n"));
    out.push_str(&format!("hash: {hash:#x}\n"));
    out.push_str(&format!("timestamp: {timestamp}\n"));
    out.push_str(&format!("transactions: {tx_count}\n"));
    out.push_str(&format!("gas_used: {gas_used}\n"));
    out.push_str(&format!("gas_limit: {gas_limit}\n"));
    out.push_str(&format!("base_fee: {base_fee} wei"));
    Ok(out)
}

/// Synchronous entry point: runs [`evm_block_impl`] on the sidecar runtime and
/// caps the output at the shared byte budget.
pub fn execute_evm_block(args: &EvmBlockArgs) -> Result<String, BlockchainError> {
    log_execution("evm_block", &args.rpc_url);
    let output = block_on(rpc_call(evm_block_impl(
        &args.rpc_url,
        args.block_tag.as_deref(),
    )))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_block_invocation(args: &EvmBlockArgs) -> String {
    match args.block_tag.as_deref() {
        Some(tag) => format!("Querying EVM block {tag} on {}.", args.rpc_url),
        None => format!("Querying latest EVM block on {}.", args.rpc_url),
    }
}
