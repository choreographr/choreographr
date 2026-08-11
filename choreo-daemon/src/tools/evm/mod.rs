//! Thin `Tool` trait wrappers over the EVM blockchain tools.
//!
//! The actual implementations (async alloy clients, argument parsing, output
//! hygiene) live in the `choreo-blockchain` crate, which also owns the tokio
//! sidecar runtime. This module only adapts them to the daemon's `Tool` trait,
//! so `choreo-daemon` itself never depends on alloy or tokio. Compiled only
//! when the `blockchain` cargo feature is enabled (off by default).

use crate::tools::ToolExecError;

/// Run a `choreo-blockchain` entry point and map its error into a
/// [`ToolExecError`] — the shared tail of every `execute_*_tool` wrapper, so
/// each tool file stays a one-liner.
pub(super) fn exec(
    result: Result<String, choreo_blockchain::BlockchainError>,
) -> Result<String, ToolExecError> {
    result.map_err(Into::into)
}

mod balance;
mod block;
mod call;
mod chain;
mod gas;
mod logs;
mod nonce;
mod resolve;
mod token_balance;
mod transaction;

pub(crate) use balance::EvmBalance;
pub(crate) use block::EvmBlock;
pub(crate) use call::EvmCall;
pub(crate) use chain::EvmChain;
pub(crate) use gas::EvmGas;
pub(crate) use logs::EvmLogs;
pub(crate) use nonce::EvmNonce;
pub(crate) use resolve::EvmResolve;
pub(crate) use token_balance::EvmTokenBalance;
pub(crate) use transaction::EvmTransaction;
