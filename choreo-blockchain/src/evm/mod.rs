//! EVM blockchain tools (Ethereum-compatible chains via alloy).
//!
//! Each tool is a synchronous `execute_*` entry point (used by the daemon's
//! `Tool` wrappers) that runs an async alloy implementation on the crate's
//! sidecar tokio runtime (see [`crate::runtime`]). The daemon references the
//! public `*Args` types for its JSON Schemas and the public `execute_*` /
//! `describe_*` functions for dispatch; everything else is internal.

use crate::BlockchainError;
pub(crate) use crate::block_on;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::BlockNumberOrTag;
use alloy::sol;
use schemars::JsonSchema;
use serde::Deserialize;
use url::Url;

sol! {
    #[allow(missing_docs)]
    function balanceOf(address account) external view returns (uint256);
}

sol! {
    #[allow(missing_docs)]
    function symbol() external view returns (string);
}

// ── Shared argument types (public so the daemon can derive JSON Schemas) ──

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RpcUrlArgs {
    /// JSON-RPC URL of the EVM node (e.g., https://ethereum-rpc.publicnode.com)
    pub rpc_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmBalanceArgs {
    /// JSON-RPC URL of the EVM node
    pub rpc_url: String,
    /// 0x-prefixed hex address
    pub address: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmTokenBalanceArgs {
    /// JSON-RPC URL of the EVM node
    pub rpc_url: String,
    /// 0x-prefixed ERC-20 token contract address
    pub token_address: String,
    /// 0x-prefixed wallet address to check balance for
    pub address: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmBlockArgs {
    /// JSON-RPC URL of the EVM node
    pub rpc_url: String,
    /// Block number (decimal or 0x-hex), or 'latest', 'finalized', 'safe', 'pending', 'earliest'
    #[serde(rename = "block_tag")]
    pub block_tag: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmTransactionArgs {
    /// JSON-RPC URL of the EVM node
    pub rpc_url: String,
    /// 0x-prefixed transaction hash
    pub tx_hash: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmCallArgs {
    /// JSON-RPC URL of the EVM node
    pub rpc_url: String,
    /// 0x-prefixed contract address to call
    pub to: String,
    /// 0x-prefixed hex-encoded call data (method selector + ABI-encoded params)
    pub data: String,
    /// Block number (decimal or 0x-hex), or 'latest', 'finalized', 'safe', 'pending', 'earliest'
    #[serde(rename = "block_tag")]
    pub block_tag: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmLogsArgs {
    /// JSON-RPC URL of the EVM node
    pub rpc_url: String,
    /// Optional 0x-prefixed contract address to filter logs by
    pub address: Option<String>,
    /// Optional 0x-prefixed event signature hash (topic0) to filter by
    pub topic0: Option<String>,
    /// Optional starting block number or tag (e.g., '0x0', 'latest')
    #[serde(rename = "from_block")]
    pub from_block: Option<String>,
    /// Optional ending block number or tag (e.g., '0x0', 'latest')
    #[serde(rename = "to_block")]
    pub to_block: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmNonceArgs {
    /// JSON-RPC URL of the EVM node
    pub rpc_url: String,
    /// 0x-prefixed hex address
    pub address: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmResolveArgs {
    /// JSON-RPC URL of the EVM node (must support ENS)
    pub rpc_url: String,
    /// ENS name (e.g., 'vitalik.eth') or 0x-prefixed address for reverse lookup
    pub name_or_address: String,
}

// ── Shared client helpers ───────────────────────────────────────────────

/// Build an alloy HTTP provider for `rpc_url`, validating the URL up front.
pub(crate) fn connect(rpc_url: &str) -> Result<impl Provider, BlockchainError> {
    let url: Url = rpc_url
        .parse()
        .map_err(|e| BlockchainError::InvalidUrl(format!("invalid RPC URL: {e}")))?;
    Ok(ProviderBuilder::default().connect_http(url))
}

/// Parse a user-supplied block tag ("latest", "safe", a decimal number, or a
/// 0x-hex number) into alloy's `BlockNumberOrTag`.
pub(crate) fn parse_block_tag(tag: &str) -> Result<BlockNumberOrTag, BlockchainError> {
    match tag.to_lowercase().as_str() {
        "latest" => Ok(BlockNumberOrTag::Latest),
        "finalized" => Ok(BlockNumberOrTag::Finalized),
        "safe" => Ok(BlockNumberOrTag::Safe),
        "pending" => Ok(BlockNumberOrTag::Pending),
        "earliest" => Ok(BlockNumberOrTag::Earliest),
        hex_or_dec => {
            if let Some(hex) = hex_or_dec
                .strip_prefix("0x")
                .or_else(|| hex_or_dec.strip_prefix("0X"))
            {
                let n = u64::from_str_radix(hex, 16).map_err(|e| {
                    BlockchainError::Other(format!("invalid hex block number: {e}"))
                })?;
                Ok(BlockNumberOrTag::Number(n))
            } else if let Ok(n) = hex_or_dec.parse::<u64>() {
                Ok(BlockNumberOrTag::Number(n))
            } else {
                Err(BlockchainError::Other(format!("invalid block tag: {tag}")))
            }
        }
    }
}

/// Wrap an alloy transport error in a [`BlockchainError::Alloy`].
pub(crate) fn alloy_err(e: impl std::fmt::Display) -> BlockchainError {
    BlockchainError::Alloy(format!("{e}"))
}

// ── Tool modules (one per tool) ─────────────────────────────────────────

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

pub use balance::execute_evm_balance;
pub use block::execute_evm_block;
pub use call::execute_evm_call;
pub use chain::execute_evm_chain;
pub use gas::execute_evm_gas;
pub use logs::execute_evm_logs;
pub use nonce::execute_evm_nonce;
pub use resolve::execute_evm_resolve;
pub use token_balance::execute_evm_token_balance;
pub use transaction::execute_evm_transaction;

pub use balance::describe_evm_balance_invocation;
pub use block::describe_evm_block_invocation;
pub use call::describe_evm_call_invocation;
pub use chain::describe_evm_chain_invocation;
pub use gas::describe_evm_gas_invocation;
pub use logs::describe_evm_logs_invocation;
pub use nonce::describe_evm_nonce_invocation;
pub use resolve::describe_evm_resolve_invocation;
pub use token_balance::describe_evm_token_balance_invocation;
pub use transaction::describe_evm_transaction_invocation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_block_tag_latest() {
        let tag = parse_block_tag("latest").unwrap();
        assert!(matches!(tag, BlockNumberOrTag::Latest));
    }

    #[test]
    fn parse_block_tag_number() {
        let tag = parse_block_tag("12345").unwrap();
        assert!(matches!(tag, BlockNumberOrTag::Number(12345)));
    }

    #[test]
    fn parse_block_tag_hex() {
        let tag = parse_block_tag("0x3039").unwrap();
        assert!(matches!(tag, BlockNumberOrTag::Number(12345)));
    }

    #[test]
    fn parse_block_tag_case_insensitive_named_tags() {
        let tag = parse_block_tag("SAFE").unwrap();
        assert!(matches!(tag, BlockNumberOrTag::Safe));
    }

    #[test]
    fn parse_block_tag_invalid() {
        assert!(parse_block_tag("not_a_block").is_err());
    }

    #[test]
    fn connect_rejects_bad_url() {
        assert!(connect("not a url").is_err());
    }

    #[test]
    fn args_schemas_are_generated() {
        // The daemon derives tool JSON Schemas from these types via schemars;
        // make sure they stay JsonSchema-compatible (this would fail to
        // compile otherwise, but the round-trip guards the derive output).
        let schema = schemars::schema_for!(EvmBalanceArgs);
        let json = serde_json::to_value(schema).unwrap();
        let props = json["properties"].as_object().unwrap();
        assert!(props.contains_key("rpc_url"));
        assert!(props.contains_key("address"));
    }
}
