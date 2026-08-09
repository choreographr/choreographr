use std::sync::mpsc;
use std::thread;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::prelude::*;

#[derive(Parser)]
// Bare `version` makes `--version` print the crate version (CARGO_PKG_VERSION);
// clap handles it before the app starts, so it works headless too.
#[command(name = "choreo-acp", version, about = "ACP bridge for Choreographr")]
struct Cli {
    /// Path to the Choreographr Unix socket.
    #[arg(long = "socket-path", default_value_t = choreo_proto::socket_path())]
    socket_path: String,

    /// Path to the log file (stderr is unused to avoid corrupting the ACP protocol stream).
    #[arg(long = "log-file", default_value_t = String::from("/tmp/choreo-acp.log"))]
    log_file: String,
}

fn setup_logging(log_file: &str) -> Result<(), anyhow::Error> {
    let file = std::fs::File::create(log_file)
        .with_context(|| format!("failed to create log file '{log_file}'"))?;
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

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();

    // Logging goes to /tmp/choreo-acp.log (never stderr, which is unused
    // in the ACP protocol — stdout carries the JSON-RPC stream).
    setup_logging(&cli.log_file).context("failed to initialize logging")?;

    tracing::info!(
        socket_path = %cli.socket_path,
        "choreographr starting"
    );

    // Shared event channel — both the ACP stdin reader and the daemon
    // socket reader send events here.  The main loop receives on this
    // single receiver so it never needs to poll.
    let (event_tx, event_rx) = mpsc::channel::<choreo_acp::daemon_client::Event>();

    // Track thread join handles for clean shutdown.
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();

    // 1. Connect to the daemon first (spawns reader + writer threads).
    //    If this fails, no other threads have been spawned yet, so there
    //    is nothing to clean up.
    let (daemon_client, writer_handle) =
        choreo_acp::daemon_client::spawn_daemon_io(&cli.socket_path, event_tx.clone())
            .with_context(|| {
                format!("could not connect to Choreographr at '{}'", cli.socket_path)
            })?;
    handles.push(writer_handle);
    handles.push(daemon_client.join_handle);

    // 2. Spawn the ACP stdin reader thread.
    let reader_handle = choreo_acp::acp_reader::spawn_acp_reader(event_tx.clone())
        .context("failed to spawn ACP reader")?;
    handles.push(reader_handle);

    // Drop our local sender — only the spawned threads should hold clones.
    drop(event_tx);

    tracing::info!("entering main event loop");

    // 3. Run the main event loop (blocks until both I/O threads exit).
    if let Err(e) = choreo_acp::acp_handler::run_event_loop(event_rx, daemon_client.writer_tx) {
        tracing::error!(error = %e, "event loop exited with error");
    }

    // 4. Wait for all I/O threads to finish before exiting.
    for handle in handles {
        let _ = handle.join();
    }

    tracing::info!("choreographr shutting down");

    Ok(())
}
