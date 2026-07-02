use crate::{ToolResult, truncate_tool_output};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::{BlockNumberOrTag, Filter, TransactionRequest};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde::Deserialize;
use std::str::FromStr;
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
struct RpcUrlArgs {
    rpc_url: String,
}

#[derive(Debug, Deserialize)]
struct EvmBalanceArgs {
    rpc_url: String,
    address: String,
}

#[derive(Debug, Deserialize)]
struct EvmTokenBalanceArgs {
    rpc_url: String,
    token_address: String,
    address: String,
}

#[derive(Debug, Deserialize)]
struct EvmBlockArgs {
    rpc_url: String,
    #[serde(rename = "block_tag")]
    block_tag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EvmTransactionArgs {
    rpc_url: String,
    tx_hash: String,
}

#[derive(Debug, Deserialize)]
struct EvmCallArgs {
    rpc_url: String,
    to: String,
    data: String,
    #[serde(rename = "block_tag")]
    block_tag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EvmLogsArgs {
    rpc_url: String,
    address: Option<String>,
    topic0: Option<String>,
    #[serde(rename = "from_block")]
    from_block: Option<String>,
    #[serde(rename = "to_block")]
    to_block: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EvmNonceArgs {
    rpc_url: String,
    address: String,
}

#[derive(Debug, Deserialize)]
struct EvmResolveArgs {
    rpc_url: String,
    name_or_address: String,
}

fn connect(rpc_url: &str) -> Result<impl Provider, String> {
    let url: Url = rpc_url
        .parse()
        .map_err(|e: url::ParseError| format!("invalid RPC URL: {e}"))?;
    Ok(ProviderBuilder::default().connect_http(url))
}

fn parse_block_tag(tag: &str) -> Result<BlockNumberOrTag, String> {
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
                    .map_err(|e| format!("invalid hex block number: {e}"))?;
                Ok(BlockNumberOrTag::Number(n))
            } else if let Ok(n) = hex_or_dec.parse::<u64>() {
                Ok(BlockNumberOrTag::Number(n))
            } else {
                Err(format!("invalid block tag: {tag}"))
            }
        }
    }
}

fn invalid_arguments(error: serde_json::Error) -> ToolResult {
    ToolResult {
        content: format!("invalid arguments: {error}"),
        is_error: true,
    }
}

fn map_result(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(content) => ToolResult {
            content: truncate_tool_output(&content),
            is_error: false,
        },
        Err(error) => ToolResult {
            content: error,
            is_error: true,
        },
    }
}

fn map_err(e: impl std::fmt::Display) -> String {
    format!("alloy error: {e}")
}

pub(crate) async fn execute_evm_chain_tool(arguments_json: &str) -> ToolResult {
    let args: RpcUrlArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(evm_chain_impl(&args.rpc_url).await)
}

pub(crate) async fn execute_evm_balance_tool(arguments_json: &str) -> ToolResult {
    let args: EvmBalanceArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(evm_balance_impl(&args.rpc_url, &args.address).await)
}

pub(crate) async fn execute_evm_token_balance_tool(arguments_json: &str) -> ToolResult {
    let args: EvmTokenBalanceArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(
        evm_token_balance_impl(&args.rpc_url, &args.token_address, &args.address).await,
    )
}

pub(crate) async fn execute_evm_block_tool(arguments_json: &str) -> ToolResult {
    let args: EvmBlockArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(evm_block_impl(&args.rpc_url, args.block_tag.as_deref()).await)
}

pub(crate) async fn execute_evm_transaction_tool(arguments_json: &str) -> ToolResult {
    let args: EvmTransactionArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(evm_transaction_impl(&args.rpc_url, &args.tx_hash).await)
}

pub(crate) async fn execute_evm_call_tool(arguments_json: &str) -> ToolResult {
    let args: EvmCallArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(
        evm_call_impl(&args.rpc_url, &args.to, &args.data, args.block_tag.as_deref()).await,
    )
}

pub(crate) async fn execute_evm_gas_tool(arguments_json: &str) -> ToolResult {
    let args: RpcUrlArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(evm_gas_impl(&args.rpc_url).await)
}

pub(crate) async fn execute_evm_logs_tool(arguments_json: &str) -> ToolResult {
    let args: EvmLogsArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(
        evm_logs_impl(
            &args.rpc_url,
            args.address.as_deref(),
            args.topic0.as_deref(),
            args.from_block.as_deref(),
            args.to_block.as_deref(),
        )
        .await,
    )
}

pub(crate) async fn execute_evm_nonce_tool(arguments_json: &str) -> ToolResult {
    let args: EvmNonceArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(evm_nonce_impl(&args.rpc_url, &args.address).await)
}

pub(crate) async fn execute_evm_resolve_tool(arguments_json: &str) -> ToolResult {
    let args: EvmResolveArgs = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return invalid_arguments(e),
    };
    map_result(evm_resolve_impl(&args.rpc_url, &args.name_or_address).await)
}

async fn evm_chain_impl(rpc_url: &str) -> Result<String, String> {
    let provider = connect(rpc_url)?;

    let chain_id = provider.get_chain_id().await.map_err(map_err)?;
    let block_number = provider.get_block_number().await.map_err(map_err)?;
    let gas_price = provider.get_gas_price().await.map_err(map_err)?;
    let client_version = provider.get_client_version().await.map_err(map_err)?;
    let max_priority_fee = provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(map_err)?;

    let mut out = String::new();
    out.push_str(&format!("chain_id: {chain_id}\n"));
    out.push_str(&format!("block_number: {block_number}\n"));
    out.push_str(&format!("gas_price: {gas_price} wei\n"));
    out.push_str(&format!("max_priority_fee: {max_priority_fee} wei\n"));
    out.push_str(&format!("client_version: {client_version}"));
    Ok(out)
}

async fn evm_balance_impl(rpc_url: &str, address_str: &str) -> Result<String, String> {
    let provider = connect(rpc_url)?;
    let address =
        Address::from_str(address_str).map_err(|e| format!("invalid address: {e}"))?;

    let balance = provider.get_balance(address).await.map_err(map_err)?;

    Ok(format!("address: {address_str}\nbalance: {balance} wei"))
}

async fn evm_token_balance_impl(
    rpc_url: &str,
    token_address_str: &str,
    address_str: &str,
) -> Result<String, String> {
    let provider = connect(rpc_url)?;
    let token_address = Address::from_str(token_address_str)
        .map_err(|e| format!("invalid token address: {e}"))?;
    let owner_address =
        Address::from_str(address_str).map_err(|e| format!("invalid owner address: {e}"))?;

    let call = balanceOfCall {
        account: owner_address,
    };
    let call_data = SolCall::abi_encode(&call);
    let tx = TransactionRequest::default()
        .with_to(token_address)
        .with_input(call_data);

    let result: Bytes = provider.call(tx).await.map_err(map_err)?;
    let return_data =
        balanceOfCall::abi_decode_returns(&result).map_err(map_err)?;
    let balance = return_data;

    let mut out = String::new();
    out.push_str(&format!("token_address: {token_address_str}\n"));
    out.push_str(&format!("owner_address: {address_str}\n"));
    out.push_str(&format!("balance: {balance}"));

    let sym_call = symbolCall {};
    let sym_data = SolCall::abi_encode(&sym_call);
    let sym_tx = TransactionRequest::default()
        .with_to(token_address)
        .with_input(sym_data);
    if let Ok(sym_result) = provider.call(sym_tx).await {
        if let Ok(sym_ret) = symbolCall::abi_decode_returns(&sym_result) {
            out.push_str(&format!("\nsymbol: {sym_ret}"));
        }
    }

    Ok(out)
}

async fn evm_block_impl(rpc_url: &str, block_tag: Option<&str>) -> Result<String, String> {
    let provider = connect(rpc_url)?;

    let block_num = match block_tag {
        Some(tag) => parse_block_tag(tag)?,
        None => BlockNumberOrTag::Latest,
    };

    let block = provider
        .get_block_by_number(block_num)
        .await
        .map_err(map_err)?
        .ok_or("block not found")?;

    let number = block.header.number;
    let hash = block.header.hash;
    let timestamp = block.header.timestamp;
    let gas_used = block.header.gas_used;
    let gas_limit = block.header.gas_limit;
    let tx_count = block.transactions.len();
    let base_fee = block.header.base_fee_per_gas.unwrap_or(0);

    let mut out = String::new();
    out.push_str(&format!("block: #{number}\n"));
    out.push_str(&format!("hash: {hash:#x}\n"));
    out.push_str(&format!("timestamp: {timestamp}\n"));
    out.push_str(&format!("transactions: {tx_count}\n"));
    out.push_str(&format!("gas_used: {gas_used}\n"));
    out.push_str(&format!("gas_limit: {gas_limit}\n"));
    out.push_str(&format!("base_fee: {base_fee} wei"));
    Ok(out)
}

async fn evm_transaction_impl(rpc_url: &str, tx_hash_str: &str) -> Result<String, String> {
    let provider = connect(rpc_url)?;

    let stripped = tx_hash_str
        .strip_prefix("0x")
        .or_else(|| tx_hash_str.strip_prefix("0X"))
        .unwrap_or(tx_hash_str);
    let hash = B256::from_str(stripped).map_err(|e| format!("invalid tx hash: {e}"))?;

    let receipt = provider
        .get_transaction_receipt(hash)
        .await
        .map_err(map_err)?
        .ok_or("transaction not found")?;

    let tx_hash = receipt.transaction_hash;
    let block_number = receipt.block_number.unwrap_or(0);
    let from = receipt.from;
    let to = receipt
        .to
        .map(|t| t.to_string())
        .unwrap_or_else(|| "contract_creation".to_string());
    let gas_used = receipt.gas_used;
    let effective_gas_price = receipt.effective_gas_price;
    let log_count = receipt.logs().len();

    let mut out = String::new();
    out.push_str(&format!("hash: {tx_hash:#x}\n"));
    out.push_str(&format!("block: #{block_number}\n"));
    out.push_str(&format!("from: {from}\n"));
    out.push_str(&format!("to: {to}\n"));
    out.push_str(&format!("gas_used: {gas_used}\n"));
    out.push_str(&format!("effective_gas_price: {effective_gas_price} wei\n"));
    out.push_str(&format!("logs: {log_count}"));
    Ok(out)
}

async fn evm_call_impl(
    rpc_url: &str,
    to_str: &str,
    data_str: &str,
    _block_tag: Option<&str>,
) -> Result<String, String> {
    let provider = connect(rpc_url)?;
    let to = Address::from_str(to_str).map_err(|e| format!("invalid 'to' address: {e}"))?;

    let data_hex = data_str
        .strip_prefix("0x")
        .or_else(|| data_str.strip_prefix("0X"))
        .unwrap_or(data_str);
    let input_data =
        hex::decode(data_hex).map_err(|e| format!("invalid hex data: {e}"))?;
    let tx = TransactionRequest::default()
        .with_to(to)
        .with_input(input_data);

    let result: Bytes = provider.call(tx).await.map_err(map_err)?;

    Ok(format!(
        "to: {to_str}\ndata: {data_str}\nresult: 0x{}",
        hex::encode(result.as_ref())
    ))
}

async fn evm_gas_impl(rpc_url: &str) -> Result<String, String> {
    let provider = connect(rpc_url)?;

    let gas_price = provider.get_gas_price().await.map_err(map_err)?;
    let max_priority_fee = provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(map_err)?;
    let estimation = provider
        .estimate_eip1559_fees()
        .await
        .map_err(map_err)?;

    let mut out = String::new();
    out.push_str(&format!("gas_price: {gas_price} wei\n"));
    out.push_str(&format!("max_priority_fee_per_gas: {max_priority_fee} wei\n"));
    out.push_str(&format!(
        "estimated_max_fee_per_gas: {} wei\n",
        estimation.max_fee_per_gas
    ));
    out.push_str(&format!(
        "estimated_max_priority_fee_per_gas: {} wei",
        estimation.max_priority_fee_per_gas
    ));
    Ok(out)
}

async fn evm_logs_impl(
    rpc_url: &str,
    address_str: Option<&str>,
    topic0_str: Option<&str>,
    from_block_str: Option<&str>,
    to_block_str: Option<&str>,
) -> Result<String, String> {
    let provider = connect(rpc_url)?;

    let mut filter = Filter::new();

    if let Some(addr) = address_str {
        let address =
            Address::from_str(addr).map_err(|e| format!("invalid address: {e}"))?;
        filter = filter.address(address);
    }

    if let Some(t0) = topic0_str {
        let stripped = t0.strip_prefix("0x").unwrap_or(t0);
        let topic =
            B256::from_str(stripped).map_err(|e| format!("invalid topic0: {e}"))?;
        filter = filter.event_signature(topic);
    }

    if let Some(fb) = from_block_str {
        filter = filter.from_block(parse_block_tag(fb)?);
    }

    if let Some(tb) = to_block_str {
        filter = filter.to_block(parse_block_tag(tb)?);
    }

    let logs = provider.get_logs(&filter).await.map_err(map_err)?;

    if logs.is_empty() {
        return Ok("no logs found for this filter".to_string());
    }

    let mut out = String::new();
    out.push_str(&format!("log_count: {}\n\n", logs.len()));

    for (i, log) in logs.iter().enumerate() {
        let log_address = log.address();
        let topics: Vec<String> = log.topics().iter().map(|t| format!("{t:#x}")).collect();
        let data_hex = hex::encode(log.data().data.clone());
        out.push_str(&format!(
            "log[{}]:\n  address: {log_address}\n  topics: [{}]\n  data: 0x{data_hex}\n\n",
            i,
            topics.join(", ")
        ));
    }

    Ok(out)
}

async fn evm_nonce_impl(rpc_url: &str, address_str: &str) -> Result<String, String> {
    let provider = connect(rpc_url)?;
    let address =
        Address::from_str(address_str).map_err(|e| format!("invalid address: {e}"))?;

    let nonce = provider
        .get_transaction_count(address)
        .await
        .map_err(map_err)?;

    Ok(format!(
        "address: {address_str}\ntransaction_count (nonce): {nonce}"
    ))
}

async fn evm_resolve_impl(rpc_url: &str, name_or_address: &str) -> Result<String, String> {
    let provider = connect(rpc_url)?;

    if name_or_address.ends_with(".eth") {
        let result: Result<Address, String> = provider
            .raw_request(
                std::borrow::Cow::Borrowed("ens_resolve"),
                (name_or_address,),
            )
            .await
            .map_err(|_| format!("ENS resolution failed for {name_or_address}"));
        match result {
            Ok(addr) => Ok(format!("name: {name_or_address}\naddress: {addr:#x}")),
            Err(e) => Err(e),
        }
    } else {
        let address = Address::from_str(name_or_address)
            .map_err(|e| format!("invalid address: {e}"))?;
        let result: Result<String, String> = provider
            .raw_request(
                std::borrow::Cow::Borrowed("ens_reverse"),
                (address,),
            )
            .await
            .map_err(|_| format!("reverse ENS lookup failed for {name_or_address}"));
        match result {
            Ok(name) => Ok(format!("address: {name_or_address}\nname: {name}")),
            Err(e) => Err(e),
        }
    }
}

define_tool!(EvmChain, "evm_chain",
    "Query information about an EVM blockchain node: chain ID, latest block number, gas price, max priority fee, and client version.",
    execute_evm_chain_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node (e.g., https://ethereum-rpc.publicnode.com)"}},"required":["rpc_url"],"additionalProperties":false})
);

define_tool!(EvmBalance, "evm_balance",
    "Query the native ETH/coin balance of an address on an EVM blockchain.",
    execute_evm_balance_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"address":{"type":"string","description":"0x-prefixed hex address"}},"required":["rpc_url","address"],"additionalProperties":false})
);

define_tool!(EvmTokenBalance, "evm_token_balance",
    "Query the ERC-20 token balance for an address. Also attempts to fetch the token symbol.",
    execute_evm_token_balance_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"token_address":{"type":"string","description":"0x-prefixed ERC-20 token contract address"},"address":{"type":"string","description":"0x-prefixed wallet address to check balance for"}},"required":["rpc_url","token_address","address"],"additionalProperties":false})
);

define_tool!(EvmBlock, "evm_block",
    "Get details about a block on an EVM blockchain: block number, hash, timestamp, transaction count, gas used/limit, and base fee.",
    execute_evm_block_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"block_tag":{"type":"string","description":"Block number (decimal or 0x-hex), or 'latest', 'finalized', 'safe', 'pending', 'earliest'","default":"latest"}},"required":["rpc_url"],"additionalProperties":false})
);

define_tool!(EvmTransaction, "evm_transaction",
    "Get details about a transaction on an EVM blockchain by its hash. Returns hash, block number, from/to, gas used, effective gas price, and log count.",
    execute_evm_transaction_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"tx_hash":{"type":"string","description":"0x-prefixed transaction hash"}},"required":["rpc_url","tx_hash"],"additionalProperties":false})
);

define_tool!(EvmCall, "evm_call",
    "Execute a read-only smart contract call (eth_call) on an EVM blockchain. Returns the raw hex-encoded result bytes.",
    execute_evm_call_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"to":{"type":"string","description":"0x-prefixed contract address to call"},"data":{"type":"string","description":"0x-prefixed hex-encoded call data (method selector + ABI-encoded params)"},"block_tag":{"type":"string","description":"Block number (decimal or 0x-hex), or 'latest', 'finalized', 'safe', 'pending', 'earliest'","default":"latest"}},"required":["rpc_url","to","data"],"additionalProperties":false})
);

define_tool!(EvmGas, "evm_gas",
    "Get current gas fee estimates on an EVM blockchain: gas price, max priority fee, and EIP-1559 fee estimation.",
    execute_evm_gas_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"}},"required":["rpc_url"],"additionalProperties":false})
);

define_tool!(EvmLogs, "evm_logs",
    "Query event logs on an EVM blockchain with optional filters by contract address, topic0, and block range.",
    execute_evm_logs_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"address":{"type":"string","description":"Optional 0x-prefixed contract address to filter logs by"},"topic0":{"type":"string","description":"Optional 0x-prefixed event signature hash (topic0) to filter by"},"from_block":{"type":"string","description":"Optional starting block number or tag (e.g., '0x0', 'latest')"},"to_block":{"type":"string","description":"Optional ending block number or tag (e.g., '0x0', 'latest')"}},"required":["rpc_url"],"additionalProperties":false})
);

define_tool!(EvmNonce, "evm_nonce",
    "Get the transaction count (nonce) for an address on an EVM blockchain.",
    execute_evm_nonce_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node"},"address":{"type":"string","description":"0x-prefixed hex address"}},"required":["rpc_url","address"],"additionalProperties":false})
);

define_tool!(EvmResolve, "evm_resolve",
    "Resolve an ENS name to an address, or reverse-resolve an address to an ENS name on an EVM blockchain.",
    execute_evm_resolve_tool,
    serde_json::json!({"type":"object","properties":{"rpc_url":{"type":"string","description":"JSON-RPC URL of the EVM node (must support ENS)"},"name_or_address":{"type":"string","description":"ENS name (e.g., 'vitalik.eth') or 0x-prefixed address for reverse lookup"}},"required":["rpc_url","name_or_address"],"additionalProperties":false})
);

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = invalid_arguments(
            serde_json::from_str::<serde_json::Value>("").unwrap_err(),
        );
        assert!(result.is_error);
        assert!(result.content.contains("invalid arguments"));
    }

    #[test]
    fn test_map_result_ok() {
        let result = map_result(Ok("hello".to_string()));
        assert!(!result.is_error);
        assert_eq!(result.content, "hello");
    }

    #[test]
    fn test_map_result_err() {
        let result = map_result(Err("oops".to_string()));
        assert!(result.is_error);
        assert_eq!(result.content, "oops");
    }
}
