//! Substrate/Polkadot blockchain tools (via subxt).
//!
//! Four read-only queries — chain info, account balance, arbitrary storage
//! reads, and block details — each exposed as a synchronous `execute_*` entry
//! point (used by the daemon's `Tool` wrappers) that runs an async subxt
//! implementation on the crate's sidecar tokio runtime (see [`crate::runtime`]).
//!
//! Every call is bounded by [`crate::RPC_TIMEOUT`] via [`crate::rpc_call`], so
//! a black-holed `ws_url` returns a clean error instead of leaking the blocked
//! execution thread until the network gives up. Node-supplied strings are
//! sanitized before they enter the transcript: scalar strings (chain/version)
//! via [`crate::sanitize_value`], serde-rendered JSON (decoded storage values,
//! block dumps) via [`crate::sanitize_json`], which keeps the JSON's
//! structural line breaks while escaping anything hostile on each line.

use crate::{
    BlockchainError, MAX_TOOL_OUTPUT_BYTES, block_on, log_execution, rpc_call, sanitize_json,
    sanitize_value, strip_hex_prefix, truncate_tool_output,
};
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
    /// Optional hex-encoded bytes for the storage key — the *un-hashed* value
    /// the pallet's hasher expects (e.g. the raw 32-byte account id for
    /// System.Account), NOT a pre-computed 32-byte storage key: subxt applies
    /// the pallet's hasher itself, so a pre-hashed key would be double-hashed
    /// and never match. 0x prefix optional.
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
    log_execution("subxt_chain", &ws_url);
    let output = block_on(rpc_call(subxt_chain_impl(&ws_url)))??;
    Ok(truncate_tool_output(&output))
}

pub fn execute_subxt_balance(args: &SubxtBalanceArgs) -> Result<String, BlockchainError> {
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    log_execution("subxt_balance", &ws_url);
    let output = block_on(rpc_call(subxt_balance_impl(&ws_url, &args.address)))??;
    Ok(truncate_tool_output(&output))
}

pub fn execute_subxt_query(args: &SubxtQueryArgs) -> Result<String, BlockchainError> {
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    log_execution("subxt_query", &ws_url);
    let output = block_on(rpc_call(subxt_query_impl(
        &ws_url,
        &args.pallet,
        &args.storage_item,
        args.key.as_deref(),
    )))??;
    Ok(truncate_tool_output(&output))
}

pub fn execute_subxt_block(args: &SubxtBlockArgs) -> Result<String, BlockchainError> {
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    log_execution("subxt_block", &ws_url);
    let output = block_on(rpc_call(subxt_block_impl(&ws_url, args.block_number)))??;
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

/// Validate that `ws_url` parses and uses a WebSocket scheme (`ws`/`wss`), so
/// a mistyped `http://`/`https://` endpoint fails here with a clear error
/// instead of a confusing transport failure from the subxt client.
fn validate_ws_url(ws_url: &str) -> Result<(), BlockchainError> {
    let url = url::Url::parse(ws_url)
        .map_err(|e| BlockchainError::InvalidUrl(format!("invalid WebSocket URL: {e}")))?;
    match url.scheme() {
        "ws" | "wss" => Ok(()),
        other => Err(BlockchainError::InvalidUrl(format!(
            "invalid WebSocket URL scheme '{other}' (expected ws or wss)"
        ))),
    }
}

/// Open the raw JSON-RPC WebSocket connection used for `system_*` / `chain_*`
/// calls. `RpcClient` is cheaply cloneable (it wraps an `mpsc` sender into a
/// reconnecting client), so callers that also need an [`SubxtClient`] hand a
/// clone to `OnlineClient::from_rpc_client` and share this one connection
/// instead of opening a second socket.
async fn connect_rpc(ws_url: &str) -> Result<SubxtRpcClient, BlockchainError> {
    validate_ws_url(ws_url)?;
    tracing::debug!(ws_url = %ws_url, "connecting to Substrate RPC endpoint");
    SubxtRpcClient::from_insecure_url(ws_url)
        .await
        .map_err(subxt_err)
}

/// Connect the metadata-aware online client. When the caller also needs raw
/// RPC access it should call [`connect_rpc`] itself and clone the handle into
/// `OnlineClient::from_rpc_client` — this helper is for calls that only need
/// the online client (balance/storage queries).
async fn connect_client(ws_url: &str) -> Result<SubxtClient, BlockchainError> {
    let rpc = connect_rpc(ws_url).await?;
    SubxtClient::from_rpc_client(rpc).await.map_err(subxt_err)
}

/// Decode a model-supplied storage key hex string (0x prefix optional, both
/// `0x` and `0X` accepted) into the raw key bytes the dynamic storage API
/// expects. Extracted from [`subxt_query_impl`] so the tolerant-prefix
/// behavior is unit-testable against the production code path.
fn decode_storage_key_hex(key_hex: &str) -> Result<Vec<u8>, BlockchainError> {
    hex::decode(strip_hex_prefix(key_hex))
        .map_err(|e| BlockchainError::Other(format!("invalid hex key: {e}")))
}

// ── Async implementations ────────────────────────────────────────────────

async fn subxt_chain_impl(ws_url: &str) -> Result<String, BlockchainError> {
    // One WebSocket connection, two handles: the raw `RpcClient` drives the
    // `system_*` calls and the `OnlineClient` (built from a clone) drives the
    // metadata-aware queries — both share the underlying socket.
    let rpc = connect_rpc(ws_url).await?;
    let client = SubxtClient::from_rpc_client(rpc.clone())
        .await
        .map_err(subxt_err)?;

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

    // chain/name/version/chain_type are scalar node strings (a hostile value
    // must not be able to inject a line) — per-value sanitize. The
    // properties/health blobs are serde-rendered JSON: sanitize_json keeps
    // their (structural) line breaks and bounds the pass to the byte budget.
    let mut out = String::new();
    out.push_str(&format!("chain: {}\n", sanitize_value(&chain)));
    out.push_str(&format!("chain_type: {}\n", sanitize_value(&chain_type)));
    out.push_str(&format!("node_name: {}\n", sanitize_value(&name)));
    out.push_str(&format!("node_version: {}\n", sanitize_value(&version)));
    out.push_str(&format!("genesis_hash: {genesis_hash:#x}\n"));
    out.push_str(&format!("best_block: #{best_number} ({best_hash:#x})\n"));
    out.push_str(&format!("finalized_head: {finalized_hash}\n"));
    out.push_str(&format!(
        "properties: {}\n",
        sanitize_json(&props.to_string(), MAX_TOOL_OUTPUT_BYTES)
    ));
    out.push_str(&format!(
        "health: {}",
        sanitize_json(&health.to_string(), MAX_TOOL_OUTPUT_BYTES)
    ));
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

/// Render the decoded `System.Account` storage value as readable text. The
/// JSON is node-decoded data — run through [`sanitize_json`] so hostile
/// storage values cannot corrupt the transcript while the pretty-printed
/// structure stays readable.
fn format_balance_value(value: &subxt::dynamic::Value, address: &str) -> String {
    let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("{value:?}"));
    format!(
        "address: {address}\naccount_info: {}",
        sanitize_json(&json, MAX_TOOL_OUTPUT_BYTES)
    )
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
            // Tolerate an optional `0x`/`0X` prefix even though the docs say
            // raw hex — the model will sometimes supply one, and a decode
            // error on a redundant prefix is pure friction.
            let bytes = decode_storage_key_hex(hex)?;
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
            // Pretty-printed structure is preserved; each line is sanitized
            // (see [`crate::sanitize_json`]).
            Ok(sanitize_json(&json, MAX_TOOL_OUTPUT_BYTES))
        }
        None => Ok("storage value: None (no value found at this key)".to_string()),
    }
}

async fn subxt_block_impl(
    ws_url: &str,
    block_number: Option<u64>,
) -> Result<String, BlockchainError> {
    // One connection shared between the metadata-aware client and the raw
    // `chain_getBlock` call (see `subxt_chain_impl`).
    let rpc = connect_rpc(ws_url).await?;
    let client = SubxtClient::from_rpc_client(rpc.clone())
        .await
        .map_err(subxt_err)?;

    let at = match block_number {
        Some(num) => client.at_block(num).await.map_err(subxt_err)?,
        None => client.at_current_block().await.map_err(subxt_err)?,
    };

    let number = at.block_number();
    let hash = at.block_hash();
    let header = at.block_header().await.map_err(subxt_err)?;
    let spec_version = at.spec_version();

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
    // The full block JSON is node-supplied — run the rendered text through
    // the capped JSON sanitizer: bounded before the sanitize pass (a full
    // block dump can be megabytes) and keeps the model readable output.
    out.push_str(&format!(
        "full_block: {}",
        sanitize_json(&block_json.to_string(), MAX_TOOL_OUTPUT_BYTES)
    ));
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

    #[test]
    fn storage_key_hex_accepts_optional_0x_prefix() {
        // The production decode path (not a re-implementation) must accept raw
        // hex and both prefix spellings, and reject non-hex input.
        assert_eq!(
            decode_storage_key_hex("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            decode_storage_key_hex("0xdeadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            decode_storage_key_hex("0XDEADBEEF").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert!(decode_storage_key_hex("zz").is_err());
    }

    #[test]
    fn validate_ws_url_accepts_ws_wss_only() {
        // The scheme gate must accept both WebSocket schemes and reject
        // http/https/other with a clear scheme error.
        assert!(validate_ws_url("wss://rpc.polkadot.io").is_ok());
        assert!(validate_ws_url("ws://127.0.0.1:9944").is_ok());
        let err = validate_ws_url("https://rpc.polkadot.io").unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
        assert!(validate_ws_url("not a url").is_err());
    }

    #[test]
    fn format_balance_value_sanitizes_json() {
        // A decoded composite value renders as pretty JSON. serde escapes
        // control chars inside string values, so the literal \n in the output
        // are serde's own structural separators — the sanitizer keeps them —
        // while the Cf format char (bidi, not escaped by serde) is escaped.
        let value = subxt::dynamic::Value::named_composite(vec![
            ("nonce".to_string(), subxt::dynamic::Value::u128(0)),
            (
                "data".to_string(),
                subxt::dynamic::Value::string("evil\u{202e}name"),
            ),
        ]);
        let out = format_balance_value(&value, "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY");
        assert!(out.starts_with("address: 5Grwva"));
        assert!(out.contains("account_info:"));
        // Multi-line pretty JSON structure is preserved, not flattened to \n.
        assert!(
            out.contains('\n'),
            "pretty JSON must keep its line breaks: {out:?}"
        );
        // The Cf format char inside the string value is escaped by the
        // sanitizer (serde_json does NOT escape it).
        assert!(
            !out.contains('\u{202e}'),
            "bidi char must be escaped: {out:?}"
        );
        assert!(
            out.contains("\\u{202e}"),
            "escaped bidi char present: {out:?}"
        );
    }
}
