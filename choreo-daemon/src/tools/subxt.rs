//! Thin `Tool` trait wrappers over the Substrate/Polkadot blockchain tools.
//!
//! The actual implementations (async subxt clients, argument parsing, output
//! hygiene) live in the `choreo-blockchain` crate, which also owns the tokio
//! sidecar runtime. This module only adapts them to the daemon's `Tool` trait,
//! so `choreo-daemon` itself never depends on subxt or tokio. Compiled only
//! when the `blockchain` cargo feature is enabled (off by default).

use crate::tools::ToolExecError;
use std::path::Path;

/// Map a `choreo-blockchain` error into a [`ToolExecError`] — the shared tail
/// of every `execute_subxt_*_tool` wrapper.
fn exec(
    result: Result<String, choreo_blockchain::BlockchainError>,
) -> Result<String, ToolExecError> {
    result.map_err(Into::into)
}

fn execute_subxt_chain_tool(
    args: &choreo_blockchain::subxt::SubxtChainArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::subxt::execute_subxt_chain(args))
}

fn execute_subxt_balance_tool(
    args: &choreo_blockchain::subxt::SubxtBalanceArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::subxt::execute_subxt_balance(args))
}

fn execute_subxt_query_tool(
    args: &choreo_blockchain::subxt::SubxtQueryArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::subxt::execute_subxt_query(args))
}

fn execute_subxt_block_tool(
    args: &choreo_blockchain::subxt::SubxtBlockArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    exec(choreo_blockchain::subxt::execute_subxt_block(args))
}

pub(crate) struct SubxtChain;

define_tool!(
    SubxtChain,
    "subxt_chain",
    "Query information about a Substrate/Polkadot blockchain node: chain name, chain type, node name/version, genesis hash, best block, finalized head, system properties, and health.",
    choreo_blockchain::subxt::SubxtChainArgs,
    execute_subxt_chain_tool,
    "blockchain",
    choreo_blockchain::subxt::describe_subxt_chain_invocation
);

pub(crate) struct SubxtBalance;

define_tool!(
    SubxtBalance,
    "subxt_balance",
    "Query the balance of an account on a Substrate/Polkadot blockchain. Returns the System.Account info (free, reserved, frozen balances).",
    choreo_blockchain::subxt::SubxtBalanceArgs,
    execute_subxt_balance_tool,
    "blockchain",
    choreo_blockchain::subxt::describe_subxt_balance_invocation
);

pub(crate) struct SubxtQuery;

define_tool!(
    SubxtQuery,
    "subxt_query",
    "Query a storage value from a Substrate/Polkadot blockchain by pallet and storage item name. Returns the decoded SCALE value as JSON.",
    choreo_blockchain::subxt::SubxtQueryArgs,
    execute_subxt_query_tool,
    "blockchain",
    choreo_blockchain::subxt::describe_subxt_query_invocation
);

pub(crate) struct SubxtBlock;

define_tool!(
    SubxtBlock,
    "subxt_block",
    "Get details about a block on a Substrate/Polkadot blockchain: block number, hash, parent hash, state root, extrinsics root, and full block JSON.",
    choreo_blockchain::subxt::SubxtBlockArgs,
    execute_subxt_block_tool,
    "blockchain",
    choreo_blockchain::subxt::describe_subxt_block_invocation
);
