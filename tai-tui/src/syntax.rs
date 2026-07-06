use std::sync::OnceLock;

use ratatui::style::Color;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(|| SyntaxSet::load_defaults_newlines())
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
    theme_set()
        .themes
        .get(THEME_NAME)
        .unwrap_or_else(|| theme_set().themes.values().next().expect("ThemeSet is empty"))
}

/// Load the default syntax set (with newline-aware grammars).
pub(crate) fn default_syntax_set() -> &'static SyntaxSet {
    syntax_set()
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

/// Map a file path to a syntect-compatible language token for highlighting.
///
/// Returns `None` for unknown extensions, which callers should handle by
/// falling back to plain text.
pub(crate) fn language_for_path(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "go" => Some("go"),
        "rb" => Some("ruby"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "hpp" | "cc" | "cxx" | "c++" | "h++" => Some("cpp"),
        "cs" => Some("csharp"),
        "swift" => Some("swift"),
        "kt" | "kts" => Some("kotlin"),
        "scala" => Some("scala"),
        "php" => Some("php"),
        "pl" | "pm" => Some("perl"),
        "lua" => Some("lua"),
        "sh" | "bash" | "zsh" | "bashrc" | "profile" => Some("bash"),
        "fish" => Some("fish"),
        "sql" => Some("sql"),
        "r" => Some("r"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "json" => Some("json"),
        "xml" | "html" | "htm" | "xhtml" => Some("xml"),
        "css" => Some("css"),
        "scss" => Some("scss"),
        "less" => Some("less"),
        "md" | "markdown" => Some("markdown"),
        "svelte" => Some("svelte"),
        "vue" => Some("vue"),
        "dart" => Some("dart"),
        "ex" | "exs" => Some("elixir"),
        "erl" => Some("erlang"),
        "hs" | "lhs" => Some("haskell"),
        "ml" | "mli" => Some("ocaml"),
        "zig" => Some("zig"),
        "nim" => Some("nim"),
        "tex" | "sty" | "cls" | "ltx" => Some("latex"),
        "dockerfile" => Some("dockerfile"),
        "cmake" | "cmake.in" => Some("cmake"),
        "makefile" | "mk" => Some("makefile"),
        "proto" => Some("protobuf"),
        "rspec" | "feature" => Some("gherkin"),
        "gradle" => Some("gradle"),
        _ => None,
    }
}
