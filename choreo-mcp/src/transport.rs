use crate::error::McpError;
use crate::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
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

    /// Stop the reader thread and kill the subprocess.
    pub fn shutdown(&mut self) {
        if let Ok(mut guard) = self.shutdown.lock() {
            *guard = true;
        }
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        self.stdin = None;
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}
