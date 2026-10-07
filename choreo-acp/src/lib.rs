//! The Agent Client Protocol (ACP) bridge for Choreographr.
//!
//! `choreo-acp` is the adapter that lets an ACP-speaking editor (Zed and
//! friends) drive a Choreographr daemon. It speaks newline-delimited JSON-RPC
//! 2.0 on stdin/stdout to the editor and the MessagePack-framed `choreo-proto`
//! protocol to the daemon's Unix socket, translating between the two.
//!
//! ## Module layout
//!
//! - [`acp_handler`] — the single-threaded event loop and the request /
//!   notification / daemon-message dispatch that turns ACP calls into daemon
//!   messages and back.
//! - [`acp_jsonrpc`] — the JSON-RPC 2.0 wire types and the ACP protocol
//!   payload types (initialize, session management, prompt, config options,
//!   streaming updates).
//! - [`acp_reader`] — the stdin reader thread that parses editor lines into
//!   [`acp_jsonrpc::RpcMessage`] values and feeds the shared event channel.
//! - [`daemon_client`] — the daemon Unix-socket reader/writer threads and the
//!   unified [`Event`](daemon_client::Event) type the event loop consumes.
//! - [`client_capabilities`] — the store of editor capabilities declared
//!   during `initialize`.
//! - [`config`] — builders for the `model`, `reasoning_effort`, and
//!   `tool_groups` config options the adapter advertises.
//! - [`sessions`] — the bidirectional ACP↔daemon session-ID mapping and the
//!   per-session prompt guard.
//! - [`pending`] — in-flight request tracking that routes asynchronous daemon
//!   replies (sync responses and streaming turn events) back to their
//!   originating JSON-RPC calls.
//! - [`streaming`] — the translation of daemon `SessionEvent`s into ACP
//!   `session/update` notifications.
//! - [`error`] — the crate-wide [`AcpError`] type.
//!
//! ## Threading model
//!
//! The bridge is three threads joined by one crossbeam event channel: the
//! stdin reader and the daemon reader each send [`Event`](daemon_client::Event)
//! values into it, and the main thread runs the event loop over the single
//! receiver. A separate daemon writer thread consumes already-framed
//! `ClientMessage`s sent over its own channel (the event loop allocates the
//! per-connection request id through its pending table). The event loop is
//! single-threaded and all I/O is blocking, so the per-loop state
//! ([`sessions::SessionManager`], [`pending::PendingRequests`]) needs no
//! locking.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod acp_handler;
pub mod acp_jsonrpc;
pub mod acp_reader;
pub mod client_capabilities;
pub mod config;
pub mod daemon_client;
pub mod error;
pub mod pending;
pub mod sessions;
pub mod streaming;

pub use error::AcpError;

use std::thread;

use anyhow::Context;
use choreo_shared::clap_styles;
use choreo_shared::logging::{LoggingConfig, Verbosity};
use clap::Parser;

#[derive(Parser)]
// `--version` prints the crate version (CARGO_PKG_VERSION) with the release
// name appended via `choreo_shared::release_name` — e.g. `0.2.0 (Lindy)`, or the
// bare version when the name file is empty. clap handles it before the app
// starts, so it works headless too.
// `color` is explicitly `Auto` (clap's default) to document the intent that
// help/error output is colored only when stdout/stderr is a TTY.
#[command(
    name = "choreo-acp",
    version = choreo_shared::release_name::version_string(env!("CARGO_PKG_VERSION")),
    about = "ACP bridge for Choreographr",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    /// Path to the Choreographr Unix socket (defaults to the daemon's standard
    /// path, or `{base-dir}/run/choreographr.sock` under `--base-dir`).
    #[arg(long = "socket-path")]
    socket_path: Option<String>,

    /// Path to the log file (defaults to `{base-dir}/log/acp.log` under a base
    /// dir, else the platform temp dir). stderr is unused to avoid corrupting
    /// the ACP protocol stream.
    #[arg(long = "log-file")]
    log_file: Option<String>,

    /// Run the instance out of this base directory instead of the platform
    /// defaults: `{base}/config`, `{base}/data`, `{base}/run` (the socket),
    /// and `{base}/log`. Equivalent to exporting `CHOREOGRAPHR_BASE_DIR`.
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<std::path::PathBuf>,

    // Increase logging verbosity (-v debug, -vv trace)
    #[command(flatten)]
    verbosity: Verbosity,
}

/// The ACP adapter's default log file: under the PLATFORM temp dir
/// (`std::env::temp_dir()`), never a hardcoded `/tmp` — on Android/Termux
/// there is no writable `/tmp`, and the adapter is started by an ACP client
/// (editor) that cannot pass CLI flags, so the default must work there.
fn default_log_file() -> String {
    std::env::temp_dir()
        .join("choreo-acp.log")
        .to_string_lossy()
        .into_owned()
}

fn setup_logging(log_file: &str, verbosity: Verbosity) {
    // The log file is auxiliary diagnostics — stdout carries the ACP JSON-RPC
    // stream and the adapter's job is to relay it, so a failure to create the
    // log must never kill the adapter (the Termux /tmp lesson: diagnostics
    // are never a startup precondition). The warning goes to stderr, which
    // ACP clients surface as adapter logs without protocol corruption.
    let Some(file) = choreo_shared::logging::create_log_file(std::path::Path::new(log_file)) else {
        eprintln!(
            "warning: could not create log file '{log_file}'; continuing without file logging"
        );
        return;
    };
    // Shared level policy (flags win over RUST_LOG), plus this adapter's own
    // module kept at debug by default: an ACP client owns the terminal, so the
    // adapter's diagnostics are only ever read from the log file, and the
    // default `info` would hide them.
    let logging = LoggingConfig::resolve(verbosity);
    let mut filter = logging.filter.clone();
    if let Ok(directive) = "choreo_acp=debug".parse::<tracing_subscriber::filter::Directive>() {
        filter = filter.add_directive(directive);
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
    // Observable now that the subscriber exists (shared wording).
    logging.emit_startup_logs();
}

/// Entry point for the `choreo-acp` bridge binary.
///
/// The workspace root declares this crate's binary as a thin wrapper that
/// simply calls this function, so the actual logic lives here in the lib.
///
/// # Errors
///
/// Returns an error when the given `Cli` arguments fail to parse
/// (`error = ...` returned by clap-derived parsing) or when daemon
/// initialization fails. A user-C-cancellation returns `Err` with the
/// propagated [`anyhow::Error`].
pub fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();

    // Apply the base-dir override BEFORE resolving the socket/log paths: clap
    // evaluates `default_value_t` at parse time, before a sibling `--base-dir`
    // is applied, so those defaults are resolved here instead.
    choreo_shared::paths::set_base_dir_from_cli(cli.base_dir.clone());
    let socket_path = cli
        .socket_path
        .clone()
        .unwrap_or_else(choreo_proto::socket_path);
    let log_file = cli
        .log_file
        .clone()
        .or_else(|| {
            choreo_shared::paths::log_file_default("acp").map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(default_log_file);

    // Logging goes to the log file (never stderr, which is unused in the ACP
    // protocol — stdout carries the JSON-RPC stream); if the log file cannot
    // be created the adapter continues without file logging.
    setup_logging(&log_file, cli.verbosity);

    tracing::info!(
        socket_path = %socket_path,
        "choreographr starting"
    );

    // Shared event channel — both the ACP stdin reader and the daemon
    // socket reader send events here.  The main loop receives on this
    // single receiver so it never needs to poll.
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<crate::daemon_client::Event>();

    // Track thread join handles for clean shutdown.
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();

    // 1. Connect to the daemon first (spawns reader + writer threads).
    //    If this fails, no other threads have been spawned yet, so there
    //    is nothing to clean up.
    let (daemon_client, writer_handle) =
        crate::daemon_client::spawn_daemon_io(&socket_path, event_tx.clone())
            .with_context(|| format!("could not connect to Choreographr at '{socket_path}'"))?;
    handles.push(writer_handle);
    handles.push(daemon_client.join_handle);

    // 2. Spawn the ACP stdin reader thread.
    let reader_handle = crate::acp_reader::spawn_acp_reader(event_tx.clone())
        .context("failed to spawn ACP reader")?;
    handles.push(reader_handle);

    // Drop our local sender — only the spawned threads should hold clones.
    drop(event_tx);

    tracing::info!("entering main event loop");

    // 3. Run the main event loop (blocks until both I/O threads exit).
    if let Err(e) = crate::acp_handler::run_event_loop(&event_rx, daemon_client.writer_tx) {
        tracing::error!(error = %e, "event loop exited with error");
    }

    // 4. Wait for all I/O threads to finish before exiting.
    for handle in handles {
        let _ = handle.join();
    }

    tracing::info!("choreographr shutting down");

    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    /// `--version` is handled by clap before any real arg parsing: it exits
    /// with a `DisplayVersion` error whose message is the version string.
    /// Assert both so the flag stays wired to `CARGO_PKG_VERSION` (it breaks
    /// silently if the derive attribute loses the bare `version` marker).
    #[test]
    fn version_flag_displays_package_version() {
        // clap returns the version as a `DisplayVersion` error instead of a
        // value; match it out by hand (Cli doesn't derive Debug, so
        // `unwrap_err()`'s Debug bound doesn't apply).
        let Err(err) = Cli::try_parse_from(["choreo-acp", "--version"]) else {
            panic!("--version should short-circuit before arg validation");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
        // And the release name (from choreo-shared/release-name.txt) rides along.
        let expected = choreo_shared::release_name::version_string(env!("CARGO_PKG_VERSION"));
        assert!(err.to_string().contains(&expected));
    }

    /// The default log file must live under the PLATFORM temp dir — never a
    /// hardcoded `/tmp`, which is not writable on Android/Termux (where the
    /// ACP client launches the adapter without CLI flags, so the default is
    /// the only path there is).
    #[test]
    fn default_log_file_is_under_the_platform_temp_dir() {
        let path = default_log_file();
        let expected = std::env::temp_dir().join("choreo-acp.log");
        assert_eq!(path, expected.to_string_lossy());
    }

    /// A log file that cannot be created must not prevent the adapter from
    /// starting: `setup_logging` degrades to no file logging (its stderr
    /// warning is the only trace, safe in ACP since stdout carries the
    /// JSON-RPC stream).
    #[test]
    fn setup_logging_survives_an_uncreatable_log_file() {
        // A path under a regular FILE cannot be created as a directory
        // child — deterministic EACCES/ENOENT without root assumptions.
        let blocker = std::env::temp_dir().join("choreo-acp-log-test-blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");
        let impossible = blocker.join("choreo-acp.log");

        // setup_logging cannot fail (it returns unit) — the assertion is
        // only that reaching here means the function degraded safely.
        setup_logging(
            &impossible.to_string_lossy(),
            Verbosity {
                verbose: 0,
                quiet: 0,
            },
        );

        let _ = std::fs::remove_file(&blocker);
    }
}
