//! The data plane over a real socket, in three parts. This module is the shared
//! preamble: it declares the two halves and owns [`wire_frames`], the production
//! data-plane encoder that a test needs in order to put bytes on the wire
//! `send_message` would never produce. `round_trip` covers the encrypted round
//! trips of every payload size and fragmentation shape plus the peer-close
//! classification; `bounds` covers the caps, deadlines and stalls that bound a
//! message — the per-frame cap on an unauthenticated length prefix there, and
//! the cumulative reassembly cap as a unit test of its arithmetic in
//! `noise::frame` (reaching it end to end would mean pushing `MAX_FRAME_SIZE` of
//! ciphertext through the socket).

mod bounds;
mod round_trip;

use choreo_transport::noise::wire::{
    FRAGMENT_CONTINUATION, FRAGMENT_HEADER_LEN, GCM_TAG_LEN, LEN_PREFIX_LEN, MAX_CIPHERTEXT_LEN,
    MAX_PLAINTEXT_CHUNK,
};

/// Encode `payload` as complete wire frames under the production data-plane
/// framing (a `LEN_PREFIX_LEN`-byte big-endian ciphertext length, then the
/// ciphertext), one `Vec` per Noise fragment.
///
/// Every figure here is the crate's own published one, so the harness cannot
/// describe a frame the transport would never write.
pub(super) fn wire_frames(transport: &mut snow::TransportState, payload: &[u8]) -> Vec<Vec<u8>> {
    // A payload travels as ceil(len / MAX_PLAINTEXT_CHUNK) fragments; only the
    // last one clears the continuation bit, and that bit rides INSIDE the
    // ciphertext so the receiver's reassembly decision rests on authenticated
    // plaintext rather than the unauthenticated wire prefix.
    let fragment_count = payload.chunks(MAX_PLAINTEXT_CHUNK).count();
    let mut frames = Vec::with_capacity(fragment_count);
    for (index, chunk) in payload.chunks(MAX_PLAINTEXT_CHUNK).enumerate() {
        let mut plaintext = Vec::with_capacity(FRAGMENT_HEADER_LEN + chunk.len());
        let more_follow = index + 1 < fragment_count;
        plaintext.push(if more_follow {
            FRAGMENT_CONTINUATION
        } else {
            0
        });
        plaintext.extend_from_slice(chunk);
        // A Noise ciphertext is the plaintext plus the GCM tag, so the output
        // buffer is exactly the size a successful write fills. A failed write
        // (a state that is not in transport mode, an exhausted nonce space)
        // leaves the buffer untouched and reports zero bytes; truncating to that
        // emits a frame the peer refuses outright as below its tag-plus-header
        // minimum, instead of one it would block on waiting for bytes that never
        // arrive.
        let mut ciphertext = vec![0u8; plaintext.len() + GCM_TAG_LEN];
        let written = transport
            .write_message(&plaintext, &mut ciphertext)
            .unwrap_or_default();
        ciphertext.truncate(written);
        // The prefix goes through the same codec the transport writes it with,
        // at the same width and cap, so a frame this harness emits is one
        // `decode_len` accepts. `unwrap_or_default` keeps the helper panic-free:
        // a length the cap refuses yields a zero prefix, which the receiver
        // rejects as below its minimum rather than misreading as a frame.
        let prefix = choreo_proto::len_prefix::encode::<LEN_PREFIX_LEN>(
            ciphertext.len(),
            MAX_CIPHERTEXT_LEN,
        )
        .unwrap_or_default();
        let mut frame = Vec::with_capacity(LEN_PREFIX_LEN + ciphertext.len());
        frame.extend_from_slice(&prefix);
        frame.extend_from_slice(&ciphertext);
        frames.push(frame);
    }
    frames
}
