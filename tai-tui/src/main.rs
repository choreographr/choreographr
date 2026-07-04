mod connection;
mod markdown_render;
mod render;
mod state;

fn main() -> anyhow::Result<()> {
    connection::run_app()?;
    Ok(())
}

#[cfg(test)]
mod app_tests;
