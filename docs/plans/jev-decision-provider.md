# Plan: Jev (TypeSafe AI) as a decision/guardrail capability

**Status:** proposed — research + design memo. Nothing implemented; no code changes
implied yet. This plan records the option space and a suggested phasing so a future
implementation run has a concrete starting point.
**Date:** 2026-09-20
**Target:** an optional, feature-gated **non-chat "decision" capability** built around
TypeSafe AI's **Jev** model (a "System One Model"), usable (a) as a model-invoked tool
and (b) as a daemon-side pre-dispatch policy hook (tool guardrails, model routing,
privacy gating). Off by default; fully pluggable; never a hard dependency of the core
loop.
**Touches (if pursued):** likely `choreo-ai-protocols` (a decision client, *not* a new
`ProviderProtocol`), possibly a new leaf crate (e.g. `choreo-decision`), `choreo-daemon`
(a `DecisionProviderHandle` mirroring `image_provider.rs`; a hook in
`requests/tool_execution.rs`; config plumbing), root `Cargo.toml` (feature), and docs
(`ARCHITECTURE.md`, `README.md`).

> **TL;DR.** Jev is a **first public "System One Model"** from TypeSafe AI: it does
> **not** generate text — it takes an unstructured `state` plus typed `questions` and
> returns **typed, calibrated probabilistic answers** (`choice` / `score` / `noul`).
> It has **no messages, no tools, no token stream**, so it must **not** be forced into
> Choreographr's chat `ProviderClient` abstraction (which is `messages` → streamed
> `Answer`/`Reasoning` text, catalogued by wire protocol). Its natural analogue in this
> codebase is the **image-generation capability** (`daemon/image_provider.rs` →
> `ImageProviderHandle`), i.e. a second non-chat model capability reached through daemon
> state. Being plain HTTP/JSON and synchronous, a Jev client needs **no sidecar runtime**
> (`ureq` is already a workspace dep), so it fits the thread-only daemon exactly like the
> content/IPFS/indexer paths. The highest-leverage uses are (A) **pre-dispatch tool
> guardrails** ("Auto Mode"), (B) **model routing / delegation / skill selection**, and
> (C) **semantic privacy / injection gates**; the lowest-risk first step is (D) a plain
> **model-invoked `classify` tool**.

---

## Table of contents

1. [What Jev is (research summary)](#1-what-jev-is-research-summary)
2. [Why it does *not* fit the existing provider layer](#2-why-it-does-not-fit-the-existing-provider-layer)
3. [Integration surfaces](#3-integration-surfaces)
   - [A. Pre-dispatch tool guardrails ("Auto Mode")](#a-pre-dispatch-tool-guardrails-auto-mode)
   - [B. Model routing & orchestration decisions](#b-model-routing--orchestration-decisions)
   - [C. Semantic privacy / injection gates](#c-semantic-privacy--injection-gates)
   - [D. Jev as a model-invoked tool](#d-jev-as-a-model-invoked-tool)
4. [Suggested architecture](#4-suggested-architecture)
5. [Change inventory (if pursued)](#5-change-inventory-if-pursued)
6. [Testing strategy](#6-testing-strategy)
7. [Phased execution plan](#7-phased-execution-plan)
8. [Decisions (to make)](#8-decisions-to-make)
9. [Risks & mitigations](#9-risks--mitigations)
10. [Out of scope / future work](#10-out-of-scope--future-work)
11. [References](#11-references)

---

## 1. What Jev is (research summary)

Jev is the first public model from **TypeSafe AI** (founded by **Diogo Almeida**, an
ex-OpenAI researcher on the instruction-following/instruct work underpinning ChatGPT),
announced **2026-09-15** in early access. It is branded a **System One Model** — a class
of model built to make fast, structured decisions that software can consume directly —
in contrast to chat LLMs.

**Mechanics**

- **Typed output, not strings.** A request carries a `state` (context) and a set of
  typed `questions`; the response is typed values with probabilities. Because the output
  structure is fixed in advance, the model "cannot make type errors" and therefore
  "cannot hallucinate" (a hallucination would be a schema/type violation). *This is
  schema-validity, not correctness — see risks.*
- **Parallel sampling.** All answers to a request are produced in one shot (not
  autoregressively); adding questions barely changes latency and costs only the extra
  (cheap) input tokens.
- **Calibrated confidence.** Every answer ships a probability + confidence score so
  calling software can threshold "act" vs. "escalate."
- **Training:** **RLCD (Reinforcement Learning for Calibrated Decisions)** — positioned
  as the successor to RLHF (human preference) and RLVR (verifiable rewards).
- **Question types:** `choice` (pick from options → probabilities + confidence),
  `score` (rate against ordered levels → continuous score + distribution + confidence),
  `noul` (yes/no → probability a statement is true). Choice cardinality up to 255.

**Claimed economics (their numbers, with acknowledged eval bias)**

- **70–500 ms** end-to-end vs. 3–329 s for frontier LLMs → ~**40×–200× faster** on
  "System One-shaped" queries; headline "×193.6 faster / ×444.6 cheaper."
- **$0.042 / million input tokens** ($42 per billion); **output tokens free**
  ("too cheap to meter").
- Named after **W. S. Jevons** (Jevons paradox); "System One" from Kahneman's
  *Thinking, Fast and Slow*.

**API / ecosystem**

- JSON POST: `{ model: "jev-latest", state, questions: { name: { type, instructions } } }`.
- Official **LangChain** integration (`langchain-typesafe`, `TypeSafeClassifier`), with
  two headline middleware patterns: **`ModelRouterMiddleware`** (route to the cheapest
  capable model) and **`AutoModeMiddleware`** (classify tool calls for risk and block
  dangerous ones *before* execution).

**Status caveats:** early access / β; pricing possibly subsidized; strong claims are
vendor evals.

**Sources:** `typesafe.ai/blog/introducing-system-one-models-and-jev`; `typesafe.ai`;
`docs.typesafe.ai`; LangChain "Building a Harness with Jev"; DataCamp write-up.

---

## 2. Why it does *not* fit the existing provider layer

Choreographr's inference path is chat/completions-shaped:

- `ProviderClient::chat_completion_turn(ChatTurnRequest { model, messages, tools,
  thinking_effort, … }) -> Result<ChatTurnResult, InferenceError>` and a streaming
  variant emitting `StreamEvent::{Answer, Reasoning}` (`choreo-ai-protocols/src/traits.rs`).
- The **provider catalog** (`choreo-ai-protocols/src/catalog/`, `PROVIDER_CATALOG`
  `ArcSwap`) describes chat endpoints by wire protocol
  (`ProviderProtocol::{OpenAi, AnthropicMessages, GoogleGenerativeAi}`) plus per-model
  facts (`context_window`, `reasoning_supported`, `openai_reasoning_levels`,
  `reasoning_passback`, …). Adding a provider is officially "add a row to
  `models-overlay.toml`."

Jev violates every one of those assumptions: no `messages`, no `tools`, no text stream,
no context window in the chat sense, no reasoning-passback semantics. Forcing it in would
mean writing a fake `messages → text` shim and mislabeling catalog facts.

**The correct analogy in-tree is image generation**, which is *also* a non-chat model
capability:

- `choreo-daemon/src/daemon/image_provider.rs` resolves an account into an
  `ImageProviderHandle` (opaque handle for a tool thread), with a typed error enum
  (`Locked`, `AccountNotConfigured`, `{NoImageBackend, NoImageCapableAccount}`), reached
  via daemon state and used by the `image_gen` tool.

A **`DecisionProviderHandle`** (resolved the same way, used by a guardrail/routing hook
and/or a `classify` tool) is the natural home. Jev is synchronous HTTP/JSON; `ureq` is a
workspace dep, so **no sidecar runtime, no tokio** — consistent with the daemon's
thread-only rule and the sidecar's single purpose (driving subxt).

---

## 3. Integration surfaces

### A. Pre-dispatch tool guardrails ("Auto Mode")

The strongest fit, mapping 1:1 onto LangChain's `AutoModeMiddleware`.

Choreographr already produces Jev's ideal input at the ideal moment: every pending tool
call has a **name**, a **JSON args blob**, and — critically — `Tool::describe_invocation(
&args) -> String`, a natural-English sentence ("Running command: `…`.", "Making POST HTTP
request to `…`."). That string is already computed before execution and delivered via
`ToolCallStarted` + the seeded placeholder result. The risky tools are exactly the ones
that matter: `sh`/`exec`/`nushell`/`fish`, `git_push`, `delete_files`,
`write_file`/`edit_file`, `retrieve_webpage`/HTTP.

**Hook site:** the concurrent tool-dispatch path (`choreo-daemon/src/requests/
tool_execution.rs`), **after** `describe_invocation_json` and **before** execution:

```
state = { tool_name, invocation_description, args (optionally redacted),
          working_dir, recent user intent, session title }
questions = {
  risk:                    choice ["safe","review","destructive","exfiltration"]
  reversible:              noul   "could this call be undone?"
  irreversible_data_loss:  noul
}
if risk >= "review" (or noul > τ) → gate per policy (block | confirm | allow | sandbox)
```

Gating is a **daemon-side decision, no LLM turn** — which is where 70–500 ms + near-free
pricing changes the economics vs. asking the driving LLM to self-police.

**Why it's real, not a gimmick:** there is currently **no semantic risk gate**. Tool
`group`s are explicitly "a discovery mechanism, not access control," and `allowed_callers`
only distinguishes Direct vs. Programmatic. Jev fills a genuine gap.

**Loop integrity:** a block must inject a proper **tool *result*** (denial/error) into the
`messages ↔ tool_results` contract so the model can adapt — never silently drop the call
(that would break the reasoning/passback invariants the builder depends on).

### B. Model routing & orchestration decisions

Choreographr has cost tiers, subsessions, tool groups, and skills — all "fast structured
decisions" currently made by the expensive driving model or by heuristics:

- **Model tier routing** — classify the request → pick model/session config
  (LangChain's exact `ModelRouterMiddleware` case).
- **Subsession delegation** — score "multi-task? warrants `spawn_subsession`?" without a
  planner turn.
- **Tool-group / skill selection** — score `load_skill` candidates against the task and
  auto-activate groups cheaply.
- **Session auto-titling / tagging** — cheaper and more consistent as a classifier.

Wired either as a model-invoked tool (§D) or as daemon policy.

### C. Semantic privacy / injection gates

`choreo-sanitize` covers string safety; Jev adds a *semantic* layer:

- Classify **outbound** tool args/state for secrets, PII, prompt-injection before they
  leave to a cloud provider.
- Classify **inbound** tool outputs (fetched pages, file reads) for injection attempts
  feeding back into context.
- A calibrated "should this leave the machine?" governance decision.

**Recursion caution:** the gate itself sends data to TypeSafe — that trust question must
be resolved explicitly and opted into, not assumed.

### D. Jev as a model-invoked tool

Lowest-risk first step: expose Jev as a **feature-gated tool group** so the agent can ask
typed questions mid-loop.

```
classify(state, questions) -> { answers: { name -> { choice|score|noul, confidence } } }
```

Falls out of the existing `Tool`/`ToolDyn` machinery (custom `output_schema`,
`define_tool!` or manual `impl Tool`, postcard path for the RISC-V VM, group-gated
discovery). Captures *capability* but not the latency/cost win — A/B/C provide that.

---

## 4. Suggested architecture

1. **A new capability, not a chat protocol.** A `DecisionProvider` trait +
   `JevDecisionProvider` (ureq). Home options: a small new crate (e.g.
   `choreo-decision`) or a module of `choreo-ai-protocols`. Leave `ProviderClient` and
   the chat catalog untouched.
2. **Daemon handle mirroring images.** `DecisionProviderHandle` resolved from an account
   via daemon state; run Jev calls on a request-worker/dedicated thread; communicate by
   `crossbeam_channel` (per the thread-communication rules — no `Arc<Mutex>` shared
   state).
3. **Config.** A `[decision]` block (or `[provider.jev]`): endpoint, model
   (`jev-latest`), API-key source, per-use-case **confidence thresholds**, and a
   fail-open/fail-closed policy. Prefer a generic "System One provider" shape so other
   calibrated-decision models slot in later.
4. **Credentials.** `TYPESAFE_API_KEY` via the keystore `ServiceCredential` path. Note
   the `// TEMPORARY` single-`x_credentials` stopgap flagged in `AGENTS.md` — a decision
   provider is a good forcing function to stop overloading that single slot.
5. **Two surfaces.** (i) a model-invoked tool group (`decision`/`jev`, feature-gated like
   `content`/`blockchain`/`mcp`); (ii) an optional daemon-side pre-dispatch policy hook
   for guardrails/routing.
6. **Caching.** Hash `(state, questions)` → verdict cache. Identical tool calls must yield
   identical gates; caching is where the cost claim compounds.

---

## 5. Change inventory (if pursued)

*Indicative — to be refined when implementation starts.*

| Area | Likely change |
|---|---|
| New capability client | `DecisionProvider` trait + `Jev` impl (ureq); JSON `(state, questions) → typed answers` |
| Daemon | `DecisionProviderHandle` (mirror `image_provider.rs`); resolve from account; typed error enum |
| Agent loop | Pre-dispatch hook in `requests/tool_execution.rs`; denial-as-tool-result plumbing |
| Tools | Optional `classify` tool in a new feature-gated group (`choreo-daemon/src/tools/*`) |
| Config | `[decision]`/provider block; thresholds; fail-open/closed; endpoint/model; key source |
| Credentials | `TYPESAFE_API_KEY` via keystore; avoid further `x_credentials` overloading |
| Features | New cargo feature (off by default), like `content`/`blockchain`/`mcp` |
| Docs | `ARCHITECTURE.md` (module row + capability section), `README.md` (config) |
| Metrics | API-call timing/errors wherever the daemon records provider metrics; cache hit rate |

---

## 6. Testing strategy

- **Provider is an injectable trait object** so unit tests use a stub classifier — never
  the network (unit tests must remain deterministic; no `sleep`/time-based waits).
- **Guardrail policy unit tests:** threshold boundaries, fail-open vs. fail-closed,
  caching identical calls yield identical verdicts.
- **Loop-integrity integration test:** a blocked call injects a tool result and the loop
  continues (in `tests/it/`, `#[ignore]`, one `it` target per crate per the Test
  Discipline rules).
- **Tool test:** `classify` schema/output round-trip; postcard path for VM.

---

## 7. Phased execution plan

1. **Phase 0 — capability.** `DecisionProvider` trait + Jev impl + model-invoked
   `classify` tool (new feature-gated group). Pure capability; no behavior change to the
   loop.
2. **Phase 1 — guardrails.** Daemon-side pre-dispatch risk gate on `shell`/`git-push`/
   `delete` tools, opt-in via config, fail-closed-when-configured, with a verdict cache —
   the actual "Auto Mode."
3. **Phase 2 — routing/governance.** Model routing, subsession delegation, skill
   selection, and the semantic privacy/injection gate.

Each phase ships independently and off by default.

---

## 8. Decisions (to make)

- **Home:** new crate vs. `choreo-ai-protocols` module.
- **Surface:** tool-only, hook-only, or both (recommend both, hook optional).
- **Fail-open vs. fail-closed** default (recommend: explicit per-policy, no silent
  default for a guardrail).
- **Selectivity:** which tool groups the guardrail covers, and whether it's per-session
  configurable.
- **Privacy posture:** what may be sent to TypeSafe (redaction rules; opt-in).
- **Generic vs. Jev-specific** config schema (recommend generic "System One provider").

---

## 9. Risks & mitigations

| Risk | Mitigation |
|---|---|
| **Latency per call** (70–500 ms) compounds across tool calls | Make the hook selective (risky tools only), cache verdicts, off by default |
| **Fail-open vs. fail-closed** when Jev is unreachable/low-confidence | Explicit per-policy choice; no silent defaults for guardrails |
| **Calibration ≠ correctness** ("no hallucination" = schema-valid) | Treat confidence as adversarial-input-sensitive advisory signal, never ground truth |
| **Privacy/trust** — tool args may contain secrets sent to a third party | Opt-in only; redaction; explicit data-flow decision |
| **Availability/pricing** (β, possibly subsidized) | Fully optional/pluggable; never a hard dependency of the core loop |
| **Determinism in tests** | Injectable trait object + stub; no network in unit tests |
| **Breaks tool-loop invariant** if a block drops the call | Inject a proper tool result (denial/error); never silently drop |
| **Overloading `x_credentials`** (already a `// TEMPORARY` stopgap) | Design a proper credential path; don't extend the single-slot reuse |

---

## 10. Out of scope / future work

- Making Jev (or any System One model) a **chat** provider — deliberately rejected.
- A WIT/standard interface for "System One providers" beyond the minimal trait.
- Auto-tuning thresholds from observed gate outcomes.
- Multi-vendor System One routing (run the same question on N providers, reconcile).

---

## 11. References

- TypeSafe AI — *Introducing System One Models & Jev*:
  `https://typesafe.ai/blog/introducing-system-one-models-and-jev`
- TypeSafe AI homepage: `https://typesafe.ai/`
- TypeSafe docs (quickstart, `concepts/state`): `https://docs.typesafe.ai/`
- LangChain — *Building a Harness with Jev*:
  `https://www.langchain.com/blog/building-a-harness-with-jev`
- DataCamp — *Jev: TypeSafe's System One Model Explained*:
  `https://www.datacamp.com/blog/system-one-models-jev`
- In-repo analogues: `choreo-daemon/src/daemon/image_provider.rs`,
  `choreo-daemon/src/requests/tool_execution.rs`,
  `choreo-ai-protocols/src/traits.rs`, `choreo-ai-protocols/src/catalog/`,
  `choreo-daemon/src/tools/mod.rs` (tool groups).
