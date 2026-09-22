# Plan: OAuth provider support (ChatGPT/Codex first)

**Status:** proposed — *awaiting a decision on the refresh-persistence question
in §6 (D4)*. The login mechanics are verified against the upstream `openai/codex`
source and cross-checked against this project's local agent-harness corpus in
`~/agents` (§16); the one genuinely open design question is whether the daemon
may retain the keystore unlock key for the unlocked session so it can persist
rotated refresh tokens. Everything else is a build-out of verified wire facts.
**Date:** 2026-09-21
**Target:** first-class OAuth credentials for inference providers, starting with
the ChatGPT/Codex subscription (`openai-codex` / new `chatgpt` slug, Responses
API at `https://chatgpt.com/backend-api/codex`), behind the existing keystore and
`AddCredential` transport — **no new secret channel**.
**Touches:** `choreo-keystore` (new `ServiceCredential::OAuth` variant),
`choreo-client-core` (PKCE + loopback/device-code login, JWT claims),
`choreo-ai-protocols` (credential-derived bearer + extra headers, OAuth provider
table), `choreo-daemon` (`accounts/mod.rs`, `daemon.rs`, `providers/mod.rs`,
`sessions.rs` per-account single-flight refresh), `choreo-proto` (one
`InferenceError`/broadcast),
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
> Refresh runs in the *daemon* (it must work unattended) and must be
> **single-flight per account**, because every mature harness in `~/agents` treats
> the refresh token as **single-use/rotating** (§16) — two concurrent requests
> spending the same refresh token both fail. There is one open question: the
> daemon currently drops the 32-byte unlock key after unlock, so it cannot
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
16. [Prior art — how other harnesses do it](#16-prior-art--how-other-harnesses-do-it)

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

Two generations of endpoint paths exist across harnesses; **verify at build
time** and keep both in the OAuth table:

- `openai/codex` (`device_code_auth.rs`): `POST {issuer}/deviceauth/usercode`,
  poll `{issuer}/deviceauth/token`, verification URL `{issuer}/codex/device`,
  exchange with `redirect_uri = {issuer}/deviceauth/callback`.
- pi (`packages/ai/src/auth/oauth/openai-codex.ts`): `POST
  {issuer}/api/accounts/deviceauth/usercode` and `…/api/accounts/deviceauth/token`
  (JSON bodies), and the poll returns **`{ authorization_code, code_verifier }`**
  — not tokens — which is then exchanged through the normal token endpoint with
  `redirect_uri = {issuer}/deviceauth/callback`.

Either way the flow is: request a user code → the user opens the verification URL
and enters the code → poll until authorized → **exchange the resulting
authorization code for tokens**. Device-code **is** a supported ChatGPT path
(pi and hermes both implement it; hermes even defaults to it), so zero's preset
comment to the contrary is stale.

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
- **Where `account_id` is read from:** upstream codex and jcode parse the
  **id_token**; pi parses the **access_token** JWT (`getAccountId(token.access)`,
  same claim path). Both are observed in the wild — the adapter should try the
  access token first and fall back to the id_token (and vice versa), since the
  claim is identical.

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
refresh (blocking ureq; JSON for refresh) — single-flight per account →
update in-memory credential → drop cached client → rebuild → retry once
                        ← D4: persistence (rotating token) is the open question
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
- **The `id_token` must survive a refresh.** A refresh response commonly returns a
  new access token *without* a new id_token; overwriting the field with `None`
  would drop the `chatgpt_account_id` claim the request headers need. The refresh
  parser must replace the id_token only when the response actually carries one
  (zero, `oauth/flow.go::PostToken`, does exactly this).
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

Every harness models request auth as a value derived *from the credential*, not a
frozen string — pi's `ModelAuth { apiKey?, headers?, baseUrl? }` (per-credential
`baseUrl` for GitHub Copilot), opencode's `auth.json` union, jcode's `AuthRoute`.
Mirror that:

- *`choreo-ai-protocols`*: let the OpenAI clients accept a credential-derived
  `ProviderAuth` — a bearer token **plus per-account extra headers** (and later a
  per-credential `base_url`, e.g. Copilot). There is already a proven per-request
  header-injection mechanism — `shared::opencode_gateway_headers`, applied on both
  the OpenAI and Anthropic paths — so this is a generalization of an existing
  seam, not new plumbing.
- *`choreo-daemon` session layer*: the session already rebuilds its provider
  lazily (`SessionState::resolve_provider`) and already invalidates cached clients
  on credential change (`drop_session_clients`). So **refresh lives at the agent
  loop**: on an auth-expired `InferenceError`, refresh the token (blocking
  `ureq`, exactly like provider calls), update the in-memory credential, drop the
  cached client, and retry once. The provider crates stay synchronous and free of
  daemon concerns (no async, no token-source trait).
- **Refresh is single-flight per account** (see D4): the guard must be per
  *account*, not per session thread — one account can back many sessions, and a
  rotating refresh token spent twice is a hard failure.

**D4 — Where refresh runs, single-flight, and the persistence wrinkle.**

Daemon-side refresh is required for unattended operation (the client may not be
connected when a token expires). Three requirements fall out of the cross-harness
review (§16):

1. **Single-flight per account.** Refresh tokens are rotated and **single-use**:
   zero serializes refreshes per key and re-loads the token *inside* the lock,
   reusing a peer's rotation rather than spending the token twice; pi does the same
   inside `CredentialStore.modify`; hermes refreshes under the `auth.json` lock
   after re-reading the row. Since one account can back many sessions on different
   threads, the daemon needs a **per-account refresh guard** — a natural fit for
   the channel-based model (a refresh coordinator the session threads ask for a
   token) or a small single-purpose lock documented under the AGENTS.md
   exceptions.
2. **Refresh on a schedule, not only on failure.** Refresh before each request
   when within a **buffer** of expiry (goose 30 s, zero/hermes 60 s, pi 5 min);
   force-refresh once on a 401 (`zero.Handle401`); and optionally run a
   **proactive best-effort scheduler** (zero `RefreshScheduler`) so a long-idle
   daemon refreshes before the token lapses.
3. **Failure taxonomy.** Terminal (`invalid_grant` / `invalid_token` /
   `refresh_token_reused`) → the credential is dead, quarantine it and surface
   "re-login required" once (do not hot-loop). Transient (network / 429 / 5xx) →
   back off and retry.

The persistence wrinkle: **the daemon does not retain the 32-byte unlock key
after unlock.** `handle_unlock` / `unlock_tail` decrypt all credential blobs into
memory and then drop the key; no `DaemonState` field holds it
(`choreo-daemon/src/daemon/keystore.rs`, `daemon.rs`). Consequently the daemon
can refresh and update its **in-memory** credential, but it **cannot re-encrypt
and persist** a rotated refresh token back to the DB blob. Options:

| Option | Behavior | Cost |
|---|---|---|
| **1. In-memory only** *(recommended v1)* | Refresh updates `DaemonState.credentials`; the persisted blob is unchanged. Works until `/lock` or restart, then the *original* refresh token is re-read. Correct **iff** the provider does not rotate refresh tokens. | None; no security-model change. Requires confirming rotation (D4-open). |
| **2. Retain the unlock key for the unlocked session** | A `Zeroizing<[u8; 32]>` on `DaemonState`; the daemon re-encrypts and persists refreshed blobs. | Changes a documented invariant ("the daemon never keeps the master key"). Should be a deliberate, documented, likely opt-in change. |
| **3. Client re-push on rotation** | Daemon emits a "credential refreshed, please re-store" event; an attached client persists. | Fails whenever no client is attached (the headless-server case). |

**Recommendation:** ship **Option 1** as v1 with explicit "re-login required"
surfacing when refresh fails, and treat Option 2 as a follow-up. Note that the
cross-harness evidence (§16) says OAuth refresh tokens **do** rotate and are
single-use, so Option 1 is only correct while the daemon process lives: after a
`/lock` or restart the *original* refresh token is re-read and may already be
spent. Two consequences: (a) the per-account single-flight guard is mandatory
regardless of which D4 option ships; (b) if reuse-after-restart proves broken in
practice, Option 2 (retain the key, documented + opt-in) becomes the path to a
durable store. This is the single decision to settle before coding the refresh
path.

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

**Shipping a third-party client id is a deliberate opt-in.** The ChatGPT flow's
`client_id` is OpenAI's public Codex CLI identity, not ours; zero bakes in such
presets but keeps them **off** unless `ZERO_OAUTH_ALLOW_PRESETS` is set, precisely
because a preset is someone else's OAuth client identity. Mirror that: the
`chatgpt` entry exists in the table but the shipped client id is used only after
an explicit user opt-in (or an override), so the default credential path carries
no borrowed client identity.

**D6 — Device-code is a first-class fallback**, selected explicitly
(`--device`) or automatically when no browser/display is available.

**D7 — Distinct error surfacing, split terminal vs transient.** Add an
`InferenceError` variant for dead auth (mapped from a **terminal** refresh failure
— `invalid_grant` / `invalid_token` / `refresh_token_reused` — not from a generic
401) plus a broadcast, so the UI shows "re-login required" rather than a generic
provider error and the daemon stops replaying a doomed exchange. Transient
failures (network / 429 / 5xx) stay on the normal retry path. This mirrors hermes'
`relogin_required` classification and goose's clear-on-refresh-failure.

## 7. Cross-crate change inventory

| Crate | Change |
|---|---|
| `choreo-keystore` | `ServiceCredential::OAuth { … }` variant (§5) with zeroize + redacted Display; empty/expiry helpers. |
| `choreo-client-core` | PKCE S256 + `state` generation; loopback callback server (std threads / `tiny_http`, no async); device-code client; JWT claim parsing (`chatgpt_account_id`, `plan_type`, `email`); build the OAuth credential and reuse `build_add_credential_from_credential`. |
| `choreo-ai-protocols` | Accept credential-derived bearer + extra headers in the OpenAI clients (generalize `shared::opencode_gateway_headers`); OAuth provider parameter table; (ChatGPT has no `/models` — plan the curated model-list path). |
| `choreo-daemon` | `api_key_for` → an auth accessor returning the OAuth credential; `ProviderAuth` wiring in `providers/mod.rs`; `ResolveAccountCmd` reply carries OAuth auth; **per-account single-flight refresh guard** + 401→refresh→rebuild→retry in the session agent loop; optional proactive refresh scheduler; `oauth` catalog auth-kind; D4 persistence decision. |
| `choreo-proto` | One `InferenceError` variant + one `DaemonMessage` (re-login-required). `AddCredential` itself is unchanged (opaque bytes). |
| `choreo-tui` / `choreo-cli` | `/login <provider>` (alias `/add-oauth`), provider-picker entry, device-code fallback, re-login prompts; a CLI login subcommand. |
| docs | `ARCHITECTURE.md` (drop the now-supported slugs from the deferred note; document the credential/auth/refresh flow), `README.md` (feature-matrix OAuth row, command list). |

## 8. Frontend UX

Reuse the existing accounts surface (`/add-key`, `/account`, `Alt+A`, the
new-account wizard):

- **`/login <provider>`** (alias `/add-oauth`) — the provider picker gains a
  "Sign in with ChatGPT" entry; runs PKCE, opens the browser (`webbrowser` crate)
  or prints the URL, binds loopback `:1455` (fallback `:1457`), exchanges the
  code, then sends `AddCredential` — the identical tail to `/add-key`.
- **`--device`** (or automatic when no browser) — device-code prompt: print the
  verification URL + user code, poll, then `AddCredential`.
- **`--no-browser` / `--print-auth-url`** — for SSH/remote: print the authorize
  URL and accept a pasted callback URL or `code`. (jcode ships
  `--print-auth-url`/`--callback-url`/`--complete` with persisted `pending-login`
  state; pi races a `manual_code` prompt against the loopback server; goose times
  the callback wait out with the URL in the message.)
- **Re-login prompt** — when the daemon broadcasts "auth expired / re-login
  required", the frontend offers to re-run `/login` for that account.

## 9. Security

- PKCE **S256** + random **`state`**; bind the loopback listener to `127.0.0.1`
  only.
- **Never log** tokens, id_tokens, or callback URLs containing `code` (the
  `redact_error_url` pattern from Codex is the model).
- **Zeroize** access/refresh/id tokens; redacted `Display`/`Debug` on the new
  variant (mirrors the existing `ServiceCredential` treatment).
- **Endpoint rule, applied to configured *and* discovered endpoints (fail
  closed):** a credential-bearing endpoint must be `https`, with loopback `http`
  exempt — zero's `ValidateEndpointURL`. Discovery metadata must never downgrade
  the login to cleartext or an attacker host.
- **Refuse redirects on credential-bearing POSTs** (token exchange/refresh,
  device authorization/poll) so a 307/308 cannot replay a code, verifier, refresh
  token, or client secret to an unvalidated origin (zero `withoutRedirects`).
- **Reserved auth params are not overridable** by provider extra-params
  (`response_type` / `client_id` / `redirect_uri` / `state` / `code_challenge*`),
  and PKCE `plain` is refused (zero `isReservedAuthParam`, `ErrPKCEDowngrade`).
- **Cap and redact error bodies** (1 MiB; `error`/`error_description` only) so
  token material in an unexpected payload never lands in a log.
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
4. **Refresh.** Per-account single-flight guard; refresh when within a buffer of
   expiry and force-refresh once on 401; rebuild the session client and retry;
   D4 Option 1 (in-memory) with the terminal/transient "re-login required"
   surfacing; integration test with a stub token source (including a concurrent
   double-refresh that must spend the token once).
5. **Device-code fallback.** `/login openai --device`; mock-server test.
6. **Catalog polish + docs.** `chatgpt` slug, model list, `ARCHITECTURE.md` /
   `README.md`.
7. **Second provider proof.** Add one non-OpenAI OAuth provider (e.g.
   `github-copilot`) purely as data, to prove the seam generalizes.

## 12. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Refresh-token **rotation** is real and the token is single-use (§16), so (a) two concurrent refreshes can invalidate each other and (b) the persisted blob may hold a spent token after restart (D4). | Single-flight the refresh **per account** regardless of which D4 option ships; confirm reuse-after-restart against a live token before coding refresh; if it breaks, escalate to D4 Option 2 (retain the key, documented + opt-in). |
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
- Cross-harness prior art (§16), verified in `~/agents` — `zero/internal/oauth/`
  (`manager.go`, `flow.go`, `scheduler.go`, `presets.go`, `oauth.go`),
  `pi/packages/ai/src/auth/` (`types.ts`, `resolve.ts`, `oauth/openai-codex.ts`,
  `oauth/device-code.ts`, `oauth/meta.ts`), `goose/crates/goose/src/oauth/mod.rs`,
  `opencode/packages/opencode/src/auth/index.ts`, `jcode/OAUTH.md`,
  `hermes-agent/docs/…/model-provider-plugin.md` and `credential-pools.md`.

## 16. Prior art — how other harnesses do it

The `~/agents` tree is this project's own checkout of ~24 agent harnesses (with
`ARCHITECTURE_COMPARISON.md` and `MULTI_ACCOUNT_PROVIDER_ANALYSIS.md`). Every
harness that supports provider OAuth converges on the same shape, which this plan
mirrors.

| Harness | Shape | Notable |
|---|---|---|
| **pi** (`packages/ai/src/auth/`) | `ProviderAuth { apiKey?, oauth? }`; `OAuthAuth { login, refresh, toAuth }`; `OAuthCredential { access, refresh, expires, accountId? }`; `ModelAuth { apiKey, headers, baseUrl }` | Cleanest abstraction; **double-checked-lock refresh inside `CredentialStore.modify`**; races a manual-code prompt against the loopback server; per-provider OAuth modules (`openai-codex.ts`, `anthropic.ts`, `github-copilot.ts`, `kimi-coding.ts`, `meta.ts`, `radius.ts`, `xai.ts`, `openrouter.ts`); device-code poller with RFC 8628 `authorization_pending`/`slow_down`. |
| **zero** (`internal/oauth/`) | Go engine: `Manager.GetFresh` / `Handle401` / `refreshAndSave`, `Store`, provider presets, `RefreshScheduler` | **Single-flight refresh per key** (mutex + re-load under lock); proactive scheduler with jitter (best-effort; on-demand is the safety net); presets **off** unless `ZERO_OAUTH_ALLOW_PRESETS`; hardening (`ValidateEndpointURL`, `withoutRedirects`, reserved params, `ErrPKCEDowngrade`); id-token-preserving `PostToken`. |
| **hermes-agent** (Python) | Credential **pool** (`credential-pools.md`), `auth_type` per profile, `refresh_credential` hook | **Cross-process refresh under an `auth.json` lock, re-reading the row first**; terminal (`invalid_grant`/`refresh_token_reused`) → **DEAD + re-login**, transient → bench+retry; 6 OAuth providers; `codex_login_flow: device_code\|browser`. |
| **jcode** (Rust) | Per-provider login (`login --provider …`), `auth_mode.rs::AuthRoute`, `auth.json` + per-provider token files | Dual-auth providers (`claude`/`anthropic-api`, `openai`/`openai-api`); **consent-gated import** from Codex/Claude/OpenCode/pi/Hermes/OpenClaw; `--no-browser`/`--print-auth-url`/`--callback-url` with `pending-login` state; applies the Claude-Code OAuth request contract (identity line, tool-name remap) and the ChatGPT `originator`/`chatgpt-account-id` headers. |
| **goose** (Rust) | `oauth/mod.rs` (axum loopback + `oauth2`/`rmcp`), `GooseCredentialStore` | Rust reference for the loopback server + callback page + timeout; `REFRESH_BUFFER_SECS=30`; re-reads stored creds before refresh; clears bad creds and falls back to browser re-auth. *(Its flow is MCP-server OAuth, not provider login, but the mechanics transfer.)* |
| **opencode** (TS) | `auth.json` union `Oauth{refresh,access,expires,accountId?,enterpriseUrl?} \| Api{key} \| WellKnown` | Origin of the `auth.json` shape other tools import; one credential per provider id; `0o600` file. |
| **codex** (Rust) | `codex-rs/login/` | The authoritative ChatGPT flow (`server.rs`, `oauth/client.rs` per-endpoint encoding, `device_code_auth.rs`, `token_data.rs` claims) — the source §3 is verified against. |

**Convergent conclusions folded into this plan:** (1) request auth is a value
derived from the credential, not a static key (D3); (2) refresh tokens rotate and
are single-use, so refresh must be single-flight per account (D4); (3) refresh on
a buffer + on 401 + optionally on a timer (D4); (4) a shipped third-party client
id is an opt-in (D5); (5) terminal vs transient auth failure drives the UI (D7);
(6) headless login (device-code + paste-the-URL) is first-class (§8); (7) zero's
endpoint/redirect/PKCE hardening is adopted wholesale (§9).
