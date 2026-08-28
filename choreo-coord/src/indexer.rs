//! Synchronous client for the `acuity-index` WebSocket JSON-RPC API.
//!
//! This speaks the indexer's wire protocol directly (no `acuity-index-api-rs`
//! dependency) using `tungstenite` in synchronous mode over a plain `TcpStream`
//! — no async runtime, matching the crate's "only subxt uses the sidecar"
//! threading rule.
//!
//! Queries are keyed by the declared query keys from `acuity.toml`. The common
//! ones this crate builds — `item_id`, `account_id`, `ipfs_hash`, and the
//! composite `item_id_revision_id` — are exposed as [`QueryKey`] constructors.
//!
//! The wire envelope is JSON-RPC 2.0. The `key` object is
//! `{"type":"Custom","value":{"name":...,"kind":...,"value":...}}` where
//! `kind` matches the `CustomValue` tag (`bytes32`, `u32`, `composite`, ...).

use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tungstenite::Message;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;

use crate::CoordError;
use crate::config::INDEXER_WS_URL;
use crate::encode::{bytes_to_hex, hex_to_bytes};

/// A query key for the indexer's `acuity_getEvents`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryKey {
    /// Exact `item_id` (32 bytes) match.
    ItemId([u8; 32]),
    /// Exact `account_id` (32 bytes) match.
    AccountId([u8; 32]),
    /// Exact `ipfs_hash` (32 bytes) match.
    IpfsHash([u8; 32]),
    /// Composite `item_id` + `revision_id` match.
    ItemRevision { item_id: [u8; 32], revision_id: u32 },
    /// Raw custom key with an explicit name and kind/value (for the arbitrary
    /// declared keys, e.g. `index`-style keys not wrapped by this crate).
    Raw { name: String, value: Value },
}

impl QueryKey {
    /// Serialize into the wire `CustomKey` object (`name` + flattened value).
    fn to_custom_key(&self) -> Value {
        match self {
            QueryKey::ItemId(b) => custom_key("item_id", "bytes32", Value::from(bytes_to_hex(b))),
            QueryKey::AccountId(b) => {
                custom_key("account_id", "bytes32", Value::from(bytes_to_hex(b)))
            }
            QueryKey::IpfsHash(b) => {
                custom_key("ipfs_hash", "bytes32", Value::from(bytes_to_hex(b)))
            }
            QueryKey::ItemRevision {
                item_id,
                revision_id,
            } => custom_key(
                "item_id_revision_id",
                "composite",
                Value::Array(vec![
                    custom_scalar("bytes32", Value::from(bytes_to_hex(item_id))),
                    custom_scalar("u32", Value::from(*revision_id)),
                ]),
            ),
            QueryKey::Raw { name, value } => {
                // A raw key carries an already-shaped CustomValue object.
                let kind = value["kind"].clone();
                let inner = value["value"].clone();
                custom_key(name, kind.as_str().unwrap_or("bytes32"), inner)
            }
        }
    }
}

/// Build the wire `Key` object for `acuity_getEvents`/`acuity_subscribeEvents`.
/// The indexer's `Key` enum is tagged `#[serde(tag = "type", content = "value")]`,
/// so the `CustomKey` payload must be wrapped as `{"type":"Custom","value":…}`;
/// sending the bare `CustomKey` fails to deserialize with `invalid_key`.
fn wire_key(custom: Value) -> Value {
    serde_json::json!({ "type": "Custom", "value": custom })
}

/// Build the inner `CustomKey` object `{"name":..., "kind":..., "value":...}`.
fn custom_key(name: &str, kind: &str, value: Value) -> Value {
    serde_json::json!({ "name": name, "kind": kind, "value": value })
}

/// Build a nested `CustomValue` object (used inside a composite).
fn custom_scalar(kind: &str, value: Value) -> Value {
    serde_json::json!({ "kind": kind, "value": value })
}

/// A hydrated event returned by `acuity_getEvents`.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodedEvent {
    pub block_number: u32,
    pub event_index: u32,
    /// Milliseconds since Unix epoch (from the block's `Timestamp::Now`).
    pub timestamp: u64,
    /// The decoded event object (`pallet_name`, `event_name`, `fields`, ...).
    pub event: StoredEvent,
}

/// The decoded event shape. The indexer serializes this camelCase
/// (`palletName`, `eventName`, …) per its documented `acuity_getEvents`
/// response format, so we must mirror that here.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEvent {
    pub pallet_name: String,
    pub event_name: String,
    pub pallet_index: u8,
    pub variant_index: u8,
    pub event_index: u8,
    /// Free-form field map; string keys map event params to their values.
    pub fields: Value,
}

impl DecodedEvent {
    /// The pallet name of this event.
    pub fn pallet_name(&self) -> &str {
        &self.event.pallet_name
    }
    /// The event variant name.
    pub fn event_name(&self) -> &str {
        &self.event.event_name
    }
    /// Look up a field by name, returning the JSON value.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.event.fields.get(name)
    }
    /// Look up a string-valued field.
    pub fn field_str(&self, name: &str) -> Option<&str> {
        self.field(name).and_then(Value::as_str)
    }
    /// Look up a numeric field (accepts integer or numeric string; the indexer
    /// renders some scalars as strings).
    pub fn field_u64(&self, name: &str) -> Option<u64> {
        self.field(name)
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
    }
}

/// Paged result for `acuity_getEvents`.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetEventsResult {
    pub events: Vec<DecodedEvent>,
}

/// Status result for `acuity_indexStatus`.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct IndexStatusResult {
    pub spans: Vec<Span>,
}

/// An indexed span (block range).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

/// Opened indexer connection (synchronous WebSocket).
struct Connection {
    ws: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    next_id: u64,
}

impl Connection {
    fn open() -> Result<Self, CoordError> {
        let request = INDEXER_WS_URL
            .into_client_request()
            .map_err(|e| CoordError::Indexer(format!("invalid indexer url: {e}")))?;
        let (ws, _resp) = tungstenite::connect(request)
            .map_err(|e| CoordError::Indexer(format!("failed to connect to indexer: {e}")))?;
        // Bound socket reads/writes so a hung indexer cannot block a daemon tool
        // thread indefinitely. The indexer is reached over plain `ws://`, so
        // the underlying stream is always a plain TcpStream (not TLS).
        if let MaybeTlsStream::Plain(tcp) = ws.get_ref() {
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(15)));
            let _ = tcp.set_write_timeout(Some(Duration::from_secs(10)));
        }
        Ok(Self { ws, next_id: 1 })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, CoordError> {
        let id = self.next_id;
        self.next_id += 1;
        let req =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.ws
            .send(Message::Text(req.to_string().into()))
            .map_err(|e| CoordError::Indexer(format!("write failed: {e}")))?;
        loop {
            let msg = self
                .ws
                .read()
                .map_err(|e| CoordError::Indexer(format!("read failed: {e}")))?;
            match msg {
                Message::Text(text) => {
                    let v: Value = serde_json::from_str(&text)
                        .map_err(|e| CoordError::Indexer(format!("bad json: {e}")))?;
                    // Match only our request id (ignore unrelated notifications).
                    if v.get("id").and_then(Value::as_u64) == Some(id) {
                        return if v.get("error").is_some() {
                            Err(CoordError::Indexer(format!(
                                "indexer error for {method}: {v}"
                            )))
                        } else {
                            Ok(v.get("result").cloned().unwrap_or_default())
                        };
                    }
                }
                Message::Ping(data) => {
                    let _ = self.ws.send(Message::Pong(data));
                }
                Message::Close(_) => {
                    return Err(CoordError::Indexer("indexer closed connection".into()));
                }
                _ => {}
            }
        }
    }

    fn close(&mut self) {
        let _ = self.ws.close(None);
    }
}

/// Query the indexer for events matching `key`, newest-first, up to `limit`.
pub fn get_events(
    key: &QueryKey,
    limit: u16,
    before: Option<(u32, u32)>,
) -> Result<Vec<DecodedEvent>, CoordError> {
    let mut conn = Connection::open()?;
    let key_json = wire_key(key.to_custom_key());
    let params = serde_json::json!({
        "key": key_json,
        "limit": limit,
        "before": before.map(|(b, e)| serde_json::json!({ "blockNumber": b, "eventIndex": e })),
    });
    let result = conn.request("acuity_getEvents", params)?;
    conn.close();
    let parsed: GetEventsResult = serde_json::from_value(result)
        .map_err(|e| CoordError::Indexer(format!("failed to decode get_events result: {e}")))?;
    Ok(parsed.events)
}

/// Query the current indexer status (indexed spans).
pub fn index_status() -> Result<IndexStatusResult, CoordError> {
    let mut conn = Connection::open()?;
    let result = conn.request("acuity_indexStatus", serde_json::json!({}))?;
    conn.close();
    serde_json::from_value(result)
        .map_err(|e| CoordError::Indexer(format!("failed to decode index status: {e}")))
}

/// Convert a hex item id string (or `0x` hex) to a [`QueryKey::ItemId`].
pub fn item_id_key(item_id_hex: &str) -> Result<QueryKey, CoordError> {
    Ok(QueryKey::ItemId(hex_to_bytes(item_id_hex)?))
}

/// Build a composite [`QueryKey::ItemRevision`] from an item id + revision id.
pub fn item_revision_key(item_id_hex: &str, revision_id: u32) -> Result<QueryKey, CoordError> {
    Ok(QueryKey::ItemRevision {
        item_id: hex_to_bytes(item_id_hex)?,
        revision_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_key_wraps_custom_key() {
        // The get_events params must carry the tagged `Key` shape the indexer
        // deserializes, not the bare CustomKey.
        let item_id = [0x11u8; 32];
        let key = wire_key(QueryKey::ItemId(item_id).to_custom_key());
        assert_eq!(key["type"], "Custom");
        assert_eq!(key["value"]["name"], "item_id");
        assert_eq!(key["value"]["kind"], "bytes32");
        assert_eq!(key["value"]["value"], bytes_to_hex(&item_id));
    }

    #[test]
    fn query_key_wire_shapes() {
        // item_id -> {"name":"item_id","kind":"bytes32","value":"0x..."}
        let item_id = [0x11u8; 32];
        let key = QueryKey::ItemId(item_id).to_custom_key();
        assert_eq!(key["name"], "item_id");
        assert_eq!(key["kind"], "bytes32");
        assert_eq!(key["value"], bytes_to_hex(&item_id));

        // composite revision key
        let rev = QueryKey::ItemRevision {
            item_id,
            revision_id: 7,
        }
        .to_custom_key();
        assert_eq!(rev["name"], "item_id_revision_id");
        assert_eq!(rev["kind"], "composite");
        assert_eq!(rev["value"][0]["kind"], "bytes32");
        assert_eq!(rev["value"][1]["kind"], "u32");
        assert_eq!(rev["value"][1]["value"], 7);
    }

    #[test]
    fn decoded_event_field_helpers() {
        let ev = DecodedEvent {
            block_number: 1,
            event_index: 2,
            timestamp: 123,
            event: StoredEvent {
                pallet_name: "Content".into(),
                event_name: "PublishRevision".into(),
                pallet_index: 7,
                variant_index: 3,
                event_index: 2,
                fields: serde_json::json!({
                    "ipfs_hash": "0xaa",
                    "revision_id": "7",
                    "n": 42,
                }),
            },
        };
        assert_eq!(ev.pallet_name(), "Content");
        assert_eq!(ev.event_name(), "PublishRevision");
        assert_eq!(ev.field_str("ipfs_hash"), Some("0xaa"));
        // revision_id arrives as a numeric string.
        assert_eq!(ev.field_u64("revision_id"), Some(7));
        assert_eq!(ev.field_u64("n"), Some(42));
        assert_eq!(ev.field_u64("missing"), None);
    }
}
