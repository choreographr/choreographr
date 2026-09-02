# Packaging

This directory holds the packaging assets for Choreographr releases. The
release scripts that consume them live in [`../scripts`](../scripts), and the
end-to-end release runbook is [`../RELEASE.md`](../RELEASE.md).

All service files follow one policy, stated up front so no installer, package
post-install hook, or release script ever gets clever:

> **Installed, never auto-enabled.**

## Policy: installed, never auto-enabled

The daemon is a *user* service. It needs accounts and API keys configured
before it can do anything useful, and running a background agent is a personal
choice — so no package manager script, installer, or release tool ever enables
it. Every installer:

1. places the unit/agent in the user's own config area —
   `~/.config/systemd/user/` (Linux) or `~/Library/LaunchAgents/` (macOS), and
2. prints the exact one-liner to opt in:

```bash
systemctl --user enable --now choreographr                                       # Linux (systemd)
launchctl load ~/Library/LaunchAgents/com.choreographr.daemon.plist              # macOS (launchd)
```

Consequences: no `%post`/`%preun` in the RPM spec, no `postinst` in the .deb,
no `systemctl enable` in `scripts/install.sh`, and no `RunAtLoad` surprises —
the launchd agent ships with `RunAtLoad`/`KeepAlive` true so it *behaves*
correctly once the user loads it, but loading is always the user's action.

## Assets

| Asset | What it is | Where it is published |
|---|---|---|
| `choreographr.service` | systemd **user** unit (runs `~/.local/bin/choreographr`, `Restart=on-failure`) | inside the release tarball, the desktop `.deb`, and the `.rpm`; installed by `scripts/install.sh`. NOT in the Termux `.deb` — there is no systemd on Android |
| `com.choreographr.daemon.plist` | launchd agent for **non-Homebrew** macOS installs (logs to `/tmp/choreographr.log`) | inside the release tarball; installed by `scripts/install.sh`. Homebrew installs use the formula's `service do` block instead |
| `homebrew/choreographr.rb` | Homebrew formula — prebuilt tarball variant, no build toolchain | the `choreographr/homebrew-choreographr` tap |
| `aur/PKGBUILD` + `aur/.SRCINFO` | Arch package `choreographr-bin` (prebuilt, empty `depends=` — static binaries) | the AUR |
| `rpm/choreographr.spec` | RPM spec for the fat package (four binaries + systemd unit) | consumed by `scripts/build-rpm.sh`; the resulting `.rpm` ships in the GitHub release |

**Termux `.deb` — no tracked asset.** The Termux package's control file is
generated inline by `scripts/build-deb-termux.sh` because it shares nothing
with the desktop package: package `choreographr` with `Architecture: aarch64`
(Termux's tag — NOT Debian's `arm64`), files at `./bin/<name>` (Termux's dpkg
maps that onto `$PREFIX/bin`; zero absolute paths — there is no `/usr` on
Android), no `Depends:` (the binaries are static bionic executables linking
only Android system libs), and no maintainer scripts or conffiles — Termux has
no root (dpkg runs as the app uid) and nothing to configure; runtime state
lives in `~/.local/share/choreographr/`, unpackaged. The archive is forced to
xz compression (`-Zxz`): Termux's dpkg build has no zstd support, while
`dpkg-deb` on the ubuntu runners defaults to zstd since 1.22 — a zstd package
fails on-device with `could not locate member control.tar{xz,lzma,}`. The
"installed, never
auto-enabled" policy above degenerates to "never auto-enabled" here: there is
no init system on Android, so the user starts `choreographr` themselves inside
the Termux session.

## Release tarball

`scripts/release.sh` packs the four binaries — `choreographr choreo-tui
choreo-im choreo-acp` — plus both service files into
`dist/choreographr-<version>-<target>.tar.gz`, with the binaries at the **top
level** of the archive (no `bin/` prefix) and their exec bits preserved. The
Linux tarball is a **fully static `x86_64-unknown-linux-musl`** build (with
mimalloc as the allocator), so one artifact serves general Linux — including
the AUR `choreographr-bin` package — regardless of the host's glibc version;
the macOS tarball is the native `aarch64-apple-darwin` build. The
tarball is published to the GitHub release
(`https://github.com/choreographr/choreographr/releases`) and mirrored at
`https://choreographr.com/download/<version>/`, where a `SHA256SUMS` file sits
beside it for `scripts/install.sh`.

`choreo-mcp` is a **library-only crate** (an MCP client used by the daemon) —
it has no `[[bin]]` target and is **not** shipped as a binary in the tarball
or in any package (.deb/.rpm/Homebrew/AUR).

## Bumping for a release

`scripts/release.sh` automates the mechanical work — version read from the
root `Cargo.toml`, release build, tarball, `SHA256SUMS`, optional
`.deb`/`.rpm`, and prints the `gh release create` command plus the manual
bump checklist. The manual steps that must stay in lockstep:

- **Homebrew** — run `scripts/update-homebrew-tap.sh` (dry-run) then
  `scripts/update-homebrew-tap.sh --push` from the Linux box: it reads the
  version from `Cargo.toml`, recomputes both `sha256` digests from the `dist/`
  tarballs (no re-download), rewrites `homebrew/choreographr.rb` (version,
  both `url` lines, both digests), and commits + pushes to the tap repo. The
  manual fallback: bump `version`, both `url` lines, and both `sha256` values
  (recompute each digest with `shasum -a 256 <downloaded>.tar.gz`) in
  `homebrew/choreographr.rb`, then push to the tap repo.
- **AUR** — bump `pkgver` in `aur/PKGBUILD`, reset `pkgrel` to 1, update the
  `source` URL and `sha256sums`, then regenerate `aur/.SRCINFO`:
  `makepkg --printsrcinfo > .SRCINFO`.
- **crates.io** — `cargo release publish` for the publish-set members in
  dependency order. Prometheus metrics and the blockchain tools are
  feature-gated and **off by default** on crates.io; the native PDF tools are
  unconditional (`pdf-inspector` is a plain registry dependency). Release
  binaries build the optional features via
  `scripts/release.sh --features metrics,blockchain`.
- **choreographr.com** — publish `scripts/install.sh` and add download
  redirects for the new version.

## Notes

- **`.deb` archives are xz-compressed.** `dpkg-deb` ≥ 1.22 defaults to zstd
  members, but both deb flavors must install on dpkg builds without zstd: the
  desktop `.deb` targets old-dpkg distros (Ubuntu 22.04's dpkg 1.21.1), and
  Termux's dpkg has no zstd at all. Both build scripts pass `-Zxz` and assert
  the `debian-binary + control.tar.xz + data.tar.xz` member set.
- **Binaries are stripped.** The workspace `[profile.release]` sets
  `strip = "symbols"` (root `Cargo.toml`), so the tarball, `.deb`, and `.rpm`
  all ship stripped binaries (~22% smaller). (Thin LTO was removed from the
  profile because it made release links slow — see `ARCHITECTURE.md`.) Panic
  `file:line` locations survive (compiled-in
  constants); only `RUST_BACKTRACE=1` symbolization is lost. `panic = "abort"`
  is deliberately NOT set (the daemon catches request-worker panics with
  `catch_unwind`). The RPM spec's `__os_install_post %{nil}` stays so brp
  never re-processes the already-stripped binaries.
- The `-bin` suffix in `choreographr-bin` is an Arch convention **required**
  for prebuilt packages; the plain name is reserved for a source package. A
  future source `choreographr` package with `makedepends=(zig)` is a planned
  option.
- The launchd plist hardcodes `/opt/homebrew/bin/choreographr` (the Homebrew
  layout). Non-Homebrew installs that place binaries elsewhere must edit
  `ProgramArguments` before loading — `scripts/install.sh` prints a reminder
  when it installs the agent.
