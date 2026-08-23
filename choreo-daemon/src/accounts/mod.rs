use choreo_proto::AccountInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use choreo_ai_protocols::openai::{MaxTokensField, RequestFormat};
use choreo_ai_protocols::retry::MAX_BACKOFF_MS;

/// Configuration for a single inference account.
///
/// `PartialEq`/`Eq` are derived so two managers can be compared for *logical*
/// equality — the external-edit reload gate uses this instead of a byte
/// compare (see [`AccountManager::save`] for why bytes can differ between
/// identical logical states).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default)]
    pub total_timeout_secs: Option<u64>,
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
            total_timeout_secs: None,
            model_list_path: None,
            responses_path: None,
            chat_completions_path: None,
            default_request_format: None,
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

    /// Validate the retry-knob invariants this crate can reason about.
    ///
    /// Layer 3 of the retry budgeting: `retry_max_backoff_ms` *is* the retry
    /// budget (see `choreo_ai_protocols::retry::retry_decision`), so the
    /// daemon rejects values that would void the budget gate *before* they
    /// reach the client — a clear config error beats a silent library clamp.
    /// `retry_initial_backoff_ms` is checked against the ceiling
    /// independently of `max` too: when `max` is unset the library clamp in
    /// `RetryConfig::new` would otherwise "lift" the 30 s default `max` up to
    /// whatever the initial got clamped to — silently widening the budget
    /// gate the user never asked to change.  The library still clamps in
    /// `RetryConfig::new` for callers that construct `ServiceConfig`
    /// directly; this is the UX layer that tells the daemon user exactly
    /// what to fix.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(initial) = self.retry_initial_backoff_ms
            && initial > MAX_BACKOFF_MS
        {
            return Err(format!(
                "account '{}': retry_initial_backoff_ms ({initial} ms) exceeds the maximum \
                 supported retry delay ({} ms); lower it or leave it unset to use the default",
                self.name, MAX_BACKOFF_MS
            ));
        }
        if let Some(max) = self.retry_max_backoff_ms {
            if max > MAX_BACKOFF_MS {
                return Err(format!(
                    "account '{}': retry_max_backoff_ms ({max} ms) exceeds the maximum \
                     supported retry delay ({} ms); lower it or leave it unset to use the default",
                    self.name, MAX_BACKOFF_MS
                ));
            }
            if let Some(initial) = self.retry_initial_backoff_ms
                && initial > max
            {
                return Err(format!(
                    "account '{}': retry_initial_backoff_ms ({initial} ms) must not exceed \
                     retry_max_backoff_ms ({max} ms)",
                    self.name
                ));
            }
        }
        Ok(())
    }

    /// Apply this account's config overrides to a ServiceConfig.
    pub fn apply_overrides(&self, config: &mut choreo_ai_protocols::openai::ServiceConfig) {
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
        // Hard wall-clock deadline for the whole request (incl. streaming
        // body).  Mirrors `request_timeout_secs` (idle/no-progress) as the
        // second half of the timeout pair applied by the provider crates.
        if let Some(total) = self.total_timeout_secs {
            config.total_timeout_secs = total;
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

    /// Convert the shared override fields into the protocol-agnostic carrier
    /// used by the provider crates (`choreo-ai-protocols`).  OpenAI-specific
    /// fields are not represented — they are applied directly via
    /// [`apply_overrides`](Self::apply_overrides).
    pub fn provider_overrides(&self) -> choreo_ai_protocols::ProviderOverrides {
        choreo_ai_protocols::ProviderOverrides::from(self)
    }

    pub fn to_info(&self, has_credential: bool) -> AccountInfo {
        AccountInfo {
            name: self.name.clone(),
            provider: self.provider.clone(),
            has_credential,
        }
    }
}

/// Convert the shared override fields of an [`AccountConfig`] into the
/// protocol-agnostic carrier used by the provider crates.
///
/// Kept as a `From` impl so `provider_overrides()` construction can't drift
/// from this field list (the carrier is the single source of truth for what
/// gets forwarded to every provider).  OpenAI-specific fields are deliberately
/// not represented here — they are applied directly via
/// [`AccountConfig::apply_overrides`].
impl From<&AccountConfig> for choreo_ai_protocols::ProviderOverrides {
    fn from(config: &AccountConfig) -> Self {
        choreo_ai_protocols::ProviderOverrides {
            base_url: config.base_url.clone(),
            streaming: config.streaming,
            retry_max_attempts: config.retry_max_attempts,
            connect_timeout_secs: config.connect_timeout_secs,
            request_timeout_secs: config.request_timeout_secs,
            total_timeout_secs: config.total_timeout_secs,
            retry_initial_backoff_ms: config.retry_initial_backoff_ms,
            retry_max_backoff_ms: config.retry_max_backoff_ms,
            context_window: config.context_window,
            model_context_windows: config.model_context_windows.clone(),
        }
    }
}

/// Filename of the accounts config under the config dir, shared by the path
/// resolver and the unified config watcher's subscription.
pub const ACCOUNTS_TOML_NAME: &str = "accounts.toml";

/// Resolve the accounts.toml path (e.g. ~/.config/choreographr/accounts.toml).
pub fn accounts_config_path() -> io::Result<PathBuf> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine config directory",
        )
    })?;
    Ok(config_dir.join("choreographr").join("accounts.toml"))
}

/// Spawn the thin consumer that watches `accounts.toml` edits surfaced by the
/// unified config transport and forwards them to the daemon command loop.
///
/// This thread does NO reading or comparing — the transport has already
/// classified the event to create/modify/remove, and the daemon command loop
/// is the single writer of `state.accounts`, so it is the one that re-reads,
/// parse-compares, and applies. Self-writes (from `AccountManager::add`/
/// `remove`) arrive here too; the parse-compare in the command loop is what
/// makes them no-ops. The thread is detached and lives until the process
/// exits.
pub fn spawn_accounts_watcher(
    daemon_tx: std::sync::mpsc::Sender<crate::daemon::DaemonCommand>,
    accounts_rx: crossbeam_channel::Receiver<crate::config_watch::ConfigChange>,
) {
    let _ = std::thread::Builder::new()
        .name("accounts-config-watch".into())
        .spawn(move || {
            for _change in accounts_rx.iter() {
                if daemon_tx
                    .send(crate::daemon::DaemonCommand::AccountsReload)
                    .is_err()
                {
                    tracing::info!("daemon command loop gone; stopping accounts config watcher");
                    break;
                }
            }
        });
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
        // Validate the retry knobs of every account at load time (not lazily
        // at first use): a typo'd retry_max_backoff_ms would otherwise slip
        // into the client and silently void the retry budget gate.  The file
        // is rejected as a whole, matching the behavior of a TOML parse
        // error above, so the user sees a pinpointed message instead of a
        // silently-clamped config.
        for cfg in &file.account {
            cfg.validate()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        }
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
    ///
    /// The file is written **deterministically** (accounts sorted by name) and
    /// **atomically** (temp + fsync + rename via `write_file_atomic`). The
    /// deterministic ordering makes the on-disk file stable and diff-friendly;
    /// the atomic write means the config-file watcher can never observe a torn
    /// file mid-write (the daemon and an editor can race on this file).
    pub fn save(&self) -> io::Result<()> {
        #[derive(Serialize)]
        struct AccountsFile<'a> {
            #[serde(rename = "account")]
            accounts: Vec<&'a AccountConfig>,
        }
        let mut configs: Vec<&AccountConfig> = self.accounts.values().collect();
        configs.sort_by(|a, b| a.name.cmp(&b.name));
        let file = AccountsFile { accounts: configs };
        let toml_str =
            toml::to_string(&file).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        choreo_ai_protocols::write_file_atomic(&self.config_path, toml_str.as_bytes())
    }

    /// Add an account. Errors if the name already exists.
    pub fn add(&mut self, config: AccountConfig) -> Result<(), String> {
        // Reject invalid retry knobs before they are persisted (the CLI add
        // path surfaces the error to the user).
        config.validate()?;
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

    /// The on-disk path this manager loads from / saves to. Empty for an
    /// un-initialized manager (`AccountManager::empty`), which the daemon uses
    /// to gate the external-edit reload until unlock has loaded a real path.
    pub fn path(&self) -> &Path {
        &self.config_path
    }

    /// The account names, sorted for deterministic iteration. Used by the
    /// external-edit reload to compute which accounts were removed.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.accounts.keys().cloned().collect();
        names.sort();
        names
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
    fn save_is_deterministic_across_rewrites() {
        // The external-edit reload gate is a parse-compare, but a deterministic
        // on-disk file is still valuable: identical logical state must produce
        // identical bytes, so an editor diffing the file sees stable content and
        // the daemon's own rewrites never churn the file needlessly. The old
        // HashMap-order serialization produced a different byte order on each
        // save; sorting by name makes it stable.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);
        mgr.add(AccountConfig::simple("zebra", "openai")).unwrap();
        mgr.add(AccountConfig::simple("alpha", "anthropic"))
            .unwrap();
        mgr.add(AccountConfig::simple("mango", "ollama")).unwrap();

        let first = std::fs::read_to_string(&path).unwrap();
        // A no-op save (same logical accounts) rewrites identical bytes.
        mgr.save().unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            first, second,
            "identical logical state must serialize identically"
        );

        // The on-disk order is sorted by account name, not insertion order.
        let alpha = first.find("name = \"alpha\"").unwrap();
        let mango = first.find("name = \"mango\"").unwrap();
        let zebra = first.find("name = \"zebra\"").unwrap();
        assert!(
            alpha < mango && mango < zebra,
            "accounts sorted by name on disk"
        );
    }

    #[test]
    fn path_and_names_accessors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);
        // An un-initialized manager has an empty path and no names.
        assert!(AccountManager::empty().path().as_os_str().is_empty());
        assert!(AccountManager::empty().names().is_empty());

        assert_eq!(mgr.path(), path);
        mgr.add(AccountConfig::simple("z", "openai")).unwrap();
        mgr.add(AccountConfig::simple("a", "anthropic")).unwrap();
        assert_eq!(mgr.names(), vec!["a".to_string(), "z".to_string()]);
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
        let mut svc_config = choreo_ai_protocols::openai::ServiceConfig::default();
        let cfg = AccountConfig {
            base_url: Some("https://custom.api.com/v1".to_string()),
            streaming: Some(false),
            retry_max_attempts: Some(5),
            connect_timeout_secs: Some(30),
            request_timeout_secs: Some(120),
            total_timeout_secs: Some(1800),
            model_list_path: Some("/v2/models".to_string()),
            responses_path: Some("/v2/responses".to_string()),
            chat_completions_path: Some("/v2/chat".to_string()),
            default_request_format: Some(choreo_ai_protocols::openai::RequestFormat::Responses),
            chat_completions_max_tokens: Some(2048),
            chat_completions_max_tokens_field: Some(
                choreo_ai_protocols::openai::MaxTokensField::MaxTokens,
            ),
            responses_max_output_tokens: Some(4096),
            model_responses_max_output_tokens: Some(std::collections::HashMap::from([(
                "gpt-4".to_string(),
                8192,
            )])),
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
        assert_eq!(svc_config.total_timeout_secs, 1800);
        assert_eq!(svc_config.model_list_path, "/v2/models");
        assert_eq!(svc_config.responses_path, "/v2/responses");
        assert_eq!(svc_config.chat_completions_path, "/v2/chat");
        assert_eq!(
            svc_config.default_request_format,
            choreo_ai_protocols::openai::RequestFormat::Responses
        );
        assert_eq!(svc_config.chat_completions_max_tokens, Some(2048));
        assert_eq!(
            svc_config.chat_completions_max_tokens_field,
            choreo_ai_protocols::openai::MaxTokensField::MaxTokens
        );
        assert_eq!(svc_config.retry_initial_backoff_ms, 500);
        assert_eq!(svc_config.retry_max_backoff_ms, 60000);
        assert_eq!(svc_config.responses_max_output_tokens, Some(4096));
        assert_eq!(
            svc_config.model_responses_max_output_tokens.get("gpt-4"),
            Some(&8192)
        );
    }

    #[test]
    fn provider_overrides_carries_total_timeout() {
        let cfg = AccountConfig {
            connect_timeout_secs: Some(15),
            request_timeout_secs: Some(60),
            total_timeout_secs: Some(7200),
            ..AccountConfig::simple("ovr", "openai")
        };
        let overrides = cfg.provider_overrides();
        // The protocol-agnostic carrier must forward every shared timeout
        // field so the non-OpenAI provider crates see the same values.
        assert_eq!(overrides.connect_timeout_secs, Some(15));
        assert_eq!(overrides.request_timeout_secs, Some(60));
        assert_eq!(overrides.total_timeout_secs, Some(7200));
    }

    #[test]
    fn validate_rejects_max_backoff_exceeding_the_ceiling() {
        // A retry_max_backoff_ms past the hard ceiling would void the retry
        // budget gate (every provider cooldown would "fit"); the daemon must
        // reject it with a pinpointed message instead of silently clamping.
        let cfg = AccountConfig {
            retry_max_backoff_ms: Some(choreo_ai_protocols::retry::MAX_BACKOFF_MS + 1),
            ..AccountConfig::simple("ovr", "openai")
        };
        let err = cfg.validate().expect_err("must reject over-ceiling max");
        assert!(err.contains("retry_max_backoff_ms"), "{err}");
        assert!(err.contains("ovr"), "error must name the account: {err}");
        // Exactly at the ceiling is a legitimate configuration.
        let cfg = AccountConfig {
            retry_max_backoff_ms: Some(choreo_ai_protocols::retry::MAX_BACKOFF_MS),
            ..AccountConfig::simple("ovr", "openai")
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_initial_above_max() {
        // initial > max would collapse the exponential backoff to its cap on
        // the first attempt — a config mistake the daemon refuses up front.
        let cfg = AccountConfig {
            retry_initial_backoff_ms: Some(2000),
            retry_max_backoff_ms: Some(1000),
            ..AccountConfig::simple("ovr", "openai")
        };
        let err = cfg.validate().expect_err("must reject inverted backoff");
        assert!(err.contains("retry_initial_backoff_ms"), "{err}");
    }

    #[test]
    fn validate_rejects_initial_over_ceiling_without_max() {
        // An over-ceiling initial with max unset used to slip through: the
        // library clamp in RetryConfig::new would then also LIFT the default
        // 30 s max up to the clamped initial, silently widening the
        // Retry-After budget gate the user never asked to change.  The daemon
        // must refuse it up front like the max knob.
        let cfg = AccountConfig {
            retry_initial_backoff_ms: Some(choreo_ai_protocols::retry::MAX_BACKOFF_MS + 1),
            ..AccountConfig::simple("ovr", "openai")
        };
        let err = cfg
            .validate()
            .expect_err("must reject over-ceiling initial");
        assert!(err.contains("retry_initial_backoff_ms"), "{err}");
        assert!(err.contains("ovr"), "error must name the account: {err}");
        // Exactly at the ceiling is a legitimate configuration.
        let cfg = AccountConfig {
            retry_initial_backoff_ms: Some(choreo_ai_protocols::retry::MAX_BACKOFF_MS),
            ..AccountConfig::simple("ovr", "openai")
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_accepts_defaults_and_in_budget_knobs() {
        // Unset knobs (provider defaults) and a normal range both validate.
        let cfg = AccountConfig::simple("ovr", "openai");
        assert!(cfg.validate().is_ok());
        let cfg = AccountConfig {
            retry_initial_backoff_ms: Some(500),
            retry_max_backoff_ms: Some(60000),
            ..AccountConfig::simple("ovr", "openai")
        };
        assert!(cfg.validate().is_ok());
    }
}
