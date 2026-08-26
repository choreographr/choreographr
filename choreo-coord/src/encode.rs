//! Content protocol: protobuf `ItemMessage` encoding/decoding, mixin
//! construction, deterministic item-id derivation, and CID <-> sha2-256-digest
//! conversion.
//!
//! On-chain content is a protobuf `ItemMessage`: an ordered list of
//! `MixinPayloadMessage { mixin_id, payload }`, each tagged with a 32-bit mixin
//! ID. The semantic *type* of an item is implied by which mixins are present
//! (a `feed`/`comment` type marker, a `profile` mixin, etc.), so the same
//! encoding serves a decentralized GitHub, a decentralized Wiki, or arbitrary
//! content — only the mixin set differs.
//!
//! The 32-byte `IpfsHash` stored on-chain is the sha2-256 digest of an item's
//! bytes, extracted from an IPFS CIDv0 multihash (`0x12 0x20 || digest`). This
//! module converts between the CID (Base58, as IPFS returns) and the digest hex
//! (`0x`-prefixed) that is what actually reaches the chain events.

use prost::Message;
use sp_crypto_hashing::blake2_256;

use crate::config::ITEM_ID_NAMESPACE;

// ── Mixin ID constants (pinned to the reference `acuity-dioxus` protocol) ────

/// BCP-47 language tag.
pub const LANGUAGE_MIXIN_ID: u32 = 0x9bc7_a0e6;
/// Human-readable title.
pub const TITLE_MIXIN_ID: u32 = 0x344f_4812;
/// Body text (markdown/plain).
pub const BODY_TEXT_MIXIN_ID: u32 = 0x2d38_2044;
/// Image (full-res + mipmap levels).
pub const IMAGE_MIXIN_ID: u32 = 0x045e_ee8c;
/// Profile (account type + location), used alongside title/body/language.
pub const PROFILE_MIXIN_ID: u32 = 0xbeef_2144;
/// Feed-type marker (empty payload).
pub const FEED_TYPE_MIXIN_ID: u32 = 0xbcec_8faa;
/// Comment-type marker (empty payload).
pub const COMMENT_TYPE_MIXIN_ID: u32 = 0x874a_ba65;

/// Default BCP-47 language tag applied when the caller omits one.
pub const DEFAULT_LANGUAGE_TAG: &str = "en";

// ── Protobuf message types ──────────────────────────────────────────────────

/// The top-level content envelope: an ordered list of mixins.
#[derive(Clone, PartialEq, Message)]
pub struct ItemMessage {
    #[prost(message, repeated, tag = "1")]
    pub mixin_payload: Vec<MixinPayloadMessage>,
}

/// A single tagged mixin: a 32-bit type discriminator plus its payload.
#[derive(Clone, PartialEq, Message)]
pub struct MixinPayloadMessage {
    #[prost(fixed32, tag = "1")]
    pub mixin_id: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub payload: Vec<u8>,
}

/// BCP-47 language tag.
#[derive(Clone, PartialEq, Message)]
pub struct LanguageMixinMessage {
    #[prost(string, tag = "1")]
    pub language_tag: String,
}

/// Human-readable title.
#[derive(Clone, PartialEq, Message)]
pub struct TitleMixinMessage {
    #[prost(string, tag = "1")]
    pub title: String,
}

/// Body text.
#[derive(Clone, PartialEq, Message)]
pub struct BodyTextMixinMessage {
    #[prost(string, tag = "1")]
    pub body_text: String,
}

/// An image reference plus its mipmap pyramid (each level a separate IPFS CID).
#[derive(Clone, PartialEq, Message)]
pub struct ImageMixinMessage {
    #[prost(string, tag = "1")]
    pub filename: String,
    #[prost(uint64, tag = "2")]
    pub filesize: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub ipfs_hash: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub width: u32,
    #[prost(uint32, tag = "5")]
    pub height: u32,
    #[prost(message, repeated, tag = "6")]
    pub mipmap_level: Vec<MipmapLevelMessage>,
}

/// One mipmap level of an [`ImageMixinMessage`].
#[derive(Clone, PartialEq, Message)]
pub struct MipmapLevelMessage {
    #[prost(uint64, tag = "1")]
    pub filesize: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub ipfs_hash: Vec<u8>,
}

/// Account profile mixin: signed account type + free-text location.
#[derive(Clone, PartialEq, Message)]
pub struct ProfileMixinMessage {
    #[prost(int32, tag = "1")]
    pub account_type: i32,
    #[prost(string, tag = "2")]
    pub location: String,
}

/// The account-type taxonomy encoded in a [`ProfileMixinMessage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum AccountType {
    Anon = 0,
    Person = 1,
    Project = 2,
    Organization = 3,
    Proxy = 4,
    Parody = 5,
    Bot = 6,
    Shill = 7,
    Test = 8,
}

// ── Content type and structured item I/O ────────────────────────────────────

/// Fixed, protocol-level content-type vocabulary. The type is *encoded* by
/// which mixins an item carries; this enum is the closure over the known
/// marker/profile mixins. `Document` is the default (title + body + language).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    schemars::JsonSchema,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    /// Title + body + language — the default item shape.
    #[default]
    Document,
    Feed,
    Comment,
    Profile,
    Image,
}

/// A mipmap level of an image, as uploaded to IPFS (a pre-encoded CID per
/// level).
#[derive(
    Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema, serde::Serialize, serde::Deserialize,
)]
pub struct MipmapLevel {
    /// Size of the level's raw bytes.
    pub filesize: u64,
    /// IPFS CID (Base58) of this level.
    pub cid: String,
}

/// Structured image reference carried in an item.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema, serde::Serialize, serde::Deserialize,
)]
pub struct ImageSpec {
    pub filename: String,
    pub filesize: u64,
    /// sha2-256 digest hex (`0x`-prefixed) of the full-resolution image.
    pub digest_hex: String,
    pub width: u32,
    pub height: u32,
    pub mipmap_levels: Vec<MipmapLevel>,
}

/// Structured profile fields.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema, serde::Serialize, serde::Deserialize,
)]
pub struct ProfileSpec {
    /// Account type (0..=8, see [`AccountType`]).
    pub account_type: i32,
    pub location: String,
}

/// Input to [`encode_item`]: everything a caller wants in a new item revision.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema, serde::Serialize, serde::Deserialize,
)]
pub struct ContentInput {
    pub content_type: ContentType,
    pub title: Option<String>,
    pub body: Option<String>,
    /// BCP-47 language tag; defaults to [`DEFAULT_LANGUAGE_TAG`].
    pub language: Option<String>,
    pub image: Option<ImageSpec>,
    pub profile: Option<ProfileSpec>,
}

/// The decoded, field-extracted view of an item (what a read tool returns).
#[derive(
    Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema, serde::Serialize, serde::Deserialize,
)]
pub struct DecodedItem {
    /// The type inferred from the present marker/profile mixins.
    pub content_type: ContentType,
    pub title: Option<String>,
    pub body: Option<String>,
    pub language: Option<String>,
    pub image: Option<ImageSpec>,
    pub profile: Option<ProfileSpec>,
}

/// Encode a [`ContentInput`] into the protobuf `ItemMessage` bytes, tagging the
/// right mixin set for the chosen [`ContentType`].
///
/// Returns an error when an embedded image's digest hex or mipmap CID is
/// malformed (see [`encode_image_mixin`]); a well-formed item always succeeds.
pub fn encode_item(input: &ContentInput) -> Result<Vec<u8>, crate::CoordError> {
    let language = input
        .language
        .clone()
        .unwrap_or_else(|| DEFAULT_LANGUAGE_TAG.to_string());

    let mut mixins: Vec<MixinPayloadMessage> = Vec::new();

    // Type markers come first (empty payloads for feed/comment).
    match input.content_type {
        ContentType::Feed => mixins.push(marker(FEED_TYPE_MIXIN_ID)),
        ContentType::Comment => mixins.push(marker(COMMENT_TYPE_MIXIN_ID)),
        _ => {}
    }

    // Ordinal content mixins.
    mixins.push(MixinPayloadMessage {
        mixin_id: LANGUAGE_MIXIN_ID,
        payload: LanguageMixinMessage {
            language_tag: language,
        }
        .encode_to_vec(),
    });

    if let Some(title) = &input.title {
        mixins.push(MixinPayloadMessage {
            mixin_id: TITLE_MIXIN_ID,
            payload: TitleMixinMessage {
                title: title.clone(),
            }
            .encode_to_vec(),
        });
    }
    if let Some(body) = &input.body {
        mixins.push(MixinPayloadMessage {
            mixin_id: BODY_TEXT_MIXIN_ID,
            payload: BodyTextMixinMessage {
                body_text: body.clone(),
            }
            .encode_to_vec(),
        });
    }
    if let Some(image) = &input.image {
        mixins.push(MixinPayloadMessage {
            mixin_id: IMAGE_MIXIN_ID,
            payload: encode_image_mixin(image)?,
        });
    }
    if let Some(profile) = &input.profile {
        mixins.push(MixinPayloadMessage {
            mixin_id: PROFILE_MIXIN_ID,
            payload: ProfileMixinMessage {
                account_type: profile.account_type,
                location: profile.location.clone(),
            }
            .encode_to_vec(),
        });
    }

    Ok(ItemMessage {
        mixin_payload: mixins,
    }
    .encode_to_vec())
}

/// Decode `ItemMessage` bytes into an extracted [`DecodedItem`]. Unknown mixins
/// are ignored (forward compatibility); a missing/ill-formed mixin degrades
/// that field to `None` rather than failing the whole decode.
pub fn decode_item(bytes: &[u8]) -> Result<DecodedItem, crate::CoordError> {
    let item = ItemMessage::decode(bytes)
        .map_err(|e| crate::CoordError::Content(format!("failed to decode item payload: {e}")))?;

    let mut out = DecodedItem {
        content_type: infer_content_type(&item),
        ..Default::default()
    };

    for mixin in &item.mixin_payload {
        match mixin.mixin_id {
            LANGUAGE_MIXIN_ID => {
                out.language = LanguageMixinMessage::decode(mixin.payload.as_slice())
                    .ok()
                    .map(|m| m.language_tag);
            }
            TITLE_MIXIN_ID => {
                out.title = TitleMixinMessage::decode(mixin.payload.as_slice())
                    .ok()
                    .map(|m| m.title);
            }
            BODY_TEXT_MIXIN_ID => {
                out.body = BodyTextMixinMessage::decode(mixin.payload.as_slice())
                    .ok()
                    .map(|m| m.body_text);
            }
            IMAGE_MIXIN_ID => {
                out.image = ImageMixinMessage::decode(mixin.payload.as_slice())
                    .ok()
                    .map(decode_image_mixin);
            }
            PROFILE_MIXIN_ID => {
                out.profile = ProfileMixinMessage::decode(mixin.payload.as_slice())
                    .ok()
                    .map(|m| ProfileSpec {
                        account_type: m.account_type,
                        location: m.location,
                    });
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Infer the [`ContentType`] of a decoded item from its marker/profile mixins.
pub fn infer_content_type(item: &ItemMessage) -> ContentType {
    for mixin in &item.mixin_payload {
        match mixin.mixin_id {
            FEED_TYPE_MIXIN_ID => return ContentType::Feed,
            COMMENT_TYPE_MIXIN_ID => return ContentType::Comment,
            PROFILE_MIXIN_ID => return ContentType::Profile,
            _ => {}
        }
    }
    if item
        .mixin_payload
        .iter()
        .any(|m| m.mixin_id == IMAGE_MIXIN_ID)
    {
        return ContentType::Image;
    }
    ContentType::Document
}

/// Build an empty-payload marker mixin.
fn marker(mixin_id: u32) -> MixinPayloadMessage {
    MixinPayloadMessage {
        mixin_id,
        payload: Vec::new(),
    }
}

/// Encode an [`ImageSpec`] into the `ImageMixinMessage` payload bytes.
///
/// Returns an error if the image digest hex or any mipmap CID is malformed,
/// rather than silently encoding an empty `ipfs_hash`. A bad image reference
/// must fail the whole publish so it can never land on-chain.
pub fn encode_image_mixin(image: &ImageSpec) -> Result<Vec<u8>, crate::CoordError> {
    let mipmap_levels = image
        .mipmap_levels
        .iter()
        .map(|l| {
            Ok(MipmapLevelMessage {
                filesize: l.filesize,
                ipfs_hash: cid_to_digest_bytes(&l.cid)?,
            })
        })
        .collect::<Result<Vec<_>, crate::CoordError>>()?;

    let message = ImageMixinMessage {
        filename: image.filename.clone(),
        filesize: image.filesize,
        ipfs_hash: digest_hex_to_bytes(&image.digest_hex)?,
        width: image.width,
        height: image.height,
        mipmap_level: mipmap_levels,
    };
    Ok(message.encode_to_vec())
}

/// Decode an `ImageMixinMessage` payload into an [`ImageSpec`], converting each
/// level's digest back to a CID for display.
pub fn decode_image_mixin(msg: ImageMixinMessage) -> ImageSpec {
    ImageSpec {
        filename: msg.filename,
        filesize: msg.filesize,
        digest_hex: bytes_to_hex(&msg.ipfs_hash),
        width: msg.width,
        height: msg.height,
        mipmap_levels: msg
            .mipmap_level
            .into_iter()
            .map(|l| MipmapLevel {
                filesize: l.filesize,
                cid: bytes_to_cid(&l.ipfs_hash),
            })
            .collect(),
    }
}

/// Extract a single mixin's payload by ID.
pub fn decode_single_mixin<M>(item: &ItemMessage, mixin_id: u32) -> Option<M>
where
    M: Message + Default,
{
    item.mixin_payload
        .iter()
        .find(|m| m.mixin_id == mixin_id)
        .and_then(|m| M::decode(m.payload.as_slice()).ok())
}

// ── Item-id derivation ──────────────────────────────────────────────────────

/// Derive a deterministic item ID from the publishing account, a caller-
/// supplied nonce, and the fixed namespace:
///
/// `blake2_256(SCALE(account_id) ++ SCALE(nonce) ++ SCALE(namespace=1000))`
///
/// The result matches what `pallet-content` computes, so a client can predict
/// an item's ID before it exists and subscribe to it.
pub fn derive_item_id(account_id: [u8; 32], nonce: [u8; 32]) -> [u8; 32] {
    let payload = [
        parity_scale_codec::Encode::encode(&account_id),
        parity_scale_codec::Encode::encode(&nonce),
        parity_scale_codec::Encode::encode(&ITEM_ID_NAMESPACE),
    ]
    .concat();
    blake2_256(&payload)
}

// ── Hex / CID helpers ──────────────────────────────────────────────────────

/// `[u8; 32]` -> `0x`-prefixed hex string.
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

/// `0x`-prefixed hex (32 bytes) -> `[u8; 32]`.
pub fn hex_to_bytes(hex_value: &str) -> Result<[u8; 32], crate::CoordError> {
    let raw = hex::decode(hex_value.trim_start_matches("0x"))
        .map_err(|_| crate::CoordError::Cid(format!("invalid hex value {hex_value}")))?;
    raw.try_into()
        .map_err(|_| crate::CoordError::Cid(format!("expected 32 bytes for {hex_value}")))
}

/// Convenience [`hex_to_bytes`] returning a `Vec<u8>` for the mixin message.
///
/// Returns an error rather than silently producing empty bytes so a malformed
/// digest fails loudly instead of encoding a wrong (empty) payload.
fn hex_to_bytes_vec(hex_value: &str) -> Result<Vec<u8>, crate::CoordError> {
    Ok(hex_to_bytes(hex_value)?.to_vec())
}

/// `0x`-prefixed digest hex -> raw digest bytes, for the protobuf mixin.
fn digest_hex_to_bytes(hex_value: &str) -> Result<Vec<u8>, crate::CoordError> {
    hex_to_bytes_vec(hex_value)
}

/// sha2-256 digest hex (`0x`) -> IPFS CIDv0 (Base58), prefixing the
/// `0x12 0x20` multihash header.
pub fn digest_hex_to_cid(hex_value: &str) -> Result<String, crate::CoordError> {
    let digest = hex_to_bytes(hex_value)?;
    let mut multihash = Vec::with_capacity(34);
    multihash.push(0x12);
    multihash.push(0x20);
    multihash.extend_from_slice(&digest);
    Ok(bs58::encode(multihash).into_string())
}

/// IPFS CIDv0 (Base58 sha2-256) -> `0x`-prefixed digest hex.
pub fn cid_to_digest_hex(cid: &str) -> Result<String, crate::CoordError> {
    let multihash = bs58::decode(cid)
        .into_vec()
        .map_err(|_| crate::CoordError::Cid(format!("failed to decode CID {cid}")))?;
    if multihash.len() != 34 || multihash[0] != 0x12 || multihash[1] != 0x20 {
        return Err(crate::CoordError::Cid(format!(
            "CID {cid} is not a sha2-256 CIDv0 multihash"
        )));
    }
    Ok(format!("0x{}", hex::encode(&multihash[2..])))
}

/// Raw 32-byte digest -> IPFS CIDv0 (Base58).
pub fn bytes_to_cid(digest: &[u8]) -> String {
    let mut multihash = Vec::with_capacity(34);
    multihash.push(0x12);
    multihash.push(0x20);
    multihash.extend_from_slice(digest);
    bs58::encode(multihash).into_string()
}

/// IPFS CIDv0 -> raw 32-byte digest, for the protobuf mixin. Performs both the
/// multihash-form validation (in [`cid_to_digest_hex`]) and the 32-byte length
/// check (in [`hex_to_bytes`]), returning an error instead of an empty payload
/// on any malformed input.
fn cid_to_digest_bytes(cid: &str) -> Result<Vec<u8>, crate::CoordError> {
    Ok(hex_to_bytes(&cid_to_digest_hex(cid)?)?.to_vec())
}

/// Abbreviate a long hex string to `first10...last8` for display.
pub fn short_hex(value: &str) -> String {
    if value.len() <= 18 {
        value.to_string()
    } else {
        format!("{}...{}", &value[..10], &value[value.len() - 8..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_item_id_is_deterministic_and_nonce_sensitive() {
        let account = [7u8; 32];
        let nonce_a = [1u8; 32];
        let nonce_b = [2u8; 32];

        assert_eq!(
            derive_item_id(account, nonce_a),
            derive_item_id(account, nonce_a)
        );
        assert_ne!(
            derive_item_id(account, nonce_a),
            derive_item_id(account, nonce_b)
        );
        // Changing the account changes the id.
        assert_ne!(
            derive_item_id(account, nonce_a),
            derive_item_id([8u8; 32], nonce_a)
        );
        // Non-zero.
        assert_ne!(derive_item_id(account, nonce_a), [0u8; 32]);
    }

    #[test]
    fn hex_helpers_round_trip_and_validate() {
        let bytes = [0xabu8; 32];
        let encoded = bytes_to_hex(&bytes);
        assert_eq!(encoded, format!("0x{}", "ab".repeat(32)));
        assert_eq!(hex_to_bytes(&encoded).unwrap(), bytes);
        assert_eq!(hex_to_bytes(&encoded[2..]).unwrap(), bytes);

        assert!(hex_to_bytes("0xzz").is_err());
        assert!(hex_to_bytes("0x1234").is_err());
    }

    #[test]
    fn cid_helpers_round_trip() {
        let digest = format!("0x{}", "11".repeat(32));
        let cid = digest_hex_to_cid(&digest).unwrap();
        assert_eq!(cid_to_digest_hex(&cid).unwrap(), digest);
        // short_hex keeps the 0x prefix in its leading 10 chars ("0x" + 8 nibbles).
        assert_eq!(
            short_hex(&digest),
            format!("0x{}...{}", "11111111", "11111111")
        );
    }

    #[test]
    fn cid_rejects_non_sha256_multihash() {
        let not_sha256 = bs58::encode(
            [0x13u8, 0x20]
                .into_iter()
                .chain([0u8; 32])
                .collect::<Vec<_>>(),
        )
        .into_string();
        assert!(cid_to_digest_hex(&not_sha256).is_err());
    }

    #[test]
    fn encode_decode_document_round_trip() {
        let input = ContentInput {
            content_type: ContentType::Document,
            title: Some("Hello".into()),
            body: Some("World".into()),
            language: Some("en".into()),
            image: None,
            profile: None,
        };
        let bytes = encode_item(&input).unwrap();
        let decoded = decode_item(&bytes).unwrap();

        assert_eq!(decoded.content_type, ContentType::Document);
        assert_eq!(decoded.title.as_deref(), Some("Hello"));
        assert_eq!(decoded.body.as_deref(), Some("World"));
        assert_eq!(decoded.language.as_deref(), Some("en"));
        assert!(decoded.image.is_none());
        assert!(decoded.profile.is_none());
    }

    #[test]
    fn encode_decode_feed_includes_marker() {
        let input = ContentInput {
            content_type: ContentType::Feed,
            title: Some("Feed".into()),
            body: None,
            language: None,
            image: None,
            profile: None,
        };
        let item = ItemMessage::decode(encode_item(&input).unwrap().as_slice()).unwrap();
        assert_eq!(infer_content_type(&item), ContentType::Feed);
        assert!(
            item.mixin_payload
                .iter()
                .any(|m| m.mixin_id == FEED_TYPE_MIXIN_ID && m.payload.is_empty())
        );
        // Default language applied.
        assert_eq!(
            decode_single_mixin::<LanguageMixinMessage>(&item, LANGUAGE_MIXIN_ID)
                .unwrap()
                .language_tag,
            DEFAULT_LANGUAGE_TAG
        );
    }

    #[test]
    fn encode_decode_comment_and_profile_types() {
        let comment = ContentInput {
            content_type: ContentType::Comment,
            title: None,
            body: Some("a comment".into()),
            language: None,
            image: None,
            profile: None,
        };
        let c_item = ItemMessage::decode(encode_item(&comment).unwrap().as_slice()).unwrap();
        assert_eq!(infer_content_type(&c_item), ContentType::Comment);

        let profile = ContentInput {
            content_type: ContentType::Profile,
            title: Some("Alice".into()),
            body: Some("bio".into()),
            language: None,
            image: None,
            profile: Some(ProfileSpec {
                account_type: AccountType::Project as i32,
                location: "Earth".into(),
            }),
        };
        let p_item = ItemMessage::decode(encode_item(&profile).unwrap().as_slice()).unwrap();
        assert_eq!(infer_content_type(&p_item), ContentType::Profile);
        let decoded = decode_item(&encode_item(&profile).unwrap()).unwrap();
        assert_eq!(decoded.content_type, ContentType::Profile);
        assert_eq!(decoded.title.as_deref(), Some("Alice"));
        assert_eq!(
            decoded.profile.as_ref().map(|p| p.location.as_str()),
            Some("Earth")
        );
        assert_eq!(decoded.profile.as_ref().map(|p| p.account_type), Some(2));
    }

    #[test]
    fn encode_decode_image_round_trip() {
        let digest = format!("0x{}", "22".repeat(32));
        let input = ContentInput {
            content_type: ContentType::Image,
            title: None,
            body: None,
            language: None,
            image: Some(ImageSpec {
                filename: "photo.jpg".into(),
                filesize: 12345,
                digest_hex: digest.clone(),
                width: 800,
                height: 600,
                mipmap_levels: vec![MipmapLevel {
                    filesize: 100,
                    cid: digest_hex_to_cid(&digest).unwrap(),
                }],
            }),
            profile: None,
        };
        let item = ItemMessage::decode(encode_item(&input).unwrap().as_slice()).unwrap();
        assert_eq!(infer_content_type(&item), ContentType::Image);
        let decoded = decode_item(&encode_item(&input).unwrap()).unwrap();
        let image = decoded.image.expect("image should decode");
        assert_eq!(image.filename, "photo.jpg");
        assert_eq!(image.filesize, 12345);
        assert_eq!(image.width, 800);
        assert_eq!(image.height, 600);
        // Round-trips the full-res digest and the mipmap CID.
        assert_eq!(image.digest_hex, digest);
        assert_eq!(image.mipmap_levels.len(), 1);
        assert_eq!(image.mipmap_levels[0].filesize, 100);
        assert_eq!(
            image.mipmap_levels[0].cid,
            digest_hex_to_cid(&digest).unwrap()
        );
    }

    #[test]
    fn decode_unknown_mixins_are_ignored() {
        let mut item = ItemMessage {
            mixin_payload: vec![MixinPayloadMessage {
                mixin_id: 0xdead_beef,
                payload: vec![1, 2, 3],
            }],
        };
        // A valid title mixin alongside the unknown one.
        item.mixin_payload.push(MixinPayloadMessage {
            mixin_id: TITLE_MIXIN_ID,
            payload: TitleMixinMessage {
                title: "kept".into(),
            }
            .encode_to_vec(),
        });
        let decoded = decode_item(&item.encode_to_vec()).unwrap();
        assert_eq!(decoded.title.as_deref(), Some("kept"));
        assert_eq!(decoded.content_type, ContentType::Document);
    }

    #[test]
    fn encode_item_rejects_malformed_image_digest() {
        // A malformed image digest must fail the whole encode rather than
        // silently producing a payload with an empty `ipfs_hash`.
        let input = ContentInput {
            content_type: ContentType::Image,
            title: None,
            body: None,
            language: None,
            image: Some(ImageSpec {
                filename: "bad.jpg".into(),
                filesize: 1,
                digest_hex: "0xnothex".into(),
                width: 0,
                height: 0,
                mipmap_levels: vec![],
            }),
            profile: None,
        };
        assert!(encode_item(&input).is_err());
    }

    #[test]
    fn encode_item_rejects_malformed_mipmap_cid() {
        // A malformed mipmap CID must fail the whole encode too.
        let input = ContentInput {
            content_type: ContentType::Image,
            title: None,
            body: None,
            language: None,
            image: Some(ImageSpec {
                filename: "b.jpg".into(),
                filesize: 1,
                digest_hex: format!("0x{}", "22".repeat(32)),
                width: 0,
                height: 0,
                mipmap_levels: vec![MipmapLevel {
                    filesize: 1,
                    cid: "not-a-cid".into(),
                }],
            }),
            profile: None,
        };
        assert!(encode_item(&input).is_err());
    }
}
