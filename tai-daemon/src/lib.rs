pub mod openai;

use crate::openai::AuthConfig;
use std::{collections::HashMap, io, path::Path, sync::Arc, time::Duration};
use tai_proto::{
    ClientMessage, DaemonMessage, ImageMetadata, MAX_IMAGE_CHUNK_SIZE, OutputStream,
    read_message, write_message,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{mpsc, Mutex},
    task::JoinHandle,
    time::sleep,
};
use tracing::{debug, error, info, warn};

pub async fn run_server(socket_path: &str, auth_config: AuthConfig) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing stale socket");
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!(%socket_path, "tai-daemon listening");

    let result = loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                debug!("accepted client connection");
                let auth_config = auth_config.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, auth_config).await {
                        error!(error = %error, "client error");
                    }
                });
            }
            ctrl_c_result = tokio::signal::ctrl_c() => {
                ctrl_c_result.map_err(io::Error::other)?;
                info!("received ctrl+c, shutting down tai-daemon");
                break Ok(());
            }
        }
    };

    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing socket");
        std::fs::remove_file(socket_path)?;
    }

    result
}

pub async fn handle_client(stream: UnixStream, auth_config: AuthConfig) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<DaemonMessage>(128);
    let requests = Arc::new(Mutex::new(HashMap::<u32, JoinHandle<()>>::new()));
    let mut selected_model: Option<String> = None;

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

                                if word.eq_ignore_ascii_case("image") || word.eq_ignore_ascii_case("img") {
                                    if emit_demo_image(&tx_clone, request_id, (index + 1) as u32).await.is_err() {
                                        warn!(request_id, image_id = (index + 1) as u32, "client disconnected before image could be delivered");
                                        return;
                                    }
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
                    ClientMessage::ListModels => {
                        debug!(base_url = %auth_config.base_url, model_list_path = %auth_config.model_list_path, "listing configured models");
                        match openai::validate_and_list_models(&auth_config).await {
                            Ok(models) => {
                                let _ = tx
                                    .send(DaemonMessage::Models {
                                        models,
                                        selected_model: selected_model.clone(),
                                    })
                                    .await;
                            }
                            Err(error) => {
                                let _ = tx
                                    .send(DaemonMessage::Failed {
                                        request_id: 0,
                                        error: format!("failed to list models: {error}"),
                                    })
                                    .await;
                            }
                        }
                    }
                    ClientMessage::SetModel { model } => {
                        debug!(base_url = %auth_config.base_url, model_list_path = %auth_config.model_list_path, requested_model = %model, "setting selected model");
                        match openai::validate_and_list_models(&auth_config).await {
                            Ok(models) => {
                                if models.iter().any(|candidate| candidate == &model) {
                                    selected_model = Some(model.clone());
                                    let _ = tx.send(DaemonMessage::ModelSelected { model }).await;
                                } else {
                                    let _ = tx
                                        .send(DaemonMessage::Failed {
                                            request_id: 0,
                                            error: format!("unknown model: {model}"),
                                        })
                                        .await;
                                }
                            }
                            Err(error) => {
                                let _ = tx
                                    .send(DaemonMessage::Failed {
                                        request_id: 0,
                                        error: format!("failed to list models: {error}"),
                                    })
                                    .await;
                            }
                        }
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

const REQUEST_IMAGE_BYTES: &[u8] = include_bytes!("../assets/dua.jpg");
const REQUEST_IMAGE_MIME_TYPE: &str = "image/jpeg";
const REQUEST_IMAGE_WIDTH: u32 = 640;
const REQUEST_IMAGE_HEIGHT: u32 = 640;

async fn emit_demo_image(tx: &mpsc::Sender<DaemonMessage>, request_id: u32, image_id: u32) -> Result<(), mpsc::error::SendError<DaemonMessage>> {
    let metadata = ImageMetadata {
        image_id,
        mime_type: REQUEST_IMAGE_MIME_TYPE.to_string(),
        width: REQUEST_IMAGE_WIDTH,
        height: REQUEST_IMAGE_HEIGHT,
        byte_len: REQUEST_IMAGE_BYTES.len() as u64,
        alt: Some("dua".to_string()),
    };
    tx.send(DaemonMessage::ImageStart { request_id, metadata }).await?;
    for data in REQUEST_IMAGE_BYTES.chunks(MAX_IMAGE_CHUNK_SIZE) {
        tx.send(DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data: data.to_vec(),
        })
        .await?;
    }
    tx.send(DaemonMessage::ImageEnd { request_id, image_id }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tai_proto::{read_message, write_message};
    use tokio::{net::UnixStream, time::{timeout, Duration}};

    fn test_auth_config() -> AuthConfig {
        AuthConfig {
            api_key: "test-key".to_string(),
            base_url: "https://example.com".to_string(),
            model_list_path: "/v1/models".to_string(),
        }
    }

    async fn recv(client: &mut UnixStream) -> DaemonMessage {
        timeout(Duration::from_secs(2), read_message::<_, DaemonMessage>(client))
            .await
            .expect("timed out")
            .expect("read failed")
    }

    #[tokio::test]
    async fn ping_round_trip() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server, test_auth_config()));

        write_message(&mut client, &ClientMessage::Ping).await.expect("write ping");
        assert!(matches!(recv(&mut client).await, DaemonMessage::Pong));

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn concurrent_requests_complete_independently() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server, test_auth_config()));

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
                DaemonMessage::ImageStart { .. }
                | DaemonMessage::ImageChunk { .. }
                | DaemonMessage::ImageEnd { .. } => {}
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
        let server_task = tokio::spawn(handle_client(server, test_auth_config()));

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
        let server_task = tokio::spawn(handle_client(server, test_auth_config()));

        write_message(
            &mut client,
            &ClientMessage::RunInput { request_id: 9, input: b"one two three four".to_vec() },
        )
        .await
        .expect("write request");

        loop {
            match recv(&mut client).await {
                DaemonMessage::Started { request_id } if request_id == 9 => break,
                DaemonMessage::OutputChunk { .. }
                | DaemonMessage::ImageStart { .. }
                | DaemonMessage::ImageChunk { .. }
                | DaemonMessage::ImageEnd { .. } => {}
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
                DaemonMessage::OutputChunk { .. }
                | DaemonMessage::ImageStart { .. }
                | DaemonMessage::ImageChunk { .. }
                | DaemonMessage::ImageEnd { .. } => {}
                other => panic!("unexpected after cancel: {other:?}"),
            }
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn image_keyword_emits_image_messages() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server, test_auth_config()));

        write_message(
            &mut client,
            &ClientMessage::RunInput { request_id: 12, input: b"show image please".to_vec() },
        )
        .await
        .expect("write request");

        let mut saw_start = false;
        let mut saw_chunk = false;
        let mut saw_end = false;

        loop {
            match recv(&mut client).await {
                DaemonMessage::ImageStart { request_id, metadata } => {
                    assert_eq!(request_id, 12);
                    assert_eq!(metadata.mime_type, REQUEST_IMAGE_MIME_TYPE);
                    saw_start = true;
                }
                DaemonMessage::ImageChunk { request_id, image_id, data } => {
                    assert_eq!(request_id, 12);
                    assert_eq!(image_id, 2);
                    assert!(!data.is_empty());
                    saw_chunk = true;
                }
                DaemonMessage::ImageEnd { request_id, image_id } => {
                    assert_eq!(request_id, 12);
                    assert_eq!(image_id, 2);
                    saw_end = true;
                }
                DaemonMessage::Done { request_id } => {
                    assert_eq!(request_id, 12);
                    break;
                }
                DaemonMessage::Started { .. } | DaemonMessage::OutputChunk { .. } => {}
                other => panic!("unexpected message: {other:?}"),
            }
        }

        assert!(saw_start && saw_chunk && saw_end);

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn empty_input_fails_request() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server, test_auth_config()));

        write_message(
            &mut client,
            &ClientMessage::RunInput { request_id: 15, input: b"   ".to_vec() },
        )
        .await
        .expect("write request");

        let mut saw_started = false;
        loop {
            match recv(&mut client).await {
                DaemonMessage::Started { request_id } => {
                    assert_eq!(request_id, 15);
                    saw_started = true;
                }
                DaemonMessage::Failed { request_id, error } => {
                    assert_eq!(request_id, 15);
                    assert!(error.contains("empty input"));
                    break;
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }

        assert!(saw_started);
        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn cancel_inactive_request_fails() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server, test_auth_config()));

        write_message(&mut client, &ClientMessage::Cancel { request_id: 99 })
            .await
            .expect("write cancel");

        match recv(&mut client).await {
            DaemonMessage::Failed { request_id, error } => {
                assert_eq!(request_id, 99);
                assert!(error.contains("not active"));
            }
            other => panic!("unexpected message: {other:?}"),
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn list_models_reports_failure_when_provider_unreachable() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let auth_config = AuthConfig {
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            model_list_path: "/v1/models".to_string(),
        };
        let server_task = tokio::spawn(handle_client(server, auth_config));

        write_message(&mut client, &ClientMessage::ListModels)
            .await
            .expect("write list-models");

        match recv(&mut client).await {
            DaemonMessage::Failed { request_id, error } => {
                assert_eq!(request_id, 0);
                assert!(error.contains("failed to list models"));
            }
            other => panic!("unexpected message: {other:?}"),
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn set_model_reports_failure_when_provider_unreachable() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let auth_config = AuthConfig {
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            model_list_path: "/v1/models".to_string(),
        };
        let server_task = tokio::spawn(handle_client(server, auth_config));

        write_message(
            &mut client,
            &ClientMessage::SetModel {
                model: "gpt-5.4-nano".to_string(),
            },
        )
        .await
        .expect("write set-model");

        match recv(&mut client).await {
            DaemonMessage::Failed { request_id, error } => {
                assert_eq!(request_id, 0);
                assert!(error.contains("failed to list models"));
            }
            other => panic!("unexpected message: {other:?}"),
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn emit_demo_image_sends_complete_sequence() {
        let (tx, mut rx) = mpsc::channel(8);

        emit_demo_image(&tx, 21, 4).await.expect("emit image");
        drop(tx);

        let first = rx.recv().await.expect("image start");
        let second = rx.recv().await.expect("image chunk");
        let third = rx.recv().await.expect("image end");

        match first {
            DaemonMessage::ImageStart { request_id, metadata } => {
                assert_eq!(request_id, 21);
                assert_eq!(metadata.image_id, 4);
                assert_eq!(metadata.byte_len, REQUEST_IMAGE_BYTES.len() as u64);
            }
            other => panic!("unexpected first message: {other:?}"),
        }

        match second {
            DaemonMessage::ImageChunk { request_id, image_id, data } => {
                assert_eq!(request_id, 21);
                assert_eq!(image_id, 4);
                assert_eq!(data, REQUEST_IMAGE_BYTES);
            }
            other => panic!("unexpected second message: {other:?}"),
        }

        match third {
            DaemonMessage::ImageEnd { request_id, image_id } => {
                assert_eq!(request_id, 21);
                assert_eq!(image_id, 4);
            }
            other => panic!("unexpected third message: {other:?}"),
        }

        assert!(rx.recv().await.is_none());
    }
}
