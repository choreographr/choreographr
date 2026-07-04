use super::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use serde::Deserialize;
use std::str::FromStr;
use subxt::rpcs::RpcClient as SubxtRpcClient;

const DEFAULT_WS_URL: &str = "wss://rpc.polkadot.io";

type SubxtClient = subxt::OnlineClient<subxt::PolkadotConfig>;

#[derive(Debug, Deserialize)]
struct WsUrlArgs {
    ws_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubxtBalanceArgs {
    address: String,
    ws_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubxtQueryArgs {
    pallet: String,
    storage_item: String,
    key: Option<String>,
    ws_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubxtBlockArgs {
    block_number: Option<u64>,
    ws_url: Option<String>,
}

pub(crate) fn execute_subxt_chain_tool(arguments_json: &str) -> ToolResult {
    match execute_subxt_chain_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_subxt_chain_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: WsUrlArgs = serde_json::from_str(arguments_json)?;
    let ws_url = args.ws_url.unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    let output = crate::runtime::get().block_on(subxt_chain_impl(&ws_url))?;
    Ok(truncate_tool_output(&output))
}

pub(crate) fn execute_subxt_balance_tool(arguments_json: &str) -> ToolResult {
    match execute_subxt_balance_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_subxt_balance_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: SubxtBalanceArgs = serde_json::from_str(arguments_json)?;
    let ws_url = args.ws_url.unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    let output =
        crate::runtime::get().block_on(subxt_balance_impl(&ws_url, &args.address))?;
    Ok(truncate_tool_output(&output))
}

pub(crate) fn execute_subxt_query_tool(arguments_json: &str) -> ToolResult {
    match execute_subxt_query_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_subxt_query_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: SubxtQueryArgs = serde_json::from_str(arguments_json)?;
    let ws_url = args.ws_url.unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    let output = crate::runtime::get().block_on(subxt_query_impl(
        &ws_url,
        &args.pallet,
        &args.storage_item,
        args.key.as_deref(),
    ))?;
    Ok(truncate_tool_output(&output))
}

pub(crate) fn execute_subxt_block_tool(arguments_json: &str) -> ToolResult {
    match execute_subxt_block_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_subxt_block_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: SubxtBlockArgs = serde_json::from_str(arguments_json)?;
    let ws_url = args.ws_url.unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    let output =
        crate::runtime::get().block_on(subxt_block_impl(&ws_url, args.block_number))?;
    Ok(truncate_tool_output(&output))
}

fn subxt_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::Other(format!("subxt error: {e}"))
}

async fn connect_client(ws_url: &str) -> Result<SubxtClient, ToolError> {
    SubxtClient::from_insecure_url(ws_url)
        .await
        .map_err(subxt_err)
}

async fn connect_rpc(ws_url: &str) -> Result<SubxtRpcClient, ToolError> {
    SubxtRpcClient::from_insecure_url(ws_url)
        .await
        .map_err(subxt_err)
}

async fn subxt_chain_impl(ws_url: &str) -> Result<String, ToolError> {
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

async fn subxt_balance_impl(ws_url: &str, address: &str) -> Result<String, ToolError> {
    let account_id = subxt::utils::AccountId32::from_str(address)
        .map_err(|e| ToolError::Other(format!("invalid SS58 address: {e}")))?;

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

fn format_balance_value(value: &subxt::dynamic::Value, address: &str) -> String {
    let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("{value:?}"));
    format!("address: {address}\naccount_info: {json}")
}

async fn subxt_query_impl(
    ws_url: &str,
    pallet: &str,
    storage_item: &str,
    key_hex: Option<&str>,
) -> Result<String, ToolError> {
    let client = connect_client(ws_url).await?;
    let at = client.at_current_block().await.map_err(subxt_err)?;

    let addr = subxt::storage::dynamic(pallet, storage_item);

    let key_parts: Vec<subxt::dynamic::Value> = match key_hex {
        Some(hex) => {
            let bytes =
                hex::decode(hex).map_err(|e| ToolError::Other(format!("invalid hex key: {e}")))?;
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

async fn subxt_block_impl(ws_url: &str, block_number: Option<u64>) -> Result<String, ToolError> {
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

define_tool!(
    SubxtChain,
    "subxt_chain",
    "Query information about a Substrate/Polkadot blockchain node: chain name, chain type, node name/version, genesis hash, best block, finalized head, system properties, and health.",
    execute_subxt_chain_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "ws_url": {
                "type": "string",
                "description": "WebSocket URL of the Substrate node (e.g., wss://rpc.polkadot.io)",
                "default": "wss://rpc.polkadot.io"
            }
        },
        "additionalProperties": false
    })
);

define_tool!(
    SubxtBalance,
    "subxt_balance",
    "Query the balance of an account on a Substrate/Polkadot blockchain. Returns the System.Account info (free, reserved, frozen balances).",
    execute_subxt_balance_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "address": {
                "type": "string",
                "description": "SS58-encoded account address"
            },
            "ws_url": {
                "type": "string",
                "description": "WebSocket URL of the Substrate node (e.g., wss://rpc.polkadot.io)",
                "default": "wss://rpc.polkadot.io"
            }
        },
        "required": ["address"],
        "additionalProperties": false
    })
);

define_tool!(
    SubxtQuery,
    "subxt_query",
    "Query a storage value from a Substrate/Polkadot blockchain by pallet and storage item name. Returns the decoded SCALE value as JSON.",
    execute_subxt_query_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "pallet": {
                "type": "string",
                "description": "Pallet name (e.g., System, Balances, Staking)"
            },
            "storage_item": {
                "type": "string",
                "description": "Storage item name (e.g., Account, TotalIssuance, Validators)"
            },
            "key": {
                "type": "string",
                "description": "Optional hex-encoded storage key bytes (without 0x prefix)"
            },
            "ws_url": {
                "type": "string",
                "description": "WebSocket URL of the Substrate node (e.g., wss://rpc.polkadot.io)",
                "default": "wss://rpc.polkadot.io"
            }
        },
        "required": ["pallet", "storage_item"],
        "additionalProperties": false
    })
);

define_tool!(
    SubxtBlock,
    "subxt_block",
    "Get details about a block on a Substrate/Polkadot blockchain: block number, hash, parent hash, state root, extrinsics root, and full block JSON.",
    execute_subxt_block_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "block_number": {
                "type": "integer",
                "description": "Optional block number (if omitted, gets the latest finalized block)"
            },
            "ws_url": {
                "type": "string",
                "description": "WebSocket URL of the Substrate node (e.g., wss://rpc.polkadot.io)",
                "default": "wss://rpc.polkadot.io"
            }
        },
        "additionalProperties": false
    })
);
