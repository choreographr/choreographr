use std::{collections::HashSet, io, sync::Arc};
use tai_proto::{read_message, socket_path, write_message, ClientMessage, DaemonMessage};
use tai_sh::{channel_closed, parse_input_line, ShellCommand};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, Mutex},
};

#[tokio::main]
async fn main() -> io::Result<()> {
    let socket_path = socket_path();
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<ClientMessage>(128);
    let active = Arc::new(Mutex::new(HashSet::<u32>::new()));

    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            write_message(&mut writer, &message).await?;
        }
        writer.shutdown().await
    });

    let active_reader = Arc::clone(&active);
    let reader_task = tokio::spawn(async move {
        loop {
            match read_message::<_, DaemonMessage>(&mut reader).await? {
                DaemonMessage::Started { request_id } => {
                    println!("[{request_id}] started");
                }
                DaemonMessage::OutputChunk { request_id, data, .. } => {
                    print!("[{request_id}] {}", String::from_utf8_lossy(&data));
                }
                DaemonMessage::Done { request_id } => {
                    println!("[{request_id}] done");
                    active_reader.lock().await.remove(&request_id);
                }
                DaemonMessage::Failed { request_id, error } => {
                    println!("[{request_id}] failed: {error}");
                    active_reader.lock().await.remove(&request_id);
                }
                DaemonMessage::Cancelled { request_id } => {
                    println!("[{request_id}] cancelled");
                    active_reader.lock().await.remove(&request_id);
                }
                DaemonMessage::Pong => println!("[daemon] pong"),
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    });

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut next_request_id = 1_u32;

    println!("Connected to tai-daemon at {socket_path}");
    println!("Enter text to run. Use ':cancel <id>' to cancel. Use ':ping' to ping.");

    while let Some(line) = lines.next_line().await? {
        match parse_input_line(&line, &mut next_request_id) {
            ShellCommand::Empty => {}
            ShellCommand::InvalidCancel(value) => eprintln!("invalid request id: {value}"),
            ShellCommand::Send(message) => {
                if let ClientMessage::RunInput { request_id, .. } = &message {
                    active.lock().await.insert(*request_id);
                }
                tx.send(message).await.map_err(channel_closed)?;
            }
        }
    }

    drop(tx);
    writer_task.await.map_err(io::Error::other)??;
    match reader_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {}
        Ok(Err(error)) => return Err(error),
        Err(error) => return Err(io::Error::other(error)),
    }
    Ok(())
}
