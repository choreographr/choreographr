# Release SOP — Choreographr

Standard Operating Procedure for cutting a Choreographr release. Follow the
phases in order; each phase has a **gate** that must pass before moving on.

A release ships three things:

1. **12 crates to crates.io** (everything except `choreo-gui`, in dependency
   order) — enables `cargo install choreographr` / `cargo binstall`.
2. **GitHub release `vX.Y.Z`** on `ethernomad/choreographr` with prebuilt
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
  `cargo release` do it (Phase 2).
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
```

---

## Phase 1 — Decide & bump the version

1. Decide the bump level (patch/minor/major; 0.1.x is a patch). Update any
   user-facing docs that state a version or install command (README install
   section) in the **same commit** as the bump.
2. **Tag name check:** confirm no tag `vX.Y.Z` exists yet:
   `git ls-remote --tags origin | grep vX.Y.Z`.

**Gate:** `just ci` green, tree clean, no conflicting tag.

---

## Phase 2 — Publish crates to crates.io

Runs **before** any binary building (binaries are versioned by the same
bump, and `cargo install` must resolve the published crates). From either
machine, on the clean tree:

```bash
cargo release publish          # bumps version in all 12 members (dependent-version = "fix"),
                               # tags vX.Y.Z, publishes in topological order
```

- `[workspace.metadata.release]` sets `dependent-version = "fix"`, so
  cross-crate requirements (`choreo-tui = "0.1"`, …) stay in lockstep across
  the whole publish set. Exact subcommand/flags vary by cargo-release
  version — `cargo release --help` for the installed one.
- **Never** publish with the `pdf` feature enabled; the published manifests
  must stay free of the git patch (they are — patches don't propagate).
- Verify the published suite installs cleanly from source in a scratch
  `CARGO_HOME` (needs `zig` on PATH — zlob's `build.rs`):

```bash
export CARGO_HOME=$(mktemp -d)
cargo install choreographr --locked
~/.cargo/bin/choreographr --version    # must print X.Y.Z
```

**Gate:** 12 crates published, `cargo install choreographr --locked` works in
a scratch CARGO_HOME, tag `vX.Y.Z` pushed.

---

## Phase 3 — Build binaries (two machines)

Both machines run the same dry-run flow. `scripts/release.sh`:

- reads the version from `Cargo.toml`,
- guards against a dirty tree,
- builds with `--features pdf,metrics` (the workspace patch hardens the PDF
  parser for the shipped binaries even though crates.io doesn't get it, and
  the `/metrics` endpoint stays available as the README advertises),
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

### Homebrew tap (`ethernomad/homebrew-choreographr`)

Edit `Formula/choreographr.rb` (mirrored in this repo at
`packaging/homebrew/choreographr.rb`):

1. Bump `version` to `X.Y.Z`.
2. Update the `url` line — tag, filename, and embedded version.
3. Recompute the digest: `curl -fL -O <url> && shasum -a 256 <downloaded>.tar.gz`.
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
| Homebrew | `brew tap ethernomad/choreographr && brew install choreographr` | no quarantine friction |
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
- [ ] `cargo release publish` → 12 crates on crates.io; `cargo install --locked` verified
- [ ] Linux box: `just release` + `just smoke-test` → musl tarball, `.deb`, `.rpm`, `SHA256SUMS`
- [ ] MacBook: `just release` + `just smoke-test` + daemon/keystore/plist/TUI smoke test
- [ ] macOS tarball copied to Linux box; combined `SHA256SUMS` regenerated
- [ ] `just release-upload` → `gh release create vX.Y.Z` with all 5 assets
- [ ] Homebrew formula bumped + pushed to tap; `brew install` verified
- [ ] AUR `pkgver`/`sha256sums` bumped, `.SRCINFO` regenerated, pushed
- [ ] choreographr.com: `install.sh`, `/download/vX.Y.Z/` redirects, `/releases/SHA256SUMS`
- [ ] All install routes verified (`cargo install`/`binstall`, brew, AUR, curl, .deb, .rpm)
- [ ] Service policy confirmed: installed, never auto-enabled
