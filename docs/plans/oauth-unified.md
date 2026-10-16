# Plan: unified OAuth — inference providers and MCP servers on one keystore

**Status:** proposed. Design complete through several rounds of review; nothing
implemented (no source changes in this change). Supersedes the credential-model,
refresh, and provisioning sections of `docs/plans/provider-oauth.md` and the
post-ship **P3 / D7** of `docs/plans/mcp-modernization.md`. Interacts with
`docs/plans/keystore-identity-keying.md` (see §20).
**Lifecycle:** this file is **deleted once the plan is fully implemented**. Nothing
written during implementation may reference it — rustdoc, `ARCHITECTURE.md`,
`README.md`, release notes, and commit messages must stand on their own, because a
reference to this plan would go stale the moment it is removed.
**Date:** 2026-10-15
**Targets:** `choreo-keystore`, `choreo-client-core`, `choreo-ai-protocols`,
`choreo-daemon` (`daemon/keystore.rs`, `daemon.rs`, `providers/mod.rs`,
`sessions/state.rs`, `mcp/`), `choreo-mcp`, `choreo-proto`, `choreo-tui`,
`choreo-cli`, and docs.
**Touches:** `choreo-keystore/src/lib.rs` (+ crypto, unchanged), `choreo-proto/src/types/*`,
`choreo-client-core/src/credentials.rs` (+ a new OAuth login module), `choreo-ai-protocols/src/{shared,openai,…}.rs`,
`choreo-daemon/src/{daemon.rs,daemon/keystore.rs,providers/mod.rs,sessions/state.rs,mcp/*}`,
`choreo-mcp/src/*` and the sidecar engine, `choreo-tui`/`choreo-cli` surfaces,
`ARCHITECTURE.md`, `README.md`.

> **TL;DR.** Today every credential is one opaque key the client encrypts to the
> daemon's keystore public key and ships over `AddCredential` (which also carries the
> unlock key); the daemon decrypts at unlock, freezes the key into a static request
> header, and **drops the private key**. OAuth needs a token with a refresh
> lifecycle, so the design is: **one OAuth credential variant** in the keystore
> holding only the durable grant (refresh token + client registration + identity
> claims) — the access token stays in memory; the **daemon becomes the sole blob
> writer**, encrypting to the persisted (non-secret) public-key binding for both
> provisioning *and* refresh-token rotation, so rotation needs no private key and no
> re-login; the **client login flow runs in the frontends** and pushes the result
> over the (now key-free) `AddCredential` transport; provider refresh is our own
> client, MCP refresh is `rmcp`'s `AuthorizationManager` backed by the same keystore.
> The keystore key means exactly one thing — at-rest confidentiality — and is never
> an authorization gate; a locked daemon is inert and does not become usable via an
> add. Every mature harness surveyed in `~/agents` converges on this shape.

---

## Table of contents

1. [Motivation & current state](#1-motivation--current-state)
2. [The key model](#2-the-key-model)
3. [Keystore lifecycle](#3-keystore-lifecycle)
4. [Credential model](#4-credential-model)
5. [Provisioning — AddCredential changes](#5-provisioning--addcredential-changes)
6. [Rotation persistence](#6-rotation-persistence)
7. [Provider runtime](#7-provider-runtime)
8. [MCP runtime](#8-mcp-runtime)
9. [Refresh coordination & failure taxonomy](#9-refresh-coordination--failure-taxonomy)
10. [Frontend login](#10-frontend-login)
11. [Prior art (`~/agents`)](#11-prior-art-agents)
12. [Cross-crate change inventory](#12-cross-crate-change-inventory)
13. [Phasing](#13-phasing)
14. [Security](#14-security)
15. [Testing strategy](#15-testing-strategy)
16. [Risks & mitigations](#16-risks--mitigations)
17. [Out of scope / future work](#17-out-of-scope--future-work)
18. [Open questions](#18-open-questions)
19. [Definition of done](#19-definition-of-done)
20. [Interaction with other plans](#20-interaction-with-other-plans)
21. [References](#21-references)

---

## 1. Motivation & current state

A user can only authenticate a provider with a static API key today. Subscription
and OAuth-only providers (a ChatGPT Plus/Pro plan, a GitHub Copilot seat, a
Qwen/Kimi coding plan) are unreachable, and remote MCP servers that require OAuth
cannot be connected at all.

Verified against this tree:

- `choreo_keystore::ServiceCredential` is `ApiKey { key }` | `X { … }` |
  `Substrate { … }` (`choreo-keystore/src/lib.rs`), with `#[zeroize(drop)]` and a
  redacting `Display`, serialized with **postcard**.
- The client encrypts the serialized credential to the X25519 public key derived
  from the daemon's unlock key and sends
  `ClientMessageType::AddCredential { service, encrypted_payload, unlock_key }`
  (`choreo-client-core/src/credentials.rs`).
- The daemon **verifies the binding**, **test-decrypts** the blob, persists it
  (`db::set_credential_blob`), and runs the shared `unlock_tail` which bulk-decrypts
  *every* blob into `DaemonState.credentials: HashMap<String, ServiceCredential>`
  keyed by the `service` string (`choreo-daemon/src/daemon/keystore.rs`). A valid
  `AddCredential` to a locked daemon **implicitly unlocks** it.
- The daemon **drops the 32-byte unlock key** after unlock; no `DaemonState` field
  holds it.
- Provider auth is **key-only**: `DaemonCommand::ResolveAccountCmd` replies with
  `ResolvedAccount { config, api_key: Option<Zeroizing<String>>, warm_policy }`, and
  `InferenceProvider::from_account_config(config, api_key, registry)` freezes the
  string into `Bearer` / `x-api-key` / `x-goog-api-key`. There is already a
  per-request header-injection seam (`choreo-ai-protocols/src/shared.rs::opencode_gateway_headers`).
- MCP is built on **`rmcp` 3.5** (`choreo-mcp/Cargo.toml`: `client`,
  `transport-child-process`, `transport-streamable-http-client-reqwest`); remote
  servers use a **static token in `headers`**, and a 401/403 maps to the actionable
  `McpError::AuthRequired` (`choreo-mcp/src/engine/convert.rs`).
- The README OAuth feature cell is **"coming"**; `ARCHITECTURE.md` (~line 780)
  lists the deferred OAuth-only providers and says *"Adding them requires daemon-side
  OAuth support and/or multi-field credentials, both out of scope for the current
  catalog model."*

The crypto envelope (`choreo-keystore/src/crypto.rs`) is a **sealed box**:
`encrypt_with_public_key` needs only the recipient **public** key;
`decrypt_with_private_key` is the only operation that needs the **private** key. This
asymmetry is what lets the daemon write refreshed blobs without ever holding the
private key.

## 2. The key model

### 2.1 Principles

1. **The keystore key is for at-rest confidentiality only.** It is never an
   authorization token, never a capability gate, and never the reason to allow or
   deny an operation.
2. **The keystore is the unlocked runtime state.** "Locked" means no private key, no
   plaintext, and no operation. A locked daemon is not usable, and an add does not
   make it usable.
3. **The daemon retains no private key.** It is present only for the decrypt instant
   at unlock, then dropped. Only **ciphertext blobs** and a **non-secret public-key
   binding** persist.
4. **The daemon is the sole blob writer**, encrypting to the binding for both
   provisioning and rotation.
5. **Only irreducible, durable material is at rest.** The refresh token is the root
   of the grant; the access token is derived, ephemeral, and lives in memory only.
6. **The daemon is not functional after an unattended restart.** It starts locked; a
   client must insert the key before it is operational.

### 2.2 Key material

| Key | Type | Resting place | Notes |
|---|---|---|---|
| **Unlock key** (private) | X25519 secret, 32 B | **client only** (`known_servers.toml`) | presented at unlock; transient in the daemon |
| **Keystore public key** (binding) | X25519 public, 32 B | **daemon DB**, persisted at bind | non-secret write target + wrong-key comparison |
| **Ephemeral X25519 + AES data key + salt/nonce** | per message | in the blob only | regenerated per encrypt |
| **Refresh token** | opaque | **keystore blob** (ciphertext) | the durable grant root |
| **Access token** | opaque | **daemon memory only** | never persisted |
| **id_token / claims** | JWT | **keystore blob** | identity for request headers |
| **client registration** (client_id/secret) | public / secret | **keystore blob** | provider: table value recorded; MCP: DCR/CIMD-issued |
| **PKCE verifier / state / code** | transient | client memory | per login, never persisted |

Envelope (`choreo-keystore/src/crypto.rs`):
`eph_pub(32) ‖ salt(32) ‖ nonce(12) ‖ AES-256-GCM(plaintext)`, key derived by
HKDF-SHA256 from `ECDH(ephemeral_secret, recipient_public)`. The private key is
needed **only to decrypt**.

### 2.3 Why the binding is persisted (and not a verifier blob)

An alternative considered was to persist **no** key material and verify by trialling
a decryption (a "verifier blob" — a sealed public constant). Rejected in favour of
the existing binding because:

- The public key is **non-secret** and is a *label* for the key, not the key itself;
  persisting it does not make the keystore "available" while locked (nothing can be
  decrypted without the private key).
- Verification becomes a cheap **comparison** (`public(presented) == binding`) rather
  than a trial decryption, with a precise wrong-key error.
- It is the **status quo** (`set/get_keystore_binding` already exist), so it is less
  change.
- A verifier blob would only buy literal "zero key-derived bytes on disk" purity,
  which is a policy point, not a security one: a 256-bit random key makes an offline
  guess infeasible, and neither scheme defends against a DB writer (who can only DoS
  unlock, never decrypt).

The binding row's **presence** is what drives `keystore_bound` /
`KeystoreState::Unbound` (unchanged).

### 2.4 Decisions settled in review

- **One credential per account**: a single `ServiceCredential` row per account; OAuth
  replaces an existing key (and vice versa) — no multiple credentials per account.
- **`/login` targets an existing account**: the account is created first (its
  `accounts.toml` row carries the provider); `/login` names the account and inserts
  the credential into it, exactly like `/add-key <account>`. No auto-created account.
- **MCP credential key = `mcp:<slug>|<target-hash>`** (target-scoped, *not*
  tier/project-scoped) — see §8.1.
- **Hash = the repo's stable FNV-1a** (`choreo_mcp::short_hash` mechanism), not a
  cryptographic hash — see §8.1.
- **The daemon resolves and supplies the MCP credential key** to the frontend
  (extend `McpStatus`), so provisioning and connect-time load agree.
- **Provider OAuth descriptor is served by the daemon** (`ListOAuthProviders`),
  keeping the table in one place and the client-id opt-in server-side — see §10.

## 3. Keystore lifecycle

```text
BIND (fresh daemon)                 UNLOCK                              LOCK
──────────────                      ──────                              ────
present key                         present key                         wipe memory:
  pub = public(priv)                  pub = public(priv)                  • credentials map
  verify: no binding → adopt          compare pub == binding              • access-token cache
  write binding = pub                 mismatch → "wrong key"              • (private key already gone)
  decrypt blobs (none)                match → decrypt ALL blobs
  locked = false                      → populate credentials map
                                      → load accounts
                                      → DROP private key
```

- **Wrong-key detection = comparison** against the persisted binding.
- **No implicit unlock.** A login or add never changes lock state.
- **`keystore_bound`** = the binding row's presence.
- The daemon starts `locked: true` (`daemon/open.rs:276`).

The private key is used only at the unlock instant; after the tail it is gone. The
**public** key, derived at unlock, is held in memory for the unlocked session's
writes (it is also readable from the binding while locked).

## 4. Credential model

One variant, `kind`-discriminated, carrying only the **durable subset** (no access
token):

```rust
/// An OAuth credential. Issued by a provider's or an MCP authorization server;
/// refreshed by the daemon when the access token expires or a request is 401.
#[serde(rename = "oauth")]
OAuth {
    /// Which OAuth profile this serves — selects the refresh client and the
    /// request-header policy.
    kind: OAuthKind,                      // Provider | Mcp
    /// Authorization-server issuer (validated against discovery; fail-closed).
    issuer: Option<String>,
    /// RFC 8707 resource indicator — the MCP server URL (MCP only), validated
    /// on load against the configured URL.
    resource: Option<String>,
    /// Authorization-server base URL, so refresh needs no re-discovery (MCP).
    authorization_server: Option<String>,
    /// Long-lived token that mints a new access token. The durable grant root;
    /// a rotated value is rewritten. `None` for flows that issue none.
    refresh_token: Option<String>,
    /// OIDC id_token (JWT) — identity claims; preserved across refresh.
    id_token: Option<String>,
    /// Identity used as the `chatgpt-account-id` header (provider; parsed at login).
    account_id: Option<String>,
    /// The client id actually used at login (recorded so refresh does not depend
    /// on the catalog table matching).
    client_id: Option<String>,
    /// Client secret, for confidential clients (MCP); `None` for public+PKCE.
    client_secret: Option<String>,
    /// Scopes granted.
    granted_scopes: Vec<String>,
    /// Bearer type; almost always "Bearer".
    token_type: Option<String>,
}
```

Rules:

- The variant participates in `#[zeroize(drop)]`; `Display`/`Debug` redact **every**
  field.
- **The access token is never in the variant.** It lives in a per-credential
  in-memory cache: `access_token: String`, `expires_at: Option<i64>`, `received_at`.
- **The id_token must survive a refresh.** A refresh commonly returns a new access
  token without a new id_token; the parser replaces `id_token` only when the response
  carries one.
- Serialized with postcard; appending a variant is forward-compatible for new blobs.
- Adapters to/from `rmcp::transport::auth::StoredCredentials` live **in the daemon**
  (no rmcp type enters the keystore).
- Row key: `<account>` for providers; `mcp:<slug>|<target-hash>` for MCP
  (target-scoped — see §8.1).
- **One credential per account** (decision): an account holds a single credential
  row, so signing in with OAuth **replaces** an existing API key for that account (and
  vice versa). An account with no credential is a valid state.
- Keep OAuth as its own well-typed variant; the *other* deferred class — static
  multi-field credentials (Bedrock/Azure/GCP) — is a separate generalization
  (`ServiceCredential::Fields(HashMap<…>)`), not coupled here.

## 5. Provisioning — `AddCredential` changes

The unlock key is **removed from the wire**. Because the daemon does not retain the
private key, it cannot decrypt a client-encrypted blob — so the daemon becomes the
encryptor.

```rust
// OLD (removed)
AddCredential { service: String, encrypted_payload: Vec<u8>, unlock_key: Vec<u8> }

// NEW — no key on the wire; the daemon encrypts
AddCredential { service: String, credential: ServiceCredential }
```

Daemon handler (`handle_save_credential`, rewritten):

```text
if self.locked            → reject ("unlock the keystore first")   // add implies availability
serialize postcard(credential)
blob = encrypt_with_public_key(binding_pubkey, bytes)              // no private key needed
db::set_credential_blob(service, blob)
insert into in-memory credentials map
reply CredentialAdded { service }
```

- **No `verify_keystore_binding`**, **no test-decrypt**, **no implicit unlock**. The
  daemon authored the ciphertext, so it is valid by construction — there is no
  poisoning path (every blob opens under the private key derived at unlock, since the
  binding public key is anchored to it).
- The client no longer resolves or uses the keystore key for provisioning; it
  serializes the parsed credential and sends it. The master key leaves the wire only
  on `Unlock`/`BindKeystore`.
- The credential is plaintext **on the Noise-encrypted, TOFU-pinned transport**; the
  daemon must see it regardless, so its at-rest protection is the blob.
- The daemon is the **sole writer** of credential blobs (provisioning and rotation).
- `PROTOCOL_VERSION` bumps at release.

Rationale for each of the removed checks (all consequences of "the key is only for
at-rest confidentiality"):

| Removed | Why it was never the key's job |
|---|---|
| binding verification | not authorization (transport/ACL is) and needs no comparison to encrypt |
| test-decrypt | nothing to validate — the daemon writes what it just received |
| implicit unlock | a locked daemon is inert; an add must not change that |

## 6. Rotation persistence

```text
daemon unlocked, serving
  refresh returns new refresh token (plaintext, in memory)
  → encrypt_with_public_key(binding_pubkey, updated durable subset)
  → db::set_credential_blob(key, blob)      // no private key involved
  → update in-memory credential + access-token cache
```

Same mechanism as provisioning, so there is exactly **one** blob-write path. No
private key, no re-login; works unattended **while the daemon is unlocked** (which is
the only time it serves). A crash between the network exchange and the persist leaves
a spent token → the next refresh is terminal → re-login: the correct safe failure.

## 7. Provider runtime

```text
/login <provider>            (frontend)
  PKCE S256 + state → loopback :1455 (fallback :1457) / device-code / paste-URL
  → build ServiceCredential::OAuth{ kind: Provider, … }
  → AddCredential(service = <account>, credential)              ← changed wire (§5)
daemon
  ResolveAccountCmd → ResolvedAccount { config, auth: ProviderAuth }
         ProviderAuth = ApiKey { key } | OAuth { credential, access_token }
  session: InferenceProvider::from_account_config(config, auth, registry)
     Authorization: Bearer <access>  +  credential-derived headers
     (chatgpt-account-id, originator; X-OpenAI-Fedramp for FedRAMP accounts)
     base URL + protocol + token endpoint  ← catalog OAuth table, selected by `issuer`
  on 401 / within expiry buffer:
     provider refresh engine (blocking ureq) ── single-flight per account
     → persist rotation (§6) → drop cached client → rebuild → retry once
```

- The existing per-request header seam (`shared::opencode_gateway_headers`) is
  generalized into a credential-derived `ProviderAuth` (bearer + per-account extra
  headers; later a per-credential `base_url`, e.g. Copilot).
- Provider crates stay synchronous and daemon-free; refresh lives in the session
  agent loop (which already rebuilds its provider lazily and invalidates cached
  clients on credential change).
- ChatGPT/Codex is the first provider: a new `chatgpt` slug (Responses API,
  `https://chatgpt.com/backend-api/codex`), reconciling the existing `openai-codex`
  entry. The ChatGPT backend has no `/models`, so the curated catalog list is the
  model source.
- All provider OAuth parameters (issuer, client id, scopes, redirect ports, token
  vs refresh encodings, header mapping) live in a **data-driven table** the catalog
  selects via `issuer`; other providers (Copilot, Qwen, Kimi-code, Radius) are added
  as data.
- **`/login <provider>` targets an existing account** (decision): the account is
  created first (its `accounts.toml` row carries the provider); `/login` names the
  account and inserts the credential into it, exactly as `/add-key <account>` does —
  it does not auto-create an account named after the provider.

## 8. MCP runtime

Built on `rmcp` 3.5's in-tree **`auth`** feature (pulls `oauth2`), which already
provides discovery (RFC 9728/8414), PKCE, DCR (SEP-837), CIMD (SEP-991), scope
step-up, `iss` validation, and refresh, plus a `CredentialStore` trait with an
`acquire_refresh_guard` hook. Both Rust peers surveyed (`goose`, `codex`) build MCP
OAuth on this layer.

```text
mcp login <slug>            (frontend)
  RFC 9728 protected-resource + RFC 8414 AS metadata
  DCR (SEP-837) or CIMD (SEP-991) or pre-registered client
  PKCE loopback (+ paste-URL fallback) → tokens + client registration
  → ServiceCredential::OAuth{ kind: Mcp, issuer, resource, authorization_server,
                              refresh_token, client_id, client_secret, granted_scopes }
  → AddCredential(service = "mcp:<slug>|<target-hash>", credential)   ← §8.1
daemon (choreo-mcp, sidecar tokio runtime)
  connect StreamableHttpClientTransport + AuthorizationManager + AuthClient
    discovery / DCR / refresh owned by rmcp
    CredentialStore adapter  ──>  CredentialBackend (a choreo-mcp trait)  ──> keystore
        load()    ← durable subset from the keystore
                    (cold start: no access token → rmcp refreshes on connect)
        save()    → STRIP the access token → daemon round-trip → persist durable subset
        clear()   → delete the row (logout)
        acquire_refresh_guard() → the shared per-credential lock (§9)
    terminal failure → McpError::AuthRequired { state: NotLoggedIn | Reauthentication }
```

- The `CredentialBackend` round-trip crosses the async/thread boundary with
  `tokio::task::spawn_blocking` → `DaemonCommand::OAuth{Load,Save,Clear}` answered by
  the command loop (no rmcp type crosses to the daemon; the blocking facade rule is
  preserved).
- **The access token never enters the keystore blob**: `save()` strips it, `load()`
  serves it from the in-process cache; `token_received_at` is not persisted either.
- Config gains an `oauth` block on remote servers; a server with no `oauth` simply
  gets no `Authorization` header (explicit `headers` still percolate).
- MCP credentials live in the **same keystore**, row-keyed by **target** (§8.1) —
  `mcp:<slug>|<target-hash>` — where the slug is the human-facing name and the hash
  fingerprints the server's target identity, so same-slug servers with different
  endpoints get distinct keys. The resource URL + issuer inside the blob are
  validated on load (fail-closed, SEP-2352; a mismatch quarantines and surfaces
  re-login). **This replaces the file-store design (D7) of `mcp-modernization.md`.**

### 8.1 Credential keying — target-scoped, not tier-scoped

MCP servers come from two tiers, already implemented in the daemon: the daemon-tier
`<config>/choreographr/mcp.json` (**trusted unconditionally** — the user authored it)
and a per-session project-tier `<project_root>/.mcp.json` (found by walking up to the
git root; **gated by the whole-project trust store** `<config>/choreographr/trust.toml`,
keyed on the exact canonical root with no ancestor inheritance, fail-closed). A slug is
therefore **not** globally unique: a project server **shadows** a same-slug daemon
server *for that session only* — the same scoping the connection pool uses
(`SlotKey::Daemon(slug)` vs `SlotKey::Project(root, slug)`).

The **credential row key is `mcp:<slug>|<target-hash>`**, one form for both tiers:

```text
target-identity = the server's target: { type, url, resource }   // canonicalized
target-hash     = short_hash(canonical(target-identity) "\0" slug)
row key         = "mcp:" + slug + "|" + target-hash
```

- The **slug** is the human-facing name (`/mcp status`, `mcp logout <slug>`).
- The **target-hash** distinguishes same-slug servers and re-pointing: two servers
  named `foo` with different endpoints hash differently (distinct rows); the same
  endpoint hashes the same (one shared row — correct, they are the same server).
- **The project root is not in the key.** Tier and trust are *not* keyed; they are
  enforced by the session's effective server set (shadowing), by trust-gated login,
  and by the in-blob `resource`/`issuer` validation below.

#### Why target-scoped, not tier-scoped

An earlier design keyed `mcp:daemon:<slug>` / `mcp:project:<canonical-root>:<slug>`.
The cross-harness survey (§11) rejects putting a project root in a credential key:

- **No surveyed harness does it.** Each keys MCP credentials by `(name, url)` or a
  hash of the server's config — pi `mcp__<name>|<url>`, codex
  `name|<sha256{type,url,headers}>`, opencode `sha1(name+url)`, zero
  `mcp:<name>.<sha256(config)>`, fx `name` + canonical endpoint; jcode keys by URL
  alone. **None encodes a project root into a credential key.**
- **zero is the reference for the cross-scope guard, and it uses the endpoint, not
  the project.** It binds the token to the server's *config identity* and marks
  project-touched servers so they cannot inherit a user-tier token — the exact threat
  we care about — without a project-root key.
- **A raw path in a key is fragile.** `mcp:project:<root>:<slug>` cannot be split
  unambiguously (`:` appears in Windows drive letters like `C:\…` and is legal in
  Unix filenames); it also duplicates the canonical-root computation at provisioning
  and at connect-time load. Target-hashing removes both problems.
- **Path-as-identity does have a home — the trust store.** pi and codex key their
  *project-trust* maps by a canonical path (unhashed), never a credential key;
  Choreographr's `trust.toml` does the same.

#### The hash

`short_hash` is the repo's existing stable, non-cryptographic **FNV-1a**
(`choreo-mcp/src/naming.rs`), the same mechanism the per-server log stem already uses
(`log_file_stem(identity, slug)`). It is deterministic and reproducible across runs,
processes, and toolchains — the property that matters for a persisted key — unlike
`std::collections::hash_map::DefaultHasher`, whose output Rust does not guarantee
stable across releases. A cryptographic hash (SHA-256) would also work but buys
nothing here: the input is user-authored config, and a hash collision only means two
servers share a row, which the `resource`/`issuer` validation catches fail-closed
(→ re-login, never a leak). Canonicalization of the target (lowercase scheme/host,
strip the default port and trailing slash, drop fragment/userinfo) matters more than
the hash function — a hash cannot fix a non-canonical input.

#### The daemon resolves and supplies the key

Because the key derives from a canonical target the frontend does not own, the
**daemon computes it and hands it to the client** (decision): `mcp login` obtains the
resolved server identity by extending `McpStatus`, rather than re-deriving the key
itself. Provisioning and connect-time load (`CredentialStore::load`) therefore derive
the identical key from one authority.

#### Tier gating (unchanged by the keying change)

Trust/tier still gate *whether* a credential may be used — just not via the key:

1. **Login requires the project to be trusted.** A project `.mcp.json` is
   repo-controlled, *including its `oauth` block* (issuer, client id, scopes, callback
   URL). `mcp login <slug>` for a project server **refuses while the root is
   untrusted** — the block is never expanded, dialed, or offered as a login option,
   exactly as `${VAR}`/header expansion is already gated. The daemon tier has no gate.
   Whole-project trust covers servers *and* credential expansion; there is no separate
   per-server credential approval.
2. **Load validation is fail-closed (zero's guard).** The stored `resource` (server
   URL) and `issuer` are checked against the *configured* values on load. A
   repo-controlled URL that a `git pull` re-points must **not** receive the stored
   token — mismatch → quarantine → re-login. A project credential is never blindly
   reused after a fetch. This is also what makes the target-hash's weaker collision
   resistance safe.
3. **Lifecycle follows the pool, but connections and credentials diverge:**

   | Event | Connection | Credential |
   |---|---|---|
   | session leaves the project (cwd change) | project slots drop | **kept** (the project is still trusted; leaving is transient) |
   | project **untrusted** | project slots drop | **kept but unusable** — the blob stays encrypted and is never loaded, dialed, or sent while untrusted; usable again on re-trust, with no new login |
   | daemon server removed from `mcp.json` | daemon slot drops | orphaned; reaped on the next reconcile |

   Untrust never deletes the token: the gate is on **use**, not on **existence**.

A session's effective set is `daemon ∪ own-project (project shadows same slug)`. A
project `foo` and a daemon `foo` pointing at different endpoints carry **distinct**
credentials (different hashes); if they share an endpoint they share one (same hash),
which is correct. `shared = false` is a *connection* attribute, not a credential
attribute: a private per-session connection still shares the one `(slug, target)`
credential, because the URL and issuer are identical.

## 9. Refresh coordination & failure taxonomy

**`OAuthRefreshLocks`** — `LazyLock<Mutex<HashMap<CredKey, Arc<Mutex<()>>>>>` (the
`file_locks` pattern, AGENTS.md exception #12). Provider refresh keyed by account;
rmcp's `acquire_refresh_guard` keyed by the MCP credential key (§8.1). One daemon per
keystore → a process-local lock suffices (no cross-process file lock, unlike codex/pi
which run many processes).

**Rules:** take the lock → re-load + re-check freshness → exchange → persist →
release. The loser of a race reuses the winner's rotation; a single-use token is
never spent twice.

| Outcome | Error class | Action |
|---|---|---|
| access token within buffer (default 60 s) of expiry | — | proactive refresh |
| 401 on a request | — | force-refresh once, retry the request once |
| `invalid_grant` / `invalid_token` / `refresh_token_reused` | **terminal** | mark dead, quarantine, surface one **re-login required** (no hot-loop) |
| network / 429 / 5xx | **transient** | back off, retry |

Provider side: one new `InferenceError` variant + one broadcast. MCP side:
`McpAuthState::LoggedOut(Reauthentication)`. Headless re-auth (device-code /
paste-URL) is the documented recovery.

## 10. Frontend login

Reuse the accounts surface (`/add-key`, `/account`, the new-account wizard).
`choreo-client-core` gains **one shared OAuth login library**:

- PKCE **S256** + random `state`;
- loopback callback server (std threads; `127.0.0.1` only), ports 1455/1457;
- RFC 8628 device-code poller;
- `--no-browser` / `--print-auth-url` / `--callback-url` with persisted pending-login
  state (SSH/remote);
- JWT claim parsing (`chatgpt_account_id`, plan, email);
- MCP only: RFC 9728/8414 discovery + DCR/CIMD.

Surfaces: `/login <provider>` (alias `/add-oauth`), `mcp login <slug>` /
`mcp logout <slug>`, `--device`, and re-login prompts. The TUI/CLI **bookend** the
grant (login / re-login); the daemon owns everything between (refresh, rotation,
persistence).

- **Provider descriptor from the daemon** (decision): issuer, client id, scopes,
  redirect ports, and per-endpoint encoding live in the daemon-side provider table;
  the frontend gets them from a small `ListOAuthProviders` request ⇄ descriptor reply
  rather than embedding a copy, so the table stays in one place and the
  third-party-client-id opt-in is enforced server-side. Adding the message amends the
  current unreleased wire version's changelog in `choreo-proto/src/frame.rs`; the
  `PROTOCOL_VERSION` bump is a release-time act, not part of the change.

## 11. Prior art (`~/agents`)

Every harness that supports OAuth converges on this shape.

| Harness | Credential store | Refresh coordination | MCP OAuth | Notable |
|---|---|---|---|---|
| **pi** (`packages/ai/src/auth/`, `packages/mcp/src/oauth/`, `extensions/mcp/oauth.ts`) | `auth.json` (per provider) and `mcp-auth.json` (keyed by `name\|url`) | `CredentialStore.modify` serialized read-modify-write; refresh runs inside `modify` (double-checked); cross-process `proper-lockfile` refresh lock per MCP server | RFC 9728/8414 discovery, PKCE, DCR **and** CIMD, scope step-up, `iss` validation, paste-URL fallback | cleanest `ModelAuth { apiKey?, headers?, baseUrl? }`; `refresh`/`toAuth` split |
| **zero** (`internal/oauth/`) | `provider:<name>` + `mcp:<name>` in one store (file / AES-GCM / keyring) | single-flight per key (`refreshLocks` map + re-load under lock); `Handle401`; `RefreshScheduler` (jittered, best-effort) | `mcp:` namespace already reserved | presets **off** unless `ZERO_OAUTH_ALLOW_PRESETS`; `ValidateEndpointURL`, `withoutRedirects`, reserved-param + PKCE-downgrade guards |
| **goose** (`crates/goose/src/oauth/`) | `GooseCredentialStore` over `Config` secrets (keychain) | rmcp `AuthorizationManager` (owns refresh) | **built on `rmcp::transport::auth`** | Rust reference for the loopback page + timeout; `REFRESH_BUFFER_SECS=30` |
| **codex** (`codex-rs/rmcp-client/src/oauth/`) | keyring (macOS Keychain / Win Cred Mgr / Linux Secret Service), file fallback | **`RefreshCredentialLock`** — cross-process file lock acquired *before* the authoritative re-read, held through refresh + persist; `acquire_refresh_guard` | rmcp-based; keyed by `compute_store_key(server_name, url)`; issuer binding | distinct `McpAuthState` (`LoggedOut(Login\|Reauthentication)`, `BearerToken`, `OAuth`) |
| **jcode** (`OAUTH.md`) | `~/.jcode/auth.json`, `openai-auth.json`, `gemini_oauth.json` | refresh per provider; consent-gated import from other tools | — | `--no-browser`/`--print-auth-url`/`--callback-url` with persisted pending-login; Claude-Code OAuth request contract |
| **hermes** | credential pool; `refresh_credential` hook | cross-process refresh under an `auth.json` lock, re-reading the row first | MCP OAuth (browser + device) | terminal (`invalid_grant`/`refresh_token_reused`) → **DEAD + re-login**; transient → back off + retry |
| **opencode** | `auth.json` union `Oauth{…} \| Api{key} \| WellKnown`, `0o600` | per-provider | MCP OAuth provider + loopback callback | origin of the `auth.json` shape |

Convergent conclusions adopted: (1) request auth is a value derived *from* the
credential; (2) refresh tokens rotate and are single-use → single-flight per
credential, held across exchange + persist with a re-read under the lock; (3) refresh
on a buffer + force-once on 401; (4) a shipped third-party client id is an opt-in;
(5) terminal-vs-transient drives the UI; (6) headless login is first-class; (7)
endpoint/redirect/PKCE hardening is adopted wholesale.

**MCP credential keying.** A second survey (focused on MCP credential storage) found
that every harness keys MCP credentials by `(name, url)` or a hash of the server's
config — pi `mcp__<name>|<url>`, codex `name|<sha256{type,url,headers}>`, opencode
`sha1(name+url)`, zero `mcp:<name>.<sha256(config)>`, fx `name` + canonical endpoint;
jcode keys by URL alone; goose/hermes/turnstone use a bare name only because they keep
a single global namespace. **No harness encodes a project root into a credential
key**; canonical-path keying appears only in the *trust* layer (pi, codex). zero is
the one harness with an explicit cross-scope guard, and it binds the token to the
endpoint, not the project. §8.1 adopts this shape:
`mcp:<slug>|<target-hash>`, with the in-blob `resource`/`issuer` validation as zero's
guard.

## 12. Cross-crate change inventory

| Crate | Change |
|---|---|
| `choreo-keystore` | `ServiceCredential::OAuth { … }` (§4) + `as_oauth()` + expiry helpers; zeroize/redact. Crypto unchanged. |
| `choreo-client-core` | OAuth login library (PKCE/loopback/device-code/paste-URL/JWT + MCP discovery/DCR/CIMD); `AddCredential` builder sends plaintext + `service` (no key); `/login`, `mcp login/logout`. |
| `choreo-ai-protocols` | credential-derived `ProviderAuth` (bearer + extra headers), generalizing the header seam; OAuth provider parameter table. |
| `choreo-daemon` | `handle_save_credential` rewritten (§5); `ResolveAccountCmd` carries `ProviderAuth`; `api_key_for` → auth accessor; `OAuthRefreshLocks`; provider refresh loop + rotation persistence (§6/§7); `CredentialBackend` impl + `DaemonCommand::OAuth*` for MCP; **target-scoped MCP credential keys (`mcp:<slug>\|<target-hash>`) + trust-gated login (§8.1)**; `oauth` catalog auth-kind; `AccountManager`/`drop_session_clients` extended. |
| `choreo-mcp` | enable `rmcp` `auth`; `AuthorizationManager`/`AuthClient` wiring; `oauth` config (expanded only for a trusted tier); `CredentialStore` adapter (strips access token, resolves the target-scoped key); `needs_auth` state. |
| `choreo-proto` | changed `AddCredential`; one `InferenceError` variant + re-login `DaemonMessage`; MCP auth state in `McpStatus`; `mcp login/logout` messages; version bump at release. |
| `choreo-tui` / `choreo-cli` | login surfaces; GUI/IM are follow-ups. |
| docs | `ARCHITECTURE.md`: replace the deferred-providers note and document the key model (no private key at rest, daemon-as-writer, binding, access-token-in-memory). `README.md`: OAuth feature row. |

## 13. Phasing

Each phase is independently shippable, lands with tests + docs, and keeps
`just pre-commit` green. **Provider OAuth (ChatGPT/Codex) ships first; MCP second.**

1. **Keystore foundation.** OAuth variant; `AddCredential` change (§5); daemon as
   sole blob writer; `OAuthRefreshLocks`. Unit tests. No network.
2. **Provider layer.** Credential-derived bearer + extra headers; `oauth` catalog
   auth-kind; `ResolveAccountCmd` carries `ProviderAuth`.
3. **Provider interactive login (PKCE).** `choreo-client-core` login library;
   `/login openai`, `--device`; mock-server integration test.
4. **Provider refresh.** 401 → refresh → rebuild → retry; buffer refresh; rotation
   persistence; terminal/transient `re-login required`; concurrency test.
5. **MCP OAuth.** enable rmcp `auth`; `CredentialStore` adapter + backend
   round-trips; `oauth` config; `needs_auth`; `mcp login/logout`; mock authorization
   server; conformance re-triage (move the `auth/*` scenarios out of the baseline).
6. **Polish + docs.** `chatgpt` slug + curated model list; `ARCHITECTURE.md` /
   `README.md`; second provider proof (`github-copilot`) as data.

## 14. Security

- **Master private key never at rest on the daemon**; transient at unlock.
- **No key on the `AddCredential` wire**; the credential rides Noise; at-rest
  protection is the blob.
- PKCE **S256** only; loopback binds `127.0.0.1`.
- **Never log** tokens, id_tokens, or callback URLs carrying `code`; log ids /
  lengths / kinds.
- Endpoints **https-only** (loopback `http` exempt), applied to configured *and*
  discovered URLs (fail closed); **no redirects** on credential-bearing POSTs;
  **reserved auth params** not overridable.
- Cap + redact error bodies (1 MiB; `error`/`error_description` only).
- Validate MCP `issuer`/`resource` on load (fail-closed, SEP-2352); a project
  server's credential is unusable while its root is untrusted.
- A project `.mcp.json` is untrusted input: its `oauth` block is not expanded, dialed,
  or offered for login until the project root is trusted.
- Zeroize all token fields; redact every field in `Display`/`Debug`.
- **Honest residual:** the sealed box has **no sender authentication** — anything
  holding the public key (which is non-secret) could craft a blob. This is
  **pre-existing** (the daemon handles plaintext while unlocked; a DB writer can
  already tamper), not introduced here; the daemon writing its own blobs is not a new
  capability.
- **A shipped third-party client id is an opt-in** (`/login chatgpt` uses OpenAI's
  public Codex CLI identity only after an explicit flag/env, matching zero's
  `ZERO_OAUTH_ALLOW_PRESETS`).

## 15. Testing strategy

Per AGENTS.md: unit tests in `src/` `#[cfg(test)]`, **no time-based waits**;
integration tests in a crate-level `tests/it/` target, `#[ignore]`, one binary per
crate.

- **Unit** (`choreo-keystore`, `choreo-client-core`, `choreo-ai-protocols`): OAuth
  variant zeroize/redaction/round-trip; `as_oauth` + expiry/buffer; PKCE challenge
  derivation; `state` gen/validate; JWT claim parsing (well-formed/malformed/missing);
  header assembly from a credential; the refresh-lock registry (single-flight,
  wait-free); catalog `auth` parsing; the `StoredCredentials` adapters.
- **Integration** (`tests/it/…`, `#[ignore]`): loopback callback server and
  device-code polling against a **mock** authorization server; the daemon
  401 → refresh → rebuild → retry loop against a stub token source, including a
  **concurrent double-refresh that must spend the token once**; the full
  `AddCredential` round-trip for an OAuth blob (plaintext wire → daemon encrypts →
  persist → unlock re-read); a **rotation-persist → relock → re-unlock reads the
  rotated token** test; MCP OAuth against an in-repo mock authorization server
  (scope step-up, refresh, revocation, issuer-mismatch quarantine); a daemon and a
  project server sharing a slug keep **distinct** credentials (tier scoping), a
  project server cannot be signed into while untrusted, and its credential is
  unusable while untrusted yet usable again after re-trust.
- **MCP conformance**: move the OAuth `auth/*` scenarios out of the committed
  expected-failures baseline as they pass.
- **Never** hit the real `auth.openai.com` in tests.

## 16. Risks & mitigations

| Risk | Mitigation |
|---|---|
| ChatGPT/Codex backend is unofficial/undocumented and may change headers/paths | confine wire specifics to the OAuth table + one adapter; clear error on rejection; source of truth = the Codex CLI |
| Client id / scope / port are OpenAI's allow-listed values and may change | data-driven table (overridable); tests pin current values; third-party client id opt-in |
| Refresh-token rotation + single-use: two concurrent refreshes invalidate each other | single-flight per credential (§9), held across exchange + persist, re-read under the lock |
| Rotation lost on restart | not applicable: the daemon is not functional after an unattended restart (§2.1); rotation persists while unlocked |
| Adding rmcp's `auth` feature pulls `oauth2` (weight, churn) | already the (default) MCP dependency; `auth` scoped to `choreo-mcp`; rmcp types stay behind the facade; pin 3.5, upgrade deliberately |
| Two refresh engines could drift | one lock registry + one persistence path + one failure taxonomy they both call; a shared integration test drives both |
| Remote-daemon loopback confusion | device-code / paste-URL is the documented remote path; loopback binds on the client machine only |
| No `/models` on the ChatGPT backend | curated catalog list (`openai-codex` already carries one); documented |
| A DB writer tampers with a blob/binding | pre-existing; blast radius is DoS + a decrypt failure at unlock, never disclosure |

## 17. Out of scope / future work

- **Static multi-field credentials** (Bedrock/Azure/Vertex) — a separate
  `ServiceCredential` generalization.
- **OAuth providers beyond the first two** — added as data once the seam proves out,
  including multi-step exchanges (Copilot's copilot-token exchange).
- **Per-account credential pools / failover** (README "Credential rotation/fallback")
  — orthogonal.
- **Proactive refresh scheduler** — an optional follow-up once on-demand is proven.
- **GUI / IM login surfaces** — follow-ups after TUI + CLI.

## 18. Open questions

1. **Does ChatGPT/Codex rotate refresh tokens, and what is their lifetime?** The
   plan assumes rotation; confirm against a live token in phase 4. If it does not
   rotate, §6 is belt-and-suspenders for that provider (every other provider still
   needs it).
2. **Exact ChatGPT request headers** beyond `Authorization`, `chatgpt-account-id`,
   `originator`, `X-OpenAI-Fedramp` — confirm `OpenAI-Beta`/`session_id` against a
   live token.
3. **Client-id opt-in mechanism.** How the user authorizes using OpenAI's public
   Codex CLI client id: an env var (zero's `ZERO_OAUTH_ALLOW_PRESETS` style), a config
   key, or a per-login confirmation. Leaning a config/env flag, default off, with the
   descriptor request withholding the preset when off.
4. **Re-login / auth-state surfacing.** Extend `AccountInfo` (auth kind +
   needs-relogin) and `McpServerStatus` (auth state), and/or add a one-shot
   "re-login required" broadcast on a terminal failure. Leaning both: a resting
   status field plus a broadcast nudge.
5. **GUI/IM login placement** (GUI modal vs loopback browser) — follow the
   provider-login precedent; TUI/CLI land first.

## 19. Definition of done

- A user can run `/login openai` (and `--device`) and have a `chatgpt` account usable
  for inference with **no API key**.
- An MCP remote server with an `oauth` block can be signed into via `mcp login <slug>`
  and used, and logged out via `mcp logout <slug>`.
- All OAuth tokens live only inside keystore blobs (refresh token + client
  registration + claims) and in daemon memory (access token); no token appears on the
  wire in cleartext, in logs, or in `Debug`.
- An expired access token is refreshed transparently for a session; a rotated refresh
  token is persisted and survives a lock/unlock cycle; a revoked refresh token
  surfaces a distinct **re-login required** state.
- A locked daemon refuses `AddCredential`; no implicit unlock; the private key is
  never retained after the unlock instant.
- MCP credentials are target-scoped (`mcp:<slug>|<target-hash>`); a project server
  cannot be signed into while its root is untrusted, and untrusting keeps its
  credential unusable — usable again on re-trust with no new login.
- The full gate passes: `just pre-commit` (clippy-strict + `test-all` + fmt) green in
  one pass; Conventional Commits (the commit message is the release note).
- `ARCHITECTURE.md` and `README.md` describe the new credential type, the key model,
  and both auth flows.
- **This plan document is deleted** once fully implemented, and no source/doc/commit
  references it.

## 20. Interaction with other plans

- **`provider-oauth.md`** — this plan supersedes its credential model (§5), refresh
  design (its D3/D4), and the `AddCredential` shape. Its verified ChatGPT wire facts
  (§3) and cross-harness survey remain the source.
- **`mcp-modernization.md`** — this plan **replaces P3/D7**: MCP credentials go to
  the **keystore**, not a `mcp-auth/` file store; MCP OAuth is built on `rmcp`'s
  `auth` feature (that plan already named `rmcp` as the engine). Everything else in
  M1 (shipped) is untouched.
- **`keystore-identity-keying.md`** — orthogonal but adjacent. That plan re-keys the
  client's per-daemon unlock key by the daemon's **Noise transport identity** rather
  than the dial address, and touches `build_add_credential_message` in
  `choreo-client-core/src/credentials.rs`. Because this plan removes the key from
  `AddCredential` entirely, the two must be sequenced: land identity-keying for the
  **unlock/lookup** path (which still needs the key), and drop any key-threading this
  plan renders moot in the **add** path. Coordinate before touching
  `credentials.rs`.

## 21. References

- **This tree:** `choreo-keystore/src/{lib,crypto}.rs` (`ServiceCredential`, the
  sealed-box envelope); `choreo-client-core/src/credentials.rs` (`AddCredential`
  build, key resolution); `choreo-daemon/src/daemon/keystore.rs` (unlock tail,
  binding, test-decrypt, implicit unlock); `choreo-daemon/src/daemon.rs`
  (`ResolvedAccount`, `api_key_for`); `choreo-daemon/src/sessions/state.rs`
  (`resolve_provider`); `choreo-daemon/src/providers/mod.rs`
  (`from_account_config`); `choreo-ai-protocols/src/shared.rs`
  (`opencode_gateway_headers`); `choreo-mcp/src/{config,engine/convert}.rs`
  (transport config, `AuthRequired`); `choreo-proto/src/types/*` (`AddCredential`);
  `ARCHITECTURE.md` (~780, deferred providers; the MCP client section).
- **`rmcp` 3.5** — `src/transport/auth.rs` (`AuthorizationManager` at `:1115`,
  `CredentialStore` at `:301`, `StoredCredentials` at `:225`, `OAuthClientConfig` at
  `:732`, `OAuthTokenResponse` at `:917`), feature `auth`.
- **Codex CLI (upstream `openai/codex`)** — the authoritative ChatGPT flow
  (issuer `auth.openai.com`, ports 1455/1457, form-encoded code exchange vs JSON
  refresh, device-code paths, id_token claims, `CHATGPT_CODEX_BASE_URL`).
- **`~/agents` prior art (§11):** `zero/internal/oauth/`, `pi/packages/ai/src/auth/`
  and `pi/packages/mcp/src/oauth/`, `goose/crates/goose/src/oauth/`,
  `opencode/packages/opencode/src/auth/`, `jcode/OAUTH.md`,
  `codex/codex-rs/rmcp-client/src/oauth/`, `hermes-agent` (MCP OAuth + credential
  pool).
