mod connection;
mod render;
mod state;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    connection::run_app().await
}

#[cfg(test)]
mod app_tests;
