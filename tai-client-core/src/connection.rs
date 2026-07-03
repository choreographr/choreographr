use crate::error::ClientError;
use tai_proto::{DaemonMessage, ProtoError, read_message_sync, write_message_sync};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

pub fn run_daemon_connection(
    socket_path: &str,
    mut handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<tai_proto::ClientMessage>,
) -> Result<(), ClientError> {
    let stream = UnixStream::connect(socket_path)?;
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    std::thread::spawn(move || {
        for msg in from_ui {
            if write_message_sync(&mut writer, &msg).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let mut reader = reader;
    loop {
        match read_message_sync::<_, DaemonMessage>(&mut reader) {
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

    Ok(())
}
