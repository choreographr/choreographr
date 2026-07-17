use std::io::{self, BufReader};
use std::sync::mpsc;

use serde::Deserialize;
use tai_proto::ThinkingEffort;
use tai_proto::TokenUsage;
use tracing::debug;

use crate::openai::{ChatRequestMessage, ChatToolDefinition};
use crate::providers::StreamEvent;
use crate::providers::shared::MAX_TOOL_CALLS;
use crate::providers::types::ChatTurnResult;
use crate::retry;

use super::{
    ChatCompletionRequest, ChatCompletionResponse, MistralConfig, MistralError,
    build_message_payloads, build_tool_payloads, response_to_turn_result, thinking_payload,
};

/// Response from GET /v1/models.
#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
}

/// Path for the chat completions endpoint.
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

/// Path for listing models.
const MODELS_PATH: &str = "/v1/models";

/// Build the full URL for a given path.
/// Trims trailing slashes from base_url to avoid double-slash when joining.
fn endpoint_url(base_url: &str, path: &str) -> io::Result<String> {
    if !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must start with '/'",
        ));
    }
    Ok(format!("{}{}", base_url.trim_end_matches('/'), path))
}

/// Fetch the list of available models from the Mistral API.
pub(super) fn list_models_request(
    agent: &ureq::Agent,
    config: &MistralConfig,
    api_key: &str,
) -> Result<Vec<String>, MistralError> {
    let url = endpoint_url(&config.base_url, MODELS_PATH)?;
    let retry_cfg = retry::RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    };

    let response = retry::retry_loop(
        || {
            agent
                .get(&url)
                .header("Authorization", &format!("Bearer {}", api_key.trim()))
                .call()
        },
        &retry_cfg,
        &mut None,
        None,
    )?;

    let payload: ModelListResponse = response
        .into_body()
        .read_json()
        .map_err(|e| MistralError::Io(io::Error::other(e)))?;

    let models: Vec<String> = payload.data.into_iter().map(|m| m.id).collect();
    Ok(models)
}

/// Send a POST /v1/chat/completions request with retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn chat_completion_request(
    agent: &ureq::Agent,
    config: &MistralConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    thinking_effort: ThinkingEffort,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ChatTurnResult, MistralError> {
    let url = endpoint_url(&config.base_url, CHAT_COMPLETIONS_PATH)?;
    let retry_cfg = retry::RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    };

    let payloads = build_message_payloads(messages);
    let tool_payloads = if tools.is_empty() {
        None
    } else {
        Some(build_tool_payloads(tools))
    };

    let reasoning = thinking_payload(thinking_effort);

    let body = serde_json::to_value(&ChatCompletionRequest {
        model,
        messages: payloads,
        tools: tool_payloads,
        stream: false,
        max_tokens: Some(config.max_tokens),
        reasoning_effort: reasoning,
    })
    .map_err(io::Error::other)?;

    debug!(
        "Mistral chat completion request: {} messages, model={}",
        body["messages"].as_array().map_or(0, |a| a.len()),
        body["model"].as_str().unwrap_or("?"),
    );

    let response = retry::retry_loop(
        || {
            agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", api_key.trim()))
                .send_json(body.clone())
        },
        &retry_cfg,
        on_retry,
        cancel_rx,
    )?;

    let payload: ChatCompletionResponse = response
        .into_body()
        .read_json()
        .map_err(|e| MistralError::Io(io::Error::other(e)))?;

    response_to_turn_result(payload)
}

/// Send a streaming POST /v1/chat/completions request with retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn chat_completion_request_streaming<F>(
    agent: &ureq::Agent,
    config: &MistralConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    thinking_effort: ThinkingEffort,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    mut on_event: F,
) -> Result<ChatTurnResult, MistralError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, CHAT_COMPLETIONS_PATH)?;
    let retry_cfg = retry::RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    };

    let payloads = build_message_payloads(messages);
    let tool_payloads = if tools.is_empty() {
        None
    } else {
        Some(build_tool_payloads(tools))
    };

    let reasoning = thinking_payload(thinking_effort);

    let body = serde_json::to_value(&ChatCompletionRequest {
        model,
        messages: payloads,
        tools: tool_payloads,
        stream: true,
        max_tokens: Some(config.max_tokens),
        reasoning_effort: reasoning,
    })
    .map_err(io::Error::other)?;

    debug!("Mistral streaming chat completion request: model={}", model,);

    let response = retry::retry_loop(
        || {
            agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", api_key.trim()))
                .send_json(body.clone())
        },
        &retry_cfg,
        on_retry,
        cancel_rx,
    )?;

    let status: u16 = response.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = response
            .into_body()
            .read_to_string()
            .unwrap_or_else(|_| "unknown error".to_string());
        return Err(map_http_status(status, detail));
    }

    let reader = BufReader::new(response.into_body().into_reader());
    let mut text_accumulated = String::new();

    // Accumulate tool call deltas by index across SSE chunks.
    // Mistral sends tool calls as a sequence of delta events — each delta
    // carries the same `index` for a given tool call. We merge deltas with
    // the same index: id comes from the first chunk that carries it, and
    // arguments are concatenated across chunks.
    let mut tool_calls_accumulated: Vec<super::StreamToolCallDelta> = Vec::new();
    // Track token usage from the API response, typically sent in the last
    // chunk alongside the [DONE] marker.
    let mut stream_usage: Option<TokenUsage> = None;

    // Mistral uses the standard OpenAI-compatible SSE streaming format
    // where each event is a JSON-encoded CompletionChunk.
    let mut sse_reader = crate::openai::SseReader::from_reader(reader);

    loop {
        let event_result = sse_reader.next_event();
        match event_result {
            Ok(Some(ref event)) => {
                // The [DONE] marker signals end of stream.
                if event.trim() == "[DONE]" {
                    break;
                }
                match serde_json::from_str::<super::CompletionChunk>(event) {
                    Ok(chunk) => {
                        // Capture token usage if the chunk includes it
                        // (typically the final chunk before [DONE]).
                        if let Some(ref u) = chunk.usage {
                            stream_usage = Some(TokenUsage {
                                input_tokens: u.prompt_tokens,
                                output_tokens: u.completion_tokens,
                                total_tokens: u.total_tokens,
                            });
                        }
                        for choice in chunk.choices {
                            // Emit content deltas immediately so the caller can forward
                            // them to subscribers without buffering the full response.
                            if let Some(content) = choice.delta.content
                                && !content.is_empty()
                            {
                                text_accumulated.push_str(&content);
                                on_event(StreamEvent::Answer(content))?;
                            }
                            // Merge tool call deltas by their index field.
                            if let Some(delta_tool_calls) = choice.delta.tool_calls {
                                for dtc in delta_tool_calls {
                                    // dtc.index is u64; convert to u32.
                                    // u32::MAX would cause a Vec allocation of 4B+ entries below,
                                    // so reject anything that doesn't fit in u32.
                                    let safe_index = u32::try_from(dtc.index).map_err(|e| {
                                        MistralError::Io(io::Error::other(format!(
                                            "tool call index {} exceeds u32::MAX: {e}",
                                            dtc.index
                                        )))
                                    })?;

                                    // Reject indices beyond the safety limit to prevent
                                    // an attacker from causing an oversized Vec allocation.
                                    let acc_idx = safe_index as usize;
                                    if acc_idx >= MAX_TOOL_CALLS {
                                        return Err(MistralError::Io(io::Error::other(format!(
                                            "tool call index {acc_idx} exceeds maximum ({MAX_TOOL_CALLS})"
                                        ))));
                                    }

                                    // Ensure the accumulator vector is large enough.
                                    while tool_calls_accumulated.len() <= acc_idx {
                                        tool_calls_accumulated
                                            .push(super::StreamToolCallDelta::default());
                                    }
                                    let entry = &mut tool_calls_accumulated[acc_idx];
                                    if let Some(id_val) = dtc.id {
                                        entry.id = Some(id_val);
                                    }
                                    if let Some(func) = dtc.function {
                                        let f = entry.function.get_or_insert_default();
                                        if let Some(name) = func.name {
                                            f.name = Some(name);
                                        }
                                        if let Some(args) = func.arguments {
                                            let current = f.arguments.get_or_insert_default();
                                            current.push_str(&args);
                                        }
                                    }
                                }
                            }
                            // If the finish_reason is "tool_calls", the stream has
                            // delivered all deltas for this turn. Drain the
                            // accumulated state into a ToolUse result now rather
                            // than waiting for the [DONE] marker.
                            if let Some(finish_reason) = choice._finish_reason
                                && finish_reason == "tool_calls"
                            {
                                let calls: Vec<crate::providers::types::ChatToolCall> =
                                    tool_calls_accumulated
                                        .iter()
                                        .filter_map(|tc| {
                                            let id = tc.id.as_ref()?;
                                            let func = tc.function.as_ref()?;
                                            let name = func.name.as_ref()?;
                                            let args = func
                                                .arguments
                                                .as_ref()
                                                .cloned()
                                                .unwrap_or_default();
                                            Some(crate::providers::types::ChatToolCall {
                                                id: id.clone(),
                                                name: name.clone(),
                                                arguments_json: args,
                                                caller: None,
                                            })
                                        })
                                        .collect();
                                if !calls.is_empty() {
                                    return Ok(ChatTurnResult::ToolUse(
                                        crate::providers::types::ChatAssistantToolUse {
                                            content: if text_accumulated.is_empty() {
                                                None
                                            } else {
                                                Some(text_accumulated.clone())
                                            },
                                            tool_calls: calls,
                                            reasoning: None,
                                            usage: stream_usage,
                                            response_id: None,
                                        },
                                    ));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to parse Mistral SSE chunk: {e} — raw: {}", event);
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                let err_msg = format!("SSE read error: {e}");
                return Err(MistralError::Io(io::Error::other(err_msg)));
            }
        }
    }

    // After the [DONE] marker, check if any tool calls were accumulated
    // without an explicit "tool_calls" finish_reason in the last chunk.
    // This handles edge cases where the finish_reason is in a different
    // chunk or the API omits it.
    if !tool_calls_accumulated.is_empty() {
        let calls: Vec<crate::providers::types::ChatToolCall> = tool_calls_accumulated
            .iter()
            .filter_map(|tc| {
                let id = tc.id.as_ref()?;
                let func = tc.function.as_ref()?;
                let name = func.name.as_ref()?;
                let args = func.arguments.as_ref().cloned().unwrap_or_default();
                Some(crate::providers::types::ChatToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments_json: args,
                    caller: None,
                })
            })
            .collect();
        if !calls.is_empty() {
            return Ok(ChatTurnResult::ToolUse(
                crate::providers::types::ChatAssistantToolUse {
                    content: if text_accumulated.is_empty() {
                        None
                    } else {
                        Some(text_accumulated.clone())
                    },
                    tool_calls: calls,
                    reasoning: None,
                    usage: stream_usage,
                    response_id: None,
                },
            ));
        }
    }

    if text_accumulated.is_empty() {
        return Err(MistralError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(
        crate::providers::types::FinalTextResult {
            content: text_accumulated,
            reasoning: None,
            usage: stream_usage,
            response_id: None,
        },
    ))
}

/// Map an HTTP status code to a MistralError variant.
///
/// `retry_after_secs` is set to None here because the Mistral API doesn't
/// consistently return a Retry-After header on 429s. The retry loop in
/// `retry::retry_loop` uses its own backoff schedule regardless.
fn map_http_status(status: u16, detail: String) -> MistralError {
    match status {
        401 => MistralError::Unauthorized { status, detail },
        429 => MistralError::RateLimited {
            retry_after_secs: None,
            detail,
        },
        s if s >= 500 => MistralError::ServerError { status, detail },
        _ => MistralError::ClientError { status, detail },
    }
}
