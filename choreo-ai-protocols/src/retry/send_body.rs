//! Serialized request bodies and the upload-complete hook.
//!
//! [`HookedJsonBody`] serializes a request body to compact JSON once and hands
//! ureq a fresh [`UploadHookReader`] per retry attempt.  The reader fires the
//! daemon's upload-complete callback — the `Waiting` boundary — once its final
//! byte is read, the last client-side point before the opaque server-side wait.
//!
//! When a provider opts into request compression (see
//! [`RequestCompression`](super::compression::RequestCompression)), the payload
//! is additionally compressed once; the reader then counts down the *compressed*
//! bytes exactly as it would the plain ones, so the upload boundary and the
//! length-delimited send are unchanged.  A body can be downgraded to identity in
//! place (see [`HookedJsonBody`] docs) so a provider that rejects the chosen
//! `Content-Encoding` with a 415 can be retried uncompressed without a
//! re-serialize.

use std::cell::Cell;
use std::io;
use std::sync::Arc;

use super::RequestUploadedCallback;
use super::compression::{CompressedBody, RequestCompression, gzip_compress, zstd_compress};

/// The media type every provider request body is sent as.
const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

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
/// When `compression` is gzip or zstd, the body is compressed once here
/// (alongside the plain bytes) and both `Content-Length` and `Content-Encoding`
/// are set from that compressed form.  A body built compressed can be
/// **[`Self::downgrade`]d** back to identity in place (the plain bytes are
/// retained), which is what lets a provider that answers a compressed request
/// with `415 Unsupported Media Type` be retried uncompressed without
/// re-serializing the conversation.
pub(crate) struct HookedJsonBody {
    /// The compact JSON payload, always retained so a downgrade can fall back
    /// to it.
    plain: Arc<[u8]>,
    /// `Content-Length` for the plain payload, rendered once so a retry re-uses
    /// it instead of allocating a fresh `String` per `apply_headers` call.
    plain_len: String,
    /// The compressed payload plus its rendered `Content-Length` and the coding
    /// it carries (from which the `Content-Encoding` token is derived), present
    /// only when compression was requested.
    compressed: Option<CompressedBody>,
    /// Whether the next send should use the compressed form.  Flipped once, by
    /// [`Self::downgrade`], when a 415 shows the provider cannot decode it.
    ///
    /// Plain [`Cell`] interior mutability, not an atomic: the whole retry loop
    /// for one request runs on a single thread, so a shared flag is neither
    /// needed nor wanted.  `Cell` keeps [`Self::apply_headers`]/[`Self::send_body`]
    /// at `&self` while making `HookedJsonBody` `!Sync` — the right compile-time
    /// marker for state that must not cross threads.
    use_compression: Cell<bool>,
    on_uploaded: Option<RequestUploadedCallback>,
}

impl HookedJsonBody {
    /// Serialize `value` to compact JSON, compress it when `compression`
    /// requests it, and remember the upload hook.
    ///
    /// Generic over [`serde::Serialize`] so a caller can pass a typed wire
    /// struct (the image and video adapters) as readily as a built
    /// [`serde_json::Value`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if serialization or the gzip pass fails; the
    /// serialization failure is wrapped via [`io::Error::other`] so callers map
    /// a single error type.  The zstd pass is infallible (see
    /// [`zstd_compress`](super::compression::zstd_compress)) and never
    /// contributes an error.
    ///
    /// When a body is compressed, one payload-free `debug!` event reports the
    /// coding and the before/after byte counts (never the body), so the
    /// compression win stays observable.
    pub(crate) fn new<T: serde::Serialize>(
        value: &T,
        on_uploaded: Option<RequestUploadedCallback>,
        compression: RequestCompression,
    ) -> Result<Self, io::Error> {
        let plain: Arc<[u8]> = serde_json::to_vec(value).map_err(io::Error::other)?.into();
        let plain_bytes = plain.len();
        let plain_len = plain_bytes.to_string();
        let compressed = match compression {
            RequestCompression::Identity => None,
            RequestCompression::Gzip => {
                Some(CompressedBody::new(gzip_compress(&plain)?, compression))
            }
            RequestCompression::Zstd => {
                Some(CompressedBody::new(zstd_compress(&plain), compression))
            }
        };
        // Payload-free upload-size telemetry: the coding and the before/after
        // byte counts only — never the body — so the compression win (fewer
        // bytes on the wire) is observable.  Same field discipline as the retry
        // and downgrade logs.
        if let Some(body) = &compressed {
            tracing::debug!(
                coding = body.coding.content_encoding().unwrap_or("identity"),
                plain_bytes,
                compressed_bytes = body.bytes.len(),
                "compressed request body",
            );
        }
        let use_compression = Cell::new(compressed.is_some());
        Ok(Self {
            plain,
            plain_len,
            compressed,
            use_compression,
            on_uploaded,
        })
    }

    /// Apply the `Content-Type`/`Content-Length` headers every request body
    /// needs (see the type docs), plus `Content-Encoding` when the body is
    /// currently sent compressed.  Kept here so the pairing cannot drift
    /// between providers and so the length always matches the bytes the reader
    /// will yield.
    pub(crate) fn apply_headers<B>(
        &self,
        request: ureq::RequestBuilder<B>,
    ) -> ureq::RequestBuilder<B> {
        let request = request.header("Content-Type", JSON_CONTENT_TYPE);
        match self.active() {
            Active::Plain => request.header("Content-Length", self.plain_len.as_str()),
            // A compressed body is never `Identity`, so the coding's token is
            // always present; derive it from the coding rather than storing a
            // second copy (see `RequestCompression::content_encoding`).
            Active::Compressed(c) => {
                let request = request.header("Content-Length", c.len.as_str());
                match c.coding.content_encoding() {
                    Some(encoding) => request.header("Content-Encoding", encoding),
                    None => request,
                }
            }
        }
    }

    /// A fresh body for one attempt.  The returned reader fires the upload hook
    /// exactly once, the instant its final byte is handed to ureq.  It reads
    /// whichever form the body is currently in, so a post-downgrade attempt
    /// reads the plain bytes.
    pub(crate) fn send_body(&self) -> ureq::SendBody<'static> {
        let bytes = match self.active() {
            Active::Plain => Arc::clone(&self.plain),
            Active::Compressed(c) => Arc::clone(&c.bytes),
        };
        ureq::SendBody::from_owned_reader(UploadHookReader::new(bytes, self.on_uploaded.clone()))
    }

    /// Whether the next send would be compressed.  `false` once downgraded.
    pub(super) fn is_compressed(&self) -> bool {
        matches!(self.active(), Active::Compressed(_))
    }

    /// Drop compression and send the retained plain bytes on every subsequent
    /// attempt.  Idempotent; a no-op for a body built identity.
    pub(super) fn downgrade(&self) {
        self.use_compression.set(false);
    }

    /// The form the next send uses.  `Compressed` is unreachable when
    /// `compressed` is `None`, because `use_compression` is only ever set true
    /// when `compressed` is `Some`.
    fn active(&self) -> Active<'_> {
        match &self.compressed {
            Some(c) if self.use_compression.get() => Active::Compressed(c),
            _ => Active::Plain,
        }
    }
}

/// The body form currently selected by a [`HookedJsonBody`].
enum Active<'a> {
    Plain,
    Compressed(&'a CompressedBody),
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
/// payload is compressed this is the last compressed byte, which is the same
/// boundary — compression only changes the bytes counted down, not where the
/// count reaches zero.
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
