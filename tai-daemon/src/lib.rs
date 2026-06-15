pub mod openai;

use crate::openai::{AuthConfig, CompletionChunkKind, OpenAiClient};
use std::{collections::HashMap, io, path::Path, sync::Arc};
use tai_proto::{
    ClientMessage, DaemonMessage, ImageMetadata, MAX_IMAGE_CHUNK_SIZE, OutputStream, read_message,
    write_message,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};

pub async fn run_server(socket_path: &str, auth_config: AuthConfig) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing stale socket");
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    let client = Arc::new(OpenAiClient::new(auth_config)?);
    info!(%socket_path, "tai-daemon listening");

    let result = loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                debug!("accepted client connection");
                let client = Arc::clone(&client);
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, client).await {
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

pub async fn handle_client(stream: UnixStream, client: Arc<OpenAiClient>) -> io::Result<()> {
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

                        let text = String::from_utf8_lossy(&input).trim().to_string();
                        if text.is_empty() {
                            warn!(request_id, "request failed: empty input");
                            let _ = tx.send(DaemonMessage::Started { request_id }).await;
                            let _ = tx
                                .send(DaemonMessage::Failed {
                                    request_id,
                                    error: "empty input".to_string(),
                                })
                                .await;
                            continue;
                        }

                        let Some(model) = selected_model.clone() else {
                            warn!(request_id, "request failed: no model selected");
                            let _ = tx.send(DaemonMessage::Started { request_id }).await;
                            let _ = tx
                                .send(DaemonMessage::Failed {
                                    request_id,
                                    error: "no model selected".to_string(),
                                })
                                .await;
                            continue;
                        };

                        info!(request_id, input_len = input.len(), selected_model = %model, "starting request");
                        let tx_clone = tx.clone();
                        let requests_clone = Arc::clone(&requests);
                        let client_clone = Arc::clone(&client);
                        let handle = tokio::spawn(async move {
                            let _ = tx_clone.send(DaemonMessage::Started { request_id }).await;
                            let completion = client_clone
                                .completion_stream(&model, &text, |kind, chunk| {
                                    let tx = tx_clone.clone();
                                    async move {
                                        tx.send(DaemonMessage::OutputChunk {
                                            request_id,
                                            stream: match kind {
                                                CompletionChunkKind::Answer => OutputStream::Answer,
                                                CompletionChunkKind::Reasoning => OutputStream::Reasoning,
                                            },
                                            data: chunk.into_bytes(),
                                        })
                                        .await
                                        .map_err(|_| {
                                            io::Error::new(
                                                io::ErrorKind::BrokenPipe,
                                                "client disconnected before output could be delivered",
                                            )
                                        })
                                    }
                                })
                                .await;

                            match completion {
                                Ok(()) => {
                                    info!(request_id, "request completed");
                                    let _ = tx_clone.send(DaemonMessage::Done { request_id }).await;
                                }
                                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                                    warn!(
                                        request_id,
                                        "client disconnected before output could be delivered"
                                    );
                                }
                                Err(error) => {
                                    warn!(request_id, error = %error, "request failed");
                                    let _ = tx_clone
                                        .send(DaemonMessage::Failed {
                                            request_id,
                                            error: format!("model request failed: {error}"),
                                        })
                                        .await;
                                }
                            }
                            requests_clone.lock().await.remove(&request_id);
                        });

                        guard.insert(request_id, handle);
                    }
                    ClientMessage::TestImage { request_id } => {
                        if requests.lock().await.contains_key(&request_id) {
                            warn!(request_id, "duplicate request id rejected");
                            let _ = tx
                                .send(DaemonMessage::Failed {
                                    request_id,
                                    error: "request id already active".to_string(),
                                })
                                .await;
                            continue;
                        }

                        info!(request_id, "sending demo image");
                        let _ = tx.send(DaemonMessage::Started { request_id }).await;
                        match emit_demo_image(&tx, request_id, 1).await {
                            Ok(()) => {
                                let _ = tx.send(DaemonMessage::Done { request_id }).await;
                            }
                            Err(_) => {
                                warn!(
                                    request_id,
                                    "client disconnected before image could be delivered"
                                );
                            }
                        }
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
                        let config = client.config();
                        debug!(base_url = %config.base_url, model_list_path = %config.model_list_path, responses_path = %config.responses_path, "listing configured models");
                        match client.validate_and_list_models().await {
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
                                    .send(DaemonMessage::ModelsFailed {
                                        error: format!("failed to list models: {error}"),
                                    })
                                    .await;
                            }
                        }
                    }
                    ClientMessage::SetModel { model } => {
                        let config = client.config();
                        debug!(base_url = %config.base_url, model_list_path = %config.model_list_path, responses_path = %config.responses_path, requested_model = %model, "setting selected model");
                        match client.validate_and_list_models().await {
                            Ok(models) => {
                                if models.iter().any(|candidate| candidate == &model) {
                                    selected_model = Some(model.clone());
                                    let _ = tx.send(DaemonMessage::ModelSelected { model }).await;
                                } else {
                                    let _ = tx
                                        .send(DaemonMessage::ModelSelectionFailed {
                                            model: model.clone(),
                                            error: format!("unknown model: {model}"),
                                        })
                                        .await;
                                }
                            }
                            Err(error) => {
                                let _ = tx
                                    .send(DaemonMessage::ModelSelectionFailed {
                                        model,
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

    let handles: Vec<_> = requests
        .lock()
        .await
        .drain()
        .map(|(_, handle)| handle)
        .collect();
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

async fn emit_demo_image(
    tx: &mpsc::Sender<DaemonMessage>,
    request_id: u32,
    image_id: u32,
) -> Result<(), mpsc::error::SendError<DaemonMessage>> {
    let metadata = ImageMetadata {
        image_id,
        mime_type: REQUEST_IMAGE_MIME_TYPE.to_string(),
        width: REQUEST_IMAGE_WIDTH,
        height: REQUEST_IMAGE_HEIGHT,
        byte_len: REQUEST_IMAGE_BYTES.len() as u64,
        alt: Some("dua".to_string()),
    };
    tx.send(DaemonMessage::ImageStart {
        request_id,
        metadata,
    })
    .await?;
    for data in REQUEST_IMAGE_BYTES.chunks(MAX_IMAGE_CHUNK_SIZE) {
        tx.send(DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data: data.to_vec(),
        })
        .await?;
    }
    tx.send(DaemonMessage::ImageEnd {
        request_id,
        image_id,
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tai_proto::{read_message, write_message};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, UnixStream},
        time::{Duration, timeout},
    };

    fn test_auth_config() -> AuthConfig {
        AuthConfig {
            api_key: "test-key".to_string(),
            base_url: "https://example.com/v1".to_string(),
            model_list_path: "/models".to_string(),
            responses_path: "/responses".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            default_request_format: openai::RequestFormat::ChatCompletions,
            model_request_formats: std::collections::HashMap::new(),
            chat_completions_max_tokens: None,
            model_max_tokens: std::collections::HashMap::new(),
            streaming: true,
        }
    }

    async fn spawn_mock_openai_server(
        chat_response: Option<&'static str>,
        chat_stream: Option<&'static [&'static str]>,
    ) -> (Arc<OpenAiClient>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock local addr");
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 8192];
                    let Ok(read_len) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read_len == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buffer[..read_len]);
                    let first_line = request.lines().next().unwrap_or_default();
                    let is_streaming = request.contains("\"stream\":true");
                    if first_line.starts_with("GET /v1/models ")
                        || first_line.starts_with("GET /models ")
                    {
                        let body = r#"{"data":[{"id":"gpt-5.4-nano"}]}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                        return;
                    }

                    if first_line.starts_with("POST /v1/chat/completions ")
                        || first_line.starts_with("POST /chat/completions ")
                    {
                        if is_streaming {
                            let chunks = chat_stream.unwrap_or(&[]);
                            let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                            let _ = stream.write_all(header.as_bytes()).await;
                            for chunk in chunks {
                                let event = format!(
                                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                                    chunk
                                );
                                let _ = stream.write_all(event.as_bytes()).await;
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            let _ = stream.write_all(b"data: [DONE]\n\n").await;
                            let _ = stream.shutdown().await;
                            return;
                        }

                        match chat_response {
                            Some(content) => {
                                let body = format!(
                                    "{{\"choices\":[{{\"message\":{{\"content\":\"{}\"}}}}]}}",
                                    content
                                );
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                                let _ = stream.shutdown().await;
                                return;
                            }
                            None => {
                                tokio::time::sleep(Duration::from_secs(30)).await;
                                return;
                            }
                        }
                    }

                    let body = r#"{"error":"not found"}"#;
                    let response = format!(
                        "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        let config = AuthConfig {
            api_key: "test-key".to_string(),
            base_url: format!("http://{}/v1", addr),
            model_list_path: "/models".to_string(),
            responses_path: "/responses".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            default_request_format: openai::RequestFormat::ChatCompletions,
            model_request_formats: std::collections::HashMap::new(),
            chat_completions_max_tokens: None,
            model_max_tokens: std::collections::HashMap::new(),
            streaming: true,
        };
        (Arc::new(OpenAiClient::new(config).expect("client")), handle)
    }

    async fn set_selected_model(client: &mut UnixStream, model: &str) {
        write_message(
            client,
            &ClientMessage::SetModel {
                model: model.to_string(),
            },
        )
        .await
        .expect("write set-model");
    }

    async fn recv(client: &mut UnixStream) -> DaemonMessage {
        timeout(
            Duration::from_secs(2),
            read_message::<_, DaemonMessage>(client),
        )
        .await
        .expect("timed out")
        .expect("read failed")
    }

    #[tokio::test]
    async fn ping_round_trip() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(
            server,
            Arc::new(OpenAiClient::new(test_auth_config()).expect("client")),
        ));

        write_message(&mut client, &ClientMessage::Ping)
            .await
            .expect("write ping");
        assert!(matches!(recv(&mut client).await, DaemonMessage::Pong));

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn concurrent_requests_complete_independently() {
        let (client_impl, mock_server) =
            spawn_mock_openai_server(Some("mock completion"), Some(&["mock ", "completion"])).await;
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server, client_impl));

        set_selected_model(&mut client, "gpt-5.4-nano").await;
        assert!(matches!(
            recv(&mut client).await,
            DaemonMessage::ModelSelected { .. }
        ));

        write_message(
            &mut client,
            &ClientMessage::RunInput {
                request_id: 1,
                input: b"alpha beta".to_vec(),
            },
        )
        .await
        .expect("write req1");
        write_message(
            &mut client,
            &ClientMessage::RunInput {
                request_id: 2,
                input: b"gamma".to_vec(),
            },
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
                DaemonMessage::OutputChunk {
                    request_id, data, ..
                } => {
                    chunks.push((request_id, String::from_utf8_lossy(&data).to_string()));
                }
                DaemonMessage::Done { request_id } => {
                    done.insert(request_id);
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }

        assert_eq!(started.len(), 2);
        let combined_one = chunks
            .iter()
            .filter(|(id, _)| *id == 1)
            .map(|(_, chunk)| chunk.as_str())
            .collect::<String>();
        let combined_two = chunks
            .iter()
            .filter(|(id, _)| *id == 2)
            .map(|(_, chunk)| chunk.as_str())
            .collect::<String>();
        assert_eq!(combined_one, "mock completion");
        assert_eq!(combined_two, "mock completion");

        drop(client);
        server_task.await.expect("join").expect("server ok");
        mock_server.abort();
    }

    #[tokio::test]
    async fn duplicate_request_id_is_rejected() {
        let (client_impl, mock_server) = spawn_mock_openai_server(None, None).await;
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server, client_impl));

        set_selected_model(&mut client, "gpt-5.4-nano").await;
        assert!(matches!(
            recv(&mut client).await,
            DaemonMessage::ModelSelected { .. }
        ));

        write_message(
            &mut client,
            &ClientMessage::RunInput {
                request_id: 7,
                input: b"first second".to_vec(),
            },
        )
        .await
        .expect("write first");
        write_message(
            &mut client,
            &ClientMessage::RunInput {
                request_id: 7,
                input: b"duplicate".to_vec(),
            },
        )
        .await
        .expect("write duplicate");

        let mut saw_failure = false;
        while !saw_failure {
            match recv(&mut client).await {
                DaemonMessage::Started { request_id } => assert_eq!(request_id, 7),
                DaemonMessage::Failed { request_id, error } => {
                    assert_eq!(request_id, 7);
                    assert!(error.contains("already active"));
                    saw_failure = true;
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
        mock_server.abort();
    }

    #[tokio::test]
    async fn cancel_stops_active_request() {
        let (client_impl, mock_server) = spawn_mock_openai_server(None, None).await;
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(server, client_impl));

        set_selected_model(&mut client, "gpt-5.4-nano").await;
        assert!(matches!(
            recv(&mut client).await,
            DaemonMessage::ModelSelected { .. }
        ));

        write_message(
            &mut client,
            &ClientMessage::RunInput {
                request_id: 9,
                input: b"one two three four".to_vec(),
            },
        )
        .await
        .expect("write request");

        loop {
            match recv(&mut client).await {
                DaemonMessage::Started { request_id } if request_id == 9 => break,
                other => panic!("unexpected before started: {other:?}"),
            }
        }

        write_message(&mut client, &ClientMessage::Cancel { request_id: 9 })
            .await
            .expect("write cancel");

        match recv(&mut client).await {
            DaemonMessage::Cancelled { request_id } => assert_eq!(request_id, 9),
            other => panic!("unexpected after cancel: {other:?}"),
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
        mock_server.abort();
    }

    #[tokio::test]
    async fn test_image_emits_complete_sequence() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(
            server,
            Arc::new(OpenAiClient::new(test_auth_config()).expect("client")),
        ));

        write_message(&mut client, &ClientMessage::TestImage { request_id: 12 })
            .await
            .expect("write request");

        let mut saw_started = false;
        let mut saw_image_start = false;
        let mut saw_image_chunk = false;
        let mut saw_image_end = false;
        loop {
            match recv(&mut client).await {
                DaemonMessage::Started { request_id } => {
                    assert_eq!(request_id, 12);
                    saw_started = true;
                }
                DaemonMessage::ImageStart {
                    request_id,
                    metadata,
                } => {
                    assert_eq!(request_id, 12);
                    assert_eq!(metadata.mime_type, REQUEST_IMAGE_MIME_TYPE);
                    assert_eq!(metadata.width, REQUEST_IMAGE_WIDTH);
                    assert_eq!(metadata.height, REQUEST_IMAGE_HEIGHT);
                    assert_eq!(metadata.byte_len, REQUEST_IMAGE_BYTES.len() as u64);
                    saw_image_start = true;
                }
                DaemonMessage::ImageChunk {
                    request_id,
                    image_id,
                    data,
                } => {
                    assert_eq!(request_id, 12);
                    assert_eq!(image_id, 1);
                    assert!(!data.is_empty());
                    saw_image_chunk = true;
                }
                DaemonMessage::ImageEnd {
                    request_id,
                    image_id,
                } => {
                    assert_eq!(request_id, 12);
                    assert_eq!(image_id, 1);
                    saw_image_end = true;
                }
                DaemonMessage::Done { request_id } => {
                    assert_eq!(request_id, 12);
                    break;
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }

        assert!(saw_started);
        assert!(saw_image_start);
        assert!(saw_image_chunk);
        assert!(saw_image_end);

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }

    #[tokio::test]
    async fn run_input_fails_when_no_model_selected() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(
            server,
            Arc::new(OpenAiClient::new(test_auth_config()).expect("client")),
        ));

        write_message(
            &mut client,
            &ClientMessage::RunInput {
                request_id: 12,
                input: b"show image please".to_vec(),
            },
        )
        .await
        .expect("write request");

        let mut saw_started = false;
        loop {
            match recv(&mut client).await {
                DaemonMessage::Started { request_id } => {
                    assert_eq!(request_id, 12);
                    saw_started = true;
                }
                DaemonMessage::Failed { request_id, error } => {
                    assert_eq!(request_id, 12);
                    assert!(error.contains("no model selected"));
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
    async fn empty_input_fails_request() {
        let (server, mut client) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(handle_client(
            server,
            Arc::new(OpenAiClient::new(test_auth_config()).expect("client")),
        ));

        write_message(
            &mut client,
            &ClientMessage::RunInput {
                request_id: 15,
                input: b"   ".to_vec(),
            },
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
        let server_task = tokio::spawn(handle_client(
            server,
            Arc::new(OpenAiClient::new(test_auth_config()).expect("client")),
        ));

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
            base_url: "http://127.0.0.1:9/v1".to_string(),
            model_list_path: "/models".to_string(),
            responses_path: "/responses".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            default_request_format: openai::RequestFormat::ChatCompletions,
            model_request_formats: std::collections::HashMap::new(),
            chat_completions_max_tokens: None,
            model_max_tokens: std::collections::HashMap::new(),
            streaming: true,
        };
        let server_task = tokio::spawn(handle_client(
            server,
            Arc::new(OpenAiClient::new(auth_config).expect("client")),
        ));

        write_message(&mut client, &ClientMessage::ListModels)
            .await
            .expect("write list-models");

        match recv(&mut client).await {
            DaemonMessage::ModelsFailed { error } => {
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
            base_url: "http://127.0.0.1:9/v1".to_string(),
            model_list_path: "/models".to_string(),
            responses_path: "/responses".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            default_request_format: openai::RequestFormat::ChatCompletions,
            model_request_formats: std::collections::HashMap::new(),
            chat_completions_max_tokens: None,
            model_max_tokens: std::collections::HashMap::new(),
            streaming: true,
        };
        let server_task = tokio::spawn(handle_client(
            server,
            Arc::new(OpenAiClient::new(auth_config).expect("client")),
        ));

        write_message(
            &mut client,
            &ClientMessage::SetModel {
                model: "gpt-5.4-nano".to_string(),
            },
        )
        .await
        .expect("write set-model");

        match recv(&mut client).await {
            DaemonMessage::ModelSelectionFailed { model, error } => {
                assert_eq!(model, "gpt-5.4-nano");
                assert!(error.contains("failed to list models"));
            }
            other => panic!("unexpected message: {other:?}"),
        }

        drop(client);
        server_task.await.expect("join").expect("server ok");
    }
}
