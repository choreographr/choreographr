# MCP OAuth conformance + confidential-client support

Status: **in progress**. This plan closes the remaining MCP authorization gaps so
Choreographr can sign in to the full range of remote MCP servers — including
servers that delegate to an external identity provider (Shopify Customer
Accounts, Microsoft Entra ID, Keycloak realms, Google) and servers that only
advertise their authorization server via the `WWW-Authenticate` challenge.

**Lifecycle:** delete this file once every phase below has landed. Nothing
written during implementation may reference it — rustdoc, `ARCHITECTURE.md`,
`README.md`, release notes, and commit messages must stand on their own.

## Problem

Choreographr's interactive MCP login (`choreo-client-core/src/oauth/mcp/`) is a
**public-client, PKCE-only, path-probing** flow. That reaches MCP-native servers
(Linear, Sentry, Notion, Atlassian — all advertise `token_endpoint_auth_methods_supported`
including `none` and a Dynamic Client Registration endpoint) but fails along two
independent axes:

1. **Client authentication.** `authorize.rs::exchange_code` sends no client
   credential. An authorization server whose metadata omits `none` from
   `token_endpoint_auth_methods_supported` requires `client_secret_basic`/`post`
   (a confidential client), so the token exchange is rejected.
   Verified failing target: **Shopify Customer Accounts** —
   `token_endpoint_auth_methods_supported: ["client_secret_basic","client_secret_post"]`.

2. **Discovery location.** `discovery.rs` probes
   `*.well-known/oauth-protected-resource[...]` then
   `{origin}/.well-known/{oauth-authorization-server,openid-configuration}`. It
   cannot resolve (a) a **path-issuer** authorization server discovered via the
   OpenID Connect **appended** convention
   (`{issuer}/.well-known/openid-configuration`), nor (b) a server that
   advertises its metadata URL only in the `401` `WWW-Authenticate` header
   (RFC 9728 §5.1). Verified failing target: **Microsoft Entra ID**
   (`https://login.microsoftonline.com/{tenant}/v2.0/.well-known/openid-configuration`
   returns 200; the path-inserted guess returns 404).

## What the MCP spec requires

Per the MCP authorization specification (2025-06-18 and the current draft):

- MCP clients **MUST** use OAuth 2.0 Protected Resource Metadata (RFC 9728) for
  authorization-server discovery, and **MUST** parse `WWW-Authenticate` on a
  `401` (RFC 9728 §5.1).
- Authorization servers **MUST** provide **either** RFC 8414 **or** OpenID
  Connect Discovery 1.0 metadata; MCP clients **MUST** support **both**.
- Authorization servers **MUST** implement OAuth 2.1 for **both confidential and
  public clients**. For an AS without DCR, a client hardcodes a client id (and,
  if applicable, client credentials) or presents a UI.

Choreographr's own conformance baseline
(`choreo-mcp/tests/conformance/expected-failures.yml`) already records the
related failing scenarios (`auth/metadata-var3`, `auth/basic-cimd`,
`auth/scope-from-www-authenticate`, `auth/token-endpoint-auth-*`, `auth/iss-*`).

## Design

### Config / wire: two new optional `oauth` fields

Extend the `oauth` block (both `choreo_mcp::McpOAuthConfig` and
`choreo_proto::McpOAuthConfig`):

- `authorizationServer` (`authorization_server: Option<String>`) — an explicit
  authorization-server issuer URL. When set, discovery uses it directly (the RFC
  8414 path-inserted, OIDC appended, and root candidates), skipping
  protected-resource probing. This is the cheap, deterministic fix for an AS
  that is not reachable via the resource's well-known paths.
- `clientSecretEnv` (`client_secret_env: Option<String>`) — the **name** of an
  environment variable holding a confidential client's secret. Never a literal
  secret (the existing policy: a secret comes from the environment, never a
  repo-controlled file). The wire carries only the name; the frontend resolves
  the value from its own environment.

`clientId`, `callbackPort`, `scopes` are unchanged. The `oauth` block still
carries no literal secret.

### Discovery (`choreo-client-core/src/oauth/mcp/discovery.rs`)

- If `authorizationServer` is configured, use it as the authorization server
  directly (no protected-resource fetch).
- Otherwise, unchanged: protected-resource candidates → first
  `authorization_servers` entry → else origin.
- Authorization-server metadata candidates, in order: RFC 8414 path-inserted,
  OIDC path-inserted, **OIDC appended (`{issuer}/.well-known/openid-configuration`)**,
  RFC 8414 root, OIDC root. Deduplicated.
- Keep the RFC 8414 §3.3 issuer-vs-URL validation for every fetched document.
- Record `authorization_response_iss_parameter_supported` from the metadata.

### Client secret + `iss` validation (`authorize.rs`, `mod.rs`, `http.rs`)

- `run()` resolves `client_secret_env` from the process environment into a
  wipe-on-drop secret (absent env var → no secret; a `clientSecretEnv` that is
  set but unset in the environment is a login error naming the variable).
- `exchange_code` sends `client_secret` (client_secret_post) when a secret is
  present.
- `finish()` stores the secret in `ServiceCredential::OAuth.client_secret`, so
  connect-time refresh — which already calls
  `OAuthClientConfig::with_client_secret` — keeps working.
- `iss` validation (RFC 9207): when the AS advertises
  `authorization_response_iss_parameter_supported: true`, require the callback
  `iss` and compare it to the recorded issuer; otherwise proceed.

Scope-from-`WWW-Authenticate` and CIMD are out of scope for this pass and
remain baselined.

## Phases

1. **Proto + engine + daemon config** — add the two fields to the wire and engine
   types, parse them in `choreo-daemon`'s `OAuthEntry`, and carry them through
   `status_auth`. Unit + round-trip tests.
2. **client-core login** — discovery (AS override + OIDC appended), client
   secret at the token endpoint, stored secret, `iss` validation. Unit tests.
3. **Docs + integration test** — README `oauth` block fields, ARCHITECTURE
   discovery paragraph, and a confidential mock authorization server in
   `choreo-daemon/tests/it/mcp_oauth.rs` proving a PKCE-only exchange is
   rejected and a `client_secret_post` exchange succeeds.

## Acceptance

- Shopify Customer Accounts MCP: `mcp login <slug>` completes against an app
  configured with `authorizationServer: "https://shopify.com/authentication/<id>"`,
  `clientId`, and `clientSecretEnv`; the customer tools then authenticate.
- Entra-backed MCP: discovery resolves the appended OIDC metadata.
- Linear/Sentry/Notion: unchanged (public client, DCR) — regression-covered.
