use choreo_proto::{ClientMessage, DaemonMessage, decode_frame, encode_payload};
use snow::TransportState;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use tracing::{debug, info};

use crate::error::TransportError;

/// An encrypted Noise IK transport stream.
///
/// Wraps a TcpStream with a snow TransportState. Every message is
/// length-prefixed (4-byte BE u32) and encrypted with AES-256-GCM.
///
/// The TransportState is behind an Arc<Mutex<>> so the stream can be
/// cloned for concurrent reader/writer threads via try_clone().
pub struct NoiseStream {
    tcp: TcpStream,
    transport: Arc<Mutex<TransportState>>,
}

impl NoiseStream {
    /// Create a new NoiseStream wrapping a TcpStream and TransportState.
    pub fn new(tcp: TcpStream, transport: TransportState) -> Self {
        NoiseStream {
            tcp,
            transport: Arc::new(Mutex::new(transport)),
        }
    }

    /// Clone the stream for concurrent reader/writer threads.
    /// The underlying TcpStream is cloned; the TransportState is shared
    /// via Arc<Mutex<>> so encrypt/decrypt calls are serialized.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(NoiseStream {
            tcp: self.tcp.try_clone()?,
            transport: Arc::clone(&self.transport),
        })
    }

    /// Send an encrypted message.
    ///
    /// Encrypts the plaintext into a separate ciphertext buffer (leaving
    /// the input untouched), then writes a 4-byte big-endian length prefix
    /// followed by the ciphertext.
    pub fn send_message(&mut self, plaintext: &[u8]) -> Result<(), TransportError> {
        let mut buf = vec![0u8; plaintext.len() + 16];
        let n = self
            .transport
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_message(plaintext, &mut buf)?;
        let len = (n as u32).to_be_bytes();
        self.tcp.write_all(&len)?;
        self.tcp.write_all(&buf[..n])?;
        Ok(())
    }

    /// Receive a decrypted message.
    ///
    /// Reads the 4-byte length prefix, then reads exactly that many
    /// bytes of ciphertext, decrypts into a plaintext buffer, and returns
    /// the plaintext.
    pub fn recv_message(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut len_buf = [0u8; 4];
        self.tcp.read_exact(&mut len_buf)?;
        let ct_len = u32::from_be_bytes(len_buf) as usize;
        let mut ct_buf = vec![0u8; ct_len];
        self.tcp.read_exact(&mut ct_buf)?;
        let mut pt_buf = vec![0u8; ct_len];
        let n = self
            .transport
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .read_message(&ct_buf, &mut pt_buf)?;
        pt_buf.truncate(n);
        Ok(pt_buf)
    }

    /// Send a typed ClientMessage (convenience for clients).
    pub fn send_client_message(&mut self, msg: &ClientMessage) -> Result<(), TransportError> {
        let payload = encode_payload(msg)?;
        self.send_message(&payload)
    }

    /// Receive a typed DaemonMessage (convenience for clients).
    pub fn recv_daemon_message(&mut self) -> Result<DaemonMessage, TransportError> {
        let payload = self.recv_message()?;
        Ok(decode_frame(&payload)?)
    }

    /// Send a typed DaemonMessage (convenience for servers).
    pub fn send_daemon_message(&mut self, msg: &DaemonMessage) -> Result<(), TransportError> {
        let payload = encode_payload(msg)?;
        self.send_message(&payload)
    }

    /// Receive a typed ClientMessage (convenience for servers).
    pub fn recv_client_message(&mut self) -> Result<ClientMessage, TransportError> {
        let payload = self.recv_message()?;
        Ok(decode_frame(&payload)?)
    }

    /// Access the underlying TcpStream for shutdown etc.
    pub fn get_ref(&self) -> &TcpStream {
        &self.tcp
    }
}

/// Perform the Noise IK handshake as the **initiator** (client side).
///
/// * `stream` — the already-connected TCP stream.
/// * `static_sk` — the client's transport.sec (32-byte X25519 secret key).
/// * `server_pk` — the server's transport.pub (32-byte X25519 public key).
///
/// On success returns a `NoiseStream` ready for encrypted message I/O.
pub fn handshake_initiator(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    server_pk: &[u8; 32],
) -> Result<NoiseStream, TransportError> {
    use snow::Builder;

    let mut handshake = Builder::new("Noise_IK_25519_AESGCM_SHA256".parse()?)
        .local_private_key(static_sk)?
        .remote_public_key(server_pk)?
        .build_initiator()?;

    // Write first handshake message (e, encrypted s)
    let mut buf = vec![0u8; 1024];
    let n = handshake.write_message(&[], &mut buf)?;
    // Use 2-byte length prefix for handshake messages (max size < 65535)
    let len_bytes = (n as u16).to_be_bytes();
    stream.write_all(&len_bytes)?;
    stream.write_all(&buf[..n])?;

    // Read second handshake message (e, encrypted empty)
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u16::from_be_bytes(len_buf) as usize;
    let mut rbuf = vec![0u8; msg_len];
    stream.read_exact(&mut rbuf)?;
    handshake.read_message(&rbuf, &mut buf)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise IK handshake complete (initiator)");
    Ok(NoiseStream::new(stream, transport))
}

/// Perform the Noise IK handshake as the **responder** (server side).
///
/// * `stream` — the accepted TCP stream from the listener.
/// * `static_sk` — the server's transport.sec (32-byte X25519 secret key).
/// * `check_client` — a closure that receives the client's 32-byte static
///   public key (extracted from the handshake) and returns `true` if the
///   client is authorized.
///
/// On success returns a `NoiseStream` ready for encrypted message I/O.
/// On authentication failure the connection is closed and `Err(AuthFailed)` is returned.
pub fn handshake_responder<F>(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    use snow::Builder;

    let mut handshake = Builder::new("Noise_IK_25519_AESGCM_SHA256".parse()?)
        .local_private_key(static_sk)?
        .build_responder()?;

    // Read first handshake message from client (e, encrypted s)
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u16::from_be_bytes(len_buf) as usize;
    let mut rbuf = vec![0u8; msg_len];
    stream.read_exact(&mut rbuf)?;
    let mut out_buf = vec![0u8; 1024];
    handshake.read_message(&rbuf, &mut out_buf)?;

    // Extract client's static public key for ACL check.
    if let Some(client_pk) = handshake.get_remote_static() {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(client_pk);
        if !check_client(&pk) {
            info!("Noise IK handshake rejected: client not in ACL");
            return Err(TransportError::AuthFailed);
        }
    } else {
        info!("Noise IK handshake rejected: client did not send static key");
        return Err(TransportError::AuthFailed);
    }

    // Write second handshake message (e, encrypted empty)
    let mut buf = vec![0u8; 1024];
    let n = handshake.write_message(&[], &mut buf)?;
    let len_bytes = (n as u16).to_be_bytes();
    stream.write_all(&len_bytes)?;
    stream.write_all(&buf[..n])?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise IK handshake complete (responder)");
    Ok(NoiseStream::new(stream, transport))
}
