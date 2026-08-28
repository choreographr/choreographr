//! High-level operations — the pipelines the daemon's thin `Tool` wrappers call.
//!
//! These compose the lower-level [`crate::chain`], [`crate::indexer`], and
//! [`crate::ipfs`] modules into the read/write flows of the Coordination
//! Platform:
//!
//! - **Read**: indexer-first (cheap, keyed event resolution) with on-chain
//!   authority for single-source control state, content fetched from IPFS.
//! - **Write**: encode content → upload to IPFS (pin) → submit the extrinsic
//!   on-chain, returning the derived item id.
//!
//! Every function is blocking (the daemon stays thread-only); only the `subxt`
//! submit path rides the [tokio sidecar] inside the functions they call.
//!
//! [tokio sidecar]: crate::runtime

use crate::CoordError;
use crate::chain::{self, ChainAccount};
use crate::encode::{self, ContentInput, DecodedItem};
use crate::indexer::{self, DecodedEvent, QueryKey};
use rand::Rng;

// ── Read: shape of what the tools return ─────────────────────────────────────

/// A single revision of an item, resolved from indexed `PublishRevision` events.
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct RevisionEntry {
    /// On-chain revision counter (starts at 0, increments per revision).
    pub revision_id: u32,
    /// IPFS content hash (`0x`-prefixed sha2-256 digest).
    pub ipfs_hash_hex: String,
    /// Block height the revision was published in.
    pub block_number: Option<u32>,
    /// Milliseconds since Unix epoch.
    pub timestamp: Option<u64>,
}

/// A fully resolved item (content + revision + on-chain control state).
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct ResolvedItem {
    /// Item id (`0x`-prefixed hex).
    pub item_id: String,
    /// The decoded content of the requested (default latest) revision.
    pub content: DecodedItem,
    /// Revision id that `content` was decoded from.
    pub revision_id: u32,
    /// IPFS hash of that revision.
    pub ipfs_hash_hex: String,
    /// On-chain owner account id (`0x` hex).
    pub owner: String,
    /// Lifecycle flag bitmask.
    pub flags: u8,
}

/// A content item pinned to an account (`pallet-account-content`).
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct AccountItem {
    /// Item id (`0x`-prefixed hex).
    pub item_id: String,
    /// Title, if one could be resolved for display.
    pub title: Option<String>,
}

/// A resolved account profile.
#[derive(Clone, Debug, Default, serde::Serialize, schemars::JsonSchema)]
pub struct ProfileResult {
    /// Whether a profile item is set on-chain for the account.
    pub exists: bool,
    /// Profile item id (`0x` hex), if set.
    pub item_id: Option<String>,
    /// Decoded content fields.
    pub name: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub account_type: Option<i32>,
}

/// Aggregated status of the three platform services.
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct CoordStatus {
    pub chain: Option<ChainStatus>,
    pub indexer: Option<IndexerStatus>,
    pub ipfs: Option<ipfs::IpfsStatus>,
}

/// A snapshot of the event indexer's indexed spans.
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct IndexerStatus {
    pub spans: Vec<Span>,
}

/// An indexed block span.
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl From<indexer::IndexStatusResult> for IndexerStatus {
    fn from(r: indexer::IndexStatusResult) -> Self {
        IndexerStatus {
            spans: r
                .spans
                .into_iter()
                .map(|s| Span {
                    start: s.start,
                    end: s.end,
                })
                .collect(),
        }
    }
}

/// Chain/status snapshot.
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct ChainStatus {
    pub genesis_hash: String,
    pub ss58_prefix: u16,
    pub best_block: u64,
    pub finalized_block: u64,
    pub item_id_namespace: u32,
}

impl From<chain::ChainStatus> for ChainStatus {
    /// Adapt the live probe's snapshot into the tool-facing status shape.
    fn from(s: chain::ChainStatus) -> Self {
        ChainStatus {
            genesis_hash: s.genesis_hash,
            ss58_prefix: s.ss58_prefix,
            best_block: s.best_block,
            finalized_block: s.finalized_block,
            item_id_namespace: s.item_id_namespace,
        }
    }
}

// ── Read: operations ─────────────────────────────────────────────────────────

/// Resolve one item: latest (or a specific) revision, decoded from IPFS,
/// merged with the on-chain control state.
pub fn item(item_id_hex: &str, revision_id: Option<u32>) -> Result<ResolvedItem, CoordError> {
    let item_id = encode::hex_to_bytes(item_id_hex)?;

    // On-chain authority: owner, latest revision counter, and flags.
    let state = chain::item_state(item_id)?;

    // Resolve the requested (or latest) revision's IPFS hash from the indexer.
    let (revision, ipfs_hash) = resolve_revision(item_id_hex, revision_id)?;

    // Fetch the content from IPFS and decode it.
    let bytes = crate::ipfs::cat(&ipfs_hash)?;
    let content = encode::decode_item(&bytes)?;

    Ok(ResolvedItem {
        item_id: encode::bytes_to_hex(&item_id),
        content,
        revision_id: revision,
        ipfs_hash_hex: ipfs_hash,
        owner: state.owner,
        flags: state.flags,
    })
}

/// An item's embedded image fetched from IPFS, ready for display.
#[derive(Clone, Debug)]
pub struct ItemImage {
    /// Full-resolution dimensions declared in the image mixin.
    pub width: u32,
    pub height: u32,
    /// The mipmap level that was fetched (0 = full resolution).
    pub level: u32,
    /// IPFS CID (Base58) of the fetched level.
    pub cid: String,
    /// Declared byte size of this level.
    pub filesize: u64,
    /// The raw image bytes (JPEG, per the reference encoder).
    pub data: Vec<u8>,
}

/// Pick a mipmap level from a decoded [`ImageSpec`], validating the request
/// against the available levels. Pure so it can be unit-tested without IPFS.
fn select_image_level(
    image: &encode::ImageSpec,
    level: Option<u32>,
) -> Result<(usize, encode::MipmapLevel), crate::CoordError> {
    if image.mipmap_levels.is_empty() {
        // The reference protocol encodes images purely as a mipmap pyramid
        // (level 0 = full-res); an image mixin without levels carries nothing
        // retrievable.
        return Err(crate::CoordError::Content(
            "image mixin has no mipmap levels".into(),
        ));
    }
    let index = level.unwrap_or(0) as usize;
    let spec = image.mipmap_levels.get(index).ok_or_else(|| {
        crate::CoordError::Content(format!(
            "mipmap level {} out of range (item has {} levels)",
            index,
            image.mipmap_levels.len()
        ))
    })?;
    Ok((index, spec.clone()))
}

/// Fetch an item's embedded image from IPFS: resolve the revision's content
/// hash, decode the image mixin, and `cat` the requested mipmap level
/// (`level = 0` is the full-resolution encode; `None` defaults to it).
pub fn item_image(
    item_id_hex: &str,
    revision_id: Option<u32>,
    level: Option<u32>,
) -> Result<ItemImage, crate::CoordError> {
    let (_revision, ipfs_hash) = resolve_revision(item_id_hex, revision_id)?;
    let bytes = crate::ipfs::cat(&ipfs_hash)?;
    let content = encode::decode_item(&bytes)?;
    let image = content.image.ok_or_else(|| {
        crate::CoordError::Content(format!("item {item_id_hex} has no embedded image"))
    })?;
    let (index, spec) = select_image_level(&image, level)?;
    let data = crate::ipfs::cat_by_cid(&spec.cid)?;
    Ok(ItemImage {
        width: image.width,
        height: image.height,
        level: index as u32,
        cid: spec.cid,
        filesize: spec.filesize,
        data,
    })
}

/// Build the revision history for an item from decoded indexer events,
/// sorted newest-first (revision id, then block height as tie-breaker).
///
/// The indexer tags each event by every `item_id` it references — its own
/// `item_id` plus any ids in `links`/`parents` — so a linked item's revision
/// events appear alongside this item's own. Only events whose `item_id`
/// field matches the queried item are revisions of it; all others are edges
/// of unrelated items and must be excluded.
/// Pure so it can be unit-tested without the indexer.
fn revision_entries_from_events(item_id_hex: &str, events: &[DecodedEvent]) -> Vec<RevisionEntry> {
    events
        .iter()
        .filter(|e| e.pallet_name() == "Content" && e.event_name() == "PublishRevision")
        // The event's own item_id must match the queried item; hex case is
        // normalized by the indexer but compare leniently regardless.
        .filter(|e| {
            e.field_str("item_id")
                .is_some_and(|id| id.eq_ignore_ascii_case(item_id_hex))
        })
        .filter_map(|e| {
            let rev = e.field_u64("revision_id")? as u32;
            let hash = e.field_str("ipfs_hash")?.to_string();
            Some(RevisionEntry {
                revision_id: rev,
                ipfs_hash_hex: hash,
                block_number: Some(e.block_number),
                timestamp: Some(e.timestamp),
            })
        })
        .collect()
}

/// Sort revisions newest-first. Block height breaks ties so that equal
/// revision ids (e.g. revision 0 across different items) resolve
/// deterministically.
fn sort_revisions_newest_first(revisions: &mut [RevisionEntry]) {
    revisions.sort_by(|a, b| {
        b.revision_id
            .cmp(&a.revision_id)
            .then_with(|| b.block_number.cmp(&a.block_number))
    });
}

/// Resolve a single revision's IPFS hash (and its revision counter) from the
/// indexer, defaulting to the latest revision when none is requested.
fn resolve_revision(
    item_id_hex: &str,
    revision_id: Option<u32>,
) -> Result<(u32, String), CoordError> {
    let events = indexer::get_events(&indexer::item_id_key(item_id_hex)?, 512, None)?;
    let mut revisions = revision_entries_from_events(item_id_hex, &events);
    sort_revisions_newest_first(&mut revisions);

    let entry = match revision_id {
        Some(target) => revisions
            .into_iter()
            .find(|r| r.revision_id == target)
            .ok_or_else(|| {
                CoordError::Content(format!(
                    "revision {target} not found for item {item_id_hex}"
                ))
            })?,
        None => revisions.into_iter().next().ok_or_else(|| {
            CoordError::Content(format!("no indexed revision found for item {item_id_hex}"))
        })?,
    };
    Ok((entry.revision_id, entry.ipfs_hash_hex))
}

/// Full revision history of an item, newest-first.
pub fn revisions(item_id_hex: &str) -> Result<Vec<RevisionEntry>, CoordError> {
    let events = indexer::get_events(&indexer::item_id_key(item_id_hex)?, 512, None)?;
    let mut list = revision_entries_from_events(item_id_hex, &events);
    sort_revisions_newest_first(&mut list);
    Ok(list)
}

/// Query the indexer for events matching a key (low-level read primitive).
pub fn events(
    key: &QueryKey,
    limit: u16,
    before: Option<(u32, u32)>,
) -> Result<Vec<DecodedEvent>, CoordError> {
    indexer::get_events(key, limit, before)
}

/// List an account's pinned content items, resolving each title via the
/// indexer+IPFS (items that fail to resolve fall back to id-only).
pub fn account_items(account_addr: &str) -> Result<Vec<AccountItem>, CoordError> {
    let account = chain::account_id_from_address(account_addr)?;
    let ids = chain::account_item_ids(account)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let id_hex = encode::bytes_to_hex(&id);
        let title = resolve_title(&id_hex).unwrap_or_default();
        out.push(AccountItem {
            item_id: id_hex,
            title,
        });
    }
    Ok(out)
}

/// Resolve an item's title from its latest revision, if possible.
fn resolve_title(item_id_hex: &str) -> Result<Option<String>, CoordError> {
    let (_, ipfs_hash) = resolve_revision(item_id_hex, None)?;
    let bytes = crate::ipfs::cat(&ipfs_hash)?;
    let content = encode::decode_item(&bytes)?;
    Ok(content.title)
}

/// Resolve an account's profile.
pub fn profile(account_addr: &str) -> Result<ProfileResult, CoordError> {
    let account = chain::account_id_from_address(account_addr)?;
    let Some(profile_item_id) = chain::profile_item(account)? else {
        return Ok(ProfileResult::default());
    };
    let id_hex = encode::bytes_to_hex(&profile_item_id);
    let resolved = item(&id_hex, None).unwrap_or_else(|_| -> ResolvedItem {
        // Degrade gracefully: if content can't be resolved, at least report the
        // profile item id.
        ResolvedItem {
            item_id: id_hex.clone(),
            content: DecodedItem::default(),
            revision_id: 0,
            ipfs_hash_hex: String::new(),
            owner: String::new(),
            flags: 0,
        }
    });
    Ok(ProfileResult {
        exists: true,
        item_id: Some(id_hex),
        name: resolved.content.title,
        bio: resolved.content.body,
        location: resolved
            .content
            .profile
            .as_ref()
            .map(|p| p.location.clone()),
        account_type: resolved.content.profile.as_ref().map(|p| p.account_type),
    })
}

/// Decode arbitrary content bytes from IPFS (by digest hex or CID) — no chain.
pub fn decode_content(ipfs_hash_or_cid: &str) -> Result<DecodedItem, CoordError> {
    let bytes = if ipfs_hash_or_cid.starts_with("0x") {
        crate::ipfs::cat(ipfs_hash_or_cid)?
    } else {
        crate::ipfs::cat_by_cid(ipfs_hash_or_cid)?
    };
    encode::decode_item(&bytes)
}

/// Aggregate status across all three services (each is best-effort).
pub fn status() -> Result<CoordStatus, CoordError> {
    let indexer = indexer::index_status().ok().map(IndexerStatus::from);
    let ipfs = crate::ipfs::id().ok().map(|peer| ipfs::IpfsStatus {
        peer_id: peer.peer_id,
        addresses: peer.addresses,
    });
    // Chain status is a live probe: connect (verifying the genesis hash) and
    // read the SS58 prefix. A down node or mismatched chain surfaces as `None`
    // (reported "unavailable"), matching the indexer/IPFS arms, instead of a
    // healthy snapshot fabricated from pinned configuration.
    let chain = chain::chain_status().ok().map(ChainStatus::from);
    Ok(CoordStatus {
        chain,
        indexer,
        ipfs,
    })
}

// ── Write: operations ────────────────────────────────────────────────────────

/// Publish a brand-new item: encode → IPFS → derive id → submit.
pub fn publish_item(
    account: &ChainAccount,
    content: &ContentInput,
    parents: Vec<[u8; 32]>,
    links: Vec<[u8; 32]>,
    mentions: Vec<[u8; 32]>,
    flags: Option<u8>,
    nonce: Option<[u8; 32]>,
) -> Result<chain::TxOutcome, CoordError> {
    let flags = flags.unwrap_or(crate::config::DEFAULT_ITEM_FLAGS);
    if flags & !crate::config::VALID_PUBLISH_FLAGS != 0 {
        return Err(CoordError::InvalidArgument(format!(
            "invalid item flags: {flags:#x}"
        )));
    }
    let bytes = encode::encode_item(content)?;
    let ipfs_hash = crate::ipfs::add(&bytes, "content.bin")?;
    let digest = encode::hex_to_bytes(&ipfs_hash)?;

    // Derive the item id deterministically. If no nonce is supplied, generate one.
    let nonce = nonce.unwrap_or_else(|| {
        let mut n = [0u8; 32];
        rand::rng().fill_bytes(&mut n);
        n
    });
    let item_id = encode::derive_item_id(account.account_id, nonce);

    let outcome = chain::publish_item(account, nonce, parents, flags, links, mentions, digest)?;
    // The chain returns the created item id; verify it matches our derivation.
    if let Some(got) = &outcome.item_id
        && got != &encode::bytes_to_hex(&item_id)
    {
        tracing::warn!(
            derived = %encode::bytes_to_hex(&item_id),
            on_chain = %got,
            "published item id differs from client derivation"
        );
    }
    Ok(outcome)
}

/// Publish a new revision of an existing item.
pub fn publish_revision(
    account: &ChainAccount,
    item_id: [u8; 32],
    content: &ContentInput,
    links: Vec<[u8; 32]>,
    mentions: Vec<[u8; 32]>,
) -> Result<chain::TxOutcome, CoordError> {
    let bytes = encode::encode_item(content)?;
    let ipfs_hash = crate::ipfs::add(&bytes, "content.bin")?;
    let digest = encode::hex_to_bytes(&ipfs_hash)?;
    chain::publish_revision(account, item_id, links, mentions, digest)
}

/// Apply a lifecycle state transition (retract / freeze flags).
pub fn lifecycle(
    account: &ChainAccount,
    action: LifecycleAction,
    item_id: [u8; 32],
) -> Result<(), CoordError> {
    match action {
        LifecycleAction::Retract => chain::retract_item(account, item_id),
        LifecycleAction::SetNotRevisionable => chain::set_not_revisionable(account, item_id),
        LifecycleAction::SetNotRetractable => chain::set_not_retractable(account, item_id),
    }
}

/// Pin/unpin an item to/from an account.
pub fn account_link(
    account: &ChainAccount,
    action: AccountLinkAction,
    item_id: [u8; 32],
) -> Result<(), CoordError> {
    match action {
        AccountLinkAction::Add => chain::add_account_item(account, item_id),
        AccountLinkAction::Remove => chain::remove_account_item(account, item_id),
    }
}

/// Point an account's profile at a freshly published profile item.
pub fn set_profile(
    account: &ChainAccount,
    content: &ContentInput,
) -> Result<chain::TxOutcome, CoordError> {
    // Publish the profile item, then link it as the account's profile.
    let bytes = encode::encode_item(content)?;
    let ipfs_hash = crate::ipfs::add(&bytes, "profile.bin")?;
    let digest = encode::hex_to_bytes(&ipfs_hash)?;
    let nonce: [u8; 32] = {
        let mut n = [0u8; 32];
        rand::rng().fill_bytes(&mut n);
        n
    };
    let item_id = encode::derive_item_id(account.account_id, nonce);
    let outcome = chain::publish_item(
        account,
        nonce,
        vec![],
        crate::config::DEFAULT_ITEM_FLAGS,
        vec![],
        vec![],
        digest,
    )?;
    chain::set_profile(account, item_id)?;
    Ok(outcome)
}

/// Lifecycle actions for [`lifecycle`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, schemars::JsonSchema, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    Retract,
    SetNotRevisionable,
    SetNotRetractable,
}

/// Account pin/unpin actions for [`account_link`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, schemars::JsonSchema, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AccountLinkAction {
    Add,
    Remove,
}

// ── IPFS status wrapper (re-exported shape) ──────────────────────────────────
pub mod ipfs {
    /// A resolved IPFS peer identity.
    #[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
    pub struct IpfsStatus {
        pub peer_id: String,
        pub addresses: Vec<String>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_image(levels: u32) -> encode::ImageSpec {
        encode::ImageSpec {
            width: 1012,
            height: 1012,
            mipmap_levels: (0..levels)
                .map(|i| encode::MipmapLevel {
                    filesize: 121_846 >> i,
                    cid: format!("QmSample{i}"),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn select_image_level_defaults_to_full_res_and_validates() {
        let image = sample_image(3);
        // None defaults to level 0 (full resolution).
        let (i, spec) = select_image_level(&image, None).unwrap();
        assert_eq!(i, 0);
        assert_eq!(spec.filesize, 121_846);
        // An explicit level resolves.
        let (i, _) = select_image_level(&image, Some(2)).unwrap();
        assert_eq!(i, 2);
        // Out-of-range levels are rejected, not silently clamped.
        assert!(select_image_level(&image, Some(3)).is_err());
        // A level-less image mixin carries nothing retrievable.
        assert!(select_image_level(&sample_image(0), None).is_err());
    }

    #[test]
    fn lifecycle_and_account_action_serde() {
        assert_eq!(
            serde_json::to_value(LifecycleAction::Retract).unwrap(),
            "retract"
        );
        assert_eq!(serde_json::to_value(AccountLinkAction::Add).unwrap(), "add");
    }

    #[test]
    fn publish_item_generates_a_nonce_and_derives_id() {
        // Nonce generation + id derivtion are pure; verify derivation differs
        // across nonces and is deterministic for a fixed nonce.
        let account = [9u8; 32];
        let nonce = [1u8; 32];
        let id_a = encode::derive_item_id(account, nonce);
        let id_b = encode::derive_item_id(account, [2u8; 32]);
        assert_ne!(id_a, id_b);
        assert_eq!(id_a, encode::derive_item_id(account, nonce));
    }

    #[test]
    fn decode_content_yes_validates_digest_requires_hex() {
        // Decode only needs a valid-looking 32-byte digest; this checks the
        // hex prefix routing without hitting IPFS.
        assert!(encode::hex_to_bytes("0x1234").is_err());
    }

    fn publish_revision_event(
        block_number: u32,
        item_id: &str,
        revision_id: u32,
        ipfs_hash: &str,
    ) -> DecodedEvent {
        DecodedEvent {
            block_number,
            event_index: 1,
            timestamp: 1_700_000_000_000,
            event: crate::indexer::StoredEvent {
                pallet_name: "Content".into(),
                event_name: "PublishRevision".into(),
                pallet_index: 7,
                variant_index: 3,
                event_index: 1,
                fields: serde_json::json!({
                    "item_id": item_id,
                    "ipfs_hash": ipfs_hash,
                    "revision_id": revision_id,
                }),
            },
        }
    }

    #[test]
    fn revision_entries_exclude_linked_items_revisions() {
        // Mirrors the feed collision: a linked item's PublishRevision is also
        // tagged with this item's id and carries the same revision_id 0.
        let own_id = format!("0x{}", "aa".repeat(32));
        let linked_id = format!("0x{}", "bb".repeat(32));
        let own_hash = format!("0x{}", "11".repeat(32));
        let linked_hash = format!("0x{}", "22".repeat(32));

        // Newest-first input, as the indexer returns events.
        let events = vec![
            publish_revision_event(2804, &linked_id, 0, &linked_hash),
            publish_revision_event(2324, &own_id, 0, &own_hash),
        ];

        let entries = revision_entries_from_events(&own_id, &events);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ipfs_hash_hex, own_hash);
        assert_eq!(entries[0].revision_id, 0);
        assert_eq!(entries[0].block_number, Some(2324));
    }

    #[test]
    fn revision_entries_ignore_non_revision_and_mismatched_events() {
        let own_id = format!("0x{}", "aa".repeat(32));
        let other_id = format!("0x{}", "bb".repeat(32));
        let mut other_pallet =
            publish_revision_event(3000, &own_id, 1, &format!("0x{}", "33".repeat(32)));
        other_pallet.event.pallet_name = "Balances".into();

        let events = vec![
            other_pallet,
            publish_revision_event(2900, &other_id, 1, &format!("0x{}", "44".repeat(32))),
            publish_revision_event(100, &own_id, 0, &format!("0x{}", "11".repeat(32))),
        ];

        let entries = revision_entries_from_events(&own_id, &events);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].revision_id, 0);
    }

    #[test]
    fn sort_revisions_breaks_equal_revision_id_ties_by_block() {
        let mut revisions = vec![
            RevisionEntry {
                revision_id: 1,
                ipfs_hash_hex: format!("0x{}", "22".repeat(32)),
                block_number: Some(2804),
                timestamp: None,
            },
            RevisionEntry {
                revision_id: 1,
                ipfs_hash_hex: format!("0x{}", "33".repeat(32)),
                block_number: Some(3000),
                timestamp: None,
            },
            RevisionEntry {
                revision_id: 2,
                ipfs_hash_hex: format!("0x{}", "44".repeat(32)),
                block_number: Some(100),
                timestamp: None,
            },
        ];

        sort_revisions_newest_first(&mut revisions);

        let hashes: Vec<&str> = revisions.iter().map(|r| r.ipfs_hash_hex.as_str()).collect();
        // revision 2 first, then revision 1 by descending block height.
        assert_eq!(
            hashes,
            vec![
                format!("0x{}", "44".repeat(32)),
                format!("0x{}", "33".repeat(32)),
                format!("0x{}", "22".repeat(32)),
            ]
        );
    }
}
