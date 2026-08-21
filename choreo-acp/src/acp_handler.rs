use std::io::{BufWriter, Write};
use std::sync::mpsc;
use tracing::{debug, error, info, warn};

use choreo_proto::{ClientMessage, DaemonMessage, SessionEvent};

use crate::acp_jsonrpc::{
    self, AgentCapabilities, AgentInfo, ConfigOptionValue, ContentBlock, InitializeResult,
    ListSessionsResult, McpCapabilities, NewSessionResult, PromptCapabilities, PromptResult,
    RpcMessage, SessionCapabilities, SessionInfo, SessionUpdateParams, UsageInfo,
};
use crate::client_capabilities::ClientCapabilitiesStore;
use crate::config;
use crate::daemon_client::Event;
use crate::error::AcpError;
use crate::pending::{ActivePrompt, ModelsPending, PendingKind, PendingRequests};
use crate::sessions::SessionManager;
use crate::streaming;

/// Error code used when the daemon connection is lost mid-request.
const DISCONNECT_ERR_CODE: i64 = -32001;
const DISCONNECT_ERR_MSG: &str = "Daemon disconnected";

// ---------------------------------------------------------------------------
// Top-level dispatch
// ---------------------------------------------------------------------------

pub fn run_event_loop(
    event_rx: mpsc::Receiver<Event>,
    daemon_writer: mpsc::Sender<ClientMessage>,
) -> Result<(), AcpError> {
    let mut sessions = SessionManager::new();
    let mut pending = PendingRequests::new();
    let mut initialized = false;
    let mut client_caps = ClientCapabilitiesStore::new();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    loop {
        let event = match event_rx.recv() {
            Ok(e) => e,
            Err(mpsc::RecvError) => {
                info!("both I/O threads exited, shutting down");
                break;
            }
        };

        match event {
            Event::AcpRequest(msg) => {
                if !initialized {
                    match &msg {
                        RpcMessage::Request(req) if req.method == "initialize" => {
                            handle_initialize(req, &mut out, &mut client_caps)?;
                            initialized = true;
                        }
                        RpcMessage::Notification(notif)
                            if notif.method == "notifications/initialized" => {}
                        _ => {
                            respond_err(
                                msg.id().unwrap_or(0),
                                -32000,
                                "Not initialized",
                                &mut out,
                            )?;
                        }
                    }
                    let _ = out.flush();
                    continue;
                }

                match msg {
                    RpcMessage::Request(req) => {
                        handle_request(&req, &mut sessions, &mut pending, &daemon_writer, &mut out)?
                    }
                    RpcMessage::Notification(notif) => {
                        handle_notification(&notif, &mut pending, &daemon_writer)?
                    }
                }
            }

            Event::AcpEof => {
                info!("ACP stdin closed (editor disconnected)");
                drop(daemon_writer);
                break;
            }

            Event::DaemonMessage(msg) => {
                handle_daemon_message(msg, &mut sessions, &mut pending, &daemon_writer, &mut out)?
            }

            Event::DaemonDisconnected => {
                error!("daemon disconnected unexpectedly");
                for (session_id, jsonrpc_id) in pending.drain_prompts() {
                    let _ = respond_err(
                        jsonrpc_id,
                        DISCONNECT_ERR_CODE,
                        DISCONNECT_ERR_MSG,
                        &mut out,
                    );
                    sessions.end_prompt(&session_id);
                }
                for entry in pending.drain_sync() {
                    let _ = respond_err(
                        entry.jsonrpc_id,
                        DISCONNECT_ERR_CODE,
                        DISCONNECT_ERR_MSG,
                        &mut out,
                    );
                }
                if let Some(p) = pending.take_models_pending() {
                    let _ = respond_err(
                        p.jsonrpc_id(),
                        DISCONNECT_ERR_CODE,
                        DISCONNECT_ERR_MSG,
                        &mut out,
                    );
                }
                break;
            }
        }

        let _ = out.flush();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: parse JSON-RPC params into a typed struct, returning None on
// missing/invalid params without needing to clone the full Value first.
// ---------------------------------------------------------------------------

fn parse_params<T: serde::de::DeserializeOwned>(params: &Option<serde_json::Value>) -> Option<T> {
    let value = params.as_ref()?;
    serde_json::from_value(value.clone()).ok()
}

// ---------------------------------------------------------------------------
// ACP request dispatch
// ---------------------------------------------------------------------------

fn handle_request(
    req: &acp_jsonrpc::JsonRpcRequest,
    sessions: &mut SessionManager,
    pending: &mut PendingRequests,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    match req.method.as_str() {
        "session/new" => dispatch_new_session(req, pending, daemon_writer, out),
        "session/load" => dispatch_load_session(req, sessions, out),
        "session/list" => dispatch_list_sessions(req, daemon_writer, pending),
        "session/delete" => dispatch_delete_session(req, sessions, daemon_writer, pending, out),
        "session/close" => dispatch_close_session(req, sessions, out),
        "session/set_config_option" => {
            dispatch_set_config_option(req, sessions, daemon_writer, pending, out)
        }
        "session/prompt" => dispatch_prompt(req, sessions, pending, daemon_writer, out),
        _ => respond_err(
            req.id,
            -32601,
            &format!("Method not found: {}", req.method),
            out,
        ),
    }
}

fn handle_notification(
    notif: &acp_jsonrpc::JsonRpcNotification,
    pending: &mut PendingRequests,
    daemon_writer: &mpsc::Sender<ClientMessage>,
) -> Result<(), AcpError> {
    match notif.method.as_str() {
        "session/cancel" => dispatch_cancel(notif, pending, daemon_writer),
        _ => {
            debug!(method = %notif.method, "ignoring unknown notification");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Handler implementations
// ---------------------------------------------------------------------------

fn handle_initialize(
    req: &acp_jsonrpc::JsonRpcRequest,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
    client_caps: &mut ClientCapabilitiesStore,
) -> Result<(), AcpError> {
    info!("handling initialize request");

    // Extract capabilities without cloning the full params Value —
    // only clone the capabilities sub-value.
    if let Some(params) = req.params.as_ref()
        && let Some(caps) = params.get("capabilities")
        && let Ok(init_caps) =
            serde_json::from_value::<crate::acp_jsonrpc::ClientCapabilities>(caps.clone())
    {
        client_caps.set(init_caps);
    }

    let result = InitializeResult {
        protocol_version: 1,
        agent_capabilities: AgentCapabilities {
            load_session: true,
            prompt_capabilities: PromptCapabilities {
                image: true,
                audio: false,
                embedded_context: false,
            },
            session_capabilities: SessionCapabilities {
                list: Some(serde_json::json!({})),
                delete: Some(serde_json::json!({})),
                close: Some(serde_json::json!({})),
            },
            mcp_capabilities: McpCapabilities {
                http: false,
                sse: false,
            },
        },
        agent_info: AgentInfo {
            name: "Choreographr".into(),
            version: "0.1.0".into(),
        },
        config_options: None,
    };

    respond(req.id, serde_json::to_value(result)?, out)
}

fn dispatch_new_session(
    req: &acp_jsonrpc::JsonRpcRequest,
    pending: &mut PendingRequests,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    info!("dispatching session/new (id={})", req.id);

    // Only session/new is blocked when a Models response is pending —
    // other methods like session/list or session/prompt can proceed
    // concurrently.
    if pending.models_pending.is_some() {
        return respond_err(req.id, -32000, "Session creation in progress", out);
    }

    let account_name = parse_params::<acp_jsonrpc::NewSessionRequest>(&req.params).and_then(|r| {
        r.metadata
            .as_ref()
            .and_then(|m| m.get("account_name"))
            .and_then(|v| v.as_str())
            .map(String::from)
    });

    send_to_daemon(daemon_writer, ClientMessage::ListModels)?;
    pending.set_models_pending(ModelsPending::CreateSession {
        jsonrpc_id: req.id,
        account_name,
    });
    Ok(())
}

fn continue_new_session_after_models(
    jsonrpc_id: u64,
    account_name: Option<String>,
    pending: &mut PendingRequests,
    daemon_writer: &mpsc::Sender<ClientMessage>,
) -> Result<(), AcpError> {
    info!(jsonrpc_id, "continuing session/new after ListModels");
    let msg = ClientMessage::CreateSession {
        title: None,
        parent_session_id: None,
        working_dir: None,
        context_config: None,
        account_name,
        selected_model: None,
        reasoning_effort: None,
    };
    send_to_daemon(daemon_writer, msg)?;
    pending.insert_sync(PendingKind::CreateSession, jsonrpc_id);
    Ok(())
}

fn dispatch_load_session(
    req: &acp_jsonrpc::JsonRpcRequest,
    sessions: &SessionManager,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    info!("dispatching session/load (id={})", req.id);

    let acp_id = parse_params::<acp_jsonrpc::LoadSessionRequest>(&req.params)
        .map(|r| r.session_id)
        .unwrap_or_default();

    if acp_id.is_empty() {
        return respond_err(req.id, -32602, "Missing session_id", out);
    }

    let session = match sessions.get(&acp_id) {
        Some(s) => s,
        None => return respond_err(req.id, -32602, &format!("Session not found: {acp_id}"), out),
    };

    // Include the current model as the only entry so the editor sees it
    // as a valid selection even when the full model list isn't available.
    let models: Vec<String> = session
        .model
        .as_ref()
        .map(|m| vec![m.clone()])
        .unwrap_or_default();
    let opts = config::build_config_options(&models, &session.model, None);
    let result = acp_jsonrpc::LoadSessionResult {
        config_options: Some(opts),
    };
    respond(req.id, serde_json::to_value(result)?, out)
}

fn dispatch_list_sessions(
    req: &acp_jsonrpc::JsonRpcRequest,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    pending: &mut PendingRequests,
) -> Result<(), AcpError> {
    info!("dispatching session/list (id={})", req.id);
    send_to_daemon(daemon_writer, ClientMessage::ListSessions)?;
    pending.insert_sync(PendingKind::ListSessions, req.id);
    Ok(())
}

fn dispatch_delete_session(
    req: &acp_jsonrpc::JsonRpcRequest,
    sessions: &mut SessionManager,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    pending: &mut PendingRequests,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    info!("dispatching session/delete (id={})", req.id);

    let session_id = parse_params::<acp_jsonrpc::DeleteSessionRequest>(&req.params)
        .map(|r| r.session_id)
        .unwrap_or_default();

    let daemon_id = match sessions.get(&session_id) {
        Some(s) => s.daemon_id,
        None => {
            return respond_err(
                req.id,
                -32602,
                &format!("Session not found: {session_id}"),
                out,
            );
        }
    };

    send_to_daemon(
        daemon_writer,
        ClientMessage::DeleteSession {
            session_id: daemon_id,
        },
    )?;
    // Session is only removed from local state after the daemon
    // confirms with SessionDeleted (see handle_sync_message).
    pending.insert_sync(PendingKind::DeleteSession(daemon_id), req.id);
    Ok(())
}

fn dispatch_close_session(
    req: &acp_jsonrpc::JsonRpcRequest,
    sessions: &mut SessionManager,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    info!("dispatching session/close (id={})", req.id);

    let session_id = parse_params::<acp_jsonrpc::CloseSessionRequest>(&req.params)
        .map(|r| r.session_id)
        .unwrap_or_default();

    // Validate the session exists in the bridge state before removing it.
    if sessions.get(&session_id).is_none() {
        return respond_err(
            req.id,
            -32602,
            &format!("Session not found: {session_id}"),
            out,
        );
    }

    // Remove the session mapping from the bridge's local state only.
    // The daemon keeps sessions alive until explicitly deleted via
    // session/delete — there is no DetachSession message in the
    // MessagePack protocol, and the daemon internally auto-detaches
    // stale attachments when the connection closes or re-attaches.
    sessions.remove(&session_id);
    respond(req.id, serde_json::json!({}), out)
}

fn dispatch_set_config_option(
    req: &acp_jsonrpc::JsonRpcRequest,
    sessions: &mut SessionManager,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    pending: &mut PendingRequests,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    info!("dispatching session/set_config_option (id={})", req.id);

    let config_req = match parse_params::<acp_jsonrpc::SetConfigOptionRequest>(&req.params) {
        Some(r) => r,
        None => return respond_err(req.id, -32602, "Invalid params", out),
    };

    let acp_id = &config_req.session_id;
    if sessions.get(acp_id).is_none() {
        return respond_err(req.id, -32602, &format!("Session not found: {acp_id}"), out);
    }

    match config_req.config_id.as_str() {
        "model" => handle_set_model(req, &config_req, daemon_writer, pending, out),
        "reasoning_effort" => {
            handle_set_reasoning_effort(req, &config_req, daemon_writer, pending, out)
        }
        "tool_groups" => handle_set_tool_groups(req, &config_req, sessions, out),
        other => respond_err(
            req.id,
            -32602,
            &format!("Unknown config option: {other}"),
            out,
        ),
    }
}

fn handle_set_model(
    req: &acp_jsonrpc::JsonRpcRequest,
    config_req: &acp_jsonrpc::SetConfigOptionRequest,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    pending: &mut PendingRequests,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    let model = match &config_req.value {
        ConfigOptionValue::String(m) => m.clone(),
        _ => return respond_err(req.id, -32602, "Model value must be a string", out),
    };
    send_to_daemon(
        daemon_writer,
        ClientMessage::SetModel {
            model: model.clone(),
        },
    )?;
    pending.insert_sync(PendingKind::SetModel, req.id);
    // State is updated when the daemon confirms (via Models / selected_model).
    // This avoids leaving s.model out of sync if ModelSelectionFailed arrives.
    pending.store_pending_session(PendingKind::SetModel, config_req.session_id.clone());
    Ok(())
}

fn handle_set_reasoning_effort(
    req: &acp_jsonrpc::JsonRpcRequest,
    config_req: &acp_jsonrpc::SetConfigOptionRequest,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    pending: &mut PendingRequests,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    let effort = match &config_req.value {
        ConfigOptionValue::String(s) => {
            let lower = s.to_lowercase();
            match lower.as_str() {
                "off" | "low" | "medium" | "high" => lower,
                other => {
                    return respond_err(
                        req.id,
                        -32602,
                        &format!("Unknown reasoning effort: {other}"),
                        out,
                    );
                }
            }
        }
        _ => return respond_err(req.id, -32602, "Reasoning effort must be a string", out),
    };
    send_to_daemon(daemon_writer, ClientMessage::SetReasoningEffort { effort })?;
    pending.insert_sync(PendingKind::SetReasoningEffort, req.id);
    // State is updated when the daemon confirms (via ReasoningEffortSet).
    pending.store_pending_session(
        PendingKind::SetReasoningEffort,
        config_req.session_id.clone(),
    );
    Ok(())
}

fn handle_set_tool_groups(
    req: &acp_jsonrpc::JsonRpcRequest,
    config_req: &acp_jsonrpc::SetConfigOptionRequest,
    sessions: &mut SessionManager,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    if let Some(s) = sessions.get_mut(&config_req.session_id)
        && let ConfigOptionValue::String(v) = &config_req.value
    {
        s.tool_groups = v.split(',').map(|s| s.trim().to_string()).collect();
    }
    respond(req.id, serde_json::json!({}), out)
}

fn dispatch_prompt(
    req: &acp_jsonrpc::JsonRpcRequest,
    sessions: &mut SessionManager,
    pending: &mut PendingRequests,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    info!("dispatching session/prompt (id={})", req.id);

    let prompt_req = match parse_params::<acp_jsonrpc::PromptRequest>(&req.params) {
        Some(r) => r,
        None => return respond_err(req.id, -32602, "Invalid params", out),
    };

    let acp_id = &prompt_req.session_id;

    let daemon_id = match sessions.get(acp_id) {
        Some(s) => s.daemon_id,
        None => return respond_err(req.id, -32602, &format!("Session not found: {acp_id}"), out),
    };

    // Validate session state *before* sending anything to the daemon so
    // that a SessionBusy/SessionNotFound error becomes a JSON-RPC error
    // response instead of crashing the event loop (issue #2).
    let daemon_request_id = sessions.next_request_id();
    let input = content_blocks_to_text(&prompt_req.prompt);

    if let Err(e) = sessions.try_begin_prompt(acp_id, daemon_request_id) {
        return respond_err(req.id, -32000, &e.to_string(), out);
    }

    send_to_daemon(
        daemon_writer,
        ClientMessage::AttachSession {
            session_id: daemon_id,
        },
    )?;

    send_to_daemon(
        daemon_writer,
        ClientMessage::RunInput {
            request_id: daemon_request_id,
            input: input.into_bytes(),
        },
    )?;

    pending.insert_prompt(
        acp_id,
        ActivePrompt {
            jsonrpc_id: req.id,
            daemon_request_id,
            session_acp_id: acp_id.clone(),
        },
    );

    Ok(())
}

fn dispatch_cancel(
    notif: &acp_jsonrpc::JsonRpcNotification,
    pending: &mut PendingRequests,
    daemon_writer: &mpsc::Sender<ClientMessage>,
) -> Result<(), AcpError> {
    let acp_id = parse_params::<acp_jsonrpc::CancelNotification>(&notif.params)
        .map(|r| r.session_id)
        .unwrap_or_default();

    let prompt = match pending.get_prompt(&acp_id) {
        Some(p) => p,
        None => {
            warn!(acp_id, "cancel for session with no active prompt");
            return Ok(());
        }
    };

    info!(
        acp_id,
        daemon_request_id = prompt.daemon_request_id,
        "cancelling prompt"
    );
    send_to_daemon(
        daemon_writer,
        ClientMessage::Cancel {
            request_id: prompt.daemon_request_id,
        },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon message dispatcher — routes to streaming or sync handler
// ---------------------------------------------------------------------------

fn handle_daemon_message(
    msg: DaemonMessage,
    sessions: &mut SessionManager,
    pending: &mut PendingRequests,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    match &msg {
        DaemonMessage::Session {
            event:
                SessionEvent::OutputChunk { .. }
                | SessionEvent::ToolCallStarted { .. }
                | SessionEvent::ToolResultChunk { .. }
                | SessionEvent::ToolCallFinished { .. }
                | SessionEvent::ToolCallFailed { .. }
                | SessionEvent::Done { .. }
                | SessionEvent::Failed { .. }
                | SessionEvent::Cancelled { .. },
            ..
        } => handle_streaming_message(&msg, pending, sessions, out),
        _ => handle_sync_message(&msg, sessions, pending, daemon_writer, out),
    }
}

// ---------------------------------------------------------------------------
// Streaming message handler — prompt-related daemon events
// ---------------------------------------------------------------------------

fn handle_streaming_message(
    msg: &DaemonMessage,
    pending: &mut PendingRequests,
    sessions: &mut SessionManager,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    // All streaming messages carry request_id — extract it from whichever
    // SessionEvent variant arrived. The `DaemonMessage::Session` envelope
    // wraps every session-scoped event, so the alternation lives on the
    // inner `event` field of one envelope arm.
    let request_id = match msg {
        DaemonMessage::Session {
            event:
                SessionEvent::OutputChunk { request_id, .. }
                | SessionEvent::ToolCallStarted { request_id, .. }
                | SessionEvent::ToolResultChunk { request_id, .. }
                | SessionEvent::ToolCallFinished { request_id, .. }
                | SessionEvent::ToolCallFailed { request_id, .. }
                | SessionEvent::Done { request_id, .. }
                | SessionEvent::Failed { request_id, .. }
                | SessionEvent::Cancelled { request_id, .. },
            ..
        } => *request_id,
        _ => return Ok(()),
    };

    // Find the matching prompt.  If this request_id doesn't belong to any
    // active prompt, the message is stale (e.g. from a prior connection)
    // and can be safely ignored.
    let prompt = match pending.find_by_request_id(request_id) {
        Some(p) => p,
        None => return Ok(()),
    };
    let jsonrpc_id = prompt.jsonrpc_id;
    let session_acp_id = prompt.session_acp_id.clone();
    // `prompt` borrow from `pending` is now released.

    // Emit streaming ACP notifications via the shared translation layer.
    // This handles text chunks, tool call events, usage updates, and
    // status updates for all streaming message types including terminal
    // states (Done, Failed, Cancelled).
    if let Some(updates) = streaming::translate_message(msg, &session_acp_id) {
        for update in &updates {
            stream_update(update, out)?;
        }
    }

    // Terminal messages produce a JSON-RPC response and clean up prompt
    // state.  Non-terminal (mid-stream) messages stop here.  All terminal
    // states are session-scoped events, so unwrap the envelope once and
    // dispatch on the inner event.
    let DaemonMessage::Session { event, .. } = msg else {
        return Ok(());
    };
    match event {
        SessionEvent::Done { token_usage, .. } => {
            send_terminal_response(
                jsonrpc_id,
                "end_turn",
                token_usage.as_ref().map(|u| UsageInfo {
                    used_input_tokens: Some(u.input_tokens),
                    used_output_tokens: Some(u.output_tokens),
                    used_reasoning_tokens: None,
                }),
                pending,
                sessions,
                &session_acp_id,
                out,
            )?;
        }
        SessionEvent::Failed { error, .. } => {
            warn!(%error, "prompt failed");
            send_terminal_response(
                jsonrpc_id,
                "refusal",
                None,
                pending,
                sessions,
                &session_acp_id,
                out,
            )?;
        }
        SessionEvent::Cancelled { .. } => {
            send_terminal_response(
                jsonrpc_id,
                "cancelled",
                None,
                pending,
                sessions,
                &session_acp_id,
                out,
            )?;
        }
        _ => {}
    }

    Ok(())
}

/// Send the final JSON-RPC response for a completed/failed/cancelled prompt
/// and clean up the tracking state (pending + session guard).
fn send_terminal_response(
    jsonrpc_id: u64,
    stop_reason: &str,
    usage: Option<UsageInfo>,
    pending: &mut PendingRequests,
    sessions: &mut SessionManager,
    session_acp_id: &str,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    respond(
        jsonrpc_id,
        serde_json::to_value(PromptResult {
            stop_reason: stop_reason.into(),
            usage,
        })?,
        out,
    )?;
    cleanup_prompt(pending, sessions, session_acp_id);
    Ok(())
}

/// Shared cleanup after a prompt completes, fails, or is cancelled:
/// remove from pending tracking and mark the session as idle.
fn cleanup_prompt(
    pending: &mut PendingRequests,
    sessions: &mut SessionManager,
    session_acp_id: &str,
) {
    pending.take_prompt(session_acp_id);
    sessions.end_prompt(session_acp_id);
}

// ---------------------------------------------------------------------------
// Sync response handler — non-streaming daemon replies
// ---------------------------------------------------------------------------

fn handle_sync_message(
    msg: &DaemonMessage,
    sessions: &mut SessionManager,
    pending: &mut PendingRequests,
    daemon_writer: &mpsc::Sender<ClientMessage>,
    out: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), AcpError> {
    debug!("handling sync daemon message");

    match msg {
        DaemonMessage::Models {
            models: _models,
            selected_model,
        } => match pending.take_models_pending() {
            Some(ModelsPending::CreateSession {
                jsonrpc_id,
                account_name,
            }) => {
                continue_new_session_after_models(
                    jsonrpc_id,
                    account_name,
                    pending,
                    daemon_writer,
                )?;
            }
            None => {
                // SetModel produces a ModelSelected + Models broadcast.
                // The SetModel sync entry is consumed here (not ModelSelected)
                // because Models carries selected_model which we need to
                // update the session state.
                if let Some(entry) = pending.take_sync(&PendingKind::SetModel) {
                    // Daemon confirmed the model — apply it to the session now.
                    if let Some(session_id) = pending.take_pending_session(&PendingKind::SetModel)
                        && let Some(s) = sessions.get_mut(&session_id)
                    {
                        s.model = selected_model.clone();
                    }
                    respond(entry.jsonrpc_id, serde_json::json!({}), out)?;
                }
            }
        },

        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionCreated { .. },
            ..
        } => {
            if let Some(entry) = pending.take_sync(&PendingKind::CreateSession) {
                let acp_id = sessions.create(*session_id);
                send_to_daemon(
                    daemon_writer,
                    ClientMessage::AttachSession {
                        session_id: *session_id,
                    },
                )?;

                let opts = config::build_config_options(&[], &None, None);
                respond(
                    entry.jsonrpc_id,
                    serde_json::to_value(NewSessionResult {
                        session_id: acp_id,
                        config_options: Some(opts),
                    })?,
                    out,
                )?;
            }
        }

        DaemonMessage::Sessions {
            sessions: session_list,
        } => {
            if let Some(entry) = pending.take_sync(&PendingKind::ListSessions) {
                let infos: Vec<SessionInfo> = session_list
                    .iter()
                    .map(|s| {
                        let acp_id = sessions
                            .get_by_daemon_id(s.session_id)
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| format!("daemon_{}", s.session_id));
                        SessionInfo {
                            session_id: acp_id,
                            title: s.title.clone(),
                            model: s.selected_model.clone(),
                            created_at: Some(s.created_at / 1000),
                        }
                    })
                    .collect();
                respond(
                    entry.jsonrpc_id,
                    serde_json::to_value(ListSessionsResult { sessions: infos })?,
                    out,
                )?;
            }
        }

        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleted,
            ..
        } => {
            if let Some(entry) = pending.take_sync(&PendingKind::DeleteSession(*session_id)) {
                // Daemon confirmed deletion — safe to remove local state now.
                // Clone the ACP ID to avoid borrow conflict with remove().
                if let Some(acp_id) = sessions.get_by_daemon_id(*session_id).map(String::from) {
                    sessions.remove(&acp_id);
                }
                respond(entry.jsonrpc_id, serde_json::json!({}), out)?;
            }
        }

        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleteFailed { error },
            ..
        } => {
            if let Some(entry) = pending.take_sync(&PendingKind::DeleteSession(*session_id)) {
                // Daemon failed to delete — local state is preserved so the
                // editor can retry the operation.
                respond_err(entry.jsonrpc_id, -32000, error, out)?;
            }
        }

        // ModelSelected is a no-op here because the SetModel sync entry
        // is consumed in the Models handler (which carries selected_model
        // for updating session state).  See the Models arm above.
        DaemonMessage::Session {
            event: SessionEvent::ModelSelected { .. },
            ..
        } => {}

        DaemonMessage::Session {
            event: SessionEvent::ModelSelectionFailed { error, .. },
            ..
        } => {
            if let Some(entry) = pending.take_sync(&PendingKind::SetModel) {
                pending.take_pending_session(&PendingKind::SetModel);
                respond_err(entry.jsonrpc_id, -32000, error, out)?;
            }
        }

        DaemonMessage::Session {
            event: SessionEvent::ReasoningEffortSet { effort, .. },
            ..
        } => {
            if let Some(entry) = pending.take_sync(&PendingKind::SetReasoningEffort) {
                if let Some(session_id) =
                    pending.take_pending_session(&PendingKind::SetReasoningEffort)
                    && let Some(s) = sessions.get_mut(&session_id)
                {
                    s.reasoning_effort = Some(effort.clone());
                }
                respond(entry.jsonrpc_id, serde_json::json!({}), out)?;
            }
        }

        DaemonMessage::Session {
            event: SessionEvent::ReasoningEffortSetFailed { error, .. },
            ..
        } => {
            if let Some(entry) = pending.take_sync(&PendingKind::SetReasoningEffort) {
                pending.take_pending_session(&PendingKind::SetReasoningEffort);
                respond_err(entry.jsonrpc_id, -32000, error, out)?;
            }
        }

        _ => {
            debug!("ignoring daemon message");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Low-level I/O helpers
// ---------------------------------------------------------------------------

fn send_to_daemon(
    writer: &mpsc::Sender<ClientMessage>,
    msg: ClientMessage,
) -> Result<(), AcpError> {
    writer.send(msg).map_err(|_| {
        error!("daemon writer channel closed");
        AcpError::TransportDisconnected
    })
}

fn respond<W: Write>(id: u64, value: serde_json::Value, out: &mut W) -> Result<(), AcpError> {
    let resp = acp_jsonrpc::make_response(id, value);
    let json = serde_json::to_string(&resp)?;
    writeln!(out, "{json}")?;
    Ok(())
}

fn respond_err<W: Write>(id: u64, code: i64, message: &str, out: &mut W) -> Result<(), AcpError> {
    let resp = acp_jsonrpc::make_error(id, code, message);
    let json = serde_json::to_string(&resp)?;
    writeln!(out, "{json}")?;
    Ok(())
}

fn stream_update<W: Write>(update: &SessionUpdateParams, out: &mut W) -> Result<(), AcpError> {
    let notif = acp_jsonrpc::make_notification("session/update", serde_json::to_value(update)?);
    let json = serde_json::to_string(&notif)?;
    writeln!(out, "{json}")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Content block conversion
// ---------------------------------------------------------------------------

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut result = String::new();
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        match block {
            ContentBlock::Text { text } => result.push_str(text),
            ContentBlock::Resource { resource } => {
                result.push_str("[Resource: ");
                result.push_str(&resource.uri);
                result.push_str("]\n");
                match &resource.content {
                    serde_json::Value::String(s) => result.push_str(s),
                    serde_json::Value::Null => {
                        // No content to append.
                    }
                    other => {
                        if let Ok(s) = serde_json::to_string(other) {
                            result.push_str(&s);
                        }
                    }
                }
            }
            ContentBlock::Image { image } => {
                result.push_str("[Image: ");
                result.push_str(&image.data.len().to_string());
                result.push_str(" bytes, ");
                result.push_str(image.mime_type.as_deref().unwrap_or("unknown"));
                result.push(']');
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_jsonrpc::{ImageContent, ResourceContent, SessionUpdateVariant};

    // ---------------------------------------------------------------
    // content_blocks_to_text tests
    // ---------------------------------------------------------------

    #[test]
    fn content_blocks_empty() {
        assert_eq!(content_blocks_to_text(&[]), "");
    }

    #[test]
    fn content_blocks_single_text() {
        let blocks = [ContentBlock::Text {
            text: "hello".into(),
        }];
        assert_eq!(content_blocks_to_text(&blocks), "hello");
    }

    #[test]
    fn content_blocks_multiple_text() {
        let blocks = [
            ContentBlock::Text {
                text: "line1".into(),
            },
            ContentBlock::Text {
                text: "line2".into(),
            },
        ];
        assert_eq!(content_blocks_to_text(&blocks), "line1\nline2");
    }

    #[test]
    fn content_blocks_resource_with_string_content() {
        let blocks = [ContentBlock::Resource {
            resource: ResourceContent {
                uri: "file:///tmp/doc.txt".into(),
                content: serde_json::Value::String("file content".into()),
            },
        }];
        let result = content_blocks_to_text(&blocks);
        assert_eq!(result, "[Resource: file:///tmp/doc.txt]\nfile content");
    }

    #[test]
    fn content_blocks_resource_with_object_content() {
        let blocks = [ContentBlock::Resource {
            resource: ResourceContent {
                uri: "data://key".into(),
                content: serde_json::json!({"nested": "value"}),
            },
        }];
        let result = content_blocks_to_text(&blocks);
        assert!(result.starts_with("[Resource: data://key]\n"));
        assert!(result.contains("nested"));
        assert!(result.contains("value"));
    }

    #[test]
    fn content_blocks_image() {
        let blocks = [ContentBlock::Image {
            image: ImageContent {
                data: "AAAA".into(),
                mime_type: Some("image/png".into()),
            },
        }];
        let result = content_blocks_to_text(&blocks);
        assert_eq!(result, "[Image: 4 bytes, image/png]");
    }

    #[test]
    fn content_blocks_image_no_mime() {
        let blocks = [ContentBlock::Image {
            image: ImageContent {
                data: "AAAA".into(),
                mime_type: None,
            },
        }];
        let result = content_blocks_to_text(&blocks);
        assert_eq!(result, "[Image: 4 bytes, unknown]");
    }

    #[test]
    fn content_blocks_mixed() {
        let blocks = [
            ContentBlock::Text {
                text: "User says:".into(),
            },
            ContentBlock::Image {
                image: ImageContent {
                    data: "abcd".into(),
                    mime_type: None,
                },
            },
            ContentBlock::Resource {
                resource: ResourceContent {
                    uri: "file:///f".into(),
                    content: serde_json::Value::String("data".into()),
                },
            },
        ];
        let result = content_blocks_to_text(&blocks);
        assert_eq!(
            result,
            "User says:\n[Image: 4 bytes, unknown]\n[Resource: file:///f]\ndata"
        );
    }

    // ---------------------------------------------------------------
    // respond / respond_err / stream_update tests
    // ---------------------------------------------------------------

    #[test]
    fn respond_writes_valid_json_rpc_response() {
        let mut buf = Vec::new();
        let value = serde_json::json!({"session_id": "abc"});
        respond(42, value, &mut buf).unwrap();
        let written = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["result"]["session_id"], "abc");
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn respond_err_writes_error_response() {
        let mut buf = Vec::new();
        respond_err(7, -32602, "Invalid params", &mut buf).unwrap();
        let written = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 7);
        assert!(parsed.get("result").is_none());
        assert_eq!(parsed["error"]["code"], -32602);
        assert_eq!(parsed["error"]["message"], "Invalid params");
    }

    #[test]
    fn respond_err_with_different_codes() {
        let mut buf = Vec::new();
        respond_err(1, -32000, "Session creation failed", &mut buf).unwrap();
        let written = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed["error"]["code"], -32000);
        assert_eq!(parsed["error"]["message"], "Session creation failed");
    }

    #[test]
    fn stream_update_writes_notification() {
        let mut buf = Vec::new();
        let update = SessionUpdateParams {
            session_id: "sess_1".into(),
            variant: SessionUpdateVariant::StatusUpdate {
                status: "completed".into(),
            },
        };
        stream_update(&update, &mut buf).unwrap();
        let written = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "session/update");
        assert!(parsed.get("id").is_none()); // notification, not request
        assert_eq!(parsed["params"]["session_id"], "sess_1");
        assert_eq!(parsed["params"]["type"], "status_update");
        assert_eq!(parsed["params"]["status"], "completed");
    }

    #[test]
    fn stream_update_tool_call_variant() {
        let mut buf = Vec::new();
        let update = SessionUpdateParams {
            session_id: "sess_1".into(),
            variant: SessionUpdateVariant::ToolCall {
                tool_call_id: "call_1".into(),
                title: "bash".into(),
                kind: "terminal".into(),
                status: "running".into(),
                content: vec![ContentBlock::Text {
                    text: r#"{"cmd":"ls"}"#.into(),
                }],
                locations: None,
            },
        };
        stream_update(&update, &mut buf).unwrap();
        let written = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed["params"]["type"], "tool_call");
        assert_eq!(parsed["params"]["tool_call_id"], "call_1");
        assert_eq!(parsed["params"]["status"], "running");
    }

    #[test]
    fn stream_update_agent_message_chunk() {
        let mut buf = Vec::new();
        let update = SessionUpdateParams {
            session_id: "sess_1".into(),
            variant: SessionUpdateVariant::AgentMessageChunk {
                message_id: "msg_1".into(),
                content: ContentBlock::Text {
                    text: "Hello".into(),
                },
            },
        };
        stream_update(&update, &mut buf).unwrap();
        let written = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed["params"]["type"], "agent_message_chunk");
        assert_eq!(parsed["params"]["message_id"], "msg_1");
    }

    #[test]
    fn respond_and_respond_err_are_newline_terminated() {
        let mut buf = Vec::new();
        respond(1, serde_json::json!({}), &mut buf).unwrap();
        respond_err(2, -1, "err", &mut buf).unwrap();
        let written = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""id":1"#));
        assert!(lines[1].contains(r#""id":2"#));
    }
}
