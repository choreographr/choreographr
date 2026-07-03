use std::sync::Arc;
use tai_daemon::DaemonState;
use tai_daemon::new_daemon_state;
use tai_daemon::openai::{OpenAiClient, RequestFormat, ServiceConfig};

pub fn test_db() -> redb::Database {
    let dir = tempfile::tempdir().unwrap();
    redb::Database::create(dir.path().join("state.redb")).unwrap()
}

pub fn test_service_config() -> ServiceConfig {
    ServiceConfig {
        base_url: "https://example.com/v1".to_string(),
        model_list_path: "/models".to_string(),
        responses_path: "/responses".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        default_request_format: RequestFormat::ChatCompletions,
        model_request_formats: std::collections::HashMap::new(),
        chat_completions_max_tokens: None,
        model_max_tokens: std::collections::HashMap::new(),
        streaming: true,
        max_turns: None,
        retry_max_attempts: 5,
        retry_initial_backoff_ms: 1000,
        retry_max_backoff_ms: 30000,
        connect_timeout_secs: 30,
        request_timeout_secs: 120,
        context: Default::default(),
    }
}

pub fn test_client() -> Arc<OpenAiClient> {
    Arc::new(OpenAiClient::new(test_service_config(), "test-key".to_string()).expect("client"))
}

pub async fn test_state_with_client() -> DaemonState {
    let state = new_daemon_state(test_db(), 25).await;
    state.lock().await.openai_client = Some(test_client());
    state
}
