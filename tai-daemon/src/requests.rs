use crate::context::{self, LoadedSkill, SkillMeta};
use crate::db::{SessionRecord, write_session_retry};
use crate::openai::{
    AssistantToolCall, AssistantToolFunction, ChatRequestMessage, ChatToolDefinition,
};
use crate::providers::types::{ChatToolCall, ChatTurnResult};
use crate::providers::{
    ChatTurnRequest, InferenceProvider, ReasoningSupport, StreamEvent, ToolResultItem,
    effective_reasoning_support, lookup_provider,
};
use crate::sessions::{RequestContext, SessionCommand, SessionMetadata, SessionState};
use crate::tools::context::ToolContext;
use crate::tools::{
    PreparedImage, ToolError, ToolOutput, ToolOutputFormat, ToolRegistry, resolve_path,
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tai_keystore::ServiceCredential;
use tai_proto::{
    AssistantToolCallRecord, ContextConfig, DaemonMessage, DisplayedImageRecord, ImageMetadata,
    OutputStream, SessionStatus, ThinkingEffort, TokenUsage,
};
use tracing::{debug, info, trace, warn};

/// Broadcast a TurnAppended message to all session subscribers, if the
/// given turn_id exists in the session's turn map.
fn broadcast_turn_appended(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    session: &SessionState,
    turn_id: u32,
) {
    if let Some(turn) = session.turns.get(&turn_id)
        && let Err(e) = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::TurnAppended {
            turn_id,
            turn: turn.clone(),
        }))
    {
        warn!(%turn_id, error = %e, "failed to broadcast TurnAppended");
    }
}

/// Persist a `PreparedImage` to the session's current active turn and
/// broadcast it to live subscribers immediately (mid-turn) so the image
/// appears as soon as the tool finishes rather than waiting for request
/// completion.  Used by both the serial and concurrent tool paths.
fn emit_image(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    image: PreparedImage,
    tool_call_id: Option<String>,
    session: &mut SessionState,
    turn_id: u32,
) {
    let record = DisplayedImageRecord {
        metadata: ImageMetadata {
            mime_type: image.mime_type,
            width: image.width,
            height: image.height,
            byte_len: image.data.len() as u64,
            alt: image.alt,
        },
        data: image.data,
        tool_call_id,
    };
    session.add_displayed_image(turn_id, record.clone());
    broadcast_turn_appended(cmd_tx, session, turn_id);
}

/// Spawn a forwarding thread that relays streaming output chunks to session
/// subscribers in real time.  Exits when the output channel is disconnected
/// (tool finished) or a kill signal is received (caller stopped waiting).
fn spawn_forwarding_thread(
    cmd_tx: mpsc::Sender<SessionCommand>,
    request_id: u32,
    call_id: String,
    output_rx: mpsc::Receiver<Vec<u8>>,
    kill_rx: mpsc::Receiver<()>,
) {
    let check_interval = Duration::from_millis(200);
    thread::spawn(move || {
        loop {
            match output_rx.recv_timeout(check_interval) {
                Ok(data) => {
                    if cmd_tx
                        .send(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk {
                            request_id,
                            call_id: call_id.clone(),
                            data,
                        }))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => match kill_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                },
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

/// Check whether a cancellation signal has been received.
///
/// This is a one-shot check — call this when you only need to check once
/// and don't need to cache the result across loop iterations.
pub(crate) fn is_cancelled_once(rx: &mpsc::Receiver<()>) -> bool {
    rx.try_recv().is_ok()
}

/// Accumulate per-turn token usage into the session-level counter and log it.
fn accumulate_token_usage(
    session: &mut SessionState,
    token_usage: &Option<TokenUsage>,
    turn: u32,
    ctx: &RequestContext,
) {
    if let Some(u) = token_usage {
        session.config.accumulated_usage.input_tokens += u.input_tokens;
        session.config.accumulated_usage.output_tokens += u.output_tokens;
        session.config.accumulated_usage.total_tokens += u.total_tokens;
        // Track the latest prompt_tokens (the actual context size being sent
        // to the model) separately from the accumulated billing counter.
        session.config.last_prompt_tokens = Some(u.input_tokens);
        debug!(
            session_id = ctx.session_id,
            turn,
            input_tokens = u.input_tokens,
            output_tokens = u.output_tokens,
            total_tokens = u.total_tokens,
            accumulated_input = session.config.accumulated_usage.input_tokens,
            accumulated_output = session.config.accumulated_usage.output_tokens,
            "accumulated token usage"
        );
    }
}

/// Broadcast the session's accumulated token usage to all subscribers so the
/// UI can update its final token display at the end of a turn.
fn broadcast_token_usage(ctx: &RequestContext, session: &SessionState) {
    let _ = ctx
        .cmd_tx
        .send(SessionCommand::Broadcast(DaemonMessage::TokenUsageUpdate {
            token_usage: session.config.accumulated_usage,
            last_prompt_tokens: session.config.last_prompt_tokens,
        }));
}

/// Resolve the execution timeout for a tool by name.
///
/// Returns `None` for sub-sessions (run indefinitely) and `Some(duration)`
/// for all other tools so that hanging tools are eventually killed.
fn determine_tool_timeout(name: &str) -> Option<Duration> {
    if name == "spawn_subsession" {
        // Sub-sessions run their own agent loop which may need many
        // turns across multiple LLM calls — no wall-clock timeout.
        None
    } else if matches!(name, "sh" | "nushell" | "fish" | "exec") {
        // Shell commands may involve compilation, tests, or long-running
        // processes that need more time than the default.
        Some(Duration::from_secs(300))
    } else {
        Some(Duration::from_secs(60))
    }
}

/// Aggregated result of a single concurrent tool execution, including any
/// image the tool emitted through its streaming channel.
struct ToolHandle {
    tool_call: ChatToolCall,
    output: ToolOutput,
    image: Option<PreparedImage>,
}

/// Parameters for spawning a single concurrent tool call.
struct SpawnToolArgs {
    tool_call: ChatToolCall,
    timeout: Option<Duration>,
    request_id: u32,
    registry: Arc<ToolRegistry>,
    cmd_tx: mpsc::Sender<SessionCommand>,
    x_credentials: Option<ServiceCredential>,
    working_dir: Option<PathBuf>,
    ctx: ToolContext,
    invocation_description: String,
}

/// Spawn a single tool call on a dedicated thread with its own forwarding
/// channel, timeout guard, and image drain.
///
/// The returned `JoinHandle` lets the caller collect results in whatever
/// order they choose — the spawned thread handles all channel wiring,
/// timeouts, and error recording internally.
fn spawn_single_tool(args: SpawnToolArgs) -> thread::JoinHandle<ToolHandle> {
    let SpawnToolArgs {
        tool_call,
        timeout,
        request_id,
        registry,
        cmd_tx,
        x_credentials,
        working_dir,
        ctx,
        invocation_description,
    } = args;
    // Channel for the execution thread to deliver its final result.
    let (result_tx, result_rx) = mpsc::channel::<Result<ToolOutput, ToolError>>();

    // Channel for streaming output forwarded to subscribers in real time.
    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();

    // Kill signal for the forwarding thread — sent when we're done waiting.
    let (kill_tx, kill_rx) = mpsc::channel::<()>();

    // Image channel — the tool may emit one image during execution.
    let (image_tx, image_rx) = mpsc::channel::<PreparedImage>();

    // ── Forwarding thread ──────────────────────────────────────────
    //
    // Forwards streaming output chunks to subscribers as they arrive.
    // Exits when the output channel is disconnected (tool finished) or
    // a kill signal is received (we stopped waiting).
    spawn_forwarding_thread(cmd_tx, request_id, tool_call.id.clone(), output_rx, kill_rx);

    // ── Execution thread ───────────────────────────────────────────
    let tc = tool_call.clone();
    let tr = registry;
    let xc = x_credentials;
    let c = working_dir;
    let tool_ctx = ctx;
    thread::spawn(move || {
        let result = tr.execute_streaming_json(
            &tc,
            ToolOutputFormat::Text,
            output_tx,
            xc.as_ref(),
            c.as_deref(),
            Some(&tool_ctx),
            Some(image_tx),
        );
        let _ = result_tx.send(result);
    });

    // ── Wait loop ──────────────────────────────────────────────────
    //
    // Two modes:
    //   Some(dur) — bounded wait with deadline; returns error on timeout.
    //   None      — unbounded wait; blocks until the tool completes.
    let deadline = timeout.map(|d| Instant::now() + d);
    let check_interval = Duration::from_millis(200);
    thread::spawn(move || {
        let output = loop {
            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break ToolOutput {
                        content: format!("tool '{}' timed out", tool_call.name,),
                        is_error: true,
                        invocation_description: invocation_description.clone(),
                    };
                }
                match result_rx.recv_timeout(remaining.min(check_interval)) {
                    Ok(Ok(output)) => break output,
                    Ok(Err(e)) => {
                        break ToolOutput {
                            content: e.to_string(),
                            is_error: true,
                            invocation_description: invocation_description.clone(),
                        };
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => {
                        break ToolOutput {
                            content: "tool execution thread panicked".to_string(),
                            is_error: true,
                            invocation_description: invocation_description.clone(),
                        };
                    }
                }
            } else {
                // No timeout — block indefinitely until the tool finishes.
                match result_rx.recv() {
                    Ok(Ok(output)) => break output,
                    Ok(Err(e)) => {
                        break ToolOutput {
                            content: e.to_string(),
                            is_error: true,
                            invocation_description: invocation_description.clone(),
                        };
                    }
                    Err(_) => {
                        break ToolOutput {
                            content: "tool execution thread panicked".to_string(),
                            is_error: true,
                            invocation_description: invocation_description.clone(),
                        };
                    }
                }
            }
        };

        // Drain any image that was emitted during execution.
        let image = image_rx.try_recv().ok();

        // Signal the forwarding thread to stop — we have our result and
        // won't be streaming any more output from this tool call.
        let _ = kill_tx.send(());

        ToolHandle {
            tool_call,
            output,
            image,
        }
    })
}

/// Resolve the effective reasoning effort for a turn, disabling it if the
/// model/provider combination does not support it.
fn resolve_reasoning_effort(
    client: &InferenceProvider,
    model: &str,
    session_id: u64,
    turn_iter: u32,
    configured_effort: ThinkingEffort,
) -> ThinkingEffort {
    if configured_effort == ThinkingEffort::Off {
        return ThinkingEffort::Off;
    }
    let slug = client.provider_slug();
    let catalog_entry = lookup_provider(slug);
    let reasoning_support = catalog_entry
        .map(|e| e.reasoning)
        .unwrap_or(ReasoningSupport::None);
    let effective = effective_reasoning_support(model, reasoning_support);
    if effective == ReasoningSupport::None {
        warn!(
            session_id, turn = turn_iter, model,
            effort = %configured_effort.as_label(),
            "model does not support reasoning effort, disabling",
        );
        ThinkingEffort::Off
    } else {
        debug!(
            session_id, turn = turn_iter,
            effort = %configured_effort.as_label(),
            "reasoning effort active in agent loop",
        );
        configured_effort
    }
}

/// Estimate the number of prompt tokens for the current request using
/// tiktoken.  Returns a (encoding, estimated_tokens) pair so the caller
/// can reuse the encoding for output-token counting during streaming.
fn estimate_prompt_tokens(
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
) -> (Option<&'static tiktoken::CoreBpe>, u32) {
    let encoding =
        tiktoken::encoding_for_model(model).or_else(|| tiktoken::get_encoding("cl100k_base"));
    let estimated = match &encoding {
        Some(enc) => {
            let content_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.content.as_deref())
                .map(|text| enc.count(text) as u32)
                .sum();

            let reasoning_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.reasoning_content.as_deref())
                .map(|text| enc.count(text) as u32)
                .sum();

            let tool_call_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.tool_calls.as_ref())
                .flat_map(|calls| calls.iter())
                .map(|tc| {
                    enc.count(&tc.id) as u32
                        + enc.count(&tc.kind) as u32
                        + enc.count(&tc.function.name) as u32
                        + enc.count(&tc.function.arguments) as u32
                })
                .sum();

            let tool_def_tokens: u32 = tools
                .iter()
                .filter_map(|def| {
                    match serde_json::to_string(def) {
                        Ok(s) => Some(enc.count(&s) as u32),
                        Err(e) => {
                            warn!(error = %e, "failed to serialize tool definition for token estimation");
                            None
                        }
                    }
                })
                .sum();

            content_tokens + reasoning_tokens + tool_call_tokens + tool_def_tokens
        }
        None => {
            tracing::warn!("no tiktoken encoding available for {model}");
            0
        }
    };
    (encoding, estimated)
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get(key)?.as_str().map(|s| s.to_string())
}

struct SystemContentParams<'a> {
    working_dir: Option<&'a Path>,
    context_config: &'a ContextConfig,
    skills: &'a [SkillMeta],
    loaded_skill_bodies: &'a [LoadedSkill],
    tool_registry: &'a ToolRegistry,
    pending_hints: &'a [String],
}

fn build_system_content(
    params: SystemContentParams,
    context_cache: &mut Option<(u64, Arc<String>)>,
) -> Option<String> {
    let working_dir = match params.working_dir {
        Some(wd) => wd,
        None => {
            warn!("cannot build system content: no working directory on session");
            return None;
        }
    };
    let groups = params.tool_registry.groups();
    let base_prompt =
        context::build_base_prompt(params.skills, &groups, params.loaded_skill_bodies);
    let mut content = base_prompt;

    // Context files with fingerprint caching
    if let Ok(bundle) = context::discover_context(working_dir, params.context_config) {
        let context_str = match context_cache {
            Some((fp, cached)) if *fp == bundle.fingerprint => {
                debug!("context cache HIT (fp={})", fp);
                cached.as_str().to_string()
            }
            _ => {
                let s = context::assemble_context(&bundle);
                debug!(
                    "context cache MISS — rebuilt context ({} bytes from {} file(s))",
                    s.len(),
                    bundle.files.len()
                );
                *context_cache = Some((bundle.fingerprint, Arc::new(s.clone())));
                s
            }
        };
        if !context_str.is_empty() {
            content.push_str("\n\n");
            content.push_str(&context_str);
        }
    }

    // Pending subdirectory hints
    if !params.pending_hints.is_empty() {
        content.push_str("\n\n## New context from project subdirectories\n");
        for hint in params.pending_hints {
            content.push('\n');
            content.push_str(hint);
        }
    }

    Some(content)
}

/// Detect a `load_skill` tool call and persist the loaded skill body into
/// the session's loaded_skill_bodies accumulator so it appears in subsequent
/// system prompts.
fn persist_loaded_skill(session: &mut SessionState, tool_name: &str, arguments_json: &str) {
    if tool_name != "load_skill" {
        return;
    }
    let Some(name) = extract_json_string(arguments_json, "name") else {
        warn!("load_skill tool call missing 'name' argument");
        return;
    };
    if session.loaded_skill_bodies.iter().any(|ls| ls.name == name) {
        debug!("skill '{}' already loaded, skipping", name);
        return;
    }
    let Some(ref working_dir) = session.config.working_dir else {
        warn!("cannot load skill '{}': no working directory", name);
        return;
    };
    if let Some(body) = context::load_skill_body(&name, working_dir) {
        info!("loaded skill body: '{}' ({} bytes)", name, body.len());
        session.loaded_skill_bodies.push(LoadedSkill { name, body });
    } else {
        warn!("skill '{}' not found or has empty body", name);
    }
}

/// Check whether a tool call touches a new subdirectory with an AGENTS.md /
/// CLAUDE.md file and, if so, collect the hint text and newly discovered paths.
fn check_subdirectory_hints(
    working_dir: Option<&Path>,
    tool_name: &str,
    arguments_json: &str,
    known_hint_paths: &mut Vec<PathBuf>,
    pending_hints: &mut Vec<String>,
) {
    if let Some((hint_text, new_paths)) =
        context::subdirectory_hints(tool_name, arguments_json, working_dir, known_hint_paths)
    {
        debug!(
            "subdirectory hints for '{}': {} new path(s)",
            tool_name,
            new_paths.len()
        );
        known_hint_paths.extend(new_paths);
        pending_hints.push(hint_text);
    }
}

struct CollectToolResultParams<'a> {
    tool_results: &'a mut Vec<ToolResultItem>,
    session: &'a mut SessionState,
    tool_call: &'a ChatToolCall,
    output: &'a ToolOutput,
    known_hint_paths: &'a mut Vec<PathBuf>,
    pending_hints: &'a mut Vec<String>,
}

/// Collect tool execution output into the result accumulator, persist any
/// `load_skill` call to the session, and check for new subdirectory hints.
/// Called after every tool execution in both the serial and concurrent phases.
fn collect_tool_result(params: CollectToolResultParams) {
    let CollectToolResultParams {
        tool_results,
        session,
        tool_call,
        output,
        known_hint_paths,
        pending_hints,
    } = params;
    trace!(
        "collecting tool result for call {} (tool: '{}')",
        tool_call.id, tool_call.name
    );
    tool_results.push(ToolResultItem {
        call_id: tool_call.id.clone(),
        output: output.content.clone(),
        caller: tool_call.caller.clone(),
    });
    persist_loaded_skill(session, &tool_call.name, &tool_call.arguments_json);
    check_subdirectory_hints(
        session.config.working_dir.as_deref(),
        &tool_call.name,
        &tool_call.arguments_json,
        known_hint_paths,
        pending_hints,
    );
}

pub(crate) fn run_agent_loop(
    client: &InferenceProvider,
    session: &mut SessionState,
    model: &str,
    request_id: u32,
    cancel_rx: &mpsc::Receiver<()>,
    ctx: &RequestContext,
    user_text: Option<String>,
) -> io::Result<bool> {
    let max_turns = session.config.max_turns.unwrap_or(ctx.max_turns_default);
    // `max_turns == 0` means *unlimited* — the loop runs until the model
    // produces a final answer, is cancelled, or hits an error.
    let limited = max_turns > 0;

    let mut prev_resp_id: Option<String> = None;
    let mut tool_results: Vec<ToolResultItem> = Vec::new();
    let mut known_hint_paths: Vec<PathBuf> = Vec::new();
    let mut pending_hints: Vec<String> = Vec::new();

    // Lazily cache discovered skills — they don't change during a session
    if session.discovered_skills.is_none()
        && let Some(ref wd) = session.config.working_dir
    {
        session.discovered_skills = Some(context::discover_skills(wd));
    }

    let mut turn_iter: u32 = 0;
    loop {
        // Enforce the iteration limit only when one is configured.
        // When `max_turns == 0` the loop is unbounded.
        if limited && turn_iter >= max_turns {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tool loop exceeded {max_turns} iterations"),
            ));
        }
        debug!(
            session_id = ctx.session_id,
            turn = turn_iter,
            "agent loop turn"
        );
        let thinking_effort = resolve_reasoning_effort(
            client,
            model,
            ctx.session_id,
            turn_iter,
            session
                .config
                .reasoning_effort
                .unwrap_or(ThinkingEffort::Off),
        );
        crate::metrics::record_turn(model);
        let tools = ctx
            .tool_registry
            .available_definitions(&session.config.active_tool_groups);
        if is_cancelled_once(cancel_rx) {
            return Ok(true);
        }

        // Start a new turn for this agent loop iteration.
        let turn_user_text = if turn_iter == 0 {
            user_text.clone()
        } else {
            None
        };
        let (current_turn_id, _) = session.start_turn(turn_user_text);
        broadcast_turn_appended(&ctx.cmd_tx, session, current_turn_id);
        if ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(SessionStatus::Inference))
            .is_err()
        {
            return Ok(false);
        }

        let system_content = {
            // Scope the immutable borrow on session so it ends before the
            // mutable borrows that follow (start_turn, set_assistant_response, etc.).
            let skills: &[SkillMeta] = session.discovered_skills.as_deref().unwrap_or_default();
            build_system_content(
                SystemContentParams {
                    working_dir: session.config.working_dir.as_deref(),
                    context_config: &session.config.context_config,
                    skills,
                    loaded_skill_bodies: &session.loaded_skill_bodies,
                    tool_registry: &ctx.tool_registry,
                    pending_hints: &pending_hints,
                },
                &mut session.context_cache,
            )
        };
        pending_hints.clear();
        let messages = build_chat_request_messages(session, system_content.as_deref());

        let (encoding, estimated_prompt_tokens) = estimate_prompt_tokens(model, &messages, &tools);

        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::Started {
                request_id,
                turn_id: current_turn_id,
                estimated_prompt_tokens,
            }));

        let mut retry_cb: Option<crate::openai::RetryCallback> = Some(Box::new({
            let cmd_tx = ctx.cmd_tx.clone();
            move |attempt, max_attempts, delay| {
                let _ = cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Retrying {
                    attempt,
                    max_attempts,
                    delay_ms: delay.as_millis() as u64,
                }));
            }
        }));

        // Running count of output tokens produced by the current turn.
        let mut output_token_count: u32 = 0;

        match client.chat_completion_turn_streaming(
            ChatTurnRequest {
                model,
                messages: &messages,
                tools: &tools,
                thinking_effort,
                on_retry: &mut retry_cb,
                cancel_rx: Some(cancel_rx),
                previous_response_id: prev_resp_id.as_deref(),
                tool_results: &tool_results,
                programmatic_tool_calling: client.supports_programmatic_tool_calling(model),
            },
            &mut |event| {
                match event {
                    StreamEvent::Answer(text) => {
                        if let Some(enc) = &encoding {
                            output_token_count += enc.count(&text) as u32;
                        }
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::OutputChunk {
                                request_id,
                                stream: OutputStream::Answer,
                                data: text.into_bytes(),
                            },
                        ));
                        // Let the UI update its live token display on every
                        // chunk so the count feels responsive.
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::LiveOutputTokenCount {
                                request_id,
                                output_tokens: output_token_count,
                            },
                        ));
                    }
                    StreamEvent::Reasoning(text) => {
                        if let Some(enc) = &encoding {
                            output_token_count += enc.count(&text) as u32;
                        }
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::OutputChunk {
                                request_id,
                                stream: OutputStream::Reasoning,
                                data: text.into_bytes(),
                            },
                        ));
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::LiveOutputTokenCount {
                                request_id,
                                output_tokens: output_token_count,
                            },
                        ));
                    }
                }
                Ok(())
            },
        ) {
            Ok(ChatTurnResult::FinalText(final_text)) => {
                debug!(
                    session_id = ctx.session_id,
                    turn = turn_iter,
                    response_len = final_text.content.len(),
                    reasoning = final_text.reasoning.as_deref().unwrap_or_default(),
                    "model returned final text",
                );
                let token_usage = final_text.usage;
                accumulate_token_usage(session, &token_usage, turn_iter, ctx);
                broadcast_token_usage(ctx, session);
                session.set_assistant_response(
                    current_turn_id,
                    Some(final_text.content),
                    final_text.reasoning,
                    Vec::new(),
                    token_usage,
                );
                finalize_and_broadcast_turn(session, ctx, current_turn_id)?;
                tool_results.clear();
                return Ok(false);
            }
            Ok(ChatTurnResult::ToolUse(tool_use)) => {
                let token_usage = tool_use.usage;
                accumulate_token_usage(session, &token_usage, turn_iter, ctx);
                broadcast_token_usage(ctx, session);
                session.set_assistant_response(
                    current_turn_id,
                    tool_use.content.clone(),
                    tool_use.reasoning.clone(),
                    tool_use
                        .tool_calls
                        .iter()
                        .map(|tc| AssistantToolCallRecord {
                            call_id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments_json: tc.arguments_json.clone(),
                        })
                        .collect(),
                    token_usage,
                );
                broadcast_turn_appended(&ctx.cmd_tx, session, current_turn_id);
                // Store response_id for chaining tool results back to this turn
                prev_resp_id = tool_use.response_id;
                tool_results.clear();

                // Partition tool calls into mutators and concurrent.
                let (mutators, concurrent): (Vec<_>, Vec<_>) =
                    tool_use.tool_calls.into_iter().partition(|tc| {
                        matches!(
                            tc.name.as_str(),
                            "load_tools" | "unload_tools" | "set_working_dir"
                        )
                    });

                // ── Phase 1: Session-mutating tools (serial) ────────
                for tool_call in mutators.into_iter() {
                    if is_cancelled_once(cancel_rx) {
                        return Ok(true);
                    }

                    if let Err(e) =
                        ctx.cmd_tx
                            .send(SessionCommand::Broadcast(DaemonMessage::ToolCallStarted {
                                request_id,
                                call_id: tool_call.id.clone(),
                                tool_name: tool_call.name.clone(),
                                arguments_json: tool_call.arguments_json.clone(),
                            }))
                    {
                        warn!(%request_id, call_id = %tool_call.id, error = %e, "failed to broadcast ToolCallStarted");
                    }

                    let tool_timeout =
                        determine_tool_timeout(&tool_call.name).unwrap_or(Duration::from_secs(60));

                    if ctx
                        .cmd_tx
                        .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                            tool_call.name.clone(),
                        )))
                        .is_err()
                    {
                        return Ok(false);
                    }

                    debug!(
                        session_id = ctx.session_id,
                        turn = turn_iter,
                        tool_name = %tool_call.name,
                        tool_call_id = %tool_call.id,
                        args_preview = %(&tool_call.arguments_json[..tool_call.arguments_json.len().min(200)]),
                        "executing tool (serial)",
                    );

                    let (image_tx, image_rx) = mpsc::channel::<PreparedImage>();
                    let turn_working_dir = session.config.working_dir.clone();
                    let mut output = execute_tool_with_timeout(
                        &tool_call,
                        None,
                        turn_working_dir.as_deref(),
                        tool_timeout,
                        request_id,
                        session,
                        cancel_rx,
                        ctx,
                        Some(image_tx),
                    );

                    if let Ok(image) = image_rx.try_recv() {
                        emit_image(
                            &ctx.cmd_tx,
                            image,
                            Some(tool_call.id.clone()),
                            session,
                            current_turn_id,
                        );
                    }

                    finish_tool_call(
                        request_id,
                        session,
                        &tool_call,
                        &mut output,
                        ctx,
                        current_turn_id,
                    );
                    collect_tool_result(CollectToolResultParams {
                        tool_results: &mut tool_results,
                        session,
                        tool_call: &tool_call,
                        output: &output,
                        known_hint_paths: &mut known_hint_paths,
                        pending_hints: &mut pending_hints,
                    });
                }

                // ── Phase 2: All remaining tools (concurrent) ───────
                if !concurrent.is_empty() {
                    for tc in concurrent.iter() {
                        if let Err(e) = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::ToolCallStarted {
                                request_id,
                                call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                arguments_json: tc.arguments_json.clone(),
                            },
                        )) {
                            warn!(%request_id, call_id = %tc.id, error = %e, "failed to broadcast ToolCallStarted");
                        }
                    }

                    if ctx
                        .cmd_tx
                        .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                            "(parallel)".into(),
                        )))
                        .is_err()
                    {
                        return Ok(false);
                    }

                    debug!(
                        session_id = ctx.session_id,
                        turn = turn_iter,
                        count = concurrent.len(),
                        "dispatching {} tools concurrently",
                        concurrent.len(),
                    );

                    let cancel_flag = Arc::new(AtomicBool::new(false));

                    let tool_ctx = ToolContext {
                        session_id: ctx.session_id,
                        db: Arc::clone(&ctx.db),
                        daemon_tx: ctx.daemon_tx.clone(),
                        active_tool_groups: session.config.active_tool_groups.clone(),
                        reasoning_effort: session.config.reasoning_effort,
                        selected_model: session.config.selected_model.clone(),
                        working_dir: session.config.working_dir.clone(),
                        cancelled: Arc::clone(&cancel_flag),
                    };

                    let cmd_tx = ctx.cmd_tx.clone();
                    let reg = Arc::clone(&ctx.tool_registry);

                    let handles: Vec<_> = concurrent
                        .into_iter()
                        .map(|tool_call| {
                            let timeout = determine_tool_timeout(&tool_call.name);
                            let invocation_description = reg.describe_invocation(&tool_call);
                            let call_info = (
                                tool_call.id.clone(),
                                tool_call.name.clone(),
                                tool_call.arguments_json.clone(),
                                Instant::now(),
                                invocation_description.clone(),
                            );
                            let handle = spawn_single_tool(SpawnToolArgs {
                                tool_call,
                                timeout,
                                request_id,
                                registry: Arc::clone(&reg),
                                cmd_tx: cmd_tx.clone(),
                                x_credentials: None,
                                working_dir: session.config.working_dir.clone(),
                                ctx: tool_ctx.clone(),
                                invocation_description,
                            });
                            (call_info, handle)
                        })
                        .collect();

                    if is_cancelled_once(cancel_rx) {
                        cancel_flag.store(true, Ordering::Relaxed);
                    }
                    for (
                        (call_id, tool_name, arguments_json, tool_start, invocation_description),
                        handle,
                    ) in handles.into_iter()
                    {
                        if is_cancelled_once(cancel_rx) {
                            cancel_flag.store(true, Ordering::Relaxed);
                        }

                        let ToolHandle {
                            tool_call,
                            mut output,
                            image,
                        } = handle.join().unwrap_or_else(|_| ToolHandle {
                            tool_call: ChatToolCall {
                                id: call_id,
                                name: tool_name.clone(),
                                arguments_json,
                                caller: None,
                            },
                            output: ToolOutput {
                                content: "tool thread panicked".to_string(),
                                is_error: true,
                                invocation_description,
                            },
                            image: None,
                        });

                        let elapsed = tool_start.elapsed();

                        debug!(
                            session_id = ctx.session_id,
                            turn = turn_iter,
                            tool_name = %tool_call.name,
                            elapsed_ms = elapsed.as_millis(),
                            result_len = output.content.len(),
                            is_error = output.is_error,
                            "tool finished (concurrent)",
                        );

                        if let Some(image) = image {
                            emit_image(
                                &ctx.cmd_tx,
                                image,
                                Some(tool_call.id.clone()),
                                session,
                                current_turn_id,
                            );
                        }

                        finish_tool_call(
                            request_id,
                            session,
                            &tool_call,
                            &mut output,
                            ctx,
                            current_turn_id,
                        );
                        collect_tool_result(CollectToolResultParams {
                            tool_results: &mut tool_results,
                            session,
                            tool_call: &tool_call,
                            output: &output,
                            known_hint_paths: &mut known_hint_paths,
                            pending_hints: &mut pending_hints,
                        });
                    }
                }
            }
            Err(tai_proto::InferenceError::Cancelled) => {
                return Ok(true);
            }
            Err(e) => {
                // Finalize the turn so the session doesn't have an orphaned
                // open turn that confuses the LLM on the next request.
                if matches!(&e, tai_proto::InferenceError::TruncatedToolCall { .. }) {
                    tracing::warn!(?e, "truncated tool call, finalizing turn gracefully");
                    session.set_assistant_response(
                        current_turn_id,
                        Some(format!("[tool call truncated: {e}]")),
                        None,
                        Vec::new(),
                        None,
                    );
                    finalize_and_broadcast_turn(session, ctx, current_turn_id)?;
                    tool_results.clear();
                    return Ok(false);
                }
                return Err(e.into());
            }
        }

        // Advance the turn counter for the next iteration.
        turn_iter += 1;
    }
}

/// Persist the finalized turn to the database and broadcast the
/// [`DaemonMessage::TurnFinalized`] event to all connected clients.
fn finalize_and_broadcast_turn(
    session: &mut SessionState,
    ctx: &RequestContext,
    current_turn_id: u32,
) -> io::Result<()> {
    session.finalize_turn(&ctx.db, ctx.session_id, current_turn_id)?;
    if let Some(turn) = session.turns.get(&current_turn_id) {
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::TurnFinalized {
                turn_id: current_turn_id,
                turn: turn.clone(),
            }));
    }
    Ok(())
}

fn finish_tool_call(
    request_id: u32,
    session: &mut SessionState,
    tool_call: &ChatToolCall,
    output: &mut ToolOutput,
    ctx: &RequestContext,
    turn_id: u32,
) {
    let is_error = output.is_error;
    let content = output.content.clone();
    let invocation_description = output.invocation_description.clone();

    session.add_tool_result(
        turn_id,
        tool_call.id.clone(),
        tool_call.name.clone(),
        content.clone(),
        is_error,
        invocation_description,
    );

    broadcast_turn_appended(&ctx.cmd_tx, session, turn_id);

    let event = if is_error {
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            error: content,
        }
    } else {
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
        }
    };
    if let Err(e) = ctx.cmd_tx.send(SessionCommand::Broadcast(event)) {
        warn!(%request_id, error = %e, "failed to broadcast tool call finished/failed event");
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_tool_with_timeout(
    tool_call: &crate::providers::types::ChatToolCall,
    x_credentials: Option<&ServiceCredential>,
    working_dir: Option<&Path>,
    timeout_dur: Duration,
    request_id: u32,
    session: &mut SessionState,
    cancel_rx: &mpsc::Receiver<()>,
    ctx: &RequestContext,
    image_tx: Option<mpsc::Sender<PreparedImage>>,
) -> ToolOutput {
    let format = match &tool_call.caller {
        Some(caller) if caller.kind == "program" => ToolOutputFormat::Json,
        _ => ToolOutputFormat::Text,
    };
    // Capture start time for tool execution metrics.
    // Meta-tools (load_tools, unload_tools) that need mutable session state
    // return early and are not timed — only the registry-executed path below
    // records metrics.
    let exec_start = std::time::Instant::now();
    match tool_call.name.as_str() {
        "load_tools" => {
            let result = crate::tools::groups::execute_load_tools(
                &mut session.config.active_tool_groups,
                &tool_call.arguments_json,
            );
            // Broadcast updated session state so the client (e.g. TUI status
            // bar) picks up the new active_tool_groups immediately.
            let _ = ctx
                .cmd_tx
                .send(SessionCommand::Broadcast(DaemonMessage::SessionState {
                    session_id: ctx.session_id,
                    title: session.config.title.clone(),
                    selected_model: session.config.selected_model.clone(),
                    parent_session_id: session.config.parent_session_id,
                    working_dir: session
                        .config
                        .working_dir
                        .as_ref()
                        .map(|p| p.display().to_string()),
                    max_turns: session.config.max_turns,
                    turns: session.turns.clone(),
                    active_tool_groups: session.config.active_tool_groups.iter().cloned().collect(),
                    token_usage: Some(session.config.accumulated_usage),
                    context_window: session.config.context_window,
                    last_prompt_tokens: session.config.last_prompt_tokens,
                    status: session.config.status.clone(),
                }));
            let _ = ctx
                .daemon_tx
                .send(crate::daemon::DaemonCommand::UpdateMetadata {
                    session_id: ctx.session_id,
                    metadata: SessionMetadata::from(&*session),
                });
            return ToolOutput {
                content: result,
                is_error: false,
                invocation_description: String::new(),
            };
        }
        "unload_tools" => {
            let result = crate::tools::groups::execute_unload_tools(
                &mut session.config.active_tool_groups,
                &tool_call.arguments_json,
            );
            // Broadcast updated session state so the client picks up the
            // new active_tool_groups immediately.
            let _ = ctx
                .cmd_tx
                .send(SessionCommand::Broadcast(DaemonMessage::SessionState {
                    session_id: ctx.session_id,
                    title: session.config.title.clone(),
                    selected_model: session.config.selected_model.clone(),
                    parent_session_id: session.config.parent_session_id,
                    working_dir: session
                        .config
                        .working_dir
                        .as_ref()
                        .map(|p| p.display().to_string()),
                    max_turns: session.config.max_turns,
                    turns: session.turns.clone(),
                    active_tool_groups: session.config.active_tool_groups.iter().cloned().collect(),
                    token_usage: Some(session.config.accumulated_usage),
                    context_window: session.config.context_window,
                    last_prompt_tokens: session.config.last_prompt_tokens,
                    status: session.config.status.clone(),
                }));
            let _ = ctx
                .daemon_tx
                .send(crate::daemon::DaemonCommand::UpdateMetadata {
                    session_id: ctx.session_id,
                    metadata: SessionMetadata::from(&*session),
                });
            return ToolOutput {
                content: result,
                is_error: false,
                invocation_description: String::new(),
            };
        }
        "set_working_dir" => {
            // Meta-tools receive raw JSON rather than typed Args because they
            // are not registered as `Tool` impls — they run inline here with
            // direct `&mut SessionState` access, avoiding the indirection of
            // the ToolRegistry dispatch path.
            let args: serde_json::Value = match serde_json::from_str(&tool_call.arguments_json) {
                Ok(a) => a,
                Err(e) => {
                    return ToolOutput {
                        content: format!("invalid arguments: {e}"),
                        is_error: true,
                        invocation_description: String::new(),
                    };
                }
            };
            let path_str = match args.get("path").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => {
                    return ToolOutput {
                        content: "missing required argument: path".to_string(),
                        is_error: true,
                        invocation_description: String::new(),
                    };
                }
            };
            // Resolve relative to the current session working directory
            // (or process cwd if none is set yet).  Tilde expansion (`~`
            // → home directory) is handled inside resolve_path.
            let resolved = resolve_path(path_str, working_dir);
            // canonicalize() resolves symlinks and normalizes the path.
            // This serves two purposes:
            //   1. Prevents symlink-escape attacks that would let a model
            //      redirect subsequent file ops outside the intended tree.
            //   2. Ensures the path actually exists so subsequent tools
            //      (find, grep, read_file) don't silently operate on
            //      a directory that was mistyped or never created.
            // The downside is that `set_working_dir` cannot target a path
            // that doesn't exist yet — this is an intentional tradeoff.
            let canonical = match resolved.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    return ToolOutput {
                        content: format!(
                            "path '{}' does not exist or cannot be resolved: {e}",
                            resolved.display()
                        ),
                        is_error: true,
                        invocation_description: String::new(),
                    };
                }
            };

            info!(
                session_id = ctx.session_id,
                path = %canonical.display(),
                "set_working_dir: changing session working directory",
            );

            session.config.working_dir = Some(canonical.clone());
            // Invalidating cached skills so they are re-discovered from
            // the new working directory on the next agent-loop turn.
            session.discovered_skills = None;

            // Notify session subscribers (e.g. TUI) so the status bar
            // updates to reflect the new working directory immediately.
            let path_str = canonical.to_string_lossy().into_owned();
            debug!(
                session_id = ctx.session_id,
                path = %path_str,
                "set_working_dir: broadcasting SessionWorkingDirSet",
            );
            if let Err(e) = ctx.cmd_tx.send(SessionCommand::Broadcast(
                DaemonMessage::SessionWorkingDirSet {
                    session_id: ctx.session_id,
                    path: Some(path_str),
                },
            )) {
                warn!(session_id = ctx.session_id, error = %e, "failed to broadcast SessionWorkingDirSet");
            }

            // Notify the daemon so the TUI/inspector can reflect the change
            // immediately without waiting for the next persist cycle.
            if let Err(e) = ctx
                .daemon_tx
                .send(crate::daemon::DaemonCommand::UpdateMetadata {
                    session_id: ctx.session_id,
                    metadata: SessionMetadata::from(&*session),
                })
            {
                warn!(session_id = ctx.session_id, error = %e, "failed to notify daemon of working dir change");
            }

            // Persist the updated working_dir so it survives a daemon restart.
            // This is the same pattern used by load_tools/unload_tools.
            let record: SessionRecord = (&*session).into();
            if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
                warn!(
                    session_id = ctx.session_id,
                    error = %e,
                    "set_working_dir: failed to persist session",
                );
            }

            return ToolOutput {
                content: format!("Working directory changed to '{}'", canonical.display()),
                is_error: false,
                invocation_description: String::new(),
            };
        }
        _ => {}
    }

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (output_tx, output_rx) = std::sync::mpsc::channel();
    let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();

    // Forward streaming output to subscribers as it arrives, exiting when
    // the output channel is disconnected (tool finished) or a kill signal
    // arrives (main loop exited).
    spawn_forwarding_thread(
        ctx.cmd_tx.clone(),
        request_id,
        tool_call.id.clone(),
        output_rx,
        kill_rx,
    );

    // Drop guard: when the main loop exits (for any reason), signal the
    // forwarder to stop so it doesn't orphan waiting on output_rx.
    struct KillGuard(mpsc::Sender<()>);
    impl Drop for KillGuard {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let _kill_guard = KillGuard(kill_tx);

    // Tool execution thread
    let tc = tool_call.clone();
    let tr = Arc::clone(&ctx.tool_registry);
    let xc = x_credentials.cloned();
    let c = working_dir.map(|p| p.to_path_buf());
    let tool_ctx = crate::tools::context::ToolContext {
        session_id: ctx.session_id,
        db: Arc::clone(&ctx.db),
        daemon_tx: ctx.daemon_tx.clone(),
        active_tool_groups: session.config.active_tool_groups.clone(),
        reasoning_effort: session.config.reasoning_effort,
        selected_model: session.config.selected_model.clone(),
        working_dir: working_dir.map(|p| p.to_path_buf()),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    std::thread::spawn(move || {
        let result = tr.execute_streaming_json(
            &tc,
            format,
            output_tx,
            xc.as_ref(),
            c.as_deref(),
            Some(&tool_ctx),
            image_tx,
        );
        let _ = result_tx.send(result);
    });

    // Event-driven wait loop: blocked on recv_timeout for most of the
    // interval, waking briefly at each check point to see whether the
    // caller has cancelled the request or the tool has finished.
    let deadline = std::time::Instant::now() + timeout_dur;
    let check_interval = Duration::from_millis(200);
    loop {
        // Check cancellation before each blocking wait so that a cancel
        // sent between tool start and our first recv_timeout is honoured
        // immediately rather than waiting up to check_interval.
        if is_cancelled_once(cancel_rx) {
            crate::metrics::record_tool_execution(
                &tool_call.name,
                exec_start.elapsed().as_secs_f64(),
                true,
            );
            return ToolOutput {
                content: format!("tool '{}' cancelled", tool_call.name),
                is_error: true,
                invocation_description: String::new(),
            };
        }

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            crate::metrics::record_tool_execution(
                &tool_call.name,
                exec_start.elapsed().as_secs_f64(),
                true,
            );
            return ToolOutput {
                content: format!(
                    "tool '{}' timed out after {}s",
                    tool_call.name,
                    timeout_dur.as_secs()
                ),
                is_error: true,
                invocation_description: String::new(),
            };
        }

        match result_rx.recv_timeout(remaining.min(check_interval)) {
            Ok(Ok(output)) => {
                crate::metrics::record_tool_execution(
                    &tool_call.name,
                    exec_start.elapsed().as_secs_f64(),
                    output.is_error,
                );
                return output;
            }
            Ok(Err(e)) => {
                crate::metrics::record_tool_execution(
                    &tool_call.name,
                    exec_start.elapsed().as_secs_f64(),
                    true,
                );
                return ToolOutput {
                    content: e.to_string(),
                    is_error: true,
                    invocation_description: String::new(),
                };
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                crate::metrics::record_tool_execution(
                    &tool_call.name,
                    exec_start.elapsed().as_secs_f64(),
                    true,
                );
                return ToolOutput {
                    content: "tool execution thread panicked".to_string(),
                    is_error: true,
                    invocation_description: String::new(),
                };
            }
        }
    }
}

fn build_chat_request_messages(
    session: &SessionState,
    system_prompt: Option<&str>,
) -> Vec<ChatRequestMessage> {
    let mut messages = Vec::new();

    // Prepend system prompt and context if provided.
    if let Some(prompt) = system_prompt {
        messages.push(ChatRequestMessage::simple("system", prompt.to_string()));
    }

    for turn in session.turns.values() {
        if turn.undone {
            continue;
        }
        // User message
        if let Some(text) = &turn.user_text {
            messages.push(ChatRequestMessage::simple("user", text.clone()));
        }
        // Assistant message (text or tool calls)
        let has_tool_calls = !turn.tool_calls.is_empty();
        if turn.assistant_text.is_some() || has_tool_calls || turn.assistant_reasoning.is_some() {
            let tool_calls = if has_tool_calls {
                Some(
                    turn.tool_calls
                        .iter()
                        .map(|tc| AssistantToolCall {
                            id: tc.call_id.clone(),
                            kind: "function".to_string(),
                            function: AssistantToolFunction {
                                name: tc.name.clone(),
                                arguments: tc.arguments_json.clone(),
                            },
                        })
                        .collect(),
                )
            } else {
                None
            };
            messages.push(ChatRequestMessage {
                role: "assistant",
                content: turn.assistant_text.clone(),
                tool_call_id: None,
                tool_calls,
                reasoning_content: turn.assistant_reasoning.clone(),
                reasoning: None,
                reasoning_text: None,
            });
        }
        // Tool result messages
        for tr in &turn.tool_results {
            messages.push(ChatRequestMessage {
                role: "tool",
                content: Some(tr.content.clone()),
                tool_call_id: Some(tr.call_id.clone()),
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
            });
        }
    }
    messages
}

pub const REQUEST_IMAGE_BYTES: &[u8] = include_bytes!("../assets/dua.jpg");
pub const REQUEST_IMAGE_MIME_TYPE: &str = "image/jpeg";
pub const REQUEST_IMAGE_WIDTH: u32 = 640;
pub const REQUEST_IMAGE_HEIGHT: u32 = 640;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::DaemonCommand;
    use crate::openai::AssistantToolCall;
    use crate::providers::InferenceProvider;
    use crate::providers::test_util::make_test_provider;
    use crate::tools::context::ToolContext;
    use crate::tools::{Tool, ToolExecError, ToolRegistry};
    use std::sync::mpsc;

    fn make_session_with_turns() -> SessionState {
        let mut session = SessionState::empty();
        let (tid0, _) = session.start_turn(Some("hello".into()));
        session.set_assistant_response(tid0, Some("hi".into()), None, vec![], None);
        session
    }

    #[test]
    fn build_chat_request_messages_empty() {
        let session = SessionState::empty();
        let result = build_chat_request_messages(&session, None);
        assert!(result.is_empty());
    }

    #[test]
    fn build_chat_request_messages_with_system_prompt() {
        let session = SessionState::empty();
        let result = build_chat_request_messages(&session, Some("system prompt"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content.as_deref(), Some("system prompt"));
    }

    #[test]
    fn build_chat_request_messages_user_and_assistant() {
        let session = make_session_with_turns();
        let result = build_chat_request_messages(&session, None);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content.as_deref(), Some("hello"));
        assert_eq!(result[1].role, "assistant");
        assert_eq!(result[1].content.as_deref(), Some("hi"));
    }

    #[test]
    fn build_chat_request_messages_with_tool_calls() {
        let mut session = SessionState::empty();
        let (tid, _) = session.start_turn(Some("list files".into()));
        session.set_assistant_response(
            tid,
            Some("thinking".into()),
            None,
            vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "ls".into(),
                arguments_json: r#"{"path": "."}"#.into(),
            }],
            None,
        );
        session.add_tool_result(
            tid,
            "call_1".into(),
            "ls".into(),
            "file.txt".into(),
            false,
            String::new(),
        );

        let result = build_chat_request_messages(&session, None);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[1].role, "assistant");
        assert!(result[1].tool_calls.is_some());
        assert_eq!(result[2].role, "tool");
        assert_eq!(result[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn build_chat_request_messages_skips_undone_turns() {
        let mut session = SessionState::empty();
        let (tid0, _) = session.start_turn(Some("visible".into()));
        session.set_assistant_response(tid0, Some("ok".into()), None, vec![], None);
        let (tid1, _) = session.start_turn(Some("hidden".into()));
        session.set_assistant_response(tid1, Some("nope".into()), None, vec![], None);
        if let Some(turn) = session.turns.get_mut(&tid1) {
            turn.undone = true;
        }

        let result = build_chat_request_messages(&session, None);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content.as_deref(), Some("visible"));
    }

    // -- Cancellation helper tests -----------------------------------------

    #[test]
    fn is_cancelled_once_no_signal() {
        let (_tx, rx) = mpsc::channel::<()>();
        assert!(!is_cancelled_once(&rx));
    }

    #[test]
    fn is_cancelled_once_with_signal() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        assert!(is_cancelled_once(&rx));
    }

    // -- broadcast_turn_appended tests -----------------------------------

    #[test]
    fn broadcast_turn_appended_sends_when_turn_exists() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let mut session = SessionState::empty();
        let (turn_id, _) = session.start_turn(Some("hello".into()));

        broadcast_turn_appended(&tx, &session, turn_id);

        match rx.try_recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::TurnAppended { turn_id: id, .. })) => {
                assert_eq!(id, turn_id);
            }
            Ok(_) => panic!("expected TurnAppended broadcast, got different command"),
            Err(e) => panic!("expected TurnAppended broadcast, got error: {e}"),
        }
    }

    #[test]
    fn broadcast_turn_appended_no_turn_no_broadcast() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let session = SessionState::empty();

        broadcast_turn_appended(&tx, &session, 999);

        assert!(rx.try_recv().is_err(), "expected no message");
    }

    #[test]
    fn broadcast_turn_appended_disconnected_receiver_no_panic() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let mut session = SessionState::empty();
        let (turn_id, _) = session.start_turn(Some("hello".into()));
        drop(rx);

        // Disconnected receiver should not panic — warn! is logged instead.
        broadcast_turn_appended(&tx, &session, turn_id);
    }

    // -- resolve_reasoning_effort tests ------------------------------------

    #[test]
    fn resolve_reasoning_effort_off_returns_off() {
        let provider = make_test_provider();
        let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, ThinkingEffort::Off);
        assert_eq!(result, ThinkingEffort::Off);
    }

    #[test]
    fn resolve_reasoning_effort_unknown_provider_disables() {
        let provider = make_test_provider();
        let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, ThinkingEffort::Low);
        // "test-stub" slug is not in the catalog, so reasoning is unsupported.
        assert_eq!(result, ThinkingEffort::Off);
    }

    #[test]
    fn resolve_reasoning_effort_openai_supported_model_preserves() {
        let config = crate::openai::ServiceConfig::default();
        let client = crate::openai::OpenAiClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_openai(client);

        let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, ThinkingEffort::High);
        assert_eq!(result, ThinkingEffort::High);
    }

    #[test]
    fn resolve_reasoning_effort_openai_unsupported_model_disables() {
        let config = crate::openai::ServiceConfig::default();
        let client = crate::openai::OpenAiClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_openai(client);

        let result = resolve_reasoning_effort(&provider, "gpt-4.1", 1, 0, ThinkingEffort::Medium);
        assert_eq!(result, ThinkingEffort::Off);
    }

    // -- estimate_prompt_tokens tests ------------------------------------

    #[test]
    fn estimate_prompt_tokens_empty() {
        let (encoding, estimated) = estimate_prompt_tokens("gpt-4", &[], &[]);
        assert!(encoding.is_some());
        assert_eq!(estimated, 0);
    }

    #[test]
    fn estimate_prompt_tokens_counts_content() {
        let messages = vec![
            ChatRequestMessage::simple("user", "hello world".into()),
            ChatRequestMessage::simple("assistant", "hi there".into()),
        ];
        let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
        assert!(
            estimated > 0,
            "expected positive token count, got {estimated}"
        );
    }

    #[test]
    fn estimate_prompt_tokens_counts_reasoning_content() {
        let messages = vec![
            ChatRequestMessage::simple("user", "hello".into()),
            ChatRequestMessage {
                role: "assistant",
                content: Some("visible".into()),
                reasoning_content: Some("thinking deep...".into()),
                tool_call_id: None,
                tool_calls: None,
                reasoning: None,
                reasoning_text: None,
            },
        ];
        let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
        assert!(
            estimated > 0,
            "expected positive token count, got {estimated}"
        );
    }

    #[test]
    fn estimate_prompt_tokens_counts_tool_call_metadata() {
        let messages = vec![ChatRequestMessage {
            role: "assistant",
            content: None,
            tool_calls: Some(vec![AssistantToolCall {
                id: "call_abc".into(),
                kind: "function".into(),
                function: AssistantToolFunction {
                    name: "read_file".into(),
                    arguments: r#"{"path": "/etc/hosts"}"#.into(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
        }];
        let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
        assert!(
            estimated > 0,
            "expected positive token count, got {estimated}"
        );
    }

    #[test]
    fn estimate_prompt_tokens_includes_tool_defs() {
        let tools = vec![ChatToolDefinition::function(
            "read_file",
            "Read a file from disk",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            }),
        )];
        let messages = vec![ChatRequestMessage::simple("user", "read file".into())];
        let (_, with_tools) = estimate_prompt_tokens("gpt-4", &messages, &tools);
        let (_, without_tools) = estimate_prompt_tokens("gpt-4", &messages, &[]);
        assert!(
            with_tools > without_tools,
            "tool defs should increase token count: {with_tools} <= {without_tools}",
        );
    }

    #[test]
    fn estimate_prompt_tokens_unknown_model_falls_back() {
        let messages = vec![ChatRequestMessage::simple("user", "hello".into())];
        let (encoding, estimated) =
            estimate_prompt_tokens("nonexistent-model-9000", &messages, &[]);
        assert!(encoding.is_some(), "should fall back to cl100k_base");
        assert!(estimated > 0);
    }

    // -- execute_tool_with_timeout tests -----------------------------------

    struct FastTestTool;

    impl Tool for FastTestTool {
        type Args = serde_json::Value;
        type Return = String;
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "_test_fast"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that completes immediately"
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", self.description())
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            Ok("fast result".into())
        }
    }

    struct BlockingTestTool {
        proceed: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl Tool for BlockingTestTool {
        type Args = serde_json::Value;
        type Return = String;
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "_test_blocking"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that blocks until proceed"
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", self.description())
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            Ok("ignored".into())
        }
        fn execute_streaming(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _output_tx: mpsc::Sender<Vec<u8>>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            if let Some(rx) = self.proceed.lock().unwrap().take() {
                let _ = rx.recv();
            }
            Ok("blocked tool done".into())
        }
    }

    fn run_exec_tool(
        tool: impl Tool + 'static,
        tool_name: &str,
        tool_args: &str,
        timeout_dur: Duration,
        cancel_rx: mpsc::Receiver<()>,
    ) -> (ToolOutput, mpsc::Receiver<SessionCommand>) {
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();

        let dir = tempfile::tempdir().expect("tempdir");
        let db = redb::Database::create(dir.path().join("test.redb")).expect("Database");

        let mut session = SessionState::empty();

        let mut registry = ToolRegistry::new();
        registry.register(tool);
        let registry = registry.build();

        let tool_call = crate::providers::types::ChatToolCall {
            id: "call_test".into(),
            name: tool_name.into(),
            arguments_json: tool_args.into(),
            caller: None,
        };

        let ctx = RequestContext {
            cmd_tx,
            session_id: 1,
            db: Arc::new(db),
            tool_registry: registry,
            daemon_tx,
            max_turns_default: 25,
        };
        let result = execute_tool_with_timeout(
            &tool_call,
            None,
            None,
            timeout_dur,
            1,
            &mut session,
            &cancel_rx,
            &ctx,
            None,
        );
        (result, cmd_rx)
    }

    #[test]
    fn execute_tool_normal_completion() {
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (result, _cmd_rx) = run_exec_tool(
            FastTestTool,
            "_test_fast",
            "{}",
            Duration::from_secs(60),
            cancel_rx,
        );
        assert!(!result.is_error, "expected success: {}", result.content);
        assert!(result.content.contains("fast result"), "{}", result.content);
    }

    #[test]
    fn execute_tool_cancelled_before_execution() {
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        cancel_tx.send(()).expect("send cancel");
        drop(cancel_tx);

        let (result, _cmd_rx) = run_exec_tool(
            FastTestTool,
            "_test_fast",
            "{}",
            Duration::from_secs(60),
            cancel_rx,
        );
        assert!(result.is_error, "expected error: {}", result.content);
        assert!(result.content.contains("cancelled"), "{}", result.content);
    }

    #[test]
    fn execute_tool_timeout() {
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (proceed_tx, proceed_rx) = mpsc::channel::<()>();

        let (result, _cmd_rx) = run_exec_tool(
            BlockingTestTool {
                proceed: std::sync::Mutex::new(Some(proceed_rx)),
            },
            "_test_blocking",
            "{}",
            Duration::from_millis(100),
            cancel_rx,
        );

        assert!(result.is_error, "expected error: {}", result.content);
        assert!(result.content.contains("timed out"), "{}", result.content);

        drop(proceed_tx);
    }

    struct StreamingTestTool;

    impl Tool for StreamingTestTool {
        type Args = serde_json::Value;
        type Return = String;
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "_test_streaming"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that sends streaming output"
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", self.description())
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            Ok("exec result".into())
        }
        fn execute_streaming(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            output_tx: mpsc::Sender<Vec<u8>>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            let _ = output_tx.send(b"streamed payload".to_vec());
            Ok("streaming done".into())
        }
    }

    #[test]
    fn execute_tool_forwards_streaming_output() {
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (result, cmd_rx) = run_exec_tool(
            StreamingTestTool,
            "_test_streaming",
            "{}",
            Duration::from_secs(60),
            cancel_rx,
        );

        assert!(!result.is_error, "expected success: {}", result.content);
        assert!(
            result.content.contains("streaming done"),
            "{}",
            result.content
        );

        // First chunk is the description string (from execute_streaming_json)
        match cmd_rx.recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk { data, .. })) => {
                assert_eq!(data, b"test tool that sends streaming output.");
            }
            Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
            Err(e) => panic!("channel disconnected while waiting for streaming output: {e}"),
        }
        // Second chunk is the actual payload from the tool's execute_streaming
        match cmd_rx.recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk { data, .. })) => {
                assert_eq!(data, b"streamed payload");
            }
            Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
            Err(e) => panic!("channel disconnected while waiting for streaming output: {e}"),
        }
    }

    // -- determine_tool_timeout tests ----------------------------------

    #[test]
    fn determine_tool_timeout_subsession_none() {
        assert!(determine_tool_timeout("spawn_subsession").is_none());
    }

    #[test]
    fn determine_tool_timeout_shell_300() {
        for name in &["sh", "nushell", "fish", "exec"] {
            assert_eq!(
                determine_tool_timeout(name),
                Some(Duration::from_secs(300)),
                "tool {name} should have 300s timeout",
            );
        }
    }

    #[test]
    fn determine_tool_timeout_default_60() {
        for name in &[
            "read_file",
            "write_file",
            "run_riscv",
            "grep",
            "http_request",
        ] {
            assert_eq!(
                determine_tool_timeout(name),
                Some(Duration::from_secs(60)),
                "tool {name} should have 60s timeout",
            );
        }
    }

    // -- spawn_single_tool tests ---------------------------------------

    fn run_spawn_single_tool(
        tool: impl Tool + 'static,
        tool_name: &str,
        tool_args: &str,
        timeout: Option<Duration>,
    ) -> ToolHandle {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
        let (_daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).expect("Database"));

        let mut registry = ToolRegistry::new();
        registry.register(tool);
        let registry = registry.build();

        let tool_call = ChatToolCall {
            id: "call_test".into(),
            name: tool_name.into(),
            arguments_json: tool_args.into(),
            caller: None,
        };

        let tool_ctx = ToolContext {
            session_id: 1,
            db,
            daemon_tx: _daemon_tx,
            active_tool_groups: std::collections::HashSet::new(),
            reasoning_effort: None,
            selected_model: None,
            working_dir: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let invocation_description = registry
            .describe_invocation_for(&tool_call.name, &tool_call.arguments_json)
            .unwrap_or_default();

        let handle = spawn_single_tool(SpawnToolArgs {
            tool_call,
            timeout,
            request_id: 1,
            registry,
            cmd_tx,
            x_credentials: None,
            working_dir: None,
            ctx: tool_ctx,
            invocation_description,
        });

        handle.join().expect("tool thread panicked")
    }

    #[test]
    fn spawn_single_tool_fast_returns_result() {
        let handle = run_spawn_single_tool(
            FastTestTool,
            "_test_fast",
            "{}",
            Some(Duration::from_secs(60)),
        );
        assert!(
            !handle.output.is_error,
            "expected success: {}",
            handle.output.content
        );
        assert!(
            handle.output.content.contains("fast result"),
            "{}",
            handle.output.content
        );
        assert!(handle.image.is_none(), "expected no image from fast tool");
    }

    #[test]
    fn spawn_single_tool_no_timeout_still_completes() {
        let handle = run_spawn_single_tool(FastTestTool, "_test_fast", "{}", None);
        assert!(
            !handle.output.is_error,
            "expected success: {}",
            handle.output.content
        );
        assert!(
            handle.output.content.contains("fast result"),
            "{}",
            handle.output.content
        );
    }

    // -- extract_json_string tests ------------------------------------------

    #[test]
    fn extract_json_string_gets_value() {
        let json = r#"{"name": "test-skill", "path": "src/main.rs"}"#;
        assert_eq!(
            extract_json_string(json, "name").as_deref(),
            Some("test-skill")
        );
        assert_eq!(
            extract_json_string(json, "path").as_deref(),
            Some("src/main.rs")
        );
    }

    #[test]
    fn extract_json_string_missing_key() {
        assert_eq!(extract_json_string(r#"{"other": "val"}"#, "name"), None);
    }

    #[test]
    fn extract_json_string_invalid_json() {
        assert_eq!(extract_json_string("not json", "name"), None);
    }

    // -- persist_loaded_skill tests -----------------------------------------

    #[test]
    fn persist_loaded_skill_adds_to_session() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".agents/skills/test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "\
---\n\
name: test-skill\n\
description: A test skill\n\
---\n\
Hello, this is the skill body.\n\
---\n",
        )
        .unwrap();

        let mut session = SessionState::empty();
        session.config.working_dir = Some(dir.path().to_path_buf());
        assert!(session.loaded_skill_bodies.is_empty());

        persist_loaded_skill(&mut session, "load_skill", r#"{"name": "test-skill"}"#);

        assert_eq!(session.loaded_skill_bodies.len(), 1);
        assert_eq!(session.loaded_skill_bodies[0].name, "test-skill");
        assert!(session.loaded_skill_bodies[0].body.contains("skill body"));
    }

    #[test]
    fn persist_loaded_skill_skips_non_load_skill() {
        let mut session = SessionState::empty();
        persist_loaded_skill(&mut session, "read_file", r#"{"path": "Cargo.toml"}"#);
        assert!(session.loaded_skill_bodies.is_empty());
    }

    #[test]
    fn persist_loaded_skill_skips_missing_name() {
        let mut session = SessionState::empty();
        session.config.working_dir = Some(PathBuf::from("/tmp"));
        persist_loaded_skill(&mut session, "load_skill", r#"{}"#);
        assert!(session.loaded_skill_bodies.is_empty());
    }

    #[test]
    fn persist_loaded_skill_skips_without_working_dir() {
        let mut session = SessionState::empty();
        persist_loaded_skill(&mut session, "load_skill", r#"{"name": "test-skill"}"#);
        assert!(session.loaded_skill_bodies.is_empty());
    }

    // -- build_system_content tests -----------------------------------------

    fn setup_build_system_content_session() -> (SessionState, Arc<ToolRegistry>, tempfile::TempDir)
    {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Project rules").unwrap();

        let mut registry = ToolRegistry::new();
        registry.register(FastTestTool);
        let registry = registry.build();

        let mut session = SessionState::empty();
        session.config.working_dir = Some(dir.path().to_path_buf());
        (session, registry, dir)
    }

    /// Call build_system_content with standard defaults derived from the
    /// session state and optional pending_hints overrides.
    fn test_build_content(
        session: &mut SessionState,
        registry: &ToolRegistry,
        pending_hints: &[String],
    ) -> Option<String> {
        build_system_content(
            SystemContentParams {
                working_dir: session.config.working_dir.as_deref(),
                context_config: &session.config.context_config,
                skills: &[],
                loaded_skill_bodies: &session.loaded_skill_bodies,
                tool_registry: registry,
                pending_hints,
            },
            &mut session.context_cache,
        )
    }

    #[test]
    fn build_system_content_with_working_dir() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        let content = test_build_content(&mut session, &registry, &[]);
        assert!(content.is_some());
        let content = content.unwrap();
        assert!(content.contains("Tool groups"));
        assert!(content.contains("core"));
        assert!(content.contains("Project rules"));
    }

    #[test]
    fn build_system_content_without_working_dir() {
        let mut session = SessionState::empty();
        let registry = ToolRegistry::new().build();
        let content = test_build_content(&mut session, &registry, &[]);
        assert!(content.is_none());
    }

    #[test]
    fn build_system_content_includes_loaded_skills() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        session.loaded_skill_bodies.push(LoadedSkill {
            name: "loaded-test".to_string(),
            body: "Loaded body text.".to_string(),
        });
        let content = test_build_content(&mut session, &registry, &[]);
        assert!(content.is_some());
        let content = content.unwrap();
        assert!(content.contains("Loaded skills"));
        assert!(content.contains("loaded-test"));
        assert!(content.contains("Loaded body text."));
    }

    #[test]
    fn build_system_content_populates_context_cache() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        assert!(session.context_cache.is_none());

        let _ = test_build_content(&mut session, &registry, &[]);
        assert!(
            session.context_cache.is_some(),
            "context_cache should be populated after first call"
        );
        let (fp, _) = session.context_cache.as_ref().unwrap();
        assert!(*fp > 0, "fingerprint should be non-zero");
    }

    #[test]
    fn build_system_content_includes_pending_hints() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        let pending_hints = vec!["Hint about subdirectory config.".to_string()];
        let content = test_build_content(&mut session, &registry, &pending_hints);
        assert!(content.is_some());
        let content = content.unwrap();
        assert!(content.contains("New context from project subdirectories"));
        assert!(content.contains("Hint about subdirectory config."));
    }
}
