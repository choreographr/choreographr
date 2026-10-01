use super::super::*;

// ── find_syntax ──────────────────────────────────────────────────────

#[test]
fn find_syntax_rust() {
    let ss = syntax_set();
    let result = find_syntax(ss, "rust");
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "Rust");
}

#[test]
fn find_syntax_typescript_maps_to_javascript() {
    let ss = syntax_set();
    let result = find_syntax(ss, "typescript");
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "JavaScript");
}

#[test]
fn find_syntax_tsx_maps_to_javascript() {
    let ss = syntax_set();
    let result = find_syntax(ss, "tsx");
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "JavaScript");
}

#[test]
fn find_syntax_vue_maps_to_html() {
    let ss = syntax_set();
    let result = find_syntax(ss, "vue");
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "HTML");
}

#[test]
fn find_syntax_svelte_maps_to_html() {
    let ss = syntax_set();
    let result = find_syntax(ss, "svelte");
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "HTML");
}

#[test]
fn find_syntax_unknown_returns_none() {
    let ss = syntax_set();
    let result = find_syntax(ss, "not-a-real-language-12345");
    assert!(result.is_none());
}

// ── highlight_code ───────────────────────────────────────────────────

#[test]
fn highlight_code_known_language_produces_coloured_spans() {
    let lines = highlight_code(Some("rust"), "fn main() {}");
    assert!(!lines.is_empty(), "should produce at least one line");

    let has_colour = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
    assert!(has_colour, "highlighted Rust should have coloured spans");
}

#[test]
fn highlight_code_unknown_language_produces_output() {
    let lines = highlight_code(Some("this-is-not-a-real-language"), "some text");
    assert!(!lines.is_empty(), "should still produce output");
}

#[test]
#[expect(clippy::assert_is_empty)] // this clippy version wants assert_ne!(v, [] as [...]) here — worse
fn highlight_code_none_language_uses_plain_text() {
    let lines = highlight_code(None, "plain text");
    assert!(!lines.is_empty());
}

#[test]
#[expect(clippy::assert_is_empty)] // this clippy version wants assert_ne!(v, [] as [...]) here — worse
fn highlight_code_empty_string() {
    let lines = highlight_code(Some("rust"), "");
    assert!(!lines.is_empty());
}

#[test]
fn highlight_code_multi_line() {
    let lines = highlight_code(Some("python"), "def foo():\n    pass");
    assert_eq!(lines.len(), 2, "should have one line per code line");
}
