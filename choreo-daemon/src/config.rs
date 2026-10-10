//! Daemon-level configuration from `config.toml`.
//!
//! Only truly global settings live here — [`DaemonConfig`]'s `max_turns`,
//! `[context]`, and `[cache_warming]`. Provider-level configuration (endpoints,
//! timeouts, retry, credentials) belongs in `accounts.toml` (see
//! [`crate::accounts`]). Fields are all `#[serde(default)]`, so a file written
//! by a newer daemon still parses and unknown keys are ignored.

use choreo_proto::ContextConfig;
use serde::Deserialize;
use std::{fs, io, path::PathBuf};

use crate::cache_warm::CacheWarmingConfig;

/// Daemon-level configuration from config.toml.
///
/// Only truly global settings belong here.  All provider-level
/// configuration (endpoints, timeouts, retry, etc.) belongs in
/// accounts.toml (see [`crate::accounts`]).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonConfig {
    /// Cap on agent-loop turns per request; `None` uses the request/session
    /// default.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Context-file discovery settings (`[context]`); defaults to
    /// `ContextConfig::default()` when absent.
    #[serde(default)]
    pub context: ContextConfig,
    /// Global cache-warming defaults (off by default). Per-account `meter`/
    /// `cache_warming` overrides in accounts.toml take precedence — see
    /// [`crate::cache_warm`].
    #[serde(default)]
    pub cache_warming: CacheWarmingConfig,
}

/// Resolve the config.toml path (e.g. ~/.config/choreographr/config.toml,
/// or `{base}/config/choreographr/config.toml` under `--base-dir`).
///
/// # Errors
///
/// Returns Err if the config directory cannot be determined.
pub fn config_path() -> io::Result<PathBuf> {
    choreo_shared::paths::config_file("config.toml")
}
/// Load daemon-level configuration from config.toml.
///
/// Only daemon-level fields are parsed (`max_turns`, `[context]`);
/// provider-level fields are ignored (they belong in accounts.toml, see
/// [`crate::accounts`]). Returns `DaemonConfig::default()` when the file
/// does not exist.
///
/// # Errors
///
/// Returns Err if the config path cannot be resolved, the file cannot be
/// read, or it is not valid TOML.
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
///
/// # Errors
///
/// Returns Err if the daemon config cannot be loaded while checking for
/// deprecated fields (the warning is logged and the error propagated).
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
        // Cache warming is off by default with the documented thresholds.
        assert_eq!(
            config.cache_warming.mode,
            crate::cache_warm::CacheWarmingMode::Off
        );
        assert_eq!(
            config.cache_warming.min_prefix_tokens,
            crate::cache_warm::DEFAULT_MIN_PREFIX_TOKENS
        );
        let default_savings = crate::cache_warm::DEFAULT_MIN_EXPECTED_SAVINGS;
        assert!((config.cache_warming.min_expected_savings - default_savings).abs() < 1e-12);
    }

    #[test]
    fn daemon_config_parses_cache_warming() {
        let raw = r#"
[cache_warming]
mode = "streaming"
min_prefix_tokens = 64000
min_expected_savings = 0.1
"#;
        let config: DaemonConfig = toml::from_str(raw).unwrap();
        assert_eq!(
            config.cache_warming.mode,
            crate::cache_warm::CacheWarmingMode::Streaming
        );
        assert_eq!(config.cache_warming.min_prefix_tokens, 64_000);
        assert!((config.cache_warming.min_expected_savings - 0.1).abs() < 1e-12);
    }

    #[test]
    fn daemon_config_cache_warming_unknown_mode_falls_back() {
        // A typo'd mode warns and falls back to Off rather than failing the
        // whole config parse.
        let config: DaemonConfig = toml::from_str("[cache_warming]\nmode = \"turbo\"\n").unwrap();
        assert_eq!(
            config.cache_warming.mode,
            crate::cache_warm::CacheWarmingMode::Off
        );
    }

    #[test]
    fn daemon_config_errors_on_invalid_toml() {
        let result: Result<DaemonConfig, _> = toml::from_str("[[[");
        assert!(result.is_err());
    }
}
