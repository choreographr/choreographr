//! The catalog's data types: the provider and per-model entries plus the
//! policy enums stored on them.
//!
//! These are the shapes the two-layer catalog pipeline produces and every
//! lookup reads — the normalized models.dev snapshot merged with the bundled
//! overlay (see [`super`]). They carry no behaviour beyond small derivations
//! ([`ProviderEntry::auth_kind`], [`ProviderProtocol`]'s display form), so they
//! live apart from the process-wide `ArcSwap` global and the lookups.

use std::fmt;

use choreo_proto::ChatReasoningField;
use serde::{Deserialize, Serialize};

use crate::retry::RequestCompression;
use crate::shared::MaxTokensField;

/// How reasoning is replayed back to the provider on subsequent turns.
/// The "unset" meaning lives in the outer `Option` on [`ModelEntry`]
/// (`None` → derive from protocol); `None` *here* is an explicit
/// "never replay" override. [`super::model_reasoning_passback`] resolves the
/// effective format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPassback {
    /// Never send reasoning back (display-only providers / fields).
    None,
    /// Echo reasoning on assistant messages that had tool calls — the
    /// minimum for a chat tool loop. A route whose separate chat fact demands
    /// the echoed field on every assistant message echoes it on all of them
    /// instead, because those upstreams ask for the continuity itself:
    /// `DeepSeek` wants every earlier turn's `reasoning_content` back once the
    /// request carries `tools`
    /// ([docs](https://api-docs.deepseek.com/guides/thinking_mode)) and `GLM`
    /// treats a missing block as a degraded preserved-thinking context
    /// ([docs](https://docs.z.ai/guides/capabilities/thinking-mode)).
    ToolLoop,
    /// Echo reasoning across all turns of the session (Anthropic keep-all,
    /// GPT-5.6 `all_turns`).
    AllTurns,
    /// Send back encrypted thought signatures (Gemini).
    Signature,
    /// Chain via `previous_response_id` / opaque reasoning items
    /// (OpenAI/xAI Responses).
    ResponseId,
}

/// Per-model token prices as recorded by models.dev, in **USD per million
/// tokens** ($/M).
///
/// `input`/`output` are always present whenever the snapshot records a cost
/// object; `cache_read`/`cache_write` are frequently absent (many
/// OpenAI-compatible models record only `input`/`output`/`cache_read`), so
/// they are optional. Consumed by the cache-warming cost gate — currently
/// **recorded but unwired**: no production caller reads it yet, alongside
/// `model_supports_temperature`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    /// Prompt (input) price, USD per million tokens.
    pub input: f64,
    /// Completion (output) price, USD per million tokens.
    pub output: f64,
    /// Cache-read (cached prompt) price, USD per million tokens, when the
    /// snapshot records one.
    #[serde(default)]
    pub cache_read: Option<f64>,
    /// Cache-write price, USD per million tokens, when the snapshot records
    /// one.
    #[serde(default)]
    pub cache_write: Option<f64>,
}

/// Prompt-cache TTL policy for a provider or model: the lifetimes, in seconds,
/// of the ephemeral prompt-cache tiers a provider exposes.
///
/// `short_secs` is the default ephemeral tier (~5 minutes at Anthropic);
/// `long_secs` is the extended tier (~1 hour). Both are optional so a provider
/// or model can declare only the tiers it actually supports. This is **overlay
/// policy** — models.dev carries no TTL fact — and the per-model value,
/// when present, overrides the provider default (see [`super::prompt_cache_ttl`]).
/// Consumed by cache-warming TTL scheduling — currently **recorded but
/// unwired** (no production caller yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheTtl {
    /// Short (default) ephemeral tier lifetime, in seconds.
    #[serde(default)]
    pub short_secs: Option<u32>,
    /// Long (extended) ephemeral tier lifetime, in seconds.
    #[serde(default)]
    pub long_secs: Option<u32>,
}

/// Per-model metadata in the provider catalog.
/// A single source of truth for context window, reasoning support,
/// explicit effort levels, and reasoning round-trip format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Model id (exact wire name).
    pub model: String,
    /// Context window in tokens; `0` means unknown.
    pub context_window: u32,
    /// Whether this model supports reasoning/thinking at all.
    /// Applicable across all protocols — `OpenAi`, `AnthropicMessages`,
    /// and `GoogleGenerativeAi` all use this flag to enable/disable
    /// their respective reasoning features per model.
    /// When `false`, the model entry's `openai_reasoning_levels`
    /// (if any) is ignored and the model is treated as non-reasoning.
    pub reasoning_supported: bool,
    /// Explicit reasoning effort level slugs for this model.
    /// Only meaningful when `reasoning_supported` is true AND the
    /// provider protocol is `OpenAi` (enforced by
    /// `model_reasoning_capability()`). Non-OpenAi protocols always
    /// use their own default levels — see `protocol_default_levels()`.
    pub openai_reasoning_levels: Vec<String>,
    /// Whether this model uses `OpenAI`'s Responses API vs Chat Completions.
    /// Only relevant for `OpenAi` protocol providers.
    pub openai_responses: bool,
    /// How reasoning is replayed back to the provider on subsequent turns.
    /// `None` means "no override — `model_reasoning_passback` derives the
    /// format from the provider protocol" (Responses → `ResponseId`,
    /// chat completions → `ToolLoop`, Anthropic → `AllTurns`, Google →
    /// `Signature`); `Some(...)` is an explicit per-model override,
    /// including `Some(ReasoningPassback::None)` for "never replay this
    /// model's reasoning".
    pub reasoning_passback: Option<ReasoningPassback>,
    /// The wire field a route wants replayed reasoning shaped as, when the
    /// provider declares one (e.g. DeepSeek/GLM 5.x chat and the
    /// OpenRouter/Ollama `reasoning_details` resellers).
    ///
    /// Ingested from the models.dev snapshot as a FACT — the model's
    /// `interleaved` value names the field — and overridable per-model by the
    /// overlay (`reasoning_echo_field = "…"`, or the older
    /// `reasoning_content_required = true|false` alias). `None` means "no
    /// declared destination": there is NO name-based fallback, so a model not
    /// covered by the snapshot needs an explicit overlay entry.
    ///
    /// This is the *destination*, and it outranks whatever field a response
    /// happened to stream the reasoning under (the artifact's observation) —
    /// see [`super::chat_reasoning_echo`]. Whether the field must also be PRESENT on
    /// every assistant message is the separate
    /// [`reasoning_echo_required`](Self::reasoning_echo_required) fact; a
    /// declared field alone never demands presence.
    #[serde(default)]
    pub reasoning_echo: Option<ChatReasoningField>,
    /// Whether [`reasoning_echo`](Self::reasoning_echo)'s field must be PRESENT on
    /// every assistant message (empty when there is nothing to echo), not merely
    /// be the preferred destination.
    ///
    /// A separate fact from the field name, because a route can want a specific
    /// destination without demanding its presence. `None` means "no requirement"
    /// — presence is opt-in, never inferred from the field.
    #[serde(default)]
    pub reasoning_echo_required: Option<bool>,
    /// Maximum output tokens the model can produce (`0` = unknown), ingested
    /// from the snapshot's `limit.output` and resolvable via
    /// [`super::lookup_max_output_tokens`] (which maps 0 back to `None`).
    #[serde(default)]
    pub max_output_tokens: u32,
    /// Whether the model accepts the `temperature` request parameter,
    /// ingested from the snapshot's `temperature` flag (absent → `true`,
    /// the permissive wire default). Resolvable via
    /// [`super::model_supports_temperature`]. Currently **recorded but unwired**:
    /// no request builder sends a `temperature` parameter today, so the
    /// resolver has no production caller — the fact is kept so the gate
    /// exists the moment temperature sending is added.
    #[serde(default = "default_true")]
    pub supports_temperature: bool,
    /// Whether the snapshot marks the model deprecated (`status ==
    /// "deprecated"`). Purely informational today; surfaced so UIs and
    /// diagnostics can flag stale model picks without a second lookup.
    #[serde(default)]
    pub deprecated: bool,
    /// Whether the model accepts image input (vision). Derived from the
    /// models.dev `modalities.input` array (`"image"` present) at ingestion;
    /// the overlay can override it where the snapshot is wrong or a model is
    /// not covered. `false` = text-only — images must be gated out of the
    /// request and replaced with a text placeholder (see the vision gate).
    /// `false` is the safe default for unknown models.
    #[serde(default)]
    pub supports_vision: bool,
    /// Whether the model can *produce* images (image output). Derived from
    /// the models.dev `modalities.output` array (`"image"` present) at
    /// ingestion; the overlay can override it the same way vision can.
    /// `false` is the safe default for unknown models (text generation only).
    #[serde(default)]
    pub supports_image_output: bool,
    /// Whether the model can *edit* an input image (image-to-image). This is
    /// **overlay-only** policy: models.dev carries no "edit" modality, so the
    /// flag is never ingested from the snapshot and there is NO name/prefix
    /// heuristic — a model is edit-capable exactly when the bundled (or a
    /// user) overlay sets `supports_image_edit = true`. `false` is the safe
    /// default, so an un-pinned model is never offered to `edit_image` and the
    /// tool fails closed rather than guessing.
    #[serde(default)]
    pub supports_image_edit: bool,
    /// Token prices (USD per million tokens) from the snapshot's `cost` object,
    /// or `None` when the model records no cost. Consumed by the cache-warming
    /// cost gate — currently recorded but unwired.
    #[serde(default)]
    pub cost: Option<ModelCost>,
    /// Per-model prompt-cache TTL override (overlay policy). `None` means "no
    /// override — fall through to the provider default" in [`super::prompt_cache_ttl`].
    #[serde(default)]
    pub prompt_cache: Option<PromptCacheTtl>,
}

// Manual `impl` rather than `#[derive(Default)]` + `#[default]` because
// `supports_temperature` must default to `true` (the permissive wire default
// shared with its `#[serde(default = "default_true")]`); derive can only
// express `false` for a `bool`. Every other field's natural zero-value default
// already matches its serde behavior (`0` for unknown windows/tokens, `None`
// for unset options), so this keeps the serde attributes untouched and the
// wire format byte-identical while letting tests say `..Default::default()`.
impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            model: String::new(),
            context_window: 0,
            reasoning_supported: false,
            openai_reasoning_levels: Vec::new(),
            openai_responses: false,
            reasoning_passback: None,
            reasoning_echo: None,
            reasoning_echo_required: None,
            max_output_tokens: 0,
            supports_temperature: true,
            deprecated: false,
            supports_vision: false,
            supports_image_output: false,
            supports_image_edit: false,
            cost: None,
            prompt_cache: None,
        }
    }
}

/// Protocol variant — selects wire format and carries protocol-specific fields.
///
/// The serde representation is the default externally-tagged enum encoding
/// (e.g. `{"OpenAi": {"max_tokens_field": "max_completion_tokens"}}`); it only
/// needs to round-trip for the embedded-artifact pipeline, so the simplest
/// correct repr is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProviderProtocol {
    /// OpenAI-compatible: Chat Completions or Responses, with the field that
    /// carries the output-token limit.
    OpenAi {
        /// The JSON field carrying the output-token limit.
        max_tokens_field: MaxTokensField,
    },
    /// Anthropic Messages API.
    AnthropicMessages,
    /// Google Gemini `generateContent` API.
    GoogleGenerativeAi,
}

/// Per-endpoint OAuth grant encoding: how the authorization server expects a
/// token-request body to be serialized. The code exchange and the refresh can
/// differ (e.g. the `ChatGPT`/Codex backend accepts a form-encoded code
/// exchange but a JSON refresh), so a profile carries one per endpoint.
///
/// The type is defined once in `choreo-shared` (and re-exported by
/// `choreo-proto` for the wire descriptor and persisted inside an OAuth
/// credential), aliased here so the catalog data names the same type rather
/// than a one-for-one copy.
pub use choreo_shared::OAuthGrantEncoding as GrantEncoding;

/// The OAuth profile for a provider: the authorization-server parameters the
/// daemon and the login library need to run a sign-in and to refresh a grant.
///
/// This is **overlay-only policy** — models.dev carries no OAuth facts — and it
/// is parsed all-or-nothing: a profile missing a required field is warned and
/// skipped rather than half applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthProfile {
    /// Authorization-server issuer (e.g. `https://auth.openai.com`).
    pub issuer: String,
    /// Path of the authorization endpoint, relative to `issuer`.
    pub authorize_path: String,
    /// Path of the token endpoint, relative to `issuer`.
    pub token_path: String,
    /// Scopes requested at login.
    pub scopes: Vec<String>,
    /// Loopback redirect ports to try, in order (the first free one wins).
    pub redirect_ports: Vec<u16>,
    /// Grant encoding for the authorization-code exchange.
    pub code_exchange_encoding: GrantEncoding,
    /// Grant encoding for the refresh exchange.
    pub refresh_encoding: GrantEncoding,
    /// Whether the provider offers a device-code (headless) flow.
    pub device_code: bool,
    /// Path of the RFC 8628 device-authorization endpoint (the `usercode`
    /// request), relative to `issuer`. Required only when `device_code` is
    /// true.
    #[serde(default)]
    pub device_authorization_path: Option<String>,
    /// Path of the device-code token-poll endpoint, relative to `issuer`.
    /// Required only when `device_code` is true.
    #[serde(default)]
    pub device_token_path: Option<String>,
    /// A PUBLIC client id only (PKCE). Never a secret; a confidential client
    /// reads its secret from config/env at login.
    pub client_id: Option<String>,
}

/// How a provider authenticates a request, derived from its catalog entry.
///
/// There is deliberately no separate `auth` attribute on the entry: an
/// [`OAuthProfile`] means OAuth and its absence means API key, so the overlay
/// `oauth` block is the single policy source while models.dev remains the fact
/// source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// API-key authentication (the default).
    ApiKey,
    /// OAuth, when the entry carries an [`OAuthProfile`].
    OAuth,
}

/// A provider and its curated model list, loaded from `<slug>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Catalog slug (the key callers look up by).
    pub slug: String,
    /// Human-readable provider name for display.
    pub display_name: String,
    /// Wire protocol this provider speaks.
    pub protocol: ProviderProtocol,
    /// API base URL (empty for models.dev-only providers with no endpoint).
    pub base_url: String,
    /// Model id selected when the account names none.
    pub default_model: String,
    /// The provider's curated model list.
    pub models: Vec<ModelEntry>,
    /// Provider-level prompt-cache TTL default (overlay policy). `None` means
    /// the provider declares no TTL; a per-model override on a [`ModelEntry`]
    /// still takes precedence when present (see [`super::prompt_cache_ttl`]).
    #[serde(default)]
    pub prompt_cache: Option<PromptCacheTtl>,
    /// Whether this provider's endpoint accepts a compressed request body, and
    /// with which coding (`gzip` or `zstd`) — overlay policy; models.dev carries
    /// no such fact. Defaults to [`RequestCompression::Identity`], so a provider
    /// the overlay does not pin is sent an uncompressed body — the safe default,
    /// since the server must decompress. A provider that rejects a compressed
    /// body with HTTP 415 is downgraded to identity for the process (see the
    /// retry layer); this field is the static per-provider policy that seeds that
    /// decision.
    #[serde(default)]
    pub request_compression: RequestCompression,
    /// The provider's OAuth profile, when it can be signed into with OAuth.
    /// Overlay-only policy (models.dev carries no OAuth facts); its presence is
    /// what makes [`ProviderEntry::auth_kind`] report [`AuthKind::OAuth`].
    #[serde(default)]
    pub oauth: Option<OAuthProfile>,
}

impl ProviderEntry {
    /// The provider's auth kind, derived from whether it carries an OAuth
    /// profile.
    #[must_use]
    pub fn auth_kind(&self) -> AuthKind {
        if self.oauth.is_some() {
            AuthKind::OAuth
        } else {
            AuthKind::ApiKey
        }
    }
}

impl fmt::Display for ProviderProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderProtocol::OpenAi { .. } => write!(f, "Protocol: OpenAI"),
            ProviderProtocol::AnthropicMessages => write!(f, "Protocol: Anthropic Messages"),
            ProviderProtocol::GoogleGenerativeAi => write!(f, "Protocol: Google Generative AI"),
        }
    }
}

/// serde default helper: `bool` fields that mean "true when absent"
/// (`supports_temperature`) cannot use plain `#[serde(default)]`, which
/// would default to `false` and flip the permissive wire default.
fn default_true() -> bool {
    true
}
