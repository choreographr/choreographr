use crate::error::McpError;
use crate::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Manages a subprocess' stdio streams, routing stdout lines into typed
/// channels for responses and notifications.
pub struct StdioTransport {
    stdin: Option<Box<dyn Write + Send>>,
    reader_handle: Option<thread::JoinHandle<()>>,
    response_rx: mpsc::Receiver<(u64, Result<JsonRpcResponse, McpError>)>,
    notification_rx: mpsc::Receiver<JsonRpcNotification>,
    /// Shared flag to signal the reader thread to stop.
    shutdown: Arc<Mutex<bool>>,
    child: Option<Child>,
}

impl StdioTransport {
    /// Spawn a subprocess and connect its stdio.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, McpError> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // On Unix, put the child in its own process group so shutdown can kill
        // the whole tree. MCP servers are frequently launched via launchers
        // (`npx`, `uvx`, ...) that spawn a grandchild which inherits our stdout
        // pipe; killing only the direct child would leave that grandchild alive
        // and holding the pipe open, so the reader thread never sees EOF.
        #[cfg(unix)]
        cmd.process_group(0);

        for (k, v) in env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::SpawnFailed(e.to_string()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::SpawnFailed("failed to capture stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::SpawnFailed("failed to capture stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpError::SpawnFailed("failed to capture stderr".into()))?;

        // Stderr reader — forward to tracing
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => debug!("[mcp-stderr] {l}"),
                    Err(_) => break,
                }
            }
        });

        let (response_tx, response_rx) = mpsc::channel();
        let (notification_tx, notification_rx) = mpsc::channel();
        let shutdown = Arc::new(Mutex::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        // Stdout reader — parse JSON-RPC lines
        let reader_handle = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                if *shutdown_clone.lock().unwrap_or_else(|e| e.into_inner()) {
                    break;
                }
                let line = match line {
                    Ok(l) => l,
                    Err(e) => {
                        warn!("MCP stdout read error: {e}");
                        break;
                    }
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // Try response first (has `id`), then notification.
                if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                    let _ = response_tx.send((resp.id, Ok(resp)));
                } else if let Ok(notif) = serde_json::from_str::<JsonRpcNotification>(trimmed) {
                    let _ = notification_tx.send(notif);
                } else {
                    warn!(line = %trimmed, "unparseable MCP message");
                }
            }
            debug!("MCP reader thread exiting");
        });

        Ok(Self {
            stdin: Some(Box::new(stdin)),
            reader_handle: Some(reader_handle),
            response_rx,
            notification_rx,
            shutdown,
            child: Some(child),
        })
    }

    /// Write a JSON-RPC request to the subprocess' stdin.
    pub fn send_request(&mut self, req: &JsonRpcRequest) -> Result<(), McpError> {
        let json = serde_json::to_string(req)
            .map_err(|e| McpError::ProtocolError(format!("serialization error: {e}")))?;
        let stdin = self.stdin.as_mut().ok_or(McpError::ServerShutdown)?;
        writeln!(stdin, "{json}").map_err(McpError::Io)?;
        stdin.flush().map_err(McpError::Io)?;
        debug!(method = %req.method, id = %req.id, "sent MCP request");
        Ok(())
    }

    /// Write a JSON-RPC notification (no `id` field) to the subprocess' stdin.
    pub fn send_notification(&mut self, notif: &JsonRpcNotification) -> Result<(), McpError> {
        let json = serde_json::to_string(notif)
            .map_err(|e| McpError::ProtocolError(format!("serialization error: {e}")))?;
        let stdin = self.stdin.as_mut().ok_or(McpError::ServerShutdown)?;
        writeln!(stdin, "{json}").map_err(McpError::Io)?;
        stdin.flush().map_err(McpError::Io)?;
        debug!(method = %notif.method, "sent MCP notification");
        Ok(())
    }

    /// Block until a response with the given `id` arrives, draining
    /// notifications that arrive before it.
    pub fn recv_response(
        &self,
        id: u64,
        timeout: std::time::Duration,
    ) -> Result<JsonRpcResponse, McpError> {
        // Drain queued notifications (non-blocking).
        loop {
            match self.notification_rx.try_recv() {
                Ok(notif) => {
                    debug!(method = %notif.method, "received MCP notification");
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // Wait for the matching response.
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(McpError::Timeout);
            }
            match self.response_rx.recv_timeout(remaining) {
                Ok((resp_id, resp)) => {
                    if resp_id == id {
                        return resp;
                    }
                    warn!(
                        expected = %id,
                        got = %resp_id,
                        "MCP response ID mismatch, retrying"
                    );
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(McpError::Timeout),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(McpError::ServerShutdown),
            }
        }
    }

    /// Stop the reader thread and kill the subprocess. Never blocks forever:
    /// every wait is bounded so a misbehaving server cannot wedge shutdown.
    pub fn shutdown(&mut self) {
        debug!("transport shutdown begin");
        if let Ok(mut guard) = self.shutdown.lock() {
            *guard = true;
        }
        if let Some(child) = &mut self.child {
            // The process-group id is only needed on Unix (the child was
            // spawned with process_group(0) so it leads its own group).
            #[cfg(unix)]
            let pid = child.id();
            // Kill the direct child, and on Unix the whole process group so
            // grandchildren (e.g. `node` under `npx`) die too and release the
            // inherited stdout/stderr pipe write-ends.
            let _ = child.kill();
            #[cfg(unix)]
            kill_process_group(pid);

            // Reap the child with a bounded wait.
            wait_bounded(child, Duration::from_secs(5));
        }
        if let Some(handle) = self.reader_handle.take() {
            // Join with a bounded wait. Normally killing the process group makes
            // stdout hit EOF so the reader thread exits on its own; if it is
            // still stuck, detach rather than hang the caller forever.
            join_bounded(handle, Duration::from_secs(5));
        }
        self.stdin = None;
        debug!("transport shutdown end");
    }
}

/// Send SIGKILL to the whole process group led by `pid`.
///
/// The child was spawned with `process_group(0)`, so it is the leader of a
/// fresh process group whose pgid equals the child's pid; a negative kill
/// target addresses that group.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // SAFETY: `-pgid` targets the process group created at spawn time.
    let rc = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if rc != 0 {
        // E.g. ESRCH if the group is already gone — nothing to do.
        warn!(pid, "failed to kill MCP process group");
    }
}

/// Wait for the child to exit, but give up after `timeout`.
fn wait_bounded(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            // Reaped (or already gone) — done.
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            warn!(timeout = ?timeout, "MCP child did not exit within timeout; detaching");
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Join a thread, but give up after `timeout` and leave it detached.
fn join_bounded(handle: thread::JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        if Instant::now() >= deadline {
            warn!(timeout = ?timeout, "MCP reader thread did not exit within timeout; detaching");
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Write` impl that records every byte written so tests can assert on
    /// the exact wire format without spawning a subprocess.
    struct RecordingWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.bytes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Build a transport whose stdin writes go to a shared recording buffer.
    fn transport_with_recording_stdin() -> (StdioTransport, Arc<Mutex<Vec<u8>>>) {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let transport = StdioTransport {
            stdin: Some(Box::new(RecordingWriter {
                bytes: Arc::clone(&bytes),
            })),
            reader_handle: None,
            response_rx: mpsc::channel().1,
            notification_rx: mpsc::channel().1,
            shutdown: Arc::new(Mutex::new(false)),
            child: None,
        };
        (transport, bytes)
    }

    #[test]
    fn send_notification_omits_id_field() {
        // `notifications/*` must be JSON-RPC notifications: no `id` field, or
        // strict servers reject the message as an unknown method request.
        let (mut transport, bytes) = transport_with_recording_stdin();
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: "notifications/initialized".into(),
            params: None,
        };
        transport
            .send_notification(&notif)
            .expect("send should succeed");
        let wire = String::from_utf8(bytes.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .expect("wire should be UTF-8");
        assert_eq!(
            wire.trim(),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "notification must carry no id field"
        );
    }

    #[test]
    fn send_request_keeps_id_field() {
        let (mut transport, bytes) = transport_with_recording_stdin();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: 42,
            method: "tools/list".into(),
            params: None,
        };
        transport.send_request(&req).expect("send should succeed");
        let wire = String::from_utf8(bytes.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .expect("wire should be UTF-8");
        assert!(wire.contains(r#""id":42"#), "request must keep id: {wire}");
    }
}
