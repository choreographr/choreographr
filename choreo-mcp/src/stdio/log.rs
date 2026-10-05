//! Per-server `stderr` capture for a stdio MCP server child.
//!
//! A stdio server's own `stderr` is drained into a size-capped, owner-only log
//! file so its diagnostics are isolated per server (the path is chosen by the
//! daemon's `config::server_log_path`) rather than mixed into the daemon's log.
//! The drain is generic over the reader (rather than taking a concrete
//! `ChildStderr`) so the truncate/rotate/append logic is unit-testable without a
//! real child process, and runs on the sidecar runtime. This is a child module
//! of `stdio`; the capped JSON-RPC transport itself lives in the parent module.

use std::path::{Path, PathBuf};
use tokio::process::ChildStderr;

/// Maximum size of a per-server log file before it is truncated and restarted.
///
/// A server's own stderr can be arbitrarily chatty; capping the file keeps one
/// verbose server from filling the disk. When the cap is reached the file is
/// emptied and logging continues from the start (a simple single-file rotation —
/// there is no history, just a bounded live log).
const MAX_SERVER_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Bytes read from the child's stderr per drain iteration.
const STDERR_CHUNK: usize = 8192;

/// Best-effort owner-only permissions (0600) on a per-server log file.
///
/// A server's stderr may echo operator-configured values (a header, a token),
/// so the log must not be group- or world-readable. Mirrors the trust store's
/// `set_file_private`; a no-op on non-Unix platforms, where the default ACL on
/// a newly created file already scopes it to the creating user.
#[cfg(unix)]
fn set_file_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) {}

/// Drain `stderr` into `path`, size-capped, until the reader reaches EOF.
///
/// Generic over the reader (rather than taking a concrete `ChildStderr`) so the
/// truncate/rotate/append logic is unit-testable without a real child process.
/// A pre-existing oversized file is truncated so a restart starts clean; the
/// file is re-created at the cap boundary (a simple single-file rotation).
async fn drain_stderr_to_log<R: tokio::io::AsyncRead + Unpin>(mut stderr: R, path: PathBuf) {
    // Truncate a pre-existing oversized file so a restart starts clean.
    let mut written = match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.len() >= MAX_SERVER_LOG_BYTES => 0,
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };
    if written == 0 {
        let _ = tokio::fs::write(&path, b"").await;
        // Tighten the freshly created file to owner-only immediately: it may
        // hold server stderr with operator secrets (see the parent module's
        // `sanitize_child_env` residual-exposure note).
        set_file_private(&path);
    }
    let open = || {
        let path = path.clone();
        async move {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .ok();
            // Re-assert owner-only on every open, so a file created earlier
            // under a laxer umask (or by the rotation reopen below) is
            // tightened too.
            if file.is_some() {
                set_file_private(&path);
            }
            file
        }
    };
    let mut file = open().await;
    let mut buf = [0u8; STDERR_CHUNK];
    loop {
        match tokio::io::AsyncReadExt::read(&mut stderr, &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Rotate before writing when the cap is reached, so the file
                // never exceeds it by more than one chunk.
                if written >= MAX_SERVER_LOG_BYTES {
                    let _ = tokio::fs::write(&path, b"").await;
                    file = open().await;
                    written = 0;
                }
                if let Some(file) = file.as_mut() {
                    // The cap fits any platform's `usize`; the fallback
                    // keeps the arithmetic total regardless.
                    let remaining =
                        usize::try_from(MAX_SERVER_LOG_BYTES - written).unwrap_or(usize::MAX);
                    let take = n.min(remaining);
                    let Some(chunk) = buf.get(..take) else {
                        continue;
                    };
                    if tokio::io::AsyncWriteExt::write_all(file, chunk)
                        .await
                        .is_ok()
                    {
                        written += take as u64;
                    }
                }
            }
        }
    }
    // Flush the last buffered write so the log is durable the moment the child
    // exits, rather than left to the file's asynchronous Drop flush.
    if let Some(file) = file.as_mut() {
        let _ = tokio::io::AsyncWriteExt::flush(file).await;
    }
}

/// Capture a stdio server's `stderr` into `path`, size-capped.
///
/// Runs on the sidecar runtime (see [`drain_stderr_to_log`], which owns the
/// drain/rotation logic); the task ends when the child closes its stderr
/// (i.e. on exit), so it needs no explicit teardown. When the sidecar runtime is
/// unavailable it builds a short-lived current-thread runtime on a dedicated
/// thread so the stream is still drained rather than left to block the child on
/// a full pipe.
pub(super) fn spawn_stderr_logger(stderr: ChildStderr, path: PathBuf) {
    let drain = drain_stderr_to_log(stderr, path);

    match crate::runtime::handle() {
        Ok(handle) => {
            handle.spawn(drain);
        }
        Err(_) => {
            // No sidecar runtime (unreachable in practice — the transport is
            // built inside `runtime::block_on`). Drive the drain on a
            // short-lived thread with its OWN current-thread runtime: there is
            // no runtime to run it on otherwise, and dropping `drain` unpolled
            // would leave the child's stderr unconsumed (a full pipe can then
            // block the child). Building the runtime here guarantees the drain
            // actually runs.
            std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(drain);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a read on the sidecar runtime, the same context the real drain
    /// runs in.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        crate::runtime::init().expect("runtime init");
        crate::runtime::block_on(fut).expect("runtime available")
    }

    /// The per-server stderr log is created owner-only (0600) on Unix: it may
    /// hold server stderr with operator-configured secrets.
    #[test]
    #[cfg(unix)]
    fn stderr_log_file_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-fresh.log");

        block_on(drain_stderr_to_log(&b"server started\n"[..], path.clone()));

        let mode = std::fs::metadata(&path)
            .expect("log file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a fresh per-server log must be owner-only");
        assert_eq!(std::fs::read(&path).expect("read log"), b"server started\n");
    }

    /// An existing log left group/world-readable (a laxer umask on a previous
    /// run) is re-tightened to 0600 when it is next opened, and un-rotated
    /// bytes below the cap are preserved (single-file rotation truncates only
    /// at the cap).
    #[test]
    #[cfg(unix)]
    fn stderr_log_file_is_tightened_on_reopen() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-existing.log");
        std::fs::write(&path, b"earlier\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        block_on(drain_stderr_to_log(&b"more\n"[..], path.clone()));

        let mode = std::fs::metadata(&path)
            .expect("log file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "an existing log must be re-tightened on open");
        assert_eq!(
            std::fs::read(&path).expect("read log"),
            b"earlier\nmore\n",
            "bytes below the cap are appended, not truncated"
        );
    }
}
