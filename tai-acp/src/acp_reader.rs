use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::thread;
use tracing::{debug, error, info};

use crate::acp_jsonrpc;
use crate::daemon_client::Event;
use crate::error::AcpError;

/// Maximum length of a single ACP JSON-RPC line from the editor.
/// Lines longer than this are discarded with a warning to prevent memory
/// exhaustion from a misbehaving or malicious editor.
const MAX_ACP_LINE: usize = 1 << 20; // 1 MiB

/// Spawn a thread that reads newline-delimited JSON-RPC 2.0 messages from
/// stdin, parses them, and sends them as `Event::AcpRequest` values into
/// the shared event channel.
///
/// When stdin reaches EOF the thread sends `Event::AcpEof` and exits.
pub fn spawn_acp_reader(event_tx: mpsc::Sender<Event>) -> Result<thread::JoinHandle<()>, AcpError> {
    thread::Builder::new()
        .name("acp-reader".into())
        .spawn(move || {
            info!("ACP stdin reader thread started");

            // Lock stdin once so we hold the exclusive reference for the
            // lifetime of this thread (avoids concurrent reads).
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin.lock());

            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if line.len() > MAX_ACP_LINE {
                            error!(
                                len = line.len(),
                                max = MAX_ACP_LINE,
                                "ACP input line exceeds maximum length, skipping"
                            );
                            continue;
                        }
                        let trimmed = line.trim().to_string();
                        // Skip blank lines and comment lines.
                        if trimmed.is_empty() || trimmed.starts_with('#') {
                            continue;
                        }
                        debug!(line = %trimmed, "received ACP line");
                        match acp_jsonrpc::parse_request(&trimmed) {
                            Ok(msg) => {
                                if event_tx.send(Event::AcpRequest(msg)).is_err() {
                                    // Main loop has shut down.
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "failed to parse ACP request line");
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "stdin read error");
                        break;
                    }
                }
            }

            let _ = event_tx.send(Event::AcpEof);
            info!("ACP stdin reader thread exiting");
        })
        .map_err(AcpError::Io)
}
