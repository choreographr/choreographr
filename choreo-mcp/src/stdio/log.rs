//! Per-server `stderr` capture for a stdio MCP server child.
//!
//! A stdio server's own `stderr` is drained into a size-capped, owner-only log
//! file so its diagnostics are isolated per server (the path is chosen by the
//! daemon's `config::server_log_path`) rather than mixed into the daemon's log.
//!
//! The drain runs on a dedicated **blocking** thread over `std::io`/`std::fs`,
//! NOT on the `rmcp` sidecar tokio runtime: only `rmcp` earns the async runtime
//! (see `runtime`), and a log file is a slow blocking sink with no reason to
//! share it (`tokio::fs` would only shunt the blocking write onto the runtime's
//! blocking pool). To get a blocking handle, the child's stderr fd is pulled out
//! of the `tokio::process::ChildStderr` and put back into blocking mode — tokio
//! sets its process pipes non-blocking for its reactor.
//!
//! The drain is generic over the reader (rather than taking a concrete
//! `ChildStderr`) so the create/rotate/append logic is unit-testable without a
//! real child process.

use std::io::{Read, Write};
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

/// Drain `reader` into `path`, size-capped, until EOF. Blocking; runs on a
/// dedicated thread.
///
/// Generic over the reader so the create/rotate/append logic is unit-testable
/// without a real child process. A pre-existing oversized file is truncated so a
/// restart starts clean; the file is re-created at the cap boundary (a simple
/// single-file rotation). The file is opened in append mode, so after a
/// truncation writes resume from the start without seeking.
fn drain_stderr_to_log<R: Read>(mut reader: R, path: &Path) {
    let Ok(mut file) = choreo_shared::logging::open_log_append(path) else {
        return;
    };
    let mut written = file.metadata().map_or(0, |m| m.len());
    if written >= MAX_SERVER_LOG_BYTES {
        let _ = file.set_len(0);
        written = 0;
    }
    let mut buf = [0u8; STDERR_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Rotate before writing when the cap is reached, so the file
                // never exceeds it by more than one chunk.
                if written >= MAX_SERVER_LOG_BYTES {
                    let _ = file.set_len(0);
                    written = 0;
                }
                // The cap fits any platform's `usize`; the fallback keeps the
                // arithmetic total regardless.
                let remaining =
                    usize::try_from(MAX_SERVER_LOG_BYTES - written).unwrap_or(usize::MAX);
                let take = n.min(remaining);
                if let Some(chunk) = buf.get(..take)
                    && file.write_all(chunk).is_ok()
                {
                    written += take as u64;
                }
            }
        }
    }
    // Flush the last buffered write so the log is durable the moment the child
    // exits, rather than left to the file's asynchronous Drop flush.
    let _ = file.flush();
}

/// Capture a stdio server's `stderr` into `path`, size-capped, on a dedicated
/// blocking thread.
///
/// Takes the raw fd out of the tokio handle (which deregisters it from the
/// reactor), switches it back to blocking mode, and hands it to a std thread
/// running [`drain_stderr_to_log`]. The thread ends when the child closes stderr
/// (i.e. on exit), so it needs no explicit teardown.
pub(super) fn spawn_stderr_logger(stderr: ChildStderr, path: PathBuf) {
    #[cfg(unix)]
    let reader = {
        let Ok(fd) = stderr.into_owned_fd() else {
            return;
        };
        set_blocking(&fd);
        std::fs::File::from(fd)
    };
    #[cfg(not(unix))]
    let reader = {
        let Ok(handle) = stderr.into_owned_handle() else {
            return;
        };
        std::fs::File::from(handle)
    };
    // A detached thread; it ends when the child closes stderr. Use the
    // fallible builder so a thread-spawn failure degrades to no file logging
    // rather than panicking the daemon (which owns this process).
    let _ = std::thread::Builder::new()
        .name("mcp-stderr-log".into())
        .spawn(move || drain_stderr_to_log(reader, &path));
}

/// Clear `O_NONBLOCK` on `fd` so a blocking `read` does not return `WouldBlock`
/// (tokio's process pipes are non-blocking by default).
#[cfg(unix)]
fn set_blocking(fd: &std::os::fd::OwnedFd) {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    if let Ok(flags) = fcntl_getfl(fd) {
        let _ = fcntl_setfl(fd, flags - OFlags::NONBLOCK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-server stderr log is created owner-only (0600) on Unix: it may
    /// hold server stderr with operator-configured secrets.
    #[test]
    #[cfg(unix)]
    fn stderr_log_file_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-fresh.log");

        drain_stderr_to_log(&b"server started\n"[..], &path);

        let mode = std::fs::metadata(&path)
            .expect("log file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a fresh per-server log must be owner-only");
        assert_eq!(std::fs::read(&path).expect("read log"), b"server started\n");
    }

    /// A symlink planted at the (predictable) log path must be refused, so a
    /// server's captured stderr never lands in an attacker-chosen file.
    #[test]
    #[cfg(unix)]
    fn stderr_log_refuses_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("victim");
        std::fs::write(&target, b"do not touch").expect("write target");
        let link = dir.path().join("mcp-link.log");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        drain_stderr_to_log(&b"secret\n"[..], &link);

        assert_eq!(
            std::fs::read(&target).expect("read target"),
            b"do not touch",
            "the symlink target must be untouched"
        );
    }

    /// An existing log left group/world-readable (a laxer umask on a previous
    /// run) is re-tightened to 0600 when it is next opened, and bytes below the
    /// cap are appended, not truncated.
    #[test]
    #[cfg(unix)]
    fn stderr_log_file_is_tightened_on_reopen() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp-existing.log");
        std::fs::write(&path, b"earlier\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        drain_stderr_to_log(&b"more\n"[..], &path);

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
