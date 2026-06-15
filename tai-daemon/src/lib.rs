pub mod openai;

use crate::openai::{
    AuthConfig, ChatAssistantToolUse, ChatRequestMessage, ChatToolCall, ChatToolDefinition,
    ChatTurnResult, CompletionChunkKind, OpenAiClient, RequestFormat,
};
use reqwest::{
    Method, StatusCode, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use std::{collections::HashMap, io, path::Path, sync::Arc, time::Duration};
use tai_proto::{
    AssistantToolCallRecord, ClientMessage, DaemonMessage, ImageMetadata, MAX_IMAGE_CHUNK_SIZE,
    OutputStream, SessionMessage, SessionSummary, read_message, write_message,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};

struct ActiveRequest {
    handle: JoinHandle<()>,
}

struct SessionState {
    title: Option<String>,
    selected_model: Option<String>,
    messages: Vec<SessionMessage>,
    active_requests: HashMap<u32, ActiveRequest>,
    subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>,
}

pub struct DaemonStateInner {
    next_session_id: u64,
    next_client_id: u64,
    sessions: HashMap<u64, Arc<Mutex<SessionState>>>,
}

pub type DaemonState = Arc<Mutex<DaemonStateInner>>;

#[derive(Debug, Clone)]
struct ToolResult {
    content: String,
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct HttpRequestArgs {
    method: String,
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<String>,
    timeout_secs: Option<u64>,
}

fn available_tools() -> Vec<ChatToolDefinition> {
    vec![
        ChatToolDefinition::function(
            "read_file",
            "Read a UTF-8 text file from the local workspace.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to a text file"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "list_files",
            "List files in a local directory.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to a directory",
                        "default": "."
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "http_request",
            "Make an HTTP request to an absolute URL and return status, response headers, and response body text. Supports custom headers such as Range for partial content requests.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "HEAD"]
                    },
                    "url": {
                        "type": "string",
                        "description": "Absolute http or https URL"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Optional request headers, including Range",
                        "additionalProperties": {
                            "type": "string"
                        }
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional UTF-8 request body"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 30,
                        "default": 10
                    }
                },
                "required": ["method", "url"],
                "additionalProperties": false
            }),
        ),
    ]
}

async fn execute_tool_call(tool_call: &ChatToolCall) -> ToolResult {
    match tool_call.name.as_str() {
        "read_file" => {
            let path = match serde_json::from_str::<serde_json::Value>(&tool_call.arguments_json)
                .ok()
                .and_then(|value| value.get("path").and_then(|value| value.as_str()).map(str::to_string))
            {
                Some(path) if !path.trim().is_empty() => path,
                _ => {
                    return ToolResult {
                        content: "missing required string argument: path".to_string(),
                        is_error: true,
                    }
                }
            };
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => ToolResult {
                    content: truncate_tool_output(&content),
                    is_error: false,
                },
                Err(error) => ToolResult {
                    content: format!("failed to read {path}: {error}"),
                    is_error: true,
                },
            }
        }
        "list_files" => {
            let path = serde_json::from_str::<serde_json::Value>(&tool_call.arguments_json)
                .ok()
                .and_then(|value| value.get("path").and_then(|value| value.as_str()).map(str::to_string))
                .unwrap_or_else(|| ".".to_string());
            match tokio::fs::read_dir(&path).await {
                Ok(mut entries) => {
                    let mut names = Vec::new();
                    loop {
                        match entries.next_entry().await {
                            Ok(Some(entry)) => {
                                let mut name = entry.file_name().to_string_lossy().to_string();
                                if entry.file_type().await.map(|kind| kind.is_dir()).unwrap_or(false) {
                                    name.push('/');
                                }
                                names.push(name);
                            }
                            Ok(None) => break,
                            Err(error) => {
                                return ToolResult {
                                    content: format!("failed to list {path}: {error}"),
                                    is_error: true,
                                }
                            }
                        }
                    }
                    names.sort();
                    ToolResult {
                        content: truncate_tool_output(&names.join("\n")),
                        is_error: false,
                    }
                }
                Err(error) => ToolResult {
                    content: format!("failed to list {path}: {error}"),
                    is_error: true,
                },
            }
        }
        "http_request" => execute_http_request_tool(&tool_call.arguments_json).await,
        _ => ToolResult {
            content: format!("unknown tool: {}", tool_call.name),
            is_error: true,
        },
    }
}

async fn execute_http_request_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<HttpRequestArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => {
            return ToolResult {
                content: format!("invalid arguments: {error}"),
                is_error: true,
            };
        }
    };

    let method = match args.method.as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "HEAD" => Method::HEAD,
        other => {
            return ToolResult {
                content: format!("unsupported method: {other}"),
                is_error: true,
            };
        }
    };

    let url = match Url::parse(&args.url) {
        Ok(url) => url,
        Err(error) => {
            return ToolResult {
                content: format!("invalid url: {error}"),
                is_error: true,
            };
        }
    };
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return ToolResult {
                content: format!("unsupported URL scheme: {other}"),
                is_error: true,
            };
        }
    }

    let timeout_secs = args.timeout_secs.unwrap_or(10).clamp(1, 30);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return ToolResult {
                content: format!("failed to build http client: {error}"),
                is_error: true,
            };
        }
    };

    let headers = match build_http_request_headers(args.headers) {
        Ok(headers) => headers,
        Err(error) => {
            return ToolResult {
                content: error,
                is_error: true,
            };
        }
    };

    let mut request = client.request(method.clone(), url).headers(headers);
    if method != Method::GET && method != Method::HEAD {
        if let Some(body) = args.body {
            request = request.body(body);
        }
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return ToolResult {
                content: format!("http request failed: {error}"),
                is_error: true,
            };
        }
    };

    let status = response.status();
    let headers = response.headers().clone();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = if method == Method::HEAD {
        String::new()
    } else if is_text_content_type(&content_type) {
        match response.text().await {
            Ok(text) => truncate_tool_output(&text),
            Err(error) => format!("body omitted: failed to decode response text: {error}"),
        }
    } else {
        "body omitted: non-text response".to_string()
    };

    ToolResult {
        content: format_http_response(status, &headers, &body),
        is_error: false,
    }
}

fn build_http_request_headers(headers: HashMap<String, String>) -> Result<HeaderMap, String> {
    let mut request_headers = HeaderMap::new();
    for (name, value) in headers {
        let header_name = HeaderName::try_from(name.as_str())
            .map_err(|error| format!("invalid header name: {name}: {error}"))?;
        let header_value = HeaderValue::from_str(&value)
            .map_err(|error| format!("invalid header value for {name}: {error}"))?;
        request_headers.insert(header_name, header_value);
    }
    Ok(request_headers)
}

fn is_text_content_type(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/x-ndjson"
                | "application/graphql-response+json"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

fn format_http_response(status: StatusCode, headers: &HeaderMap, body: &str) -> String {
    let mut output = format!("status: {}", status);

    let mut entries = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or("<non-utf8>").to_string(),
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, value) in entries {
        output.push('\n');
        output.push_str(&name);
        output.push_str(": ");
        output.push_str(&value);
    }

    output.push_str("\n\n");
    output.push_str(body);
    output
}

fn truncate_tool_output(content: &str) -> String {
    const MAX_TOOL_OUTPUT_CHARS: usize = 16 * 1024;
    if content.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return content.to_string();
    }
    let truncated = content.chars().take(MAX_TOOL_OUTPUT_CHARS).collect::<String>();
    format!("{truncated}\n...[truncated]")
}

pub fn new_daemon_state() -> DaemonState {
    let mut sessions = HashMap::new();
    sessions.insert(
        1,
        Arc::new(Mutex::new(SessionState {
            title: Some("default".to_string()),
            selected_model: None,
            messages: Vec::new(),
            active_requests: HashMap::new(),
            subscribers: HashMap::new(),
        })),
    );
    Arc::new(Mutex::new(DaemonStateInner {
        next_session_id: 2,
        next_client_id: 1,
        sessions,
    }))
}

pub async fn run_server(socket_path: &str, auth_config: AuthConfig) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing stale socket");
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    let client = Arc::new(OpenAiClient::new(auth_config)?);
    let state = new_daemon_state();
    info!(%socket_path, "tai-daemon listening");

    let result = loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                debug!("accepted client connection");
                let client = Arc::clone(&client);
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, client, state).await {
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

pub async fn handle_client(
    stream: UnixStream,
    client: Arc<OpenAiClient>,
    state: DaemonState,
) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<DaemonMessage>(128);
    let client_id = {
        let mut guard = state.lock().await;
        let client_id = guard.next_client_id;
        guard.next_client_id = guard.next_client_id.wrapping_add(1);
        client_id
    };
    let mut attached_session_id = default_session_id(&state).await;
    if let Some(session_id) = attached_session_id {
        update_subscription(&state, client_id, None, Some(session_id), &tx).await;
    }

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
                    ClientMessage::CreateSession { title } => {
                        let (session_id, session) = {
                            let mut guard = state.lock().await;
                            let session_id = guard.next_session_id;
                            guard.next_session_id = guard.next_session_id.wrapping_add(1);
                            let session = Arc::new(Mutex::new(SessionState {
                                title: title.clone(),
                                selected_model: None,
                                messages: Vec::new(),
                                active_requests: HashMap::new(),
                                subscribers: HashMap::new(),
                            }));
                            guard.sessions.insert(session_id, Arc::clone(&session));
                            (session_id, session)
                        };
                        update_subscription(&state, client_id, attached_session_id, Some(session_id), &tx)
                            .await;
                        attached_session_id = Some(session_id);
                        let _ = tx
                            .send(DaemonMessage::SessionCreated {
                                session_id,
                                title: title.clone(),
                            })
                            .await;
                        let _ = tx.send(DaemonMessage::SessionAttached { session_id }).await;
                        let snapshot = session_snapshot(session_id, &session).await;
                        let _ = tx.send(snapshot).await;
                    }
                    ClientMessage::ListSessions => {
                        let sessions = list_sessions(&state).await;
                        let _ = tx.send(DaemonMessage::Sessions { sessions }).await;
                    }
                    ClientMessage::AttachSession { session_id } => {
                        let Some(session) = session_by_id(&state, session_id).await else {
                            let _ = tx
                                .send(DaemonMessage::SessionFailed {
                                    operation: "attach_session".to_string(),
                                    error: format!("unknown session: {session_id}"),
                                })
                                .await;
                            continue;
                        };
                        update_subscription(&state, client_id, attached_session_id, Some(session_id), &tx)
                            .await;
                        attached_session_id = Some(session_id);
                        let _ = tx.send(DaemonMessage::SessionAttached { session_id }).await;
                        let snapshot = session_snapshot(session_id, &session).await;
                        let _ = tx.send(snapshot).await;
                    }
                    ClientMessage::GetSessionState { session_id } => {
                        let Some(session) = session_by_id(&state, session_id).await else {
                            let _ = tx
                                .send(DaemonMessage::SessionFailed {
                                    operation: "get_session_state".to_string(),
                                    error: format!("unknown session: {session_id}"),
                                })
                                .await;
                            continue;
                        };
                        let snapshot = session_snapshot(session_id, &session).await;
                        let _ = tx.send(snapshot).await;
                    }
                    ClientMessage::RunInput { request_id, input } => {
                        let Some((session_id, session)) =
                            require_attached_session(&state, attached_session_id, &tx).await?
                        else {
                            continue;
                        };

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

                        let model = {
                            let mut guard = session.lock().await;
                            if guard.active_requests.contains_key(&request_id) {
                                warn!(request_id, session_id, "duplicate request id rejected");
                                let _ = tx
                                    .send(DaemonMessage::Failed {
                                        request_id,
                                        error: "request id already active".to_string(),
                                    })
                                    .await;
                                continue;
                            }
                            let Some(model) = guard.selected_model.clone() else {
                                warn!(request_id, session_id, "request failed: no model selected");
                                let _ = tx.send(DaemonMessage::Started { request_id }).await;
                                let _ = tx
                                    .send(DaemonMessage::Failed {
                                        request_id,
                                        error: "no model selected".to_string(),
                                    })
                                    .await;
                                continue;
                            };
                            let message = SessionMessage::UserText {
                                content: text.clone(),
                            };
                            guard.messages.push(message.clone());
                            drop(guard);
                            broadcast_message_appended(&session, message, Some(client_id)).await;
                            model
                        };

                        let request_format = client.config().request_format_for_model(&model);
                        info!(request_id, session_id, input_len = input.len(), selected_model = %model, ?request_format, "starting request");
                        let client_clone = Arc::clone(&client);
                        let session_clone = Arc::clone(&session);
                        let handle = tokio::spawn(async move {
                            broadcast_to_session(
                                &session_clone,
                                DaemonMessage::Started { request_id },
                                None,
                            )
                            .await;

                            let result = match request_format {
                                RequestFormat::Responses => {
                                    execute_plain_request(&client_clone, &session_clone, &model, request_id).await
                                }
                                RequestFormat::ChatCompletions => {
                                    execute_chat_tool_request(&client_clone, &session_clone, &model, request_id).await
                                }
                            };

                            match result {
                                Ok(()) => {
                                    info!(request_id, session_id, "request completed");
                                    broadcast_to_session(
                                        &session_clone,
                                        DaemonMessage::Done { request_id },
                                        None,
                                    )
                                    .await;
                                }
                                Err(error) => {
                                    warn!(request_id, session_id, error = %error, "request failed");
                                    broadcast_to_session(
                                        &session_clone,
                                        DaemonMessage::Failed {
                                            request_id,
                                            error: format!("model request failed: {error}"),
                                        },
                                        None,
                                    )
                                    .await;
                                }
                            }
                            session_clone.lock().await.active_requests.remove(&request_id);
                        });

                        session
                            .lock()
                            .await
                            .active_requests
                            .insert(request_id, ActiveRequest { handle });
                    }
                    ClientMessage::TestImage { request_id } => {
                        let Some((_session_id, _session)) =
                            require_attached_session(&state, attached_session_id, &tx).await?
                        else {
                            continue;
                        };
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
                        let Some((session_id, session)) =
                            require_attached_session(&state, attached_session_id, &tx).await?
                        else {
                            continue;
                        };
                        if let Some(active_request) =
                            session.lock().await.active_requests.remove(&request_id)
                        {
                            info!(request_id, session_id, "cancelling active request");
                            active_request.handle.abort();
                            let _ = tx.send(DaemonMessage::Cancelled { request_id }).await;
                            broadcast_to_session(
                                &session,
                                DaemonMessage::Cancelled { request_id },
                                Some(client_id),
                            )
                            .await;
                        } else {
                            warn!(
                                request_id,
                                session_id, "cancel requested for inactive request"
                            );
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
                        let selected_model = match attached_session_id {
                            Some(session_id) => match session_by_id(&state, session_id).await {
                                Some(session) => session.lock().await.selected_model.clone(),
                                None => None,
                            },
                            None => None,
                        };
                        match client.validate_and_list_models().await {
                            Ok(models) => {
                                let _ = tx
                                    .send(DaemonMessage::Models {
                                        models,
                                        selected_model,
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
                        let Some((session_id, session)) =
                            require_attached_session(&state, attached_session_id, &tx).await?
                        else {
                            continue;
                        };
                        let config = client.config();
                        debug!(base_url = %config.base_url, model_list_path = %config.model_list_path, responses_path = %config.responses_path, requested_model = %model, session_id, "setting selected model");
                        match client.validate_and_list_models().await {
                            Ok(models) => {
                                if models.iter().any(|candidate| candidate == &model) {
                                    session.lock().await.selected_model = Some(model.clone());
                                    broadcast_to_session(&session, DaemonMessage::ModelSelected { model }, None)
                                        .await;
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

    update_subscription(&state, client_id, attached_session_id, None, &tx).await;
    drop(tx);
    writer_task.abort();
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) =>
        {
            debug!(error = %error, "writer task ended after client disconnect");
        }
        Ok(Err(error)) => return Err(error),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(io::Error::other(error)),
    }
    debug!("client handler finished");
    Ok(())
}

async fn default_session_id(state: &DaemonState) -> Option<u64> {
    state.lock().await.sessions.keys().min().copied()
}

async fn update_subscription(
    state: &DaemonState,
    client_id: u64,
    previous_session_id: Option<u64>,
    next_session_id: Option<u64>,
    tx: &mpsc::Sender<DaemonMessage>,
) {
    if previous_session_id == next_session_id {
        return;
    }

    if let Some(session_id) = previous_session_id
        && let Some(session) = session_by_id(state, session_id).await
    {
        session.lock().await.subscribers.remove(&client_id);
    }

    if let Some(session_id) = next_session_id
        && let Some(session) = session_by_id(state, session_id).await
    {
        session
            .lock()
            .await
            .subscribers
            .insert(client_id, tx.clone());
    }
}

async fn broadcast_to_session(
    session: &Arc<Mutex<SessionState>>,
    message: DaemonMessage,
    exclude_client_id: Option<u64>,
) {
    let subscribers = {
        let guard = session.lock().await;
        guard
            .subscribers
            .iter()
            .filter(|(client_id, _)| Some(**client_id) != exclude_client_id)
            .map(|(_, tx)| tx.clone())
            .collect::<Vec<_>>()
    };
    for tx in subscribers {
        let _ = tx.send(message.clone()).await;
    }
}

async fn broadcast_message_appended(
    session: &Arc<Mutex<SessionState>>,
    message: SessionMessage,
    exclude_client_id: Option<u64>,
) {
    let subscribers = {
        let guard = session.lock().await;
        guard
            .subscribers
            .iter()
            .filter(|(client_id, _)| Some(**client_id) != exclude_client_id)
            .map(|(_, tx)| tx.clone())
            .collect::<Vec<_>>()
    };
    for tx in subscribers {
        let _ = tx
            .send(DaemonMessage::SessionMessageAppended {
                message: message.clone(),
            })
            .await;
    }
}

async fn session_by_id(state: &DaemonState, session_id: u64) -> Option<Arc<Mutex<SessionState>>> {
    state.lock().await.sessions.get(&session_id).cloned()
}

async fn list_sessions(state: &DaemonState) -> Vec<SessionSummary> {
    let sessions: Vec<(u64, Arc<Mutex<SessionState>>)> = state
        .lock()
        .await
        .sessions
        .iter()
        .map(|(session_id, session)| (*session_id, Arc::clone(session)))
        .collect();
    let mut summaries = Vec::with_capacity(sessions.len());
    for (session_id, session) in sessions {
        let guard = session.lock().await;
        summaries.push(SessionSummary {
            session_id,
            title: guard.title.clone(),
            selected_model: guard.selected_model.clone(),
            message_count: guard.messages.len() as u32,
        });
    }
    summaries.sort_by_key(|summary| summary.session_id);
    summaries
}

async fn session_snapshot(session_id: u64, session: &Arc<Mutex<SessionState>>) -> DaemonMessage {
    let guard = session.lock().await;
    DaemonMessage::SessionState {
        session_id,
        title: guard.title.clone(),
        selected_model: guard.selected_model.clone(),
        messages: guard.messages.clone(),
    }
}

async fn require_attached_session(
    state: &DaemonState,
    attached_session_id: Option<u64>,
    tx: &mpsc::Sender<DaemonMessage>,
) -> io::Result<Option<(u64, Arc<Mutex<SessionState>>)>> {
    let Some(session_id) = attached_session_id else {
        let _ = tx
            .send(DaemonMessage::SessionFailed {
                operation: "require_attached_session".to_string(),
                error: "no session attached".to_string(),
            })
            .await;
        return Ok(None);
    };
    let Some(session) = session_by_id(state, session_id).await else {
        let _ = tx
            .send(DaemonMessage::SessionFailed {
                operation: "require_attached_session".to_string(),
                error: format!("unknown session: {session_id}"),
            })
            .await;
        return Ok(None);
    };
    Ok(Some((session_id, session)))
}

async fn execute_plain_request(
    client: &OpenAiClient,
    session: &Arc<Mutex<SessionState>>,
    model: &str,
    request_id: u32,
) -> io::Result<()> {
    let prompt = {
        let guard = session.lock().await;
        build_prompt(&guard.messages)
    };
    let answer = Arc::new(Mutex::new(String::new()));
    let answer_clone = Arc::clone(&answer);
    client
        .completion_stream(model, &prompt, |kind, chunk| {
            let answer = Arc::clone(&answer_clone);
            let session_for_chunk = Arc::clone(session);
            async move {
                if matches!(kind, CompletionChunkKind::Answer) {
                    answer.lock().await.push_str(&chunk);
                }
                broadcast_to_session(
                    &session_for_chunk,
                    DaemonMessage::OutputChunk {
                        request_id,
                        stream: match kind {
                            CompletionChunkKind::Answer => OutputStream::Answer,
                            CompletionChunkKind::Reasoning => OutputStream::Reasoning,
                        },
                        data: chunk.into_bytes(),
                    },
                    None,
                )
                .await;
                Ok(())
            }
        })
        .await?;

    let final_answer = answer.lock().await.trim().to_string();
    if !final_answer.is_empty() {
        session
            .lock()
            .await
            .messages
            .push(SessionMessage::AssistantText {
                content: final_answer,
            });
    }
    Ok(())
}

async fn execute_chat_tool_request(
    client: &OpenAiClient,
    session: &Arc<Mutex<SessionState>>,
    model: &str,
    request_id: u32,
) -> io::Result<()> {
    let tools = available_tools();
    for _ in 0..8 {
        let messages = {
            let guard = session.lock().await;
            build_chat_request_messages(&guard.messages)
        };
        match client.chat_completion_turn(model, &messages, &tools).await? {
            ChatTurnResult::FinalText(content) => {
                broadcast_to_session(
                    session,
                    DaemonMessage::OutputChunk {
                        request_id,
                        stream: OutputStream::Answer,
                        data: content.clone().into_bytes(),
                    },
                    None,
                )
                .await;
                session
                    .lock()
                    .await
                    .messages
                    .push(SessionMessage::AssistantText { content });
                return Ok(());
            }
            ChatTurnResult::ToolUse(tool_use) => {
                persist_assistant_tool_use(session, &tool_use).await;
                for tool_call in tool_use.tool_calls {
                    broadcast_to_session(
                        session,
                        DaemonMessage::ToolCallStarted {
                            request_id,
                            call_id: tool_call.id.clone(),
                            tool_name: tool_call.name.clone(),
                            arguments_json: tool_call.arguments_json.clone(),
                        },
                        None,
                    )
                    .await;
                    let result = execute_tool_call(&tool_call).await;
                    session.lock().await.messages.push(SessionMessage::ToolResult {
                        call_id: tool_call.id.clone(),
                        name: tool_call.name.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    });
                    let event = if result.is_error {
                        DaemonMessage::ToolCallFailed {
                            request_id,
                            call_id: tool_call.id.clone(),
                            tool_name: tool_call.name.clone(),
                            error: result.content.clone(),
                        }
                    } else {
                        DaemonMessage::ToolCallFinished {
                            request_id,
                            call_id: tool_call.id.clone(),
                            tool_name: tool_call.name.clone(),
                            output: result.content.clone(),
                        }
                    };
                    broadcast_to_session(session, event, None).await;
                }
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "tool loop exceeded maximum iterations",
    ))
}

async fn persist_assistant_tool_use(
    session: &Arc<Mutex<SessionState>>,
    tool_use: &ChatAssistantToolUse,
) {
    session
        .lock()
        .await
        .messages
        .push(SessionMessage::AssistantToolUse {
            content: tool_use.content.clone(),
            tool_calls: tool_use
                .tool_calls
                .iter()
                .map(|tool_call| AssistantToolCallRecord {
                    call_id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    arguments_json: tool_call.arguments_json.clone(),
                })
                .collect(),
            reasoning_content: tool_use.reasoning_content.clone(),
            reasoning: tool_use.reasoning.clone(),
            reasoning_text: tool_use.reasoning_text.clone(),
        });
}

fn build_chat_request_messages(messages: &[SessionMessage]) -> Vec<ChatRequestMessage> {
    messages
        .iter()
        .map(|message| match message {
            SessionMessage::SystemText { content } => ChatRequestMessage {
                role: "system",
                content: Some(content.clone()),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
            },
            SessionMessage::UserText { content } => ChatRequestMessage {
                role: "user",
                content: Some(content.clone()),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
            },
            SessionMessage::AssistantText { content } => ChatRequestMessage {
                role: "assistant",
                content: Some(content.clone()),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
            },
            SessionMessage::AssistantToolUse {
                content,
                tool_calls,
                reasoning_content,
                reasoning,
                reasoning_text,
            } => ChatRequestMessage {
                role: "assistant",
                content: content.clone(),
                tool_call_id: None,
                tool_calls: Some(
                    tool_calls
                        .iter()
                        .map(|tool_call| openai::AssistantToolCall {
                            id: tool_call.call_id.clone(),
                            kind: "function".to_string(),
                            function: openai::AssistantToolFunction {
                                name: tool_call.name.clone(),
                                arguments: tool_call.arguments_json.clone(),
                            },
                        })
                        .collect(),
                ),
                reasoning_content: reasoning_content.clone(),
                reasoning: reasoning.clone(),
                reasoning_text: reasoning_text.clone(),
            },
            SessionMessage::ToolResult {
                call_id, content, ..
            } => ChatRequestMessage {
                role: "tool",
                content: Some(content.clone()),
                tool_call_id: Some(call_id.clone()),
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
            },
        })
        .collect()
}

fn build_prompt(messages: &[SessionMessage]) -> String {
    let mut prompt = String::new();
    for message in messages {
        let line = message.render_line();
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(line.trim());
    }
    prompt
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
        assert!(result.content.contains("content-type: application/octet-stream"));
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
}
