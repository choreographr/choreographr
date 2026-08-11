use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_block_tool(
    args: &choreo_blockchain::evm::EvmBlockArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_block(args))
}

pub(crate) struct EvmBlock;

define_tool!(
    EvmBlock,
    "evm_block",
    "Get details about a block on an EVM blockchain: block number, hash, timestamp, transaction count, gas used/limit, and base fee.",
    choreo_blockchain::evm::EvmBlockArgs,
    execute_evm_block_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_block_invocation
);
