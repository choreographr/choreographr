use super::driver::ping_request;
use super::policy::{PlanState, economics, gate};
use super::*;
use crate::providers::test_util::make_recording_provider;
use choreo_ai_protocols::catalog::{ModelCost, PromptCacheTtl};
use choreo_ai_protocols::openai::ChatRequestMessage;
use serde::Deserialize;
use std::time::Duration;

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

    // Prompt caching disabled on the policy.
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
        state: PlanState::Armed {
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
        state: PlanState::Armed {
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
        state: PlanState::Armed {
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
        state: PlanState::Armed {
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

#[test]
fn skip_reason_labels_are_stable_snake_case() {
    // The metric label must not track the prose Display form.
    assert_eq!(SkipReason::ModeOff.label(), "mode_off");
    assert_eq!(SkipReason::DeadlineMissed.label(), "deadline_missed");
    assert_eq!(SkipReason::MeterIncompatible.label(), "meter_incompatible");
}

// ── Warmer-driver tests ────────────────────────────────────────────

/// A `WarmRequest` built from the shared `facts()` fixture.
fn warm_request() -> WarmRequest {
    WarmRequest {
        model: "tiny-model".into(),
        messages: vec![ChatRequestMessage::simple("user", "hello".into())],
        tools: Vec::new(),
        facts: facts(),
        session_id: "42".into(),
        request_id: "7".into(),
        max_output_tokens: 1,
    }
}

#[test]
fn ping_request_shape_caps_output_no_retry_thinking_off() {
    let request = warm_request();
    let mut on_retry = None;
    let ping = ping_request(&request, &mut on_retry);
    // The exact honoured shape: cap output, never retry, never think, never
    // share the loop's cancel receiver.
    assert_eq!(ping.max_output_tokens_override, Some(1));
    assert!(ping.no_retry);
    assert_eq!(ping.thinking_effort, "off");
    assert!(ping.cancel_rx.is_none());
    assert!(ping.previous_response_id.is_none());
    // The prefix and routing identity are carried verbatim.
    assert_eq!(ping.model, "tiny-model");
    assert_eq!(ping.messages.len(), 1);
    assert_eq!(ping.session_id, "42");
    assert_eq!(ping.request_id, "7");
}

#[test]
fn ping_request_uses_the_per_protocol_cap() {
    // Anthropic's pre-warm cap of 0 flows through verbatim (no output billed);
    // every other protocol's arm site supplies at least 1.
    let request = WarmRequest {
        max_output_tokens: 0,
        ..warm_request()
    };
    let mut on_retry = None;
    assert_eq!(
        ping_request(&request, &mut on_retry).max_output_tokens_override,
        Some(0)
    );
}

#[test]
fn driver_stop_ends_the_thread() {
    // A `Stop` (also sent by `Drop`) must end the driver; `stop()` joins,
    // so a driver that failed to observe it would hang this test.
    let (provider, recorder) = make_recording_provider();
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let mut handle = spawn_warmer(provider, policy(MeterKind::Tokens), cancel_rx);
    handle.stop();
    assert_eq!(recorder.calls(), 0, "stopping must not ping");
}

#[test]
fn driver_ignores_ineligible_arm() {
    // Mode off is statically ineligible: arming yields an inactive plan,
    // so the driver declines without ever contacting the provider. Arm and
    // Stop are ordered on the same control channel, so by the time
    // `stop()` joins the arm has been fully processed and declined.
    let (provider, recorder) = make_recording_provider();
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let off = WarmPolicy {
        mode: CacheWarmingMode::Off,
        ..policy(MeterKind::Tokens)
    };
    let mut handle = spawn_warmer(provider, off, cancel_rx);
    handle.arm(warm_request());
    handle.stop();
    assert_eq!(recorder.calls(), 0, "an ineligible arm must never ping");
}

#[test]
fn driver_observes_relayed_cancel() {
    // A cancel relayed on the dedicated receiver must end the driver
    // (and `stop()` joins, so a miss would hang the test).
    let (provider, recorder) = make_recording_provider();
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let mut handle = spawn_warmer(provider, policy(MeterKind::Tokens), cancel_rx);
    cancel_tx.send(()).unwrap();
    handle.stop();
    assert_eq!(
        recorder.calls(),
        0,
        "a cancel before any ping must not ping"
    );
}
