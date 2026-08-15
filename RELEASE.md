# Release SOP — Choreographr

Standard Operating Procedure for cutting a Choreographr release. Follow the
phases in order; each phase has a **gate** that must pass before moving on.

A release ships three things:

1. **13 crates to crates.io** (everything except `choreo-gui`, in dependency
   order) — enables `cargo install choreographr` / `cargo binstall`.
2. **GitHub release `vX.Y.Z`** on `choreographr/choreographr` with prebuilt
   artifacts (tarballs, `.deb`, `.rpm`, `SHA256SUMS`) — enables Homebrew,
   AUR, `cargo binstall`, and the `choreographr.com` installer.
3. **Channel updates** — Homebrew tap, AUR, choreographr.com.

One release conductor drives all three; two build machines are involved (see
[Build machines](#build-machines)). There is **no CI** — every step runs on
owned machines, GitHub is only artifact hosting.

---

## Build machines

| Machine | What it builds | Notes |
|---|---|---|
| **Linux x86_64 box** | `x86_64-unknown-linux-musl` tarball (static, mimalloc), `.deb`, `.rpm` | Needs `cargo-zigbuild` (musl cross-build), optional `dpkg-deb` / `rpmbuild` |
| **M1 MacBook** | `aarch64-apple-darwin` tarball (native host build) | Also does the daemon/TUI smoke test |

Artifacts are **staged and uploaded from the Linux box** — it can build
everything except the macOS tarball, so the macOS tarball is copied to it
before upload (see [Phase 4](#phase-4--assemble-and-upload)).

---

## Versioning & gates

- **Version source of truth:** `[workspace.package] version` in the root
  `Cargo.toml`. `scripts/release.sh`, the Homebrew formula, and the AUR
  PKGBUILD all mirror it — do not edit them by hand for a version bump; let
  `cargo release` do it (Phase 1).
- **Tag format:** `vX.Y.Z` (e.g. `v0.1.1`). Release notes are generated from
  the tag diff (`gh release create --generate-notes`).
- **crates.io gate (pdf-inspector):** the `pdf` feature is **off by default**
  on crates.io — `choreo-daemon` declares the registry `pdf-inspector = "0.1"`
  and the workspace-root `[patch.crates-io]` redirect (security fork,
  RUSTSEC-2026-0187) applies **only to local builds**, never to published
  manifests. Publishing is therefore unblocked as long as nobody flips the
  `pdf` feature to be non-optional. When upstream publishes a fixed
  `pdf-inspector` (lopdf ≥ 0.42), delete the patch section, make `pdf`
  unconditional, and re-run the full suite.

### Preflight (before Phase 1)

```bash
# 1. Working tree clean, on master, up to date with origin.
git status --porcelain      # must be empty
git checkout master && git pull --ff-only origin master

# 2. Full quality gate — fmt, clippy (warnings denied), unit + integration.
just ci

# 3. Toolchain present on the build machines you'll use:
#    Linux box:  zig, cargo-zigbuild, gh, (dpkg-deb, rpmbuild optional)
#    MacBook:    zig, gh
just preflight               # checks cargo + zig, notes nextest
#
# Note: `just release` raises the fd soft limit itself (release.sh runs
# `ulimit -n 65536` — linking the four large binaries can open thousands of
# files and die with ProcessFdQuotaExceeded at the default 1024). If your
# shell refuses the raise, run it under a raised limit:
# `ulimit -n 65536 && just release`.
```

---

## Phase 1 — Decide & bump the version

1. **Decide the level** — the release conductor's judgment call, made before
   any tooling runs. There are only three options; which one applies is
   determined by what changed since the last tag:

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
   this table's examples when 1.0.0 ships (Phase 6 commits doc drift).

2. **Enact the decision** — the command that carries it out is
   `cargo release version <level>`, where `<level>` is replaced with the
   level you decided in step 1 (`patch` / `minor` / `major`). Nothing else
   needs to know the decision: `cargo release publish` takes no level, and
   there is no config flag — the level is this one argument. The command
   makes the single `[workspace.package] version` edit (plus `Cargo.lock`);
   all 13 members inherit it. Dry-run first (the default); `-x` applies it:

   ```bash
   cargo release version <level>    # dry-run: preview the bump plan
   cargo release version <level> -x # apply — e.g. decided `minor`:
                                    #   cargo release version minor -x
                                    # edits version = "0.1.1" → "0.2.0"
   ```

   `cargo release version` only edits the manifests — it does **not** commit
   or tag. Commit the bump together with any user-facing docs that state a
   version or install command (README install section):

   ```bash
   git add Cargo.toml Cargo.lock README.md   # + any other docs touched
   git commit -m "release: bump to X.Y.Z"
   ```

   (Prefer this over the one-shot `cargo release <level>`, which bumps, tags,
   publishes, and pushes in a single cargo-release-made commit — fine when
   nothing else needs to ride along with the bump.)

3. **Tag name check:** confirm no tag `vX.Y.Z` exists yet:
   `git ls-remote --tags origin | grep vX.Y.Z`.

4. **Tag the bump commit** (cargo-release reads the version back from
   `Cargo.toml`): `cargo release tag -x` → creates `vX.Y.Z` at HEAD. The tag
   is pushed together with the commit once Phase 2 has published. First
   release only: if `v0.1.0` was already tagged locally before the release
   tooling existed (`git tag -l`), `cargo release tag` reports `disabled due
   to existing tag` and skips — that's fine as long as the tag sits on the
   commit you're shipping; just push it in Phase 2.

**Gate:** `just ci` green, tree clean, no conflicting tag.

---

## Phase 2 — Publish crates to crates.io

Runs **before** any binary building (binaries are versioned by the same
bump, and `cargo install` must resolve the published crates). From either
machine, on the clean tree:

```bash
cargo release publish --workspace  # publishes the Phase-1-bumped version in
                                   # topological order — publish does NOT bump
                                   # or tag (that was `cargo release version`
                                   # and `cargo release tag` in Phase 1)
```

`--workspace` is **mandatory**. cargo-release ≥ 1.0 selects only the current
package by default: a bare `cargo release publish` plans just `choreographr`,
marks every workspace member as `disabled by user, skipping`, and then dies
with `error: choreographr 0.1.0 depends on unpublished workspace package
choreo-*` — the root's deps are neither in the publish set nor on crates.io
yet. `--workspace` puts all 13 publish-set members in the set; cargo-release
hands them to a single `cargo publish` call and cargo uploads them in
dependency order (`choreo-gui` drops out on its own via `publish = false`).

- `[workspace.metadata.release]` sets `dependent-version = "fix"`, so
  cross-crate requirements (`choreo-tui = "0.1"`, …) stay in lockstep across
  the whole publish set — `cargo release version` already rewrote them when
  it bumped. Exact subcommand/flags vary by cargo-release version —
  `cargo release --help` for the installed one.
- Push the bump commit and the `vX.Y.Z` tag created in Phase 1:
  `git push origin master --tags`.
- **Never** publish with the `pdf` feature enabled; the published manifests
  must stay free of the git patch (they are — patches don't propagate).
- Verify the published suite installs cleanly from source in a scratch
  `CARGO_HOME` (needs `zig` on PATH — zlob's `build.rs`):

```bash
export CARGO_HOME=$(mktemp -d)
cargo install choreographr --locked
~/.cargo/bin/choreographr --version    # must print X.Y.Z
```

#### New-crate rate limit

crates.io throttles **new-crate creation** per account to a burst of **5** with
refill of **1 every 10 minutes** (a token bucket; updates to existing crates
get burst 30/minute). cargo-release mirrors this via
`rate-limit-new-packages` in `[workspace.metadata.release]` (default 5) and
refuses upfront when a plan would publish more new crates than the burst:

```
error: attempting to publish N new crates which is above the rate limit: 5
error: dry-run failed, resolve the above errors and try again.
```

The 0.1.0 first release had **12 new crates** (see the batched staging plan
below for how that was done). The next release publishes **12 updates plus one
new crate — `choreo-blockchain`** (the blockchain-tools crate added since
0.1.0, referenced by `choreo-daemon`'s optional `blockchain` feature). One
new crate fits easily in the burst, so no batching is needed; if a future
release ever introduces several new crates at once, stage them in
≤ 5-crate batches:

1. **Ask crates.io for a burst override** on the publishing account (the
   crates.io team raises the per-user burst in `publish_rate_overrides`). Then
   set `rate-limit-new-packages` to match and publish in one shot:
   `cargo release publish --workspace -x`.
2. **Stage the first release in dependency-closed batches of ≤ 5 new crates**
   using `-p` selection, waiting ~10 minutes (one token refill) between
   batches. Every workspace dependency of a batched crate is either in the
   batch or already published:
   - Batch 1: `cargo release publish -p choreo-proto -p choreo-keystore -p choreo-markdown -p choreo-mcp -p choreo-transport -x`
   - Batch 2: `cargo release publish -p choreo-blockchain -p choreo-acp -p choreo-ai-protocols -p choreo-client-core -x`
   - Batch 3: `cargo release publish -p choreo-daemon -p choreo-im -p choreo-tui -x`
   - Batch 4: `cargo release publish -p choreographr -x`

   Dry-run each batch first (omit `-x`) and confirm it plans only that
   batch's crates. Once all 13 exist on crates.io, later releases are
   *updates* and go in a single `cargo release publish --workspace -x`.

**Gate:** 13 crates published, `cargo install choreographr --locked` works in
a scratch CARGO_HOME, tag `vX.Y.Z` pushed.

---

## Phase 3 — Build binaries (two machines)

Both machines run the same dry-run flow. `scripts/release.sh`:

- reads the version from `Cargo.toml`,
- guards against a dirty tree,
- builds with `--features pdf,metrics,blockchain` (the workspace patch hardens
the PDF parser for the shipped binaries even though crates.io doesn't get it,
the `/metrics` endpoint stays available as the README advertises, and the
EVM/Substrate blockchain tools ship in the released binaries),
- writes the tarball + `SHA256SUMS` (covering everything already in `dist/`
  for this version) into `dist/`,
- builds `.deb`/`.rpm` best-effort (Linux only, host glibc, no mimalloc),
- prints the `gh release create` command and the post-publish checklist.

### 3a. Linux x86_64 box

```bash
just release            # dry-run: musl tarball + SHA256SUMS + .deb + .rpm
just smoke-test         # extract tarball; verify 4 binaries, --version, --help
```

Confirm `dist/` contains:

```
choreographr-<V>-x86_64-unknown-linux-musl.tar.gz   # static musl + mimalloc
choreographr-<V>-x86_64.deb
choreographr-<V>-x86_64.rpm
SHA256SUMS
```

### 3b. M1 MacBook

```bash
just release            # dry-run: aarch64 tarball + SHA256SUMS (no .deb/.rpm)
just smoke-test
```

Then the **manual daemon smoke test** (the tarball smoke test only checks
`--version`/`--help`):

1. Extract the tarball, run `./choreographr` — confirm the socket
   (`/tmp/Choreographr.sock`) and keystore initialize.
2. Load the bundled `com.choreographr.daemon.plist` in a throwaway launch
   agents dir; confirm the daemon starts and logs to `/tmp/choreographr.log`.
3. Run `./choreo-tui` and complete one round-trip with a configured account.

**Gate:** both machines' tarballs pass `scripts/smoke-test.sh`; macOS daemon
smoke test passes. Keep the macOS tarball — it's needed in Phase 4.

---

## Phase 4 — Assemble & upload (Linux box)

GitHub uploads happen **once, from the Linux box**, so all assets land in one
release:

```bash
scp macbook:…/choreographr-<V>-aarch64-apple-darwin.tar.gz dist/
just smoke-test         # re-validate on the Linux box for good measure
just release-upload     # regenerates a combined SHA256SUMS over ALL dist/ artifacts
                        # (host tarball + staged macOS tarball + .deb/.rpm) and
                        # uploads every tarball it finds + SHA256SUMS + .deb/.rpm
```

`scripts/release.sh` regenerates `SHA256SUMS` from the `choreographr-<V>-*`
glob **after** the `.deb`/`.rpm` step and assembles the upload list from every
tarball present in `dist/` — so staging the macOS tarball first is what makes
the uploaded checksum file complete and the macOS asset appear in the release.

Equivalent manual form (what `--upload` assembles):

```bash
gh release create vX.Y.Z \
  dist/choreographr-X.Y.Z-x86_64-unknown-linux-musl.tar.gz \
  dist/choreographr-X.Y.Z-aarch64-apple-darwin.tar.gz \
  dist/choreographr-X.Y.Z-x86_64.deb \
  dist/choreographr-X.Y.Z-x86_64.rpm \
  dist/SHA256SUMS \
  --title "choreographr X.Y.Z" --generate-notes
```

**Gate:** release page lists all five assets + `SHA256SUMS`; assets download.

---

## Phase 5 — Channel updates

### Homebrew tap (`choreographr/homebrew-choreographr`)

Run the tap updater on the Linux box — after Phase 4, so the macOS tarball
is staged in `dist/`:

```bash
scripts/update-homebrew-tap.sh            # dry-run: shows the diff, pushes nothing
scripts/update-homebrew-tap.sh --push     # commit + push to the tap repo
```

`scripts/update-homebrew-tap.sh` reads the version from `Cargo.toml`,
recomputes both `sha256` digests from the `dist/` tarballs (no re-download —
it hashes the exact artifacts that were uploaded), rewrites
`Formula/choreographr.rb` in `choreographr/homebrew-choreographr` (version,
both `url` lines, both digests), validates the result (exact-count rewrite
checks, no stale version/placeholder, `ruby -c` syntax check when ruby is
present), and prints the diff. `--push` commits and pushes to the tap repo's
default branch. The x86_64 branch is left untouched when no
`choreographr-<V>-x86_64-apple-darwin.tar.gz` is in `dist/` (Intel macOS is
not shipped yet — the branch stays a placeholder).

The one step that stays manual, on the MacBook (Homebrew is macOS-only):

```bash
brew install ./choreographr.rb && choreographr --version
```

…then commit the mirrored-formula drift in this repo
(`packaging/homebrew/choreographr.rb`) during Phase 6.

Manual fallback (what the script automates — only when the script cannot be
run):

1. Bump `version` to `X.Y.Z` in `Formula/choreographr.rb` (mirrored in this
   repo at `packaging/homebrew/choreographr.rb`).
2. Update both `url` lines — tag, filename, and embedded version.
3. Recompute the digests: `curl -fL -O <url> && shasum -a 256 <downloaded>.tar.gz`.
4. Sanity-check: `brew install ./choreographr.rb && choreographr --version`.
5. Commit + push to the **tap repo** (not this repo).

### AUR (`choreographr-bin`)

Edit `packaging/aur/PKGBUILD`:

1. Bump `pkgver` to `X.Y.Z`, reset `pkgrel` to `1`.
2. Update the `source` URL and `sha256sums` (take the digest from the combined
   `SHA256SUMS` — the tarball is `choreographr-<V>-x86_64-unknown-linux-musl.tar.gz`).
3. Regenerate and push:
   ```bash
   cd packaging/aur && makepkg --printsrcinfo > .SRCINFO && git add PKGBUILD .SRCINFO
   ```

### choreographr.com (static hosting)

1. Publish `scripts/install.sh` (or a per-version
   `install/vX.Y.Z.sh` and repoint `install.sh` — keep the versioned URL
   scheme from day one).
2. Add `/download/vX.Y.Z/…` 302 redirects for each asset (tarballs, `.deb`,
   `.rpm`) → the GitHub release URLs.
3. Publish `/releases/SHA256SUMS` (the combined file).

**Gate:** every channel's `--version` reports `X.Y.Z`.

---

## Phase 6 — Post-release verification

Exercise every install route from a clean environment:

| Route | Command | Expect |
|---|---|---|
| crates.io (source) | `cargo install choreographr --locked` (with zig) | builds, `--version` = X.Y.Z |
| binstall (prebuilt) | `cargo binstall choreographr` | fetches tarball, no toolchain |
| Homebrew | `brew tap choreographr/choreographr && brew install choreographr` | no quarantine friction |
| AUR | `choreographr-bin` | installs, `choreographr --version` |
| curl installer | `curl -fsSL https://choreographr.com/install.sh \| sh` | sha256-verified extract |
| .deb / .rpm | `dpkg -i` / `dnf install` on clean distro VMs | installs; unit present, **not enabled** |

Confirm the service policy held everywhere: the systemd unit / launchd agent
is installed but **never auto-enabled** — `systemctl --user enable --now
choreographr` / `launchctl load …` remain explicit user actions.

Finally, on the Linux box, commit any post-release doc/version drift in this
repo and push.

---

## Hotfix / rollback

- **Bad crates.io publish:** yanking is a last resort (breaks `--locked`
  installs). Prefer publishing an immediate patch (Phases 2–6) — crates.io
  treats versions as immutable, so the patch **is** the fix.
- **Bad GitHub release:** `gh release delete vX.Y.Z` then re-create after
  fixing; assets are immutable once uploaded, so re-create with corrected
  artifacts.
- **Channel rollback:** Homebrew — revert the tap commit; AUR — bump `pkgrel`
  (`pkgrel=2`) or revert and push; choreographr.com — point redirects at the
  previous version (the versioned URL scheme makes this a one-line change).
- Hotfixes still run the full SOP; `--allow-dirty` is only for CI-style
  staged-but-uncommitted trees, never a substitute for the quality gate.

---

## Quick checklist (condensed)

- [ ] `just ci` green; tree clean; master pulled
- [ ] `cargo release version <level> -x` (level from Phase 1) → bump committed with doc updates; `cargo release tag -x` → `vX.Y.Z`
- [ ] `cargo release publish --workspace` → 13 crates on crates.io; `cargo install --locked` verified
- [ ] First release only: 13 new crates staged in ≤5-crate batches (or crates.io burst override) — see Phase 2; the next release adds just `choreo-blockchain` as a new crate
- [ ] Linux box: `just release` + `just smoke-test` → musl tarball, `.deb`, `.rpm`, `SHA256SUMS`
- [ ] MacBook: `just release` + `just smoke-test` + daemon/keystore/plist/TUI smoke test
- [ ] macOS tarball copied to Linux box; combined `SHA256SUMS` regenerated
- [ ] `just release-upload` → `gh release create vX.Y.Z` with all 5 assets
- [ ] `scripts/update-homebrew-tap.sh --push` run (tap formula bumped from `dist/`, pushed); `brew install` verified on the MacBook
- [ ] AUR `pkgver`/`sha256sums` bumped, `.SRCINFO` regenerated, pushed
- [ ] choreographr.com: `install.sh`, `/download/vX.Y.Z/` redirects, `/releases/SHA256SUMS`
- [ ] All install routes verified (`cargo install`/`binstall`, brew, AUR, curl, .deb, .rpm)
- [ ] Service policy confirmed: installed, never auto-enabled
