//! Tests for the encrypted stream: the post-decode wipe allowlist and the
//! cipher's own authentication.
//!
//! Both are in-memory — no sockets, no threads, no timers.

use snow::Builder;
use x25519_dalek::{PublicKey, StaticSecret};

/// The wipe exemption is a fail-safe allowlist: `Image` (raw attachment
/// bytes) is the only variant that skips the wipe, while a credential-bearing
/// variant and any unlisted variant are treated as secret. A memory-safety
/// regression here would be a silent credential leak, so it is pinned.
#[test]
fn only_image_is_exempt_from_the_post_decode_wipe() {
    use choreo_proto::{DaemonMessage, DaemonMessageType, ImageKey};

    let image = DaemonMessage::broadcast(DaemonMessageType::Image {
        session_id: 1,
        turn_id: 1,
        key: ImageKey::Displayed { index: 0 },
        data: Some(vec![0u8; 8]),
    });
    assert!(
        !super::daemon_message_may_carry_secret(&image),
        "Image carries raw attachment bytes and is the one exempt bulk variant"
    );

    // A credential-bearing reply is always wiped...
    let credential = DaemonMessage::broadcast(DaemonMessageType::Credential {
        service: "anthropic".to_string(),
        key: Some("sk-secret".to_string()),
    });
    assert!(super::daemon_message_may_carry_secret(&credential));

    // ...and so is an arbitrary unlisted variant (the fail-safe default).
    let pong = DaemonMessage::broadcast(DaemonMessageType::Pong);
    assert!(
        super::daemon_message_may_carry_secret(&pong),
        "an unlisted variant must default to being wiped"
    );
}

/// In-memory crypto test (no sockets, no sleeps): prove that flipping a
/// single ciphertext byte makes GCM decryption fail. The flipped byte is
/// in the *payload* region of the ciphertext — the trailing 16 bytes are
/// the AES-GCM auth tag — so this proves the tag authenticates the
/// ciphertext itself, not just the tag.
#[test]
fn transport_state_rejects_tampered_ciphertext() {
    let client_secret = StaticSecret::random_from_rng(&mut rand::rng());
    let server_secret = StaticSecret::random_from_rng(&mut rand::rng());
    let client_sk = client_secret.to_bytes();
    let server_sk = server_secret.to_bytes();
    let server_pk = PublicKey::from(&server_secret).to_bytes();

    // Mirror the production handshake (handshake_initiator /
    // handshake_responder) exactly: same pattern string, same key roles
    // (client = initiator with server's public key, server = responder),
    // and the same prologue both sides of an IK handshake derive from
    // HandshakeMode::Ik — the prologue feeds the handshake hash the
    // transport keys come from, so omitting it would derive states a
    // production peer could not interop with.
    let mode = crate::handshake::HandshakeMode::Ik;
    let prologue = mode.prologue();
    // The pattern comes from the mode, never a copy: a hand-typed string
    // here would keep validating a pattern production no longer uses.
    let mut initiator = Builder::new(mode.pattern().parse().unwrap())
        .local_private_key(&client_sk)
        .unwrap()
        .remote_public_key(&server_pk)
        .unwrap()
        .prologue(&prologue)
        .unwrap()
        .build_initiator()
        .unwrap();
    let mut responder = Builder::new(mode.pattern().parse().unwrap())
        .local_private_key(&server_sk)
        .unwrap()
        .prologue(&prologue)
        .unwrap()
        .build_responder()
        .unwrap();

    // Message 1: initiator -> responder (ephemeral key + encrypted
    // static key). The plaintext is empty; the point is just to run the
    // same message order the wire protocol uses.
    let mut msg1 = vec![0u8; 1024];
    let n1 = initiator.write_message(&[], &mut msg1).unwrap();
    let mut scratch = vec![0u8; 1024];
    responder.read_message(&msg1[..n1], &mut scratch).unwrap();

    // Message 2: responder -> initiator (ephemeral key, empty payload).
    let mut msg2 = vec![0u8; 1024];
    let n2 = responder.write_message(&[], &mut msg2).unwrap();
    let mut scratch2 = vec![0u8; 1024];
    initiator.read_message(&msg2[..n2], &mut scratch2).unwrap();

    // Both sides now derive identical TransportStates (same key schedule,
    // same nonce counters starting at 0).
    let mut client_ts = initiator.into_transport_mode().unwrap();
    let mut server_ts = responder.into_transport_mode().unwrap();

    let plaintext = b"attack at dawn";

    // Sanity check first: an untampered message round-trips, proving the
    // two transport states were actually derived from the same handshake.
    let mut ct = vec![0u8; plaintext.len() + 16];
    let ct_len = client_ts.write_message(plaintext, &mut ct).unwrap();
    let mut pt = vec![0u8; plaintext.len() + 16];
    let pt_len = server_ts.read_message(&ct[..ct_len], &mut pt).unwrap();
    assert_eq!(&pt[..pt_len], plaintext);

    // Now encrypt a fresh message and flip one byte at index 4. The
    // plaintext is 14 bytes, so index 4 is deep inside the payload
    // region (not the 16-byte tag). GCM authenticates every ciphertext
    // byte, so read_message must return an error.
    let mut tampered = vec![0u8; plaintext.len() + 16];
    let tampered_len = client_ts.write_message(plaintext, &mut tampered).unwrap();
    tampered[4] ^= 0xff;
    let mut bad_pt = vec![0u8; plaintext.len() + 16];
    let result = server_ts.read_message(&tampered[..tampered_len], &mut bad_pt);
    assert!(
        result.is_err(),
        "tampered ciphertext must be rejected by GCM authentication"
    );
}
