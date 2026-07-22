use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tai_proto::AccountInfo;

use crate::openai::{MaxTokensField, RequestFormat};

/// Configuration for a single inference account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub streaming: Option<bool>,
    #[serde(default)]
    pub stream_options: Option<bool>,
    #[serde(default)]
    pub retry_max_attempts: Option<u32>,
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
    // Endpoint path overrides (OpenAI-compatible only)
    #[serde(default)]
    pub model_list_path: Option<String>,
    #[serde(default)]
    pub responses_path: Option<String>,
    #[serde(default)]
    pub chat_completions_path: Option<String>,
    // Request format overrides (OpenAI-compatible only)
    #[serde(default)]
    pub default_request_format: Option<RequestFormat>,
    #[serde(default)]
    pub model_request_formats: Option<HashMap<String, RequestFormat>>,
    // Token limit overrides (OpenAI-compatible only)
    #[serde(default)]
    pub chat_completions_max_tokens: Option<u32>,
    #[serde(default)]
    pub model_max_tokens: Option<HashMap<String, u32>>,
    #[serde(default)]
    pub chat_completions_max_tokens_field: Option<MaxTokensField>,
    // Responses API overrides (OpenAI Responses API)
    #[serde(default)]
    pub responses_max_output_tokens: Option<u32>,
    #[serde(default)]
    pub model_responses_max_output_tokens: Option<HashMap<String, u32>>,
    #[serde(default)]
    pub programmatic_tool_calling: Option<bool>,
    #[serde(default)]
    pub model_max_tokens_fields: Option<HashMap<String, MaxTokensField>>,
    // Context window overrides
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub model_context_windows: Option<HashMap<String, u32>>,
    // Retry timing (all providers)
    #[serde(default)]
    pub retry_initial_backoff_ms: Option<u64>,
    #[serde(default)]
    pub retry_max_backoff_ms: Option<u64>,
}

impl AccountConfig {
    /// Create an AccountConfig with just a name and provider; all other
    /// fields are `None` (meaning "use the provider default").
    pub fn simple(name: &str, provider: &str) -> Self {
        Self {
            name: name.to_string(),
            provider: provider.to_string(),
            base_url: None,
            streaming: None,
            stream_options: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
            model_list_path: None,
            responses_path: None,
            chat_completions_path: None,
            default_request_format: None,
            model_request_formats: None,
            chat_completions_max_tokens: None,
            model_max_tokens: None,
            chat_completions_max_tokens_field: None,
            model_max_tokens_fields: None,
            responses_max_output_tokens: None,
            model_responses_max_output_tokens: None,
            programmatic_tool_calling: None,
            retry_initial_backoff_ms: None,
            retry_max_backoff_ms: None,
            context_window: None,
            model_context_windows: None,
        }
    }

    /// Apply this account's config overrides to a ServiceConfig.
    pub fn apply_overrides(&self, config: &mut crate::openai::ServiceConfig) {
        if let Some(base_url) = &self.base_url {
            config.base_url = base_url.clone();
        }
        if let Some(streaming) = self.streaming {
            config.streaming = streaming;
        }
        if let Some(stream_options) = self.stream_options {
            config.stream_options = stream_options;
        }
        if let Some(retry) = self.retry_max_attempts {
            config.retry_max_attempts = retry;
        }
        if let Some(connect) = self.connect_timeout_secs {
            config.connect_timeout_secs = connect;
        }
        if let Some(request) = self.request_timeout_secs {
            config.request_timeout_secs = request;
        }
        if let Some(path) = &self.model_list_path {
            config.model_list_path = path.clone();
        }
        if let Some(path) = &self.responses_path {
            config.responses_path = path.clone();
        }
        if let Some(path) = &self.chat_completions_path {
            config.chat_completions_path = path.clone();
        }
        if let Some(fmt) = self.default_request_format {
            config.default_request_format = fmt;
        }
        if let Some(ref map) = self.model_request_formats {
            config.model_request_formats = map.clone();
        }
        if let Some(n) = self.chat_completions_max_tokens {
            config.chat_completions_max_tokens = Some(n);
        }
        if let Some(ref map) = self.model_max_tokens {
            config.model_max_tokens = map.clone();
        }
        if let Some(field) = self.chat_completions_max_tokens_field {
            config.chat_completions_max_tokens_field = field;
        }
        if let Some(ref map) = self.model_max_tokens_fields {
            config.model_max_tokens_fields = map.clone();
        }
        if let Some(n) = self.responses_max_output_tokens {
            config.responses_max_output_tokens = Some(n);
        }
        if let Some(ref map) = self.model_responses_max_output_tokens {
            config.model_responses_max_output_tokens = map.clone();
        }
        if let Some(v) = self.programmatic_tool_calling {
            config.programmatic_tool_calling = v;
        }
        if let Some(ms) = self.retry_initial_backoff_ms {
            config.retry_initial_backoff_ms = ms;
        }
        if let Some(ms) = self.retry_max_backoff_ms {
            config.retry_max_backoff_ms = ms;
        }
        config
            .context_window_config
            .apply_overrides(self.context_window, self.model_context_windows.as_ref());
    }

    pub fn to_info(&self, has_credential: bool) -> AccountInfo {
        AccountInfo {
            name: self.name.clone(),
            provider: self.provider.clone(),
            has_credential,
        }
    }
}

/// Resolve the accounts.toml path (e.g. ~/.config/tai-daemon/accounts.toml).
pub fn accounts_config_path() -> io::Result<PathBuf> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine config directory",
        )
    })?;
    Ok(config_dir.join("tai-daemon").join("accounts.toml"))
}

/// Manages a collection of accounts backed by a TOML file.
pub struct AccountManager {
    config_path: PathBuf,
    accounts: HashMap<String, AccountConfig>,
}

impl AccountManager {
    /// Create an empty account manager with the default config path.
    /// Used as a fallback when the accounts file cannot be loaded.
    pub fn empty() -> Self {
        Self {
            config_path: PathBuf::new(),
            accounts: HashMap::new(),
        }
    }

    /// Load accounts from a TOML file. If the file doesn't exist, returns an
    /// empty manager (no error).
    pub fn load(path: &Path) -> io::Result<Self> {
        if !path.exists() {
            return Ok(Self {
                config_path: path.to_path_buf(),
                accounts: HashMap::new(),
            });
        }
        let raw = fs::read_to_string(path)?;
        #[derive(Deserialize)]
        struct AccountsFile {
            #[serde(default)]
            account: Vec<AccountConfig>,
        }
        let file: AccountsFile =
            toml::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let accounts: HashMap<String, AccountConfig> = file
            .account
            .into_iter()
            .map(|cfg| (cfg.name.clone(), cfg))
            .collect();
        Ok(Self {
            config_path: path.to_path_buf(),
            accounts,
        })
    }

    /// Save all accounts to the TOML file, creating the parent directory if
    /// needed.
    pub fn save(&self) -> io::Result<()> {
        #[derive(Serialize)]
        struct AccountsFile<'a> {
            #[serde(rename = "account")]
            accounts: Vec<&'a AccountConfig>,
        }
        let file = AccountsFile {
            accounts: self.accounts.values().collect(),
        };
        let toml_str =
            toml::to_string(&file).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.config_path, toml_str)?;
        Ok(())
    }

    /// Add an account. Errors if the name already exists.
    pub fn add(&mut self, config: AccountConfig) -> Result<(), String> {
        if self.accounts.contains_key(&config.name) {
            return Err(format!("account '{}' already exists", config.name));
        }
        self.accounts.insert(config.name.clone(), config);
        self.save()
            .map_err(|e| format!("failed to save accounts: {e}"))?;
        Ok(())
    }

    /// Remove an account by name. Errors if not found.
    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        if self.accounts.remove(name).is_none() {
            return Err(format!("account '{}' not found", name));
        }
        self.save()
            .map_err(|e| format!("failed to save accounts: {e}"))?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&AccountConfig> {
        self.accounts.get(name)
    }

    pub fn list(&self, credentialed: &std::collections::HashSet<String>) -> Vec<AccountInfo> {
        let mut configs: Vec<AccountInfo> = self
            .accounts
            .values()
            .map(|cfg| cfg.to_info(credentialed.contains(&cfg.name)))
            .collect();
        configs.sort_by(|a, b| a.name.cmp(&b.name));
        configs
    }

    pub fn all_configs(&self) -> Vec<AccountConfig> {
        let mut configs: Vec<AccountConfig> = self.accounts.values().cloned().collect();
        configs.sort_by(|a, b| a.name.cmp(&b.name));
        configs
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.accounts.contains_key(name)
    }

    pub fn first(&self) -> Option<&AccountConfig> {
        let mut keys: Vec<&String> = self.accounts.keys().collect();
        keys.sort();
        keys.first().and_then(|k| self.accounts.get(*k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manager(path: &std::path::Path) -> AccountManager {
        AccountManager::load(path).unwrap()
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does_not_exist.toml");
        let mgr = manager(&path);
        assert!(mgr.is_empty());
        assert!(mgr.first().is_none());
    }

    #[test]
    fn load_empty_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        std::fs::write(&path, "").unwrap();
        let mgr = manager(&path);
        assert!(mgr.is_empty());
    }

    #[test]
    fn load_valid_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        std::fs::write(
            &path,
            r#"
[[account]]
name = "main"
provider = "openai"

[[account]]
name = "backup"
provider = "anthropic"
model = "claude-4"
"#,
        )
        .unwrap();
        let mgr = manager(&path);
        assert!(!mgr.is_empty());
        assert_eq!(mgr.accounts.len(), 2);
        assert!(mgr.contains("main"));
        assert!(mgr.contains("backup"));
    }

    #[test]
    fn add_and_retrieve() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(AccountConfig::simple("prod", "openai")).unwrap();
        assert!(mgr.contains("prod"));
        assert_eq!(mgr.get("prod").unwrap().provider, "openai");
    }

    #[test]
    fn add_duplicate_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(AccountConfig::simple("dup", "openai")).unwrap();
        let err = mgr.add(AccountConfig::simple("dup", "openai")).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn remove_account() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(AccountConfig::simple("temp", "openai")).unwrap();
        assert!(mgr.contains("temp"));
        mgr.remove("temp").unwrap();
        assert!(!mgr.contains("temp"));
    }

    #[test]
    fn remove_missing_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        let err = mgr.remove("nope").unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn first_returns_sorted_first() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(AccountConfig::simple("zebra", "openai")).unwrap();
        mgr.add(AccountConfig::simple("alpha", "openai")).unwrap();
        mgr.add(AccountConfig::simple("omega", "openai")).unwrap();

        let first = mgr.first().unwrap();
        assert_eq!(first.name, "alpha");
    }

    #[test]
    fn list_sorted_alphabetically() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(AccountConfig::simple("z", "openai")).unwrap();
        mgr.add(AccountConfig::simple("a", "openai")).unwrap();
        let list = mgr.list(&std::collections::HashSet::new());
        assert_eq!(list[0].name, "a");
        assert_eq!(list[1].name, "z");
    }

    #[test]
    fn list_reports_credential_status() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(AccountConfig::simple("has_it", "openai")).unwrap();
        mgr.add(AccountConfig::simple("missing", "openai")).unwrap();

        let mut credentialed = std::collections::HashSet::new();
        credentialed.insert("has_it".to_string());

        let list = mgr.list(&credentialed);
        assert_eq!(list.len(), 2);
        for info in &list {
            match info.name.as_str() {
                "has_it" => assert!(info.has_credential, "has_it should show credential"),
                "missing" => assert!(!info.has_credential, "missing should not show credential"),
                other => panic!("unexpected account: {other}"),
            }
        }
    }

    #[test]
    fn all_configs_sorted_alphabetically() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(AccountConfig::simple("b", "openai")).unwrap();
        mgr.add(AccountConfig::simple("a", "openai")).unwrap();
        let configs = mgr.all_configs();
        assert_eq!(configs[0].name, "a");
        assert_eq!(configs[1].name, "b");
    }

    #[test]
    fn save_persists_and_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        {
            let mut mgr = manager(&path);
            mgr.add(AccountConfig::simple("persist", "openai")).unwrap();
        }
        let mgr = manager(&path);
        assert!(mgr.contains("persist"));
    }

    #[test]
    fn to_info_roundtrip() {
        let cfg = AccountConfig::simple("test", "openai");
        let info = cfg.to_info(true);
        assert_eq!(info.name, "test");
        assert_eq!(info.provider, "openai");
        assert!(info.has_credential);
    }

    #[test]
    fn apply_overrides() {
        let mut svc_config = crate::openai::ServiceConfig::default();
        let cfg = AccountConfig {
            base_url: Some("https://custom.api.com/v1".to_string()),
            streaming: Some(false),
            retry_max_attempts: Some(5),
            connect_timeout_secs: Some(30),
            request_timeout_secs: Some(120),
            model_list_path: Some("/v2/models".to_string()),
            responses_path: Some("/v2/responses".to_string()),
            chat_completions_path: Some("/v2/chat".to_string()),
            default_request_format: Some(crate::openai::RequestFormat::Responses),
            chat_completions_max_tokens: Some(2048),
            chat_completions_max_tokens_field: Some(crate::openai::MaxTokensField::MaxTokens),
            responses_max_output_tokens: Some(4096),
            model_responses_max_output_tokens: Some(HashMap::from([("gpt-4".to_string(), 8192)])),
            retry_initial_backoff_ms: Some(500),
            retry_max_backoff_ms: Some(60000),
            ..AccountConfig::simple("ovr", "openai")
        };
        cfg.apply_overrides(&mut svc_config);
        assert_eq!(svc_config.base_url, "https://custom.api.com/v1");
        assert!(!svc_config.streaming);
        assert_eq!(svc_config.retry_max_attempts, 5);
        assert_eq!(svc_config.connect_timeout_secs, 30);
        assert_eq!(svc_config.request_timeout_secs, 120);
        assert_eq!(svc_config.model_list_path, "/v2/models");
        assert_eq!(svc_config.responses_path, "/v2/responses");
        assert_eq!(svc_config.chat_completions_path, "/v2/chat");
        assert_eq!(
            svc_config.default_request_format,
            crate::openai::RequestFormat::Responses
        );
        assert_eq!(svc_config.chat_completions_max_tokens, Some(2048));
        assert_eq!(
            svc_config.chat_completions_max_tokens_field,
            crate::openai::MaxTokensField::MaxTokens
        );
        assert_eq!(svc_config.retry_initial_backoff_ms, 500);
        assert_eq!(svc_config.retry_max_backoff_ms, 60000);
        assert_eq!(svc_config.responses_max_output_tokens, Some(4096));
        assert_eq!(
            svc_config.model_responses_max_output_tokens.get("gpt-4"),
            Some(&8192)
        );
    }
}
