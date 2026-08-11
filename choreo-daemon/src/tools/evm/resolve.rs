use super::exec;
use crate::tools::ToolExecError;
use std::path::Path;

fn execute_evm_resolve_tool(
    args: &choreo_blockchain::evm::EvmResolveArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::evm::execute_evm_resolve(args))
}

pub(crate) struct EvmResolve;

define_tool!(
    EvmResolve,
    "evm_resolve",
    "Resolve an ENS name to an address, or reverse-resolve an address to an ENS name on an EVM blockchain.",
    choreo_blockchain::evm::EvmResolveArgs,
    execute_evm_resolve_tool,
    "blockchain",
    choreo_blockchain::evm::describe_evm_resolve_invocation
);
