use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tai_proto::AccountInfo;

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
    pub retry_max_attempts: Option<u32>,
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

impl AccountConfig {
    /// Apply this account's config overrides to a ServiceConfig.
    pub fn apply_overrides(&self, config: &mut crate::openai::ServiceConfig) {
        if let Some(base_url) = &self.base_url {
            config.base_url = base_url.clone();
        }
        if let Some(streaming) = self.streaming {
            config.streaming = streaming;
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

    fn config(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            provider: "openai".to_string(),
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
        }
    }

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

        mgr.add(config("prod")).unwrap();
        assert!(mgr.contains("prod"));
        assert_eq!(mgr.get("prod").unwrap().provider, "openai");
    }

    #[test]
    fn add_duplicate_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(config("dup")).unwrap();
        let err = mgr.add(config("dup")).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn remove_account() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(config("temp")).unwrap();
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

        mgr.add(config("zebra")).unwrap();
        mgr.add(config("alpha")).unwrap();
        mgr.add(config("omega")).unwrap();

        let first = mgr.first().unwrap();
        assert_eq!(first.name, "alpha");
    }

    #[test]
    fn list_sorted_alphabetically() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(config("z")).unwrap();
        mgr.add(config("a")).unwrap();
        let list = mgr.list(&std::collections::HashSet::new());
        assert_eq!(list[0].name, "a");
        assert_eq!(list[1].name, "z");
    }

    #[test]
    fn list_reports_credential_status() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.toml");
        let mut mgr = manager(&path);

        mgr.add(config("has_it")).unwrap();
        mgr.add(config("missing")).unwrap();

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

        mgr.add(config("b")).unwrap();
        mgr.add(config("a")).unwrap();
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
            mgr.add(config("persist")).unwrap();
        }
        let mgr = manager(&path);
        assert!(mgr.contains("persist"));
    }

    #[test]
    fn to_info_roundtrip() {
        let cfg = AccountConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
        };
        let info = cfg.to_info(true);
        assert_eq!(info.name, "test");
        assert_eq!(info.provider, "openai");
        assert!(info.has_credential);
    }

    #[test]
    fn apply_overrides() {
        let mut svc_config = crate::openai::ServiceConfig::default();
        let cfg = AccountConfig {
            name: "ovr".to_string(),
            provider: "openai".to_string(),
            base_url: Some("https://custom.api.com/v1".to_string()),
            streaming: Some(false),
            retry_max_attempts: Some(5),
            connect_timeout_secs: Some(30),
            request_timeout_secs: Some(120),
        };
        cfg.apply_overrides(&mut svc_config);
        assert_eq!(svc_config.base_url, "https://custom.api.com/v1");
        assert!(!svc_config.streaming);
        assert_eq!(svc_config.retry_max_attempts, 5);
        assert_eq!(svc_config.connect_timeout_secs, 30);
        assert_eq!(svc_config.request_timeout_secs, 120);
    }
}
