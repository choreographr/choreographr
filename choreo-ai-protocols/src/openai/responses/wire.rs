//! Responses API wire types.
//!
//! The request, response, and output-item shapes of `OpenAI`'s Responses API
//! (<https://developers.openai.com/api/docs/guides/reasoning>), shared by the
//! body builders ([`super::body`]) and the request entry points.

use serde::{Deserialize, Serialize};

use super::super::{ChatToolDefinition, Usage};
use crate::cache_retention::CacheRetentionOptions;
use crate::types::CallerInfo;

// ── Responses API wire types ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponsesInputItem {
    Message {
        role: String,
        /// A plain string, or an array of `input_text` / `input_image` parts
        /// when the message carries images (vision). Built via
        /// [`ChatRequestMessage::responses_content_value`].
        content: serde_json::Value,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: String,
        output: String,
        /// The originating call's attribution, copied byte-for-byte from the
        /// `caller` the response's `function_call` item carried. The service
        /// uses this value to resume the correct generated program, so it must
        /// be identical (see
        /// <https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling>).
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<CallerInfo>,
    },
    /// An assistant turn's tool request, replayed when the history is rebuilt
    /// from messages rather than chained through `previous_response_id`.
    ///
    /// Every `function_call_output` names its originating call by `call_id`, so
    /// the call itself has to appear earlier in `input`; the API rejects an
    /// output whose call was never introduced. `arguments` is the raw JSON
    /// string exactly as the model emitted it (never re-serialized).
    ///
    /// A `caller` rides **both** the call and the output that answers it: the
    /// two are one attribution, copied byte-for-byte from the response and
    /// never interpreted here, because the service needs the same value on the
    /// output to resume the program that generated the call. `program` items
    /// are never replayed — programmatic tool calling is withheld on the
    /// stateless dialect, and its one route chains through
    /// `previous_response_id`, so a finished tool round carries only outputs.
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
        /// The call's attribution as the response reported it, replayed
        /// unchanged (see [`ResponsesInputItem::FunctionCallOutput`]).
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<CallerInfo>,
    },
}

/// Nested `reasoning` object for Responses requests.
///
/// This object carries both the effort (`reasoning.effort`) and the reasoning
/// *summary* (`reasoning.summary`) — the summary is **not** an `include` value
/// (the `include` enum is a fixed set; `reasoning.summary` is a 400), and a
/// top-level `reasoning_effort` is not a Responses parameter at all.
#[derive(Debug, Serialize)]
pub(crate) struct ResponsesReasoning<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesRequest<'a> {
    pub(crate) model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<ResponsesReasoning<'a>>,
    /// Extra output-item kinds the response should carry, from the API's fixed
    /// `include` enum.
    ///
    /// Only `reasoning.encrypted_content` is ever requested, and only by the
    /// stateless ChatGPT/Codex dialect: with `store: false` the provider keeps
    /// no server-side transcript, so the opaque encrypted reasoning payload is
    /// the *only* way to hand captured reasoning back on a later request (see
    /// [`crate::ReasoningPassback::AllTurns`]). The reasoning summary is
    /// requested through `reasoning.summary` instead — `include` is a closed
    /// enum and rejects `reasoning.summary` with a 400.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) include: Option<Vec<&'a str>>,
    /// Whether the provider should keep this response so a later request can
    /// chain onto it with `previous_response_id`.
    ///
    /// Three states, and the distinction matters: `Some(true)` stores the
    /// transcript, `Some(false)` is an explicit **don't store** that still
    /// reaches the wire, and `None` omits the field entirely so the backend's
    /// own default applies. A request that must not persist has to say so —
    /// omitting the field would accept whatever the backend stores by default,
    /// which is the opposite of the stateless dialect's contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous_response_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<serde_json::Value>,
    /// Per-session cache-affinity hint for providers that accept it (see
    /// [`crate::CacheAffinity`]); omitted when the provider's dialect is not
    /// `prompt-cache-key` or the turn has no session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_key: Option<&'a str>,
    /// Per-session cache-affinity hint in `OpenRouter`'s dialect (top-level
    /// `session_id`). At most one of the two affinity fields is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<&'a str>,
    /// Extended prompt-cache retention for pre-GPT-5.6 `OpenAI` models (see
    /// [`crate::CacheRetentionDialect`]); omitted unless the model opted in and
    /// declares a long TTL tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_retention: Option<&'static str>,
    /// Extended prompt-cache retention for GPT-5.6+ `OpenAI` models, nested
    /// under `prompt_cache_options`; the Responses API is where the extended
    /// `"12h"` value is accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_options: Option<CacheRetentionOptions>,
}

/// Raw Responses API response envelope.
///
/// Fields like `id` come from the wire but aren't always read in the
/// current code path — they're kept for deserialization completeness
/// and future use (streaming contexts, resumption, etc.).
///
/// `output` holds each item as the raw `serde_json::Value` exactly as
/// received — mirroring the streaming path, which keeps reasoning items as
/// raw values. Display/tool-call extraction parses them lazily into
/// `ResponseOutputItem`, while the reasoning round-trip artifact preserves
/// every field (including unknown ones) byte-for-byte.
#[derive(Debug, Deserialize)]
// Read only in some build targets, so `dead_code` cannot be an `expect`;
// kept as an `allow` with its `allow_attributes` exemption.
#[allow(clippy::allow_attributes)]
#[allow(dead_code)]
pub(crate) struct ResponsesResponse {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) output: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) usage: Option<Usage>,
    /// `incomplete_details: { reason }` on a response that stopped early
    /// (`status: "incomplete"`): `max_output_tokens`, `content_filter`, …
    /// Parsed so the stop keeps its provider-stated cause instead of being
    /// flattened into one error.
    #[serde(default)]
    pub(crate) incomplete_details: Option<ResponsesIncompleteDetails>,
}

/// Why a Responses API response stopped early.
#[derive(Debug, Deserialize)]
pub(crate) struct ResponsesIncompleteDetails {
    /// The provider's stop reason (`max_output_tokens`, `content_filter`, …).
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

/// Items in a Responses API response output array.
///
/// Deserialize-only: the raw `serde_json::Value` of each output item is kept
/// for the round-trip artifact, so this typed form is used solely for
/// display/tool-call extraction and is never re-serialized (re-serializing
/// through the struct would drop unknown fields like a newer `content` shape).
/// Fields like `id`/`role` are kept for wire-format completeness (matching
/// `ResponsesResponse`) even though the current code reads only the fields it
/// needs.
#[derive(Debug, Deserialize)]
#[expect(dead_code)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponseOutputItem {
    Message {
        #[serde(default)]
        content: Vec<ResponseContentPart>,
        #[serde(default)]
        role: Option<String>,
    },
    Reasoning {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        summary: Vec<serde_json::Value>,
        /// Opaque encrypted reasoning content — present only when the request
        /// was sent with `store: false` (stateless mode). Kept verbatim for
        /// the round-trip artifact; never interpreted by the daemon.
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
        #[serde(default)]
        caller: Option<CallerInfo>,
    },
    Program {
        #[serde(default)]
        id: Option<String>,
        call_id: String,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        fingerprint: Option<String>,
    },
    #[serde(rename = "program_output")]
    ProgramOutput {
        #[serde(default)]
        id: Option<String>,
        call_id: String,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        status: Option<String>,
    },
    /// An output-item kind this client does not model.
    ///
    /// The backend behind the stateless Responses dialect is undocumented and
    /// unversioned, so a new item kind can appear in `output` with no counterpart
    /// here — and the streaming path already ignores events it does not model.
    /// Failing the deserialization instead would lose an answer the provider
    /// already billed for, and would make the two paths disagree about the same
    /// response. An unknown item kind is data this client does not model, not a
    /// malformed request, so it is skipped rather than rejected.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseContentPart {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) text: Option<String>,
}

/// A tool definition in a Responses API request.
///
/// For regular function tools all fields are used; for the
/// `programmatic_tool_calling` hosted tool only `type` is needed — the
/// empty name/description/parameters are omitted via `skip_serializing_if`
/// so the wire format matches the `OpenAI` spec (just `{"type":"programmatic_tool_calling"}`).
/// See <https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling>
#[derive(Debug, Serialize)]
pub(crate) struct ResponsesTool {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) description: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub(crate) parameters: serde_json::Value,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) strict: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) allowed_callers: Option<Vec<super::super::AllowedCaller>>,
}

impl From<&ChatToolDefinition> for ResponsesTool {
    fn from(tool: &ChatToolDefinition) -> Self {
        Self {
            kind: "function".to_string(),
            name: tool.function.name.clone(),
            description: tool.function.description.clone(),
            parameters: tool.function.parameters.clone(),
            strict: false,
            output_schema: tool.function.output_schema.clone(),
            allowed_callers: tool.function.allowed_callers.clone(),
        }
    }
}
