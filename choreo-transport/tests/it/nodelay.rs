//! The control-plane socket policy: Nagle (`TCP_NODELAY`) is disabled on the
//! client↔daemon TCP sockets — by the standalone helper, and by the handshake
//! that hands the socket to the data plane.

use choreo_transport::handshake::{handshake_initiator, handshake_responder};
use choreo_transport::socket::tune_control_socket;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

use crate::common::noise_test_pair;

/// The helper flips `TCP_NODELAY` on each socket independently (the option is
/// per-socket), which is what the client dial and daemon accept rely on to keep
/// the handshake's small round trips off the Nagle/ACK timer.
#[test]
#[ignore = "integration"]
fn tune_control_socket_enables_nodelay_on_both_ends() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        tune_control_socket(&stream);
        stream.nodelay().expect("query nodelay on server")
    });

    let stream = TcpStream::connect(addr).expect("connect");
    tune_control_socket(&stream);
    assert!(
        stream.nodelay().expect("query nodelay on client"),
        "the client socket must run with Nagle disabled"
    );
    assert!(
        server.join().expect("server thread"),
        "the server socket must run with Nagle disabled"
    );
}

/// A completed handshake hands the socket to the data plane with Nagle already
/// disabled on BOTH ends — the intrinsic guarantee `finish` provides even when a
/// caller never tuned the raw socket (this test dials without pre-tuning).
#[test]
#[ignore = "integration"]
fn noise_handshake_sets_nodelay_on_both_ends() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk, |pk| *pk == client_pk);
        tx.send(result.map(|noise| noise.get_ref().nodelay()))
            .expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");
    assert!(
        client.get_ref().nodelay().expect("query client nodelay"),
        "the client data plane must run with Nagle disabled"
    );

    let server = rx
        .recv()
        .expect("recv server result")
        .expect("server handshake");
    assert!(
        server.expect("query server nodelay"),
        "the server data plane must run with Nagle disabled"
    );
}
