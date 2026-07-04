use super::ToolError;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::BlockNumberOrTag;
use alloy::sol;
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

#[derive(Debug, Deserialize)]
pub(crate) struct RpcUrlArgs {
    pub(crate) rpc_url: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvmBalanceArgs {
    pub(crate) rpc_url: String,
    pub(crate) address: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvmTokenBalanceArgs {
    pub(crate) rpc_url: String,
    pub(crate) token_address: String,
    pub(crate) address: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvmBlockArgs {
    pub(crate) rpc_url: String,
    #[serde(rename = "block_tag")]
    pub(crate) block_tag: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvmTransactionArgs {
    pub(crate) rpc_url: String,
    pub(crate) tx_hash: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvmCallArgs {
    pub(crate) rpc_url: String,
    pub(crate) to: String,
    pub(crate) data: String,
    #[serde(rename = "block_tag")]
    pub(crate) block_tag: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvmLogsArgs {
    pub(crate) rpc_url: String,
    pub(crate) address: Option<String>,
    pub(crate) topic0: Option<String>,
    #[serde(rename = "from_block")]
    pub(crate) from_block: Option<String>,
    #[serde(rename = "to_block")]
    pub(crate) to_block: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvmNonceArgs {
    pub(crate) rpc_url: String,
    pub(crate) address: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvmResolveArgs {
    pub(crate) rpc_url: String,
    pub(crate) name_or_address: String,
}

pub(crate) fn connect(rpc_url: &str) -> Result<impl Provider, ToolError> {
    let url: Url = rpc_url
        .parse()
        .map_err(|e| ToolError::InvalidUrl(format!("invalid RPC URL: {e}")))?;
    Ok(ProviderBuilder::default().connect_http(url))
}

pub(crate) fn parse_block_tag(tag: &str) -> Result<BlockNumberOrTag, ToolError> {
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
                let n = u64::from_str_radix(hex, 16)
                    .map_err(|e| ToolError::Other(format!("invalid hex block number: {e}")))?;
                Ok(BlockNumberOrTag::Number(n))
            } else if let Ok(n) = hex_or_dec.parse::<u64>() {
                Ok(BlockNumberOrTag::Number(n))
            } else {
                Err(ToolError::Other(format!("invalid block tag: {tag}")))
            }
        }
    }
}

pub(crate) fn alloy_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::Other(format!("alloy error: {e}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolResult, tool_err, tool_ok};

    #[test]
    fn test_parse_block_tag_latest() {
        let tag = parse_block_tag("latest").unwrap();
        assert!(matches!(tag, BlockNumberOrTag::Latest));
    }

    #[test]
    fn test_parse_block_tag_number() {
        let tag = parse_block_tag("12345").unwrap();
        assert!(matches!(tag, BlockNumberOrTag::Number(12345)));
    }

    #[test]
    fn test_parse_block_tag_hex() {
        let tag = parse_block_tag("0x3039").unwrap();
        assert!(matches!(tag, BlockNumberOrTag::Number(12345)));
    }

    #[test]
    fn test_parse_block_tag_invalid() {
        assert!(parse_block_tag("not_a_block").is_err());
    }

    #[test]
    fn test_invalid_arguments() {
        let result: ToolResult =
            ToolError::InvalidArguments(serde_json::from_str::<serde_json::Value>("").unwrap_err())
                .into();
        assert!(result.is_error);
        assert!(result.content.contains("invalid arguments"));
    }

    #[test]
    fn test_tool_ok() {
        let result = tool_ok("hello".to_string());
        assert!(!result.is_error);
        assert_eq!(result.content, "hello");
    }

    #[test]
    fn test_tool_err() {
        let result = tool_err("oops");
        assert!(result.is_error);
        assert_eq!(result.content, "oops");
    }
}
