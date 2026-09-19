#!/usr/bin/env bash
# scripts/check-crates-io-token.sh — confirm the stored crates.io API token
# actually authenticates, as a RELEASE preflight.
#
# Why this exists: Phase 2 publishes the workspace in dependency-closed batches
# (`RELEASE.md`), and a missing/expired/revoked/wrong-scope token fails the
# *upload* — after the earlier crates have already published — with
# `403 Forbidden: authentication failed`. Catching that before Phase 1 needs a
# token-authenticated request.
#
# The obvious check — `GET /api/v1/me` with `Authorization: <token>` — has been
# broken for years and can NEVER return 200: crates.io declares that endpoint
# `AuthCheck::only_cookie()` (only cookie/web-session auth is allowed; API
# tokens are explicitly rejected — rust-lang/crates.io#3518, 2021). The endpoint
# still *authenticates* the token before refusing it, though, so its error body
# distinguishes a real token from a bad one:
#
#   * valid token              -> 403 `this action can only be performed on the
#                                      crates.io website`
#   * well-formed but unknown  -> 403 `authentication failed`
#   * malformed / pre-2020     -> 401 `... does not match the format used by
#                                      crates.io ...`
#
# So "the token is good" == HTTP 200 (if crates.io ever re-allows tokens here)
# OR the specific website-only 403. Anything else fails. There is no other
# read-only, token-authenticated endpoint to use: crates.io's own token tests
# were moved onto the publish endpoint for exactly this reason
# (rust-lang/crates.io#11357, 2025).
#
# A custom User-Agent is REQUIRED: crates.io's Fastly edge answers requests
# sending curl's default UA with an HTML 403 (content-type text/html), which
# would otherwise masquerade as an auth failure.
#
# Read-only; wired into `just check-crates-io-token` and `just pre-release`.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="https://crates.io/api/v1/me"
USER_AGENT="choreographr-release-preflight (https://choreographr.com)"
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"

# Pull `token` out of a cargo credentials file's `[registry]` section only —
# `[registries.<name>]` sections (alternate registries) must not be picked up.
read_registry_token() {
    awk '
        /^[[:space:]]*\[/ {
            line = $0
            gsub(/[[:space:]]/, "", line)
            in_registry = (line == "[registry]")
            next
        }
        in_registry && $0 ~ /^[[:space:]]*token[[:space:]]*=/ {
            line = $0
            sub(/^[[:space:]]*token[[:space:]]*=[[:space:]]*/, "", line)
            gsub(/^"|"$/, "", line)
            sub(/[[:space:]]*$/, "", line)
            print line
            exit
        }
    ' "$1"
}

# Resolve the token the same way cargo does: the `CARGO_REGISTRY_TOKEN`
# environment variable wins, then the credentials file (`.toml` is the modern
# name; the bare `credentials` file is the legacy spelling).
token="${CARGO_REGISTRY_TOKEN:-}"
if [ -z "$token" ]; then
    for f in "$CARGO_HOME_DIR/credentials.toml" "$CARGO_HOME_DIR/credentials"; do
        [ -f "$f" ] || continue
        token="$(read_registry_token "$f")"
        [ -n "$token" ] && break
    done
fi

if [ -z "$token" ]; then
    echo "error: no crates.io token found (checked \$CARGO_REGISTRY_TOKEN and" >&2
    echo "       $CARGO_HOME_DIR/credentials[.toml]). Mint one with the" >&2
    echo "       publish-new + publish-update scopes at https://crates.io/settings/tokens" >&2
    echo "       then store it:  cargo login" >&2
    exit 1
fi

# Capture the body and the status code together (body first, code on its own
# trailing line). `|| true` lets us report a clean error on a connection failure
# instead of tripping `set -e`.
response="$(curl -sS -m 20 -A "$USER_AGENT" -H "Authorization: $token" \
    -w $'\n%{http_code}' "$API_URL" 2>/dev/null || true)"
code="${response##*$'\n'}"
body="${response%$'\n'*}"

# Success: the token authenticated. Either the endpoint accepted it outright
# (200) or, as today, it refused the *endpoint* while proving the *token*
# (the website-only 403). Both mean the credential is valid.
if [ "$code" = "200" ]; then
    echo "OK: crates.io token is valid (HTTP 200)"
    exit 0
fi
if [ "$code" = "403" ] && printf '%s' "$body" | grep -qF "can only be performed on the crates.io website"; then
    echo "OK: crates.io token is valid (authenticated; /api/v1/me is website-only)"
    exit 0
fi

echo "error: crates.io token check failed (HTTP ${code:-<none>})" >&2
if [ -n "$body" ]; then
    echo "       $body" >&2
fi
echo "       The token is missing, expired, revoked, or under-scoped." >&2
echo "       Mint a new one (publish-new + publish-update scopes) at" >&2
echo "       https://crates.io/settings/tokens and store it:  cargo login" >&2
exit 1
