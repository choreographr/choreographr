use super::{EvmResolveArgs, block_on, connect, log_execution, rpc_call};
use crate::{BlockchainError, truncate_tool_output};
use alloy::ens::ProviderEnsExt;
use alloy::primitives::Address;
use std::str::FromStr;

async fn evm_resolve_impl(rpc_url: &str, name_or_address: &str) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;

    if name_or_address.ends_with(".eth") {
        // Forward lookup. `ProviderEnsExt::resolve_name` performs the ENS
        // Universal Resolver contract calls over `eth_call` — a standard
        // method every node implements — unlike the old implementation's
        // fabricated `ens_resolve` RPC method, which no node supports.
        let address = provider.resolve_name(name_or_address).await.map_err(|e| {
            BlockchainError::Other(format!("ENS resolution failed for {name_or_address}: {e}"))
        })?;
        Ok(format!("name: {name_or_address}\naddress: {address:#x}"))
    } else {
        let address = Address::from_str(name_or_address)
            .map_err(|e| BlockchainError::Other(format!("invalid address: {e}")))?;
        // Reverse lookup via the ENS reverse registrar (also `eth_call`-based).
        let name = provider.lookup_address(&address).await.map_err(|e| {
            BlockchainError::Other(format!(
                "reverse ENS lookup failed for {name_or_address}: {e}"
            ))
        })?;
        Ok(format!("address: {name_or_address}\nname: {name}"))
    }
}

/// Synchronous entry point: runs [`evm_resolve_impl`] on the sidecar runtime
/// and caps the output at the shared byte budget.
pub fn execute_evm_resolve(args: &EvmResolveArgs) -> Result<String, BlockchainError> {
    log_execution("evm_resolve", &args.rpc_url);
    let output = block_on(rpc_call(evm_resolve_impl(
        &args.rpc_url,
        &args.name_or_address,
    )))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_resolve_invocation(args: &EvmResolveArgs) -> String {
    format!(
        "Resolving {} via ENS on {}.",
        args.name_or_address, args.rpc_url
    )
}
