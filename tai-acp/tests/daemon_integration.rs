use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::thread;

use tai_acp::daemon_client::{Event, spawn_daemon_io};
use tai_proto::{ClientMessage, DaemonMessage, read_message, write_message};

/// Create a temporary directory and return a unique socket path within it.
fn temp_socket_path() -> (std::path::PathBuf, std::path::PathBuf) {
    let base = std::env::temp_dir().join(format!("tai-acp-integration-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&base);
    let sock = base.join("daemon.sock");
    (base, sock)
}

/// A simple fake daemon that accepts one connection, reads `ListModels`,
/// replies with a `Models` message, then echoes back any `DaemonMessage` it
/// receives as an `OutputChunk`.
#[test]
#[ignore]
fn daemon_io_send_and_receive() {
    let (_dir, socket_path) = temp_socket_path();

    // ------------------------------------------------------------------
    // Fake daemon: listen then accept one connection.
    // ------------------------------------------------------------------
    let listener = UnixListener::bind(&socket_path).unwrap();
    let daemon_ready: mpsc::Receiver<()>;
    let (daemon_done_tx, daemon_done_rx) = mpsc::channel::<()>();

    {
        let (ready_tx, ready_rx) = mpsc::channel();
        daemon_ready = ready_rx;

        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = BufWriter::new(stream);

            ready_tx.send(()).unwrap();

            // Read the first message from tai-acp.
            let msg: ClientMessage = read_message(&mut reader).unwrap();
            assert!(matches!(msg, ClientMessage::ListModels));

            // Respond with Models to confirm the handshake.
            let response = DaemonMessage::Models {
                models: vec!["claude-4".into(), "gpt-5".into()],
                selected_model: Some("claude-4".into()),
            };
            write_message(&mut writer, &response).unwrap();
            writer.flush().unwrap();

            // Read a second message and echo the request_id as OutputChunk.
            let msg2: ClientMessage = read_message(&mut reader).unwrap();
            let request_id = match &msg2 {
                ClientMessage::RunInput { request_id, .. } => *request_id,
                _ => panic!("expected RunInput, got {msg2:?}"),
            };

            let echo = DaemonMessage::OutputChunk {
                request_id,
                stream: tai_proto::OutputStream::Answer,
                data: b"echo".to_vec(),
            };
            write_message(&mut writer, &echo).unwrap();
            writer.flush().unwrap();

            // Send Done to terminate the stream.
            let done = DaemonMessage::Done {
                request_id,
                token_usage: Some(tai_proto::TokenUsage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 8,
                }),
                last_prompt_tokens: None,
            };
            write_message(&mut writer, &done).unwrap();
            writer.flush().unwrap();

            // Signal that the daemon has sent all its messages.
            let _ = daemon_done_tx.send(());
        });
    }

    // ------------------------------------------------------------------
    // tai-acp daemon client connecting to the fake daemon.
    // ------------------------------------------------------------------
    let (event_tx, event_rx) = mpsc::channel::<Event>();
    let (client, writer_handle) = spawn_daemon_io(socket_path.to_str().unwrap(), event_tx).unwrap();

    // Wait for the fake daemon to be ready.
    daemon_ready.recv().unwrap();

    // Send ListModels (this matches what dispatch_new_session does).
    client.writer_tx.send(ClientMessage::ListModels).unwrap();

    // Receive the Models response (deterministic: the fake daemon
    // sends synchronously before we reach this point).
    let models_event = event_rx.recv().unwrap();
    match models_event {
        Event::DaemonMessage(DaemonMessage::Models {
            models,
            selected_model,
        }) => {
            assert_eq!(models.len(), 2);
            assert_eq!(selected_model.as_deref(), Some("claude-4"));
        }
        other => panic!("expected Models, got {other:?}"),
    }

    // Send a RunInput to trigger an echoed response.
    client
        .writer_tx
        .send(ClientMessage::RunInput {
            request_id: 42,
            input: b"hello".to_vec(),
        })
        .unwrap();

    // Receive the OutputChunk.
    let chunk_event = event_rx.recv().unwrap();
    match chunk_event {
        Event::DaemonMessage(DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        }) => {
            assert_eq!(request_id, 42);
            assert!(matches!(stream, tai_proto::OutputStream::Answer));
            assert_eq!(data, b"echo");
        }
        other => panic!("expected OutputChunk, got {other:?}"),
    }

    // Receive the Done.
    let done_event = event_rx.recv().unwrap();
    match done_event {
        Event::DaemonMessage(DaemonMessage::Done {
            request_id,
            token_usage,
            ..
        }) => {
            assert_eq!(request_id, 42);
            let usage = token_usage.unwrap();
            assert_eq!(usage.input_tokens, 5);
            assert_eq!(usage.output_tokens, 3);
        }
        other => panic!("expected Done, got {other:?}"),
    }

    // Wait for the fake daemon to finish sending (deterministic — no sleep).
    daemon_done_rx.recv().unwrap();

    // ------------------------------------------------------------------
    // Cleanup: drop the client (closes writer channel), wait for threads.
    // ------------------------------------------------------------------
    drop(client);
    writer_handle.join().unwrap();

    // Clean up temp dir.
    let _ = std::fs::remove_dir_all(&socket_path.parent().unwrap());
}
