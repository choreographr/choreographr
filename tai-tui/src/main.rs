mod connection;
mod markdown_render;
mod render;
mod state;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    connection::run_app().await?;
    Ok(())
}

#[cfg(test)]
mod app_tests;
