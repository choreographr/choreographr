# Plan: MCP-served skills (SEP-2640) as a second skill source

**Status:** **Not started — design only.** No code written yet.
**Lifecycle:** this file is **deleted once the plan is fully implemented**. Nothing
written during implementation may reference it — rustdoc, `ARCHITECTURE.md`,
`README.md`, release notes, and commit messages must stand on their own, because a
reference to this plan would go stale the moment it is removed.
**Date:** 2026-11-13
**Targets:** `choreo-mcp` (protocol surface, digest verification, content cache),
`choreo-daemon` (`context::*` skill discovery/loading, `tools/admin/load_skill.rs`,
`requests/system_content.rs`, session state, approval store), `choreo-tui`
(approval prompt + `/skills` surface), `choreo-proto` (approval + status messages),
`ARCHITECTURE.md`, `README.md`.
**Dependency:** none new. `rmcp` ≥ 3.10 (already pinned) implements the client half.

> **TL;DR.** Choreographr already has skills: a directory with a `SKILL.md` whose
> YAML frontmatter carries `name` and `description`, discovered from
> `<global_home>/.agents/skills` and a project tree, listed in the base prompt,
> and loaded on demand by the `load_skill` tool. MCP servers can now ship skills
> alongside their tools — SEP-2640 serves each skill file as a resource under
> `skill://<path>/…`, enumerated by `skills/list`, described by `skills/get`, read
> by the ordinary `resources/read` — and the skill format it delegates to is the
> same `SKILL.md` we already parse. So this is not new file-format work. It is
> **trust** work: MCP-served skill content is server-authored instruction text
> aimed at the model, so the job is to make its origin visible, gate it on
> explicit per-skill approval that is bound to the content the user approved,
> verify the content on every read, resolve names per origin so a server cannot
> shadow a local skill, and never let a server widen its own permissions. The
> recommendation is to ship it **opt-in per server, read-only, approval-gated,
> deny-by-default**, and never written to the filesystem.

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [The extension, as it affects a client](#2-the-extension-as-it-affects-a-client)
3. [Gap analysis](#3-gap-analysis)
4. [Design decisions](#4-design-decisions)
5. [Target architecture](#5-target-architecture)
6. [Work breakdown](#6-work-breakdown)
7. [Testing strategy](#7-testing-strategy)
8. [Security & trust model](#8-security--trust-model)
9. [Configuration & control surface](#9-configuration--control-surface)
10. [Documentation deliverables](#10-documentation-deliverables)
11. [Risks & mitigations](#11-risks--mitigations)
12. [Out of scope](#12-out-of-scope)
13. [Open questions](#13-open-questions)
14. [Definition of done](#14-definition-of-done)

---

## 1. Motivation & evidence

**What we have today** (`choreo-daemon/src/context.rs`):

- `SkillMeta { name, description, path }` — parsed from a `SKILL.md`'s YAML
  frontmatter by `scan_skills_dir`, deduped by `name` with project-beats-global
  precedence, cached per session in `SessionState::discovered_skills`.
- `build_base_prompt` lists them under `## Available skills` and tells the model to
  call `load_skill`; `load_skill_body_from` reads the file lazily on demand.
- A loaded skill's body is persisted into `SessionState::loaded_skill_bodies` and
  re-injected every turn inside a `<skill name="…">` tag, so it survives the tool
  result scrolling out of the window.
- The identity of a skill is **its `name`** alone, plus a filesystem `path`.

That shape has two consequences this plan has to answer for: a skill must be a
**local file**, and a skill **name is globally unique** by construction (discovery
dedups on it).

**What SEP-2640 adds.** A server can now publish skills:

- The format is the [Agent Skills](https://agentskills.io/) specification — the
  same `SKILL.md` + YAML frontmatter (`name`, `description`, optional
  `license`/`metadata`) that we already parse. A skill is a *directory*, and every
  file in it is exposed as an MCP resource under `skill://<skill-path>/<file-path>`.
- `skills/list` enumerates skills (paginated; may be empty or partial), and
  `skills/get` returns any single skill's entry by URI.
- An entry carries the skill's frontmatter verbatim, plus a **complete** list of
  its files with a SHA-256 `digest` and byte `size` each — or the literal
  `"dynamic"` when the content cannot be pinned.
- Files are read with the ordinary `resources/read`; a skill is *activated* only
  by the host's own skill-loading path, never by a read.
- Servers declare the extension in their capabilities
  (`capabilities.extensions["io.modelcontextprotocol/skills"]`), optionally with
  `directoryRead: true`.
- Per-skill limits: **512 resources** and **16 MiB** total, so a host can budget a
  skill from the entry alone before fetching a byte.

**Why this is worth doing.** A server and the instructions that teach an agent to
use it are currently versioned, discovered and installed separately; a user who
installs a server has no signal that a companion skill exists. And the alternative
channel — the server's `instructions` field — is practically bounded in size, so a
workflow of any real length does not fit. Serving the skill next to the tools it
describes is the fix, and it arrives on top of surface we already speak (resources).

**Why now.** `rmcp` 3.10 already implements the client side
(`Peer::list_skills`, `Peer::get_skill`, `Peer::list_all_skills`, and the
`Skill` / `SkillFrontmatter` / `SkillResources` / `SkillsCapability` model types),
so no protocol plumbing is needed. The work is entirely on our side of the
boundary, and it is the kind of work that is cheap to do first and expensive to
retrofit — approval, provenance and verification are not features you bolt on
afterwards.

## 2. The extension, as it affects a client

Only what a host must do, in the order a host does it:

| Step | Surface | What the spec requires of a host |
|---|---|---|
| Declare | server `capabilities.extensions[…]` | Do not call `resources/directory/read` unless `directoryRead: true` |
| Enumerate | `skills/list` (paginated) | Must not treat empty/partial as "no skills"; entries are atomic per skill |
| Inspect | entry fields | Complete `resources` list (uri + digest + size) or `"dynamic"`; `frontmatter` verbatim |
| Budget | entry alone | Support ≥ 512 resources / 16 MiB per skill; a host may decline a larger one |
| Retrieve | `resources/read` | Only on demand — never prefetch a skill's files |
| Verify | fetched bytes | Digest **and** size must match the entry; on mismatch, re-fetch the entry and treat the old approval as revoked |
| Check identity | `SKILL.md` frontmatter | Must equal the entry's `frontmatter` field-by-field, or the skill is not loaded |
| Activate | host's own path | Fresh per-skill approval; persists bound to the digest set observed at approval time |
| Label | model context | Origin server identity must accompany the content and be distinguishable from a local skill |

Two details that shape the design more than they look:

- **Digests are not a security boundary.** They are unsigned and supplied by the
  same server as the content; a match proves listing and body agree, not that
  either is trustworthy.
- **Nested skills.** A skill directory may contain further skills. From the
  enclosing skill's point of view their files are ordinary supporting files, and a
  nested `SKILL.md` read that way is ordinary markdown whose frontmatter must not
  be acted on. Activating one requires its own approval.

## 3. Gap analysis

| Concern | Today | With SEP-2640 |
|---|---|---|
| Skill source | filesystem only (`SkillMeta.path`) | filesystem **and** servers |
| Identity | `name` (unique by dedup) | `(origin, uri)`; names collide across origins |
| Body retrieval | read the local file | fetch a resource, verified against digest+size |
| Provenance | implicit (a local path) | must be carried into model context |
| Trust | user-authored (their own repo/home) | **untrusted**, server-authored, aimed at the model |
| Approval | none (files are the user's own) | per-skill, content-bound, revocable |
| Refresh | re-walk filesystem, fingerprint-gated | `skills/list` on connect + list-changed events |

The one thing that carries over unchanged: **the body format**. `SKILL.md`
frontmatter already matches what an entry's `frontmatter` field returns.

## 4. Design decisions

**D1 — Opt-in per server, off by default.** `McpServerConfig` gains
`skills: bool` (default `false`). A server is connected for its *tools*; taking on
its instruction text is a different, higher-risk relationship, so it is a
per-server yes. Opting in is also what lets the prompt stay quiet for the servers
that do not serve skills.

**D2 — MCP-served skills are untrusted input.** The skill body is text that the
model may act on. It reaches the model **tagged with its origin server** and is
never presented as equivalent to a local skill. Concretely, the `<skill …>` wrapper
gains an origin attribute (e.g. `<skill name="mcp__<slug>__<name>" origin="mcp:<slug>">`),
and the base prompt's `## Available skills` list marks MCP entries with their
server.

**D3 — Names resolve per origin; nothing shadows.** A skill from origin
`(server slug, uri)` is addressed by a **composed name** that encodes both,
built with the existing `choreo-mcp::naming` helpers (provider-safe alphabet,
length cap, stable hash suffix) so it survives being named in a tool argument and
in a prompt. Names must not collide with a local skill's name, and two servers
serving the same skill name must both be visible and distinguishable rather than
one silently winning — the current "dedup by name" rule is exactly the behaviour
the spec forbids across origins.

**D4 — Never materialize server content into a skill-discovery path.** The
tempting shortcut is to write fetched skills to `~/.agents/skills/<name>` and let
the existing discovery find them. Refused: that path is scanned by discovery, so
server content would silently become *filesystem-trust* content and survive the
server's removal. If a content cache is added at all (see D8) it lives under the
state directory, outside every discovery path, and is verified on each access
rather than trusted for being local.

**D5 — Verify on every read, not on load.** A fetched file must match its entry's
`digest` **and** `size`; a `SKILL.md` must additionally match its entry's
`frontmatter` field-by-field. Any mismatch means the skill is not loaded, and the
entry is re-fetched (a changed `resources` set revokes the approval per D7). An
entry whose `resources` is `"dynamic"` is **refused**: it offers no content
binding, so it cannot be covered by an approval.

**D6 — Budget from the entry, before fetching.** Adopt the spec's ceilings as our
caps — 512 resources and 16 MiB per skill — plus a per-server aggregate so one
server cannot fill the catalogue with thousands of skills. A skill that exceeds a
cap is declined **with a message naming the reason**, never silently dropped. The
existing `MAX_TOOLS_PER_SERVER` / `MAX_SCHEMA_BYTES` constants establish the
house pattern; the new caps sit beside them in `choreo-mcp::protocol`.

**D7 — Approval is per skill and bound to the content.** The first `load_skill`
of an MCP skill requires explicit user approval (TUI prompt; a CLI flag for
headless use). The approval record is `(server slug, uri) → digest set`, so a
server that rotates, adds or removes a file after approval has its approval
revoked and must re-prompt. Records persist in daemon state (not in the server's
config), are inspectable, and are dropped when the server is removed.

**D8 — Lazy, never prefetch.** `skills/list` results are cached per connection;
bodies are fetched when a skill is loaded and supporting files when they are read
(if we expose that at all — see §12). A server publishing hundreds of skills must
not cost a fetch of hundreds of bodies at connect time.

**D9 — Refresh on connect and on list-changed.** The listing is fetched when a
server connects (only if `skills` is opted in) and refreshed when the subscription
stream reports a skills change. `choreo-mcp::protocol::McpListKind` grows a
`Skills` variant if the SDK surfaces the notification; otherwise the listing is
refreshed on the first `load_skill` after a reconnect. Either way a stale listing
cannot load unapproved content, because D5/D7 verify against the *current* entry.

**D10 — One discovery snapshot, two sources.** `SessionState::discovered_skills`
stays the single cache the prompt, `load_skill` and `persist_loaded_skill` all
read, so the three cannot disagree. `SkillMeta` gains a source:

```rust
enum SkillSource {
    File { path: PathBuf },
    Mcp { server: String, uri: String },
}
```

and `load_skill_body_from` dispatches on it — a file read for the first, a
verified `resources/read` for the second, with the origin tag added to the body
wrapper. The invalidation rules (working-dir change, reconnect) extend to the new
source.

**D11 — `choreo-mcp` owns the protocol and the cache; the daemon owns policy.**
`choreo-mcp` grows a typed `McpSkill { name, description, uri, resources,
origin }` (no `rmcp` type escapes the crate, as today), the digest verification,
and the entry cache behind its existing dispatcher thread. The daemon decides
*whether* to surface a skill, *which* names are addressable, and *what* approval
means. That keeps the blocking facade contract ("the daemon never sees an `rmcp`
type") intact and keeps trust policy where the session lives.

**D12 — A skill never widens permissions.** Nothing in the frontmatter affects
tool availability: `allowed-tools` from an MCP origin is ignored outright, our
tool groups are unchanged by loading a skill, and `load_skill` keeps returning
text. The spec's "no implicit local execution" rule is therefore satisfied by
construction rather than by a check — and the check that a future change must not
break is that `load_skill` never grows a side effect beyond persisting a body.

## 5. Target architecture

```
        connect (skills: true)
                │
      ┌─────────▼──────────┐  skills/list (paginated)   ┌───────────────────┐
      │ choreo-mcp engine  │ ─────────────────────────► │ MCP server        │
      │  • typed McpSkill  │ ◄───────────────────────── │  skill://…        │
      │  • entry cache     │  skills/get / resources/read│                   │
      │  • digest verify   │                            └───────────────────┘
      └─────────┬──────────┘
                │ channel (blocking facade, unchanged)
      ┌─────────▼──────────────────────────────────────────────┐
      │ choreo-daemon session                                  │
      │  discovered_skills: Vec<SkillMeta>  (File | Mcp)       │
      │  approvals: (slug, uri) → digest set                   │
      │  build_base_prompt  ← listing (origin-labelled)        │
      │  load_skill tool    → file read | verified fetch       │
      │  persist_loaded_skill → <skill …origin="mcp:slug">     │
      └────────────────────────────────────────────────────────┘
```

The daemon's agent loop is unchanged: it still populates `discovered_skills` before
any tool runs, and `load_skill` still resolves against that snapshot.

## 6. Work breakdown

Each phase lands with its tests and is independently reviewable.

**P1 — Protocol surface (`choreo-mcp`).** Capability detection
(`SkillsCapability` present in the negotiated server capabilities), `McpSkill` /
`McpSkillResource` in `protocol.rs`, `list_skills` on the blocking facade and the
dispatcher, entry cache keyed by `(slug, uri)`, and the D6 caps as constants.
Tests: a unit test over a canned listing (including a `"dynamic"` entry and an
over-cap skill), and that a server without the extension surfaces no skills.

**P2 — Verified fetch (`choreo-mcp`).** `read_skill_file(slug, uri, entry)`:
`resources/read`, then size + digest + (for `SKILL.md`) frontmatter verification.
Typed errors distinguishing *not found*, *digest mismatch*, *size mismatch*,
*frontmatter mismatch*, and *over budget*, each naming what failed.
Tests: tampered body, truncated body, rotated entry, `"dynamic"` refusal.

**P3 — Discovery merge (`choreo-daemon`).** `SkillSource` on `SkillMeta` (D10),
MCP entries folded into `discover_skills_ambient`'s result with composed names
(D3), origin labelling in `build_base_prompt`, cache invalidation on reconnect.
Tests: same name from two servers stays two addressable skills; a local skill is
never shadowed; the prompt lists both with origins.

**P4 — Approval (`choreo-daemon` + `choreo-proto` + `choreo-tui`).** Approval
store (D7), a client request/reply pair for the prompt, the TUI prompt, a
`--approve-skill <slug>/<uri>` headless path, `/skills` status listing entries
with origin, size, approval state and digests. Tests: approve-then-load; refusal;
rotated content re-prompts.

**P5 — `load_skill` over both sources.** Dispatch on `SkillSource`, add the origin
attribute to the wrapper, and keep `persist_loaded_skill` reading from the same
snapshot. Tests: body round-trips with its origin tag; a failed verification
surfaces as a tool error rather than an empty skill.

**P6 — Refresh + caps.** `McpListKind::Skills` end-to-end if the SDK surfaces the
notification (otherwise refresh-on-use, documented as such), per-server aggregate
cap, and decline messages naming the cap.

**P7 — Docs + release note.** `ARCHITECTURE.md` (a new subsection under the MCP
security model: skills as untrusted input, origin, approval, verification) and
`README.md` (the `choreo-mcp` row and the skills concept). No reference to this
plan anywhere.

## 7. Testing strategy

- **Unit (in `src/`)**: listing decode and cap enforcement; digest/size/frontmatter
  verification against fixtures built in-test; composed-name determinism and
  collision suffixes; `load_skill` dispatch; approval store semantics (bind,
  revoke on changed set, drop on server removal).
- **Integration (`tests/it/`)**: extend the scripted fixture server to serve
  skills, and cover (a) two servers serving the same skill name, (b) a
  `resources` set that changes between listing and read, (c) a tampered body,
  (d) a `"dynamic"` skill, (e) a skill at exactly the 512-entry / 16 MiB ceiling
  and one byte over, (f) a nested skill that must not activate implicitly.
- **Conformance**: run the scenario servers through the official conformance suite
  if it grows skills scenarios; otherwise note the gap in the plan's completion.
- No time-based waits anywhere (workspace rule): the fixture server drives every
  step by responding to a request, and the daemon assertions are channel-driven.

## 8. Security & trust model

| Threat | Control |
|---|---|
| Prompt injection via a skill body | Origin tag in model context; per-skill approval; content shown to the user before load |
| Server impersonating a popular skill name | Per-origin namespacing (D3); collisions surfaced, never resolved silently |
| Server widening its own permissions | `allowed-tools` ignored (D12); no tool-group change on load |
| Server swapping content after approval | Approval bound to the digest set (D7); verification on every read (D5) |
| Confused deputy across servers | Every read is bound to the skill's originating server; no cross-origin read path |
| Disk-cache poisoning | No filesystem materialization (D4); any future cache is outside discovery paths and verified per access |
| Resource exhaustion | Entry-based budgeting (D6) before any fetch; lazy retrieval (D8) |
| Silent unsupported content | `"dynamic"` entries refused with a message; over-cap skills declined visibly |

## 9. Configuration & control surface

```toml
[[mcpServers]]
name    = "acme"
command = "acme-mcp"
skills  = true          # opt in; absent/false leaves skills unserved
# optional: skillsApprove = ["skill://acme/billing/refunds/SKILL.md"]
```

- TUI: `/skills` lists `name — origin — size — approved?`; the approval prompt
  shows the origin, the resource count and total bytes before the body is shown.
- CLI: `choreographr mcp skills <server>` and
  `--approve-skill <server>/<uri>` for headless runs.
- Daemon refuses `skills = true` for a server whose extension is absent, with a
  message saying so (a visible misconfiguration, not a silent no-op).

## 10. Documentation deliverables

- `ARCHITECTURE.md`: MCP security model subsection (skills as untrusted input;
  origin; approval bound to content; verification; no materialization) and the
  `choreo-mcp` module map entry for the skills surface.
- `README.md`: `choreo-mcp` row (skills alongside tools), the crate table's
  `choreo-daemon` skills entry, and the `mcpServers` reference for `skills`.
- rustdoc for every new public item; `choreo-mcp` and `choreo-daemon` already carry
  `#![warn(missing_docs)]`.

## 11. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Approval friction makes the feature unused | Approval is per skill and persistent; origin+size shown so it is a one-glance decision; a server can be pre-approved in config |
| Context blowup from a large skill body | Body sizes are bounded by the 16 MiB entry budget and, practically, by the prompt budget — surface the size in the approval prompt and in `/skills` |
| Listing churn breaks the snapshot | Verification is against the *current* entry, so a stale listing fails closed rather than loading unapproved bytes |
| Confusion with local skills in the prompt | Origin marker in both the listing and the `<skill>` wrapper; `/skills` groups by origin |
| Scope creep into skill *authoring* or publishing | Explicitly out of scope (§12) |

## 12. Out of scope

- Serving our own skills over MCP (the `skills-fs` server role). This plan makes us
  a consumer only.
- Exposing a server's supporting files as model-callable reads. If wanted later, it
  needs the origin-scoped, per-call-approved path the spec describes, and it is a
  separate plan.
- `resources/directory/read` beyond whatever a future supporting-file surface
  needs; we do not call it while `directoryRead` is undeclared.
- Writing MCP-served skills anywhere the filesystem discovery sees.
- Skill authoring, editing, or publishing.
- Entering skills into the compaction/summary pipeline differently from other
  context.

## 13. Open questions

1. **Composed-name format.** `mcp__<slug>__<name>` matches the tool-name convention
   and is greppable; a dotted form reads better in the prompt. Decide before P3,
   because it is persisted in approvals.
2. **Where approvals live.** Daemon state file vs the existing session DB. The
   DB gives durability and one migration story; the file is inspectable by hand.
3. **Headless default.** Is an unapproved skill simply unavailable in a non-TUI
   run (recommended), or does the `--approve-skill` flag imply a global opt-in?
4. **Cross-server skill sharing** — a skill from server A naming a file served by
   server B. The spec forbids it without a per-call approval naming both; not
   implemented either way yet.
5. **Do supporting files matter for v1?** If a workflow's `SKILL.md` says "pick a
   template from `templates/`", a body-only implementation is visibly incomplete.
   Option A: v1 loads `SKILL.md` only and says so in the prompt; Option B: wait
   for the supporting-file read surface.

## 14. Definition of done

- A server that declares the skills extension and is opted in has its skills
  listed in the base prompt with their origin, and `load_skill` on one returns the
  verified body only after approval.
- Two servers serving the same skill name are both addressable and
  distinguishable; no local skill is ever shadowed by a server's.
- Approving a skill and then having the server rotate its content re-prompts
  instead of loading; a tampered body never reaches the model; a `"dynamic"`
  skill is refused with a stated reason.
- No MCP-served content is written anywhere skill discovery scans.
- `just pre-commit` green; `choreo-mcp` and `choreo-daemon` rustdoc warning-free;
  `ARCHITECTURE.md` and `README.md` describe the behaviour without referencing
  this plan; this file deleted in the same change that completes the work.
