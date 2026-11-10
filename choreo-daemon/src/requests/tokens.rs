//! Prompt-size estimation for the context-window display and compaction
//! weighting: `estimate_prompt_tokens` and the fixed per-image estimate it
//! counts with ([`IMAGE_TOKEN_ESTIMATE`]).
//! Re-exported from `requests.rs` via `pub(crate) use tokens::*;`.
//!
//! The estimate is informational only — billing always uses the provider's
//! reported usage — and it carries a billing-shaped decision (what a chained
//! request actually costs, spelled out on `estimate_prompt_tokens`). It is
//! separate from `run_agent_loop` so that decision is documented in one place
//! instead of inside the loop's per-turn body.
use crate::reasoning::reasoning_artifact_tokens;
use choreo_ai_protocols::openai::{ChatRequestMessage, ChatToolDefinition};
use tracing::warn;

/// Estimate the number of prompt tokens for the current request using
/// tiktoken.  Returns a (encoding, `estimated_tokens`) pair so the caller
/// can reuse the encoding for output-token counting during streaming.
///
/// The estimate counts the `messages` slice as-is, which is the FULL visible
/// conversation. For a chained request (`previous_response_id` set) that is
/// deliberate: the adapter trims only the *wire* payload to the chain tail,
/// but the provider bills the whole context it holds server-side — and the
/// full conversation in `messages` IS that chained context plus the new tail,
/// so counting it already reflects the real billed input (the only thing it
/// misses is the previous system prompt, which stays in the chain while the
/// rebuilt one is sent afresh — a bounded, sub-request-sized undercount).
/// There is therefore NO chained-context addend here: adding the last
/// request's actual `prompt_tokens` (from usage) would count the conversation
/// twice, roughly doubling the estimate.
pub(crate) fn estimate_prompt_tokens(
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
) -> (Option<&'static tiktoken::CoreBpe>, u32) {
    let encoding =
        tiktoken::encoding_for_model(model).or_else(|| tiktoken::get_encoding("cl100k_base"));
    let estimated = if let Some(enc) = &encoding {
        // Reasoning artifacts are NOT excluded: since phase 4b the builder
        // attaches them to assistant messages under echo policies
        // (ToolLoop/AllTurns/Signature), and providers bill replayed
        // reasoning as input tokens (the round-trip payload is part of the
        // context on keep-all models). The legacy string fields
        // (reasoning_content/reasoning/reasoning_text) are still never
        // populated by the daemon, so only `reasoning_artifact` is counted.
        let content_tokens: u32 = messages
            .iter()
            .filter_map(|m| m.content.as_deref())
            // u128→u32 token counts: real conversations are far below u32::MAX,
            // and the estimate is informational (billing uses provider usage).
            .map(|text| {
                let n = enc.count(text);
                #[expect(clippy::cast_possible_truncation)]
                {
                    n as u32
                }
            })
            .sum();

        // Vision images are billed by the provider as tokens based on their
        // (resized) dimensions. We don't know the exact per-provider
        // tokenizer for images, so use a fixed estimate (~1000 tokens/image):
        // the estimate feeds the
        // context-window display and compaction weighting, not billing.
        let image_tokens: u32 = messages
            .iter()
            .map(|m| {
                #[expect(clippy::cast_possible_truncation)]
                // usize→u32 image count: turns never carry 4 billion images
                (m.images.len() as u32).saturating_mul(IMAGE_TOKEN_ESTIMATE)
            })
            .sum();

        let tool_call_tokens: u32 = messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flat_map(|calls| calls.iter())
            .map(|tc| {
                #[expect(clippy::cast_possible_truncation)]
                // u128→u32 token counts, informational only
                let tc_tokens = (enc.count(&tc.id) + enc.count(&tc.kind)) as u32
                    + (enc.count(&tc.function.name) + enc.count(&tc.function.arguments)) as u32;
                tc_tokens
            })
            .sum();

        let tool_def_tokens: u32 = tools
            .iter()
            .filter_map(|def| match serde_json::to_string(def) {
                Ok(s) => {
                    #[expect(clippy::cast_possible_truncation)]
                    // u128→u32 token counts, informational only
                    Some(enc.count(&s) as u32)
                }
                Err(e) => {
                    warn!(error = %e, "failed to serialize tool definition for token estimation");
                    None
                }
            })
            .sum();

        let artifact_tokens: u32 = messages
            .iter()
            .filter_map(|m| m.reasoning_artifact.as_ref())
            .map(|artifact| reasoning_artifact_tokens(enc, artifact))
            .sum();

        content_tokens + tool_call_tokens + tool_def_tokens + artifact_tokens + image_tokens
    } else {
        // Effectively unreachable — `get_encoding("cl100k_base")` above
        // always succeeds — but kept as defense-in-depth: if the fallback
        // encoding ever fails to load, report 0 rather than panic or reuse
        // a stale estimate. The estimate is informational only (billing
        // uses the provider-reported usage).
        tracing::warn!("no tiktoken encoding available for {model}");
        0
    };
    (encoding, estimated)
}

/// Fixed per-image token estimate for prompt-token accounting. Providers bill
/// image input as tokens derived from (resized) dimensions, with no portable
/// way to compute the exact count client-side; ~1000 tokens/image is a good
/// middle estimate (`DeepSeek` caps at 384,
/// Anthropic/OpenAI high-detail run higher). This feeds the context-window
/// display and compaction weighting, not billing (which uses provider usage).
pub const IMAGE_TOKEN_ESTIMATE: u32 = 1000;
