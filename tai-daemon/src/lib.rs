use std::{collections::HashMap, io, path::Path, sync::Arc, time::Duration};
use tai_proto::{read_message, write_message, ClientMessage, DaemonMessage, OutputStream};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{mpsc, Mutex},
    task::JoinHandle,
    time::sleep,
};
use tracing::{debug, error, info, warn};

pub async fn run_server(socket_path: &str) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing stale socket");
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!(%socket_path, "tai-daemon listening");

    loop {
        let (stream, _) = listener.accept().await?;
        debug!("accepted client connection");
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream).await {
                error!(error = %error, "client error");
            }
        });
    }
}

pub async fn handle_client(stream: UnixStream) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<DaemonMessage>(128);
    let requests = Arc::new(Mutex::new(HashMap::<u32, JoinHandle<()>>::new()));

    debug!("starting client handler");

    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            debug!(?message, "sending daemon message");
            write_message(&mut writer, &message).await?;
        }
        debug!("writer task shutting down");
        writer.shutdown().await
    });

    loop {
        match read_message::<_, ClientMessage>(&mut reader).await {
            Ok(message) => {
                debug!(?message, "received client message");
                match message {
                    ClientMessage::RunInput { request_id, input } => {
                        let mut guard = requests.lock().await;
                        if guard.contains_key(&request_id) {
                            warn!(request_id, "duplicate request id rejected");
                            let _ = tx
                                .send(DaemonMessage::Failed {
                                    request_id,
                                    error: "request id already active".to_string(),
                                })
                                .await;
                            continue;
                        }

                        info!(request_id, input_len = input.len(), "starting request");
                        let tx_clone = tx.clone();
                        let requests_clone = Arc::clone(&requests);
                        let handle = tokio::spawn(async move {
                            let _ = tx_clone.send(DaemonMessage::Started { request_id }).await;
                            let text = String::from_utf8_lossy(&input).trim().to_string();

                            for (index, word) in text.split_whitespace().enumerate() {
                                let chunk = format!("[{request_id}:{index}] {}\n", word.to_uppercase())
                                    .into_bytes();
                                debug!(request_id, chunk_index = index, word, "emitting output chunk");
                                if tx_clone
                                    .send(DaemonMessage::OutputChunk {
                                        request_id,
                                        stream: OutputStream::Stdout,
                                        data: chunk,
                                    })
                                    .await
                                    .is_err()
                                {
                                    warn!(request_id, "client disconnected before output could be delivered");
                                    return;
                                }
                                sleep(Duration::from_millis(150)).await;
                            }

                            let final_msg = if text.is_empty() {
                                warn!(request_id, "request failed: empty input");
                                DaemonMessage::Failed {
                                    request_id,
                                    error: "empty input".to_string(),
                                }
                            } else {
                                info!(request_id, "request completed");
                                DaemonMessage::Done { request_id }
                            };
                            let _ = tx_clone.send(final_msg).await;
                            requests_clone.lock().await.remove(&request_id);
                        });

                        guard.insert(request_id, handle);
                    }
                    ClientMessage::Cancel { request_id } => {
                        if let Some(handle) = requests.lock().await.remove(&request_id) {
                            info!(request_id, "cancelling active request");
                            handle.abort();
                            let _ = tx.send(DaemonMessage::Cancelled { request_id }).await;
                        } else {
                            warn!(request_id, "cancel requested for inactive request");
                            let _ = tx
                                .send(DaemonMessage::Failed {
                                    request_id,
                                    error: "request id not active".to_string(),
                                })
                                .await;
                        }
                    }
                    ClientMessage::Ping => {
                        debug!("responding to ping");
                        let _ = tx.send(DaemonMessage::Pong).await;
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                debug!(error = %error, "client disconnected");
                break;
            }
            Err(error) => {
                error!(error = %error, "failed to read client message");
                return Err(error);
            }
        }
    }

    let handles: Vec<_> = requests.lock().await.drain().map(|(_, handle)| handle).collect();
    if !handles.is_empty() {
        debug!(count = handles.len(), "aborting remaining request tasks");
    }
    for handle in handles {
        handle.abort();
    }
    drop(tx);
    writer_task.await.map_err(io::Error::other)??;
    debug!("client handler finished");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tai_proto::{read_message, write_message};
    use tokio::{net::UnixStream, time::{timeout, Duration}};

    async fn recv(client: &mut UnixStream) -> DaemonMessage {
        timeout(Duration::from_secs(2), read_message::<_, DaemonMessage>(client))
            .await
            .expect("timed out")
            .expect("read failed")
    }

    #[tokio::test]
    async fn ping_round_trip() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server));

        write_message(&mut client, &ClientMessage::Ping).await.expect("write ping");
        assert!(matches!(recv(&mut client).await, DaemonMessage::Pong));

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn concurrent_requests_complete_independently() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server));

        write_message(
            &mut client,
            &ClientMessage::RunInput { request_id: 1, input: b"alpha beta".to_vec() },
        )
        .await
        .expect("write req1");
        write_message(
            &mut client,
            &ClientMessage::RunInput { request_id: 2, input: b"gamma".to_vec() },
        )
        .await
        .expect("write req2");

        let mut started = std::collections::HashSet::new();
        let mut done = std::collections::HashSet::new();
        let mut chunks = Vec::new();

        while done.len() < 2 {
            match recv(&mut client).await {
                DaemonMessage::Started { request_id } => {
                    started.insert(request_id);
                }
                DaemonMessage::OutputChunk { request_id, data, .. } => {
                    chunks.push((request_id, String::from_utf8_lossy(&data).to_string()));
                }
                DaemonMessage::Done { request_id } => {
                    done.insert(request_id);
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }

        assert_eq!(started.len(), 2);
        assert!(chunks.iter().any(|(id, chunk)| *id == 1 && chunk.contains("ALPHA")));
        assert!(chunks.iter().any(|(id, chunk)| *id == 2 && chunk.contains("GAMMA")));

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn duplicate_request_id_is_rejected() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server));

        write_message(
            &mut client,
            &ClientMessage::RunInput { request_id: 7, input: b"first second".to_vec() },
        )
        .await
        .expect("write first");
        write_message(
            &mut client,
            &ClientMessage::RunInput { request_id: 7, input: b"duplicate".to_vec() },
        )
        .await
        .expect("write duplicate");

        let mut saw_failure = false;
        while !saw_failure {
            if let DaemonMessage::Failed { request_id, error } = recv(&mut client).await {
                assert_eq!(request_id, 7);
                assert!(error.contains("already active"));
                saw_failure = true;
            }
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn cancel_stops_active_request() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server));

        write_message(
            &mut client,
            &ClientMessage::RunInput { request_id: 9, input: b"one two three four".to_vec() },
        )
        .await
        .expect("write request");

        loop {
            match recv(&mut client).await {
                DaemonMessage::Started { request_id } if request_id == 9 => break,
                DaemonMessage::OutputChunk { .. } => {}
                other => panic!("unexpected before started: {other:?}"),
            }
        }

        write_message(&mut client, &ClientMessage::Cancel { request_id: 9 })
            .await
            .expect("write cancel");

        loop {
            match recv(&mut client).await {
                DaemonMessage::Cancelled { request_id } => {
                    assert_eq!(request_id, 9);
                    break;
                }
                DaemonMessage::OutputChunk { .. } => {}
                other => panic!("unexpected after cancel: {other:?}"),
            }
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }
}
