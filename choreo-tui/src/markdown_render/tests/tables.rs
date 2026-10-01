use super::super::*;

#[test]
fn table_cells_pretty_print_math() {
    let result = markdown_lines("| A | B |\n|---|---|\n| $x_i$ | $\\alpha$ |", 80);
    let rendered: Vec<String> = result.iter().map(ToString::to_string).collect();
    let rendered = rendered.join("\n");
    assert!(
        rendered.contains("xᵢ"),
        "inline math subscript in table cell: {rendered}"
    );
    assert!(rendered.contains("α"), "greek in table cell: {rendered}");
}

#[test]
fn table_uses_rounded_corners_and_a_plain_header_rule() {
    // nushell-style frame: rounded OUTER corners, square T-junctions, and a
    // uniform header rule — the GFM delimiter row's alignment colons are
    // expressed by cell padding, never echoed into the rendered rule.
    let md = "| Name | Role | Years |\n|:--|:--:|--:|\n| Ada Lovelace | Mathematician | 1842 |";
    let lines = markdown_lines(md, 60);
    let joined = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    // Outer corners are rounded…
    for corner in ['╭', '╮', '╰', '╯'] {
        assert!(joined.contains(corner), "rounded corner {corner}: {joined}");
    }
    // …no square corner survives anywhere…
    for corner in ['┌', '┐', '└', '┘'] {
        assert!(!joined.contains(corner), "square corner {corner}: {joined}");
    }
    // …and the T-junctions stay square (only the frame's corners round).
    assert!(joined.contains('┬'), "top junction: {joined}");
    assert!(joined.contains('╯'), "bottom junction: {joined}");

    // The header rule is uniform: none of the source delimiter row's colons
    // may appear (the fixed cell text below contains no colon of its own).
    assert!(!joined.contains(':'), "no alignment colons: {joined}");

    // The header row — and only it — is drawn bold.
    let header = lines
        .iter()
        .find(|line| line.to_string().contains("Name"))
        .expect("header row present");
    assert!(
        header
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
        "header row must be bold: {header:?}"
    );
    let body = lines
        .iter()
        .find(|line| line.to_string().contains("Ada Lovelace"))
        .expect("body row present");
    assert!(
        !body
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
        "body row must not be bold: {body:?}"
    );
}
