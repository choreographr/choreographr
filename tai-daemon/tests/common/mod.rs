use std::sync::Arc;
use tai_daemon::DaemonState;
use tai_daemon::openai::{OpenAiClient, RequestFormat, ServiceConfig};
use tokio::sync::mpsc;

pub fn test_db() -> redb::Database {
    let dir = tempfile::tempdir().unwrap();
    redb::Database::create(dir.path().join("state.redb")).unwrap()
}

    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub fn test_client() -> Arc<OpenAiClient> {
    Arc::new(OpenAiClient::new(test_service_config(), "test-key".to_string()).expect("client"))
}

    #[allow(dead_code)]
    pub fn test_state_with_client() -> DaemonState {
    let db = Arc::new(test_db());
    let (daemon_tx, _) = mpsc::unbounded_channel();
    DaemonState {
        next_session_id: 1,
        max_turns: 25,
        active_sessions: std::collections::HashMap::new(),
        session_metadata: std::collections::HashMap::new(),
        openai_client: Some(test_client()),
        keystore: None,
        x_credentials: None,
        db,
        tool_registry: Arc::new(tai_daemon::tools::ToolRegistry::new()),
        daemon_tx,
    }
}
