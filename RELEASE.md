# Release SOP — Choreographr

Standard Operating Procedure for cutting a Choreographr release. Follow the
phases in order; each phase has a **gate** that must pass before moving on.

A release ships three things:

1. **19 crates to crates.io** (every workspace member except `choreo-gui`, in
   dependency order) — enables
   `cargo install choreographr choreo-tui` / `cargo binstall`.
2. **GitHub release `vX.Y.Z`** on `choreographr/choreographr` with prebuilt
   artifacts (Linux x86_64 + arm64 musl tarballs, macOS/Android tarballs,
   desktop `.deb` + `.rpm` in both Linux arches,
   Termux-native `.deb`, combined `SHA256SUMS`) — enables Homebrew,
   AUR, `cargo binstall`, and the `choreographr.com` installer. (The Windows
   `.zip` is built and smoke-tested on every tag but is not attached to the
   release yet — see [CI builds](#ci-builds-github-actions).)
3. **Channel updates** — Homebrew tap, choreographr.com (AUR deferred: no account).

One release conductor drives all three. The binary artifacts for all shipped
platforms are built **on GitHub Actions** by `.github/workflows/release.yml`
(see [CI builds](#ci-builds-github-actions)): pushing the `vX.Y.Z` tag builds
every platform and creates the GitHub release with the combined
`SHA256SUMS`. The two-machine build/upload flow is no longer part of the
normal path — it is documented in condensed form in the
[appendix](#appendix--manual-build--upload-fallback) for releases from
machines without push access (rare; it also covers what CI does not, namely
nothing — the channel updates are conductor tasks in every case).

> **Shell.** Every command snippet in this SOP is written for
> [nushell](https://www.nushell.sh) (`nu`). Nushell has no `\` line
> continuation and no `&&`: long commands collect their flags in a list and
> spread it with `...$list`, and `&&` becomes `;` (or two lines). Angle-bracket
> tokens (`<level>`, `<V>`, `<url>`) and `X.Y.Z` are **placeholders** — replace
> them before running.

---

## CI builds (GitHub Actions)

`.github/workflows/release.yml` builds the release binaries on GitHub-hosted
runners. It reuses `scripts/release.sh` (Linux + macOS jobs run it verbatim)
and `scripts/build-android.sh`, so build flags, `--locked`, feature selection,
artifact naming, and smoke tests stay in the scripts — the workflow only adds
per-runner toolchain setup and artifact plumbing. `choreo-gui` (a stub) is
built nowhere.

**Triggers** (deliberately narrow — full multi-platform builds are expensive):

- **`v*` tag push** — builds everything, then creates the GitHub release with
  all artifacts and one combined `SHA256SUMS`. The release job guards that the
  pushed tag matches the manifest version, generates the release body from the
  commit messages (git-cliff), and creates the release with all artifacts and
  one combined `SHA256SUMS`.
- **`workflow_dispatch`** — identical builds **and the same release-job checks**
  (the supply-chain gate and the git-cliff release-notes generation), but
  **no release is created** (only the final `gh release create` step is
  push-gated); artifacts attach to the workflow run (default 90-day retention).
  This is how the *whole* pipeline — builds **and** the release job — is tested
  without spamming tags.

| Job | Runner | Artifacts |
|---|---|---|
| `linux-x86_64` | ubuntu-latest | static `x86_64-unknown-linux-musl` tarball + `.deb` + `.rpm` (via `scripts/release.sh`; `rpmbuild` is apt-installed in the job, since it is not preinstalled) |
| `linux-arm64` | ubuntu-24.04-arm | static `aarch64-unknown-linux-musl` tarball + arm64 `.deb` + `.rpm` (via `scripts/release.sh` on a NATIVE arm64 runner, so its smoke tests execute the arm64 binaries on real arm64 hardware) |
| `macos-arm64` | macos-latest | native `aarch64-apple-darwin` tarball **plus** the cross-built `x86_64-apple-darwin` tarball (both via one `scripts/release.sh` pass; the Intel slice is smoke-tested under Rosetta and verified by construction downstream — no free x64 macOS runner exists) |
| `windows-msvc` | windows-latest | `x86_64-pc-windows-msvc` zip of the shipped `.exe` files — **built and smoke-tested on every tag, but currently NOT part of the published release** (the `release` job's `needs` omits this job until the Windows runtime is ready to ship) |
| `android-termux` | ubuntu-latest + NDK | `aarch64-linux-android` Termux tarball (via `scripts/build-android.sh --features metrics,blockchain`) + the Termux-native `.deb` (via `scripts/build-deb-termux.sh`, structural smoke-test on the runner; the packaged binaries are then extracted with Termux's own dpkg-deb under qemu-user and executed against an unpacked Termux aarch64 rootfs — see the workflow's qemu step) |
| `ios-build` | macos-latest | **none** — `choreo-gui` (the only crate that ships to iOS) compile check for both iOS targets, the real Xcode app link via the `ios/` scaffold, and a non-blocking simulator boot smoke (`continue-on-error` until the plumbing has proven stable). Deliberately not part of the release; a failing link is diagnosed from the log |

Every build job smoke-tests its own artifact beyond the clap surface: the
four desktop jobs run `scripts/daemon-smoke.sh` (boots the shipped daemon
hermetically — scratch socket + config dir — and proves the listener comes
up); the arm64 Linux job runs both smoke suites NATIVELY on real arm64
hardware; the macOS job additionally runs the x86_64 tarball's smoke suites under
Rosetta (an approximation, not native execution — no free x64 macOS runner
exists; see the release.yml comments); and the android job **executes** its
binaries under qemu-user
against the official Termux aarch64 rootfs (skopeo fetches the image layers;
no docker), closing the "never executed before release" gap.

The `release` job runs on every tag push **and** on `workflow_dispatch` (only
the final `gh release create` step is push-gated, so a dispatch runs every other
check). It downloads the four shipping platforms' build artifacts
(`linux-x86_64`, `linux-arm64`, `macos-arm64`, `android-termux` — deliberately
not the not-yet-shipped `windows-msvc`), generates one combined `SHA256SUMS`
over everything, installs `cargo-deny` + `git-cliff`, runs the supply-chain
guard, then on a tag guards that the pushed tag matches the manifest version,
generates the release body from the commit messages with git-cliff
(`scripts/release-notes.sh` / `cliff.toml` — the commit subjects become the Keep
a Changelog bullets and the bodies their paragraphs; an empty result fails the
job), and creates the release with
`gh release create vX.Y.Z dist/* --notes-file /tmp/release-notes.md`. The
release **title** is read from `choreo-shared/release-name.txt` — the same file
compiled into the binaries, so the title and `--version` cannot drift (an empty
file yields the bare `choreographr X.Y.Z`). A
re-run after the release already exists fails on create — assets are
immutable once uploaded; delete and re-create per the
[Hotfix / rollback](#hotfix--rollback) section rather than editing the job.

Every job builds the same shipped binaries (`choreographr choreo-tui` — the
`choreo-im`/`choreo-acp` bridges live in their own packages and are not built
for release) with the package-scoped
`--features choreographr/metrics,choreographr/blockchain` on the **stable**
toolchain, smoke-tests its artifact, and uploads it; the `release` job only
runs for tag pushes. The Termux artifacts ship **in addition to** the
Homebrew/AUR channels — those package the same tarballs CI produces. (The
Windows `.zip` is built and uploaded to the workflow run, but as noted above
it is not yet attached to the release.)

How this slots into the SOP: push the `vX.Y.Z` tag (Phase 3) **after** the
crates.io publish (Phase 2) — the tag push both publishes binaries and is the
release trigger — then verify the release page (Phase 3's gate) and do the
channel updates in Phase 4 as before.

---

## Versioning & gates

- **Version source of truth:** `[workspace.package] version` in the root
  `Cargo.toml`. `scripts/release.sh`, the Homebrew formula, and the AUR
  PKGBUILD all mirror it — do not edit them by hand for a version bump; let
  `cargo release` do it (Phase 1).
- **Tag format:** `vX.Y.Z` (e.g. `v0.1.1`). Release notes are generated from
  the commit messages of the new tag's range by git-cliff
  (`scripts/release-notes.sh`).
- **Release names:** a **major or minor** release sets a fun name — a dance
  style, e.g. *Lindy* — chosen by the conductor at release time (there is no
  pre-assigned list; pick whatever fits). **Patch releases keep the current
  name.** The name is a *per-minor-series* attribute stored in ONE file,
  `choreo-shared/release-name.txt` — the single source of truth. The file is
  compiled into the binaries (so `choreographr --version` prints
  `choreographr 0.2.0 (Lindy)`) and read by the CI release job for the GitHub
  release title. The name is release *metadata*: it is never in
  the git tag, the crate versions, or any install identifier.

### Preflight (before Phase 1)

Run the release gate on `master`, up to date with origin, with a clean working
tree:

```nu
just pre-release
```

`just pre-release` is everything a release needs before Phase 1, and never edits
the working tree (no `clippy --fix` / `fmt`): the toolchain check (cargo + zig +
git-cliff + cargo-zigbuild, notes nextest), the git release-state check (on
`master`, clean, not behind `origin/master`), `fmt --check`, clippy with warnings
denied, the full unit + integration suite, the supply-chain guard, the
cross-target type-check (`just check-cross` — the only local step that compiles
the `#[cfg(windows)]` / `#[cfg(target_os = "macos")]` code the host-target steps
never see, so a platform break fails fast before the pushed dry run), and the
crates.io credential check. **As its final step it pushes `master` and kicks the
release workflow** (`gh workflow run release.yml`) — a `workflow_dispatch` dry
run that builds every platform AND runs the release job's checks (the
supply-chain gate and the git-cliff release-notes generation) exactly like a tag
does, but creates **no** GitHub release, so the *whole* pipeline is proven on
GitHub before you ever tag (watch it with `gh run watch`). That push is why
`master` must be clean and up to date before you start.

The individual steps remain available as `just preflight`,
`just check-release-state`, `just check-crates-io-token`,
`just release-workflow-dry-run`, etc. if you need to isolate one.

---

## Phase 1 — Decide & bump the version

1. **Decide the level** — the release conductor's judgment call, made before
   any tooling runs. There are only three options; which one applies is
   determined by what changed since the last tag (`git log v<last>..HEAD` — the
   commit messages are the release notes; preview the rendered result with
   `just release-notes X.Y.Z`):

   | Level | Bump | When to pick it |
   |---|---|---|
   | `patch` | 0.1.0 → 0.1.1 | Bug fixes, security fixes, doc/UX polish — no new user-facing features |
   | `minor` | 0.1.1 → 0.2.0 | New features or behavior changes. While on 0.x, breaking changes also land here (semver treats 0.x minor as "may break") |
   | `major` | 0.2.0 → 1.0.0 | Breaking changes after 1.0, or the deliberate move to 1.0.0 (stability commitment) |

   **After 1.0.0 this policy shifts.** `minor` (1.0.0 → 1.1.0) starts
   *promising* backwards compatibility, so breaking changes move from
   `minor` to `major` (1.x → 2.x) and the everyday bump becomes `minor`, not
   `patch`. The inter-crate requirements flip from `"0.1"` (which Cargo reads
   as `< 0.2`) to `"1"` (`< 2`), so `dependent-version = "fix"` stops
   rewriting manifests on ordinary releases and only fires on a major. Update
   this table's examples when 1.0.0 ships (Phase 5 commits doc drift).

   For a **major or minor** release (not a patch), also choose a **new name**
   here — a dance style such as *Lindy*; there is no pre-assigned list, pick
   whatever fits (see [Release names](#versioning--gates)). A **patch** release
   keeps the current name: leave `choreo-shared/release-name.txt` as it is
   (step 2).

2. **Enact the decision** — the command that carries it out is
   `cargo release version <level>`, where `<level>` is replaced with the
   level you decided in step 1 (`patch` / `minor` / `major`). Nothing else
   needs to know the decision: `cargo release publish` takes no level, and
   there is no config flag — the level is this one argument. The command
   makes the single `[workspace.package] version` edit (plus `Cargo.lock`);
   all members inherit it. Dry-run first (the default); `-x` applies it:

   ```nu
   # Substitute the level you decided in step 1 (patch | minor | major).
   cargo release version minor        # dry-run: preview the bump plan
   cargo release version minor -x     # apply — edits version = "0.1.1" → "0.2.0"
   ```

   `cargo release version` only edits the manifests — it does **not** commit
   or tag. Run it **first**: it refuses to run on a dirty tree, so the bump
   must land before the accompanying edits below. Then, before committing:

   1. `cargo release version <level> -x` — applies the manifest bump (clean
      tree required; see above).
   2. Set the release name (**major/minor only**) — write one line to
      `choreo-shared/release-name.txt`; a patch release leaves it untouched.
   3. Update any user-facing docs that state a version or install command
      (README install section).

   ```nu
   git add Cargo.toml Cargo.lock choreo-shared/release-name.txt README.md  # + any other docs touched
   git commit -m "release: bump to X.Y.Z"
   ```

   (Prefer this over the one-shot `cargo release <level>`, which bumps, tags,
   publishes, and pushes in a single cargo-release-made commit — fine when
   nothing else needs to ride along with the bump.)

3. **Tag name check:** confirm no tag `vX.Y.Z` exists yet:
   `git ls-remote --tags origin | grep vX.Y.Z`.

   > **How the release notes are produced:** at the tag, the CI `release` job
   > runs git-cliff over the commits in the new tag's range (`cliff.toml`, via
   > `scripts/release-notes.sh`) and ships the result as the release body — the
   > commit messages ARE the release notes, so write them for the release page
   > (see AGENTS.md → Release notes via commits). The release **title** comes
   > from `choreo-shared/release-name.txt`. Preview the notes locally with
   > `just release-notes X.Y.Z`.

4. **Tag the bump commit** (cargo-release reads the version back from
   `Cargo.toml`): `cargo release tag -p choreographr -x` → creates `vX.Y.Z` at
   HEAD. **Select the root package explicitly with `-p choreographr`:** with no
   `-p`/`--workspace`, cargo-release acts on the workspace's
   `default-members = [".", "choreo-tui"]`, so a bare `cargo release tag -x`
   also tags the `choreo-tui` member as `choreo-tui-vX.Y.Z` (a non-root package
   gets a `<crate-name>-` tag prefix). That stray tag lands on the same commit
   but is not the release tag and does not match the CI `v*` trigger. The tag
   is pushed together with the commit once Phase 2 has published. First
   release only: if `v0.1.0` was already tagged locally before the release
   tooling existed (`git tag -l`), `cargo release tag` reports `disabled due
   to existing tag` and skips — that's fine as long as the tag sits on the
   commit you're shipping; just push it in Phase 2.

**Gate:** `just pre-release` green, tree clean, no conflicting tag.

---

## Phase 2 — Publish crates to crates.io

Runs **before** any binary building (binaries are versioned by the same
bump, and `cargo install` must resolve the published crates), on a clean
tree:

0. **Confirm you are signed in to crates.io.** The publish is
   token-authenticated; a missing, expired, or wrong-scoped token fails the
   *upload* — after the earlier crates in a batch have already published — with
   `403 Forbidden: authentication failed`. `just pre-release` already ran this
   check; re-run it on its own after minting or rotating a token:

   ```nu
   just check-crates-io-token
   ```

   This works where the old `curl … /api/v1/me` recipe did not: that endpoint
   is **cookie-only** — crates.io forbids API tokens on it
   (rust-lang/crates.io#3518) — so it answers `403` ("this action can only be
   performed on the crates.io website") for a *valid* token, never `200`. The
   script reads the token cargo would use (`$CARGO_REGISTRY_TOKEN`, else
   `~/.cargo/credentials.toml`) and confirms the token *authenticates*.

   If that fails, mint a token at <https://crates.io/settings/tokens> — it needs the
   **publish-new** and **publish-update** scopes (a full/legacy token also
   works) — and store it:

   ```nu
   cargo login                 # paste the token at the prompt
   # non-interactively:  $env.CARGO_REGISTRY_TOKEN = "<token>"
   ```

   Do **not** use `cargo owner --list` as the check — it returns public data
   even with a bogus token.

1. **Sync the MSRV claim first.** Dependency resolution is MSRV-unconstrained
   (`resolver.incompatible-rust-versions = "allow"` in `.cargo/config.toml`),
   so the workspace `rust-version` may lag the resolved tree. Compute the
   resolved tree's floor:
   `cargo metadata --format-version 1 | jq -r '[.packages[].rust_version | select(. != null)] | sort_by(split(".") | map(tonumber)) | last'`
   and set the result in `[workspace.package]` `rust-version` (root
   `Cargo.toml`), committing the bump together with `Cargo.lock`, so the
   crates.io metadata published below is truthful.

#### New-crate rate limit

crates.io throttles **new-crate creation** per account to a burst of **5** with
refill of **1 every 10 minutes** (a token bucket; updates to existing crates get
burst 30/minute). cargo-release mirrors this via its `rate-limit-new-packages`
setting (default 5, which the workspace does not override) and refuses a plan
upfront when it would publish more new crates than the burst:

```
error: attempting to publish N new crates which is above the rate limit: 5
error: dry-run failed, resolve the above errors and try again.
```

The 0.1.0 first release had **12 new crates**, staged in dependency-closed
batches. **Whenever a release creates more than the burst of 5 new crates, the
single-shot `publish --workspace` cannot succeed** — take one of two paths:

1. **Ask crates.io for a burst override** on the publishing account (the
   crates.io team raises the per-user burst in `publish_rate_overrides`). Then
   set `rate-limit-new-packages` to match and publish in one shot:
   `./scripts/publish-stable.sh publish --workspace -x`.
2. **Stage in two dependency-closed batches, ≥ 10 minutes apart.** Batch 1
   holds every crate that depends on no other workspace member; batch 2 holds
   the rest, which cargo-release publishes in dependency order.

**0.2.0 takes path 2.** It creates **six new crates** (`choreo-blockchain`,
`choreo-sanitize`, `choreo-image`, `choreo-sockreg`, `choreo-power-events`,
`choreo-content`), each of which *must* ship because a published crate depends
on it (cargo refuses to publish a crate whose dependency — optional deps
included — is not on crates.io). The split is 4 new + 2 new; the token bucket
(burst 5, +1 per 10 min) covers batch 2 after a single refill (4 spent → 1 left
→ +1 = 2 ≥ 2).

**Publish procedure.** Run each batch below twice — once as a dry run (no
`-x`), confirming it ends with `aborting release due to dry run`, then again
with `-x` to execute. `publish` does NOT bump or tag — that was
`cargo release version` and `cargo release tag` in Phase 1.

```nu
# Nushell has no `\` line continuation — collect the flags in a list and spread
# them into the command with `...$list`.
# ── Batch 1 — the nine dependency leaves ──
let b1 = ["publish"
  "-p" "choreo-proto" "-p" "choreo-keystore"
  "-p" "choreo-markdown" "-p" "choreo-mcp"
  "-p" "choreo-sanitize" "-p" "choreo-image"
  "-p" "choreo-sockreg" "-p" "choreo-power-events"
  "-p" "choreo-shared"]
./scripts/publish-stable.sh ...$b1        # dry-run; must end `aborting release due to dry run`
./scripts/publish-stable.sh ...$b1 -x     # execute

# ── wait ≥ 10 minutes so the new-crate token bucket refills (5 → 1 → +1 = 2) ──

# ── Batch 2 — everything else (2 new crates) ──
let b2 = ["publish"
  "-p" "choreo-transport" "-p" "choreo-ai-protocols"
  "-p" "choreo-acp" "-p" "choreo-blockchain"
  "-p" "choreo-content" "-p" "choreo-client-core"
  "-p" "choreo-im" "-p" "choreo-tui"
  "-p" "choreo-daemon" "-p" "choreographr"]
./scripts/publish-stable.sh ...$b2        # dry-run; must end `aborting release due to dry run`
./scripts/publish-stable.sh ...$b2 -x     # execute
```

> **This step is written for 0.2.0.** `choreo-shared` (a dependency leaf: clap
> + tracing-subscriber only) is a NEW publish-set member added after 0.2.0, so
> the set is now 19 crates and the next release that ships it is again a
> multi-crate one — add `"-p" "choreo-shared"` to Batch 1 above. Once all 19
> exist on crates.io every later release is pure *updates* and publishes
> single-shot with `./scripts/publish-stable.sh publish --workspace -x`.
> Re-derive that no-decision-needed rule from the dry run's ending when cutting
> the next release.

Publishing runs through `scripts/publish-stable.sh` (or `just publish-stable`),
not bare `cargo release`: the per-profile `rustflags` keys in the root
`Cargo.toml` (the `-Z…` frontend flags plus the repo-wide `-C target-cpu=native`)
require the nightly-only `profile-rustflags` cargo feature, and they ride along
into the uploaded `.crate`. A published manifest that still contains
`[profile.*] rustflags` **hard-breaks stable `cargo install`** (verified: stable
cargo errors with "The package requires the Cargo feature called
`profile-rustflags`") — which would kill the crates.io install route this SOP
verifies in Phase 5. The wrapper strips exactly those keys (plus the
`[unstable]` config opt-in) while `cargo release` runs and restores them on
exit, so the published source builds at the target's default CPU on stable,
exactly like the dist binaries.

The wrapper always appends an `--exclude <name>` for every `publish = false`
workspace member, derived from the manifests: cargo-release 1.1.5 does not
honor `publish = false` when selecting with `--workspace` (verified: its plan
includes the private crate and a real publish would then fail on cargo's own
refusal for a publish=false crate). `choreo-gui` is the only such member today
— a leaf client nothing depends on, so it can stay unpublished. INVARIANT: any
member a published crate depends on MUST be published (cargo refuses to package
a crate whose dependency is unpublished — optional deps included), so
`publish = false` is not an option for those. The wrapper
also owns the dirty-tree gate for the publish: it refuses to
start on an uncommitted tree by default — the `.crate` is built from the
working tree (cargo package), so unreviewed uncommitted code must never ship.
Pass `--allow-dirty` to skip that gate (e.g. to dry-run a plan from a dirty
tree; it does NOT make an execute-publish work on a dirty tree — see below).
The strip itself dirties the tree, but cargo-release 1.1.5 enforces an
**unconditional** clean-tree check on the publish step (there is no
`--allow-dirty` flag or config key in this version — `verify_git_is_clean` in
cargo-release's `src/steps/mod.rs` hard-fails on any dirty tree in execute
mode), so the wrapper masks exactly the two files it modifies with
`git update-index --skip-worktree` for the duration of the run (libgit2 then
reports them clean), and clears the masks on exit. Net effect: an actual
(non-dry-run) publish still requires a clean — committed — tree for every
file except the wrapper's own two masked files, which is exactly the hygiene
the gate exists for.

`--workspace` is **mandatory**. cargo-release ≥ 1.0 selects only the current
package by default: a bare `cargo release publish` plans just `choreographr`,
marks every workspace member as `disabled by user, skipping`, and then dies
with `error: choreographr 0.1.0 depends on unpublished workspace package
choreo-*` — the root's deps are neither in the publish set nor on crates.io
yet. `--workspace` puts all 19 publish-set members in the set; cargo-release
hands them to a single `cargo publish` call and cargo uploads them in
dependency order. `choreo-gui` is the one private member kept out by the
wrapper's derived `--exclude` flag — cargo-release 1.1.5 does *not* honor
`publish = false` in `--workspace` selection, despite the crate's manifest flag
(without the exclude its plan lists `choreo-gui`, and a real publish would then
fail on cargo's own refusal).

- `[workspace.metadata.release]` sets `dependent-version = "fix"`, so
  cross-crate requirements (`choreo-tui = "0.1"`, …) stay in lockstep across
  the whole publish set — `cargo release version` already rewrote them when
  it bumped. Exact subcommand/flags vary by cargo-release version —
  `cargo release --help` for the installed one.
- Push the bump commit and the `vX.Y.Z` tag created in Phase 1:
  `git push origin master --tags`. **This tag push is also the CI build
  trigger** — see Phase 3.
- Verify the published suite installs cleanly from source in a scratch
  `CARGO_HOME` (needs `zig` on PATH — zlob's `build.rs`):

```nu
$env.CARGO_HOME = (mktemp -d)
cargo install choreographr choreo-tui --locked
^$"($env.CARGO_HOME)/bin/choreographr" --version    # must print X.Y.Z
^$"($env.CARGO_HOME)/bin/choreo-tui" --version      # the TUI is its own package now
```

**Gate:** 19 crates published, `cargo install choreographr choreo-tui --locked`
works in a scratch CARGO_HOME, tag `vX.Y.Z` pushed.

---

## Phase 3 — CI build & GitHub release (tag push)

The `vX.Y.Z` tag pushed in Phase 2 is the build trigger. Nothing to run
locally — `.github/workflows/release.yml` builds every platform, smoke-tests
each artifact, and creates the GitHub release automatically (job table and
details in [CI builds](#ci-builds-github-actions)).

Conductor duties while the workflow runs:

1. **Watch the run** (`gh run watch` on the `release` workflow) — the four
   jobs the release waits on (`linux-x86_64`, `linux-arm64`, `macos-arm64`,
   `android-termux`)
   must go green. The `windows-msvc` job runs and smoke-tests its artifact but
   does NOT gate the release (it is not in the release job's `needs`), and the
   `ios-build` job may report its (non-blocking) smoke result; investigate a
   failure in either log, but neither holds the release.
2. **Verify the release page** once the `release` job completes:
   - the tag on the release matches `vX.Y.Z` and the manifest version
     (the job guards this too — a guard failure means a Phase 1/2 mistake);
   - all assets are present: five tarballs (Linux musl x86_64 + arm64,
     macOS arm64, macOS x86_64, Android Termux),
     the desktop `.deb` and `.rpm` for both Linux arches, the Termux-native
     `.deb`, and the
     combined `SHA256SUMS` (the Windows `.zip` is built but intentionally not
     attached — see [CI builds](#ci-builds-github-actions));
   - each asset downloads.

**Gate:** workflow green, release page lists all assets + `SHA256SUMS`,
assets download.

---

## Phase 4 — Channel updates

### Homebrew tap (`choreographr/homebrew-choreographr`)

Run the tap updater from a `dist/` holding the release's tarballs — with the
CI path, download them from the release first (the updater hashes the exact
artifacts that were uploaded; it does not re-download to compare):

```nu
gh release download vX.Y.Z -p 'choreographr-*.tar.gz' -D dist/
./scripts/update-homebrew-tap.sh            # dry-run: shows the diff, pushes nothing
./scripts/update-homebrew-tap.sh --push     # push the tap + sync the in-repo mirror
```

`scripts/update-homebrew-tap.sh` reads the version from `Cargo.toml`,
recomputes both `sha256` digests from the `dist/` tarballs, and rewrites
`Formula/choreographr.rb` in `choreographr/homebrew-choreographr` (version,
both `url` lines, both digests). It also reconciles the tap's `bin.install`
list against the in-repo mirrored formula
(`packaging/homebrew/choreographr.rb`, the source of truth for it), so a
changed shipped-binary set cannot silently break `brew install`. It validates
the result (exact-count rewrite checks, no stale version/placeholder, `ruby -c`
syntax check when ruby is present) and prints the diff. `--push` writes the
resolved formula back to `packaging/homebrew/choreographr.rb` (keeping the two
in lockstep — **commit that file with the rest of the release**) and commits +
pushes to the tap repo's default branch. The x86_64 branch is left untouched
when no `choreographr-<V>-x86_64-apple-darwin.tar.gz` is in `dist/` (Intel
macOS is not shipped yet — the branch stays a placeholder).

Then verify the channel — no Mac required: the `homebrew-verify` workflow
installs from the tap on a macOS arm64 runner and asserts `--version`:

```nu
gh workflow run homebrew-verify.yml --ref master -f version=X.Y.Z
gh run watch
```

(On a Mac you can do the same check by hand:
`brew install ./choreographr.rb` then `choreographr --version`.)

Manual fallback (what the script automates — only when the script cannot be
run):

1. Bump `version` to `X.Y.Z` in `Formula/choreographr.rb` (mirrored in this
   repo at `packaging/homebrew/choreographr.rb`).
2. Update both `url` lines — tag, filename, and embedded version.
3. Recompute the digests: `curl -fL -O <url>` then `shasum -a 256 <downloaded>.tar.gz`.
4. Verify: `gh workflow run homebrew-verify.yml -f version=X.Y.Z` (or
   `brew install ./choreographr.rb` then `choreographr --version` on a Mac).
5. Commit + push to the **tap repo** (not this repo).

### AUR (`choreographr-bin`) — ⏸️ DEFERRED (no AUR account)

**Skip this channel for now.** There is no AUR account for the project and AUR
new-account registration is temporarily closed
(<https://aur.archlinux.org/register>); the `choreographr-bin` package does not
exist on the AUR. Do **not** tick the AUR gate item — record it as deferred.
`packaging/aur/` is still kept current (source of truth) so the channel can be
published the moment an account exists.

When an AUR account is available — `packaging/aur/PKGBUILD`:

1. Bump `pkgver` to `X.Y.Z`, reset `pkgrel` to `1`.
2. Update both `source_<arch>` URLs and both `sha256sums_<arch>` digests (from
   the release's `SHA256SUMS` — the tarballs are
   `choreographr-<V>-x86_64-unknown-linux-musl.tar.gz` and
   `choreographr-<V>-aarch64-unknown-linux-musl.tar.gz`). The aarch64 digest is
   a placeholder until the arm64 tarball ships — fill it here.
3. Regenerate `.SRCINFO` and commit **in this repo** (`packaging/aur/` is part
   of `choreographr/choreographr`, not an AUR checkout):
   ```nu
   cd packaging/aur
   makepkg --printsrcinfo | save -f .SRCINFO
   cd ../..; git add packaging/aur/PKGBUILD packaging/aur/.SRCINFO
   git commit -m "chore(release): bump AUR PKGBUILD to X.Y.Z"
   ```
4. Upload to the AUR (a **separate** repo; needs your AUR account's SSH key):
   ```nu
   git clone ssh://aur.archlinux.org/choreographr-bin.git /tmp/choreographr-bin
   cp packaging/aur/PKGBUILD packaging/aur/.SRCINFO /tmp/choreographr-bin/
   cd /tmp/choreographr-bin
   git add PKGBUILD .SRCINFO
   git commit -m "choreographr-bin X.Y.Z"
   git push
   ```

### choreographr.com (static hosting)

1. Publish `scripts/install.sh` (or a per-version
   `install/vX.Y.Z.sh` and repoint `install.sh` — keep the versioned URL
   scheme from day one).
2. Add `/download/vX.Y.Z/…` 302 redirects for each asset (tarballs, `.deb`,
   `.rpm`, Termux `.deb`) → the GitHub release URLs.
3. Publish `/releases/SHA256SUMS` (the combined file).

**Gate:** every **published** channel's `--version` reports `X.Y.Z` (Homebrew,
choreographr.com). AUR is **deferred** (no account; registration closed — see
above) and is excluded from this gate until an account exists.

---

## Phase 5 — Post-release verification

Exercise every install route from a clean environment:

| Route | Command | Expect |
|---|---|---|
| crates.io (source) | `cargo install choreographr choreo-tui --locked` (with zig) | builds, `--version` = X.Y.Z |
| binstall (prebuilt) | `cargo binstall choreographr choreo-tui` | fetches tarball, no toolchain |
| Homebrew | `brew tap choreographr/choreographr` then `brew install choreographr` | no quarantine friction |
| AUR | `choreographr-bin` | **deferred** — no AUR account yet (registration closed) |
| curl installer | `curl -fsSL https://choreographr.com/install.sh \| sh` | sha256-verified extract |
| .deb / .rpm | `dpkg -i` / `dnf install` on clean distro VMs | installs; unit present, **not enabled** |
| Termux | `dpkg -i` the Termux-native `.deb` on a device | installs; binaries run under Termux's $PREFIX |

Confirm the service policy held everywhere: the systemd unit / launchd agent
is installed but **never auto-enabled** — `systemctl --user enable --now
choreographr` / `launchctl load …` remain explicit user actions.

Finally, commit any post-release doc/version drift in this repo and push.

---

## Hotfix / rollback

- **Bad crates.io publish:** yanking is a last resort (breaks `--locked`
  installs). Prefer publishing an immediate patch (Phases 1–5) — crates.io
  treats versions as immutable, so the patch **is** the fix.
- **Bad GitHub release:** `gh release delete vX.Y.Z` then re-create after
  fixing; assets are immutable once uploaded, so re-create with corrected
  artifacts (re-pushing the tag re-triggers the CI build — the `release` job
  fails on an existing release, so delete the release first).
- **Channel rollback:** Homebrew — revert the tap commit; AUR — bump `pkgrel`
  (`pkgrel=2`) or revert and push; choreographr.com — point redirects at the
  previous version (the versioned URL scheme makes this a one-line change).
- Hotfixes still run the full SOP; `--allow-dirty` is only for CI-style
  staged-but-uncommitted trees, never a substitute for the quality gate.

---

## Quick checklist (condensed)

- [ ] `just pre-release` green; tree clean; master pulled
- [ ] MSRV sync: `cargo metadata --format-version 1 | jq -r '[.packages[].rust_version | select(. != null)] | sort_by(split(".") | map(tonumber)) | last'` → update `rust-version` in `[workspace.package]` (with `Cargo.lock`) if changed
- [ ] `choreo-shared/release-name.txt`: one line with the new name for a major/minor release; left untouched for a patch
- [ ] Release notes preview: `just release-notes X.Y.Z` reads as a proper release page (they are generated from the commit messages at tag time by git-cliff)
- [ ] `cargo release version <level> -x` (level from Phase 1) → bump committed with doc updates; `cargo release tag -p choreographr -x` → `vX.Y.Z` (the explicit `-p` avoids a stray `choreo-tui-vX.Y.Z` tag)
- [ ] Signed in to crates.io (Phase 2 step 0): `just check-crates-io-token` passes, else `cargo login` a token with the publish-new/publish-update scopes
- [ ] Publish in **two batches** (0.2.0 created 6 new crates > burst 5; the post-0.2.0 `choreo-shared` adds a 19th member, also in Batch 1 — a single `--workspace` is refused): dry-run then `-x` each — Batch 1 (`-p choreo-proto … -p choreo-power-events -p choreo-shared`), wait ≥ 10 min, Batch 2 (`-p choreo-transport … -p choreographr`) → 19 crates on crates.io; `cargo install --locked` verified
- [ ] Push the bump commit + `vX.Y.Z` tag → CI builds all platforms and creates the GitHub release; verify the release page lists every asset + `SHA256SUMS` and they download
- [ ] `gh release download vX.Y.Z -p 'choreographr-*.tar.gz' -D dist/`, then `scripts/update-homebrew-tap.sh --push` (commit the synced `packaging/homebrew/choreographr.rb`); `gh workflow run homebrew-verify.yml -f version=X.Y.Z` green
- [ ] AUR — **deferred** (no AUR account; registration closed): `packaging/aur/` bumped in-repo but not pushed
- [ ] choreographr.com: `install.sh`, `/download/vX.Y.Z/` redirects, `/releases/SHA256SUMS`
- [ ] All install routes verified (`cargo install`/`binstall`, brew, curl, .deb, .rpm, Termux; **AUR deferred**)
- [ ] Service policy confirmed: installed, never auto-enabled

---

## Appendix — manual build & upload fallback

Only for releases from machines without push access to trigger CI (the
normal path is [Phase 3](#phase-3--ci-build--github-release-tag-push)). Both
desktop machines run `scripts/release.sh`, which:

- builds the shipped binaries on **stable** Rust (each cargo build runs through
  `scripts/build-stable.sh`, reproducible and matching the crates.io/MSRV
  story; see the README build notes) under the workspace's dedicated
  `[profile.dist]` profile — `--profile dist`, not `--release` — so the
  shipped artifacts land in `target/<triple>/dist/`, separate from any local
  `cargo build --release` output the packaging steps could otherwise pick up
  by mistake (see root `Cargo.toml`),
- builds every artifact at an explicit **CPU floor per target** via
  `RUSTFLAGS="-C target-cpu=…"` (see ARCHITECTURE.md "Release & packaging"):
  x86-64-v2 for the musl tarball, the target default (`apple-a14`) for the
  macOS arm64 tarball, x86-64-v3 (AVX2/FMA) for the macOS x86_64 tarball —
  the last Intel-capable macOS fleet is exactly the 2019–2020 Intel Macs,
  all AVX2-capable — and baseline for the `.deb`/`.rpm` and for the arm64 musl
  tarball (the generic aarch64 baseline already includes NEON, so no flag) —
  the local
  `-C target-cpu=native` profile
  flags (and the nightly `-Z…` flags) are additionally stripped by
  `scripts/build-stable.sh` before each stable build, so the build machine's
  CPU can never leak into a shipped artifact,
- reads the version from `Cargo.toml`,
- guards against a dirty tree,
- builds with the package-scoped
  `--features choreographr/metrics,choreographr/blockchain`,
- writes the tarball + `SHA256SUMS` (covering everything already in `dist/`
  for this version) into `dist/`,
- builds `.deb`/`.rpm` best-effort (Linux only, host glibc, no mimalloc),
- prints the `gh release create` command and the post-publish checklist.

The manual flow needs one **Linux box** — x86_64 or arm64, each producing its
own arch's musl tarball (static, mimalloc) plus that arch's `.deb`/`.rpm`;
needs `cargo-zigbuild`, optional
`dpkg-deb`/`rpmbuild` — and one **M1 MacBook** (both darwin tarballs —
release.sh on a Darwin-arm64 host native-builds the aarch64 tarball AND
cross-builds the x86_64 one in the same pass).
Artifacts are staged and uploaded from the Linux box — the arm64 Linux and
macOS tarballs are
copied there before upload. Windows and Android/Termux artifacts have no
manual path; if CI is unavailable for them, skip those assets for the
release or wait for CI (a re-pushed tag after `gh release delete` re-triggers
it).

### Linux box

```nu
just release            # dry-run: this arch's musl tarball + SHA256SUMS + .deb + .rpm
just smoke-test         # extract tarball; verify 2 binaries, --version, --help
```

Confirm `dist/` contains the musl tarball, `.deb`, `.rpm`, and `SHA256SUMS`.
Run it on an x86_64 box for the x86_64 set and on an arm64 box for the arm64 set
(or cross-build the arm64 tarball from x86_64 with the same `--target`
`scripts/release.sh` uses; the `.deb`/`.rpm` are emitted only for the host arch).

### M1 MacBook

```nu
just release            # dry-run: BOTH darwin tarballs + SHA256SUMS (no .deb/.rpm)
just smoke-test
```

Then the **manual daemon smoke test** (the tarball smoke test only checks
`--version`/`--help`; CI's `scripts/daemon-smoke.sh` covers this normally):

1. Extract the tarball, run `./choreographr` — confirm the socket
   (`choreographr.sock` under the platform temp dir, i.e.
   `/tmp/choreographr.sock` on Linux) and keystore initialize.
2. Load the bundled `com.choreographr.daemon.plist` in a throwaway launch
   agents dir; confirm the daemon starts and logs to `/tmp/choreographr.log`.
3. Run `./choreo-tui` and complete one round-trip with a configured account.

### Assemble & upload (Linux box)

GitHub uploads happen **once, from the Linux box**, so all assets land in one
release:

```nu
scp "macbook:…/choreographr-<V>-aarch64-apple-darwin.tar.gz" \
    "macbook:…/choreographr-<V>-x86_64-apple-darwin.tar.gz" dist/
just smoke-test         # re-validate on the Linux box for good measure
just release-upload     # regenerates a combined SHA256SUMS over ALL dist/ artifacts
                        # (host tarball + staged macOS tarball + .deb/.rpm) and
                        # uploads every tarball it finds + SHA256SUMS + .deb/.rpm
```

`scripts/release.sh` regenerates `SHA256SUMS` from the `choreographr-<V>-*`
glob **after** the `.deb`/`.rpm` step and assembles the upload list from every
tarball present in `dist/` — so staging the macOS tarball first is what makes
the uploaded checksum file complete and the macOS asset appear in the release.

Equivalent manual form (what `--upload` assembles) — the title comes from the
source-of-truth file, so it matches the CI-produced title exactly (an empty
`release-name.txt`, the unnamed series, yields the bare `choreographr X.Y.Z`):

```nu
# Title from the source-of-truth file, matching the CI job: a named series gets
# a trailing " (Name)", the unnamed series stays bare.
let NAME = (open --raw choreo-shared/release-name.txt | str trim)
let TITLE = if ($NAME | is-empty) { "choreographr X.Y.Z" } else { "choreographr X.Y.Z (" + $NAME + ")" }

# gh needs a real path for --notes-file (nushell has no <(...) substitution):
# generate the release notes from the commit messages into a temp file.
let NOTES = (mktemp)
^./scripts/release-notes.sh X.Y.Z | save -f $NOTES

# spread the asset paths from a list (no line continuation in nushell)
let assets = [
  "dist/choreographr-X.Y.Z-x86_64-unknown-linux-musl.tar.gz"
  "dist/choreographr-X.Y.Z-aarch64-unknown-linux-musl.tar.gz"
  "dist/choreographr-X.Y.Z-aarch64-apple-darwin.tar.gz"
  "dist/choreographr-X.Y.Z-x86_64-apple-darwin.tar.gz"
  "dist/choreographr-X.Y.Z-x86_64.deb"
  "dist/choreographr-X.Y.Z-aarch64.deb"
  "dist/choreographr-X.Y.Z-x86_64.rpm"
  "dist/choreographr-X.Y.Z-aarch64.rpm"
  "dist/SHA256SUMS"
]
gh release create vX.Y.Z ...$assets --title $TITLE --notes-file $NOTES
```

**Gate:** release page lists the manual-flow assets + `SHA256SUMS`;
assets download.
