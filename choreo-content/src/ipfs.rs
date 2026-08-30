//! Blocking IPFS client — reads/writes content payloads on the local daemon.
//!
//! This uses `ureq` (synchronous HTTP) so it needs no async runtime, unlike the
//! subxt path which drives the tokio sidecar. Content is addressed by its
//! sha2-256 digest (the 32-byte `IpfsHash` stored on-chain); this module
//! converts between that digest hex and the IPFS CIDv0 that `add`/`cat` speak.
//!
//! - [`add`] uploads bytes (multipart `api/v0/add`) with `pin=true` and returns
//!   the `0x`-prefixed digest hex that goes into a `publish_*` extrinsic.
//! - [`cat`] fetches bytes by digest hex (converting to a CID first).

use serde::Deserialize;
use std::io::Read;
use std::time::Duration;

use crate::ContentError;
use crate::config::IPFS_API_URL;
use crate::encode::{cid_to_digest_hex, digest_hex_to_cid, hex_to_bytes};

/// A `ureq` agent configured with bounded connect + overall timeouts so a
/// hung IPFS daemon cannot block a daemon tool thread indefinitely.
fn agent() -> ureq::Agent {
    ureq::config::Config::builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// The `Hash` pair returned by one multipart `api/v0/add` line.
#[derive(Deserialize)]
struct IpfsAddEntry {
    #[serde(rename = "Hash")]
    hash: String,
}

/// A live `api/v0/id` snapshot (peer identity) used by `coord_status`.
#[derive(Deserialize)]
pub struct IpfsPeerInfo {
    #[serde(rename = "ID")]
    pub peer_id: String,
    #[serde(rename = "Addresses")]
    pub addresses: Vec<String>,
}

/// Upload `bytes` to IPFS (with `pin=true`) and return the content's sha2-256
/// digest hex (`0x`-prefixed) — the value that becomes an `IpfsHash` on-chain.
pub fn add(bytes: &[u8], filename: &str) -> Result<String, ContentError> {
    use ureq::unversioned::multipart::{Form, Part};

    let form = Form::new().part("file", Part::bytes(bytes).file_name(filename));
    let response = agent()
        .post(&format!("{IPFS_API_URL}/api/v0/add"))
        .query("pin", "true")
        .query("quieter", "true")
        .send(form)
        .map_err(|e| ContentError::Ipfs(format!("failed to upload payload: {e}")))?;

    let text = response
        .into_body()
        .read_to_string()
        .map_err(|e| ContentError::Ipfs(format!("failed to read add response: {e}")))?;
    // IPFS emits one JSON line per chunk; the root CID is the last non-empty.
    let last_line = text
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .ok_or_else(|| ContentError::Ipfs("add returned an empty response".into()))?;
    let entry: IpfsAddEntry = serde_json::from_str(last_line)
        .map_err(|e| ContentError::Ipfs(format!("failed to decode add response: {e}")))?;
    cid_to_digest_hex(&entry.hash)
}

/// Fetch the bytes stored at a digest hex (`0x`-prefixed), converting it to a
/// CID for `api/v0/cat`.
pub fn cat(digest_hex: &str) -> Result<Vec<u8>, ContentError> {
    let cid = digest_hex_to_cid(digest_hex)?;
    cat_by_cid(&cid)
}

/// Fetch the bytes stored at a raw IPFS CID (Base58).
pub fn cat_by_cid(cid: &str) -> Result<Vec<u8>, ContentError> {
    let response = agent()
        .post(&format!("{IPFS_API_URL}/api/v0/cat"))
        .query("arg", cid)
        .send_empty()
        .map_err(|e| ContentError::Ipfs(format!("failed to read {cid}: {e}")))?;
    let mut buf = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| ContentError::Ipfs(format!("failed to read ipfs bytes for {cid}: {e}")))?;
    Ok(buf)
}

/// Return the local daemon's peer identity (for `coord_status`).
pub fn id() -> Result<IpfsPeerInfo, ContentError> {
    let response = agent()
        .post(&format!("{IPFS_API_URL}/api/v0/id"))
        .send_empty()
        .map_err(|e| ContentError::Ipfs(format!("ipfs /id failed: {e}")))?;
    let bytes = response
        .into_body()
        .read_to_string()
        .map_err(|e| ContentError::Ipfs(format!("failed to read /id response: {e}")))?;
    let info: IpfsPeerInfo = serde_json::from_str(&bytes)
        .map_err(|e| ContentError::Ipfs(format!("failed to decode /id response: {e}")))?;
    Ok(info)
}

/// Convert a digest hex to the raw 32-byte digest (validates the length).
pub fn digest_bytes(digest_hex: &str) -> Result<[u8; 32], ContentError> {
    hex_to_bytes(digest_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `add`/`cat` need a live IPFS daemon; these are excluded from the default
    /// unit suite (integration tests in `tests/` exercise the real daemon — see
    /// the crate-level `#[ignore]` suite). Compile-only sanity here.
    #[test]
    fn digest_bytes_validates() {
        assert_eq!(
            digest_bytes(&format!("0x{}", "ab".repeat(32))).unwrap(),
            [0xab; 32]
        );
        assert!(digest_bytes("0x1234").is_err());
    }
}
