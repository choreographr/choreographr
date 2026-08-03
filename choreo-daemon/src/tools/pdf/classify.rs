//! `pdf_classify` — cheap PDF classification for smart OCR-vs-local routing.

use super::{pdf_type_label, read_validated_pdf, render_page_list};
use crate::tools::{ToolExecError, sanitize_name};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use tracing::debug;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PdfClassifyArgs {
    /// Path to the PDF file
    pub path: String,
}

pub fn execute_pdf_classify(
    args: &PdfClassifyArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }
    let bytes = read_validated_pdf(&args.path, working_dir)?;
    // DetectOnly mode: scans content streams for text/image operators in
    // ~10-50ms without extracting anything — cheap enough to call first when
    // deciding whether a PDF should be routed to OCR or parsed locally.
    let result = pdf_inspector::detect_pdf_mem(&bytes).map_err(super::map_pdf_error)?;
    debug!(
        path = %sanitize_name(&args.path),
        pdf_type = ?result.pdf_type,
        confidence = result.confidence,
        pages = result.page_count,
        "pdf_classify complete"
    );
    Ok(format!(
        "pdf_type: {}\nconfidence: {:.2}\npages: {}\npages_needing_ocr: [{}]",
        pdf_type_label(result.pdf_type),
        result.confidence,
        result.page_count,
        render_page_list(&result.pages_needing_ocr),
    ))
}

pub fn describe_pdf_classify_invocation(args: &PdfClassifyArgs) -> String {
    // Sanitize the path: the description renders in the TUI, so a hostile
    // filename must not inject terminal escapes there either (same policy
    // as the tracing fields).
    format!(
        "Classifying PDF `{}` (type, confidence, OCR pages).",
        sanitize_name(&args.path)
    )
}

pub(crate) struct PdfClassify;

define_tool!(
    PdfClassify,
    "pdf_classify",
    "Classify a PDF as text-based, scanned, image-based, or mixed, with a confidence score and per-page OCR routing. Fast (~10-50ms), no OCR. Use it to decide whether to extract text locally (pdf_to_markdown) or route to OCR/vision for scanned or image-heavy PDFs.",
    PdfClassifyArgs,
    execute_pdf_classify,
    "core",
    describe_pdf_classify_invocation
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::pdf::test_fixtures::{image_only_pdf, minimal_text_pdf, write_temp};

    #[test]
    fn classify_text_pdf() {
        let file = write_temp(&minimal_text_pdf());
        let out = execute_pdf_classify(
            &PdfClassifyArgs {
                path: file.path().to_str().unwrap().to_string(),
            },
            None,
        )
        .unwrap();
        assert!(out.contains("pdf_type: text_based"), "{out}");
        assert!(out.contains("confidence:"), "{out}");
        assert!(out.contains("pages: 1"), "{out}");
        assert!(out.contains("pages_needing_ocr: []"), "{out}");
    }

    #[test]
    fn classify_image_only_pdf_is_not_text_based() {
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
    fn classify_rejects_empty_path() {
        let err = execute_pdf_classify(&PdfClassifyArgs { path: "".into() }, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("path"), "{err}");
    }

    #[test]
    fn invocation_description_includes_path() {
        let classify = describe_pdf_classify_invocation(&PdfClassifyArgs {
            path: "doc.pdf".into(),
        });
        assert!(classify.contains("doc.pdf"), "{classify}");
    }

    #[test]
    fn invocation_description_sanitizes_control_chars_in_path() {
        // The description renders in the TUI, so a hostile filename with an
        // embedded newline must arrive escaped, not as a real line break.
        let classify = describe_pdf_classify_invocation(&PdfClassifyArgs {
            path: "evil\ndoc.pdf".into(),
        });
        assert!(classify.contains("evil\\ndoc.pdf"), "{classify}");
        assert!(!classify.contains('\n'), "{classify}");
    }
}
