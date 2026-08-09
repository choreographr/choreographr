//! # choreo-ai-protocols
//!
//! Provider protocols for Choreographr.  This crate owns the wire-protocol
//! clients that talk to LLM providers, the `ProviderClient` trait they all
//! implement, the shared turn/error/message types the trait is expressed in,
//! and the static provider catalog that maps provider slugs to protocol,
//! base URL, and curated model lists.
//!
//! The crate is intentionally free of daemon concerns: no metrics, no
//! account configuration, no sessions.  The daemon (`choreographr`)
//! depends on this crate and supplies those concerns at the boundary
//! (e.g. via `ProviderOverrides` and by timing calls itself).
//!
//! ## Layout
//!
//! - [`openai`] — OpenAI-compatible client (Chat Completions + Responses API),
//!   plus the canonical [`openai::ChatRequestMessage`] /
//!   [`openai::ChatToolDefinition`] types that the other clients translate
//!   into their own wire formats.
//! - [`anthropic`] — Anthropic Messages API client.
//! - [`google`] — Google Gemini client.
//! - [`catalog`] — provider catalog (bundled TOML data + lookups).
//! - [`retry`] — shared HTTP retry machinery used by all clients.
//! - [`ProviderClient`], [`ChatTurnRequest`], [`ChatTurnResult`] — the trait
//!   and shared types every client implements/uses.
//! - [`ProviderError`], [`ContextWindowConfig`], [`ProviderOverrides`] —
//!   shared error, context-window, and account-override carriers.

pub mod anthropic;
pub mod catalog;
pub mod google;
pub mod openai;
pub mod retry;

// Test-only helpers (scripted HTTP mock provider for the integration tests);
// compiled only when the `test-utils` feature is enabled (see Cargo.toml).
#[cfg(feature = "test-utils")]
pub mod test_utils;

mod context_window;
mod overrides;
mod shared;
mod stream;
mod traits;
mod types;

pub use anthropic::{AnthropicClient, AnthropicConfig};
pub use catalog::{
    ModelEntry, PROVIDER_CATALOG, ProviderEntry, ProviderProtocol, ReasoningPassback,
    all_display_names, all_slugs, lookup_context_window, lookup_provider,
    model_reasoning_capability, model_reasoning_passback, model_request_format,
};
pub use context_window::ContextWindowConfig;
pub use google::{GoogleClient, GoogleConfig};
pub use openai::{AllowedCaller, OpenAiClient, RequestFormat, ServiceConfig};
pub use overrides::ProviderOverrides;
pub use retry::RetryCallback;
pub use shared::{MaxTokensField, ProviderError};
pub use traits::{ChatTurnRequest, ProviderClient, ToolResultItem};
pub use types::{
    CallerInfo, ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
