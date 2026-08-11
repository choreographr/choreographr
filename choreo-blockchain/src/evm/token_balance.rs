use super::{
    EvmTokenBalanceArgs, alloy_err, balanceOfCall, block_on, connect, log_execution, rpc_call,
    symbolCall,
};
use crate::{BlockchainError, truncate_tool_output};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol_types::SolCall;
use std::str::FromStr;

async fn evm_token_balance_impl(
    rpc_url: &str,
    token_address_str: &str,
    address_str: &str,
) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;
    let token_address = Address::from_str(token_address_str)
        .map_err(|e| BlockchainError::Other(format!("invalid token address: {e}")))?;
    let owner_address = Address::from_str(address_str)
        .map_err(|e| BlockchainError::Other(format!("invalid owner address: {e}")))?;

    let call = balanceOfCall {
        account: owner_address,
    };
    let call_data = SolCall::abi_encode(&call);
    let tx = TransactionRequest::default()
        .with_to(token_address)
        .with_input(call_data);

    let result: Bytes = provider.call(tx).await.map_err(alloy_err)?;
    let balance = balanceOfCall::abi_decode_returns(&result).map_err(alloy_err)?;

    let mut out = String::new();
    out.push_str(&format!("token_address: {token_address_str}\n"));
    out.push_str(&format!("owner_address: {address_str}\n"));
    out.push_str(&format!("balance: {balance}"));

    // Best-effort ERC-20 `symbol()` fetch — a non-standard token may revert,
    // in which case the balance result still stands without the symbol.
    let sym_call = symbolCall {};
    let sym_data = SolCall::abi_encode(&sym_call);
    let sym_tx = TransactionRequest::default()
        .with_to(token_address)
        .with_input(sym_data);
    if let Ok(sym_result) = provider.call(sym_tx).await
        && let Ok(sym_ret) = symbolCall::abi_decode_returns(&sym_result)
    {
        out.push_str(&format!("\nsymbol: {sym_ret}"));
    }

    Ok(out)
}

/// Synchronous entry point: runs [`evm_token_balance_impl`] on the sidecar
/// runtime and caps the output at the shared byte budget.
pub fn execute_evm_token_balance(args: &EvmTokenBalanceArgs) -> Result<String, BlockchainError> {
    log_execution("evm_token_balance", &args.rpc_url);
    let output = block_on(rpc_call(evm_token_balance_impl(
        &args.rpc_url,
        &args.token_address,
        &args.address,
    )))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_token_balance_invocation(args: &EvmTokenBalanceArgs) -> String {
    format!(
        "Querying ERC-20 balance of {} for token {} on {}.",
        args.address, args.token_address, args.rpc_url
    )
}
