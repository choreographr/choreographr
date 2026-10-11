//! Serialized request bodies and the upload-complete hook.
//!
//! [`HookedJsonBody`] serializes a request body to compact JSON once and hands
//! ureq a fresh [`UploadHookReader`] per retry attempt.  The reader fires the
//! daemon's upload-complete callback — the `Waiting` boundary — once its final
//! byte is read, the last client-side point before the opaque server-side wait.

use std::io;
use std::sync::Arc;

use super::RequestUploadedCallback;

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
/// yields.  A known length is also what lets a future request-body compressor
/// run *before* this hook: compress to bytes, then hand those bytes to the
/// same countdown reader (updating `content_length` alongside `bytes`).
pub(crate) struct HookedJsonBody {
    bytes: Arc<[u8]>,
    /// The `Content-Length` value, rendered once here so a retry re-uses it
    /// instead of allocating a fresh `String` per `apply_headers` call.
    content_length: String,
    on_uploaded: Option<RequestUploadedCallback>,
}

impl HookedJsonBody {
    /// Serialize `value` to compact JSON and remember the upload hook.
    ///
    /// Generic over [`serde::Serialize`] so a caller can pass a typed wire
    /// struct (the image and video adapters) as readily as a built
    /// [`serde_json::Value`].
    pub(crate) fn new<T: serde::Serialize>(
        value: &T,
        on_uploaded: Option<RequestUploadedCallback>,
    ) -> Result<Self, serde_json::Error> {
        let bytes: Arc<[u8]> = serde_json::to_vec(value)?.into();
        let content_length = bytes.len().to_string();
        Ok(Self {
            bytes,
            content_length,
            on_uploaded,
        })
    }

    /// Apply the `Content-Type`/`Content-Length` headers every request body
    /// needs (see the type docs).  Kept here so the pairing cannot drift
    /// between providers and so the length always matches the bytes the reader
    /// will yield.
    pub(crate) fn apply_headers<B>(
        &self,
        request: ureq::RequestBuilder<B>,
    ) -> ureq::RequestBuilder<B> {
        request
            .header("Content-Type", JSON_CONTENT_TYPE)
            .header("Content-Length", self.content_length.as_str())
    }

    /// A fresh body for one attempt.  The returned reader fires the upload hook
    /// exactly once, the instant its final byte is handed to ureq.
    pub(crate) fn send_body(&self) -> ureq::SendBody<'static> {
        ureq::SendBody::from_owned_reader(UploadHookReader::new(
            Arc::clone(&self.bytes),
            self.on_uploaded.clone(),
        ))
    }
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
/// last point the transport exposes before the response is read.
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
