use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_logs_tool(
    args: &choreo_blockchain::evm::EvmLogsArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_logs(args))
}

pub(crate) struct EvmLogs;

define_tool!(
    EvmLogs,
    "evm_logs",
    "Query event logs on an EVM blockchain with optional filters by contract address, topic0, and block range.",
    choreo_blockchain::evm::EvmLogsArgs,
    execute_evm_logs_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_logs_invocation
);
