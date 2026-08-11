use super::{EvmResolveArgs, block_on, connect};
use crate::{BlockchainError, truncate_tool_output};
use alloy::primitives::Address;
use alloy::providers::Provider;
use std::str::FromStr;

async fn evm_resolve_impl(rpc_url: &str, name_or_address: &str) -> Result<String, BlockchainError> {
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
            Err(e) => Err(BlockchainError::Other(e)),
        }
    } else {
        let address = Address::from_str(name_or_address)
            .map_err(|e| BlockchainError::Other(format!("invalid address: {e}")))?;
        let result: Result<String, String> = provider
            .raw_request(std::borrow::Cow::Borrowed("ens_reverse"), (address,))
            .await
            .map_err(|_| format!("reverse ENS lookup failed for {name_or_address}"));
        match result {
            Ok(name) => Ok(format!("address: {name_or_address}\nname: {name}")),
            Err(e) => Err(BlockchainError::Other(e)),
        }
    }
}

/// Synchronous entry point: runs [`evm_resolve_impl`] on the sidecar runtime
/// and caps the output at the shared byte budget.
pub fn execute_evm_resolve(args: &EvmResolveArgs) -> Result<String, BlockchainError> {
    let output = block_on(evm_resolve_impl(&args.rpc_url, &args.name_or_address))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_resolve_invocation(args: &EvmResolveArgs) -> String {
    format!(
        "Resolving {} via ENS on {}.",
        args.name_or_address, args.rpc_url
    )
}
