use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_nonce_tool(
    args: &choreo_blockchain::evm::EvmNonceArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_nonce(args))
}

pub(crate) struct EvmNonce;

define_tool!(
    EvmNonce,
    "evm_nonce",
    "Get the transaction count (nonce) for an address on an EVM blockchain.",
    choreo_blockchain::evm::EvmNonceArgs,
    execute_evm_nonce_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_nonce_invocation
);
