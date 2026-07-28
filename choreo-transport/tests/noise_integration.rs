use choreo_transport::noise::{handshake_initiator, handshake_responder};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use x25519_dalek::{PublicKey, StaticSecret};

/// Test full Noise IK handshake between client and server.
#[test]
#[ignore]
fn noise_ik_handshake_round_trip() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_pk = PublicKey::from(&server_sk);
    let server_sk_bytes = server_sk.to_bytes();
    let server_pk_bytes = server_pk.to_bytes();

    let client_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let client_pk = PublicKey::from(&client_sk);
    let client_sk_bytes = client_sk.to_bytes();
    let client_pk_bytes = client_pk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    let server_sk = server_sk_bytes;
    let client_pk_ref = client_pk_bytes;
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk, |pk| *pk == client_pk_ref);
        tx.send(result.map(|_| ())).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let result = handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes);
    assert!(
        result.is_ok(),
        "client handshake should succeed: {:?}",
        result.err()
    );

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "server handshake should succeed: {:?}",
        server_result.err()
    );
}

/// Test that ACL rejection works.
#[test]
#[ignore]
fn noise_ik_handshake_rejects_unknown_client() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_pk = PublicKey::from(&server_sk);
    let server_sk_bytes = server_sk.to_bytes();
    let server_pk_bytes = server_pk.to_bytes();

    let client_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let client_sk_bytes = client_sk.to_bytes();

    let wrong_pk = PublicKey::from(&StaticSecret::random_from_rng(&mut rand::rng()));

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk_bytes, |pk| *pk == wrong_pk.to_bytes());
        tx.send(result.map(|_| ())).expect("send");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let client_result = handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes);
    assert!(
        client_result.is_err(),
        "client handshake should fail when server rejects"
    );

    let server_result = rx.recv().expect("recv");
    assert!(
        server_result.is_err(),
        "server should reject unknown client"
    );
}
