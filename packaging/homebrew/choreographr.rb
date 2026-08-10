# Choreographr — Homebrew formula for the choreographr/homebrew-choreographr tap.
#
# Bump procedure per release (keep in lockstep with scripts/release.sh and
# packaging/aur/PKGBUILD):
#   1. Bump `version` to the new release tag (e.g. 0.1.1).
#   2. Update both `url` lines — tag, filename, and embedded version.
#   3. Recompute the checksums and paste them into the `sha256` fields:
#        curl -fL -O <url> && shasum -a 256 <downloaded>.tar.gz
#   4. Sanity-check locally: brew install ./choreographr.rb && choreographr --version
#   5. Commit and push to the tap repo.
#
# This is the prebuilt-tarball variant: no build toolchain is required, the
# binaries ship as-is from the GitHub release.
class Choreographr < Formula
  desc "Agentic coding assistant — daemon, TUI, and bridges"
  homepage "https://choreographr.com"
  version "0.1.0"

  # arm64 (Apple Silicon) is the 0.1.0 macOS target. The x86_64 branch is a
  # placeholder: Intel macOS tarballs are not shipped in 0.1, but keeping the
  # branch means adding them later is a one-digest change rather than a
  # formula restructure.
  if Hardware::CPU.arm?
    url "https://github.com/choreographr/choreographr/releases/download/v0.1.0/choreographr-0.1.0-aarch64-apple-darwin.tar.gz"
    sha256 "<sha256-aarch64>"
  else
    # x86_64 macOS is not shipped in 0.1 — kept for future-proofing.
    url "https://github.com/choreographr/choreographr/releases/download/v0.1.0/choreographr-0.1.0-x86_64-apple-darwin.tar.gz"
    sha256 "<sha256-x86_64>"
  end

  def install
    # All four release binaries sit at the tarball root (see scripts/release.sh).
    # (choreo-mcp is a library-only crate — it ships no binary.)
    bin.install "choreographr", "choreo-tui", "choreo-im", "choreo-acp"
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
