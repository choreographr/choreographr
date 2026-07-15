mod cache;
mod connection;
mod db;
mod diff_render;
mod markdown_render;
mod render;
mod scrollbar;
mod state;
mod syntax;

use anyhow::Context;
use clap::Parser;

#[derive(Parser)]
#[command(name = "tai-tui", about = "Tai AI terminal UI")]
struct Cli {
    /// Connect via TCP/Noise IK at this address (e.g. 127.0.0.1:9443)
    #[arg(long = "tcp-addr")]
    tcp_addr: Option<String>,

    /// Path to the server's Noise IK public key (defaults to ~/.config/tai-daemon/transport.pub)
    #[arg(long = "server-pk")]
    server_pk: Option<String>,
}

fn main() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;

    let cli = Cli::parse();

    let mode = if let Some(addr) = cli.tcp_addr {
        let server_pk = tai_client_core::read_server_pk(cli.server_pk.as_deref())
            .context("failed to read server public key")?;
        tai_client_core::ConnectionMode::Tcp { addr, server_pk }
    } else {
        tai_client_core::ConnectionMode::UnixSocket(tai_proto::socket_path())
    };

    let log_file = std::fs::File::create("/tmp/tai-tui.log")?;
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
