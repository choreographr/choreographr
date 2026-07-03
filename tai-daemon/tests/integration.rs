use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tai_daemon::{
    DaemonState, handle_client, new_daemon_state,
    openai::{OpenAiClient, RequestFormat, ServiceConfig},
};
use tai_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixStream},
    time::{Duration, timeout},
};

fn test_db() -> redb::Database {
    let dir = tempfile::tempdir().unwrap();
    redb::Database::create(dir.path().join("state.redb")).unwrap()
}

fn test_service_config() -> ServiceConfig {
    ServiceConfig {
        base_url: "https://example.com/v1".to_string(),
        model_list_path: "/models".to_string(),
        responses_path: "/responses".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        default_request_format: RequestFormat::ChatCompletions,
        model_request_formats: std::collections::HashMap::new(),
        chat_completions_max_tokens: None,
        model_max_tokens: std::collections::HashMap::new(),
        streaming: true,
        max_turns: None,
    }
}

fn test_client() -> Arc<OpenAiClient> {
    Arc::new(OpenAiClient::new(test_service_config(), "test-key".to_string()).expect("client"))
}

async fn test_state_with_client() -> DaemonState {
    let state = new_daemon_state(test_db(), 25).await;
    state.lock().await.openai_client = Some(test_client());
    state
}

async fn recv(client: &mut UnixStream) -> DaemonMessage {
    timeout(
        Duration::from_secs(3),
        read_message::<_, DaemonMessage>(client),
    )
    .await
    .expect("timed out")
    .expect("read failed")
}

async fn spawn_tool_call_server(
    tool_path: String,
) -> (DaemonState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let tool_path = tool_path.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 32 * 1024];
                let Ok(read_len) = stream.read(&mut buffer).await else {
                    return;
                };
                if read_len == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read_len]);
                let first_line = request.lines().next().unwrap_or_default();
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
                    let body = if request.contains("\"role\":\"tool\"") {
                        r#"{"choices":[{"message":{"content":"tool answer"}}]}"#.to_string()
                    } else {
                        format!(
                            "{{\"choices\":[{{\"message\":{{\"content\":null,\"tool_calls\":[{{\"id\":\"call_1\",\"type\":\"function\",\"function\":{{\"name\":\"read_file\",\"arguments\":\"{{\\\"path\\\":\\\"{}\\\"}}\"}}}}]}}}}]}}",
                            tool_path.replace('\\', "\\\\").replace('"', "\\\"")
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
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
    };
    let state = new_daemon_state(test_db(), 25).await;
    {
        let mut guard = state.lock().await;
        guard.openai_client = Some(Arc::new(
            OpenAiClient::new(config, "test-key".to_string()).expect("client"),
        ));
    }
    (state, handle)
}

async fn spawn_http_tool_call_server() -> (DaemonState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 32 * 1024];
                let Ok(read_len) = stream.read(&mut buffer).await else {
                    return;
                };
                if read_len == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read_len]);
                let first_line = request.lines().next().unwrap_or_default();
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
                    let body = if request.contains("\"role\":\"tool\"") {
                        r#"{"choices":[{"message":{"content":"http tool answer"}}]}"#.to_string()
                    } else {
                        serde_json::json!({
                            "choices": [{
                                "message": {
                                    "content": serde_json::Value::Null,
                                    "tool_calls": [{
                                        "id": "call_1",
                                        "type": "function",
                                        "function": {
                                            "name": "http_request",
                                            "arguments": serde_json::json!({
                                                "method": "GET",
                                                "url": format!("http://{addr}/chunk"),
                                                "headers": {
                                                    "Range": "bytes=0-4"
                                                }
                                            }).to_string()
                                        }
                                    }]
                                }
                            }]
                        })
                        .to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if first_line.starts_with("GET /chunk ") {
                    let range = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, value)| value.trim())
                        .unwrap_or_default();
                    assert_eq!(range, "bytes=0-4");
                    let body = "hello";
                    let response = format!(
                        "HTTP/1.1 206 Partial Content\r\ncontent-type: text/plain\r\ncontent-range: bytes 0-4/11\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
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
    };
    let state = new_daemon_state(test_db(), 25).await;
    {
        let mut guard = state.lock().await;
        guard.openai_client = Some(Arc::new(
            OpenAiClient::new(config, "test-key".to_string()).expect("client"),
        ));
    }
    (state, handle)
}

async fn spawn_display_image_tool_server() -> (DaemonState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 32 * 1024];
                let Ok(read_len) = stream.read(&mut buffer).await else {
                    return;
                };
                if read_len == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read_len]);
                let first_line = request.lines().next().unwrap_or_default();
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
                    let body = if request.contains("\"role\":\"tool\"") {
                        r#"{"choices":[{"message":{"content":"image answer"}}]}"#.to_string()
                    } else {
                        serde_json::json!({
                            "choices": [{
                                "message": {
                                    "content": serde_json::Value::Null,
                                    "tool_calls": [{
                                        "id": "call_1",
                                        "type": "function",
                                        "function": {
                                            "name": "display_image",
                                            "arguments": serde_json::json!({
                                                "mime_type": "image/svg+xml",
                                                "svg_text": "<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='red'/></svg>",
                                                "alt": "red rectangle"
                                            }).to_string()
                                        }
                                    }]
                                }
                            }]
                        }).to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
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
    };
    let state = new_daemon_state(test_db(), 25).await;
    {
        let mut guard = state.lock().await;
        guard.openai_client = Some(Arc::new(
            OpenAiClient::new(config, "test-key".to_string()).expect("client"),
        ));
    }
    (state, handle)
}

#[ignore]
#[tokio::test]
async fn daemon_handler_run_input_requires_selected_model() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let state = test_state_with_client().await;
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::RunInput {
            request_id: 1,
            input: b"alpha beta".to_vec(),
        },
    )
    .await
    .expect("write req1");

    let mut saw_started = false;
    loop {
        match recv(&mut client).await {
            DaemonMessage::Started { request_id } => {
                assert_eq!(request_id, 1);
                saw_started = true;
            }
            DaemonMessage::Failed { request_id, error } => {
                assert_eq!(request_id, 1);
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
async fn daemon_handler_set_model_fails_when_provider_unreachable() {
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
    };
    let state = new_daemon_state(test_db(), 25).await;
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
async fn daemon_handler_executes_http_request_tool() {
    let (state, mock_server) = spawn_http_tool_call_server().await;
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        },
    )
    .await
    .expect("write set-model");
    assert!(matches!(
        recv(&mut client).await,
        DaemonMessage::ModelSelected { .. }
    ));

    write_message(
        &mut client,
        &ClientMessage::RunInput {
            request_id: 41,
            input: b"use http tool".to_vec(),
        },
    )
    .await
    .expect("write request");

    let mut saw_tool_start = false;
    let mut saw_tool_finish = false;
    let mut saw_answer = false;
    loop {
        match recv(&mut client).await {
            DaemonMessage::Started { request_id } => assert_eq!(request_id, 41),
            DaemonMessage::ToolCallStarted {
                request_id,
                call_id,
                tool_name,
                arguments_json,
            } => {
                assert_eq!(request_id, 41);
                assert_eq!(call_id, "call_1");
                assert_eq!(tool_name, "http_request");
                assert!(arguments_json.contains("\"Range\":\"bytes=0-4\""));
                saw_tool_start = true;
            }
            DaemonMessage::ToolCallFinished {
                request_id,
                call_id,
                tool_name,
                output,
            } => {
                assert_eq!(request_id, 41);
                assert_eq!(call_id, "call_1");
                assert_eq!(tool_name, "http_request");
                assert!(output.contains("status: 206 Partial Content"));
                assert!(output.contains("content-range: bytes 0-4/11"));
                assert!(output.ends_with("hello"));
                saw_tool_finish = true;
            }
            DaemonMessage::OutputChunk {
                request_id, data, ..
            } => {
                assert_eq!(request_id, 41);
                assert_eq!(String::from_utf8_lossy(&data), "http tool answer");
                saw_answer = true;
            }
            DaemonMessage::Done { request_id } => {
                assert_eq!(request_id, 41);
                break;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    assert!(saw_tool_start);
    assert!(saw_tool_finish);
    assert!(saw_answer);

    drop(client);
    server_task.await.expect("join").expect("server ok");
    mock_server.abort();
}

#[ignore]
#[tokio::test]
async fn daemon_handler_executes_chat_tools() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let tool_path = std::env::temp_dir().join(format!("tai-tool-test-{unique}.txt"));
    tokio::fs::write(&tool_path, "hello from tool\n")
        .await
        .expect("write tool file");

    let (state, mock_server) = spawn_tool_call_server(tool_path.display().to_string()).await;
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        },
    )
    .await
    .expect("write set-model");
    assert!(matches!(
        recv(&mut client).await,
        DaemonMessage::ModelSelected { .. }
    ));

    write_message(
        &mut client,
        &ClientMessage::RunInput {
            request_id: 42,
            input: b"use a tool".to_vec(),
        },
    )
    .await
    .expect("write request");

    let mut saw_tool_start = false;
    let mut saw_tool_finish = false;
    let mut saw_answer = false;
    loop {
        match recv(&mut client).await {
            DaemonMessage::Started { request_id } => assert_eq!(request_id, 42),
            DaemonMessage::ToolCallStarted {
                request_id,
                call_id,
                tool_name,
                arguments_json,
            } => {
                assert_eq!(request_id, 42);
                assert_eq!(call_id, "call_1");
                assert_eq!(tool_name, "read_file");
                assert!(arguments_json.contains(tool_path.to_string_lossy().as_ref()));
                saw_tool_start = true;
            }
            DaemonMessage::ToolCallFinished {
                request_id,
                call_id,
                tool_name,
                output,
            } => {
                assert_eq!(request_id, 42);
                assert_eq!(call_id, "call_1");
                assert_eq!(tool_name, "read_file");
                assert!(output.contains("hello from tool"));
                saw_tool_finish = true;
            }
            DaemonMessage::OutputChunk {
                request_id, data, ..
            } => {
                assert_eq!(request_id, 42);
                assert_eq!(String::from_utf8_lossy(&data), "tool answer");
                saw_answer = true;
            }
            DaemonMessage::Done { request_id } => {
                assert_eq!(request_id, 42);
                break;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    assert!(saw_tool_start);
    assert!(saw_tool_finish);
    assert!(saw_answer);

    let _ = tokio::fs::remove_file(&tool_path).await;
    drop(client);
    server_task.await.expect("join").expect("server ok");
    mock_server.abort();
}

#[ignore]
#[tokio::test]
async fn daemon_handler_display_image_tool_emits_svg_image_messages() {
    let (state, mock_server) = spawn_display_image_tool_server().await;
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(server, state));

    write_message(
        &mut client,
        &ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        },
    )
    .await
    .expect("write set-model");
    assert!(matches!(
        recv(&mut client).await,
        DaemonMessage::ModelSelected { .. }
    ));

    write_message(
        &mut client,
        &ClientMessage::RunInput {
            request_id: 77,
            input: b"show an svg".to_vec(),
        },
    )
    .await
    .expect("write request");

    let mut saw_tool_start = false;
    let mut saw_tool_finish = false;
    let mut saw_image_start = false;
    let mut saw_image_chunk = false;
    let mut saw_image_end = false;
    let mut saw_answer = false;
    loop {
        match recv(&mut client).await {
            DaemonMessage::Started { request_id } => assert_eq!(request_id, 77),
            DaemonMessage::ToolCallStarted {
                request_id,
                call_id,
                tool_name,
                ..
            } => {
                assert_eq!(request_id, 77);
                assert_eq!(call_id, "call_1");
                assert_eq!(tool_name, "display_image");
                saw_tool_start = true;
            }
            DaemonMessage::ImageStart {
                request_id,
                metadata,
            } => {
                assert_eq!(request_id, 77);
                assert_eq!(metadata.image_id, 1);
                assert_eq!(metadata.mime_type, "image/svg+xml");
                assert_eq!(metadata.width, 4);
                assert_eq!(metadata.height, 3);
                assert_eq!(metadata.alt.as_deref(), Some("red rectangle"));
                saw_image_start = true;
            }
            DaemonMessage::ImageChunk {
                request_id,
                image_id,
                data,
            } => {
                assert_eq!(request_id, 77);
                assert_eq!(image_id, 1);
                assert!(!data.is_empty());
                saw_image_chunk = true;
            }
            DaemonMessage::ImageEnd {
                request_id,
                image_id,
            } => {
                assert_eq!(request_id, 77);
                assert_eq!(image_id, 1);
                saw_image_end = true;
            }
            DaemonMessage::ToolCallFinished {
                request_id,
                tool_name,
                output,
                ..
            } => {
                assert_eq!(request_id, 77);
                assert_eq!(tool_name, "display_image");
                assert!(output.contains("displayed image"));
                saw_tool_finish = true;
            }
            DaemonMessage::OutputChunk {
                request_id, data, ..
            } => {
                assert_eq!(request_id, 77);
                assert_eq!(String::from_utf8_lossy(&data), "image answer");
                saw_answer = true;
            }
            DaemonMessage::Done { request_id } => {
                assert_eq!(request_id, 77);
                break;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    assert!(saw_tool_start);
    assert!(saw_tool_finish);
    assert!(saw_image_start);
    assert!(saw_image_chunk);
    assert!(saw_image_end);
    assert!(saw_answer);

    drop(client);
    server_task.await.expect("join").expect("server ok");
    mock_server.abort();
}
