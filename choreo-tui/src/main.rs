mod cache;
mod connection;
mod diff_render;
mod markdown_render;
mod render;
mod scrollbar;
mod state;
mod syntax;

use anyhow::Context;
use clap::Parser;

#[derive(Parser)]
// Bare `version` makes `--version` print the crate version (CARGO_PKG_VERSION);
// clap handles it before the app starts, so it works headless too.
#[command(name = "choreo-tui", version, about = "Choreographr terminal UI")]
struct Cli {
    /// Connect via TCP/Noise IK at this address (e.g. 127.0.0.1:9443)
    #[arg(long = "tcp-addr")]
    tcp_addr: Option<String>,

    /// Path to the server's Noise IK public key (defaults to ~/.config/choreographr/transport.pub)
    #[arg(long = "server-pk")]
    server_pk: Option<String>,
}

fn main() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;

    let cli = Cli::parse();

    let mode = if let Some(addr) = cli.tcp_addr {
        let server_pk = choreo_client_core::read_server_pk(cli.server_pk.as_deref())
            .context("failed to read server public key")?;
        choreo_client_core::ConnectionMode::Tcp { addr, server_pk }
    } else {
        choreo_client_core::ConnectionMode::UnixSocket(choreo_proto::socket_path())
    };

    let log_path = format!("/tmp/choreo-tui-{}.log", std::process::id());
    let log_file = std::fs::File::create(&log_path)?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(log_file)
        .with_ansi(false);
    tracing_subscriber::registry().with(file_layer).init();

    connection::run_app(mode)?;
    Ok(())
}

#[cfg(test)]
mod app_tests;
#[cfg(test)]
mod render_tests;
#[cfg(test)]
mod test_util;
