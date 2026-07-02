use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use alloy::primitives::Address;
use alloy::providers::Provider;
use std::str::FromStr;

use super::{EvmResolveArgs, connect};

pub(crate) async fn execute_evm_resolve_tool(arguments_json: &str) -> ToolResult {
    match execute_evm_resolve_inner(arguments_json).await {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

async fn execute_evm_resolve_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: EvmResolveArgs = serde_json::from_str(arguments_json)?;
    let output = evm_resolve_impl(&args.rpc_url, &args.name_or_address).await?;
    Ok(truncate_tool_output(&output))
}

async fn evm_resolve_impl(rpc_url: &str, name_or_address: &str) -> Result<String, ToolError> {
    let provider = connect(rpc_url)?;

    if name_or_address.ends_with(".eth") {
        let result: Result<Address, String> = provider
            .raw_request(
                std::borrow::Cow::Borrowed("ens_resolve"),
                (name_or_address,),
            )
            .await
            .map_err(|_| format!("ENS resolution failed for {name_or_address}"));
        match result {
            Ok(addr) => Ok(format!("name: {name_or_address}\naddress: {addr:#x}")),
            Err(e) => Err(ToolError::Other(e)),
        }
    } else {
        let address = Address::from_str(name_or_address)
            .map_err(|e| ToolError::Other(format!("invalid address: {e}")))?;
        let result: Result<String, String> = provider
            .raw_request(
                std::borrow::Cow::Borrowed("ens_reverse"),
                (address,),
            )
            .await
            .map_err(|_| format!("reverse ENS lookup failed for {name_or_address}"));
        match result {
            Ok(name) => Ok(format!("address: {name_or_address}\nname: {name}")),
            Err(e) => Err(ToolError::Other(e)),
        }
    }
}

define_tool!(EvmResolve, "evm_resolve",
    "Resolve an ENS name to an address, or reverse-resolve an address to an ENS name on an EVM blockchain.",
    execute_evm_resolve_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node (must support ENS)"},"name_or_address":{"type":"string","description":"ENS name (e.g., 'vitalik.eth') or 0x-prefixed address for reverse lookup"}},"required":["rpc_url","name_or_address"],"additionalProperties":false})
);
