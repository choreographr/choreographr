//! Serialized request bodies, request-body compression, and the
//! upload-complete hook.
//!
//! [`HookedJsonBody`] serializes a request body to compact JSON once and hands
//! ureq a fresh [`UploadHookReader`] per retry attempt.  The reader fires the
//! daemon's upload-complete callback — the `Waiting` boundary — once its final
//! byte is read, the last client-side point before the opaque server-side wait.
//!
//! When a provider opts into request compression, the payload is additionally
//! gzipped once (see [`RequestCompression`]); the reader then counts down the
//! *compressed* bytes exactly as it would the plain ones, so the upload boundary
//! and the length-delimited send are unchanged.  A body can be downgraded to
//! identity in place (see [`HookedJsonBody`] docs) so a provider that rejects
//! `Content-Encoding: gzip` with a 415 can be retried uncompressed without a
//! re-serialize.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use flate2::Compression;
use flate2::write::GzEncoder;

use super::RequestUploadedCallback;

/// The media type every provider request body is sent as.
const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

/// gzip level for the compressed request body. 6 mirrors the `session_turns`
/// DB codec's level: a good default speed/ratio trade-off, and this runs once
/// per turn on the request path (not per byte), so a higher level buys little
/// for a real latency cost on large conversations.
const GZIP_LEVEL: u32 = 6;

/// Whether a provider request body is compressed on the wire.
///
/// Request compression is opt-in per provider (the server must decompress), so
/// the default is [`RequestCompression::Identity`] and only providers the
/// catalog overlay marks as supporting it send a `Content-Encoding` header.
/// The set of supported codings is deliberately a subset of what the HTTP stack
/// accepts on responses (ureq decompresses gzip and brotli); gzip is the one
/// coding universally supported by providers that accept compressed requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RequestCompression {
    /// Send the JSON body uncompressed.
    #[default]
    Identity,
    /// Send the JSON body gzip-compressed with `Content-Encoding: gzip`.
    Gzip,
}

/// A serialized JSON request body plus the optional upload-complete hook.
///
/// The payload is serialized to compact JSON once; every retry attempt hands
/// ureq a fresh [`UploadHookReader`] over a shared clone of those bytes.  The
/// caller must apply [`Self::apply_headers`] to set `Content-Length`
/// explicitly, so the body is sent length-delimited rather than chunked — ureq
/// only honours a caller-supplied length for a reader body, never the reader's
/// own.  This is load-bearing, not cosmetic: with an explicit length ureq's
/// send loop stops the instant the last byte is read (never issuing a
/// terminating zero-length read), and a short body would spin that loop
/// forever, so the declared length must always equal the bytes the reader
/// yields.
///
/// When `compression` is [`RequestCompression::Gzip`], the body is gzipped once
/// here (alongside the plain bytes) and both `Content-Length` and
/// `Content-Encoding` are set from that compressed form.  A body built
/// compressed can be **[`Self::downgrade`]d** back to identity in place (the
/// plain bytes are retained), which is what lets a provider that answers a
/// compressed request with `415 Unsupported Media Type` be retried uncompressed
/// without re-serializing the conversation.
pub(crate) struct HookedJsonBody {
    /// The compact JSON payload, always retained so a downgrade can fall back
    /// to it.
    plain: Arc<[u8]>,
    /// `Content-Length` for the plain payload, rendered once so a retry re-uses
    /// it instead of allocating a fresh `String` per `apply_headers` call.
    plain_len: String,
    /// The gzipped payload plus its rendered `Content-Length`, present only
    /// when compression was requested.
    gzip: Option<GzippedBody>,
    /// Whether the next send should use the gzipped form.  Flipped once, from a
    /// send thread observing a 415, by [`Self::downgrade`] — a single-bit
    /// control flag carrying no data (the sanctioned shared-state exception),
    /// so [`Self::apply_headers`] and [`Self::send_body`] stay `&self`.
    use_gzip: AtomicBool,
    on_uploaded: Option<RequestUploadedCallback>,
}

/// The gzipped form of a request body plus its rendered `Content-Length`.
struct GzippedBody {
    bytes: Arc<[u8]>,
    len: String,
}

impl HookedJsonBody {
    /// Serialize `value` to compact JSON, gzip it when `compression` requests
    /// it, and remember the upload hook.
    ///
    /// Generic over [`serde::Serialize`] so a caller can pass a typed wire
    /// struct (the image and video adapters) as readily as a built
    /// [`serde_json::Value`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if serialization or the gzip pass fails; the
    /// serialization failure is wrapped via [`io::Error::other`] so callers map
    /// a single error type.
    pub(crate) fn new<T: serde::Serialize>(
        value: &T,
        on_uploaded: Option<RequestUploadedCallback>,
        compression: RequestCompression,
    ) -> Result<Self, io::Error> {
        let plain: Arc<[u8]> = serde_json::to_vec(value).map_err(io::Error::other)?.into();
        let plain_len = plain.len().to_string();
        let gzip = match compression {
            RequestCompression::Identity => None,
            RequestCompression::Gzip => {
                let compressed = gzip_compress(&plain)?;
                let len = compressed.len().to_string();
                Some(GzippedBody {
                    bytes: compressed.into(),
                    len,
                })
            }
        };
        let use_gzip = AtomicBool::new(gzip.is_some());
        Ok(Self {
            plain,
            plain_len,
            gzip,
            use_gzip,
            on_uploaded,
        })
    }

    /// Apply the `Content-Type`/`Content-Length` headers every request body
    /// needs (see the type docs), plus `Content-Encoding: gzip` when the body
    /// is currently sent compressed.  Kept here so the pairing cannot drift
    /// between providers and so the length always matches the bytes the reader
    /// will yield.
    pub(crate) fn apply_headers<B>(
        &self,
        request: ureq::RequestBuilder<B>,
    ) -> ureq::RequestBuilder<B> {
        let request = request.header("Content-Type", JSON_CONTENT_TYPE);
        match self.active() {
            Active::Plain => request.header("Content-Length", self.plain_len.as_str()),
            Active::Gzip(GzippedBody { len, .. }) => request
                .header("Content-Encoding", "gzip")
                .header("Content-Length", len.as_str()),
        }
    }

    /// A fresh body for one attempt.  The returned reader fires the upload hook
    /// exactly once, the instant its final byte is handed to ureq.  It reads
    /// whichever form the body is currently in, so a post-downgrade attempt
    /// reads the plain bytes.
    pub(crate) fn send_body(&self) -> ureq::SendBody<'static> {
        let bytes = match self.active() {
            Active::Plain => Arc::clone(&self.plain),
            Active::Gzip(GzippedBody { bytes, .. }) => Arc::clone(bytes),
        };
        ureq::SendBody::from_owned_reader(UploadHookReader::new(bytes, self.on_uploaded.clone()))
    }

    /// Whether the next send would be gzip-compressed.  `false` once downgraded.
    pub(super) fn is_compressed(&self) -> bool {
        self.use_gzip.load(Ordering::Relaxed) && self.gzip.is_some()
    }

    /// Drop gzip and send the retained plain bytes on every subsequent attempt.
    /// Idempotent; a no-op for a body built identity.
    pub(super) fn downgrade(&self) {
        self.use_gzip.store(false, Ordering::Relaxed);
    }

    /// The form the next send uses.  `Gzip` is unreachable when `gzip` is
    /// `None`, because `use_gzip` is only ever set true when `gzip` is `Some`.
    fn active(&self) -> Active<'_> {
        match &self.gzip {
            Some(g) if self.use_gzip.load(Ordering::Relaxed) => Active::Gzip(g),
            _ => Active::Plain,
        }
    }
}

/// The body form currently selected by a [`HookedJsonBody`].
enum Active<'a> {
    Plain,
    Gzip(&'a GzippedBody),
}

/// Compress `bytes` to a gzip stream (RFC 1952: zlib deflate plus the CRCs and
/// length trailer ureq's gzip decoders and every provider's expect).
fn gzip_compress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(GZIP_LEVEL));
    encoder.write_all(bytes)?;
    encoder.finish()
}

/// A `Read` over a shared in-memory payload that fires its hook once, when the
/// last byte has been read.
///
/// The hook counts bytes rather than waiting for a zero-length read because
/// ureq's length-delimited send loop stops as soon as the declared
/// `Content-Length` is satisfied (it never issues a terminating `read` that
/// returns `0`).  Firing on the last real byte is what makes the boundary
/// observable even when the provider withholds response headers until
/// generation starts.  Returning `0` afterwards never re-fires (the hook is
/// taken on first completion).
///
/// The hook fires the instant the final byte is read into ureq's output
/// buffer, a moment before ureq transmits that final chunk — so a transmit
/// failure on the last chunk still leaves the boundary already reported.  That
/// is the intended granularity: the boundary tracks the client-side upload, the
/// last point the transport exposes before the response is read.  When the
/// payload is gzip-compressed this is the last compressed byte, which is the
/// same boundary — compression only changes the bytes counted down, not where
/// the count reaches zero.
pub(super) struct UploadHookReader {
    bytes: Arc<[u8]>,
    pos: usize,
    on_uploaded: Option<RequestUploadedCallback>,
}

impl UploadHookReader {
    /// Wrap a payload with an optional single-shot upload hook.
    pub(super) fn new(bytes: Arc<[u8]>, on_uploaded: Option<RequestUploadedCallback>) -> Self {
        Self {
            bytes,
            pos: 0,
            on_uploaded,
        }
    }
}

impl io::Read for UploadHookReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.bytes.get(self.pos..).unwrap_or_default();
        let n = remaining.len().min(buf.len());
        if let Some(dst) = buf.get_mut(..n) {
            dst.copy_from_slice(remaining.get(..n).unwrap_or_default());
        }
        self.pos += n;
        // Guard on a non-empty read so the hook fires on the last real byte,
        // never on the first read of an empty payload (which would report an
        // upload that sent nothing).
        if n > 0
            && self.pos == self.bytes.len()
            && let Some(cb) = self.on_uploaded.take()
        {
            cb();
        }
        Ok(n)
    }
}
