//! choreo-gui's own settings store — `gui-settings.toml` next to the rest of
//! the config family (`~/.config/choreographr/gui-settings.toml`, or the iOS
//! app-sandbox equivalent that `choreo_keystore::paths::config_dir()`
//! resolves to). This is GUI-level USER PREFERENCE storage, deliberately NOT
//! the daemon DB: the daemon database holds session/credential state, and a
//! preference that gates how the embedded daemon is *constructed* (the
//! on-device tools bridge) must be readable before any daemon exists.
//!
//! The store is deliberately MINIMAL (one field so far) and deliberately
//! TOLERANT on load — the `known_servers.toml` convention: a missing,
//! unreadable, or unparseable file loads as DEFAULTS with a warning, never
//! an error, because a corrupt preference file must never keep the app from
//! launching. Writes are whole-file (tiny file; no lock needed at GUI scale
//! — the GUI is the single writer of its own settings in practice).
//!
//! The whole module compiles on every target (the settings file is a
//! platform-neutral concept; desktop may grow its own fields later) — only
//! the iOS consumers below are cfg-gated. Host unit tests exercise the
//! load/persist round-trip deterministically via the shared
//! `TestConfigGuard` config-root override, so the iOS-only call sites are
//! the only untested-on-host pieces.

// the host unit tests still exercise the load/persist paths.
#![cfg_attr(not(target_os = "ios"), allow(dead_code))]

use std::path::{Path, PathBuf};

/// The persisted GUI preferences. New fields MUST be `#[serde(default)]`
/// (or wrapped in Option): the file is written before fields exist in
/// older binaries, and a missing field must deserialize as the default —
/// never as a parse error (the load policy is tolerant).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct GuiSettings {
    /// Whether the iOS embedded daemon registers the on-device tool group
    /// (clipboard_write/clipboard_read/open_url/notify over the Swift
    /// bridge). Default ON: the four tools need no iOS permission prompts,
    /// so the sensible default is available. Toggling takes effect on the
    /// NEXT app start, because the bridge is handed to `DaemonState::open`
    /// during startup — there is no live re-registration path.
    #[serde(default = "default_true")]
    pub(crate) on_device_tools: bool,
}

fn default_true() -> bool {
    true
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            on_device_tools: true,
        }
    }
}

/// Path to the GUI's own settings file, resolved through the shared config
/// dir (so test overrides and the iOS sandbox agree with every other store).
pub(crate) fn settings_path() -> Result<PathBuf, String> {
    choreo_keystore::paths::config_dir()
        .map(|dir| dir.join("gui-settings.toml"))
        .map_err(|e| format!("cannot resolve config dir: {e}"))
}

/// Load from the default path (tolerant policy — see module docs).
pub(crate) fn load() -> GuiSettings {
    match settings_path() {
        Ok(path) => load_from(&path),
        Err(e) => {
            tracing::warn!(error = %e, "gui settings path unavailable; using defaults");
            GuiSettings::default()
        }
    }
}

/// Load from an explicit path (the test seam; production callers use
/// [`load`]). Missing/corrupt/unreadable file → defaults + warn, never an
/// error: a broken preference must never block app launch.
pub(crate) fn load_from(path: &Path) -> GuiSettings {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "gui-settings.toml is not valid TOML; using defaults"
                );
                GuiSettings::default()
            }
        },
        // Missing file = first run: defaults, no warning (the normal case).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GuiSettings::default(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not read gui-settings.toml; using defaults"
            );
            GuiSettings::default()
        }
    }
}

impl GuiSettings {
    /// Persist to the default path (creating the config dir on first write).
    pub(crate) fn persist(&self) -> Result<(), String> {
        let path = settings_path()?;
        self.persist_to(&path)
    }

    /// Persist to an explicit path (the test seam). Whole-file rewrite; the
    /// file is tiny and the GUI is its only writer, so no advisory lock is
    /// taken (unlike known_servers.toml, which multiple processes share).
    pub(crate) fn persist_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create settings dir {}: {e}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| format!("cannot serialize gui settings: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        tracing::debug!(path = %path.display(), "persisted gui settings");
        Ok(())
    }
}

// ── iOS startup cache + toggle ───────────────────────────────────────────────
//
// The bridge decision happens ONCE at startup (inside
// `embedded_connection_mode`, before `DaemonState::open`), but the toolbar
// toggle needs the same value afterwards. A lock-free AtomicBool is the
// cache — it carries one startup-resolved bit and no protocol data; all
// real state changes go through the persisted file (the source of truth the
// next launch reads), never through the atomic alone.
#[cfg(target_os = "ios")]
pub(crate) static ON_DEVICE_TOOLS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Startup hook (iOS only): load the persisted setting and cache it in
/// [`ON_DEVICE_TOOLS`]; returns the value for the caller's bridge decision.
#[cfg(target_os = "ios")]
pub(crate) fn init_on_device_tools() -> bool {
    let enabled = load().on_device_tools;
    tracing::info!(enabled, "on-device tools setting loaded");
    ON_DEVICE_TOOLS.store(enabled, std::sync::atomic::Ordering::Relaxed);
    enabled
}

/// The toolbar toggle's read side (iOS only): the startup-cached value.
#[cfg(target_os = "ios")]
pub(crate) fn on_device_tools_cached() -> bool {
    ON_DEVICE_TOOLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Flip the on-device-tools preference, persist it, and update the cache —
/// in that order: the FILE is the source of truth (the next launch reads
/// it), so persist FIRST and only adopt the new value in the cache on
/// success (a failed write leaves the cache matching what actually survives
/// on disk, and the next launch reverts to it). Returns the new value, or
/// an error message for the UI.
#[cfg(target_os = "ios")]
pub(crate) fn toggle_on_device_tools() -> Result<bool, String> {
    let new_value = !on_device_tools_cached();
    GuiSettings {
        on_device_tools: new_value,
    }
    .persist()?;
    ON_DEVICE_TOOLS.store(new_value, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(enabled = new_value, "on-device tools setting toggled");
    Ok(new_value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip through the real path resolution with the shared config
    /// root overridden to a tempdir — deterministic, no sleeps, no threads.
    /// `TestConfigGuard` resets the override on drop (even on a panicking
    /// assert); the TempDir must be held for the whole body.
    #[test]
    fn persist_and_reload_round_trip() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(temp.path().to_path_buf()));

        // First run: no file → default ON.
        assert!(load().on_device_tools);

        // Toggle OFF, persist through the default path, reload: the
        // preference is durable and the file landed in the config family.
        let settings = GuiSettings {
            on_device_tools: false,
        };
        settings.persist().unwrap();
        let path = settings_path().unwrap();
        assert!(path.ends_with("gui-settings.toml"));
        assert!(path.starts_with(temp.path()));
        assert!(!load().on_device_tools);

        // Flip back ON and confirm the round trip in the other direction.
        GuiSettings::default().persist().unwrap();
        assert!(load().on_device_tools);
    }

    /// The tolerant load policy: a garbage file loads as DEFAULTS (never an
    /// error, never a half-value) — a corrupt preference must never keep
    /// the app from launching.
    #[test]
    fn corrupt_file_loads_defaults() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("gui-settings.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();
        assert_eq!(load_from(&path), GuiSettings::default());

        // A VALID file with the field ABSENT also loads as the default:
        // every field is #[serde(default)] so older files stay parseable.
        std::fs::write(&path, "").unwrap();
        assert_eq!(load_from(&path), GuiSettings::default());
    }

    /// The explicit-path persist seam writes exactly the field we set —
    /// pins the TOML shape future readers will hand-edit.
    #[test]
    fn persisted_shape_has_the_field() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("nested").join("gui-settings.toml");
        GuiSettings {
            on_device_tools: false,
        }
        .persist_to(&path)
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("on_device_tools = false"));
    }
}
