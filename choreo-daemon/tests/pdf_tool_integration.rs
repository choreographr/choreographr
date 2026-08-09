//! Integration tests for the native PDF tools (`pdf_classify`,
//! `pdf_to_markdown`).
//!
//! These exercise the full registry path (`ToolRegistry::execute_json`) and
//! the exported tool functions against real tempfile PDFs. The deterministic
//! fixture builders live in `src/tools/pdf/test_fixtures.rs` and are shared
//! with the unit tests via a `#[path]` include (integration tests cannot
//! `use` items from `src/` directly). Marked `#[ignore]` per repo
//! conventions: `cargo test -- --ignored pdf`.
//!
//! The whole module is gated on the `pdf` feature: the parser dependency is a
//! crates.io registry version patched to a security fork only in the workspace
//! root (never published), and these tests exercise the fork's behavior (e.g.
//! the RUSTSEC-2026-0187 regression guard below), so they only build where the
//! patch is in effect.
#![cfg(feature = "pdf")]

use choreo_ai_protocols::ChatToolCall;
use choreo_daemon::tools::{ToolOutputFormat, ToolRegistry};
use choreo_daemon::{
    PdfClassifyArgs, PdfToMarkdownArgs, execute_pdf_classify, execute_pdf_to_markdown,
};
use std::collections::HashSet;

// Include the shared fixture builders (xref-correct PDF generation,
// image-only fixture, RUSTSEC-2026-0187 PoC, temp-file writer) from the
// crate source so the unit tests and this integration test can never drift
// apart.
#[path = "../src/tools/pdf/test_fixtures.rs"]
mod test_fixtures;

use test_fixtures::{
    build_pdf, image_only_pdf, minimal_text_pdf, nested_array_poc_pdf, write_temp,
};

#[test]
#[ignore]
fn pdf_tools_registered_in_core_group() {
    let registry = ToolRegistry::new().build();
    let mut active = HashSet::new();
    active.insert("core".to_string());
    let defs = registry.available_definitions(&active);
    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    for tool in ["pdf_classify", "pdf_to_markdown"] {
        assert!(names.contains(&tool), "missing {tool} in core: {names:?}");
    }
}

#[test]
#[ignore]
fn pdf_classify_through_registry() {
    let registry = ToolRegistry::new().build();
    let file = write_temp(&minimal_text_pdf());
    let tool_call = ChatToolCall {
        id: "call_pdf_classify".to_string(),
        name: "pdf_classify".to_string(),
        arguments_json: format!(r#"{{"path": "{}"}}"#, file.path().display()),
        caller: None,
    };
    let output = registry
        .execute_json(&tool_call, ToolOutputFormat::Text, None, None, None, None)
        .expect("tool execution should succeed");
    assert!(!output.is_error, "classify failed: {}", output.content);
    assert!(
        output.content.contains("pdf_type: text_based"),
        "{}",
        output.content
    );
    assert!(output.content.contains("confidence:"), "{}", output.content);
}

#[test]
#[ignore]
fn pdf_to_markdown_through_registry() {
    let registry = ToolRegistry::new().build();
    let file = write_temp(&minimal_text_pdf());
    let tool_call = ChatToolCall {
        id: "call_pdf_md".to_string(),
        name: "pdf_to_markdown".to_string(),
        arguments_json: format!(r#"{{"path": "{}"}}"#, file.path().display()),
        caller: None,
    };
    let output = registry
        .execute_json(&tool_call, ToolOutputFormat::Text, None, None, None, None)
        .expect("tool execution should succeed");
    assert!(
        !output.is_error,
        "pdf_to_markdown failed: {}",
        output.content
    );
    assert!(output.content.contains("Hello World"), "{}", output.content);
    assert!(
        output
            .content
            .contains("UNTRUSTED content extracted from PDF"),
        "{}",
        output.content
    );
}

#[test]
#[ignore]
fn pdf_to_markdown_pages_filter() {
    let file = write_temp(&build_pdf(&[
        "BT /F1 24 Tf 72 720 Td (FIRST PAGE ONLY) Tj ET",
        "BT /F1 24 Tf 72 720 Td (SECOND PAGE ONLY) Tj ET",
    ]));
    let out = execute_pdf_to_markdown(
        &PdfToMarkdownArgs {
            path: file.path().to_str().unwrap().to_string(),
            pages: Some(vec![2]),
            compact: None,
        },
        None,
    )
    .unwrap();
    assert!(out.contains("SECOND PAGE ONLY"), "{out}");
    assert!(!out.contains("FIRST PAGE ONLY"), "{out}");
}

#[test]
#[ignore]
fn pdf_to_markdown_routes_scanned_to_ocr() {
    let file = write_temp(&image_only_pdf());
    let out = execute_pdf_to_markdown(
        &PdfToMarkdownArgs {
            path: file.path().to_str().unwrap().to_string(),
            pages: None,
            compact: None,
        },
        None,
    )
    .unwrap();
    assert!(
        out.contains("Route to OCR/vision") && out.contains("no extractable text"),
        "{out}"
    );
}

#[test]
#[ignore]
fn pdf_classify_scanned_is_not_text_based() {
    let file = write_temp(&image_only_pdf());
    let out = execute_pdf_classify(
        &PdfClassifyArgs {
            path: file.path().to_str().unwrap().to_string(),
        },
        None,
    )
    .unwrap();
    assert!(out.contains("pdf_type:"), "{out}");
    assert!(!out.contains("pdf_type: text_based"), "{out}");
}

#[test]
#[ignore]
fn nested_array_poc_does_not_abort_process() {
    // Regression guard for RUSTSEC-2026-0187. With lopdf < 0.42 this PDF
    // (deeply nested /X array in the Catalog) aborts the process via stack
    // overflow — a SIGABRT no `catch_unwind` can intercept, so the daemon
    // would die. With the pinned lopdf >= 0.42 the parser caps nesting
    // depth: the object fails to load (logged by lopdf), the document ends
    // up with no usable pages, and detection classifies it as scanned —
    // exactly the upstream PR #198 verification (`Type: SCANNED`, exit 0).
    // Reaching the assertions below proves the process survived.
    let file = write_temp(&nested_array_poc_pdf(10_380));
    let result = execute_pdf_classify(
        &PdfClassifyArgs {
            path: file.path().to_str().unwrap().to_string(),
        },
        None,
    );
    match result {
        // Clean parse failure is one acceptable outcome...
        Err(e) => assert!(!e.to_string().is_empty(), "error must be non-empty"),
        // ...and so is a graceful (non-aborting) classification.
        Ok(out) => assert!(out.contains("pdf_type:"), "unexpected output: {out}"),
    }
}
