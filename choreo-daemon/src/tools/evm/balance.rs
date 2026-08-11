use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_balance_tool(
    args: &choreo_blockchain::evm::EvmBalanceArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_balance(args))
}

pub(crate) struct EvmBalance;

define_tool!(
    EvmBalance,
    "evm_balance",
    "Query the native ETH/coin balance of an address on an EVM blockchain.",
    choreo_blockchain::evm::EvmBalanceArgs,
    execute_evm_balance_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_balance_invocation
);
