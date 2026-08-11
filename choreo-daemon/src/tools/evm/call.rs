use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_call_tool(
    args: &choreo_blockchain::evm::EvmCallArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_call(args))
}

pub(crate) struct EvmCall;

define_tool!(
    EvmCall,
    "evm_call",
    "Execute a read-only smart contract call (eth_call) on an EVM blockchain. Returns the raw hex-encoded result bytes.",
    choreo_blockchain::evm::EvmCallArgs,
    execute_evm_call_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_call_invocation
);
