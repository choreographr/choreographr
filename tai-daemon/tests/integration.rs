use tai_daemon::handle_client;
use tai_proto::{read_message, write_message, ClientMessage, DaemonMessage};
use tokio::{net::UnixStream, time::{timeout, Duration}};

async fn recv(client: &mut UnixStream) -> DaemonMessage {
    timeout(Duration::from_secs(3), read_message::<_, DaemonMessage>(client))
        .await
        .expect("timed out")
        .expect("read failed")
}

#[tokio::test]
async fn daemon_handler_supports_multiple_in_flight_requests() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(server));

    write_message(
        &mut client,
        &ClientMessage::RunInput { request_id: 1, input: b"alpha beta".to_vec() },
    )
    .await
    .expect("write req1");
    write_message(
        &mut client,
        &ClientMessage::RunInput { request_id: 2, input: b"gamma delta".to_vec() },
    )
    .await
    .expect("write req2");

    let mut started = std::collections::HashSet::new();
    let mut done = std::collections::HashSet::new();
    let mut saw_req1 = false;
    let mut saw_req2 = false;

    while done.len() < 2 {
        match recv(&mut client).await {
            DaemonMessage::Started { request_id } => {
                started.insert(request_id);
            }
            DaemonMessage::OutputChunk { request_id, data, .. } => {
                let text = String::from_utf8_lossy(&data);
                if request_id == 1 && text.contains("ALPHA") {
                    saw_req1 = true;
                }
                if request_id == 2 && text.contains("GAMMA") {
                    saw_req2 = true;
                }
            }
            DaemonMessage::Done { request_id } => {
                done.insert(request_id);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    assert_eq!(started.len(), 2);
    assert!(saw_req1);
    assert!(saw_req2);

    drop(client);
    server_task.await.expect("join").expect("server ok");
}

