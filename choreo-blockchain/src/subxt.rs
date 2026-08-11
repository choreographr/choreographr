//! Substrate/Polkadot blockchain tools (via subxt).
//!
//! Four read-only queries — chain info, account balance, arbitrary storage
//! reads, and block details — each exposed as a synchronous `execute_*` entry
//! point (used by the daemon's `Tool` wrappers) that runs an async subxt
//! implementation on the crate's sidecar tokio runtime (see [`crate::runtime`]).

use crate::{BlockchainError, block_on, truncate_tool_output};
use schemars::JsonSchema;
use serde::Deserialize;
use std::str::FromStr;
use subxt::rpcs::RpcClient as SubxtRpcClient;

const DEFAULT_WS_URL: &str = "wss://rpc.polkadot.io";

type SubxtClient = subxt::OnlineClient<subxt::PolkadotConfig>;

// ── Argument types (public so the daemon can derive JSON Schemas) ────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubxtChainArgs {
    /// WebSocket URL of the Substrate node (e.g., wss://rpc.polkadot.io)
    pub ws_url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubxtBalanceArgs {
    /// SS58-encoded account address
    pub address: String,
    /// WebSocket URL of the Substrate node (e.g., wss://rpc.polkadot.io)
    pub ws_url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubxtQueryArgs {
    /// Pallet name (e.g., System, Balances, Staking)
    pub pallet: String,
    /// Storage item name (e.g., Account, TotalIssuance, Validators)
    pub storage_item: String,
    /// Optional hex-encoded storage key bytes (without 0x prefix)
    pub key: Option<String>,
    /// WebSocket URL of the Substrate node (e.g., wss://rpc.polkadot.io)
    pub ws_url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubxtBlockArgs {
    /// Optional block number (if omitted, gets the latest finalized block)
    pub block_number: Option<u64>,
    /// WebSocket URL of the Substrate node (e.g., wss://rpc.polkadot.io)
    pub ws_url: Option<String>,
}

// ── Synchronous entry points (used by the daemon's Tool wrappers) ────────

pub fn execute_subxt_chain(args: &SubxtChainArgs) -> Result<String, BlockchainError> {
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    let output = block_on(subxt_chain_impl(&ws_url))??;
    Ok(truncate_tool_output(&output))
}

pub fn execute_subxt_balance(args: &SubxtBalanceArgs) -> Result<String, BlockchainError> {
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    let output = block_on(subxt_balance_impl(&ws_url, &args.address))??;
    Ok(truncate_tool_output(&output))
}

pub fn execute_subxt_query(args: &SubxtQueryArgs) -> Result<String, BlockchainError> {
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    let output = block_on(subxt_query_impl(
        &ws_url,
        &args.pallet,
        &args.storage_item,
        args.key.as_deref(),
    ))??;
    Ok(truncate_tool_output(&output))
}

pub fn execute_subxt_block(args: &SubxtBlockArgs) -> Result<String, BlockchainError> {
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    let output = block_on(subxt_block_impl(&ws_url, args.block_number))??;
    Ok(truncate_tool_output(&output))
}

// ── Invocation descriptions (shown in the TUI / tool transcripts) ────────

pub fn describe_subxt_chain_invocation(args: &SubxtChainArgs) -> String {
    let url = args.ws_url.as_deref().unwrap_or(DEFAULT_WS_URL);
    format!("Querying Substrate/Polkadot chain info from {url}.")
}

pub fn describe_subxt_balance_invocation(args: &SubxtBalanceArgs) -> String {
    let url = args.ws_url.as_deref().unwrap_or(DEFAULT_WS_URL);
    format!("Querying balance of {} on {url}.", args.address)
}

pub fn describe_subxt_query_invocation(args: &SubxtQueryArgs) -> String {
    let url = args.ws_url.as_deref().unwrap_or(DEFAULT_WS_URL);
    format!(
        "Querying storage {}.{} on {url}.",
        args.pallet, args.storage_item
    )
}

pub fn describe_subxt_block_invocation(args: &SubxtBlockArgs) -> String {
    let url = args.ws_url.as_deref().unwrap_or(DEFAULT_WS_URL);
    match args.block_number {
        Some(n) => format!("Querying block #{n} on {url}."),
        None => format!("Querying latest block on {url}."),
    }
}

// ── Shared client plumbing ───────────────────────────────────────────────

fn subxt_err(e: impl std::fmt::Display) -> BlockchainError {
    BlockchainError::Subxt(format!("{e}"))
}

async fn connect_client(ws_url: &str) -> Result<SubxtClient, BlockchainError> {
    SubxtClient::from_insecure_url(ws_url)
        .await
        .map_err(subxt_err)
}

async fn connect_rpc(ws_url: &str) -> Result<SubxtRpcClient, BlockchainError> {
    SubxtRpcClient::from_insecure_url(ws_url)
        .await
        .map_err(subxt_err)
}

// ── Async implementations ────────────────────────────────────────────────

async fn subxt_chain_impl(ws_url: &str) -> Result<String, BlockchainError> {
    let rpc = connect_rpc(ws_url).await?;
    let client = connect_client(ws_url).await?;

    let chain: String = rpc
        .request("system_chain", subxt::rpcs::rpc_params![])
        .await
        .map_err(subxt_err)?;
    let name: String = rpc
        .request("system_name", subxt::rpcs::rpc_params![])
        .await
        .map_err(subxt_err)?;
    let version: String = rpc
        .request("system_version", subxt::rpcs::rpc_params![])
        .await
        .map_err(subxt_err)?;
    let chain_type: String = rpc
        .request("system_chainType", subxt::rpcs::rpc_params![])
        .await
        .map_err(subxt_err)?;
    let props: serde_json::Value = rpc
        .request("system_properties", subxt::rpcs::rpc_params![])
        .await
        .map_err(subxt_err)?;
    let health: serde_json::Value = rpc
        .request("system_health", subxt::rpcs::rpc_params![])
        .await
        .map_err(subxt_err)?;
    let finalized_hash: String = rpc
        .request("chain_getFinalizedHead", subxt::rpcs::rpc_params![])
        .await
        .map_err(subxt_err)?;

    let genesis_hash = client.genesis_hash();

    let at = client.at_current_block().await.map_err(subxt_err)?;
    let best_number = at.block_number();
    let best_hash = at.block_hash();

    let mut out = String::new();
    out.push_str(&format!("chain: {chain}\n"));
    out.push_str(&format!("chain_type: {chain_type}\n"));
    out.push_str(&format!("node_name: {name}\n"));
    out.push_str(&format!("node_version: {version}\n"));
    out.push_str(&format!("genesis_hash: {genesis_hash:#x}\n"));
    out.push_str(&format!("best_block: #{best_number} ({best_hash:#x})\n"));
    out.push_str(&format!("finalized_head: {finalized_hash}\n"));
    out.push_str(&format!("properties: {props}\n"));
    out.push_str(&format!("health: {health}"));
    Ok(out)
}

async fn subxt_balance_impl(ws_url: &str, address: &str) -> Result<String, BlockchainError> {
    let account_id = subxt::utils::AccountId32::from_str(address)
        .map_err(|e| BlockchainError::Other(format!("invalid SS58 address: {e}")))?;

    let client = connect_client(ws_url).await?;
    let at = client.at_current_block().await.map_err(subxt_err)?;

    let addr = subxt::storage::dynamic("System", "Account");
    let key = subxt::dynamic::Value::from_bytes(account_id.0);
    let result = at
        .storage()
        .try_fetch(addr, vec![key])
        .await
        .map_err(subxt_err)?;

    match result {
        Some(storage_value) => {
            let decoded: subxt::dynamic::Value = storage_value.decode().map_err(subxt_err)?;
            Ok(format_balance_value(&decoded, address))
        }
        None => Ok(format!(
            "address {address}: account does not exist on chain"
        )),
    }
}

/// Render the decoded `System.Account` storage value as readable text.
fn format_balance_value(value: &subxt::dynamic::Value, address: &str) -> String {
    let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("{value:?}"));
    format!("address: {address}\naccount_info: {json}")
}

async fn subxt_query_impl(
    ws_url: &str,
    pallet: &str,
    storage_item: &str,
    key_hex: Option<&str>,
) -> Result<String, BlockchainError> {
    let client = connect_client(ws_url).await?;
    let at = client.at_current_block().await.map_err(subxt_err)?;

    let addr = subxt::storage::dynamic(pallet, storage_item);

    let key_parts: Vec<subxt::dynamic::Value> = match key_hex {
        Some(hex) => {
            let bytes = hex::decode(hex)
                .map_err(|e| BlockchainError::Other(format!("invalid hex key: {e}")))?;
            vec![subxt::dynamic::Value::from_bytes(bytes)]
        }
        None => vec![],
    };

    let result = at
        .storage()
        .try_fetch(addr, key_parts)
        .await
        .map_err(subxt_err)?;

    match result {
        Some(storage_value) => {
            let decoded: subxt::dynamic::Value = storage_value.decode().map_err(subxt_err)?;
            let json =
                serde_json::to_string_pretty(&decoded).unwrap_or_else(|_| format!("{decoded:?}"));
            Ok(json)
        }
        None => Ok("storage value: None (no value found at this key)".to_string()),
    }
}

async fn subxt_block_impl(
    ws_url: &str,
    block_number: Option<u64>,
) -> Result<String, BlockchainError> {
    let client = connect_client(ws_url).await?;

    let at = match block_number {
        Some(num) => client.at_block(num).await.map_err(subxt_err)?,
        None => client.at_current_block().await.map_err(subxt_err)?,
    };

    let number = at.block_number();
    let hash = at.block_hash();
    let header = at.block_header().await.map_err(subxt_err)?;
    let spec_version = at.spec_version();

    let rpc = connect_rpc(ws_url).await?;
    let block_json: serde_json::Value = rpc
        .request(
            "chain_getBlock",
            subxt::rpcs::rpc_params![serde_json::json!(format!("{hash:#x}"))],
        )
        .await
        .map_err(subxt_err)?;

    let mut out = String::new();
    out.push_str(&format!("block: #{number} ({hash:#x})\n"));
    out.push_str(&format!("spec_version: {spec_version}\n"));
    out.push_str(&format!(
        "header parent_hash: {parent_hash:#x}\n",
        parent_hash = header.parent_hash
    ));
    out.push_str(&format!(
        "header state_root: {state_root:#x}\n",
        state_root = header.state_root
    ));
    out.push_str(&format!(
        "header extrinsics_root: {extrinsics_root:#x}\n",
        extrinsics_root = header.extrinsics_root
    ));
    out.push_str(&format!(
        "header number: {header_number}\n",
        header_number = header.number
    ));
    out.push_str(&format!("full_block: {block_json}"));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_balance_value_pretty_prints() {
        // A decoded System.Account value renders address + pretty JSON.
        let value = subxt::dynamic::Value::u128(1_000_000_000_000u128);
        let out = format_balance_value(&value, "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY");
        assert!(out.starts_with("address: 5Grwva"));
        assert!(out.contains("account_info:"));
    }

    #[test]
    fn describe_invocation_defaults_to_polkadot_rpc() {
        assert!(
            describe_subxt_chain_invocation(&SubxtChainArgs { ws_url: None })
                .contains(DEFAULT_WS_URL)
        );
        let args = SubxtChainArgs {
            ws_url: Some("wss://custom.example".into()),
        };
        assert!(describe_subxt_chain_invocation(&args).contains("wss://custom.example"));
    }
}
