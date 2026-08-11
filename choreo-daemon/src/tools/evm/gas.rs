use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_gas_tool(
    args: &choreo_blockchain::evm::RpcUrlArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_gas(args))
}

pub(crate) struct EvmGas;

define_tool!(
    EvmGas,
    "evm_gas",
    "Get current gas fee estimates on an EVM blockchain: gas price, max priority fee, and EIP-1559 fee estimation.",
    choreo_blockchain::evm::RpcUrlArgs,
    execute_evm_gas_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_gas_invocation
);
