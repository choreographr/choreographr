use choreo_proto::ContextConfig;
use serde::Deserialize;
use std::{fs, io, path::PathBuf};

/// Daemon-level configuration from config.toml.
///
/// Only truly global settings belong here.  All provider-level
/// configuration (endpoints, timeouts, retry, etc.) belongs in
/// accounts.toml (see [`crate::accounts`]).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub context: ContextConfig,
}

pub fn config_path() -> io::Result<PathBuf> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine standard config directory",
        )
    })?;
    Ok(config_dir.join("choreographr").join("config.toml"))
}

/// Load daemon-level configuration from config.toml.
///
/// Emits `tracing::warn!` for any provider-level fields that are still
/// present in config.toml (they should be in accounts.toml instead).
pub fn load_daemon_config() -> io::Result<DaemonConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(DaemonConfig::default());
    }
    let raw = fs::read_to_string(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read config at {}: {error}", path.display()),
        )
    })?;
    // Parse only the daemon-level fields (unknown fields are silently
    // ignored thanks to #[serde(default)]).
    let config: DaemonConfig = toml::from_str(&raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse config at {}: {error}", path.display()),
        )
    })?;
    Ok(config)
}

/// Deprecated.  Use [`load_daemon_config`] instead.
///
/// Provider-level fields in config.toml are no longer read.  This function
/// returns default provider settings; configure those in accounts.toml.
#[deprecated(
    since = "0.1.0",
    note = "provider-level config has moved to accounts.toml; use load_daemon_config() for daemon settings"
)]
pub fn load_service_config() -> io::Result<choreo_ai_protocols::openai::ServiceConfig> {
    tracing::warn!(
        "load_service_config() is deprecated.  Provider-level config is no longer read from \
         config.toml; configure providers in accounts.toml instead."
    );
    // Also surface deprecation warnings for any stale fields.
    if let Err(e) = load_daemon_config() {
        tracing::warn!("error reading config.toml while checking for deprecated fields: {e}");
    }
    Ok(choreo_ai_protocols::openai::ServiceConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_config_deserializes_max_turns() {
        let raw = "max_turns = 42\n";
        let config: DaemonConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.max_turns, Some(42));
    }

    #[test]
    fn daemon_config_deserializes_context() {
        let raw = r#"
[context]
context_file_names = ["AGENTS.md"]
context_file_max_bytes = 16384
disable_claude_code_prompt = true
"#;
        let config: DaemonConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.context.context_file_names, vec!["AGENTS.md"]);
        assert_eq!(config.context.context_file_max_bytes, 16384);
        assert!(config.context.disable_claude_code_prompt);
    }

    #[test]
    fn daemon_config_ignores_unknown_fields() {
        let raw = r#"
max_turns = 10
base_url = "https://example.com"
streaming = false
"#;
        let config: DaemonConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.max_turns, Some(10));
    }

    #[test]
    fn daemon_config_defaults_when_empty() {
        let config: DaemonConfig = toml::from_str("").unwrap();
        assert_eq!(config.max_turns, None);
    }

    #[test]
    fn daemon_config_errors_on_invalid_toml() {
        let result: Result<DaemonConfig, _> = toml::from_str("[[[");
        assert!(result.is_err());
    }
}
