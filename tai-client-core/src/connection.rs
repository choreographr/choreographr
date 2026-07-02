use std::io;
use tai_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::mpsc::UnboundedReceiver,
};

pub async fn run_daemon_connection(
    socket_path: &str,
    mut handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: UnboundedReceiver<ClientMessage>,
) -> io::Result<()> {
    let stream = UnixStream::connect(socket_path).await?;
    let (mut reader, mut writer) = stream.into_split();

    let writer_task = tokio::spawn(async move {
        let mut from_ui = from_ui;
        while let Some(message) = from_ui.recv().await {
            write_message(&mut writer, &message).await?;
        }
        writer.shutdown().await
    });

    loop {
        match read_message::<_, DaemonMessage>(&mut reader).await {
            Ok(message) => handle_daemon_message(message),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }

    match writer_task.await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(error)) => return Err(error),
    }
    Ok(())
}
