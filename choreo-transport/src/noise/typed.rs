//! The typed, protocol-coupled conveniences on top of [`NoiseStream`].
//!
//! These methods are the only part of the transport that names a `choreo_proto`
//! wire type: they encode a `ClientMessage`/`DaemonMessage` to bytes and hand
//! them to the stream, or decode bytes the stream returned. Keeping them apart
//! from the stream core (which moves opaque byte frames) means the core carries
//! no dependency on the wire schema — the framing in `send`/`recv`/`frame` is
//! true of any payload, and only this module knows what the payload means.
//!
//! They are inherent `impl NoiseStream` methods, so call sites and their
//! rustdoc links are unchanged by the move.

use choreo_proto::{ClientMessage, DaemonMessage, DaemonMessageType, decode_frame, encode_payload};
use zeroize::{Zeroize, Zeroizing};

use super::NoiseStream;
use crate::error::TransportError;

impl NoiseStream {
    /// Send a typed `ClientMessage` (convenience for clients).
    ///
    /// # Errors
    ///
    /// Returns any error propagated by [`Self::send_message`] or the payload
    /// encoding.
    pub fn send_client_message(&mut self, msg: &ClientMessage) -> Result<(), TransportError> {
        let payload: Zeroizing<Vec<u8>> = encode_payload(msg)?;
        self.send_message(&payload)
    }

    /// Receive a typed `DaemonMessage` (convenience for clients).
    ///
    /// The raw frame buffer is wiped after decoding UNLESS the decoded variant
    /// is one the daemon can only ever send in cleartext-equivalent bulk data
    /// (see `daemon_message_may_carry_secret`): [`DaemonMessageType::Image`]
    /// carries raw attachment bytes up to the 64 MiB frame cap, so exempting it
    /// avoids a pointless 64 MiB memset on the largest payload the daemon sends,
    /// while every credential-bearing variant (`Credential`, `Keystore`, …) and
    /// every variant not on the allowlist is still wiped. A decode failure is
    /// unknown content and is wiped too.
    ///
    /// The wipe covers the raw frame buffer ONLY. Once `decode_frame` succeeds,
    /// the decoded `DaemonMessage` owns its own `String`/`Vec` fields (a
    /// credential's `String`, for instance), and those are dropped un-erased —
    /// the exemption/wiping policy here says nothing about them. Closing that gap
    /// would mean zeroizing the wire types themselves in `choreo-proto`, so the
    /// decoded value's owned buffers are explicitly out of scope on this path.
    ///
    /// # Errors
    ///
    /// Returns any error propagated by `recv_message_plain` or the
    /// payload decoding.
    pub fn recv_daemon_message(&mut self) -> Result<DaemonMessage, TransportError> {
        let mut payload = self.recv_message_plain()?;
        let decoded = decode_frame(&payload);
        // Wipe the frame unless it decoded to a known non-secret bulk variant;
        // the guard is an allowlist, so unknown content (a decode error, a
        // future variant) is wiped rather than skipped.
        if !matches!(&decoded, Ok(msg) if !daemon_message_may_carry_secret(msg)) {
            payload.zeroize();
        }
        Ok(decoded?)
    }

    /// Send a typed `DaemonMessage` (convenience for servers).
    ///
    /// # Errors
    ///
    /// Returns any error propagated by [`Self::send_message`] or the payload
    /// encoding.
    pub fn send_daemon_message(&mut self, msg: &DaemonMessage) -> Result<(), TransportError> {
        let payload: Zeroizing<Vec<u8>> = encode_payload(msg)?;
        self.send_message(&payload)
    }

    /// Receive a typed `ClientMessage` (convenience for servers).
    ///
    /// # Errors
    ///
    /// Returns any error propagated by [`Self::recv_message`] or the payload
    /// decoding.
    pub fn recv_client_message(&mut self) -> Result<ClientMessage, TransportError> {
        let payload = self.recv_message()?;
        Ok(decode_frame(&payload)?)
    }
}

/// Whether an inbound [`DaemonMessage`] may carry credential material, and so
/// must have its raw frame wiped after decoding.
///
/// This is the ONE place the receive path decides to skip the wipe, and it is a
/// **fail-safe allowlist**: only a variant whose payload is structurally
/// incapable of holding a secret is listed false, and anything else — including
/// every future variant and a frame that failed to decode — is treated as
/// secret and wiped. A missed entry therefore only costs the memset it was
/// trying to save, never a leaked credential.
///
/// [`DaemonMessageType::Image`] is the only exemption: it carries raw attachment
/// bytes (`Option<Vec<u8>>`) and is the largest payload the daemon sends (up to
/// the 64 MiB frame cap), so skipping its wipe turns the per-message zeroize
/// from a 64 MiB memset into nothing for the one bulk, provably non-secret case.
/// Every credential-bearing variant (`Credential`, `Keystore`, `Unlock`-adjacent
/// state, …) falls through to `true` and is wiped.
pub(super) fn daemon_message_may_carry_secret(msg: &DaemonMessage) -> bool {
    !matches!(&msg.inner, DaemonMessageType::Image { .. })
}
