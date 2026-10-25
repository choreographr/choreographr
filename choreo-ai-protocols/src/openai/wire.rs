//! The small wire shapes the `OpenAI` client parses itself.
//!
//! These are the envelopes that belong to no single adapter: the token-usage
//! object both the chat-completions and Responses replies report, and the
//! `/models` listing the client's own `validate_and_list_models` reads. The
//! per-format request/response types live with their adapters
//! (`chat_completions::wire`, `responses::wire`).

use choreo_proto::TokenUsage;
use serde::Deserialize;

/// The `/models` envelope: the provider's model list under `data`.
#[derive(Debug, Deserialize)]
pub(super) struct ModelListResponse {
    pub(super) data: Vec<ModelInfo>,
}

/// One entry of the `/models` list; only the id is read.
#[derive(Debug, Deserialize)]
pub(super) struct ModelInfo {
    pub(super) id: String,
}

/// Token usage as reported by the provider, shared by every `OpenAI` reply
/// shape: chat completions and Responses both report the same three top-level
/// counts, and both nest a prompt-cache hit count (under a different key, see
/// [`CachedTokenDetails`]).
///
/// Providers differ in how much of the cache breakdown they expose, so every
/// cache field is optional and at most one shape is present. [`Usage::cache_hit_tokens`]
/// is the single resolver for them; nothing else reads the raw fields.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Usage {
    /// The provider's prompt/input count for this request.
    ///
    /// This is the FULL input: `DeepSeek`'s `prompt_tokens` already includes
    /// the cache-hit tokens it reports alongside in
    /// `prompt_cache_hit_tokens`, so that count is a breakdown of this one, not
    /// an addition to it. Nothing may add `cached_tokens` to this value.
    pub(crate) prompt_tokens: u32,
    pub(crate) completion_tokens: u32,
    pub(crate) total_tokens: u32,
    /// Chat-completions per-prompt breakdown. z.ai (and `OpenAI` itself) report
    /// `prompt_tokens_details.cached_tokens` here; providers that omit the
    /// object entirely still parse via `#[serde(default)]`.
    #[serde(default)]
    pub(crate) prompt_tokens_details: Option<CachedTokenDetails>,
    /// Responses API per-input breakdown — the same `{ cached_tokens }` shape
    /// under `input_tokens_details`.
    #[serde(default)]
    pub(crate) input_tokens_details: Option<CachedTokenDetails>,
    /// `DeepSeek`'s flat cache-hit count, reported at the top level of `usage`
    /// instead of inside a details object.
    #[serde(default)]
    pub(crate) prompt_cache_hit_tokens: Option<u32>,
    /// `DeepSeek`'s flat cache-miss count. It completes the flat pair so the
    /// two counts are parsed together (hit + miss covers the whole prompt),
    /// but no projection consumes it: the miss count is simply
    /// `prompt_tokens - cached_tokens`, and nothing downstream needs a second
    /// way to say that.
    // Read only by the derived `Deserialize`; every other read goes through
    // `cache_hit_tokens`. Kept as an `expect` because no test target reads it
    // either, so the lint fires in every build.
    #[expect(dead_code)]
    #[serde(default)]
    pub(crate) prompt_cache_miss_tokens: Option<u32>,
}

/// The `{ cached_tokens }` sub-object both `OpenAI` shapes nest their
/// prompt-cache hit count in: `prompt_tokens_details` for chat completions and
/// `input_tokens_details` for the Responses API.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CachedTokenDetails {
    #[serde(default)]
    pub(crate) cached_tokens: u32,
}

impl Usage {
    /// The prompt-cache hit count, from whichever shape the provider sent.
    ///
    /// The nested details objects are the canonical `OpenAI` shape, so they win
    /// when present; `DeepSeek`'s flat `prompt_cache_hit_tokens` is the
    /// fallback for a provider that reports only the flat pair. Preferring the
    /// details object matters when a provider sends both: it is the field the
    /// provider documents as authoritative, so a stray flat count must not
    /// override it.
    fn cache_hit_tokens(&self) -> u32 {
        self.prompt_tokens_details
            .as_ref()
            .or(self.input_tokens_details.as_ref())
            .map_or_else(
                || self.prompt_cache_hit_tokens.unwrap_or(0),
                |details| details.cached_tokens,
            )
    }

    /// Project the provider usage onto the shared [`TokenUsage`] counter.
    ///
    /// This is the ONE projection every `OpenAI` path (chat completions
    /// streaming and non-streaming, Responses streaming and non-streaming)
    /// uses, so a newly modelled cache field is picked up everywhere at once.
    ///
    /// `input_tokens` stays the provider's `prompt_tokens` verbatim — cache
    /// hits are already inside it on every shape this client talks to, so
    /// `cached_tokens` must never be added to it.
    pub(crate) fn to_token_usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cached_tokens: self.cache_hit_tokens(),
            // No `OpenAI`-compatible shape carries a cache-write count: the
            // cache is populated implicitly by the provider, never billed as a
            // separate write. Anthropic is the one adapter that reports one.
            cache_write_tokens: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Usage;

    fn parse(usage: &str) -> Usage {
        serde_json::from_str(usage).expect("usage parses")
    }

    #[test]
    fn flat_deepseek_hit_tokens_project_into_cached_tokens() {
        // DeepSeek reports the cache breakdown as two flat top-level fields and
        // no details object at all.
        let usage = parse(
            r#"{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,
                "prompt_cache_hit_tokens":64,"prompt_cache_miss_tokens":36}"#,
        );
        let projected = usage.to_token_usage();
        assert_eq!(projected.cached_tokens, 64);
        // `prompt_tokens` already includes the hits, so it must not grow.
        assert_eq!(projected.input_tokens, 100);
        assert_eq!(projected.cache_write_tokens, 0);
    }

    #[test]
    fn nested_details_win_when_both_cache_shapes_are_present() {
        let usage = parse(
            r#"{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,
                "prompt_tokens_details":{"cached_tokens":50},
                "prompt_cache_hit_tokens":64}"#,
        );
        assert_eq!(usage.to_token_usage().cached_tokens, 50);
    }

    #[test]
    fn responses_input_tokens_details_project_into_cached_tokens() {
        let usage = parse(
            r#"{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,
                "input_tokens_details":{"cached_tokens":80}}"#,
        );
        assert_eq!(usage.to_token_usage().cached_tokens, 80);
    }

    #[test]
    fn no_cache_shape_projects_zero_cached_tokens() {
        let usage = parse(r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#);
        let projected = usage.to_token_usage();
        assert_eq!(projected.cached_tokens, 0);
        assert_eq!(projected.input_tokens, 10);
        assert_eq!(projected.output_tokens, 5);
        assert_eq!(projected.total_tokens, 15);
    }
}
