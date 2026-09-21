# Plan: OAuth provider support (ChatGPT/Codex first)

**Status:** proposed — *awaiting a decision on the refresh-persistence question
in §6 (D4)*. The login mechanics are verified against the upstream `openai/codex`
source; the one genuinely open design question is whether the daemon may retain
the keystore unlock key for the unlocked session so it can persist rotated
refresh tokens. Everything else is a build-out of verified wire facts.
**Date:** 2026-09-21
**Target:** first-class OAuth credentials for inference providers, starting with
the ChatGPT/Codex subscription (`openai-codex` / new `chatgpt` slug, Responses
API at `https://chatgpt.com/backend-api/codex`), behind the existing keystore and
`AddCredential` transport — **no new secret channel**.
**Touches:** `choreo-keystore` (new `ServiceCredential::OAuth` variant),
`choreo-client-core` (PKCE + loopback/device-code login, JWT claims),
`choreo-ai-protocols` (credential-derived bearer + extra headers, OAuth provider
table), `choreo-daemon` (`accounts/mod.rs`, `daemon.rs`, `providers/mod.rs`,
`sessions.rs` refresh-on-401), `choreo-proto` (one `InferenceError`/broadcast),
`choreo-tui` + `choreo-cli` (`/login` UX and device-code fallback), and docs
(`ARCHITECTURE.md`, `README.md`).

> **TL;DR.** Every inference credential today is a single opaque API key that the
> client encrypts to the daemon's keystore public key and ships over
> `AddCredential`; the daemon holds the plaintext in memory and the provider
> client freezes it into a `Bearer`/`x-api-key`/`x-goog-api-key` header at
> construction. OAuth needs four things it doesn't have: a **credential shape
> with a refresh lifecycle**, a **token→header abstraction** (dynamic bearer +
> credential-derived extra headers), an **interactive login flow** in the
> frontends, and a **refresh story** in the daemon. The interactive dance
> (PKCE loopback or device-code) runs in the *frontend*, because the loopback
> redirect must be on the browser's machine and the daemon is often remote —
> then the tokens ride the **existing encrypted `AddCredential` path unchanged**.
> Refresh runs in the *daemon* (it must work unattended), with one open question:
> the daemon currently drops the 32-byte unlock key after unlock, so it cannot
> re-encrypt and persist a rotated refresh token (see §6 D4).

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [What "OAuth support" means here](#2-what-oauth-support-means-here)
3. [The OpenAI/ChatGPT flow (verified)](#3-the-openaichatgpt-flow-verified)
4. [Architecture](#4-architecture)
5. [Credential model](#5-credential-model)
6. [Decisions](#6-decisions)
7. [Cross-crate change inventory](#7-cross-crate-change-inventory)
8. [Frontend UX](#8-frontend-ux)
9. [Security](#9-security)
10. [Testing strategy](#10-testing-strategy)
11. [Phased execution plan](#11-phased-execution-plan)
12. [Risks & mitigations](#12-risks--mitigations)
13. [Out of scope / future work](#13-out-of-scope--future-work)
14. [Verification / definition of done](#14-verification--definition-of-done)
15. [References](#15-references)

---

## 1. Motivation & evidence

Today a user can only authenticate a provider with a static API key. Subscription
and OAuth-only providers are therefore unreachable, even though they are the
cheapest way for an end user to use a model (a ChatGPT Plus/Pro plan, a GitHub
Copilot seat, a Qwen/Kimi coding plan).

The codebase already records this gap:

- `ARCHITECTURE.md` (~line 1366) lists the deferred providers — `chatgpt`
  (Codex), `openai-codex` OAuth, `copilot`/`copilot-acp`, `qwen-oauth`,
  `kimi-code` OAuth, `github-copilot` OAuth, `radius` — with the explicit note:
  *"Adding them requires daemon-side OAuth support and/or multi-field
  credentials, both out of scope for the current catalog model."*
- `README.md` (feature matrix, ~line 183) marks this project's **OAuth** cell as
  *"coming"*.
- `openai-codex` is already catalogued (`catalog/models-overlay.toml`,
  `base_url = "https://chatgpt.com/backend-api"`, `display_name = "OpenAI Codex
  subscription"`), so the model list is present; only the auth path is missing.

This plan closes the gap for ChatGPT/Codex first, and defines the seam so the
other OAuth providers can be added as data.

## 2. What "OAuth support" means here

The current credential path is deliberately minimal:

- `choreo_keystore::ServiceCredential` is `ApiKey { key }` | `X { … }` |
  `Substrate { … }` (`choreo-keystore/src/lib.rs`), with `#[zeroize(drop)]` and a
  redacting `Display`.
- A client encrypts the serialized credential with the X25519 public key derived
  from the daemon's **unlock key** and sends
  `ClientMessage::AddCredential { service, encrypted_payload, unlock_key }`
  (`choreo-proto/src/types.rs`, `choreo-client-core/src/credentials.rs`).
- The daemon test-decrypts the blob, persists it as an encrypted blob
  (`db::set_credential_blob`), and runs the shared unlock tail which publishes
  the plaintext into `DaemonState.credentials: HashMap<String, ServiceCredential>`
  keyed by **account name** (`choreo-daemon/src/daemon/keystore.rs`,
  `daemon.rs::finish_unlock`).
- The account→client path is **key-only**: `ResolveAccountCmd` replies with
  `(AccountConfig, Option<Zeroizing<String>>)` (`daemon.rs`), and
  `InferenceProvider::from_account_config(config, api_key, registry)` freezes
  that string into `Bearer` / `x-api-key` / `x-goog-api-key`
  (`choreo-daemon/src/providers/mod.rs`, `choreo-ai-protocols/src/openai/retry.rs`,
  `anthropic/requests.rs`, `google/requests.rs`).

So "OAuth support" is four deltas, not one:

1. **A credential with a refresh lifecycle** — access token, refresh token,
   expiry, and the identity claims needed for request headers.
2. **A token→header abstraction** — a dynamic `Authorization: Bearer` plus
   *credential-derived* extra headers (`chatgpt-account-id`, `originator`), as
   opposed to the one static key string.
3. **An interactive login flow** — PKCE loopback and/or device-code, run by the
   frontend, producing the credential above.
4. **A refresh story** — the daemon renews tokens unattended and surfaces
   "re-login required" when it cannot.

The keystore, the encryption, the `AddCredential` transport, the account model,
and the catalog all stay as they are; the deltas are additive.

## 3. The OpenAI/ChatGPT flow (verified)

All facts below were read from the upstream `openai/codex` tree
(`codex-rs/login/src/…`, `codex-rs/model-provider-info/src/lib.rs`) at the time
of writing. Two grants are used; both end with
`{ id_token, access_token, refresh_token }`.

### 3.1 Authorization code + PKCE (interactive)

- **Issuer:** `https://auth.openai.com` (`server.rs::DEFAULT_ISSUER`).
- **Authorize:** `{issuer}/oauth/authorize`; **token:** `{issuer}/oauth/token`.
- **Redirect:** `http://localhost:{port}/auth/callback`, port **1455**
  (`DEFAULT_PORT`), fallback **1457** (`FALLBACK_PORT`) — *"Keep in sync with the
  Codex CLI Hydra redirect URI allow-list."*
- **Client id:** `app_EMoamEEZ73f0CkXaXp7hrann`
  (`auth/manager.rs::CLIENT_ID`; overridable by env in Codex).
- **PKCE** S256 + a random `state` (`server.rs`, `pkce.rs`).
- **Scope:** `openid profile email offline_access api.connectors.read
  api.connectors.invoke`.
- **Extra authorize params:** `id_token_add_organizations=true`,
  `codex_cli_simplified_flow=true`, `originator=<value>` (and optional
  `allowed_workspace_id`).
- **Token exchange encoding:** *form-encoded*
  (`grant_type=authorization_code`, `client_id`, `code`, `redirect_uri`,
  `code_verifier`).
- **Refresh encoding:** *JSON*
  (`grant_type=refresh_token`, `client_id`, `refresh_token`) — the grant encoding
  is **per-endpoint**, not global (`oauth/client.rs::TokenEncoding`).

### 3.2 Device code (headless / SSH / remote)

- `POST {issuer}/deviceauth/usercode` → returns `user_code` + `device_auth_id`.
- User opens `{issuer}/codex/device` and enters the code.
- Poll `POST {issuer}/deviceauth/token` with `{ device_auth_id, user_code }`.
- Final exchange uses `redirect_uri = {issuer}/deviceauth/callback`.

### 3.3 Using the token

From `token_data.rs`, `model-provider/src/auth.rs`,
`model-provider-info/src/lib.rs:76`:

- **Base URL:** `https://chatgpt.com/backend-api/codex` (`CHATGPT_CODEX_BASE_URL`).
- **Responses API only** (`wire_api = "responses"`).
- **Request headers:** `Authorization: Bearer <access_token>`,
  `chatgpt-account-id: <account_id>`, `originator: <value>`; FedRAMP accounts add
  `X-OpenAI-Fedramp: true`.
- **Identity claims:** the id_token is a JWT; its payload carries
  `chatgpt_account_id`, `chatgpt_user_id`, `chatgpt_plan_type`, and `email`
  (the latter under the `https://api.openai.com/profile` claim; the rest under
  `https://api.openai.com/auth`). `account_id` is parsed from
  `https://api.openai.com/auth.chatgpt_account_id`.

> **Unverified / confirm-against-a-live-token.** I did **not** verify (a) whether
> the provider **rotates** the refresh token on refresh, nor (b) the exact
> `OpenAI-Beta`/`session_id` request headers — I only confirmed `originator`,
> `chatgpt-account-id`, `Authorization`, and `X-OpenAI-Fedramp`. (a) is the
> load-bearing unknown behind §6 D4; (b) is a straightforward addition once
> observed.

## 4. Architecture

```
/login openai          (frontend; loopback PKCE or device-code)
  │  browser ──authorize──▶ auth.openai.com ──302 code──▶ localhost:1455/callback
  │  POST /oauth/token (form) ──▶ { id_token, access_token, refresh_token }
  │
  ▼  build ServiceCredential::OAuth{…}, encrypt to keystore pubkey
AddCredential(service=account, encrypted_payload, unlock_key)   ← EXISTING PATH
  │
  ▼  test-decrypt → db::set_credential_blob → publish to DaemonState.credentials
daemon
  │  ResolveAccountCmd  → (AccountConfig, ProviderAuth::OAuth{…})   ← EXTENDED
  ▼
session thread: InferenceProvider::from_account_config(config, auth, registry)
  │  Authorization: Bearer <access_token>
  │  chatgpt-account-id / originator   ← credential-derived, per-account
  ▼
chatgpt.com/backend-api/codex/responses
  │  on 401 / expiry:
  ▼
refresh (blocking ureq, JSON) → update in-memory credential → drop cached
client → rebuild → retry once        ← D4: persistence is the open question
```

Two boundaries are deliberately unchanged: the **encrypted `AddCredential`
transport** (tokens never travel in cleartext) and the **account model**
(credential keyed by account name). The one extended daemon reply is
`ResolveAccountCmd`, which must carry an auth value richer than a bare key.

## 5. Credential model

Add one variant to `choreo_keystore::ServiceCredential`:

```rust
/// An OAuth credential (access + refresh token). Issued by a provider's
/// authorization server; refreshed by the daemon when the access token
/// expires or a request is rejected with 401.
#[serde(rename = "oauth")]
OAuth {
    /// Short-lived bearer token used on inference requests.
    access_token: String,
    /// Long-lived token used to mint a new access token. `None` for
    /// providers/flows that do not issue one.
    refresh_token: Option<String>,
    /// The OpenID Connect id_token (JWT), kept for identity claims
    /// (account id, plan type) needed to build request headers.
    id_token: Option<String>,
    /// Identity used as the `chatgpt-account-id` header (parsed from the
    /// id_token at login; may be `None` for providers that don't need it).
    account_id: Option<String>,
    /// Access-token expiry as Unix milliseconds, if known.
    expires_at_ms: Option<i64>,
    /// Which OAuth profile produced this credential (e.g. "chatgpt",
    /// "github-copilot"). Selects the refresh/header policy.
    issuer: String,
}
```

- The variant participates in the existing `#[zeroize(drop)]` derive and the
  redacting `Display`/`Debug` (all fields `***`).
- `ServiceCredential` is serialized with **postcard**; appending a variant is
  forward-compatible for new blobs (old daemons cannot read new blobs, which is
  fine — same binary), and new daemons continue to read old blobs.
- Keep OAuth as its **own well-typed variant**. The *other* deferred class —
  static multi-field credentials (Bedrock keys/region, Azure resource+key, GCP
  service account), also named in the `ARCHITECTURE.md` note — is a **separate**
  generalization (`ServiceCredential::Fields(HashMap<…>)`); do not couple the two
  under one shape.

## 6. Decisions

**D1 — The interactive dance runs in the frontend, not the daemon.**
The loopback redirect must be on the machine with the browser; consent is
interactive; and the daemon is frequently remote (TCP transport) or headless.
The frontend therefore performs PKCE loopback (or device-code) and pushes the
resulting tokens through the **existing** `AddCredential` path. This reuses the
whole keystore/encryption transport with **no new wire message and no new secret
channel**. Device-code covers the headless case.

**D2 — New credential variant, not a bag of fields.** See §5. OAuth is the one
credential class with a refresh lifecycle, so it earns a dedicated variant with
its own zeroize/redact treatment.

**D3 — Token→header abstraction lives in two layers, with minimal provider-churn.**

- *`choreo-ai-protocols`*: let the OpenAI clients accept a credential-derived
  bearer token **plus per-account extra headers**. There is already a proven
  per-request header-injection mechanism — `shared::opencode_gateway_headers`,
  applied on both the OpenAI and Anthropic paths — so this is a generalization of
  an existing seam, not new plumbing.
- *`choreo-daemon` session layer*: the session already rebuilds its provider
  lazily (`SessionState::resolve_provider`) and already invalidates cached clients
  on credential change (`drop_session_clients`). So **refresh lives at the agent
  loop**: on an auth-expired `InferenceError`, refresh the token (blocking
  `ureq`, exactly like provider calls), update the in-memory credential, drop the
  cached client, and retry once. The provider crates stay synchronous and free of
  daemon concerns (no async, no token-source trait).

**D4 — Where refresh runs, and the persistence wrinkle (the open question).**
Daemon-side refresh is required for unattended operation (the client may not be
connected when a token expires). The catch found in the code: **the daemon does
not retain the 32-byte unlock key after unlock.** `handle_unlock` /
`unlock_tail` decrypt all credential blobs into memory and then drop the key;
no `DaemonState` field holds it (`choreo-daemon/src/daemon/keystore.rs`,
`daemon.rs`). Consequently the daemon can refresh and update its **in-memory**
credential, but it **cannot re-encrypt and persist** a rotated refresh token back
to the DB blob. Options:

| Option | Behavior | Cost |
|---|---|---|
| **1. In-memory only** *(recommended v1)* | Refresh updates `DaemonState.credentials`; the persisted blob is unchanged. Works until `/lock` or restart, then the *original* refresh token is re-read. Correct **iff** the provider does not rotate refresh tokens. | None; no security-model change. Requires confirming rotation (D4-open). |
| **2. Retain the unlock key for the unlocked session** | A `Zeroizing<[u8; 32]>` on `DaemonState`; the daemon re-encrypts and persists refreshed blobs. | Changes a documented invariant ("the daemon never keeps the master key"). Should be a deliberate, documented, likely opt-in change. |
| **3. Client re-push on rotation** | Daemon emits a "credential refreshed, please re-store" event; an attached client persists. | Fails whenever no client is attached (the headless-server case). |

**Recommendation:** ship **Option 1** as v1 with explicit "re-login required"
surfacing when refresh fails, and treat Option 2 as a follow-up gated on
confirming whether ChatGPT rotates refresh tokens. This is the single decision to
settle before coding the refresh path.

**D5 — Auth-kind in the catalog.** Add an `auth` attribute to the provider
entry (`api_key` vs `oauth`, plus an OAuth-profile reference) so the daemon knows
which credential variant is valid and which extra headers/endpoint apply.
Candidate slugs: a new `chatgpt` (Codex subscription) entry with
`base_url = https://chatgpt.com/backend-api/codex`, Responses protocol,
`responses_path = /responses`; and reconcile the existing `openai-codex` entry.
All OAuth provider parameters (issuer, client id, scopes, redirect ports, token
and refresh encodings, header mapping) live in a small **data-driven table** so
GitHub Copilot, Qwen, Kimi-code, and Radius can be added without OpenAI-specific
code (Copilot's device-code→copilot-token exchange is a different shape).

**D6 — Device-code is a first-class fallback**, selected explicitly
(`--device`) or automatically when no browser/display is available.

**D7 — Distinct error surfacing.** Add an `InferenceError` variant for
expired/revoked auth (mapped from 401s and failed refreshes) plus a broadcast, so
the UI shows "re-login required" rather than a generic provider error.

## 7. Cross-crate change inventory

| Crate | Change |
|---|---|
| `choreo-keystore` | `ServiceCredential::OAuth { … }` variant (§5) with zeroize + redacted Display; empty/expiry helpers. |
| `choreo-client-core` | PKCE S256 + `state` generation; loopback callback server (std threads / `tiny_http`, no async); device-code client; JWT claim parsing (`chatgpt_account_id`, `plan_type`, `email`); build the OAuth credential and reuse `build_add_credential_from_credential`. |
| `choreo-ai-protocols` | Accept credential-derived bearer + extra headers in the OpenAI clients (generalize `shared::opencode_gateway_headers`); OAuth provider parameter table; (ChatGPT has no `/models` — plan the curated model-list path). |
| `choreo-daemon` | `api_key_for` → an auth accessor returning the OAuth credential; `ProviderAuth` wiring in `providers/mod.rs`; `ResolveAccountCmd` reply carries OAuth auth; 401→refresh→rebuild→retry in the session agent loop; `oauth` catalog auth-kind; D4 persistence decision. |
| `choreo-proto` | One `InferenceError` variant + one `DaemonMessage` (re-login-required). `AddCredential` itself is unchanged (opaque bytes). |
| `choreo-tui` / `choreo-cli` | `/login <provider>` (alias `/add-oauth`), provider-picker entry, device-code fallback, re-login prompts; a CLI login subcommand. |
| docs | `ARCHITECTURE.md` (drop the now-supported slugs from the deferred note; document the credential/auth/refresh flow), `README.md` (feature-matrix OAuth row, command list). |

## 8. Frontend UX

Reuse the existing accounts surface (`/add-key`, `/account`, `Ctrl+A`, the
new-account wizard):

- **`/login <provider>`** (alias `/add-oauth`) — the provider picker gains a
  "Sign in with ChatGPT" entry; runs PKCE, opens the browser (`webbrowser` crate)
  or prints the URL, binds loopback `:1455` (fallback `:1457`), exchanges the
  code, then sends `AddCredential` — the identical tail to `/add-key`.
- **`--device`** (or automatic when no browser) — device-code prompt: print the
  verification URL + user code, poll, then `AddCredential`.
- **Re-login prompt** — when the daemon broadcasts "auth expired / re-login
  required", the frontend offers to re-run `/login` for that account.

## 9. Security

- PKCE **S256** + random **`state`**; bind the loopback listener to `127.0.0.1`
  only.
- **Never log** tokens, id_tokens, or callback URLs containing `code` (the
  `redact_error_url` pattern from Codex is the model).
- **Zeroize** access/refresh/id tokens; redacted `Display`/`Debug` on the new
  variant (mirrors the existing `ServiceCredential` treatment).
- The **refresh-persistence** decision (D4) is a security-model decision and must
  be documented in `ARCHITECTURE.md` whichever way it goes.
- The token transport needs **no** new code: tokens ride the same X25519-encrypted
  `AddCredential` blob as every other credential.

## 10. Testing strategy

Per the repo's **Test Discipline** (unit tests in `src/` `#[cfg(test)]` with no
time-based waits; integration tests in a crate-level `tests/it/` target, marked
`#[ignore]`):

- **Unit** (`choreo-client-core`, `choreo-keystore`, `choreo-ai-protocols`):
  PKCE challenge derivation; `state` generation/validation; JWT claim parsing
  (well-formed, malformed, missing-auth); expiry decision (fresh vs. stale vs.
  unknown); `ServiceCredential::OAuth` zeroize + redaction; header assembly from a
  credential.
- **Integration** (`tests/it/…`, `#[ignore]`): the loopback callback server and
  the device-code polling loop against a **mock** authorization server; the
  daemon-side 401→refresh→rebuild→retry loop against a stub token source; the
  full `AddCredential` round-trip for an OAuth blob (encrypt → daemon
  test-decrypt → persist → unlock re-read).
- **Never** hit the real `auth.openai.com` in tests.

## 11. Phased execution plan

1. **Credential + transport.** Add `ServiceCredential::OAuth`; extend
   `ResolveAccountCmd` and the daemon auth accessor; unit-test round-trip,
   zeroize, redaction. No network yet.
2. **Provider layer.** Credential-derived bearer + extra headers through the
   generalized header seam; an `oauth` catalog auth-kind; a unit-tested
   header-assembly path. (Can be exercised with a hard-coded token.)
3. **Interactive login (PKCE).** `choreo-client-core` PKCE + loopback server +
   token exchange + JWT claims; wire `/login openai` in the TUI and CLI; mock-
   server integration test.
4. **Refresh.** Session agent-loop 401→refresh→rebuild→retry; D4 Option 1
   (in-memory) with "re-login required" surfacing; integration test with a stub
   token source.
5. **Device-code fallback.** `/login openai --device`; mock-server test.
6. **Catalog polish + docs.** `chatgpt` slug, model list, `ARCHITECTURE.md` /
   `README.md`.
7. **Second provider proof.** Add one non-OpenAI OAuth provider (e.g.
   `github-copilot`) purely as data, to prove the seam generalizes.

## 12. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Refresh-token **rotation** invalidates the persisted blob after restart (D4). | Confirm against a live token **before** coding refresh; if it rotates, escalate to D4 Option 2 (retain the key, documented + opt-in). |
| The ChatGPT backend is unofficial/undocumented and may change headers or paths. | Confine wire specifics to the OAuth provider table + one adapter; add a clear error when the endpoint rejects a request; document the source of truth as the Codex CLI. |
| Client id / scope / port are OpenAI's allow-listed values and may change. | Keep them in the data-driven table (overridable), with tests pinning current values. |
| No `/models` endpoint on the ChatGPT backend. | Curated list from the catalog (`openai-codex` already carries one); document the divergence. |
| Remote-daemon loopback confusion. | Device-code fallback is the documented path for remote/headless; loopback binds on the client machine only. |
| Retaining the master key (if D4 Option 2) weakens a documented invariant. | Make it explicit, documented, and opt-in; default stays Option 1. |

## 13. Out of scope / future work

- **Static multi-field credentials** (Bedrock, Azure, Vertex) — a separate
  `ServiceCredential` generalization.
- **OAuth providers beyond the first two** — added as data once the seam proves
  out, including multi-step exchanges (Copilot's copilot-token exchange).
- **Per-account credential pools / failover** (the README's "Credential
  rotation/fallback" row) — orthogonal.

## 14. Verification / definition of done

- A user can run `/login openai` (and `--device`), complete the flow in a
  browser, and have a `chatgpt` account become usable for inference with **no
  API key**.
- The tokens travel only inside the encrypted `AddCredential` blob; no token
  appears in logs, `Debug`, or on the wire in cleartext.
- An expired access token is refreshed transparently for an **unattended**
  session; a revoked refresh token surfaces a distinct "re-login required" state
  to clients.
- The full gate passes: `just pre-commit` (clippy-strict + `test-all` + fmt) is
  green in one pass, and the commit follows Conventional Commits (the commit
  message **is** the release note).
- `ARCHITECTURE.md` and `README.md` describe the new credential type, the auth
  flow, and the D4 persistence decision.

## 15. References

- `openai/codex` — `codex-rs/login/src/server.rs` (issuer, port 1455/1457,
  redirect, PKCE, scopes, extra params), `…/oauth/client.rs` (per-endpoint token
  encoding: form for code exchange, JSON for refresh), `…/auth/manager.rs`
  (`CLIENT_ID = app_EMoamEEZ73f0CkXaXp7hrann`), `…/token_data.rs` (id_token JWT
  claims), `…/device_code_auth.rs` (device-code endpoints),
  `codex-rs/model-provider-info/src/lib.rs` (`CHATGPT_CODEX_BASE_URL`),
  `codex-rs/model-provider/src/auth.rs` (`chatgpt-account-id`,
  `X-OpenAI-Fedramp`).
- This repo — `choreo-keystore/src/lib.rs` (`ServiceCredential`),
  `choreo-client-core/src/credentials.rs` (`AddCredential` build),
  `choreo-daemon/src/daemon/keystore.rs` (unlock tail; key is not retained),
  `choreo-daemon/src/providers/mod.rs` (`from_account_config`),
  `choreo-daemon/src/sessions.rs` (`resolve_provider`),
  `choreo-ai-protocols/src/shared.rs` (`opencode_gateway_headers` — the
  per-request header seam), `choreo-ai-protocols/catalog/models-overlay.toml`
  (`openai-codex`), `ARCHITECTURE.md` (~line 1366, deferred providers).
```
