# Plan: per-session prompt-cache affinity and extended cache retention

**Status:** implemented. Phases 1, 2a, 2b, 3 and 4 are on `master`; Phase 3
(extended retention) ships **dormant** — the mechanism is complete and no model is
seeded.
**Lifecycle:** this is a **design record** for shipped work, not a pending plan. Plan
files are deleted once implemented; this one stays because it records the reasoning
the shipped artefacts deliberately do not carry — the commit messages are the
changelog, and the in-source rustdoc plus `ARCHITECTURE.md` carry the *current
contract*, not the alternatives that were rejected to reach it. Nothing in the
source tree references this file.
**Date:** 2026-10-26
**Targets:** `choreo-ai-protocols` (request-body field resolution, the rejection
registry, the `cache_affinity` / `cache_retention` overlay scalars); `choreo-daemon`
(the cache-warming gate, the `/metrics` cache-hit histogram); `ARCHITECTURE.md`,
`README.md`, `AGENTS.md`.
**Touches (as shipped):** `choreo-ai-protocols/src/{session_routing.rs,
cache_retention.rs, openai/{config.rs,chat_completions.rs,chat_completions/wire.rs,
responses/{body.rs,wire.rs},retry.rs,wire.rs}, google/{mod.rs,requests.rs},
catalog/{entry.rs,lookup.rs,overlay.rs,modelsdev.rs}}`,
`choreo-ai-protocols/catalog/models-overlay.toml`,
`choreo-daemon/src/{metrics.rs,requests.rs,cache_warm/{mod.rs,policy.rs}}`,
`choreo-proto/src/types/common.rs`.

> **TL;DR.** Every turn of an agent loop re-sends a large, growing prefix, so one
> prompt-cache hit is worth the difference between paying the input rate and paying
> the cached-read rate — on the providers we route to, roughly a 90–97.5% discount
> per cached token. But cache state is **per server**, not per account: two requests
> for the same session can hit the same prefix on one backend and miss on another.
> Prefix stability is necessary and not sufficient; **routing** decides. The shipped
> stack therefore (1) reports cache hits on every adapter, (2) sends each session's
> **real** id in the affinity dialect its endpoint documents, (3) remembers a
> provider that rejects that field and stops sending it, (4) declines cache warming
> once the hint is withheld, and (5) selects extended retention per model — which is
> shipped but **dormant**, because no model in the curated catalog declares a long
> tier yet.

---

## Table of contents

1. [Context](#1-context)
2. [The authority matrix](#2-the-authority-matrix)
3. [What shipped, per phase](#3-what-shipped-per-phase)
4. [Decisions and the alternatives that were rejected](#4-decisions-and-the-alternatives-that-were-rejected)
5. [Open follow-ons](#5-open-follow-ons)
6. [Where the current contract lives](#6-where-the-current-contract-lives)

---

## 1. Context

### The prefix is the bill

An agent loop is a sequence of requests that share a prefix and append to it: system
prompt, tool definitions, project context, then the transcript so far. By turn 20 the
prefix is most of the request, and it is being re-sent verbatim each turn. Providers
price that prefix twice: the **input rate** when it is cold, and a much lower
**cached-read rate** when the server recognises it.

The discounts are not marginal. The figures the implementation was designed against:

| Provider | Cached-read price, relative to input |
|---|---|
| DeepSeek (via Venice) | ~**97.5%** below the input rate |
| Kimi (via Venice) | ~**90%** below the input rate |
| OpenAI | **≤0.1×** the input rate (a cache hit costs at most a tenth) |

Even the weakest of those turns the dominant cost line of a long session into a
rounding error — but only when the prefix actually hits. A miss is not a small
surcharge: it is a full-price re-read of everything.

### Cache state is per server, so routing is the lever

A prompt cache lives in a specific backend's memory or disk, keyed by the prefix
content. A provider behind a load balancer has many such backends. Two requests that
carry a byte-identical prefix can therefore land on different machines, and only the
one that has seen the prefix answers with a hit. Three consequences shaped this work:

- **Prefix stability is table stakes.** Anything that perturbs an earlier byte of the
  request (a reordered tool list, a timestamp in the system prompt, a rewritten
  history) invalidates the cache no matter where the request lands. That discipline
  predates this work and is not what these phases address.
- **Affinity is the remaining lever.** Where an endpoint accepts a session identifier
  on the request, sending the conversation's real id biases the balancer to keep
  routing that conversation to one backend, which is exactly the state we want warm.
- **Some providers need nothing from us.** DeepSeek caches automatically on disk
  prefix, Gemini caches implicitly, Anthropic takes explicit `cache_control`
  breakpoints, and the opencode gateways already route per session through their own
  headers. Sending an extra field there is at best noise and at worst a rejected
  request.

The daemon also acts *on* the cache rather than only benefiting from it: cache
warming re-sends the last request mid-tool-call to refresh the prefix's TTL. That
strategy only makes sense if the ping lands where the prefix is, which is the link
Phase 4 draws between the hint and the warmer.

## 2. The authority matrix

Per provider, verified against each endpoint's own documentation (not inferred from
shared OpenAI-compatible behaviour — the compat surface diverges exactly here).

| Provider (catalog slug) | Request-side affinity mechanism | Response-side cache reporting |
|---|---|---|
| OpenAI ≤5.5 (`openai`) | top-level `prompt_cache_key`; extended retention via top-level `prompt_cache_retention = "24h"` | `usage.prompt_tokens_details.cached_tokens` |
| OpenAI ≥5.6 / `gpt-6*` (`openai`) | `prompt_cache_key` still accepted, but it now **partitions cache accounting** while routing is decided by a hash of the prompt tokens; extended retention via nested `prompt_cache_options.ttl = "12h"`, **Responses API only** | `usage.input_tokens_details.cached_tokens` |
| OpenRouter (`openrouter`) | documented preference: the `session_id` body field, else the `x-session-id` header, else `prompt_cache_key`; a `session_id` must match `[A-Za-z0-9_-]` and be ≤256 bytes | OpenAI-compatible `prompt_tokens_details.cached_tokens` |
| Venice (`venice`) | top-level `prompt_cache_key`, documented as a conversation/session id | no cache-hit counter in the reference we worked from |
| DeepSeek (`deepseek`) | **none** — automatic disk-based prefix cache, no request key | flat `usage.prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` |
| Anthropic (`anthropic`) | `cache_control` breakpoints placed in the request content | `usage.cache_read_input_tokens` / `cache_creation_input_tokens` |
| Google Gemini (`google`) | **none** — implicit caching | `usageMetadata.cachedContentTokenCount` |
| opencode zen/go (`opencode`, `opencode-go`, `opencode-go-anthropic-compatible`) | gateway routes per session itself via its `x-opencode-session` headers | the upstream OpenAI-compatible usage, passed through |

The asymmetry is the point of the table: the request side has **three** distinct
shapes (a body key, an explicit content breakpoint, nothing at all) plus two
exceptions that look like "nothing at all" but are actually "already handled
elsewhere", and the reply side has **four** spellings for the same number.

## 3. What shipped, per phase

Shipped in this order; Phase 3 landed last even though it is numbered before Phase 4.

### Phase 1 — report the hits (`e294bed`)

The cache counts were being parsed for some providers and dropped for others, which
made the whole feature unmeasurable. Every adapter now projects its provider's
cache-hit field into `TokenUsage.cached_tokens`: DeepSeek's flat
`prompt_cache_hit_tokens`, the Responses API's `input_tokens_details.cached_tokens`,
and Gemini's `usageMetadata.cachedContentTokenCount`. Each protocol has **one**
projection shared by its streaming and non-streaming path, so a count can no longer
be projected on one path and missed on the other. Cache hits stay a **subset** of
the prompt everywhere (`input_tokens` is never inflated), and the daemon publishes a
`choreo_prompt_cache_hit_ratio` histogram on `/metrics` — each turn's
cached-to-prompt ratio, keyed by provider slug and model, so a warm cache is visible
while it is working. Token counts only; never prompt or response content.

### Phase 2a — send the key (`d801111`)

Each conversation's real session id now rides the dialect its provider documents,
selected by the `cache_affinity` scalar on the provider's overlay entry (`off`,
`prompt-cache-key`, `openrouter-session`); the overlay pins `openai` and `venice` to
`prompt-cache-key` and `openrouter` to `openrouter-session`, and leaves every other
provider `off`. `openrouter-session` validates the id against OpenRouter's charset
and length limit and omits it when invalid rather than sending a value the provider
would drop. The same commit collapsed the chat-completions body construction into a
**single builder**, so a request field can no longer be added to one path and missed
on another — the same failure mode Phase 1 fixed on the parse side.

### Phase 2b — fail safe on a rejection (`8166c0e`)

An optional field that a provider rejects with an HTTP **400 naming that field**
cannot be retried into working; it has to be withdrawn. The process remembers the
provider in `cache_field_rejection::CACHE_FIELD_REJECTED` and omits the field from
every later request. The detector is deliberately narrow (only a `400`, only when
the body names one of the prompt-cache fields the request actually carried, only
once the attempt is final after retries) so nothing unrelated downgrades a working
provider. Operators can force the capability on or off per account with
`prompt_cache_affinity`, without editing the catalog.

### Phase 4 — stop warming a cache you cannot reach (`267037b`)

A warm ping's cost model assumes the ping lands on a backend that still holds the
prefix (`warm_cost = cache_read × prefix + output`, versus `miss_cost` = a full-price
write). Losing the affinity hint invalidates exactly that premise, so the warmer
declines **before any timer is armed** with `SkipReason::AffinityUnavailable`, and
reports it as `choreo_cache_warm_skips_total{reason="affinity_unavailable"}`. This is
a static eligibility check, not a meter rule: providers the catalog leaves `off`
never set the fact and warm exactly as before.

### Phase 3 — select extended retention per model (`bfd0e01`, dormant)

A prompt cache is ephemeral by default, so a warm ping only helps inside the short
tier (typically minutes). Some providers expose a longer tier the request can ask
for, and the field that asks for it differs across one provider's model
generations — so the spelling is a **per-model** overlay scalar (`cache_retention`)
rather than a provider setting, and it is emitted only when the model's resolved TTL
data also declares a long tier. **No model is seeded**, so this ships in place and
dormant (see the decision below).

## 4. Decisions and the alternatives that were rejected

### The dialect is a provider capability; the value is the session id

`CacheAffinity` is resolved from the catalog entry for the **endpoint** and baked
into the provider client at construction; the body builders only fill in the turn's
session id. The rejected alternative was a `dialect` field on the request struct:
it would put a per-endpoint fact into every call site, so each of the (now several)
body builders would have to be told the same thing again, and a provider added
later would compile and silently send nothing. Making it a construction-time
capability also buys the invalidation for free — the catalog-swap comparison already
drops a client whose entry changed, so an overlay edit reaches a live session's next
request.

### Retention is a per-model dialect, not a per-provider one

`cache_retention` deliberately has **no provider-level fallback**, unlike
`cache_affinity`. The reason is concrete: `openai` serves both `gpt-5.4`-class models
(top-level `prompt_cache_retention`) and `gpt-6`-class models (nested
`prompt_cache_options.ttl`, Responses only), so a single provider value would be
wrong for half its models. A per-provider dialect was the obvious first
implementation and is unshippable for the one provider that actually has a long
tier.

### Retention is declared explicitly in the overlay, never inferred

The tempting shortcut is to read the model-id string — `gpt-6*` gets the nested
field, everything else the flat one — and skip the overlay entirely. That is rejected
because it fails *silently and in the expensive direction*: the day a provider adds a
generation, an inference rule either mis-classifies it (sending a field that is
rejected or ignored, so no extension and possibly a 400) or classifies it correctly
by luck. An un-pinned model simply gets no selector, which is a known-good
behaviour with no guessing. The same reasoning applies to the long-tier gate: the
selector reuses the existing curated `PromptCacheTtl { short_secs, long_secs }` data
instead of adding a second source of truth that could disagree with it.

### The key is never a constant

A constant key is the cheapest possible implementation and it is wrong on both
generations. On older models it pins **every** session to one routing bucket — the
opposite of affinity, since unrelated conversations then contend for the same
backend's cache. On GPT-5.6+ it is actively worse than useless: the key partitions
cache **accounting** while routing is decided by a prompt-token hash, so a constant
would only merge unrelated sessions into one accounting bucket without changing
routing at all.

### One rejection registry for the whole prompt-cache request surface

The affinity key and the retention selectors are one family of optional additions to
the same request body, so they share one process-wide set (exception 16 in
`AGENTS.md`), following the precedent of the compression-downgrade registry rather
than minting a second one whose lifecycle would immediately duplicate it. The
consequence is deliberate and worth stating plainly: a `400` naming **any one**
prompt-cache field withholds **the whole family** for that provider. That is the
right trade because a provider that rejects one optional prompt-cache parameter is
not a provider to keep probing with the others — each probe costs a wasted turn, and
the fields are mutually exclusive on a body, so at most one could ever have been in
flight anyway.

### Retention ships dormant

Nothing is seeded with `cache_retention` because no OpenAI model in the curated
catalog declares a long tier, and declaring one to make the feature "do something"
would be a guess about a provider's retention policy with a real bill attached
(a `payg` account would pay for writes nothing reads). Shipping the mechanism and
the schema while it is dormant keeps the cost of waiting at zero: when a long tier is
verified for a model, enabling it is two overlay lines.

## 5. Open follow-ons

Not defects — deliberate boundaries of the shipped work.

- **Explicit `prompt_cache_breakpoint` placement on GPT-5.6+.** Newer OpenAI models
  accept explicit breakpoints marking where a cacheable span ends. That is a
  different mechanism from the session key (it is about *what* is cacheable, not
  *which backend* sees it), and placing breakpoints well depends on the transcript
  layout the loop produces, so it needs its own design pass.
- **Anthropic's 1-hour extended TTL.** Real and documented, reached through the
  `anthropic-beta` header rather than a body field, so it is neither of the two
  retention dialects and stays out of `cache_retention` entirely.
- **Venice manual `cache_control` for pre-caching a large document.** Useful when a
  session opens with one big attachment many turns will re-read; it wants an explicit
  decision about *where* to place the breakpoint, which is a session/attachment
  concern rather than a request-default.
- **Seeding `cache_retention`.** Gated on verifying a model's long tier and its
  price, then adding `cache_retention` and a justified `prompt_cache_long` together in
  one overlay edit.

## 6. Where the current contract lives

This file is a record of *why*. The behaviour a change must preserve is documented
where it changes:

- **`ARCHITECTURE.md`** — *Per-session cache affinity* (the capability, the three-layer
  precedence of override → catalog → rejection registry, the downgrade detector, and
  the warming gate) and *Extended cache retention* (the two dialects, the long-tier
  gate, the dormancy).
- **In-source rustdoc** — `session_routing` (the dialect map, the registry's
  read/write sides), `cache_retention` (the selector resolution),
  `ServiceConfig::effective_cache_affinity` (the resolved value the body builders use),
  and `catalog::cache_affinity` / `catalog::cache_retention` (the raw overlay values).
- **`models-overlay.toml`** — the schema comment block, which is canonical for the
  scalar names and the dialect tokens.
- **`AGENTS.md` exception 16** — the shared-state justification for the registry.
