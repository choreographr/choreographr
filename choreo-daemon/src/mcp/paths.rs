//! Config-directory resolution and the shared config-file names.
//!
//! Owns the thread-local test override, the `choreographr/mcp.json` daemon-tier
//! config path, and the neighbouring `trust.toml` path, plus the two file-name
//! constants (`mcp.json`'s key shape and `.mcp.json` live in `config.rs`; these
//! are the *paths*, not the formats). Resolved through
//! [`choreo_shared::paths`] so the layout follows `--base-dir`/XDG like every
//! other config file.

use std::path::PathBuf;

/// The project-tier MCP config file name (a checkout's own server set).
///
/// The MCP-ecosystem convention for a repository-local server declaration. The
/// daemon-tier counterpart lives beside this crate's other config files
/// (`<config>/choreographr/mcp.json`).
pub const PROJECT_CONFIG_FILE: &str = ".mcp.json";

/// The MCP trust-store file name (`<config>/choreographr/trust.toml`).
pub const TRUST_FILE: &str = "trust.toml";

thread_local! {
    /// Test-only override for the base config directory. When set,
    /// [`config_dir`] returns `<root>/choreographr` instead of the user's real
    /// config dir.
    ///
    /// Deliberately NOT `#[cfg(test)]`-gated: integration tests in `tests/`
    /// compile the crate without `cfg(test)`, so the hook must exist in normal
    /// builds too (it is a no-op unless explicitly set).
    static TEST_CONFIG_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only override for the base config directory (see `TEST_CONFIG_ROOT`).
///
/// This is needed because `dirs::config_dir()` honors `XDG_CONFIG_HOME` only
/// on Linux — on macOS it always returns `$HOME/Library/Application Support`,
/// so an integration test cannot redirect the config path via environment
/// variables.
#[doc(hidden)]
pub fn set_test_config_root(root: Option<PathBuf>) {
    TEST_CONFIG_ROOT.with(|cell| cell.replace(root));
}

/// Resolve the choreographr config directory (`<config>/choreographr`).
///
/// # Errors
///
/// Returns an error when the config directory cannot be determined.
pub fn config_dir() -> std::io::Result<PathBuf> {
    if let Some(root) = TEST_CONFIG_ROOT.with(|cell| cell.borrow().clone()) {
        return Ok(root.join("choreographr"));
    }
    choreo_shared::paths::config_dir()
}

/// The path of the MCP trust store (`<config>/choreographr/trust.toml`), or
/// `None` when the config directory cannot be resolved.
#[must_use]
pub fn trust_path() -> Option<PathBuf> {
    config_dir().ok().map(|d| d.join(TRUST_FILE))
}
