mod connection;
mod db;
mod diff_render;
mod markdown_render;
mod render;
mod state;
mod syntax;

fn main() -> anyhow::Result<()> {
    connection::run_app()?;
    Ok(())
}

#[cfg(test)]
mod app_tests;
