use std::sync::Arc;
use tai_daemon::Keystore;
use tai_daemon::{
    REQUEST_IMAGE_BYTES, REQUEST_IMAGE_HEIGHT, REQUEST_IMAGE_MIME_TYPE, REQUEST_IMAGE_WIDTH,
    handle_client, new_daemon_state, DaemonState,
};
use tai_daemon::openai::{OpenAiClient, RequestFormat, ServiceConfig};
use tai_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixStream},
    time::{Duration, timeout},
};

mod common;

static CREDENTIAL_TEST_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn spawn_mock_openai_server(
    chat_response: Option<&'static str>,
    chat_stream: Option<&'static [&'static str]>,
) -> (DaemonState, tokio::task::JoinHandle<()>) {
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
                            std::future::pending::<()>().await;
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
    let config = ServiceConfig {
        base_url: format!("http://{}/v1", addr),
        model_list_path: "/models".to_string(),
        responses_path: "/responses".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        default_request_format: RequestFormat::ChatCompletions,
        model_request_formats: std::collections::HashMap::new(),
        chat_completions_max_tokens: None,
        model_max_tokens: std::collections::HashMap::new(),
        streaming: true,
        max_turns: None,
        retry_max_attempts: 5,
        retry_initial_backoff_ms: 1000,
        retry_max_backoff_ms: 30000,
        connect_timeout_secs: 30,
        request_timeout_secs: 120,
        context: Default::default(),
    };
    let state = new_daemon_state(common::test_db(), 25).await;
    {
        let mut guard = state.lock().await;
        guard.openai_client = Some(Arc::new(
            OpenAiClient::new(config, "test-key".to_string()).expect("client"),
        ));
    }
    (state, handle)
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

fn test_keystore_path() -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!("tai-test-keystore-{unique}.enc"))
}

#[ignore]
#[tokio::test]
async fn ping_round_trip() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(&mut client, &ClientMessage::Ping)
        .await
        .expect("write ping");
    assert!(matches!(recv(&mut client).await, DaemonMessage::Pong));

    drop(client);
    server_task.await.expect("join").expect("server ok");
}

#[ignore]
#[tokio::test]
async fn concurrent_requests_complete_independently() {
    let (state, mock_server) =
        spawn_mock_openai_server(Some("mock completion"), Some(&["mock ", "completion"])).await;
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(server, state));

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

#[ignore]
#[tokio::test]
async fn duplicate_request_id_is_rejected() {
    let (state, mock_server) = spawn_mock_openai_server(None, None).await;
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(server, state));

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

#[ignore]
#[tokio::test]
async fn cancel_stops_active_request() {
    let (state, mock_server) = spawn_mock_openai_server(None, None).await;
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(server, state));

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

    match recv(&mut client).await {
        DaemonMessage::Started { request_id: 9 } => {}
        other => panic!("unexpected before started: {other:?}"),
    }

    write_message(&mut client, &ClientMessage::Cancel { request_id: 9 })
        .await
        .expect("write cancel");

    match recv(&mut client).await {
        DaemonMessage::Cancelled { request_id } => assert_eq!(request_id, 9),
        other => panic!("unexpected after cancel: {other:?}"),
    }

    drop(client);
    server_task.abort();
    let _ = server_task.await;
    mock_server.abort();
}

#[ignore]
#[tokio::test]
async fn test_image_emits_complete_sequence() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

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

#[ignore]
#[tokio::test]
async fn run_input_fails_when_no_model_selected() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

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

#[ignore]
#[tokio::test]
async fn empty_input_fails_request() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

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

#[ignore]
#[tokio::test]
async fn cancel_inactive_request_fails() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

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

#[ignore]
#[tokio::test]
async fn list_models_fails_when_provider_unreachable() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let config = ServiceConfig {
        base_url: "http://127.0.0.1:9/v1".to_string(),
        model_list_path: "/models".to_string(),
        responses_path: "/responses".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        default_request_format: RequestFormat::ChatCompletions,
        model_request_formats: std::collections::HashMap::new(),
        chat_completions_max_tokens: None,
        model_max_tokens: std::collections::HashMap::new(),
        streaming: true,
        max_turns: None,
        retry_max_attempts: 5,
        retry_initial_backoff_ms: 1000,
        retry_max_backoff_ms: 30000,
        connect_timeout_secs: 30,
        request_timeout_secs: 120,
        context: Default::default(),
    };
    let state = new_daemon_state(common::test_db(), 25).await;
    {
        let mut guard = state.lock().await;
        guard.openai_client = Some(Arc::new(
            OpenAiClient::new(config, "test-key".to_string()).expect("client"),
        ));
    }
    let server_task = tokio::spawn(handle_client(server, state));

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

#[ignore]
#[tokio::test]
async fn set_model_fails_when_provider_unreachable() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let config = ServiceConfig {
        base_url: "http://127.0.0.1:9/v1".to_string(),
        model_list_path: "/models".to_string(),
        responses_path: "/responses".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        default_request_format: RequestFormat::ChatCompletions,
        model_request_formats: std::collections::HashMap::new(),
        chat_completions_max_tokens: None,
        model_max_tokens: std::collections::HashMap::new(),
        streaming: true,
        max_turns: None,
        retry_max_attempts: 5,
        retry_initial_backoff_ms: 1000,
        retry_max_backoff_ms: 30000,
        connect_timeout_secs: 30,
        request_timeout_secs: 120,
        context: Default::default(),
    };
    let state = new_daemon_state(common::test_db(), 25).await;
    {
        let mut guard = state.lock().await;
        guard.openai_client = Some(Arc::new(
            OpenAiClient::new(config, "test-key".to_string()).expect("client"),
        ));
    }
    let server_task = tokio::spawn(handle_client(server, state));

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

#[ignore]
#[tokio::test]
async fn add_api_key_round_trip() {
    let _guard = CREDENTIAL_TEST_MUTEX.lock().await;
    let ks_path = test_keystore_path();
    unsafe { std::env::set_var("TAI_KEYSTORE_PATH", ks_path.to_str().unwrap()) };
    let passphrase = "test-passphrase";

    Keystore::init(&ks_path, passphrase).expect("init keystore");

    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::AddApiKey {
            service: "openai".to_string(),
            passphrase: passphrase.to_string(),
            key: "sk-test-key".to_string(),
        },
    )
    .await
    .expect("write add-api-key");

    match recv(&mut client).await {
        DaemonMessage::CredentialAdded { service } => {
            assert_eq!(service, "openai");
        }
        other => panic!("unexpected message: {other:?}"),
    }

    let loaded = Keystore::load(&ks_path, passphrase).expect("reload keystore");
    assert_eq!(loaded.get_api_key("openai"), Some("sk-test-key"));

    drop(client);
    server_task.await.expect("join").expect("server ok");

    let _ = std::fs::remove_file(&ks_path);
}

#[ignore]
#[tokio::test]
async fn add_api_key_rejects_wrong_passphrase() {
    let _guard = CREDENTIAL_TEST_MUTEX.lock().await;
    let ks_path = test_keystore_path();
    unsafe { std::env::set_var("TAI_KEYSTORE_PATH", ks_path.to_str().unwrap()) };

    Keystore::init(&ks_path, "correct").expect("init keystore");

    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::AddApiKey {
            service: "openai".to_string(),
            passphrase: "wrong".to_string(),
            key: "sk-test-key".to_string(),
        },
    )
    .await
    .expect("write add-api-key");

    match recv(&mut client).await {
        DaemonMessage::CredentialAddFailed { service, error } => {
            assert_eq!(service, "openai");
            assert!(error.contains("failed to unlock keystore"));
        }
        other => panic!("unexpected message: {other:?}"),
    }

    drop(client);
    server_task.await.expect("join").expect("server ok");

    let _ = std::fs::remove_file(&ks_path);
}

#[ignore]
#[tokio::test]
async fn add_x_credential_round_trip() {
    let _guard = CREDENTIAL_TEST_MUTEX.lock().await;
    let ks_path = test_keystore_path();
    unsafe { std::env::set_var("TAI_KEYSTORE_PATH", ks_path.to_str().unwrap()) };
    let passphrase = "x-passphrase";

    Keystore::init(&ks_path, passphrase).expect("init keystore");

    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::AddXCredential {
            service: "twitter".to_string(),
            passphrase: passphrase.to_string(),
            api_key: "ck".to_string(),
            api_key_secret: "cs".to_string(),
            access_token: "at".to_string(),
            access_token_secret: "ats".to_string(),
            bearer_token: Some("bearer123".to_string()),
        },
    )
    .await
    .expect("write add-x-credential");

    match recv(&mut client).await {
        DaemonMessage::CredentialAdded { service } => {
            assert_eq!(service, "twitter");
        }
        other => panic!("unexpected message: {other:?}"),
    }

    let loaded = Keystore::load(&ks_path, passphrase).expect("reload keystore");
    let x = loaded.get_x_credentials("twitter").expect("x creds");
    assert_eq!(x.api_key, "ck");
    assert_eq!(x.api_key_secret, "cs");
    assert_eq!(x.access_token, "at");
    assert_eq!(x.access_token_secret, "ats");
    assert_eq!(x.bearer_token, Some("bearer123".to_string()));

    drop(client);
    server_task.await.expect("join").expect("server ok");

    let _ = std::fs::remove_file(&ks_path);
}

#[ignore]
#[tokio::test]
async fn remove_credential_round_trip() {
    let _guard = CREDENTIAL_TEST_MUTEX.lock().await;
    let ks_path = test_keystore_path();
    unsafe { std::env::set_var("TAI_KEYSTORE_PATH", ks_path.to_str().unwrap()) };
    let passphrase = "remove-passphrase";

    Keystore::init(&ks_path, passphrase).expect("init keystore");

    {
        let mut keystore = Keystore::load(&ks_path, passphrase).expect("load");
        keystore.add(
            "openai".to_string(),
            tai_keystore::ServiceCredential::ApiKey {
                key: "sk-to-remove".to_string(),
            },
        );
        keystore.save(&ks_path, passphrase).expect("save");
    }

    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::RemoveCredential {
            service: "openai".to_string(),
            passphrase: passphrase.to_string(),
        },
    )
    .await
    .expect("write remove-credential");

    match recv(&mut client).await {
        DaemonMessage::CredentialRemoved { service } => {
            assert_eq!(service, "openai");
        }
        other => panic!("unexpected message: {other:?}"),
    }

    let loaded = Keystore::load(&ks_path, passphrase).expect("reload keystore");
    assert!(loaded.get_api_key("openai").is_none());

    drop(client);
    server_task.await.expect("join").expect("server ok");

    let _ = std::fs::remove_file(&ks_path);
}

#[ignore]
#[tokio::test]
async fn remove_credential_fails_for_missing_service() {
    let _guard = CREDENTIAL_TEST_MUTEX.lock().await;
    let ks_path = test_keystore_path();
    unsafe { std::env::set_var("TAI_KEYSTORE_PATH", ks_path.to_str().unwrap()) };
    let passphrase = "remove-missing";

    Keystore::init(&ks_path, passphrase).expect("init keystore");

    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = common::test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::RemoveCredential {
            service: "nonexistent".to_string(),
            passphrase: passphrase.to_string(),
        },
    )
    .await
    .expect("write remove-credential");

    match recv(&mut client).await {
        DaemonMessage::CredentialRemoveFailed { service, error } => {
            assert_eq!(service, "nonexistent");
            assert!(error.contains("service not found"));
        }
        other => panic!("unexpected message: {other:?}"),
    }

    drop(client);
    server_task.await.expect("join").expect("server ok");

    let _ = std::fs::remove_file(&ks_path);
}
