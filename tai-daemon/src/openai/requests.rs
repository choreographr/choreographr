use tracing::{debug, info, trace, warn};

use super::ServiceConfig;
use super::retry;
use super::{
    ChatCompletionsRequest, ChatCompletionsResponse, ChatCompletionsStreamOptions,
    ChatCompletionsStreamResponse, ChatRequestMessage, ChatToolDefinition, ModelListResponse,
    OpenAiClient, RequestFormat, ResponseOutputItem, ResponsesInputItem, ResponsesRequest,
    ResponsesResponse, ResponsesStreamEvent, ResponsesTool, SseReader, StreamToolCallDelta,
    endpoint_url, messages_to_responses_input, parse_responses_stream_event,
    reasoning_effort_api_value,
};
use crate::providers::StreamEvent;
use crate::providers::shared::MAX_TOOL_CALLS;
use crate::providers::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult,
};
use crate::providers::{ChatTurnRequest, ToolResultItem};
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use tai_proto::TokenUsage;

/// Filter tool calls whose `arguments_json` is not valid JSON (e.g. truncated
/// mid-stream by the provider).  Returns the names of discarded calls.
/// Providers (especially cheaper models via OpenAI-compatible APIs) sometimes
/// return incomplete `function.arguments` strings, which would cause tool
/// execution to fail with a JSON parse error and trigger an error-recovery
/// loop that inflates the context and eventually hits the provider's 400/500.
fn validate_tool_call_arguments(tool_calls: &mut Vec<ChatToolCall>) -> Vec<String> {
    let mut discarded = Vec::new();
    tool_calls.retain(|tc| {
        if serde_json::from_str::<serde_json::Value>(&tc.arguments_json).is_ok() {
            true
        } else {
            warn!(
                name = %tc.name,
                args_len = tc.arguments_json.len(),
                "discarding tool call with invalid (truncated) arguments JSON",
            );
            discarded.push(tc.name.clone());
            false
        }
    });
    discarded
}

impl OpenAiClient {
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, super::OpenAiError> {
        info!("listing models from {}", self.config.base_url);
        let url = endpoint_url(&self.config.base_url, &self.config.model_list_path)?;
        let retry = retry::retry_config_from_config(&self.config);
        let response = retry::retry_send_get_simple(&self.http, &url, &self.api_key, &retry)?;
        let payload: ModelListResponse = response
            .into_body()
            .read_json()
            .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
        let models: Vec<String> = payload.data.into_iter().map(|model| model.id).collect();
        info!("models returned: {}", models.len());
        Ok(models)
    }

    pub fn completion(&self, model: &str, prompt: &str) -> Result<String, super::OpenAiError> {
        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => {
                responses_request(&self.http, &self.config, &self.api_key, model, prompt)
            }
            RequestFormat::ChatCompletions => {
                chat_completions_request(&self.http, &self.config, &self.api_key, model, prompt)
            }
        }
    }

    pub fn completion_stream<F>(
        &self,
        model: &str,
        prompt: &str,
        mut on_event: F,
    ) -> Result<(), super::OpenAiError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        if !self.config.streaming {
            let content = self.completion(model, prompt)?;
            if !content.is_empty() {
                on_event(StreamEvent::Answer(content))?;
            }
            return Ok(());
        }

        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => responses_request_streaming(
                &self.http,
                &self.config,
                &self.api_key,
                model,
                prompt,
                &mut on_event,
            ),
            RequestFormat::ChatCompletions => chat_completions_request_streaming(
                &self.http,
                &self.config,
                &self.api_key,
                model,
                prompt,
                None, // No reasoning_effort for simple completion
                &mut on_event,
            ),
        }
    }

    pub fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, super::OpenAiError> {
        let reasoning_effort = reasoning_effort_api_value(params.thinking_effort);
        debug!(?params.thinking_effort, ?reasoning_effort, "chat_completion_turn");
        match self.config.request_format_for_model(params.model) {
            RequestFormat::Responses => responses_request_with_tools(
                &self.http,
                &self.config,
                &self.api_key,
                params.model,
                params.messages,
                params.tools,
                reasoning_effort,
                params.previous_response_id,
                params.tool_results,
                params.programmatic_tool_calling,
            ),
            RequestFormat::ChatCompletions => chat_completions_request_with_tools(
                &self.http,
                &self.config,
                &self.api_key,
                params.model,
                params.messages,
                params.tools,
                reasoning_effort,
                params.on_retry,
                params.cancel_rx,
            ),
        }
    }

    pub fn chat_completion_turn_streaming<F>(
        &self,
        params: ChatTurnRequest<'_>,
        mut on_event: F,
    ) -> Result<ChatTurnResult, super::OpenAiError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        let reasoning_effort = reasoning_effort_api_value(params.thinking_effort);
        debug!(
            ?params.thinking_effort,
            ?reasoning_effort,
            "chat_completion_turn_streaming"
        );
        if !self.config.streaming {
            let result = self.chat_completion_turn(params)?;
            let result =
                crate::providers::shared::emit_non_streaming_events(result, &mut on_event)?;
            return Ok(result);
        }

        match self.config.request_format_for_model(params.model) {
            RequestFormat::Responses => responses_request_streaming_with_tools(
                &self.http,
                &self.config,
                &self.api_key,
                params.model,
                params.messages,
                params.tools,
                reasoning_effort,
                params.previous_response_id,
                params.tool_results,
                params.on_retry,
                params.cancel_rx,
                params.programmatic_tool_calling,
                on_event,
            ),
            RequestFormat::ChatCompletions => chat_completions_request_streaming_with_tools(
                &self.http,
                &self.config,
                &self.api_key,
                params.model,
                params.messages,
                params.tools,
                reasoning_effort,
                params.on_retry,
                params.cancel_rx,
                on_event,
            ),
        }
    }
}

fn responses_request(
    agent: &ureq::Agent,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> Result<String, super::OpenAiError> {
    let (url, body) = build_simple_responses_body(config, model, prompt, false)?;
    let retry = retry::retry_config_from_config(config);
    let response = retry::retry_send_simple(agent, &url, api_key, &body, &retry)?;
    let payload: ResponsesResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let content = payload
        .output
        .into_iter()
        .filter_map(|item| {
            if let ResponseOutputItem::Message { content, .. } = item {
                Some(content)
            } else {
                None
            }
        })
        .flatten()
        .filter_map(|part| part.text)
        .map(|text| text.trim().to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_default();

    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(content)
}

fn chat_completions_request(
    agent: &ureq::Agent,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> Result<String, super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match config.max_tokens_field_for_model(model) {
            super::MaxTokensField::MaxTokens => (max_tokens, None),
            super::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &messages,
        tools: None,
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort: None,
    })
    .map_err(io::Error::other)?;
    let response = retry::retry_send_simple(agent, &url, api_key, &body, &retry)?;
    let payload: ChatCompletionsResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let content = payload
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .unwrap_or_default()
        .trim()
        .to_string();

    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(content)
}

#[allow(clippy::too_many_arguments)]
fn chat_completions_request_with_tools(
    agent: &ureq::Agent,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&'static str>,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match config.max_tokens_field_for_model(model) {
            super::MaxTokensField::MaxTokens => (max_tokens, None),
            super::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages,
        tools: Some(tools),
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, on_retry, cancel_rx)?;
    let payload: ChatCompletionsResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let elapsed = start.elapsed();
    debug!(
        model = %model,
        elapsed_ms = elapsed.as_millis(),
        prompt_tokens = payload.usage.as_ref().map(|u| u.prompt_tokens),
        completion_tokens = payload.usage.as_ref().map(|u| u.completion_tokens),
        total_tokens = payload.usage.as_ref().map(|u| u.total_tokens),
        "chat completion turn",
    );
    let Some(mut choice) = payload.choices.into_iter().next() else {
        return Err(super::OpenAiError::EmptyResponse);
    };

    // Extract reasoning early (before partial moves into tool_calls / content)
    let reasoning = choice.message.take_reasoning();

    // Extract token usage from the API response for cost tracking / display.
    let turn_usage: Option<TokenUsage> = payload.usage.map(|u| TokenUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
    });

    let mut tool_calls: Vec<ChatToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tool_call| ChatToolCall {
            id: tool_call.id,
            name: tool_call.function.name,
            arguments_json: tool_call.function.arguments,
            caller: None,
        })
        .collect();
    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: choice.message.content,
            tool_calls,
            reasoning,
            usage: turn_usage,
            response_id: None,
        }));
    }

    if !discarded.is_empty() {
        return Err(super::OpenAiError::TruncatedToolCall {
            tool_names: discarded,
        });
    }

    let content = choice
        .message
        .content
        .unwrap_or_default()
        .trim()
        .to_string();
    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        reasoning,
        usage: turn_usage,
        response_id: None,
    }))
}

fn chat_completions_request_streaming<F>(
    agent: &ureq::Agent,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    reasoning_effort: Option<&'static str>,
    on_event: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match config.max_tokens_field_for_model(model) {
            super::MaxTokensField::MaxTokens => (max_tokens, None),
            super::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(ChatCompletionsStreamOptions {
            include_usage: true,
        }),
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    let response = retry::retry_send_simple(agent, &url, api_key, &body, &retry)?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    let mut has_any_output = false;
    while let Some(data) = reader.next_event()? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            if let Some(content) = delta.content.filter(|content| !content.is_empty()) {
                has_any_output = true;
                on_event(StreamEvent::Answer(content))?;
            }
            for reasoning in [
                delta.reasoning_content,
                delta.reasoning,
                delta.reasoning_text,
            ]
            .into_iter()
            .flatten()
            .filter(|content| !content.is_empty())
            {
                has_any_output = true;
                on_event(StreamEvent::Reasoning(reasoning))?;
            }
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}

fn responses_request_streaming<F>(
    agent: &ureq::Agent,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    on_event: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let (url, body) = build_simple_responses_body(config, model, prompt, true)?;
    let retry = retry::retry_config_from_config(config);
    let response = retry::retry_send_simple(agent, &url, api_key, &body, &retry)?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    let mut has_any_output = false;
    while let Some(data) = reader.next_event()? {
        let event = parse_responses_stream_event(&data)?;
        match event {
            Some(ResponsesStreamEvent::TextDelta(text)) => {
                has_any_output = true;
                on_event(StreamEvent::Answer(text))?;
            }
            Some(ResponsesStreamEvent::TextDone) => {
                // Text output is complete; continue waiting for completion event.
            }
            Some(ResponsesStreamEvent::ResponseCompleted { .. }) => {
                break;
            }
            Some(ResponsesStreamEvent::ResponseFailed(error)) => {
                return Err(super::OpenAiError::Io(io::Error::other(error)));
            }
            Some(ResponsesStreamEvent::ResponseIncomplete) => {
                return Err(super::OpenAiError::Io(io::Error::other(
                    "response incomplete",
                )));
            }
            _ => {}
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}

/// Build the `input` JSON value for a Responses API request, based on
/// whether this is a follow-up turn carrying tool results or a first turn
/// built from message history.
fn build_responses_input(
    tool_results: &[ToolResultItem],
    messages: &[ChatRequestMessage],
) -> Result<Option<serde_json::Value>, super::OpenAiError> {
    if tool_results.is_empty() {
        // First call in a turn: convert messages to Responses input items.
        // System messages go into `input` as `{role: "system"}` items.
        let items = messages_to_responses_input(messages);
        let input_value = if items.is_empty() {
            serde_json::Value::String(String::new())
        } else {
            serde_json::to_value(&items).map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?
        };
        Ok(Some(input_value))
    } else {
        // Returning tool results from a previous turn: build
        // function_call_output items.  Both `instructions` and `messages`
        // are omitted because the Responses API remembers the full
        // conversation history from the original turn via
        // `previous_response_id` — only the new tool results are needed.
        let items: Vec<ResponsesInputItem> = tool_results
            .iter()
            .map(|tr| ResponsesInputItem::FunctionCallOutput {
                call_id: tr.call_id.clone(),
                output: tr.output.clone(),
                caller: tr.caller.clone(),
            })
            .collect();
        let input_value = serde_json::to_value(&items)
            .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
        Ok(Some(input_value))
    }
}

/// Shared helper: extract the reasoning text from a Responses API
/// reasoning-summary array.  Each entry may be a plain string or an object
/// with a `"text"` field.
fn extract_reasoning_text(summary: &[serde_json::Value]) -> Option<String> {
    let parts: Vec<String> = summary
        .iter()
        .filter_map(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.get("text").and_then(|t| t.as_str().map(String::from)))
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Shared helper: build the URL and serialised request body for a
/// Responses API request with tools (and optionally tool results from a
/// previous turn).
#[allow(clippy::too_many_arguments)]
fn build_responses_request_body(
    config: &ServiceConfig,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&'static str>,
    previous_response_id: Option<&str>,
    tool_results: &[ToolResultItem],
    stream: bool,
    programmatic_tool_calling: bool,
) -> Result<(String, serde_json::Value), super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let max_output_tokens = config.max_output_tokens_for_model(model);
    let input_value = build_responses_input(tool_results, messages)?;

    let mut responses_tools: Vec<ResponsesTool> = tools.iter().map(ResponsesTool::from).collect();
    if programmatic_tool_calling {
        responses_tools.push(ResponsesTool {
            kind: "programmatic_tool_calling".to_string(),
            name: String::new(),
            description: String::new(),
            parameters: serde_json::Value::Null,
            strict: false,
            output_schema: None,
            allowed_callers: None,
        });
    }
    let tools_opt = if responses_tools.is_empty() {
        None
    } else {
        Some(responses_tools)
    };

    // Always request reasoning summary — the server omits it for models
    // that don't support reasoning.
    let include = Some(vec!["reasoning.summary"]);

    // tool_choice: "auto" tells the model to use function calling.
    // Without this, some models may generate tool calls as plain text instead.
    let tool_choice = if tools_opt.is_some() {
        Some("auto".into())
    } else {
        None
    };

    let body = serde_json::to_value(&ResponsesRequest {
        model,
        input: input_value,
        instructions: None,
        tools: tools_opt,
        stream,
        max_output_tokens,
        reasoning_effort,
        // store: true is required for previous_response_id to work correctly
        // and matches the @ai-sdk/openai default behavior.
        store: true,
        previous_response_id,
        include,
        parallel_tool_calls: None,
        tool_choice,
    })
    .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    Ok((url, body))
}

/// Build the URL and serialised request body for a simple (no-tool) Responses API request.
/// Unlike `build_responses_request_body`, this sends `input` as a plain string and
/// omits instructions, tools, and chaining fields — appropriate for one-shot completions.
fn build_simple_responses_body(
    config: &ServiceConfig,
    model: &str,
    prompt: &str,
    stream: bool,
) -> Result<(String, serde_json::Value), super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let body = serde_json::to_value(&ResponsesRequest {
        model,
        // Simple (non-turn) requests send input as a plain string rather than
        // an items array.  The server expands it to a single user message.
        input: Some(serde_json::Value::String(prompt.to_string())),
        instructions: None,
        tools: None,
        stream,
        max_output_tokens: None,
        reasoning_effort: None,
        // Simple requests are ephemeral — don't persist on the server side.
        store: false,
        previous_response_id: None,
        include: None,
        parallel_tool_calls: None,
        tool_choice: None,
    })
    .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
    Ok((url, body))
}

/// Non-streaming Responses API turn with tool definitions, reasoning effort,
/// and optional tool results from a previous turn.
#[allow(clippy::too_many_arguments)]
fn responses_request_with_tools(
    agent: &ureq::Agent,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&'static str>,
    previous_response_id: Option<&str>,
    tool_results: &[ToolResultItem],
    programmatic_tool_calling: bool,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();

    let (url, body) = build_responses_request_body(
        config,
        model,
        messages,
        tools,
        reasoning_effort,
        previous_response_id,
        tool_results,
        false,
        programmatic_tool_calling,
    )?;

    let has_instructions = messages.iter().any(|m| m.role == "system");
    info!(
        model = %model,
        tool_count = tools.len(),
        tool_result_count = tool_results.len(),
        has_instructions = has_instructions,
        "responses request with tools",
    );

    let retry = retry::retry_config_from_config(config);
    let response = retry::retry_send_simple(agent, &url, api_key, &body, &retry)?;
    let payload: ResponsesResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let elapsed = start.elapsed();
    debug!(
        model = %model,
        elapsed_ms = elapsed.as_millis(),
        prompt_tokens = payload.usage.as_ref().map(|u| u.prompt_tokens),
        completion_tokens = payload.usage.as_ref().map(|u| u.completion_tokens),
        total_tokens = payload.usage.as_ref().map(|u| u.total_tokens),
        "responses turn",
    );

    let response_id = payload.id.clone();
    let turn_usage = payload.usage.map(|u| TokenUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
    });

    // Parse output items: extract text, reasoning, and tool calls.
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();

    for item in payload.output {
        match item {
            ResponseOutputItem::Message { content, .. } => {
                for part in content {
                    if part.kind == "output_text"
                        && let Some(ref t) = part.text
                    {
                        full_text.push_str(t);
                    }
                }
            }
            ResponseOutputItem::Reasoning { summary } => {
                if let Some(text) = extract_reasoning_text(&summary) {
                    full_reasoning.push_str(&text);
                }
            }
            ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                caller,
            } => {
                tool_calls.push(ChatToolCall {
                    id: call_id,
                    name,
                    arguments_json: arguments,
                    caller,
                });
            }
            // Program items contain JS code generated by the model for
            // orchestrating tool calls in OpenAI's hosted V8 runtime.
            // We log it but don't execute it — the program makes
            // function_call items (with `caller` pointing back to this program)
            // that we handle normally via the tool loop.
            // Ref: https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling
            ResponseOutputItem::Program { call_id, code, .. } => {
                if let Some(ref code_text) = code {
                    tracing::debug!(
                        program_call_id = %call_id,
                        code_len = code_text.len(),
                        "programmatic tool calling program",
                    );
                }
            }
            // Program output signals a program finished executing in the
            // hosted runtime. The `result` is the JSON string the program
            // emitted via `text(...)`, and `status` is "completed" or
            // "incomplete". We log it but don't act on it directly — the
            // function_call items the program produced are already handled.
            // Ref: https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling
            ResponseOutputItem::ProgramOutput {
                call_id,
                result,
                status,
                ..
            } => {
                tracing::debug!(
                    program_call_id = %call_id,
                    has_result = result.is_some(),
                    status = status.as_deref().unwrap_or("unknown"),
                    "programmatic tool calling program output",
                );
            }
        }
    }

    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: if full_text.is_empty() {
                None
            } else {
                Some(full_text)
            },
            tool_calls,
            reasoning: if full_reasoning.is_empty() {
                None
            } else {
                Some(full_reasoning)
            },
            usage: turn_usage,
            response_id,
        }))
    } else if !discarded.is_empty() && !full_text.is_empty() {
        // All calls were discarded but the model also produced text —
        // return the text instead of failing.
        Ok(ChatTurnResult::FinalText(FinalTextResult {
            content: full_text,
            reasoning: if full_reasoning.is_empty() {
                None
            } else {
                Some(full_reasoning)
            },
            usage: turn_usage,
            response_id,
        }))
    } else if !discarded.is_empty() {
        Err(super::OpenAiError::TruncatedToolCall {
            tool_names: discarded,
        })
    } else if full_text.is_empty() {
        Err(super::OpenAiError::EmptyResponse)
    } else {
        Ok(ChatTurnResult::FinalText(FinalTextResult {
            content: full_text,
            reasoning: if full_reasoning.is_empty() {
                None
            } else {
                Some(full_reasoning)
            },
            usage: turn_usage,
            response_id,
        }))
    }
}

/// Accumulates tool call fields across streaming SSE chunks keyed by the
/// tool call index assigned by the API.
#[derive(Debug, Default)]
struct AccumulatingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Accumulator for Responses API tool call arguments keyed by call_id.
/// Used in `responses_request_streaming_with_tools` to merge delta chunks.
struct AccCall {
    name: Option<String>,
    arguments: String,
    /// Monotonically increasing index assigned to this tool call, used to
    /// preserve insertion order since the Responses API does not provide
    /// server-side indices like Chat Completions does.
    index: u32,
}

impl AccCall {
    fn new(index: u32) -> Self {
        Self {
            name: None,
            arguments: String::new(),
            index,
        }
    }
}

/// Accumulate tool call deltas from streaming SSE chunks into ordered tool
/// calls.  Deltas with the same index are combined — `id` and `name` are taken
/// from the last chunk that carries them, and `arguments` is concatenated.
fn accumulate_tool_calls_from_deltas(
    deltas: impl IntoIterator<Item = StreamToolCallDelta>,
) -> Vec<ChatToolCall> {
    let mut map: HashMap<u32, AccumulatingToolCall> = HashMap::new();
    for tc_delta in deltas {
        let entry = map.entry(tc_delta.index).or_default();
        if let Some(id) = tc_delta.id {
            entry.id = Some(id);
        }
        if let Some(func) = tc_delta.function {
            if let Some(name) = func.name {
                entry.name = Some(name);
            }
            if let Some(args) = func.arguments {
                entry.arguments.push_str(&args);
            }
        }
    }
    let mut calls: Vec<_> = map.into_iter().collect();
    calls.sort_by_key(|(idx, _)| *idx);
    calls
        .into_iter()
        .map(|(_, tc)| ChatToolCall {
            id: tc.id.unwrap_or_default(),
            name: tc.name.unwrap_or_default(),
            arguments_json: tc.arguments,
            caller: None,
        })
        .collect()
}

/// Streaming variant of `chat_completions_request_with_tools`.
///
/// Sends `stream: true` with tool definitions, reads SSE chunks, and calls
/// `on_chunk` for each content / reasoning delta so the caller can forward
/// it to subscribers immediately.  Tool call deltas are accumulated across
/// chunks and returned as `ChatTurnResult::ToolUse` when the stream ends.
#[allow(clippy::too_many_arguments)]
fn chat_completions_request_streaming_with_tools<F>(
    agent: &ureq::Agent,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&'static str>,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    mut on_event: F,
) -> Result<ChatTurnResult, super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match config.max_tokens_field_for_model(model) {
            super::MaxTokensField::MaxTokens => (max_tokens, None),
            super::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages,
        tools: Some(tools),
        stream: true,
        // Configurable stream_options — some OpenAI-compatible providers
        // reject the `stream_options` field entirely, so users can disable
        // it per-account to maintain compatibility.
        stream_options: if config.stream_options {
            Some(ChatCompletionsStreamOptions {
                include_usage: true,
            })
        } else {
            None
        },
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, on_retry, cancel_rx)?;
    let mut has_any_output = false;
    let mut full_content = String::new();
    let mut full_reasoning = String::new();
    // Collect raw tool call deltas across all chunks, then delegate to the
    // shared accumulator once the stream is fully consumed.
    let mut raw_tool_call_deltas: Vec<StreamToolCallDelta> = Vec::new();
    let mut seen_tool_call_indices = [false; MAX_TOOL_CALLS];
    let mut distinct_tool_call_count = 0usize;

    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    // Track usage from the final SSE chunk (OpenAI sends a usage chunk with
    // choices: [] when stream_options.include_usage is true).
    let mut last_usage: Option<TokenUsage> = None;
    while let Some(data) = reader.next_event()? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;

        // Capture usage from the final chunk (OpenAI sends a usage chunk
        // with choices: []).
        if let Some(ref u) = payload.usage {
            debug!(
                prompt_tokens = u.prompt_tokens,
                completion_tokens = u.completion_tokens,
                total_tokens = u.total_tokens,
                "OpenAI streaming turn usage"
            );
            let usage = TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            };
            last_usage = Some(usage);
        }

        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            // Content chunks: answer text
            if let Some(content) = delta.content.filter(|c| !c.is_empty()) {
                has_any_output = true;
                full_content.push_str(&content);
                on_event(StreamEvent::Answer(content))?;
            }

            // Reasoning chunks — use references to avoid partial moves.
            for reasoning in [
                &delta.reasoning_content,
                &delta.reasoning,
                &delta.reasoning_text,
            ]
            .into_iter()
            .flatten()
            .filter(|r| !r.is_empty())
            {
                has_any_output = true;
                full_reasoning.push_str(reasoning);
                on_event(StreamEvent::Reasoning(reasoning.clone()))?;
            }

            // Collect raw tool call deltas — the shared accumulator
            // (accumulate_tool_calls_from_deltas) will merge them by index
            // and produce sorted ChatToolCall output after the stream ends.
            if let Some(ref tcs) = delta.tool_calls {
                has_any_output = true;
                for tc in tcs.iter() {
                    if distinct_tool_call_count >= MAX_TOOL_CALLS {
                        return Err(super::OpenAiError::Io(io::Error::other(format!(
                            "too many tool calls (max {MAX_TOOL_CALLS})"
                        ))));
                    }
                    if (tc.index as usize) >= MAX_TOOL_CALLS {
                        return Err(super::OpenAiError::Io(io::Error::other(format!(
                            "tool call index {} out of bounds (max {})",
                            tc.index,
                            MAX_TOOL_CALLS - 1,
                        ))));
                    }
                    if !seen_tool_call_indices[tc.index as usize] {
                        seen_tool_call_indices[tc.index as usize] = true;
                        distinct_tool_call_count += 1;
                    }
                    raw_tool_call_deltas.push(tc.clone());
                }
            }
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    if !raw_tool_call_deltas.is_empty() {
        let mut tool_calls = accumulate_tool_calls_from_deltas(raw_tool_call_deltas);
        let discarded = validate_tool_call_arguments(&mut tool_calls);
        if !tool_calls.is_empty() {
            return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
                content: if full_content.is_empty() {
                    None
                } else {
                    Some(full_content)
                },
                tool_calls,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: last_usage,
                response_id: None,
            }));
        }
        if !discarded.is_empty() {
            return Err(super::OpenAiError::TruncatedToolCall {
                tool_names: discarded,
            });
        }
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: full_content,
        reasoning: if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        },
        usage: last_usage,
        response_id: None,
    }))
}

/// Streaming Responses API turn with tool definitions, reasoning effort,
/// tool results, retry support, and cancellation.
///
/// Sends `stream: true` with tool definitions, reads SSE chunks, and calls
/// `on_chunk` for each content / reasoning delta so the caller can forward
/// it to subscribers immediately.  Tool call deltas are accumulated across
/// chunks and returned as `ChatTurnResult::ToolUse` when the stream ends.
#[allow(clippy::too_many_arguments)]
fn responses_request_streaming_with_tools<F>(
    agent: &ureq::Agent,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&'static str>,
    previous_response_id: Option<&str>,
    tool_results: &[ToolResultItem],
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    programmatic_tool_calling: bool,
    mut on_event: F,
) -> Result<ChatTurnResult, super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let (url, body) = build_responses_request_body(
        config,
        model,
        messages,
        tools,
        reasoning_effort,
        previous_response_id,
        tool_results,
        true,
        programmatic_tool_calling,
    )?;

    info!(
        model = %model,
        tool_count = tools.len(),
        tool_result_count = tool_results.len(),
        streaming = true,
        "responses streaming request with tools",
    );

    let retry = retry::retry_config_from_config(config);
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, on_retry, cancel_rx)?;

    let mut has_any_output = false;
    let mut full_content = String::new();
    let mut full_reasoning = String::new();
    let mut last_usage: Option<TokenUsage> = None;
    let mut response_id: Option<String> = None;

    let mut acc_calls: HashMap<String, AccCall> = HashMap::new();
    let mut next_tool_index: u32 = 0;

    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    while let Some(data) = reader.next_event()? {
        let event = parse_responses_stream_event(&data)?;
        match event {
            Some(ResponsesStreamEvent::TextDelta(text)) => {
                has_any_output = true;
                full_content.push_str(&text);
                on_event(StreamEvent::Answer(text))?;
            }
            Some(ResponsesStreamEvent::TextDone) => {
                // Text output is complete — continue listening for the
                // completion / failure events and any remaining tool calls.
            }
            Some(ResponsesStreamEvent::ReasoningSummary(summary)) => {
                if let Some(text) = extract_reasoning_text(&summary) {
                    has_any_output = true;
                    full_reasoning.push_str(&text);
                    on_event(StreamEvent::Reasoning(text))?;
                }
            }
            Some(ResponsesStreamEvent::FunctionCallArgumentsDelta { call_id, delta }) => {
                has_any_output = true;
                if acc_calls.len() >= MAX_TOOL_CALLS {
                    return Err(super::OpenAiError::Io(io::Error::other(format!(
                        "too many tool calls (max {MAX_TOOL_CALLS})"
                    ))));
                }
                let entry = acc_calls.entry(call_id).or_insert_with(|| {
                    let i = next_tool_index;
                    next_tool_index += 1;
                    AccCall::new(i)
                });
                entry.arguments.push_str(&delta);
            }
            Some(ResponsesStreamEvent::FunctionCallArgumentsDone {
                call_id,
                name,
                arguments,
            }) => {
                has_any_output = true;
                let entry = acc_calls.entry(call_id).or_insert_with(|| {
                    let i = next_tool_index;
                    next_tool_index += 1;
                    AccCall::new(i)
                });
                entry.name = Some(name);
                entry.arguments = arguments;
                trace!(
                    tool_name = ?entry.name,
                    args_len = entry.arguments.len(),
                    "openai responses: function call args done",
                );
            }
            Some(ResponsesStreamEvent::ResponseCompleted { id, usage }) => {
                response_id = id;
                if let Some(u) = usage {
                    debug!(
                        prompt_tokens = u.prompt_tokens,
                        completion_tokens = u.completion_tokens,
                        total_tokens = u.total_tokens,
                        "responses streaming turn usage",
                    );
                    let usage = TokenUsage {
                        input_tokens: u.prompt_tokens,
                        output_tokens: u.completion_tokens,
                        total_tokens: u.total_tokens,
                    };
                    last_usage = Some(usage);
                }
                break;
            }
            Some(ResponsesStreamEvent::ResponseFailed(error)) => {
                return Err(super::OpenAiError::Io(io::Error::other(error)));
            }
            Some(ResponsesStreamEvent::ResponseIncomplete) => {
                return Err(super::OpenAiError::Io(io::Error::other(
                    "response incomplete",
                )));
            }
            Some(ResponsesStreamEvent::ProgramCodeDelta(delta)) => {
                // Streaming JS code from the model-generated program that runs
                // in OpenAI's hosted V8. We just observe it; the function_call
                // items it triggers come through as separate SSE events.
                tracing::trace!(delta_len = delta.len(), "program code delta");
            }
            Some(ResponsesStreamEvent::ProgramCodeDone {
                call_id,
                fingerprint,
            }) => {
                // The program definition is complete. The `fingerprint` is an
                // opaque token for resuming/replaying this program.
                tracing::debug!(
                    %call_id,
                    has_fingerprint = fingerprint.is_some(),
                    "program code complete",
                );
            }
            Some(ResponsesStreamEvent::ProgramOutputDone {
                call_id,
                result,
                status,
            }) => {
                // The program finished with a result. `status` is "completed"
                // or "incomplete". A final `message` item may follow in a
                // later SSE event; the caller must keep reading.
                tracing::debug!(
                    %call_id,
                    result_len = result.len(),
                    %status,
                    "program output done",
                );
            }
            _ => {
                // Unknown event types are silently ignored.
            }
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    if !acc_calls.is_empty() {
        let mut sorted: Vec<(String, AccCall)> = acc_calls.into_iter().collect();
        sorted.sort_by_key(|(_, acc)| acc.index);
        let total_calls = sorted.len();

        // First pass: filter out calls without a name (the provider sent
        // incomplete function call events, e.g. arguments delta without a
        // done event).
        let mut tool_calls: Vec<ChatToolCall> = sorted
            .into_iter()
            .filter_map(|(call_id, acc)| {
                let name = acc.name.as_deref().filter(|n| !n.is_empty())?;
                Some(ChatToolCall {
                    id: call_id,
                    name: name.to_string(),
                    arguments_json: acc.arguments,
                    caller: None,
                })
            })
            .collect();

        // Second pass: discard calls with truncated arguments JSON.
        let discarded = validate_tool_call_arguments(&mut tool_calls);

        if !tool_calls.is_empty() {
            return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
                content: if full_content.is_empty() {
                    None
                } else {
                    Some(full_content)
                },
                tool_calls,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: last_usage,
                response_id,
            }));
        }

        // All calls were discarded — show a user-visible error in the TUI,
        // not just a log line, unless the model also produced text.
        if !discarded.is_empty() && !full_content.is_empty() {
            return Ok(ChatTurnResult::FinalText(FinalTextResult {
                content: full_content,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: last_usage,
                response_id,
            }));
        }
        if !discarded.is_empty() {
            return Err(super::OpenAiError::TruncatedToolCall {
                tool_names: discarded,
            });
        }
        warn!("discarded {total_calls} incomplete tool call(s) from provider (no name set)",);
    }

    // If only reasoning events arrived without any answer text, treat it as
    // empty — matching the non-streaming variant's behaviour.
    if full_content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: full_content,
        reasoning: if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        },
        usage: last_usage,
        response_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{AllowedCaller, StreamToolCallFunctionDelta};
    use crate::providers::types::CallerInfo;
    use std::time::Duration;
    use tai_proto::ThinkingEffort;

    // -- validate_tool_call_arguments tests --------------------------------

    #[test]
    fn validate_valid_arguments_kept() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "search".into(),
                arguments_json: r#"{"q":"rust"}"#.into(),
                caller: None,
            },
        ];
        let discarded = validate_tool_call_arguments(&mut calls);
        assert!(discarded.is_empty());
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn validate_invalid_arguments_discarded() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "bad_tool".into(),
                arguments_json: "truncated garbage".into(),
                caller: None,
            },
        ];
        let discarded = validate_tool_call_arguments(&mut calls);
        assert_eq!(discarded, vec!["bad_tool"]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
    }

    #[test]
    fn validate_all_invalid_returns_all_names() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "tool_a".into(),
                arguments_json: "bad".into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "tool_b".into(),
                arguments_json: "also bad".into(),
                caller: None,
            },
        ];
        let discarded = validate_tool_call_arguments(&mut calls);
        assert_eq!(discarded.len(), 2);
        assert!(calls.is_empty());
    }

    #[test]
    fn validate_empty_list_returns_empty() {
        let mut calls: Vec<ChatToolCall> = vec![];
        let discarded = validate_tool_call_arguments(&mut calls);
        assert!(discarded.is_empty());
        assert!(calls.is_empty());
    }

    // -- sleep_or_cancel tests -------------------------------------------

    #[test]
    fn sleep_or_cancel_signal_returns_cancelled() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        let result = crate::retry::sleep_or_cancel(Duration::from_secs(10), Some(&rx));
        assert!(result.is_err());
    }

    #[test]
    fn sleep_or_cancel_disconnected_returns_ok() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        let result = crate::retry::sleep_or_cancel(Duration::from_millis(1), Some(&rx));
        assert!(result.is_ok());
    }

    // -- tool call accumulation tests ------------------------------------

    #[test]
    fn accumulate_no_deltas_returns_empty_vec() {
        let result = accumulate_tool_calls_from_deltas(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn accumulate_single_tool_call_in_one_chunk() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: Some("function".into()),
            function: Some(StreamToolCallFunctionDelta {
                name: Some("get_weather".into()),
                arguments: Some(r#"{"city":"London"}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[0].arguments_json, r#"{"city":"London"}"#);
    }

    #[test]
    fn accumulate_arguments_concatenated_across_chunks() {
        let deltas = vec![
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("get_weather".into()),
                    // First half: split inside the value string, not at a
                    // quote boundary, so concatenation yields valid JSON.
                    arguments: Some(r#"{"city":"Lon"#.into()),
                }),
            },
            StreamToolCallDelta {
                index: 0,
                id: None,
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"don"}"#.into()),
                }),
            },
        ];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[0].arguments_json, r#"{"city":"London"}"#);
    }

    #[test]
    fn accumulate_multiple_tool_calls_sorted_by_index() {
        let deltas = vec![
            // Tool call 1, first chunk
            StreamToolCallDelta {
                index: 1,
                id: Some("call_2".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("search".into()),
                    arguments: Some(r#"{"q":"rust"}"#.into()),
                }),
            },
            // Tool call 0, arrives after index 1
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("get_weather".into()),
                    arguments: Some(r#"{"city":"Paris"}"#.into()),
                }),
            },
        ];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 2);
        // Must be sorted by index: index 0 first, then index 1
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[1].id, "call_2");
        assert_eq!(result[1].name, "search");
    }

    #[test]
    fn accumulate_missing_id_defaults_to_empty() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: None,
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: Some("get_weather".into()),
                arguments: Some(r#"{}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "");
        assert_eq!(result[0].name, "get_weather");
    }

    #[test]
    fn accumulate_missing_name_defaults_to_empty() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: None,
                arguments: Some(r#"{}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "");
    }

    #[test]
    fn accumulate_no_function_delta_produces_empty_call() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: None,
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "");
        assert_eq!(result[0].arguments_json, "");
    }

    // -- streaming delta deserialisation tests --------------------------

    #[test]
    fn stream_delta_tool_calls_deserialises() {
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{
                "choices":[{
                    "delta":{
                        "content":"Hello",
                        "tool_calls":[{
                            "index":0,
                            "id":"call_abc",
                            "type":"function",
                            "function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}
                        }]
                    }
                }]
            }"#,
        )
        .expect("parse");
        let delta = payload.choices.into_iter().next().unwrap().delta.unwrap();
        assert_eq!(delta.content.as_deref(), Some("Hello"));
        let tcs = delta.tool_calls.expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].index, 0);
        assert_eq!(tcs[0].id.as_deref(), Some("call_abc"));
        assert_eq!(tcs[0].kind.as_deref(), Some("function"));
        let func = tcs[0].function.as_ref().unwrap();
        assert_eq!(func.name.as_deref(), Some("get_weather"));
        assert_eq!(func.arguments.as_deref(), Some(r#"{"city":"London"}"#));
    }

    #[test]
    fn stream_delta_tool_calls_absent_when_not_in_json() {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#).expect("parse");
        let delta = payload.choices.into_iter().next().unwrap().delta.unwrap();
        assert_eq!(delta.content.as_deref(), Some("Hi"));
        assert!(delta.tool_calls.is_none());
    }

    // -- accumulated deltas -> ChatTurnResult integration test ----------

    #[test]
    fn accumulate_deltas_to_tool_use_result() {
        // Simulate what the streaming function does: collect deltas from
        // multiple SSE chunks and pass them through the accumulator.
        let deltas = vec![
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("search".into()),
                    arguments: Some(r#"{"q":"we"#.into()),
                }),
            },
            StreamToolCallDelta {
                index: 0,
                id: None,
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"ather"}"#.into()),
                }),
            },
        ];
        let tool_calls = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].name, "search");
        assert_eq!(tool_calls[0].arguments_json, r#"{"q":"weather"}"#);

        let result = ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: Some("I'll search for that.".into()),
            tool_calls,
            reasoning: None,
            usage: None,
            response_id: None,
        });
        match result {
            ChatTurnResult::ToolUse(use_) => {
                assert_eq!(use_.content.as_deref(), Some("I'll search for that."));
                assert_eq!(use_.tool_calls.len(), 1);
            }
            _ => panic!("expected ToolUse"),
        }
    }

    // -- reasoning_effort serialization tests ---------------------------

    #[test]
    fn reasoning_effort_serialization() {
        // Off → None (omitted from body)
        assert_eq!(
            crate::openai::reasoning_effort_api_value(ThinkingEffort::Off),
            None
        );

        // Low → "low"
        assert_eq!(
            crate::openai::reasoning_effort_api_value(ThinkingEffort::Low),
            Some("low")
        );

        // Medium → "medium"
        assert_eq!(
            crate::openai::reasoning_effort_api_value(ThinkingEffort::Medium),
            Some("medium")
        );

        // High → "high"
        assert_eq!(
            crate::openai::reasoning_effort_api_value(ThinkingEffort::High),
            Some("high")
        );
    }

    #[test]
    fn chat_completions_request_omits_reasoning_effort_when_none() {
        let body = serde_json::to_value(&ChatCompletionsRequest {
            model: "gpt-4.1",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: None,
            stream: false,
            stream_options: None,
            max_tokens: None,
            max_completion_tokens: None,
            reasoning_effort: None,
        })
        .unwrap();
        assert!(body.get("reasoning_effort").is_none(), "should be omitted");
    }

    // -- token usage streaming response tests ----------------------------

    #[test]
    fn stream_response_deserializes_usage_chunk() {
        // OpenAI sends a usage-only chunk at the end of a stream with
        // stream_options.include_usage=true.
        let json = r#"{"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":25,"total_tokens":75}}"#;
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(json).unwrap();
        assert!(payload.choices.is_empty());
        let usage = payload.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 50);
        assert_eq!(usage.completion_tokens, 25);
        assert_eq!(usage.total_tokens, 75);
    }

    #[test]
    fn stream_response_without_usage_defaults_to_none() {
        let json = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(json).unwrap();
        assert_eq!(payload.choices.len(), 1);
        assert!(payload.usage.is_none());
    }

    #[test]
    fn test_chat_completions_response_non_streaming_with_usage() {
        // Non-streaming response with usage
        let json = r#"{"choices":[{"message":{"content":"Hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn chat_completions_request_includes_reasoning_effort_when_set() {
        let body = serde_json::to_value(&ChatCompletionsRequest {
            model: "o3-mini",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: None,
            stream: false,
            stream_options: None,
            max_tokens: None,
            max_completion_tokens: None,
            reasoning_effort: Some("low"),
        })
        .unwrap();
        assert_eq!(
            body.get("reasoning_effort"),
            Some(&serde_json::Value::String("low".into()))
        );
    }

    #[test]
    fn stream_options_included_when_enabled() {
        let body = serde_json::to_value(&ChatCompletionsRequest {
            model: "gpt-4",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: None,
            stream: true,
            stream_options: Some(ChatCompletionsStreamOptions {
                include_usage: true,
            }),
            max_tokens: None,
            max_completion_tokens: None,
            reasoning_effort: None,
        })
        .unwrap();
        assert_eq!(
            body.get("stream_options"),
            Some(&serde_json::json!({"include_usage": true}))
        );
    }

    #[test]
    fn stream_options_omitted_when_disabled() {
        let body = serde_json::to_value(&ChatCompletionsRequest {
            model: "gpt-4",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: None,
            stream: true,
            stream_options: None,
            max_tokens: None,
            max_completion_tokens: None,
            reasoning_effort: None,
        })
        .unwrap();
        assert!(body.get("stream_options").is_none());
    }

    // ── Responses API serialization / deserialization tests ──────────

    #[test]
    fn responses_request_serializes_with_all_fields() {
        let req = ResponsesRequest {
            model: "gpt-5.6-sol",
            input: Some(serde_json::Value::String("hello".into())),
            instructions: Some("be helpful"),
            tools: None,
            stream: true,
            max_output_tokens: Some(1000),
            reasoning_effort: Some("medium"),
            store: true,
            previous_response_id: Some("resp_abc"),
            include: None,
            parallel_tool_calls: None,
            tool_choice: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "gpt-5.6-sol");
        assert_eq!(json["input"], "hello");
        assert_eq!(json["instructions"], "be helpful");
        assert_eq!(json["stream"], true);
        assert_eq!(json["store"], true);
        assert_eq!(json["max_output_tokens"], 1000);
        assert_eq!(json["reasoning_effort"], "medium");
        assert_eq!(json["previous_response_id"], "resp_abc");
    }

    #[test]
    fn responses_request_serializes_with_input_items() {
        let input_items = serde_json::json!([
            {"type": "message", "role": "user", "content": "hello"},
            {"type": "function_call_output", "call_id": "call_1", "output": "result"}
        ]);
        let req = ResponsesRequest {
            model: "gpt-5.6-sol",
            input: Some(input_items),
            instructions: None,
            tools: None,
            stream: false,
            max_output_tokens: None,
            reasoning_effort: None,
            store: false,
            previous_response_id: None,
            include: None,
            parallel_tool_calls: None,
            tool_choice: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["input"][0]["type"], "message");
        assert_eq!(json["input"][0]["content"], "hello");
        assert_eq!(json["input"][1]["type"], "function_call_output");
        assert_eq!(json["input"][1]["call_id"], "call_1");
    }

    #[test]
    fn responses_request_omits_optional_fields_when_none() {
        let req = ResponsesRequest {
            model: "gpt-4.1",
            input: Some(serde_json::Value::String("hi".into())),
            instructions: None,
            tools: None,
            stream: false,
            max_output_tokens: None,
            reasoning_effort: None,
            store: false,
            previous_response_id: None,
            include: None,
            parallel_tool_calls: None,
            tool_choice: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("max_output_tokens").is_none());
        assert!(json.get("previous_response_id").is_none());
        assert!(json.get("include").is_none());
        assert!(json.get("tools").is_none());
        assert!(json.get("parallel_tool_calls").is_none());
        assert!(json.get("instructions").is_none());
        assert!(json.get("tool_choice").is_none());
        assert!(json.get("stream").is_none());
        assert!(json.get("store").is_none());
    }

    #[test]
    fn responses_response_deserializes_message_item() {
        let json = r#"{
            "id": "resp_abc",
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "Hello"}],
                "role": "assistant"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        }"#;
        let resp: ResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id.as_deref(), Some("resp_abc"));
        assert_eq!(resp.output.len(), 1);
        let usage = resp.usage.unwrap();
        assert_eq!(usage.total_tokens, 8);
    }

    #[test]
    fn responses_response_deserializes_function_call_item() {
        let json = r#"{
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "get_weather",
                "arguments": "{\"city\":\"London\"}"
            }]
        }"#;
        let resp: ResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.output.len(), 1);
    }

    #[test]
    fn responses_response_deserializes_reasoning_item() {
        let json = r#"{
            "output": [{
                "type": "reasoning",
                "summary": [{"text": "thinking step 1"}]
            }]
        }"#;
        let resp: ResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.output.len(), 1);
    }

    #[test]
    fn messages_to_responses_input_puts_system_message_in_input() {
        let messages = vec![
            ChatRequestMessage::simple("system", "You are helpful".into()),
            ChatRequestMessage::simple("user", "Hello".into()),
        ];
        let items = messages_to_responses_input(&messages);
        assert_eq!(items.len(), 2);
        match &items[0] {
            ResponsesInputItem::Message { role, content } => {
                assert_eq!(role, "system");
                assert_eq!(content, "You are helpful");
            }
            _ => panic!("expected Message item for system message"),
        }
        match &items[1] {
            ResponsesInputItem::Message { role, content } => {
                assert_eq!(role, "user");
                assert_eq!(content, "Hello");
            }
            _ => panic!("expected Message item for user message"),
        }
    }

    #[test]
    fn messages_to_responses_input_converts_tool_results() {
        let messages = vec![ChatRequestMessage {
            role: "tool",
            content: Some("file content".into()),
            tool_call_id: Some("call_1".into()),
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
        }];
        let items = messages_to_responses_input(&messages);
        assert_eq!(items.len(), 1);
        match &items[0] {
            ResponsesInputItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(output, "file content");
            }
            _ => panic!("expected FunctionCallOutput item"),
        }
    }

    #[test]
    fn responses_tool_converts_from_chat_tool_definition() {
        let chat_tool = ChatToolDefinition::function(
            "get_weather",
            "Get the weather for a city",
            serde_json::json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        );
        let resp_tool = ResponsesTool::from(&chat_tool);
        assert_eq!(resp_tool.kind, "function");
        assert_eq!(resp_tool.name, "get_weather");
        assert_eq!(resp_tool.description, "Get the weather for a city");
        assert_eq!(resp_tool.parameters["properties"]["city"]["type"], "string");
        assert!(!resp_tool.strict);
    }

    #[test]
    fn chat_completion_turn_dispatches_to_responses_when_configured() {
        let mut config = ServiceConfig::default();
        config.default_request_format = RequestFormat::Responses;
        assert_eq!(
            config.request_format_for_model("gpt-4"),
            RequestFormat::Responses
        );
        // Per-model override to ChatCompletions should take precedence.
        config
            .model_request_formats
            .insert("gpt-4".to_string(), RequestFormat::ChatCompletions);
        assert_eq!(
            config.request_format_for_model("gpt-4"),
            RequestFormat::ChatCompletions
        );
    }

    // ── build_responses_input tests ──────────────────────────────────

    #[test]
    fn build_responses_input_first_turn_with_no_tool_results() {
        let messages = vec![
            ChatRequestMessage::simple("system", "be helpful".into()),
            ChatRequestMessage::simple("user", "Hello".into()),
        ];
        let input = build_responses_input(&[], &messages).expect("should succeed");
        let input_value = input.expect("input should be Some");
        assert_eq!(input_value[0]["type"], "message");
        assert_eq!(input_value[0]["role"], "system");
        assert_eq!(input_value[0]["content"], "be helpful");
        assert_eq!(input_value[1]["type"], "message");
        assert_eq!(input_value[1]["role"], "user");
        assert_eq!(input_value[1]["content"], "Hello");
    }

    #[test]
    fn build_responses_input_first_turn_preserves_assistant_messages() {
        let messages = vec![
            ChatRequestMessage::simple("assistant", "Hi there".into()),
            ChatRequestMessage::simple("user", "What is the weather?".into()),
        ];
        let input = build_responses_input(&[], &messages).expect("should succeed");
        let input_value = input.expect("input should be Some");
        let arr = input_value.as_array().expect("expected array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "assistant");
        assert_eq!(arr[0]["content"], "Hi there");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[1]["content"], "What is the weather?");
    }

    #[test]
    fn build_responses_input_tool_results_produces_function_call_output() {
        let tool_results = vec![
            ToolResultItem {
                call_id: "call_1".into(),
                output: "sunny".into(),
                caller: None,
            },
            ToolResultItem {
                call_id: "call_2".into(),
                output: "error: timeout".into(),
                caller: None,
            },
        ];
        let input = build_responses_input(&tool_results, &[]).expect("should succeed");
        let input_value = input.expect("input should be Some");
        assert_eq!(input_value.as_array().map(|a| a.len()), Some(2));
        assert_eq!(input_value[0]["type"], "function_call_output");
        assert_eq!(input_value[0]["call_id"], "call_1");
        assert_eq!(input_value[0]["output"], "sunny");
        assert_eq!(input_value[1]["call_id"], "call_2");
        assert_eq!(input_value[1]["output"], "error: timeout");
    }

    #[test]
    fn build_responses_input_tool_results_ignores_messages() {
        let tool_results = vec![ToolResultItem {
            call_id: "call_1".into(),
            output: "42".into(),
            caller: None,
        }];
        // Even with messages present, tool_results branch is taken
        let messages = vec![ChatRequestMessage::simple("user", "Hello".into())];
        let input = build_responses_input(&tool_results, &messages).expect("should succeed");
        let input_value = input.expect("input should be Some");
        assert_eq!(input_value.as_array().map(|a| a.len()), Some(1));
        assert_eq!(input_value[0]["type"], "function_call_output");
    }

    // ── extract_reasoning_text tests ──────────────────────────────────

    #[test]
    fn extract_reasoning_text_plain_strings() {
        let summary = vec![
            serde_json::Value::String("first".into()),
            serde_json::Value::String("second".into()),
        ];
        assert_eq!(
            extract_reasoning_text(&summary).as_deref(),
            Some("first second")
        );
    }

    #[test]
    fn extract_reasoning_text_objects_with_text_field() {
        let summary = vec![
            serde_json::json!({"text": "thinking step 1"}),
            serde_json::json!({"text": "step 2"}),
        ];
        assert_eq!(
            extract_reasoning_text(&summary).as_deref(),
            Some("thinking step 1 step 2")
        );
    }

    #[test]
    fn extract_reasoning_text_mixed_strings_and_objects() {
        let summary = vec![
            serde_json::Value::String("start".into()),
            serde_json::json!({"text": "middle"}),
            serde_json::Value::String("end".into()),
        ];
        assert_eq!(
            extract_reasoning_text(&summary).as_deref(),
            Some("start middle end")
        );
    }

    #[test]
    fn extract_reasoning_text_empty_array_returns_none() {
        assert!(extract_reasoning_text(&[]).is_none());
    }

    #[test]
    fn extract_reasoning_text_no_textual_content_returns_none() {
        let summary = vec![
            serde_json::json!({"other": "field"}),
            serde_json::Value::Null,
            serde_json::json!({"score": 42}),
        ];
        assert!(extract_reasoning_text(&summary).is_none());
    }

    #[test]
    fn extract_reasoning_text_skips_non_text_entries_joins_rest() {
        let summary = vec![
            serde_json::json!({"text": "valid"}),
            serde_json::json!({"score": 42}),
            serde_json::Value::String("also valid".into()),
        ];
        assert_eq!(
            extract_reasoning_text(&summary).as_deref(),
            Some("valid also valid")
        );
    }

    // ── function_with_options tests ──────────────────────────────────

    #[test]
    fn function_with_options_sets_output_schema_and_allowed_callers() {
        let tool = ChatToolDefinition::function_with_options(
            "get_weather",
            "Get the weather",
            serde_json::json!({"type": "object"}),
            Some(serde_json::json!({"type": "string"})),
            Some(vec![AllowedCaller::Direct, AllowedCaller::Programmatic]),
        );
        assert_eq!(tool.function.name, "get_weather");
        assert_eq!(
            tool.function.output_schema,
            Some(serde_json::json!({"type": "string"}))
        );
        assert_eq!(
            tool.function.allowed_callers,
            Some(vec![AllowedCaller::Direct, AllowedCaller::Programmatic])
        );
    }

    #[test]
    fn function_with_options_none_fields() {
        let tool = ChatToolDefinition::function_with_options(
            "noop",
            "Does nothing",
            serde_json::json!({"type": "object"}),
            None,
            None,
        );
        assert!(tool.function.output_schema.is_none());
        assert!(tool.function.allowed_callers.is_none());
    }

    // ── ResponsesTool conversion tests ───────────────────────────────

    #[test]
    fn responses_tool_from_function_with_options() {
        let chat_tool = ChatToolDefinition::function_with_options(
            "search",
            "Search the web",
            serde_json::json!({"type": "object"}),
            Some(serde_json::json!({"type": "string"})),
            Some(vec![AllowedCaller::Direct]),
        );
        let resp_tool = ResponsesTool::from(&chat_tool);
        assert_eq!(resp_tool.kind, "function");
        assert_eq!(resp_tool.name, "search");
        assert_eq!(
            resp_tool.output_schema,
            Some(serde_json::json!({"type": "string"}))
        );
        assert_eq!(resp_tool.allowed_callers, Some(vec![AllowedCaller::Direct]));
    }

    #[test]
    fn responses_tool_from_plain_function_has_no_options() {
        let chat_tool = ChatToolDefinition::function(
            "ping",
            "Ping test",
            serde_json::json!({"type": "object"}),
        );
        let resp_tool = ResponsesTool::from(&chat_tool);
        assert_eq!(resp_tool.name, "ping");
        assert!(resp_tool.output_schema.is_none());
        assert!(resp_tool.allowed_callers.is_none());
    }

    // ── build_responses_request_body with programmatic_tool_calling ──

    #[test]
    fn build_responses_request_body_includes_programmatic_tool() {
        let config = ServiceConfig::default();
        let tools = vec![ChatToolDefinition::function(
            "get_weather",
            "Get weather",
            serde_json::json!({"type": "object"}),
        )];
        let (url, body) = build_responses_request_body(
            &config,
            "gpt-5.6-sol",
            &[], // messages
            &tools,
            None,  // reasoning_effort
            None,  // previous_response_id
            &[],   // tool_results
            false, // stream
            true,  // programmatic_tool_calling
        )
        .expect("request body");
        assert!(url.contains("/responses"));
        let tools_arr = body["tools"].as_array().expect("tools array");
        // Should have the regular tool + the programmatic_tool_calling tool
        assert_eq!(tools_arr.len(), 2);
        assert_eq!(tools_arr[0]["type"], "function");
        assert_eq!(tools_arr[1]["type"], "programmatic_tool_calling");
        // The programmatic tool should only have the type field
        assert!(tools_arr[1].get("name").is_none());
        assert!(tools_arr[1].get("description").is_none());
        assert!(tools_arr[1].get("parameters").is_none());
    }

    #[test]
    fn build_responses_request_body_without_programmatic_tool() {
        let config = ServiceConfig::default();
        let tools = vec![ChatToolDefinition::function(
            "get_weather",
            "Get weather",
            serde_json::json!({"type": "object"}),
        )];
        let (_url, body) = build_responses_request_body(
            &config,
            "gpt-5.6-sol",
            &[], // messages
            &tools,
            None,  // reasoning_effort
            None,  // previous_response_id
            &[],   // tool_results
            false, // stream
            false, // programmatic_tool_calling disabled
        )
        .expect("request body");
        let tools_arr = body["tools"].as_array().expect("tools array");
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["type"], "function");
    }

    #[test]
    fn build_responses_request_body_programmatic_tool_only() {
        // No regular tools, only programmatic tool calling enabled.
        let config = ServiceConfig::default();
        let (_url, body) = build_responses_request_body(
            &config,
            "gpt-5.6-sol",
            &[],   // messages
            &[],   // tools
            None,  // reasoning_effort
            None,  // previous_response_id
            &[],   // tool_results
            false, // stream
            true,  // programmatic_tool_calling
        )
        .expect("request body");
        let tools_arr = body["tools"].as_array().expect("tools array");
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["type"], "programmatic_tool_calling");
    }

    // ── FunctionCallOutput with caller serialization ─────────────────

    #[test]
    fn function_call_output_serializes_without_caller_when_none() {
        let item = ResponsesInputItem::FunctionCallOutput {
            call_id: "call_1".into(),
            output: "result".into(),
            caller: None,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["type"], "function_call_output");
        assert_eq!(json["call_id"], "call_1");
        assert_eq!(json["output"], "result");
        assert!(
            json.get("caller").is_none(),
            "caller should be omitted when None"
        );
    }

    #[test]
    fn function_call_output_serializes_with_caller() {
        let item = ResponsesInputItem::FunctionCallOutput {
            call_id: "call_1".into(),
            output: "result".into(),
            caller: Some(CallerInfo {
                kind: "program".into(),
                caller_id: "prog_1".into(),
            }),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["caller"]["type"], "program");
        assert_eq!(json["caller"]["caller_id"], "prog_1");
    }

    // ── Response output item deserialization tests ───────────────────

    #[test]
    fn response_output_item_deserializes_function_call_with_caller() {
        let json = r#"{
            "type": "function_call",
            "call_id": "call_1",
            "name": "get_weather",
            "arguments": "{\"city\":\"London\"}",
            "caller": {"type": "program", "caller_id": "prog_1"}
        }"#;
        let item: ResponseOutputItem = serde_json::from_str(json).unwrap();
        match item {
            ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                caller,
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name, "get_weather");
                assert_eq!(arguments, r#"{"city":"London"}"#);
                let caller = caller.expect("caller should be present");
                assert_eq!(caller.kind, "program");
                assert_eq!(caller.caller_id, "prog_1");
            }
            _ => panic!("expected FunctionCall variant"),
        }
    }

    #[test]
    fn response_output_item_deserializes_program() {
        let json = r#"{
            "type": "program",
            "call_id": "prog_1",
            "code": "console.log('hello')",
            "fingerprint": "fp_abc"
        }"#;
        let item: ResponseOutputItem = serde_json::from_str(json).unwrap();
        match item {
            ResponseOutputItem::Program {
                call_id,
                code,
                fingerprint,
                ..
            } => {
                assert_eq!(call_id, "prog_1");
                assert_eq!(code.as_deref(), Some("console.log('hello')"));
                assert_eq!(fingerprint.as_deref(), Some("fp_abc"));
            }
            _ => panic!("expected Program variant"),
        }
    }

    #[test]
    fn response_output_item_deserializes_program_minimal() {
        let json = r#"{
            "type": "program",
            "call_id": "prog_1"
        }"#;
        let item: ResponseOutputItem = serde_json::from_str(json).unwrap();
        match item {
            ResponseOutputItem::Program {
                call_id,
                code,
                fingerprint,
                ..
            } => {
                assert_eq!(call_id, "prog_1");
                assert!(code.is_none());
                assert!(fingerprint.is_none());
            }
            _ => panic!("expected Program variant"),
        }
    }

    #[test]
    fn response_output_item_deserializes_program_output() {
        let json = r#"{
            "type": "program_output",
            "call_id": "prog_1",
            "result": "{\"status\":\"ok\"}",
            "status": "completed"
        }"#;
        let item: ResponseOutputItem = serde_json::from_str(json).unwrap();
        match item {
            ResponseOutputItem::ProgramOutput {
                call_id,
                result,
                status,
                ..
            } => {
                assert_eq!(call_id, "prog_1");
                assert_eq!(result.as_deref(), Some(r#"{"status":"ok"}"#));
                assert_eq!(status.as_deref(), Some("completed"));
            }
            _ => panic!("expected ProgramOutput variant"),
        }
    }

    #[test]
    fn response_output_item_deserializes_program_output_minimal() {
        let json = r#"{
            "type": "program_output",
            "call_id": "prog_1"
        }"#;
        let item: ResponseOutputItem = serde_json::from_str(json).unwrap();
        match item {
            ResponseOutputItem::ProgramOutput {
                call_id,
                result,
                status,
                ..
            } => {
                assert_eq!(call_id, "prog_1");
                assert!(result.is_none());
                assert!(status.is_none());
            }
            _ => panic!("expected ProgramOutput variant"),
        }
    }
}
