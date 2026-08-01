# Provider base-URL / model verification notes

Live-verified 2026-08-01 via http_request. Where a value in the catalog
couldn't be independently confirmed online, the source is the agent union
(tau catalog.toml, goose declarative JSON, pi provider files, zero catalog).

## Confirmed against official docs / live endpoints

| Provider | What was checked | Result |
|---|---|---|
| mistral | `docs.mistral.ai` API reference | Endpoint is `POST /v1/chat/completions` — OpenAI wire format. Catalog routes mistral as `openai`. ✅ |
| deepseek | `api-docs.deepseek.com` | OpenAI base `https://api.deepseek.com`, Anthropic alt `/anthropic`. Models `deepseek-v4-flash`/`deepseek-v4-pro`. ✅ matches catalog. |
| together | `docs.together.ai` | Docs server is `https://api.together.ai/v1` (`.ai`, not `.xyz`). ✅ catalog uses `.ai`. |
| zai | `docs.z.ai` (llms.txt) | Current models GLM-5.2, GLM-5.1, GLM-5-Turbo — matches zai.toml. Base URL (`/api/coding/paas/v4`) from pi/tau (coding plan endpoint). |
| moonshotai | `platform.moonshot.ai` pricing | Current models: Kimi K3, K2.7 Code, K2.6, K2.5, Moonshot V1 — matches moonshotai.toml (kimi-k2.7-code default). |

## Assumed from agent sources (docs unreachable / JS-gated, single source or
## minor disagreement among agents)

- zai base path: pi uses `https://api.z.ai/api/coding/paas/v4`; zero uses
  `/api/paas/v4`. Catalog uses pi's coding-plan endpoint.
- moonshot: international `https://api.moonshot.ai/v1` (tau/pi). goose
  defaults to CN `api.moonshot.cn`. Catalog uses `.ai`.
- minimax: anthropic-mode `https://api.minimax.io/anthropic` (goose/tau agree).
- xiaomi: tau/pi `https://api.xiaomimimo.com/v1` (catalog); goose/zero old
  endpoint `api.mimo.xiaomi.com/openai/v1` was used by the legacy
  `xiaomi-mimo` alias (removed).
- fireworks: catalog uses OpenAI-compatible inference base
  `https://api.fireworks.ai/inference/v1` with openai protocol.
- opencode zen/go: `https://opencode.ai/zen/v1` and `/zen/go/v1` (tau/pi/goose
  agree). Per-model `responses` flag from tau's metadata (GPT-5.x → Responses).
- vercel-ai-gateway: `https://ai-gateway.vercel.sh` (goose/tau/pi agree);
  catalog protocol = anthropic (goose/tau list it under anthropic-messages).
- qwen-token-plan endpoints from pi (`token-plan.*.maas.aliyuncs.com`).
- cloudflare-ai-gateway / workers-ai base URLs are per-account prefixes; the
  catalog entries use the documented prefix and expect a per-account
  `base_url` override. Models are dynamic (empty list).
- omlx / llama-swap / tanzu are local/enterprise endpoints; catalog defaults
  are placeholders (localhost / example host) meant to be overridden.
