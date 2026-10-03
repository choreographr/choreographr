//! A capped stdio transport for a child-process MCP server.
//!
//! `rmcp`'s own `TokioChildProcess` reads newline-delimited JSON with no bound:
//! its `AsyncRwTransport` grows its line buffer without limit, so a hostile or
//! buggy server that never sends a newline (or sends one gigantic line) can make
//! the client allocate until it is killed. This module reimplements that
//! transport over a [`BoundedLineReader`] that fails the read once a single
//! frame exceeds [`MAX_STDIO_FRAME_BYTES`], so a malformed server is dropped
//! instead of exhausting memory.
//!
//! The transport is a thin wrapper around `rmcp`'s own
//! [`AsyncRwTransport`](rmcp::transport::async_rw::AsyncRwTransport): it owns
//! the spawned child (so the process tree is killed on close, matching the
//! child-process transport) and delegates the JSON-RPC framing to rmcp. Only the
//! read side differs — the write side and the framing are rmcp's unchanged.

use crate::config::{McpServerConfig, McpTransport};
use crate::error::McpError;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::{ChildStdin, ChildStdout};

/// Maximum size, in bytes, of a single newline-delimited JSON frame read from a
/// stdio server.
///
/// A frame larger than this is treated as a protocol violation: the read fails
/// and the transport closes, so the bounded restart policy can rebuild the
/// connection. 8 MiB is far larger than any legitimate JSON-RPC message while
/// keeping a single hostile line from consuming unbounded memory.
pub const MAX_STDIO_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Bytes read from the inner reader per `poll_read`, independently of the
/// caller's buffer size.
///
/// Bounding each syscall keeps the adapter from handing a huge slice on to the
/// framing layer, and lets the per-frame byte count advance smoothly so the cap
/// trips at the right point rather than after an oversized read lands.
const READ_CHUNK: usize = 16 * 1024;

/// A reader that fails once more than `limit` bytes have arrived without a
/// newline.
///
/// Once the limit is crossed it fails every subsequent read, so the framing
/// layer above cannot recover by reading on: an oversized frame is a fatal
/// protocol violation, not a hiccup.
struct BoundedLineReader<R> {
    inner: R,
    limit: usize,
    /// Bytes seen since the most recent newline.
    since_newline: usize,
    /// Set once the limit is exceeded; every later read fails immediately.
    breached: bool,
}

impl<R> BoundedLineReader<R> {
    fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            limit,
            since_newline: 0,
            breached: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLineReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.breached {
            return Poll::Ready(Err(frame_too_large(this.limit)));
        }

        // Never read past the remaining allowance for this frame: cap the read
        // to `limit - since_newline` so the buffered line cannot exceed the
        // limit before the error is raised.
        let remaining = this.limit.saturating_sub(this.since_newline);
        if remaining == 0 {
            this.breached = true;
            return Poll::Ready(Err(frame_too_large(this.limit)));
        }
        let allowance = buf.remaining().min(remaining).min(READ_CHUNK);

        let mut scratch = [0u8; READ_CHUNK];
        let Some(scratch_slice) = scratch.get_mut(..allowance) else {
            // Unreachable: `allowance` is always at most `READ_CHUNK`, the
            // scratch length. Handled without a panic regardless.
            return Poll::Ready(Err(frame_too_large(this.limit)));
        };
        let mut read_buf = ReadBuf::new(scratch_slice);
        match Pin::new(&mut this.inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled();
                // Track the byte offset of the last newline in this chunk; a
                // chunk with no newline simply extends the current frame.
                match filled.iter().rposition(|&byte| byte == b'\n') {
                    Some(last) => this.since_newline = filled.len() - last - 1,
                    None => this.since_newline += filled.len(),
                }
                buf.put_slice(filled);
                Poll::Ready(Ok(()))
            }
            // Pending / Err pass through unchanged; the framing layer decides
            // how to treat the error.
            other => other,
        }
    }
}

/// The I/O error raised when a frame exceeds [`MAX_STDIO_FRAME_BYTES`].
fn frame_too_large(limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("stdio frame exceeded the {limit}-byte limit"),
    )
}

/// A capped stdio transport plus the child process it owns.
///
/// Owns the spawned child so it can be killed on `close` (and on `Drop`, so a
/// transport dropped without a close — e.g. the service loop errored — does not
/// leak the process tree).
pub(crate) struct StdioTransport {
    child: Option<Box<dyn ChildWrapper>>,
    inner: AsyncRwTransport<RoleClient, BoundedLineReader<ChildStdout>, ChildStdin>,
}

impl StdioTransport {
    /// Spawn the configured subprocess and wrap its stdio in a capped transport.
    ///
    /// On Unix the child is placed in its own process group so launcher chains
    /// (`npx` → `node`) are killed together on shutdown rather than orphaning
    /// the grandchild that holds the pipe.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::SpawnFailed`] when the subprocess cannot be spawned
    /// or its stdio pipes cannot be captured, and [`McpError::ProtocolError`]
    /// when called for a non-stdio server.
    pub(crate) fn spawn(config: &McpServerConfig) -> Result<Self, McpError> {
        let McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } = &config.transport
        else {
            return Err(McpError::ProtocolError(
                "StdioTransport::spawn called for a non-stdio server".into(),
            ));
        };

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        // An explicit executable + args only — never a shell string — so config
        // values cannot be reinterpreted as shell syntax.
        for (key, value) in env {
            cmd.env(key, value);
        }
        // A configured working directory is applied to the child only; it must
        // exist, or the spawn fails with a clear error rather than the child
        // silently inheriting the daemon's cwd.
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // stdin/stdout are the JSON-RPC channel; stderr stays inherited so the
        // server's own logging reaches the daemon's stderr.
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());

        let mut wrap = CommandWrap::from(cmd);
        #[cfg(unix)]
        wrap.wrap(ProcessGroup::leader());
        let mut child = wrap
            .spawn()
            .map_err(|e| McpError::SpawnFailed(e.to_string()))?;

        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| McpError::SpawnFailed("child stdin was not captured".into()))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| McpError::SpawnFailed("child stdout was not captured".into()))?;

        let reader = BoundedLineReader::new(stdout, MAX_STDIO_FRAME_BYTES);
        // The write side and the JSON-RPC framing are rmcp's; only the reader is
        // ours, so the transport is otherwise identical to the child-process one.
        let inner = AsyncRwTransport::new_client(reader, stdin);
        Ok(Self {
            child: Some(child),
            inner,
        })
    }

    /// Kill the child process tree, if one is still owned.
    async fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = Box::into_pin(child.kill()).await;
        }
    }
}

impl Transport<RoleClient> for StdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.inner.receive()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        // Close the write side first so the server sees EOF, then reap the
        // process tree.
        let closed = self.inner.close().await;
        self.kill_child().await;
        closed
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        // A transport dropped without a close still owns its child; kill the
        // tree so it does not outlive the connection. The kill is async, so it
        // runs on the sidecar runtime (or the current one if we are already on
        // it).
        let Some(mut child) = self.child.take() else {
            return;
        };
        let kill = async move {
            let _ = Box::into_pin(child.kill()).await;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(kill);
        } else if let Some(rt) = crate::runtime::get() {
            rt.spawn(kill);
        }
        // With no runtime available the kill is skipped: the process is left to
        // the OS. This only happens during teardown, where the alternative
        // (panicking in Drop) is strictly worse.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// Drive a read on the sidecar runtime, the same context the real transport
    /// runs in.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        crate::runtime::init().expect("runtime init");
        crate::runtime::block_on(fut).expect("runtime available")
    }

    #[test]
    fn frames_within_limit_read_through() {
        let input: &[u8] = b"{\"a\":1}\n{\"b\":2}\n";
        let mut reader = BoundedLineReader::new(input, 1024);
        let mut out = Vec::new();
        block_on(async {
            reader
                .read_to_end(&mut out)
                .await
                .expect("frames within the limit read cleanly");
        });
        assert_eq!(out, input);
    }

    #[test]
    fn oversized_frame_fails_instead_of_growing() {
        // 100 bytes with no newline, limit 64: the read must fail rather than
        // buffer the whole (unbounded) frame.
        let input = vec![b'x'; 100];
        let mut reader = BoundedLineReader::new(input.as_slice(), 64);
        let err = block_on(async {
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await
        })
        .expect_err("an oversized frame must fail the read");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn multiple_frames_each_get_their_own_budget() {
        // Two frames each just under the limit: neither trips the cap because
        // the counter resets at every newline.
        let frame = format!("{}\n", "y".repeat(60));
        let input = format!("{frame}{frame}");
        let mut reader = BoundedLineReader::new(input.as_bytes(), 64);
        let mut out = Vec::new();
        block_on(async {
            reader
                .read_to_end(&mut out)
                .await
                .expect("back-to-back frames each fit their own budget");
        });
        assert_eq!(out, input.into_bytes());
    }
}
