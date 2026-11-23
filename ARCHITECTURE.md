# Choreographr Architecture

## Overview

`Choreographr` is a client/server AI assistant built as a Rust workspace. A **daemon** process
communicates with multiple LLM providers through a pluggable trait-based provider
system, while **clients** (terminal, desktop, and IM platforms) connect to the daemon
over a Unix domain socket (or a Noise-encrypted TCP connection for remote peers) using a custom length-prefixed binary protocol.

```
┌──────────────┐    Unix socket     ┌──────────────┐    HTTP/SSE     ┌──────────────────────┐
│   choreo-tui     │◄──────────────────►│              │◄──────────────►│  OpenAI API          │
│  (terminal)  │                    │              │                ├──────────────────────┤
├──────────────┤                    │  choreographr  │◄──────────────►│  Anthropic Messages   │
│ choreo-gui       │◄──────────────────►│              │                ├──────────────────────┤
│ (desktop/mobile) │  Unix socket or    │              │◄──────────────►│  Google Gemini API    │
│                  │  TCP/Noise         │              │                │                      │
├──────────────┤                    │              │                ├──────────────────────┤
│   choreo-im     │◄──────────────────►│              │◄──────────────►│  Mistral API          │
│ (IM bridge)  │    Unix socket     │              │                ├──────────────────────┤
└──────────────┘                    └──────────────┘                └──────────────────────┘
                                                                    │  200+ OpenAI-compat   │
                                                                    │  providers via catalog│
                                                                    └──────────────────────┘
```

---

## Workspace topology

Twenty crates in one Cargo workspace (resolver = "3") — the root package plus
nineteen members. A crate's role, its binary, and its feature gating are
`README.md`'s crate table and that crate's own `//!` header; the root
`Cargo.toml` owns membership and the `default-members` policy (a bare
`cargo build` produces the daemon and the TUI, never the GUI or the bridge
crates). The layering below is the compile-time shape the table does not show.

### Dependency graph

The layering is one-directional and shallow: the leaf crates
(`choreo-shared`, `choreo-sanitize`, `choreo-image`, `choreo-markdown`,
`choreo-sockreg`, `choreo-power-events`, and `choreo-content`) have no
workspace dependencies; `choreo-proto` sits above `choreo-shared`; the
protocol, transport, keystore, and MCP crates sit above those; `choreo-daemon`
sits above almost all of them; and the root package depends on `choreo-daemon`
alone. The clients depend on `choreo-client-core` (with
`choreo-image`/`choreo-markdown` for the renderers), never on the daemon —
except `choreo-gui`, which also depends on `choreo-daemon` because it hosts an
embedded in-process daemon on iOS. The manifests are the authority for the
exact edges.

---

## Release & packaging

Releases are cut locally with `scripts/release.sh`, or on GitHub Actions by
pushing a `vX.Y.Z` tag: [`release.yml`](../.github/workflows/release.yml) runs
that script verbatim on the Linux, macOS, and FreeBSD jobs (the Android job
runs its sibling, the Windows job a hand-rolled zip build), so build flags,
`--locked`, feature selection, service-file staging, and tarball naming stay in
one place. A `workflow_dispatch` run builds everything but creates no release,
so the first real tag is never the pipeline's first test. Two choices follow
from shipped artifacts having to be reproducible rather than merely
release-built: the version is `[workspace.package] version` in the root
`Cargo.toml` (the single source of truth the Homebrew formula, AUR PKGBUILD,
and installer mirror), and every shipped build selects the workspace's
`[profile.dist]` rather than `[profile.release]`, on stable Rust through
`scripts/build-stable.sh`. Each target's **CPU floor** is passed as env
`RUSTFLAGS` — never `target-cpu=native`, never profile rustflags (they ignore
`--target` and are nightly-only) — and no alternative linker is added in CI;
the levels, the fleet reasoning, and both rationales are commented at each
build site.

[`RELEASE.md`](./RELEASE.md) is the phase-by-phase runbook, each script's
header comment is its own description, and
[`packaging/README.md`](../packaging/README.md) owns the per-asset inventory
together with the **installed, never auto-enabled** service policy.

---

## Wire protocol

Defines all shared message types and framing. The rustdoc in
`choreo-proto` owns the per-variant shapes, the envelope's reply and stream
axes, and the version history.

**Wire format:**

```
┌──────────────────┬────────────────────────────────────────┐
│ 4 bytes (BE u32) │ msgpack((protocol_version: u8, msg))   │
│   payload len    │                                        │
└──────────────────┴────────────────────────────────────────┘
```

- Protocol version: `9`, written by the encoder and gated by the decoder, so a
  mixed-version peer fails at decode. Each version's changes — and the
  policy that lets an **unreleased** version be amended in place — are
  `PROTOCOL_VERSION`'s rustdoc in `choreo-proto/src/frame.rs`.
- Max payload size: 64 MiB (`MAX_FRAME_SIZE`, enforced on encode and decode).

## Enrollment & transport trust

The TCP enrollment model is a deliberate ASYMMETRY — the SSH `known_hosts`
pattern, not symmetric TOFU: a client's transport key is provisioned into the
daemon's ACL by the operator out of band, while the server's key is learned
in-band on first contact, confirmed by a human, then pinned. The asymmetry
exists so the first-contact window can never observe the daemon's keystore
unlock key, which clients send inside `Unlock`; blind TOFU would hand a
first-contact MITM that key, and a symmetric out-of-band exchange would add
client-side file handling for no security gain.

**Decision rule:** a new peer is always provisioned by an operator who can
touch a trusted machine — never by the network. Any future feature that seems
to require in-band trust bootstrapping is a sign of trying to automate
operator trust; revisit the XX confirm UX before weakening the gate. A
daemon-as-client future reuses this shape: one keypair per daemon serving both
roles, enrollment operator-driven, headless confirmation via the base64
public-key flag (`--trust-pubkey`).

The value compared during first contact is the peer's **transport public key in
standard base64** — there is no separate fingerprint notion. An operator
obtains a key with `choreographr pubkey` on the machine that owns it; no
public-key file is stored, because the public half is derived from
`transport.sec` on every load. A client supplies a server's key directly for a
stateless/no-store-write connection with `--server-pubkey <base64>`.

Every mechanism behind that model — the ACL provisioning paths and their
advisory-lock write discipline, the parse-compare reload policy, the base64
public-key rendering, the first-contact gate that structurally holds back
every queued message (including auto-`Unlock`) until the human confirms, the
pin/unlock-key store's own hardening, the zeroizing of the plaintext frame
and the transport's staging buffer, and the atomic publish of the transport
secret (a sibling temp file, fsync, rename — `choreo-shared`'s `atomic_write`,
the same writer the catalog artifacts use) — is documented in-source
(`choreo-daemon`'s `server/acl`, `choreo-transport`'s `key` and `noise`,
`choreo-client-core`'s `known_servers` and `connection`, `choreo-tui`'s
`lib`).

## Provider Architecture

Provider support is layered across two crates, and the seam between them is
the point of the split. `choreo-ai-protocols` owns every wire protocol — the
`ProviderClient` trait, the `ChatTurnRequest` / `ChatTurnResult` turn types it
is expressed in, the chat adapters, and the static catalog. `choreo-daemon`
owns `InferenceProvider`, the only daemon type that dispatches by protocol: it
wraps a `dyn ProviderClient` together with the **catalog slug** (e.g.
`"opencode"` even when the client is an `OpenAiClient`) because catalog
lookups need the provider identity the client itself may report generically.
All wire-format knowledge stays in `choreo-ai-protocols`; the daemon supplies
the account-config, metrics, and session concerns at the boundary
(`choreo-daemon/src/providers/mod.rs` owns the facade's contract).

One contract crosses that boundary as a public surface in its own right:
**metrics**. The `endpoint` label is the **catalog slug**, not the protocol
name — more precise, but part of the public Prometheus contract, so renaming a
slug changes that provider's series. Error labels come from
`InferenceError::metric_label()` in `choreo-proto`, the single canonical
mapping shared by all providers.

### Provider auth: grant versus request material

Provider authentication is a **two-layer** model: the daemon resolves an account
to a `ProviderAuth` (an API-key grant, or the whole durable OAuth grant plus any
cached access token) and projects it into the `RequestAuth` a client consumes —
the bearer material and the credential-derived headers, nothing else. The grant
therefore never leaves the daemon, and `choreo-ai-protocols` gains no
`choreo-shared` dependency: only the already-derived request material crosses
that seam. Both layers, the projection that turns a not-yet-signed-in account
into a clean error, and the single header-derivation function are documented in
`choreo-daemon/src/providers/auth.rs`. A provider's auth kind is **derived** from
whether the catalog carries an `oauth` profile, and how the flow identifies
itself is a compiled per-slug policy that the overlay deliberately cannot
override (`choreo-ai-protocols/src/identity.rs`).

#### Provider OAuth login (frontend)

The interactive sign-in runs in the **frontend**, never the daemon: the shared
login library in `choreo-client-core` (`src/oauth/`) runs PKCE S256 with a random
`state` plus a loopback callback server (or an RFC 8628 device-code poll on
headless hosts) on a **worker thread**, streams progress as events over a
channel, and returns the `AddCredential` frame carrying the durable credential —
so no render crate constructs an OAuth credential and the credential's shape is
assembled in one place. Dropping the returned session handle cancels the flow.
The daemon serves each provider's sign-in parameters over a `ListOAuthProviders`
⇄ `OAuthProviders` pair assembled from the catalog `oauth` profile plus the
identity policy, so a frontend embeds no copy of the issuer, scopes, redirect
ports, encodings, or client id, and a borrowed vendor id is withheld unless the
per-install opt-in is on. The `id_token` claim decode, the
endpoint-security predicate, and the token-reply reader are shared with the
refresh path in `choreo-shared`, so login and refresh cannot drift.

#### Provider OAuth refresh (runtime)

The access token is refreshed on demand in the **session thread**: the blocking
exchange stays in `choreo-ai-protocols` (`oauth::refresh_oauth_token`, which
refuses to send the durable refresh token to anything but `https` or a loopback
address), while the daemon's command loop owns the token cache and is the sole
writer of the credential blob. `choreo-daemon/src/oauth_refresh/` is the seam —
an `OAuthTokenSource` the session implements over its command channel, plus one
single-flight driver. The wipe-on-drop refresh material, the proactive expiry
buffer, the one forced refresh on a `401`, the rotate-if-present rule, and the
terminal-versus-transient classification are documented there and in
`choreo-ai-protocols/src/oauth`; the per-credential lock that makes a concurrent
refresh re-read the winner's token is the **fourteenth** sanctioned shared-state
exception (see AGENTS.md). A terminal rejection quarantines the account
(`DaemonState::dead_oauth`) and broadcasts `OAuthReloginRequired` exactly once,
on the quarantine transition.

### Per-session cache affinity

Some OpenAI-compatible providers accept a **session identifier** in the request
body as a routing hint, so requests sharing it are more likely to reach the same
backend, whose prompt cache is then warm for their shared prefix. The hint's wire
shape is per provider, so it is modelled as a capability selected by the
`cache_affinity` scalar on that provider's `[provider.<slug>]` overlay entry
(`off`, `prompt-cache-key`, or `openrouter-session`). The value is always the
daemon's **real per-session id**, never a constant: a fixed key would pin every
session to one routing bucket and, on the models that bucket cache *accounting*
by the key, merely partition the accounting rather than improve routing. One
provider class is withheld outright — the **stateless Responses dialect**
documents no affinity field, and an unverified body field either 400s the turn or
is ignored — and the withholding is folded into the one provider-level
resolution both the cache-warming gate and the body builders read, so they cannot
disagree. The dialects, their model-generation semantics and their validation are
`choreo-ai-protocols/src/session_routing.rs`; the withholding and the layered
resolution are `openai/config.rs`.

#### Extended cache retention

A warm ping only helps within the provider's cache tier, and some providers
expose a longer one a request can ask for. Its wire shape varies across a
provider's model generations, so it is a **per-model** overlay scalar
(`cache_retention`) gated on the model's curated long TTL and never inferred from
a model-id string; the bundled overlay seeds no model. Values, gates, and each
dialect's restriction are `choreo-ai-protocols/src/cache_retention.rs`, with the
schema in `models-overlay.toml`.

#### Downgrade on a rejected field

The hint fails safe: a `(provider, model)` that answers a prompt-cache request
field with an HTTP **400 naming that very field** receives none of them for the
rest of the process, while a provider that merely ignores an unknown field never
downgrades. One registry (`cache_field_rejection::CACHE_FIELD_REJECTED`, the
**sixteenth** sanctioned shared-state exception) covers the **whole** prompt-cache
request surface — affinity and retention alike — and is keyed by the exact pair,
so a rejection on one model never withholds the hint from a sibling the provider
still honours. The detector's narrowness and the three-layer resolution
precedence (a per-account override outranks the catalog dialect, which outranks
the registry) are documented in that module and in `openai/config.rs`.

#### Cache warming reads both facts

Cache warming assumes its ping reaches a backend that **still holds the prefix**,
so with the hint withheld it could pay for a cache write the next real turn never
reads. The arming step therefore resolves the dialect the request bodies actually
send and declines to arm when the registry withheld it
(`SkipReason::AffinityUnavailable`, `choreo_cache_warm_skips_total{reason="affinity_unavailable"}`);
the same step resolves the retention tier so the ping schedule is computed
against the lifetime the cache really has. Both resolutions defer to the
resolvers the body builders use, so the ping can never disagree with what a
request sends (`choreo-daemon`'s `cache_warm::arming`).

### Provider-specific 400 handling

The retry layer consults the response body in exactly two provider-specific
cases, each gated tightly enough that the status-only "a 4xx is a deterministic
rejection" contract holds everywhere else. The opencode.ai zen/go gateway
validates message *content* on only some routes, so the identical bytes flip
`200/400/200` there; `retry_loop_with_provider_policies` resends the identical
body up to two more times when the slug is a known gateway slug — the same
exact-match set that gates the gateway routing headers, so the two cannot drift —
**and** the terminal `400`'s detail matches one of the gateway's own
content-rejection wordings or its opaque model-only stub (`retry/gateway.rs`).
Separately, a bare `400` naming no field that refuses a request over a tool call
would reach the operator with no cause attached, so the one funnel that turns a
provider error into an `InferenceError` appends a short clause (and tags one log
line with the matched family) for a known rejection wording
(`shared::TOOL_CALL_REJECTION_CLASSES`); an unrecognized body passes through
byte-identical, and nothing here feeds retries or any downgrade.

### Runtime catalog refresh (S4)

The merged catalog is parsed lazily into `PROVIDER_CATALOG`, a process-global
`LazyLock<ArcSwap<Vec<ProviderEntry>>>`: readers are lock-free, `replace_catalog()`
swaps the whole catalog atomically, and lookups therefore return *owned* values
cloned out of the atomic guard rather than `&'static` references. That
single-writer `ArcSwap` shape is the workspace's **fourth** sanctioned
shared-state exception (see AGENTS.md).

One maintenance thread (`choreo-daemon/src/catalog/`) owns the runtime pipeline —
the conditional GET against models.dev, the user-overlay reload, the attempt
cooldown — but never mutates the catalog: every change travels to the daemon
command loop, the sole writer of the `ArcSwap`, over a channel. The thread holds
**no copy of the base** between events, which leaves `catalog.bin`'s single
reader its single writer and makes the re-merge race-free. Because a provider
client bakes its catalog entry's protocol, base URL, compression coding, and
affinity dialect in at construction, a swap also invalidates — `DropProvider` —
only the sessions whose provider's *client-relevant* config changed or vanished,
so a routine model-list refresh leaves warm clients and their connection pools
alone. The pacing arithmetic, the cache/etag write ordering, the burst
coalescing, the fingerprint gate, and the `/refresh-models` and `CatalogUpdated`
wire variants are documented in-source (`choreo-daemon`'s `catalog` module and
`choreo-ai-protocols`'s `catalog::refresh` and `choreo-proto`).

### Client-facing MCP surfaces

`/mcp` shows the state of every MCP server visible to the session:
`ClientMessage::McpStatusRequest` is answered with one `McpServerStatus` per
server, and that record is what every frontend reads. It carries the slug, tier,
transport, target, connected state, tool count, the recorded last error, the
target-scoped keystore key (`mcp:<slug>|<target-hash>`), and an `McpAuthState`,
so a client derives its sign-in/sign-out action from the status alone instead of
holding a copy of the server's config. The daemon is the sole authority for that
key (one derivation feeds both provisioning and connect-time load, so the two
cannot drift), and the wire `oauth` block carries no literal secret: a
confidential client's `clientSecretEnv` names the environment variable that does.
A terminally rejected grant is quarantined, and the two surfaces treat it
differently on purpose — the connect-path snapshot withholds it, so a connect
surfaces a re-login rather than re-spending a spent token, while the status reads
the quarantine from the store and reports `Reauthentication`, so the server that
needs that re-login stays visible to the operator.

The requests beyond the status record are `McpReconnect` (rebuild one server — a
refreshed list on success, `McpReconnectFailed { slug, error }` otherwise),
`McpReload` (re-read the config and reconcile the running set with it —
`McpReloaded { summary, servers }` or `McpReloadFailed { error }`), and the
`McpTrust`/`McpUntrust`/`McpTrustList` trio that admits, revokes, or lists a
trusted project root. The connection thread translates each into a
`DaemonCommand`, because the command loop is the sole owner of the manager and
the tool catalogue. The variants and their replies are documented on
`choreo-proto`'s `ClientMessage`/`DaemonMessage`, the handling in
`choreo-daemon`'s `daemon::mcp_commands` and `mcp::trust`, and the user-facing
commands (`/mcp`, `/mcp reconnect`, `/mcp reload`, `/mcp trust`, and the
`choreographr mcp` CLI, whose offline `list`/`add`/`remove` edit `mcp.json` with
neither a daemon nor the `mcp` cargo feature) in the README.

**Remote-server OAuth** is the part that spans crates. The interactive PKCE
sign-in — discovery, dynamic client registration, loopback callback — belongs to
the frontend (`choreo-client-core`'s `oauth::mcp`), the durable
`ServiceCredential::OAuth { kind: Mcp }` is stored under the key above, and
connect-time refresh is `rmcp`'s own `AuthorizationManager`, driven through
`choreo-mcp`'s `auth` module, which keeps every `rmcp` type on its side of the
crate boundary and exposes only its own `CredentialBackend`. Two policies are
contractual rather than incidental: a load is **fail-closed** on the canonical
target and on the issuer, so a re-pointed server never receives a token minted
for another; and trust gates *use*, not existence — an untrusted project's
stored grant is neither loaded nor sent, yet survives un-trust and works again on
re-trust with no new login.

The daemon's backend reads the command-loop-published credential snapshot rather
than round-tripping the loop, which would deadlock a connect that runs while the
loop is blocked resolving a session overlay. Its writes are fire-and-forget
`DaemonCommand`s, and each one first advances that snapshot synchronously, so a
concurrent refresh of the same grant re-reads the rotation instead of spending an
already-rotated single-use token. That two-writer read view is the **fifteenth**
sanctioned shared-state exception (AGENTS.md): a single-key mutation publishes
only its own key, a whole-map republish is reserved for the two wholesale changes
(lock clears the set, unlock decrypts it afresh), and the slot is re-installable
so a restarted embedded daemon gets a fresh snapshot and its own command sender.
It carries no per-message data — only credential values the command loop already
owns.

### Reasoning round-trip (capture → carry → re-emit)

For Anthropic (thinking blocks + signatures), DeepSeek/Kimi (`reasoning_content`),
Gemini (thought signatures), and OpenAI/xAI Responses (opaque reasoning items +
`previous_response_id`), reasoning must be sent back on the next request or the
tool-call loop fails with a 400. The round-trip payload is an **opaque,
provider-owned artifact** whose custody is split four ways: the catalog decides
*how* to send (the per-model passback policy, else the protocol default, plus the
chat echo policy); each adapter captures the artifact verbatim at its parse
boundary and re-emits it verbatim in its own wire format; the daemon's request
builder (`choreo-daemon/src/reasoning.rs`) decides *whether* to send — derived,
never configured, from same-model provenance plus the passback policy plus the
`previous_response_id` continuity chain — and never interprets the bytes; and the
chat adapter's lowering decides *which wire key* carries it. The artifact never
leaves the daemon process: client-bound `SessionEvent`s carry it as `None`.

Whether a Responses route may chain is a **provider-level catalog fact**
(`responses_stateless`), read through the catalog rather than an adapter-side slug
list, and the catalog test `every_stateless_provider_replays_rather_than_chains`
fails the build if a model on such a provider resolves to
`ReasoningPassback::ResponseId` — the stateless dialect cannot chain, so it must
replay its captured items instead. The remaining rules (which key wins, the
structured `reasoning_details` array, the single reasoning byte budget shared by
capture and replay, and the empty-turn predicate) are the owning modules'
rustdoc: `catalog/entry.rs`, `openai/chat_projection`, and
`openai/reasoning_details`.

### Supported providers

The full provider list, its count, and the protocol each entry speaks are the
README's user-facing catalog overview. What stays here is a scope decision that
sits between the catalog data and the daemon's credential model — which known
providers the catalog deliberately does not offer:

> **Note — providers present in agent catalogs but deferred from the daemon catalog**
> (present in models.dev, catalogued for their model lists but with no API
> endpoint / no API-key path in the current catalog):
>
> | Provider(s) | Reason deferred |
> |---|---|
> | `amazon-bedrock`, `google-vertex`, `azure-foundry`, `azure-openai-responses` | Multi-field credentials (AWS keys/region, GCP service account, Azure resource + key). The daemon's single-API-key credential model cannot represent them yet. |
> | `copilot`/`copilot-acp`, `qwen-oauth`, `kimi-code` OAuth, `github-copilot` OAuth, `radius` | OAuth-only / dynamic-catalog auth — no static API-key path. The catalog carries an `oauth` profile mechanism and a per-slug client-identity table (see Provider auth above), and the provider OAuth **sign-in and refresh** flow ships for the `chatgpt` (Codex) and token-sharing `openai` routes; these remaining providers are added through the catalog `oauth` profile and the client-identity table as data. |
> | `cursor` | t3code subprocess driver (spawns the Cursor CLI), not a direct HTTP provider. |
>
> Adding the multi-field-credential providers requires a credential generalization (a `Fields` credential variant); the OAuth providers are added through the catalog `oauth` profile and the client-identity table, not a code change.

---

## Security model

### Local trust domain (the Unix socket)

The Unix-domain socket is the daemon's **local trust domain**: it carries no
Noise handshake, so any peer that reaches it is trusted exactly as the daemon
owner is — it can read session transcripts, retrieve plaintext credentials
while the keystore is unlocked (`GetCredential`), run tools, and enroll other
clients (`/acl add`, which is refused on the TCP/Noise listener). Transport
authentication cannot be what protects it; the operating system's file
protections are, in three layers: an owner-only (`0700`) parent directory, a
`0600` socket file narrowed immediately after `bind(2)`, and a Linux/Android
`SO_PEERCRED` peer-uid check that rejects a foreign uid (and a socket whose
credentials cannot be read) before a single message is read. A directory owned
by another user is refused outright — never merely warned about — because that
user could enumerate or swap the socket path; a *pre-existing shared* parent
(`$XDG_RUNTIME_DIR`, the platform temp dir) is never re-moded, since narrowing
it would be destructive.

Each layer's rationale, the bind-then-chmod window, the per-uid temp-dir
fallback (and why the path has one definition in
`choreo_shared::paths::default_socket_path`, delegated to by `choreo-proto`),
and the shared `choreo_shared::perm` policy behind the modes are documented
in-source (`choreo_shared`'s `perm`/`paths`, `choreo-daemon`'s
`server/lifecycle` and `server/connection`).

### On-disk state database

The `redb` store (`state.redb`, plus every `state.redb.bak-v*` pre-migration
snapshot) holds plaintext conversation history, tool output — which routinely
carries secrets — and image attachments, so it is protected by the filesystem
**independently of the keystore's AES-GCM credential blobs**, which are the only
encrypted-at-rest records in the suite. The same shared policy applies: the
*immediate* parent through `ensure_private_dir` (`0700`, foreign owner refused
fatally), the file itself forced to `0600` on every open path and after every
backup copy (a typical `redb` create honours the umask and lands `0644`). A
shared ancestor is never narrowed, and any tightening failure other than a
foreign-owned directory only warns — losing the daemon over a permission quirk
is worse than a visible log line.

The per-path decision (which directories are shared roots, the open/create/
recreate call sites, the backup copies) is documented in-source
(`choreo-daemon`'s `db` module).

### Lock/Unlock flow

Each daemon's credential keystore is governed by a keypair whose private half (the
**unlock key**) is held client-side, one per daemon, in the client's
`known_servers.toml`. The daemon stores only the public half as a **binding**, and
it starts **locked** with no credentials in memory. The contract between the two
halves is deliberately narrow: `ClientMessage::BindKeystore` is the ONLY path
that can create a binding — it ADOPTS the presented key on an unbound keystore
(TOFU, loud `KEYSTORE BOUND` log) and merely verifies it on a bound one — while
`Unlock` is strictly VERIFY-ONLY, answering `KeystoreUnbound` (deliberately
distinct from the bound-but-wrong-key `LockedError`) rather than ever adopting.
Binding is TOFU-once: no rotation is implemented, and re-binding a daemon whose
DB was reset is what the next connect's auto-bind does.

`AddCredential` carries the credential as a TYPED plaintext value and no key at
all, and requires an already-unlocked keystore (a locked daemon refuses it rather
than unlocking implicitly); the daemon encrypts each credential to its own
binding before persisting, so provisioning and refresh rotation share one
blob-write path that never needs the private key. Because the client records a
per-daemon unlock key only after a targeted confirmation (except the auto-bind
key, which must be recorded pre-send), one ordering guarantee crosses both
halves: **the targeted reply to `Unlock`/`BindKeystore` reaches the acting
client's writer queue before the lock-state transition broadcast**, so the
broadcast can never overtake it.

**Multi-client provisioning** follows from those rules: the first client to
reach a fresh (unbound) daemon auto-binds it with a freshly minted key, and any
other client stays locked until that key is shared out of band (added to its
`known_servers.toml`, or supplied with `/unlock <base64-key>`).

The per-side mechanics — the `unlock_tail`/`finish_unlock` ordering that leaves a
failed unlock's plaintext unpublished, the broadcast-to-all-subscribers and
send-on-subscribe `Keystore { state }` pushes, the write-free `/unlock` key path
and pre-send bind record, the `known_servers.toml` hardening, and the lazily
rebuilt per-session providers — are documented in-source (`choreo-daemon`'s
`daemon/keystore` and `daemon/subscriber_handlers`, `choreo-client-core`'s
`credentials` and `known_servers`, `choreo-keystore`'s `crypto`).

### Dependency supply chain

The workspace ships Rust that must trust its third-party dependencies, so the
dependency graph is part of the security surface: the 2026-08-20 `arrayref`
republishing showed a trusted crate can turn hostile between resolutions.
Three repository controls make a repeat fail loudly instead of landing
silently — `Cargo.lock` with `--locked` (a silent re-resolution against the
live registry fails the command), `deny.toml` (the attacker's versions banned
by name, a crates.io-only source restriction, scoped advisories), and
`scripts/check-supply-chain.sh` (the cache scan for the DELETED `.crate` files
no graph tool can see anymore, then the policy check, with a `cargo-audit`
fallback) — and both tools read the shared RustSec advisory database.
`deny.toml` and that script are the authoritative ban list and their headers
carry the incident rationale. The strongest remaining option — bit-for-bit
reproducible builds from a checked-in `cargo vendor` snapshot — is
intentionally not enabled (repository size).

`rmcp`, the MCP path's heaviest dependency, is pinned, limited to the
transports it needs (`default-features = false`), and the whole tree resolves
a single `reqwest` major; `deny.toml` keeps `multiple-versions = "warn"` so a
future divergence is reported rather than silently duplicated.

### MCP client trust boundary

The MCP client treats every server as untrusted and every server-supplied string as
data, never as instructions or configuration. The boundary is enforced at four
sites, each documented where it lives: schema bounding and catalogue caps in
`choreo-mcp`'s `protocol`, header/transport discipline and capability honesty in
its `engine`, process safety and environment hygiene in its `stdio` (with the
injection-variable list shared with the shell tool through `choreo-sanitize`'s
`child_env`), and least privilege in the daemon, where `ToolPolicy::Mobile`
registers no shell, sandbox, or MCP group at all.

Two consequences belong to the system view rather than to any one module. An MCP
server runs with the daemon user's authority, so the daemon's environment is NOT
a secret channel: the code-injection variables are stripped but everything else is
inherited, and anything a server needs must be set through that server's own
`env`. And credentials are per-server: a static token in `headers`, or a
keystore-stored OAuth grant keyed to the canonical target (`choreo-mcp`'s
`auth::keying`), so a re-pointed project `.mcp.json` cannot be handed an existing
grant.

---

## Tool system

The daemon's tool machinery — the typed `Tool` trait, its type-erased `ToolDyn`
counterpart, the `ToolRegistry` that owns and dispatches every tool, and the
`define_tool!` macro — is documented in-source (see `choreo-daemon`'s `tools`
module, `cargo doc -p choreo-daemon` / `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

Three dispatch paths cross the tool boundary, and the split is a contract, not
an implementation detail: two JSON paths (`ToolDyn::execute_json` and
`execute_streaming_json`) serve LLM tool calls and return a `ToolOutput` rendered
as `Text` (human/LLM) or `Json` (Programmatic Tool Calling), while
`ToolDyn::execute_postcard` serves RISC-V VM guest ecalls and answers in postcard
bytes — the guest↔host wire format described under "Postcard binary encoding"
below. A tool's `Error` type stays structured per tool so a guest can match on its
own variants (`DbError::NotFound`, `HttpError::InvalidUrl`, …), and a tool that
produces an image carries it on its typed return value, drained by the request
worker over an out-of-band multi-message channel (an MCP result may carry several
image blocks) rather than embedded in the response struct.

### Tool output sanitization and bounding

Tool output is defended in three layers at once — at the **source** (the tool
that produces the bytes), at the **transcript** (what the model sees on the next
call), and at the **sink** (the terminal that renders it) — and all three sit on
one Unicode policy and one byte budget so they cannot drift. `choreo-sanitize`
owns those shared primitives; each layer's per-tool application, including the
ordering rule that keeps a critical tail alive through the transcript's
post-sanitize re-cap, is documented where it lives (`choreo-daemon`'s
`tools/sanitize.rs` and its transcript choke point, `choreo-tui`'s
`text_filter`).

**Content is capped; delivery is bounded separately.** The byte budget caps one
tool's content at the source and in every client's live accumulation, while the
lossless delivery design separately bounds the per-client in-flight bytes that
trigger lag-eviction (see the `choreo-daemon` `broadcast` module docs). The two
do not collide — content is capped at the source, delivery per client queue.

### PDF tools

`pdf_classify` and `pdf_to_markdown` wrap `pdf-inspector` (Firecrawl), a
pure-Rust, extraction-only parser built on `lopdf`: with no JavaScript engine
and no page rendering, the classic PDF malware *execution* vectors are excluded
by construction. Input gating, the untrusted content framing, and the
containment story are documented in-source (`tools/pdf/`). The dependency's
RUSTSEC-2026-0187 fix (`lopdf >= 0.42`'s nesting cap, with the regression guard
`nested_array_poc_does_not_abort_process` still in
`tests/it/pdf_tool_integration.rs`), the `pdf` feature's placement in `default`,
and the iOS embedded build's opt-out are explained in
`choreo-daemon/Cargo.toml`.

### Postcard binary encoding

Tools reach the RISC-V sandbox over a `postcard` binary protocol. The frame
shapes, the args encoding, and the nested `Result<Result<R, E>, ToolError>`
return encoding — the outer layer for infrastructure failures (arg
deserialisation), the inner for the tool's own error type, so a guest can
pattern-match its variants — are documented in-source (`choreo-daemon`'s `tools`
module and `tools/vm.rs`).

### Tool groups

Tools are grouped to reduce context overhead, and each declares its group via
`Tool::group`. The group catalog — each group's name, its model-facing
description, and its cargo-feature gating — is in-source (`choreo-daemon`'s
`tools` module), as is the default active set (`core`, `git`, `shell`) with the
session-config default in `sessions`. The system prompt lists every group and
its description; the model activates and deactivates them with
`load_tools`/`unload_tools`, and **core** cannot be unloaded.

Groups affect only tool **availability** in the API `tools` array — they are a
discovery mechanism, not access control. The RISC-V VM (`run_riscv`) reaches
every registered tool regardless of group state.

### Concurrent tool dispatch

Each response's tool calls are partitioned into a **serial (session-config)**
phase — `load_tools`, `unload_tools`, and `set_working_dir` — and a
**concurrent** phase holding every remaining tool (shell, filesystem, VM, HTTP,
Git, `spawn_subsession`, …) whose execution is independent of session state. The
typed `PendingConfigChange` capture, the `is_session_config_tool` predicate, and
the Phase-3 worker-copy mirror that carries successful mutations onto the
request worker's config copy live in `requests/config_tools.rs`; the dispatch
and execution machinery — thread topology, per-tool timeout resolution, the wait
loops, the streaming forwarder, and per-tool result recording — lives in
`requests/tool_execution.rs`.

Three properties hold across both phases however the races fall. Cancellation is
fully event-driven (every wait is a `select_biased!` that lists the cancel arm
first, over the result channel, the cancel channel, and an exact
`after(remaining)` timer — never a poll loop), and a cancel that wins a race
still **drains** an already-completed result rather than discarding it, so the
transcript keeps the tool's real output while the request still stops on the
sticky flag. A cancel observed by the concurrent collector sends every
still-running wait-loop a per-tool kill and keeps draining until all batch
handles have arrived, so each unfinished call records a deterministic outcome
and the placeholder sweep (`mark_unexecuted_tool_results`) stays only a safety
net for a wait-loop that dies before delivering. Tool results are always
rendered in the model's original call order — placeholders are seeded in call
order and filled **in place by `call_id`** — and a tool thread that panics is
caught and reported as an error `ToolOutput` instead of crashing the daemon.

Both phases share a `ToolContext.cancelled` flag with the running tool so a tool
that consults it can stop early — the one sanctioned shared-state exception to
the repo's channel-only thread-communication rule (see AGENTS.md).

### spawn_subsession

`spawn_subsession` is a core-group tool on the concurrent dispatch path: it asks
the daemon command loop for a child session (`parent_session_id` recorded;
working directory, model, and tool groups inherited), drives the child's own
agent loop to completion under the daemon-wide `max_turns` cap, and returns the
child's assistant text. Its contract is in-source (`choreo-daemon`'s
`tools/subsession.rs`). The daemon tracks the parent→child relationship
(`DaemonState::children`) so a parent's cancellation, exit, or deletion also
stops its children — see `cancel_children_of` and the cascade-deletion docs —
and the TUI's navigation round trip is documented in `choreo-tui`'s
`state/subsession.rs`.

---

## Session architecture

Sessions are persisted to a `redb` embedded key-value store at
`~/.local/share/choreographr/state.redb`, and each active session's in-memory state
is a `SessionState` owned by its control thread with the persisted fields extracted
into `SessionConfig`. The table schema (every key and value codec), the
`SessionState`/`SessionConfig` split with every field's rationale, and the store's
owner-only filesystem permissions are documented in-source (see `choreo-daemon`'s
`db` and `sessions` modules, `cargo doc -p choreo-daemon` / `just doc`); the
permissions themselves are summarized under **On-disk state database** in the
Security model.

### Turn durability

Each `Turn` is written when the **agent-loop iteration that produced it**
completes, not when the request does: a request is not one turn, and its tool
loop can iterate unbounded for tens of minutes, so the request is far too coarse
a durability unit. The iteration boundary is the first moment a turn is complete
(assistant message plus one recorded result per call) and it precedes the next
provider round-trip, so a checkpoint there bounds the crash window to the single
turn in flight. The worker builds its turns on a private `SessionState` clone
and the session thread's merge at `RequestFinished` stays the authoritative
final write; the checkpoint itself, that merge's `persisted_turns` re-write
skip, and the swallow-and-retry handling of a failed checkpoint are documented
in-source (`choreo-daemon`'s `requests/tool_execution.rs`, `sessions/state.rs`,
and `sessions/handlers/inference.rs`).

Three invariants hold around that boundary:

- **A turn is written whole or not at all.** One `write_turn` transaction covers
  the zstd-compressed blob and every attachment slot, so a reader can never see a
  turn with some of its images. Image bytes are additionally written per emit
  (see `emit_image`) because a client fetches them by key the moment the
  turn's `TurnAppended` arrives.
- **A committed turn is durable.** `redb` write transactions default to
  `Durability::Immediate` and the daemon never lowers it, so a committed turn has
  been fsynced (see the `db` module docs).
- **Every request ending persists.** Completion, inference failure, and
  cancellation all finalize the turn and route the worker's snapshot through
  `RequestFinished`, so a graceful shutdown (which cancels in-flight requests)
  loses nothing; only process death, which cannot cancel, skips the paths that
  would have written.

**Crash-recovery skew.** Durability covers the turn rows, not every field of the
worker's `SessionState`: the worker-owned fields (`last_response_id`, the
continuation-chain head, and `accumulated_usage`, the billing total) change on
that private clone and merge only at `RequestFinished`, so after a mid-request
crash the on-disk turns can be newer than they are — config a tool changes
through the session's own commands is persisted when the command is applied, so
only the worker-owned fields lag. On restart the session is rebuilt from the
turn rows: `next_turn_id` comes from the maximum turn id (never the record's
advisory `turn_count`), the billing total and context-size readout are
re-derived from the per-turn usage those rows carry, and the stale
`last_response_id` is restored only when its producing provider+model still
matches — a `ResponseId` provider then continues from that older head with the
checkpointed turns re-sent as new input. That is the accepted cost of turn-level
durability: the conversation itself is never lost.

### Schema versioning & migrations

The `meta` table holds the persisted schema version, and the daemon runs the
migration chain immediately after opening the database on every startup. The
policy — an **additive** change needs no migration, a **breaking** one gets a
numbered migration and a `SCHEMA_VERSION` bump — and the contract every entry
must satisfy (a chain contiguous over `1..SCHEMA_VERSION`, one write transaction
per step, idempotent under re-run, old record shapes decoded through frozen
local copies), with the pre-migration `bak-v{from}` backup and the downgrade
refusal, are documented in-source (`choreo-daemon`'s `db` module).

### Undo/Redo

`/undo` and `/redo` mutate turn history through the `undone` flag on each `Turn`
(the struct itself is documented in-source in `choreo-proto`; the daemon strips the
opaque reasoning payload from every client-bound `DaemonMessage`, and the builder's
same-model provenance check drops it after a mid-session model switch — see
**Reasoning round-trip**). Two contracts govern the pair:

- **Redo is LIFO-symmetric with undo.** A single `/undo` hides the most recent user
  turn together with every still-live turn after it (a tool-loop iteration creates
  several turns per user turn) and records only the turns it actually flipped as
  one **redo batch**; `/redo` pops the newest batch and restores exactly it, so one
  redo re-applies one undo and never collapses several pending undos at once. A new
  user turn clears the stack, and undo also clears the persisted `last_response_id`
  so a chained request cannot leak the hidden turns back into the model.
- **Both commands require an idle session.** An in-flight request worker holds the
  turn history on a private clone and merges it back at `RequestFinished`, so a
  history mutation underneath it would race that merge (and could resurrect the
  cleared chain). The daemon refuses a busy session with
  `Failed { error: "session is not idle" }`; the TUI applies the same idle check
  client-side to skip the doomed round-trip.

### Cancellation and connection accounting

Two sanctioned shared-state exceptions (see AGENTS.md) underpin session
cancellation and the daemon's connection accounting. Each session owns a
`choreo-sockreg` registry (the **seventh** exception) — plus one daemon-level
registry for non-session-scoped work (prefetch, catalog fetch) — so the command
loop can force-close a wedged session's provider sockets from the thread that
decided the cancel: `shutdown(fd, SHUT_RDWR)` is the only way to un-block a
worker blocked in a provider `read()`, and the registry's lock is held only for
a handful of fd syscalls. The daemon's live-connection counter (the **third**
exception, released by the RAII `ConnectionSlot`) enforces
`MAX_CONCURRENT_CONNECTIONS` atomically across every accept path — a bound no
channel can express without a dedicated accounting thread. Both rationales live
on the types (`DaemonState::session_registries`, `server/lifecycle.rs`).

---

## Metrics / OpenMetrics monitoring

`--metrics-addr <ADDR>` (e.g. `127.0.0.1:9464`) starts a dedicated HTTP thread
serving `GET /metrics` in the OpenMetrics format
(`text/plain; version=0.0.4; charset=utf-8`); without the flag no metrics
server runs. The machinery is behind the `metrics` cargo feature, **off by
default** at both the `choreo-daemon` crate and the root `choreographr`
package: a plain build drops the `prometheus`/`tiny_http` dependencies and
degrades the module's public API to inert no-op stubs, so the daemon's
instrumentation call sites compile unchanged. Release binaries enable it
explicitly; a feature-off build still accepts `--metrics-addr` but refuses to
start rather than silently ignoring the requested endpoint.

The full metric set — names, types, labels, and help text — is the `metrics`
module's rustdoc, and `cargo test-lean` (the feature-off unit run) is what
keeps the stub and refusal paths compiled.

---

## Data flow: a prompt from input to response

A client parses the input line and frames a `ClientMessage::RunInput` carrying
a per-connection request id; its writer task sends it over the socket. The
daemon's connection thread turns it into a session command, and the session
thread assigns the run's `stream_id` (a per-session, monotonic counter — the
client never chooses one), accepting with BOTH a targeted reply and the
identical `Started` broadcast, so every subscriber learns the run's ids and the
turn it is streaming into. The request worker builds the message array from the
session's history, drives the provider client, and broadcasts each SSE event as
a `SessionEvent` on the stream axis; the client routes chunks by the
`stream_id` it learned from `Started` and re-renders as they arrive. A final
`Done` closes the run, and the finished turn reaches clients as one
`TurnAppended` broadcast. The payload shapes are `choreo-proto`'s; the loop is
`choreo-daemon`'s `requests`.

### Broadcast fan-out (lossless delivery, lag eviction)

All three subscriber fan-outs — the all-activity broadcast, the summary
broadcast, and the per-session broadcast — share ONE policy through
`crate::broadcast::SubscriberSink`: unbounded per-client queues, so the daemon
never drops a broadcast message and a slow subscriber can never stall the
command loop or a session thread, with memory bounded by lag-based eviction
instead of drops. The policy, `LagLimits`, the counter-balance invariant, and
the solicited-`Image` carve-out are that module's docs.

Two facts cross layer lines. **Eviction needs no daemon-held socket handle**:
each connection's writer gets a socket write timeout, so a wedged client cannot
stall its writer forever — the write fails, the writer shuts the socket down
itself, and the reader's blocking read unblocks into the normal teardown. And
**evictions are not silent**: each increments `choreo_evictions_total`, served
on `/metrics`, so a permanently wedged subscriber stays observable.

Four sanctioned shared-state exceptions sit behind this design — the
delivery-lag byte counters (#6), `ClientId` (#10), the log-directory test
override (#11), and the per-file mutation locks (#12). Each is stated in
AGENTS.md with its rationale on its own type docs.

---

## Design decisions

1. **Unix sockets, not HTTP for client↔daemon** — keeps everything local,
   avoids port conflicts, and hands access control to the operating system
   (see **Local trust domain** above); the encrypted TCP/Noise listener exists
   only for remote peers.

2. **Binary protocol (MessagePack, named mode), not JSON** — self-describing,
   compact, and typed, over a length-prefixed frame (so framing is unambiguous)
   whose version gate rejects a mixed-version peer at decode rather than
   letting it misread a payload.

3. **Sessions, not per-client state** — a session owns its model, working
   directory, and messages, and is independent of the connections watching it;
   clients subscribe and unsubscribe, and every lifecycle operation (create,
   attach, list, delete) works while the daemon is locked, because credentials
   are only needed to run a model. A session carries an optional `account_name`
   that decides which credential a request uses, so there is no global
   "default account" fallback, and its provider is resolved lazily from the
   daemon's provider registry rather than pinned at creation.

4. **SSE streaming is hand-written, not a library** — a reader per wire dialect
   (OpenAI `data:` lines plus `[DONE]`, Anthropic's `event:`/`data:` pairs)
   keeps parsing and buffering ours. Three properties are contractual: the
   blocking socket read never runs on the consumer's thread, so events,
   cancellation, and the deadline are all observable at once; a stream is
   bounded in **three** independent ways (one event's accumulated bytes, a
   per-attempt wall-clock deadline that fires even on a keep-alive trickle, and
   a socket-level idle timeout that resets per byte and so never cuts a steady
   stream short); and deadline expiry is its own non-retryable error, distinct
   from a socket `Io` failure. Losing any of the three is what lets a gateway,
   a proxy, or a silent provider wedge an inference worker.

5. **Markdown as the intermediate format** — all text (tool output, assistant
   text, error messages) is treated as markdown and rendered as HTML (desktop)
   or shaped to terminal output (`choreo-tui`), so one rendering layer serves
   every client.

6. **One canonical message history, many wire shapes** — both OpenAI Chat
   Completions and the Responses API are first-class, resolved per model
   (`RequestFormat`, from the catalog's `openai_responses` flag with
   `default_request_format` as the fallback), but there is **one canonical
   `messages` history** that each adapter lowers into its own wire shape, so a
   history stays sendable across a model switch or a restored session instead
   of being bound to the format that produced it.

   One fact crosses all of them: a tool call's **caller attribution** is
   carried, never interpreted — captured by the adapter, persisted on the turn,
   copied back **unchanged** onto the call and the output answering it by the
   rebuilt request, and stripped by the chat projection, whose wire has no such
   field. The value is typed in `choreo-proto`, which sits below
   `choreo-ai-protocols` and so cannot name the adapter's caller type.

7. **OS threads with a sidecar async runtime** — the daemon avoids async Rust
   everywhere except where third-party libraries (alloy, subxt, rmcp) require
   it, and those live in their own crates (`choreo-blockchain`,
   `choreo-content`, `choreo-mcp`), each owning a global
   `OnceLock<tokio::runtime::Runtime>` as a sidecar and running its clients via
   `block_on()`. The daemon links those crates only behind the `blockchain`,
   `content`, and `mcp` cargo features and calls their synchronous entry
   points, so tokio is never a direct dependency of the daemon. What each crate
   must therefore own is the bound the daemon cannot supply: a synchronous
   `block_on` cannot be interrupted, so the daemon's tool timeout can only
   *abandon* the execution thread and it is the crate-level cap that turns a
   black-holed endpoint into a clean error instead of a leaked thread (see
   `choreo-blockchain`'s `RPC_TIMEOUT` docs). Keeping the daemon thread-only
   simplifies the mental model (each thread owns its data, no `Send` bounds on
   shared state, no `Pin<Box<dyn Future>>`), improves stack traces, and avoids
   the complexity of async cancellation.

---

## Testing

Each crate's suite lives beside the crate it tests, and the per-layer inventory
is that crate's own business. The authoring contract — the unit/integration
split, no time-based waits in unit tests, one `tests/it` binary per crate — is
AGENTS.md → Test Discipline; the nextest profile and the cargo aliases carry
their own rationales (`.config/nextest.toml`, `.cargo/config.toml`). One
placement rule spans the workspace: an integration test that spawns a binary
via `env!("CARGO_BIN_EXE_...")` must live in the crate that owns that binary.
`choreo-daemon/tests/it/common/` is the shared harness that boots the real
`run_server` over both transports and drives it with the real client library;
its module docs own the details.
