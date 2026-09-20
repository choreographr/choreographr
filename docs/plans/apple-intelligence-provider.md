# Plan: Apple Intelligence provider (Option B — in-process, no helper)

**Status:** proposed — *awaiting the macOS spike in §6*. The whole design rests on
one unproven link: a Rust daemon process calling `SystemLanguageModel` in-process
via a Swift static library. Everything else is known; that link must be closed on
a real Apple-silicon Mac before any production code is written.
**Date:** 2026-09-20
**Target:** a new `ProviderClient` (`AppleIntelligenceClient`) behind a new
`apple-intelligence` cargo feature (off by default), working in-process on macOS
26+/iOS 26+, with **no helper subprocess**.
**Touches:** `choreo-ai-protocols` (new `ProviderProtocol` variant + client),
a new leaf crate `choreo-apple-intelligence` (Swift bridge + C ABI + `build.rs`),
`choreo-daemon` (`providers/mod.rs`, `accounts/mod.rs`, `daemon.rs` credential
gates, `embedded.rs`), `choreo-gui` (iOS transport, Phase 4), root `Cargo.toml`
(workspace dep + feature), `justfile`, `scripts/release.sh`,
`.github/workflows/release.yml`, `packaging/`, and docs (`ARCHITECTURE.md`,
`README.md`).

> **TL;DR.** Apple exposes on-device inference to third parties through the
> **Foundation Models framework** — a *Swift-only, in-process* framework, not an
> HTTP API. Choreographr has no Swift host on desktop (the daemon and the GUI are
> both Rust binaries), so we bridge Rust↔Swift with a **linked Swift static
> library exposing a C ABI** — the exact mechanism the iOS tool bridge already
> uses (`ios/IosToolHost.swift` ↔ `choreo-gui/src/ios_bridge.rs` ↔
> `choreo-daemon/src/tools/ios_bridge.rs`). **No subprocess is needed.** Apple's
> own `/usr/bin/fm` CLI (shipped with macOS 27) proves a plain, non-app-bundle
> process can drive the model; the on-device path requires **no entitlement**.
> The one open risk is the Rust-side static link + off-main-thread call, isolated
> into the §6 spike.

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [What "support" means here](#2-what-support-means-here)
3. [Architecture (Option B)](#3-architecture-option-b)
4. [The Rust↔Swift bridge](#4-the-rustswift-bridge)
5. [Provider, catalog & accounts integration](#5-provider-catalog--accounts-integration)
6. [The macOS verification test (the spike)](#6-the-macos-verification-test-the-spike)
7. [Change inventory](#7-change-inventory)
8. [Testing strategy](#8-testing-strategy)
9. [Phased execution plan](#9-phased-execution-plan)
10. [Decisions](#10-decisions)
11. [Risks & mitigations](#11-risks--mitigations)
12. [Out of scope / future work](#12-out-of-scope--future-work)
13. [Verification / definition of done](#13-verification--definition-of-done)
14. [References](#14-references)

---

## 1. Motivation & evidence

Apple Intelligence is exposed to third-party apps through the **Foundation
Models** framework (`import FoundationModels`), available on **iOS/iPadOS/
macOS/visionOS 26.0+** and **watchOS 27+** (verified from
`developer.apple.com/documentation/foundationmodels/…`):

- `SystemLanguageModel.default` — the on-device model. `availability`
  (`.available` / `.unavailable(.deviceNotEligible | .modelNotReady | …)`),
  `isAvailable`, `contextSize` (tokens), `supportedLanguages`,
  `supportsLocale(_:)`, `tokenCount(for:)`.
- `LanguageModelSession` — `respond(to:) async`, `streamResponse(…)`
  (async sequence), a `Transcript`, `usage`, guided generation
  (`@Generable` / `GenerationSchema`), and a Swift-side **`Tool` protocol**.
  The class is `Sendable`.
- `PrivateCloudComputeLanguageModel` — a larger server model, gated by the
  `com.apple.developer.private-cloud-compute` entitlement.
- Multimodal: `Attachment` / `ImageAttachmentContent` (image prompting).

Why Option B is the right shape, and why **no helper is needed**:

1. **It is not an HTTP API.** There is no socket, no CLI contract, no wire
   protocol — it is a Swift framework linked into a process. Any integration must
   either link Swift or shell out.
2. **A plain CLI process can call it.** macOS 27 ships `/usr/bin/fm` — a
   command-line tool that runs the on-device model from Terminal ("no Xcode
   project, no download, no signup", per Apple's WWDC26 session 334). Multiple
   third-party CLIs (`foundationmodels-cli`, `foundation-model-cli`, `afm`,
   `apfel`) `swiftc`-compile a single file against `FoundationModels`. This is
   direct evidence that a daemon-class process — **no app bundle, no
   entitlement** — can host the model. Only Private Cloud Compute needs an
   entitlement.
3. **The binding mechanism already exists in-tree.** The iOS tool path links a
   Swift static library into the Rust binary and exchanges messages over a C ABI
   with a documented reply-slot ownership contract and a `MockBridge` test seam.
   We reuse that discipline; we do not invent a new one.
4. **Therefore the daemon hosts it directly.** A stdio helper would only be
   justified if the framework refused to run outside an app bundle (it does not)
   or to avoid building Swift into the Rust binary (a build-complexity
   tradeoff, not a requirement). Since the macOS GUI is also a Rust binary, a
   helper would not even spare us the Swift bridge — it would just add a
   process, a port, and lifecycle management for nothing.

### Hard facts to design against

| Fact | Source | Design consequence |
|---|---|---|
| macOS 26.0+ / iOS 26.0+ (framework); `fm` CLI is macOS 27+ | Apple docs / WWDC26 #334 | macOS 26 can use the framework via our link; the no-code CLI route needs 27. |
| No entitlement for the on-device model; PCC needs `com.apple.developer.private-cloud-compute` | Apple docs; Python SDK requirements list no entitlement | Ship on-device first; PCC is a later opt-in. |
| Availability: `.available` / `.unavailable(.deviceNotEligible) / (.modelNotReady) / …` | `SystemLanguageModel.Availability` | Map to typed `InferenceError`s; never assume available. |
| Model is a ~7 GB background download; unlicensed `fm` exits 69; `fm available` → `System model available` / `modelNotReady` / `appleIntelligenceNotEnabled` | mac.install.guide (tested 27.0 build 26A428) | Surface "not ready" distinctly from "ineligible"; the framework's own availability covers the link path. |
| Named blockers: unsupported region (mainland China excluded), Mac language ≠ Siri language, external boot volume, SIP off | macOS itself | Detectable via availability; report, don't crash. |
| On-device model ≈ 3B params, **4096-token context covering instructions + prompt + answer** | Apple docs; macOS 27 guide | Realistic only for small/compacted sessions. Needs an explicit context/overflow policy. |
| `Sendable` session/class | Apple docs | Off-main-thread use is *plausible* but unproven → §6 spike. |

---

## 2. What "support" means here

An Apple Intelligence **account** a session can select, behaving like any other
provider: `list_models` returns the on-device model; `chat_completion_turn` /
`…_streaming` run a turn; tool calls flow through the daemon's existing agent
loop; reasoning is absent; vision (image input) is present on 27+ and gated by
capability. It is **keyless** and **local**.

Non-goals for v1: Private Cloud Compute, guided-generation exposure to the LLM,
image *generation*, and any use of Foundation Models' own `Tool` protocol for
orchestration (see §3).

---

## 3. Architecture (Option B)

```
                choreo-daemon worker thread (the provider call)
                        │  AppleIntelligenceClient: ProviderClient
                        ▼
        ┌─────────────────────────── choreo-apple-intelligence ───────────────────────────┐
        │  pub trait AppleFmTransport (leaf, all targets):                                │
        │     fn availability(&self) -> Result<AppleFmAvailability, AppleFmError>;        │
        │     fn chat(&self, req: AppleFmRequest, on_event: &mut dyn FnMut(AppleFmEvent)) │
        │         -> Result<AppleFmResponse, AppleFmError>;                               │
        └───┬───────────────────────────────────────────────────────────────┬─────────────┘
            │ cfg(target_os = "macos")                                       │ cfg(target_os = "ios")
            ▼                                                                ▼
   MacOsAppleFmTransport                                         IosAppleFmTransport
   (links Swift static lib built by build.rs)                    (choreo-gui; C ABI → Swift host,
            │                                                     same shape as tools/ios_bridge.rs)
            ▼                                                                │
   swift/bridge.swift  ──  SystemLanguageModel / LanguageModelSession         ▼
   (@_cdecl C ABI; async work on a Task; deltas via a Rust fn-ptr callback)  ios/AppleFmHost.swift
```

**One Rust client, two transports.** The wire-format translation (canonical
`ChatRequestMessage` ↔ the on-device model's prompt/transcript) lives in Rust;
the transports only ferry structured requests/events across the FFI boundary.
This mirrors how every other protocol works: the client owns the semantics, the
crate owns the bytes.

### The tool-calling decision (important)

The daemon's agent loop **owns** tool execution (`ToolRegistry`, permissions,
the VM sandbox, iteration cap, undo/redo history). We therefore **do not** use
Foundation Models' async `Tool` protocol to execute tools from inside a Swift
`respond` call — that would push orchestration into Swift, hide per-tool turns
from the daemon, and require re-entering Rust synchronously from inside the
model's call stack.

Instead, **one step = one `chat` call**, exactly like every other provider:

- The bridge presents the available tools to the model (as instructions +
  a guided-generation schema), and returns either **final text** or **tool-call
  requests** (`{name, arguments_json}`) — mapped to `ChatTurnResult::FinalText`
  vs `::ToolUse`.
- The daemon runs the tools and calls `chat` again with the results appended.

The precise mechanism (a `@Generable` "either text or tool calls" schema vs.
instructing the model to emit a JSON envelope) is a **prototyping question** —
tracked in §10 as D6 and validated during Phase 2.

### Context window policy

The on-device model's ~4096-token budget covers instructions + prompt + answer.
The bridge must therefore publish `contextSize` and the client must:

- build a fresh `LanguageModelSession` per turn from the canonical history
  (the daemon is stateless per request), truncated to fit the budget with the
  most recent turns retained; or
- return a distinct `contextSizeExceeded`-derived `InferenceError` so the
  daemon's normal failure/compaction path handles it.

v1 may simply surface `context_window` from `contextSize` so the existing
UI/limits logic is honest, and fail cleanly on overflow.

---

## 4. The Rust↔Swift bridge

### C-ABI surface (macOS, `swift/bridge.swift`)

Symmetric with the iOS tool bridge: Rust→Swift is a linked `@_cdecl` symbol;
Swift→Rust is a function pointer passed as an argument, carrying a boxed reply
slot. Ownership follows the **existing binding contract** (reproduced in the
bridge's module header): Rust boxes a one-shot sender, transfers ownership at the
call, never frees it; Swift replies exactly once and drops the slot; an abandoned
(disconnected) reply is the designed timeout/cancel path.

```c
// Availability (synchronous, cheap): returns a malloc'd JSON string; caller frees.
char *afm_availability(void);

// One turn. Deltas stream via `on_event` (may be called many times);
// exactly one terminal `on_reply` delivers the final result or an error.
// `reply_ctx` is an opaque ownership token, freed only by the reply path.
void afm_chat(const char *request_json,      // canonical request (serde_json)
              void *reply_ctx,
              void (*on_event)(void *event_ctx, int32_t kind, const char *text),
              void *event_ctx,
              void (*on_reply)(void *reply_ctx, int32_t status, const char *payload));
```

Streaming rides `on_event` (kind = answer/reasoning), matching
`ProviderClient::chat_completion_turn_streaming`'s `on_event` callback. The
request/response JSON is a small, versioned internal contract owned by
`choreo-apple-intelligence` — **not** a wire protocol (it never crosses a
socket), so it is free to change with the crate.

### Linking strategy (settled by the spike)

Two viable options; the spike picks one:

- **Dylib (preferred for a first cut):** `build.rs` runs
  `xcrun swiftc -emit-library -o $OUT_DIR/libafm.dylib swift/bridge.swift`, then
  `cargo:rustc-link-search=native=$OUT_DIR`,
  `cargo:rustc-link-lib=dylib=afm`, and an rpath
  (`cargo:rustc-link-arg=-Wl,-rpath,$OUT_DIR`). The Swift runtime ships with
  macOS, so no runtime packaging is needed.
- **Static lib:** `-emit-library -static` (or `-emit-object` + `ar`). Simpler to
  distribute, but Swift static linking sometimes needs explicit
  `-L $(xcrun --show-sdk-path)/usr/lib/swift` + `-lswiftCore`. The spike records
  which works cleanly on the build host.

`build.rs` **must** compile Swift only when `CARGO_CFG_TARGET_OS == "macos"`;
every other target links nothing (the feature is inert there). This keeps the
Linux static-musl and Android builds untouched, and the macOS release runners
(macOS guests) are the only place the Swift step runs.

### Threading

Foundation Models' API is `async`; the framework does not require a run loop.
The bridge wraps each call in a Swift `Task`, and the Rust caller **blocks on
the reply channel** (never on the main queue, never by busy-waiting) with a
deadline + cancellation poll, exactly as `IosToolPending::wait` does. To
exercise the key risk, the spike deliberately issues the call from a **spawned
worker thread** (not `main`).

---

## 5. Provider, catalog & accounts integration

### `choreo-ai-protocols`

- Add `ProviderProtocol::AppleFoundationModels` (the enum is `#[non_exhaustive]`
  — the intended extension point) and `AppleIntelligenceClient` implementing
  `ProviderClient`:
  - `provider_slug()` → `"apple-intelligence"`.
  - `chat_completion_turn` / `…_streaming` → translate `ChatTurnRequest`
    (`messages`, `tools`, `cancel_rx`, …) to `AppleFmRequest`, call the
    transport, map `AppleFmResponse` → `ChatTurnResult`.
  - `list_models()` → the on-device model id (e.g. `apple-system`).
  - `context_window_for_model()` → `contextSize`.
  - No reasoning, no programmatic tool calling.
- The client holds an `Arc<dyn AppleFmTransport>` (injected; see accounts).

### Catalog & overlay

- The overlay schema (`catalog/overlay.rs :: parse_protocol`) gains an
  `"apple_fm"` value; add the `ProviderProtocol` variant to the serde
  round-trip and `Display`.
- A **bundled overlay-only provider** `apple-intelligence` (no `base_url`, no
  models.dev coverage), with one model entry: `context_window` from
  `contextSize`, `reasoning_supported = false`, `supports_vision` per OS
  capability, `reasoning_passback = "none"`.
- A new provider-level fact `requires_credential: bool` (default `true`) so the
  keyless provider is expressible without special-casing the slug in the daemon.

### `choreo-daemon`

- `providers/mod.rs :: from_account_config` gains an arm for
  `ProviderProtocol::AppleFoundationModels` that builds an
  `AppleIntelligenceClient` from the injected transport **and ignores
  `api_key`** (no "no API key" error). Because the match is already
  `_ => Err(...)`-guarded, adding the arm is the whole protocol change.
- `accounts/mod.rs`: honour `requires_credential = false` — the account lists
  with `has_credential` meaningless for it (or omitted from the credential UI).
  **Inspect the four credential gates** that currently hard-require a key
  (`daemon.rs:1026`, `:1126`, `:1756`, `:2628`, `api_key_for().is_none()`), and
  route them through a `provider_requires_credential(slug)` helper so an
  Apple Intelligence account can run while the keystore is locked.
- Transport injection: add an `Option<Arc<dyn AppleFmTransport>>` to
  `DaemonState`, defaulting to the macOS transport (built lazily, `cfg(macos)`)
  and overridable by the embedder.

### Feature gating

`apple-intelligence` cargo feature, **off by default**, exactly like
`blockchain` / `content` / `mcp`: it pulls in `choreo-apple-intelligence` (and
the Swift build step) and a plain build contains none of it. Wire through the
root `Cargo.toml` (`apple-intelligence = ["choreo-daemon/apple-intelligence"]`)
and add it to nothing in `default`.

---

## 6. The macOS verification test (the spike)

**This is the gate.** Run it on an Apple-silicon Mac with Apple Intelligence
enabled before writing production code. It answers the only two open questions:
(1) can a plain Rust process link Swift and call `SystemLanguageModel` in-process,
and (2) does it work **off the main thread**.

Create a throwaway crate **outside** the workspace (so it never ships) — e.g.
`/tmp/afm-spike/` — with this exact layout:

```
/tmp/afm-spike/
├── Cargo.toml
├── build.rs
├── swift/bridge.swift
└── src/main.rs
```

### Step 0 — prove the framework works from a plain process at all

```bash
cat > /tmp/afm-probe.swift <<'SWIFT'
import FoundationModels
@main struct Probe {
  static func main() async {
    let m = SystemLanguageModel.default
    print("availability:", m.availability, "context:", m.contextSize)
    do {
      let s = LanguageModelSession()
      let r = try await s.respond(to: "Reply with exactly: ok")
      print("answer:", r.content)
    } catch { print("error:", error) }
  }
}
SWIFT
xcrun swiftc /tmp/afm-probe.swift -o /tmp/afm-probe && /tmp/afm-probe

# And the OS-blessed CLI path (macOS 27+):
sudo fm license           # one-time; exit 69 until accepted
fm available              # "System model available" | modelNotReady | appleIntelligenceNotEnabled
echo 'say ok' | fm respond
```

Expected: `availability: available`, a token count, and a short answer. If it
says `modelNotReady`, let the ~7 GB download finish; if it says
`appleIntelligenceNotEnabled`, fix the region/language/SIP conditions first.

### Step 1 — the Rust↔Swift in-process link

**`Cargo.toml`**

```toml
[package]
name = "afm-spike"
version = "0.0.0"
edition = "2024"

[build-dependencies]
# none — build.rs shells out to xcrun directly
```

**`build.rs`**

```rust
fn main() {
    // Compile (and link) the Swift bridge only on macOS; on any other host the
    // crate is inert — this is the gate that keeps Linux/Android builds clean.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let dylib = format!("{out}/libafm_spike.dylib");
    let status = std::process::Command::new("xcrun")
        .args(["swiftc", "-emit-library", "-o", &dylib, "swift/bridge.swift"])
        .status()
        .expect("failed to run xcrun swiftc");
    assert!(status.success(), "swiftc failed");
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=dylib=afm_spike");
    // Load the dylib at runtime from OUT_DIR (rpath); Swift's runtime ships with macOS.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{out}");
    println!("cargo:rerun-if-changed=swift/bridge.swift");
}
```

**`swift/bridge.swift`**

```swift
import Foundation
import FoundationModels

// Availability probe — synchronous, cheap. Returns a malloc'd JSON string that
// the Rust side frees with `libc::free` via a helper below; NULL only on OOM.
@_cdecl("afm_availability")
public func afm_availability() -> UnsafeMutablePointer<CChar>? {
    let m = SystemLanguageModel.default
    var reason = "available"
    if case .unavailable(let r) = m.availability { reason = "\(r)" }
    let obj: [String: Any] = [
        "isAvailable": m.isAvailable,
        "reason": reason,
        "contextSize": m.contextSize,
        "languages": m.supportedLanguages.map { $0.identifier },
    ]
    guard let data = try? JSONSerialization.data(withJSONObject: obj),
          let s = String(data: data, encoding: .utf8) else { return nil }
    return strdup(s)
}

// One turn. Does the async work on a Task (no run loop needed) and replies
// exactly once through the Rust callback. Ownership of reply_ctx transfers to
// Swift; Swift only hands it back through reply_cb, which frees it.
@_cdecl("afm_chat")
public func afm_chat(_ prompt: UnsafePointer<CChar>?,
                     _ replyCtx: UnsafeMutableRawPointer?,
                     _ replyCb: @convention(c)
                        (UnsafeMutableRawPointer?, Int32, UnsafePointer<CChar>?) -> Void) {
    let p = prompt.map { String(cString: $0) } ?? ""
    guard let ctx = replyCtx else { return }
    Task {
        do {
            let session = LanguageModelSession()
            let r = try await session.respond(to: p)
            let buf = strdup(r.content)
            replyCb(ctx, 0, buf)          // Rust copies inside the call…
            if let buf { free(buf) }      // …then Swift frees.
        } catch {
            let buf = strdup("\(error)")
            replyCb(ctx, 1, buf)
            if let buf { free(buf) }
        }
    }
}
```

**`src/main.rs`**

```rust
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::mpsc;

unsafe extern "C" {
    fn afm_availability() -> *mut c_char;
    fn afm_chat(
        prompt: *const c_char,
        reply_ctx: *mut c_void,
        reply_cb: extern "C" fn(*mut c_void, i32, *const c_char),
    );
    fn free(ptr: *mut c_void);
}

// Swift→Rust reply: reconstruct the boxed sender (its single free point),
// copy the payload, deliver it. This is the ownership contract in miniature.
extern "C" fn on_reply(ctx: *mut c_void, status: i32, payload: *const c_char) {
    let tx = unsafe { Box::from_raw(ctx.cast::<mpsc::Sender<(i32, String)>>()) };
    let s = if payload.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(payload) }.to_string_lossy().into_owned()
    };
    let _ = tx.send((status, s));
}

fn main() {
    // Availability.
    unsafe {
        let a = afm_availability();
        if !a.is_null() {
            println!("availability: {}", CStr::from_ptr(a).to_string_lossy());
            free(a.cast::<c_void>());
        }
    }

    // The load-bearing test: call from a SPAWNED worker thread, not main —
    // this is exactly what the daemon will do (a provider worker thread).
    let handle = std::thread::spawn(|| {
        let prompt = CString::new("Reply with exactly: ok").unwrap();
        let (tx, rx) = mpsc::channel::<(i32, String)>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();
        unsafe { afm_chat(prompt.as_ptr(), ctx, on_reply) };
        // Block on the reply channel (the daemon's wait pattern), not a poll.
        match rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok((0, s)) => println!("answer: {s}"),
            Ok((code, s)) => println!("bridge error {code}: {s}"),
            Err(e) => println!("channel error: {e}"),
        }
    });
    handle.join().unwrap();
}
```

### Step 2 — the iOS-equivalent check (only if pursuing iOS in the same pass)

Confirm the framework is reachable from the app's Swift host by adding a trivial
`@_cdecl("afm_probe")` to `ios/IosToolHost.swift` and calling it over the
existing bridge. This is a one-line smoke test; full iOS wiring is Phase 4.

### Spike acceptance criteria

- Step 0 prints `availability: available` and an answer.
- Step 1 prints an `availability:` JSON line **and** `answer: ok...` from the
  **worker thread**.
- The process runs to completion with the dylib loaded (rpath works) — no
  `dyld: Library not loaded`.
- Recorded in the PR/issue: the exact macOS build, the link strategy that
  worked (dylib vs static), and whether `-static` needed extra Swift-runtime
  flags.

If Step 1 fails, the fallback is `fm serve` (macOS 27's OpenAI-compatible local
HTTP endpoint on a port or Unix socket) with an `openai`-protocol account — a
near-zero-Rust escape hatch. It is **not** the primary path, only the
contingency recorded here.

---

## 7. Change inventory

### New crate: `choreo-apple-intelligence`

- `pub trait AppleFmTransport` + request/response/event structs + `AppleFmError`
  (thiserror). **No platform code** — compiles on every target; the daemon's
  `ProviderClient` depends on the trait only.
- `MacOsAppleFmTransport` behind `#[cfg(target_os = "macos")]`: the C ABI
  (`unsafe extern "C"`), the `build.rs` Swift compile, the blocking wait with
  deadline + `cancel_rx` polling (the `IosToolPending` pattern).
- `swift/bridge.swift`: `SystemLanguageModel` / `LanguageModelSession` wrapper.
- `MockAppleFmTransport` (test seam, mirroring `MockBridge`) so the client's
  translation logic is unit-tested with no Apple runtime on any dev box.
- `build.rs`: macOS-only `swiftc` invocation; `cargo:rerun-if-changed`.

### `choreo-ai-protocols`

- `catalog/mod.rs`: `ProviderProtocol::AppleFoundationModels` (+ `Display`,
  serde round-trip).
- `catalog/overlay.rs`: `parse_protocol` accepts `"apple_fm"`; `requires_credential`
  provider fact.
- `catalog/models-overlay.toml`: overlay-only provider `apple-intelligence`.
- new `src/apple/` module: `AppleIntelligenceClient: ProviderClient` + message
  translation (`ChatRequestMessage` → `AppleFmRequest`; `AppleFmResponse` +
  events → `ChatTurnResult`/`StreamEvent`).
- `Cargo.toml`: optional dep on `choreo-apple-intelligence` behind the
  `apple-intelligence` feature (the workspace promotes the dep).

### `choreo-daemon`

- `providers/mod.rs`: the new match arm; accept a transport; ignore `api_key`.
- `accounts/mod.rs`: `requires_credential` awareness + credential-status
  reporting.
- `daemon.rs`: route the four `api_key_for(...).is_none()` gates through
  `provider_requires_credential(slug)`; build/store the macOS transport in
  `DaemonState`.
- `embedded.rs` / `EmbeddedOptions`: an injectable transport (Phase 4, iOS).
- `Cargo.toml`: `apple-intelligence` feature forwarding to
  `choreo-apple-intelligence`.

### Build, CI & packaging

- root `Cargo.toml`: workspace member `choreo-apple-intelligence`, workspace
  dep, root feature.
- `justfile`: a `check-macos` extension that includes the new crate (the existing
  `check-macos` gate already type-checks libs for `aarch64-apple-darwin`).
- `.github/workflows/release.yml`: the macOS jobs already run on macOS runners —
  ensure the `apple-intelligence` feature is in the macOS build set and that
  `swiftc`/Xcode is present on those runners (it is, for the iOS job).
- `scripts/release.sh`: confirm the feature list for macOS artifacts.
- `packaging/`: **no new runtime dependency** for the on-device path (no helper
  binary, no dylib to ship beyond the app's own OUT_DIR-linked library — verify
  the dylib lands inside the binary's search path or is statically linked).

### Docs

- `ARCHITECTURE.md`: a `choreo-apple-intelligence` module row + the FFI contract
  rationale (extends the iOS-bridge section).
- `README.md`: the provider list + an Apple Intelligence section (setup:
  enable Apple Intelligence, accept the model, expect `modelNotReady` until the
  download completes).
- This plan is marked done at the end.

---

## 8. Testing strategy

| Layer | Where | What |
|---|---|---|
| Client translation | `choreo-ai-protocols` unit tests | `MockAppleFmTransport` scripts responses; assert `ChatRequestMessage`→`AppleFmRequest` mapping, `ChatTurnResult::ToolUse` vs `FinalText`, streaming events, cancellation precedence. **No Apple runtime.** |
| Transport contract | `choreo-apple-intelligence` unit tests | Reuse the iOS bridge's contract tests shape: reply-slot ownership, exactly-once, timeout (`Duration::ZERO`), abandoned reply. Deterministic, no sleeps. |
| Catalog/accounts | `choreo-ai-protocols`/`choreo-daemon` unit tests | `parse_protocol("apple_fm")`, `requires_credential=false` behavior, keyless `from_account_config` arm. |
| FFI (macOS only) | the §6 spike | The one thing CI cannot cover from Linux; run on a Mac, record results. |
| Integration (optional) | `choreo-daemon/tests/it/` | A `#[ignore]` test that (on macOS) builds a real provider against the system model and runs one turn — kept `#[ignore]` because it needs Apple Intelligence. |

Unit tests must never sleep (AGENTS.md); the FFI spike is a manual macOS step,
not a unit test.

---

## 9. Phased execution plan

Each phase is one commit; overlapping-file phases run as **serial subsessions**.

- **Phase 0 — spike (this document, §6).** Run on a Mac; record link strategy +
  OS build. **Gate: do not start Phase 1 until Step 1 passes.**
- **Phase 1 — crate + transport + bridge (macOS).** `choreo-apple-intelligence`
  with the trait, `MacOsAppleFmTransport`, `bridge.swift`, `build.rs`,
  `MockAppleFmTransport`, and contract tests. Compiles inert off-macOS.
- **Phase 2 — provider client.** `ProviderProtocol::AppleFoundationModels`,
  `AppleIntelligenceClient`, translation, streaming, availability→error mapping;
  catalog + overlay + `models-overlay.toml`; feature wiring. Tool-call mechanism
  (D6) chosen and unit-tested against the mock.
- **Phase 3 — daemon integration.** `from_account_config` arm; keyless
  `requires_credential` threading through the account gates; `DaemonState`
  transport; end-to-end with the mock.
- **Phase 4 — iOS.** `IosAppleFmTransport` in `choreo-gui` +
  `ios/AppleFmHost.swift`; inject via `EmbeddedOptions`.
- **Phase 5 — release/packaging/docs.** macOS release set, README/ARCHITECTURE,
  mark this plan done.

Dependencies: 0 → 1 → 2 → 3 → (4) → 5.

---

## 10. Decisions

**Locked:**

- **D1 — Option B, in-process, no helper.** Link a Swift static lib (dylib
  preferred for the first cut); no subprocess except the documented `fm serve`
  contingency.
- **D2 — On-device only in v1.** Private Cloud Compute deferred (it needs an
  entitlement and, in macOS 27.0, isn't reachable via `fm` anyway).
- **D3 — Feature-gated, off by default** (`apple-intelligence`), macOS-first.
- **D4 — Tool orchestration stays in the daemon.** One `chat` call per step;
  no Foundation Models `Tool` protocol for the agent loop.
- **D5 — Keyless provider** via a catalog `requires_credential` fact.

**To confirm during implementation:**

- **D6 — Tool-call mechanism:** guided-generation schema emitting
  "text-or-tool-calls" vs. a prompt-embedded JSON envelope. *Prototype in
  Phase 2; pick whichever yields reliable `ToolUse` turns.*
- **D7 — Link strategy:** dylib vs static (decided by the §6 spike).
- **D8 — Context policy:** truncate-and-retain vs. hard-fail on overflow.
  *Recommendation: publish `contextSize`, truncate oldest turns in v1, keep
  overflow as an explicit error.*
- **D9 — Vision:** expose image input (27+, `Attachment`) in v1 or defer?
  *Recommendation: defer to a follow-up; text-first.*
- **D10 — Model ids:** single `apple-system` in v1 (add `apple-pcc` later).

---

## 11. Risks & mitigations

- **Rust↔Swift static link unproven (the gating risk).** Isolated in the §6
  spike; contingency is `fm serve`. *Do not proceed past Phase 0 if Step 1 fails.*
- **Off-main-thread behavior.** `Sendable`, but unverified — the spike calls
  from a spawned thread on purpose.
- **Tiny context (4k).** Realistic only for small/compacted sessions; publish
  the window and fail cleanly (D8). Post-v1, PCC lifts this.
- **Availability is environmental.** Region/language/SIP/external-volume and the
  7 GB download produce `unavailable(...)`; map to clear, actionable errors and
  never treat "installed OS" as "usable".
- **Unsigned binaries.** `fm` is Apple-signed; community CLIs are ad-hoc-signed
  and work. Our unsigned Homebrew/tarball binaries should be fine (no
  entitlement), but this is one more thing the spike implicitly validates — if
  a signature is ever required for the *library* path, the fallback is the
  Apple-signed `fm`/`fm serve`.
- **macOS-only build step.** `build.rs`/`swiftc` runs only on macOS runners; it
  must be a strict no-op elsewhere so Linux/Android/musl builds are unaffected
  (verified by `just test-lean` on a non-macOS host).
- **Model quality.** ~3B params, no world knowledge, weak at code/math (Apple
  says so plainly). Set expectations in docs; it is an *offline/private/local*
  provider, not a frontier replacement.

---

## 12. Out of scope / future work

- Private Cloud Compute (entitlement + larger context + stronger reasoning).
- Image prompting (`Attachment`) — deferred (D9).
- `@Generable`/guided generation exposed to the LLM as a tool.
- Apple's Python SDK (`apple-fm-sdk`) — not used; we link Swift directly.
- `fm serve` / `fm respond` **as the product path** — contingency only.
- Windows/Linux "Apple Intelligence" — does not exist.

---

## 13. Verification / definition of done

- §6 spike passed on a real Apple-silicon Mac, results recorded (OS build, link
  strategy, worker-thread success).
- `apple-intelligence` feature off → byte-for-byte today's build (no Swift, no
  new deps compiled).
- On macOS with the feature on and Apple Intelligence enabled: a session selects
  the Apple Intelligence account and completes a tool-using turn through the
  daemon's normal loop; streaming deltas reach the client; `modelNotReady` /
  `deviceNotEligible` surface as clear errors.
- Unit tests green everywhere (mock transport); `just pre-commit` green on the
  dev host; the macOS crate type-checks under `just check-macos`.
- Docs updated (`ARCHITECTURE.md`, `README.md`); commit messages written as
  release notes.

---

## 14. References

- Apple — [Foundation Models](https://developer.apple.com/documentation/foundationmodels)
  (framework overview; availability 26.0+).
- Apple — [`SystemLanguageModel`](https://developer.apple.com/documentation/foundationmodels/systemlanguagemodel)
  (`availability`, `contextSize`, `supportedLanguages`).
- Apple — [`LanguageModelSession`](https://developer.apple.com/documentation/foundationmodels/languagemodelsession)
  (`respond`, `streamResponse`, `Transcript`, `usage`).
- Apple — WWDC26 session 334, *Build AI-powered scripts with the `fm` CLI and
  Python SDK* (the `fm` tool, `--model pcc`, `--image`, `--schema`).
- Apple — [Foundation Models SDK for Python](https://github.com/apple/python-apple-fm-sdk)
  (requirements: macOS 26.0+, Xcode 26.0+, Apple Intelligence — **no entitlement**).
- mac.install.guide — [The `fm` Command for Apple AI](https://mac.install.guide/terminal/fm-command)
  (license gate/exit 69, `fm available` states, ~3B/4096-token model, `fm serve`).
- In-tree precedents: `choreo-daemon/src/tools/ios_bridge.rs`,
  `choreo-gui/src/ios_bridge.rs`, `ios/IosToolHost.swift`,
  `choreo-daemon/src/embedded.rs`, `choreo-daemon/src/providers/mod.rs`,
  `choreo-ai-protocols/src/catalog/mod.rs`.
- Sibling plan: `docs/plans/risc-v-linux-target.md` (plan structure + the
  "feature-gate so a plain build is unaffected" pattern).
