use super::*;
use crate::openai::{AuthConfig, OpenAiClient};
use crate::requests::{
    REQUEST_IMAGE_BYTES, REQUEST_IMAGE_HEIGHT, REQUEST_IMAGE_MIME_TYPE, REQUEST_IMAGE_WIDTH,
};
use std::sync::Arc;
use tai_proto::{ClientMessage, DaemonMessage, read_message, write_message};
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

async fn spawn_http_tool_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind http tool server");
    let addr = listener.local_addr().expect("http tool server addr");
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
                let request = String::from_utf8_lossy(&buffer[..read_len]).to_string();
                let first_line = request.lines().next().unwrap_or_default().to_string();

                if first_line.starts_with("HEAD /meta ") {
                    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 42\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                if first_line.starts_with("GET /range ") {
                    let range = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, value)| value.trim().to_string())
                        .unwrap_or_default();
                    let body = "abcdefghij";
                    let response = format!(
                        "HTTP/1.1 206 Partial Content\r\ncontent-type: text/plain\r\ncontent-length: {}\r\ncontent-range: bytes 0-9/100\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    assert_eq!(range, "bytes=0-9");
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                if first_line.starts_with("GET /binary ") {
                    let body = [0_u8, 159, 146, 150];
                    let header = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                if first_line.starts_with("GET /long ") {
                    let body = "x".repeat((16 * 1024) + 128);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                if first_line.starts_with("POST /echo ") {
                    let body = request
                        .split_once("\r\n\r\n")
                        .map(|(_, body)| body)
                        .unwrap_or_default();
                    let response_body = format!("echo:{body}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                let body = "not found";
                let response = format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), handle)
}

fn test_temp_path(prefix: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique}.txt"))
}

#[tokio::test]
async fn read_file_range_tool_reads_numbered_line_chunks() {
    let path = test_temp_path("tai-read-range-tool");
    tokio::fs::write(&path, "alpha\nbeta\ngamma\ndelta\n")
        .await
        .expect("seed file");

    let result = execute_read_file_range_tool(
        &serde_json::json!({
            "path": path,
            "start_line": 2,
            "max_lines": 2
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("lines: 2-3 of 4"));
    assert!(result.content.contains("2 | beta"));
    assert!(result.content.contains("3 | gamma"));

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn read_file_range_tool_clamps_to_eof() {
    let path = test_temp_path("tai-read-range-eof-tool");
    tokio::fs::write(&path, "alpha\nbeta\ngamma\n")
        .await
        .expect("seed file");

    let result = execute_read_file_range_tool(
        &serde_json::json!({
            "path": path,
            "start_line": 2,
            "max_lines": 10
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("lines: 2-3 of 3"));
    assert!(result.content.contains("2 | beta"));
    assert!(result.content.contains("3 | gamma"));

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn read_file_range_tool_rejects_start_line_past_eof() {
    let path = test_temp_path("tai-read-range-past-eof-tool");
    tokio::fs::write(&path, "alpha\nbeta\n")
        .await
        .expect("seed file");

    let result = execute_read_file_range_tool(
        &serde_json::json!({
            "path": path,
            "start_line": 5,
            "max_lines": 1
        })
        .to_string(),
    )
    .await;

    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("past end of file"));

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn read_file_range_tool_rejects_excessive_max_lines() {
    let path = test_temp_path("tai-read-range-max-lines-tool");
    tokio::fs::write(&path, "alpha\n").await.expect("seed file");

    let result = execute_read_file_range_tool(
        &serde_json::json!({
            "path": path,
            "start_line": 1,
            "max_lines": 201
        })
        .to_string(),
    )
    .await;

    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("max_lines must be <= 200"));

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn write_file_tool_writes_new_file() {
    let path = test_temp_path("tai-write-tool");

    let result = execute_write_file_tool(
        &serde_json::json!({
            "path": path,
            "content": "hello from write tool\n"
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "hello from write tool\n"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn write_file_tool_refuses_overwrite_when_disabled() {
    let path = test_temp_path("tai-write-tool-existing");
    tokio::fs::write(&path, "original\n")
        .await
        .expect("seed file");

    let result = execute_write_file_tool(
        &serde_json::json!({
            "path": path,
            "content": "replacement\n",
            "overwrite": false
        })
        .to_string(),
    )
    .await;

    assert!(result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .contains("refusing to overwrite existing file")
    );
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "original\n"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn write_file_tool_creates_parent_directories() {
    let dir = test_temp_path("tai-write-tool-dir").with_extension("");
    let path = dir.join("nested/output.txt");

    let result = execute_write_file_tool(
        &serde_json::json!({
            "path": path,
            "content": "nested hello\n",
            "create_parents": true
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "nested hello\n"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn edit_file_tool_replaces_single_unique_match() {
    let path = test_temp_path("tai-edit-tool-single");
    tokio::fs::write(&path, "alpha\nbeta\ngamma\n")
        .await
        .expect("seed file");

    let result = execute_edit_file_tool(
        &serde_json::json!({
            "path": path,
            "edits": [
                {
                    "old_text": "beta",
                    "new_text": "delta"
                }
            ]
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("edited file:"));
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "alpha\ndelta\ngamma\n"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn edit_file_tool_fails_when_old_text_is_missing() {
    let path = test_temp_path("tai-edit-tool-missing");
    tokio::fs::write(&path, "hello\nworld\n")
        .await
        .expect("seed file");

    let result = execute_edit_file_tool(
        &serde_json::json!({
            "path": path,
            "edits": [
                {
                    "old_text": "absent",
                    "new_text": "present"
                }
            ]
        })
        .to_string(),
    )
    .await;

    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("old_text not found"));
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "hello\nworld\n"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn edit_file_tool_fails_on_ambiguous_non_replace_all_edit() {
    let path = test_temp_path("tai-edit-tool-ambiguous");
    tokio::fs::write(&path, "repeat\nrepeat\n")
        .await
        .expect("seed file");

    let result = execute_edit_file_tool(
        &serde_json::json!({
            "path": path,
            "edits": [
                {
                    "old_text": "repeat",
                    "new_text": "done"
                }
            ]
        })
        .to_string(),
    )
    .await;

    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("matched 2 locations"));
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "repeat\nrepeat\n"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn edit_file_tool_supports_replace_all_and_dry_run() {
    let path = test_temp_path("tai-edit-tool-replace-all");
    tokio::fs::write(&path, "foo\nfoo\n")
        .await
        .expect("seed file");

    let result = execute_edit_file_tool(
        &serde_json::json!({
            "path": path,
            "dry_run": true,
            "edits": [
                {
                    "old_text": "foo",
                    "new_text": "bar",
                    "replace_all": true
                }
            ]
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("would edit file:"));
    assert!(result.content.contains("2 replacements"));
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "foo\nfoo\n"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn edit_file_tool_validates_expected_sha256() {
    let path = test_temp_path("tai-edit-tool-sha");
    let original = "red\nblue\n";
    tokio::fs::write(&path, original).await.expect("seed file");
    let expected_sha256 = sha256_hex(original);

    let success = execute_edit_file_tool(
        &serde_json::json!({
            "path": path,
            "expected_sha256": expected_sha256,
            "edits": [
                {
                    "old_text": "blue",
                    "new_text": "green"
                }
            ]
        })
        .to_string(),
    )
    .await;
    assert!(!success.is_error, "{}", success.content);
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "red\ngreen\n"
    );

    let failure = execute_edit_file_tool(
        &serde_json::json!({
            "path": path,
            "expected_sha256": expected_sha256,
            "edits": [
                {
                    "old_text": "green",
                    "new_text": "purple"
                }
            ]
        })
        .to_string(),
    )
    .await;
    assert!(failure.is_error, "{}", failure.content);
    assert!(failure.content.contains("expected_sha256 mismatch"));
    assert_eq!(
        tokio::fs::read_to_string(&path).await.expect("read file"),
        "red\ngreen\n"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn http_request_tool_supports_range_header() {
    let (base_url, server) = spawn_http_tool_server().await;
    let result = execute_http_request_tool(
        &serde_json::json!({
            "method": "GET",
            "url": format!("{base_url}/range"),
            "headers": {
                "Range": "bytes=0-9"
            }
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("status: 206 Partial Content"));
    assert!(result.content.contains("content-range: bytes 0-9/100"));
    assert!(result.content.ends_with("abcdefghij"));

    server.abort();
}

#[tokio::test]
async fn http_request_tool_supports_head_requests() {
    let (base_url, server) = spawn_http_tool_server().await;
    let result = execute_http_request_tool(
        &serde_json::json!({
            "method": "HEAD",
            "url": format!("{base_url}/meta")
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("status: 200 OK"));
    assert!(result.content.contains("accept-ranges: bytes"));
    assert!(result.content.ends_with("\n\n"));

    server.abort();
}

#[tokio::test]
async fn http_request_tool_summarizes_non_text_responses() {
    let (base_url, server) = spawn_http_tool_server().await;
    let result = execute_http_request_tool(
        &serde_json::json!({
            "method": "GET",
            "url": format!("{base_url}/binary")
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .contains("content-type: application/octet-stream")
    );
    assert!(result.content.ends_with("body omitted: non-text response"));

    server.abort();
}

#[tokio::test]
async fn http_request_tool_truncates_large_text_responses() {
    let (base_url, server) = spawn_http_tool_server().await;
    let result = execute_http_request_tool(
        &serde_json::json!({
            "method": "GET",
            "url": format!("{base_url}/long")
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("...[truncated]"));

    server.abort();
}

#[tokio::test]
async fn http_request_tool_supports_post_body() {
    let (base_url, server) = spawn_http_tool_server().await;
    let result = execute_http_request_tool(
        &serde_json::json!({
            "method": "POST",
            "url": format!("{base_url}/echo"),
            "body": "hello"
        })
        .to_string(),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.ends_with("echo:hello"));

    server.abort();
}

#[tokio::test]
async fn ping_round_trip() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(
        server,
        Arc::new(OpenAiClient::new(test_auth_config()).expect("client")),
        new_daemon_state(),
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
    let server_task = tokio::spawn(handle_client(server, client_impl, new_daemon_state()));

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
    let server_task = tokio::spawn(handle_client(server, client_impl, new_daemon_state()));

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
    let server_task = tokio::spawn(handle_client(server, client_impl, new_daemon_state()));

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

#[tokio::test]
async fn test_image_emits_complete_sequence() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(
        server,
        Arc::new(OpenAiClient::new(test_auth_config()).expect("client")),
        new_daemon_state(),
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
        new_daemon_state(),
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
        new_daemon_state(),
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
        new_daemon_state(),
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
        new_daemon_state(),
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
        new_daemon_state(),
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
