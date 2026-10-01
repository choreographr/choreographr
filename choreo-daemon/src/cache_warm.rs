//! Cache-warming policy: the configuration surface, the billing **meter**
//! model, and the warm-decision state machine.
//!
//! A warm ping re-sends the last request with a 1-token output cap to refresh
//! the provider's in-memory prompt cache before its TTL expires, so the next
//! real turn reads the prefix from cache instead of re-writing it. The ping is
//! only worthwhile when it *saves* more than it costs — and what "saves" means
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
//! Everything here is **pure and unwired**: [`WarmPolicy`] is resolved from the
//! parsed config and [`WarmPolicy::arm`]/[`WarmPlan::poll`] form a clock-injected
//! state machine, but **no runtime path calls any of it yet** — the daemon's
//! request loop, the provider adapters, and the metrics broadcast are untouched.
//! The intended consumer is a later warmer thread; until that lands this module
//! is exercised only by its unit tests. The decision logic takes an injected
//! `now` (`Duration`, a monotonic value the caller supplies) and never reads a
//! clock itself, so it is fully deterministic and testable without any timers.

use std::fmt;
use std::time::Duration;

use choreo_ai_protocols::catalog::{ModelCost, PromptCacheTtl};
use serde::{Deserialize, Deserializer, Serialize};
use tracing::warn;

/// Default minimum prefix (prompt) size, in tokens, for the `tokens`-metered
/// gate. Sits above the ~24 000-token break-even so warming has margin.
pub const DEFAULT_MIN_PREFIX_TOKENS: u32 = 32_000;

/// Default minimum expected dollar saving, in USD, for the `payg` gate.
pub const DEFAULT_MIN_EXPECTED_SAVINGS: f64 = 0.05;

/// A prompt-cache TTL at or below this many seconds cannot be warmed safely:
/// there is no room to schedule a ping that both fires before expiry and leaves
/// a margin, so such a model is treated as ineligible.
const MIN_TTL_SECS: u32 = 10;

/// How an account is billed. Drives which resource the warm gate treats as
/// binding. Serialized `snake_case`; an unrecognized string deserializes to
/// [`MeterKind::Unknown`] with a warning rather than failing the config parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeterKind {
    /// Pay-as-you-go: dollars are the binding resource, so the dollar gate
    /// applies.
    Payg,
    /// Token-quota metered: the prefix size is the binding resource.
    Tokens,
    /// Request/rate metered: a ping adds to the window and saves none, so
    /// warming is never worthwhile.
    Requests,
    /// Flat/unmetered plan: warming saves nothing, so it is never worthwhile.
    Flat,
    /// Unknown billing model — the conservative default; we will not guess.
    #[default]
    Unknown,
}

impl<'de> Deserialize<'de> for MeterKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // A typo'd meter in accounts.toml must not brick the file, so an
        // unrecognized string warns and falls back to the conservative default
        // (matching how the catalog overlay warns-and-skips unknown keys).
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "payg" => Self::Payg,
            "tokens" => Self::Tokens,
            "requests" => Self::Requests,
            "flat" => Self::Flat,
            "unknown" => Self::Unknown,
            other => {
                warn!(value = %other, "unknown cache-warming meter; treating as 'unknown'");
                Self::Unknown
            }
        })
    }
}

/// Cache-warming mode. Serialized `snake_case`; an unrecognized string
/// deserializes to [`CacheWarmingMode::Off`] with a warning rather than failing
/// the config parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheWarmingMode {
    /// Warming disabled — the conservative default.
    #[default]
    Off,
    /// Warm while a long tool call blocks (the only shipping mode once the
    /// warmer thread lands).
    Streaming,
}

impl<'de> Deserialize<'de> for CacheWarmingMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Same warn-and-default discipline as `MeterKind`: an unknown mode must
        // not reject the whole config file.
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "streaming" => Self::Streaming,
            "off" => Self::Off,
            other => {
                warn!(value = %other, "unknown cache-warming mode; treating as 'off'");
                Self::Off
            }
        })
    }
}

/// The `[cache_warming]` table in config.toml, holding the global defaults a
/// per-account `cache_warming`/`meter` can override.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CacheWarmingConfig {
    /// Global mode; per-account `cache_warming` overrides it.
    #[serde(default)]
    pub mode: CacheWarmingMode,
    /// Minimum prefix size, in tokens, for the `tokens`-metered gate.
    #[serde(default = "default_min_prefix_tokens")]
    pub min_prefix_tokens: u32,
    /// Minimum expected dollar saving, in USD, for the `payg` gate.
    #[serde(default = "default_min_expected_savings")]
    pub min_expected_savings: f64,
}

fn default_min_prefix_tokens() -> u32 {
    DEFAULT_MIN_PREFIX_TOKENS
}

fn default_min_expected_savings() -> f64 {
    DEFAULT_MIN_EXPECTED_SAVINGS
}

impl Default for CacheWarmingConfig {
    fn default() -> Self {
        Self {
            mode: CacheWarmingMode::default(),
            min_prefix_tokens: DEFAULT_MIN_PREFIX_TOKENS,
            min_expected_savings: DEFAULT_MIN_EXPECTED_SAVINGS,
        }
    }
}

/// Which prompt-cache tier a warm ping targets. `Short` is the default
/// ephemeral tier; `Long` is the extended tier when a model exposes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetentionTier {
    /// The default ephemeral tier.
    #[default]
    Short,
    /// The extended (long) tier, when available.
    Long,
}

/// The resolved cache-warming policy for one account: the global config
/// ([`CacheWarmingConfig`]) merged with the account's `meter`/`cache_warming`
/// overrides and its effective prompt-cache setting.
///
/// Resolve once per account (or account switch) and reuse for every warm
/// decision; the resolution precedence lives in [`WarmPolicy::resolve`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarmPolicy {
    /// Effective mode (account override, else global default).
    pub mode: CacheWarmingMode,
    /// Effective billing meter (account value, else [`MeterKind::Unknown`]).
    pub meter: MeterKind,
    /// Whether prompt caching is enabled for this account. `false` makes the
    /// policy permanently ineligible — there is no cache to keep warm.
    pub prompt_cache_enabled: bool,
    /// Effective `tokens`-gate threshold.
    pub min_prefix_tokens: u32,
    /// Effective `payg`-gate threshold.
    pub min_expected_savings: f64,
}

impl WarmPolicy {
    /// Resolve the effective policy.
    ///
    /// Precedence: the account's `cache_warming` overrides the global
    /// [`CacheWarmingConfig::mode`]; the account's `meter` defaults to
    /// [`MeterKind::Unknown`] (never warm) when unset; and prompt caching being
    /// off makes the policy permanently ineligible regardless of the other
    /// knobs.
    #[must_use]
    pub fn resolve(
        config: &CacheWarmingConfig,
        account_meter: Option<MeterKind>,
        account_mode: Option<CacheWarmingMode>,
        prompt_cache_enabled: bool,
    ) -> Self {
        Self {
            mode: account_mode.unwrap_or(config.mode),
            meter: account_meter.unwrap_or_default(),
            prompt_cache_enabled,
            min_prefix_tokens: config.min_prefix_tokens,
            min_expected_savings: config.min_expected_savings,
        }
    }

    /// Arm a plan from the per-request facts, at clock value `now`.
    ///
    /// If the request is ineligible (mode off, caching disabled, no/too-short
    /// TTL, or not replayable) the returned plan is inactive and carries the
    /// [`SkipReason`]. Otherwise it schedules the first warm ping at
    /// `now + delay` — see [`warm_delay_secs`] for the delay rule — and the
    /// refresh deadline a ping must not miss.
    #[must_use]
    pub fn arm(&self, facts: WarmFacts, now: Duration) -> WarmPlan {
        let policy = *self;
        if let Some(reason) = ineligibility(policy, facts) {
            return WarmPlan {
                policy,
                facts,
                state: PlanState::Inactive(reason),
            };
        }
        // `ineligibility` rejects a `None`/too-short TTL, so this is `Some`.
        let Some(ttl_secs) = ttl_secs(facts.cache_ttl, facts.retention) else {
            return WarmPlan {
                policy,
                facts,
                state: PlanState::Inactive(SkipReason::NoTtl),
            };
        };
        let delay_secs = warm_delay_secs(ttl_secs);
        let next_warm_at = now + Duration::from_secs(u64::from(delay_secs));
        // The margin left after the ping is split in half: the ping should land
        // comfortably before the refresh deadline, which is where the cache has
        // roughly half its remaining life gone and a late ping starts to cost
        // more than it saves.
        let remaining_secs = ttl_secs - delay_secs;
        let refresh_deadline = next_warm_at + Duration::from_secs(u64::from(remaining_secs / 2));
        WarmPlan {
            policy,
            facts,
            state: PlanState::Armed {
                next_warm_at,
                refresh_deadline,
            },
        }
    }
}

/// The per-request facts a warm decision is evaluated against. All fields are
/// static for the lifetime of one request — the clock is the only variable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarmFacts {
    /// Whether prompt caching is enabled for this account/request.
    pub prompt_cache_enabled: bool,
    /// The effective prompt-cache TTL, or `None` when the model exposes none.
    pub cache_ttl: Option<PromptCacheTtl>,
    /// The cache tier the request uses.
    pub retention: RetentionTier,
    /// The size of the cacheable prefix, in tokens.
    pub prefix_tokens: u32,
    /// The model's token prices, or `None` when the catalog records none.
    pub cost: Option<ModelCost>,
    /// Whether a 1-token re-send of the last request would be cache-equivalent
    /// (see [`is_replayable`]).
    pub replayable: bool,
}

/// A resolved warm plan: an inactive plan carrying its [`SkipReason`], or an
/// armed plan carrying the two clock deadlines it fires between.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarmPlan {
    policy: WarmPolicy,
    facts: WarmFacts,
    state: PlanState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PlanState {
    /// The request cannot be warmed; the reason is carried for the status line.
    Inactive(SkipReason),
    /// A ping is scheduled at `next_warm_at` and must fire no later than
    /// `refresh_deadline`.
    Armed {
        next_warm_at: Duration,
        refresh_deadline: Duration,
    },
}

/// The next thing the (future) warmer should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Sleep until `now + Duration` and poll again.
    Wait(Duration),
    /// Send the warm ping now.
    Ping,
    /// Stop; the request will not be warmed (or is no longer worth warming).
    Stop(SkipReason),
}

/// Why a warm ping was (or will be) skipped. Used both by the decision and by
/// a future `/session`-style status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Warming is off for this account.
    ModeOff,
    /// Prompt caching is disabled, so there is no cache to keep warm.
    CachingDisabled,
    /// The model exposes no prompt-cache TTL.
    NoTtl,
    /// The TTL is too short to schedule a ping with any margin.
    TtlTooShort,
    /// An Anthropic request with thinking enabled is not replayable.
    NotReplayable,
    /// The billing meter makes warming a net loss (requests/flat/unknown).
    MeterIncompatible,
    /// The request is below the meter's savings threshold.
    BelowThreshold,
    /// The `payg` gate needs token prices the catalog does not record.
    EconomicsUnavailable,
    /// The ping would land past the refresh deadline (a late ping is a
    /// money-losing cache write).
    DeadlineMissed,
    /// The request changed in a way that invalidates the plan (reserved for the
    /// warmer thread; the policy itself never produces this).
    ContextChanged,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::ModeOff => "cache warming is off",
            Self::CachingDisabled => "prompt caching is disabled",
            Self::NoTtl => "the model exposes no prompt-cache TTL",
            Self::TtlTooShort => "the prompt-cache TTL is too short to warm",
            Self::NotReplayable => "the request is not replayable",
            Self::MeterIncompatible => "the billing meter makes warming a net loss",
            Self::BelowThreshold => "the request is below the savings threshold",
            Self::EconomicsUnavailable => "token prices are unavailable",
            Self::DeadlineMissed => "the refresh deadline was missed",
            Self::ContextChanged => "the request context changed",
        };
        f.write_str(text)
    }
}

/// The dollar economics of one warm decision, in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Economics {
    /// What the ping itself costs: a cache read of the prefix plus one output
    /// token.
    pub warm_cost: f64,
    /// What the next real turn costs *extra* if the cache expired: a cache
    /// write (or full-price input) instead of a cache read.
    pub miss_cost: f64,
    /// `miss_cost - warm_cost`; positive means warming pays for itself.
    pub expected_savings: f64,
}

/// A full warm decision — the status-line analog: the inputs the gate saw and
/// the action it chose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// The cacheable prefix size the decision was made for.
    pub prefix_tokens: u32,
    /// The billing meter the gate applied.
    pub meter: MeterKind,
    /// The ping's cost, when token prices are available.
    pub warm_cost: Option<f64>,
    /// The miss's extra cost, when token prices are available.
    pub miss_cost: Option<f64>,
    /// The expected saving, when token prices are available.
    pub expected_savings: Option<f64>,
    /// What to do next.
    pub action: Action,
}

impl WarmPlan {
    /// Evaluate the plan at clock value `now` and return the full decision.
    ///
    /// The state machine is stateless with respect to the clock beyond the two
    /// deadlines computed at [`WarmPolicy::arm`] — a caller that receives
    /// [`Action::Ping`] re-arms with the *same facts* and the *current* `now`
    /// to schedule the next ping; the plan does not carry that re-arm itself.
    #[must_use]
    pub fn decide(&self, now: Duration) -> Decision {
        let economics = economics(self.facts.cost, self.facts.prefix_tokens);
        let action = match self.state {
            PlanState::Inactive(reason) => Action::Stop(reason),
            PlanState::Armed {
                next_warm_at,
                refresh_deadline,
            } => {
                if now > refresh_deadline {
                    // Hard guard: past the deadline the cache has (or is about
                    // to have) expired, so a ping would RE-WRITE the prefix at
                    // ~1.25x the input price — strictly worse than not pinging.
                    // A ping must never fire late.
                    Action::Stop(SkipReason::DeadlineMissed)
                } else if now < next_warm_at {
                    Action::Wait(next_warm_at.saturating_sub(now))
                } else {
                    match gate(&self.policy, &self.facts, economics) {
                        Ok(()) => Action::Ping,
                        Err(reason) => Action::Stop(reason),
                    }
                }
            }
        };
        Decision {
            prefix_tokens: self.facts.prefix_tokens,
            meter: self.policy.meter,
            warm_cost: economics.map(|e| e.warm_cost),
            miss_cost: economics.map(|e| e.miss_cost),
            expected_savings: economics.map(|e| e.expected_savings),
            action,
        }
    }

    /// The next [`Action`] at clock value `now` — the decision minus the
    /// status fields.
    #[must_use]
    pub fn poll(&self, now: Duration) -> Action {
        self.decide(now).action
    }
}

/// Whether a 1-token re-send of an Anthropic-protocol request with thinking
/// enabled would be cache-equivalent to the original.
///
/// An Anthropic `thinking` block's `budget_tokens` derives from the configured
/// `max_tokens`, so capping output at 1 token changes the thinking budget and
/// therefore the prompt cache key — such a request is not replayable. Every
/// other request is replayable.
#[must_use]
pub fn is_replayable(protocol_is_anthropic_messages: bool, thinking_enabled: bool) -> bool {
    !(protocol_is_anthropic_messages && thinking_enabled)
}

/// Static ineligibility: the reasons a plan can never fire, independent of the
/// clock and the meter gate. `None` means "eligible; arm it".
fn ineligibility(policy: WarmPolicy, facts: WarmFacts) -> Option<SkipReason> {
    if policy.mode == CacheWarmingMode::Off {
        return Some(SkipReason::ModeOff);
    }
    if !(policy.prompt_cache_enabled && facts.prompt_cache_enabled) {
        return Some(SkipReason::CachingDisabled);
    }
    if !facts.replayable {
        return Some(SkipReason::NotReplayable);
    }
    match ttl_secs(facts.cache_ttl, facts.retention) {
        None => Some(SkipReason::NoTtl),
        Some(secs) if secs <= MIN_TTL_SECS => Some(SkipReason::TtlTooShort),
        Some(_) => None,
    }
}

/// Resolve the active TTL tier, in seconds.
///
/// `Long` falls back to the short tier when the model declares no long tier —
/// the long tier is opt-in, and the short tier is the only lifetime that
/// exists otherwise.
fn ttl_secs(ttl: Option<PromptCacheTtl>, retention: RetentionTier) -> Option<u32> {
    let ttl = ttl?;
    match retention {
        RetentionTier::Long => ttl.long_secs.or(ttl.short_secs),
        RetentionTier::Short => ttl.short_secs,
    }
}

/// The warm delay in seconds: `min(ttl * 0.9, ttl - 10)`, so a ping fires at
/// 90% of the TTL but never later than 10 s before it expires — whichever is
/// earlier leaves the most margin against clock skew. `ttl_secs` must exceed
/// [`MIN_TTL_SECS`] (the ineligibility check guarantees it).
fn warm_delay_secs(ttl_secs: u32) -> u32 {
    // `ttl - ttl/10` is the integer 0.9*ttl without an overflow-prone `*9`.
    let ninety_percent = ttl_secs - ttl_secs / 10;
    let ten_before = ttl_secs - MIN_TTL_SECS;
    ninety_percent.min(ten_before)
}

/// The meter-kind gate: the pure core of the "should we warm?" decision.
///
/// `Ok(())` means warm; `Err(reason)` means do not, with the reason for the
/// status line.
fn gate(
    policy: &WarmPolicy,
    facts: &WarmFacts,
    economics: Option<Economics>,
) -> Result<(), SkipReason> {
    match policy.meter {
        MeterKind::Payg => {
            // Dollars are the binding resource, so the ping must save money.
            // Without token prices the math is unknown — treat as ineligible
            // rather than warming on a guess.
            let Some(econ) = economics else {
                return Err(SkipReason::EconomicsUnavailable);
            };
            if econ.expected_savings >= policy.min_expected_savings {
                Ok(())
            } else {
                Err(SkipReason::BelowThreshold)
            }
        }
        MeterKind::Tokens => {
            // The token quota is the binding resource, so a large enough prefix
            // (whose cached re-read saves a corresponding chunk of quota) is
            // what matters.
            if facts.prefix_tokens >= policy.min_prefix_tokens {
                Ok(())
            } else {
                Err(SkipReason::BelowThreshold)
            }
        }
        // A ping adds a request to the window and saves none on a request-
        // metered plan; a flat plan saves nothing at all; and an unknown meter
        // is never guessed. All three decline.
        MeterKind::Requests | MeterKind::Flat | MeterKind::Unknown => {
            Err(SkipReason::MeterIncompatible)
        }
    }
}

/// The dollar economics of warming a `prefix_tokens`-size prefix under `cost`.
///
/// Uses the streaming continuation probability of 1 (a warm ping is only ever
/// sent when the tool call is still blocking, so the prefix is expected to be
/// re-used). Returns `None` when the catalog records no prices.
///
/// - `warm_cost = cache_read * prefix/1e6 + output/1e6` — the ping re-reads the
///   prefix from cache and emits a single output token.
/// - `miss_cost = (cache_write_or_input - cache_read) * prefix/1e6` — a miss
///   re-writes the prefix at the write price; `cache_write` of `0` (or absent)
///   falls back to the full input price, and a missing `cache_read` is `0`.
/// - `expected_savings = miss_cost - warm_cost`.
fn economics(cost: Option<ModelCost>, prefix_tokens: u32) -> Option<Economics> {
    let cost = cost?;
    let per_million = f64::from(prefix_tokens) / 1_000_000.0;
    let cache_read = cost.cache_read.unwrap_or(0.0);
    let warm_cost = cache_read * per_million + cost.output / 1_000_000.0;
    let write_price = match cost.cache_write {
        Some(write) if write > 0.0 => write,
        _ => cost.input,
    };
    let miss_cost = (write_price - cache_read) * per_million;
    Some(Economics {
        warm_cost,
        miss_cost,
        expected_savings: miss_cost - warm_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A floating-point comparison that tolerates the tiny rounding of the
    /// cost arithmetic (clippy's `float_cmp` forbids bare `==` on floats).
    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn ttl_pair(short: u32, long: Option<u32>) -> PromptCacheTtl {
        PromptCacheTtl {
            short_secs: Some(short),
            long_secs: long,
        }
    }

    /// Anthropic-flavoured prices: input 3, output 15, cache read 0.3, cache
    /// write 3.75 (USD per million tokens).
    fn cost() -> ModelCost {
        ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: Some(3.75),
        }
    }

    fn policy(meter: MeterKind) -> WarmPolicy {
        WarmPolicy {
            mode: CacheWarmingMode::Streaming,
            meter,
            prompt_cache_enabled: true,
            min_prefix_tokens: DEFAULT_MIN_PREFIX_TOKENS,
            min_expected_savings: DEFAULT_MIN_EXPECTED_SAVINGS,
        }
    }

    fn facts() -> WarmFacts {
        WarmFacts {
            prompt_cache_enabled: true,
            cache_ttl: Some(ttl_pair(300, None)),
            retention: RetentionTier::Short,
            prefix_tokens: 40_000,
            cost: Some(cost()),
            replayable: true,
        }
    }

    #[test]
    fn meter_kind_deserializes_known_strings() {
        for (raw, want) in [
            ("\"payg\"", MeterKind::Payg),
            ("\"tokens\"", MeterKind::Tokens),
            ("\"requests\"", MeterKind::Requests),
            ("\"flat\"", MeterKind::Flat),
            ("\"unknown\"", MeterKind::Unknown),
        ] {
            let got: MeterKind = toml::from_str(&format!("meter = {raw}"))
                .map(|t: MeterHolder| t.meter)
                .expect("known meter string parses");
            assert_eq!(got, want, "raw {raw}");
        }
    }

    #[derive(Deserialize)]
    struct MeterHolder {
        #[serde(default)]
        meter: MeterKind,
    }

    #[test]
    fn meter_kind_defaults_and_unknown_fall_back() {
        // Absent field -> default.
        let holder: MeterHolder = toml::from_str("").expect("empty parses");
        assert_eq!(holder.meter, MeterKind::Unknown);
        // A typo warns and falls back rather than failing the parse.
        let holder: MeterHolder =
            toml::from_str("meter = \"banana\"").expect("unknown meter must not fail");
        assert_eq!(holder.meter, MeterKind::Unknown);
        assert_eq!(MeterKind::default(), MeterKind::Unknown);
    }

    #[test]
    fn cache_warming_mode_deserializes_and_falls_back() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default)]
            mode: CacheWarmingMode,
        }
        let h: Holder = toml::from_str("mode = \"streaming\"").expect("streaming parses");
        assert_eq!(h.mode, CacheWarmingMode::Streaming);
        let h: Holder = toml::from_str("mode = \"off\"").expect("off parses");
        assert_eq!(h.mode, CacheWarmingMode::Off);
        let h: Holder = toml::from_str("").expect("absent parses");
        assert_eq!(h.mode, CacheWarmingMode::Off);
        // A typo warns and falls back to Off rather than failing.
        let h: Holder = toml::from_str("mode = \"turbo\"").expect("unknown mode must not fail");
        assert_eq!(h.mode, CacheWarmingMode::Off);
    }

    #[test]
    fn config_defaults_and_round_trip() {
        let d = CacheWarmingConfig::default();
        assert_eq!(d.mode, CacheWarmingMode::Off);
        assert_eq!(d.min_prefix_tokens, DEFAULT_MIN_PREFIX_TOKENS);
        assert!(approx(d.min_expected_savings, DEFAULT_MIN_EXPECTED_SAVINGS));

        // Serializes snake_case and deserializes back.
        let toml_str = toml::to_string(&d).expect("serialize");
        assert!(toml_str.contains("mode = \"off\""), "{toml_str}");
        let back: CacheWarmingConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(back, d);
    }

    #[test]
    fn config_parse_partial_uses_defaults() {
        let c: CacheWarmingConfig = toml::from_str("mode = \"streaming\"").expect("partial parses");
        assert_eq!(c.mode, CacheWarmingMode::Streaming);
        assert_eq!(c.min_prefix_tokens, DEFAULT_MIN_PREFIX_TOKENS);
        assert!(approx(c.min_expected_savings, DEFAULT_MIN_EXPECTED_SAVINGS));
    }

    #[test]
    fn resolve_precedence() {
        let config = CacheWarmingConfig {
            mode: CacheWarmingMode::Streaming,
            min_prefix_tokens: 10_000,
            min_expected_savings: 0.2,
        };
        // Account mode overrides the global default.
        let p = WarmPolicy::resolve(
            &config,
            Some(MeterKind::Payg),
            Some(CacheWarmingMode::Off),
            true,
        );
        assert_eq!(p.mode, CacheWarmingMode::Off);
        assert_eq!(p.meter, MeterKind::Payg);
        assert_eq!(p.min_prefix_tokens, 10_000);
        assert!(approx(p.min_expected_savings, 0.2));

        // Unset account knobs fall through to the defaults: global mode, and
        // the conservative Unknown meter.
        let p = WarmPolicy::resolve(&config, None, None, true);
        assert_eq!(p.mode, CacheWarmingMode::Streaming);
        assert_eq!(p.meter, MeterKind::Unknown);

        // Prompt caching off is carried through to the policy.
        let p = WarmPolicy::resolve(&config, Some(MeterKind::Tokens), None, false);
        assert!(!p.prompt_cache_enabled);
    }

    #[test]
    fn gate_payg_boundary() {
        // savings for the 40k prefix: miss 3.45*0.04 = 0.138, warm
        // 0.3*0.04 + 15e-6 = 0.012015 -> 0.125985.
        let econ = economics(Some(cost()), 40_000).expect("cost present");
        assert!(approx(econ.expected_savings, 0.125_985));
        let f = facts();

        // Just below the threshold: no.
        let mut p = policy(MeterKind::Payg);
        p.min_expected_savings = 0.13;
        assert_eq!(gate(&p, &f, Some(econ)), Err(SkipReason::BelowThreshold));

        // Exactly at the threshold: yes (>=).
        let mut p = policy(MeterKind::Payg);
        p.min_expected_savings = econ.expected_savings;
        assert_eq!(gate(&p, &f, Some(econ)), Ok(()));

        // Above: yes.
        let mut p = policy(MeterKind::Payg);
        p.min_expected_savings = 0.10;
        assert_eq!(gate(&p, &f, Some(econ)), Ok(()));

        // No prices -> economics unavailable, never a guess.
        assert_eq!(
            gate(&policy(MeterKind::Payg), &f, None),
            Err(SkipReason::EconomicsUnavailable)
        );
    }

    #[test]
    fn gate_tokens_boundary() {
        let p = policy(MeterKind::Tokens);
        let mut f = facts();
        f.prefix_tokens = DEFAULT_MIN_PREFIX_TOKENS - 1;
        assert_eq!(gate(&p, &f, None), Err(SkipReason::BelowThreshold));
        f.prefix_tokens = DEFAULT_MIN_PREFIX_TOKENS;
        assert_eq!(gate(&p, &f, None), Ok(()));
        f.prefix_tokens = DEFAULT_MIN_PREFIX_TOKENS + 1;
        assert_eq!(gate(&p, &f, None), Ok(()));
    }

    #[test]
    fn gate_requests_flat_unknown_never_warm() {
        // A huge prefix and a clear dollar win must not matter: these meters
        // never warm, regardless of prefix size or cost.
        let econ = economics(Some(cost()), 1_000_000);
        let mut f = facts();
        f.prefix_tokens = 1_000_000;
        for meter in [MeterKind::Requests, MeterKind::Flat, MeterKind::Unknown] {
            let p = policy(meter);
            assert_eq!(
                gate(&p, &f, econ),
                Err(SkipReason::MeterIncompatible),
                "{meter:?} must never warm"
            );
            // Even without economics the answer is the same.
            assert_eq!(gate(&p, &f, None), Err(SkipReason::MeterIncompatible));
        }
    }

    #[test]
    fn arm_ineligibility_reasons() {
        let now = Duration::from_secs(1000);

        // Mode off.
        let p = WarmPolicy {
            mode: CacheWarmingMode::Off,
            ..policy(MeterKind::Payg)
        };
        let plan = p.arm(facts(), now);
        assert_eq!(plan.poll(now), Action::Stop(SkipReason::ModeOff));

        // Prompt caching disabled.
        let p = policy(MeterKind::Payg);
        let plan = p.arm(
            WarmFacts {
                prompt_cache_enabled: false,
                ..facts()
            },
            now,
        );
        assert_eq!(plan.poll(now), Action::Stop(SkipReason::CachingDisabled));
        // The policy flag alone also disables.
        let p = WarmPolicy {
            prompt_cache_enabled: false,
            ..policy(MeterKind::Payg)
        };
        assert_eq!(
            p.arm(facts(), now).poll(now),
            Action::Stop(SkipReason::CachingDisabled)
        );

        // No TTL.
        let p = policy(MeterKind::Payg);
        let plan = p.arm(
            WarmFacts {
                cache_ttl: None,
                ..facts()
            },
            now,
        );
        assert_eq!(plan.poll(now), Action::Stop(SkipReason::NoTtl));
        // A TTL with no tier for the requested retention is also NoTtl.
        let plan = p.arm(
            WarmFacts {
                cache_ttl: Some(PromptCacheTtl::default()),
                ..facts()
            },
            now,
        );
        assert_eq!(plan.poll(now), Action::Stop(SkipReason::NoTtl));

        // TTL too short (<= 10 s).
        let plan = p.arm(
            WarmFacts {
                cache_ttl: Some(ttl_pair(10, None)),
                ..facts()
            },
            now,
        );
        assert_eq!(plan.poll(now), Action::Stop(SkipReason::TtlTooShort));

        // Not replayable.
        let plan = p.arm(
            WarmFacts {
                replayable: false,
                ..facts()
            },
            now,
        );
        assert_eq!(plan.poll(now), Action::Stop(SkipReason::NotReplayable));
    }

    #[test]
    fn deadline_guard_and_wait() {
        let start = Duration::from_secs(1000);
        let p = policy(MeterKind::Payg);
        let plan = p.arm(facts(), start);

        // ttl 300 -> delay 270, next_warm_at = start+270, remaining 30 -> /2 =
        // 15, refresh_deadline = start+285.
        assert_eq!(
            plan.poll(start + Duration::from_secs(269)),
            Action::Wait(Duration::from_secs(1))
        );

        // At next_warm_at the gate runs and (a healthy payg win) pings.
        assert_eq!(plan.poll(start + Duration::from_secs(270)), Action::Ping);

        // Between next_warm_at and the deadline it still pings (already due).
        assert_eq!(plan.poll(start + Duration::from_secs(285)), Action::Ping);

        // Past the deadline: hard stop, the money-losing case.
        assert_eq!(
            plan.poll(start + Duration::from_secs(286)),
            Action::Stop(SkipReason::DeadlineMissed)
        );
    }

    #[test]
    fn reschedule_arithmetic() {
        let start = Duration::from_secs(0);
        let p = policy(MeterKind::Payg);
        let plan = p.arm(
            WarmFacts {
                cache_ttl: Some(ttl_pair(300, None)),
                ..facts()
            },
            start,
        );
        let WarmPlan {
            state:
                PlanState::Armed {
                    next_warm_at,
                    refresh_deadline,
                },
            ..
        } = plan
        else {
            panic!("expected an armed plan");
        };
        assert_eq!(next_warm_at, Duration::from_secs(270));
        assert_eq!(refresh_deadline, Duration::from_secs(285));

        // A short TTL where `ttl - 10` wins the min: ttl 20 -> delay 10,
        // remaining 10 -> /2 = 5 -> deadline at 15.
        let plan = p.arm(
            WarmFacts {
                cache_ttl: Some(ttl_pair(20, None)),
                ..facts()
            },
            start,
        );
        let WarmPlan {
            state:
                PlanState::Armed {
                    next_warm_at,
                    refresh_deadline,
                },
            ..
        } = plan
        else {
            panic!("expected an armed plan");
        };
        assert_eq!(next_warm_at, Duration::from_secs(10));
        assert_eq!(refresh_deadline, Duration::from_secs(15));
    }

    #[test]
    fn retention_tier_resolution() {
        let p = policy(MeterKind::Payg);
        // Long tier present and selected.
        let plan = p.arm(
            WarmFacts {
                cache_ttl: Some(ttl_pair(300, Some(3600))),
                retention: RetentionTier::Long,
                ..facts()
            },
            Duration::from_secs(0),
        );
        let WarmPlan {
            state:
                PlanState::Armed {
                    next_warm_at,
                    refresh_deadline,
                },
            ..
        } = plan
        else {
            panic!("expected an armed plan");
        };
        // 3600 -> delay 3240, remaining 360 -> /2 = 180 -> deadline 3420.
        assert_eq!(next_warm_at.as_secs(), 3240);
        assert_eq!(refresh_deadline.as_secs(), 3420);

        // Long selected but absent -> falls back to the short tier (300).
        let plan = p.arm(
            WarmFacts {
                cache_ttl: Some(ttl_pair(300, None)),
                retention: RetentionTier::Long,
                ..facts()
            },
            Duration::from_secs(0),
        );
        let WarmPlan {
            state:
                PlanState::Armed {
                    next_warm_at,
                    refresh_deadline,
                },
            ..
        } = plan
        else {
            panic!("expected an armed plan");
        };
        assert_eq!(next_warm_at, Duration::from_secs(270));
        assert_eq!(refresh_deadline, Duration::from_secs(285));
    }

    #[test]
    fn is_replayable_branches() {
        // Anthropic + thinking is the only non-replayable combination.
        assert!(!is_replayable(true, true));
        assert!(is_replayable(true, false));
        assert!(is_replayable(false, true));
        assert!(is_replayable(false, false));
    }

    #[test]
    fn economics_math_and_fallbacks() {
        // Standard case.
        let e = economics(Some(cost()), 1_000_000).expect("cost present");
        assert!(approx(e.warm_cost, 0.3 + 0.000_015));
        assert!(approx(e.miss_cost, 3.45));
        assert!(approx(e.expected_savings, 3.45 - 0.300_015));

        // cache_write == 0 falls back to input; cache_read absent is 0.
        let c = ModelCost {
            input: 2.0,
            output: 8.0,
            cache_read: None,
            cache_write: Some(0.0),
        };
        let e = economics(Some(c), 1_000_000).expect("cost present");
        assert!(approx(e.warm_cost, 8.0 / 1_000_000.0));
        assert!(approx(e.miss_cost, 2.0));

        // cache_write absent -> input.
        let c = ModelCost {
            input: 5.0,
            output: 1.0,
            cache_read: Some(0.5),
            cache_write: None,
        };
        let e = economics(Some(c), 1_000_000).expect("cost present");
        assert!(approx(e.miss_cost, 5.0 - 0.5));

        // No prices at all -> None.
        assert!(economics(None, 1_000_000).is_none());
    }

    #[test]
    fn decide_carries_status_fields() {
        let start = Duration::from_secs(0);
        let p = policy(MeterKind::Payg);
        let plan = p.arm(facts(), start);
        let d = plan.decide(start + Duration::from_secs(270));
        assert_eq!(d.prefix_tokens, 40_000);
        assert_eq!(d.meter, MeterKind::Payg);
        assert_eq!(d.action, Action::Ping);
        assert!(approx(d.expected_savings.expect("priced"), 0.125_985));
    }

    #[test]
    fn skip_reason_display_is_nonempty() {
        for reason in [
            SkipReason::ModeOff,
            SkipReason::CachingDisabled,
            SkipReason::NoTtl,
            SkipReason::TtlTooShort,
            SkipReason::NotReplayable,
            SkipReason::MeterIncompatible,
            SkipReason::BelowThreshold,
            SkipReason::EconomicsUnavailable,
            SkipReason::DeadlineMissed,
            SkipReason::ContextChanged,
        ] {
            assert!(!reason.to_string().is_empty(), "{reason:?}");
        }
    }
}
