//! The control-plane socket policy: Nagle (`TCP_NODELAY`) is disabled and TCP
//! keepalive is armed on the client↔daemon TCP sockets — by the standalone
//! helper, and by the handshake that hands the socket to the data plane.

use choreo_transport::handshake::{handshake_initiator, handshake_responder};
use choreo_transport::socket::tune_control_socket;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

#[cfg(unix)]
use std::os::fd::AsFd;

use crate::common::noise_test_pair;

/// Whether `SO_KEEPALIVE` is set on a connected TCP socket.
///
/// Unix-only: the option has no stable std accessor, and the Windows path needs
/// Winsock FFI the test has no business carrying (the production setter,
/// `choreo_sockreg::SocketTuning`, already covers both platforms and has its own
/// cross-platform suite).
///
/// The query's own error is returned rather than folded into `false`: a plain
/// helper in a `tests/` module is outside clippy's test allowances (see AGENTS.md
/// → Test Discipline), so it must not panic, and a `Result` additionally keeps a
/// failed `getsockopt` distinguishable from an option that is genuinely off — the
/// `#[test]` caller is where the assertion belongs.
#[cfg(unix)]
fn keepalive_enabled(stream: &TcpStream) -> Result<bool, String> {
    nix::sys::socket::getsockopt(&stream.as_fd(), nix::sys::socket::sockopt::KeepAlive)
        .map_err(|e| format!("query SO_KEEPALIVE: {e}"))
}

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

/// The same helper arms keepalive, which is the connection's **liveness** bound:
/// it is what reaps a peer that vanished without closing (a phone that slept, a
/// network that changed) and so what releases the daemon's connection slot. It is
/// not an activity timeout — probes only fire on an idle connection, so a
/// slow-but-alive transfer is never affected.
#[test]
#[ignore = "integration"]
#[cfg(unix)]
fn tune_control_socket_arms_keepalive_on_both_ends() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        tune_control_socket(&stream);
        keepalive_enabled(&stream)
    });

    let stream = TcpStream::connect(addr).expect("connect");
    // Untuned sockets do not carry the option, so the assertion below is proving
    // the helper's effect rather than a platform default.
    tune_control_socket(&stream);
    assert!(
        keepalive_enabled(&stream).expect("query client SO_KEEPALIVE"),
        "the client socket must have SO_KEEPALIVE armed"
    );
    assert!(
        server
            .join()
            .expect("server thread")
            .expect("query server SO_KEEPALIVE"),
        "the server socket must have SO_KEEPALIVE armed"
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
