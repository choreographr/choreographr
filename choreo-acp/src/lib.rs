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

use std::sync::mpsc;
use std::thread;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::prelude::*;

/// Shared clap [`Styles`] for this crate's CLI binary.
///
/// Each CLI crate keeps its own copy (choreo-proto is the wire protocol and
/// must not host CLI styling); if this ever grows, promote it to a dedicated
/// micro-crate instead of putting it in choreo-proto.
///
/// Uses real ANSI hues (green headers/usage, cyan literals/placeholders) rather
/// than bold/underline only, so help output stays legible even in terminals whose
/// bold text isn't visually distinct (e.g. themes that don't remap the bold color).
/// `Styles::styled()` keeps clap's default error/invalid/valid coloring; the
/// overrides colorize the help elements.
fn clap_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}

#[derive(Parser)]
// Bare `version` makes `--version` print the crate version (CARGO_PKG_VERSION);
// clap handles it before the app starts, so it works headless too.
// `color` is explicitly `Auto` (clap's default) to document the intent that
// help/error output is colored only when stdout/stderr is a TTY.
#[command(
    name = "choreo-acp",
    version,
    about = "ACP bridge for Choreographr",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    /// Path to the Choreographr Unix socket.
    #[arg(long = "socket-path", default_value_t = choreo_proto::socket_path())]
    socket_path: String,

    /// Path to the log file (stderr is unused to avoid corrupting the ACP protocol stream).
    #[arg(long = "log-file", default_value_t = default_log_file())]
    log_file: String,
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

fn setup_logging(log_file: &str) -> Result<(), anyhow::Error> {
    // The log file is auxiliary diagnostics — stdout carries the ACP JSON-RPC
    // stream and the adapter's job is to relay it, so a failure to create the
    // log must never kill the adapter (the Termux /tmp lesson: diagnostics
    // are never a startup precondition). The warning goes to stderr, which
    // ACP clients surface as adapter logs without protocol corruption.
    let Ok(file) = std::fs::File::create(log_file) else {
        eprintln!(
            "warning: could not create log file '{log_file}'; continuing without file logging"
        );
        return Ok(());
    };
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false);
    let choreographr_acp_directive = "choreo_acp=debug".parse().unwrap_or_else(|e| {
        // Warning only — not fatal; logged before tracing is fully initialized.
        eprintln!("warning: failed to parse default log directive: {e}");
        tracing_subscriber::filter::LevelFilter::DEBUG.into()
    });
    let filter = tracing_subscriber::EnvFilter::builder()
        .from_env_lossy()
        .add_directive(choreographr_acp_directive);
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .init();
    Ok(())
}

/// Entry point for the `choreo-acp` bridge binary.
///
/// The workspace root declares this crate's binary as a thin wrapper that
/// simply calls this function, so the actual logic lives here in the lib.
pub fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();

    // Logging goes to $TMPDIR/choreo-acp.log (never stderr, which is unused
    // in the ACP protocol — stdout carries the JSON-RPC stream); if the log
    // file cannot be created the adapter continues without file logging.
    setup_logging(&cli.log_file).context("failed to initialize logging")?;

    tracing::info!(
        socket_path = %cli.socket_path,
        "choreographr starting"
    );

    // Shared event channel — both the ACP stdin reader and the daemon
    // socket reader send events here.  The main loop receives on this
    // single receiver so it never needs to poll.
    let (event_tx, event_rx) = mpsc::channel::<crate::daemon_client::Event>();

    // Track thread join handles for clean shutdown.
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();

    // 1. Connect to the daemon first (spawns reader + writer threads).
    //    If this fails, no other threads have been spawned yet, so there
    //    is nothing to clean up.
    let (daemon_client, writer_handle) =
        crate::daemon_client::spawn_daemon_io(&cli.socket_path, event_tx.clone()).with_context(
            || format!("could not connect to Choreographr at '{}'", cli.socket_path),
        )?;
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
    if let Err(e) = crate::acp_handler::run_event_loop(event_rx, daemon_client.writer_tx) {
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
    /// Assert both so the flag stays wired to CARGO_PKG_VERSION (it breaks
    /// silently if the derive attribute loses the bare `version` marker).
    #[test]
    fn version_flag_displays_package_version() {
        // clap returns the version as a `DisplayVersion` error instead of a
        // value; match it out by hand (Cli doesn't derive Debug, so
        // `unwrap_err()`'s Debug bound doesn't apply).
        let err = match Cli::try_parse_from(["choreo-acp", "--version"]) {
            Err(e) => e,
            Ok(_) => panic!("--version should short-circuit before arg validation"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
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
    /// starting: setup_logging degrades to no file logging (its stderr
    /// warning is the only trace, safe in ACP since stdout carries the
    /// JSON-RPC stream).
    #[test]
    fn setup_logging_survives_an_uncreatable_log_file() {
        // A path under a regular FILE cannot be created as a directory
        // child — deterministic EACCES/ENOENT without root assumptions.
        let blocker = std::env::temp_dir().join("choreo-acp-log-test-blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");
        let impossible = blocker.join("choreo-acp.log");

        setup_logging(&impossible.to_string_lossy())
            .expect("an uncreatable log file must degrade, not fail");

        let _ = std::fs::remove_file(&blocker);
    }
}
