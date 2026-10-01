//! Cache-warming policy and runtime: the configuration surface, the billing
//! **meter** model, the warm-decision state machine, and the per-request warmer
//! thread that drives it.
//!
//! A warm ping re-sends the last request with a capped output to refresh the
//! provider's in-memory prompt cache before its TTL expires, so the next real
//! turn reads the prefix from cache instead of re-writing it. The ping is only
//! worthwhile when it *saves* more than it costs — and what "saves" means
//! depends on how the account is billed:
//!
//! | `meter` | Gate | Warms? |
//! |---|---|---|
//! | `payg` | `expected_savings_usd >= min_expected_savings` | if the dollar math clears |
//! | `tokens` | `prefix_tokens >= min_prefix_tokens` | if the prefix is large |
//! | `requests` | — | never (a ping burns request budget and saves none) |
//! | `flat` | — | never (unmetered) |
//! | `unknown` | — | never (conservative; we will not guess) |
//!
//! The dollar gate is only valid under pay-as-you-go. Under a monthly or
//! request-metered plan the binding resource is quota/requests, not dollars, so
//! a ping actively *costs* — hence the gate is meter-kind aware and defaults to
//! "do not warm".
//!
//! The module splits cleanly into two halves:
//!
//! * [`policy`] — the **pure, clock-injected** state machine: [`WarmPolicy`]
//!   resolved from the parsed config, and [`WarmPolicy::arm`]/[`WarmPlan::decide`]
//!   taking an injected `now` (`Duration`, a monotonic value the caller
//!   supplies) so nothing reads a clock and everything is deterministic and
//!   testable without timers.
//! * [`driver`] — the runtime consumer ([`spawn_warmer`]): a thread that owns an
//!   [`InferenceProvider`], waits event-driven on a control channel plus a
//!   timer, and on [`Action::Ping`] re-sends the armed request with a capped
//!   output. The agent loop arms it after a `ToolUse` result and just before
//!   the tools run (the blocking window), and the returned [`WarmHandle`] is
//!   dropped at request end so the thread is always joined. The ping is
//!   best-effort: it never touches session/turn state, never retries, and
//!   swallows every error.

mod driver;
mod policy;

#[cfg(test)]
mod tests;

pub use driver::{WarmHandle, WarmRequest, spawn_warmer};
pub use policy::{
    Action, CacheWarmingConfig, CacheWarmingMode, DEFAULT_MIN_EXPECTED_SAVINGS,
    DEFAULT_MIN_PREFIX_TOKENS, Decision, Economics, MeterKind, RetentionTier, SkipReason,
    WarmFacts, WarmPlan, WarmPolicy, is_replayable,
};
