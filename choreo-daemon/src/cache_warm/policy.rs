//! The pure cache-warming policy: the config surface, the billing **meter**
//! model, and the warm-decision state machine. Nothing here touches the clock
//! (every decision takes an injected `now`) or performs I/O, so it is fully
//! deterministic and testable without timers. The runtime driver that consumes
//! it lives in [`super::driver`].

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
    /// Warm while a long tool call blocks (the only shipping mode).
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
/// Resolved per account (at spawn and refreshed whenever the session re-resolves
/// its account) and reused for every warm decision; the resolution precedence
/// lives in [`WarmPolicy::resolve`].
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
    /// `now + delay` — see `warm_delay_secs` for the delay rule — and the
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

impl Default for WarmPolicy {
    /// The conservative default: warming **off**, meter unknown (never warm).
    ///
    /// Seeds every [`SessionState`](crate::sessions::SessionState) that has not
    /// yet resolved its account, and the many request/context fixtures that do
    /// not exercise warming — a defaulted policy never spawns a warmer (the
    /// agent loop skips the spawn when `mode == Off`), so the value is inert
    /// there. `prompt_cache_enabled` defaults to `true` because that is the
    /// account default (`prompt_cache = None`); with `mode == Off` it is moot in
    /// any case.
    fn default() -> Self {
        Self::resolve(&CacheWarmingConfig::default(), None, None, true)
    }
}

/// The per-request facts a warm decision is evaluated against. All fields are
/// static for the lifetime of one request — the clock is the only variable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarmFacts {
    /// The effective prompt-cache TTL, or `None` when the model exposes none.
    pub cache_ttl: Option<PromptCacheTtl>,
    /// The cache tier the request uses.
    pub retention: RetentionTier,
    /// The size of the cacheable prefix, in tokens.
    pub prefix_tokens: u32,
    /// The model's token prices, or `None` when the catalog records none.
    pub cost: Option<ModelCost>,
    /// Whether a capped re-send of the last request would be cache-equivalent
    /// (see [`is_replayable`]).
    pub replayable: bool,
}

/// A resolved warm plan: an inactive plan carrying its [`SkipReason`], or an
/// armed plan carrying the two clock deadlines it fires between.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarmPlan {
    pub(super) policy: WarmPolicy,
    pub(super) facts: WarmFacts,
    pub(super) state: PlanState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum PlanState {
    /// The request cannot be warmed; the reason is carried for the status line.
    Inactive(SkipReason),
    /// A ping is scheduled at `next_warm_at` and must fire no later than
    /// `refresh_deadline`.
    Armed {
        next_warm_at: Duration,
        refresh_deadline: Duration,
    },
}

/// The next thing the warmer should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Sleep until `now + Duration` and poll again.
    Wait(Duration),
    /// Send the warm ping now.
    Ping,
    /// Stop; the request will not be warmed (or is no longer worth warming).
    Stop(SkipReason),
}

/// Why a warm ping was (or will be) skipped.
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

impl SkipReason {
    /// A stable cardinality-bounded label for the `cache_warm` skip metric.
    ///
    /// Distinct from [`Display`](fmt::Display): the metric label is
    /// `snake_case` and never carries prose, so the Prometheus series stays a
    /// fixed enum rather than growing one label value per phrasing.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ModeOff => "mode_off",
            Self::CachingDisabled => "caching_disabled",
            Self::NoTtl => "no_ttl",
            Self::TtlTooShort => "ttl_too_short",
            Self::NotReplayable => "not_replayable",
            Self::MeterIncompatible => "meter_incompatible",
            Self::BelowThreshold => "below_threshold",
            Self::EconomicsUnavailable => "economics_unavailable",
            Self::DeadlineMissed => "deadline_missed",
            Self::ContextChanged => "context_changed",
        }
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

/// Whether a capped re-send of an Anthropic-protocol request with thinking
/// enabled would be cache-equivalent to the original.
///
/// An Anthropic `thinking` block's `budget_tokens` derives from the configured
/// `max_tokens`, so capping output changes the thinking budget and therefore
/// the prompt cache key — such a request is not replayable. Every other request
/// is replayable.
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
    if !policy.prompt_cache_enabled {
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
pub(super) fn gate(
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
/// - `expected_savings = miss_cost - warm_cost`. If a cache read is priced
///   above the write price (a mis-keyed catalog entry), the saving is negative
///   and the gate declines — never a reason to warm.
pub(super) fn economics(cost: Option<ModelCost>, prefix_tokens: u32) -> Option<Economics> {
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
