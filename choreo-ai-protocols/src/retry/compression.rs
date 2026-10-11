//! Request-body compression: the per-provider coding, the codecs, and the
//! process-wide downgrade learned from a provider's HTTP 415.
//!
//! A provider opts into request compression through its catalog-overlay
//! `request_compression` scalar, which lands on this [`RequestCompression`]
//! enum.  The serialized body that carries the resulting compressed bytes (and
//! the `Content-Encoding`/`Content-Length` pairing) lives in the
//! [`super::send_body`] submodule; this module owns only the coding selection,
//! the compression itself, and the process-global downgrade registry consulted
//! by [`effective_request_compression`].

use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::{Arc, LazyLock, RwLock};

use flate2::Compression;
use flate2::write::GzEncoder;

/// gzip level for the compressed request body.  6 mirrors the `session_turns`
/// DB codec's level: a good default speed/ratio trade-off, and this runs once
/// per turn on the request path (not per byte), so a higher level buys little
/// for a real latency cost on large conversations.
const GZIP_LEVEL: u32 = 6;

/// zstd level for the compressed request body.  3 is zstd's usual default and
/// already matches gzip's ratio for this kind of JSON at a fraction of the
/// cost, so it is the right trade-off for the request path.
const ZSTD_LEVEL: i32 = 3;

/// Whether a provider request body is compressed on the wire.
///
/// Request compression is opt-in per provider (the server must decompress), so
/// the default is [`RequestCompression::Identity`] and only providers the
/// catalog overlay marks as supporting it send a `Content-Encoding` header.
/// The supported codings are a subset of what the HTTP stack accepts on
/// responses (ureq decompresses gzip and brotli); in practice each coding is
/// accepted by only some providers, so the choice is per provider:
/// [`RequestCompression::Gzip`] is the broadly-supported one, and
/// [`RequestCompression::Zstd`] targets endpoints known to decode zstd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RequestCompression {
    /// Send the JSON body uncompressed.
    #[default]
    Identity,
    /// Send the JSON body gzip-compressed with `Content-Encoding: gzip`.
    Gzip,
    /// Send the JSON body zstd-compressed with `Content-Encoding: zstd`.
    Zstd,
}

impl RequestCompression {
    /// Parse an overlay coding token — the lowercase spelling the
    /// `request_compression` overlay scalar uses (`"identity"`, `"gzip"`,
    /// `"zstd"`).  Returns `None` for an unrecognized value so the caller can
    /// warn and keep the safe identity default rather than guess a coding the
    /// server may not decode.
    #[must_use]
    pub(crate) fn from_overlay_str(value: &str) -> Option<Self> {
        match value {
            "identity" => Some(Self::Identity),
            "gzip" => Some(Self::Gzip),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    /// The `Content-Encoding` token this coding is sent under, or `None` for
    /// [`Self::Identity`] (which sends no `Content-Encoding` header).  The
    /// single source of truth for the wire token, so the coding, its codec, and
    /// its label cannot drift apart.
    #[must_use]
    pub(super) fn content_encoding(self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::Gzip => Some("gzip"),
            Self::Zstd => Some("zstd"),
        }
    }
}

/// A compressed request body: its bytes, its rendered `Content-Length`, and the
/// coding it carries (from which the `Content-Encoding` token is derived).
/// Owned here so [`super::send_body::HookedJsonBody`] can carry either coding
/// behind one field and pair the length/encoding with the bytes it will
/// actually read.
pub(super) struct CompressedBody {
    pub(super) bytes: Arc<[u8]>,
    pub(super) len: String,
    /// The coding these bytes carry; [`RequestCompression::content_encoding`]
    /// derives the `Content-Encoding` token from it.
    pub(super) coding: RequestCompression,
}

impl CompressedBody {
    /// Wrap compressed `bytes` together with the `coding` they were produced
    /// by, rendering the length once.
    pub(super) fn new(bytes: Vec<u8>, coding: RequestCompression) -> Self {
        let len = bytes.len().to_string();
        Self {
            bytes: bytes.into(),
            len,
            coding,
        }
    }
}

/// Compress `bytes` to a gzip stream (RFC 1952: zlib deflate plus the CRCs and
/// length trailer every gzip decoder expects).
pub(super) fn gzip_compress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(GZIP_LEVEL));
    encoder.write_all(bytes)?;
    encoder.finish()
}

/// Compress `bytes` to a single standard zstd frame at [`ZSTD_LEVEL`].
///
/// Uses the same pure-Rust codec the daemon's `session_turns` table uses (no
/// libzstd C build) and emits a standard zstd frame, so a provider expecting
/// `Content-Encoding: zstd` decodes it.  Infallible — any input frames — so it
/// returns the `Vec` directly rather than a `Result`.
pub(super) fn zstd_compress(bytes: &[u8]) -> Vec<u8> {
    structured_zstd::encoding::compress_slice_to_vec(
        bytes,
        structured_zstd::encoding::CompressionLevel::from_level(ZSTD_LEVEL),
    )
}

/// Provider slugs whose endpoint answered a compressed request body with HTTP
/// 415 in this process, i.e. do not decompress request-body compression.
///
/// [`effective_request_compression`] reads this before compressing and
/// [`note_request_compression_downgraded`] inserts into it on a 415, so one
/// rejected request permanently (for the process) downgrades that provider to
/// identity — retrying a body the server cannot decode can only waste attempts.
///
/// This is a process-wide behavioral cache: every request of a
/// compression-opted-in provider consults it (a shared read lock — the identity
/// default skips the lookup entirely), and only a 415 writes it (a write lock,
/// once per provider), so readers never contend with one another.  It is
/// single-purpose bookkeeping that carries no protocol data; the lock recovery
/// follows AGENTS.md's poisoned-lock rule, with a lost entry an acceptable
/// failure mode for a downgrade hint.
static COMPRESSION_DOWNGRADED: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// The request compression to actually use for `slug`: `configured`, unless
/// this process has already learned from a 415 that `slug` does not accept
/// compressed request bodies.
#[must_use]
pub(crate) fn effective_request_compression(
    slug: &str,
    configured: RequestCompression,
) -> RequestCompression {
    if configured == RequestCompression::Identity {
        return RequestCompression::Identity;
    }
    let downgraded = COMPRESSION_DOWNGRADED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(slug);
    if downgraded {
        RequestCompression::Identity
    } else {
        configured
    }
}

/// Record that `slug` rejected a compressed request body, so future requests
/// skip compression.  Warns once per slug.
pub(super) fn note_request_compression_downgraded(slug: &str) {
    let fresh = COMPRESSION_DOWNGRADED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(slug.to_string());
    if fresh {
        tracing::warn!(
            provider = %slug,
            "provider rejected compressed request bodies (HTTP 415); \
             sending uncompressed bodies for the rest of this process",
        );
    }
}
