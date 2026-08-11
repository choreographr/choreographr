use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_transaction_tool(
    args: &choreo_blockchain::evm::EvmTransactionArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_transaction(args))
}

pub(crate) struct EvmTransaction;

define_tool!(
    EvmTransaction,
    "evm_transaction",
    "Get details about a transaction on an EVM blockchain by its hash. Returns hash, block number, from/to, gas used, effective gas price, and log count.",
    choreo_blockchain::evm::EvmTransactionArgs,
    execute_evm_transaction_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_transaction_invocation
);
