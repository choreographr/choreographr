use std::sync::OnceLock;

use ratatui::style::Color;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Load the default syntax set (with newline-aware grammars).
pub(crate) fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    TS.get_or_init(ThemeSet::load_defaults)
}

/// The theme shared across all syntax-highlighted output in tai-tui.
///
/// Uses a dark-terminal-friendly theme by default; falls back to the first
/// available theme if the named one is missing (shouldn't happen in practice
/// since syntect ships `base16-ocean.dark` in its default set).
pub(crate) fn highlight_theme() -> &'static Theme {
    const THEME_NAME: &str = "base16-ocean.dark";
    theme_set().themes.get(THEME_NAME).unwrap_or_else(|| {
        theme_set().themes.values().next().unwrap_or_else(|| {
            // Fallback: create a minimal empty theme. This should never
            // happen since syntect ships with bundled themes.
            static FALLBACK: OnceLock<Theme> = OnceLock::new();
            FALLBACK.get_or_init(|| Theme {
                name: Some("fallback".into()),
                ..Theme::default()
            })
        })
    })
}

/// Convert a syntect RGBA colour to a ratatui `Color`.
///
/// Syntect colours with alpha < 128 are treated as transparent, mapping to
/// `Color::Reset` so the terminal default shows through.
pub(crate) fn to_ratatui_color(c: syntect::highlighting::Color) -> Color {
    if c.a < 128 {
        Color::Reset
    } else {
        Color::Rgb(c.r, c.g, c.b)
    }
}

/// Look up a syntect syntax definition for the given file path by extension.
///
/// Uses syntect's built-in extension matching, which covers all languages
/// in the default syntax set. Does **not** perform any filesystem I/O,
/// unlike `find_syntax_for_file` (which tries to open the path for
/// content-based sniffing).
pub(crate) fn syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?;
    syntax_set().find_syntax_by_extension(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_for_path_rs_is_rust() {
        let s = syntax_for_path("src/main.rs");
        assert!(s.is_some());
        assert_eq!(s.unwrap().name, "Rust");
    }

    #[test]
    fn syntax_for_path_extensionless_returns_none() {
        assert!(syntax_for_path("Makefile").is_none());
    }

    #[test]
    fn syntax_for_path_unknown_extension_returns_none() {
        assert!(syntax_for_path("foo.xyzzy").is_none());
    }

    #[test]
    fn syntax_for_path_does_not_perform_filesystem_io() {
        // Should not try to open this path on disk
        let s = syntax_for_path("/nonexistent/path/to/file.py");
        assert!(s.is_some());
        assert_eq!(s.unwrap().name, "Python");
    }

    #[test]
    fn syntax_for_path_empty_string_returns_none() {
        assert!(syntax_for_path("").is_none());
    }
}
