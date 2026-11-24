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

/// Encode `payload` as complete wire frames under the production data-plane
/// framing (4-byte big-endian ciphertext length, then the ciphertext), one
/// `Vec` per Noise fragment.
pub(super) fn wire_frames(transport: &mut snow::TransportState, payload: &[u8]) -> Vec<Vec<u8>> {
    // A payload of N bytes travels as ceil(N / 65518) fragments; only the last
    // one clears the continuation flag, and that flag rides INSIDE the
    // ciphertext so the receiver's reassembly decision rests on authenticated
    // plaintext rather than the unauthenticated wire prefix.
    let fragment_count = payload.chunks(65518).count();
    let mut frames = Vec::with_capacity(fragment_count);
    for (index, chunk) in payload.chunks(65518).enumerate() {
        let mut plaintext = Vec::with_capacity(1 + chunk.len());
        plaintext.push(u8::from(index + 1 < fragment_count));
        plaintext.extend_from_slice(chunk);
        // A Noise ciphertext is the plaintext plus the 16-byte GCM tag, so the
        // output buffer is exactly the size a successful write fills. A failed
        // write (a state that is not in transport mode, an exhausted nonce
        // space) leaves the buffer untouched and reports zero bytes; truncating
        // to that emits a frame the peer refuses outright as below its
        // tag-plus-header minimum, instead of one it would block on waiting for
        // bytes that never arrive.
        let mut ciphertext = vec![0u8; plaintext.len() + 16];
        let written = transport
            .write_message(&plaintext, &mut ciphertext)
            .unwrap_or_default();
        ciphertext.truncate(written);
        // Snow caps a fragment's ciphertext at 65535 bytes, so this conversion
        // is total; `unwrap_or_default` spells an infallible conversion without
        // a crash surface (see AGENTS.md's error-handling rules).
        let length = u32::try_from(ciphertext.len()).unwrap_or_default();
        let mut frame = Vec::with_capacity(4 + ciphertext.len());
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&ciphertext);
        frames.push(frame);
    }
    frames
}
