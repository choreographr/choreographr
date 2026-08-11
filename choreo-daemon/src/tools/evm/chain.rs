use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_chain_tool(
    args: &choreo_blockchain::evm::RpcUrlArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_chain(args))
}

pub(crate) struct EvmChain;

define_tool!(
    EvmChain,
    "evm_chain",
    "Query information about an EVM blockchain node: chain ID, latest block number, gas price, max priority fee, and client version.",
    choreo_blockchain::evm::RpcUrlArgs,
    execute_evm_chain_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_chain_invocation
);
