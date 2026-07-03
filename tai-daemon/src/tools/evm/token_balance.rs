use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol_types::SolCall;
use std::str::FromStr;

use super::{EvmTokenBalanceArgs, alloy_err, balanceOfCall, connect, symbolCall};

pub(crate) fn execute_evm_token_balance_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_token_balance_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_evm_token_balance_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: EvmTokenBalanceArgs = serde_json::from_str(arguments_json)?;
    let output = tokio::runtime::Handle::current().block_on(
        evm_token_balance_impl(&args.rpc_url, &args.token_address, &args.address),
    )?;
    Ok(truncate_tool_output(&output))
}

async fn evm_token_balance_impl(
    rpc_url: &str,
    token_address_str: &str,
    address_str: &str,
) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;
    let token_address = Address::from_str(token_address_str)
        .map_err(|e| ToolError::Other(format!("invalid token address: {e}")))?;
    let owner_address =
        Address::from_str(address_str).map_err(|e| ToolError::Other(format!("invalid owner address: {e}")))?;

    let call = balanceOfCall {
        account: owner_address,
    };
    let call_data = SolCall::abi_encode(&call);
    let tx = TransactionRequest::default()
        .with_to(token_address)
        .with_input(call_data);

    let result: Bytes = provider.call(tx).await.map_err(alloy_err)?;
    let return_data =
        balanceOfCall::abi_decode_returns(&result).map_err(alloy_err)?;
    let balance = return_data;

    let mut out = String::new();
    out.push_str(&format!("token_address: {token_address_str}\n"));
    out.push_str(&format!("owner_address: {address_str}\n"));
    out.push_str(&format!("balance: {balance}"));

    let sym_call = symbolCall {};
    let sym_data = SolCall::abi_encode(&sym_call);
    let sym_tx = TransactionRequest::default()
        .with_to(token_address)
        .with_input(sym_data);
    if let Ok(sym_result) = provider.call(sym_tx).await {
        if let Ok(sym_ret) = symbolCall::abi_decode_returns(&sym_result) {
            out.push_str(&format!("\nsymbol: {sym_ret}"));
        }
    }

    Ok(out)
}

define_tool!(EvmTokenBalance, "evm_token_balance",
    "Query the ERC-20 token balance for an address. Also attempts to fetch the token symbol.",
    execute_evm_token_balance_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"token_address":{"type":"string","description":"0x-prefixed ERC-20 token contract address"},"address":{"type":"string","description":"0x-prefixed wallet address to check balance for"}},"required":["rpc_url","token_address","address"],"additionalProperties":false})
);
