//! The small wire shapes the `OpenAI` client parses itself.
//!
//! These are the envelopes that belong to no single adapter: the token-usage
//! object both the chat-completions and Responses replies report, and the
//! `/models` listing the client's own `validate_and_list_models` reads. The
//! per-format request/response types live with their adapters
//! (`chat_completions::wire`, `responses::wire`).

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
/// shape (chat completions and Responses both use these field names).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Usage {
    pub(crate) prompt_tokens: u32,
    pub(crate) completion_tokens: u32,
    pub(crate) total_tokens: u32,
    /// `OpenAI`-style per-prompt breakdown. z.ai (and `OpenAI` itself) report
    /// `prompt_tokens_details.cached_tokens` here; providers that omit the
    /// object entirely (e.g. plain `DeepSeek`) still parse via `#[serde(default)]`.
    /// NOTE: `DeepSeek` also exposes a differently-shaped flat
    /// `prompt_cache_hit_tokens` field — parsing that is a possible follow-up.
    #[serde(default)]
    pub(crate) prompt_tokens_details: Option<PromptTokensDetails>,
}

/// The `prompt_tokens_details` sub-object, of which only the cached-prompt
/// count is read today.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    pub(crate) cached_tokens: u32,
}
