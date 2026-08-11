use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_token_balance_tool(
    args: &choreo_blockchain::evm::EvmTokenBalanceArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_token_balance(args))
}

pub(crate) struct EvmTokenBalance;

define_tool!(
    EvmTokenBalance,
    "evm_token_balance",
    "Query the ERC-20 token balance for an address. Also attempts to fetch the token symbol.",
    choreo_blockchain::evm::EvmTokenBalanceArgs,
    execute_evm_token_balance_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_token_balance_invocation
);
