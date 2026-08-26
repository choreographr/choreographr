//! Hardcoded Coordination Platform endpoint + identity configuration.
//!
//! The Choreographr Coordination Platform is currently a single local
//! deployment, so these are compile-time constants (per the design decision to
//! "hardcode the endpoint + identity config" rather than read it from
//! configuration). The three backends match the reference implementation
//! (`acuity-dioxus`):
//!
//! - the Substrate node (`choreo-runtime`) at `ws://127.0.0.1:9944`
//! - the event indexer (`acuity-index`, TOML schema `~/acuity-index/acuity.toml`)
//!   at `ws://127.0.0.1:8172`
//! - the local IPFS daemon at `http://127.0.0.1:5001`
//!
//! `GENESIS_HASH`, `ITEM_ID_NAMESPACE`, and the item bounds are pinned to the
//! runtime configuration so item-id derivation and validation match the chain.

/// WebSocket endpoint of the Coordination Platform Substrate node.
pub const CHAIN_WS_URL: &str = "ws://127.0.0.1:9944";

/// WebSocket endpoint of the `acuity-index` JSON-RPC event indexer.
pub const INDEXER_WS_URL: &str = "ws://127.0.0.1:8172";

/// HTTP base URL of the local IPFS daemon (the `/api/v0/*` raft).
pub const IPFS_API_URL: &str = "http://127.0.0.1:5001";

/// Chain genesis hash (hex, `0x`-prefixed) from `acuity-index/acuity.toml`.
/// Used to sanity-check that the connected node is the Coordination Platform.
pub const GENESIS_HASH: &str =
    "0xd6be38e4049024b2a41d76e8d6dc3fa38173d85c74c4c2d3966669c082199219";

/// Item-id derivation namespace (`choreo-runtime`'s `ItemIdNamespace`), pinned
/// to `1000`. Item IDs are computed as
/// `blake2_256(account ++ nonce ++ namespace)`.
pub const ITEM_ID_NAMESPACE: u32 = 1000;

/// Upper bound on parent item references (`choreo-runtime`'s `MaxParents`).
pub const MAX_PARENTS: usize = 32;

/// Upper bound on linked item references (`choreo-runtime`'s `MaxLinks`).
pub const MAX_LINKS: usize = 128;

/// Upper bound on mentioned accounts (`choreo-runtime`'s `MaxMentions`).
pub const MAX_MENTIONS: usize = 256;

/// Item lifecycle flag bits, matching `pallet-content`. `publish_item` only
/// accepts `REVISIONABLE | RETRACTABLE`; reserved bits are rejected.
pub mod flags {
    /// New revisions are allowed.
    pub const REVISIONABLE: u8 = 1 << 0;
    /// The item can be marked retracted.
    pub const RETRACTABLE: u8 = 1 << 1;
    /// Set by `retract_item`.
    pub const RETRACTED: u8 = 1 << 2;
}

/// Valid flag mask accepted by `publish_item`.
pub const VALID_PUBLISH_FLAGS: u8 = flags::REVISIONABLE | flags::RETRACTABLE;

/// Default flags for a freshly published item: revisionable (so it can be
/// updated) and retractable (so it can be withdrawn), with the retracted bit
/// clear.
pub const DEFAULT_ITEM_FLAGS: u8 = flags::REVISIONABLE | flags::RETRACTABLE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_item_flags_are_within_valid_mask() {
        assert_eq!(DEFAULT_ITEM_FLAGS & VALID_PUBLISH_FLAGS, DEFAULT_ITEM_FLAGS);
        // The retracted bit is never set at publish.
        assert_eq!(DEFAULT_ITEM_FLAGS & flags::RETRACTED, 0);
    }

    #[test]
    fn flags_are_distinct_bits() {
        // The two publish-accepted bits are independent flags, not the same bit.
        assert_ne!(flags::REVISIONABLE, flags::RETRACTABLE);
        assert_ne!(flags::REVISIONABLE, flags::RETRACTED);
        assert_ne!(flags::RETRACTABLE, flags::RETRACTED);
        // Distinct flags do not overlap.
        assert_eq!(flags::REVISIONABLE & flags::RETRACTABLE, 0);
        assert_eq!(flags::REVISIONABLE & flags::RETRACTED, 0);
        assert_eq!(flags::RETRACTABLE & flags::RETRACTED, 0);
    }

    #[test]
    fn genesis_hash_is_hex_512_bits() {
        assert!(GENESIS_HASH.starts_with("0x"));
        assert_eq!(GENESIS_HASH.len(), 2 + 64);
    }

    #[test]
    fn item_bounds_are_positive() {
        assert!(MAX_PARENTS > 0);
        assert!(MAX_LINKS > 0);
        assert!(MAX_MENTIONS > 0);
        assert_eq!(ITEM_ID_NAMESPACE, 1000);
    }
}
