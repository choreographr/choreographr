use std::io;
use tai_proto::socket_path;

#[tokio::main]
async fn main() -> io::Result<()> {
    let socket_path = socket_path();
    tai_daemon::run_server(&socket_path).await
}
