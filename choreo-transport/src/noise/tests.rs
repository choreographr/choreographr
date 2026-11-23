//! Tests for the encrypted stream: the per-direction single-actor slots, the
//! post-decode wipe allowlist, and the cipher's own authentication.
//!
//! All in-memory — no sockets, no threads, no timers.

use x25519_dalek::{PublicKey, StaticSecret};

use super::slot::SingleSlot;

/// A slot is exclusive while a call holds it and free again the moment the
/// guard drops — the invariant that keeps two senders (or two readers) from
/// interleaving fragments. Pure state, no socket, no thread, no timer.
#[test]
fn a_slot_is_exclusive_until_the_guard_drops() {
    let slot = SingleSlot::new("send_message");

    let first = slot.acquire().expect("a free slot must be claimable");
    let refusal = slot.acquire();
    let Err(err) = refusal else {
        panic!("a second caller must be refused while the first holds the slot");
    };
    assert!(
        err.to_string().contains("concurrent send_message"),
        "the refusal must name the entry point it guards: {err}"
    );

    // Dropping the guard frees the slot, so the stream is usable again after a
    // completed (or failed) call.
    drop(first);
    let second = slot
        .acquire()
        .expect("the slot must be free after the drop");
    drop(second);
    slot.acquire().expect("and free again after a second drop");
}

/// Clones share one slot: a `try_clone` handle is the same connection, so two
/// clones must contend for the same single-actor right rather than each getting
/// its own. Both directions behave identically — the message differs only in the
/// entry point it names.
#[test]
fn cloned_slots_share_one_right_per_direction() {
    for what in ["send_message", "recv_message"] {
        let slot = SingleSlot::new(what);
        let clone = slot.clone();

        let held = slot.acquire().expect("claim on the original");
        let refused = clone.acquire();
        let Err(err) = refused else {
            panic!("a clone must see the original's claim");
        };
        assert!(err.to_string().contains(what), "the refusal names {what}");
        drop(held);
        clone.acquire().expect("the clone must see the release");
    }
}

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
        !super::typed::daemon_message_may_carry_secret(&image),
        "Image carries raw attachment bytes and is the one exempt bulk variant"
    );

    // A credential-bearing reply is always wiped...
    let credential = DaemonMessage::broadcast(DaemonMessageType::Credential {
        service: "anthropic".to_string(),
        key: Some("sk-secret".to_string()),
    });
    assert!(super::typed::daemon_message_may_carry_secret(&credential));

    // ...and so is an arbitrary unlisted variant (the fail-safe default).
    let pong = DaemonMessage::broadcast(DaemonMessageType::Pong);
    assert!(
        super::typed::daemon_message_may_carry_secret(&pong),
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
    use crate::handshake::{HandshakeMode, Role, build};

    let client_secret = StaticSecret::random_from_rng(&mut rand::rng());
    let server_secret = StaticSecret::random_from_rng(&mut rand::rng());
    let client_sk = client_secret.to_bytes();
    let server_sk = server_secret.to_bytes();
    let server_pk = PublicKey::from(&server_secret).to_bytes();

    // Build BOTH halves through the production constructor: same pattern, same
    // key roles (client = initiator, which knows the server's public key;
    // server = responder) and the same prologue the mode derives. The prologue
    // feeds the handshake hash the transport keys come from, so a hand-rolled
    // builder here could derive states a production peer cannot interop with
    // while still passing this test.
    let mode = HandshakeMode::Ik;
    let mut initiator = build(mode, &client_sk, Some(&server_pk), Role::Initiator).expect("build");
    let mut responder = build(mode, &server_sk, None, Role::Responder).expect("build");

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
