//! The trust and enrollment preflight, run BEFORE committing to a session.
//!
//! These helpers let a front-end establish that it can talk to a daemon, and
//! that the daemon will admit it, without opening a real session: [`probe_server_key`]
//! learns a first-contact server's static key, [`verify_daemon_authorization`]
//! proves the daemon will admit this client, and [`own_transport_pubkey`] reads
//! out this client's own identity for enrollment remediation. They share the
//! handshake-failure classification with the live connection path (see
//! `super::handshake`), so the advice a preflight gives cannot drift from what a
//! real connection would report.

use crate::error::ClientError;
use choreo_transport::error::TransportError;
use choreo_transport::handshake::{handshake_initiator, handshake_initiator_xx};
use tracing::{debug, info};

use super::handshake::{client_transport_keypair, handshake_error, is_handshake_refusal};
use super::tcp::dial_control_tcp;

/// Probe a daemon's static public key without establishing a session.
///
/// Performs the XX first-contact handshake ONLY: connect (the initiator writes
/// the mode preamble itself), the three handshake messages, extract the learned
/// server key, and DROP the stream — no data-plane message is ever sent or
/// received. This is the building block UIs use to implement the trust flow
/// synchronously (in a normal-mode terminal, before any TUI/GUI starts): learn
/// the key, show the base64, get the human's confirmation, pin, and only then
/// open the real connection with [`super::ConnectionMode::Tcp`] / IK.
///
/// The probe itself authenticates NOTHING about the server (there is no pin
/// yet — that is the point), and it does **not** test enrollment either: the XX
/// initiator completes its side of the exchange after sending message 3, before
/// the responder runs its ACL check, so a client the daemon would reject still
/// gets its key back from here. What this returns is "a Noise responder holding
/// the key the daemon prints answered at this address" — reachability plus the
/// key to compare out of band. Use [`verify_daemon_authorization`] (IK) to find
/// out whether the daemon will actually admit this client.
///
/// # Errors
///
/// Returns [`ClientError`] on connect/handshake/I/O failures while probing.
pub fn probe_server_key(addr: &str) -> Result<[u8; 32], ClientError> {
    info!("probing server key at {addr} (XX first contact)");

    let (client_sk, _client_pk) = client_transport_keypair()?;

    let tcp = dial_control_tcp(addr).map_err(ClientError::Io)?;
    // The XX initiator declares the first-contact mode itself (see
    // `handshake_initiator_xx`).
    let (noise, server_pk) =
        handshake_initiator_xx(tcp, client_sk.as_bytes()).map_err(handshake_error)?;

    // Drop the established transport immediately: the probe never speaks
    // the protocol. Closing here is also what keeps the daemon's connection
    // slot usage bounded — the real connection opens fresh afterwards.
    drop(noise);
    debug!(addr, "server key probe complete; transport dropped");
    Ok(server_pk)
}

/// Why a preflight authorization check failed (see
/// [`verify_daemon_authorization`]). The two cases need DIFFERENT
/// remediation — "start the daemon / check the network" vs "get your key
/// enrolled" — so they are distinguished at the type level instead of by
/// string-matching an `io::ErrorKind` (a dial refusal and a handshake
/// rejection both involve connection-level I/O and must not be conflated).
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    /// The daemon could not be reached at all (dial failure).
    #[error("cannot reach the daemon: {0}")]
    Unreachable(#[source] std::io::Error),
    /// This client's own transport identity could not be loaded or generated, so
    /// the daemon was never dialed. Reported apart from [`Self::Unreachable`]
    /// because the remediation is local — a config directory this process can
    /// read and write — not a network or daemon problem.
    #[error("cannot load this client's transport keypair: {0}")]
    Identity(#[source] std::io::Error),
    /// The daemon was reached but the Noise IK handshake was REFUSED: the
    /// daemon either does not hold the expected server key, or its ACL
    /// did not admit this client's transport key. (An IK handshake is the
    /// ONLY probe that can detect the ACL rejection — the daemon aborts
    /// the handshake before message 2 — which is why this preflight runs
    /// IK even on a first-contact flow that already probed with XX.)
    ///
    /// A handshake that merely failed to complete without a refusal — a
    /// timeout, a socket error other than a peer close — is
    /// [`Self::Unreachable`] instead: it says nothing about enrollment, so
    /// presenting it as a rejection would send the user after the wrong fix.
    /// (At an initiator the refusal is the handshake read hitting EOF, because
    /// the responder aborts the exchange rather than sending a rejection — see
    /// `handshake::is_handshake_refusal`.)
    #[error("the daemon rejected the connection handshake: {0}")]
    Rejected(#[source] TransportError),
}

/// The client's OWN Noise transport public key (generating the on-disk
/// keypair first if absent). This is the identity the daemon's ACL judges:
/// UIs embed it (in base64) in enrollment-remediation messages so
/// a TUI-only user never needs the daemon binary just to read out their key.
///
/// # Errors
///
/// Returns [`ClientError`] if the local transport keypair cannot be loaded
/// or generated.
pub fn own_transport_pubkey() -> Result<[u8; 32], ClientError> {
    let (_sk, pk) = client_transport_keypair()?;
    Ok(pk)
}

/// Verify that the daemon at `addr` will actually admit this client BEFORE
/// the caller commits to a full session (the TUI runs this before starting
/// any UI).
///
/// Dials `addr`, runs the complete Noise IK handshake against `server_pk`,
/// and immediately drops the established transport — no protocol message is
/// ever sent. A successful IK handshake proves BOTH properties at once:
/// the daemon holds the expected static key (the handshake authenticates
/// it) AND the daemon's ACL admitted this client's key (the responder
/// checks the ACL mid-handshake and closes the connection before completing
/// it when the client is not enrolled). No other check can detect the
/// enrollment case: the XX first-contact probe completes client-side before
/// the daemon's ACL check runs, so it always "succeeds" for un-enrolled
/// clients.
///
/// The cost is one extra handshake per connect — negligible against the
/// session it gates, and it converts "TUI starts, then dies with a cryptic
/// I/O error on first use" into a clear refusal before any UI exists.
///
/// # Errors
///
/// Returns [`PreflightError::Identity`] if this client's transport keypair
/// cannot be loaded or generated, [`PreflightError::Unreachable`] if the dial
/// fails or the handshake fails without a refusal, or
/// [`PreflightError::Rejected`] if the IK handshake is REFUSED — which means the
/// daemon does not hold `server_pk`, or its ACL did not admit this client.
///
/// Only a refusal produces `Rejected`, because that is the case whose
/// remediation is "get this client's key enrolled". A handshake timeout or a
/// broken socket is `Unreachable`, whose remediation is to check the network and
/// the daemon.
pub fn verify_daemon_authorization(addr: &str, server_pk: &[u8; 32]) -> Result<(), PreflightError> {
    info!(
        addr,
        "authorization preflight: probing daemon with IK handshake"
    );

    let (client_sk, _client_pk) = client_transport_keypair().map_err(PreflightError::Identity)?;

    // Dial failure = the daemon is down / unreachable — reported as-is so
    // the caller's message can point at the network, not at enrollment.
    let tcp = dial_control_tcp(addr).map_err(PreflightError::Unreachable)?;

    // Only a REFUSAL (wrong server key, or this client not enrolled in the
    // daemon's ACL) is `Rejected`; the caller renders the enrollment
    // remediation for that and nothing else. A timeout or a broken socket is
    // `Unreachable`: it says nothing about the ACL, and treating it as a
    // rejection would send the user to re-enroll a key the daemon never judged.
    // The IK initiator writes the mode preamble as its first step (see
    // `handshake_initiator`).
    let noise = handshake_initiator(tcp, client_sk.as_bytes(), server_pk).map_err(|error| {
        if is_handshake_refusal(&error) {
            PreflightError::Rejected(error)
        } else {
            PreflightError::Unreachable(std::io::Error::other(error))
        }
    })?;

    // The handshake succeeded — the daemon will admit us. Drop the
    // transport; the real session opens fresh (the daemon cleans the
    // preflight connection up through its normal disconnect path).
    drop(noise);
    debug!(addr, "authorization preflight passed");
    Ok(())
}
