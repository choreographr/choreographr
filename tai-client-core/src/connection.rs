use crate::error::ClientError;
use tai_proto::{ClientMessage, DaemonMessage, ProtoError, read_message, write_message};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::mpsc::UnboundedReceiver,
};

pub async fn run_daemon_connection(
    socket_path: &str,
    mut handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: UnboundedReceiver<ClientMessage>,
) -> Result<(), ClientError> {
    let stream = UnixStream::connect(socket_path).await?;
    let (mut reader, mut writer) = stream.into_split();

    let writer_task = tokio::spawn(async move {
        let mut from_ui = from_ui;
        while let Some(message) = from_ui.recv().await {
            write_message(&mut writer, &message).await?;
        }
        Ok::<(), ProtoError>(writer.shutdown().await.map_err(ProtoError::from)?)
    });

    loop {
        match read_message::<_, DaemonMessage>(&mut reader).await {
            Ok(message) => handle_daemon_message(message),
            Err(ProtoError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }

    match writer_task.await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(error)) => return Err(error.into()),
    }
    Ok(())
}
