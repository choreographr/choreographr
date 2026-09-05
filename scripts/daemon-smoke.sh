#!/usr/bin/env bash
# daemon-smoke.sh — boots the SHIPPED daemon from a release artifact and
# proves the socket listener actually comes up, hermetically.
#
#   Usage: daemon-smoke.sh <archive>
#     e.g. daemon-smoke.sh dist/choreographr-0.1.0-x86_64-unknown-linux-musl.tar.gz
#          daemon-smoke.sh dist/choreographr-0.1.0-x86_64-pc-windows-msvc.zip
#
# Why this exists on top of smoke-test.sh: that script only exercises
# --version/--help (clap paths that never touch the daemon). A musl/MSVC/darwin
# build can pass all of that and still die at startup — bad static-linking of
# the keypair/DB paths, an env-dependent dirs::config_dir() resolution, a
# missing syscall in the release profile. This script catches exactly that by
# running the daemon the way a user would, with every piece of mutable state
# redirected into a scratch dir:
#
#   * CHOREOGRAPHR_SOCKET_PATH → scratch socket (choreo-proto/src/io.rs)
#   * XDG_CONFIG_HOME / APPDATA → scratch config dir (transport keypair
#     transport.sec/pub, authorized_clients.toml, config.toml, accounts.toml
#     all resolve under dirs::config_dir()/choreographr)
#   * HOME / USERPROFILE → scratch too, belt-and-braces against anything that
#     bypasses dirs and reads $HOME directly
#
# Scope of the "prove serve" check (deliberate, not laziness): the shipped
# artifact set is the daemon (a server) and choreo-tui — and the TUI has NO
# non-interactive mode. Its CLI only takes TCP flags (--tcp-addr/
# --server-pk/--trust-fingerprint, which are for the Noise XX first-contact
# flow), and after that flow it enters crossterm raw mode, which cannot run
# headless. So a full Noise-encrypted client round-trip is NOT achievable with
# shipped binaries alone and is NOT faked here. The check is:
#   * the socket endpoint exists (a real Unix socket on Linux/macOS; on
#     Windows the daemon binds via uds_windows, whose endpoint is likewise a
#     FILE, not a named pipe — hence test -e, not Test-Path \\.\pipe\),
#   * the daemon log shows the "choreographr listening" line,
#   * on Linux/macOS a plain localhost socket connect succeeds when python3
#     is available (proves something is accepting, not just that the file
#     exists), and
#   * the daemon stays alive through all of it.
#
# The Windows Job Object kill-switch (choreo-daemon's tools/shell_util.rs)
# is likewise NOT exercised here: driving a shell-tool request through the
# server pipeline requires a connected, authenticated client, which the
# shipped binaries cannot provide headless (see above). It remains covered by
# the crate's unit/integration tests.
#
# Polling note: the readiness loop sleeps 0.1s per iteration — a time-based
# wait. That does not violate the workspace's no-sleeps-in-tests discipline:
# that rule is about determinism in Rust unit tests, whereas this is CI
# orchestration around a real multi-process boot, where polling a filesystem
# event (the socket appearing) is the only available synchronization.
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <archive>" >&2
    echo "  e.g. $0 dist/choreographr-0.1.0-x86_64-unknown-linux-musl.tar.gz" >&2
    echo "       $0 dist/choreographr-0.1.0-x86_64-pc-windows-msvc.zip" >&2
    exit 1
fi
ARCHIVE="$1"
[ -f "$ARCHIVE" ] || { echo "error: archive not found: $ARCHIVE" >&2; exit 1; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The version assertion mirrors smoke-test.sh: --version must report the
# workspace manifest's version, and the fallback keeps that meaningful if the
# script runs outside the checkout.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)"
VERSION="${VERSION:-0.1.0}"

# Git Bash on windows-latest reports MINGW*_NT; Linux and macOS take the POSIX
# branches. Only Windows differs in endpoint checking, shutdown, and env vars.
case "$(uname -s)" in
    MINGW*) OS=windows ;;
    *)      OS=posix  ;;
esac

TMP="$(mktemp -d)"
SCRATCH_CONFIG="$TMP/config"
mkdir -p "$SCRATCH_CONFIG"
DAEMON_LOG="$TMP/daemon.log"
FAIL=0
DAEMON_PID=""

# dump_log prints the captured daemon output — required on EVERY failure path
# (the daemon's own error lines are usually the entire diagnosis).
dump_log() {
    echo "───── daemon log ($DAEMON_LOG) ─────" >&2
    cat "$DAEMON_LOG" >&2 2>/dev/null || echo "(log file missing or unreadable)" >&2
    echo "─────────────────────────────────────" >&2
}

cleanup() {
    # Last-resort reaping: every normal path stops the daemon explicitly; this
    # covers a mid-script set -e abort so the runner is never left with an
    # orphaned daemon holding the scratch socket.
    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

echo "==> extracting $ARCHIVE"
case "$ARCHIVE" in
    *.tar.gz) tar -xzf "$ARCHIVE" -C "$TMP" ;;
    *.zip)    unzip -q "$ARCHIVE" -d "$TMP" ;;
    *) echo "error: unsupported archive type (want .tar.gz or .zip): $ARCHIVE" >&2; exit 1 ;;
esac

# Binary name: the zip ships .exe files at archive root, the tarballs ship
# extensionless ELF/Mach-O binaries.
if [ "$OS" = windows ]; then
    DAEMON="$TMP/choreographr.exe"
    # Windows resolves config_dir() from %APPDATA% (FOLDERID_RoamingAppData is
    # not env-driven, but dirs falls back to %APPDATA% when the known-folder
    # API is unavailable in this context) — set it alongside USERPROFILE.
    export APPDATA="$SCRATCH_CONFIG"
    export USERPROFILE="$TMP"
else
    DAEMON="$TMP/choreographr"
    # macOS has no XDG_CONFIG_HOME by default but dirs::config_dir() honors it
    # when set; on Linux it is the primary knob. HOME backs both up.
    export XDG_CONFIG_HOME="$SCRATCH_CONFIG"
    export HOME="$TMP"
fi
export CHOREOGRAPHR_SOCKET_PATH="$TMP/choreographr.sock"
# Pin the log level: the readiness check greps for the daemon's own
# "choreographr listening" line, which the default (info) filter emits —
# setting RUST_LOG explicitly keeps that contract independent of flag defaults.
export RUST_LOG=info

[ -x "$DAEMON" ] || { echo "error: $DAEMON missing or not executable after extraction" >&2; exit 1; }

echo "==> $DAEMON --version must report $VERSION"
VER_OUT="$("$DAEMON" --version 2>&1)" || {
    echo "  FAIL: daemon --version exited non-zero" >&2
    exit 1
}
case "$VER_OUT" in
    *"$VERSION"*) echo "  ok: $VER_OUT" ;;
    *)
        echo "  FAIL: --version did not report $VERSION: $VER_OUT" >&2
        FAIL=1
        ;;
esac

echo "==> booting daemon (socket: $CHOREOGRAPHR_SOCKET_PATH, config: $SCRATCH_CONFIG)"
# stdout+stderr both go to the log: tracing writes to stdout, and the "listen"
# line plus any startup panic land here for the readiness check and dump_log.
"$DAEMON" >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!

# Readiness poll — bounded (100 × 0.1s = 10s ceiling), NOT a fixed sleep.
# Each iteration first checks the process is still alive: a daemon that exits
# during startup must fail IMMEDIATELY with its log dumped, not after the full
# timeout. (See the header comment for why a sleep-based poll is acceptable
# here.)
SOCKET_UP=0
for _ in $(seq 1 100); do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "  FAIL: daemon exited during startup (before socket appeared)" >&2
        dump_log
        exit 1
    fi
    # Unix: the bind creates a real socket inode. Windows: uds_windows creates
    # a regular rendezvous FILE at the path (not a named pipe), so test -e.
    if [ "$OS" = windows ]; then
        if [ -e "$CHOREOGRAPHR_SOCKET_PATH" ]; then SOCKET_UP=1; break; fi
    else
        if [ -S "$CHOREOGRAPHR_SOCKET_PATH" ]; then SOCKET_UP=1; break; fi
    fi
    sleep 0.1
done

if [ "$SOCKET_UP" -ne 1 ]; then
    echo "  FAIL: socket did not appear within 10s" >&2
    dump_log
    exit 1
fi
echo "  ok: socket endpoint exists; daemon alive"

if ! grep -q "choreographr listening" "$DAEMON_LOG"; then
    echo "  FAIL: daemon log lacks the \"choreographr listening\" line" >&2
    dump_log
    exit 1
fi
echo "  ok: daemon log reports the listener started"

# Plain socket connect (Linux/macOS, best-effort on tooling availability): the
# socket file existing only proves the bind ran; a successful connect proves
# the accept queue is live. python3's socket module handles AF_UNIX directly;
# when it is absent, the file+log+process-liveness checks above stand as the
# evidence (they are sufficient — see the scope note in the header).
if [ "$OS" != windows ] && command -v python3 >/dev/null 2>&1; then
    if python3 - "$CHOREOGRAPHR_SOCKET_PATH" <<'PYEOF'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2)
s.connect(sys.argv[1])
s.close()
PYEOF
    then
        echo "  ok: plain socket connect succeeded"
    else
        echo "  FAIL: socket exists but connect failed" >&2
        dump_log
        exit 1
    fi
fi

# Hermeticity proof: `fingerprint` with no argument reads THIS config dir's
# auto-generated transport.pub (see choreo-daemon/src/cli.rs). It succeeding
# under the scratch env proves the daemon's keypair landed in the scratch
# config dir and the daemon never touched the runner's real one.
echo "==> daemon fingerprint (proves config-dir hermeticity)"
if "$DAEMON" fingerprint >"$TMP/fingerprint.out" 2>"$TMP/fingerprint.err"; then
    # tracing writes to stdout, so the fingerprint line arrives mixed with
    # INFO events; the fingerprint is the last line that is not a log event.
    FP="$(grep -v ' INFO \| WARN \| ERROR ' "$TMP/fingerprint.out" | tail -n1)"
    echo "  ok: fingerprint from scratch config dir: $FP"
else
    echo "  FAIL: fingerprint failed against the scratch config dir" >&2
    cat "$TMP/fingerprint.err" >&2 || true
    dump_log
    exit 1
fi

# Shutdown. POSIX: SIGTERM and REQUIRE exit 0 — the daemon installs a SIGTERM
# handler, so a nonzero exit after TERM is a crash (e.g. a panic in teardown),
# which is exactly what this check exists to catch.
# Windows: Git Bash's kill -TERM cannot be delivered to a native process, and
# taskkill //PID needs a Windows pid while $! is an MSYS pid — the pragmatic
# equivalent here is kill -9 (MSYS maps it to TerminateProcess), so the exit
# code is EXPECTED to be nonzero on Windows and is not asserted there.
echo "==> stopping daemon"
kill "$DAEMON_PID" 2>/dev/null || true
if [ "$OS" != windows ]; then
    WAIT_ATTEMPTS=0
    while kill -0 "$DAEMON_PID" 2>/dev/null; do
        WAIT_ATTEMPTS=$((WAIT_ATTEMPTS + 1))
        if [ "$WAIT_ATTEMPTS" -gt 50 ]; then
            echo "  FAIL: daemon did not exit within 5s of SIGTERM" >&2
            dump_log
            kill -9 "$DAEMON_PID" 2>/dev/null || true
            exit 1
        fi
        sleep 0.1
    done
    set +e
    wait "$DAEMON_PID"
    EXIT_CODE=$?
    set -e
    if [ "$EXIT_CODE" -ne 0 ]; then
        echo "  FAIL: daemon exited with code $EXIT_CODE on SIGTERM (expected 0)" >&2
        dump_log
        exit 1
    fi
    echo "  ok: daemon exited 0 on SIGTERM"
else
    # Force-kill the whole path on Windows (see comment above the block).
    kill -9 "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    echo "  ok: daemon terminated (Windows force-kill; exit code not asserted)"
fi

if [ "$FAIL" -ne 0 ]; then
    dump_log
    echo "DAEMON SMOKE TEST FAILED" >&2
    exit 1
fi
echo "DAEMON SMOKE TEST PASSED"
