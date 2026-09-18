# Choreographr — Homebrew formula for the choreographr/homebrew-choreographr tap.
#
# Bump procedure per release (keep in lockstep with scripts/release.sh and
# packaging/aur/PKGBUILD): scripts/update-homebrew-tap.sh automates steps 1-3
# and 5 (dry-run by default; --push commits + pushes). Step 4 is verifiable in
# CI via the `homebrew-verify` workflow (a macOS arm64 runner that runs the
# install + `--version` check), so it no longer needs a physical Mac.
#   1. Bump `version` to the new release tag (e.g. 0.1.1).
#   2. Update both `url` lines — tag, filename, and embedded version.
#   3. Recompute the checksums and paste them into the `sha256` fields:
#        curl -fL -O <url> && shasum -a 256 <downloaded>.tar.gz
#      (scripts/update-homebrew-tap.sh automates this for BOTH branches; both
#      darwin tarballs are required — the script aborts if either is missing.)
#   4. Sanity-check locally: brew install ./choreographr.rb && choreographr --version
#   5. Commit and push to the tap repo.
#
# This is the prebuilt-tarball variant: no build toolchain is required, the
# binaries ship as-is from the GitHub release.
class Choreographr < Formula
  desc "Agentic coding assistant — daemon, TUI, and bridges"
  homepage "https://choreographr.com"
  version "0.2.1"

  # Both macOS targets ship: aarch64 (native) and x86_64 (cross-built with
  # target-cpu=x86-64-v3 on the arm64 CI host — see scripts/release.sh). brew
  # selects the branch at install time via Hardware::CPU, matching the
  # installer's own Homebrew prefix (/opt/homebrew vs /usr/local).
  if Hardware::CPU.arm?
    url "https://github.com/choreographr/choreographr/releases/download/v0.2.1/choreographr-0.2.1-aarch64-apple-darwin.tar.gz"
    sha256 "efe730748688c5c7e665bb7f56b155c0b60fe19c784e2d64ef11d455cb77f7f2"
  else
    # x86_64-apple-darwin: cross-built by release.sh on the arm64 CI host.
    url "https://github.com/choreographr/choreographr/releases/download/v0.2.1/choreographr-0.2.1-x86_64-apple-darwin.tar.gz"
    sha256 "9c3506fb1ff3cc83dda0c7430dde92fb9b5fb580ea21d1596a27bf51b1c6fe29"
  end

  def install
    # The shipped release binaries sit at the tarball root (see
    # scripts/release.sh). The IM/ACP bridges are feature-gated and not in the
    # tarball; choreo-mcp is a library-only crate — it ships no binary. Keep this
    # list in lockstep with the release build's binary set.
    bin.install "choreographr", "choreo-tui"
  end

  # Homebrew-managed launchd service (`brew services start choreographr`).
  # This is the Homebrew path; non-Homebrew installs use the launchd agent in
  # packaging/com.choreographr.daemon.plist instead. Still "never auto-enabled":
  # `brew services` only starts it on explicit user request.
  service do
    run [opt_bin/"choreographr"]
    keep_alive true
    log_path var/"log/choreographr/choreographr.log"
    error_log_path var/"log/choreographr/choreographr.log"
  end

  test do
    # The clap bare `version` marker makes --version print the package version
    # and exit 0; assert the version so formula/version drift fails the test.
    assert_match version.to_s, shell_output("#{bin}/choreographr --version")
  end
end
