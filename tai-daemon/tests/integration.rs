use std::sync::Arc;
use tai_daemon::{
    handle_client, new_daemon_state,
    openai::{AuthConfig, OpenAiClient, RequestFormat},
};
use tai_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use tokio::{
    net::UnixStream,
    time::{Duration, timeout},
};

fn test_auth_config() -> AuthConfig {
    AuthConfig {
        api_key: "test-key".to_string(),
        base_url: "https://example.com/v1".to_string(),
        model_list_path: "/models".to_string(),
        responses_path: "/responses".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        default_request_format: RequestFormat::ChatCompletions,
        model_request_formats: std::collections::HashMap::new(),
        chat_completions_max_tokens: None,
        model_max_tokens: std::collections::HashMap::new(),
        streaming: true,
    }
}

async fn recv(client: &mut UnixStream) -> DaemonMessage {
    timeout(
        Duration::from_secs(3),
        read_message::<_, DaemonMessage>(client),
    )
    .await
    .expect("timed out")
    .expect("read failed")
}

#[tokio::test]
async fn daemon_handler_run_input_requires_selected_model() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let server_task = tokio::spawn(handle_client(
        server,
        Arc::new(OpenAiClient::new(test_auth_config()).expect("client")),
        new_daemon_state(),
    ));

    write_message(
        &mut client,
        &ClientMessage::RunInput {
            request_id: 1,
            input: b"alpha beta".to_vec(),
        },
    )
    .await
    .expect("write req1");

    let mut saw_started = false;
    loop {
        match recv(&mut client).await {
            DaemonMessage::Started { request_id } => {
                assert_eq!(request_id, 1);
                saw_started = true;
            }
            DaemonMessage::Failed { request_id, error } => {
                assert_eq!(request_id, 1);
                assert!(error.contains("no model selected"));
                break;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    assert!(saw_started);

    drop(client);
    server_task.await.expect("join").expect("server ok");
}

#[tokio::test]
async fn daemon_handler_set_model_reports_failure_when_provider_unreachable() {
    let (server, mut client) = UnixStream::pair().expect("pair");
    let auth_config = AuthConfig {
        api_key: "test-key".to_string(),
        base_url: "http://127.0.0.1:9/v1".to_string(),
        model_list_path: "/models".to_string(),
        responses_path: "/responses".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        default_request_format: RequestFormat::ChatCompletions,
        model_request_formats: std::collections::HashMap::new(),
        chat_completions_max_tokens: None,
        model_max_tokens: std::collections::HashMap::new(),
        streaming: true,
    };
    let server_task = tokio::spawn(handle_client(
        server,
        Arc::new(OpenAiClient::new(auth_config).expect("client")),
        new_daemon_state(),
    ));

    write_message(
        &mut client,
        &ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        },
    )
    .await
    .expect("write set-model");

    match recv(&mut client).await {
        DaemonMessage::ModelSelectionFailed { model, error } => {
            assert_eq!(model, "gpt-5.4-nano");
            assert!(error.contains("failed to list models"));
        }
        other => panic!("unexpected message: {other:?}"),
    }

    drop(client);
    server_task.await.expect("join").expect("server ok");
}
