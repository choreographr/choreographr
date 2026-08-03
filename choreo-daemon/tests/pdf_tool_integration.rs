//! Integration tests for the native PDF tools (`pdf_classify`,
//! `pdf_to_markdown`).
//!
//! These exercise the full registry path (`ToolRegistry::execute_json`) and
//! the exported tool functions against real tempfile PDFs built by the
//! programmatic fixture builders below. Marked `#[ignore]` per repo
//! conventions: `cargo test -- --ignored pdf`.

use choreo_ai_protocols::ChatToolCall;
use choreographr::tools::{ToolOutputFormat, ToolRegistry};
use choreographr::{
    PdfClassifyArgs, PdfToMarkdownArgs, execute_pdf_classify, execute_pdf_to_markdown,
};
use std::collections::HashSet;
use std::io::Write;

/// Build a deterministic PDF from per-page content streams with correctly
/// computed xref offsets. Duplicated from the unit-test builder (integration
/// tests cannot import from `src/`).
fn build_pdf(contents: &[&str]) -> Vec<u8> {
    let n = contents.len() as u32;
    let mut objs: Vec<Vec<u8>> = Vec::new();

    objs.push(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec());

    let kids: String = (3..3 + n)
        .map(|i| format!("{i} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    objs.push(
        format!("2 0 obj\n<< /Type /Pages /Kids [{kids}] /Count {n} >>\nendobj\n").into_bytes(),
    );

    let font_id = 3 + n;
    for (i, _) in contents.iter().enumerate() {
        let page_id = 3 + i as u32;
        let content_id = font_id + 1 + i as u32;
        objs.push(
            format!(
                "{page_id} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {content_id} 0 R >>\nendobj\n"
            )
            .into_bytes(),
        );
    }
    objs.push(
        format!(
            "{font_id} 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n"
        )
        .into_bytes(),
    );
    for (i, content) in contents.iter().enumerate() {
        let content_id = font_id + 1 + i as u32;
        objs.push(
            format!(
                "{content_id} 0 obj\n<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
                content.len()
            )
            .into_bytes(),
        );
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objs.len());
    for obj in &objs {
        offsets.push(out.len());
        out.extend_from_slice(obj);
    }
    let xref_pos = out.len();
    let mut xref = format!("xref\n0 {}\n", objs.len() + 1);
    xref.push_str("0000000000 65535 f \n");
    for off in &offsets {
        xref.push_str(&format!("{off:010} 00000 n \n"));
    }
    out.extend_from_slice(xref.as_bytes());
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn minimal_text_pdf() -> Vec<u8> {
    build_pdf(&["BT /F1 24 Tf 72 720 Td (Hello World) Tj ET"])
}

/// A single-page PDF whose content stream is a full-page image `Do` with no
/// text operators — classified scanned/image-based by pdf-inspector.
fn image_only_pdf() -> Vec<u8> {
    let mut objs: Vec<Vec<u8>> = Vec::new();
    objs.push(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec());
    objs.push(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n".to_vec());
    objs.push(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>\nendobj\n"
            .to_vec(),
    );
    let content = b"q 72 72 468 648 re W n /Im0 Do Q";
    objs.push(
        format!(
            "4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
            content.len(),
            String::from_utf8_lossy(content)
        )
        .into_bytes(),
    );
    objs.push(
        b"5 0 obj\n<< /Type /XObject /Subtype /Image /Width 1 /Height 1 \
          /ColorSpace /DeviceGray /BitsPerComponent 8 /Length 1 >>\nstream\n\x00\nendstream\nendobj\n"
            .to_vec(),
    );
    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objs.len());
    for obj in &objs {
        offsets.push(out.len());
        out.extend_from_slice(obj);
    }
    let xref_pos = out.len();
    let mut xref = format!("xref\n0 {}\n", objs.len() + 1);
    xref.push_str("0000000000 65535 f \n");
    for off in &offsets {
        xref.push_str(&format!("{off:010} 00000 n \n"));
    }
    out.extend_from_slice(xref.as_bytes());
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Write bytes to a fresh temp file and return its path (kept alive for the
/// call's duration).
fn write_temp(bytes: &[u8]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file
}

/// Build the RUSTSEC-2026-0187 PoC: a minimal PDF whose Catalog carries a
/// deeply nested array (`/X [[[ … ]]]`, ~10,380 levels).
///
/// With `lopdf < 0.42` parsing this aborts the whole process via stack
/// overflow (SIGABRT) — unrecoverable by `catch_unwind`. With the pinned
/// `lopdf >= 0.42` the parser caps nesting depth and returns an `Err`
/// instead. This test's *survival* is the assertion; the explicit error
/// check is secondary.
fn nested_array_poc_pdf(depth: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n");

    let mut catalog = String::from("1 0 obj\n<< /Type /Catalog /X ");
    for _ in 0..depth {
        catalog.push('[');
    }
    for _ in 0..depth {
        catalog.push(']');
    }
    catalog.push_str(" >>\nendobj\n");
    let catalog_off = out.len();
    out.extend_from_slice(catalog.as_bytes());

    let pages_off = out.len();
    out.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");

    let xref_pos = out.len();
    let trailer = format!(
        "xref\n0 3\n0000000000 65535 f \n{catalog_off:010} 00000 n \n{pages_off:010} 00000 n \ntrailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n"
    );
    out.extend_from_slice(trailer.as_bytes());
    out
}

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
