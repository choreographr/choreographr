use super::{
    EvmTransactionArgs, alloy_err, block_on, connect, log_execution, rpc_call, strip_hex_prefix,
};
use crate::{BlockchainError, truncate_tool_output};
use alloy::primitives::B256;
use alloy::providers::Provider;
use std::str::FromStr;

async fn evm_transaction_impl(rpc_url: &str, tx_hash_str: &str) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;

    let stripped = strip_hex_prefix(tx_hash_str);
    let hash = B256::from_str(stripped)
        .map_err(|e| BlockchainError::Other(format!("invalid tx hash: {e}")))?;

    let receipt = provider
        .get_transaction_receipt(hash)
        .await
        .map_err(alloy_err)?
        .ok_or_else(|| BlockchainError::Other("transaction not found".to_string()))?;

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

/// Synchronous entry point: runs [`evm_transaction_impl`] on the sidecar
/// runtime and caps the output at the shared byte budget.
pub fn execute_evm_transaction(args: &EvmTransactionArgs) -> Result<String, BlockchainError> {
    log_execution("evm_transaction", &args.rpc_url);
    let output = block_on(rpc_call(evm_transaction_impl(&args.rpc_url, &args.tx_hash)))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_transaction_invocation(args: &EvmTransactionArgs) -> String {
    format!("Querying transaction {} on {}.", args.tx_hash, args.rpc_url)
}
