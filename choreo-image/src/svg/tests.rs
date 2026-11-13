//! Unit tests for the shared SVG parse and rasterize policy.
//!
//! These pin the two decisions the module exists for: that a rasterized fill
//! keeps the colour the document specifies when it is translucent (the alpha
//! convention at the `tiny-skia` → `image` boundary), and that a document nested
//! past [`MAX_SVG_DEPTH`] is refused rather than parsed.

use super::*;

/// Wrap `body` in a 4×4 viewport, the smallest canvas that leaves an interior
/// pixel unambiguous under anti-aliasing.
fn document(body: &str) -> String {
    format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">{body}</svg>"#)
}

/// An SVG whose body nests `depth` `<g>` elements around a filled square.
fn nested_groups(depth: usize) -> String {
    let mut body = "<g>".repeat(depth);
    body.push_str(r##"<rect width="4" height="4" fill="#000000"/>"##);
    body.push_str(&"</g>".repeat(depth));
    document(&body)
}

#[test]
fn a_parse_applies_the_shared_options() {
    // The rendered size comes from the tree's own viewport, so a parse that
    // dropped the shared `Options` would still pass this — but a parse that
    // failed to share them would make the depth test below unreachable.
    let tree = parse(document(r#"<rect width="4" height="4"/>"#).as_bytes()).expect("parses");
    let size = tree.size();
    assert_eq!((size.width(), size.height()), (4.0, 4.0));
}

#[test]
fn a_translucent_fill_keeps_its_straight_alpha_colour() {
    // Half-opacity red is the pixel that separates the two alpha conventions:
    // premultiplied storage holds (128, 0, 0, 128), and straight alpha — what
    // the `image` crate's RgbaImage means — holds (255, 0, 0, 128). Reading the
    // premultiplied buffer out as straight would deliver a dark red here.
    let tree = parse(
        document(r##"<rect width="4" height="4" fill="#ff0000" fill-opacity="0.5"/>"##).as_bytes(),
    )
    .expect("a solid opaque square parses");
    let image = render(&tree, tiny_skia::Transform::identity(), 4, 4).expect("rasterizes");

    let [red, green, blue, alpha] = image.get_pixel(2, 2).0;
    assert!(
        red >= 250,
        "red must be the straight value (~255), got {red} in {:?}",
        [red, green, blue, alpha]
    );
    assert_eq!(
        (green, blue),
        (0, 0),
        "a pure red fill has no green or blue"
    );
    assert!(
        (125..=130).contains(&alpha),
        "half opacity is ~128, got {alpha}"
    );
}

#[test]
fn nesting_within_the_limit_parses() {
    let tree = parse(nested_groups(8).as_bytes()).expect("a shallow tree parses");
    assert_eq!(tree.size().width(), 4.0);
}

#[test]
fn a_wide_document_is_not_a_deep_one() {
    // The cap is on nesting, not on size: the parser's own element limit is a
    // separate, much larger bound, so a document with many siblings must still
    // parse. This keeps the depth test from passing on a document that merely
    // tripped a node count.
    let mut body = String::new();
    for _ in 0..2_000 {
        body.push_str(r#"<rect width="1" height="1"/>"#);
    }
    parse(document(&body).as_bytes()).expect("a wide, shallow document parses");
}

#[test]
fn nesting_deeper_than_the_limit_is_rejected() {
    // Comfortably past the cap: the parser counts element nesting, so the margin
    // keeps the test from depending on whether the root element counts.
    let depth = usize::try_from(MAX_SVG_DEPTH).expect("u32 fits usize") + 8;
    let error = parse(nested_groups(depth).as_bytes()).expect_err("past the cap must be refused");
    // `usvg` reports a refused document as a parse failure; the inner error is the
    // XML parser's `NodesLimitReached`, which is the depth budget `max_depth`
    // feeds. Matching the outer variant keeps this pinned to the contract
    // (`ParsingFailed`) rather than to that crate's variant name.
    assert!(
        matches!(error, usvg::Error::ParsingFailed(_)),
        "a too-deep document is a parse failure, got {error:?}"
    );
    assert!(
        error.to_string().to_lowercase().contains("limit"),
        "the failure names the limit it hit, got {error}"
    );
}

#[test]
fn a_zero_sized_target_is_refused() {
    let tree = parse(document(r#"<rect width="4" height="4"/>"#).as_bytes()).expect("parses");
    let error = render(&tree, tiny_skia::Transform::identity(), 0, 4)
        .expect_err("a zero-width pixmap cannot be allocated");
    assert!(matches!(
        error,
        RenderError::TargetSize {
            width: 0,
            height: 4
        }
    ));
}
