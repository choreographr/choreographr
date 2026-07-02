use crate::server::handle_client_message;
use crate::sessions::{default_session_id, update_subscription};
use crate::DaemonState;
use std::io;
use tai_proto::{ClientMessage, DaemonMessage, ProtoError, read_message, write_message};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::mpsc,
};
use tracing::{debug, error};

pub async fn handle_client(stream: UnixStream, state: DaemonState) -> anyhow::Result<()> {
    const PER_CLIENT_MESSAGE_CHANNEL_SIZE: usize = 128;
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<DaemonMessage>(PER_CLIENT_MESSAGE_CHANNEL_SIZE);
    let client_id = {
        let mut guard = state.lock().await;
        let client_id = guard.next_client_id;
        guard.next_client_id = guard.next_client_id.wrapping_add(1);
        client_id
    };
    let mut attached_session_id = default_session_id(&state).await;
    if let Some(session_id) = attached_session_id {
        update_subscription(&state, client_id, None, Some(session_id), &tx).await;
    }

    debug!("starting client handler");

    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            debug!(?message, "sending daemon message");
            write_message(&mut writer, &message).await?;
        }
        debug!("writer task shutting down");
        Ok::<(), ProtoError>(writer.shutdown().await.map_err(ProtoError::from)?)
    });

    loop {
        match read_message::<_, ClientMessage>(&mut reader).await {
            Ok(message) => {
                debug!(?message, "received client message");
                handle_client_message(
                    message,
                    &state,
                    &tx,
                    client_id,
                    &mut attached_session_id,
                )
                .await?;
            }
            Err(ProtoError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                debug!(error = %error, "client disconnected");
                break;
            }
            Err(error) => {
                error!(error = %error, "failed to read client message");
                return Err(error.into());
            }
        }
    }

    update_subscription(&state, client_id, attached_session_id, None, &tx).await;
    drop(tx);
    writer_task.abort();
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(ProtoError::Io(error)))
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) =>
        {
            debug!(error = %error, "writer task ended after client disconnect");
        }
        Ok(Err(error)) => return Err(error.into()),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(anyhow::Error::from(error)),
    }
    debug!("client handler finished");
    Ok(())
}
