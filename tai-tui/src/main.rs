mod cache;
mod connection;
mod db;
mod diff_render;
mod markdown_render;
mod render;
mod state;
mod syntax;

fn main() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;

    let log_file = std::fs::File::create("/tmp/tai-tui.log")?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(log_file)
        .with_ansi(false);
    tracing_subscriber::registry().with(file_layer).init();

    connection::run_app()?;
    Ok(())
}

#[cfg(test)]
mod app_tests;
#[cfg(test)]
mod render_tests;
